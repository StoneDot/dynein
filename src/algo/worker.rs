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
use crate::algo::bucket::Bucket;
use crate::algo::congestion::{AimdController, BoostSlot, CongestionStats, TargetGauge};
use crate::algo::governor::ScaleOutGovernor;
use crate::algo::monitor::Probe;
use futures::future::join_all;
use log::{debug, error, info, trace};
use rand::random;
use std::fmt::Debug;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::error::{SendError, TrySendError};
use tokio::sync::mpsc::{channel, Receiver};
use tokio::task::JoinHandle;
use tokio::time::Instant;

#[derive(PartialEq, Debug, Clone)]
pub enum Signal<T>
where
    T: Clone,
{
    Close,
    ChangeRefill(f64),
    ChangeMaxCap(f64),
    Process(T),
}

struct ThrottledWorker<T: Clone> {
    recv: Receiver<Signal<T>>,
    process_notifier: Arc<tokio::sync::Notify>,
    bucket: Bucket,
    probe: Probe<f64>,
    congestion_stats: Arc<CongestionStats>,
}

/// The outcome of a single resource-consuming process execution.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcessResult {
    /// The amount of resource actually consumed.
    pub consumed: f64,
    /// Whether the process observed a capacity shortage. Used as the
    /// congestion signal for AIMD control of the effective target.
    pub throttled: bool,
}

/// Trait representing a process with resource constraints.
///
/// This trait provides methods for estimating the resource required by the process and
/// processing while consuming the resource.
/// TODO: extend the implementation to support GSIs and tables
pub trait ResourceConstraintProcess {
    /// Estimates the resource based on the given code.
    ///
    /// This method returns an estimation of the amount of resources.
    /// The actual implementation of how the estimation is calculated
    /// should be provided by the implementor of this trait.
    ///
    /// # Returns
    ///
    /// Returns an instance of `f64` that represents
    /// the estimated amount of resources.
    fn estimate_resource(&self) -> f64;

    /// Processes and consumes the resource asynchronously.
    ///
    /// This function asynchronously processes and consumes a resource, returning a `Future`
    /// that will eventually resolve to a [`ProcessResult`] carrying the amount of consumed
    /// resource and whether the process observed a capacity shortage (throttling).
    fn process_and_consume_resource(&self) -> impl Future<Output = ProcessResult> + Send;
}

impl<T: ResourceConstraintProcess + Send + Clone + Debug + 'static> ThrottledWorker<T> {
    fn new(
        recv: Receiver<Signal<T>>,
        process_notifier: Arc<tokio::sync::Notify>,
        bucket: Bucket,
        probe: Probe<f64>,
        congestion_stats: Arc<CongestionStats>,
    ) -> ThrottledWorker<T> {
        ThrottledWorker {
            recv,
            process_notifier,
            bucket,
            probe,
            congestion_stats,
        }
    }

    async fn start(mut self) {
        debug!("New worker has started");
        while let Some(v) = self.recv.recv().await {
            match v {
                Signal::Close => break,
                Signal::ChangeRefill(refill) => {
                    debug!("Changed refill rate: {}", refill);
                    self.bucket.update_refill_rate(refill)
                }
                Signal::ChangeMaxCap(max_cap) => {
                    debug!("Changed max cap: {}", max_cap);
                    self.bucket.update_max_cap(max_cap)
                }
                Signal::Process(p) => {
                    trace!("Got process request {:?}", p);
                    let estimate = p.estimate_resource();
                    loop {
                        if self.bucket.try_consume(estimate) {
                            break;
                        }
                        tokio::time::sleep_until(self.bucket.estimate_available_at(estimate)).await;
                    }
                    let result = p.process_and_consume_resource().await;
                    self.probe
                        .add_observation(result.consumed)
                        .expect("Failed to insert an observation");
                    self.congestion_stats
                        .record(result.throttled, result.consumed);
                    self.bucket.feedback(estimate - result.consumed);
                    self.process_notifier.notify_one();
                }
            }
        }
        debug!("A worker has exited");
    }
}

