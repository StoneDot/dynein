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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Multiplicative decrease factor applied to the effective target when
/// congestion (throttling) is detected.
const DECREASE_FACTOR: f64 = 0.5;

/// Minimum time between two consecutive decreases. Throttle events arriving
/// within this window after a decrease are considered part of the same
/// congestion event and do not decrease the target again.
const DECREASE_COOLDOWN: Duration = Duration::from_secs(1);

/// The effective target recovers only after this long without any throttle
/// event. Co-located production workloads get the room first; the batch
/// workload takes it back gradually.
const RECOVERY_CALM_PERIOD: Duration = Duration::from_secs(2);

/// Additive recovery step per calm period, as a ratio of the user target.
/// With 0.1, a fully halved target returns to 100% after several calm periods.
const RECOVERY_STEP_RATIO: f64 = 0.1;

/// Lock-free counters shared between workers (producers) and the executor
/// (consumer) to observe throttling without additional channels.
#[derive(Debug, Default)]
pub struct CongestionStats {
    requests: AtomicUsize,
    throttled: AtomicUsize,
}

impl CongestionStats {
    /// Records the outcome of one processed request.
    pub fn record(&self, throttled: bool) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if throttled {
            self.throttled.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Returns cumulative `(requests, throttled)` counts.
    pub fn snapshot(&self) -> (usize, usize) {
        (
            self.requests.load(Ordering::Relaxed),
            self.throttled.load(Ordering::Relaxed),
        )
    }
}

/// AIMD (additive-increase / multiplicative-decrease) controller for the
/// effective resource consumption target.
///
/// The user-specified target is a ceiling. When throttling is observed, the
/// effective target is halved so that co-located production workloads take
/// priority. While no throttling is observed, the effective target recovers
/// additively up to the ceiling.
///
/// This struct is pure logic: time is always injected by the caller so the
/// behavior is fully unit-testable.
#[derive(Debug)]
pub struct AimdController {
    user_target: f64,
    min_target: f64,
    effective_target: f64,
    latest_decrease: Option<Instant>,
    /// The last time a throttle event was observed or the target was changed.
    /// Recovery starts only after a calm period measured from this point.
    calm_since: Instant,
}

impl AimdController {
    pub fn new(user_target: f64, min_target: f64, now: Instant) -> AimdController {
        assert!(user_target > 0f64);
        assert!(min_target > 0f64);
        assert!(min_target <= user_target);
        AimdController {
            user_target,
            min_target,
            effective_target: user_target,
            latest_decrease: None,
            calm_since: now,
        }
    }

    /// Current effective target. Always within `[min_target, user_target]`.
    pub fn effective_target(&self) -> f64 {
        self.effective_target
    }

    /// Whether the controller is currently backing off (effective < user target).
    pub fn is_congested(&self) -> bool {
        self.effective_target < self.user_target
    }

