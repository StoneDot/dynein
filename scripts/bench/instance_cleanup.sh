#!/usr/bin/env bash
# Independent guardian cleanup for a benchmark instance (postmortem §4.4).
#
# Invoked by the dynein-bench-cleanup.service OnFailure unit when the runner
# unit dies (OOM, SIGKILL, nonzero exit): the runner's own EXIT trap cannot
# be relied on — SIGKILL skips traps, and in run 090552 the OOM killed the
# runner and its cleanup trap together while a 39k-WCU table kept billing.
#
# Order of operations (experiment-protocol §8): money first (delete the
# run's tables), evidence second (salvage artifacts + boot log to S3),
# compute last (shutdown; the instance is launched with
# instance-initiated-shutdown-behavior=terminate).
#
# Reads its parameters from the environment (EnvironmentFile=/opt/dynein-bench/env):
#   RUN_ID, REGION, BUCKET, INSTANCE_TYPE, SHARD, WORK_DIR

set -uo pipefail  # deliberately no -e: every step is best-effort

log() { printf '[cleanup] %s %s\n' "$(date -u +%FT%TZ)" "$*"; }

: "${RUN_ID:?}" "${REGION:?}" "${BUCKET:?}"
INSTANCE_TYPE="${INSTANCE_TYPE:-unknown}"
SHARD="${SHARD:-0}"
WORK_DIR="${WORK_DIR:-/opt/dynein-bench/work}"
S3_PREFIX="s3://$BUCKET/runs/$RUN_ID/results/$INSTANCE_TYPE"

log "guardian cleanup started (runner unit failed)"

# 1. Money: delete every table belonging to this run.
tables=$(aws dynamodb list-tables --region "$REGION" \
    --output text --query 'TableNames[]' 2>/dev/null | tr '\t' '\n' \
    | grep "^dynein-bench-${RUN_ID}-" || true)
for t in $tables; do
    log "deleting table $t"
    aws dynamodb delete-table --region "$REGION" --table-name "$t" >/dev/null 2>&1 \
        || log "WARNING: failed to delete $t"
done

# 2. Evidence: salvage partial artifacts and the boot log.
mkdir -p "$WORK_DIR/artifacts" 2>/dev/null
cp /var/log/dynein-bench.log "$WORK_DIR/artifacts/boot-shard-$SHARD.log" 2>/dev/null
journalctl -u dynein-bench-runner.service --no-pager -n 500 \
    > "$WORK_DIR/artifacts/runner-unit-shard-$SHARD.log" 2>/dev/null
date -u +%FT%TZ > "$WORK_DIR/artifacts/guardian-cleanup-shard-$SHARD.marker"
aws s3 cp --recursive "$WORK_DIR/artifacts" "$S3_PREFIX/" >/dev/null 2>&1 \
    || log "WARNING: artifact salvage upload failed"

# 3. Compute: shut down (terminates the instance).
log "guardian cleanup done; shutting down"
shutdown -h now 2>/dev/null || sudo shutdown -h now
