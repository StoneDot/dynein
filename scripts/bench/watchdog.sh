#!/usr/bin/env bash
# Off-instance run watchdog with abort authority (postmortem §4).
#
# Watches HEALTH and MONEY — not liveness — and acts on its own:
#   1. Stall: for every expensive table, CloudWatch consumed WCU must stay
#      >= --stall-threshold x provisioned; --stall-minutes consecutive
#      violations while the table exists trigger an abort. (Run 090552's
#      idle 39k table would have been caught at minute 5, ~$2.)
#   2. Spend ceiling: integrates table-WCU-hours at the worst-case rate and
#      aborts when the projection (spent + one more billing hour) crosses
#      --budget-usd. The budget is a hard ceiling, not a notification.
#   3. Heartbeat age: runners upload a per-minute heartbeat; a running
#      instance whose heartbeat is stale (> --heartbeat-max-age) or absent
#      after the boot grace is treated as wedged.
#   4. External cell deadline: a heartbeat stuck in the same cell for more
#      than 2x its budget_secs is treated as wedged (the on-instance timeout
#      may itself be dead — that is exactly how run 090552 failed).
#
# Abort order (experiment-protocol §8): money first (delete the run's
# tables), evidence second (SSM-salvage logs, stop — never terminate — the
# instances), human last (loud report + s3 abort marker). Exit code 3.
#
# The watchdog exits 0 by itself when the run finishes cleanly (all shard
# done markers present, no tables, no running instances).
#
# Usage:
#   watchdog.sh --run-id ID --bucket B --region R --budget-usd N
#       [--interval 60] [--stall-minutes 5] [--stall-threshold 0.05]
#       [--heartbeat-max-age 300] [--boot-grace 1500] [--expensive-wcu 1000]
#       [--max-ticks N]
#
# Testing: all AWS access goes through $WATCHDOG_AWS (default: aws), so a
# mock CLI can drive every branch — see tests/watchdog_test.sh.

set -uo pipefail

RUN_ID="" BUCKET="" REGION="" BUDGET_USD=""
INTERVAL=60
STALL_MINUTES=5
STALL_THRESHOLD=0.05
HEARTBEAT_MAX_AGE=300
BOOT_GRACE=1500
EXPENSIVE_WCU=1000
MAX_TICKS=0   # 0 = unlimited
WCU_RATE=0.00065  # USD per WCU-hour (us-west-2 provisioned)

while [ $# -gt 0 ]; do
    case "$1" in
        --run-id) RUN_ID="$2"; shift ;;
        --bucket) BUCKET="$2"; shift ;;
        --region) REGION="$2"; shift ;;
        --budget-usd) BUDGET_USD="$2"; shift ;;
        --interval) INTERVAL="$2"; shift ;;
        --stall-minutes) STALL_MINUTES="$2"; shift ;;
        --stall-threshold) STALL_THRESHOLD="$2"; shift ;;
        --heartbeat-max-age) HEARTBEAT_MAX_AGE="$2"; shift ;;
        --boot-grace) BOOT_GRACE="$2"; shift ;;
        --expensive-wcu) EXPENSIVE_WCU="$2"; shift ;;
        --max-ticks) MAX_TICKS="$2"; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done
: "${RUN_ID:?--run-id required}" "${BUCKET:?--bucket required}"
: "${REGION:?--region required}" "${BUDGET_USD:?--budget-usd required (hard ceiling)}"

AWS="${WATCHDOG_AWS:-aws}"
STATE_DIR=$(mktemp -d /tmp/dynein-watchdog.XXXXXX)
trap 'rm -rf "$STATE_DIR"' EXIT

log() { printf '[watchdog] %s %s\n' "$(date -u +%FT%TZ)" "$*"; }

# --- AWS helpers (every call goes through $AWS for mockability) ----------------

