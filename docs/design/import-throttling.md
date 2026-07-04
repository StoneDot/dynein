# Design Document: Throttled Import/Export Foundation for dynein

- Status: Draft (design record for the wip branch `improve-export-import`)
- Last updated: 2026-07-04
- Target branch: `improve-export-import` (source of truth: remote `my/improve-export-import`)
- Related experimental branch: `improve-export-import-async-channel-queue` (8 parallel chunkers; benchmark not settled yet)

This document records the design decisions already implemented with their rationale, as well as **surrounding decisions that are not implemented yet but constrain future design** (congestion control, multi-table support, admission control, etc.). The goal is that the next person or agent touching this code can pick up the design philosophy without rediscovering it.

## 1. Goals and Non-Goals

**Goals**

- Turn `dy import` into a streaming, parallel writer that saturates a specified WCU target while being safely throttled
- Essentially, build a **reusable foundation for "processing something while keeping WCU/RCU consumption constant"** (the `src/algo` module). Import is merely its first consumer
- Future applications: parallel scan for export (the RCU variant), other batch workloads

**Non-Goals (for now)**

- Writing to multiple tables at once (but keep the design extensible; see §5.2)
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
- **Memory argument for unboundedness**: the number of items in the retry queue ≤ the number of items admitted into the pipeline and not yet completed. Currently the whole input file is loaded into memory anyway, so this adds no new upper bound. **When file reading is made streaming, this argument weakens — do that change together with the semaphore admission control of §5.3**
- **Intent of drain-retries-first**: backpressure pointed in the direction of "finish the work you have taken in before accepting new work". It keeps the retry queue practically empty and demotes unboundedness to an insurance policy
- **Rejected alternatives**: bounded + retry-priority (the cycle remains); synchronous in-worker retries (breaks bucket fairness and round-robin)

### 4.3 Transport errors: "retry after the first success, fail on the first attempt"

- **Decision**: for `TimeoutError` / `DispatchFailure` / `ResponseError`, if at least one request has succeeded before, the network configuration is assumed correct and all items are retried. Failures starting from the very first request most likely indicate a configuration problem, so the items are marked permanently failed and the import terminates
- **Known hole (not addressed yet)**: if the network dies permanently after the first success, we get an infinite-retry livelock (zero progress, no termination). The countermeasure is a deadline safety valve — "abort when there is zero progress for a certain period" (§5.4) — planned to land together with AIMD

### 4.4 Classification of service errors

- Retryable: `InternalServerError` / `ProvisionedThroughputExceededException` / `RequestLimitExceeded` → re-queue all items
- Any other service error (e.g. `ResourceNotFoundException`, validation-type errors) → counted as permanent failures and reported at the end via `PermanentWriteFailure`

### 4.5 Bucket feedback and worker partitioning

- Estimates are consumed up front; the difference against the measured consumption (sum of `consumed_capacity`) is refunded or charged. Introduced as the countermeasure to "wobbly WCU consumption"
- The executor splits the target rate evenly as `target_limit / num_workers`, and each worker looks only at its own bucket (lock-free). This rests on **the assumption that traffic is uniform across workers**. Multi-table support may make this assumption too strong (§5.5)
- Scale-out doubles the worker count when measured throughput is statistically (3σ) below the target. `1f2d0a2` added over-scale-out suppression, but **the fundamental fix for "scaling out in the wrong direction when throttling is the reason the target is missed" belongs to the AIMD side** (§5.1)

## 5. Groundwork for Future Design (not implemented, but direction-setting)

### 5.1 AIMD congestion control (next implementation target)

- **Requirement**: co-located production workloads take priority. When throttling occurs, it is acceptable for the batch-side target to temporarily drop to roughly half
- **Design**: two control loops
  - **Fast loop (primary control)**: locally observed throttling rate. When it crosses a threshold, multiplicatively halve `effective_target` (clipped at a floor); recover additively while calm. The existing `Signal::ChangeRefill` / `ChangeMaxCap` can be used as-is to distribute rate changes
  - **Slow loop (optional, permission-gated)**: estimate the production traffic share from CloudWatch `ConsumedWriteCapacityUnits` / `WriteThrottleEvents` and adjust the target ceiling. With 1-minute granularity and 1–3 minutes delivery delay it cannot serve as primary control. Per-GSI hotspots are only visible in CloudWatch (local throttling errors do not tell you which GSI caused them). Without permissions, silently degrade to the fast loop only
- **Wiring needed**: a backchannel for throttling events from workers to the executor. `Probe` currently only carries the consumed amount as f64, so widen the observation type to something like `{consumed, throttled}` (this touches the same spot as the type generalization of §5.2, so do only the observation-type extension first)
- **Scale-out decisions** must switch from the raw target to `effective_target`, and worker additions must be frozen while the congestion window is lowered
- Once this lands, the "retry storm caused by the bucket refunding the full estimate on errors and retrying immediately" problem is effectively resolved as well (the lowered target throttles the flow itself)

### 5.2 Multi-table / GSI support: vectorizing the resource

