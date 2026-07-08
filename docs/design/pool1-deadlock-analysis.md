# Pool1 (Candidate A') Deadlock Analysis

**Date**: 2026-07-07 (discovered), 2026-07-08 (root-caused and fixed)
**Discovered**: Tier-1 Stage 1 run 20260707-152019 (pool1-w10-mixed cells)
**Affected**: candidate A' (`DYNEIN_BENCH_EXECUTOR=pool1`) — but the underlying
defect exists at every queue depth; depth 1 merely makes it reachable.

> **History note.** The first version of this document (2026-07-07) blamed a
> lost-wakeup race on `tokio::sync::Notify` and recommended pre-creating the
> `Notified` future. That diagnosis was **wrong**: `Notify::notify_one()`
> stores a permit when no task is waiting, so the next `notified().await`
> completes immediately — the "notify fired into the void" sequence it
> described cannot occur (tokio 1.47 `notify.rs`). A second hypothesis
> (2026-07-08) — "any pool1 run whose dispatch phase outlives the 60 s
> recovery period wedges, item size irrelevant" — was **also wrong**, falsified
> by the Stage 1 uniform cells (see Evidence). Both are recorded here so the
> same dead ends are not re-explored. The account below is the one supported by
> the fix and the EC2 data.

## Summary

Rate-update control signals (`ChangeMaxCap` / `ChangeRefill`) travelled the
**same per-worker queue as work**, delivered with blocking `send()`. A control
signal occupies a queue slot but completes no work, so a worker that dequeues
one fires **no** `process_notifier.notify_one()` (only `Signal::Process`
notifies). At `queue_depth == 1` a single buffered control signal can therefore
sit in the sole slot while the dispatcher is parked on `notifier.notified()`
waiting for a completion that will never come. Neither side can wake the other.

The fix moves the control plane to a dedicated per-worker queue, so every
message on a work queue is work and runs to completion. See
[import-throttling.md](import-throttling.md) §4.13.

## The mechanism (src/algo/worker.rs, pre-fix)

The dispatcher distributes work round-robin: it `try_send`s to each worker in
turn, and when **every** worker queue is full it parks on
`self.notifier.notified().await`. A worker wakes it by calling
`process_notifier.notify_one()` — but **only after completing a
`Signal::Process`**. Control signals shared this same queue and this same
`notify_one()` obligation, which they did not honour.

After the AIMD controller changes the effective target,
`adjust_effective_target` broadcasts the pair to every worker with blocking
sends onto the **work** queue:

```rust
for tx in &self.workers_tx {
    futures.push(tx.send(Signal::ChangeMaxCap(target_each_worker)));
    futures.push(tx.send(Signal::ChangeRefill(target_each_worker)));
}
```

A blocking `send()` resolves once the message is **buffered**, not once it is
handled. At `queue_depth == 1`:

1. `send(ChangeMaxCap)` succeeds — the worker had already taken the previous
   `Process`, so the slot was free.
2. `send(ChangeRefill)` blocks until the worker dequeues `ChangeMaxCap`.
3. The worker dequeues `ChangeMaxCap`, applies it — **no notify** — and loops.
4. `ChangeRefill` is now buffered in the slot; the broadcast returns.
5. The dispatcher loops, pulls the next `Process` from upstream, and
   `try_send`s it — the slot is **Full** (`ChangeRefill`). All workers full →
   it parks on `notifier.notified()`.
6. The worker dequeues `ChangeRefill`, applies it — **no notify** — and returns
   to `recv()`, which now blocks (queue empty).
7. **Deadlock.** The slot is free, but the dispatcher already finished its scan
   and is parked; nothing will notify it. Upstream still holds items.

Every target change is a chance to hit this window. The escape is for the
worker to drain `ChangeRefill` *before* the dispatcher's `try_send` observes
Full — a race the dispatcher loses whenever the buffered control signal is
still present at the moment it scans.

### Why deeper queues hide it

With `queue_depth == 16` the buffer almost always has room for the next
`Process` behind the two control signals, so `try_send` succeeds and the
dispatcher never reaches the park path. The defect is latent at every depth;
only depth 1 removes the slack that masks it.

## Evidence

### EC2 (Stage 1, run 20260707-152019, m9g.xlarge, commit 8f4cdc5)

`result.json` exit codes, 3 reps each:

| pool1 cell | mix | items | wall | exit |
|---|---|---|---|---|
| pool1-w10-mixed | mixed | 1380 | timeout | **124 (wedged) ×3** |
| pool1-w10-small | uniform-small | 4000 | ~388 s | 0 ×3 |
| pool1-w10-large | uniform-large | 115 | 476–589 s | 0 ×3 |

The mixed cell wedged in all three reps; both uniform cells completed cleanly
in all three. This is the decisive result:

