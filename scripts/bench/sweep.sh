#!/usr/bin/env bash
# List (and with --delete, remove) leftover benchmark resources
# (benchmark-plan.md §5.2 safety nets):
#   - DynamoDB tables named dynein-bench-* (any run)
#   - EC2 instances tagged with key "dynein-bench" (any run)
#
# Default is list-only; nothing is deleted without --delete.
#
# Usage:
#   scripts/bench/sweep.sh [--region ap-northeast-1] [--delete]

set -euo pipefail

REGION="ap-northeast-1"
DELETE=0

while [ $# -gt 0 ]; do
    case "$1" in
        --region) REGION="$2"; shift ;;
        --delete) DELETE=1 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

echo "== DynamoDB tables (dynein-bench-*) in $REGION =="
TABLES=$(aws dynamodb list-tables --region "$REGION" \
    --output text --query 'TableNames[]' | tr '\t' '\n' \
    | grep '^dynein-bench-' || true)
if [ -z "$TABLES" ]; then
    echo "(none)"
else
    echo "$TABLES"
    if [ "$DELETE" = "1" ]; then
        for t in $TABLES; do
            echo "deleting table $t"
            aws dynamodb delete-table --region "$REGION" --table-name "$t" >/dev/null
        done
    fi
fi

echo
echo "== EC2 instances tagged dynein-bench in $REGION =="
INSTANCES=$(aws ec2 describe-instances --region "$REGION" \
    --filters "Name=tag-key,Values=dynein-bench" \
    "Name=instance-state-name,Values=pending,running,stopping,stopped" \
    --output text \
    --query 'Reservations[].Instances[].[InstanceId,InstanceType,State.Name,Tags[?Key==`dynein-bench`]|[0].Value]')
if [ -z "$INSTANCES" ]; then
    echo "(none)"
else
    echo "$INSTANCES"
    if [ "$DELETE" = "1" ]; then
        IDS=$(echo "$INSTANCES" | awk '{print $1}')
        # shellcheck disable=SC2086
        echo "terminating:" $IDS
        # shellcheck disable=SC2086
        aws ec2 terminate-instances --region "$REGION" --instance-ids $IDS >/dev/null
    fi
fi

if [ "$DELETE" = "0" ]; then
    echo
    echo "(list-only; re-run with --delete to remove the above)"
fi
