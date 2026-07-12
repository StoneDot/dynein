#!/usr/bin/env bash
# Periodic health probe for Tier-1 runs — closes the watchdog's blind spots:
# account-wide table inventory (not run-scoped), watchdog process liveness,
# and monotonic results progress. Read-only.
set -u
R1=20260707-152019
B=dynein-bench-975049903426
SP="${SESSION_SCRATCH:-/tmp}"

echo "== $(date -u +%FT%TZ)"
# 1. guardian liveness
if pgrep -f "[w]atchdog.sh --run-id $R1" >/dev/null; then echo "watchdog(stage1): ALIVE"; else echo "watchdog(stage1): DEAD"; fi
tail -1 "$SP/watchdog-$R1.log"

# 2. ACCOUNT-WIDE us-west-2 inventory (catches out-of-prefix tables)
tables=$(aws dynamodb list-tables --region us-west-2 --query 'TableNames' --output text 2>&1)
echo "us-west-2 tables: ${tables:-none}"
for t in $tables; do
  aws dynamodb describe-table --region us-west-2 --table-name "$t" \
    --query 'Table.[TableName,ProvisionedThroughput.WriteCapacityUnits,TableStatus]' --output text 2>/dev/null
done
inst=$(aws ec2 describe-instances --region us-west-2 --filters "Name=instance-state-name,Values=pending,running" "Name=tag-key,Values=dynein-bench" --query 'Reservations[].Instances[].[InstanceId,Tags[?Key==`Name`].Value|[0]]' --output text 2>&1)
echo "bench instances running: ${inst:-none}"

# 3. results progress (monotonic check against last snapshot)
n=$(aws s3 ls --recursive s3://$B/runs/$R1/results/ 2>/dev/null | grep -c result.json)
prev=$(cat "$SP/.stage1_results_count" 2>/dev/null || echo 0)
echo "$n" > "$SP/.stage1_results_count"
echo "stage1 result.json: $n (prev $prev) / 36 expected"

# 4. heartbeat age
hb=$(aws s3 cp s3://$B/runs/$R1/heartbeat/m9g.xlarge-s0.json - 2>/dev/null)
echo "heartbeat: ${hb:-MISSING}"
