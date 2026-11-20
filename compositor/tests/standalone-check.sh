#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# standalone-check.sh — the Stage 3 standalone-compositor check.
#
# Boots the standalone compositor on a real DRM device (virtio-gpu in a
# KVM guest on the KVM box) and proves the Stage 3 milestones headless:
#
#   1. starts without a parent graphical compositor (--standalone, no
#      Weston/X anywhere in the guest);
#   2. controls one documented output (native mode in the READY marker);
#   3. receives keyboard + pointer input (injected through QEMU: QMP
#      `input-send-event` absolute pointer via the virtio-tablet, QMP
#      `send-key` via the PS/2 keyboard; both arrive as libinput events);
#   4. launches a Shell stand-in; 5. launches an app stand-in;
#      6. displays both surfaces (both commit, presentation-time
#      feedback arrives for both). The real Shell / RN app do not exist
#      in this repo — the C test client stands in, one instance per
#      surface with distinct app_ids (sevyn.shell / sevyn.app);
#   7. focus + close work (pointer/keyboard reach the focused surface;
#      bridge `close` for the app session exits it, session recorded
#      stopped);
#   8. safe return to a terminal after exit (clean exit 0, DRM master
#      released — proven by launching the compositor a SECOND time on
#      the same DRM device and reaching READY again, and by the guest
#      shell staying usable throughout);
#   9. startup + shutdown documented (README / RECOVERY.md describe the
#      actual CLI, markers, and exit codes exercised here);
#   10. crashes produce useful logs (SEVYN_TEST_PANIC=1 deliberate
#       panic: SEVYN CRASH marker + /tmp/genesis.log entry + clean DRM
#       release after it, proven by a second launch reaching READY).
#
# Also proves: the Pixman fallback is ATTEMPTED on a SECOND guest whose
# only DRM device is vkms — and on this box it is NOT PROVEN, honestly:
# the virtio-gpu plane advertises no explicit modifiers (modifier
# Invalid only), and Smithay's Pixman renderer refuses to bind anything
# but LINEAR while its DrmCompositor refuses to allocate a modifier the
# plane does not advertise. vkms advertises LINEAR for all its formats
# (verified with modetest on the KVM box), but Mesa GBM reports every
# buffer it creates on these virtual drivers as modifier Invalid —
# proven directly with a gbm probe on the box (v1/v2/plain+LINEAR all
# report Invalid; v1[Invalid] errors outright) — so the fallback cannot
# present a frame on any DRM device available here. The check records
# that as NOT PROVEN (with the evidence) and exits 1 after proving
# everything else, rather than faking a pass. Real-GPU proof is owed
# (GF65: Intel GBM reports LINEAR). The hardware cursor path is
# code-path only headless (the built-in default arrow element is
# present in the render path — neither virtual driver exposes a cursor
# plane to observe, so plane placement itself is NOT proven here);
# presentation-time feedback arrives (send_presentation_feedback,
# worker 2).
#
# What this check proves: function on real DRM, not fps. The GF65 is the
# fps proof (COMPOSITOR_PLAN.md, "60 fps bar, GF65 is the proof"); QEMU
# cannot prove it. VT-switch re-activation is NOT proven here (no VT
# switch is exercised) and is named as owed in the report.
#
# How it runs: this script is the HOST driver (it runs on the KVM box,
# 45.77.162.169). It boots TWO Ubuntu cloud-image guests under QEMU/KVM
# and drives each over SSH, injecting input over the QMP monitor socket
# on the first one. Guest 1 has virtio-gpu (NOT virgl: this box's QEMU
# 8.2.2 cannot load the virtio-gpu-gl module — `opengl is not available`
# / undefined symbol qemu_egl_display — so the GLES renderer runs on
# Mesa llvmpipe in the guest) and proves milestones 1–8. Guest 2 has
# NO virtual GPU; it loads vkms instead, and proves the Pixman
# fallback + the crash test. The guest-side steps live in
# compositor/tests/standalone-guest.sh.
#
# Usage (on the KVM box, from the repo root):
#   cargo build --release --locked          # in compositor/
#   compositor/tests/standalone-check.sh
#
# Environment (all optional):
#   SEVYN_COMPOSITOR_BIN  path to sevyn-compositor
#                         (default: compositor/target/release/sevyn-compositor)
#   SEVYN_TEST_CLIENT_SRC path to test-client.c (default: compositor/tests/test-client.c)
#   SEVYN_CHECK_DIR       host dir holding the cloud image + run logs
#                         (default: $HOME/genesis-stage3-check)
#   SEVYN_GUEST_QCOW2     base guest disk (default: $SEVYN_CHECK_DIR/noble.qcow2;
#                         downloaded + converted from the Ubuntu cloud image
#                         if missing)
#   SEVYN_SSH_PORT        host port forwarded to the virtio guest's SSH
#                         (default: 2222; the vkms guest uses this + 1)
#   SEVYN_GUEST_MEM       guest RAM in MB (default: 4096)
#   SEVYN_GUEST_CPUS      guest vCPUs (default: 4)
#   STANDALONE_TIMEOUT    seconds per wait step (default: 60)
#
# Exit 0 when every milestone is proven, non-zero otherwise. QEMU is
# killed on exit (success or failure); run logs are preserved in
# $SEVYN_CHECK_DIR/last-standalone-run/.

set -u
cd "$(dirname "$0")/../.." || exit 1

