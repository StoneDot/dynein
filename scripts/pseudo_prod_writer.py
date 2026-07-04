"""Pseudo production workload: steady PutItem stream at a fixed WCU rate.

Runs for --duration seconds at --rate items/sec (items are ~110 bytes = 1 WCU),
then exits. Throttling errors are counted but not retried (a real production
workload's requests just fail or get retried by its own SDK).
"""

import argparse
import time

import boto3
from botocore.config import Config

parser = argparse.ArgumentParser()
parser.add_argument("--table", required=True)
parser.add_argument("--rate", type=float, default=7.0)
parser.add_argument("--duration", type=float, default=240.0)
args = parser.parse_args()

# Disable SDK retries so throttled writes fail fast and pacing stays accurate.
client = boto3.client(
    "dynamodb",
    region_name="ap-northeast-1",
    config=Config(retries={"max_attempts": 1}),
)

start = time.monotonic()
sent = 0
throttled = 0
i = 0
while time.monotonic() - start < args.duration:
    next_at = start + i / args.rate
    delay = next_at - time.monotonic()
    if delay > 0:
        time.sleep(delay)
    try:
        client.put_item(
            TableName=args.table,
            Item={"pk": {"S": f"prod{i:06d}"}, "value": {"S": "p" * 100}},
        )
        sent += 1
    except client.exceptions.ProvisionedThroughputExceededException:
        throttled += 1
    except Exception as e:  # noqa: BLE001
        print(f"error: {e}", flush=True)
    i += 1
    if i % 100 == 0:
        elapsed = time.monotonic() - start
        print(f"[prod] t={elapsed:.0f}s sent={sent} throttled={throttled}", flush=True)

print(f"[prod] done: sent={sent} throttled={throttled}", flush=True)
