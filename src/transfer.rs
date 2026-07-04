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
use serde_json::{de::StrRead, Deserializer, StreamDeserializer, Value as JsonValue};
use std::collections::VecDeque;
use std::fmt::Debug;
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

pub async fn import(
    cx: &app::Context,
    input_file: String,
    format: Option<String>,
    enable_set_inference: bool,
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

    info!("Start loading a file");
    let input_string: String = if Path::new(&input_file).exists() {
        fs::read_to_string(&input_file)?
    } else {
        error!("Couldn't find the input file '{}'.", &input_file);
        std::process::exit(1);
    };
    info!("Loaded a file");

    // Give the AIMD congestion control a realistic starting point derived from
    // known table information instead of the (high) user-specified ceiling.
    let initial_wcu = initial_target_wcu(cx, &ts).await;

    match format_str {
        None | Some("json") | Some("json-compact") => {
            info!("Start JSON conversion");
            let array_of_json_obj: Vec<JsonValue> = serde_json::from_str(&input_string)?;
            info!("End JSON conversion");
            // TODO: to change configurable
            stream_write_of_jsons_with_chunked(
                cx,
                array_of_json_obj.into_iter(),
                enable_set_inference,
                100_000.0,
                initial_wcu,
            )
            .await?;
        }
        Some("jsonl") => {
            // JSON Lines can be deserialized with into_iter() as below.
            let array_of_json_obj: StreamDeserializer<'_, StrRead<'_>, JsonValue> =
                Deserializer::from_str(&input_string).into_iter::<JsonValue>();
            // list_of_jsons contains deserialize results. Filter them and get only valid items.
            let array_of_valid_json_obj: Vec<JsonValue> =
                array_of_json_obj.filter_map(Result::ok).collect();
            stream_write_of_jsons_with_chunked(
                cx,
                array_of_valid_json_obj.into_iter(),
                enable_set_inference,
                100_000.0,
                initial_wcu,
            )
            .await?;
        }
        Some("csv") => {
            let lines: Vec<&str> = input_string
                .split('\n')
                .collect::<Vec<&str>>() // split by "\n" and get lines
                .into_iter()
                .filter(|&x| !x.is_empty())
                .collect::<Vec<&str>>(); // remove blank line (e.g. last line)
            let headers: Vec<&str> = lines[0].split(',').collect::<Vec<&str>>();
            let mut matrix: Vec<Vec<&str>> = vec![];
            // Iterate over lines (from index = 1, as index = 0 is the header line)
            for line in lines.iter().skip(1) {
                let cells: Vec<&str> = line.split(',').collect::<Vec<&str>>();
                debug!("splitted line => {:?}", cells);
                matrix.push(cells);
            }

            let request_items: Vec<WriteRequest> = batch::csv_matrix_to_request_items(
                matrix.as_slice(),
                headers.as_slice(),
                enable_set_inference,
            )
            .await?;
            stream_writes_with_chucked(cx, request_items.into_iter(), 100_000.0, initial_wcu)
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

/// Ratio applied to the provisioned WCU to decide the initial effective target.
/// Not starting at 100% leaves room for co-located production workloads from
/// the beginning; the AIMD recovery then probes for the remaining capacity.
const INITIAL_TARGET_RATIO_OF_PROVISIONED: f64 = 0.8;

/// The default warm throughput of on-demand tables (4,000 WCU).
/// The aws-sdk-dynamodb version in use does not expose the `warm_throughput`
/// field of DescribeTable yet; once the SDK is upgraded, read the actual value
/// from the table description instead of assuming the platform default.
const DEFAULT_ON_DEMAND_WARM_WCU: f64 = 4000.0;

/// Decides the initial effective WCU target for AIMD congestion control from
/// known table information: a ratio of the provisioned capacity, or the
/// default warm throughput for on-demand tables.
async fn initial_target_wcu(cx: &app::Context, ts: &app::TableSchema) -> Option<f64> {
    match ts.mode {
        table::Mode::Provisioned => {
            let desc = control::describe_table_api(cx, ts.name.clone()).await;
            desc.provisioned_throughput
                .and_then(|p| p.write_capacity_units)
                .map(|wcu| wcu as f64 * INITIAL_TARGET_RATIO_OF_PROVISIONED)
        }
        table::Mode::OnDemand => Some(DEFAULT_ON_DEMAND_WARM_WCU),
    }
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

async fn stream_write_of_jsons_with_chunked(
    cx: &app::Context,
    iter: impl Iterator<Item = JsonValue>,
    enable_set_inference: bool,
    max_wcu: f64,
    initial_wcu: Option<f64>,
) -> Result<(), batch::DyneinBatchError> {
    let iter = iter.map(|item| {
        let item = batch::convert_jsonval_to_hashmap(&item, enable_set_inference);
        batch::construct_put_write_request(item)
    });
    stream_writes_with_chucked(cx, iter.into_iter(), max_wcu, initial_wcu).await
}

async fn stream_writes_with_chucked(
    cx: &app::Context,
    iter: impl Iterator<Item = WriteRequest>,
    max_wcu: f64,
    initial_wcu: Option<f64>,
) -> Result<(), batch::DyneinBatchError> {
    let progress_status = Arc::new(std::sync::Mutex::new(ProgressState::new(
        MAX_NUMBER_OF_OBSERVES,
    )));
    let complete_items_count = Arc::new(AtomicUsize::new(0));
    let failed_items_count = Arc::new(AtomicUsize::new(0));

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

            algo::worker::ProcessResult {
                consumed: summary.consumed_capacity,
                throttled: summary.throttled,
            }
        }
    }

    // This channel is used to queue each a BatchWriteItem request.
    let (tx2, rx2) = tokio::sync::mpsc::channel::<BatchWriteProcess>(16);
    let mut executor = algo::worker::ThrottledExecutor::new(rx2, max_wcu, initial_wcu);

    // This channel is used to retry unprocessed items. It must be unbounded to avoid
    // a deadlock: workers enqueue retries while the chunking process may be blocked
    // on sending work to those same workers. The number of queued retries is bounded
    // by the total number of items anyway.
    let (tx3, mut rx3) = tokio::sync::mpsc::unbounded_channel::<WriteRequest>();

    let cx = cx.clone();
    let status = progress_status.clone();
    let count = complete_items_count.clone();
    let failed = failed_items_count.clone();
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
                // Try to fill the rest of the batch with items that are read from files
                let num_available_space = 25 - items.len();
                select! {
                    n = rx.recv_many(&mut items, num_available_space) => {
                        if n == 0 {
                            // The producer has queued all input items and dropped the sender.
                            producer_open = false;
                        }
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

    // Consume all data provided by
    let mut total_items_count: usize = 0;
    for write_request in iter {
        trace!("Send write_request to queue: {:?}", write_request);
        tx.send(write_request)
            .await
            .expect("Unexpected channel close.");
        total_items_count += 1;
    }
    info!("Queued all items");
    drop(tx);

    // Start monitoring the end of the chunking process.
    // The process terminates when every item is accounted for: either successfully
    // written or permanently failed. Items queued for retry belong to neither yet.
    // As a safety valve against livelocks, it also aborts the pipeline when no
    // progress has been made for STALL_DEADLINE.
    let complete_count_for_monitor = complete_items_count.clone();
    let failed_count_for_monitor = failed_items_count.clone();
    let stalled = Arc::new(AtomicBool::new(false));
    let stalled_for_monitor = stalled.clone();
    let monitoring_handle = tokio::spawn(async move {
        let mut stall_detector = StallDetector::new(STALL_DEADLINE, Instant::now());
        loop {
            let complete_items_count = complete_count_for_monitor.load(Ordering::Relaxed);
            let failed_items_count = failed_count_for_monitor.load(Ordering::Relaxed);
            debug!(
                "complete_items: {}/{} (failed_items: {})",
                complete_items_count, total_items_count, failed_items_count
            );
            let resolved_items = complete_items_count + failed_items_count;
            if total_items_count == resolved_items {
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
            total_items_count,
        ));
    }

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
