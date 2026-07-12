#!/usr/bin/env bash
# Per-instance benchmark cell runner (benchmark-plan.md §5.2 step 4).
#
# Reads config.json (schema: see scripts/bench/README.md), selects the cells
# assigned to this shard, and for each cell x repetition:
#   1. creates a provisioned table, fresh per rep (burst capacity reset,
#      import-throttling.md §6, plus no cross-rep partition-heat carryover).
#      Measured billing behavior (benchmark-plan.md §8, 2026-07-07):
#      provisioned capacity bills only COMPLETE clock hours of table
#      existence — partial hours are dropped, not rounded up — so a
#      sub-60-minute per-rep table bills nothing, while a table reused
#      across cells lives for hours and accrues real charge. Fresh-per-rep
#      is therefore both the cheapest and the best-isolated strategy.
#      The reuse branch below is retained but disabled (threshold sentinel);
#      if billing ever contradicts the measured model, launch from commit
#      8f4cdc5 (its canary PASS marker covers the reuse strategy)
#   2. generates the input file with gen_input.py (deterministic seed)
#   3. optionally starts the pseudo production writer
#   4. runs `dy import` under /usr/bin/time -v + perf record (if available)
#      + pidstat, inside a pty (dialoguer prompt) and under `timeout`
#   5. writes result.json, uploads the artifact dir to S3, deletes the table
#      (per-rep tables only; reused tables are deleted by the exit cleanup)
#
# Usage:
#   DY_BIN=/path/to/dy [INSTANCE_TYPE=...] [RUNNING_ON_EC2=1] \
#     run_cells.sh <config.json> <shard-id>
#
# Environment:
#   DY_BIN          path to the dy binary (required)
#   INSTANCE_TYPE   label used in the S3 result prefix (default: local)
#   RUNNING_ON_EC2  1 => schedule hard shutdown guard + shutdown on exit
#   WORK_DIR        scratch dir for inputs/artifacts (default: mktemp -d)
#   HARD_SHUTDOWN_MINUTES  last-resort shutdown guard (default: 480 = 8 h)

set -euo pipefail

CONFIG="${1:?usage: run_cells.sh <config.json> <shard-id>}"
SHARD="${2:?usage: run_cells.sh <config.json> <shard-id>}"
DY_BIN="${DY_BIN:?set DY_BIN to the dy binary path}"
INSTANCE_TYPE="${INSTANCE_TYPE:-local}"
RUNNING_ON_EC2="${RUNNING_ON_EC2:-0}"
HARD_SHUTDOWN_MINUTES="${HARD_SHUTDOWN_MINUTES:-480}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK_DIR="${WORK_DIR:-$(mktemp -d /tmp/dynein-bench.XXXXXX)}"
mkdir -p "$WORK_DIR"

# --- config values -----------------------------------------------------------
cfg() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))[sys.argv[2]])' "$CONFIG" "$1"; }
RUN_ID="$(cfg run_id)"
REGION="$(cfg region)"
BUCKET="$(cfg bucket)"
COMMIT_SHA="$(cfg commit_sha)"

S3_PREFIX="s3://$BUCKET/runs/$RUN_ID/results/$INSTANCE_TYPE"

# Sentinel: table reuse is disabled — every rep gets a fresh table (billing
# note in the header: partial clock hours are unbilled, so short-lived
# fresh tables are free while reused ones accrue complete hours). Lower this
# back to e.g. 1000 only if the measured billing model is contradicted.
REUSE_WCU_THRESHOLD=999999999

log() { printf '[run_cells] %s %s\n' "$(date -u +%FT%TZ)" "$*"; }

