# Watchdog Silence Postmortem — Stage 1 pool1 Wedge

**Date**: 2026-07-07 (run 20260707-152019, Stage 1)
**Incident**: pool1 (candidate A') deadlocked on w10-mixed cells. The dy
process stopped making progress (~700s in), but the off-instance watchdog
ran for its full duration without detecting or aborting. The in-process
`timeout` (budget × 2 = 1400s) eventually killed the process.

**Impact**: no data loss (timeout worked), but the wedged cells consumed
wall-clock time and EC2 cost that a watchdog abort could have saved. More
importantly, the watchdog's purpose is to be the safety net when the
operator is absent — if a similar stall happened on an expensive (quota)
table, the watchdog would have been equally silent while billing accrued.

## Root cause: three independent detection mechanisms were structurally blind

### 1. Stall detection skips cheap tables

```bash
# watchdog.sh line 249
if [ "$wcu" -lt "$EXPENSIVE_WCU" ]; then continue; fi
```

`EXPENSIVE_WCU` defaults to 1000. All Stage 1 tables were w10. The stall
detector — the most direct indicator of "table exists but nothing is being
written" — never even looked at them. This was an intentional design
choice (CloudWatch has 1-minute granularity; 10 WCU consumed at 0.5 WCU/s
is noisy and low-signal) but the intention was wrong: a stalled cheap
table still costs EC2 time, and detecting zero consumption is reliable at
any WCU.

**Fix (F1)**: add a zero-consumption check for ALL tables, independent of
the existing threshold-based check. The threshold check (< 5% of
provisioned) can keep the `EXPENSIVE_WCU` gate — at low WCU, CloudWatch
noise makes fractional-threshold detection unreliable. But zero (or
near-zero) consumption is unambiguous at any WCU.

No phase conditioning is needed: no legitimate phase keeps a table alive
with zero consumption for 5+ minutes. Table creation takes seconds,
upload takes seconds, and the table is deleted immediately after.
CloudWatch's 1-2 minute reporting lag is absorbed by the 5-minute
strike window (`STALL_MINUTES`).

### 2. External cell deadline never fires before in-process timeout

```bash
# watchdog.sh line 291
limit=$(( ${CELL_BUDGET[$cell]} * 2 + 300 ))
```
```bash
# run_cells.sh line 354
timeout --kill-after=30 "$((budget * 2))" ...
```

The in-process timeout fires at `budget * 2` seconds. The watchdog's
external deadline fires at `budget * 2 + 300` seconds. The difference
(300s = 5 watchdog ticks) means the in-process timeout always wins in
normal operation. The external deadline exists to catch exactly the case
where "the on-instance timeout may itself be dead" (the comment in
watchdog.sh says so).

**Analysis**: this gap is intentional and correct. The two mechanisms have
fundamentally different blast radii:

- **In-process timeout** (`timeout` in run_cells.sh): kills ONE cell's dy
  process. The runner continues to the next cell. Only the affected rep
  gets exit=124; all other reps/cells proceed normally.
- **External deadline** → `abort_run()` in watchdog.sh: deletes ALL
  tables of the run, stops ALL instances. The entire run is terminated.

The external deadline is a last-resort backstop for when the in-process
`timeout` itself is dead (kernel hang, zombie process, OOM of the entire
cgroup). The 300s gap gives the runner time to clean up after a timed-out
cell (kill samplers, write result.json, upload artifacts, delete table)
before the watchdog concludes the timeout mechanism itself failed.

Earlier detection of wedged cells — the actual gap this incident exposed
— should come from F1 (zero-consumption stall) and F2 (progress counter),
which provide faster and more targeted signals without the full-run-abort
blast radius. **No changes needed to the external deadline.**

### 3. Heartbeat proves liveness, not progress

```bash
# run_cells.sh line 121–135
heartbeat_loop() {
    while true; do
        # reads phase.json, stamps ts, uploads to S3
        sleep 60
    done
}
heartbeat_loop &
```

The heartbeat loop runs as an independent background process. It reads
`phase.json` and uploads it every 60s regardless of whether the dy process
is making progress. A wedged dy has no effect on heartbeat freshness — the
heartbeat will report "phase: import" with a fresh timestamp forever.

The watchdog checks heartbeat age (line 279: `age > HEARTBEAT_MAX_AGE`)
and alarms on staleness. But a wedged runner produces a perfectly fresh
heartbeat every minute, so the watchdog's age check passes.

**This is exactly the "liveness not health" antipattern that
experiment-protocol §4 warns against**: "A monitor that only watches
liveness cannot distinguish silence from progress."

**Fix (F2)**: the heartbeat must include a **progress counter** that
advances only when actual work completes. The watchdog then checks not
just "is the heartbeat fresh?" but "has the progress counter advanced
since the last tick?". A fresh heartbeat with a frozen progress counter
is a definitive stall signal.

**Implementation: stats-based** (read `DYNEIN_BENCH_STATS`, not a new
file). The stats emitter (`DYNEIN_BENCH_STATS=<path>`) already writes
per-second JSONL; the progress counter is its **`resolved_items`** field
(complete + failed items — the same quantity dy's own in-process
`StallDetector` watches, so the two share semantics). The heartbeat loop
(`run_cells.sh`) reads the last stats line for the current cell/rep and
attaches `resolved_items` as `progress`. No dy binary changes needed.
(An earlier draft named the field `total_requests`; the emitter has no
such field, and `requests` advances on retries even when nothing resolves,
so `resolved_items` is the correct "work completed" signal.)

**Threshold — must fire LATER than dy's in-process StallDetector, not
earlier.** dy already aborts a stalled import in-process at
`STALL_DEADLINE = 300s` of frozen `resolved_items` (`transfer.rs`), and
that abort has a *single-cell* blast radius: the rep fails, the runner
moves on. F2 → `abort_run` kills the *whole run*. So F2 must not preempt
dy's own narrow recovery — it is a backstop for the case dy's recovery
*itself* wedges. That is precisely what happened in run 152019: the
monitoring task detected the stall and fired at ~300s, but its remediation
(`executor_handle.await` after `terminate`) never returns while the pool1
dispatcher is deadlocked, so the process hung — fresh heartbeat, zero WCU —
until the in-process `timeout` SIGKILLed it at budget×2 = 1400s. Recovery
detected, recovery could not execute.

Therefore `--progress-frozen-strikes` defaults to **6** (360s at the 60s
interval), safely past the 300s in-process window: for a *recoverable*
stall dy exits/advances the cell before strike 6 and F2 resets; only a
*wedged* recovery (phase stuck at `import`, progress frozen past 300s)
reaches the strike limit and trips the whole-run abort — turning a
1400s-until-SIGKILL wait into a ~360s watchdog abort.

The concern about "stats loop coupled to the async runtime" is overblown
for the actual failure modes:
- **Pool1 deadlock**: only the dispatcher task is blocked on
  `notified().await`. The tokio runtime is not wedged — other tasks
  (stats emitter) continue running. The stats file keeps updating with
  a frozen `total_requests`. The watchdog detects the freeze.
- **OOM kill**: the dy process is dead, so the stats file stops updating
  entirely. The heartbeat loop (separate bash process, outside the
  systemd scope) survives and reports a stale counter. Detected.

**Phase-awareness is critical**: the progress counter is legitimately
frozen during non-import phases:
- **create-table retry loop**: up to 30 × 60s = 30 minutes for quota
  exhaustion. No import is running; `total_requests` is zero or stale
  from the previous cell.
- **pregen phase**: multi-GB input generation, no import.
- **upload phase**: dy has exited, artifacts being uploaded.

The watchdog must only alarm on frozen progress when the heartbeat's
`phase` field is `import`. During other phases, a frozen counter is
expected and must not trigger strikes.

## Detection timeline reconstruction

| Time (approx) | Event | Watchdog signal |
|---|---|---|
| T+0s | pool1 cell starts (w10-mixed) | heartbeat: "phase: import" |
| T+700s | dispatcher↔worker deadlock (shared control/work queue) | dy stops writing |
| T+700..1400s | dy is wedged, 0 WCU consumed | **stall**: skipped (w10 < 1000) |
| | | **heartbeat**: fresh every 60s (loop is independent) |
| | | **deadline**: limit=1700s not yet reached |
| T+1000s | in-process StallDetector fires (300s frozen), sends terminate | but `executor_handle.await` hangs on the deadlocked pool — recovery detected, cannot execute |
| T+1400s | in-process `timeout` kills dy | exit code 124 |
| T+1700s | external deadline would have fired | **never reached** |

## Compound failure: all three are needed simultaneously

With the fixes, detection/abort would improve as follows:
- F1 (zero-consumption stall on all tables): strikes from ~T+760s (CloudWatch
  lag) and aborts at 5 consecutive checks ≈ T+1060s.
- F2 (progress-frozen heartbeat, phase-aware): strikes from stall onset and
  aborts at 6 consecutive import checks ≈ T+1060s. This is deliberately
  *not* earlier than dy's 300s in-process StallDetector (which fires ≈T+1000s
  but cannot execute its recovery here) — F2's whole-run abort must stay a
  backstop for wedged recovery, never a preemption of dy's single-cell abort.

