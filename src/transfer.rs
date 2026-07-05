/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 *
 * Licensed under the Apache License, Version 2.0 (the "License").
 * You may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use super::batch;
use super::control;
use super::data;
use super::ddb::table;
use super::{algo, app};
use aws_sdk_dynamodb::operation::batch_write_item::BatchWriteItemError;
use aws_sdk_dynamodb::operation::RequestId;
use aws_sdk_dynamodb::types::ReturnConsumedCapacity;
use aws_sdk_dynamodb::{
    operation::scan::ScanOutput,
    types::{AttributeValue, WriteRequest},
    Client as DynamoDbSdkClient,
};
use aws_smithy_runtime_api::client::result::SdkError;
use console::Term;
use dialoguer::Confirm;
use log::{debug, error, info, trace, warn};
use serde_json::{Deserializer, Value as JsonValue};
use std::collections::VecDeque;
use std::fmt::Debug;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{
    collections::HashMap,
    fs,
    io::{Error as IOError, Write},
    path::Path,
};
use thiserror::Error;
use tokio::select;

#[derive(Error, Debug)]
pub enum DyneinExportError {
    #[error("io error")]
    IO(#[from] std::io::Error),
    #[error("serde error")]
    SerdeError(#[from] serde_json::Error),
}

impl From<dialoguer::Error> for DyneinExportError {
    fn from(e: dialoguer::Error) -> Self {
        match e {
            dialoguer::Error::IO(e) => DyneinExportError::IO(e),
        }
    }
}

#[derive(Debug)]
struct SuggestedAttribute {
    name: String,
    type_str: String,
}

#[derive(Clone, Debug, Hash, PartialOrd, PartialEq)]
struct ProgressState {
    processed_items: usize,
    recent_processed_items: VecDeque<(Instant, usize)>,
    max_recordable_observations: usize,
}

impl ProgressState {
    fn new(max_recordable_observations: usize) -> ProgressState {
        ProgressState {
            processed_items: 0,
            recent_processed_items: VecDeque::with_capacity(max_recordable_observations),
            max_recordable_observations,
        }
    }

    fn add_observation(&mut self, processed_items: usize) {
        self.add_observation_with_time(processed_items, Instant::now())
    }

    fn add_observation_with_time(&mut self, processed_items: usize, at: Instant) {
        self.processed_items += processed_items;

        if self.recent_processed_items.len() == self.max_recordable_observations {
            self.recent_processed_items.pop_back();
        }
        self.recent_processed_items
            .push_front((at, processed_items));
    }

    fn processed_items(&self) -> usize {
        self.processed_items
    }

    fn recent_average_processed_items_per_second(&self) -> f64 {
        self.recent_average_processed_items_per_second_with_time(Instant::now())
    }

    fn recent_average_processed_items_per_second_with_time(&self, at: Instant) -> f64 {
        let mut sum = 0.0;
        for v in &self.recent_processed_items {
            sum += v.1 as f64
        }
        if let Some((oldest_time, _)) = self.recent_processed_items.back() {
            if at == *oldest_time {
                f64::NAN
            } else {
                sum / at.duration_since(*oldest_time).as_secs_f64()
            }
        } else {
            0.0
        }
    }

    fn show(&self) {
        let items = self.processed_items();
        let items_per_sec = self.recent_average_processed_items_per_second();
        let mut term = Term::stdout();
        term.clear_line().expect("Failed to clear line");
        write!(
            term,
            "{} items processed ({:.2} items/sec)",
            items, items_per_sec
        )
        .expect("Failed to update message");
        term.flush().expect("Failed to flush");
    }
}

const MAX_NUMBER_OF_OBSERVES: usize = 256;

const VISUALIZE_INTERVAL: Duration = Duration::from_millis(200);

/* =================================================
Public functions
================================================= */

/// Export items in a DynamoDB table into specified format (JSON, JSONL, JSON compact, or CSV. default is JSON).
/// As CSV is a kind of "structured" format, you cannot export DynamoDB's NoSQL-ish "unstructured" data into CSV without any instruction from users.
/// Thus as an "instruction" this function takes --attributes or --keys-only options. If neither of them are given, dynein "guesses" attributes to export from the first item.
pub async fn export(
    cx: &app::Context,
    given_attributes: Option<String>,
    keys_only: bool,
    output_file: String,
    format: Option<String>,
) -> Result<(), DyneinExportError> {
    // TODO: Parallel scan to make it faster https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/Scan.html#Scan.ParallelScan
    // TODO: Show rough progress bar (sum(scan_output.scanned_item)/item_size_of_the_table(6hr)) to track progress.
    let ts: app::TableSchema = app::table_schema(cx).await;
    let format_str: Option<&str> = format.as_deref();

    if ts.mode == table::Mode::Provisioned {
        let msg = "WARN: For the best performance on import/export, dynein recommends OnDemand mode. However the target table is Provisioned mode now. Proceed anyway?";
        if !Confirm::new().with_prompt(msg).interact()? {
            app::bye(0, "Operation has been cancelled.");
        }
    }

    // Basically given_attributes would be used, but on CSV format, it can be overwritten by suggested attributes
    let attributes: Option<String> = match format_str {
        Some("csv") => {
            if !keys_only && given_attributes.is_none() {
                overwrite_attributes_or_exit(cx, &ts)
                    .await
                    .expect("failed to overwrite attributes based on a scanned item")
            } else {
                given_attributes
            }
        }
        None | Some(_) => {
            if keys_only || given_attributes.is_some() {
                app::bye(
                    1,
                    "You can use --keys-only and --attributes only with CSV format.",
                )
            }
            given_attributes
        }
    };

    // Create output file. If target file already exists, ask users if it's ok to delete contents of the file.
    // Though final output file is created here, it would be blank until scan all items. You can see progress in temporary output file.
    let f: fs::File = if Path::new(&output_file).exists() {
        let msg = "Specified output file already exists. Is it OK to truncate contents?";
        if !Confirm::new().with_prompt(msg).interact()? {
            app::bye(0, "Operation has been cancelled.");
        }
        debug!("truncating existing output file.");
        let _f = fs::OpenOptions::new().append(true).open(&output_file)?;
        _f.set_len(0)?;
        _f
    } else {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&output_file)?
    };

    // These temporary file is used to store data "body" and finally merged into output file.
    let tmp_output_filename: &str = &format!("{}_tmp", output_file);
    let mut tmp_output_file: fs::File = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(tmp_output_filename)?;
    tmp_output_file.set_len(0)?;

    let mut last_evaluated_key: Option<HashMap<String, AttributeValue>> = None;
    let mut progress_status = ProgressState::new(MAX_NUMBER_OF_OBSERVES);
    loop {
        // Invoke Scan API here. At the 1st iteration exclusive_start_key would be "None" as defined above, outside of the loop.
        // On 2nd iteration and later, passing last_evaluated_key from the previous loop as an exclusive_start_key.
        let scan_output: ScanOutput = data::scan_api(
            cx,
            None,  /* index */
            false, /* consistent_read */
            &attributes,
            keys_only,
            None,               /* limit */
            last_evaluated_key, /* exclusive_start_key */
        )
        .await;

        let items = scan_output
            .items
            .expect("Scan result items should be 'Some' even if no item returned.");

        progress_status.add_observation(items.len());
        match format_str {
            None | Some("json") => {
                let s = serde_json::to_string_pretty(&data::convert_to_json_vec(&items))?;
                tmp_output_file.write_all(connectable_json(s, false).as_bytes())?;
            }
            Some("jsonl") => {
                let mut s: String = String::new();
                for item in &items {
                    s.push_str(&serde_json::to_string(&data::convert_to_json(item))?);
                    s.push('\n');
                }
                tmp_output_file.write_all(s.as_bytes())?;
            }
            Some("json-compact") => {
                let s = serde_json::to_string(&data::convert_to_json_vec(&items))?;
                tmp_output_file.write_all(connectable_json(s, true).as_bytes())?;
            }
            Some("csv") => {
                let s = data::convert_items_to_csv_lines(
                    &items,
                    &ts,
                    &attrs_to_append(&ts, &attributes),
                    keys_only,
                );
                tmp_output_file.write_all(s.as_bytes())?;
            }
            Some(o) => panic!("Invalid output format is given: {}", o),
        }
        progress_status.show();

        // update last_evaluated_key for the next iteration.
        // If there's no more item in the table, last_evaluated_key would be "None" and it means it's ok to break the loop.
        debug!(
            "scan_output.last_evaluated_key is: {:?}",
            &scan_output.last_evaluated_key
        );
        match scan_output.last_evaluated_key {
            None => break,
            Some(lek) => last_evaluated_key = Some(lek),
        }
    }

    match format_str {
        None | Some("json") => json_finish(f, tmp_output_filename)?.write_all(b"\n]")?,
        Some("json-compact") => json_finish(f, tmp_output_filename)?.write_all(b"]")?,
        Some("jsonl") => jsonl_finish(f, tmp_output_filename)?,
        Some("csv") => csv_finish(
            f,
            tmp_output_filename,
            &ts,
            attrs_to_append(&ts, &attributes),
            keys_only,
        )?
        .write_all(b"\n")?,
        Some(o) => panic!("Invalid output format is given: {}", o),
    };

    // As mentioned earlier, deleting temporary file here in all formats.
    fs::remove_file(tmp_output_filename)?;

    Ok(())
}

/// Resolves the congestion controller ceiling and the capacity hints from
/// the optional --max-wcu. No --max-wcu means **no ceiling**: pacing is left
/// entirely to the congestion control (known-information initial target +
/// AIMD throttle feedback + the CloudWatch slow loop). An unbounded ceiling
/// requires a finite starting point (the controller asserts this: an
/// infinite effective target could never back off), so a missing capacity
/// hint is backfilled with the conservative warm default.
fn resolve_target_ceiling(max_wcu: Option<f64>, hints: CapacityHints) -> (f64, CapacityHints) {
    match max_wcu {
        Some(ceiling) => (ceiling, hints),
        None => (
            f64::INFINITY,
            CapacityHints {
                initial_wcu: hints.initial_wcu.or(Some(DEFAULT_ON_DEMAND_WARM_WCU)),
                ..hints
            },
        ),
    }
}

pub async fn import(
    cx: &app::Context,
    input_file: String,
    format: Option<String>,
    enable_set_inference: bool,
    max_wcu: Option<f64>,
) -> Result<(), batch::DyneinBatchError> {
    let format_str: Option<&str> = format.as_deref();

    let ts: app::TableSchema = app::table_schema(cx).await;
    if ts.mode == table::Mode::Provisioned {
        let msg = "WARN: For the best performance on import/export, dynein recommends OnDemand mode. However the target table is Provisioned mode now. Proceed anyway?";
        if !Confirm::new().with_prompt(msg).interact()? {
            println!("Operation has been cancelled.");
            return Ok(());
        }
    }

    // Items are streamed out of the file while the pipeline writes them, so
    // memory usage is bounded by the admission cap, not by the input size.
    // Opening the file up front keeps "file not found" a pre-pipeline error.
    let file: fs::File = if Path::new(&input_file).exists() {
        fs::File::open(&input_file)?
    } else {
        error!("Couldn't find the input file '{}'.", &input_file);
        std::process::exit(1);
    };
    info!("Start streaming items from the input file");

    // Give the AIMD congestion control a realistic starting point and a
    // capacity reference derived from known table information instead of the
    // user-specified ceiling (which is unbounded unless --max-wcu is given).
    let hints = capacity_hints(cx, &ts).await;
    let (max_wcu, hints) = resolve_target_ceiling(max_wcu, hints);

    match format_str {
        None | Some("json") | Some("json-compact") => {
            stream_writes_with_chucked(
                cx,
                move |sink| {
                    stream_json_array_items(std::io::BufReader::new(file), &mut |v| {
                        let item = batch::convert_jsonval_to_hashmap(&v, enable_set_inference);
                        sink(batch::construct_put_write_request(item))
                    })
                },
                max_wcu,
                hints,
            )
            .await?;
        }
        Some("jsonl") => {
            stream_writes_with_chucked(
                cx,
                move |sink| {
                    stream_jsonl_items(std::io::BufReader::new(file), &mut |v| {
                        let item = batch::convert_jsonval_to_hashmap(&v, enable_set_inference);
                        sink(batch::construct_put_write_request(item))
                    })
                },
                max_wcu,
                hints,
            )
            .await?;
        }
        Some("csv") => {
            stream_writes_with_chucked(
                cx,
                move |sink| {
                    stream_csv_rows(std::io::BufReader::new(file), enable_set_inference, sink)
                },
                max_wcu,
                hints,
            )
            .await?;
        }
        Some(o) => panic!("Invalid input format is given: {}", o),
    }
    Ok(())
}

/* =================================================
Private functions
================================================= */

async fn overwrite_attributes_or_exit(
    cx: &app::Context,
    ts: &app::TableSchema,
) -> Result<Option<String>, dialoguer::Error> {
    println!("As neither --keys-only nor --attributes options are given, fetching an item to understand attributes to export...");
    let suggested_attributes: Vec<SuggestedAttribute> = suggest_attributes(cx, ts).await;

    // if at least one attribute found
    println!("Found following attributes in the first item in the table:");
    for preview_attribute in &suggested_attributes {
        println!(
            "  - {} ({})",
            preview_attribute.name, preview_attribute.type_str
        );
    }
    let msg = "Are you OK to export items in CSV with columns(attributes) above?";
    if !Confirm::new().with_prompt(msg).interact()? {
        app::bye(0, "Operation has been cancelled. You can use --keys-only or --attributes option to specify columns explicitly.");
    }

    // Overwrite given attributes with suggested attributes beased on a sampled item
    Ok(Some(
        suggested_attributes
            .into_iter()
            .map(|sa| sa.name)
            .collect::<Vec<String>>()
            .join(","),
    ))
}

/// This function scan the fisrt item from the target table and use it as a source of attributes.
async fn suggest_attributes(cx: &app::Context, ts: &app::TableSchema) -> Vec<SuggestedAttribute> {
    let mut attributes_suggestion = vec![];

    // items: Vec<HashMap<String, AttributeValue>>
    let items = data::scan_api(
        cx,
        None,    /* index */
        false,   /* consistent_read */
        &None,   /* attributes */
        false,   /* keys_only */
        Some(1), /* limit */
        None,    /* esk */
    )
    .await
    .items
    .expect("items should be 'Some' even if there's no item in the table.");

    if items.is_empty() {
        app::bye(0, "No item to export in this table. Quit the operation.");
    }

    // Filter out primary keys. i.e. select attributes that aren't required by the table's keyschema.
    let primary_keys = [
        Some(ts.pk.name.to_owned()),
        ts.sk.to_owned().map(|x| x.name),
    ];
    let non_key_attributes = items[0]
        .iter()
        .filter(
            |(attr, _)| {
                !primary_keys
                    .iter()
                    .any(|key| Some(attr.to_owned()) == key.as_ref())
            }, // ).map(|(k, _)| k).collect::<Vec<&String>>();
        )
        .collect::<Vec<(&String, &AttributeValue)>>();

    for (attr, attrval) in non_key_attributes {
        attributes_suggestion.push(SuggestedAttribute {
            name: attr.to_owned(),
            type_str: data::attrval_to_type(attrval).expect("attrval should be mapped"),
        });
    }

    debug!("Suggested attributes to use: {:?}", attributes_suggestion);
    attributes_suggestion
}

fn attrs_to_append(ts: &app::TableSchema, attributes: &Option<String>) -> Option<Vec<String>> {
    attributes
        .as_ref()
        .map(|ats| filter_attributes_to_append(ts, ats))
}

/// This function takes list of attributes separated by comma (e.g. "name,age,address")
/// and return vec of these strings, filtering pk/sk.
fn filter_attributes_to_append(ts: &app::TableSchema, ats: &str) -> Vec<String> {
    let mut attributes_to_append: Vec<String> = vec![];
    let splitted_attributes: Vec<String> = ats.split(',').map(|x| x.trim().to_owned()).collect();
    for attr in splitted_attributes {
        // skip if attributes contain primary key(s)
        if attr == ts.pk.name || (ts.sk.is_some() && attr == ts.sk.as_ref().unwrap().name) {
            println!("NOTE: primary keys are included by default and you don't need to give them as a part of --attributes.");
            continue;
        }
        attributes_to_append.push(attr);
    }
    attributes_to_append
}

/// This function tweaks scan output items.
/// Each scan iteration, converted string would be a single JSON array: e.g. [ {a:1}, {a:2} ]
/// When multiple scan is needed (i.e. when last_evaluated_key is Some), connected string would be: e.g. [ {a:1}, {a:2} ][ {a:3}, {a:4} ]
/// To avoid this invalid JSON from written to output file, this method remove the first "[" and the last "]", then add "," after the last item.
fn connectable_json(mut s: String, compact: bool) -> String {
    s.remove(0); // remove first char "["
    let len = s.len();
    if compact || len == 1 {
        // empty array even if not compact is on one line
        s.truncate(len - 1); // remove last char "]"
    } else {
        s.truncate(len - 2); // remove last char "]" and newline
    }
    s.push(','); // add last "," so that continue to next iteration
    s
}

/// This function takes final output file and temporary filename which has incomplete JSON body, and write final output JSON file.
/// last "]" is not added in this function, as it depends on json or json-compact.
fn json_finish(mut f: fs::File, tmp_output_filename: &str) -> Result<fs::File, IOError> {
    f.write_all(b"[")?; // write initial "[" as the first letter of JSON array.
    let mut contents = fs::read_to_string(tmp_output_filename)?;
    let len = contents.len();
    contents.truncate(len - 1); // remove last ","
    f.write_all(contents.as_bytes())?;
    Ok(f)
}

/// This function takes final output file and temporary filename. For JSON"L", copying whole content is enough.
fn jsonl_finish(mut f: fs::File, tmp_output_filename: &str) -> Result<(), IOError> {
    let contents = fs::read_to_string(tmp_output_filename)?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}

/// This function takes final output file and temporary filename, writing CSV header and then copying contents to the output file.
fn csv_finish(
    mut f: fs::File,
    tmp_output_filename: &str,
    ts: &app::TableSchema,
    attributes_to_append: Option<Vec<String>>,
    keys_only: bool,
) -> Result<fs::File, IOError> {
    f.write_all(build_csv_header(ts, attributes_to_append, keys_only).as_bytes())?;
    let contents = fs::read_to_string(tmp_output_filename)?;
    f.write_all(contents.as_bytes())?;
    Ok(f)
}

/// This function generate CSV headers for the output file to export.
fn build_csv_header(
    ts: &app::TableSchema,
    attributes_to_append: Option<Vec<String>>,
    keys_only: bool,
) -> String {
    // First of all put pk (and sk, if exists)
    let mut header_str: String = ts.pk.name.clone();
    if let Some(sk) = &ts.sk {
        header_str.push(',');
        header_str.push_str(&sk.name);
    };

    if keys_only {
    } else if let Some(attrs) = attributes_to_append {
        header_str.push(',');
        header_str.push_str(&attrs.join(","));
    }

    header_str.push('\n');
    header_str
}

const BATCH_WRITE_BUFFER_SIZE: usize = 500;

/// Upper bound on the number of items resident in the import pipeline at
/// once (main channel, chunker batch, process channel, in-flight requests
/// and the retry queue). With streaming file reads the input size no longer
/// bounds memory, and the unbounded retry channel needs a new bound: the
/// producer acquires one admission permit per item and the permit is
/// released only when the item is finally resolved (written or permanently
/// failed), so retries circulate without interfering with admission.
///
/// Sizing: the steady-state population needed to saturate the highest
/// targets is roughly main channel (500) + process channel (16×25) + the
/// task executor's default in-flight ceiling (256 requests × 25 items) ≈
/// 7,300 items; 10,000 leaves headroom. It must also comfortably exceed one
/// batch (25) — the chunker waits for a full batch, so a cap smaller than a
/// batch would deadlock admission against batching.
const ADMISSION_ITEM_CAP: usize = 10_000;

/// Ratio applied to the provisioned WCU to decide the initial effective target.
/// Not starting at 100% leaves room for co-located production workloads from
/// the beginning; the AIMD recovery then probes for the remaining capacity.
const INITIAL_TARGET_RATIO_OF_PROVISIONED: f64 = 0.8;

/// The platform-default warm throughput of on-demand tables (4,000 WCU).
/// Used as the fallback when DescribeTable does not report the table's
/// actual warm throughput (`warm_write_units`), and as the finite starting
/// point that an unbounded ceiling requires when no capacity hint exists.
const DEFAULT_ON_DEMAND_WARM_WCU: f64 = 4000.0;

/// Capacity-related hints for congestion control, derived from table information.
#[derive(Clone, Copy, Debug, Default)]
struct CapacityHints {
    /// Initial effective WCU target (a realistic, known-information starting point).
    initial_wcu: Option<f64>,
    /// The table's provisioned WCU. Enables the CloudWatch slow control loop,
    /// which needs a capacity reference to estimate available headroom.
    provisioned_wcu: Option<f64>,
}

/// Derives capacity hints for AIMD congestion control from known table
/// information: a ratio of the provisioned capacity, or the default warm
/// throughput for on-demand tables.
async fn capacity_hints(cx: &app::Context, ts: &app::TableSchema) -> CapacityHints {
    match ts.mode {
        table::Mode::Provisioned => {
            let desc = control::describe_table_api(cx, ts.name.clone()).await;
            let provisioned_wcu = desc
                .provisioned_throughput
                .and_then(|p| p.write_capacity_units)
                .map(|wcu| wcu as f64);
            CapacityHints {
                initial_wcu: provisioned_wcu.map(|wcu| wcu * INITIAL_TARGET_RATIO_OF_PROVISIONED),
                provisioned_wcu,
            }
        }
        // The slow loop is not enabled for on-demand tables for now: warm
        // throughput says what the table absorbs instantly, but using it as
        // the headroom reference for boosts is a semantic extension that has
        // not been designed yet.
        table::Mode::OnDemand => {
            let desc = control::describe_table_api(cx, ts.name.clone()).await;
            CapacityHints {
                // The actual warm throughput of the table: what it can absorb
                // right now without ramping up. Tables that never scaled keep
                // the platform default, so the fallback rarely matters.
                initial_wcu: Some(warm_write_units(&desc).unwrap_or(DEFAULT_ON_DEMAND_WARM_WCU)),
                provisioned_wcu: None,
            }
        }
    }
}

/// The table's warm throughput for writes (units/second) as reported by
/// DescribeTable, if present.
fn warm_write_units(desc: &aws_sdk_dynamodb::types::TableDescription) -> Option<f64> {
    desc.warm_throughput
        .as_ref()
        .and_then(|w| w.write_units_per_second)
        .map(|units| units as f64)
}

/// Interval of the slow control loop. Aligned with the CloudWatch metric
/// granularity; consulting more often cannot observe anything new.
const SLOW_LOOP_INTERVAL: Duration = Duration::from_secs(60);

/// Fetches the latest complete 1-minute datapoint of the table-level consumed
/// WCU from CloudWatch, as a per-second rate.
async fn fetch_table_consumed_rate(
    cw: &aws_sdk_cloudwatch::Client,
    table_name: &str,
) -> Result<
    Option<f64>,
    SdkError<
        aws_sdk_cloudwatch::operation::get_metric_statistics::GetMetricStatisticsError,
        aws_smithy_runtime_api::client::orchestrator::HttpResponse,
    >,
> {
    // Skip the most recent minute: its datapoint may not be complete yet.
    let end = std::time::SystemTime::now() - Duration::from_secs(60);
    let start = end - Duration::from_secs(600);
    let resp = cw
        .get_metric_statistics()
        .namespace("AWS/DynamoDB")
        .metric_name("ConsumedWriteCapacityUnits")
        .dimensions(
            aws_sdk_cloudwatch::types::Dimension::builder()
                .name("TableName")
                .value(table_name)
                .build(),
        )
        .start_time(aws_smithy_types::DateTime::from(start))
        .end_time(aws_smithy_types::DateTime::from(end))
        .period(60)
        .statistics(aws_sdk_cloudwatch::types::Statistic::Sum)
        .send()
        .await?;
    let latest = resp
        .datapoints
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.timestamp.is_some() && d.sum.is_some())
        .max_by_key(|d| d.timestamp.unwrap().secs());
    // The datapoint is a 60-second sum; convert it into a per-second rate.
    Ok(latest.map(|d| d.sum.unwrap() / 60.0))
}

