# Benchmark Plan: Executor/Chunker Architecture for Throttled Import

- Status: Planned (not executed yet)
- Parent document: `import-throttling.md` (see §5.3/§5.4 there for the open questions this plan settles)
- Last updated: 2026-07-05 (rev 2: per-regime hypotheses, quota-scale WCU, deep observability)

This document is deliberately detailed so that the work can be resumed from
scratch (by a human or an agent) without the original conversation context.

## 0. Current Status (checklist)

**Nothing has been executed yet** — neither the simulation scenarios nor any
EC2/quota-scale AWS runs. Do not launch EC2 instances or create quota-scale
tables until the simulation phase is done and the user approves the fleet
run.

- [x] Algo layer migrated to `tokio::time::Instant` so it runs under virtual
  time (commit `8c3584d`) — the only preparation done so far
- [ ] Candidate switch: A′ (queue depth 1), B (needs `async-channel` dep),
  C prototype; runtime selection via `DYNEIN_BENCH_EXECUTOR`
- [ ] Simulation scenarios implemented and run (§2.5) → record predictions
- [ ] Stats emitter, tokio-metrics integration, input generator
- [ ] Harness scripts + S3 bucket + IAM instance profile
- [ ] Tier-1 EC2 sweep → decision per §7

## 1. Questions to Answer

- **Q1 (executor topology)**: Is the current fixed worker pool with per-worker
  queues and split buckets the right architecture, or is a
  task-per-request model simpler *and* at least as fast?
- **Q2 (token waste of partitioned buckets)**: Quantify the hypothesis that
  split per-worker buckets lose throughput under heterogeneous item sizes:
  idle workers' buckets overflow and discard refill while busy workers starve.
  A shared bucket cannot lose tokens this way. Feedback corrects intra-worker
  estimation error but not inter-worker imbalance.
- **Q3 (queue-depth skew)**: The per-worker queue depth is 16 batches. Once a
  queue is deep, the distributor cannot rebalance it, so slow/expensive items
  hold up to 16×25 items hostage on one worker (visible as a long completion
  tail). How much of architecture A's weakness disappears by just setting the
  queue depth to 1?
- **Q4 (task overhead / CPU affinity)**: The task-per-request model was
  originally avoided for spawn overhead, and the worker-pool model was
  credited with CPU cache affinity. Both claims are unverified: tokio spawn
  costs ~µs against ~5–50 ms network calls, and unpinned worker tasks migrate
  across runtime threads anyway (work stealing), so the affinity advantage
  may be illusory. Decide with measured CPU time, not intuition.
- **Q5 (chunker parallelism)**: single mpsc chunker vs 8 async-channel
  chunkers (branch `improve-export-import-async-channel-queue`). Only
  relevant if a fixed-pool topology survives Q1; the task-per-request model
  spawns from the chunker directly and removes this axis.
- **Q6 (platform)**: price-performance on the instance families we would
  actually use: m9g.xlarge (Graviton/ARM) vs m8a.xlarge (AMD/x86_64).

## 2. Candidates

| ID | Executor | Notes |
|----|----------|-------|
| A  | Current: fixed pool, per-worker queue (depth 16), round-robin `try_send` with skip-on-full, split buckets (`target/n`) | Baseline = branch HEAD |
| A′ | A with per-worker queue depth 1 (`CHANNEL_BUFFER_SIZE` 16→1) | One-line change; isolates the queue-depth-skew effect (Q3) |
| B  | Shared MPMC process queue (async-channel), fixed workers pulling, split buckets | Work-conserving queueing while keeping the pool model |
| C  | Task-per-request: chunker acquires a `Semaphore` permit (max concurrency = current `DEFAULT_MAX_CONCURRENT_CONNECTION`), spawns one tokio task per BatchWriteItem request, **shared bucket** (single rate limiter), AIMD updates the shared refill directly | Removes Signal channels, round-robin, and all scale-out logic; concurrency emerges from rate × latency. Reuses `ResourceConstraintProcess` unchanged |

Chunker variants (Q5, combined only with A/A′/B): `single` (current) vs
`multi8` (the async-channel branch approach).

**Granularity note for C**: the task unit is one BatchWriteItem *request*
(≤25 items), never one item. Per-item tasks would mean millions of spawns
for large imports (seconds of pure overhead plus memory) and the 25-item
chunking is needed for the API anyway — the request is the natural unit.
Spawn cost (~µs, a few hundred bytes) is three orders of magnitude below the
5–50 ms network call it wraps.

**Per-regime hypotheses (to falsify)** — each candidate is expected to have a
regime where it wins, and the workloads are designed around these:

