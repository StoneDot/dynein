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

use std::time::Duration;
use tokio::time::Instant;

// TODO: implement holistic bucket for tables including GSI capacity.

#[derive(PartialEq, Debug, Clone)]
pub struct Bucket {
    max_cap: f64,
    refill_per_sec: f64,
    cap: f64,
    last_filled: Instant,
}

macro_rules! when_debug {
    ($b:block) => {
        if cfg!(debug_assertions) $b
    };
    ($s:stmt) => {
        if cfg!(debug_assertions) { $s }
    };
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

    #[cfg(debug_assertions)]
    fn inspect_internal_state(&self) {
        assert!(self.max_cap >= 0f64);
        assert!(self.max_cap <= f64::MAX);
        assert!(self.refill_per_sec > 0f64);
        assert!(self.refill_per_sec <= f64::MAX);
        assert!(self.cap <= self.max_cap);
    }

    pub fn fill(&mut self) {
        self.cap = self.max_cap;
        self.last_filled = Instant::now();
        when_debug!(self.inspect_internal_state());
    }

    pub fn is_sufficient(&self, amount: f64) -> bool {
        self.cap >= amount || self.cap == self.max_cap
    }

    pub fn update_refill_rate(&mut self, refill_per_sec: f64) {
        assert!(refill_per_sec > 0f64);
        assert!(refill_per_sec <= f64::MAX);
        self.refill_per_sec = refill_per_sec;
        when_debug!(self.inspect_internal_state());
    }

    pub fn update_max_cap(&mut self, max_cap: f64) {
        assert!(max_cap >= 0f64);
        assert!(max_cap <= f64::MAX);
        self.max_cap = max_cap;
        self.cap = self.cap.min(self.max_cap);
        when_debug!(self.inspect_internal_state());
    }

    fn refill(&mut self) {
        let cur = Instant::now();
        let elapsed = cur.duration_since(self.last_filled);
        let refill_amount = self.refill_per_sec * elapsed.as_secs_f64();
        let cap = self.cap;
        self.cap = (cap + refill_amount).min(self.max_cap).max(cap);
        self.last_filled = cur;
        when_debug!(self.inspect_internal_state());
    }

    pub fn estimate_available_at(&self, amount: f64) -> Instant {
        if self.is_sufficient(amount) {
            Instant::now()
        } else {
            Instant::now()
                + Duration::from_secs_f64(
                    (amount.min(self.max_cap) - self.cap) / self.refill_per_sec,
                )
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
            when_debug!(self.inspect_internal_state());
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
        when_debug!(self.inspect_internal_state());
    }
}
