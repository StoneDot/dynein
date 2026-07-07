#!/usr/bin/env bash
# Cloud-side orphan-table reaper (last-resort backstop, independent failure
# domain — survives the dev machine, the session agent, the watchdog and the
# fleet all dying at once).
#
# EventBridge rule (rate 15 min) -> Lambda: delete any us-west-2 table that
#   - name starts with dynein-bench-
#   - carries the dynein-bench tag (run_cells.sh always sets it)
#   - is ACTIVE and older than TTL_MINUTES (default 90; legit fresh-per-rep
#     tables live <= ~25 min, so 90 gives 3.6x margin — if the reuse
#     fallback strategy (commit 8f4cdc5, ~4h shared table) is ever used,
#     RAISE the TTL or disable the rule first)
#
# Bounded orphan cost at 39k WCU: ~TTL/60 x $25.35.
#
# Usage: setup_reaper.sh [--region us-west-2] [--ttl-minutes 90] [--teardown]
set -euo pipefail

REGION=us-west-2
TTL_MINUTES=90
TEARDOWN=0
while [ $# -gt 0 ]; do
    case "$1" in
        --region) REGION="$2"; shift ;;
        --ttl-minutes) TTL_MINUTES="$2"; shift ;;
        --teardown) TEARDOWN=1 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

FN=dynein-bench-reaper
ROLE=dynein-bench-reaper-role
RULE=dynein-bench-reaper-schedule
ACCOUNT=$(aws sts get-caller-identity --query Account --output text)

if [ "$TEARDOWN" = "1" ]; then
    aws events remove-targets --region "$REGION" --rule "$RULE" --ids reaper || true
    aws events delete-rule --region "$REGION" --name "$RULE" || true
    aws lambda delete-function --region "$REGION" --function-name "$FN" || true
    aws iam delete-role-policy --role-name "$ROLE" --policy-name reaper || true
    aws iam delete-role --role-name "$ROLE" || true
    echo "reaper torn down"
    exit 0
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# --- Lambda code ---------------------------------------------------------------
cat > "$TMP/reaper.py" <<'PYEOF'
import boto3, datetime, os

TTL_MIN = int(os.environ.get("TTL_MINUTES", "90"))
PREFIX = "dynein-bench-"

def handler(event, context):
    ddb = boto3.client("dynamodb")
    now = datetime.datetime.now(datetime.timezone.utc)
    deleted, kept = [], []
    names = []
    for page in ddb.get_paginator("list_tables").paginate():
        names += [t for t in page["TableNames"] if t.startswith(PREFIX)]
    for t in names:
        d = ddb.describe_table(TableName=t)["Table"]
        age_min = (now - d["CreationDateTime"]).total_seconds() / 60
        tags = ddb.list_tags_of_resource(ResourceArn=d["TableArn"]).get("Tags", [])
        if not any(tag["Key"] == "dynein-bench" for tag in tags):
            kept.append((t, "no dynein-bench tag"))
            continue
        if age_min > TTL_MIN and d["TableStatus"] == "ACTIVE":
            ddb.delete_table(TableName=t)
            deleted.append((t, round(age_min)))
        else:
            kept.append((t, f"age {round(age_min)}m status {d['TableStatus']}"))
    result = {"deleted": deleted, "kept": kept, "ttl_minutes": TTL_MIN}
    print(result)
    return result
PYEOF
(cd "$TMP" && zip -q reaper.zip reaper.py)

# --- IAM role -------------------------------------------------------------------
cat > "$TMP/trust.json" <<'EOF'
{"Version": "2012-10-17", "Statement": [{"Effect": "Allow",
  "Principal": {"Service": "lambda.amazonaws.com"}, "Action": "sts:AssumeRole"}]}
EOF
cat > "$TMP/policy.json" <<EOF
{"Version": "2012-10-17", "Statement": [
  {"Effect": "Allow", "Action": ["dynamodb:ListTables"], "Resource": "*"},
  {"Effect": "Allow",
   "Action": ["dynamodb:DescribeTable", "dynamodb:ListTagsOfResource",
               "dynamodb:DeleteTable"],
   "Resource": "arn:aws:dynamodb:$REGION:$ACCOUNT:table/dynein-bench-*"},
  {"Effect": "Allow",
   "Action": ["logs:CreateLogGroup", "logs:CreateLogStream", "logs:PutLogEvents"],
   "Resource": "*"}
]}
EOF
aws iam create-role --role-name "$ROLE" \
    --assume-role-policy-document "file://$TMP/trust.json" >/dev/null 2>&1 || true
aws iam put-role-policy --role-name "$ROLE" --policy-name reaper \
    --policy-document "file://$TMP/policy.json"
sleep 8  # IAM propagation before Lambda creation

# --- Lambda ---------------------------------------------------------------------
if aws lambda get-function --region "$REGION" --function-name "$FN" >/dev/null 2>&1; then
    aws lambda update-function-code --region "$REGION" --function-name "$FN" \
        --zip-file "fileb://$TMP/reaper.zip" >/dev/null
    aws lambda wait function-updated --region "$REGION" --function-name "$FN"
    aws lambda update-function-configuration --region "$REGION" --function-name "$FN" \
        --environment "Variables={TTL_MINUTES=$TTL_MINUTES}" >/dev/null
else
    aws lambda create-function --region "$REGION" --function-name "$FN" \
        --runtime python3.12 --handler reaper.handler --timeout 60 \
        --role "arn:aws:iam::$ACCOUNT:role/$ROLE" \
        --zip-file "fileb://$TMP/reaper.zip" \
        --environment "Variables={TTL_MINUTES=$TTL_MINUTES}" \
        --tags dynein-bench=infra >/dev/null
fi
aws lambda wait function-active --region "$REGION" --function-name "$FN"

# --- EventBridge schedule --------------------------------------------------------
aws events put-rule --region "$REGION" --name "$RULE" \
    --schedule-expression "rate(15 minutes)" \
    --description "dynein-bench orphan table reaper" >/dev/null
aws lambda add-permission --region "$REGION" --function-name "$FN" \
    --statement-id eventbridge-reaper --action lambda:InvokeFunction \
    --principal events.amazonaws.com \
    --source-arn "arn:aws:events:$REGION:$ACCOUNT:rule/$RULE" >/dev/null 2>&1 || true
aws events put-targets --region "$REGION" --rule "$RULE" \
    --targets "Id=reaper,Arn=arn:aws:lambda:$REGION:$ACCOUNT:function:$FN" >/dev/null

echo "reaper deployed: $FN in $REGION, TTL ${TTL_MINUTES}m, rate(15 minutes)"
echo "manual invoke:  aws lambda invoke --region $REGION --function-name $FN /dev/stdout"
echo "teardown:       $0 --region $REGION --teardown"
