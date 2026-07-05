# Design Document: Throttled Import/Export Foundation for dynein

- Status: Draft (design record for the wip branch `improve-export-import`)
- Last updated: 2026-07-05
- Target branch: `improve-export-import` (source of truth: remote `my/improve-export-import`)
- Related experimental branch: `improve-export-import-async-channel-queue` (8 parallel chunkers; benchmark not settled yet)

This document records the design decisions already implemented with their rationale, as well as **surrounding decisions that are not implemented yet but constrain future design** (congestion control, multi-table support, admission control, etc.). The goal is that the next person or agent touching this code can pick up the design philosophy without rediscovering it.

## 1. Goals and Non-Goals

**Goals**

- Turn `dy import` into a streaming, parallel writer that saturates a specified WCU target while being safely throttled
- Essentially, build a **reusable foundation for "processing something while keeping WCU/RCU consumption constant"** (the `src/algo` module). Import is merely its first consumer
- Future applications: parallel scan for export (the RCU variant), other batch workloads

**Non-Goals (for now)**

- Writing to multiple tables at once (but keep the design extensible; see §5.1)
- Managing GSI capacity individually (same as above)

## 2. Architecture Overview

```
producer ──▶ [main ch: bounded 500] ──▶ chunker ──▶ [process ch: bounded 16] ──▶ ThrottledExecutor
(file iter)                               ▲                                          │ round-robin
                                          │                                          ▼
                                          └──── [retry ch: unbounded] ◀──── workers (per-worker Bucket)
                                                                                     │
                                                                                     ▼
                                                                                BatchWriteItem
```

- **`src/algo/bucket.rs`** — Token bucket. Consumes the estimated amount up front and corrects the difference against the actually consumed amount via `feedback(estimate - actual)`
- **`src/algo/monitor.rs`** — `Probe` (sends observations) / `Monitor` (statistical throughput judgement using mean + standard deviation)
- **`src/algo/worker.rs`** — `ThrottledWorker` (waits on the bucket, then processes) and `ThrottledExecutor` (round-robin distribution, automatic scale-out when the target is missed). The workload is abstracted behind the `ResourceConstraintProcess` trait (`estimate_resource` / `process_and_consume_resource`) and is **DynamoDB-agnostic**
- **`src/ddb/item.rs`** — Item size → WCU estimation based on the heuristics in the official documentation
- **`src/transfer.rs`** — Pipeline assembly (`stream_writes_with_chucked`). It takes an iterator of `WriteRequest`s, so json/jsonl/csv all go through this path

## 3. Invariants (breaking these is a bug)

1. **Item accounting**: every item submitted to BatchWriteItem is classified into **exactly one** of "successful / queued for retry / permanently failed" per request result (`summarize_batch_write_result`, pinned by unit tests).
   - If this breaks, you get either "silent item loss" (a bug that actually existed; §6) or "a hang waiting forever for items nobody retries"
2. **Termination condition**: the monitoring task sends the termination signal only when `successful + permanently failed == total submitted`. If permanently failed > 0, the whole import ends with `DyneinBatchError::PermanentWriteFailure` (no hangs, no silent swallowing)
3. **Deadlock freedom**: enqueueing retries from a worker must **never block** (unbounded channel). See the cycle analysis in §4.2

## 4. Implemented Decisions (Decision Record)

### 4.1 Response handling is centralized in the pure function `summarize_batch_write_result`

- **Decision**: extract the classification of BatchWriteItem results into a side-effect-free function (input: result + submitted items + has-prior-success flag; output: accounting summary) and pin the specification with unit tests
- **Rationale**: the previous inline-match implementation had a bug where a retryable error logged "queued to retry" without actually re-queueing anything, and it was only discovered under real production-like conditions. Exhaustiveness of the accounting (§3.1) is a property that should be guarded by types and tests, not mixed into pipeline wiring
- **Side effect**: this extraction also removed an overcounting bug where `successful_writes` subtracted the number of unprocessed *tables* instead of *items*

### 4.2 Retry is a hybrid of "unbounded channel + drain-retries-first"

- **Decision**: the dedicated retry channel is unbounded. On every loop iteration the chunker fills the batch with retries via `try_recv` **first**, and only then tops it up with new items
- **Deadlock analysis** (the cycle that occurs with a bounded channel):
  a worker blocks on sending a retry → the worker never completes, so the executor's `Notify` never fires → the executor stops draining the process channel → the chunker blocks on sending to the process channel → the chunker stops draining retries → the cycle closes. The more throttling, the higher the retry volume and the more likely this is to occur
