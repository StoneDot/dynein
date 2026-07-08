#!/usr/bin/env bash
# Mock `aws` CLI for watchdog self-tests (set WATCHDOG_AWS to this script).
#
# Scenario state lives in $MOCK_DIR:
#   tables.tsv            "name<TAB>wcu" per line (mutated by delete-table)
#   instances.txt         instance ids currently running
#   instances_stopped.txt instance ids stopped (stop-instances moves them here)
#   cw_rate               consumed WCU **per minute** (Sum) returned for any table
#   heartbeat/<name>      heartbeat JSON bodies
#   config.json           run config (served for s3 cp .../config.json)
#   empty_after           optional: tick count after which tables/instances
#                         read as empty (drives the completion path);
#                         "ticks" are counted per list-tables call
#   calls.log             every invocation, appended (assertions read this)

set -uo pipefail
: "${MOCK_DIR:?set MOCK_DIR}"
echo "$*" >> "$MOCK_DIR/calls.log"

args=("$@")

# fail_auth_after: once the tick counter (advanced by list-tables) reaches
# this value, EVERY call fails like an expired SSO token — the failure mode
# where a watchdog must not mistake "cannot see" for "nothing exists".
auth_dead() {
    local limit tick
    limit=$(cat "$MOCK_DIR/fail_auth_after" 2>/dev/null) || return 1
    tick=$(cat "$MOCK_DIR/.tick" 2>/dev/null || echo 0)
    [ "$tick" -ge "$limit" ]
}
if auth_dead; then
    echo "An error occurred (ExpiredTokenException): The security token included in the request is expired" >&2
    exit 255
fi

has() { local w; for w in "${args[@]}"; do [ "$w" = "$1" ] && return 0; done; return 1; }
arg_after() {  # value following the given flag
    local i
    for ((i = 0; i < ${#args[@]}; i++)); do
        if [ "${args[$i]}" = "$1" ]; then echo "${args[$((i + 1))]}"; return 0; fi
    done
    return 1
}

past_empty() {
    local limit tick
    limit=$(cat "$MOCK_DIR/empty_after" 2>/dev/null) || return 1
    tick=$(cat "$MOCK_DIR/.tick" 2>/dev/null || echo 0)
    [ "$tick" -ge "$limit" ]
}

case "$1 $2" in
"dynamodb list-tables")
    tick=$(($(cat "$MOCK_DIR/.tick" 2>/dev/null || echo 0) + 1))
    echo "$tick" > "$MOCK_DIR/.tick"
    past_empty && exit 0
    cut -f1 "$MOCK_DIR/tables.tsv" 2>/dev/null
    ;;
"dynamodb describe-table")
    t=$(arg_after --table-name)
    awk -F'\t' -v t="$t" '$1 == t {print $2}' "$MOCK_DIR/tables.tsv" 2>/dev/null
    ;;
"dynamodb delete-table")
    t=$(arg_after --table-name)
    grep -v "^$t	" "$MOCK_DIR/tables.tsv" > "$MOCK_DIR/tables.tsv.new" 2>/dev/null || true
    mv "$MOCK_DIR/tables.tsv.new" "$MOCK_DIR/tables.tsv"
    ;;
"ec2 describe-instances")
    past_empty && exit 0
    # State filter is the second --filters argument: Name=instance-state-name,Values=...
    states=""
    for a in "${args[@]}"; do
        case "$a" in Name=instance-state-name,Values=*) states="${a#*Values=}" ;; esac
    done
    case ",$states," in
    *,running,*|*,pending,*)
        cat "$MOCK_DIR/instances.txt" 2>/dev/null ;;
    esac
    case ",$states," in
    *,stopped,*|*,stopping,*)
        cat "$MOCK_DIR/instances_stopped.txt" 2>/dev/null ;;
    esac
    ;;
"ec2 stop-instances")
    cat "$MOCK_DIR/instances.txt" >> "$MOCK_DIR/instances_stopped.txt" 2>/dev/null || true
    : > "$MOCK_DIR/instances.txt"
    ;;
"cloudwatch get-metric-statistics")
    cat "$MOCK_DIR/cw_rate" 2>/dev/null || echo "None"
    ;;
"s3 ls")
    # Heartbeat listing: emulate `aws s3 ls` output (last field = key name).
    if [ -d "$MOCK_DIR/heartbeat" ]; then
        for f in "$MOCK_DIR/heartbeat"/*; do
            [ -e "$f" ] || continue
            echo "2026-01-01 00:00:00        123 $(basename "$f")"
        done
    fi
    ;;
"s3 cp")
    src="$3" dst="$4"
    case "$src" in
    *"/heartbeat/"*)
        body=$(cat "$MOCK_DIR/heartbeat/$(basename "$src")" 2>/dev/null)
        # progress_advancing: bump the heartbeat's `progress` field on every
        # read so the runner looks like it is genuinely making progress (F2
        # liveness test). Without the flag the body is served verbatim.
        if [ -f "$MOCK_DIR/progress_advancing" ] && [ -n "$body" ]; then
            n=$(( $(cat "$MOCK_DIR/.progress" 2>/dev/null || echo 0) + 100 ))
            echo "$n" > "$MOCK_DIR/.progress"
            body=$(python3 -c 'import json,sys; b=json.loads(sys.stdin.read()); b["progress"]=int(sys.argv[1]); print(json.dumps(b))' "$n" <<<"$body")
        fi
        printf '%s\n' "$body" ;;
    *"config.json")
        cp "$MOCK_DIR/config.json" "$dst" 2>/dev/null || exit 1 ;;
    *)
        : ;;  # uploads (abort marker, salvage): log only
    esac
    ;;
"ssm send-command")
    : ;;
"sts get-caller-identity")
    : ;;  # auth probe: success unless auth_dead already failed above
*)
    echo "mock_aws: unhandled: $*" >&2
    exit 1
    ;;
esac
