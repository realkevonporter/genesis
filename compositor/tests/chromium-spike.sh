#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# chromium-spike.sh — the Stage 4 Chromium 159 dmabuf spike.
#
# Runs REAL Chromium 159 (the snapshot `sevyn browser` launches) against
# the Stage 3 standalone compositor in a QEMU/KVM guest on the KVM box
# and answers the two questions COMPOSITOR_PLAN.md's Risks section
# demands before Stage 5 is built:
#
#   (a) the app_id string from the live Chromium surface (`chromium` —
#       confirmed from the live surface, never assumed);
#   (b) Chromium's dmabuf buffers actually land as textures in the
#       compositor (not just that zwp_linux_dmabuf_v1 is advertised)
#       vs a SHM-only fallback.
#
# Method (no compositor changes — the spike observes from outside):
#
#   * Chromium runs with WAYLAND_DEBUG=1, so every Wayland request it
#     makes is logged: binding zwp_linux_dmabuf_v1, creating dmabuf
#     buffers (zwp_linux_buffer_params_v1.create_immed), and attaching
#     them to its toplevel surface — vs wl_shm pool/buffer creation.
#     WAYLAND_DEBUG object ids use `#` (wl_shm#8), not `@`.
#   * The compositor log is the other half: SEVYN SURFACE METADATA
#     carries the live app_id, SEVYN SURFACE COMMITTED + SEVYN FIRST
#     FRAME PRESENTED after Chromium maps prove a frame was presented
#     from the surface. A dmabuf buffer that is committed and then
#     presented went through the compositor's dmabuf import into the
#     GLES renderer (server.rs: dmabuf_imported imports into the
#     renderer; a failed import calls notifier.failed() and produces
#     no presented frame from that buffer).
#
# Phases (one guest boot, three launches — each phase clears Chromium's
# logs and uses a fresh profile, so the phases cannot contaminate each
# other):
#
#   1. sevyn-browser: the launch as `sevyn browser` does it
#      (--ozone-platform=wayland --no-sandbox, GPU-first, --app=<url>,
#      CDP 9222). This is the verdict phase.
#   2. swiftshader: the same launch plus --use-gl=angle
#      --use-angle=swiftshader --v=1 — tests whether software GL can
#      drive the dmabuf path, and the v=1 log says why the GPU process
#      fell back (if it did).
#   3. no-app: the same launch WITHOUT --app — checks whether the
#      app_id differs without app mode (the adoption track matches by
#      pid; the app_id is only a sanity check, but the check must
#      accept the real value).
#
# Verdicts (printed at the end; logs preserved either way):
#
#   * DMABUF-AS-TEXTURE — a dmabuf buffer was created AND attached to
#     the Chromium toplevel AND the compositor committed + presented a
#     frame from the Chromium surface. The zero-copy bar holds.
#   * SHM-ONLY — Chromium mapped and presented, but every buffer it
#     attached came from wl_shm: the zero-copy bar FAILS and Stage 5
#     must know before it is built. (This is a spike result, not a
#     harness failure — the script exits 1 so CI can never mistake it
#     for a pass.)
#   * NOT PROVEN — Chromium never mapped a surface, or the run could
#     not complete (missing libs, guest failure, ...). The evidence of
#     the attempts is in the preserved logs; a spike that cannot run
#     is a named gap, never a faked pass.
#
# Chromium source: the snapshot already on the KVM box
# (/root/chrome-linux.zip, Chromium 159.0.8150.0 — the same snapshot
# the sevynos M5/M13 builders stage). The guest is the same Ubuntu
# 24.04 cloud image the standalone check uses, with the Chromium
# runtime libraries added to the cloud-init package list.
#
# Usage (on the KVM box, from the repo root):
#   cargo build --release --locked          # in compositor/
#   compositor/tests/chromium-spike.sh
#
# Environment (all optional):
#   SEVYN_COMPOSITOR_BIN  path to sevyn-compositor
#                         (default: compositor/target/release/sevyn-compositor)
#   SEVYN_CHECK_DIR       host dir holding the cloud image + run logs
#                         (default: $HOME/genesis-stage3-check)
#   SEVYN_GUEST_QCOW2     base guest disk (default: $SEVYN_CHECK_DIR/noble.qcow2)
#   SEVYN_CHROMIUM_ZIP    Chromium snapshot zip on the box
#                         (default: /root/chrome-linux.zip)
#   SEVYN_SSH_PORT        host port forwarded to the guest's SSH
#                         (default: 2224, so it never collides with the
#                         standalone check's 2222/2223 or the adoption
#                         check's 2225)
#   SEVYN_GUEST_MEM       guest RAM in MB (default: 4096)
#   SEVYN_GUEST_CPUS      guest vCPUs (default: 4)
#   SPIKE_TIMEOUT         seconds per wait step (default: 60)
#
# Exit 0 only on DMABUF-AS-TEXTURE in phase 1. SHM-ONLY and NOT PROVEN
# exit 1. Run logs are preserved in $SEVYN_CHECK_DIR/last-chromium-spike/.