/// The slow control loop: estimates the production workload from CloudWatch
/// metrics and suggests a safe target so the executor can recover more
/// aggressively than the conservative fast-loop recovery when it looks safe.
/// On a fetch error (e.g. missing CloudWatch permissions), it silently
/// degrades to the fast loop only.
async fn slow_control_loop(
    cw: aws_sdk_cloudwatch::Client,
    table_name: String,
    table_capacity: f64,
    stats: Arc<algo::congestion::CongestionStats>,
    boost_slot: Arc<algo::congestion::BoostSlot>,
    mut terminate_rx: tokio::sync::watch::Receiver<bool>,
) {
    let mut prev_consumed = stats.consumed();
    let mut prev_at = Instant::now();
    loop {
        select! {
            _ = tokio::time::sleep(SLOW_LOOP_INTERVAL) => {}
            _ = terminate_rx.changed() => break,
        }

        // Our own consumption rate over the last interval.
        let consumed = stats.consumed();
        let now = Instant::now();
        let own_rate = (consumed - prev_consumed) / now.duration_since(prev_at).as_secs_f64();
        prev_consumed = consumed;
        prev_at = now;

        match fetch_table_consumed_rate(&cw, &table_name).await {
            Ok(Some(table_rate)) => {
                let safe_target =
                    algo::congestion::estimate_safe_target(table_capacity, table_rate, own_rate);
                debug!(
                    "Slow loop: table {:.2} WCU/s, own {:.2} WCU/s -> safe target {:.2}",
                    table_rate, own_rate, safe_target
                );
                boost_slot.suggest(safe_target);
            }
            Ok(None) => {
                debug!("Slow loop: no complete CloudWatch datapoint available yet");
            }
            Err(e) => {
                // Likely missing CloudWatch permissions. Keep importing with
                // the conservative fast loop only.
                info!(
                    "Slow control loop is disabled (failed to fetch CloudWatch metrics): {}",
                    e
                );
                break;
            }
        }
    }
}

