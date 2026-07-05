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

//! Deterministic in-process simulation of the executor candidates
//! (`docs/design/benchmark-plan.md` §2.5).
//!
//! Everything here runs under tokio virtual time
//! (`#[tokio::test(start_paused = true)]`): the algo layer uses
//! `tokio::time::Instant` throughout, so bucket refills and pacing run in
//! milliseconds of real time regardless of the simulated duration.
//!
//! The simulated "server" only adds latency and always consumes exactly the
//! estimate (phase 1: infinite capacity, never throttled). This deliberately
//! isolates the client-side scheduling questions; see the design document.

use crate::algo::worker::{ProcessResult, ResourceConstraintProcess};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

/// One completed simulated request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompletionRecord {
    pub finished_at: Instant,
    pub cost: f64,
}

/// Collects completion records from all [`SimProcess`]es of one scenario run.
#[derive(Debug, Clone, Default)]
pub struct SimRecorder(Arc<Mutex<Vec<CompletionRecord>>>);

impl SimRecorder {
    pub fn new() -> SimRecorder {
        SimRecorder::default()
    }

    /// Creates a simulated request with the given capacity cost and
    /// service latency, reporting its completion into this recorder.
    pub fn process(&self, cost: f64, latency: Duration) -> SimProcess {
        SimProcess {
            cost,
            latency,
            recorder: self.clone(),
        }
    }

    pub fn records(&self) -> Vec<CompletionRecord> {
        self.0.lock().unwrap().clone()
    }
}

/// A [`ResourceConstraintProcess`] whose "server" is pure latency: it always
/// consumes exactly its estimate and never reports throttling.
#[derive(Debug, Clone)]
pub struct SimProcess {
    cost: f64,
    latency: Duration,
    recorder: SimRecorder,
}

impl ResourceConstraintProcess for SimProcess {
    fn estimate_resource(&self) -> f64 {
        self.cost
    }

    fn process_and_consume_resource(&self) -> impl Future<Output = ProcessResult> + Send {
        let cost = self.cost;
        let latency = self.latency;
        let recorder = self.recorder.clone();
        async move {
            tokio::time::sleep(latency).await;
            recorder.0.lock().unwrap().push(CompletionRecord {
                finished_at: Instant::now(),
                cost,
            });
            ProcessResult {
                consumed: cost,
                throttled: false,
            }
        }
    }
}

/// Comparable outcome of one candidate on one scenario.
#[derive(Debug, Clone, Copy)]
pub struct ScenarioResult {
    /// Number of completed requests.
    pub completed: usize,
    /// Time from scenario start to the last completion, in seconds.
    pub makespan_secs: f64,
    /// Ideal time (Σcost ÷ target rate) divided by the makespan.
    pub utilization: f64,
    /// Completion tail: t(100%) − t(90%), in seconds.
    pub tail_secs: f64,
}

/// Computes the comparable metrics from the completion records of one run.
pub fn compute_scenario_result(
    records: &[CompletionRecord],
    start: Instant,
    target_rate: f64,
) -> ScenarioResult {
    let mut offsets: Vec<f64> = records
        .iter()
        .map(|r| r.finished_at.duration_since(start).as_secs_f64())
        .collect();
    offsets.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let makespan_secs = offsets.last().copied().unwrap_or(0.0);
    let total_cost: f64 = records.iter().map(|r| r.cost).sum();
    let ideal_secs = total_cost / target_rate;
    let utilization = if makespan_secs > 0.0 {
        ideal_secs / makespan_secs
    } else {
        0.0
    };
    // t(90%): the completion time of the smallest k with k ≥ 0.9 × n.
    let tail_secs = if offsets.is_empty() {
        0.0
    } else {
        let idx90 = (records.len() * 9).div_ceil(10).max(1) - 1;
        makespan_secs - offsets[idx90]
    };
    ScenarioResult {
        completed: records.len(),
        makespan_secs,
        utilization,
        tail_secs,
    }
}

#[cfg(test)]
mod scenarios {
    //! The four regime scenarios of benchmark-plan.md §2.5, run manually with
    //! `cargo test --bin dy sim_ -- --ignored --nocapture`.
    //!
    //! The comparative numbers are printed, not asserted: they are recorded
    //! as *predictions* for the EC2 runs (a falsified hypothesis must not
    //! break the build). Only completeness is asserted.

    use super::*;
    use crate::algo::executor::{AnyExecutor, ExecutorKind};
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use tokio::sync::mpsc::channel;

    /// One request: (capacity cost, service latency).
    type Spec = (f64, Duration);

    const CANDIDATES: [(&str, ExecutorKind); 4] = [
        ("A  pool16", ExecutorKind::Pool { queue_depth: 16 }),
        ("A' pool1", ExecutorKind::Pool { queue_depth: 1 }),
        ("B  mpmc", ExecutorKind::Mpmc),
        (
            "C  task",
            ExecutorKind::Task {
                max_in_flight: None,
            },
        ),
    ];

    /// Latency model: a 5–20ms base (connection/server time) plus a
    /// size-proportional transfer component. Deterministic per seed.
    fn latency_for(cost: f64, rng: &mut StdRng) -> Duration {
        Duration::from_secs_f64((rng.gen_range(5.0..20.0) + cost * 1.5) / 1000.0)
    }

    fn uniform_small_specs(n: usize, seed: u64) -> Vec<Spec> {
        let mut rng = StdRng::seed_from_u64(seed);
        (0..n).map(|_| (1.0, latency_for(1.0, &mut rng))).collect()
    }

