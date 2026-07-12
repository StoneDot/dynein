#!/usr/bin/env bash
# Self-tests for watchdog.sh using the mock AWS CLI (no real AWS access).
# Verifies every abort branch, the abort ordering (money before compute)
# and the clean-completion path. Run:  scripts/bench/tests/watchdog_test.sh

set -uo pipefail

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_DIR="$(dirname "$TESTS_DIR")"
WATCHDOG="$BENCH_DIR/watchdog.sh"
export WATCHDOG_AWS="$TESTS_DIR/mock_aws.sh"
export WATCHDOG_SALVAGE_WAIT=0  # don't wall-clock the suite on mocked SSM waits

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

# ============ scenario 6: expired credentials != completed run ===============
# An expired SSO token makes every AWS call fail with empty output. The
# watchdog must treat "cannot see" as an alarm state — never as "no tables,
# no instances, therefore finished" (which would retire the guardian while
# resources may still be billing), and never as grounds for blind abort.
fresh_scenario
printf 'dynein-bench-testrun-task-w39000-large-1\t39000\n' > "$MOCK_DIR/tables.tsv"
printf 'i-authdead1\n' > "$MOCK_DIR/instances.txt"
echo "540" > "$MOCK_DIR/cw_rate"
mkdir -p "$MOCK_DIR/heartbeat"
python3 -c 'import json,time; print(json.dumps({"ts": int(time.time()), "cell": "task-w39000-large", "cell_started": int(time.time())-60, "phase": "import"}))' \
    > "$MOCK_DIR/heartbeat/m9g.xlarge-s0.json"
echo "3" > "$MOCK_DIR/fail_auth_after"  # healthy first, then the token dies
run_watchdog --budget-usd 1000 --max-ticks 8; rc=$?
check "auth-fail: does NOT report the run finished" \
    bash -c '! grep -q "run finished" "$MOCK_DIR/watchdog.log"'
check "auth-fail: logs AUTH FAILURE loudly" \
    grep -q "AUTH FAILURE" "$MOCK_DIR/watchdog.log"
check "auth-fail: takes no blind abort actions" \
    bash -c '! grep -qE "delete-table|stop-instances" "$MOCK_DIR/calls.log"'
rm -rf "$MOCK_DIR"

# ===== scenario 7: F1 zero-consumption stall on a CHEAP table aborts =========
# Before F1 the stall detector skipped every table below EXPENSIVE_WCU, so a
# wedged w10 cell (all of Stage 1) idled undetected. Zero consumption is
# unambiguous at any WCU. progress advances here so ONLY F1 can abort.
fresh_scenario
printf 'dynein-bench-testrun-pool1-w10-mixed-1\t10\n' > "$MOCK_DIR/tables.tsv"
printf 'i-cheap0001\n' > "$MOCK_DIR/instances.txt"
echo "None" > "$MOCK_DIR/cw_rate"   # zero consumption on a 10-WCU table
echo 1 > "$MOCK_DIR/progress_advancing"
mkdir -p "$MOCK_DIR/heartbeat"
python3 -c 'import json,time; print(json.dumps({"ts": int(time.time()), "cell": "pool1-w10-mixed", "cell_started": int(time.time())-60, "phase": "import", "progress": 500}))' \
    > "$MOCK_DIR/heartbeat/m9g.xlarge-s0.json"
run_watchdog --budget-usd 1000 --stall-minutes 3 --max-ticks 10; rc=$?
check "f1-cheap-stall: exits with abort code 3" [ "$rc" = "3" ]
check "f1-cheap-stall: reason mentions the stalled table" \
    grep -q "stalled" "$MOCK_DIR/watchdog.log"
check "f1-cheap-stall: cheap table was deleted" \
    grep -q "dynamodb delete-table" "$MOCK_DIR/calls.log"
rm -rf "$MOCK_DIR"