# --- safety nets -------------------------------------------------------------
cleanup() {
    local rc=$?
    set +e
    log "cleanup (exit code $rc)"
    [ -n "${HEARTBEAT_PID:-}" ] && kill "$HEARTBEAT_PID" 2>/dev/null
    # Best-effort: delete any leftover tables of this run.
    local tables
    tables=$(aws dynamodb list-tables --region "$REGION" \
        --output text --query 'TableNames[]' 2>/dev/null | tr '\t' '\n' \
        | grep "^dynein-bench-${RUN_ID}-" || true)
    for t in $tables; do
        log "deleting leftover table $t"
        aws dynamodb delete-table --region "$REGION" --table-name "$t" >/dev/null 2>&1
    done
    # Best-effort: upload whatever partial artifacts exist, plus the boot
    # log — the instance self-terminates, so this is the only post-mortem
    # trail when a run dies mid-way.
    mkdir -p "$WORK_DIR/artifacts"
    cp /var/log/dynein-bench.log "$WORK_DIR/artifacts/boot-shard-$SHARD.log" 2>/dev/null || true
    # The runner executes as a systemd unit, so its own orchestration log
    # (retry loops, cell timings, this cleanup) lives in journald, not in
    # the boot log — salvage it on every exit, not only via the OnFailure
    # guardian (the 2026-07-05 canary's 15-minute IAM retry loop left no
    # trace in the uploaded logs because only the failure path saved it).
    journalctl -u dynein-bench-runner.service --no-pager -n 5000 \
        > "$WORK_DIR/artifacts/runner-shard-$SHARD.log" 2>/dev/null || true
    if [ -d "$WORK_DIR/artifacts" ]; then
        aws s3 cp --recursive "$WORK_DIR/artifacts" "$S3_PREFIX/" >/dev/null 2>&1
    fi
    if [ "$RUNNING_ON_EC2" = "1" ]; then
        log "shutting down instance"
        shutdown -h now 2>/dev/null || sudo shutdown -h now 2>/dev/null
    fi
}
trap cleanup EXIT

if [ "$RUNNING_ON_EC2" = "1" ]; then
    # Last-resort cost guard, independent of this script's fate.
    shutdown +"$HARD_SHUTDOWN_MINUTES" 2>/dev/null \
        || sudo shutdown +"$HARD_SHUTDOWN_MINUTES" 2>/dev/null \
        || log "WARNING: could not schedule hard shutdown guard"
fi

# --- heartbeat (postmortem §4.5: silence must differ from slow progress) ------
# run_one keeps $WORK_DIR/phase.json updated with the current cell/rep/phase;
# this loop stamps it with a timestamp and uploads it every minute. The
# off-instance watchdog alarms on the heartbeat's age, so a wedged or dead
# runner becomes visible within minutes even when S3 result counts look
# plausible.
HEARTBEAT_OBJ="s3://$BUCKET/runs/$RUN_ID/heartbeat/$INSTANCE_TYPE-s$SHARD.json"
set_phase() {
    printf '{"cell":"%s","rep":"%s","phase":"%s","cell_started":%s}\n' \
        "$1" "$2" "$3" "${4:-null}" > "$WORK_DIR/phase.json"
}
set_phase "-" "-" "boot" ""
heartbeat_loop() {
    while true; do
        python3 - "$WORK_DIR/phase.json" "$WORK_DIR" <<'PYEOF' > "$WORK_DIR/heartbeat.json" 2>/dev/null
import json, os, sys, time
work_dir = sys.argv[2]
try:
    body = json.load(open(sys.argv[1]))
except Exception:
    body = {"phase": "unknown"}
body["ts"] = int(time.time())
# F2 (watchdog-silence-postmortem.md): a fresh heartbeat proves the loop is
# alive, not that dy is resolving items. Attach the stats-derived
# resolved_items counter so the watchdog can detect a wedge whose heartbeat
# stays fresh. Only meaningful mid-import — create-table/pregen/upload have
# no active stats file; the watchdog phase-gates on this.
if body.get("phase") == "import" and body.get("cell") not in (None, "-"):
    stats = os.path.join(work_dir, "artifacts", str(body["cell"]),
                         "rep" + str(body.get("rep", "")), "stats.jsonl")
    try:
        last = None
        with open(stats) as f:
            for line in f:
                line = line.strip()
                if line:
                    last = line
        if last:
            body["progress"] = int(json.loads(last).get("resolved_items", 0))
    except Exception:
        pass
print(json.dumps(body))
PYEOF
        aws s3 cp "$WORK_DIR/heartbeat.json" "$HEARTBEAT_OBJ" >/dev/null 2>&1 || true
        sleep 60
    done
}
heartbeat_loop &
HEARTBEAT_PID=$!

