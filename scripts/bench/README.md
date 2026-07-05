# Benchmark harness (`scripts/bench/`)

Implements the execution infrastructure of `docs/design/benchmark-plan.md`
(§3 workloads, §4 metrics, §5 disposable EC2 → S3 → self-terminate, §6 items
5–7). Read that document first; this README only covers how the pieces fit.

## Pieces

| File | Role | Runs on |
|------|------|---------|
| `setup_aws.sh` | One-time: S3 bucket `dynein-bench-<account-id>` + IAM role/instance profile `dynein-bench-instance` (least privilege, §5.1). Idempotent. Run only with user approval, then record the ARNs in `CLAUDE.local.md` | dev machine |
| `launch.sh` | Orchestrator: renders `config.json` from the cell matrix, uploads it, launches the fleet. **Defaults to `--dry-run`; pass `--execute` to touch AWS** | dev machine |
| `user_data.sh.tpl` | EC2 user-data template (`{{RUN_ID}}`, `{{SHARD}}`, `{{BUCKET}}`, `{{REGION}}`, `{{COMMIT_SHA}}`, `{{INSTANCE_TYPE}}`); installs tooling, obtains the `dy` binary, clones the repo at the pinned SHA, runs `run_cells.sh` | EC2 (rendered by launch.sh) |
| `run_cells.sh` | Per-instance runner: table create → input gen → import under `/usr/bin/time -v` + perf + pidstat → `result.json` → S3 upload → table delete; EXIT trap cleans up and self-terminates | EC2 (or locally for testing) |
| `gen_input.py` | Deterministic jsonl input generator for the §3 item mixes | both |
| `collect.sh` | Downloads `runs/<run-id>/results/` from S3 | dev machine |
| `analyze.py` | Parses the collected tree, prints the markdown comparison table (mean ± σ across reps) | dev machine |
| `sweep.sh` | Lists/deletes leftover `dynein-bench-*` tables and tagged instances (list-only by default) | dev machine |

## Flow

```
# 0. one time, with user approval (idempotent):
scripts/bench/setup_aws.sh

# 1. inspect what would happen (writes config.json + user-data locally):
scripts/bench/launch.sh --bucket dynein-bench-<acct> --dry-run

# 2. (optional but recommended) pre-upload prebuilt binaries so instances
#    skip the ~5 min on-instance build. Build with the prof profile and
#    tokio_unstable (flame-graph symbols + RuntimeMonitor, plan §4):
#    RUSTFLAGS="--cfg tokio_unstable" cargo build --profile prof --bin dy
#    aws s3 cp target/prof/dy s3://<bucket>/runs/<run-id>/binaries/x86_64/dy
#    (and an aarch64 cross-build under .../binaries/aarch64/dy)

# 3. launch for real:
scripts/bench/launch.sh --bucket dynein-bench-<acct> --run-id <id> --execute

# 4. watch progress (no SSH; S3 object counts only):
aws s3 ls --recursive s3://<bucket>/runs/<id>/results/ | wc -l

# 5. collect + analyze:
scripts/bench/collect.sh <id> --bucket <bucket>
python3 scripts/bench/analyze.py scripts/bench/out/<id>/results

# 6. verify nothing leaked (tables cost money; instances too):
scripts/bench/sweep.sh            # list
scripts/bench/sweep.sh --delete   # remove leftovers
```

Instances self-terminate: `run_cells.sh` has a `trap ... EXIT` that deletes
leftover tables of its run, uploads partial results and runs
`shutdown -h now` (instances are launched with
`--instance-initiated-shutdown-behavior terminate`). A hard `shutdown +480`
scheduled at start is the last-resort cost guard.

## config.json schema

Produced by `launch.sh`, consumed by `run_cells.sh`, stored at
`s3://<bucket>/runs/<run-id>/config.json`:

```json
{
  "run_id": "20260705-120000",
  "commit_sha": "<40-char pinned SHA, never a branch name>",
  "region": "ap-northeast-1",
  "bucket": "dynein-bench-<account-id>",
  "num_shards": 2,
  "cells": [
    {
      "cell_id": "pool16-w10-small",
      "executor": "pool16",          // pool16|pool1|mpmc|task → DYNEIN_BENCH_EXECUTOR
      "mix": "uniform-small",        // uniform-small|mixed|uniform-large
      "wcu": 10,                     // provisioned WCU of the per-rep table
      "items": 4000,                 // input item count
      "prod_rate": 0,                // >0 starts pseudo_prod_writer.py at this WCU/s
      "reps": 3,                     // repetitions (fresh table per rep)
      "budget_secs": 700,            // expected duration; timeout = budget × 2
      "shard": 0,                    // which instance (per type) runs this cell
      "seed": 12345                  // optional; default = crc32(cell_id)
    }
  ]
}
```

Notes:

- **Sharding**: cells are assigned round-robin to `num_shards` shards; every
  instance *type* runs all shards (Q6 compares types on identical cells).
  `run_cells.sh <config> <shard>` executes only its shard.
- **Seed** is per cell (not per rep): repetitions run on identical input, so
  they measure run-to-run variance, and multi-GB quota-scale inputs are
  generated once per cell.
- **Item counts** (plan §3): `items = ceil(WCU × 400 / avgWCU)` with
  avgWCU = 1 (uniform-small), 2.9 (mixed), 35 (uniform-large) — ≥ 400 s of
  steady state per cell.
- Table names are `dynein-bench-<run-id>-<cell-id>-<rep>`; a fresh table per
  rep also resets DynamoDB burst capacity (`import-throttling.md` §6 —
  measuring throttling on an idle table is meaningless).
- The dialoguer confirmation prompt on provisioned tables needs a pty:
  `run_cells.sh` uses `printf 'y\n' | script -qec "..." /dev/null`.
- `pseudo_prod_writer.py` currently pins region `ap-northeast-1` internally;
  prod-on cells only work in that region until it grows a `--region` flag.

## Per-rep artifacts

Uploaded to
`s3://<bucket>/runs/<run-id>/results/<instance-type>/<cell-id>/rep<k>/`:

- `result.json` — run metadata + exit code, wall seconds, parsed
  `/usr/bin/time -v` fields (max RSS, user/sys CPU)
- `stats.jsonl` — the dy binary's per-second stats
  (`DYNEIN_BENCH_STATS`): one JSON object per line with cumulative
  `consumed_wcu`, `requests`, `throttled`, `resolved_items`, `failed_items`,
  the current `effective_target`, and optional `tokio` task metrics
- `run.log`, `time.txt`, `pidstat.txt`, `perf.data` (perf is skipped
  gracefully when unavailable), `prod.log` (prod-on cells)

`analyze.py` turns the tree into the §4 comparison table: wall time,
effective items/s, completion tail t(100%)−t(90%), target adherence
(mean ± σ of consumed rate vs effective target), token waste
∫(target − consumed) while backlog exists, throttled counts, CPU, max RSS.

## Cost note (plan §5.4)

- EC2: 2 types × 2 instances × ~4 h × ~US$0.2/h ≈ **< US$2** per Tier-1 sweep
  (on-demand; instances self-terminate)
- DynamoDB: tables exist only for the minutes their cell runs. Low/mid cells
  are cents; a 40k-WCU quota-scale table costs ~US$0.50/min — the
  create-just-before / delete-right-after discipline in `run_cells.sh` is
  what keeps this small. **Always run `sweep.sh` after a fleet run** to
  confirm nothing leaked.
- S3/CloudWatch: negligible.