# ===== scenario 8: F2 frozen progress during import aborts ====================
# A fresh heartbeat proves liveness, not progress. resolved_items frozen
# across the strike window while phase=import is a wedge the heartbeat-age
# check cannot see. Healthy WCU (540/min = 9/s) rules out F1 — only F2 fires.
fresh_scenario
printf 'dynein-bench-testrun-task-w10-mixed-1\t10\n' > "$MOCK_DIR/tables.tsv"
printf 'i-frozen001\n' > "$MOCK_DIR/instances.txt"
echo "540" > "$MOCK_DIR/cw_rate"
mkdir -p "$MOCK_DIR/heartbeat"
python3 -c 'import json,time; print(json.dumps({"ts": int(time.time()), "cell": "task-w10-mixed", "cell_started": int(time.time())-60, "phase": "import", "progress": 4200}))' \
    > "$MOCK_DIR/heartbeat/m9g.xlarge-s0.json"
run_watchdog --budget-usd 1000 --progress-frozen-strikes 3 --max-ticks 10; rc=$?
check "f2-frozen-import: exits with abort code 3" [ "$rc" = "3" ]
check "f2-frozen-import: reason mentions frozen progress" \
    grep -q "progress frozen" "$MOCK_DIR/watchdog.log"
check "f2-frozen-import: table deleted" \
    grep -q "dynamodb delete-table" "$MOCK_DIR/calls.log"
rm -rf "$MOCK_DIR"

# ===== scenario 9: F2 does NOT fire outside import (phase-aware) =============
# During create-table retries / pregen / upload the counter is legitimately
# frozen. A frozen counter must strike ONLY while phase=import.
fresh_scenario
printf 'dynein-bench-testrun-task-w10-mixed-1\t10\n' > "$MOCK_DIR/tables.tsv"
printf 'i-pregen001\n' > "$MOCK_DIR/instances.txt"
echo "540" > "$MOCK_DIR/cw_rate"
mkdir -p "$MOCK_DIR/heartbeat"
python3 -c 'import json,time; print(json.dumps({"ts": int(time.time()), "cell": "task-w10-mixed", "cell_started": int(time.time())-60, "phase": "create-table", "progress": 4200}))' \
    > "$MOCK_DIR/heartbeat/m9g.xlarge-s0.json"
run_watchdog --budget-usd 1000 --progress-frozen-strikes 3 --max-ticks 6; rc=$?
check "f2-phase-gate: does NOT abort when phase != import" [ "$rc" = "0" ]
check "f2-phase-gate: never deleted a table" \
    bash -c '! grep -q "dynamodb delete-table" "$MOCK_DIR/calls.log"'
rm -rf "$MOCK_DIR"

# ===== scenario 10: F2 tolerates advancing progress (liveness) ==============
# A genuinely progressing import must never strike, however long it runs.
fresh_scenario
printf 'dynein-bench-testrun-task-w10-mixed-1\t10\n' > "$MOCK_DIR/tables.tsv"
printf 'i-alive0001\n' > "$MOCK_DIR/instances.txt"
echo "540" > "$MOCK_DIR/cw_rate"
echo 1 > "$MOCK_DIR/progress_advancing"   # mock bumps progress every read
mkdir -p "$MOCK_DIR/heartbeat"
python3 -c 'import json,time; print(json.dumps({"ts": int(time.time()), "cell": "task-w10-mixed", "cell_started": int(time.time())-60, "phase": "import", "progress": 0}))' \
    > "$MOCK_DIR/heartbeat/m9g.xlarge-s0.json"
run_watchdog --budget-usd 1000 --progress-frozen-strikes 3 --max-ticks 8; rc=$?
check "f2-advancing: does NOT abort while progress climbs" [ "$rc" = "0" ]
rm -rf "$MOCK_DIR"

echo
if [ "$FAILURES" = "0" ]; then
    echo "WATCHDOG SELF-TEST PASSED"
else
    echo "WATCHDOG SELF-TEST FAILED ($FAILURES failure(s))"
    exit 1
fi
