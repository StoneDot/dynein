#!/usr/bin/env bash
# Download a benchmark run's results tree from S3 (benchmark-plan.md §5.3).
#
# Usage:
#   scripts/bench/collect.sh <run-id> [--dest DIR] [--bucket B] [--region R]
#
# Then: scripts/bench/analyze.py <dest>

set -euo pipefail

RUN_ID=""
DEST=""
BUCKET="${BENCH_BUCKET:-}"
REGION="ap-northeast-1"

while [ $# -gt 0 ]; do
    case "$1" in
        --dest) DEST="$2"; shift ;;
        --bucket) BUCKET="$2"; shift ;;
        --region) REGION="$2"; shift ;;
        -*) echo "unknown argument: $1" >&2; exit 2 ;;
        *)
            if [ -n "$RUN_ID" ]; then
                echo "unexpected argument: $1" >&2
                exit 2
            fi
            RUN_ID="$1"
            ;;
    esac
    shift
done

if [ -z "$RUN_ID" ]; then
    echo "usage: collect.sh <run-id> [--dest DIR] [--bucket B] [--region R]" >&2
    exit 2
fi
if [ -z "$BUCKET" ]; then
    echo "--bucket (or BENCH_BUCKET) is required" >&2
    exit 2
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEST="${DEST:-$SCRIPT_DIR/out/$RUN_ID/results}"
mkdir -p "$DEST"

aws s3 sync "s3://$BUCKET/runs/$RUN_ID/results/" "$DEST" --region "$REGION"
echo "downloaded to $DEST"
echo "next: python3 $SCRIPT_DIR/analyze.py $DEST"
