#!/usr/bin/env bash
# Canary verification + PASS marker (postmortem G2).
#
# Verifies that a `launch.sh --canary --execute` run completed end-to-end on
# the real platform, then writes s3://<bucket>/canary/<commit>/PASS.
# launch.sh --execute refuses to launch a full matrix without that marker
# for the same commit, so this script is the only way to open the gate.
#
# Pass criteria (postmortem §3 G2):
#   - config.json says the run was a canary
#   - every cell x rep has result.json with exit_code 0 within its budget
#   - stats.jsonl exists per rep (instrumentation path worked)
#   - max RSS stays flat (< --max-rss-mb) — proves streaming reads on the
#     real platform, the failure mode that killed run 090552
#   - shard done marker exists (runner reached its normal end)
#   - no dynein-bench-<run-id>-* tables remain (cleanup worked)
#   - all fleet instances are terminated (self-termination worked)
#
# Usage:
#   canary_verify.sh --run-id ID --bucket B --region R \
#       [--instance-type m9g.xlarge] [--max-rss-mb 1024]

set -uo pipefail

RUN_ID="" BUCKET="" REGION="" INSTANCE_TYPE="m9g.xlarge" MAX_RSS_MB=1024
while [ $# -gt 0 ]; do
    case "$1" in
        --run-id) RUN_ID="$2"; shift ;;
        --bucket) BUCKET="$2"; shift ;;
        --region) REGION="$2"; shift ;;
        --instance-type) INSTANCE_TYPE="$2"; shift ;;
        --max-rss-mb) MAX_RSS_MB="$2"; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done
: "${RUN_ID:?--run-id required}" "${BUCKET:?--bucket required}" "${REGION:?--region required}"

FAIL=0
pass() { printf 'PASS  %s\n' "$*"; }
fail() { printf 'FAIL  %s\n' "$*"; FAIL=1; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# --- config -------------------------------------------------------------------
if ! aws s3 cp "s3://$BUCKET/runs/$RUN_ID/config.json" "$TMP/config.json" >/dev/null 2>&1; then
    fail "config.json not found for run $RUN_ID"
    echo "CANARY VERIFY FAILED"; exit 1
fi
COMMIT_SHA=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["commit_sha"])' "$TMP/config.json")
IS_CANARY=$(python3 -c 'import json,sys; print(1 if json.load(open(sys.argv[1])).get("canary") else 0)' "$TMP/config.json")
[ "$IS_CANARY" = "1" ] && pass "run $RUN_ID is a canary run (commit $COMMIT_SHA)" \
    || fail "run $RUN_ID is NOT a canary run — the marker must come from a canary"

# --- per-rep results ----------------------------------------------------------
S3_RES="s3://$BUCKET/runs/$RUN_ID/results/$INSTANCE_TYPE"
while IFS=$'\t' read -r cell_id reps budget; do
    for rep in $(seq 1 "$reps"); do
        prefix="$S3_RES/$cell_id/rep$rep"
        if ! aws s3 cp "$prefix/result.json" "$TMP/result.json" >/dev/null 2>&1; then
            fail "$cell_id rep$rep: result.json missing"
            continue
        fi
        verdict=$(python3 - "$TMP/result.json" "$budget" "$MAX_RSS_MB" <<'PYEOF'
import json, sys
r = json.load(open(sys.argv[1]))
budget, max_rss_mb = int(sys.argv[2]), int(sys.argv[3])
problems = []
if r.get("exit_code") != 0:
    problems.append(f"exit_code={r.get('exit_code')}")
if r.get("wall_seconds", 10**9) > budget * 2:
    problems.append(f"wall={r.get('wall_seconds')}s > 2x budget {budget}s")
rss_kb = r.get("max_rss_kb")
if rss_kb is None:
    problems.append("max_rss_kb missing")
elif rss_kb > max_rss_mb * 1024:
    problems.append(f"max_rss={rss_kb//1024}MB > {max_rss_mb}MB (streaming regression?)")
print("; ".join(problems) if problems else "OK "
      f"(exit 0, wall {r.get('wall_seconds')}s, rss {int(rss_kb or 0)//1024}MB)")
PYEOF
        )
        case "$verdict" in
            OK*) pass "$cell_id rep$rep: $verdict" ;;
            *)   fail "$cell_id rep$rep: $verdict" ;;
        esac
        aws s3 ls "$prefix/stats.jsonl" >/dev/null 2>&1 \
            && pass "$cell_id rep$rep: stats.jsonl present" \
            || fail "$cell_id rep$rep: stats.jsonl missing"
    done
done < <(python3 - "$TMP/config.json" <<'PYEOF'
import json, sys
cfg = json.load(open(sys.argv[1]))
for c in cfg["cells"]:
    print(c["cell_id"] + "\t" + str(c["reps"]) + "\t" + str(c.get("budget_secs", 600)))
PYEOF
)

aws s3 ls "$S3_RES/shard-0.done" >/dev/null 2>&1 \
    && pass "shard-0.done marker exists (runner reached normal end)" \
    || fail "shard-0.done marker missing"

# --- cleanup checks -----------------------------------------------------------
leftover=$(aws dynamodb list-tables --region "$REGION" \
    --output text --query 'TableNames[]' 2>/dev/null | tr '\t' '\n' \
    | grep -c "^dynein-bench-${RUN_ID}-" || true)
[ "${leftover:-0}" = "0" ] && pass "no leftover dynein-bench-$RUN_ID-* tables" \
    || fail "$leftover leftover table(s) of run $RUN_ID still exist"

not_terminated=$(aws ec2 describe-instances --region "$REGION" \
    --filters "Name=tag:dynein-bench,Values=$RUN_ID" \
    --query 'Reservations[].Instances[?State.Name!=`terminated` && State.Name!=`shutting-down`].[InstanceId]' \
    --output text 2>/dev/null | grep -c . || true)
[ "${not_terminated:-0}" = "0" ] && pass "all canary instances terminated" \
    || fail "$not_terminated canary instance(s) still running/stopped"

# --- marker -------------------------------------------------------------------
echo
if [ "$FAIL" != "0" ]; then
    echo "CANARY VERIFY FAILED — no PASS marker written"
    exit 1
fi
python3 - "$RUN_ID" "$COMMIT_SHA" "$INSTANCE_TYPE" > "$TMP/PASS" <<'PYEOF'
import json, sys, time
print(json.dumps({
    "run_id": sys.argv[1],
    "commit_sha": sys.argv[2],
    "instance_type": sys.argv[3],
    "verified_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
}, indent=2))
PYEOF
aws s3 cp "$TMP/PASS" "s3://$BUCKET/canary/$COMMIT_SHA/PASS" >/dev/null
echo "CANARY VERIFY PASSED — marker written: s3://$BUCKET/canary/$COMMIT_SHA/PASS"