    /// 90% small (1 WCU) and 10% large (20 WCU) items, randomly interleaved.
    fn mixed_specs(n: usize, seed: u64) -> Vec<Spec> {
        let mut rng = StdRng::seed_from_u64(seed);
        (0..n)
            .map(|_| {
                let cost = if rng.gen_range(0..10) == 0 { 20.0 } else { 1.0 };
                (cost, latency_for(cost, &mut rng))
            })
            .collect()
    }

    fn uniform_large_specs(n: usize, seed: u64) -> Vec<Spec> {
        let mut rng = StdRng::seed_from_u64(seed);
        (0..n)
            .map(|_| {
                let cost = rng.gen_range(20.0..50.0);
                (cost, latency_for(cost, &mut rng))
            })
            .collect()
    }

    /// Runs one candidate over the given request sequence and returns the
    /// comparable metrics. The channel capacity matches the process channel
    /// of the transfer pipeline.
    async fn run_candidate(kind: ExecutorKind, target: f64, specs: &[Spec]) -> ScenarioResult {
        let recorder = SimRecorder::new();
        let start = Instant::now();
        let (tx, rx) = channel(16);
        let mut executor = AnyExecutor::new(kind, rx, target, Some(target));
        let feeder = {
            let recorder = recorder.clone();
            let specs = specs.to_vec();
            tokio::spawn(async move {
                for (cost, latency) in specs {
                    if tx.send(recorder.process(cost, latency)).await.is_err() {
                        break;
                    }
                }
            })
        };
        executor.run().await.expect("The candidate executor failed");
        feeder.await.expect("The feeder task failed");
        compute_scenario_result(&recorder.records(), start, target)
    }

    async fn run_scenario(name: &str, target: f64, specs: &[Spec]) {
        let total_cost: f64 = specs.iter().map(|s| s.0).sum();
        println!();
        println!(
            "=== scenario {name}: {} requests, Σcost {:.0}, target {:.0}/s (ideal {:.1}s) ===",
            specs.len(),
            total_cost,
            target,
            total_cost / target,
        );
        println!(
            "{:<10} {:>10} {:>12} {:>10} {:>10}",
            "candidate", "makespan", "utilization", "tail", "completed"
        );
        for (label, kind) in CANDIDATES {
            let result = run_candidate(kind, target, specs).await;
            println!(
                "{:<10} {:>9.2}s {:>11.1}% {:>9.2}s {:>10}",
                label,
                result.makespan_secs,
                result.utilization * 100.0,
                result.tail_secs,
                result.completed,
            );
            assert_eq!(
                result.completed,
                specs.len(),
                "candidate {:?} lost requests",
                kind
            );
        }
    }

    /// Regime "many small uniform items at a high request rate" — the
    /// hypothesis table expects A (the pool amortizes per-request overhead;
    /// note CPU cost is invisible under virtual time, so only scheduling
    /// differences can show up here).
    #[tokio::test(start_paused = true)]
    #[ignore]
    async fn sim_uniform_small_high_rate() {
        run_scenario("uniform-small", 1000.0, &uniform_small_specs(2000, 42)).await;
    }

    /// Regime "small and large items randomly mixed" — the hypothesis table
    /// expects B (work-conserving shared queue), with the dissenting
    /// sub-hypothesis that C wins (shared bucket wastes no tokens).
    #[tokio::test(start_paused = true)]
    #[ignore]
    async fn sim_mixed() {
        run_scenario("mixed", 100.0, &mixed_specs(1000, 42)).await;
    }

    /// Regime "mostly large items" — the hypothesis table expects C
    /// (scheduling flexibility dominates, spawn overhead negligible).
    #[tokio::test(start_paused = true)]
    #[ignore]
    async fn sim_uniform_large() {
        run_scenario("uniform-large", 100.0, &uniform_large_specs(300, 42)).await;
    }

    /// Low-rate variant of the mixed regime: the queue-depth hostage effect
    /// (Q3) is worst when a single expensive item blocks a deep queue at a
    /// tiny per-worker rate.
    #[tokio::test(start_paused = true)]
    #[ignore]
    async fn sim_low_rate_mixed() {
        run_scenario("low-rate-mixed", 10.0, &mixed_specs(200, 42)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn test_compute_scenario_result() {
        let start = Instant::now();
        // 10 completions: 9 evenly at 1..=9s (cost 2 each), the last at 20s.
        let mut records: Vec<CompletionRecord> = (1..=9)
            .map(|i| CompletionRecord {
                finished_at: start + Duration::from_secs(i),
                cost: 2.0,
            })
            .collect();
        records.push(CompletionRecord {
            finished_at: start + Duration::from_secs(20),
            cost: 2.0,
        });

        // Σcost = 20, target 2.0/s → ideal 10s; makespan 20s → utilization 0.5.
        let result = compute_scenario_result(&records, start, 2.0);
        assert_eq!(result.completed, 10);
        assert_eq!(result.makespan_secs, 20.0);
        assert_eq!(result.utilization, 0.5);
        // t(90%) is the 9th completion (t=9s): tail = 20 − 9 = 11s.
        assert_eq!(result.tail_secs, 11.0);
    }

    #[tokio::test(start_paused = true)]
    async fn test_sim_process_consumes_estimate_after_latency() {
        let recorder = SimRecorder::new();
        let start = Instant::now();
        let process = recorder.process(2.0, Duration::from_millis(5));

        assert_eq!(process.estimate_resource(), 2.0);
        let result = process.process_and_consume_resource().await;

        assert_eq!(result.consumed, 2.0);
        assert!(!result.throttled);
        let records = recorder.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].cost, 2.0);
        assert_eq!(records[0].finished_at, start + Duration::from_millis(5));
    }
}
