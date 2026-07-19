# Design Document: A Rate-Controlled Transfer Foundation for dynein

- Last updated: 2026-07-17
- **What this document is**: the design record for the rate-controlled
  transfer foundation (`src/algo`) and its first consumer, the `dy import`
  write path — why the initial implementation had to be replaced (§1),
  the user-visible contract (§2), the architecture and its invariants
  (§3–§4), each non-obvious decision with its rationale and the
  alternatives rejected (§5), and the measurements that settled the
  executor architecture (§6). It is addressed to reviewers of this
  redesign and to future contributors: the code plus this document should suffice
  to work on these modules without rediscovering the design
- **What it does not cover**: `bwrite` and `bootstrap` still use the
  pre-existing simple write path (§1.3 explains why; §7 has the plan),
  and export is future work (§7)
- **Experiment archive**: the benchmark plan, the disposable-fleet
  harness, the postmortems, and the raw run data behind §6 are
  deliberately kept out of the repository; they are preserved at tag
  `pre-task-unification-20260711` on the author's fork. The archive is
  needed only to reproduce or audit the raw runs — not to follow this
  document

## 1. Motivation

### 1.1 Anatomy of the initial implementation

The initial implementation, which this redesign replaces
(src/transfer.rs and src/batch.rs before the redesign), is a simple
sequential loop with four structural properties:

1. **The whole input is materialized before the first write**:
   `fs::read_to_string` reads the entire file, and the parser builds a
   `Vec<JsonValue>` of all items. Memory is proportional to input size.
2. **Writes are strictly sequential**: the vector is split into
   `chunks(25)` and written one `BatchWriteItem` request at a time, each
   awaited to completion before the next begins. Throughput is capped at
   25 items per round-trip — a few hundred items/s at typical
   latencies — regardless of table capacity.
3. **There is no pacing**: unprocessed items are resent immediately with
   no backoff, no sleep and no rate limit (`batch_write_until_processed`),
   so against a throttled table the loop sends requests as fast as it
   can. The tool's only concession to capacity is a confirmation prompt
   recommending the user switch the table to on-demand mode — rate
   control is delegated to the user.
4. **There is no error model**: item outcomes are not accounted anywhere,
   so the code can only either keep looping (unprocessed items) or abort
   the whole import (`?` on any SDK error). What a failure means —
   transient or permanent, and which items it affected — is not
   representable in this structure.

### 1.2 Why this cannot be fixed incrementally

Each structural property of §1.1 blocks one of the four things users
need from a bulk writer:

- **Scale** (blocked by item 1): whole-file materialization makes memory
  proportional to input size — an 8.5 GB input was measured driving RSS
  to 15.6 GB and an OOM kill on a 16 GB host. Fixing it requires
  streaming, which in turn requires bounding the in-flight population
  some other way.
- **Throughput** (blocked by item 2): sequential awaiting caps throughput
  at latency, not capacity. Fixing it requires concurrent in-flight
  requests.
- **Coexistence** (blocked by item 3): an import with no pacing consumes
  whatever capacity the table will give, competing with co-located
  production traffic — and adding concurrency (the throughput fix) makes
  this worse, not better. Fixing it requires a feedback controller, not a
  sleep constant.
- **Reliability** (blocked by item 4): with no representation of per-item
  outcomes, a failure can only stop the import — it cannot say what was
  written and what was not. Worse, accounting is a *prerequisite* for
  fixing the other three: once concurrency and retry queues exist, an
  accounting mistake no longer shows up as a visible error — it shows up
  as items that silently vanish or as a completion that never comes
  (§5.1.1).

These four requirements depend on each other — concurrency needs pacing, streaming
needs admission control, pacing needs accounting to know what was actually
consumed — which is why the redesign replaces the loop with a pipeline
rather than patching it.

### 1.3 The goal: dynein as a rate-controlled DynamoDB pipe

The replacement is deliberately built as a **reusable foundation for
"process a stream of work while holding RCU/WCU consumption to a
controlled target"** (`src/algo`; the operation to run is behind one
trait, §3.1). `dy import` is its first consumer, not its purpose. The
foundation is meant to be adopted step by step:

- **import** (implemented by this redesign): file → DynamoDB,
  WCU-controlled
- **export**: parallel `Scan` → file, the RCU variant of the same
  foundation
- **bootstrap**: sample-data loading uses the naive
  `batch_write_until_processed` path (unchanged by this redesign) and will
  move onto the foundation