# --- memory isolation (postmortem §4.4) ----------------------------------------
# The measured dy process runs in its own systemd scope with a MemoryMax a
# few GB below the instance RAM: a runaway import is then OOM-killed inside
# the scope (recorded as a failed rep; the runner continues) instead of the
# kernel picking a victim in the runner's cgroup and taking the guardian
# down with the workload — which is how run 090552 left a 39k table idling.
MEMORY_SCOPE_OK=0
if [ "$RUNNING_ON_EC2" = "1" ] && command -v systemd-run >/dev/null 2>&1; then
    mem_total_kb=$(awk '/MemTotal/ {print $2}' /proc/meminfo)
    # Leave 4GB for the OS, the runner and the AWS CLI.
    DY_MEMORY_MAX_KB=$((mem_total_kb - 4 * 1024 * 1024))
    if [ "$DY_MEMORY_MAX_KB" -gt $((2 * 1024 * 1024)) ] \
        && systemd-run --scope --quiet -p MemoryMax=1G -- true >/dev/null 2>&1; then
        MEMORY_SCOPE_OK=1
        log "dy runs under systemd scope MemoryMax=$((DY_MEMORY_MAX_KB / 1024 / 1024))G"
    else
        log "WARNING: systemd-run scope unavailable; dy runs unisolated"
    fi
fi

# --- input cache ---------------------------------------------------------------
# gen_input.py is deterministic in (mix, items, seed), so generated inputs are
# cached in S3 under a content-addressed name and shared across instances AND
# runs: regenerating the multi-GB quota inputs on every instance of every run
# wastes minutes of fleet time each, and a download (~1 min for the largest
# input, same region) is strictly faster. The cache also pins the exact input
# bytes across runs — even a Python upgrade that changes RNG details cannot
# silently alter the workload between two compared runs.
S3_INPUT_CACHE="s3://$BUCKET/inputs"
ensure_input() {  # ensure_input <mix> <items> <seed>; file lands at the shared path
    local mix=$1 items=$2 seed=$3
    local name="input-$mix-$items-$seed.jsonl"
    local input="$WORK_DIR/$name"
    [ -f "$input" ] && return 0
    # Download via a temp name: an interrupted `aws s3 cp` leaves a partial
    # file at the destination, which the -f check above would then trust.
    if aws s3 cp "$S3_INPUT_CACHE/$name" "$input.part" --only-show-errors 2>/dev/null; then
        mv "$input.part" "$input"
        log "input cache hit: $name"
        return 0
    fi
    rm -f "$input.part"
    log "generating $name"
    python3 "$SCRIPT_DIR/gen_input.py" \
        --mix "$mix" --items "$items" --seed "$seed" --out "$input"
    # Best-effort: later instances/runs skip generation. Failure is fine —
    # the local file exists and the run proceeds.
    aws s3 cp "$input" "$S3_INPUT_CACHE/$name" --only-show-errors 2>/dev/null \
        || log "WARNING: input cache upload failed (continuing)"
}

# --- tool availability -------------------------------------------------------
PERF_OK=0
if command -v perf >/dev/null 2>&1 \
    && perf record -o "$WORK_DIR/.perfcheck.data" -- true >/dev/null 2>&1; then
    PERF_OK=1
    rm -f "$WORK_DIR/.perfcheck.data"
else
    log "perf unavailable or not permitted; skipping flame-graph capture"
fi
PIDSTAT_OK=0
command -v pidstat >/dev/null 2>&1 && PIDSTAT_OK=1

