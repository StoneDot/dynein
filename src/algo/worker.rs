use crate::algo::bucket::Bucket;
use std::future::Future;
use tokio::sync::mpsc::Receiver;

enum Signal<T> {
    Close,
    ChangeRefill(f64),
    ChangeMaxCap(f64),
    Process(T),
}

struct ThrottledWorker<T> {
    recv: Receiver<Signal<T>>,
    bucket: Bucket,
}

/// Trait representing a process with resource constraints.
///
/// This trait provides methods for estimating the resource required by the process and
/// processing while consuming the resource.
trait ResourceConstraintProcess {
    type ResourceAmountType;

    /// Estimates the amount of resources.
    ///
    /// This method returns an estimation of the amount of resources.
    /// The actual implementation of how the estimation is calculated
    /// should be provided by the implementor of this trait.
    ///
    /// # Returns
    ///
    /// Returns an instance of `Self::ResourceAmountType` that represents
    /// the estimated amount of resources.
    fn estimate_resource(&self) -> Self::ResourceAmountType;

    /// Process and consume a resource.
    ///
    /// This function asynchronously processes and consumes a resource, returning a `Future` that will eventually resolve to the amount of consumed resource.
    /// The consumed resource type is determined by the associated type `ResourceAmountType` of the struct implementing this method.
    ///
    /// # Returns
    ///
    /// An `impl Future` that resolves to the consumed resource amount.
    fn process_and_consume_resource(&self) -> impl Future<Output = Self::ResourceAmountType>;
}

impl<T: ResourceConstraintProcess<ResourceAmountType = f64> + Send + 'static> ThrottledWorker<T> {
    fn new(recv: Receiver<Signal<T>>, bucket: Bucket) -> ThrottledWorker<T> {
        ThrottledWorker { recv, bucket }
    }

    // async fn start(&mut self, estimator: fn(&T) -> f64, process: fn(&T) -> Pin<Box<dyn Future<Output=f64> + Send>>) {
    async fn start(&mut self) {
        while let Some(v) = self.recv.recv().await {
            match v {
                Signal::Close => break,
                Signal::ChangeRefill(refill) => self.bucket.update_refill_rate(refill),
                Signal::ChangeMaxCap(max_cap) => self.bucket.update_max_cap(max_cap),
                Signal::Process(p) => {
                    let estimate = p.estimate_resource();
                    loop {
                        if self.bucket.try_consume(estimate) {
                            break;
                        }
                        tokio::time::sleep_until(self.bucket.estimate_available_at(estimate)).await;
                    }
                    let actual = p.process_and_consume_resource().await;
                    self.bucket.feedback(estimate - actual);
                }
            }
        }
    }
}
#[cfg(test)]
mod test {
    use super::*;
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
        type ResourceAmountType = f64;

        fn estimate_resource(&self) -> Self::ResourceAmountType {
            self.tx.send(Message::Estimated).unwrap();
            return self.estimate;
        }

        fn process_and_consume_resource(&self) -> impl Future<Output = Self::ResourceAmountType> {
            self.tx.send(Message::Consumed).unwrap();
            let v = self.actual;
            return async move { v };
        }
    }

    #[tokio::test]
    async fn test_test_process() {
        let (process, rx) = TestProcess::new(1f64, 2f64);
        assert_eq!(process.estimate_resource(), 1f64);
        assert_timing!(0, 0.1, assert_eq!(rx.recv().unwrap(), Message::Estimated));
        assert_eq!(process.process_and_consume_resource().await, 2f64);
        assert_timing!(0, 0.1, assert_eq!(rx.recv().unwrap(), Message::Consumed));
    }

    #[tokio::test]
    async fn test_throttled_worker() {
        // Initial setup
        let (tx, rx) = channel::<Signal<TestProcess>>(1);
        let mut bucket = Bucket::new(1f64, 1f64);
        bucket.fill();
        // cap = 1
        let mut worker = ThrottledWorker::new(rx, bucket);
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
        tx.send(Signal::ChangeMaxCap(3f64)).await.unwrap();
        let (process, rx) = TestProcess::new(2f64, 2f64);
        tx.send(Signal::Process(process)).await.unwrap();
        assert_timing!(1.9, 2.1, rx.wait_consumed().await);

        // Exit worker
        tx.send(Signal::Close).await.unwrap();
        handle.await.unwrap()
    }
}
