#!/usr/bin/env bash
# =============================================================================
# deploy.sh — Deploy the BTC sniper bot to an AWS EC2 instance
# =============================================================================
#
# Usage:
#   ./deploy.sh <ssh-host>                    # e.g. ubuntu@54.23.45.67
#   ./deploy.sh <ssh-host> --setup            # first-time: run setup.sh too
#   SSH_KEY=~/.ssh/sniper.pem ./deploy.sh ... # custom key
#
# What it does:
#   1. Builds a release binary locally (cross-compile for aarch64 if needed)
#   2. rsync's the binary + config to the remote server
#   3. Restarts the systemd service
#
# Prerequisites on the EC2 instance (first deploy only — use --setup):
#   - Ubuntu 24.04 (c6g.medium for dry-run, c7gn.xlarge+ for production)
#   - SSH access with key auth
#   - setup.sh has been run once (--setup flag does this)
# =============================================================================

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log()  { printf "${GREEN}[deploy]${NC} %s\n" "$*"; }
warn() { printf "${YELLOW}[deploy]${NC} %s\n" "$*"; }
die()  { printf "${RED}[deploy]${NC} %s\n" "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Args
# ---------------------------------------------------------------------------
HOST="${1:-}"
SETUP=false
[[ "${2:-}" == "--setup" ]] && SETUP=true
[[ -z "$HOST" ]] && die "Usage: $0 <ssh-host> [--setup]"

SSH_KEY="${SSH_KEY:-}"
SSH_OPTS="-o StrictHostKeyChecking=accept-new -o ConnectTimeout=10"
[[ -n "$SSH_KEY" ]] && SSH_OPTS="$SSH_OPTS -i $SSH_KEY"

REMOTE_DIR="/opt/btc-sniper"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# ---------------------------------------------------------------------------
# 1. Build release binary
# ---------------------------------------------------------------------------
log "building release binary..."
cd "$SCRIPT_DIR"

# Detect if we need to cross-compile (local=x86, remote=arm or vice versa)
LOCAL_ARCH="$(uname -m)"
REMOTE_ARCH="$(ssh $SSH_OPTS "$HOST" uname -m 2>/dev/null || echo "unknown")"
log "local=$LOCAL_ARCH remote=$REMOTE_ARCH"

TARGET=""
if [[ "$LOCAL_ARCH" == "x86_64" && "$REMOTE_ARCH" == "aarch64" ]]; then
    TARGET="--target aarch64-unknown-linux-gnu"
    rustup target add aarch64-unknown-linux-gnu 2>/dev/null || true
    warn "cross-compiling x86 → aarch64 (needs linker: apt install gcc-aarch64-linux-gnu)"
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
elif [[ "$LOCAL_ARCH" == "aarch64" && "$REMOTE_ARCH" == "x86_64" ]]; then
    TARGET="--target x86_64-unknown-linux-gnu"
    rustup target add x86_64-unknown-linux-gnu 2>/dev/null || true
    warn "cross-compiling aarch64 → x86_64"
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc
fi

cargo build --release $TARGET 2>&1 | tail -5

# Find the binary
if [[ -n "$TARGET" ]]; then
    BINARY="target/$(echo $TARGET | sed 's/--target //')/release/sniper"
else
    BINARY="target/release/sniper"
fi
[[ -f "$BINARY" ]] || die "binary not found at $BINARY"
log "binary: $BINARY ($(du -h "$BINARY" | cut -f1))"

# ---------------------------------------------------------------------------
# 2. Create remote directory structure
# ---------------------------------------------------------------------------
log "setting up remote directories..."
ssh $SSH_OPTS "$HOST" "sudo mkdir -p $REMOTE_DIR/{logs,state,config} && sudo chown -R \$(whoami) $REMOTE_DIR"

# ---------------------------------------------------------------------------
# 3. rsync files to remote
# ---------------------------------------------------------------------------
log "syncing files to $HOST:$REMOTE_DIR..."

RSYNC_OPTS="-az --progress"
[[ -n "$SSH_KEY" ]] && RSYNC_OPTS="$RSYNC_OPTS -e 'ssh -i $SSH_KEY'"

# Binary
rsync $RSYNC_OPTS "$BINARY" "$HOST:$REMOTE_DIR/sniper"

# Config and systemd
rsync $RSYNC_OPTS \
    .env.example \
    systemd/sniper.service \
    setup.sh \
    "$HOST:$REMOTE_DIR/"

# If .env exists locally, sync it (but don't overwrite remote .env)
if [[ -f .env ]]; then
    rsync $RSYNC_OPTS --ignore-existing .env "$HOST:$REMOTE_DIR/.env"
    log "synced .env (won't overwrite existing remote .env)"
fi

# ---------------------------------------------------------------------------
# 4. First-time setup (--setup flag)
# ---------------------------------------------------------------------------
if $SETUP; then
    log "running first-time setup..."

    ssh $SSH_OPTS "$HOST" bash -s <<'REMOTE_SETUP'
set -euo pipefail

cd /opt/btc-sniper

# Create sniper user if it doesn't exist
if ! id -u sniper &>/dev/null; then
    sudo useradd -r -s /usr/sbin/nologin -d /opt/btc-sniper sniper
    echo "[deploy] created sniper user"
fi

# Install the binary
sudo install -m 0755 sniper /usr/local/bin/sniper

# Install systemd unit
sudo install -m 0644 sniper.service /etc/systemd/system/sniper.service
sudo systemctl daemon-reload

# Create .env from example if it doesn't exist
if [[ ! -f .env ]]; then
    cp .env.example .env
    echo "[deploy] created .env from .env.example — EDIT IT with your keys!"
fi

# Fix ownership
sudo chown -R sniper:sniper /opt/btc-sniper
sudo chmod 600 /opt/btc-sniper/.env

# Run the system setup (kernel tuning, hugepages, etc.)
sudo bash setup.sh

echo "[deploy] first-time setup complete"
REMOTE_SETUP

    warn "IMPORTANT: Edit /opt/btc-sniper/.env on the server with your Polymarket credentials"
    warn "  ssh $HOST 'sudo nano /opt/btc-sniper/.env'"
fi

# ---------------------------------------------------------------------------
# 5. Deploy: install binary + restart service
# ---------------------------------------------------------------------------
log "deploying binary and restarting service..."
ssh $SSH_OPTS "$HOST" bash -s <<'REMOTE_DEPLOY'
set -euo pipefail
cd /opt/btc-sniper

# Install updated binary
sudo install -m 0755 sniper /usr/local/bin/sniper

# Verify
/usr/local/bin/sniper --help 2>/dev/null || echo "(binary installed, no --help)"

# Fix ownership
sudo chown -R sniper:sniper /opt/btc-sniper

# Restart service
if systemctl is-enabled sniper.service &>/dev/null; then
    sudo systemctl restart sniper.service
    sleep 2
    if systemctl is-active sniper.service &>/dev/null; then
        echo "[deploy] sniper service restarted successfully"
        sudo journalctl -u sniper -n 10 --no-pager
    else
        echo "[deploy] WARNING: service failed to start"
        sudo journalctl -u sniper -n 30 --no-pager
    fi
else
    echo "[deploy] service not enabled — enable with: sudo systemctl enable --now sniper"
fi
REMOTE_DEPLOY

# ---------------------------------------------------------------------------
# Done
# ---------------------------------------------------------------------------
log "deploy complete!"
log ""
log "Useful commands:"
log "  ssh $HOST 'sudo journalctl -u sniper -f'     # tail logs"
log "  ssh $HOST 'sudo systemctl status sniper'      # check status"
log "  ssh $HOST 'sudo kill -USR1 \$(pidof sniper)'  # dump latency stats"
log "  ssh $HOST 'sudo systemctl stop sniper'        # stop bot"