/// Environment variable enabling the benchmark stats emitter: when set to a
/// file path, a JSON line of cumulative counters is appended every second.
/// Machine-readable by design — the benchmark harness must not parse human
/// logs (docs/design/benchmark-plan.md §4).
const BENCH_STATS_ENV: &str = "DYNEIN_BENCH_STATS";

/// Interval between two benchmark stats lines.
const BENCH_STATS_INTERVAL: Duration = Duration::from_secs(1);

/// Renders one line of the benchmark stats stream. All counters are
/// cumulative since the start of the import.
#[allow(clippy::too_many_arguments)]
fn render_bench_stats_line(
    elapsed_secs: f64,
    consumed_wcu: f64,
    requests: usize,
    throttled: usize,
    effective_target: f64,
    resolved_items: usize,
    failed_items: usize,
    task_metrics: Option<&tokio_metrics::TaskMetrics>,
) -> String {
    let mut line = serde_json::json!({
        "t": elapsed_secs,
        "consumed_wcu": consumed_wcu,
        "requests": requests,
        "throttled": throttled,
        "effective_target": effective_target,
        "resolved_items": resolved_items,
        "failed_items": failed_items,
    });
    if let Some(m) = task_metrics {
        line["tokio"] = serde_json::json!({
            "instrumented_count": m.instrumented_count,
            "total_poll_count": m.total_poll_count,
            "total_poll_duration_us": m.total_poll_duration.as_micros() as u64,
            "total_scheduled_count": m.total_scheduled_count,
            "total_scheduled_duration_us": m.total_scheduled_duration.as_micros() as u64,
            "total_slow_poll_count": m.total_slow_poll_count,
        });
    }
    line.to_string()
}