COMPOSITOR_BIN="${SEVYN_COMPOSITOR_BIN:-compositor/target/release/sevyn-compositor}"
CLIENT_SRC="${SEVYN_TEST_CLIENT_SRC:-compositor/tests/test-client.c}"
GUEST_SCRIPT_SRC="compositor/tests/standalone-guest.sh"
CHECK_DIR="${SEVYN_CHECK_DIR:-$HOME/genesis-stage3-check}"
BASE_QCOW2="${SEVYN_GUEST_QCOW2:-$CHECK_DIR/noble.qcow2}"
SSH_PORT="${SEVYN_SSH_PORT:-2222}"
GUEST_MEM="${SEVYN_GUEST_MEM:-4096}"
GUEST_CPUS="${SEVYN_GUEST_CPUS:-4}"
TIMEOUT="${STANDALONE_TIMEOUT:-60}"
SOCKET_NAME="sevyn-standalone-test"

WORK="$(mktemp -d /tmp/sevyn-standalone-check.XXXXXX)"
LAST_RUN="$CHECK_DIR/last-standalone-run"
QMP_SOCK="$WORK/qmp.sock"
SSH_KEY="$WORK/id_ed25519"
# The SSH port is per-guest (2222 virtio, 2223 vkms): rebuild the
# options whenever it changes.
set_guest_port() {
    SSH_PORT="$1"
    SSH_OPTS="-p $SSH_PORT -i $SSH_KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o LogLevel=ERROR"
}
set_guest_port "$SSH_PORT"
# shellcheck disable=SC2086
ssh_guest() { ssh $SSH_OPTS sevyn@127.0.0.1 "$@"; }
scp_to_guest() { scp -P "$SSH_PORT" -i "$SSH_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o LogLevel=ERROR "$@"; }
scp_from_guest() { scp -P "$SSH_PORT" -i "$SSH_KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o LogLevel=ERROR "$@"; }

fail() {
    echo "standalone-check: FAIL: $*" >&2
    preserve_logs
    echo "standalone-check: --- qemu.log (tail) ---" >&2
    for f in "$WORK"/qemu-*.log; do
        [ -f "$f" ] && { echo "standalone-check: --- $(basename "$f") ---" >&2; tail -30 "$f" >&2; }
    done
    echo "standalone-check: --- guest serial (tail) ---" >&2
    for f in "$WORK"/guest-serial-*.log; do
        [ -f "$f" ] && { echo "standalone-check: --- $(basename "$f") ---" >&2; tail -30 "$f" >&2; }
    done
    echo "standalone-check: --- compositor logs (tail) ---" >&2
    for f in compositor.log compositor-gles.log compositor-pixman.log compositor-crash.log genesis.log genesis-pixman.log genesis-crash.log; do
        [ -f "$WORK/$f" ] && { echo "standalone-check: --- $f ---" >&2; tail -40 "$WORK/$f" >&2; }
    done
    echo "standalone-check: run logs preserved in $LAST_RUN" >&2
    exit 1
}

preserve_logs() {
    mkdir -p "$LAST_RUN"
    # Copy the run's own logs (not the QMP sockets or the throwaway SSH key).
    for f in qemu-virtio.log qemu-vkms.log guest-serial-virtio.log guest-serial-vkms.log compositor.log compositor.err compositor-gles.log compositor-pixman.log compositor-crash.log genesis-pixman.log client-shell.log client-app.log genesis-crash.log genesis.log measurements.txt; do
        [ -f "$WORK/$f" ] && cp -a "$WORK/$f" "$LAST_RUN/$f" 2>/dev/null || true
    done
    # Fetch whatever the guest still has (best effort; the guest may be gone).
    scp_from_guest "sevyn@127.0.0.1:/tmp/compositor.log" "$LAST_RUN/compositor.log" 2>/dev/null || true
    scp_from_guest "sevyn@127.0.0.1:/tmp/compositor.err" "$LAST_RUN/compositor.err" 2>/dev/null || true
    scp_from_guest "sevyn@127.0.0.1:/tmp/client-shell.log" "$LAST_RUN/client-shell.log" 2>/dev/null || true
    scp_from_guest "sevyn@127.0.0.1:/tmp/client-app.log" "$LAST_RUN/client-app.log" 2>/dev/null || true
    scp_from_guest "sevyn@127.0.0.1:/tmp/genesis.log" "$LAST_RUN/genesis.log" 2>/dev/null || true
}

cleanup() {
    for pidfile in "$WORK"/qemu-*.pid; do
        [ -f "$pidfile" ] || continue
        kill "$(cat "$pidfile")" 2>/dev/null || true
        sleep 2
        kill -9 "$(cat "$pidfile")" 2>/dev/null || true
    done
    rm -rf "$WORK"
}
trap cleanup EXIT

# wait_for_guest <file> <pattern> [description]: poll a guest file until
# the pattern appears (one SSH round-trip per poll).
wait_for_guest() {
    local file="$1" pattern="$2" what="${3:-$pattern}"
    local deadline=$((SECONDS + TIMEOUT))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if ssh_guest "grep -q -- '$pattern' '$file' 2>/dev/null"; then
            return 0
        fi
        sleep 1
    done
    # Pull the guest log into the work dir before failing, so the
    # failure dump below shows what the compositor actually said.
    scp_from_guest "sevyn@127.0.0.1:$file" "$WORK/compositor.log" 2>/dev/null || true
    fail "timed out waiting for $what (pattern: $pattern in guest $file)"
}

# poll_guest <file> <pattern>: like wait_for_guest, but returns 1 on
# timeout instead of failing — for the Pixman NOT PROVEN path, where a
# timeout with a known environmental cause is recorded, not fatal.
poll_guest() {
    local file="$1" pattern="$2"
    local deadline=$((SECONDS + TIMEOUT))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if ssh_guest "grep -q -- '$pattern' '$file' 2>/dev/null"; then
            return 0
        fi
        sleep 1
    done
    return 1
}

# wait_for_guest_file <file> [description]: poll until a guest file exists.
wait_for_guest_file() {
    local file="$1" what="${2:-$file}"
    local deadline=$((SECONDS + TIMEOUT))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if ssh_guest "test -f '$file' 2>/dev/null"; then
            return 0
        fi
        sleep 1
    done
    fail "timed out waiting for guest file $what"
}

# fetch_guest <remote> <local>: copy a guest file to the host workdir.
fetch_guest() {
    scp_from_guest "sevyn@127.0.0.1:$1" "$2" 2>/dev/null \
        || fail "could not fetch guest file $1"
}

assert_grep() {
    grep -q -- "$2" "$1" || fail "missing marker in $1: $2"
}

# QMP: one python helper for every monitor command (send-key,
# input-send-event absolute pointer, button click). QMP speaks JSON over
# a unix socket: read the greeting, negotiate capabilities, then execute.
cat >"$WORK/qmp.py" <<'PYEOF'
import json, socket, sys
path, cmd = sys.argv[1], sys.argv[2]
args = sys.argv[3:]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(15)
s.connect(path)
f = s.makefile("rw")
greeting = json.loads(f.readline())
assert "QMP" in greeting, greeting
def execute(obj):
    f.write(json.dumps(obj) + "\n")
    f.flush()
    while True:
        msg = json.loads(f.readline())
        if "return" in msg or "error" in msg:
            if "error" in msg:
                raise SystemExit("QMP error: %s" % (msg["error"],))
            return msg.get("return")
execute({"execute": "qmp_capabilities"})
if cmd == "sendkey":
    execute({"execute": "send-key", "arguments": {"keys": [{"type": "qcode", "data": args[0]}]}})
elif cmd == "abs":
    x, y = int(args[0]), int(args[1])
    # QAPI InputAxis values are "x"/"y" (the "abs" event type is what
    # makes them absolute); "abs-x"/"abs-y" is rejected by QMP.
    execute({"execute": "input-send-event", "arguments": {"events": [
        {"type": "abs", "data": {"axis": "x", "value": x}},
        {"type": "abs", "data": {"axis": "y", "value": y}}]}})
elif cmd == "click":
    execute({"execute": "input-send-event", "arguments": {"events": [
        {"type": "btn", "data": {"button": "left", "down": True}}]}})
    execute({"execute": "input-send-event", "arguments": {"events": [
        {"type": "btn", "data": {"button": "left", "down": False}}]}})
else:
    raise SystemExit("unknown qmp command: %s" % cmd)
PYEOF
qmp() { python3 "$WORK/qmp.py" "$QMP_SOCK" "$@" || fail "QMP command failed: $*"; }

# --- Host prerequisites -----------------------------------------------------
[ -x "$COMPOSITOR_BIN" ] || fail "compositor binary not found: $COMPOSITOR_BIN (run cargo build --release --locked in compositor/ first)"
[ -f "$CLIENT_SRC" ] || fail "test client source not found: $CLIENT_SRC"
[ -f "$GUEST_SCRIPT_SRC" ] || fail "guest script not found: $GUEST_SCRIPT_SRC"
for tool in qemu-system-x86_64 qemu-img genisoimage python3 ssh scp ssh-keygen; do
    command -v "$tool" >/dev/null || fail "host tool not found: $tool"
done
[ -e /dev/kvm ] || fail "no /dev/kvm — the standalone check needs the KVM box (no KVM/DRM device here)"
if (exec 3<>/dev/tcp/127.0.0.1/"$SSH_PORT") 2>/dev/null; then
    exec 3>&- 3<&-
    fail "something already listens on 127.0.0.1:$SSH_PORT (stale QEMU? set SEVYN_SSH_PORT)"
fi
mkdir -p "$CHECK_DIR"

# --- Guest disk --------------------------------------------------------------
if [ ! -f "$BASE_QCOW2" ]; then
    RAW_IMG="$CHECK_DIR/noble-server-cloudimg-amd64.img"
    if [ ! -f "$RAW_IMG" ]; then
        echo "standalone-check: downloading Ubuntu 24.04 cloud image (one-time)..."
        curl -fL -o "$RAW_IMG" https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.img \
            || fail "cloud image download failed"
    fi
    qemu-img convert -O qcow2 "$RAW_IMG" "$BASE_QCOW2" \
        || fail "qemu-img convert failed"
    qemu-img resize "$BASE_QCOW2" 8G || fail "qemu-img resize failed"
fi
# --- Guests ------------------------------------------------------------------
# boot_guest <mode>: boot one guest and set it up. mode is `virtio`
# (virtio-gpu DRM, for the GLES run) or `vkms` (no virtual GPU at all;
# the guest loads the vkms kernel driver, whose plane advertises LINEAR
# — the Pixman run needs it, see the header). Sets SSH_PORT, SSH_OPTS,
# QMP_SOCK, and QEMU_PID_FILE for the current guest; each guest gets
# its own overlay disk, SSH port, QMP socket, and log files.
boot_guest() {
    local mode="$1"
    local port qemu_devices setup_arg
    case "$mode" in
    virtio)
        port="${SEVYN_SSH_PORT:-2222}"
        qemu_devices="-device virtio-gpu-pci -device virtio-tablet-pci -device virtio-rng-pci"
        setup_arg="setup"
        ;;
    vkms)
        port="$(( ${SEVYN_SSH_PORT:-2222} + 1 ))"
        qemu_devices="-device virtio-tablet-pci -device virtio-rng-pci"
        setup_arg="setup vkms"
        ;;
    *) fail "unknown guest mode: $mode" ;;
    esac
    set_guest_port "$port"
    QMP_SOCK="$WORK/qmp-$mode.sock"
    QEMU_PID_FILE="$WORK/qemu-$mode.pid"
    if (exec 3<>/dev/tcp/127.0.0.1/"$SSH_PORT") 2>/dev/null; then
        exec 3>&- 3<&-
        fail "something already listens on 127.0.0.1:$SSH_PORT (stale QEMU?)"
    fi
    # Per-run overlay so runs are repeatable and the base stays pristine.
    qemu-img create -f qcow2 -b "$BASE_QCOW2" -F qcow2 "$WORK/guest-$mode.qcow2" 16G \
        || fail "guest overlay disk creation failed ($mode)"
    # virtio-gpu (2D, no virgl — see the header), virtio-tablet for the
    # absolute pointer QMP injects, user networking with SSH forwarded
    # to the host. The vkms guest has no GPU device at all.
    # shellcheck disable=SC2086
    qemu-system-x86_64 -machine q35,accel=kvm -cpu host -m "$GUEST_MEM" -smp "$GUEST_CPUS" \
        -drive file="$WORK/guest-$mode.qcow2",if=virtio,format=qcow2 \
        -drive file="$WORK/seed.iso",format=raw,if=ide,media=cdrom,readonly=on \
        $qemu_devices \
        -netdev user,id=net0,hostfwd=tcp:127.0.0.1:"$SSH_PORT"-:22 \
        -device virtio-net-pci,netdev=net0 \
        -display none -serial file:"$WORK/guest-serial-$mode.log" \
        -qmp unix:"$QMP_SOCK",server,nowait \
        >"$WORK/qemu-$mode.log" 2>&1 &
    echo $! >"$QEMU_PID_FILE"
    QEMU_PID="$(cat "$QEMU_PID_FILE")"
    sleep 2
    kill -0 "$QEMU_PID" 2>/dev/null || fail "QEMU failed to start ($mode, see $WORK/qemu-$mode.log)"

    echo "standalone-check: waiting for guest SSH ($mode, up to 300 s)..."
    deadline=$((SECONDS + 300))
    while ! ssh_guest true 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do sleep 5; done
    ssh_guest true 2>/dev/null || fail "guest SSH never came up ($mode)"

    echo "standalone-check: waiting for cloud-init to finish ($mode, up to 660 s)..."
    timeout 660 ssh $SSH_OPTS sevyn@127.0.0.1 "sudo cloud-init status --wait" 2>/dev/null \
        || fail "cloud-init did not finish ($mode; guest package install incomplete?)"

    # --- Sync the tree's test inputs into the guest ---------------------------
    scp_to_guest "$COMPOSITOR_BIN" "sevyn@127.0.0.1:/tmp/sevyn-compositor" \
        || fail "scp compositor binary failed ($mode)"
    scp_to_guest "$CLIENT_SRC" "sevyn@127.0.0.1:/tmp/test-client.c" \
        || fail "scp test client source failed ($mode)"
    scp_to_guest "$GUEST_SCRIPT_SRC" "sevyn@127.0.0.1:/tmp/standalone-guest.sh" \
        || fail "scp guest script failed ($mode)"
    ssh_guest "sudo chmod +x /tmp/standalone-guest.sh /tmp/sevyn-compositor" \
        || fail "chmod in guest failed ($mode)"
    echo "standalone-check: guest setup ($mode: seatd + test client build)..."
    ssh_guest "sudo /tmp/standalone-guest.sh $setup_arg" \
        || fail "guest setup failed ($mode)"
}