- **pipe modes**: item insertion from an unbounded **stdin stream**, and
  `Scan` output streamed to **stdout** — making dynein composable as *a
  DynamoDB pipe for arbitrary programs*. Streaming sources with bounded
  resident memory (§5.3.2) are a hard prerequisite for this: a pipe has
  no length to preallocate.

`bwrite` stays on the simple path on purpose: it writes small, mixed
put/delete request sets assembled from CLI arguments, where rate control
has nothing to regulate.

## 2. User-visible behavior

`dy import -t <table> -f <format> -i <file> [--max-wcu <N>]`

- The import runs **many BatchWriteItem requests concurrently**, paced by
  a token bucket toward a target rate; concurrency is grown automatically
  only while it measurably helps (§3.2). Progress (items/s) is displayed
  continuously.
- **`--max-wcu <N>` (new)** sets a hard ceiling on the WCU/s the import
  may consume, for users who want to reserve explicit headroom for
  production traffic. **When absent there is no ceiling**: pacing is
  delegated entirely to congestion control, which starts from a realistic
  target derived from the table settings (§5.2.2) and probes upward.
  Positive finite values only; zero, negatives, NaN and infinities are
  rejected at parse time.
- **Throttling response**: on throttle signals the import halves its
  target immediately and recovers gradually — co-located production
  workloads take priority over import speed by design (§5.2.2).
- **Termination is exact**: every admitted item ends as written or
  permanently failed. Permanent failures abort with
  `PermanentWriteFailure(count)`; a stall with zero progress for 300 s
  aborts with `ProgressStalled(resolved, total)` (§5.4). Both exit
  nonzero with clear error messages.
- Memory is **flat regardless of input size** (~20 MB RSS measured on
  30 MB and 300 MB inputs alike; §5.3.2).
- The confirmation prompt that recommended switching provisioned tables
  to on-demand mode is **removed** (from both import and export):
  congestion control makes provisioned tables a first-class target, so
  the recommendation no longer reflects how the tool behaves.

### 2.1 Behavior changes relative to the initial implementation

Reviewers should weigh these deliberate changes in observable behavior:

| Area | initial implementation | this redesign |
|---|---|---|
| Invalid `jsonl` document | Everything after the first invalid line silently dropped (`filter_map(Result::ok)` over a stream deserializer that stops at the first error) | Import aborts with a positioned parse error; the already-admitted prefix is written and reported |
| Malformed `csv` row (cell-count mismatch) | `process::exit(1)` mid-import | Normal error with row context |
| Sustained throttling | Unprocessed items are resent immediately with no backoff; a fully-throttled request aborts the import once the SDK's internal retries are exhausted | AIMD backoff, production-first |
| First-request transport failure | SDK error after internal retries aborts import | Items marked permanently failed with count; distinguishes configuration errors from temporary network failures (§5.1.3) |
| Memory | ∝ input size (2–3× file size) | Flat ~20 MB |
| Failure reporting | Aborts on the first SDK error; the in-flight batch's outcome is unreported | Exact written / permanently-failed counts |
| `--max-wcu` | (absent) | New option |
| Provisioned-table prompt (import & export) | Warns and recommends switching to on-demand mode, asks for confirmation | Removed — no prompt, no recommendation |

## 3. Architecture

### 3.1 How items flow

```
producer ──▶ [main ch: bounded 500] ──▶ chunker ──▶ [process ch: bounded 16] ──▶ TaskExecutor
(streaming file reader,                   ▲                                        │ shared Bucket pacing;
 admitted one item at a time;             │                                        │ 1 tokio task per request,
 each item acquires a permit)             │                                        │ in-flight cap grown by
                                          │                                        │ the governor (§3.2)
                                          │                                        ▼
                                          └──── [retry ch: unbounded] ◀── request tasks
                                                     (items keep their permit)     │ (resolve ⇒ return permits)
                                                                                   ▼
                                                                              BatchWriteItem
```

`src/transfer.rs` assembles the stages. Its entry point
`stream_writes_with_chunked` does not take a list of items — it takes a
reader function. The pipeline hands the reader a callback (the sink),
and the reader pushes one `WriteRequest` at a time into it while reading
the file. json, jsonl and csv each supply only their own reader;
everything past the sink is shared. Following one item through the
pipeline:

1. **Admission and production**: a format-specific streaming source runs
   on a `spawn_blocking` thread and pushes items into the bounded main
   channel. Before entering, every item takes one admission permit, and
   the permit stays with the item until it is finally written or
   permanently failed (§5.3.2).