/// Spawns the benchmark stats emitter when `DYNEIN_BENCH_STATS` is set.
/// The task appends one stats line per second and a final line when the
/// pipeline signals termination, so short runs still produce a series.
#[allow(clippy::too_many_arguments)]
fn spawn_bench_stats_emitter(
    path: String,
    stats: Arc<algo::congestion::CongestionStats>,
    target_gauge: Arc<algo::congestion::TargetGauge>,
    task_monitor: tokio_metrics::TaskMonitor,
    complete_items_count: Arc<AtomicUsize>,
    failed_items_count: Arc<AtomicUsize>,
    mut terminate_rx: tokio::sync::watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut file = match fs::OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => f,
            Err(e) => {
                error!("Failed to open the bench stats file '{}': {}", path, e);
                return;
            }
        };
        let start = Instant::now();
        let emit = |file: &mut fs::File| {
            let (requests, throttled) = stats.snapshot();
            let line = render_bench_stats_line(
                start.elapsed().as_secs_f64(),
                stats.consumed(),
                requests,
                throttled,
                target_gauge.get(),
                complete_items_count.load(Ordering::Relaxed),
                failed_items_count.load(Ordering::Relaxed),
                Some(&task_monitor.cumulative()),
            );
            if let Err(e) = writeln!(file, "{}", line) {
                error!("Failed to write the bench stats file '{}': {}", path, e);
                return false;
            }
            true
        };
        loop {
            select! {
                _ = tokio::time::sleep(BENCH_STATS_INTERVAL) => {
                    if !emit(&mut file) {
                        return;
                    }
                }
                _ = terminate_rx.changed() => break,
            }
        }
        // Final datapoint so that the series always covers the full run.
        emit(&mut file);
    })
}

/// Aborts the import when no progress has been made for this long.
/// This is the safety valve against livelocks (e.g. transport errors retried
/// forever after the network died); see the design document.
const STALL_DEADLINE: Duration = Duration::from_secs(300);

/// Detects that the pipeline has made no progress for a deadline period.
/// Pure logic with injected time so the behavior is unit-testable.
struct StallDetector {
    deadline: Duration,
    last_progress: usize,
    last_change_at: Instant,
}

impl StallDetector {
    fn new(deadline: Duration, now: Instant) -> StallDetector {
        StallDetector {
            deadline,
            last_progress: 0,
            last_change_at: now,
        }
    }

    /// Feeds the current progress (resolved item count). Returns true when the
    /// progress has not advanced for the deadline period.
    fn observe(&mut self, progress: usize, now: Instant) -> bool {
        if progress != self.last_progress {
            self.last_progress = progress;
            self.last_change_at = now;
            return false;
        }
        now.duration_since(self.last_change_at) >= self.deadline
    }
}

/// Summary of a single BatchWriteItem call result.
/// This describes what the caller has to do next: how many items completed,
/// which requests must be queued again for retry, and how many items failed permanently.
#[derive(Debug, Default)]
struct BatchWriteResultSummary {
    /// Write requests that should be sent again (unprocessed items and retryable errors).
    retry_requests: Vec<WriteRequest>,
    /// Number of items successfully written.
    successful_items: usize,
    /// Number of items failed permanently (non-retryable errors).
    failed_items: usize,
    /// Total consumed WCU reported by the response.
    consumed_capacity: f64,
    /// Whether this request observed a capacity shortage: the whole request was
    /// rejected with a throughput/limit error, or at least one of its items came
    /// back unprocessed. Used as the congestion signal for AIMD control.
    throttled: bool,
}

/// Classifies the result of a BatchWriteItem call.
/// Every requested item must be accounted for in exactly one of
/// `retry_requests`, `successful_items` or `failed_items`, so that the whole
/// import can neither lose items silently nor wait forever for items nobody retries.
///
/// `has_prior_success` tells whether any request has succeeded before this one.
/// Transport-level errors (timeout, dispatch failure, invalid response) are
/// retryable once we know the network configuration works, i.e. after the
/// first success. On the very first attempt they most likely indicate a
/// configuration problem, so the items are marked as failed instead.
fn summarize_batch_write_result(
    result: Result<
        aws_sdk_dynamodb::operation::batch_write_item::BatchWriteItemOutput,
        SdkError<BatchWriteItemError, aws_smithy_runtime_api::client::orchestrator::HttpResponse>,
    >,
    requested_items: &HashMap<String, Vec<WriteRequest>>,
    has_prior_success: bool,
) -> BatchWriteResultSummary {
    let total_requested: usize = requested_items.values().map(|reqs| reqs.len()).sum();
    let retry_all = |throttled: bool| BatchWriteResultSummary {
        retry_requests: requested_items.values().flatten().cloned().collect(),
        successful_items: 0,
        failed_items: 0,
        consumed_capacity: 0.0,
        throttled,
    };
    let fail_all = || BatchWriteResultSummary {
        retry_requests: vec![],
        successful_items: 0,
        failed_items: total_requested,
        consumed_capacity: 0.0,
        throttled: false,
    };
    match result {
        Ok(output) => {
            let retry_requests: Vec<WriteRequest> = output
                .unprocessed_items
                .unwrap_or_default()
                .into_values()
                .flatten()
                .collect();
            let consumed_capacity = output
                .consumed_capacity
                .unwrap_or_default()
                .iter()
                .map(|x| x.capacity_units.unwrap_or(0.0))
                .sum();
            BatchWriteResultSummary {
                successful_items: total_requested - retry_requests.len(),
                failed_items: 0,
                // Every unprocessed item is a server-side rejection due to a capacity
                // shortage. To protect co-located production workloads, even a single
                // one counts as a congestion signal; the decrease cooldown of the AIMD
                // controller keeps this strictness from over-reacting.
                throttled: !retry_requests.is_empty(),
                retry_requests,
                consumed_capacity,
            }
        }
        // Check whether retryable error.
        // https://docs.rs/aws-sdk-dynamodb/latest/aws_sdk_dynamodb/enum.Error.html
        // https://docs.rs/aws-sdk-dynamodb/latest/aws_sdk_dynamodb/operation/batch_write_item/enum.BatchWriteItemError.html
        //
        // If it is retryable, all requested items are queued again for retry.
        // If it is not retryable, all requested items are marked as failed
        // so that the import can terminate instead of waiting for them forever.
        Err(SdkError::ServiceError(err)) => match err.err() {
            BatchWriteItemError::ProvisionedThroughputExceededException(_)
            | BatchWriteItemError::RequestLimitExceeded(_) => {
                warn!(
                    "BatchWriteItem got retryable error (queued to retry): {}\n{:?}",
                    err.err(),
                    err
                );
                // Capacity shortage: retry and report congestion.
                retry_all(true)
            }
            BatchWriteItemError::InternalServerError(_) => {
                warn!(
                    "BatchWriteItem got retryable error (queued to retry): {}\n{:?}",
                    err.err(),
                    err
                );
                // Retryable, but a server-side issue rather than a capacity shortage.
                retry_all(false)
            }
            _ => {
                // Non-retryable errors
                error!("BatchWriteItem got fatal error: {}\n{:?}", err.err(), err);
                error!("Request ID: {:?}", err.raw().request_id());
                fail_all()
            }
        },
        Err(
            err @ (SdkError::TimeoutError(_)
            | SdkError::DispatchFailure(_)
            | SdkError::ResponseError(_)),
        ) => {
            // We can assume that the network configuration is correct if requests
            // have been successful previously. In that case, transport-level errors are
            // retryable unless DynamoDB undergoes a significant service issue.
            if has_prior_success {
                warn!("BatchWriteItem got {:?} (queued to retry)", err);
                retry_all(false)
            } else {
                // If this is the first attempt, it might be a non-retryable error
                // caused by a configuration problem.
                error!(
                    "BatchWriteItem got {:?} (failed at the first request attempt)",
                    err
                );
                fail_all()
            }
        }
        Err(err) => {
            // Non-retryable errors.
            error!("BatchWriteItem got fatal error: {:?}", err);
            fail_all()
        }
    }
}

/// Streams the elements of a top-level JSON array from `reader` into `sink`
/// without materializing the whole array (the whole-file `Vec<JsonValue>`
/// deserialization was the OOM cause on multi-GB inputs). `sink` returns
/// `Break` to stop early — e.g. the pipeline is shutting down — and an early
/// stop is not an error. Content after the closing bracket is an error, for
/// parity with the previous `from_str::<Vec<JsonValue>>` behavior.
fn stream_json_array_items<R: std::io::Read>(
    reader: R,
    sink: &mut dyn FnMut(JsonValue) -> ControlFlow<()>,
) -> Result<(), batch::DyneinBatchError> {
    struct ArraySeed<'a> {
        sink: &'a mut dyn FnMut(JsonValue) -> ControlFlow<()>,
        stopped: &'a mut bool,
    }

    impl<'de> serde::de::Visitor<'de> for ArraySeed<'_> {
        type Value = ();

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(f, "a top-level JSON array of items")
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<(), A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            while let Some(v) = seq.next_element::<JsonValue>()? {
                if (self.sink)(v).is_break() {
                    *self.stopped = true;
                    return Err(serde::de::Error::custom(
                        "the pipeline stopped accepting items",
                    ));
                }
            }
            Ok(())
        }
    }

    impl<'de> serde::de::DeserializeSeed<'de> for ArraySeed<'_> {
        type Value = ();

        fn deserialize<D>(self, deserializer: D) -> Result<(), D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_seq(self)
        }
    }

    let mut stopped = false;
    let mut deserializer = Deserializer::from_reader(reader);
    let result = serde::de::DeserializeSeed::deserialize(
        ArraySeed {
            sink,
            stopped: &mut stopped,
        },
        &mut deserializer,
    );
    match result {
        Ok(()) => {
            deserializer.end()?;
            Ok(())
        }
        // The early stop travels through serde as an error; it is not one.
        Err(_) if stopped => Ok(()),
        Err(e) => Err(batch::DyneinBatchError::PraseJSON(e)),
    }
}

/// Streams whitespace-separated JSON documents from `reader` — JSON Lines
/// and the looser concatenated form the previous StreamDeserializer-based
/// implementation accepted. An invalid document aborts with an error
/// carrying its position: the old `filter_map(Result::ok)` looked like a
/// skip but actually stopped reading at the first error, silently losing
/// every document after it.
fn stream_jsonl_items<R: std::io::Read>(
    reader: R,
    sink: &mut dyn FnMut(JsonValue) -> ControlFlow<()>,
) -> Result<(), batch::DyneinBatchError> {
    for result in Deserializer::from_reader(reader).into_iter::<JsonValue>() {
        if sink(result?).is_break() {
            return Ok(());
        }
    }
    Ok(())
}