- **It is workload-specific, not duration-driven.** uniform-small ran ~388 s —
  far past the 60 s recovery period, with many target changes — and never
  wedged. So "a long enough dispatch phase wedges" is false; **variable item
  sizes** are what make the timing window align in practice. (This is what the
  original document got right, for the wrong reason.)
- Only candidate A' is affected. pool16 (A), mpmc (B), and task (C) completed
  every cell; B and C never share a work queue with control signals (B uses a
  separate `ctrl` channel; C is semaphore-based).

Stall signature in the wedged reps: `stats.jsonl` `total_requests` stops
advancing (~200/1380 in the local mirror), the progress line's items/sec decays
monotonically, and the in-process `timeout` (budget × 2) kills the process at
exit 124. Thread stacks and stats are preserved in S3
`runs/20260707-152019/salvage/` and `.../results/m9g.xlarge/pool1-w10-mixed/`.
(The repo-root `perf.data`, dated 2024-08, is unrelated to this run — an earlier
version of this doc cited it in error.)

### Local reproduction (DynamoDB Local) and its subtlety

DynamoDB Local never throttles, so the AIMD *decrease* path cannot fire there.
The reproduction instead relies on **recovery creep**: with `ceiling: inf` the
effective target starts below the (infinite) user target, so after 60 s of calm
the controller creeps it upward — a target change, hence a control broadcast —
*while the depth-1 backpressure keeps the dispatcher still dispatching*. A
mixed 400-item import on a 10-WCU local table (healthy ≈ 145 s) reproduces it:

| binary | exit | wall | target changes observed | items |
|---|---|---|---|---|
| pre-fix | 124 | 280 s (timeout) | 1 (`→8.80` at ~60 s) | froze at 200/400 |
| post-fix | 0 | 144 s | 2 (`→8.80`, `→9.68`) | 400/400 |

The pre-fix binary wedged immediately after its **first** target change; the
post-fix binary absorbed two and finished. This is a *powered* comparison: the
trigger (`Congestion control changed the effective target`) is observed in the
log, not assumed.

Two rig pitfalls cost hours and are recorded so they are not repeated:
- A run must outlive 60 s **and** keep the dispatcher busy past that point, or
  no target change fires and the wedge is unreachable. Short runs prove nothing.
- The table must be paced so the run is neither instant (dispatcher drains
  before 60 s) nor timeout-dominated for unrelated reasons. Discarding dy's
  output hid the fact that early "wedge" runs were merely slower than the
  timeout — always keep a positive control (a known-good executor must finish
  fast on the same rig) before trusting a negative result.

## The fix

**Separate the control plane from the work plane.** Each worker gets a second
queue carrying `CtrlSignal { ChangeRefill | ChangeMaxCap }`; the worker
`select!`s over both with `biased` (control first). `Signal` now carries only
`Close | Process(T)`. `Close` deliberately stays on the **work** queue so it
remains ordered behind items already handed to the worker — routing it through
the priority control queue would let it overtake queued work and strand those
items, breaking item accounting and the termination condition
(import-throttling.md invariants 1–2).

With this, every message on a work queue is work: each dequeue runs to
completion and fires `notify_one()`, restoring the premise the dispatcher's
park/wake protocol depends on — at *every* queue depth, not just depth 1.
`notify_one()` stays on `Process` completion, so §4.2's unbounded-retry
deadlock-freedom argument (which relies on exactly that) is unchanged.

Rejected alternatives:
- **Move the wakeup to dequeue instead of completion.** Works, but changes the
  notifier's meaning and forces §4.2's argument to be rewritten to follow it.
  Separating the queues keeps both the wakeup semantics and §4.2 intact.
- **Remove pool1.** The defect is latent at all depths; fixing the shared-queue
  race is correct regardless of whether A' stays a benchmark candidate.

## Verification

- Unit: `test_queue_depth_one_survives_effective_target_change` drives an
  executor at `queue_depth == 1` with a throttling workload (forces an AIMD
  decrease → broadcast) and asserts all messages complete within 5 s.
  Deterministically red before the fix (`2/8 processed`), green after.
- Guard tests for the new `select!` loop:
  `test_close_does_not_strand_queued_work` (Close stays behind work) and
  `test_closed_control_queue_does_not_starve_work` (a dropped control sender
  must not spin the `biased` branch). The latter is mutation-checked: reverting
  the closed-channel guard makes only it fail.
- Simulation: `sim_` scenarios pass with all four candidates completing,
  including A'.
- Local E2E: the powered mixed reproduction above (pre-fix 124 → post-fix 0).
- `cargo test --bin dy`: full suite green; clippy/fmt clean.

## Impact on Tier-1

- The fix is an on-instance code change → **requires a fresh G2 canary** before
  Stage 2.
- Stage 1 pool1-mixed results (3/3 timed out) are invalid and must be re-run.
  pool1-small and pool1-large completed and are usable, but re-running them on
  the fixed binary is cheap insurance.
- Stage 2 (quota) exercises pool1, so the fix is a prerequisite.