| Regime | Expected winner | Reasoning |
|--------|-----------------|-----------|
| Many small uniform items (high request rate) | **A** | Per-request overhead ratio is highest here; the pool amortizes distribution and avoids per-request spawns; a shared queue/bucket sees its highest contention |
| Small and large items randomly mixed | **B** | The shared queue is work-conserving (no hostage batches behind a big one), while the fixed pool still amortizes overhead |
| Mostly large items | **C** | Long, variable per-request service times; scheduling flexibility dominates and spawn overhead is fully negligible |

A dissenting sub-hypothesis worth recording: C may also win the mixed regime
(shared *bucket* removes token waste that B — split buckets — still has).
The simulation and the EC2 runs arbitrate; whichever way it falls, the
result closes the question.

## 2.5 Simulation Phase (before any EC2 run)

Build the per-regime scenarios as **deterministic in-process simulations**
first, using tokio virtual time (`#[tokio::test(start_paused = true)]`; the
algo layer uses `tokio::time::Instant` throughout, so bucket refills, AIMD
timing and scale-out all run under paused time — minutes of simulated time
execute in milliseconds).

- **Setup**: a `SimProcess` implementing `ResourceConstraintProcess` with a
  configurable cost and simulated latency; a scenario runner that feeds the
  same request sequence (deterministic seed) to each candidate executor and
  records completion timestamps and consumed-capacity integrals
- **Server model, phase 1 — infinite capacity on purpose**: the simulated
  "server" only adds latency; every request consumes exactly its estimate
  (`actual == estimate`, never throttled). This deliberately isolates the
  *client-side* questions (token scheduling across split vs shared buckets,
  queue skew, tails): the throughput ceiling is the client target itself,
  AIMD stays inert, and the retry path (which lives in transfer.rs, not in
  the executor under test) stays out of the picture. A phase-2 variant with
  a server-side bucket returning partial grants can be added later to
  exercise AIMD dynamics per topology — do not mix the two phases
- **Scenarios** = the three regimes of the hypothesis table above, plus a
  low-rate variant (the regime where queue-depth hostage-taking is worst)
- **Metrics per scenario run**: makespan; utilization = ideal time
  (Σcost ÷ target rate) ÷ makespan; completion tail = t(100%) − t(90%);
  request completion timeline for plotting. Assertions in the test code
  check only completeness (all requests finished) — the comparative numbers
  are printed, not asserted, so a falsified hypothesis does not "break the
  build"
- **Implementation hooks required**: a queue-depth parameter on
  `ThrottledExecutor` (for A′; currently `CHANNEL_BUFFER_SIZE` is a const),
  the `async-channel` dependency (for B), and the C prototype. Determinism
  notes: `start_paused` implies the current-thread runtime; the worker
  spawn-jitter uses `rand::random`, which is acceptable noise but can be
  seeded if runs must be exactly reproducible
- **What simulation can and cannot decide**: it isolates scheduling and
  token-bucket semantics (queue skew, token waste, tails). It **cannot**
  observe CPU cost, cache effects, or lock contention — virtual time hides
  them. Therefore a simulated "A loses everywhere" would NOT disqualify A:
  A's hypothesized edge (small-uniform regime) is precisely the CPU-bound
  one and can only be confirmed on EC2. Record simulated results as
  predictions for the EC2 runs, not verdicts
- **Location**: `src/algo/sim.rs` (test-only module), scenarios as
  `#[ignore]`d tests run manually with
  `cargo test --bin dy sim_ -- --ignored --nocapture`

## 3. Workloads

Discriminating power matters: with uniform items and ample WCU, *all*
candidates will sit at the target and look identical. The differences only
appear under heterogeneity and low per-worker rates.

- **Item mix** (mirrors the per-regime hypotheses)
  - `uniform-small`: ~110 B items (1 WCU each) — A's regime
  - `mixed`: 90% × 110 B + 10% × ~20 KB (20 WCU each), randomly interleaved — B's regime, drives Q2/Q3
  - `uniform-large`: ~20–50 KB items — C's regime
- **Table WCU (provisioned)**: 2 / 10 / 100 for the low/mid regimes, plus
  high-rate cells at **1,000 / 10,000 / up to the account quota** (e.g.
  40,000) for the small-uniform regime — the current assumptions saturate
  far below where per-request overhead could matter. Cost stays small
  because every cell creates its provisioned table immediately before the
  run and deletes it right after completion (a 40k-WCU table at
  ~US$0.0007/WCU-h costs ~US$0.50/min — cells are minutes long)
- **Pseudo production**: off / on (rate = 50% of table WCU, using
  `scripts/pseudo_prod_writer.py`) — exercises AIMD + slow-loop interplay per
  architecture
- **Duration**: each cell sized for ≥5 minutes of steady state (item count =
  ceil(WCU × 400) items for uniform; mixed scaled by average WCU/item ≈ 3)
