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

//! Candidate B of the benchmark plan: a shared MPMC process queue.
//!
//! The pool model of [`crate::algo::worker::ThrottledExecutor`] is kept
//! (fixed workers, split per-worker buckets, monitor-driven scale-out, AIMD),
//! but the per-worker queues and the round-robin distributor are replaced by
//! one shared bounded `async-channel` queue that idle workers pull from.
//! Queueing becomes work-conserving: an expensive request can only ever hold
//! one worker, never a queue of batches behind it (benchmark-plan.md Q3).

use crate::algo::bucket::Bucket;
use crate::algo::congestion::{AimdController, BoostSlot, CongestionStats, TargetGauge};
use crate::algo::executor::ExecutorError;
use crate::algo::governor::ScaleOutGovernor;
use crate::algo::monitor::Probe;
use crate::algo::worker::{
    ResourceConstraintProcess, DEFAULT_MAX_CONCURRENT_CONNECTION, MINIMUM_WORKER_TARGET_LIMIT,
};
use futures::future::join_all;
use log::{debug, info};
use rand::random;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// Capacity of the shared process queue. Deliberately small: queued work
/// cannot be rebalanced, only work still in this queue is work-conserving.
const SHARED_QUEUE_CAPACITY: usize = 16;

/// Control messages delivered per worker (rate changes on scale-out or
/// congestion-control updates). Work items travel the shared queue instead.
#[derive(Debug, Clone, PartialEq)]
enum MpmcSignal {
    ChangeRefill(f64),
    ChangeMaxCap(f64),
}

struct MpmcWorker<T: Clone> {
    ctrl: Receiver<MpmcSignal>,
    shared: async_channel::Receiver<T>,
    bucket: Bucket,
    probe: Probe<f64>,
    congestion_stats: Arc<CongestionStats>,
}

impl<T: ResourceConstraintProcess + Send + Clone + Debug + 'static> MpmcWorker<T> {
    async fn start(mut self) {
        debug!("New MPMC worker has started");
        loop {
            tokio::select! {
                // Rate updates first so a queued backlog cannot delay them.
                biased;
                ctrl = self.ctrl.recv() => match ctrl {
                    Some(MpmcSignal::ChangeRefill(refill)) => {
                        debug!("Changed refill rate: {}", refill);
                        self.bucket.update_refill_rate(refill);
                    }
                    Some(MpmcSignal::ChangeMaxCap(max_cap)) => {
                        debug!("Changed max cap: {}", max_cap);
                        self.bucket.update_max_cap(max_cap);
                    }
                    // The executor is gone; no more work can arrive.
                    None => break,
                },
                received = self.shared.recv() => match received {
                    // The shared queue is closed and fully drained.
                    Err(_) => break,
                    Ok(process) => self.handle(process).await,
                },
            }
        }
        debug!("An MPMC worker has exited");
    }

    async fn handle(&mut self, process: T) {
        let estimate = process.estimate_resource();
        loop {
            if self.bucket.try_consume(estimate) {
                break;
            }
            tokio::time::sleep_until(self.bucket.estimate_available_at(estimate)).await;
        }
        let result = process.process_and_consume_resource().await;
        self.probe
            .add_observation(result.consumed)
            .expect("Failed to insert an observation");
        self.congestion_stats
            .record(result.throttled, result.consumed);
        self.bucket.feedback(estimate - result.consumed);
    }
}

pub struct MpmcExecutor<T: ResourceConstraintProcess + Clone> {
    /// This channel gets a task to proceed with resource constraint
    recv: Receiver<T>,
    /// Producer side of the shared work queue
    shared_tx: async_channel::Sender<T>,
    /// Consumer side, cloned into each worker
    shared_rx: async_channel::Receiver<T>,
    /// Per-worker control channels (rate updates)
    workers_ctrl: Vec<Sender<MpmcSignal>>,
    /// Tokio task handles for each worker
    workers_handle: Vec<JoinHandle<()>>,
    /// Target resource consumption
    target_limit: f64,
    /// Decides when the worker count should grow
    governor: ScaleOutGovernor,
    probe: Probe<f64>,
    /// Throttling counters shared with the workers
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
    /// Instruments the worker tasks for the benchmark stats
    task_monitor: tokio_metrics::TaskMonitor,
}