pub struct ThrottledExecutor<T: ResourceConstraintProcess + Clone> {
    /// This channel gets a task to proceed with resource constraint
    recv: Receiver<T>,
    /// Communication channels to each worker
    workers_tx: Vec<tokio::sync::mpsc::Sender<Signal<T>>>,
    /// Tokio task handles for each worker
    workers_handle: Vec<JoinHandle<()>>,
    /// This notifier is used to wait worker completion
    notifier: Arc<tokio::sync::Notify>,
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
    /// Capacity of each per-worker queue (benchmark axis; see benchmark-plan.md Q3)
    queue_depth: usize,
    /// Instruments the worker tasks for the benchmark stats
    task_monitor: tokio_metrics::TaskMonitor,
}

// Even if round trip time is 1s, we can achieve specified WCU with this setting
// unless latency is too high.
pub(crate) const MINIMUM_WORKER_TARGET_LIMIT: f64 = 1.0;

pub(crate) const DEFAULT_MAX_CONCURRENT_CONNECTION: usize = 1024;

impl<T: ResourceConstraintProcess + Send + Clone + Debug + 'static> ThrottledExecutor<T> {
    /// Creates an executor. `target_limit` is the user-specified ceiling of the
    /// resource consumption. `initial_target` optionally gives a more realistic
    /// starting point derived from known information (e.g. provisioned capacity);
    /// when `None`, the executor starts at the ceiling. `queue_depth` is the
    /// per-worker queue capacity (the production default is 16; candidate A′
    /// of the benchmark plan uses 1).
    pub fn with_queue_depth(
        recv: Receiver<T>,
        target_limit: f64,
        initial_target: Option<f64>,
        queue_depth: usize,
    ) -> ThrottledExecutor<T> {
        assert!(queue_depth >= 1);
        let governor = ScaleOutGovernor::new();
        let probe = governor.probe();
        let min_target = MINIMUM_WORKER_TARGET_LIMIT.min(target_limit);
        let mut initial = ThrottledExecutor {
            recv,
            workers_tx: vec![],
            workers_handle: vec![],
            notifier: Arc::new(tokio::sync::Notify::new()),
            target_limit,
            governor,
            probe,
            congestion_stats: Arc::new(CongestionStats::default()),
            congestion: AimdController::with_initial_target(
                target_limit,
                initial_target.unwrap_or(target_limit),
                min_target,
                Instant::now(),
            ),
            seen_requests: 0,
            seen_throttled: 0,
            boost_slot: Arc::new(BoostSlot::default()),
            target_gauge: Arc::new(TargetGauge::new(
                initial_target
                    .unwrap_or(target_limit)
                    .clamp(MINIMUM_WORKER_TARGET_LIMIT.min(target_limit), target_limit),
            )),
            queue_depth,
            task_monitor: tokio_metrics::TaskMonitor::new(),
        };
        info!(
            "Executor starts with the effective target {:.2} (ceiling: {:.2})",
            initial.congestion.effective_target(),
            target_limit
        );
        initial.create_worker(1);
        initial
    }

    fn num_workers(&self) -> usize {
        self.workers_handle.len()
    }

    /// Shared counters of processed/throttled requests and consumed capacity.
    /// The slow control loop reads these to estimate our own consumption rate.
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

    /// Capacity of each per-worker queue.
    #[cfg(test)]
    pub fn queue_depth(&self) -> usize {
        self.queue_depth
    }

    /// Monitor instrumenting the worker tasks.
    pub fn task_monitor(&self) -> tokio_metrics::TaskMonitor {
        self.task_monitor.clone()
    }

    fn create_worker(&mut self, target_total_worker_num: usize) {
        let target_limit = self.congestion.effective_target() / target_total_worker_num as f64;
        let jitter_sec =
            random::<f64>() * ScaleOutGovernor::jitter_max_secs(target_total_worker_num);
        let (tx, rx) = channel::<Signal<T>>(self.queue_depth);
        let bucket = Bucket::new(target_limit, target_limit);
        let worker = ThrottledWorker::new(
            rx,
            self.notifier.clone(),
            bucket,
            self.probe.clone(),
            self.congestion_stats.clone(),
        );
        self.workers_tx.push(tx);
        self.workers_handle
            .push(tokio::spawn(self.task_monitor.instrument(async move {
                tokio::time::sleep(Duration::from_secs_f64(jitter_sec)).await;
                worker.start().await;
            })));
    }

    /// Runs the throttled executor, distributing incoming messages to workers in a round-robin
    /// fashion. The function also handles scaling out workers if the load exceeds target limits.
    ///
    /// # Errors
    /// Returns a `Vec` of `tokio::sync::mpsc::error::SendError<Signal<T>>` if any workers fail to
    /// terminate gracefully when the input channel is closed.
    ///
    /// # Details
    /// - The function receives messages from the input channel and sends them to the workers.
    /// - Messages are distributed using a round-robin approach.
    /// - If a worker's channel is closed, it tries to deliver the message to another worker.
    /// - If all worker channels are closed, the function terminates.
    /// - The function monitors workers' performance, scaling out if necessary to meet the target limit.
    /// - When the input channel closes, the function ensures all workers receive a `Close` signal and checks that they terminate successfully.
    pub async fn run(&mut self) -> Result<(), Vec<tokio::sync::mpsc::error::SendError<Signal<T>>>> {
        let mut selected_worker = 0;

        while let Some(message) = self.recv.recv().await {
            trace!("Received new message in a worker: {:?}", message);

            let signal = Signal::Process(message);
            let num_workers = self.num_workers();

            // Distribute messages in round-robin fashion.
            'delivery_loop: loop {
                let mut num_closed = 0;
                for _ in 0..num_workers {
                    // Try to send a message
                    match self.workers_tx[selected_worker].try_send(signal.clone()) {
                        Ok(_) => {
                            // A message is successfully sent.
                            break 'delivery_loop;
                        }
                        Err(err) => match err {
                            TrySendError::Full(_) => {}
                            TrySendError::Closed(_) => num_closed += 1,
                        },
                    }

                    // Move to a next worker.
                    selected_worker = (selected_worker + 1) % num_workers;
                }
                // The process should quit when all channels are closed.
                if num_closed >= num_workers {
                    return Ok(());
                }

                // Wait completion of a worker
                self.notifier.notified().await;
            }

            // Adjust the effective target based on observed throttling (AIMD).
            self.adjust_effective_target().await;

            // Scale out if the number of workers is insufficient to achieve the target limit.
            self.scale_out_if_needed().await;
        }

        // When input channel is closed, all workers should be terminated.
        let result = self.terminate_all_workers().await;

        // Wait until every worker has drained its queue and exited, so that
        // returning from run() means all accepted work has been processed.
        for handle in self.workers_handle.drain(..) {
            if let Err(e) = handle.await {
                error!("A worker task failed: {}", e);
            }
        }

        // Check whether all workers are terminated successfully.
        let result: Vec<_> = result.into_iter().filter_map(|item| item.err()).collect();
        if result.is_empty() {
            Ok(())
        } else {
            Err(result)
        }
    }

    async fn terminate_all_workers(&mut self) -> Vec<Result<(), SendError<Signal<T>>>> {
        let num_workers = self.num_workers();
        let mut waits = Vec::with_capacity(num_workers);
        for i in 0..num_workers {
            waits.push(self.workers_tx[i].send(Signal::Close));
        }
        join_all(waits).await
    }

    fn max_workers(&self) -> usize {
        DEFAULT_MAX_CONCURRENT_CONNECTION
            .min(f64::floor(self.target_limit / MINIMUM_WORKER_TARGET_LIMIT) as usize)
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
            let target_each_worker = new_target / self.num_workers() as f64;
            let mut futures = Vec::with_capacity(self.workers_tx.len() * 2);
            for tx in &self.workers_tx {
                futures.push(tx.send(Signal::ChangeMaxCap(target_each_worker)));
                futures.push(tx.send(Signal::ChangeRefill(target_each_worker)));
            }
            let _ = join_all(futures).await;
        }
    }

    async fn scale_out_if_needed(&mut self) {
        if self.governor.should_grow(
            self.workers_tx.len(),
            self.max_workers(),
            self.congestion.effective_target(),
            self.congestion.is_congested(Instant::now()),
        ) {
            self.scale_out(self.workers_tx.len()).await;
        }
    }

    /// Scale out workers.
    async fn scale_out(&mut self, requested_additional_size: usize) {
        let original_size = self.workers_tx.len();
        let mut total_size = original_size + requested_additional_size;

        info!("Start scaling out from {} to {}", original_size, total_size);

        // Saturate total worker
        total_size = total_size.min(self.max_workers());

        // Calculate new limit for each worker
        let target_each_worker = self.congestion.effective_target() / total_size as f64;
        info!("New target limit each worker: {}", target_each_worker);

        // Notify the change of the rate to each worker
        let mut features = Vec::with_capacity(original_size * 2);
        for tx in &self.workers_tx {
            features.push(tx.send(Signal::ChangeMaxCap(target_each_worker)));
            features.push(tx.send(Signal::ChangeRefill(target_each_worker)));
        }
        let _ = join_all(features).await;
        // TODO: Error handling

        // Create new workers
        self.workers_tx.reserve(requested_additional_size);
        self.workers_handle.reserve(requested_additional_size);
        for _ in 0..requested_additional_size {
            self.create_worker(total_size);
        }

        // Memorize the throughput achieved before this growth and restart
        // the observation window.
        self.governor.record_growth(original_size);

        info!("Scaled out from {} to {}", original_size, total_size)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::algo::monitor::Monitor;
    use tokio::sync::mpsc::channel;

    macro_rules! assert_timing {
        ( $b:literal , $e:literal , $( $x:stmt ),* ) => {
            {
                let start = std::time::Instant::now();
                $(
                    $x
                )*
                let elapsed = start.elapsed();
                assert!(elapsed >= std::time::Duration::from_secs_f64($b as f64), "Execution is too fast");
                assert!(elapsed <= std::time::Duration::from_secs_f64($e as f64), "Execution is too slow")
            }
        };
    }

    #[derive(Eq, PartialEq, Debug)]
    enum Message {
        Estimated,
        Consumed,
    }

    #[derive(Debug, Clone)]
    struct TestProcess {
        estimate: f64,
        actual: f64,
        tx: std::sync::mpsc::Sender<Message>,
    }

    trait MessageWaiter {
        async fn wait_consumed(self);
    }
    impl MessageWaiter for std::sync::mpsc::Receiver<Message> {
        async fn wait_consumed(self) {
            let h = tokio::task::spawn_blocking(move || {
                while let Ok(m) = self.recv() {
                    match m {
                        Message::Estimated => continue,
                        Message::Consumed => return,
                    }
                }
                panic!("channel is closed unexpectedly")
            });
            h.await.unwrap()
        }
    }

    impl TestProcess {
        fn new(estimate: f64, actual: f64) -> (TestProcess, std::sync::mpsc::Receiver<Message>) {
            let (tx, rx) = std::sync::mpsc::channel();
            (
                TestProcess {
                    estimate,
                    actual,
                    tx,
                },
                rx,
            )
        }
    }

    impl ResourceConstraintProcess for TestProcess {
        fn estimate_resource(&self) -> f64 {
            self.tx.send(Message::Estimated).unwrap();
            self.estimate
        }

        fn process_and_consume_resource(&self) -> impl Future<Output = ProcessResult> {
            self.tx.send(Message::Consumed).unwrap();
            let v = self.actual;
            async move {
                ProcessResult {
                    consumed: v,
                    throttled: false,
                }
            }
        }
    }

    #[tokio::test]
    async fn test_test_process() {
        let (process, rx) = TestProcess::new(1f64, 2f64);
        assert_eq!(process.estimate_resource(), 1f64);
        assert_timing!(0, 0.1, assert_eq!(rx.recv().unwrap(), Message::Estimated));
        assert_eq!(process.process_and_consume_resource().await.consumed, 2f64);
        assert_timing!(0, 0.1, assert_eq!(rx.recv().unwrap(), Message::Consumed));
    }

    #[tokio::test]
    async fn test_with_queue_depth_and_target_gauge() {
        let (_tx, rx) = channel::<TestProcess>(1);
        let executor = ThrottledExecutor::with_queue_depth(rx, 100.0, Some(40.0), 1);
        assert_eq!(executor.queue_depth(), 1);
        // The gauge publishes the initial effective target right away.
        assert_eq!(executor.target_gauge().get(), 40.0);
    }

    #[tokio::test]
    async fn test_throttled_worker() {
        // Initial setup
        let (tx, rx) = channel::<Signal<TestProcess>>(1);
        let mut bucket = Bucket::new(1f64, 1f64);
        bucket.fill();
        // cap = 1
        let (probe, _monitor) = Monitor::new(3, 3);
        let worker = ThrottledWorker::new(
            rx,
            Arc::new(tokio::sync::Notify::new()),
            bucket,
            probe,
            Arc::new(CongestionStats::default()),
        );
        let handle = tokio::spawn(async move { worker.start().await });

        // Consume all capacity immediately
        let (process, rx) = TestProcess::new(1f64, 1f64);
        tx.send(Signal::Process(process)).await.unwrap();
        assert_timing!(0, 0.1, rx.wait_consumed().await);
        // cap = 0

        // Need to wait about a second to refill the capacity
        let (process, rx) = TestProcess::new(1f64, 1f64);
        tx.send(Signal::Process(process)).await.unwrap();
        assert_timing!(0.9, 1.1, rx.wait_consumed().await);
        // cap = 0

        // Change refill rate to two
        tx.send(Signal::ChangeRefill(2f64)).await.unwrap();
        let (process, rx) = TestProcess::new(1f64, 1f64);
        tx.send(Signal::Process(process)).await.unwrap();
        assert_timing!(0.4, 0.6, rx.wait_consumed().await);
        // cap = 0

        // Try over consuming
        let (process, rx) = TestProcess::new(1f64, 2f64);
        tx.send(Signal::Process(process)).await.unwrap();
        assert_timing!(0.4, 0.6, rx.wait_consumed().await);
        // cap = -1
        // Need to wait coming capacity back to zero
        let (process, rx) = TestProcess::new(0f64, 0f64);
        tx.send(Signal::Process(process)).await.unwrap();
        assert_timing!(0.4, 0.6, rx.wait_consumed().await);
        // cap = 0

        // Change refill rate back and change max capacity
        tx.send(Signal::ChangeRefill(1f64)).await.unwrap();
        // Check overestimate
        let (process, rx) = TestProcess::new(1f64, 0.5);
        tx.send(Signal::Process(process)).await.unwrap();
        assert_timing!(0.9, 1.1, rx.wait_consumed().await);
        // cap = 0.5
        let (process, rx) = TestProcess::new(0.5, 0.5);
        tx.send(Signal::Process(process)).await.unwrap();
        assert_timing!(0.0, 0.1, rx.wait_consumed().await);
        // cap = 0

        // Change max capacity
        tx.send(Signal::ChangeMaxCap(3f64)).await.unwrap();
        let (process, rx) = TestProcess::new(2f64, 2f64);
        tx.send(Signal::Process(process)).await.unwrap();
        assert_timing!(1.9, 2.1, rx.wait_consumed().await);
        // cap = 0
        // Check severe overestimate
        let (process, rx) = TestProcess::new(3.0, 1.0);
        tx.send(Signal::Process(process)).await.unwrap();
        assert_timing!(2.9, 3.1, rx.wait_consumed().await);
        // cap = 2
        let (process, rx) = TestProcess::new(2.0, 2.0);
        tx.send(Signal::Process(process)).await.unwrap();
        assert_timing!(0.0, 0.1, rx.wait_consumed().await);
        // cap = 0

        // Exit worker
        tx.send(Signal::Close).await.unwrap();
        handle.await.unwrap()
    }
}
