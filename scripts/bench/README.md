# Benchmark harness (`scripts/bench/`)

Implements the execution infrastructure of `docs/design/benchmark-plan.md`
(§3 workloads, §4 metrics, §5 disposable EC2 → S3 → self-terminate, §6 items
5–7). Read that document first; this README only covers how the pieces fit.

## Pieces

| File | Role | Runs on |
|------|------|---------|
| `setup_aws.sh` | One-time: S3 bucket `dynein-bench-<account-id>` + IAM role/instance profile `dynein-bench-instance` (least privilege, §5.1). Idempotent. Run only with user approval, then record the ARNs in `CLAUDE.local.md` | dev machine |
| `preflight.sh` | **Gate G1** (postmortem): platform facts + capacity/cost arithmetic + rendered-template checks + watchdog self-test; must exit 0 before any `--execute`. `launch.sh --execute` runs it automatically | dev machine |
| `launch.sh` | Orchestrator: renders `config.json` from the cell matrix, uploads it, launches the fleet. **Defaults to `--dry-run`; pass `--execute` to touch AWS.** `--canary` launches the G2 canary (1 instance, 2 cells); `--execute` refuses a full matrix without a canary PASS marker for the same commit | dev machine |
| `canary_verify.sh` | **Gate G2**: verifies a canary run end-to-end (exit 0, budgets, flat max-RSS = streaming proof, tables deleted, instances terminated) and writes `s3://<bucket>/canary/<commit>/PASS` | dev machine |
| `watchdog.sh` | Off-instance monitor **with abort authority**: CloudWatch stall detection, spend ceiling, heartbeat age, external cell deadlines. Abort = tables → SSM salvage → stop instances → report. Start it right after every `--execute` | dev machine |
| `user_data.sh.tpl` | EC2 user-data template (`{{RUN_ID}}`, `{{SHARD}}`, `{{BUCKET}}`, `{{REGION}}`, `{{COMMIT_SHA}}`, `{{INSTANCE_TYPE}}`); installs tooling, obtains the `dy` binary, clones the repo at the pinned SHA, then hands off to the `dynein-bench-runner` systemd unit whose `OnFailure=` guardian unit cleans up even when the runner is SIGKILLed | EC2 (rendered by launch.sh) |
| `run_cells.sh` | Per-instance runner: table create → input gen → import under `/usr/bin/time -v` + perf + pidstat, inside a `systemd-run` scope with `MemoryMax` (an OOM fails the rep, not the runner) → `result.json` → S3 upload → table delete; uploads a per-minute heartbeat; EXIT trap cleans up and self-terminates | EC2 (or locally for testing) |
| `instance_cleanup.sh` | Guardian executed by the `OnFailure=` unit: deletes the run's tables, salvages artifacts/logs, shuts down — survives the runner's death | EC2 |
| `gen_input.py` | Deterministic jsonl input generator for the §3 item mixes | both |
| `collect.sh` | Downloads `runs/<run-id>/results/` from S3 | dev machine |
| `analyze.py` | Parses the collected tree, prints the markdown comparison table (mean ± σ across reps) | dev machine |
| `sweep.sh` | Lists/deletes leftover `dynein-bench-*` tables and tagged instances (list-only by default) | dev machine |
| `tests/watchdog_test.sh` | Mock-AWS self-test of every watchdog abort branch (also run by preflight) | dev machine |

## Flow (gates G0–G3, `benchmark-run-postmortem.md` §3)

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

# 3. G2 canary: ONE instance, one w10 cell + one 1000-WCU mixed cell.
#    (--execute auto-runs preflight = G1/G0.)
scripts/bench/launch.sh --canary --execute --bucket <bucket> --region <r> \
    --subnet-id <subnet> --security-group <sg>
scripts/bench/watchdog.sh --run-id <canary-id> --bucket <bucket> \
    --region <r> --budget-usd 5          # in a second terminal, immediately

# 4. verify the canary and write the PASS marker (gates the full launch):
scripts/bench/canary_verify.sh --run-id <canary-id> --bucket <bucket> --region <r>

# 5. full launch (refuses without the canary PASS marker for this commit):
scripts/bench/launch.sh --bucket <bucket> --run-id <id> --execute \
    --region <r> --subnet-id <subnet> --security-group <sg>
scripts/bench/watchdog.sh --run-id <id> --bucket <bucket> --region <r> \
    --budget-usd <agreed ceiling>        # in a second terminal, immediately

# 6. collect + analyze:
scripts/bench/collect.sh <id> --bucket <bucket>
python3 scripts/bench/analyze.py scripts/bench/out/<id>/results

# 7. verify nothing leaked (tables cost money; instances too):
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
- **Input cache**: generated inputs are content-addressed
  (`s3://<bucket>/inputs/input-<mix>-<items>-<seed>.jsonl`) and reused across
  instances and runs — only the first instance ever to need an input
  generates and uploads it; everyone else downloads (~1 min for the largest
  input vs minutes of generation). This also pins the exact input bytes
  across compared runs. Storage is ~86GB ≈ $2/month for the full Tier-1 set;
  clear it with `aws s3 rm --recursive s3://<bucket>/inputs/` when the
  benchmark season ends. (IAM: the instance role needs the `inputs/*`
  statements added by the current `setup_aws.sh` — re-run it once.)
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
- system-level 1s samplers, identical for every cell (needed to *explain*
  anomalies, not just detect them):
  - `mpstat.txt` — per-core %usr/%sys/%iowait/%irq/%soft/**%steal**
  - `iostat.txt` — per-device r/s, w/s, MB/s, await, **aqu-sz** (queue
    depth), %util (the gp3 root volume is a 125MB/s device)
  - `meminfo.txt` — MemFree/MemAvailable/Buffers/**Cached**/Dirty/Writeback
    (page-cache behavior of the streaming reads)
  - `netdev.txt` — cumulative interface byte/packet/error counters
  - `pidstat.txt` — dy's own CPU, RSS, disk I/O (-d) and context
    switches (-w)
- `run.log`, `time.txt`, `perf.data` (perf is skipped gracefully when
  unavailable), `prod.log` (prod-on cells)

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