    /// Feeds the number of requests observed since the last call and how many
    /// of them were throttled. Returns `Some(new_effective_target)` when the
    /// effective target changes, `None` otherwise.
    pub fn on_observation(
        &mut self,
        requests: usize,
        throttled: usize,
        now: Instant,
    ) -> Option<f64> {
        let _ = requests;
        if throttled > 0 {
            // Congestion is ongoing: the recovery clock restarts even when the
            // cooldown suppresses a further decrease.
            self.calm_since = now;

            let cooldown_elapsed = match self.latest_decrease {
                None => true,
                Some(at) => now.duration_since(at) >= DECREASE_COOLDOWN,
            };
            if !cooldown_elapsed {
                return None;
            }
            self.latest_decrease = Some(now);

            let new_target = (self.effective_target * DECREASE_FACTOR).max(self.min_target);
            if new_target == self.effective_target {
                // Already at the floor.
                return None;
            }
            self.effective_target = new_target;
            return Some(new_target);
        }

        // No throttle observed: recover additively after a calm period.
        if self.is_congested() && now.duration_since(self.calm_since) >= RECOVERY_CALM_PERIOD {
            self.calm_since = now;
            let new_target = (self.effective_target + self.user_target * RECOVERY_STEP_RATIO)
                .min(self.user_target);
            self.effective_target = new_target;
            return Some(new_target);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Add;

    const USER_TARGET: f64 = 100.0;
    const MIN_TARGET: f64 = 1.0;

    fn controller(now: Instant) -> AimdController {
        AimdController::new(USER_TARGET, MIN_TARGET, now)
    }

    #[test]
    fn test_initial_state_runs_at_user_target() {
        let now = Instant::now();
        let c = controller(now);
        assert_eq!(c.effective_target(), USER_TARGET);
        assert!(!c.is_congested());
    }

    #[test]
    fn test_no_throttle_keeps_target_unchanged() {
        let now = Instant::now();
        let mut c = controller(now);
        assert_eq!(c.on_observation(10, 0, now), None);
        // Even after a long calm period, the target does not exceed the ceiling.
        assert_eq!(
            c.on_observation(10, 0, now.add(Duration::from_secs(60))),
            None
        );
        assert_eq!(c.effective_target(), USER_TARGET);
    }

    #[test]
    fn test_throttle_halves_target() {
        let now = Instant::now();
        let mut c = controller(now);
        assert_eq!(c.on_observation(10, 1, now), Some(USER_TARGET * 0.5));
        assert_eq!(c.effective_target(), USER_TARGET * 0.5);
        assert!(c.is_congested());
    }

    #[test]
    fn test_throttles_within_cooldown_do_not_decrease_again() {
        let now = Instant::now();
        let mut c = controller(now);
        assert_eq!(c.on_observation(10, 1, now), Some(USER_TARGET * 0.5));
        // 0.5s later: still within the cooldown; same congestion event.
        let t1 = now.add(Duration::from_millis(500));
        assert_eq!(c.on_observation(10, 3, t1), None);
        assert_eq!(c.effective_target(), USER_TARGET * 0.5);
    }

    #[test]
    fn test_throttle_after_cooldown_decreases_again() {
        let now = Instant::now();
        let mut c = controller(now);
        c.on_observation(10, 1, now);
        let t1 = now.add(Duration::from_millis(1500));
        assert_eq!(c.on_observation(10, 1, t1), Some(USER_TARGET * 0.25));
    }

    #[test]
    fn test_decrease_is_clipped_at_min_target() {
        let now = Instant::now();
        let mut c = AimdController::new(USER_TARGET, 30.0, now);
        assert_eq!(c.on_observation(10, 1, now), Some(50.0));
        let t1 = now.add(Duration::from_secs(2));
        assert_eq!(c.on_observation(10, 1, t1), Some(30.0));
        // Already at the floor: no further change is reported.
        let t2 = now.add(Duration::from_secs(4));
        assert_eq!(c.on_observation(10, 1, t2), None);
        assert_eq!(c.effective_target(), 30.0);
    }

    #[test]
    fn test_recovery_after_calm_period() {
        let now = Instant::now();
        let mut c = controller(now);
        c.on_observation(10, 1, now);
        // Not calm long enough yet.
        let t1 = now.add(Duration::from_millis(1900));
        assert_eq!(c.on_observation(10, 0, t1), None);
        // Calm for the full period since the throttle event: recover additively.
        let t2 = now.add(Duration::from_millis(2100));
        assert_eq!(
            c.on_observation(10, 0, t2),
            Some(USER_TARGET * 0.5 + USER_TARGET * RECOVERY_STEP_RATIO)
        );
    }

    #[test]
    fn test_throttle_within_cooldown_restarts_calm_period() {
        let now = Instant::now();
        let mut c = controller(now);
        c.on_observation(10, 1, now);
        // A throttle at 0.5s does not decrease again, but it proves congestion
        // is ongoing, so the calm period restarts from here.
        let t1 = now.add(Duration::from_millis(500));
        assert_eq!(c.on_observation(10, 1, t1), None);
        // 2s after the first throttle but only 1.5s after the second: no recovery.
        let t2 = now.add(Duration::from_millis(2000));
        assert_eq!(c.on_observation(10, 0, t2), None);
        // 2s after the second throttle: recovery.
        let t3 = now.add(Duration::from_millis(2600));
        assert!(c.on_observation(10, 0, t3).is_some());
    }

    #[test]
    fn test_recovery_is_capped_at_user_target() {
        let now = Instant::now();
        let mut c = controller(now);
        c.on_observation(10, 1, now);
        // Recover repeatedly with calm periods.
        let mut t = now;
        let mut last = c.effective_target();
        for _ in 0..20 {
            t = t.add(Duration::from_secs(3));
            if let Some(v) = c.on_observation(10, 0, t) {
                assert!(v > last);
                assert!(v <= USER_TARGET);
                last = v;
            }
        }
        assert_eq!(c.effective_target(), USER_TARGET);
        assert!(!c.is_congested());
        // Fully recovered: further calm periods report no change.
        t = t.add(Duration::from_secs(3));
        assert_eq!(c.on_observation(10, 0, t), None);
    }
}
