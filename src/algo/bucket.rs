use std::time::Duration;
use tokio::time::Instant;

pub struct Bucket {
    max_cap: f64,
    refill_per_sec: f64,
    cap: f64,
    last_filled: Instant,
}

impl Bucket {
    pub fn new(max_cap: f64, refill_per_sec: f64) -> Bucket {
        assert!(max_cap >= 0f64);
        assert!(max_cap <= f64::MAX);
        assert!(refill_per_sec > 0f64);
        assert!(refill_per_sec <= f64::MAX);
        Bucket {
            max_cap,
            refill_per_sec,
            cap: 0f64,
            last_filled: Instant::now(),
        }
    }

    pub fn fill(&mut self) {
        self.cap = self.max_cap;
        self.last_filled = Instant::now();
    }

    pub fn is_sufficient(&self, amount: f64) -> bool {
        self.cap >= amount || self.cap == self.max_cap
    }

    pub fn update_refill_rate(&mut self, refill_per_sec: f64) {
        assert!(refill_per_sec > 0f64);
        assert!(refill_per_sec <= f64::MAX);
        self.refill_per_sec = refill_per_sec;
    }

    pub fn update_max_cap(&mut self, max_cap: f64) {
        assert!(max_cap >= 0f64);
        assert!(max_cap <= f64::MAX);
        self.max_cap = max_cap;
    }

    fn refill(&mut self) {
        let cur = Instant::now();
        let elapsed = cur.duration_since(self.last_filled);
        let refill_amount = self.refill_per_sec * elapsed.as_secs_f64();
        self.cap = (self.cap + refill_amount).min(self.max_cap);
        self.last_filled = cur;
    }

    pub fn estimate_available_at(&self, amount: f64) -> Instant {
        if self.is_sufficient(amount) {
            Instant::now()
        } else {
            Instant::now() + Duration::from_secs_f64((amount.min(self.max_cap) - self.cap) / self.refill_per_sec)
        }
    }

    /// Try to consume a given amount from the bucket.
    ///
    /// This method will attempt to consume a specified amount from the bucket.
    ///
    /// # Arguments
    ///
    /// * `amount` - The amount to consume from the bucket.
    ///
    /// # Returns
    ///
    /// A boolean indicating whether the consumption was successful or not.
    /// `true` if the consumption was successful, `false` if the container is empty.
    pub fn try_consume(&mut self, amount: f64) -> bool {
        self.refill();
        if self.is_sufficient(amount) {
            self.cap -= amount;
            true
        } else {
            false
        }
    }

    /// Adjusts the capacity of the feedback mechanism.
    ///
    /// This method takes in a floating-point value `adjust` and adjusts the capacity
    /// of the feedback mechanism by adding the value to the current capacity.
    ///
    /// # Arguments
    ///
    /// * `adjust` - The value by which to adjust the capacity. You can calculate it by `estimate - actual`.
    pub fn feedback(&mut self, adjust: f64) {
        self.cap += adjust;
    }
}