# --- cell iteration ----------------------------------------------------------
# One line per (cell, rep): cell_id executor mix wcu items prod_rate rep budget seed
CELL_LINES="$WORK_DIR/cells.tsv"
python3 - "$CONFIG" "$SHARD" > "$CELL_LINES" <<'PYEOF'
import json, sys, zlib, math
cfg = json.load(open(sys.argv[1]))
shard = int(sys.argv[2])
AVG_WCU = {"uniform-small": 1.0, "mixed": 2.9, "uniform-large": 35.0}
# Run cheap cells first and expensive cells last. With fresh-per-rep tables
# the ordering no longer affects billing (header note); it is kept so the
# cheap smoke cells still fail fast before any high-WCU capacity exists.
ordered = sorted(
    (c for c in cfg["cells"] if int(c.get("shard", 0)) == shard),
    key=lambda c: c["wcu"],
)
for cell in ordered:
    avg = AVG_WCU[cell["mix"]]
    budget = cell.get("budget_secs") or int(math.ceil(cell["items"] * avg / cell["wcu"]) + 300)
    # Deterministic seed per (mix, items) — NOT per cell: every executor
    # then processes byte-identical input (stronger comparability), and the
    # generated file is shared across cells and reps (quota-scale inputs
    # are multi-GB; per-cell seeds needed ~4x the disk).
    seed = cell.get("seed")
    if seed is None:
        seed = zlib.crc32(f"{cell['mix']}-{cell['items']}".encode()) & 0x7FFFFFFF
    for rep in range(1, int(cell["reps"]) + 1):
        print("\t".join(str(x) for x in [
            cell["cell_id"], cell["executor"], cell["mix"], cell["wcu"],
            cell["items"], cell.get("prod_rate", 0), rep, budget, seed,
        ]))
PYEOF

if ! [ -s "$CELL_LINES" ]; then
    log "no cells assigned to shard $SHARD; nothing to do"
    exit 0
fi
log "shard $SHARD: $(wc -l < "$CELL_LINES") cell-reps to run (run $RUN_ID, commit $COMMIT_SHA)"

