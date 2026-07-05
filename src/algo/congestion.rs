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

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::time::Instant;

/// Multiplicative decrease factor applied to the effective target when
/// congestion (throttling) is detected.
const DECREASE_FACTOR: f64 = 0.5;

/// Minimum time between two consecutive decreases. Throttle events arriving
/// within this window after a decrease are considered part of the same
/// congestion event and do not decrease the target again.
const DECREASE_COOLDOWN: Duration = Duration::from_secs(1);

/// The effective target recovers only after this long without any throttle
/// event, and one recovery step is taken per this period.
///
/// The period is deliberately aligned with the 60-second granularity of
/// CloudWatch metrics: a future slow control loop can consult the metrics
/// between recovery steps. It also implements the policy that co-located
/// production workloads get the room first and the batch workload takes it
/// back slowly (decrease fast, recover slow).
const RECOVERY_CALM_PERIOD: Duration = Duration::from_secs(60);

/// Recovery step per calm period, as a ratio of the *current* effective
/// target (i.e. gentle compounding growth of x1.1 per calm period; a halved
/// target returns to its previous level in about seven minutes).
///
/// The step is deliberately relative to the current level, not to the user
/// ceiling: the ceiling can be far above realistic capacity (it is only an
/// upper bound), and a ceiling-relative step would make the first recovery
/// jump enormous when running at a low effective target.
const RECOVERY_STEP_RATIO: f64 = 0.1;

/// Ratio of the estimated available capacity that an informed recovery may
/// jump to at once. The estimation assumes all capacity unused by other
/// workloads is ours to take, so only half of it is claimed in one step as a
/// safety margin against estimation errors and production traffic changes.
const BOOST_USABLE_RATIO: f64 = 0.5;

/// Micro-unit scale used to accumulate consumed capacity in an atomic counter.
const CONSUMED_UNIT_SCALE: f64 = 1e6;

/// Lock-free counters shared between workers (producers) and the executor
/// (consumer) to observe throttling without additional channels.
#[derive(Debug, Default)]
pub struct CongestionStats {
    requests: AtomicUsize,
    throttled: AtomicUsize,
    /// Cumulative consumed capacity in micro-units (see CONSUMED_UNIT_SCALE).
    consumed_micro: AtomicU64,
}

impl CongestionStats {
    /// Records the outcome of one processed request.
    pub fn record(&self, throttled: bool, consumed: f64) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if throttled {
            self.throttled.fetch_add(1, Ordering::Relaxed);
        }
        if consumed > 0.0 {
            self.consumed_micro
                .fetch_add((consumed * CONSUMED_UNIT_SCALE) as u64, Ordering::Relaxed);
        }
    }

    /// Returns cumulative `(requests, throttled)` counts.
    pub fn snapshot(&self) -> (usize, usize) {
        (
            self.requests.load(Ordering::Relaxed),
            self.throttled.load(Ordering::Relaxed),
        )
    }

    /// Returns the cumulative consumed capacity.
    pub fn consumed(&self) -> f64 {
        self.consumed_micro.load(Ordering::Relaxed) as f64 / CONSUMED_UNIT_SCALE
    }
}

/// A single-value slot carrying the latest safe-target suggestion from the
/// slow control loop (CloudWatch) to the executor. Overwritten by newer
/// suggestions; consumed at most once.
#[derive(Debug)]
pub struct BoostSlot(AtomicU64);

impl Default for BoostSlot {
    fn default() -> Self {
        BoostSlot(AtomicU64::new(f64::NAN.to_bits()))
    }
}

impl BoostSlot {
    /// Stores a new suggestion, replacing any pending one.
    pub fn suggest(&self, target: f64) {
        self.0.store(target.to_bits(), Ordering::Relaxed);
    }

    /// Takes the pending suggestion, leaving the slot empty.
    pub fn take(&self) -> Option<f64> {
        let bits = self.0.swap(f64::NAN.to_bits(), Ordering::Relaxed);
        let value = f64::from_bits(bits);
        if value.is_nan() {
            None
        } else {
            Some(value)
        }
    }
}

/// A lock-free gauge publishing the current effective target so that
/// observers outside the executor (e.g. the benchmark stats emitter) can read
/// it without extra channels. Executors update it whenever the effective
/// target changes.
#[derive(Debug)]
pub struct TargetGauge(AtomicU64);

impl TargetGauge {
    pub fn new(initial: f64) -> TargetGauge {
        TargetGauge(AtomicU64::new(initial.to_bits()))
    }