2. **Batching**: the chunker collects items into full 25-item batches
   (the BatchWriteItem maximum), always taking retried items first
   (§5.1.2, §5.3.1).
3. **Execution**: `TaskExecutor` (`src/algo/task_executor.rs`) takes each
   batch, waits until the shared token bucket has enough tokens for the
   batch's estimated WCU cost, then spawns one tokio task for the
   request. The estimate for a put is the item's byte size (computed per
   the official documentation's rules, `src/ddb/item.rs`) rounded up to
   1 KiB write units. A delete is counted as a flat 1 WCU: its real cost
   depends on the *stored* item's size, which the client cannot know —
   the feedback correction (§3.2) absorbs the difference. How fast tokens
   arrive, and how many requests may be in flight at once, are decided by
   the feedback loops of §3.2.
4. **Resolution**: each request task classifies every item of its
   response as written, retryable, or permanently failed (invariant 1,
   §4). Retryable items go back to the chunker through the unbounded
   retry channel, keeping their permits; resolved items return theirs.

The executor itself does not know what work it is running: the workload
behind it is abstracted as `ResourceConstraintProcess`
(`estimate_resource` / `process_and_consume_resource`). BatchWriteItem
is the first implementation; the export scan is planned as the second,
with RCU as the constrained resource (§7). The task-per-request executor
shape was selected over fixed worker pools by measurement (§6).

### 3.2 How the pace is set

Three cooperating feedback loops decide how fast §3.1 is allowed to run.
The AIMD controller and the governor run inside the `TaskExecutor`'s own
loop. The CloudWatch slow loop is a separate task, but it only posts
suggestions into an atomic slot; the executor applies them, so a slow
metrics call can never block request processing. The shared bucket (`src/algo/bucket.rs`) is the
junction between the two views: items consume its tokens (§3.1), and the
controllers set its refill rate. Each request takes its estimated cost
from the bucket up front; the estimate is corrected by
`feedback(estimate − actual)` once the real consumption is known
(§5.2.1). Throughput measurements come from `src/algo/monitor.rs`
(`Probe`/`Monitor`: windowed rate estimation, mean + sample σ over stat
points). This is the subtle part of the design; the rules below are the
contract.

```
                     per-request outcomes (consumed, throttled?)
   request tasks ─────────────────────────▶ CongestionStats (3 shared atomics)
                                                    │ deltas read each executor iteration
                                                    ▼
 CloudWatch slow loop ──60s──▶ BoostSlot ──▶ AimdController ◀── user ceiling (--max-wcu, ∞ if absent)
 (production ≈ table rate      (atomic,        │ effective target
  − own rate; may claim        upward only,    ├──────────────▶ shared Bucket refill
  ≤ half the free headroom)    refused while   │                 (+ feedback(estimate−actual))
                               congested)      ▼
                                        ScaleOutGovernor
                                        (grows in-flight cap 1→2→…→256 while measured
                                         throughput lags the effective target AND the
                                         previous growth helped; frozen while congested)
```

Interaction rules (each prevents a specific failure mode):

- **Decrease fast, recover slow**: a throttle signal halves the effective
  target (with a cooldown so one congestion event causes one decrease);
  recovery adds 10% of the *current* target per 60 s calm period —
  deliberately not ceiling-relative, so a very high ceiling cannot
  cause a very large first recovery step
- **The governor looks for the minimum concurrency that sustains the
  target, not the maximum the system tolerates** — excess in-flight
  requests add no throughput and cost memory and CPU (§5.2.4). It
  compares against the *effective* target and is frozen while congested —
  otherwise throttling (target missed for external reasons) would trigger
  scale-out in exactly the wrong direction: the target is being missed
  *because* the table is refusing work, and every doubling adds more
  pressure to it
- **Boosts are upward-only, refused while congested, capped at the
  ceiling** — the slow loop can only raise the target, and only when
  metrics show spare capacity; lowering the target is the fast loop's job
  alone
- **The initial effective target comes from known information** instead of
  probing down from the ceiling: provisioned tables start at 80% of
  provisioned WCU (headroom for production from the first request);
  on-demand tables at the table's actual
  `warm_throughput.write_units_per_second`, falling back to the platform
  default 4,000. *Empirical note*: despite documentation stating every
  table has warm-throughput values, `DescribeTable` returned null for
  both a fresh on-demand table and a long-lived provisioned one — the
  field can simply be absent in practice, which is why the fallback
  matters
