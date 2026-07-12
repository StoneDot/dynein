#!/usr/bin/env bash
# EC2 user-data template for benchmark instances (benchmark-plan.md §5.2).
# Rendered by launch.sh: {{RUN_ID}}, {{SHARD}}, {{BUCKET}}, {{REGION}},
# {{COMMIT_SHA}}, {{INSTANCE_TYPE}} are substituted before launch.
#
# Flow: install tooling -> obtain the dy binary (prebuilt from S3 preferred,
# on-instance cargo build as fallback) -> clone the repo at the pinned commit
# -> download config.json -> hand off to the dynein-bench-runner systemd
# unit. The runner unit carries OnFailure=dynein-bench-cleanup.service
# (postmortem §4.4): if the runner dies in a way that skips its EXIT trap
# (OOM SIGKILL took both the workload and the trap down in run 090552), an
# independent, memory-capped guardian unit deletes the run's tables,
# salvages artifacts and shuts the instance down.

set -euxo pipefail
exec > /var/log/dynein-bench.log 2>&1

# cloud-init runs this as root but WITHOUT $HOME in the environment; with
# `set -u` the rustup env sourcing below would abort the whole bootstrap
# ("HOME: unbound variable" — found the hard way, benchmark run 090552).
export HOME=/root

RUN_ID="{{RUN_ID}}"
SHARD="{{SHARD}}"
BUCKET="{{BUCKET}}"
REGION="{{REGION}}"
COMMIT_SHA="{{COMMIT_SHA}}"
INSTANCE_TYPE="{{INSTANCE_TYPE}}"

# Absolute last-resort guard in case the runner unit never starts.
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

# --- config -------------------------------------------------------------------
aws s3 cp "s3://$BUCKET/runs/$RUN_ID/config.json" "$WORK/config.json"

# --- runner + guardian units (postmortem §4.4) --------------------------------
cat > "$WORK/env" <<ENVEOF
RUN_ID=$RUN_ID
SHARD=$SHARD
BUCKET=$BUCKET
REGION=$REGION
INSTANCE_TYPE=$INSTANCE_TYPE
DY_BIN=$DY_BIN
RUNNING_ON_EC2=1
WORK_DIR=$WORK/work
HOME=/root
ENVEOF

cat > /etc/systemd/system/dynein-bench-cleanup.service <<UNITEOF
[Unit]
Description=dynein bench guardian cleanup (tables, salvage, shutdown)

[Service]
Type=oneshot
EnvironmentFile=$WORK/env
# The guardian must survive memory pressure that killed the runner.
MemoryMax=256M
ExecStart=/usr/bin/bash $WORK/repo/scripts/bench/instance_cleanup.sh
UNITEOF

cat > /etc/systemd/system/dynein-bench-runner.service <<UNITEOF
[Unit]
Description=dynein bench cell runner (run $RUN_ID shard $SHARD)
OnFailure=dynein-bench-cleanup.service

[Service]
Type=oneshot
EnvironmentFile=$WORK/env
ExecStart=/usr/bin/bash $WORK/repo/scripts/bench/run_cells.sh $WORK/config.json $SHARD
UNITEOF

systemctl daemon-reload
# --no-block: the runner runs for hours; cloud-init must not wait on it.
# On success run_cells.sh's EXIT trap shuts the instance down; on unit
# failure the OnFailure guardian does.
systemctl start --no-block dynein-bench-runner.service
echo "runner unit started; boot script done"