- **Memory argument for unboundedness**: the number of items in the retry queue ≤ the number of items admitted into the pipeline and not yet completed. Currently the whole input file is loaded into memory anyway, so this adds no new upper bound. **When file reading is made streaming, this argument weakens — do that change together with the semaphore admission control of §5.2**
- **Intent of drain-retries-first**: backpressure pointed in the direction of "finish the work you have taken in before accepting new work". It keeps the retry queue practically empty and demotes unboundedness to an insurance policy
- **Rejected alternatives**: bounded + retry-priority (the cycle remains); synchronous in-worker retries (breaks bucket fairness and round-robin)

### 4.3 Transport errors: "retry after the first success, fail on the first attempt"

- **Decision**: for `TimeoutError` / `DispatchFailure` / `ResponseError`, if at least one request has succeeded before, the network configuration is assumed correct and all items are retried. Failures starting from the very first request most likely indicate a configuration problem, so the items are marked permanently failed and the import terminates
- **Known hole, now closed**: if the network dies permanently after the first success, the transport-error retries would loop forever (zero progress, no termination). The progress deadline (§4.8) aborts the import in this situation

### 4.4 Classification of service errors

- Retryable: `InternalServerError` / `ProvisionedThroughputExceededException` / `RequestLimitExceeded` → re-queue all items
- Any other service error (e.g. `ResourceNotFoundException`, validation-type errors) → counted as permanent failures and reported at the end via `PermanentWriteFailure`

### 4.5 Bucket feedback and worker partitioning

- Estimates are consumed up front; the difference against the measured consumption (sum of `consumed_capacity`) is refunded or charged. Introduced as the countermeasure to "wobbly WCU consumption"
- The executor splits the target rate evenly as `target_limit / num_workers`, and each worker looks only at its own bucket (lock-free). This rests on **the assumption that traffic is uniform across workers**. Multi-table support may make this assumption too strong (§5.3)
- Scale-out doubles the worker count when measured throughput is statistically (3σ) below the target. `1f2d0a2` added over-scale-out suppression. In addition, scale-out decisions now use the AIMD *effective* target and are frozen entirely while the congestion controller is backing off, which fixes "scaling out in the wrong direction when throttling is the reason the target is missed" (§4.6)

### 4.6 AIMD congestion control, fast loop (`src/algo/congestion.rs`)

- **Requirement**: co-located production workloads take priority. When throttling occurs, the batch-side target temporarily drops to roughly half
- **Controller**: `AimdController` is pure logic with injected time (fully unit-tested). The user target is a ceiling; on a congestion signal the effective target is multiplicatively halved (with a cooldown so one congestion event causes one decrease); after a calm period it recovers gradually
- **Timing design (decrease fast, recover slow)**: the calm period between recovery steps is **60 s, deliberately aligned with the CloudWatch metric granularity** so the future slow loop can consult metrics between steps. The recovery step is **10% of the *current* effective target** (gentle x1.1 compounding; a halved target returns in ~7 minutes) — deliberately *not* a ratio of the user ceiling, because the ceiling can be far above realistic capacity and a ceiling-relative step would make the first recovery jump enormous when running low
- **Initial effective target from known information**: instead of starting blind at the ceiling, `initial_target_wcu()` (transfer.rs) derives a realistic starting point: provisioned tables start at **80% of the provisioned WCU** (leaving headroom for production from the beginning), on-demand tables at the **platform-default warm throughput (4,000 WCU)**. The aws-sdk-dynamodb version in use (1.28) does not expose the `warm_throughput` field of DescribeTable; after an SDK upgrade, read the actual value. The same gradual recovery then probes upward from the initial value toward the ceiling
- **`is_congested` semantics**: "a throttle event was observed within the last calm period" — used to freeze scale-out. Starting below the ceiling due to an initial target is *not* congestion (otherwise scale-out would be frozen from the start)
- **Congestion signal definition** (decided in `summarize_batch_write_result`): a request is "throttled" when the whole request was rejected with `ProvisionedThroughputExceededException` / `RequestLimitExceeded`, or when **at least one** of its items came back unprocessed. Every unprocessed item is a server-side rejection due to a capacity shortage, so for production-workload protection even a single one counts (a majority-based threshold was considered and rejected as too lenient). The decrease cooldown keeps this strictness from over-reacting, at the cost of the steady state possibly sitting somewhat below the entitlement — accepted as the intended production-first policy; revisit with benchmark data if it proves too conservative. `InternalServerError` is retryable but is *not* a congestion signal
- **Wiring**: `ResourceConstraintProcess::process_and_consume_resource` returns `ProcessResult { consumed, throttled }` (the minimal observation-type extension; the keyed generalization of §5.1 remains). Workers record outcomes into `CongestionStats` (two shared atomics — no extra channels); the executor reads deltas on each run-loop iteration, feeds the controller, and broadcasts new per-worker rates via the pre-existing `Signal::ChangeRefill` / `ChangeMaxCap`
- **Interaction with scale-out**: decisions compare against the effective target, and scale-out is frozen while congested
- **Caveat**: the controller only ticks when messages flow through the executor. Under total silence it does not tick, but in that situation there is nothing to pace either; the progress deadline (§4.8) covers pathological cases