# stop_guest: shut down the current guest's QEMU (best effort; the
# EXIT trap kills any stragglers).
stop_guest() {
    [ -n "${QEMU_PID_FILE:-}" ] && [ -f "$QEMU_PID_FILE" ] || return 0
    kill "$(cat "$QEMU_PID_FILE")" 2>/dev/null || true
    sleep 2
    kill -9 "$(cat "$QEMU_PID_FILE")" 2>/dev/null || true
    QEMU_PID_FILE=""
}

# --- Seed ISO (cloud-init NoCloud) -------------------------------------------
ssh-keygen -t ed25519 -f "$SSH_KEY" -N "" -C "sevyn-standalone-check" >/dev/null \
    || fail "ssh-keygen failed"
mkdir -p "$WORK/seed"
cat >"$WORK/seed/meta-data" <<EOF
instance-id: sevyn-standalone-check
local-hostname: sevyn-standalone-check
EOF
cat >"$WORK/seed/user-data" <<EOF
#cloud-config
hostname: sevyn-standalone-check
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
EOF
genisoimage -output "$WORK/seed.iso" -volid cidata -joliet -rock "$WORK/seed/user-data" "$WORK/seed/meta-data" >/dev/null 2>&1 \
    || fail "seed ISO creation failed"

# --- Guest 1: virtio-gpu (GLES run, milestones 1–8) ---------------------------
boot_guest virtio