- **The effective target must start finite** (invariant 5) — halving is
  the backoff mechanism and ∞ × 0.5 = ∞ cannot back off

## 4. Invariants (breaking these is a bug)

1. **Item accounting is exactly-once**: every item submitted to
   BatchWriteItem is classified into exactly one of successful / queued
   for retry / permanently failed per request result
   (`summarize_batch_write_result`, side-effect-free, pinned by unit
   tests). Breaking this yields either silent item loss or a hang waiting
   for items nobody will retry (§5.1.1).
2. **Termination**: the monitor signals completion only when the producer
   has finished admitting **and** successful + permanently failed ==
   total admitted. Permanent failures > 0 fail the import with
   `PermanentWriteFailure` — no hangs, and no silently ignored failures.
   The completion comparison is `>=`, and going above raises an explicit
   error, so a hypothetical accounting bug is reported immediately instead
   of turning into a silent stall (§5.4.2).
3. **Deadlock freedom**: enqueueing retries must never block — the retry
   channel is unbounded by design (cycle analysis in §5.1.2).
4. **Admission permit conservation**: every admitted item holds exactly
   one semaphore permit from admission until resolution; retrying items
   keep theirs. A leaked permit blocks admission until the stall deadline
   aborts the import; a double-returned permit unbounds resident memory.
   The exactness of (1) is what makes the permit return exact (§5.3.2).
5. **The effective target is always finite**, even though the user ceiling
   may be ∞ (`AimdController::with_initial_target` asserts this;
   `resolve_target_ceiling` backfills a missing capacity hint with the
   4,000 default whenever the ceiling is unbounded) (§5.5).

## 5. Design decisions

Grouped by subsystem; each records the decision, the reasoning, and the
alternatives rejected.

### 5.1 Accounting and error handling

#### 5.1.1 Response classification is a pure function

`summarize_batch_write_result` (input: result + submitted items +
has-prior-success flag; output: accounting summary) centralizes all
BatchWriteItem response handling, pinned by unit tests.

The scenario this guards against: a BatchWriteItem response has many
shapes — all items written, some unprocessed, the whole request
throttled, a fatal service error, a transport error — and several of
them occur only under sustained throttling or network trouble,
conditions a development environment rarely produces. If classification
is written inline at the call site, a path that miscounts or forgets to
re-queue looks correct in every normal run and fails only in
production — and it fails *silently*, as lost items or as a termination
condition that never becomes true (invariants 1–2).

Options considered:
- **Inline handling at the call site** — Pros: no indirection. Cons: the
  rare paths can only be exercised by producing real throttling and
  network failures, so they effectively stay untested.
- **A side-effect-free classification function (chosen)** — Pros: every
  response shape is enumerable in fast unit tests; the accounting
  invariant has a single owner. Cons: one more layer between the
  response and the pipeline wiring.

#### 5.1.2 Retry: unbounded channel + drain-retries-first

The dedicated retry channel is unbounded; on every iteration the chunker
fills the batch from retries via `try_recv` first, then fills the rest
with new items.

- **Why unbounded — the deadlock cycle with any bounded channel**: a
  request task blocks sending a retry → the task never completes → the
  executor's completion signal never fires → the executor stops draining
  the process channel → the chunker blocks sending to it → the chunker
  stops draining retries → cycle closed. Throttling raises retry volume,
  so the failure mode concentrates exactly where the tool must be robust.
- **Memory bound**: retry-queue population ≤ admitted-and-unresolved
  items ≤ `ADMISSION_ITEM_CAP` (§5.3.2) — the semaphore, not the channel,
  is the bound.
- **Why drain-first**: backpressure points toward "finish what you took
  in before accepting new work"; the retry queue stays near-empty and
  the unbounded capacity remains only as a safety margin.
- **Options considered**:
  - *Bounded retry channel + priority drain* — Pros: a hard memory bound
    on the queue itself. Cons: the deadlock cycle above remains; a bound
    only changes how much throttling it takes to close it.
  - *Synchronous in-request retries* (the request task retries its own
    items before completing) — Pros: no retry queue at all. Cons: the
    retrying task bypasses the shared pacing order, and its concurrency
    slot is held hostage for as long as its items keep failing.
  - *Unbounded channel + drain-retries-first (chosen)* — Pros:
    deadlock-free by construction; the retry population is still bounded,
    just by the admission semaphore instead of the channel. Cons: the
    bound lives in a different mechanism than the queue, which this
    section has to explain.

