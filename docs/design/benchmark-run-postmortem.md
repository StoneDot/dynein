# Postmortem: Tier-1 Benchmark Run Attempts (2026-07-05)

- Status: Final
- Scope: the *process* failures of the four EC2 launch attempts
  (runs 084300, 085028, 090224, 090552), not the product OOM bug they
  surfaced (that finding lives in `import-throttling.md` §6)
- Outcome: run 090552 aborted by **user decision**; ~US$20–30 spent
  (mostly the 39k table's 34 minutes + idle stall), 7 valid results
  salvaged. **Without user intervention the idle 39k table would have
  burned toward ~$100+**: the AI-side monitor did not detect the stall

## 1. Timeline of failures

| # | Failure | How it was found | Was it foreseeable? |
|---|---------|------------------|---------------------|
| 1 | Launch into account without default VPC failed | `run-instances` error at launch | Yes — one `describe-vpcs` beforehand |
| 2 | m9g.xlarge does not exist in ap-northeast-1 | launch error + user pointed at region cost | Yes — `describe-instance-type-offerings` |
| 3 | 2×40k quota cells + residual table exceed the 80k account quota | DDB Local happened to emulate the validation | Yes — arithmetic on known quotas |
| 4 | Billing granularity for short-lived provisioned tables unknown; cost estimate quoted anyway (twice) | User asked "how did you estimate this?" | Yes — the estimate's key assumption was never flagged as unverified |
| 5 | user-data aborted on `HOME` unset + `set -u`; fleet idled doing nothing | SSM inspection (only possible because SSH/SSM had just been added **on user request**) | Yes — the template had **never been executed anywhere**, not even in a container |
| 6 | 8GB root volume vs ~86GB of generated inputs | Noticed during an unrelated SSM check, 15 min before ENOSPC | Yes — input sizes were exact, computable constants |
| 7 | Input generation idles the billed 39k table 4–8 min per cell | User asked about the idle metric | Yes — visible in the §5.2 step ordering at design time |
| 8 | Mid-run background pregen risked asymmetric interference with the measurement | User asked "doesn't this affect the benchmark?" | Yes — I introduced it myself as a hotfix without a validity check |
| 9 | **dy import OOM-killed on the 8.5GB input; systemd took the runner down; cleanup trap never fired; 39k table sat idle ~20 min** | **User** noticed zero consumption; kernel log confirmed OOM | Yes — `import-throttling.md` §4.2 *explicitly states* the importer holds the whole file in memory. File sizes and instance RAM were known. One line of arithmetic (8.5GB × ~2 parse expansion > 16GB) was never done |
| 10 | Monitor (results count / instance liveness / table count) saw nothing wrong during the stall | — | Yes — it watched *liveness*, not *health*; "silence looks identical to progress" was a known monitoring pitfall |

## 2. Root causes

1. **No canary, scale-first.** Four full-matrix launches; not one
   single-cell end-to-end run on the real platform. Every failure above
   would have surfaced in a ~15-minute, <$1 canary — including the OOM
   (a single quota-mixed rep dies identically at any matrix size).
2. **Tested components, untested glue.** run_cells.sh was smoke-tested
   locally with stubs; sim/DDB Local/small-WCU covered the *product*. The
   boot path (user-data → tooling → build → runner) and the
   platform assumptions (VPC, instance types, quotas, RAM, disk) were
   never exercised until real money was on the meter.
3. **No written risk pass before spending.** Capacity arithmetic
   (RAM vs input size, disk vs Σ inputs, account WCU quota vs concurrent
   tables, cost model per billing interpretation) uses only known
   constants — none of it was done up front. Known facts weren't
   cross-referenced either: the §4.2 memory note that predicts the OOM
   was in a document written the same week.
4. **Monitoring watched liveness, not health, and had no authority.**
   No stall detection (consumption ≈ 0 while an expensive table exists),
   no per-cell external deadline, no spend ceiling, no automatic abort.
   Every intervention required a human reading CloudWatch.
5. **On-instance failure domain too coarse.** The import, the runner
   loop, and the cleanup trap lived in one cgroup: the OOM killed the
   *guardian* along with the workload, which is exactly when the
   guardian was needed.

## 3. Mandatory gates before the next paid run

**G0 — capacity & cost arithmetic (written into the run plan):**
max in-memory input estimate (file size × 2.5) < instance RAM;
Σ input files + build + artifacts < root volume; max concurrent
provisioned WCU (incl. residual tables) < account quota; cost stated
under BOTH billing interpretations (prorated and hour-rounded), each
under an agreed ceiling.

**G1 — mechanical preflight (`scripts/bench/preflight.sh`, must exit 0):**
instance-type availability in the target AZ; subnet/SG/IGW reachability;
IAM instance profile + SSM policy present; S3 bucket RW; user-data
rendering has no unsubstituted `{{}}`; `bash -n` on every script;
G0 numbers recomputed from the actual config.json.

**G2 — canary on the real platform:** one instance, `--canary` cell set =
1× w10 rep + 1× reduced quota rep (~1000 WCU, 2 min) **including one
mixed-size input scaled to the RAM check boundary**. Pass = artifacts
complete in S3, exit 0, table deleted, instance self-terminated.
`launch.sh --execute` refuses to run a full matrix unless a canary PASS
marker for the same commit exists in S3 (`--skip-canary-check` requires
an explicit human decision).

**G3 — staged scale-up:** w10-only matrix → single quota-small rep →
full quota block. Each stage reviewed before the next.

## 4. AI-side monitoring so a human is not the safety net

The monitor must watch **health and money, with authority to act**:

1. **Stall watchdog (off-instance, the session-side monitor):** for every
   expensive table, poll CloudWatch `ConsumedWriteCapacityUnits` each
   minute. `consumption < 5% of provisioned for 5 consecutive minutes
   while the table exists` → automatic action: SSM-inspect the runner
   unit; if it is dead or wedged, delete the run's tables, stop (not
   terminate) the instances, notify. The stall in run 090552 would have
   been caught at minute 5 (~$2) instead of minute 20+ (user).