run_tables() {  # -> lines of "table_name<TAB>wcu"
    local names t wcu
    names=$($AWS dynamodb list-tables --region "$REGION" \
        --output text --query 'TableNames[]' 2>/dev/null | tr '\t' '\n' \
        | grep "^dynein-bench-${RUN_ID}-" || true)
    for t in $names; do
        wcu=$($AWS dynamodb describe-table --region "$REGION" --table-name "$t" \
            --output text \
            --query 'Table.ProvisionedThroughput.WriteCapacityUnits' 2>/dev/null)
        [ -n "$wcu" ] && [ "$wcu" != "None" ] && printf '%s\t%s\n' "$t" "$wcu"
    done
}

run_instances() {  # $1 = state filter -> instance ids
    $AWS ec2 describe-instances --region "$REGION" \
        --filters "Name=tag:dynein-bench,Values=$RUN_ID" \
                  "Name=instance-state-name,Values=$1" \
        --query 'Reservations[].Instances[].InstanceId' --output text 2>/dev/null \
        | tr '\t' '\n' | grep . || true
}

consumed_rate() {  # $1 = table -> consumed WCU/s over the last complete minute
    local end start
    end=$(date -u -d '-60 seconds' +%FT%TZ 2>/dev/null || date -u +%FT%TZ)
    start=$(date -u -d '-240 seconds' +%FT%TZ 2>/dev/null || date -u +%FT%TZ)
    $AWS cloudwatch get-metric-statistics --region "$REGION" \
        --namespace AWS/DynamoDB --metric-name ConsumedWriteCapacityUnits \
        --dimensions "Name=TableName,Value=$1" \
        --start-time "$start" --end-time "$end" --period 60 --statistics Sum \
        --output text \
        --query 'sort_by(Datapoints,&Timestamp)[-1].Sum' 2>/dev/null \
        | awk '$1 == "None" || $1 == "" {print 0; next} {printf "%.2f", $1 / 60}'
}

heartbeats() {  # -> lines of "name<TAB>age_secs<TAB>cell<TAB>cell_started<TAB>phase"
    local keys k body
    keys=$($AWS s3 ls "s3://$BUCKET/runs/$RUN_ID/heartbeat/" 2>/dev/null \
        | awk '{print $NF}' | grep . || true)
    for k in $keys; do
        body=$($AWS s3 cp "s3://$BUCKET/runs/$RUN_ID/heartbeat/$k" - 2>/dev/null) || continue
        python3 - "$k" <<PYEOF
import json, sys, time
try:
    b = json.loads('''$body''')
    age = int(time.time()) - int(b.get("ts", 0))
    print(f"{sys.argv[1]}\t{age}\t{b.get('cell','-')}\t{b.get('cell_started') or 0}\t{b.get('phase','-')}")
except Exception:
    pass
PYEOF
    done
}

# --- abort (money -> evidence -> compute -> human) -----------------------------

abort_run() {
    local reason="$1"
    log "ABORT: $reason"

    # 1. Money: delete every table of this run.
    local t wcu
    while IFS=$'\t' read -r t wcu; do
        log "deleting table $t (wcu $wcu)"
        $AWS dynamodb delete-table --region "$REGION" --table-name "$t" \
            >/dev/null 2>&1 || log "WARNING: failed to delete $t"
    done < <(run_tables)

    # 2. Evidence: best-effort SSM salvage, then STOP (not terminate) so the
    #    machines stay inspectable.
    local ids
    ids=$(run_instances "running")
    if [ -n "$ids" ]; then
        # shellcheck disable=SC2086
        $AWS ssm send-command --region "$REGION" \
            --document-name "AWS-RunShellScript" \
            --instance-ids $ids \
            --parameters 'commands=["aws s3 cp --recursive /opt/dynein-bench/work/artifacts s3://'"$BUCKET"'/runs/'"$RUN_ID"'/salvage/$(hostname)/ || true","aws s3 cp /var/log/dynein-bench.log s3://'"$BUCKET"'/runs/'"$RUN_ID"'/salvage/$(hostname)-boot.log || true"]' \
            >/dev/null 2>&1 && sleep 30 || log "WARNING: SSM salvage failed"
        log "stopping instances: $ids"
        # shellcheck disable=SC2086
        $AWS ec2 stop-instances --region "$REGION" --instance-ids $ids \
            >/dev/null 2>&1 || log "WARNING: stop-instances failed"
    fi

    # 3. Human: abort marker + loud report.
    printf '{"run_id":"%s","aborted_at":"%s","reason":"%s","spent_usd":%.2f}\n' \
        "$RUN_ID" "$(date -u +%FT%TZ)" "$reason" "$SPENT_USD" > "$STATE_DIR/abort.json"
    $AWS s3 cp "$STATE_DIR/abort.json" \
        "s3://$BUCKET/runs/$RUN_ID/watchdog-abort.json" >/dev/null 2>&1
    echo
    echo "================ WATCHDOG ABORT ================"
    echo "run:    $RUN_ID"
    echo "reason: $reason"
    echo "spent:  ~\$$SPENT_USD (worst-case integration)"
    echo "tables deleted; instances stopped (NOT terminated) for inspection."
    echo "================================================"
    exit 3
}

