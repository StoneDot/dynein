#!/usr/bin/env bash
# Developer-machine orchestrator (benchmark-plan.md §5.3).
#
# Generates a run-id, renders config.json from the editable cell matrix below,
# uploads it to S3, and launches the benchmark fleet (m9g.xlarge/arm64 +
# m8a.xlarge/x86_64, Amazon Linux 2023, self-terminating).
#
# SAFETY: this script defaults to --dry-run. Nothing touches AWS unless you
# pass --execute. In dry-run mode it prints every aws command it would run
# and writes config.json + rendered user-data files locally for inspection.
#
# Usage:
#   scripts/bench/launch.sh [--execute | --dry-run] \
#       --bucket dynein-bench-<account-id> \
#       [--run-id ID] [--region ap-northeast-1] [--commit SHA] \
#       [--instances-per-type N] [--iam-profile dynein-bench-instance] \
#       [--quota-wcu 40000] [--out-dir DIR]

set -euo pipefail

if [ "${BASH_SOURCE[0]}" != "$0" ]; then
    echo "launch.sh must be executed, not sourced" >&2
    return 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

DRY_RUN=1
RUN_ID=""
REGION="ap-northeast-1"
BUCKET="${BENCH_BUCKET:-}"
COMMIT_SHA=""
INSTANCES_PER_TYPE=2
IAM_PROFILE="${BENCH_IAM_PROFILE:-dynein-bench-instance}"
# 39k, not the 40k table cap: two instances can then hold one quota-scale
# table each (2 x 39k + the residual dynein-throttle-exp table fits in the
# 80k account-level provisioned-capacity quota; 2 x 40k + anything does not).
QUOTA_WCU=39000
OUT_DIR=""
# type:arch pairs (benchmark-plan.md §5.1 / Q6)
INSTANCE_TYPES="${BENCH_INSTANCE_TYPES:-m9g.xlarge:arm64 m8a.xlarge:x86_64}"
# Required in accounts without a default VPC (e.g. Control Tower): a public
# subnet with internet egress and an egress-only security group.
SUBNET_ID="${BENCH_SUBNET_ID:-}"
SECURITY_GROUP="${BENCH_SECURITY_GROUP:-}"

while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        --execute) DRY_RUN=0 ;;
        --run-id) RUN_ID="$2"; shift ;;
        --region) REGION="$2"; shift ;;
        --bucket) BUCKET="$2"; shift ;;
        --commit) COMMIT_SHA="$2"; shift ;;
        --instances-per-type) INSTANCES_PER_TYPE="$2"; shift ;;
        --iam-profile) IAM_PROFILE="$2"; shift ;;
        --subnet-id) SUBNET_ID="$2"; shift ;;
        --security-group) SECURITY_GROUP="$2"; shift ;;
        --quota-wcu) QUOTA_WCU="$2"; shift ;;
        --out-dir) OUT_DIR="$2"; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

if [ -z "$BUCKET" ]; then
    if [ "$DRY_RUN" = "1" ]; then
        BUCKET="dynein-bench-ACCOUNT_ID"  # placeholder, dry-run only
    else
        echo "--bucket (or BENCH_BUCKET) is required with --execute" >&2
        exit 2
    fi
fi
RUN_ID="${RUN_ID:-$(date -u +%Y%m%d-%H%M%S)}"
COMMIT_SHA="${COMMIT_SHA:-$(git -C "$SCRIPT_DIR/../.." rev-parse HEAD)}"
OUT_DIR="${OUT_DIR:-$SCRIPT_DIR/out/$RUN_ID}"
mkdir -p "$OUT_DIR"

run() {
    if [ "$DRY_RUN" = "1" ]; then
        echo "DRY-RUN> $*"
    else
        "$@"
    fi
}

# --- cell matrix (edit freely; Tier 1 by default, benchmark-plan.md §3) ------
# "quota" as WCU resolves to --quota-wcu. items is computed from WCU x 400 s
# of steady state divided by the mix's average WCU/item (1 / 2.9 / 35).
MATRIX_JSON="$OUT_DIR/matrix.json"
cat > "$MATRIX_JSON" <<EOF
{
  "executors": ["pool16", "pool1", "mpmc", "task"],
  "wcus": [10, "quota"],
  "mixes": ["uniform-small", "mixed", "uniform-large"],
  "reps": 3,
  "prod_rate": 0,
  "quota_wcu": $QUOTA_WCU,
  "quota_reps": 2,
  "quota_steady_secs": 300
}
EOF

# --- render config.json -------------------------------------------------------
CONFIG="$OUT_DIR/config.json"
python3 - "$MATRIX_JSON" "$RUN_ID" "$COMMIT_SHA" "$REGION" "$BUCKET" \
    "$INSTANCES_PER_TYPE" > "$CONFIG" <<'PYEOF'