Either way the run is aborted around T+1060s instead of hanging to the
T+1400s SIGKILL — and, more importantly, the same signals would catch a wedge
on an *expensive* table where the intervening minutes actually bill.

F1 and F2 are complementary: F1 catches stalls via CloudWatch (external,
works even if heartbeat upload fails, WCU-agnostic after this fix), F2
catches them via the progress counter (works even if CloudWatch has extended
lag, and pinpoints the wedged instance). The external cell deadline stays as
the last-resort backstop.

The lesson is that three independent mechanisms failed simultaneously
because they shared a common assumption: "cheap tables don't need deep
monitoring." This violates the experiment-protocol principle that defense
layers should have **diverse** failure modes.

## Fixes to implement

| # | Fix | Status | Scope |
|---|---|---|---|
| F1 | Zero-consumption stall check for all tables | **Implemented** | watchdog.sh |
| F2 | Progress counter in heartbeat (stats-based, phase-aware) | **Implemented** | run_cells.sh + watchdog.sh |
| F3 | Watchdog test scenarios for cheap-table stall + frozen progress | **Implemented** | tests/watchdog_test.sh |

Implemented in this session (TDD: scenarios 7–10 in `tests/watchdog_test.sh`
first, then the watchdog/runner changes until green — full suite passes).
F1 strikes on `consumed ≤ --zero-wcu-rate` (default 0.5 WCU/s) for any table,
keeping the `--stall-threshold`/`--expensive-wcu` fractional check for the
high-WCU tables. F2 adds `--progress-frozen-strikes` (default 6). A
testability seam, `WATCHDOG_SALVAGE_WAIT` (default 30s, 0 in the self-test),
keeps the mocked suite from wall-clocking on the SSM salvage sleep.

**On-instance changes**: F2 modifies run_cells.sh (heartbeat loop reads the
per-rep `stats.jsonl` and attaches `resolved_items` as `progress`) → **requires
a fresh G2 canary before Tier-1 Stage 2**. F1 and the F2 watchdog-side logic
are off-instance (watchdog.sh), no canary needed on their own.

## Pool1 deadlock root cause (cross-reference)

The underlying bug is a shared-queue deadlock: rate-update control signals
travelled the same per-worker queue as work, and a buffered control signal
(which completes no work and so fires no `notify_one()`) could occupy the sole
slot at `queue_depth=1` while the dispatcher was parked waiting for a
completion. Fixed by separating the control plane onto its own queue. See
[pool1-deadlock-analysis.md] for the full write-up (including two earlier
mis-diagnoses now retracted: a "lost-wakeup on `Notify`" theory and a
"duration, not workload" theory). Key facts:
- Only affects candidate A' (pool1), not pool16/mpmc/task
- Mixed workload triggers it; uniform-small/large completed 3/3 on EC2 —
  variable item sizes, not run length, align the timing window
- EC2 evidence: thread stacks, stats preserved in S3 `runs/20260707-152019/`
  (the repo-root `perf.data`, dated 2024-08, is unrelated)