### 4.7 CloudWatch slow control loop (informed recovery)

- **Intent**: the throttle response stays very conservative (halve immediately, recover at x1.1/min), and the slow loop is the *accelerator that only fires when metrics say it is safe*. Design: estimate the production workload as `table-level ConsumedWriteCapacityUnits rate − our own consumed rate`; the remaining capacity (`provisioned − production`) is assumed fully available to the batch, but only **half of it** (`BOOST_USABLE_RATIO = 0.5`) may be claimed in one jump, as the safety margin against estimation error and production traffic changes
- **Boost semantics** (`AimdController::boost_to`): upward only, refused while congestion is ongoing (recent throttle), capped at the user ceiling. Suggestions travel through a single-value `BoostSlot` (atomic; latest wins) so the slow loop never blocks the executor
- **Cadence**: 60-second interval matching the metric granularity, and the most recent minute's datapoint is skipped as potentially incomplete
- **Degradation**: any CloudWatch fetch error (typically missing permissions) logs once and permanently falls back to the fast loop only
- **v1 limitations** (recorded deliberately):
  - Provisioned tables only — on-demand tables lack a clear capacity reference to compute headroom against (revisit once warm throughput is readable after the SDK upgrade)
  - Window misalignment: our own rate is measured over the last interval while the CloudWatch datapoint is 1–3 minutes old, so the production estimate can skew when our own rate changes quickly. The 0.5 margin absorbs this; a per-minute history of our own consumption would align the windows properly if it proves insufficient
  - `WriteThrottleEvents` is not consulted yet (could distinguish "production is being throttled" from "we are"); per-GSI metrics wait for the keyed resource model (§5.1)

### 4.8 Progress deadline (livelock safety valve)

- The monitoring task aborts the pipeline when `successful + failed` has not advanced for `STALL_DEADLINE` (300 s), returning `DyneinBatchError::ProgressStalled`. This closes the infinite-retry livelock of §4.3 and any unforeseen stall
- On abort, workers may still hold in-flight retries whose receiver is gone; those sends now log-and-drop instead of panicking (correct because the import is reporting an error anyway)
- `StallDetector` is pure logic with injected time (unit-tested)

### 4.9 Chunker waits for full batches (`fill_to_capacity`)

- **Decision**: the chunker keeps reading the producer channel until the
  batch holds 25 items or the producer closes, instead of sending whatever
  a single `recv_many` returned. Retries are still drained first at every
  iteration, and a partial final batch is sent when the producer is done
- **Rationale**: an executor that consumes faster than the producer feeds
  turns every `recv_many` remainder into a partial BatchWriteItem request
  (the 2026-07-04 finding; measured at 21.4 items/request with the
  task-per-request candidate on DynamoDB Local). Waiting costs nothing
  downstream because the token bucket paces requests anyway, and fuller
  batches mean strictly fewer requests for the same items
- **Interaction with the invariants**: termination and accounting are
  untouched — the fill only blocks while the producer channel is open, and
  the producer always closes it after queueing all input (§3 still holds)

### 4.10 Shared concurrency governor (`ScaleOutGovernor`) and estimator fixes

- **Decision**: the essential control problem — the number of parallel
  in-flight requests against the required throughput — is the same in every
  executor architecture, so the pool's scale-out decision was extracted
  into `src/algo/governor.rs` and is now used by all candidates: the pools
  grow their worker count and the task-per-request candidate grows its
  in-flight cap (start 1, doubling, ceiling `task<N>`, default 256) under
  identical logic (grow only when measured throughput is statistically
  below the effective target; stop when the previous growth did not
  demonstrably help; freeze while congested; ramp wait between steps)