# Milestone 1 (no parent compositor) is asserted in the guest before the
# first launch: no Weston, Xorg, or Xvfb process, no DISPLAY.
GUEST_NO_PARENT="$(ssh_guest "sudo /tmp/standalone-guest.sh status")"
echo "$GUEST_NO_PARENT" | grep -q "weston: *$" || fail "a weston process is running in the guest: $GUEST_NO_PARENT"
echo "$GUEST_NO_PARENT" | grep -q "Xorg: *$" || fail "an Xorg process is running in the guest: $GUEST_NO_PARENT"
echo "$GUEST_NO_PARENT" | grep -q "DISPLAY=unset" || fail "DISPLAY is set in the guest: $GUEST_NO_PARENT"
echo "standalone-check: milestone 1 (no parent compositor) — guest has no Weston/X, DISPLAY unset"

# --- Phase 1: GLES run — milestones 2–8 ---------------------------------------
echo "standalone-check: phase 1 — GLES run (Shell + app)..."
ssh_guest "sudo /tmp/standalone-guest.sh start-compositor gles $SOCKET_NAME" \
    || fail "could not start compositor (GLES run)"
wait_for_guest /tmp/compositor.log "SEVYN READY" "compositor READY (GLES)"
wait_for_guest /tmp/compositor.log "SEVYN FIRST FRAME PRESENTED" "first presented frame (GLES)"
fetch_guest /tmp/compositor.log "$WORK/compositor-gles.log"
READY_LINE="$(grep "SEVYN READY" "$WORK/compositor-gles.log" | head -1)"
echo "$READY_LINE" | grep -q "backend=drm" || fail "READY marker is not backend=drm: $READY_LINE"
echo "$READY_LINE" | grep -q "renderer=gles" || fail "READY marker is not renderer=gles: $READY_LINE"
OUT_W="$(echo "$READY_LINE" | grep -o "output=[0-9]*x[0-9]*" | cut -d= -f2 | cut -dx -f1)"
OUT_H="$(echo "$READY_LINE" | grep -o "output=[0-9]*x[0-9]*" | cut -d= -f2 | cut -dx -f2)"
SOCKET_UP_MS="$(echo "$READY_LINE" | grep -o "socket_up_ms=[0-9]*" | cut -d= -f2)"
FIRST_FRAME_MS="$(grep "SEVYN FIRST FRAME PRESENTED" "$WORK/compositor-gles.log" | head -1 | grep -o "first_frame_ms=[0-9]*" | cut -d= -f2)"
[ -n "$OUT_W" ] && [ -n "$OUT_H" ] && [ -n "$SOCKET_UP_MS" ] && [ -n "$FIRST_FRAME_MS" ] \
    || fail "could not parse READY / FIRST FRAME markers: $READY_LINE"
