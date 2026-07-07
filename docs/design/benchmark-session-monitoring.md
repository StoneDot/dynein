# Session-Side Monitoring for Paid Benchmark Runs

- Status: Active (first used for Tier-1 Stage 1, run 20260707-152019)
- Companions: `benchmark-run-postmortem.md` §4 (monitor design principles),
  `scripts/bench/watchdog.sh` (the automated guardian this document wraps)

## 1. The three monitoring layers and what each cannot see

| Layer | Watches | Cannot see |
|---|---|---|
| On-instance (systemd scope, MemoryMax, OnFailure guardian, per-minute heartbeat) | one rep / one runner | anything after the instance dies; its own cgroup being killed |
| Off-instance watchdog (`watchdog.sh`, 60s ticks, abort authority) | run-scoped tables, spend integration+projection, stall on expensive tables, heartbeat age, per-cell external deadline | **(a) tables outside the run's name prefix** (a bug creating differently-named tables = invisible unbounded spend), **(b) its own death** (guardian has no guardian), **(c) fast-fail churn** — reps failing quickly *advance* through cells, so deadlines never fire and the run "completes" worthless |
| Session-side agent (periodic wake-up, ~4.5 min) | the two gaps above + judgment calls | nothing structural — but it is the slowest layer; it complements, never replaces, the watchdog's automatic abort |

The session agent's unique duties are exactly the watchdog's blind spots:
**guard the guardian, check account-wide invariants, and judge progress
quality** (not just progress existence).

## 2. Wake-up checklist (each item exists for a specific failure mode)

Run `stage_watch.sh` (or equivalent) and evaluate:

1. **Guardian liveness** — `pgrep` the watchdog process (bracket-trick
   pattern, see §4). If DEAD while billed resources exist: restart it
   immediately (it is stateless except integrated spend — note the reset),
   then investigate why it died. *Failure mode: watchdog crash leaves the
   fleet unguarded with abort authority gone.*
2. **Account-wide inventory, not run-scoped** — `list-tables` in the run
   region (ALL names) + WCU of each + running instances tagged dynein-bench.
   Compare against the written expectation for the current stage (e.g.
   Stage 1: exactly one `…-w10-…` table, one instance). Anything
   unexpected — foreign prefix, wrong WCU, second instance — is an
   immediate investigate-then-abort candidate. *Failure mode: naming bug
   or leftover from a parallel run bills outside the watchdog's view.*
3. **Progress monotonicity and rate** — `result.json` count must increase
   between checks and roughly track budget arithmetic (Stage 1: ~8 min/rep,
   36 reps). Count increasing *too fast* is as suspicious as stalled:
   fast-fail churn looks like progress to every automated layer.
   Spot-check a random fresh result.json for `exit_code: 0` and item-count
   match while the run is still young. *Failure mode: (c) above.*
4. **Heartbeat freshness AND phase plausibility** — age < 2 min, and the
   phase should move through create-table → import → upload across checks.
   A fresh heartbeat pinned to the same phase across several checks is a
   wedge the deadline hasn't caught yet. *Failure mode: in-worker timeout
   dead, external deadline still hours away.*
5. **Spend curve** — watchdog's integrated `spent=~$…` should match the
   stage's expectation (Stage 1: cents; a 39k quota rep: ~$0.42/min while
   the table exists). A kink upward = something is provisioned that the
   plan didn't include. *Failure mode: silent capacity change.*

## 3. Escalation matrix

| Observation | Action |
|---|---|
| Watchdog dead, resources exist | Restart watchdog → investigate cause → report |
| Unexpected table/instance (any prefix) | Investigate via CloudTrail; if not explainable in ~5 min, delete it (money first), report |
| Progress flat across 2 checks but heartbeat fresh | SSM into the instance, inspect runner journal before the deadline fires |
| Progress advancing with failures (spot-check finds exit≠0) | Abort early — a full run of failed reps costs the same as a good one |
| Heartbeat missing/stale | Let the watchdog act (it will); verify it did; if it didn't, manual abort |
| Watchdog logs `AUTH FAILURE` | he51's AWS credentials died. Nobody off-instance can see OR abort until re-login (`setsid nohup aws sso login --no-browser` + hand the URL to the user; if unreachable, the on-instance layer — instance-profile creds never expire — plus the 9h boot-level `shutdown +540` remain the backstop). Watchdog freezes completion/abort decisions and retries; never treat its silence as success |
| Manual abort needed | `sweep.sh` order: tables → salvage → stop instances (postmortem §8: money, evidence, compute, human) |

## 4. Cadence and mechanics

- **~270 s interval while billed resources exist** — under the 5-minute
  prompt-cache TTL, so periodic checks stay cheap; 5-minute-even intervals
  are the worst case (cache miss every time). Relax to 20–30 min only for
  phases with no expensive table and a healthy trend.
- On run completion (watchdog logs `run finished`): stop periodic checks,
  collect + review results, only then start the next stage.
- **Pitfalls already paid for** (do not rediscover):
  - `pgrep -f "watchdog.sh --run-id X"` matches the *waiting shell itself*
    →永久待機. Use `pgrep -f "[w]atchdog.sh --run-id X"`.
  - Completion-line greps must include the actual wording (`run finished`),
    not assumed synonyms (`complete`).
  - `launch.sh --commit` needs the **full SHA** — the canary-marker path is
    keyed by the literal string, and a short SHA fails the gate lookup.
  - Long-lived waiters must be `setsid nohup … &` detached; plain
    background jobs die with the session turn.

## 5. Fourth layer: cloud-side orphan reaper

`scripts/bench/setup_reaper.sh` deploys an EventBridge rule (rate 15 min) →
Lambda that deletes any us-west-2 table which is `dynein-bench-` prefixed,
carries the `dynein-bench` tag, is ACTIVE, and is older than TTL (90 min;
legit fresh-per-rep tables live ≤ ~25 min). This is the only layer that
survives the dev machine, the session agent, the watchdog and the fleet all
dying together — it bounds the orphan-table cost to ~TTL/60 × $25 at 39k WCU.

- **Raise the TTL or disable the rule before using the reuse-fallback
  strategy** (commit 8f4cdc5's ~4h shared table would be reaped mid-run).
- Deletion-path testing (dummy table + TTL=0) only in a no-live-tables
  window — with the schedule enabled, a lowered TTL races against live runs.
- Teardown after Tier-1: `setup_reaper.sh --teardown`.