- **Bug fixes surfaced by the extraction** (both unit-tested):
  - `Monitor::average_per_second` had a fencepost bias: the sum of all n
    window values was divided by the (n−1) intervals spanning them, up to
    2× overestimation on small windows. This silently degraded the
    scale-out effectiveness comparison all along
  - the effectiveness veto used a strict `>`; with the unbiased estimator
    a saturated server reproduces the previous throughput exactly, and
    exact equality must count as "growth did not help"
- **Verification**: on DynamoDB Local at a saturating target, candidate C
  self-tuned its cap to 4 and matched the pools' throughput without any
  manual cap (benchmark-plan.md §2.7.2)
- **Design intent, clarified in review**: the two-layer structure of the
  Monitor is deliberate and unchanged — each stat point is one windowed
  rate estimate, and the mean/sample-σ over the stat-point series measure
  the estimate and its dispersion. The fencepost fix only removes the
  systematic error in each point's *value*; small-sample conservatism
  still exists (few points → larger genuine σ). The old inflation was not
  acceptable as "conservatism" because the effectiveness veto compares the
  *recorded* previous-phase average against the current one and never
  resets: a phase recorded with small-window inflation (e.g. a true 10/s
  recorded as ~17.5/s) makes a genuine doubling look like no improvement,
  permanently freezing scale-out. If explicit small-sample conservatism is
  wanted later, add it transparently (e.g. a minimum stat-point count in
  `should_grow`), not by biasing the estimator

## 5. Groundwork for Future Design (not implemented, but direction-setting)

### 5.1 Multi-table / GSI support: vectorizing the resource

- **Current constraint**: resource = scalar f64 is baked into every layer (return values of `ResourceConstraintProcess`, `Bucket`, `Signal`, `Monitor`, the executor target). BatchWriteItem can write to multiple tables in one request, but consumption and throttling are independent per table (+ per GSI)
- **Direction**: generalize f64 into a lightweight vector type keyed by resource (`Table(name)` / `Gsi(table, index)` → amount). **GSI support just adds more keys**, so the same abstraction covers it automatically (one abstraction removes both TODOs in `bucket.rs` and `worker.rs`)
- **Timing**: do it in Phase 4 (foundation generalization). Now — while `BatchWriteProcess` is the only trait implementor — is the cheapest moment for the breaking change. However, Phase 3 (benchmark) may still reshape executor internals, so do it after that settles
- **Groundwork in the transfer layer**: `WriteRequest` flowing through the pipeline carries no table name (the chunker injects the single table name). For multi-table, make the channel element `(table name, WriteRequest)` and let the chunker group by table. This can be changed independently of algo
- **Consumption semantics**: a request spanning multiple keys consumes atomically only when capacity is sufficient for all keys (all-or-nothing). `estimate_available_at` becomes the max across keys

### 5.2 Semaphore-based admission control (a prerequisite for the streaming-read era)

- Once file reading becomes streaming, the argument "unbounded retry is safe because everything is in memory anyway" (§4.2) collapses
- **Direction**: cap the total number of items existing inside the pipeline with a semaphore. Acquire a permit on admission; release it when the item is finally resolved as successful or permanently failed. This keeps the resident population bounded **even with unbounded channels**, reconciling deadlock freedom (unbounded) with a memory cap (semaphore)
- The retry path merely circulates while holding its permit, so it does not interfere with admission control

### 5.3 Open performance questions: executor topology, partitioned vs shared buckets

These are **unverified performance hypotheses** that must be settled by
measurement, not intuition. The detailed experiment design lives in
`benchmark-plan.md`; this section records what is in question and why.

- **Task-per-request vs fixed worker pool**: the task-per-request model
  (spawn one tokio task per BatchWriteItem request, bounded by a semaphore,
  paced by a single shared bucket) was originally avoided on the assumption
  of spawn overhead. That assumption is suspect: spawn costs ~µs against
  5–50 ms network calls, and the model would delete the Signal channels,
  round-robin distribution, and the entire scale-out machinery (concurrency
  emerges from rate × latency; AIMD would just update the shared refill)
