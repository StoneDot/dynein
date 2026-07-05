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

//! Candidate C of the benchmark plan: task-per-request execution.
//!
//! Instead of a fixed worker pool with per-worker queues and split buckets,
//! the run loop acquires tokens from a **single shared bucket**, then spawns
//! one tokio task per request (one BatchWriteItem call — never one item; see
//! benchmark-plan.md §2). AIMD congestion control updates the shared
//! bucket's refill rate directly; there are no Signal channels and no
//! round-robin distribution.
//!
//! In-flight concurrency is governed the same way the pools govern their
//! worker count: the cap starts at 1 and doubles under the shared
//! [`ScaleOutGovernor`] while the measured throughput is below the effective
//! target *and* growing keeps helping. Rate × latency then determines how
//! much of the cap is actually used; the governor keeps the cap from
//! climbing when a saturated server inflates latency instead of throttling.

use crate::algo::bucket::Bucket;
use crate::algo::congestion::{AimdController, BoostSlot, CongestionStats, TargetGauge};
use crate::algo::executor::ExecutorError;
use crate::algo::governor::ScaleOutGovernor;
use crate::algo::monitor::Probe;
use crate::algo::worker::{ResourceConstraintProcess, MINIMUM_WORKER_TARGET_LIMIT};
use log::info;
use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::Receiver;
use tokio::sync::Semaphore;
use tokio::time::Instant;

/// Default ceiling for the adaptive in-flight cap. The concurrency this
/// architecture actually needs emerges from rate × latency: even a
/// quota-scale target (40k WCU/s in 25-WCU batches at ~30ms) keeps only
/// ~50 requests in flight, and the governor stops growth well before any
/// realistic ceiling. This constant is the last-resort bound.
pub(crate) const DEFAULT_TASK_MAX_IN_FLIGHT: usize = 256;

/// The adaptive in-flight cap starts here and doubles per approved growth
/// step (parity with the pools, which start at one worker).
const INITIAL_IN_FLIGHT: usize = 1;

pub struct TaskExecutor<T: ResourceConstraintProcess + Clone> {
    /// This channel gets a task to proceed with resource constraint
    recv: Receiver<T>,
    /// User-specified ceiling of the resource consumption
    target_limit: f64,
    /// The single shared token bucket pacing all requests
    bucket: Arc<Mutex<Bucket>>,
    /// Bounds the number of in-flight request tasks (grown adaptively)
    semaphore: Arc<Semaphore>,
    /// Current adaptive in-flight cap (permits issued so far)
    current_cap: usize,
    /// Ceiling for the adaptive growth
    max_concurrency: usize,
    /// Decides when the in-flight cap should grow
    governor: ScaleOutGovernor,
    probe: Probe<f64>,
    /// Throttling counters shared with the request tasks
    congestion_stats: Arc<CongestionStats>,
    /// AIMD controller deciding the effective target based on throttling
    congestion: AimdController,
    /// Cumulative counts already consumed from `congestion_stats`
    seen_requests: usize,
    seen_throttled: usize,
    /// Latest safe-target suggestion from the slow control loop
    boost_slot: Arc<BoostSlot>,
    /// Publishes the current effective target for external observers
    target_gauge: Arc<TargetGauge>,
    /// Instruments the spawned request tasks for the benchmark stats
    task_monitor: tokio_metrics::TaskMonitor,
}