impl<T: ResourceConstraintProcess + Send + Clone + Debug + 'static> MpmcExecutor<T> {
    /// Creates an executor with one initial worker. Parameters mirror
    /// [`crate::algo::worker::ThrottledExecutor::new`].
    pub fn new(recv: Receiver<T>, target_limit: f64, initial_target: Option<f64>) -> Self {
        Self::with_workers(recv, target_limit, initial_target, 1)
    }

    /// Same, but starting with a fixed number of workers (used by tests and
    /// simulations that want to exclude scale-out dynamics).
    pub fn with_workers(
        recv: Receiver<T>,
        target_limit: f64,
        initial_target: Option<f64>,
        initial_workers: usize,
    ) -> Self {
        assert!(initial_workers >= 1);
        let governor = ScaleOutGovernor::new();
        let probe = governor.probe();
        let (shared_tx, shared_rx) = async_channel::bounded(SHARED_QUEUE_CAPACITY);
        let min_target = MINIMUM_WORKER_TARGET_LIMIT.min(target_limit);
        let congestion = AimdController::with_initial_target(
            target_limit,
            initial_target.unwrap_or(target_limit),
            min_target,
            Instant::now(),
        );
        let effective = congestion.effective_target();
        info!(
            "MPMC executor starts with the effective target {:.2} (ceiling: {:.2})",
            effective, target_limit
        );
        let mut executor = MpmcExecutor {
            recv,
            shared_tx,
            shared_rx,
            workers_ctrl: vec![],
            workers_handle: vec![],
            target_limit,
            governor,
            probe,
            congestion_stats: Arc::new(CongestionStats::default()),
            congestion,
            seen_requests: 0,
            seen_throttled: 0,
            boost_slot: Arc::new(BoostSlot::default()),
            target_gauge: Arc::new(TargetGauge::new(effective)),
            task_monitor: tokio_metrics::TaskMonitor::new(),
        };
        for _ in 0..initial_workers {
            executor.create_worker(initial_workers);
        }
        executor
    }

    fn num_workers(&self) -> usize {
        self.workers_handle.len()
    }

    fn create_worker(&mut self, target_total_worker_num: usize) {
        let target_limit = self.congestion.effective_target() / target_total_worker_num as f64;
        let jitter_sec =
            random::<f64>() * ScaleOutGovernor::jitter_max_secs(target_total_worker_num);
        let (ctrl_tx, ctrl_rx) = channel::<MpmcSignal>(4);
        let bucket = Bucket::new(target_limit, target_limit);
        let worker = MpmcWorker {
            ctrl: ctrl_rx,
            shared: self.shared_rx.clone(),
            bucket,
            probe: self.probe.clone(),
            congestion_stats: self.congestion_stats.clone(),
        };
        self.workers_ctrl.push(ctrl_tx);
        self.workers_handle
            .push(tokio::spawn(self.task_monitor.instrument(async move {
                tokio::time::sleep(Duration::from_secs_f64(jitter_sec)).await;
                worker.start().await;
            })));
    }

    fn max_workers(&self) -> usize {
        DEFAULT_MAX_CONCURRENT_CONNECTION
            .min(f64::floor(self.target_limit / MINIMUM_WORKER_TARGET_LIMIT) as usize)
    }

    /// Broadcasts a new per-worker rate to all existing workers.
    async fn broadcast_per_worker_rate(&self, target_each_worker: f64) {
        let mut futures = Vec::with_capacity(self.workers_ctrl.len() * 2);
        for tx in &self.workers_ctrl {
            futures.push(tx.send(MpmcSignal::ChangeMaxCap(target_each_worker)));
            futures.push(tx.send(MpmcSignal::ChangeRefill(target_each_worker)));
        }
        let _ = join_all(futures).await;
    }

    /// Feeds throttling observations into the AIMD controller, applies any
    /// pending informed jump from the slow control loop, and distributes the
    /// new per-worker rate when the effective target changes.
    async fn adjust_effective_target(&mut self) {
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
            self.broadcast_per_worker_rate(new_target / self.num_workers() as f64)
                .await;
        }
    }

    async fn scale_out_if_needed(&mut self) {
        if self.governor.should_grow(
            self.workers_ctrl.len(),
            self.max_workers(),
            self.congestion.effective_target(),
            self.congestion.is_congested(Instant::now()),
        ) {
            self.scale_out(self.workers_ctrl.len()).await;
        }
    }

    /// Scale out workers.
    async fn scale_out(&mut self, requested_additional_size: usize) {
        let original_size = self.workers_ctrl.len();
        let mut total_size = original_size + requested_additional_size;

        info!("Start scaling out from {} to {}", original_size, total_size);

        // Saturate total worker
        total_size = total_size.min(self.max_workers());

        // Notify the change of the rate to the existing workers
        let target_each_worker = self.congestion.effective_target() / total_size as f64;
        info!("New target limit each worker: {}", target_each_worker);
        self.broadcast_per_worker_rate(target_each_worker).await;

        // Create new workers
        let additional = total_size - original_size;
        self.workers_ctrl.reserve(additional);
        self.workers_handle.reserve(additional);
        for _ in 0..additional {
            self.create_worker(total_size);
        }

        // Memorize the throughput achieved before this growth and restart
        // the observation window.
        self.governor.record_growth(original_size);

        info!("Scaled out from {} to {}", original_size, total_size)
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

    /// Monitor instrumenting the worker tasks.
    pub fn task_monitor(&self) -> tokio_metrics::TaskMonitor {
        self.task_monitor.clone()
    }

    /// Runs the executor: forwards incoming messages into the shared queue
    /// (blocking when it is full — that is the backpressure), adjusting the
    /// effective target and scaling out as in the pool executor. Returns once
    /// the input channel is closed and all workers have drained the queue.
    pub async fn run(&mut self) -> Result<(), ExecutorError> {
        while let Some(message) = self.recv.recv().await {
            // Blocking on a full queue is the backpressure: unlike the
            // round-robin distributor there is nothing to skip to, any idle
            // worker pulls from this same queue.
            if self.shared_tx.send(message).await.is_err() {
                return Err(ExecutorError(
                    "All MPMC workers have terminated unexpectedly".to_string(),
                ));
            }

            // Adjust the effective target based on observed throttling (AIMD).
            self.adjust_effective_target().await;

            // Scale out if the number of workers is insufficient to achieve the target limit.
            self.scale_out_if_needed().await;
        }

        // The input channel is closed. Closing the shared queue lets the
        // workers drain the remaining items and then exit.
        self.shared_tx.close();
        for handle in self.workers_handle.drain(..) {
            handle
                .await
                .map_err(|e| ExecutorError(format!("An MPMC worker failed: {e}")))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::sim::SimRecorder;
    use tokio::sync::mpsc::channel as mpsc_channel;

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
    async fn test_single_worker_paces_at_target() {
        let recorder = SimRecorder::new();
        let start = Instant::now();
        let (tx, rx) = mpsc_channel(16);
        // Target 1.0/s; the worker bucket starts empty, so tokens for the
        // three cost-1 requests become available at t=1s, 2s and 3s.
        let mut executor = MpmcExecutor::new(rx, 1.0, None);
        for _ in 0..3 {
            tx.send(recorder.process(1.0, Duration::from_millis(10)))
                .await
                .unwrap();
        }
        drop(tx);

        executor.run().await.unwrap();

        let offsets = completion_offsets(&recorder, start);
        assert_eq!(offsets.len(), 3);
        assert!((1.0..1.2).contains(&offsets[0]), "got {:?}", offsets);
        assert!((2.0..2.2).contains(&offsets[1]), "got {:?}", offsets);
        assert!((3.0..3.2).contains(&offsets[2]), "got {:?}", offsets);
    }

    #[tokio::test(start_paused = true)]
    async fn test_shared_queue_is_work_conserving() {
        let recorder = SimRecorder::new();
        let start = Instant::now();
        let (tx, rx) = mpsc_channel(16);
        // Two workers, ample tokens. One slow request (1s service time)
        // occupies one worker; the four fast ones must all flow through the
        // other worker instead of queueing behind the slow request.
        let mut executor = MpmcExecutor::with_workers(rx, 10_000.0, None, 2);
        tx.send(recorder.process(1.0, Duration::from_secs(1)))
            .await
            .unwrap();
        for _ in 0..4 {
            tx.send(recorder.process(1.0, Duration::from_millis(10)))
                .await
                .unwrap();
        }
        drop(tx);

        executor.run().await.unwrap();

        let offsets = completion_offsets(&recorder, start);
        assert_eq!(offsets.len(), 5);
        // The four fast requests complete quickly (serially on one worker,
        // plus up to ~0.2s of worker start-up jitter)...
        assert!(offsets[3] < 0.5, "got {:?}", offsets);
        // ...while the slow one defines the makespan.
        assert!(
            (1.0..1.3).contains(offsets.last().unwrap()),
            "got {:?}",
            offsets
        );
    }
}
