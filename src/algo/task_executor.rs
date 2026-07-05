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
//! benchmark-plan.md §2). Concurrency emerges from rate × latency and is
//! bounded by a semaphore. AIMD congestion control updates the shared
//! bucket's refill rate directly; there are no Signal channels, no
//! round-robin distribution and no scale-out machinery.

use crate::algo::bucket::Bucket;
use crate::algo::congestion::{AimdController, BoostSlot, CongestionStats, TargetGauge};
use crate::algo::executor::ExecutorError;
use crate::algo::worker::{
    ResourceConstraintProcess, DEFAULT_MAX_CONCURRENT_CONNECTION, MINIMUM_WORKER_TARGET_LIMIT,
};
use log::info;
use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::Receiver;
use tokio::sync::Semaphore;
use tokio::time::Instant;

pub struct TaskExecutor<T: ResourceConstraintProcess + Clone> {
    /// This channel gets a task to proceed with resource constraint
    recv: Receiver<T>,
    /// User-specified ceiling of the resource consumption
    target_limit: f64,
    /// The single shared token bucket pacing all requests
    bucket: Arc<Mutex<Bucket>>,
    /// Bounds the number of in-flight request tasks
    semaphore: Arc<Semaphore>,
    max_concurrency: usize,
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
    /// Creates a task-per-request executor. Parameters mirror
    /// [`crate::algo::worker::ThrottledExecutor::new`].
    pub fn new(
        recv: Receiver<T>,
        target_limit: f64,
        initial_target: Option<f64>,
    ) -> TaskExecutor<T> {
        Self::with_max_concurrency(
            recv,
            target_limit,
            initial_target,
            DEFAULT_MAX_CONCURRENT_CONNECTION,
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
        TaskExecutor {
            recv,
            target_limit,
            bucket: Arc::new(Mutex::new(Bucket::new(effective, effective))),
            semaphore: Arc::new(Semaphore::new(max_concurrency)),
            max_concurrency,
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
            tokio::spawn(self.task_monitor.instrument(async move {
                let result = process.process_and_consume_resource().await;
                stats.record(result.throttled, result.consumed);
                bucket.lock().unwrap().feedback(estimate - result.consumed);
                drop(permit);
            }));

            // Adjust the effective target based on observed throttling (AIMD).
            self.adjust_effective_target();
        }

        // The input channel is closed: wait for all in-flight tasks. Their
        // permits return to the semaphore when the tasks finish (including on
        // panic, since the owned permit is dropped on unwind).
        let _all = self
            .semaphore
            .acquire_many(self.max_concurrency as u32)
            .await
            .expect("The request semaphore is never closed");
        Ok(())
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

    #[tokio::test(start_paused = true)]
    async fn test_requests_run_concurrently() {
        let recorder = SimRecorder::new();
        let start = Instant::now();
        let (tx, rx) = channel(16);
        // Ample token rate: 10 requests of 100ms latency must overlap
        // (a serial executor would need ~1s).
        let mut executor = TaskExecutor::new(rx, 1000.0, None);
        for _ in 0..10 {
            tx.send(recorder.process(1.0, Duration::from_millis(100)))
                .await
                .unwrap();
        }
        drop(tx);

        executor.run().await.unwrap();

        let offsets = completion_offsets(&recorder, start);
        assert_eq!(offsets.len(), 10);
        let makespan = offsets.last().unwrap();
        assert!((0.1..0.2).contains(makespan), "got {:?}", offsets);
    }

    #[tokio::test(start_paused = true)]
    async fn test_semaphore_bounds_concurrency() {
        let recorder = SimRecorder::new();
        let start = Instant::now();
        let (tx, rx) = channel(16);
        // Tokens are ample; with at most 2 tasks in flight, 4 requests of
        // 100ms latency need two waves.
        let mut executor = TaskExecutor::with_max_concurrency(rx, 1_000_000.0, None, 2);
        for _ in 0..4 {
            tx.send(recorder.process(1.0, Duration::from_millis(100)))
                .await
                .unwrap();
        }
        drop(tx);

        executor.run().await.unwrap();

        let offsets = completion_offsets(&recorder, start);
        assert_eq!(offsets.len(), 4);
        assert!(
            (0.2..0.3).contains(offsets.last().unwrap()),
            "got {:?}",
            offsets
        );
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