- **Current constraint**: resource = scalar f64 is baked into every layer (return values of `ResourceConstraintProcess`, `Bucket`, `Signal`, `Monitor`, the executor target). BatchWriteItem can write to multiple tables in one request, but consumption and throttling are independent per table (+ per GSI)
- **Direction**: generalize f64 into a lightweight vector type keyed by resource (`Table(name)` / `Gsi(table, index)` → amount). **GSI support just adds more keys**, so the same abstraction covers it automatically (one abstraction removes both TODOs in `bucket.rs` and `worker.rs`)
- **Timing**: do it in Phase 4 (foundation generalization). Now — while `BatchWriteProcess` is the only trait implementor — is the cheapest moment for the breaking change. However, Phase 3 (benchmark) may still reshape executor internals, so do it after that settles
- **Groundwork in the transfer layer**: `WriteRequest` flowing through the pipeline carries no table name (the chunker injects the single table name). For multi-table, make the channel element `(table name, WriteRequest)` and let the chunker group by table. This can be changed independently of algo
- **Consumption semantics**: a request spanning multiple keys consumes atomically only when capacity is sufficient for all keys (all-or-nothing). `estimate_available_at` becomes the max across keys

### 5.3 Semaphore-based admission control (a prerequisite for the streaming-read era)

- Once file reading becomes streaming, the argument "unbounded retry is safe because everything is in memory anyway" (§4.2) collapses
- **Direction**: cap the total number of items existing inside the pipeline with a semaphore. Acquire a permit on admission; release it when the item is finally resolved as successful or permanently failed. This keeps the resident population bounded **even with unbounded channels**, reconciling deadlock freedom (unbounded) with a memory cap (semaphore)
- The retry path merely circulates while holding its permit, so it does not interfere with admission control

### 5.4 Progress deadline (livelock safety valve)

- As the last line of defense against the infinite-retry livelock of §4.3 and any unforeseen stalls, add a watchdog that aborts with an error when `successful + failed` has not advanced for a certain period. Planned to be implemented together with AIMD

### 5.5 Open design question: partitioned buckets vs a shared bucket

- With multi-table support, evenly-split per-worker buckets (§4.5) additionally require that the table mix flowing to each worker is uniform — a stronger assumption
- Options: (a) keep the even split and absorb skew via feedback, (b) a shared bucket per table (accurate but contended), (c) per-table worker pools (gives up multi-table batching)
- **Settle this with experiments.** Until then, have workers query a "capacity provider" abstraction so the design can fall either way

### 5.6 Phase 3 benchmark plan (not executed yet)

- Subject: single mpsc chunker (current) vs 8 parallel async-channel chunkers (`improve-export-import-async-channel-queue`)
- Metrics: (1) effective throughput, (2) adherence of consumed WCU to the target (moving-window mean ± stddev = quantifying the "wobble"), (3) wasted requests (requests discarded due to throttling), (4) behavior at low WCU / high WCU / varied item sizes
- **Run it after AIMD lands** (retry storms and wrong-direction scale-out would distort the results as noise)
- Close the losing branch

## 6. Empirical Findings (2026-07-04)

- The old implementation (missing retry re-queueing) reproducibly **hung forever at 561/2000** on a 2-WCU table with 2000 items. About 57 fully-throttled requests × 25 items ≈ 1439 items were silently lost
- After the fix: five 2000-item runs at 25 WCU and one 300-item run under sustained throttling at 2 WCU — all exit 0 with exact table counts
- **Beware of burst capacity**: a table accumulates roughly 300 seconds of unused capacity (about 7500 for 25 WCU). A freshly created or idle table will not throttle, so a throttling experiment on one is meaningless. Deplete it first or recreate the table at low WCU
- Observed the "wrong-direction" scale-out 1→2→4→8 in the middle of a throttling storm (the evidence behind §5.1)
- The current chunker sends whatever `recv_many` returns, producing many partial chunks of fewer than 25 items (a throughput inefficiency; to be quantified in the benchmark)

## 7. Roadmap

1. ~~Build the experiment environment, reproduce the stall, fix item accounting~~ (done)
2. **Minimal AIMD** (fast loop only) + progress deadline + effective-target-based scale-out decisions ← next
3. Settle the chunker architecture via benchmark (§5.6)
4. Foundation generalization: resource vectorization (§5.2), move generic parts of `BatchWriteProcess` into algo, reduce `expect`s, scale-in
5. Finish import: a `--max-wcu`-style CLI option (currently hardcoded to `100_000.0`), streaming file reads + semaphore admission control (§5.3)
6. CloudWatch slow loop (§5.1, optional)
7. Parallel scan for export (RCU variant, separate branch)
8. Squash, sign, and tidy up the wip commits

## 8. Notable Commits

- `e5dc939` retry channel separation + drain-retries-first (bounded at the time)
- `036b98c` re-queueing on retryable errors + transport error policy
- `1f2d0a2` over-scale-out suppression
- `cfacfee` item accounting model (summarize) + unbounded retry + permanent-failure termination (implements §3 and §4 of this document)