run_one() {
    local cell_id=$1 executor=$2 mix=$3 wcu=$4 items=$5 prod_rate=$6 rep=$7 budget=$8 seed=$9
    local table="dynein-bench-${RUN_ID}-${cell_id}-${rep}"
    local dir="$WORK_DIR/artifacts/$cell_id/rep$rep"
    local cell_started
    cell_started=$(date +%s)
    mkdir -p "$dir"
    log "=== cell=$cell_id rep=$rep executor=$executor mix=$mix wcu=$wcu items=$items budget=${budget}s table=$table"
    set_phase "$cell_id" "$rep" "create-table" "$cell_started"

    # 1. Table. Expensive tables are reused across cells (billing note in
    #    the header); cheap ones are fresh per rep. Quota-scale cells from
    #    several instances can transiently exceed the account-level
    #    provisioned-capacity quota (default 80k WCU); retry with backoff so
    #    the instances serialize on the quota instead of failing the cell.
    local reuse=0
    if [ "$wcu" -ge "$REUSE_WCU_THRESHOLD" ]; then
        reuse=1
        table="dynein-bench-${RUN_ID}-shared-w${wcu}-${INSTANCE_TYPE}-s${SHARD}"
    fi
    if [ "$reuse" = "0" ] || ! aws dynamodb describe-table --region "$REGION" \
        --table-name "$table" >/dev/null 2>&1; then
        local create_attempts=0
        # Append (not truncate) so a failing attempt's error text survives
        # the retry that eventually succeeds — the 2026-07-05 IAM incident
        # left an empty err file because the last attempt overwrote it.
        until aws dynamodb create-table --region "$REGION" --table-name "$table" \
            --attribute-definitions AttributeName=pk,AttributeType=S \
            --key-schema AttributeName=pk,KeyType=HASH \
            --provisioned-throughput "ReadCapacityUnits=5,WriteCapacityUnits=$wcu" \
            --tags "Key=dynein-bench,Value=$RUN_ID" >/dev/null 2>>"$dir/create-table.err"; do
            create_attempts=$((create_attempts + 1))
            if [ "$create_attempts" -ge 30 ]; then
                log "SKIP cell=$cell_id rep=$rep: create-table kept failing: $(tail -1 "$dir/create-table.err")"
                return 1
            fi
            log "create-table failed (attempt $create_attempts, likely account capacity quota); retrying in 60s"
            sleep 60
        done
    fi
    aws dynamodb wait table-exists --region "$REGION" --table-name "$table"

    # 2. Input file (deterministic; same seed for all reps of a cell, so the
    #    file is shared across reps and cached in S3 across instances/runs).
    #    Normally a no-op: the pregen loop below already fetched everything.
    local input="$WORK_DIR/input-$mix-$items-$seed.jsonl"
    ensure_input "$mix" "$items" "$seed"

    # 3. Pseudo production workload, if the cell asks for it.
    local prod_pid=""
    if [ "$prod_rate" != "0" ] && [ "$prod_rate" != "0.0" ]; then
        # NOTE: pseudo_prod_writer.py pins region ap-northeast-1 internally.
        python3 "$SCRIPT_DIR/../pseudo_prod_writer.py" \
            --table "$table" --rate "$prod_rate" --duration "$((budget * 2))" \
            > "$dir/prod.log" 2>&1 &
        prod_pid=$!
    fi

    # 4. Background system sampling (1s cadence, identical for every cell so
    #    it cannot bias the comparison). Process-level pidstat is joined by
    #    system-level samplers: benchmark anomalies need %steal/%iowait,
    #    page-cache behavior and disk queue depth to be explainable, not
    #    just dy's own CPU/RSS.
    local sampler_pids=()
    if [ "$PIDSTAT_OK" = "1" ]; then
        # -d: dy's own disk I/O; -w: its context switches.
        pidstat -h -u -r -d -w -C dy 1 > "$dir/pidstat.txt" 2>&1 &
        sampler_pids+=($!)
        # Per-core CPU incl. %usr %sys %iowait %irq %soft %steal.
        mpstat -P ALL 1 > "$dir/mpstat.txt" 2>&1 &
        sampler_pids+=($!)
    fi
    if command -v iostat >/dev/null 2>&1; then
        # Extended device stats: r/s w/s rMB/s wMB/s await aqu-sz %util.
        iostat -dxz 1 > "$dir/iostat.txt" 2>&1 &
        sampler_pids+=($!)
    fi
    # Page cache / dirty writeback / available memory, straight from /proc.
    (
        while true; do
            date -u +%FT%TZ
            grep -E '^(MemFree|MemAvailable|Buffers|Cached|Dirty|Writeback|SwapFree):' /proc/meminfo
            sleep 1
        done
    ) > "$dir/meminfo.txt" 2>&1 &
    sampler_pids+=($!)
    # Interface byte/packet/error counters (cumulative; diff at analysis).
    (
        while true; do
            date -u +%FT%TZ
            grep -v -e 'lo:' -e '|' -e 'face' /proc/net/dev
            sleep 1
        done
    ) > "$dir/netdev.txt" 2>&1 &
    sampler_pids+=($!)

    # The import command. dialoguer's provisioned-table prompt needs a pty
    # (import-throttling.md §6), hence `script -qec`. /usr/bin/time -v gives
    # max RSS + user/sys CPU (§4 / Q4). timeout = budget x 2: a hung run must
    # not block the fleet.
    local inner="/usr/bin/time -v -o '$dir/time.txt' \
env DYNEIN_BENCH_STATS='$dir/stats.jsonl' DYNEIN_BENCH_EXECUTOR='$executor' RUST_LOG=dy=info \
'$DY_BIN' -r '$REGION' import -t '$table' -f jsonl -i '$input'"
    if [ "$PERF_OK" = "1" ]; then
        inner="perf record -F 99 -g -o '$dir/perf.data' -- $inner"
    fi
    if [ "$MEMORY_SCOPE_OK" = "1" ]; then
        # Own scope: a memory blowup OOM-kills this rep, not the runner.
        inner="systemd-run --scope --quiet -p MemoryMax=${DY_MEMORY_MAX_KB}K -- $inner"
    fi

    set_phase "$cell_id" "$rep" "import" "$cell_started"
    local started ended rc=0
    started=$(date +%s)
    printf 'y\n' | timeout --kill-after=30 "$((budget * 2))" \
        script -qec "$inner" /dev/null > "$dir/run.log" 2>&1 || rc=$?
    ended=$(date +%s)
    log "cell=$cell_id rep=$rep finished: exit=$rc wall=$((ended - started))s"

    local sp
    for sp in "${sampler_pids[@]}"; do
        kill "$sp" 2>/dev/null || true
    done
    if [ -n "$prod_pid" ]; then
        kill "$prod_pid" 2>/dev/null || true
        wait "$prod_pid" 2>/dev/null || true
    fi

    # 5. result.json (metadata + parsed /usr/bin/time fields).
    RESULT_CELL="$cell_id" RESULT_REP="$rep" RESULT_EXECUTOR="$executor" \
    RESULT_MIX="$mix" RESULT_WCU="$wcu" RESULT_ITEMS="$items" \
    RESULT_PROD_RATE="$prod_rate" RESULT_SEED="$seed" RESULT_EXIT="$rc" \
    RESULT_WALL="$((ended - started))" RESULT_RUN_ID="$RUN_ID" \
    RESULT_COMMIT="$COMMIT_SHA" RESULT_INSTANCE="$INSTANCE_TYPE" \
    RESULT_TABLE="$table" \
    python3 - "$dir/time.txt" > "$dir/result.json" <<'PYEOF'
import json, os, re, sys
time_fields = {}
try:
    with open(sys.argv[1]) as f:
        text = f.read()
    def grab(pattern, cast=float):
        m = re.search(pattern, text)
        return cast(m.group(1)) if m else None
    time_fields = {
        "user_cpu_secs": grab(r"User time \(seconds\): ([\d.]+)"),
        "sys_cpu_secs": grab(r"System time \(seconds\): ([\d.]+)"),
        "max_rss_kb": grab(r"Maximum resident set size \(kbytes\): (\d+)", int),
    }
except OSError:
    pass
e = os.environ
print(json.dumps({
    "run_id": e["RESULT_RUN_ID"],
    "commit_sha": e["RESULT_COMMIT"],
    "instance_type": e["RESULT_INSTANCE"],
    "cell_id": e["RESULT_CELL"],
    "rep": int(e["RESULT_REP"]),
    "executor": e["RESULT_EXECUTOR"],
    "mix": e["RESULT_MIX"],
    "wcu": float(e["RESULT_WCU"]),
    "items": int(e["RESULT_ITEMS"]),
    "prod_rate": float(e["RESULT_PROD_RATE"]),
    "seed": int(e["RESULT_SEED"]),
    "table": e["RESULT_TABLE"],
    "exit_code": int(e["RESULT_EXIT"]),
    "wall_seconds": int(e["RESULT_WALL"]),
    **time_fields,
}, indent=2))
PYEOF

    # 6. Upload artifacts, then drop the table. (If the disabled reuse branch
    #    is ever re-enabled, reused tables are removed by the exit cleanup.)
    set_phase "$cell_id" "$rep" "upload" "$cell_started"
    aws s3 cp --recursive "$dir" "$S3_PREFIX/$cell_id/rep$rep/" >/dev/null
    if [ "$reuse" = "0" ]; then
        aws dynamodb delete-table --region "$REGION" --table-name "$table" >/dev/null
        aws dynamodb wait table-not-exists --region "$REGION" --table-name "$table"
    fi
}

# Pre-fetch/generate every input before the first table exists: obtaining
# the multi-GB quota inputs takes minutes, and a provisioned high-WCU table
# idling while an input is prepared is pure billed waste.
set_phase "-" "-" "pregen" ""
while IFS=$'\t' read -r _cell_id _executor mix _wcu items _prod_rate _rep _budget seed; do
    ensure_input "$mix" "$items" "$seed"
done < "$CELL_LINES"

while IFS=$'\t' read -r cell_id executor mix wcu items prod_rate rep budget seed; do
    run_one "$cell_id" "$executor" "$mix" "$wcu" "$items" "$prod_rate" \
        "$rep" "$budget" "$seed"
done < "$CELL_LINES"

# Instance-level summary marker (progress is observed via S3 object counts).
set_phase "-" "-" "done" ""
date -u +%FT%TZ > "$WORK_DIR/artifacts/shard-$SHARD.done"
aws s3 cp "$WORK_DIR/artifacts/shard-$SHARD.done" "$S3_PREFIX/shard-$SHARD.done" >/dev/null
log "shard $SHARD complete"