/// Streams CSV rows as put requests. The first non-empty line is the header;
/// empty lines are skipped (parity with the previous whole-file
/// implementation, which filtered them out anywhere in the file).
fn stream_csv_rows<R: std::io::BufRead>(
    reader: R,
    enable_set_inference: bool,
    sink: &mut dyn FnMut(WriteRequest) -> ControlFlow<()>,
) -> Result<(), batch::DyneinBatchError> {
    let mut lines = reader.lines();
    let header_line = loop {
        match lines.next() {
            Some(line) => {
                let line = line?;
                if !line.is_empty() {
                    break line;
                }
            }
            None => {
                return Err(batch::DyneinBatchError::InvalidInput(
                    "The CSV input has no header line".to_string(),
                ))
            }
        }
    };
    let headers: Vec<&str> = header_line.split(',').collect();

    for line in lines {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let request = batch::csv_row_to_request_item(&headers, &line, enable_set_inference)?;
        if sink(request).is_break() {
            return Ok(());
        }
    }
    Ok(())
}

/// Fills `items` up to `capacity` from the producer channel, waiting for
/// more input as long as the producer is alive. Returns `false` once the
/// producer has closed the channel.
///
/// Waiting for a full batch matters: an executor that consumes faster than
/// the producer feeds (e.g. the task-per-request candidate with ample
/// tokens) would otherwise turn every `recv_many` remainder into a partial
/// BatchWriteItem request — measured as 21.4 items/request and +17%
/// requests on DynamoDB Local (benchmark-plan.md §2.7). Waiting costs
/// nothing downstream because the token bucket paces requests anyway.
/// Cancel-safe: items received before cancellation stay in `items`.
async fn fill_to_capacity(
    rx: &mut tokio::sync::mpsc::Receiver<WriteRequest>,
    items: &mut Vec<WriteRequest>,
    capacity: usize,
) -> bool {
    while items.len() < capacity {
        if rx.recv_many(items, capacity - items.len()).await == 0 {
            return false;
        }
    }
    true
}

