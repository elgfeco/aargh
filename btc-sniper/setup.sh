#!/usr/bin/env bash
# =============================================================================
# setup.sh — BTC Sniper bot server configuration for AWS EC2 (Ubuntu 24.04)
# =============================================================================
# Target hardware: c7gn.16xlarge (ARM Graviton3, 64 vCPU, 128 GB, 200 Gbps)
#   — or —        i4i.metal / z1d.metal for dedicated-core bare metal
#
# This script is IDEMPOTENT: it can be re-run safely. It:
#   1. Installs baseline packages, build toolchain, and Rust stable
#   2. Tunes the kernel (sysctl) for ultra-low-latency TCP/UDP
#   3. Disables CPU C/P-states, enables performance governor
#   4. Configures 2MB hugepages (eliminates TLB misses on hot path)
#   5. Pins NIC IRQs to isolated cores; disables irqbalance
#   6. Disables hyperthreading on trading cores (cores 4-15 reserved)
#   7. Enables BBR congestion control + TCP_NODELAY defaults
#   8. Installs the systemd unit and enables auto-start
#
# Run as root:  sudo bash setup.sh
# =============================================================================

set -euo pipefail

LOG() { printf '\e[32m[setup]\e[0m %s\n' "$*" >&2; }
WARN() { printf '\e[33m[setup]\e[0m %s\n' "$*" >&2; }
DIE() { printf '\e[31m[setup]\e[0m %s\n' "$*" >&2; exit 1; }

[[ $EUID -eq 0 ]] || DIE "Must run as root. Use sudo."

# -----------------------------------------------------------------------------
# 0. Detect platform & capabilities
# -----------------------------------------------------------------------------
ARCH="$(uname -m)"
KERNEL="$(uname -r)"
NPROC="$(nproc)"
LOG "arch=$ARCH kernel=$KERNEL cpus=$NPROC"

if ! grep -q "Ubuntu 24" /etc/os-release 2>/dev/null; then
  WARN "This script targets Ubuntu 24.04 — proceeding anyway."
fi

# Cores 0-3: OS/housekeeping. Cores 4-63: isolated for trading threads.
HOUSEKEEPING_CORES="0-3"
ISOLATED_CORES="4-$((NPROC - 1))"
LOG "housekeeping_cores=$HOUSEKEEPING_CORES isolated_cores=$ISOLATED_CORES"

# -----------------------------------------------------------------------------
# 1. Baseline packages
# -----------------------------------------------------------------------------
LOG "installing baseline packages..."
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y --no-install-recommends \
  build-essential pkg-config libssl-dev ca-certificates curl git jq \
  cpufrequtils linux-tools-common linux-tools-generic \
  ethtool numactl hwloc tuned \
  htop iotop sysstat net-tools iproute2 \
  chrony python3-pip python3-venv \
  clang llvm libclang-dev \
  dpdk dpdk-dev libdpdk-dev 2>/dev/null || WARN "dpdk packages skipped"

# Rust stable toolchain (pinned to 2021 edition via rust-toolchain)
if ! command -v rustc >/dev/null; then
  LOG "installing Rust stable..."
  curl --proto '=https' --tlsv1.3 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal
fi
# shellcheck disable=SC1091
source "$HOME/.cargo/env" 2>/dev/null || true
rustup component add rustfmt clippy

# -----------------------------------------------------------------------------
# 2. Kernel sysctl tuning — the heart of low-latency Linux networking
# -----------------------------------------------------------------------------
LOG "writing /etc/sysctl.d/99-sniper.conf..."
cat > /etc/sysctl.d/99-sniper.conf <<'SYSCTL'
# ====== BTC Sniper low-latency tuning ==========================================
# Buffers: 64 MiB. Large enough to avoid drops at 100Gb burst, small enough
# to keep queuing latency predictable.
net.core.rmem_default        = 67108864
net.core.wmem_default        = 67108864
net.core.rmem_max            = 67108864
net.core.wmem_max            = 67108864
net.core.optmem_max          = 16777216
net.core.netdev_max_backlog  = 250000
net.core.netdev_budget       = 600
net.core.netdev_budget_usecs = 8000
net.core.somaxconn           = 65535
# SO_BUSY_POLL: spin on NIC RX for up to N microseconds instead of interrupt.
# Dramatic jitter reduction at the cost of CPU. 50µs is a good tradeoff.
net.core.busy_poll           = 50
net.core.busy_read           = 50