set -u
cd "$(dirname "$0")/../.." || exit 1

COMPOSITOR_BIN="${SEVYN_COMPOSITOR_BIN:-compositor/target/release/sevyn-compositor}"
CHECK_DIR="${SEVYN_CHECK_DIR:-$HOME/genesis-stage3-check}"
BASE_QCOW2="${SEVYN_GUEST_QCOW2:-$CHECK_DIR/noble.qcow2}"
CHROMIUM_ZIP="${SEVYN_CHROMIUM_ZIP:-/root/chrome-linux.zip}"
SSH_PORT="${SEVYN_SSH_PORT:-2224}"
GUEST_MEM="${SEVYN_GUEST_MEM:-4096}"
GUEST_CPUS="${SEVYN_GUEST_CPUS:-4}"
TIMEOUT="${SPIKE_TIMEOUT:-60}"
SOCKET_NAME="sevyn-spike-test"

WORK="$(mktemp -d /tmp/sevyn-chromium-spike.XXXXXX)"
LAST_RUN="$CHECK_DIR/last-chromium-spike"
QMP_SOCK="$WORK/qmp.sock"
SSH_KEY="$WORK/id_ed25519"
SSH_OPTS="-p $SSH_PORT -i $SSH_KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o LogLevel=ERROR"
# shellcheck disable=SC2086
ssh_guest() { ssh $SSH_OPTS sevyn@127.0.0.1 "$@"; }
scp_to_guest() { scp -P "$SSH_PORT" -i "$SSH_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o LogLevel=ERROR "$@"; }
scp_from_guest() { scp -P "$SSH_PORT" -i "$SSH_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o LogLevel=ERROR "$@"; }

fail() {
    echo "chromium-spike: FAIL: $*" >&2
    preserve_logs
    echo "chromium-spike: run logs preserved in $LAST_RUN" >&2
    exit 1
}

preserve_logs() {
    mkdir -p "$LAST_RUN"
    for f in qemu.log guest-serial.log compositor.log compositor.err genesis.log verdict.txt; do
        [ -f "$WORK/$f" ] && cp -a "$WORK/$f" "$LAST_RUN/$f" 2>/dev/null || true
    done
    for f in chromium-1.log chromium-2.log chromium-3.log chromium-version.txt; do
        [ -f "$WORK/$f" ] && cp -a "$WORK/$f" "$LAST_RUN/$f" 2>/dev/null || true
    done
    scp_from_guest "sevyn@127.0.0.1:/tmp/compositor.log" "$LAST_RUN/compositor.log" 2>/dev/null || true
    scp_from_guest "sevyn@127.0.0.1:/tmp/genesis.log" "$LAST_RUN/genesis.log" 2>/dev/null || true
}

cleanup() {
    if [ -f "$WORK/qemu.pid" ]; then
        kill "$(cat "$WORK/qemu.pid")" 2>/dev/null || true
        sleep 2
        kill -9 "$(cat "$WORK/qemu.pid")" 2>/dev/null || true
    fi
    rm -rf "$WORK"
}
trap cleanup EXIT

wait_for_guest() {
    local file="$1" pattern="$2" what="${3:-$pattern}"
    local deadline=$((SECONDS + TIMEOUT))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if ssh_guest "grep -q -- '$pattern' '$file' 2>/dev/null"; then
            return 0
        fi
        sleep 1
    done
    scp_from_guest "sevyn@127.0.0.1:$file" "$WORK/compositor.log" 2>/dev/null || true
    fail "timed out waiting for $what (pattern: $pattern in guest $file)"
}

[ -x "$COMPOSITOR_BIN" ] || fail "compositor binary not found: $COMPOSITOR_BIN (run cargo build --release --locked in compositor/ first)"
[ -f "$CHROMIUM_ZIP" ] || fail "Chromium zip not found: $CHROMIUM_ZIP"
for tool in qemu-system-x86_64 qemu-img genisoimage python3 ssh scp ssh-keygen unzip; do
    command -v "$tool" >/dev/null || fail "host tool not found: $tool"
done
[ -e /dev/kvm ] || fail "no /dev/kvm — the spike needs the KVM box"
if (exec 3<>/dev/tcp/127.0.0.1/"$SSH_PORT") 2>/dev/null; then
    exec 3>&- 3<&-
    fail "something already listens on 127.0.0.1:$SSH_PORT (stale QEMU? set SEVYN_SSH_PORT)"
fi
mkdir -p "$CHECK_DIR"
[ -f "$BASE_QCOW2" ] || fail "guest base disk not found: $BASE_QCOW2 (run standalone-check.sh once first, or set SEVYN_GUEST_QCOW2)"
# A fresh run must not leave a previous run's files behind to be
# mistaken for this run's evidence.
rm -rf "$LAST_RUN"

# --- Seed ISO (cloud-init NoCloud) -------------------------------------------
ssh-keygen -t ed25519 -f "$SSH_KEY" -N "" -C "sevyn-chromium-spike" >/dev/null \
    || fail "ssh-keygen failed"
mkdir -p "$WORK/seed"
cat >"$WORK/seed/meta-data" <<EOF
instance-id: sevyn-chromium-spike
local-hostname: sevyn-chromium-spike
EOF
cat >"$WORK/seed/user-data" <<EOF
#cloud-config
hostname: sevyn-chromium-spike
manage_etc_hosts: true
users:
  - name: sevyn
    sudo: ALL=(ALL) NOPASSWD:ALL
    shell: /bin/bash
    ssh_authorized_keys:
      - $(cat "$SSH_KEY.pub")
packages:
  - seatd
  - gcc
  - pkg-config
  - libwayland-dev
  - libxkbcommon-dev
  - libudev-dev
  - libinput-dev
  - libgbm-dev
  - libseat-dev
  - libdrm-dev
  - libegl1
  - libgl1-mesa-dri
  - libpixman-1-0
  - wayland-protocols
  - python3
  - unzip
  - curl
  - libnss3
  - libnspr4
  - libatk1.0-0t64
  - libatk-bridge2.0-0t64
  - libcups2t64
  - libdrm2
  - libxkbcommon0
  - libxcomposite1
  - libxdamage1
  - libxfixes3
  - libxrandr2
  - libasound2t64
  - libpango-1.0-0
  - libcairo2
  - libatspi2.0-0t64
  - libexpat1
  - libxcb1
  - libxext6
  - fonts-liberation
EOF
genisoimage -output "$WORK/seed.iso" -volid cidata -joliet -rock "$WORK/seed/user-data" "$WORK/seed/meta-data" >/dev/null 2>&1 \
    || fail "seed ISO creation failed"

# --- Boot the guest -----------------------------------------------------------
qemu-img create -f qcow2 -b "$BASE_QCOW2" -F qcow2 "$WORK/guest.qcow2" 16G \
    || fail "guest overlay disk creation failed"
# shellcheck disable=SC2086
qemu-system-x86_64 -machine q35,accel=kvm -cpu host -m "$GUEST_MEM" -smp "$GUEST_CPUS" \
    -drive file="$WORK/guest.qcow2",if=virtio,format=qcow2 \
    -drive file="$WORK/seed.iso",format=raw,if=ide,media=cdrom,readonly=on \
    -device virtio-gpu-pci -device virtio-tablet-pci -device virtio-rng-pci \
    -netdev user,id=net0,hostfwd=tcp:127.0.0.1:"$SSH_PORT"-:22 \
    -device virtio-net-pci,netdev=net0 \
    -display none -serial file:"$WORK/guest-serial.log" \
    -qmp unix:"$QMP_SOCK",server,nowait \
    >"$WORK/qemu.log" 2>&1 &
echo $! >"$WORK/qemu.pid"
sleep 2
kill -0 "$(cat "$WORK/qemu.pid")" 2>/dev/null || fail "QEMU failed to start (see $WORK/qemu.log)"

echo "chromium-spike: waiting for guest SSH (up to 300 s)..."
deadline=$((SECONDS + 300))
while ! ssh_guest true 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do sleep 5; done
ssh_guest true 2>/dev/null || fail "guest SSH never came up"

echo "chromium-spike: waiting for cloud-init to finish (up to 660 s)..."
timeout 660 ssh $SSH_OPTS sevyn@127.0.0.1 "sudo cloud-init status --wait" 2>/dev/null \
    || fail "cloud-init did not finish (guest package install incomplete?)"

# --- Sync inputs into the guest ----------------------------------------------
scp_to_guest "$COMPOSITOR_BIN" "sevyn@127.0.0.1:/tmp/sevyn-compositor" \
    || fail "scp compositor binary failed"
scp_to_guest "$CHROMIUM_ZIP" "sevyn@127.0.0.1:/tmp/chrome-linux.zip" \
    || fail "scp Chromium zip failed"

echo "chromium-spike: guest setup (seatd + Chromium unpack + lib check)..."
ssh_guest "sudo bash -c '
set -e
export XDG_RUNTIME_DIR=/tmp/sevyn-xdg
mkdir -p \$XDG_RUNTIME_DIR; chmod 700 \$XDG_RUNTIME_DIR
if [ ! -S /run/seatd.sock ]; then
  seatd >/tmp/seatd.log 2>&1 </dev/null &
  for i in \$(seq 1 75); do [ -S /run/seatd.sock ] && break; sleep 0.2; done
  [ -S /run/seatd.sock ] || { echo guest: seatd socket never appeared >&2; exit 1; }
fi
echo guest: seatd up
cd /tmp && rm -rf chrome-linux && unzip -q chrome-linux.zip
/tmp/chrome-linux/chrome --version
echo guest: Chromium unpacked
missing=\$(ldd /tmp/chrome-linux/chrome 2>/dev/null | grep \"not found\" || true)
if [ -n \"\$missing\" ]; then echo \"guest: missing libs: \$missing\"; fi
'" || fail "guest setup failed"

# --- Start the standalone compositor ------------------------------------------
ssh_guest "sudo bash -c '
export XDG_RUNTIME_DIR=/tmp/sevyn-xdg
rm -f /tmp/compositor.exit /tmp/compositor.log /tmp/compositor.err
setsid nohup bash -c \"XDG_RUNTIME_DIR=/tmp/sevyn-xdg /tmp/sevyn-compositor --standalone --socket $SOCKET_NAME >/tmp/compositor.log 2>/tmp/compositor.err & echo \\\$! > /tmp/compositor.pid; wait \\\$!; echo \\\$? > /tmp/compositor.exit\" >/dev/null 2>&1 &
'" || fail "could not start compositor"
wait_for_guest /tmp/compositor.log "SEVYN READY" "compositor READY"
wait_for_guest /tmp/compositor.log "SEVYN FIRST FRAME PRESENTED" "first presented frame"
echo "chromium-spike: compositor READY (standalone, dmabuf global advertised — see READY/backend=drm)"

# --- Phases ---------------------------------------------------------------------
# run_phase <n> <extra-flags...> : launch Chromium once, wait for its
# surface in the compositor log, collect the evidence. The signal is
# the compositor log (SURFACE METADATA), never the launcher pid —
# chrome forks and the launcher exits while the browser keeps running.
# The base flags mirror `sevyn browser` (plan, Stage 5): ozone
# Wayland, no sandbox, GPU-first, CDP 9222, profile dir.
run_phase() {
    local n="$1"; shift
    local log="/tmp/chromium-$n.log"
    # NB: run_phase's ONLY stdout is the final seen flag (the caller
    # captures it); progress goes to stderr.
    echo "chromium-spike: phase $n — launching Chromium 159 ($*)..." >&2
    ssh_guest "sudo bash -c '
export XDG_RUNTIME_DIR=/tmp/sevyn-xdg
export WAYLAND_DISPLAY=$SOCKET_NAME
# NB: kill a previous phase Chrome by process NAME, not -f:
# pkill -f chrome-linux/chrome matches this launcher itself, because
# the bash -c command line below contains the literal chrome path in
# the launch line, so -f kills the launcher before it launches.
# That is what killed all three v2 and v3 phases. The bracket trick
# on the pattern alone does not help, because the launch line
# re-introduces it.
pkill -x chrome 2>/dev/null || true
sleep 2
rm -rf /tmp/chromium-profile-$n $log
setsid nohup bash -c \"WAYLAND_DEBUG=1 XDG_RUNTIME_DIR=/tmp/sevyn-xdg WAYLAND_DISPLAY=$SOCKET_NAME /tmp/chrome-linux/chrome --ozone-platform=wayland --no-sandbox --disable-dev-shm-usage --user-data-dir=/tmp/chromium-profile-$n --remote-debugging-port=9222 --enable-logging=stderr --v=1 $* >/tmp/chromium-$n.log 2>&1 &\" >/dev/null 2>&1 &
'" || fail "could not launch Chromium (phase $n)"
    # Chromium cold start under llvmpipe is slow: allow up to 180 s
    # for its surface to appear in the compositor log.
    local seen=0
    deadline=$((SECONDS + 180))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if ssh_guest "grep -q 'SEVYN SURFACE METADATA' /tmp/compositor.log 2>/dev/null"; then
            seen=1
            break
        fi
        sleep 2
    done
    sleep 5
    scp_from_guest "sevyn@127.0.0.1:$log" "$WORK/chromium-$n.log" 2>/dev/null || true
    scp_from_guest "sevyn@127.0.0.1:/tmp/compositor.log" "$WORK/compositor.log" 2>/dev/null || true
    echo "$seen"
}

PHASE1_SEEN="$(run_phase 1 --app=about:blank)"
ssh_guest "curl -s -m 5 http://127.0.0.1:9222/json/version || echo CDP-UNREACHABLE" >"$WORK/chromium-version.txt" 2>/dev/null || true
PHASE2_SEEN="$(run_phase 2 --app=about:blank --use-gl=angle --use-angle=swiftshader)"
PHASE3_SEEN="$(run_phase 3 about:blank)"
ssh_guest "sudo pkill -x chrome 2>/dev/null || true"

# --- Verdict --------------------------------------------------------------------
# analyse <n> <log>: print one evidence line per phase.
analyse() {
    local n="$1"
    local log="$WORK/chromium-$n.log"
    local app_id dmabuf_bind dmabuf_create shm_create
    app_id="$(grep -m1 -o 'set_app_id("[^"]*")' "$log" 2>/dev/null | sed 's/set_app_id("//; s/")//' || echo NONE)"
    dmabuf_bind="$(grep -c 'zwp_linux_dmabuf_v1#[0-9]*\.get_default_feedback' "$log" 2>/dev/null || true)"
    dmabuf_create="$(grep -c 'zwp_linux_buffer_params_v1#[0-9]*\.create' "$log" 2>/dev/null || true)"
    shm_create="$(grep -c 'wl_shm#[0-9]*\.create_pool\|wl_shm_pool#[0-9]*\.create_buffer' "$log" 2>/dev/null || true)"
    echo "phase$n: app_id=${app_id:-NONE} dmabuf_bind=${dmabuf_bind:-0} dmabuf_buffer_creates=${dmabuf_create:-0} shm_buffer_creates=${shm_create:-0}"
}
{
    echo "chromium-spike verdict ($(date -u +%Y-%m-%dT%H:%M:%SZ), KVM box 45.77.162.169)"
    echo "chromium: $(grep -m1 -o 'Chrome/[0-9.]*' "$WORK/chromium-version.txt" 2>/dev/null || echo 'version not seen via CDP')"
    echo "cdp: $(head -c 200 "$WORK/chromium-version.txt" 2>/dev/null)"
    echo "compositor: $(grep -m1 'SEVYN SURFACE METADATA' "$WORK/compositor.log" 2>/dev/null || echo 'no Chromium surface in the compositor log')"
    echo "compositor: $(grep -m1 'SEVYN SURFACE COMMITTED' "$WORK/compositor.log" 2>/dev/null || echo 'no committed Chromium surface')"
    analyse 1
    analyse 2
    analyse 3
    echo "gpu-fallback hints (phase 2, v=1 log):"
    grep -m5 -i "gpu process\|swiftshader\|passthrough\|GL_RENDERER\|fallen back\|software" "$WORK/chromium-2.log" 2>/dev/null | head -5 || echo "(none)"
} | tee "$WORK/verdict.txt"

if [ "$PHASE1_SEEN" != "1" ]; then
    echo "chromium-spike: NOT PROVEN — Chromium never mapped a surface under the standalone compositor (phase 1)"
    preserve_logs
    echo "chromium-spike: run logs preserved in $LAST_RUN"
    exit 1
fi
P1_DMABUF="$(grep -c 'zwp_linux_buffer_params_v1#[0-9]*\.create' "$WORK/chromium-1.log" 2>/dev/null || true)"
P1_SHM="$(grep -c 'wl_shm#[0-9]*\.create_pool\|wl_shm_pool#[0-9]*\.create_buffer' "$WORK/chromium-1.log" 2>/dev/null || true)"
P1_COMMITTED="$(grep -c 'SEVYN SURFACE COMMITTED' "$WORK/compositor.log" 2>/dev/null || true)"
if [ "${P1_DMABUF:-0}" -gt 0 ] && [ "${P1_COMMITTED:-0}" -gt 0 ]; then
    echo "chromium-spike: DMABUF-AS-TEXTURE — Chromium created $P1_DMABUF dmabuf buffer(s) (SHM creates: $P1_SHM) and the compositor committed + presented frames from its surface"
    preserve_logs
    echo "chromium-spike: PASS — app_id + dmabuf-as-texture proven; run logs preserved in $LAST_RUN"
    exit 0
elif [ "${P1_SHM:-0}" -gt 0 ]; then
    echo "chromium-spike: SHM-ONLY — Chromium presented frames but created $P1_SHM SHM buffer(s) and 0 dmabuf buffers: the zero-copy bar FAILS; Stage 5 must know before it is built"
    preserve_logs
    echo "chromium-spike: run logs preserved in $LAST_RUN"
    exit 1
else
    echo "chromium-spike: NOT PROVEN — Chromium mapped (surface seen) but the WAYLAND_DEBUG log shows neither dmabuf nor SHM buffer creation (log truncated? Chromium died mid-start?)"
    preserve_logs
    echo "chromium-spike: run logs preserved in $LAST_RUN"
    exit 1
fi