echo "standalone-check: milestone 2 — one DRM output ${OUT_W}x${OUT_H} (native mode, from READY)"
echo "standalone-check: startup: socket up ${SOCKET_UP_MS} ms (budget 2000), first frame ${FIRST_FRAME_MS} ms (budget 3000)"

ssh_guest "sudo /tmp/standalone-guest.sh start-clients $SOCKET_NAME" \
    || fail "could not start Shell + app clients"
wait_for_guest /tmp/client-shell.log "M1CLIENT READY" "Shell client READY"
wait_for_guest /tmp/client-app.log "M1CLIENT READY" "app client READY"
wait_for_guest /tmp/client-shell.log "PRESENTATION presented" "Shell presentation feedback"
wait_for_guest /tmp/client-app.log "PRESENTATION presented" "app presentation feedback"
fetch_guest /tmp/compositor.log "$WORK/compositor-gles.log"
fetch_guest /tmp/client-shell.log "$WORK/client-shell.log"
fetch_guest /tmp/client-app.log "$WORK/client-app.log"
assert_grep "$WORK/compositor-gles.log" "SEVYN CLIENT CONNECTED pid="
assert_grep "$WORK/compositor-gles.log" "SEVYN SURFACE CREATED id=1 session=runtime:"
assert_grep "$WORK/compositor-gles.log" "SEVYN SURFACE CREATED id=2 session=runtime:"
assert_grep "$WORK/compositor-gles.log" "SEVYN SESSION ASSOCIATED session=runtime:.* surface=1"
assert_grep "$WORK/compositor-gles.log" "SEVYN SESSION ASSOCIATED session=runtime:.* surface=2"
# The two sessions must be distinct (association happens before the
# client sets its app_id, so the ASSOCIATED line itself carries
# app_id=(none); the METADATA lines below carry the real app_ids).
SHELL_SESSION="$(grep "SESSION ASSOCIATED" "$WORK/compositor-gles.log" | grep "surface=1" | head -1 | grep -o "session=[^ ]*" | cut -d= -f2)"
APP_SESSION="$(grep "SESSION ASSOCIATED" "$WORK/compositor-gles.log" | grep "surface=2" | head -1 | grep -o "session=[^ ]*" | cut -d= -f2)"
[ -n "$SHELL_SESSION" ] && [ -n "$APP_SESSION" ] && [ "$SHELL_SESSION" != "$APP_SESSION" ] \
    || fail "Shell and app sessions are not two distinct sessions ($SHELL_SESSION / $APP_SESSION)"
assert_grep "$WORK/compositor-gles.log" "SEVYN SURFACE METADATA id=1 app_id=sevyn.shell title=Sevyn Shell"
assert_grep "$WORK/compositor-gles.log" "SEVYN SURFACE METADATA id=2 app_id=sevyn.app title=Sevyn App"
assert_grep "$WORK/compositor-gles.log" "SEVYN SURFACE COMMITTED id=1 size=800x300"
assert_grep "$WORK/compositor-gles.log" "SEVYN SURFACE COMMITTED id=2 size=800x300"
assert_grep "$WORK/compositor-gles.log" "SEVYN FOCUS KEYBOARD surface=2"
assert_grep "$WORK/client-shell.log" "PRESENTATION presented"
assert_grep "$WORK/client-app.log" "PRESENTATION presented"
echo "standalone-check: milestones 4–6 — Shell (id=1, sevyn.shell) + app (id=2, sevyn.app) up, both committed, presentation feedback for both"