#### 5.1.3 Transport errors: retry after first valid response, fail on first attempt

For `TimeoutError` / `DispatchFailure` / `ResponseError`: if at least one
prior request returned a **valid response** — including one where every
item came back unprocessed (a fully throttled but healthy start) —
connectivity is proven and the items are retried. Failures from the very
first request onward most likely indicate a configuration problem, so the
items are marked permanently failed and the import terminates. "Success"
is deliberately *a response*, not *a written item*: counting written items
would misclassify a temporary network failure after a fully-throttled
start as a first-attempt failure. The remaining risk — the network dying permanently
*after* a first success would retry forever — is closed by the progress
deadline (§5.4.1).

#### 5.1.4 Service-error classification

Retryable and re-queued: `InternalServerError`,
`ProvisionedThroughputExceededException`, `RequestLimitExceeded`. Every
other service error (`ResourceNotFoundException`, validation errors, …)
counts as a permanent failure, reported at termination via
`PermanentWriteFailure`. **Known limitation (accepted)**: the triggering
error is logged but the failed items themselves are not identified
anywhere — recovering them means re-running the input (puts are
idempotent by key). Identifying failed items is deferred to the
dead-letter design that pipe modes need anyway (§7).

### 5.2 Flow control

#### 5.2.1 Token bucket with consumption feedback

Estimates are consumed from the shared bucket up front; the delta against
measured consumption (`consumed_capacity` sums) is refunded or charged via
`feedback`. Positive refunds clamp at `max_cap`. The wait computation has
a 1 ms floor: near the token boundary the exact-remainder wait is so
short that the matching refill rounds to zero tokens, and without the
floor the wait loop makes no progress (pinned by a unit test).

#### 5.2.2 AIMD congestion control, fast loop (`src/algo/congestion.rs`)

- **Requirement**: co-located production workloads take priority; on
  throttle signals the import backs off to roughly half.
- `AimdController` is pure logic with injected time, fully unit-tested.
  The user ceiling bounds the effective target; a congestion signal halves
  it (cooldown: one decrease per event); calm periods recover it at 10% of
  the current value per 60 s step — the 60 s cadence deliberately matches
  CloudWatch metric granularity so the slow loop can consult metrics
  between steps, and ~7 minutes returns a halved target.
- **Congestion signal definition** (decided in
  `summarize_batch_write_result`): a request is throttled when it was
  rejected whole with a throughput exception, or when **at least one**
  item came back unprocessed. Per the API contract an unprocessed item
  means capacity shortage or internal failure; both warrant backing off.
  Two thresholds were weighed: *a majority of items unprocessed* (Pros:
  fewer false alarms from a single straggler item; Cons: sustained
  partial rejection — exactly what per-partition throttling looks like —
  would never trigger backoff, leaving production unprotected) versus
  *at least one item unprocessed* (chosen — Pros: production is protected
  from the first sign of contention; Cons: occasional over-reaction,
  bounded by the decrease cooldown). The steady state may sit somewhat
  below the provisioned capacity — accepted as part of the
  production-first policy. `InternalServerError` is retryable but is
  *not* a congestion signal.
- **`is_congested`** means "a throttle was observed within the last calm
  period" and gates both scale-out and boosts. Starting below the ceiling
  because of the initial target is *not* congestion.
- **Wiring**: `process_and_consume_resource` returns
  `ProcessResult { consumed, throttled }`; request tasks record outcomes
  into three shared atomics; the executor reads deltas each iteration,
  feeds the controller, and applies new targets directly to the shared
  bucket. No extra channels.
- **Caveat**: the controller only ticks while messages flow. Under total
  silence there is nothing to pace either; pathological stalls belong to
  the progress deadline (§5.4.1).

#### 5.2.3 CloudWatch slow loop (informed recovery)

The fast loop's response is deliberately conservative (halve immediately,
recover at ×1.1/min). The slow loop only raises the target, and only when
metrics show spare capacity: estimate production as table-level
`ConsumedWriteCapacityUnits` rate minus our own measured rate; of the free
headroom (provisioned − production), claim at most **half** in a single step
(`BOOST_USABLE_RATIO = 0.5`) as the margin against estimation error.

- `boost_to` is upward-only, refused during congestion, capped at the
  ceiling. Suggestions travel a single-value atomic slot (latest wins), so
  the slow loop can never block the executor.