- **Token waste of partitioned buckets** (hypothesis): with split per-worker
  buckets, an idle worker's bucket saturates at max_cap and discards refill
  while a busy worker starves — so under heterogeneous item sizes the
  aggregate consumption falls below the target even though the average
  "looks" capped by WCU. `feedback` corrects intra-worker estimation error
  but cannot fix inter-worker imbalance. A shared bucket cannot lose tokens
  this way. This is also why "the WCU cap makes topologies equivalent" is
  only true for homogeneous workloads — benchmarks must include mixed item
  sizes to have discriminating power
- **Queue-depth skew**: per-worker queues are 16 batches deep; the
  round-robin distributor skips full workers (so it is not blind), but it
  cannot rebalance work already queued — expensive items hold up to 16×25
  items hostage on one worker, visible as a completion tail. Depth 1 may
  recover most of this within the pool model
- **CPU affinity**: the pool model's presumed cache-affinity advantage is
  likely illusory — workers are ordinary tokio tasks and migrate across
  runtime threads (work stealing) unless pinned. Decide with measured CPU
  time on the target instance families
- **Multi-table extension** (kept from before): evenly-split buckets
  additionally require the table mix per worker to be uniform. Options: (a)
  even split + feedback, (b) shared bucket per table (contended), (c)
  per-table pools (gives up multi-table batching). Note that if the
  task-per-request model wins, (b) becomes the natural fit. Until settled,
  have workers query a "capacity provider" abstraction so the design can
  fall either way

### 5.4 Phase 3 benchmark plan

**See `benchmark-plan.md` for the full experiment design** (candidates,
workload matrix, metrics, EC2/S3 disposable-fleet infrastructure, decision
rules). Summary:

- Candidates: A = current pool (queue 16), A′ = pool with queue depth 1,
  C = task-per-request + shared bucket; B (shared MPMC queue + pool) held in
  reserve. The chunker axis (single vs 8 parallel,
  `improve-export-import-async-channel-queue`) is evaluated only if a pool
  topology survives — the task model spawns from the chunker directly and
  removes that axis
- Metrics: throughput, WCU adherence (mean ± σ), token waste, completion
  tail, wasted requests, CPU time / max RSS
- Environment: disposable EC2 (m9g.xlarge / m8a.xlarge) bootstrapped via
  user data, results persisted to S3, instances self-terminate — designed
  for running many cells cheaply
- **Run after AIMD** (done): retry storms and wrong-direction scale-out
  would have distorted results as noise
- Close the losing branch when Q5 is settled

## 6. Empirical Findings

### 2026-07-05 (benchmark preparation)

- **Bucket floating-point livelock (real bug, found by simulation)**:
  `Bucket::estimate_available_at` returned the exact-remainder wait; near
  the token boundary the matching f64 refill increment rounds to zero, so
  the wait loop (sleep remainder → refill → retry) stops progressing — a
  busy CPU spin in real time (each iteration adds µs of wall clock, so it
  eventually escapes, wasting CPU), a hard livelock under virtual time.
  Fixed with a 1ms wait floor. `Bucket::feedback` also clamps positive
  refunds at `max_cap` now (required for candidate C's shared bucket)
- **`ThrottledExecutor::run` now waits for worker termination** (it used to
  await only the Close sends), so run() returning means all accepted work
  was processed — the executor is usable standalone, without the item
  accounting of transfer.rs, e.g. in the simulations
- Executor candidates (`DYNEIN_BENCH_EXECUTOR=pool16|pool1|mpmc|task`) and
  their small-WCU live validation, plus the phase-1 simulation predictions:
  see `benchmark-plan.md` §0 and §2.6. Highlights: C's AIMD (shared-bucket
  refill update) halved and recovered correctly under a live throttle;
  production-first held on the new executors; the simulation predicts C
  removes the scale-out ramp-up penalty entirely in the high-rate regime
  (CPU cost remains unmeasured until EC2)
- **DynamoDB Local high-rate check** (`benchmark-plan.md` §2.7): at a
  reachable 4k WCU/s target all four candidates are indistinguishable and
  hit the target exactly; at an unreachable 32k target (server saturated)
  C loses 42% throughput with 2.4× CPU — no queue backpressure means
  partial batches (21.4 items/request) and up to 1024 requests in flight
  against a saturated server — while B matches the pools with half the
  workers and ~30% less CPU. Recorded as sub-hypotheses for the EC2 runs