# Steady-state RSS with Shell + app up and idle (budget ≤ 200 MB).
sleep 2
RSS_KIB="$(ssh_guest "sudo /tmp/standalone-guest.sh rss" | tr -d ' ')"
[ -n "$RSS_KIB" ] && [ "$RSS_KIB" -gt 0 ] 2>/dev/null \
    || fail "could not read compositor RSS (got: $RSS_KIB)"
RSS_MB="$(awk "BEGIN {printf \"%.1f\", $RSS_KIB / 1024}")"
echo "standalone-check: memory: steady-state RSS ${RSS_MB} MB (${RSS_KIB} KiB, budget 200 MB)"

# Milestone 3 + 7 — input. Windows cascade 48 px (place_window): Shell
# at (0,0), app at (48,48), each 800x300. Shell-only point (20,20);
# app-only point: the x-strip past the Shell's right edge when the
# output is wide enough, else the y-strip below the Shell's bottom.
SHELL_X=20; SHELL_Y=20
if [ "$OUT_W" -gt 848 ]; then APP_X=824; APP_Y=150;
elif [ "$OUT_H" -gt 348 ]; then APP_X=400; APP_Y=324;
else fail "output ${OUT_W}x${OUT_H} too small to distinguish the two windows"; fi
abs_for() { # <logical x> <logical y> -> QMP 0..32767 values
    echo "$(( $1 * 32767 / OUT_W )) $(( $2 * 32767 / OUT_H ))"
}
SHELL_ABS="$(abs_for $SHELL_X $SHELL_Y)"
APP_ABS="$(abs_for $APP_X $APP_Y)"
# Pointer to the Shell-only point, click, key 'a' (evdev 30).
qmp abs $SHELL_ABS
sleep 0.5
wait_for_guest /tmp/compositor.log "SEVYN POINTER FOCUS surface=1" "pointer focus on Shell"
qmp click
sleep 0.5
wait_for_guest /tmp/client-shell.log "PTR BUTTON .* button=272 state=pressed" "Shell click"
qmp sendkey a
sleep 0.5
wait_for_guest /tmp/client-shell.log "KEY .* key=30 state=pressed" "Shell key 'a'"
fetch_guest /tmp/compositor.log "$WORK/compositor-gles.log"
assert_grep "$WORK/compositor-gles.log" "SEVYN POINTER BUTTON button=272 state=pressed surface=1"
assert_grep "$WORK/compositor-gles.log" "SEVYN KEY .* state=pressed focus=1"
assert_grep "$WORK/compositor-gles.log" "SEVYN FOCUS KEYBOARD surface=1"
echo "standalone-check: milestone 3 (Shell) — pointer + click + key 'a' reached the Shell surface"
# Pointer to the app-only point, click, key 'b' (evdev 48).
qmp abs $APP_ABS
sleep 0.5
wait_for_guest /tmp/compositor.log "SEVYN POINTER FOCUS surface=2" "pointer focus on app"
qmp click
sleep 0.5
wait_for_guest /tmp/client-app.log "PTR BUTTON .* button=272 state=pressed" "app click"
qmp sendkey b
sleep 0.5
wait_for_guest /tmp/client-app.log "KEY .* key=48 state=pressed" "app key 'b'"
fetch_guest /tmp/compositor.log "$WORK/compositor-gles.log"
assert_grep "$WORK/compositor-gles.log" "SEVYN POINTER BUTTON button=272 state=pressed surface=2"
assert_grep "$WORK/compositor-gles.log" "SEVYN KEY .* state=pressed focus=2"
assert_grep "$WORK/compositor-gles.log" "SEVYN FOCUS KEYBOARD surface=2"
echo "standalone-check: milestones 3+7 (app) — pointer + click + key 'b' reached the app surface after focus moved to it"

# Milestone 7 — close the app from the Shell side via the bridge
# (APP_SESSION was extracted with the association asserts above).
[ -n "$APP_SESSION" ] || fail "could not read the app session id from the compositor log"
BRIDGE_RESPONSE="$(ssh_guest "sudo /tmp/standalone-guest.sh bridge-close $APP_SESSION")"
echo "$BRIDGE_RESPONSE" | grep -q '"ok":true' \
    || fail "bridge close failed: $BRIDGE_RESPONSE"
APP_PID="$(ssh_guest "cat /tmp/client-app.pid" | tr -d ' ')"
deadline=$((SECONDS + TIMEOUT))
while ssh_guest "sudo kill -0 $APP_PID 2>/dev/null" && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.5; done
ssh_guest "sudo kill -0 $APP_PID 2>/dev/null" && fail "app client did not exit after bridge close"
wait_for_guest /tmp/compositor.log "SEVYN SESSION STOPPED session=$APP_SESSION surface=2 reason=closed" \
    "SESSION STOPPED (closed) for the app"
SHELL_PID="$(ssh_guest "cat /tmp/client-shell.pid" | tr -d ' ')"
ssh_guest "sudo kill -0 $SHELL_PID 2>/dev/null" \
    || fail "Shell client died when the app was closed (it must survive)"
echo "standalone-check: milestone 7 — bridge close exited the app, session recorded stopped, Shell survived"