# --- cell budgets from config ---------------------------------------------------
declare -A CELL_BUDGET
CELL_BUDGET_COUNT=0
if $AWS s3 cp "s3://$BUCKET/runs/$RUN_ID/config.json" "$STATE_DIR/config.json" >/dev/null 2>&1; then
    while IFS=$'\t' read -r cid budget; do
        [ -z "$cid" ] && continue
        CELL_BUDGET[$cid]=$budget
        CELL_BUDGET_COUNT=$((CELL_BUDGET_COUNT + 1))
    done < <(python3 - "$STATE_DIR/config.json" <<'PYEOF'
import json, sys
for c in json.load(open(sys.argv[1]))["cells"]:
    print(c["cell_id"] + "\t" + str(c.get("budget_secs", 600)))
PYEOF
    )
    log "loaded $CELL_BUDGET_COUNT cell budgets from config.json"
else
    log "WARNING: config.json not found; external cell deadlines disabled"
fi

# --- main loop -------------------------------------------------------------------
declare -A STALL_COUNT
declare -A HB_MISS_COUNT
SPENT_USD=0
WATCH_STARTED=$(date +%s)
TICK=0
SAW_ACTIVITY=0

log "watching run $RUN_ID (budget \$$BUDGET_USD, stall ${STALL_MINUTES}x${INTERVAL}s @ <$STALL_THRESHOLD, heartbeat max age ${HEARTBEAT_MAX_AGE}s)"