/// Streams write requests produced by `source` into the throttled write
/// pipeline. `source` runs on a blocking thread (file I/O) and pushes items
/// through the sink it is given; the sink blocks on admission control and
/// returns `Break` when the pipeline stops accepting items (abort), which
/// the source must propagate by returning promptly.
async fn stream_writes_with_chucked(
    cx: &app::Context,
    source: impl FnOnce(
            &mut dyn FnMut(WriteRequest) -> ControlFlow<()>,
        ) -> Result<(), batch::DyneinBatchError>
        + Send
        + 'static,
    max_wcu: f64,
    hints: CapacityHints,
) -> Result<(), batch::DyneinBatchError> {
    let progress_status = Arc::new(std::sync::Mutex::new(ProgressState::new(
        MAX_NUMBER_OF_OBSERVES,
    )));
    let complete_items_count = Arc::new(AtomicUsize::new(0));
    let failed_items_count = Arc::new(AtomicUsize::new(0));

    // Admission control (see ADMISSION_ITEM_CAP): one permit per item inside
    // the pipeline. Permits are forgotten on acquisition and re-added when
    // items resolve, so the semaphore counts the resident population.
    let admission = Arc::new(tokio::sync::Semaphore::new(ADMISSION_ITEM_CAP));
    // Items admitted so far. Grows while the producer runs; final once
    // `producer_done` is set (store-Release / load-Acquire pairing).
    let total_queued_count = Arc::new(AtomicUsize::new(0));
    let producer_done = Arc::new(AtomicBool::new(false));

    // This channel is used to terminate chunking process.
    // Turning value into true indicates terminating signal.
    let (terminate_tx, mut terminate_rx) = tokio::sync::watch::channel::<bool>(false);

    // This channel is used to queue each write request to a table.
    // Retryable individual items are queued into this queue.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<WriteRequest>(BATCH_WRITE_BUFFER_SIZE);

    // Setup DynamoDB Client for BatchWriteItem
    let retry_config = cx
        .retry
        .as_ref()
        .map(|v| v.batch_write_item.as_ref().unwrap_or(&v.default));
    let config = cx
        .effective_sdk_config_with_retry(retry_config.cloned())
        .await;
    let ddb = DynamoDbSdkClient::new(&config);

    #[derive(Clone, Debug)]
    struct BatchWriteProcess {
        write_items: HashMap<String, Vec<WriteRequest>>,
        ddb: DynamoDbSdkClient,
        tx_retry: tokio::sync::mpsc::UnboundedSender<WriteRequest>,
        progress_status: Arc<std::sync::Mutex<ProgressState>>,
        complete_items_count: Arc<AtomicUsize>,
        failed_items_count: Arc<AtomicUsize>,
        admission: Arc<tokio::sync::Semaphore>,
    }

    impl algo::worker::ResourceConstraintProcess for BatchWriteProcess {
        fn estimate_resource(&self) -> f64 {
            let mut total_estimate = 0.0;
            for reqs in self.write_items.values() {
                for req in reqs {
                    if let Some(ref put_req) = req.put_request {
                        let bytes = crate::ddb::item::calculate_estimated_item_size(&put_req.item)
                            .expect("An invalid attribute is detected");
                        total_estimate += f64::ceil(bytes as f64 / 1024.0);
                    }
                    if req.delete_request.is_some() {
                        // We cannot estimate exact consumed WCU because consumed WCU is based on the size of an item.
                        // See: https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/read-write-operations.html#write-operation-consumption
                        // Therefore, we always estimate it as 1 WCR/WRU.
                        // We will revisit based on customer feedbacks.
                        // Please note that this behavior is acceptable because we have feedback mechanism.
                        // A duration to the next request will be adjusted based on the actual consumption.
                        total_estimate += 1.0;
                    }
                }
            }
            total_estimate
        }

        async fn process_and_consume_resource(&self) -> algo::worker::ProcessResult {
            let result = self
                .ddb
                .batch_write_item()
                .set_request_items(Some(self.write_items.clone()))
                .set_return_consumed_capacity(Some(ReturnConsumedCapacity::Total))
                .send()
                .await;

            let has_prior_success = self.complete_items_count.load(Ordering::Relaxed) > 0;
            let summary =
                summarize_batch_write_result(result, &self.write_items, has_prior_success);

            // Queue items for later retry. The retry channel is unbounded so this never
            // blocks; blocking here can deadlock the whole pipeline because the chunking
            // process may be waiting for this worker at the same time.
            for write_request in summary.retry_requests {
                if self.tx_retry.send(write_request).is_err() {
                    // The chunking process has already gone. This only happens while
                    // the pipeline is aborting (e.g. stall detection), so dropping the
                    // item is fine: the import is going to report an error anyway.
                    warn!("Dropped a retry item because the pipeline is shutting down");
                }
            }

            // Update progress
            if summary.successful_items > 0 {
                debug!("successful_writes: {}", summary.successful_items);
                self.complete_items_count
                    .fetch_add(summary.successful_items, Ordering::Relaxed);
                let mut progress_status = self.progress_status.lock().unwrap();
                progress_status.add_observation(summary.successful_items);
            }

            // Items failed with non-retryable errors are also accounted so that
            // the whole import can terminate (reporting an error at the end).
            if summary.failed_items > 0 {
                self.failed_items_count
                    .fetch_add(summary.failed_items, Ordering::Relaxed);
            }

            // Return the admission permits of resolved items. Items queued
            // for retry keep theirs — they are still inside the pipeline.
            // The item accounting invariant (each item resolves exactly once)
            // guarantees permits are returned exactly once per item.
            let resolved = summary.successful_items + summary.failed_items;
            if resolved > 0 {
                self.admission.add_permits(resolved);
            }

            algo::worker::ProcessResult {
                consumed: summary.consumed_capacity,
                throttled: summary.throttled,
            }
        }
    }

    // This channel is used to queue each a BatchWriteItem request.
    let (tx2, rx2) = tokio::sync::mpsc::channel::<BatchWriteProcess>(16);
    // The executor architecture is selectable via DYNEIN_BENCH_EXECUTOR while
    // the benchmark of docs/design/benchmark-plan.md is being settled.
    let executor_kind = algo::executor::ExecutorKind::from_env();
    info!("Using executor architecture {:?}", executor_kind);
    let mut executor =
        algo::executor::AnyExecutor::new(executor_kind, rx2, max_wcu, hints.initial_wcu);

    // Start the slow control loop when a capacity reference is known. It
    // consults CloudWatch to recover more aggressively when it looks safe.
    let slow_loop_handle = if let Some(table_capacity) = hints.provisioned_wcu {
        let config = cx.effective_sdk_config().await;
        let cw = aws_sdk_cloudwatch::Client::new(&config);
        Some(tokio::spawn(slow_control_loop(
            cw,
            cx.effective_table_name(),
            table_capacity,
            executor.congestion_stats(),
            executor.boost_slot(),
            terminate_rx.clone(),
        )))
    } else {
        None
    };

    // Benchmark instrumentation (no-op unless DYNEIN_BENCH_STATS is set).
    let bench_stats_handle = std::env::var(BENCH_STATS_ENV)
        .ok()
        .filter(|p| !p.is_empty())
        .map(|path| {
            spawn_bench_stats_emitter(
                path,
                executor.congestion_stats(),
                executor.target_gauge(),
                executor.task_monitor(),
                complete_items_count.clone(),
                failed_items_count.clone(),
                terminate_rx.clone(),
            )
        });

    // This channel is used to retry unprocessed items. It must be unbounded to avoid
    // a deadlock: workers enqueue retries while the chunking process may be blocked
    // on sending work to those same workers. The number of queued retries is bounded
    // by the total number of items anyway.
    let (tx3, mut rx3) = tokio::sync::mpsc::unbounded_channel::<WriteRequest>();

    let cx = cx.clone();
    let status = progress_status.clone();
    let count = complete_items_count.clone();
    let failed = failed_items_count.clone();
    let admission_for_chunker = admission.clone();
    let chunking_handle = tokio::spawn(async move {
        let mut items = Vec::with_capacity(25);
        // Becomes false once all input items have been queued and the producer closed
        // the channel. After that, only the retry channel can deliver items.
        let mut producer_open = true;
        loop {
            // Unprocessed items must be handled first to avoid buffer congestion for retry
            while items.len() < 25 {
                if let Ok(item) = rx3.try_recv() {
                    items.push(item);
                } else {
                    break;
                }
            }

            if items.len() < 25 && producer_open {
                // Fill the rest of the batch with items that are read from
                // files, waiting until the batch is full (or the producer is
                // done) so that a fast executor cannot force partial batches.
                select! {
                    open = fill_to_capacity(&mut rx, &mut items, 25) => {
                        // false: the producer has queued all input items and
                        // dropped the sender.
                        producer_open = open;
                    }
                    _ = terminate_rx.changed() => {
                        // This cancel is safe because a termination signal would send after all items were processed.
                        info!("chunking process has been terminated");
                        break;
                    }
                }
            } else if items.is_empty() {
                // The producer is done and no retry is queued at this moment.
                // Wait for a next retry item or the termination signal.
                select! {
                    received = rx3.recv() => {
                        match received {
                            Some(item) => items.push(item),
                            None => break,
                        }
                    }
                    _ = terminate_rx.changed() => {
                        // This cancel is safe because a termination signal would send after all items were processed.
                        info!("chunking process has been terminated");
                        break;
                    }
                }
                // Give queued retries a chance to fill the batch before sending it.
                continue;
            }

            if items.is_empty() {
                continue;
            }

            // Send batch execution
            debug!("{} items are chunked", items.len());
            let request_items = HashMap::from([(cx.effective_table_name(), items)]);
            tx2.send(BatchWriteProcess {
                write_items: request_items,
                ddb: ddb.clone(),
                tx_retry: tx3.clone(),
                progress_status: status.clone(),
                complete_items_count: count.clone(),
                failed_items_count: failed.clone(),
                admission: admission_for_chunker.clone(),
            })
            .await
            .expect("Failed to pass items to write");
            items = Vec::with_capacity(25);
        }
    });

    // Start the background process to show current progress
    let status = progress_status.clone();
    let visualize_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(VISUALIZE_INTERVAL).await;
            {
                let status = status.lock().unwrap();
                status.show();
            }
        }
    });

    // Start executor
    let executor_handle = tokio::spawn(async move { executor.run().await });

    // Run the producer on a blocking thread: it streams items out of the
    // input file and admits them into the pipeline one permit at a time.
    // It finishes long before the import does only when the input is small;
    // in general it runs for most of the import, paced by admission control.
    let producer_admission = admission.clone();
    let producer_total = total_queued_count.clone();
    let producer_done_flag = producer_done.clone();
    let runtime = tokio::runtime::Handle::current();
    let producer_handle = tokio::task::spawn_blocking(move || {
        let result = source(&mut |write_request: WriteRequest| {
            trace!("Send write_request to queue: {:?}", write_request);
            // Wait for an admission permit. The permit is carried by the
            // item through the pipeline (including the retry loop) and is
            // re-added by the worker when the item resolves. A closed
            // semaphore means the pipeline aborted: stop producing.
            match runtime.block_on(producer_admission.acquire()) {
                Ok(permit) => permit.forget(),
                Err(_) => return ControlFlow::Break(()),
            }
            // Count before sending: the monitor acts on this total only
            // after `producer_done`, and an overcount can happen only on
            // the abort path below, where termination no longer relies on
            // the count.
            producer_total.fetch_add(1, Ordering::Relaxed);
            // A send error means the chunker is gone (abort in progress).
            match tx.blocking_send(write_request) {
                Ok(()) => ControlFlow::Continue(()),
                Err(_) => ControlFlow::Break(()),
            }
        });
        info!("Queued all items");
        // Release ordering pairs with the monitor's Acquire load: once the
        // monitor observes producer_done == true, the total is final.
        producer_done_flag.store(true, Ordering::Release);
        // Returning drops `tx`, letting the chunker see the end of input.
        result
    });

    // Start monitoring the end of the chunking process.
    // The process terminates when the producer has admitted everything and
    // every admitted item is accounted for: either successfully written or
    // permanently failed. Items queued for retry belong to neither yet.
    // As a safety valve against livelocks, it also aborts the pipeline when no
    // progress has been made for STALL_DEADLINE.
    let complete_count_for_monitor = complete_items_count.clone();
    let failed_count_for_monitor = failed_items_count.clone();
    let total_count_for_monitor = total_queued_count.clone();
    let producer_done_for_monitor = producer_done.clone();
    let admission_for_monitor = admission.clone();
    let stalled = Arc::new(AtomicBool::new(false));
    let stalled_for_monitor = stalled.clone();
    let monitoring_handle = tokio::spawn(async move {
        let mut stall_detector = StallDetector::new(STALL_DEADLINE, Instant::now());
        loop {
            let complete_items_count = complete_count_for_monitor.load(Ordering::Relaxed);
            let failed_items_count = failed_count_for_monitor.load(Ordering::Relaxed);
            let producer_done = producer_done_for_monitor.load(Ordering::Acquire);
            let total_items_count = total_count_for_monitor.load(Ordering::Relaxed);
            debug!(
                "complete_items: {}/{}{} (failed_items: {})",
                complete_items_count,
                total_items_count,
                if producer_done { "" } else { "+" },
                failed_items_count
            );
            let resolved_items = complete_items_count + failed_items_count;
            if producer_done && total_items_count == resolved_items {
                terminate_tx
                    .send(true)
                    .expect("Failed to terminate the chunking process");
                break;
            }
            if stall_detector.observe(resolved_items, Instant::now()) {
                error!(
                    "No progress has been made for {:?}; aborting the import",
                    STALL_DEADLINE
                );
                stalled_for_monitor.store(true, Ordering::Relaxed);
                // Unblock a producer waiting on admission; its next sink
                // call then returns Break and the producer winds down.
                admission_for_monitor.close();
                terminate_tx
                    .send(true)
                    .expect("Failed to terminate the chunking process");
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    // Wait the termination of the process
    executor_handle
        .await
        .expect("Failed to successfully exit executors")
        .expect("Failed to wait all workers completions");
    chunking_handle
        .await
        .expect("Failed to wait the chunking process");
    monitoring_handle
        .await
        .expect("Failed to wait the monitoring process");
    // A producer error (unreadable or invalid input) surfaces after the
    // already-admitted items have drained through the pipeline above.
    let producer_result = producer_handle
        .await
        .expect("Failed to wait the producer process");
    if let Some(handle) = slow_loop_handle {
        // The slow control loop exits on the termination signal sent above.
        handle.await.expect("Failed to wait the slow control loop");
    }
    if let Some(handle) = bench_stats_handle {
        // The stats emitter exits on the termination signal sent above.
        handle
            .await
            .expect("Failed to wait the bench stats emitter");
    }

    // Stop visualization task
    visualize_handle.abort();
    assert!(visualize_handle
        .await
        .expect_err("Visualization task should be canceled.")
        .is_cancelled());

    // Update to latest status
    progress_status
        .lock()
        .expect("Failed to show final progress")
        .show();

    // The stall abort takes precedence: items neither written nor failed remain.
    if stalled.load(Ordering::Relaxed) {
        let resolved_items = complete_items_count.load(Ordering::Relaxed)
            + failed_items_count.load(Ordering::Relaxed);
        return Err(batch::DyneinBatchError::ProgressStalled(
            resolved_items,
            total_queued_count.load(Ordering::Relaxed),
        ));
    }

    // Next, a producer-side read/parse error: the admitted prefix of the
    // input has drained, but the rest was never read.
    producer_result?;

    // Report items failed with non-retryable errors as an error of the whole import.
    let failed_items = failed_items_count.load(Ordering::Relaxed);
    if failed_items > 0 {
        return Err(batch::DyneinBatchError::PermanentWriteFailure(failed_items));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_dynamodb::operation::batch_write_item::BatchWriteItemOutput;
    use aws_sdk_dynamodb::types::error::{
        ProvisionedThroughputExceededException, ResourceNotFoundException,
    };
    use aws_sdk_dynamodb::types::{ConsumedCapacity, PutRequest};
    use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
    use aws_smithy_runtime_api::http::StatusCode;
    use aws_smithy_types::body::SdkBody;
    use std::convert::TryFrom;
    use std::ops::Add;
    use std::time::Duration;

    fn put_req(pk: &str) -> WriteRequest {
        WriteRequest::builder()
            .put_request(
                PutRequest::builder()
                    .item("pk", AttributeValue::S(pk.to_string()))
                    .build()
                    .unwrap(),
            )
            .build()
    }

    fn requested_items(pks: &[&str]) -> HashMap<String, Vec<WriteRequest>> {
        HashMap::from([(
            "test-table".to_string(),
            pks.iter().map(|pk| put_req(pk)).collect(),
        )])
    }

    fn raw_http_response() -> HttpResponse {
        HttpResponse::new(StatusCode::try_from(400).unwrap(), SdkBody::from("{}"))
    }

    fn timeout_error() -> SdkError<BatchWriteItemError, HttpResponse> {
        SdkError::timeout_error("request timed out")
    }

    #[test]
    fn test_summarize_all_items_succeeded() {
        let requested = requested_items(&["pk1", "pk2", "pk3"]);
        let output = BatchWriteItemOutput::builder()
            .consumed_capacity(ConsumedCapacity::builder().capacity_units(5.0).build())
            .build();

        let summary = summarize_batch_write_result(Ok(output), &requested, false);

        assert_eq!(summary.successful_items, 3);
        assert_eq!(summary.failed_items, 0);
        assert!(summary.retry_requests.is_empty());
        assert_eq!(summary.consumed_capacity, 5.0);
        assert!(!summary.throttled);
    }

    #[test]
    fn test_summarize_unprocessed_items_are_retried() {
        let requested = requested_items(&["pk1", "pk2", "pk3"]);
        let unprocessed = HashMap::from([(
            "test-table".to_string(),
            vec![put_req("pk2"), put_req("pk3")],
        )]);
        let output = BatchWriteItemOutput::builder()
            .set_unprocessed_items(Some(unprocessed))
            .consumed_capacity(ConsumedCapacity::builder().capacity_units(1.0).build())
            .build();

        let summary = summarize_batch_write_result(Ok(output), &requested, false);

        // Two items out of three are unprocessed. They must be retried, not counted as complete.
        assert_eq!(summary.successful_items, 1);
        assert_eq!(summary.failed_items, 0);
        assert_eq!(summary.retry_requests.len(), 2);
        assert_eq!(summary.consumed_capacity, 1.0);
    }

    #[test]
    fn test_summarize_any_unprocessed_item_signals_congestion() {
        let requested = requested_items(&["pk1", "pk2", "pk3"]);
        let unprocessed = HashMap::from([("test-table".to_string(), vec![put_req("pk3")])]);
        let output = BatchWriteItemOutput::builder()
            .set_unprocessed_items(Some(unprocessed))
            .build();

        let summary = summarize_batch_write_result(Ok(output), &requested, false);

        // Every unprocessed item is a server-side rejection due to a capacity
        // shortage. To protect co-located production workloads, even a single
        // one counts as a congestion signal.
        assert!(summary.throttled);
        assert_eq!(summary.retry_requests.len(), 1);
    }

    #[test]
    fn test_stall_detector() {
        let deadline = Duration::from_secs(10);
        let now = Instant::now();
        let mut detector = StallDetector::new(deadline, now);

        // Progress is advancing: never stalled.
        assert!(!detector.observe(10, now.add(Duration::from_secs(9))));
        assert!(!detector.observe(20, now.add(Duration::from_secs(18))));

        // No progress, but the deadline has not passed since the last advance.
        assert!(!detector.observe(20, now.add(Duration::from_secs(27))));

        // No progress for the full deadline period since the last advance (t=18).
        assert!(detector.observe(20, now.add(Duration::from_secs(28))));

        // Progress resumes: the clock resets.
        assert!(!detector.observe(21, now.add(Duration::from_secs(29))));
        assert!(!detector.observe(21, now.add(Duration::from_secs(38))));
        assert!(detector.observe(21, now.add(Duration::from_secs(39))));
    }

    #[test]
    fn test_summarize_throttled_request_requeues_all_items() {
        let requested = requested_items(&["pk1", "pk2", "pk3"]);
        let err = SdkError::service_error(
            BatchWriteItemError::ProvisionedThroughputExceededException(
                ProvisionedThroughputExceededException::builder().build(),
            ),
            raw_http_response(),
        );

        // The whole request was throttled. All items must be queued for retry
        // regardless of prior successes; losing them here makes the import wait
        // forever for completions that never come.
        let summary = summarize_batch_write_result(Err(err), &requested, false);

        assert_eq!(summary.successful_items, 0);
        assert_eq!(summary.failed_items, 0);
        assert_eq!(summary.retry_requests.len(), 3);
        assert_eq!(summary.consumed_capacity, 0.0);
        assert!(summary.throttled);
    }

    #[test]
    fn test_summarize_fatal_service_error_marks_items_failed() {
        let requested = requested_items(&["pk1", "pk2"]);
        let err = SdkError::service_error(
            BatchWriteItemError::ResourceNotFoundException(
                ResourceNotFoundException::builder().build(),
            ),
            raw_http_response(),
        );

        let summary = summarize_batch_write_result(Err(err), &requested, true);

        // Non-retryable: nothing to retry, but items must still be accounted as failed
        // so that the import can terminate (with an error) instead of hanging.
        assert_eq!(summary.successful_items, 0);
        assert_eq!(summary.failed_items, 2);
        assert!(summary.retry_requests.is_empty());
    }

    #[test]
    fn test_summarize_transport_error_after_success_is_retried() {
        let requested = requested_items(&["pk1", "pk2"]);

        // The network configuration is proven to work by previous successes,
        // so a transport-level error is considered transient and retryable.
        let summary = summarize_batch_write_result(Err(timeout_error()), &requested, true);

        assert_eq!(summary.successful_items, 0);
        assert_eq!(summary.failed_items, 0);
        assert_eq!(summary.retry_requests.len(), 2);
    }

    #[test]
    fn test_summarize_transport_error_at_first_attempt_marks_items_failed() {
        let requested = requested_items(&["pk1", "pk2"]);

        // A transport-level error on the very first attempt likely indicates a
        // configuration problem. Mark items as failed so the import terminates.
        let summary = summarize_batch_write_result(Err(timeout_error()), &requested, false);

        assert_eq!(summary.successful_items, 0);
        assert_eq!(summary.failed_items, 2);
        assert!(summary.retry_requests.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn test_fill_to_capacity_waits_for_a_full_batch() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<WriteRequest>(64);
        let mut items = Vec::new();

        // 10 items available now, 20 more arriving later: the fill must wait
        // for the producer instead of returning a partial batch.
        for i in 0..10 {
            tx.send(put_req(&format!("now{}", i))).await.unwrap();
        }
        let sender = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            for i in 0..20 {
                tx.send(put_req(&format!("later{}", i))).await.unwrap();
            }
            tx // keep the channel open
        });

        let open = fill_to_capacity(&mut rx, &mut items, 25).await;
        assert!(open);
        assert_eq!(items.len(), 25);

        // The producer closes with 5 items left: the fill returns the rest
        // as a partial batch and reports the closed channel.
        let tx = sender.await.unwrap();
        drop(tx);
        let mut rest = Vec::new();
        let open = fill_to_capacity(&mut rx, &mut rest, 25).await;
        assert!(!open);
        assert_eq!(rest.len(), 5);

        // A closed, drained channel keeps reporting closed without items.
        let mut empty = Vec::new();
        let open = fill_to_capacity(&mut rx, &mut empty, 25).await;
        assert!(!open);
        assert!(empty.is_empty());
    }

    #[test]
    fn test_render_bench_stats_line_without_task_metrics() {
        let line = render_bench_stats_line(1.5, 12.25, 10, 2, 80.0, 250, 1, None);
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["t"], 1.5);
        assert_eq!(v["consumed_wcu"], 12.25);
        assert_eq!(v["requests"], 10);
        assert_eq!(v["throttled"], 2);
        assert_eq!(v["effective_target"], 80.0);
        assert_eq!(v["resolved_items"], 250);
        assert_eq!(v["failed_items"], 1);
        assert!(v.get("tokio").is_none());
        // One JSON object per line: the rendered line must not contain newlines.
        assert!(!line.contains('\n'));
    }

    #[tokio::test]
    async fn test_render_bench_stats_line_with_task_metrics() {
        let monitor = tokio_metrics::TaskMonitor::new();
        monitor.instrument(async {}).await;
        let metrics = monitor.cumulative();

        let line = render_bench_stats_line(2.0, 0.0, 0, 0, 10.0, 0, 0, Some(&metrics));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["tokio"]["instrumented_count"], 1);
        assert!(v["tokio"]["total_poll_count"].is_u64());
        assert!(v["tokio"]["total_poll_duration_us"].is_u64());
        assert!(v["tokio"]["total_scheduled_duration_us"].is_u64());
        assert!(v["tokio"]["total_slow_poll_count"].is_u64());
    }

    #[test]
    fn test_warm_write_units_reads_describe_table() {
        let desc = aws_sdk_dynamodb::types::TableDescription::builder()
            .warm_throughput(
                aws_sdk_dynamodb::types::TableWarmThroughputDescription::builder()
                    .read_units_per_second(12_000)
                    .write_units_per_second(4_000)
                    .build(),
            )
            .build();
        assert_eq!(warm_write_units(&desc), Some(4_000.0));
    }

    #[test]
    fn test_warm_write_units_absent_when_not_reported() {
        let desc = aws_sdk_dynamodb::types::TableDescription::builder().build();
        assert_eq!(warm_write_units(&desc), None);

        // warm_throughput present but without a write value.
        let desc = aws_sdk_dynamodb::types::TableDescription::builder()
            .warm_throughput(
                aws_sdk_dynamodb::types::TableWarmThroughputDescription::builder().build(),
            )
            .build();
        assert_eq!(warm_write_units(&desc), None);
    }

    #[test]
    fn test_resolve_target_ceiling_uses_user_value_as_is() {
        let hints = CapacityHints {
            initial_wcu: Some(80.0),
            provisioned_wcu: Some(100.0),
        };
        let (ceiling, resolved) = resolve_target_ceiling(Some(500.0), hints);
        assert_eq!(ceiling, 500.0);
        assert_eq!(resolved.initial_wcu, Some(80.0));
        assert_eq!(resolved.provisioned_wcu, Some(100.0));
    }

    #[test]
    fn test_resolve_target_ceiling_is_unbounded_by_default() {
        // Without --max-wcu, pacing is left to the congestion control alone.
        let hints = CapacityHints {
            initial_wcu: Some(80.0),
            provisioned_wcu: Some(100.0),
        };
        let (ceiling, resolved) = resolve_target_ceiling(None, hints);
        assert_eq!(ceiling, f64::INFINITY);
        assert_eq!(resolved.initial_wcu, Some(80.0));
    }

    #[test]
    fn test_resolve_target_ceiling_unbounded_backfills_missing_initial() {
        // An unbounded ceiling with no capacity hint would start the
        // controller at infinity (which can never back off), so a missing
        // hint is backfilled with the conservative warm default.
        let hints = CapacityHints {
            initial_wcu: None,
            provisioned_wcu: None,
        };
        let (ceiling, resolved) = resolve_target_ceiling(None, hints);
        assert_eq!(ceiling, f64::INFINITY);
        assert_eq!(resolved.initial_wcu, Some(DEFAULT_ON_DEMAND_WARM_WCU));
    }

    #[test]
    fn test_resolve_target_ceiling_bounded_keeps_missing_initial() {
        // With an explicit ceiling the executor may start at it, as before.
        let hints = CapacityHints {
            initial_wcu: None,
            provisioned_wcu: None,
        };
        let (ceiling, resolved) = resolve_target_ceiling(Some(500.0), hints);
        assert_eq!(ceiling, 500.0);
        assert_eq!(resolved.initial_wcu, None);
    }

    fn collect_json_values(
        result_sink: &mut Vec<JsonValue>,
    ) -> impl FnMut(JsonValue) -> std::ops::ControlFlow<()> + '_ {
        move |v| {
            result_sink.push(v);
            std::ops::ControlFlow::Continue(())
        }
    }

    #[test]
    fn test_stream_json_array_items_streams_elements() {
        let input = r#"[{"a": 1}, {"b": 2}]"#;
        let mut seen = vec![];
        let result = stream_json_array_items(input.as_bytes(), &mut collect_json_values(&mut seen));
        assert!(result.is_ok());
        assert_eq!(
            seen,
            vec![serde_json::json!({"a": 1}), serde_json::json!({"b": 2})]
        );
    }

    #[test]
    fn test_stream_json_array_items_accepts_empty_array() {
        let mut seen = vec![];
        let result = stream_json_array_items("[]".as_bytes(), &mut collect_json_values(&mut seen));
        assert!(result.is_ok());
        assert!(seen.is_empty());
    }

    #[test]
    fn test_stream_json_array_items_rejects_non_array() {
        let mut seen = vec![];
        let result = stream_json_array_items(
            r#"{"a": 1}"#.as_bytes(),
            &mut collect_json_values(&mut seen),
        );
        assert!(result.is_err());
        assert!(seen.is_empty());
    }

    #[test]
    fn test_stream_json_array_items_propagates_element_error() {
        // The first element must already have been delivered when the error
        // on the second element surfaces: parsing is incremental.
        let input = r#"[{"a": 1}, oops]"#;
        let mut seen = vec![];
        let result = stream_json_array_items(input.as_bytes(), &mut collect_json_values(&mut seen));
        assert!(result.is_err());
        assert_eq!(seen, vec![serde_json::json!({"a": 1})]);
    }

    #[test]
    fn test_stream_json_array_items_rejects_trailing_garbage() {
        let input = r#"[{"a": 1}] x"#;
        let mut seen = vec![];
        let result = stream_json_array_items(input.as_bytes(), &mut collect_json_values(&mut seen));
        assert!(result.is_err());
    }

    #[test]
    fn test_stream_json_array_items_early_stop_is_not_an_error() {
        let input = r#"[{"a": 1}, {"b": 2}, {"c": 3}]"#;
        let mut seen = vec![];
        let result = stream_json_array_items(input.as_bytes(), &mut |v| {
            seen.push(v);
            std::ops::ControlFlow::Break(())
        });
        assert!(result.is_ok());
        assert_eq!(seen, vec![serde_json::json!({"a": 1})]);
    }

    #[test]
    fn test_stream_jsonl_items_streams_documents() {
        let input = "{\"a\": 1}\n{\"b\": 2}\n";
        let mut seen = vec![];
        let result = stream_jsonl_items(input.as_bytes(), &mut collect_json_values(&mut seen));
        assert!(result.is_ok());
        assert_eq!(
            seen,
            vec![serde_json::json!({"a": 1}), serde_json::json!({"b": 2})]
        );
    }

    #[test]
    fn test_stream_jsonl_items_accepts_documents_spanning_lines() {
        // Parity with the previous StreamDeserializer-based implementation:
        // whitespace-separated documents are accepted even across lines.
        let input = "{\n  \"a\": 1\n}\n{\"b\": 2}";
        let mut seen = vec![];
        let result = stream_jsonl_items(input.as_bytes(), &mut collect_json_values(&mut seen));
        assert!(result.is_ok());
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn test_stream_jsonl_items_accepts_empty_input() {
        let mut seen = vec![];
        let result = stream_jsonl_items("".as_bytes(), &mut collect_json_values(&mut seen));
        assert!(result.is_ok());
        assert!(seen.is_empty());
    }

    #[test]
    fn test_stream_jsonl_items_fails_on_invalid_document() {
        // An invalid document aborts the import instead of silently losing
        // the rest of the file (the old filter_map(Result::ok) behavior).
        let input = "{\"a\": 1}\nnot json\n{\"b\": 2}\n";
        let mut seen = vec![];
        let result = stream_jsonl_items(input.as_bytes(), &mut collect_json_values(&mut seen));
        assert!(result.is_err());
        assert_eq!(seen, vec![serde_json::json!({"a": 1})]);
    }

    #[test]
    fn test_stream_jsonl_items_early_stop_is_not_an_error() {
        let input = "{\"a\": 1}\n{\"b\": 2}\n";
        let mut seen = vec![];
        let result = stream_jsonl_items(input.as_bytes(), &mut |v| {
            seen.push(v);
            std::ops::ControlFlow::Break(())
        });
        assert!(result.is_ok());
        assert_eq!(seen.len(), 1);
    }

    fn put_item_attr(req: &WriteRequest, attr: &str) -> AttributeValue {
        req.put_request()
            .expect("should be a put request")
            .item()
            .get(attr)
            .expect("attribute should exist")
            .clone()
    }

    #[test]
    fn test_stream_csv_rows_streams_rows() {
        let input = "pk,sk\n\"pk1\",1\n\"pk2\",2\n";
        let mut seen: Vec<WriteRequest> = vec![];
        let result = stream_csv_rows(input.as_bytes(), false, &mut |req| {
            seen.push(req);
            std::ops::ControlFlow::Continue(())
        });
        assert!(result.is_ok());
        assert_eq!(seen.len(), 2);
        assert_eq!(
            put_item_attr(&seen[0], "pk"),
            AttributeValue::S("pk1".to_string())
        );
        assert_eq!(
            put_item_attr(&seen[0], "sk"),
            AttributeValue::N("1".to_string())
        );
        assert_eq!(
            put_item_attr(&seen[1], "pk"),
            AttributeValue::S("pk2".to_string())
        );
    }

    #[test]
    fn test_stream_csv_rows_skips_empty_lines() {
        let input = "pk,sk\n\"pk1\",1\n\n\"pk2\",2\n\n";
        let mut seen: Vec<WriteRequest> = vec![];
        let result = stream_csv_rows(input.as_bytes(), false, &mut |req| {
            seen.push(req);
            std::ops::ControlFlow::Continue(())
        });
        assert!(result.is_ok());
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn test_stream_csv_rows_fails_on_cell_count_mismatch() {
        // The old implementation called process::exit(1); now the mismatch is
        // reported as a normal error so admitted items can drain first.
        let input = "pk,sk\n\"pk1\",1,42\n";
        let mut seen: Vec<WriteRequest> = vec![];
        let result = stream_csv_rows(input.as_bytes(), false, &mut |req| {
            seen.push(req);
            std::ops::ControlFlow::Continue(())
        });
        assert!(result.is_err());
        assert!(seen.is_empty());
    }

    #[test]
    fn test_stream_csv_rows_fails_on_empty_input() {
        let mut seen: Vec<WriteRequest> = vec![];
        let result = stream_csv_rows("".as_bytes(), false, &mut |req| {
            seen.push(req);
            std::ops::ControlFlow::Continue(())
        });
        assert!(result.is_err());
    }

    #[test]
    fn test_stream_csv_rows_early_stop_is_not_an_error() {
        let input = "pk,sk\n\"pk1\",1\n\"pk2\",2\n";
        let mut seen: Vec<WriteRequest> = vec![];
        let result = stream_csv_rows(input.as_bytes(), false, &mut |req| {
            seen.push(req);
            std::ops::ControlFlow::Break(())
        });
        assert!(result.is_ok());
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn test_progress_status() {
        let mut progress = ProgressState::new(2);

        let first_observation = Instant::now();
        progress.add_observation_with_time(10, first_observation);
        assert_eq!(progress.processed_items(), 10);
        assert_eq!(progress.recent_processed_items.len(), 1);
        assert!(progress
            .recent_average_processed_items_per_second_with_time(first_observation)
            .is_nan());

        let second_observation = first_observation.add(Duration::from_millis(500));
        progress.add_observation_with_time(10, second_observation);
        assert_eq!(progress.processed_items(), 20);
        assert_eq!(progress.recent_processed_items.len(), 2);
        assert_eq!(
            progress.recent_average_processed_items_per_second_with_time(second_observation),
            (10.0 + 10.0) / 0.5
        );

        let third_observation = first_observation.add(Duration::from_millis(1000));
        progress.add_observation_with_time(12, third_observation);
        assert_eq!(progress.processed_items(), 32);
        assert_eq!(progress.recent_processed_items.len(), 2);
        assert_eq!(
            progress.recent_average_processed_items_per_second_with_time(third_observation),
            (10.0 + 12.0) / 0.5
        );
    }
}