# Milestone 8 — clean exit, then the guest shell must still work.
EXIT_CODE="$(ssh_guest "sudo /tmp/standalone-guest.sh stop-compositor" | tr -d ' ')"
[ "$EXIT_CODE" = "0" ] || fail "compositor exit code was $EXIT_CODE, not 0"
fetch_guest /tmp/compositor.log "$WORK/compositor-gles.log"
assert_grep "$WORK/compositor-gles.log" "SEVYN SHUTDOWN clean"
ssh_guest "sudo kill -0 $SHELL_PID 2>/dev/null" && fail "Shell client remains after compositor shutdown"
ssh_guest "echo guest-shell-ok" | grep -q "guest-shell-ok" || fail "guest shell unusable after compositor exit"
echo "standalone-check: milestone 8 — clean exit 0, SHUTDOWN clean, no orphans, guest shell usable"

# --- Guest 2: vkms (Pixman fallback + crash test) ------------------------------
# Guest 1 is done; the Pixman run needs a driver whose plane advertises
# LINEAR, so boot the vkms guest (no virtual GPU at all).
stop_guest
boot_guest vkms

# --- Phase 2: Pixman fallback -------------------------------------------------
# The fallback is attempted, not assumed: on the DRM devices available
# on this box it cannot present a frame (see the header). If the first
# frame never presents AND the log shows the known environmental cause
# ("Unsupported modifier: Invalid"), the item is recorded as NOT
# PROVEN and the check continues to prove everything else, exiting 1
# at the end. Any other failure is a hard fail — a new compositor bug
# must not hide behind the known limitation.
echo "standalone-check: phase 2 — Pixman fallback (--no-gles, vkms guest)..."
ssh_guest "sudo /tmp/standalone-guest.sh start-compositor pixman sevyn-standalone-pixman" \
    || fail "could not start compositor (Pixman run)"
wait_for_guest /tmp/compositor.log "SEVYN READY" "compositor READY (Pixman)"
if poll_guest /tmp/compositor.log "SEVYN FIRST FRAME PRESENTED"; then
    fetch_guest /tmp/compositor.log "$WORK/compositor-pixman.log"
    READY_PIXMAN="$(grep "SEVYN READY" "$WORK/compositor-pixman.log" | head -1)"
    echo "$READY_PIXMAN" | grep -q "backend=drm" || fail "Pixman READY marker is not backend=drm: $READY_PIXMAN"
    echo "$READY_PIXMAN" | grep -q "renderer=pixman" || fail "Pixman READY marker is not renderer=pixman: $READY_PIXMAN"
    ssh_guest "sudo /tmp/standalone-guest.sh start-one-client sevyn-standalone-pixman sevyn.app 'Sevyn App' /tmp/client-app.log /tmp/client-app.pid" \
        || fail "could not start app client (Pixman run)"
    wait_for_guest /tmp/client-app.log "M1CLIENT READY" "app client READY (Pixman)"
    wait_for_guest /tmp/client-app.log "PRESENTATION presented" "app presentation feedback (Pixman)"
    fetch_guest /tmp/compositor.log "$WORK/compositor-pixman.log"
    assert_grep "$WORK/compositor-pixman.log" "SEVYN SURFACE COMMITTED id=1 size=800x300"
    EXIT_CODE="$(ssh_guest "sudo /tmp/standalone-guest.sh stop-compositor" | tr -d ' ')"
    [ "$EXIT_CODE" = "0" ] || fail "Pixman run exit code was $EXIT_CODE, not 0"
    fetch_guest /tmp/compositor.log "$WORK/compositor-pixman.log"
    assert_grep "$WORK/compositor-pixman.log" "SEVYN SHUTDOWN clean"
    echo "standalone-check: Pixman fallback — renderer=pixman, surface committed, frame presented, clean exit 0 (vkms guest)"
else
    fetch_guest /tmp/compositor.log "$WORK/compositor-pixman.log"
    # NB: the "first frame failed" diag lines go to the compositor's
    # own log (/tmp/genesis.log), not its stdout (/tmp/compositor.log,
    # which carries only the SEVYN markers) — grep the right file.
    fetch_guest /tmp/genesis.log "$WORK/genesis-pixman.log"
    if grep -q "first frame failed: Unsupported modifier: Invalid" "$WORK/genesis-pixman.log"; then
        PIXMAN_NOT_PROVEN=1
        echo "standalone-check: NOT PROVEN — Pixman fallback cannot present on this DRM stack:"
        echo "standalone-check:   vkms advertises LINEAR, but Mesa GBM reports every buffer it creates as"
        echo "standalone-check:   modifier Invalid (direct gbm-probe on the KVM box: v1/v2/plain+LINEAR all"
        echo "standalone-check:   report Invalid; v1[Invalid] errors outright), and Smithay's Pixman"
        echo "standalone-check:   renderer refuses to bind anything but LINEAR. virtio-gpu is worse: its"
        echo "standalone-check:   plane advertises no explicit modifiers at all. This is a property of the"
        echo "standalone-check:   virtual drivers + Mesa GBM here, not of the compositor — the compositor"
        echo "standalone-check:   negotiates LINEAR when the plane offers it and the renderer initializes"
        echo "standalone-check:   (READY renderer=pixman was reached). Real-GPU proof is owed (GF65)."
        echo "standalone-check:   continuing to prove everything else; the check exits 1 at the end."
        EXIT_CODE="$(ssh_guest "sudo /tmp/standalone-guest.sh stop-compositor" | tr -d ' ')"
        [ "$EXIT_CODE" = "0" ] || fail "Pixman run exit code was $EXIT_CODE, not 0 (after NOT PROVEN)"
        fetch_guest /tmp/compositor.log "$WORK/compositor-pixman.log"
        assert_grep "$WORK/compositor-pixman.log" "SEVYN SHUTDOWN clean"
    else
        fail "Pixman first frame never presented, and not for the known environmental reason (see $LAST_RUN/compositor-pixman.log and $LAST_RUN/genesis-pixman.log)"
    fi