while :; do
    TICK=$((TICK + 1))

    # -- inventory
    TABLES=$(run_tables)
    RUNNING=$(run_instances "running,pending")
    if [ -n "$TABLES" ] || [ -n "$RUNNING" ]; then SAW_ACTIVITY=1; fi

    # -- 2. spend ceiling ---------------------------------------------------
    TOTAL_WCU=0
    if [ -n "$TABLES" ]; then
        TOTAL_WCU=$(printf '%s\n' "$TABLES" | awk -F'\t' '{s += $2} END {print s+0}')
    fi
    SPENT_USD=$(awk -v s="$SPENT_USD" -v w="$TOTAL_WCU" -v r="$WCU_RATE" -v i="$INTERVAL" \
        'BEGIN {printf "%.4f", s + w * r * i / 3600}')
    # Projection: what we have integrated plus one more worst-case billing
    # hour of everything currently provisioned.
    PROJECTED=$(awk -v s="$SPENT_USD" -v w="$TOTAL_WCU" -v r="$WCU_RATE" \
        'BEGIN {printf "%.4f", s + w * r}')
    if awk -v p="$PROJECTED" -v b="$BUDGET_USD" 'BEGIN {exit !(p > b)}'; then
        abort_run "spend projection \$$PROJECTED exceeds budget \$$BUDGET_USD (integrated \$$SPENT_USD, current ${TOTAL_WCU} WCU)"
    fi

    # -- 1. stall detection on expensive tables ------------------------------
    while IFS=$'\t' read -r table wcu; do
        [ -z "$table" ] && continue
        if [ "$wcu" -lt "$EXPENSIVE_WCU" ]; then continue; fi
        rate=$(consumed_rate "$table")
        low=$(awk -v r="${rate:-0}" -v w="$wcu" -v t="$STALL_THRESHOLD" \
            'BEGIN {print (r < w * t) ? 1 : 0}')
        if [ "$low" = "1" ]; then
            STALL_COUNT[$table]=$(( ${STALL_COUNT[$table]:-0} + 1 ))
            log "table $table: consumed ${rate:-0}/s < ${STALL_THRESHOLD} x ${wcu} (strike ${STALL_COUNT[$table]}/$STALL_MINUTES)"
            if [ "${STALL_COUNT[$table]}" -ge "$STALL_MINUTES" ]; then
                abort_run "expensive table $table stalled: consumption < ${STALL_THRESHOLD} x provisioned for ${STALL_MINUTES} consecutive checks"
            fi
        else
            STALL_COUNT[$table]=0
        fi
    done <<< "$TABLES"

    # -- 3./4. heartbeats: age and external cell deadline ---------------------
    HB=$(heartbeats)
    if [ -n "$RUNNING" ]; then
        now=$(date +%s)
        if [ -z "$HB" ]; then
            # No heartbeat at all: only tolerable during boot (build ~15 min).
            if [ $((now - WATCH_STARTED)) -gt "$BOOT_GRACE" ]; then
                HB_MISS_COUNT[__boot__]=$(( ${HB_MISS_COUNT[__boot__]:-0} + 1 ))
                log "no heartbeat after boot grace (strike ${HB_MISS_COUNT[__boot__]}/3)"
                [ "${HB_MISS_COUNT[__boot__]}" -ge 3 ] \
                    && abort_run "instances running but no heartbeat ever appeared (boot wedged?)"
            fi
        else
            while IFS=$'\t' read -r name age cell cell_started phase; do
                [ -z "$name" ] && continue
                if [ "$age" -gt "$HEARTBEAT_MAX_AGE" ]; then
                    HB_MISS_COUNT[$name]=$(( ${HB_MISS_COUNT[$name]:-0} + 1 ))
                    log "heartbeat $name stale (${age}s old, strike ${HB_MISS_COUNT[$name]}/3)"
                    [ "${HB_MISS_COUNT[$name]}" -ge 3 ] \
                        && abort_run "heartbeat $name stale for 3 consecutive checks (runner dead or wedged)"
                else
                    HB_MISS_COUNT[$name]=0
                fi
                # External per-cell deadline (only meaningful mid-cell).
                if [ "$cell" != "-" ] && [ "${cell_started:-0}" -gt 0 ] \
                    && [ -n "${CELL_BUDGET[$cell]:-}" ]; then
                    elapsed=$((now - cell_started))
                    limit=$(( ${CELL_BUDGET[$cell]} * 2 + 300 ))
                    if [ "$elapsed" -gt "$limit" ]; then
                        abort_run "cell $cell on $name exceeded its external deadline (${elapsed}s > ${limit}s; phase $phase)"
                    fi
                fi
            done <<< "$HB"
        fi
    fi

    # -- completion ----------------------------------------------------------
    # Only after we have seen the run alive at least once: a watchdog started
    # moments before the fleet appears must not mistake "not yet" for "done".
    NOT_DONE=$(run_instances "pending,running,stopping,stopped")
    if [ "$SAW_ACTIVITY" = "1" ] && [ -z "$TABLES" ] && [ -z "$NOT_DONE" ]; then
        log "run finished: no tables, no live instances (integrated spend ~\$$SPENT_USD)"
        exit 0
    fi

    log "tick $TICK: tables=$(printf '%s' "$TABLES" | grep -c . || true) wcu=$TOTAL_WCU running=$(printf '%s' "$RUNNING" | grep -c . || true) spent=~\$$SPENT_USD"

    if [ "$MAX_TICKS" -gt 0 ] && [ "$TICK" -ge "$MAX_TICKS" ]; then
        log "max ticks reached; exiting (no abort condition met)"
        exit 0
    fi
    sleep "$INTERVAL"
done
