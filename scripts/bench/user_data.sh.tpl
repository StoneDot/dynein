#!/usr/bin/env bash
# EC2 user-data template for benchmark instances (benchmark-plan.md §5.2).
# Rendered by launch.sh: {{RUN_ID}}, {{SHARD}}, {{BUCKET}}, {{REGION}},
# {{COMMIT_SHA}}, {{INSTANCE_TYPE}} are substituted before launch.
#
# Flow: install tooling -> obtain the dy binary (prebuilt from S3 preferred,
# on-instance cargo build as fallback) -> clone the repo at the pinned commit
# -> download config.json -> run run_cells.sh (which self-terminates the
# instance via its EXIT trap; the instance is launched with
# --instance-initiated-shutdown-behavior terminate).

set -euxo pipefail
exec > /var/log/dynein-bench.log 2>&1

RUN_ID="{{RUN_ID}}"
SHARD="{{SHARD}}"
BUCKET="{{BUCKET}}"
REGION="{{REGION}}"
COMMIT_SHA="{{COMMIT_SHA}}"
INSTANCE_TYPE="{{INSTANCE_TYPE}}"

# Absolute last-resort guard in case run_cells.sh never starts.
shutdown +540 "dynein-bench boot-level hard guard"

# --- tooling (Amazon Linux 2023) ---------------------------------------------
dnf install -y git gcc perf sysstat python3 python3-pip util-linux tar gzip
# boto3 is only needed by pseudo_prod_writer.py (Tier-2 prod-on cells).
python3 -m pip install --quiet boto3 || true

WORK=/opt/dynein-bench
mkdir -p "$WORK"
cd "$WORK"

# --- repo at the pinned commit (harness scripts + fallback build source) -----
git clone https://github.com/StoneDot/dynein repo
cd repo
git checkout "$COMMIT_SHA"

# --- dy binary: prebuilt from S3 preferred, cargo build as fallback ----------
ARCH="$(uname -m)"
DY_BIN="$WORK/dy"
if aws s3 cp "s3://$BUCKET/runs/$RUN_ID/binaries/$ARCH/dy" "$DY_BIN"; then
    chmod +x "$DY_BIN"
else
    echo "no prebuilt binary for $ARCH; building on-instance (~5 min)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --profile minimal
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
    # tokio_unstable enables RuntimeMonitor metrics (benchmark-plan.md §4);
    # profile prof = release + debug symbols so perf flame graphs resolve.
    RUSTFLAGS="--cfg tokio_unstable" cargo build --profile prof --bin dy
    DY_BIN="$WORK/repo/target/prof/dy"
fi
"$DY_BIN" --version

# --- config + run -------------------------------------------------------------
aws s3 cp "s3://$BUCKET/runs/$RUN_ID/config.json" "$WORK/config.json"

export DY_BIN INSTANCE_TYPE
export RUNNING_ON_EC2=1
export WORK_DIR="$WORK/work"
bash "$WORK/repo/scripts/bench/run_cells.sh" "$WORK/config.json" "$SHARD"

# run_cells.sh's EXIT trap already shuts the instance down; this is belt and
# braces in case the trap is ever removed.
shutdown -h now