- 60 s cadence; the most recent minute's datapoint is skipped as
  potentially incomplete.
- Any CloudWatch error (typically missing permissions) logs once and
  degrades permanently to the fast loop alone.
- **v1 limitations (recorded deliberately)**: provisioned tables only
  (using warm throughput as an on-demand headroom reference needs its own
  design pass — warm throughput grows with usage, unlike a fixed
  provisioned amount); the production estimate can skew when our own rate
  changes quickly (window misalignment, absorbed by the 0.5 margin);
  `WriteThrottleEvents` is not yet consulted; per-GSI metrics wait for
  resource vectorization (§7).

#### 5.2.4 Concurrency governor (`src/algo/governor.rs`)

In-flight concurrency is not a free resource. The concurrency a workload
*needs* is roughly target rate × request latency; anything beyond that
level adds no throughput while costing memory and CPU. The achievable
throughput is a property of the whole system — client CPU, runtime,
network, and server — and once any of them is saturated, additional
in-flight requests actively *reduce* throughput instead of raising it.
This was measured, not assumed: with no effective in-flight limit (1,024)
and partially filled batches, the task-per-request executor lost 42% of
its throughput at 2.4× the CPU of the fixed pools when a DynamoDB Local
setup was driven past its capacity; with batching fixed, an oversized
limit of 256 still cost ~10%
while a hand-tuned 32 restored parity; and the governor then found a cap
of 4 by itself on the same workload, matching the best candidate with no
manual tuning.

`ScaleOutGovernor` therefore searches for the **minimum concurrency that
sustains the effective target**, approaching it from below: start at 1,
double only while measured throughput is statistically (3σ) below the
effective target *and* the previous growth measurably helped, freeze
while the controller reports congestion, and wait out a ramp period
between steps. The default ceiling (256) is a safety bound, not a goal.
The control problem — in-flight parallelism versus required throughput —
is executor-independent, which is why the logic lives in its own module.

Two estimator details are load-bearing, each pinned by a unit test:

- `Monitor::average_per_second` divides the sum of the n window values by
  n, not by the n−1 intervals between them. The fencepost version
  overestimates small windows by up to 2×, and the governor compares the
  *recorded* previous-phase average against the current one without ever
  resetting it — so a single inflated phase makes a genuine doubling look
  like no improvement and freezes growth permanently.
- The growth-effectiveness veto counts *equal* throughput as "growth did
  not help". The scenario: once the system is saturated, one more
  doubling reproduces the previous throughput exactly; treating equality
  as improvement would keep doubling into the saturation.

### 5.3 Pipeline shape and memory

#### 5.3.1 The chunker waits for full batches

The chunker keeps reading the producer channel until the batch holds 25
items or the producer closes (retries still drain first; a partial final
batch flushes at close). An executor that consumes faster than the
producer feeds otherwise turns every `recv_many` remainder into a partial
request (measured: 21.4 items/request under saturation). Waiting costs
nothing — the token bucket paces requests anyway — and fuller batches are
strictly fewer requests for the same items. Termination is unaffected: the
fill only blocks while the producer channel is open, and the producer
always closes it.

#### 5.3.2 Streaming sources + semaphore admission control

The scale half of the foundation (and the precondition for pipe modes,
§1.3). No format materializes the input:

- **json / json-compact**: a `DeserializeSeed` visitor walks the top-level
  array element by element (serde has no pull-based array iteration, hence
  the push-based pipeline entry: `stream_writes_with_chunked` takes a
  source closure, not an iterator)
- **jsonl**: `Deserializer::from_reader(...).into_iter()` — the collect()
  removed; an invalid document aborts with a positioned error (§2.1)
- **csv**: line-by-line via `BufRead::lines`

The source runs on a `spawn_blocking` thread (file I/O must not occupy a
runtime worker). Its sink acquires **one admission permit per item**
(`Semaphore::acquire` + `forget`) and `blocking_send`s into the bounded
main channel. Request tasks return permits for exactly the items each
request resolves; retried items keep theirs while circulating.

`ADMISSION_ITEM_CAP = 10_000` bounds the resident population:
steady-state saturation needs ≈7,300 items (main channel 500 + process
channel 16×25 + in-flight ceiling 256×25), and the cap must stay well
above one batch (25) or admission would deadlock against the full-batch
wait (§5.3.1). For the admission unit, *byte-weighted permits* (Pros:
bounds actual memory, not item count; Cons: more bookkeeping, and the
weight would need the same size estimation as pacing) were weighed
against *one permit per item* (chosen — Pros: simple and exact to
account; Cons: the memory bound is indirect — 10k of even 400 KB
worst-case items caps at ~4 GB, while typical populations stay in the
tens of MB). Revisit with resource vectorization (§7).