fi

# --- Phase 3: crash test (milestone 10) ----------------------------------------
echo "standalone-check: phase 3 — crash test (SEVYN_TEST_PANIC=1, vkms guest)..."
ssh_guest "sudo /tmp/standalone-guest.sh start-compositor panic sevyn-standalone-crash" \
    || fail "could not start compositor (crash run)"
wait_for_guest /tmp/compositor.log "SEVYN READY" "compositor READY (crash run)"
wait_for_guest_file /tmp/compositor.exit "crash exit code"
CRASH_EXIT="$(ssh_guest "cat /tmp/compositor.exit" | tr -d ' ')"
[ "$CRASH_EXIT" = "101" ] || fail "crash run exit code was $CRASH_EXIT, not 101"
fetch_guest /tmp/compositor.log "$WORK/compositor-crash.log"
assert_grep "$WORK/compositor-crash.log" "SEVYN CRASH panic=SEVYN_TEST_PANIC=1"
fetch_guest /tmp/genesis.log "$WORK/genesis-crash.log"
assert_grep "$WORK/genesis-crash.log" "SEVYN CRASH panic=SEVYN_TEST_PANIC=1"
assert_grep "$WORK/genesis-crash.log" "SEVYN READY"
# NB: the bracket trick — the regex matches the compositor's cmdline
# but not this ssh command's own cmdline (which contains the literal
# "[r]"), so a self-match cannot false-positive here.
ssh_guest "pgrep -f 'sevyn-composito[r]'" >/dev/null 2>&1 && fail "compositor process remains after the panic"
echo "standalone-check: milestone 10 — SEVYN CRASH marker + /tmp/genesis.log entry, exit 101"
# DRM master released after the crash: a second launch must reach READY.
ssh_guest "sudo /tmp/standalone-guest.sh start-compositor gles $SOCKET_NAME" \
    || fail "could not start compositor (second launch after crash)"
wait_for_guest /tmp/compositor.log "SEVYN READY" "compositor READY (second launch after crash)"
EXIT_CODE="$(ssh_guest "sudo /tmp/standalone-guest.sh stop-compositor" | tr -d ' ')"
[ "$EXIT_CODE" = "0" ] || fail "second-launch exit code was $EXIT_CODE, not 0"
echo "standalone-check: milestone 8 (crash) — second launch reached READY after the crash (DRM master released), clean exit 0"
stop_guest

# --- Phase 4: startup + shutdown documented (milestone 9) ----------------------
echo "standalone-check: phase 4 — documentation matches what was exercised..."
for needle in "--standalone" "SEVYN READY" "socket_up_ms" "SEVYN SHUTDOWN clean" "101" "NOT PROVEN"; do
    grep -q -- "$needle" compositor/README.md || fail "README.md does not document: $needle"
done
for needle in "SEVYN CRASH" "SEVYN_TEST_PANIC" "101"; do
    grep -q -- "$needle" compositor/RECOVERY.md || fail "RECOVERY.md does not document: $needle"
done
"$COMPOSITOR_BIN" --bogus >/dev/null 2>&1
USAGE_EXIT=$?
[ "$USAGE_EXIT" = "2" ] || fail "usage error exit code was $USAGE_EXIT, not 2"
# Hardware cursor: code-path only headless — the built-in default arrow
# element is in the render path (render.rs), but virtio-gpu exposes no
# cursor plane to observe, so plane placement is NOT proven here.
grep -q "default_cursor_element" compositor/src/render.rs \
    || fail "default cursor element missing from render.rs"
grep -q "Kind::Cursor" compositor/src/render.rs \
    || fail "cursor element is not marked Kind::Cursor in render.rs"
echo "standalone-check: milestone 9 — README/RECOVERY.md describe the exercised CLI, markers, and exit codes (0/2/101 observed; 1 is the documented no-DRM fatal)"

# --- Measurements + PASS ---------------------------------------------------------
{
    echo "standalone-check measurements (QEMU virtio-gpu, KVM box 45.77.162.169, $(date -u +%Y-%m-%dT%H:%M:%SZ))"
    echo "output=${OUT_W}x${OUT_H}"
    echo "steady_state_rss_kib=$RSS_KIB"
    echo "steady_state_rss_mb=$RSS_MB"
    echo "socket_up_ms=$SOCKET_UP_MS"
    echo "first_frame_ms=$FIRST_FRAME_MS"
    if [ -n "${PIXMAN_NOT_PROVEN:-}" ]; then
        echo "pixman=NOT_PROVEN (Unsupported modifier: Invalid — Mesa GBM reports every buffer Invalid on this box's virtual DRM devices; real-GPU proof owed)"
    else
        echo "pixman=proven"
    fi
} >"$WORK/measurements.txt"
preserve_logs
if [ -n "${PIXMAN_NOT_PROVEN:-}" ]; then
    echo "standalone-check: NOT PROVEN — Pixman fallback (see above + measurements.txt); everything else passed"
    echo "standalone-check: run logs preserved in $LAST_RUN"
    exit 1
fi
echo "standalone-check: PASS — all Stage 3 milestones proven"
echo "standalone-check: output ${OUT_W}x${OUT_H} backend=drm renderer=gles; Shell + app surfaces, input routed, close + clean shutdown, Pixman fallback (vkms guest), crash log + second launch"
echo "standalone-check: MEASUREMENTS rss=${RSS_MB} MB (budget 200) socket_up=${SOCKET_UP_MS} ms (budget 2000) first_frame=${FIRST_FRAME_MS} ms (budget 3000)"
echo "standalone-check: run logs preserved in $LAST_RUN"
