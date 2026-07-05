#!/usr/bin/env bash
# Self-tests for watchdog.sh using the mock AWS CLI (no real AWS access).
# Verifies every abort branch, the abort ordering (money before compute)
# and the clean-completion path. Run:  scripts/bench/tests/watchdog_test.sh

set -uo pipefail

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_DIR="$(dirname "$TESTS_DIR")"
WATCHDOG="$BENCH_DIR/watchdog.sh"
export WATCHDOG_AWS="$TESTS_DIR/mock_aws.sh"

FAILURES=0
check() {  # check <description> <condition...>
    local desc="$1"; shift
    if "$@"; then
        printf 'ok    %s\n' "$desc"
    else
        printf 'FAIL  %s\n' "$desc"
        FAILURES=$((FAILURES + 1))
    fi
}

fresh_scenario() {
    MOCK_DIR=$(mktemp -d /tmp/dynein-watchdog-test.XXXXXX)
    export MOCK_DIR
    : > "$MOCK_DIR/calls.log"
    : > "$MOCK_DIR/instances_stopped.txt"
}

run_watchdog() {  # extra args appended; interval 0 so ticks are instant
    "$WATCHDOG" --run-id testrun --bucket test-bucket --region us-west-2 \
        --interval 0 "$@" > "$MOCK_DIR/watchdog.log" 2>&1
}

# ============ scenario 1: stalled expensive table triggers abort =============
fresh_scenario
printf 'dynein-bench-testrun-shared-w39000\t39000\n' > "$MOCK_DIR/tables.tsv"
printf 'i-stall0001\n' > "$MOCK_DIR/instances.txt"
echo "None" > "$MOCK_DIR/cw_rate"   # zero consumption
mkdir -p "$MOCK_DIR/heartbeat"
python3 -c 'import json,time; print(json.dumps({"ts": int(time.time()), "cell": "-", "cell_started": 0, "phase": "import"}))' \
    > "$MOCK_DIR/heartbeat/m9g.xlarge-s0.json"

run_watchdog --budget-usd 1000 --stall-minutes 3 --max-ticks 10; rc=$?
check "stall: exits with abort code 3" [ "$rc" = "3" ]
check "stall: reason mentions the stalled table" \
    grep -q "stalled" "$MOCK_DIR/watchdog.log"
check "stall: table deleted" \
    grep -q "dynamodb delete-table" "$MOCK_DIR/calls.log"
check "stall: instances stopped, not terminated" \
    grep -q "ec2 stop-instances" "$MOCK_DIR/calls.log"
check "stall: no terminate-instances ever issued" \
    bash -c '! grep -q terminate-instances "$MOCK_DIR/calls.log"'
# Abort ordering: money (delete-table) must precede compute (stop-instances).
del_line=$(grep -n "dynamodb delete-table" "$MOCK_DIR/calls.log" | head -1 | cut -d: -f1)
stop_line=$(grep -n "ec2 stop-instances" "$MOCK_DIR/calls.log" | head -1 | cut -d: -f1)
check "stall: tables deleted BEFORE instances stopped" \
    [ "${del_line:-9999}" -lt "${stop_line:-0}" ]
check "stall: SSM salvage attempted before stopping" \
    grep -q "ssm send-command" "$MOCK_DIR/calls.log"
check "stall: abort marker uploaded" \
    grep -q "watchdog-abort.json" "$MOCK_DIR/calls.log"
rm -rf "$MOCK_DIR"

# ============ scenario 2: spend projection exceeds the budget ================
fresh_scenario
printf 'dynein-bench-testrun-shared-w39000\t39000\n' > "$MOCK_DIR/tables.tsv"
printf 'i-budget001\n' > "$MOCK_DIR/instances.txt"
echo "120000" > "$MOCK_DIR/cw_rate"  # healthy consumption — not a stall
# 39000 WCU x $0.00065 ~= $25/h projection; budget $5 must abort on tick 1.
run_watchdog --budget-usd 5 --max-ticks 10; rc=$?
check "budget: exits with abort code 3" [ "$rc" = "3" ]
check "budget: reason mentions the spend projection" \
    grep -q "spend projection" "$MOCK_DIR/watchdog.log"
check "budget: table deleted" \
    grep -q "dynamodb delete-table" "$MOCK_DIR/calls.log"