Termination and aborts: the monitor runs alongside the producer and
completes on `producer_done && admitted == resolved` (Release/Acquire pair
— an observed `true` implies the admitted total is final). A stall abort
closes the admission semaphore, which unblocks a producer waiting on
`acquire`; a producer-side read/parse error stops admission, lets the
admitted prefix drain, and surfaces after the pipeline settles. Error
precedence: stall abort > producer error > permanent write failures.

**Measured**: 30 MB / 33,359 items → 18.3 MB max RSS; 300 MB / 333,588
items → 19.9 MB (10× input, flat memory), exact counts, throughput
unchanged. Unit tests pin each source's streaming, early-stop and
error-position semantics; an integration test pins "invalid jsonl document
aborts with the admitted prefix written".

### 5.4 Lifecycle and abort

#### 5.4.1 Progress deadline (livelock protection)

The monitor aborts the pipeline when successful + failed has not advanced
for 300 s (`ProgressStalled`). This closes the §5.1.3 infinite-retry
livelock and any unforeseen stall (including a hung producer).
`StallDetector` is pure logic with injected time. On abort, in-flight
retry sends whose receiver is gone log-and-drop instead of panicking —
correct because the import is already reporting an error.

#### 5.4.2 Abort responsiveness and defensive accounting

- **The stall abort preempts the executor's token wait**: `TaskExecutor`
  accepts an external shutdown signal (`with_shutdown`, wired to the
  pipeline's terminate channel). Without it, an abort decided during a
  long token wait (low target × large batch estimate) left the process
  sleeping on the bucket for up to hours, because the pipeline awaits the
  executor before reporting. On shutdown the executor abandons
  not-yet-executed work, waits only for already-spawned request tasks,
  and returns. Pinned by a virtual-time unit test.
- **Known limitation (accepted)**: a panic inside a spawned request task
  returns its concurrency permit (owned permit drops on unwind) but
  records no accounting; its items resolve nowhere and the run ends via
  the progress deadline rather than immediately. Such a panic is itself a
  bug; the deadline — prompt thanks to the shutdown signal — is the
  designed safety net.
- Shutdown latency of the slow loop is bounded by one in-flight
  `GetMetricStatistics` call (accepted).

### 5.5 Interface: `--max-wcu`, no ceiling by default

When the option is absent there is **no ceiling at all**: pacing is left
to the initial-target derivation, AIMD feedback, and informed recovery.
Two defaults were weighed: *a built-in ceiling constant* (Pros: a bounded
worst case even if the controller misbehaves; Cons: any single number is
wrong for most tables — far too high for a small provisioned table, an
artificial limit on a large one — and it hides the fact that pacing is
the congestion controller's job) versus *no default ceiling* (chosen —
Pros: one owner for pacing; the ceiling exists only for the user who
wants to *impose* a bound, e.g. explicit headroom for production; Cons:
correctness of the controller and of the finite-start invariant below
carries all the weight).

"No ceiling" is `f64::INFINITY` through the existing plumbing, so no
executor signature changes. The arithmetic is exact — clamping into
`[min, ∞]` preserves the initial target, recovery and boosts `min(∞)`
never cap, halving stays finite — **provided the effective target starts
finite** (invariant 5). Clap rejects zero/negative/NaN/∞; unbounded is
expressible only by omission. Verified live: no option → effective target
4,000 (ceiling ∞) paced at ~4k items/s; `--max-wcu 100` → a steady ~100
items/s for 1 KB items.

## 6. Executor selection: benchmark evidence

The executor topology was an explicit open question settled by
measurement, not intuition. Four candidates were implemented behind a
runtime switch: fixed worker pools (16 workers with per-worker queues and
partitioned per-worker buckets; the same with queue depth 1), a pool fed
by one shared MPMC queue, and task-per-request + single shared bucket. All
four shared the same accounting, AIMD controller and governor. They were
measured on real DynamoDB at account-quota scale (39,000-WCU provisioned
tables, fresh table per repetition, us-west-2; uniform ~1 KB, mixed
~2.9 WCU avg, and uniform 35 KB workloads; byte-identical inputs per
mix), on Graviton first and confirmed on AMD x86. Findings:

- **Adopted: task-per-request + shared bucket + governor-capped
  in-flight.** No candidate hit a disqualifying regression; the adopted
  model is best-or-tied on throughput in every regime and uniformly best
  on token waste (31–78k WCU wasted vs the pools' 55–124k), CPU
  (e.g. 117k items/cpu-s vs 85k/65k for the pools on the small mix) and
  RSS (21–61 MB vs up to 152 MB). The losing implementations and the
  switch were deleted from the codebase (preserved at the archive tag).
- **The decisive evidence is adherence, not mean throughput**: over all
  24 quota-scale repetitions, per-second consumed/target adherence during
  saturation was 0.997–1.001 (mean) and ≥0.935 (P5); on the highest
  plateau the controller ever requested (37.8k WCU/s), every candidate
  sustained ≥0.977. No executor was ever the bottleneck; the ceiling was
  DynamoDB's. This is what justifies consolidating on the simplest model.
- **Rep-to-rep throughput variance is controller trajectory, not
  executor**: 6/8 mixed-workload reps followed an identical AIMD
  trajectory and finished within 0.1% of each other; the outliers match
  their mean-target integrals exactly. Comparisons must condition on the
  trajectory (count throttle events per rep) or they measure random
  differences in the controller's path, not the executor. The same held
  on x86 (6/6 exit 0, adherence
  1.000–1.002, one rep −24% exactly proportional to its mean target).
- **Throttling at quota scale is partition-granular**: a 39k-WCU table has
  ~40 partitions × 1,000 WCU/s hard caps, and per-second multinomial
  fluctuation alone crosses a partition cap from ~95% of provisioned —
  observed at 97% with uniformly hashed keys. Headroom budgeting must
  target the per-partition cap, not the aggregate.
- **The spawn-overhead assumption was refuted**: task spawn costs ~µs
  against 5–50 ms network calls, and the task model deletes the
  distribution machinery entirely (concurrency emerges from rate ×
  latency; AIMD just updates the shared refill). The pools' presumed CPU
  affinity advantage was not observed in practice (they cost *more* CPU — workers
  are ordinary tokio tasks and migrate anyway).
- **A retained lesson for any future queue-based executor**: while the
  pools were alive, a deadlock was found and fixed where control signals
  (rate updates) shared the work queue with items — a control message
  occupies a queue slot but completes no work and thus fires no completion
  notification; one buffered control signal at queue depth 1 blocked the
  dispatcher forever. The invariant to preserve: **everything on a work
  queue must run to completion, and control traffic needs its own
  channel**. The full analysis is archived (pool1-deadlock-analysis.md at
  the tag).

Testing and reproduction notes for future contributors:

- `src/algo/sim.rs` is a virtual-time simulation of the whole
  executor/controller stack (`cargo test --bin dy sim_ -- --ignored`); it
  found the bucket f64 busy-wait bug (§5.2.1) before any cloud spend
- DynamoDB Local **neither throttles nor emits CloudWatch metrics**;
  throttling behavior is testable only against real tables
- A real table accumulates ~300 s of unused burst capacity; a freshly
  created or idle table will not throttle, so deplete the burst first or
  the experiment measures nothing
- DynamoDB's consumed-capacity metric backfills zero datapoints stamped
  up to ~9 minutes *before* table creation — use CloudTrail, not metrics,
  for existence timelines

## 7. Future work

1. **Resource vectorization** — the implemented design keeps the
   resource a scalar f64 in every layer (single-table WCU). Generalize
   to a small
   vector keyed by resource (`Table(name)` / `Gsi(table, index)`):
   multi-table BatchWriteItem grouping in the chunker, all-or-nothing
   multi-key token consumption (`estimate_available_at` = max over keys),
   per-GSI throttle attribution. Now — while `BatchWriteProcess` is the
   only trait implementor — is the cheapest moment for this breaking
   change
2. **Export**: parallel `Scan` on the same foundation (the RCU variant)
3. **Bootstrap** onto the foundation (still on the naive path)
4. **Pipe modes**: stdin → DynamoDB and Scan → stdout (§1.3). Note that
   failure reporting must become incremental here (e.g. a dead-letter
   output for permanently failed items): an unbounded stream has no end
   at which a summary like `PermanentWriteFailure(count)` could be
   reported
5. Scale-in (the governor can only grow; shrinking is unimplemented),
   moving the generic parts of `BatchWriteProcess` into `algo`, reducing
   `expect`s