- **Repetitions**: 3 per cell

**Tiering** (the full cross-product is too large to run at once):

- **Tier 1** (settles Q1–Q4): executors {A, A′, B, C} × WCU {10, quota-scale}
  × mix {uniform-small, mixed, uniform-large} × prod off × 3 reps = 72 cells;
  shard across instances to keep wall-clock in the a-few-hours range
- **Tier 2** (production interplay): winner of Tier 1 + A, × WCU {10} × mix
  {uniform} × prod on × 3 reps
- **Tier 3** (Q5, only if a pool topology won): chunker {single, multi8} on
  the winning pool config
- **Q6** runs Tier 1 on both instance types (cells shard across instances, so
  wall-clock stays ~4 h in parallel)

## 4. Metrics

Each run must produce a machine-readable JSON result (plus the raw log):

- Wall time; effective items/s; completion tail = t(100%) − t(90%)
- Requests total / throttled-whole-request / requests-with-unprocessed /
  unprocessed item count (wasted request pressure)
- Consumed WCU integral (from `ReturnConsumedCapacity`) and a 1-second time
  series of {consumed, effective target, resolved items} → target adherence
  (mean ± σ of consumed rate vs target) and **token waste** = ∫target −
  ∫consumed during saturated periods
- CPU user/sys time and max RSS (`/usr/bin/time -v`) — settles Q4
- Run metadata: candidate ID, workload cell, commit SHA, instance type, run id

To maximize what each (expensive) run teaches us, every EC2 cell also
collects deep-dive artifacts:

- **Flame graph**: `perf record -F 99 -g` on the `dy` process →
  `perf script | stackcollapse | flamegraph.pl` (or `cargo flamegraph`).
  Build with the existing `[profile.prof]` (release + `debug = 1`) so
  symbols survive. Answers *where* CPU time differences come from, not just
  how big they are
- **tokio task metrics** (`tokio-metrics` crate): wrap each candidate's task
  paths in `TaskMonitor`s (pool workers vs spawned request tasks) and dump
  interval snapshots (poll counts, mean poll duration, scheduled delay,
  slow-poll ratio) into the stats stream. Runtime-level metrics
  (`RuntimeMonitor`) additionally need `RUSTFLAGS="--cfg tokio_unstable"` —
  enable it for bench builds only
- **System sampling**: `pidstat 1` (CPU%, RSS) and optionally
  `perf stat` (IPC, cache misses — the affinity question Q4 in hard numbers)

All artifacts upload to the same S3 prefix as the JSON results.

**Instrumentation prerequisite**: a lightweight stats emitter in `dy`
(e.g. env var `DYNEIN_BENCH_STATS=<path>` making the monitoring task append a
JSON line per second: cumulative consumed WCU, requests, throttled, effective
target, resolved items). Parsing human logs is too fragile for this.

## 5. Execution Infrastructure (disposable EC2 → S3 → self-terminate)

Goal: run many cells cheaply with zero babysitting. Instances are cattle: they
bootstrap themselves via user data, execute their shard of cells, persist
everything to S3, and terminate themselves.

### 5.1 Components

- **S3 bucket** `dynein-bench-<account-id>` (ap-northeast-1):
  - `runs/<run-id>/config.json` — the cell matrix, commit SHA, shard map
  - `runs/<run-id>/results/<instance-type>/<cell-id>/rep<k>/{result.json,run.log,time.txt}`
  - `runs/<run-id>/binaries/<arch>/dy` — optional prebuilt binaries
- **IAM instance profile** (least privilege):
  - DynamoDB: Create/Delete/DescribeTable + BatchWriteItem/PutItem/Scan on
    `arn:...:table/dynein-bench-*` only
  - `cloudwatch:GetMetricStatistics`
  - S3 Put/Get on the bucket prefix
  - No `ec2:TerminateInstances` needed: launch with
    `--instance-initiated-shutdown-behavior terminate` and self-terminate via
    `shutdown -h now`
- **EC2**: m9g.xlarge (arm64) and m8a.xlarge (x86_64), Amazon Linux 2023+,
  on-demand (cost is negligible at these durations; spot optional but adds
  interruption handling for little gain)

### 5.2 User-data flow (per instance)

1. Install git + rust toolchain (or download the prebuilt binary for the
   instance's arch from S3 — preferred once cross-builds are set up; building
   on-instance costs ~5 min and is an acceptable fallback)
2. Clone `https://github.com/StoneDot/dynein`, checkout the **pinned commit
   SHA** from `config.json` (never a branch name — runs must be reproducible)
3. Download `config.json`; select the shard assigned to this instance
   (shard key passed via user-data variable)