# TCP tuning for HFT
net.ipv4.tcp_rmem             = 4096 87380 67108864
net.ipv4.tcp_wmem             = 4096 65536 67108864
net.ipv4.tcp_mem              = 786432 1048576 67108864
net.ipv4.tcp_low_latency      = 1
net.ipv4.tcp_fastopen         = 3
net.ipv4.tcp_timestamps       = 0
net.ipv4.tcp_sack             = 1
net.ipv4.tcp_window_scaling   = 1
net.ipv4.tcp_syncookies       = 1
net.ipv4.tcp_tw_reuse         = 1
net.ipv4.tcp_fin_timeout      = 10
net.ipv4.tcp_slow_start_after_idle = 0
net.ipv4.tcp_no_metrics_save  = 1
net.ipv4.tcp_mtu_probing      = 1
# BBR v2 congestion control — lower tail latency than CUBIC under load
net.ipv4.tcp_congestion_control = bbr
net.core.default_qdisc          = fq

# Reduce swap aggressiveness — hot path must never fault
vm.swappiness                 = 1
vm.dirty_ratio                = 10
vm.dirty_background_ratio     = 5
vm.zone_reclaim_mode          = 0
vm.max_map_count              = 1048576

# Security sane defaults (don't disable SYN cookies under attack)
net.ipv4.conf.all.rp_filter    = 1
net.ipv4.conf.default.rp_filter = 1

# File descriptors — each WS socket + REST conn pool eats FDs
fs.file-max                   = 2097152
fs.nr_open                    = 2097152
SYSCTL
sysctl --system >/dev/null

# -----------------------------------------------------------------------------
# 3. ulimit / systemd limits
# -----------------------------------------------------------------------------
LOG "configuring ulimits..."
cat > /etc/security/limits.d/99-sniper.conf <<'LIMITS'
*    soft    nofile    1048576
*    hard    nofile    1048576
*    soft    memlock   unlimited
*    hard    memlock   unlimited
*    soft    nproc     unlimited
*    hard    nproc     unlimited
*    soft    rtprio    99
*    hard    rtprio    99
LIMITS

mkdir -p /etc/systemd/system.conf.d
cat > /etc/systemd/system.conf.d/99-sniper.conf <<'LIMITS'
[Manager]
DefaultLimitNOFILE=1048576
DefaultLimitMEMLOCK=infinity
DefaultLimitRTPRIO=99
LIMITS

# -----------------------------------------------------------------------------
# 4. CPU governor: performance — disable frequency scaling entirely
# -----------------------------------------------------------------------------
LOG "setting CPU governor=performance..."
for g in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
  [[ -f "$g" ]] && echo performance > "$g" || true
done

# Disable C-states deeper than C1 for isolated trading cores
# (Graviton3 exposes limited cpuidle; this is a best-effort for x86 too)
if [[ -d /sys/devices/system/cpu/cpu0/cpuidle ]]; then
  for d in /sys/devices/system/cpu/cpu*/cpuidle/state[2-9]/disable; do
    [[ -f "$d" ]] && echo 1 > "$d" || true
  done
fi

# Persistent via tuned
systemctl enable --now tuned 2>/dev/null || true
tuned-adm profile network-latency 2>/dev/null || true

# -----------------------------------------------------------------------------
# 5. Disable hyperthreading on trading cores (x86 only; no-op on Graviton)
# -----------------------------------------------------------------------------
if [[ -f /sys/devices/system/cpu/smt/control ]]; then
  LOG "disabling SMT/HyperThreading..."
  echo off > /sys/devices/system/cpu/smt/control || WARN "SMT control failed"
fi

# -----------------------------------------------------------------------------
# 6. Hugepages — 2 MiB pages preallocated at boot
# -----------------------------------------------------------------------------
LOG "configuring 2MiB hugepages (2048 pages = 4 GiB)..."
echo 2048 > /proc/sys/vm/nr_hugepages || WARN "hugepages write failed"
if ! grep -q hugetlbfs /etc/fstab; then
  mkdir -p /mnt/huge
  echo 'hugetlbfs /mnt/huge hugetlbfs defaults,pagesize=2M 0 0' >> /etc/fstab
  mount /mnt/huge 2>/dev/null || true
fi

