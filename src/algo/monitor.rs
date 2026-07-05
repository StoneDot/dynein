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

use std::collections::VecDeque;
use tokio::sync::mpsc::error::SendError;
use tokio::time::Instant;

#[derive(Clone, Debug, PartialEq)]
pub struct DataPoint<T> {
    timestamp: Instant,
    data: T,
}

#[derive(Clone, Debug)]
pub struct Probe<T> {
    tx: tokio::sync::mpsc::UnboundedSender<DataPoint<T>>,
}

impl<T> Probe<T>
where
    T: Clone + Into<f64>,
{
    pub fn add_observation(&self, value: T) -> Result<(), SendError<DataPoint<T>>> {
        self.add_observation_with_time(value, Instant::now())
    }

    fn add_observation_with_time(
        &self,
        value: T,
        at: Instant,
    ) -> Result<(), SendError<DataPoint<T>>> {
        self.tx.send(DataPoint {
            data: value,
            timestamp: at,
        })
    }
}

#[derive(Debug)]
pub struct Monitor<T>
where
    T: Copy + Into<f64>,
{
    observations: VecDeque<DataPoint<T>>,
    stat_points: VecDeque<f64>,
    max_recordable_observations: usize,
    max_stat_points: usize,
    rx: tokio::sync::mpsc::UnboundedReceiver<DataPoint<T>>,
}

