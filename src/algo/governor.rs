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

//! The concurrency-growth decision shared by all executor architectures.
//!
//! The essential control problem is the same everywhere: grow the number of
//! parallel in-flight requests until the measured throughput reaches the
//! effective target, and stop growing as soon as growing stops helping.
//! The pool executors express "concurrency" as the worker count; the
//! task-per-request executor expresses it as the in-flight request cap.
//! This module extracts the decision so every candidate uses the same
//! logic:
//!
//! - grow only when the measured throughput is statistically (3σ) below
//!   the effective target,
//! - do not grow while a previous growth step has not shown a clear
//!   improvement (guards against scaling into a saturated server),
//! - do not grow while congestion control is backing off,
//! - wait a ramp period between steps.

use crate::algo::monitor::{Monitor, Probe};
use itertools::Itertools;
use std::time::Duration;
use tokio::time::Instant;

pub(crate) const NUM_MONITORING_OBSERVATIONS: usize = 256;
pub(crate) const NUM_STATS_OBSERVATIONS: usize = 256;

pub(crate) const MAX_CLIENT_GENERATION_PER_SECOND: f64 = 10.0;

const SIGMA: f64 = 3.0;

const SIGMA_CROSS_AVG: f64 = 2.0;

const SCALE_WAIT_FACTOR: f64 = 3.0;

struct StatDataPoint {
    avg: f64,
    std_dev: f64,
}

/// Decides when the concurrency of an executor should grow, based on
/// observed resource consumption. See the module documentation.
pub(crate) struct ScaleOutGovernor {
    monitor: Monitor<f64>,
    probe: Probe<f64>,
    latest_growth: Instant,
    achieved_throughput: Vec<(usize, StatDataPoint)>,
    prev_throughput_idx: usize,
}

impl ScaleOutGovernor {
    pub(crate) fn new() -> ScaleOutGovernor {
        let (probe, monitor) = Monitor::new(NUM_MONITORING_OBSERVATIONS, NUM_STATS_OBSERVATIONS);
        ScaleOutGovernor {
            monitor,
            probe,
            latest_growth: Instant::now(),
            achieved_throughput: Vec::new(),
            prev_throughput_idx: usize::MAX,
        }
    }

    /// The probe that request executions report their consumption to.
    pub(crate) fn probe(&self) -> Probe<f64> {
        self.probe.clone()
    }

    /// Maximum start-up jitter for spawning `size` clients; also the base of
    /// the ramp wait between growth steps.
    pub(crate) fn jitter_max_secs(size: usize) -> f64 {
        f64::min(1.0, size as f64 / MAX_CLIENT_GENERATION_PER_SECOND)
    }

    fn elapsed_enough_time_to_grow(&self, current_size: usize) -> bool {
        self.latest_growth.elapsed()
            >= Duration::from_secs_f64(SCALE_WAIT_FACTOR * Self::jitter_max_secs(current_size))
    }

    /// Returns true when the concurrency should grow beyond `current_size`.
    pub(crate) fn should_grow(
        &mut self,
        current_size: usize,
        max_size: usize,
        effective_target: f64,
        congested: bool,
    ) -> bool {
        // While throttling has been observed recently, the target is lowered
        // on purpose; growing would push in the wrong direction.
        if congested {
            return false;
        }

        // Wait ramp up time to scale resource consumption
        if !self.elapsed_enough_time_to_grow(current_size) {
            return false;
        }

        // Update monitored metrics based on recent data points
        self.monitor
            .consume_available_data_points_and_update_metrics();

        // Evaluate whether growing is effective to increase resource consumption
        if let (Some(avg), Some(std_dev)) = (self.monitor.avg(), self.monitor.std_dev()) {
            if self.prev_throughput_idx != usize::MAX {
                let prev_throughput = &self.achieved_throughput[self.prev_throughput_idx].1;
                // `>=`, not `>`: with an unbiased estimator a saturated
                // server can reproduce the previous throughput *exactly*
                // (zero deviation), and "exactly the same" means the
                // previous growth had no effect.
                if prev_throughput.avg + prev_throughput.std_dev * SIGMA_CROSS_AVG
                    >= avg - std_dev * SIGMA_CROSS_AVG
                {
                    // Skip the growth decision because the previous growth
                    // did not have enough effect.
                    return false;
                }
            }
        }

        // Grow if resource consumption is not enough
        self.monitor
            .metric_less_than_statistically(effective_target, SIGMA)
            && current_size < max_size
    }

