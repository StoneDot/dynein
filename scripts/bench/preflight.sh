#!/usr/bin/env bash
# Mechanical preflight gate for a bench fleet launch (postmortem G1).
# Every check here exists because its absence burned us on 2026-07-05
# (docs/design/benchmark-run-postmortem.md). Must exit 0 before
# launch.sh --execute is allowed.
#
# Usage:
#   preflight.sh --config <config.json> --region <region> \
#       --subnet-id <subnet> --security-group <sg> \
#       --instance-types "m9g.xlarge:arm64 ..." \
#       [--instance-ram-gb 16] [--root-volume-gb 100] \
#       [--iam-profile dynein-bench-instance] [--budget-usd 100]

set -uo pipefail

CONFIG="" REGION="" SUBNET="" SG="" TYPES=""
RAM_GB=16 ROOT_GB=100 IAM_PROFILE="dynein-bench-instance" BUDGET_USD=100
while [ $# -gt 0 ]; do
    case "$1" in
        --config) CONFIG="$2"; shift ;;
        --region) REGION="$2"; shift ;;
        --subnet-id) SUBNET="$2"; shift ;;
        --security-group) SG="$2"; shift ;;
        --instance-types) TYPES="$2"; shift ;;
        --instance-ram-gb) RAM_GB="$2"; shift ;;
        --root-volume-gb) ROOT_GB="$2"; shift ;;
        --iam-profile) IAM_PROFILE="$2"; shift ;;
        --budget-usd) BUDGET_USD="$2"; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done
: "${CONFIG:?--config required}" "${REGION:?--region required}"
: "${SUBNET:?--subnet-id required}" "${SG:?--security-group required}"
: "${TYPES:?--instance-types required}"

FAIL=0
pass() { printf 'PASS  %s\n' "$*"; }
fail() { printf 'FAIL  %s\n' "$*"; FAIL=1; }

# --- platform checks ----------------------------------------------------------
AZ=$(aws ec2 describe-subnets --region "$REGION" --subnet-ids "$SUBNET" \
    --query 'Subnets[0].AvailabilityZone' --output text 2>/dev/null)
if [ -z "$AZ" ] || [ "$AZ" = "None" ]; then
    fail "subnet $SUBNET not found in $REGION"
else
    pass "subnet $SUBNET exists (AZ $AZ)"
    VPC=$(aws ec2 describe-subnets --region "$REGION" --subnet-ids "$SUBNET" \
        --query 'Subnets[0].VpcId' --output text)
    # Internet egress: the subnet's route table (or the VPC main one) must
    # default-route to an internet gateway.
    IGW_ROUTE=$(aws ec2 describe-route-tables --region "$REGION" \
        --filters "Name=vpc-id,Values=$VPC" \
        --query 'RouteTables[].Routes[?DestinationCidrBlock==`0.0.0.0/0`].GatewayId' \
        --output text | grep -c '^igw-' || true)
    [ "$IGW_ROUTE" -ge 1 ] && pass "default route to an internet gateway exists" \
        || fail "no 0.0.0.0/0 -> igw route in VPC $VPC (fleet needs egress)"
fi

for pair in $TYPES; do
    itype=${pair%%:*}
    offered=$(aws ec2 describe-instance-type-offerings --region "$REGION" \
        --location-type availability-zone \
        --filters "Name=instance-type,Values=$itype" "Name=location,Values=$AZ" \
        --query 'length(InstanceTypeOfferings)' --output text 2>/dev/null)
    [ "$offered" = "1" ] && pass "$itype offered in $AZ" \
        || fail "$itype NOT offered in $AZ"
done

aws ec2 describe-security-groups --region "$REGION" --group-ids "$SG" >/dev/null 2>&1 \
    && pass "security group $SG exists" || fail "security group $SG not found"

aws iam get-instance-profile --instance-profile-name "$IAM_PROFILE" >/dev/null 2>&1 \
    && pass "instance profile $IAM_PROFILE exists" \
    || fail "instance profile $IAM_PROFILE not found"
aws iam list-attached-role-policies --role-name "$IAM_PROFILE" \
    --query 'AttachedPolicies[].PolicyArn' --output text 2>/dev/null \
    | grep -q AmazonSSMManagedInstanceCore \
    && pass "SSM Session Manager policy attached (rescue path)" \
    || fail "AmazonSSMManagedInstanceCore not attached to $IAM_PROFILE"

# --- config-derived capacity arithmetic (postmortem G0) ------------------------
python3 - "$CONFIG" "$RAM_GB" "$ROOT_GB" "$BUDGET_USD" <<'PYEOF'
import json, sys
cfg = json.load(open(sys.argv[1]))
ram_gb, root_gb, budget = float(sys.argv[2]), float(sys.argv[3]), float(sys.argv[4])
fail = 0
def check(ok, msg):
    global fail
    print(("PASS  " if ok else "FAIL  ") + msg)
    if not ok: fail = 1