- **Non-streaming file reads are a hard scale blocker (EC2 run 090552,
  aborted)**: `dy import` reads the whole input into memory
  (`fs::read_to_string` + full deserialization). An 8.5GB mixed-size JSONL
  input drove dy to 15.6GB RSS on a 16GB m9g.xlarge and the kernel
  OOM-killed it 22 seconds after "Loaded a file"; systemd took the whole
  runner unit down with it (SIGKILL → no cleanup trap), leaving the
  39k-WCU table idling. Verified from the kernel log via SSM. Consequence:
  **roadmap item 6 (streaming reads + semaphore admission control, §5.2)
  is a prerequisite for the quota-scale mixed/large benchmark cells**, not
  a nice-to-have that can wait until after the benchmark. The quota-scale
  uniform-small cells (1.3GB) and everything at 10 WCU ran fine

### 2026-07-04

- The old implementation (missing retry re-queueing) reproducibly **hung forever at 561/2000** on a 2-WCU table with 2000 items. About 57 fully-throttled requests × 25 items ≈ 1439 items were silently lost
- After the fix: five 2000-item runs at 25 WCU and one 300-item run under sustained throttling at 2 WCU — all exit 0 with exact table counts
- **Beware of burst capacity**: a table accumulates roughly 300 seconds of unused capacity (about 7500 for 25 WCU). A freshly created or idle table will not throttle, so a throttling experiment on one is meaningless. Deplete it first or recreate the table at low WCU
- Observed the "wrong-direction" scale-out 1→2→4→8 in the middle of a throttling storm (the evidence behind §4.6)
- The current chunker sends whatever `recv_many` returns, producing many partial chunks of fewer than 25 items (a throughput inefficiency; to be quantified in the benchmark)
- AIMD verification (10-WCU table, 2000 items, sustained throttling): the effective target halved stepwise from the hardcoded 100,000 down to ~98 while throttling persisted, and the import completed with exit 0. This run motivated the known-information initial target: starting from the absurd 100,000 ceiling took ~13 halvings (~30 s) to reach realistic levels. The `--max-wcu` CLI option (roadmap 6) still matters for setting a sane ceiling
- Two-loop end-to-end verification (10-WCU table, 2000-item import + a pseudo production writer at 7 WCU/s via `scripts/pseudo_prod_writer.py`): while production ran, the import backed off 8→4→2→1 and slow-loop suggestions stayed below the effective target, so no boost fired — production stayed protected. After the production writer stopped, CloudWatch reflected it within ~3 minutes and the slow loop boosted 1.66→2.42→5.00 (exactly half of the freed 10 WCU), after which the fast loop kept probing upward (5.50, 6.05, ...). The `max(0, ..)` guard absorbed window-misalignment moments where our own measured rate exceeded the table-level rate. DynamoDB Local cannot be used for these experiments: it neither throttles nor emits CloudWatch metrics

## 7. Roadmap

1. ~~Build the experiment environment, reproduce the stall, fix item accounting~~ (done)
2. ~~Minimal AIMD (fast loop only) + progress deadline + effective-target-based scale-out decisions~~ (done; §4.6, §4.8)
3. ~~CloudWatch slow control loop (informed recovery)~~ (done; §4.7)
4. Settle the executor/chunker architecture via benchmark (§5.4, `benchmark-plan.md`) ← next
5. Foundation generalization: resource vectorization (§5.1), move generic parts of `BatchWriteProcess` into algo, reduce `expect`s, scale-in
6. Finish import: a `--max-wcu`-style CLI option (currently hardcoded to `100_000.0`), streaming file reads + semaphore admission control (§5.2)
7. Parallel scan for export (RCU variant, separate branch)
8. Squash, sign, and tidy up the wip commits

## 8. Notable Commits

- `e5dc939` retry channel separation + drain-retries-first (bounded at the time)
- `036b98c` re-queueing on retryable errors + transport error policy
- `1f2d0a2` over-scale-out suppression
- `cfacfee` item accounting model (summarize) + unbounded retry + permanent-failure termination (implements §3 and §4 of this document)
- `44d64c8` AIMD fast loop + progress deadline (§4.6, §4.8)
- `83e73fc` AIMD timing tuning + known-information initial target (§4.6)
- `f11ac21` CloudWatch slow control loop (§4.7)
- `8c3584d` algo layer on `tokio::time::Instant` (virtual-time simulation prep; see `benchmark-plan.md` §2.5)
