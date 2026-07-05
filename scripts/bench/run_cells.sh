#!/usr/bin/env bash
# Per-instance benchmark cell runner (benchmark-plan.md §5.2 step 4).
#
# Reads config.json (schema: see scripts/bench/README.md), selects the cells
# assigned to this shard, and for each cell x repetition:
#   1. creates a provisioned table. Cheap tables (< REUSE_WCU_THRESHOLD) are
#      fresh per rep (burst capacity reset, import-throttling.md §6);
#      expensive tables are created once per (wcu, shard) and reused across
#      cells because DynamoDB bills provisioned capacity at hourly
#      granularity and the docs do not promise sub-hour proration for
#      deleted tables — 24 short-lived 39k-WCU tables could bill up to 24
#      table-hours, one reused table bills its actual wall-clock hours.
#      Reuse is safe for Tier-1: with the initial target at 80% of the
#      provisioned WCU and no co-located writer these cells never throttle,
#      so burst-capacity reset is irrelevant (item overwrites consume the
#      same WCU as fresh puts)
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

# Tables provisioned at or above this WCU are created once and reused across
# cells (see the billing note in the header).
REUSE_WCU_THRESHOLD=1000

log() { printf '[run_cells] %s %s\n' "$(date -u +%FT%TZ)" "$*"; }

# --- safety nets -------------------------------------------------------------
cleanup() {
    local rc=$?
    set +e
    log "cleanup (exit code $rc)"
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
    # trail when a run dies mid-way (there is no SSH/SSM on the fleet).
    mkdir -p "$WORK_DIR/artifacts"
    cp /var/log/dynein-bench.log "$WORK_DIR/artifacts/boot-shard-$SHARD.log" 2>/dev/null || true
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
# Run cheap cells first and expensive cells last, contiguously: high-WCU
# tables are reused across cells (billing note in the header), and grouping
# them keeps the reused table's lifetime — and its billed hours — minimal.
ordered = sorted(
    (c for c in cfg["cells"] if int(c.get("shard", 0)) == shard),
    key=lambda c: c["wcu"],
)
for cell in ordered:
    avg = AVG_WCU[cell["mix"]]
    budget = cell.get("budget_secs") or int(math.ceil(cell["items"] * avg / cell["wcu"]) + 300)
    # Deterministic per-cell seed; identical across reps so repetitions
    # measure run-to-run variance on the same input (and the generated
    # file is reused across reps -- matters for quota-scale multi-GB inputs).
    seed = cell.get("seed")
    if seed is None:
        seed = zlib.crc32(cell["cell_id"].encode()) & 0x7FFFFFFF
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
    mkdir -p "$dir"
    log "=== cell=$cell_id rep=$rep executor=$executor mix=$mix wcu=$wcu items=$items budget=${budget}s table=$table"

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
        until aws dynamodb create-table --region "$REGION" --table-name "$table" \
            --attribute-definitions AttributeName=pk,AttributeType=S \
            --key-schema AttributeName=pk,KeyType=HASH \
            --provisioned-throughput "ReadCapacityUnits=5,WriteCapacityUnits=$wcu" \
            --tags "Key=dynein-bench,Value=$RUN_ID" >/dev/null 2>"$dir/create-table.err"; do
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
    #    generated file is cached and reps run on identical input).
    local input="$WORK_DIR/input-$mix-$items-$seed.jsonl"
    if ! [ -f "$input" ]; then
        python3 "$SCRIPT_DIR/gen_input.py" \
            --mix "$mix" --items "$items" --seed "$seed" --out "$input"
    fi

    # 3. Pseudo production workload, if the cell asks for it.
    local prod_pid=""
    if [ "$prod_rate" != "0" ] && [ "$prod_rate" != "0.0" ]; then
        # NOTE: pseudo_prod_writer.py pins region ap-northeast-1 internally.
        python3 "$SCRIPT_DIR/../pseudo_prod_writer.py" \
            --table "$table" --rate "$prod_rate" --duration "$((budget * 2))" \
            > "$dir/prod.log" 2>&1 &
        prod_pid=$!
    fi

    # 4. Background system sampling.
    local pidstat_pid=""
    if [ "$PIDSTAT_OK" = "1" ]; then
        pidstat -h -u -r -C dy 1 > "$dir/pidstat.txt" 2>&1 &
        pidstat_pid=$!
    fi

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

    local started ended rc=0
    started=$(date +%s)
    printf 'y\n' | timeout --kill-after=30 "$((budget * 2))" \
        script -qec "$inner" /dev/null > "$dir/run.log" 2>&1 || rc=$?
    ended=$(date +%s)
    log "cell=$cell_id rep=$rep finished: exit=$rc wall=$((ended - started))s"

    [ -n "$pidstat_pid" ] && { kill "$pidstat_pid" 2>/dev/null || true; }
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

    # 6. Upload artifacts, then drop the table. Reused tables stay for the
    #    following cells and are removed by the exit cleanup (deleting and
    #    recreating them would multiply the billed table-hours).
    aws s3 cp --recursive "$dir" "$S3_PREFIX/$cell_id/rep$rep/" >/dev/null
    if [ "$reuse" = "0" ]; then
        aws dynamodb delete-table --region "$REGION" --table-name "$table" >/dev/null
        aws dynamodb wait table-not-exists --region "$REGION" --table-name "$table"
    fi
}

while IFS=$'\t' read -r cell_id executor mix wcu items prod_rate rep budget seed; do
    run_one "$cell_id" "$executor" "$mix" "$wcu" "$items" "$prod_rate" \
        "$rep" "$budget" "$seed"
done < "$CELL_LINES"

# Instance-level summary marker (progress is observed via S3 object counts).
date -u +%FT%TZ > "$WORK_DIR/artifacts/shard-$SHARD.done"
aws s3 cp "$WORK_DIR/artifacts/shard-$SHARD.done" "$S3_PREFIX/shard-$SHARD.done" >/dev/null
log "shard $SHARD complete"
