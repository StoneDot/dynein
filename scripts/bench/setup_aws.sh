#!/usr/bin/env bash
# One-time AWS setup for the benchmark fleet (benchmark-plan.md §5.1, §6 item 7):
#   - S3 bucket dynein-bench-<account-id>
#   - IAM role + instance profile "dynein-bench-instance" with the
#     least-privilege policy (DynamoDB on dynein-bench-* tables only,
#     cloudwatch:GetMetricStatistics, S3 Put/Get on the bucket runs/ prefix)
#
# Idempotent: every step checks for the resource before creating it, so the
# script can be re-run safely. Run it once, with user approval, then record
# the ARNs in CLAUDE.local.md.
#
# Usage:
#   scripts/bench/setup_aws.sh [--region ap-northeast-1]

set -euo pipefail

if [ "${BASH_SOURCE[0]}" != "$0" ]; then
    echo "setup_aws.sh must be executed, not sourced" >&2
    return 1
fi

REGION="ap-northeast-1"
ROLE_NAME="dynein-bench-instance"
POLICY_NAME="dynein-bench-policy"

while [ $# -gt 0 ]; do
    case "$1" in
        --region) REGION="$2"; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)
BUCKET="dynein-bench-$ACCOUNT_ID"
echo "account: $ACCOUNT_ID / region: $REGION / bucket: $BUCKET"

# --- S3 bucket -----------------------------------------------------------------
if aws s3api head-bucket --bucket "$BUCKET" 2>/dev/null; then
    echo "bucket $BUCKET already exists"
else
    echo "creating bucket $BUCKET"
    aws s3api create-bucket --bucket "$BUCKET" --region "$REGION" \
        --create-bucket-configuration "LocationConstraint=$REGION"
    aws s3api put-public-access-block --bucket "$BUCKET" \
        --public-access-block-configuration \
        "BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true"
fi

# --- IAM role -------------------------------------------------------------------
TRUST_DOC='{
  "Version": "2012-10-17",
  "Statement": [{
    "Effect": "Allow",
    "Principal": {"Service": "ec2.amazonaws.com"},
    "Action": "sts:AssumeRole"
  }]
}'

POLICY_DOC=$(cat <<EOF
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "DyneinBenchTables",
      "Effect": "Allow",
      "Action": [
        "dynamodb:CreateTable",
        "dynamodb:DeleteTable",
        "dynamodb:DescribeTable",
        "dynamodb:BatchWriteItem",
        "dynamodb:PutItem",
        "dynamodb:Scan",
        "dynamodb:TagResource"
      ],
      "Resource": "arn:aws:dynamodb:*:$ACCOUNT_ID:table/dynein-bench-*"
    },
    {
      "Sid": "ListTablesForCleanup",
      "Effect": "Allow",
      "Action": ["dynamodb:ListTables"],
      "Resource": "*"
    },
    {
      "Sid": "CloudWatchSlowLoop",
      "Effect": "Allow",
      "Action": ["cloudwatch:GetMetricStatistics"],
      "Resource": "*"
    },
    {
      "Sid": "BenchArtifacts",
      "Effect": "Allow",
      "Action": ["s3:GetObject", "s3:PutObject"],
      "Resource": [
        "arn:aws:s3:::$BUCKET/runs/*",
        "arn:aws:s3:::$BUCKET/inputs/*"
      ]
    },
    {
      "Sid": "BenchBucketList",
      "Effect": "Allow",
      "Action": ["s3:ListBucket"],
      "Resource": "arn:aws:s3:::$BUCKET",
      "Condition": {"StringLike": {"s3:prefix": ["runs/*", "inputs/*"]}}
    }
  ]
}
EOF
)

if aws iam get-role --role-name "$ROLE_NAME" >/dev/null 2>&1; then
    echo "role $ROLE_NAME already exists"
else
    echo "creating role $ROLE_NAME"
    aws iam create-role --role-name "$ROLE_NAME" \
        --assume-role-policy-document "$TRUST_DOC" >/dev/null
fi

# put-role-policy is an upsert; safe to repeat (keeps the policy current).
echo "attaching inline policy $POLICY_NAME"
aws iam put-role-policy --role-name "$ROLE_NAME" \
    --policy-name "$POLICY_NAME" --policy-document "$POLICY_DOC"

# SSM Session Manager access for data rescue: the fleet has no SSH keys and
# no inbound security-group rules, so SSM (agent preinstalled on AL2023) is
# the way into a live instance when a run misbehaves.
echo "attaching AmazonSSMManagedInstanceCore (Session Manager access)"
aws iam attach-role-policy --role-name "$ROLE_NAME" \
    --policy-arn arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore

if aws iam get-instance-profile --instance-profile-name "$ROLE_NAME" >/dev/null 2>&1; then
    echo "instance profile $ROLE_NAME already exists"
else
    echo "creating instance profile $ROLE_NAME"
    aws iam create-instance-profile --instance-profile-name "$ROLE_NAME" >/dev/null
fi

ATTACHED=$(aws iam get-instance-profile --instance-profile-name "$ROLE_NAME" \
    --query 'InstanceProfile.Roles[].RoleName' --output text)
case " $ATTACHED " in
    *" $ROLE_NAME "*) echo "role already attached to instance profile" ;;
    *)
        echo "attaching role to instance profile"
        aws iam add-role-to-instance-profile \
            --instance-profile-name "$ROLE_NAME" --role-name "$ROLE_NAME"
        ;;
esac

echo
echo "done. record these in CLAUDE.local.md:"
echo "  bucket:           s3://$BUCKET"
echo "  instance profile: $ROLE_NAME"
aws iam get-role --role-name "$ROLE_NAME" --query 'Role.Arn' --output text
aws iam get-instance-profile --instance-profile-name "$ROLE_NAME" \
    --query 'InstanceProfile.Arn' --output text