AVG_BYTES = {"uniform-small": 110, "mixed": 2099, "uniform-large": 35000}
# Streaming reads + admission control (import-throttling.md §4.11) bound the
# importer's resident set by the admission cap, not the input size: at most
# ADMISSION_ITEM_CAP items live in the pipeline at once. The old whole-file
# rule (input x 2.5 < RAM, which OOM-killed run 090552) is replaced by the
# cap-derived bound with a 3x safety factor; the canary independently
# verifies flat RSS on the real platform (canary_verify.sh --max-rss-mb).
ADMISSION_ITEM_CAP = 10_000  # keep in sync with src/transfer.rs
MEM_OVERHEAD = 1e9           # runtime + buffers, generous

inputs = {}
max_wcu_by_shard = {}
max_item_bytes = 0
for c in cfg["cells"]:
    size = c["items"] * AVG_BYTES[c["mix"]]
    inputs[(c["mix"], c["items"], c.get("seed"))] = size
    max_item_bytes = max(max_item_bytes, AVG_BYTES[c["mix"]])
    key = c.get("shard", 0)
    max_wcu_by_shard[key] = max(max_wcu_by_shard.get(key, 0), c["wcu"])

resident = ADMISSION_ITEM_CAP * max_item_bytes * 3 + MEM_OVERHEAD
check(resident < ram_gb * 1e9,
      f"streaming resident bound {resident/1e9:.1f}GB "
      f"({ADMISSION_ITEM_CAP} items x {max_item_bytes}B x3 + overhead) fits {ram_gb:.0f}GB RAM")

total_inputs = sum(inputs.values())
check(total_inputs + 10e9 < root_gb * 1e9,
      f"sum of inputs {total_inputs/1e9:.1f}GB + 10GB build/artifacts fits root {root_gb:.0f}GB")

# One expensive table per shard at a time (serial reuse); assume the worst
# case that every shard's max table coexists.
concurrent_wcu = sum(max_wcu_by_shard.values())
check(concurrent_wcu <= 78000,
      f"worst-case concurrent provisioned WCU {concurrent_wcu} <= 78000 "
      f"(80k account quota minus residual headroom)")

# Cost under both billing interpretations (USD, us-west-2 rate).
RATE = 0.00065
prorated = 0.0
hour_rounded = 0.0
for c in cfg["cells"]:
    secs = (c.get("budget_secs", 600) + 120) * c["reps"]
    prorated += c["wcu"] * RATE * secs / 3600
    hour_rounded += c["wcu"] * RATE * max(1.0, secs / 3600 + 1)
print(f"INFO  cost estimate: prorated ~${prorated:.0f} / hour-rounded worst ~${hour_rounded:.0f} "
      f"(budget ${budget:.0f})")
check(hour_rounded <= budget,
      f"worst-case cost ${hour_rounded:.0f} within budget ${budget:.0f}")
sys.exit(fail)
PYEOF
[ $? -ne 0 ] && FAIL=1

# --- rendered artifacts -------------------------------------------------------
OUT_DIR=$(dirname "$CONFIG")
if ls "$OUT_DIR"/user-data-*.sh >/dev/null 2>&1; then
    if grep -l '{{' "$OUT_DIR"/user-data-*.sh >/dev/null 2>&1; then
        fail "unsubstituted {{placeholders}} in rendered user-data"
    else
        pass "rendered user-data has no unsubstituted placeholders"
    fi
    for f in "$OUT_DIR"/user-data-*.sh; do
        bash -n "$f" || fail "bash -n failed: $f"
    done
    pass "bash -n on rendered user-data"
else
    fail "no rendered user-data found next to $CONFIG (run launch.sh --dry-run first)"
fi
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
for f in "$SCRIPT_DIR"/run_cells.sh "$SCRIPT_DIR"/launch.sh \
         "$SCRIPT_DIR"/watchdog.sh "$SCRIPT_DIR"/canary_verify.sh \
         "$SCRIPT_DIR"/instance_cleanup.sh; do
    bash -n "$f" && pass "bash -n $f" || fail "bash -n failed: $f"
done

# The watchdog is part of the run's safety case: its abort logic must pass
# its mock-driven self-test before any launch relies on it.
if "$SCRIPT_DIR"/tests/watchdog_test.sh >/dev/null 2>&1; then
    pass "watchdog self-test (mocked abort/budget/deadline paths)"
else
    fail "watchdog self-test failed — run scripts/bench/tests/watchdog_test.sh"
fi

echo
if [ "$FAIL" = "0" ]; then
    echo "PREFLIGHT PASSED"
else
    echo "PREFLIGHT FAILED — do not launch"
fi
exit $FAIL