4. For each cell × repetition:
   a. Create a fresh table `dynein-bench-<run-id>-<cell>-<rep>` with the
      cell's WCU. **A fresh table also resets burst capacity** — this is why
      tables are per-run, not reused (see the burst pitfall in
      `import-throttling.md` §6)
   b. Generate the input file for the cell's item mix (deterministic seed)
   c. Start pseudo production if the cell requires it
   d. Run `dy import` under `/usr/bin/time -v` with `DYNEIN_BENCH_STATS`
      enabled, wrapped in `timeout` (cell budget × 2) — a hung run must not
      block the fleet
   e. Upload result.json, logs, time output to S3; delete the table
5. Upload an instance-level summary and `shutdown -h now`

Safety nets:

- `trap` on EXIT in the runner script: best-effort delete of any leftover
  `dynein-bench-*` tables, upload of whatever partial results exist, then
  shutdown — the instance terminates even on script failure
- A hard `shutdown +<N>` scheduled at boot (N = generous whole-run budget,
  e.g. 8 h) as the last-resort cost guard
- A local `sweep.sh` to list/delete leftover `dynein-bench-*` tables and
  stray tagged instances, runnable any time

### 5.3 Orchestration from the developer machine

- `scripts/bench/launch.sh`: generates run-id, renders config.json (cells,
  SHA, shards), uploads it, launches N instances per type with the templated
  user-data, tags them `dynein-bench=<run-id>`
- Progress is observed purely via S3 object counts (no SSH); a small
  `scripts/bench/collect.sh` downloads `runs/<run-id>/results/` and
  `scripts/bench/analyze.py` produces the comparison tables (per metric ×
  cell × candidate, with mean ± σ across reps)

### 5.4 Cost estimate

- EC2: 2 instances × ~4 h × ~US$0.2/h ≈ **< US$2 per full Tier-1 sweep**
- DynamoDB: provisioned 2–100 WCU tables for minutes each — cents; the 100
  WCU cells dominate at ~US$0.07/h each
- S3/CloudWatch: negligible

## 6. Prerequisite Work Items (before the first fleet run)

1. **Candidate switch**: implement A′, B and C selectable at runtime (env var
   `DYNEIN_BENCH_EXECUTOR=pool16|pool1|mpmc|task`) so one binary per arch
   covers all candidates; B needs the `async-channel` dependency; C is a
   prototype module beside `ThrottledExecutor` implementing the same
   "consume `Receiver<T>`, respect AIMD" contract
2. **Simulation phase** (§2.5): `SimProcess` + scenario runner + the four
   regime scenarios, recording predicted winners before any EC2 spend
3. **Stats emitter** (`DYNEIN_BENCH_STATS`, §4)
4. **tokio-metrics integration** (TaskMonitors per candidate path; bench-only
   `tokio_unstable` build flag for runtime metrics)
5. **Input generator** for the item mixes with deterministic seeds (extend
   the existing scratchpad generators into `scripts/bench/gen_input.py`)
6. **Harness scripts** (`scripts/bench/`): user-data template (installs
   perf/flamegraph tooling), launch.sh, collect.sh, analyze.py, sweep.sh
7. S3 bucket + IAM role/instance profile (one-time setup, document ARNs in
   `CLAUDE.local.md` once created)

## 7. Decision Rules

- Score every candidate per regime (throughput, token waste, tail, CPU
  time). Adopt the candidate with **no disqualifying regression in any
  regime** (>10% throughput loss or >2× CPU) and the best aggregate;
  simplicity breaks ties (C > B > A′ > A — C deletes the most code)
- If winners genuinely split by regime with large margins, a regime switch
  (pick the executor from the average item size, which is known after
  parsing the input) may be considered — but only with strong evidence;
  the added complexity must pay for itself
- Chunker multi8 (Q5) is adopted only if it improves throughput ≥ 5% on the
  winning pool topology (it adds a dependency and complexity)
- Record the outcome and the numbers in `import-throttling.md` §6 and close
  the corresponding open questions in §5.3

## 8. Known Pitfalls (carried over from earlier experiments)

- **Burst capacity** invalidates throttling measurements on fresh/idle
  tables; always create the table immediately before the cell or drain first
  (`pseudo_prod_writer.py --rate 100 --duration 60`)
- CloudWatch metrics lag 1–3 minutes; Tier-2 cells must run ≥ 8 minutes to
  observe slow-loop effects
- DynamoDB Local is useless here: no throttling, no CloudWatch metrics
- The dialoguer confirmation prompt on provisioned tables needs a pty:
  `printf 'y\n' | script -qec "dy ... import ..." /dev/null`
- Capacity decreases are limited to 4/day/table — another reason for
  fresh-table-per-cell instead of resizing