    pub fn set(&self, target: f64) {
        self.0.store(target.to_bits(), Ordering::Relaxed);
    }

    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// Estimates a safe effective target from the table-level consumption
/// observed via CloudWatch.
///
/// The production (non-batch) consumption is inferred by subtracting our own
/// consumption rate from the table-level consumption rate. The remaining
/// capacity is considered available to the batch workload, and
/// `BOOST_USABLE_RATIO` of it may be claimed at once.
pub fn estimate_safe_target(
    table_capacity: f64,
    table_consumed_rate: f64,
    own_consumed_rate: f64,
) -> f64 {
    let production_rate = (table_consumed_rate - own_consumed_rate).max(0.0);
    let available = (table_capacity - production_rate).max(0.0);
    available * BOOST_USABLE_RATIO
}

/// AIMD (additive-increase / multiplicative-decrease) controller for the
/// effective resource consumption target.
///
/// The user-specified target is a ceiling. When throttling is observed, the
/// effective target is halved so that co-located production workloads take
/// priority. While no throttling is observed, the effective target recovers
/// gradually up to the ceiling. A ceiling of `f64::INFINITY` means "no
/// ceiling": pacing is then driven purely by throttle feedback (and requires
/// a finite initial target, see `with_initial_target`).
///
/// The controller can start below the ceiling when better information is
/// available (e.g. the provisioned capacity of the table); the effective
/// target then also creeps up toward the ceiling through the same gradual
/// recovery, probing for unused capacity.
///
/// This struct is pure logic: time is always injected by the caller so the
/// behavior is fully unit-testable.
#[derive(Debug)]
pub struct AimdController {
    user_target: f64,
    min_target: f64,
    effective_target: f64,
    latest_decrease: Option<Instant>,
    /// The last time a throttle event was observed.
    latest_throttle: Option<Instant>,
    /// The last time a throttle event was observed or the target was changed.
    /// Recovery starts only after a calm period measured from this point.
    calm_since: Instant,
}

impl AimdController {
    /// Creates a controller whose effective target starts at `initial_target`
    /// (clamped into `[min_target, user_target]`) instead of the ceiling.
    /// Use this when known information such as the provisioned capacity gives
    /// a more realistic starting point than the user-specified ceiling.
    pub fn with_initial_target(
        user_target: f64,
        initial_target: f64,
        min_target: f64,
        now: Instant,
    ) -> AimdController {
        assert!(user_target > 0f64);
        assert!(min_target > 0f64);
        assert!(min_target <= user_target);
        let effective_target = initial_target.clamp(min_target, user_target);
        // An infinite effective target could never back off (halving infinity
        // is still infinity), so an unbounded ceiling requires a finite
        // starting point.
        assert!(
            effective_target.is_finite(),
            "an unbounded ceiling requires a finite initial target"
        );
        AimdController {
            user_target,
            min_target,
            effective_target,
            latest_decrease: None,
            latest_throttle: None,
            calm_since: now,
        }
    }

    /// Current effective target. Always within `[min_target, user_target]`.
    pub fn effective_target(&self) -> f64 {
        self.effective_target
    }

    /// Whether a throttle event has been observed recently (within the calm
    /// period). Starting below the ceiling with an initial target does NOT
    /// count as congestion: this signal means "we are actively being pushed
    /// back right now", and is used to freeze scale-out during that time.
    pub fn is_congested(&self, now: Instant) -> bool {
        match self.latest_throttle {
            None => false,
            Some(at) => now.duration_since(at) < RECOVERY_CALM_PERIOD,
        }
    }

