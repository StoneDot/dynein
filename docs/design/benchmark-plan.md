# Benchmark Plan: Executor/Chunker Architecture for Throttled Import

- Status: Planned (not executed yet)
- Parent document: `import-throttling.md` (see §5.3/§5.4 there for the open questions this plan settles)
- Last updated: 2026-07-05

This document is deliberately detailed so that the work can be resumed from
scratch (by a human or an agent) without the original conversation context.

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
| C  | Task-per-request: chunker acquires a `Semaphore` permit (max concurrency = current `DEFAULT_MAX_CONCURRENT_CONNECTION`), spawns one tokio task per BatchWriteItem request, **shared bucket** (single rate limiter), AIMD updates the shared refill directly | Removes Signal channels, round-robin, and all scale-out logic; concurrency emerges from rate × latency. Reuses `ResourceConstraintProcess` unchanged |
| B  | Shared MPMC process queue (async-channel), fixed workers pulling, split buckets | Reserve: only evaluated if C is disqualified but A's queueing is proven bad |

Chunker variants (Q5, combined only with A/A′/B): `single` (current) vs
`multi8` (the async-channel branch approach).

**Prediction to falsify**: C matches or beats A on throughput and token waste
with drastically less code; A retains an edge only in CPU time, if anywhere.

## 3. Workloads

Discriminating power matters: with uniform items and ample WCU, *all*
candidates will sit at the target and look identical. The differences only
appear under heterogeneity and low per-worker rates.

- **Item mix**
  - `uniform`: ~110 B items (1 WCU each)
  - `mixed`: 90% × 110 B + 10% × ~20 KB (20 WCU each) — drives Q2/Q3
- **Table WCU (provisioned)**: 2 / 10 / 100
- **Pseudo production**: off / on (rate = 50% of table WCU, using
  `scripts/pseudo_prod_writer.py`) — exercises AIMD + slow-loop interplay per
  architecture
- **Duration**: each cell sized for ≥5 minutes of steady state (item count =
  ceil(WCU × 400) items for uniform; mixed scaled by average WCU/item ≈ 3)
- **Repetitions**: 3 per cell

**Tiering** (the full cross-product is too large to run at once):

- **Tier 1** (settles Q1–Q4): executors {A, A′, C} × WCU {2, 100} × mix
  {uniform, mixed} × prod off × 3 reps = 36 cells ≈ 4 h serial per instance
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

1. **Candidate switch**: implement A′ and C selectable at runtime (env var
   `DYNEIN_BENCH_EXECUTOR=pool16|pool1|task`) so one binary per arch covers
   all candidates; C is a prototype module beside `ThrottledExecutor`
   implementing the same "consume `Receiver<T>`, respect AIMD" contract
2. **Stats emitter** (`DYNEIN_BENCH_STATS`, §4)
3. **Input generator** for the item mixes with deterministic seeds (extend
   the existing scratchpad generators into `scripts/bench/gen_input.py`)
4. **Harness scripts** (`scripts/bench/`): user-data template, launch.sh,
   collect.sh, analyze.py, sweep.sh
5. S3 bucket + IAM role/instance profile (one-time setup, document ARNs in
   `CLAUDE.local.md` once created)

## 7. Decision Rules

- Adopt **C** if, across all Tier-1 cells: throughput ≥ A − 3%, token waste ≤
  A, and CPU time ≤ 1.5 × A. Rationale: C deletes a large amount of executor
  code, so it wins ties
- If C is disqualified, evaluate **B**, and keep whichever of A/A′ measured
  better as the fallback
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