impl<T> Monitor<T>
where
    T: Copy + Into<f64> + PartialEq,
{
    pub fn new(
        max_recordable_observations: usize,
        max_stat_points: usize,
    ) -> (Probe<T>, Monitor<T>) {
        assert!(max_recordable_observations >= 3);
        assert!(max_stat_points >= 3);

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let probe = Probe { tx };
        let monitor = Monitor {
            observations: VecDeque::with_capacity(max_recordable_observations),
            stat_points: VecDeque::with_capacity(max_stat_points),
            max_recordable_observations,
            max_stat_points,
            rx,
        };
        (probe, monitor)
    }

    pub fn metric_less_than_statistically(&mut self, target: f64, sigma: f64) -> bool {
        if let (Some(std_dev), Some(avg)) = (self.std_dev(), self.avg()) {
            avg + sigma * std_dev < target
        } else {
            false
        }
    }

    pub fn clear_data_points(&mut self) {
        self.stat_points.clear();
        self.observations.clear();
    }

    pub fn consume_available_data_points_and_update_metrics(&mut self) {
        while let Ok(data_point) = self.rx.try_recv() {
            if self.observations.len() == self.max_recordable_observations {
                self.observations.pop_back();
            }
            self.observations.push_front(data_point);
            self.observe_stat()
        }
    }

    fn observe_stat(&mut self) {
        if let Some(std_dev) = self.average_per_second() {
            if self.stat_points.len() == self.max_stat_points {
                self.stat_points.pop_back();
            }
            self.stat_points.push_front(std_dev);
        }
    }

    pub fn avg(&self) -> Option<f64> {
        // Skip calculation if there is no enough data to calculate average
        if self.stat_points.is_empty() {
            return None;
        }

        Some(self.stat_points.iter().sum::<f64>() / self.stat_points.len() as f64)
    }

    pub fn std_dev(&self) -> Option<f64> {
        // Skip calculation if there is no enough data to calculate standard deviation
        if self.stat_points.len() <= 1 {
            return None;
        }

        // Calculate standard deviation
        let avg = self.avg()?;
        let pow2_sum: f64 = self
            .stat_points
            .iter()
            .map(|v| {
                let d = v - avg;
                d * d
            })
            .sum();
        Some(f64::sqrt(pow2_sum / (self.stat_points.len() - 1) as f64))
    }

    fn average_per_second(&self) -> Option<f64> {
        if self.observations.len() <= 1 {
            return None;
        }

        let since = self.observations.back().unwrap(); // always safe
        let latest = self.observations.front().unwrap(); // always safe
                                                         // The oldest observation only defines when the window starts; its
                                                         // value did not arrive within the window. Counting it too would
                                                         // inflate the rate by n/(n-1) (2x for the smallest window), which
                                                         // destabilizes the scale-out governor's throughput comparisons.
        let sum: f64 = self
            .observations
            .iter()
            .take(self.observations.len() - 1)
            .map(|v| v.data.into())
            .sum();
        if since == latest {
            None
        } else {
            Some(
                sum / latest
                    .timestamp
                    .duration_since(since.timestamp)
                    .as_secs_f64(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Add;
    use std::time::Duration;

    macro_rules! assert_delta {
        ($x:expr, $y:expr, $d:expr) => {
            if f64::abs($x - $y) >= $d {
                panic!(
                    "{} ({}) is not equal {} ({}) with delta {}",
                    $x,
                    stringify!($x),
                    $y,
                    stringify!($y),
                    $d
                );
            }
        };
    }

    const DELTA: f64 = 0.000001;

    // NOTE (2026-07-05): the expectations were rewritten when the fencepost
    // bug in average_per_second was fixed. The old estimator divided the sum
    // of ALL n window values by the (n-1) intervals spanning them, inflating
    // the rate by n/(n-1) — 2x for the smallest window. The inflated,
    // phase-dependent bias made the scale-out governor's effectiveness
    // comparison unreliable (a genuine doubling of throughput could drown in
    // the inflated variance). The estimator now excludes the oldest value:
    // observations that arrived after the window started, divided by the
    // window duration.
    #[test]
    fn test_monitor() {
        let (probe, mut monitor) = Monitor::new(32, 32);

        let first_observation = Instant::now();
        probe
            .add_observation_with_time(12, first_observation)
            .expect("failed to insert observation");
        monitor.consume_available_data_points_and_update_metrics();
        assert!(!monitor.metric_less_than_statistically(40.0, 1.0));
        assert_eq!(monitor.avg(), None);
        assert_eq!(monitor.std_dev(), None);

        // 10 units arrived in the 0.5s since the window started: 20/s.
        let second_observation = first_observation.add(Duration::from_millis(500));
        probe
            .add_observation_with_time(10, second_observation)
            .expect("failed to insert observation");
        monitor.consume_available_data_points_and_update_metrics();
        assert!(!monitor.metric_less_than_statistically(40.0, 1.0));
        assert_eq!(monitor.avg(), Some(20.0));
        assert_eq!(monitor.std_dev(), None);

        // 20 more units over 1.0s: still exactly 20/s; the deviation is zero,
        // so the metric is now statistically below the 40/s target.
        let third_observation = first_observation.add(Duration::from_millis(1000));
        probe
            .add_observation_with_time(10, third_observation)
            .expect("failed to insert observation");
        monitor.consume_available_data_points_and_update_metrics();
        assert!(monitor.metric_less_than_statistically(40.0, 1.0));
        assert_eq!(monitor.avg(), Some(20.0));
        assert_delta!(monitor.std_dev().unwrap(), 0.0, DELTA);

        // A steady 20/s stream stays at 20/s with zero deviation.
        let forth_observation = first_observation.add(Duration::from_millis(1500));
        probe
            .add_observation_with_time(10, forth_observation)
            .expect("failed to insert observation");
        monitor.consume_available_data_points_and_update_metrics();
        assert!(monitor.metric_less_than_statistically(40.0, 1.0));
        assert_eq!(monitor.avg(), Some(20.0));
        assert_delta!(monitor.std_dev().unwrap(), 0.0, DELTA);

        // The metric is not below a target it actually meets.
        assert!(!monitor.metric_less_than_statistically(19.0, 1.0));
    }

    #[test]
    fn test_average_per_second_has_no_small_window_bias() {
        // 1.0 every 100ms is 10/s; the estimate must say so already for the
        // smallest window instead of the fencepost-inflated 20/s.
        let (probe, mut monitor) = Monitor::new(32, 32);
        let t0 = Instant::now();
        probe
            .add_observation_with_time(1.0, t0)
            .expect("failed to insert observation");
        probe
            .add_observation_with_time(1.0, t0.add(Duration::from_millis(100)))
            .expect("failed to insert observation");
        monitor.consume_available_data_points_and_update_metrics();
        assert_delta!(monitor.avg().unwrap(), 10.0, DELTA);
    }
}