rm -rf "$MOCK_DIR"

# ============ scenario 3: healthy run to clean completion ====================
fresh_scenario
printf 'dynein-bench-testrun-pool16-w10-small-1\t10\n' > "$MOCK_DIR/tables.tsv"
printf 'i-healthy01\n' > "$MOCK_DIR/instances.txt"
echo "540" > "$MOCK_DIR/cw_rate"    # 9 WCU/s on a 10-WCU table
mkdir -p "$MOCK_DIR/heartbeat"
python3 -c 'import json,time; print(json.dumps({"ts": int(time.time()), "cell": "pool16-w10-small", "cell_started": int(time.time())-60, "phase": "import"}))' \
    > "$MOCK_DIR/heartbeat/m9g.xlarge-s0.json"
cat > "$MOCK_DIR/config.json" <<'EOF'
{"cells": [{"cell_id": "pool16-w10-small", "reps": 1, "budget_secs": 700}]}
EOF
echo "3" > "$MOCK_DIR/empty_after"  # after 3 list-tables calls the run is over
run_watchdog --budget-usd 1000 --max-ticks 10; rc=$?
check "healthy: exits 0 on clean completion" [ "$rc" = "0" ]
check "healthy: reports the run finished" \
    grep -q "run finished" "$MOCK_DIR/watchdog.log"
check "healthy: never deleted a table" \
    bash -c '! grep -q "dynamodb delete-table" "$MOCK_DIR/calls.log"'
check "healthy: never stopped an instance" \
    bash -c '! grep -q "ec2 stop-instances" "$MOCK_DIR/calls.log"'
rm -rf "$MOCK_DIR"

# ============ scenario 4: external cell deadline =============================
fresh_scenario
printf 'dynein-bench-testrun-pool16-w10-small-1\t10\n' > "$MOCK_DIR/tables.tsv"
printf 'i-wedged001\n' > "$MOCK_DIR/instances.txt"
echo "540" > "$MOCK_DIR/cw_rate"
mkdir -p "$MOCK_DIR/heartbeat"
# Fresh heartbeat (runner loop alive) but the cell started 10000s ago and
# budget is 700s: the on-instance timeout should long have fired — external
# deadline treats the instance as wedged.
python3 -c 'import json,time; print(json.dumps({"ts": int(time.time()), "cell": "pool16-w10-small", "cell_started": int(time.time())-10000, "phase": "import"}))' \
    > "$MOCK_DIR/heartbeat/m9g.xlarge-s0.json"
cat > "$MOCK_DIR/config.json" <<'EOF'
{"cells": [{"cell_id": "pool16-w10-small", "reps": 1, "budget_secs": 700}]}
EOF
run_watchdog --budget-usd 1000 --max-ticks 10; rc=$?
check "deadline: exits with abort code 3" [ "$rc" = "3" ]
check "deadline: reason mentions the external deadline" \
    grep -q "external deadline" "$MOCK_DIR/watchdog.log"
rm -rf "$MOCK_DIR"

# ============ scenario 5: stale heartbeat while instances run ================
fresh_scenario
printf 'dynein-bench-testrun-pool16-w10-small-1\t10\n' > "$MOCK_DIR/tables.tsv"
printf 'i-silent001\n' > "$MOCK_DIR/instances.txt"
echo "540" > "$MOCK_DIR/cw_rate"
mkdir -p "$MOCK_DIR/heartbeat"
python3 -c 'import json,time; print(json.dumps({"ts": int(time.time())-4000, "cell": "-", "cell_started": 0, "phase": "pregen"}))' \
    > "$MOCK_DIR/heartbeat/m9g.xlarge-s0.json"
run_watchdog --budget-usd 1000 --max-ticks 10; rc=$?
check "stale-heartbeat: exits with abort code 3" [ "$rc" = "3" ]
check "stale-heartbeat: reason mentions the heartbeat" \
    grep -q "heartbeat" "$MOCK_DIR/watchdog.log"
rm -rf "$MOCK_DIR"

echo
if [ "$FAILURES" = "0" ]; then
    echo "WATCHDOG SELF-TEST PASSED"
else
    echo "WATCHDOG SELF-TEST FAILED ($FAILURES failure(s))"
    exit 1
fi