    /// Applies an informed upward jump of the effective target, suggested by
    /// the slow control loop based on observed table-level consumption.
    ///
    /// The jump is refused while congestion is ongoing (throttle response
    /// stays conservative), never lowers the target, and never exceeds the
    /// user ceiling. Returns `Some(new_effective_target)` when applied.
    pub fn boost_to(&mut self, safe_target: f64, now: Instant) -> Option<f64> {
        if self.is_congested(now) {
            return None;
        }
        let new_target = safe_target.min(self.user_target);
        if new_target <= self.effective_target {
            return None;
        }
        self.calm_since = now;
        self.effective_target = new_target;
        Some(new_target)
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
            self.latest_throttle = Some(now);
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

        // No throttle observed: recover gradually after a calm period. This
        // also creeps up from a conservative initial target toward the ceiling.
        if self.effective_target < self.user_target
            && now.duration_since(self.calm_since) >= RECOVERY_CALM_PERIOD
        {
            self.calm_since = now;
            let new_target = (self.effective_target + self.effective_target * RECOVERY_STEP_RATIO)
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
    /// A duration slightly shorter/longer than the calm period.
    const JUST_BEFORE_CALM: Duration =
        Duration::from_millis(RECOVERY_CALM_PERIOD.as_millis() as u64 - 100);
    const JUST_AFTER_CALM: Duration =
        Duration::from_millis(RECOVERY_CALM_PERIOD.as_millis() as u64 + 100);

    fn controller(now: Instant) -> AimdController {
        AimdController::with_initial_target(USER_TARGET, USER_TARGET, MIN_TARGET, now)
    }

    #[test]
    fn test_unbounded_ceiling_recovery_is_uncapped() {
        // f64::INFINITY as the user target means "no ceiling": recovery keeps
        // compounding upward forever, driven only by throttle feedback.
        let now = Instant::now();
        let mut c = AimdController::with_initial_target(f64::INFINITY, 100.0, MIN_TARGET, now);
        assert_eq!(c.effective_target(), 100.0);
        let step1 = 100.0 + 100.0 * RECOVERY_STEP_RATIO;
        let t1 = now.add(JUST_AFTER_CALM);
        assert_eq!(c.on_observation(10, 0, t1), Some(step1));
        let step2 = step1 + step1 * RECOVERY_STEP_RATIO;
        let t2 = t1.add(JUST_AFTER_CALM);
        assert_eq!(c.on_observation(10, 0, t2), Some(step2));
        assert!(c.effective_target().is_finite());
    }

    #[test]
    fn test_unbounded_ceiling_throttle_still_halves() {
        let now = Instant::now();
        let mut c = AimdController::with_initial_target(f64::INFINITY, 100.0, MIN_TARGET, now);
        assert_eq!(c.on_observation(10, 1, now), Some(50.0));
        assert!(c.is_congested(now));
    }

    #[test]
    fn test_unbounded_ceiling_boost_is_uncapped() {
        let now = Instant::now();
        let mut c = AimdController::with_initial_target(f64::INFINITY, 100.0, MIN_TARGET, now);
        assert_eq!(c.boost_to(5000.0, now), Some(5000.0));
        assert_eq!(c.effective_target(), 5000.0);
    }

    #[test]
    #[should_panic]
    fn test_unbounded_ceiling_requires_finite_initial_target() {
        // With no ceiling the effective target would start (and stay) at
        // infinity: halving infinity is still infinity, so the controller
        // could never back off. Callers must provide a finite starting point.
        let now = Instant::now();
        AimdController::with_initial_target(f64::INFINITY, f64::INFINITY, MIN_TARGET, now);
    }

    #[test]
    fn test_initial_state_runs_at_user_target() {
        let now = Instant::now();
        let c = controller(now);
        assert_eq!(c.effective_target(), USER_TARGET);
        assert!(!c.is_congested(now));
    }

    #[test]
    fn test_no_throttle_keeps_target_unchanged() {
        let now = Instant::now();
        let mut c = controller(now);
        assert_eq!(c.on_observation(10, 0, now), None);
        // Even after a long calm period, the target does not exceed the ceiling.
        assert_eq!(
            c.on_observation(10, 0, now.add(RECOVERY_CALM_PERIOD * 10)),
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
        assert!(c.is_congested(now));
    }

    #[test]
    fn test_congestion_expires_after_calm_period() {
        let now = Instant::now();
        let mut c = controller(now);
        c.on_observation(10, 1, now);
        assert!(c.is_congested(now.add(JUST_BEFORE_CALM)));
        assert!(!c.is_congested(now.add(JUST_AFTER_CALM)));
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
        let mut c = AimdController::with_initial_target(USER_TARGET, USER_TARGET, 30.0, now);
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
        assert_eq!(c.on_observation(10, 0, now.add(JUST_BEFORE_CALM)), None);
        // Calm for the full period since the throttle event: recover gradually,
        // relative to the current effective target.
        let halved = USER_TARGET * 0.5;
        assert_eq!(
            c.on_observation(10, 0, now.add(JUST_AFTER_CALM)),
            Some(halved + halved * RECOVERY_STEP_RATIO)
        );
    }

    #[test]
    fn test_throttle_within_cooldown_restarts_calm_period() {
        let now = Instant::now();
        let mut c = controller(now);
        c.on_observation(10, 1, now);
        // A throttle at 0.5s does not decrease again, but it proves congestion
        // is ongoing, so the calm period restarts from here.
        let second_throttle = Duration::from_millis(500);
        let t1 = now.add(second_throttle);
        assert_eq!(c.on_observation(10, 1, t1), None);
        // A calm period after the first throttle but not after the second: no recovery.
        let t2 = now.add(JUST_AFTER_CALM);
        assert_eq!(c.on_observation(10, 0, t2), None);
        // A calm period after the second throttle: recovery.
        let t3 = now.add(second_throttle + JUST_AFTER_CALM);
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
            t = t.add(JUST_AFTER_CALM);
            if let Some(v) = c.on_observation(10, 0, t) {
                assert!(v > last);
                assert!(v <= USER_TARGET);
                last = v;
            }
        }
        assert_eq!(c.effective_target(), USER_TARGET);
        // Fully recovered: further calm periods report no change.
        t = t.add(JUST_AFTER_CALM);
        assert_eq!(c.on_observation(10, 0, t), None);
    }

    #[test]
    fn test_congestion_stats_accumulate_consumed_capacity() {
        let stats = CongestionStats::default();
        stats.record(false, 1.5);
        stats.record(true, 2.25);
        stats.record(false, 0.0);
        assert_eq!(stats.snapshot(), (3, 1));
        assert!((stats.consumed() - 3.75).abs() < 1e-5);
    }

    #[test]
    fn test_target_gauge_publishes_latest_value() {
        let gauge = TargetGauge::new(5.0);
        assert_eq!(gauge.get(), 5.0);
        gauge.set(7.5);
        assert_eq!(gauge.get(), 7.5);
    }

    #[test]
    fn test_boost_slot_keeps_latest_suggestion() {
        let slot = BoostSlot::default();
        assert_eq!(slot.take(), None);
        slot.suggest(5.0);
        slot.suggest(7.0);
        assert_eq!(slot.take(), Some(7.0));
        assert_eq!(slot.take(), None);
    }

    #[test]
    fn test_estimate_safe_target() {
        // No other workload: half of the capacity may be claimed at once.
        assert_eq!(estimate_safe_target(10.0, 0.0, 0.0), 5.0);
        // Table consumes 7, we consume 3 of it: production is 4, available is 6.
        assert_eq!(estimate_safe_target(10.0, 7.0, 3.0), 3.0);
        // Our own consumption exceeding the table-level observation (window
        // misalignment) must not produce a negative production estimate.
        assert_eq!(estimate_safe_target(10.0, 3.0, 5.0), 5.0);
        // Production consuming more than the capacity (burst): nothing available.
        assert_eq!(estimate_safe_target(10.0, 25.0, 5.0), 0.0);
    }

    #[test]
    fn test_boost_raises_target_when_calm() {
        let now = Instant::now();
        let mut c = controller(now);
        c.on_observation(10, 1, now);
        // The congestion has expired; an informed jump is allowed.
        let t1 = now.add(JUST_AFTER_CALM);
        assert_eq!(c.boost_to(80.0, t1), Some(80.0));
        assert_eq!(c.effective_target(), 80.0);
    }

    #[test]
    fn test_boost_is_refused_while_congested() {
        let now = Instant::now();
        let mut c = controller(now);
        c.on_observation(10, 1, now);
        // Throttle response stays conservative: no jump during congestion.
        let t1 = now.add(Duration::from_secs(5));
        assert_eq!(c.boost_to(80.0, t1), None);
        assert_eq!(c.effective_target(), USER_TARGET * 0.5);
    }

    #[test]
    fn test_boost_is_capped_at_user_target() {
        let now = Instant::now();
        let mut c = AimdController::with_initial_target(USER_TARGET, 40.0, MIN_TARGET, now);
        assert_eq!(c.boost_to(1000.0, now), Some(USER_TARGET));
    }

    #[test]
    fn test_boost_never_lowers_target() {
        let now = Instant::now();
        let mut c = AimdController::with_initial_target(USER_TARGET, 40.0, MIN_TARGET, now);
        assert_eq!(c.boost_to(30.0, now), None);
        assert_eq!(c.boost_to(40.0, now), None);
        assert_eq!(c.effective_target(), 40.0);
    }

    #[test]
    fn test_initial_target_starts_below_ceiling() {
        let now = Instant::now();
        let c = AimdController::with_initial_target(USER_TARGET, 40.0, MIN_TARGET, now);
        assert_eq!(c.effective_target(), 40.0);
        // Starting low with known information is not congestion.
        assert!(!c.is_congested(now));
    }

    #[test]
    fn test_initial_target_is_clamped() {
        let now = Instant::now();
        let c = AimdController::with_initial_target(USER_TARGET, 1000.0, MIN_TARGET, now);
        assert_eq!(c.effective_target(), USER_TARGET);
        let c = AimdController::with_initial_target(USER_TARGET, 0.001, 5.0, now);
        assert_eq!(c.effective_target(), 5.0);
    }

    #[test]
    fn test_initial_target_creeps_up_toward_ceiling_while_calm() {
        let now = Instant::now();
        let mut c = AimdController::with_initial_target(USER_TARGET, 40.0, MIN_TARGET, now);
        // Probing for unused capacity: one recovery step per calm period.
        let t1 = now.add(JUST_AFTER_CALM);
        assert_eq!(
            c.on_observation(10, 0, t1),
            Some(40.0 + 40.0 * RECOVERY_STEP_RATIO)
        );
        // A throttle then halves from the current effective target as usual.
        let t2 = t1.add(Duration::from_secs(1));
        assert_eq!(c.on_observation(10, 1, t2), Some(22.0));
    }
}