    /// Records that the concurrency grew from `original_size`: memorizes the
    /// throughput achieved at that size and restarts the observation window.
    pub(crate) fn record_growth(&mut self, original_size: usize) {
        self.latest_growth = Instant::now();

        if let (Some(avg), Some(std_dev)) = (self.monitor.avg(), self.monitor.std_dev()) {
            self.achieved_throughput
                .push((original_size, StatDataPoint { avg, std_dev }));
            self.achieved_throughput
                .sort_unstable_by(|l, r| l.0.cmp(&r.0));
            // The below unwrap is always safe because the element inserted in this block
            self.prev_throughput_idx = self
                .achieved_throughput
                .iter()
                .find_position(|x| x.0 == original_size)
                .unwrap()
                .0;
        }

        self.monitor.clear_data_points();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Feeds `n` evenly spaced observations of `value` so that the monitor
    /// computes a stable throughput of roughly `value / interval` per second.
    async fn feed(governor: &ScaleOutGovernor, value: f64, n: usize, interval: Duration) {
        let probe = governor.probe();
        for _ in 0..n {
            tokio::time::sleep(interval).await;
            probe.add_observation(value).expect("probe closed");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_no_growth_without_observations() {
        let mut governor = ScaleOutGovernor::new();
        tokio::time::sleep(Duration::from_secs(10)).await;
        // No data: growing cannot be justified.
        assert!(!governor.should_grow(1, 100, 100.0, false));
    }

    #[tokio::test(start_paused = true)]
    async fn test_grows_when_throughput_is_below_target() {
        let mut governor = ScaleOutGovernor::new();
        // ~10/s measured against a target of 1000/s.
        feed(&governor, 1.0, 50, Duration::from_millis(100)).await;
        assert!(governor.should_grow(1, 100, 1000.0, false));
    }

    #[tokio::test(start_paused = true)]
    async fn test_no_growth_when_target_is_met() {
        let mut governor = ScaleOutGovernor::new();
        // ~10/s measured against a target of 10/s: nothing to gain.
        feed(&governor, 1.0, 50, Duration::from_millis(100)).await;
        assert!(!governor.should_grow(1, 100, 10.0, false));
    }

    #[tokio::test(start_paused = true)]
    async fn test_frozen_while_congested() {
        let mut governor = ScaleOutGovernor::new();
        feed(&governor, 1.0, 50, Duration::from_millis(100)).await;
        assert!(!governor.should_grow(1, 100, 1000.0, true));
    }

    #[tokio::test(start_paused = true)]
    async fn test_capped_at_max_size() {
        let mut governor = ScaleOutGovernor::new();
        feed(&governor, 1.0, 50, Duration::from_millis(100)).await;
        assert!(!governor.should_grow(100, 100, 1000.0, false));
    }

    #[tokio::test(start_paused = true)]
    async fn test_waits_for_the_ramp_period_after_growth() {
        let mut governor = ScaleOutGovernor::new();
        feed(&governor, 1.0, 50, Duration::from_millis(100)).await;
        assert!(governor.should_grow(1, 100, 1000.0, false));
        governor.record_growth(1);

        // Immediately after growing (to size 2): the ramp wait applies
        // (3 × jitter_max(2) = 0.6s).
        feed(&governor, 1.0, 3, Duration::from_millis(100)).await;
        assert!(!governor.should_grow(2, 100, 1000.0, false));
    }

    #[tokio::test(start_paused = true)]
    async fn test_no_further_growth_when_the_previous_step_did_not_help() {
        let mut governor = ScaleOutGovernor::new();
        // Throughput ~10/s at size 1.
        feed(&governor, 1.0, 50, Duration::from_millis(100)).await;
        assert!(governor.should_grow(1, 100, 1000.0, false));
        governor.record_growth(1);

        // After doubling, the throughput stays ~10/s (a saturated server):
        // the effectiveness check must veto further growth despite the
        // target still being far away.
        feed(&governor, 1.0, 50, Duration::from_millis(100)).await;
        assert!(!governor.should_grow(2, 100, 1000.0, false));
    }

    #[tokio::test(start_paused = true)]
    async fn test_grows_again_when_the_previous_step_helped() {
        let mut governor = ScaleOutGovernor::new();
        // Throughput ~10/s at size 1.
        feed(&governor, 1.0, 50, Duration::from_millis(100)).await;
        assert!(governor.should_grow(1, 100, 1000.0, false));
        governor.record_growth(1);

        // After doubling the throughput doubles too (~20/s): keep growing.
        feed(&governor, 2.0, 50, Duration::from_millis(100)).await;
        assert!(governor.should_grow(2, 100, 1000.0, false));
    }
}