# -----------------------------------------------------------------------------
# 7. IRQ affinity — pin NIC RX/TX queues to housekeeping cores, keep
#    isolated cores interrupt-free for the trading loop.
# -----------------------------------------------------------------------------
LOG "tuning NIC IRQs..."
systemctl stop irqbalance 2>/dev/null || true
systemctl disable irqbalance 2>/dev/null || true

NIC="$(ip -o -4 route show to default | awk '{print $5}' | head -n1)"
if [[ -n "${NIC:-}" ]]; then
  LOG "primary NIC=$NIC"
  # Enable Enhanced Networking features
  ethtool -K "$NIC" tx on rx on tso on gro on lro off 2>/dev/null || true
  ethtool -G "$NIC" rx 4096 tx 4096 2>/dev/null || true
  ethtool -C "$NIC" adaptive-rx off adaptive-tx off \
      rx-usecs 0 tx-usecs 0 rx-frames 1 tx-frames 1 2>/dev/null || true
  # Pin all NIC IRQs to cores 0-3
  grep -l "$NIC" /proc/interrupts 2>/dev/null | while read -r _; do :; done
  for irq in $(awk -v n="$NIC" '$0 ~ n {gsub(":",""); print $1}' /proc/interrupts); do
    echo f > "/proc/irq/${irq}/smp_affinity" 2>/dev/null || true
  done
fi

# -----------------------------------------------------------------------------
# 8. GRUB kernel params for isolcpus, nohz_full, rcu_nocbs
# -----------------------------------------------------------------------------
LOG "updating GRUB kernel command line..."
GRUB_FILE=/etc/default/grub
if [[ -f "$GRUB_FILE" ]] && ! grep -q 'isolcpus=' "$GRUB_FILE"; then
  KERNEL_PARAMS="isolcpus=${ISOLATED_CORES} nohz_full=${ISOLATED_CORES} rcu_nocbs=${ISOLATED_CORES}"
  KERNEL_PARAMS="$KERNEL_PARAMS mitigations=off intel_idle.max_cstate=1 processor.max_cstate=1"
  KERNEL_PARAMS="$KERNEL_PARAMS idle=poll transparent_hugepage=never skew_tick=1"
  sed -i.bak "s|^GRUB_CMDLINE_LINUX_DEFAULT=\"|GRUB_CMDLINE_LINUX_DEFAULT=\"${KERNEL_PARAMS} |" "$GRUB_FILE"
  update-grub 2>/dev/null || WARN "update-grub failed — reboot needed to take effect"
  WARN "GRUB updated. A reboot is required for isolcpus to take effect."
fi

# -----------------------------------------------------------------------------
# 9. Time sync — chrony with high-precision servers. Critical for timestamps.
# -----------------------------------------------------------------------------
LOG "configuring chrony..."
cat > /etc/chrony/conf.d/99-sniper.conf <<'CHRONY' 2>/dev/null || true
# Low-latency NTP pool; prefer stratum-1 where reachable
server time.aws.com iburst minpoll 4 maxpoll 6
server time1.google.com iburst minpoll 4 maxpoll 6
makestep 0.1 3
rtcsync
maxupdateskew 100.0
CHRONY
systemctl restart chrony 2>/dev/null || true

# -----------------------------------------------------------------------------
# 10. Install the binary + systemd unit
# -----------------------------------------------------------------------------
if [[ -f ./target/release/sniper ]]; then
  LOG "installing sniper binary to /usr/local/bin..."
  install -m 0755 ./target/release/sniper /usr/local/bin/sniper
fi

if [[ -f ./systemd/sniper.service ]]; then
  LOG "installing systemd unit..."
  install -m 0644 ./systemd/sniper.service /etc/systemd/system/sniper.service
  systemctl daemon-reload
  systemctl enable sniper.service || true
fi

# -----------------------------------------------------------------------------
# 11. Final verification
# -----------------------------------------------------------------------------
LOG "=== verification ==="
LOG "governor     : $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo n/a)"
LOG "congestion   : $(sysctl -n net.ipv4.tcp_congestion_control)"
LOG "busy_poll    : $(sysctl -n net.core.busy_poll)µs"
LOG "hugepages    : $(cat /proc/sys/vm/nr_hugepages) × 2MiB"
LOG "rmem_max     : $(sysctl -n net.core.rmem_max)"
LOG "nofile limit : $(ulimit -n)"
LOG "smt control  : $(cat /sys/devices/system/cpu/smt/control 2>/dev/null || echo n/a)"

LOG "done. A reboot is recommended for isolcpus/nohz_full to activate."