2. **Spend ceiling:** the run plan carries a hard budget (table-hours ×
   rate, worst-case billing). The watchdog integrates elapsed
   table-hours and aborts the run when the projection crosses the
   budget. Abort = tables first, instances second, always salvage logs.
3. **Progress deadline per cell (external):** expected wall time is in
   config.json (`budget_secs`); if a cell exceeds budget × 2 *as observed
   from S3 timestamps*, treat as wedged (the on-instance `timeout` may
   itself be dead, as happened here).
4. **On-instance failure isolation:**
   - wrap the `dy` invocation in `systemd-run --scope -p MemoryMax=<RAM-4G>`
     so an OOM kills the *cell* (recorded as a failed rep, runner
     continues) instead of the runner unit;
   - `OnFailure=dynein-bench-cleanup.service` on the runner unit: an
     independent, memory-capped unit that deletes the run's tables,
     salvages artifacts and shuts down — the guardian survives the
     workload's death.
5. **Heartbeat over silence:** the runner appends a per-minute heartbeat
   line (cell, rep, phase) to S3; the watchdog alarms on heartbeat age,
   not only on results count. Silence must be distinguishable from
   slow progress.

## 5. What went right (kept deliberately)

- Sim → DDB Local → small-WCU laddering caught real product bugs cheaply
  *for the product*; the failure was not extending the same ladder to the
  experiment infrastructure.
- SSM access (user request) turned every later failure from "blind" into
  "diagnosable in minutes"; it is now a permanent part of the fleet.
- Fresh-table costs were bounded by design decisions taken *before* the
  spend (serial 39k table, billing-safe reuse) — the remaining risk was
  operational, not architectural.
- All aborts preserved data (salvage to S3) and stayed within small
  absolute cost.