import json, math, sys
matrix = json.load(open(sys.argv[1]))
run_id, commit_sha, region, bucket = sys.argv[2:6]
num_shards = int(sys.argv[6])
AVG_WCU = {"uniform-small": 1.0, "mixed": 2.9, "uniform-large": 35.0}
MIX_SHORT = {"uniform-small": "small", "mixed": "mixed", "uniform-large": "large"}
cells = []
for executor in matrix["executors"]:
    for wcu in matrix["wcus"]:
        wcu_label = "quota" if wcu == "quota" else f"w{wcu}"
        wcu_val = matrix["quota_wcu"] if wcu == "quota" else wcu
        for mix in matrix["mixes"]:
            avg = AVG_WCU[mix]
            # Quota-scale cells dominate the DynamoDB cost (~$0.5/min of
            # table lifetime), so they run the plan's minimum steady state
            # (300s) and fewer reps; the cheap low-rate cells keep the full
            # settings.
            steady_secs = matrix.get("quota_steady_secs", 400) if wcu == "quota" else 400
            reps = matrix.get("quota_reps", matrix["reps"]) if wcu == "quota" else matrix["reps"]
            items = int(math.ceil(wcu_val * steady_secs / avg))
            work_secs = math.ceil(items * avg / wcu_val)
            cells.append({
                "cell_id": f"{executor}-{wcu_label}-{MIX_SHORT[mix]}",
                "executor": executor,
                "mix": mix,
                "wcu": wcu_val,
                "items": items,
                "prod_rate": matrix["prod_rate"],
                "reps": reps,
                "budget_secs": work_secs + 300,
            })
# Round-robin shard assignment; both instance types run all shards (Q6).
for i, cell in enumerate(cells):
    cell["shard"] = i % num_shards
json.dump({
    "run_id": run_id,
    "commit_sha": commit_sha,
    "region": region,
    "bucket": bucket,
    "num_shards": num_shards,
    "cells": cells,
}, sys.stdout, indent=2)
print()
PYEOF
echo "config: $CONFIG ($(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))["cells"]))' "$CONFIG") cells, $INSTANCES_PER_TYPE shards per type)"

# --- upload config -------------------------------------------------------------
run aws s3 cp "$CONFIG" "s3://$BUCKET/runs/$RUN_ID/config.json" --region "$REGION"
echo "note: optionally pre-upload prebuilt binaries to" \
     "s3://$BUCKET/runs/$RUN_ID/binaries/{aarch64,x86_64}/dy" \
     "(otherwise instances build from source, ~5 min each)"

# --- launch fleet ---------------------------------------------------------------
for pair in $INSTANCE_TYPES; do
    itype="${pair%%:*}"
    arch="${pair##*:}"
    # AL2023 AMI via the public SSM parameter.
    ami_param="/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-$arch"
    if [ "$DRY_RUN" = "1" ]; then
        echo "DRY-RUN> aws ssm get-parameters --region $REGION --names $ami_param --query 'Parameters[0].Value' --output text"
        ami="ami-DRYRUN-$arch"
    else
        ami=$(aws ssm get-parameters --region "$REGION" --names "$ami_param" \
            --query 'Parameters[0].Value' --output text)
    fi
    shard=0
    while [ "$shard" -lt "$INSTANCES_PER_TYPE" ]; do
        userdata="$OUT_DIR/user-data-$itype-shard$shard.sh"
        sed -e "s|{{RUN_ID}}|$RUN_ID|g" \
            -e "s|{{SHARD}}|$shard|g" \
            -e "s|{{BUCKET}}|$BUCKET|g" \
            -e "s|{{REGION}}|$REGION|g" \
            -e "s|{{COMMIT_SHA}}|$COMMIT_SHA|g" \
            -e "s|{{INSTANCE_TYPE}}|$itype|g" \
            "$SCRIPT_DIR/user_data.sh.tpl" > "$userdata"
        network_args=()
        if [ -n "$SUBNET_ID" ]; then
            network_args+=(--subnet-id "$SUBNET_ID")
        fi
        if [ -n "$SECURITY_GROUP" ]; then
            network_args+=(--security-group-ids "$SECURITY_GROUP")
        fi
        run aws ec2 run-instances --region "$REGION" \
            --image-id "$ami" \
            --instance-type "$itype" \
            --count 1 \
            --iam-instance-profile "Name=$IAM_PROFILE" \
            --instance-initiated-shutdown-behavior terminate \
            --user-data "file://$userdata" \
            "${network_args[@]}" \
            --tag-specifications \
            "ResourceType=instance,Tags=[{Key=dynein-bench,Value=$RUN_ID},{Key=Name,Value=dynein-bench-$RUN_ID-$itype-s$shard}]"
        shard=$((shard + 1))
    done
done

echo
echo "run-id: $RUN_ID"
echo "observe progress: aws s3 ls --recursive s3://$BUCKET/runs/$RUN_ID/results/ | wc -l"
echo "collect results:  scripts/bench/collect.sh $RUN_ID --bucket $BUCKET --region $REGION"
if [ "$DRY_RUN" = "1" ]; then
    echo "(dry-run: nothing was executed; rendered files are in $OUT_DIR)"
fi