impl<T: ResourceConstraintProcess + Send + Clone + Debug + 'static> TaskExecutor<T> {
    /// Creates a task-per-request executor with the default in-flight cap.
    pub fn new(
        recv: Receiver<T>,
        target_limit: f64,
        initial_target: Option<f64>,
    ) -> TaskExecutor<T> {
        Self::with_max_concurrency(
            recv,
            target_limit,
            initial_target,
            DEFAULT_TASK_MAX_IN_FLIGHT,
        )
    }

    pub fn with_max_concurrency(
        recv: Receiver<T>,
        target_limit: f64,
        initial_target: Option<f64>,
        max_concurrency: usize,
    ) -> TaskExecutor<T> {
        let min_target = MINIMUM_WORKER_TARGET_LIMIT.min(target_limit);
        let congestion = AimdController::with_initial_target(
            target_limit,
            initial_target.unwrap_or(target_limit),
            min_target,
            Instant::now(),
        );
        let effective = congestion.effective_target();
        info!(
            "Task executor starts with the effective target {:.2} (ceiling: {:.2})",
            effective, target_limit
        );
        assert!(max_concurrency >= 1);
        let governor = ScaleOutGovernor::new();
        let probe = governor.probe();
        let current_cap = INITIAL_IN_FLIGHT.min(max_concurrency);
        TaskExecutor {
            recv,
            target_limit,
            bucket: Arc::new(Mutex::new(Bucket::new(effective, effective))),
            semaphore: Arc::new(Semaphore::new(current_cap)),
            current_cap,
            max_concurrency,
            governor,
            probe,
            congestion_stats: Arc::new(CongestionStats::default()),
            congestion,
            seen_requests: 0,
            seen_throttled: 0,
            boost_slot: Arc::new(BoostSlot::default()),
            target_gauge: Arc::new(TargetGauge::new(effective)),
            task_monitor: tokio_metrics::TaskMonitor::new(),
        }
    }

    /// Shared counters of processed/throttled requests and consumed capacity.
    pub fn congestion_stats(&self) -> Arc<CongestionStats> {
        self.congestion_stats.clone()
    }

    /// The slot through which the slow control loop suggests safe targets.
    pub fn boost_slot(&self) -> Arc<BoostSlot> {
        self.boost_slot.clone()
    }

    /// Gauge publishing the current effective target.
    pub fn target_gauge(&self) -> Arc<TargetGauge> {
        self.target_gauge.clone()
    }

    /// Monitor instrumenting the spawned request tasks.
    pub fn task_monitor(&self) -> tokio_metrics::TaskMonitor {
        self.task_monitor.clone()
    }

    /// Current adaptive in-flight cap.
    #[cfg(test)]
    pub fn in_flight_cap(&self) -> usize {
        self.current_cap
    }

    /// Runs the executor: paces requests through the shared bucket and spawns
    /// one task per request. Returns once the input channel is closed and all
    /// spawned request tasks have finished.
    pub async fn run(&mut self) -> Result<(), ExecutorError> {
        while let Some(process) = self.recv.recv().await {
            let estimate = process.estimate_resource();

            // Acquire tokens from the shared bucket before spawning. This is
            // the single pacing point: token acquisition is serialized here,
            // execution itself is not.
            loop {
                let wait_until = {
                    let mut bucket = self.bucket.lock().unwrap();
                    if bucket.try_consume(estimate) {
                        None
                    } else {
                        Some(bucket.estimate_available_at(estimate))
                    }
                };
                match wait_until {
                    None => break,
                    Some(at) => tokio::time::sleep_until(at).await,
                }
            }

            let permit = self
                .semaphore
                .clone()
                .acquire_owned()
                .await
                .expect("The request semaphore is never closed");
            let bucket = self.bucket.clone();
            let stats = self.congestion_stats.clone();
            let probe = self.probe.clone();
            tokio::spawn(self.task_monitor.instrument(async move {
                let result = process.process_and_consume_resource().await;
                probe
                    .add_observation(result.consumed)
                    .expect("Failed to insert an observation");
                stats.record(result.throttled, result.consumed);
                bucket.lock().unwrap().feedback(estimate - result.consumed);
                drop(permit);
            }));

            // Adjust the effective target based on observed throttling (AIMD).
            self.adjust_effective_target();

            // Grow the in-flight cap if the measured throughput justifies it.
            self.grow_in_flight_if_needed();
        }

        // The input channel is closed: wait for all in-flight tasks. Their
        // permits return to the semaphore when the tasks finish (including on
        // panic, since the owned permit is dropped on unwind).
        let _all = self
            .semaphore
            .acquire_many(self.current_cap as u32)
            .await
            .expect("The request semaphore is never closed");
        Ok(())
    }

    /// Doubles the in-flight cap when the governor approves: measured
    /// throughput statistically below the effective target, the previous
    /// growth demonstrably helped, no recent congestion, and the ceiling
    /// not yet reached.
    fn grow_in_flight_if_needed(&mut self) {
        if self.governor.should_grow(
            self.current_cap,
            self.max_concurrency,
            self.congestion.effective_target(),
            self.congestion.is_congested(Instant::now()),
        ) {
            let original = self.current_cap;
            let additional = original.min(self.max_concurrency - original);
            self.semaphore.add_permits(additional);
            self.current_cap += additional;
            self.governor.record_growth(original);
            info!(
                "Grew the in-flight request cap from {} to {}",
                original, self.current_cap
            );
        }
    }

    /// Feeds throttling observations into the AIMD controller, applies any
    /// pending informed jump from the slow control loop, and updates the
    /// shared bucket when the effective target changes.
    fn adjust_effective_target(&mut self) {
        let now = Instant::now();
        let mut changed: Option<f64> = None;

        let (requests, throttled) = self.congestion_stats.snapshot();
        let new_requests = requests - self.seen_requests;
        let new_throttled = throttled - self.seen_throttled;
        if new_requests > 0 || new_throttled > 0 {
            self.seen_requests = requests;
            self.seen_throttled = throttled;
            if let Some(new_target) =
                self.congestion
                    .on_observation(new_requests, new_throttled, now)
            {
                info!(
                    "Congestion control changed the effective target to {:.2} (user target: {:.2})",
                    new_target, self.target_limit
                );
                changed = Some(new_target);
            }
        }

        // Apply a pending informed jump from the slow control loop, if any.
        if let Some(suggested) = self.boost_slot.take() {
            if let Some(new_target) = self.congestion.boost_to(suggested, now) {
                info!(
                    "Slow loop boosted the effective target to {:.2} (user target: {:.2})",
                    new_target, self.target_limit
                );
                changed = Some(new_target);
            }
        }

        if let Some(new_target) = changed {
            self.target_gauge.set(new_target);
            let mut bucket = self.bucket.lock().unwrap();
            bucket.update_max_cap(new_target);
            bucket.update_refill_rate(new_target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::sim::SimRecorder;
    use std::time::Duration;
    use tokio::sync::mpsc::channel;

    /// Sorted completion offsets (secs since `start`) of all records.
    fn completion_offsets(recorder: &SimRecorder, start: Instant) -> Vec<f64> {
        let mut offsets: Vec<f64> = recorder
            .records()
            .iter()
            .map(|r| r.finished_at.duration_since(start).as_secs_f64())
            .collect();
        offsets.sort_by(|a, b| a.partial_cmp(b).unwrap());
        offsets
    }

    #[tokio::test(start_paused = true)]
    async fn test_paces_requests_by_the_shared_bucket() {
        let recorder = SimRecorder::new();
        let start = Instant::now();
        let (tx, rx) = channel(16);
        // Target 1.0/s; the bucket starts empty, so tokens for the three
        // cost-1 requests become available at t=1s, 2s and 3s.
        let mut executor = TaskExecutor::new(rx, 1.0, None);
        for _ in 0..3 {
            tx.send(recorder.process(1.0, Duration::from_millis(10)))
                .await
                .unwrap();
        }
        drop(tx);

        executor.run().await.unwrap();

        let offsets = completion_offsets(&recorder, start);
        assert_eq!(offsets.len(), 3);
        assert!((1.0..1.1).contains(&offsets[0]), "got {:?}", offsets);
        assert!((2.0..2.1).contains(&offsets[1]), "got {:?}", offsets);
        assert!((3.0..3.1).contains(&offsets[2]), "got {:?}", offsets);
    }

    // NOTE: the earlier test_requests_run_concurrently asserted that a cold
    // start runs everything concurrently at once. The requirement changed:
    // the in-flight cap now starts at 1 and grows adaptively under the
    // shared ScaleOutGovernor (concurrency must be *controlled* against the
    // target, not merely available), so cold-start concurrency ramps instead
    // of being instant. The two tests below cover the new behavior.

    #[tokio::test(start_paused = true)]
    async fn test_in_flight_cap_grows_until_the_target_is_reached() {
        let recorder = SimRecorder::new();
        let start = Instant::now();
        let (tx, rx) = channel(16);
        // Target 250/s with 20-40ms latencies (the realistic DynamoDB range)
        // needs ~8 requests in flight; processing 1000 requests serially
        // (cap 1) would take ~30s. The latency regime matters: the governor
        // decides from throughput statistics, and very few completions per
        // ramp window (high latency at low concurrency) degrade them — see
        // the note on governor statistics in the design document.
        let mut executor = TaskExecutor::new(rx, 250.0, None);
        let feeder = {
            let recorder = recorder.clone();
            tokio::spawn(async move {
                for i in 0..1000u64 {
                    let latency = Duration::from_millis(20 + (i % 5) * 5);
                    if tx.send(recorder.process(1.0, latency)).await.is_err() {
                        break;
                    }
                }
            })
        };

        executor.run().await.unwrap();
        feeder.await.unwrap();

        let offsets = completion_offsets(&recorder, start);
        assert_eq!(offsets.len(), 1000);
        // The adaptive cap must have grown well past the serial regime...
        assert!(
            executor.in_flight_cap() >= 4,
            "cap stayed at {}",
            executor.in_flight_cap()
        );
        // ...bringing the makespan far below the serial ~30s.
        assert!(offsets.last().unwrap() < &15.0, "got {:?}", offsets.last());
    }

    #[tokio::test(start_paused = true)]
    async fn test_max_concurrency_is_the_growth_ceiling() {
        let recorder = SimRecorder::new();
        let start = Instant::now();
        // All 30 requests are queued before run() starts consuming, so the
        // channel must hold them all.
        let (tx, rx) = channel(30);
        // Ample tokens, 50ms latency, ceiling 2: throughput is capped at
        // ~40 requests/s no matter how far the target is.
        let mut executor = TaskExecutor::with_max_concurrency(rx, 1_000_000.0, None, 2);
        for _ in 0..30 {
            tx.send(recorder.process(1.0, Duration::from_millis(50)))
                .await
                .unwrap();
        }
        drop(tx);

        executor.run().await.unwrap();

        let offsets = completion_offsets(&recorder, start);
        assert_eq!(offsets.len(), 30);
        assert!(executor.in_flight_cap() <= 2);
        // 30 requests at ≤2 in flight and 50ms each need ≥ 0.75s.
        assert!(offsets.last().unwrap() >= &0.75, "got {:?}", offsets.last());
    }

    #[tokio::test(start_paused = true)]
    async fn test_boost_updates_the_shared_bucket_refill() {
        let recorder = SimRecorder::new();
        let start = Instant::now();
        let (tx, rx) = channel(16);
        // Start at 1.0/s with a ceiling of 100. A pending boost suggestion is
        // applied after the first request, so the remaining two are paced at
        // 100/s instead of 1/s (without the boost they finish after ~3s).
        let mut executor = TaskExecutor::new(rx, 100.0, Some(1.0));
        executor.boost_slot().suggest(100.0);
        for _ in 0..3 {
            tx.send(recorder.process(1.0, Duration::from_millis(10)))
                .await
                .unwrap();
        }
        drop(tx);

        executor.run().await.unwrap();

        let offsets = completion_offsets(&recorder, start);
        assert_eq!(offsets.len(), 3);
        assert!((1.0..1.1).contains(&offsets[0]), "got {:?}", offsets);
        assert!(offsets[2] < 1.5, "got {:?}", offsets);
        assert_eq!(executor.target_gauge().get(), 100.0);
    }
}
