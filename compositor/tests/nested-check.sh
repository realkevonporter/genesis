#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# nested-check.sh — the Stage 2 nested-compositor check.
#
# Boots the nested compositor under a headless parent compositor and proves
# the Stage 2 milestones with a real Wayland client:
#
#   1. the compositor launches inside an existing Wayland session and
#      creates one nested output window (SEVYN READY);
#   2. a client connects and creates an XDG surface (M1CLIENT READY);
#   3. the compositor associates the surface with a Runtime session
#      (SESSION ASSOCIATED) and records its metadata (SURFACE METADATA);
#   4. the surface presents a buffer and a frame is presented
#      (SURFACE COMMITTED, FIRST FRAME PRESENTED);
#   5. pointer input reaches the surface actually under the pointer
#      (client PTR ENTER A/B) and a click + key reach the focused surface
#      (client PTR BUTTON / KEY, compositor POINTER BUTTON / KEY);
#   6. the Shell-side close works: a bridge `close` for the session makes
#      the client exit and the session is recorded stopped
#      (SESSION STOPPED reason=closed);
#   7. the compositor exits cleanly on SIGTERM (SHUTDOWN clean) with no
#      application processes left behind.
#
# What this check proves: function, not fps. The GF65 is the fps proof
# (COMPOSITOR_PLAN.md, "60 fps bar, GF65 is the proof"); QEMU and this
# headless check cannot prove it.
#
# The parent compositor is headless Weston (development infrastructure
# only — see the plan's "The decision"). Weston itself needs a display
# server, so it runs on its X11 backend under a virtual X server (Xvfb);
# input is injected into that X server with xdotool and flows
# X -> Weston -> winit -> Genesis -> client, which is exactly the path
# the milestones require.
#
# Usage:
#   compositor/tests/nested-check.sh
#
# Environment (all optional):
#   SEVYN_COMPOSITOR_BIN  path to sevyn-compositor
#                          (default: compositor/target/debug/sevyn-compositor)
#   SEVYN_TEST_CLIENT_SRC path to test-client.c (default: compositor/tests/test-client.c)
#   DISPLAY_NUMBER        X display number for Xvfb (default: 98)
#   NESTED_TIMEOUT        seconds per wait step (default: 30)
#
# Exit 0 when every milestone is proven, non-zero otherwise. All
# processes are killed on exit (success or failure).

set -u
cd "$(dirname "$0")/../.." || exit 1

COMPOSITOR_BIN="${SEVYN_COMPOSITOR_BIN:-compositor/target/debug/sevyn-compositor}"
CLIENT_SRC="${SEVYN_TEST_CLIENT_SRC:-compositor/tests/test-client.c}"
DISPLAY_NUMBER="${DISPLAY_NUMBER:-98}"
TIMEOUT="${NESTED_TIMEOUT:-30}"

WORK="$(mktemp -d /tmp/sevyn-nested-check.XXXXXX)"
RUNTIME_DIR="$WORK/run"
mkdir -p "$RUNTIME_DIR"
COMPOSITOR_LOG="$WORK/compositor.log"
COMPOSITOR_ERR="$WORK/compositor.err"
WESTON_LOG="$WORK/weston.log"
XVFB_LOG="$WORK/xvfb.log"
CLIENT_LOG="$WORK/client.log"
# The test client hard-codes its log path (see test-client.c).
CLIENT_HARDCODED_LOG="/tmp/sevyn-m1-client.log"

fail() {
    echo "nested-check: FAIL: $*" >&2
    echo "nested-check: --- compositor.log ---" >&2
    cat "$COMPOSITOR_LOG" >&2 || true
    echo "nested-check: --- compositor.err ---" >&2
    tail -20 "$COMPOSITOR_ERR" >&2 || true
    echo "nested-check: --- weston.log ---" >&2
    tail -20 "$WESTON_LOG" >&2 || true
    echo "nested-check: --- client log ---" >&2
    cat "$CLIENT_HARDCODED_LOG" >&2 || true
    exit 1
}

cleanup() {
    # Kill by pid file where we have one; pkill patterns are kept exact
    # so the cleanup never matches this script's own command line.
    for pid_file in "$WORK"/xvfb.pid "$WORK"/weston.pid "$WORK"/compositor.pid "$WORK"/client.pid; do
        if [ -f "$pid_file" ]; then
            kill "$(cat "$pid_file")" 2>/dev/null || true
        fi
    done
    # The client is started by path; match that exact path only.
    # (NB: pgrep/pkill -x cannot match it — "sevyn-test-client" is 17
    # chars, longer than the kernel's 15-char comm — so match the full
    # command line, with the bracket trick so pkill cannot match the
    # shell running this cleanup.)
    pkill -f "sevyn-test-clien[t]" 2>/dev/null || true
    sleep 1
    for pid_file in "$WORK"/xvfb.pid "$WORK"/weston.pid "$WORK"/compositor.pid; do
        if [ -f "$pid_file" ]; then
            kill -9 "$(cat "$pid_file")" 2>/dev/null || true
        fi
    done
    rm -rf "$WORK"
}
trap cleanup EXIT

# wait_for <file> <pattern> [description]: poll until the pattern appears.
wait_for() {
    local file="$1" pattern="$2" what="${3:-$pattern}"
    local deadline=$((SECONDS + TIMEOUT))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if [ -f "$file" ] && grep -q -- "$pattern" "$file"; then
            return 0
        fi
        sleep 0.2
    done
    fail "timed out waiting for $what (pattern: $pattern)"
}

assert_marker() {
    grep -q -- "$1" "$COMPOSITOR_LOG" || fail "missing compositor marker: $1"
}

[ -x "$COMPOSITOR_BIN" ] || fail "compositor binary not found: $COMPOSITOR_BIN (run cargo build first)"
command -v Xvfb >/dev/null || fail "Xvfb not found"
command -v weston >/dev/null || fail "weston not found"
command -v xdotool >/dev/null || fail "xdotool not found"
command -v python3 >/dev/null || fail "python3 not found"
command -v gcc >/dev/null || fail "gcc not found"
command -v wayland-scanner >/dev/null || fail "wayland-scanner not found"

# --- Build the test client -----------------------------------------------
# xdg-shell.xml comes from the installed wayland-protocols package; fall
# back to the copy vendored in the wayland-protocols crate source (the
# sandbox/CI may not have the -dev package installed). The client also
# needs presentation-time.xml (Stage 3: it logs the `presented` event);
# same lookup for both.
XDG_SHELL_XML="/usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml"
PRESENTATION_XML="/usr/share/wayland-protocols/stable/presentation-time/presentation-time.xml"
if [ ! -f "$XDG_SHELL_XML" ]; then
    XDG_SHELL_XML="$(ls ~/.cargo/registry/src/*/wayland-protocols-*/protocols/stable/xdg-shell/xdg-shell.xml 2>/dev/null | head -1)"
    PRESENTATION_XML="$(ls ~/.cargo/registry/src/*/wayland-protocols-*/protocols/stable/presentation-time/presentation-time.xml 2>/dev/null | head -1)"
fi
[ -n "$XDG_SHELL_XML" ] && [ -f "$XDG_SHELL_XML" ] || fail "xdg-shell.xml not found"
[ -n "$PRESENTATION_XML" ] && [ -f "$PRESENTATION_XML" ] || fail "presentation-time.xml not found"
wayland-scanner client-header "$XDG_SHELL_XML" "$WORK/xdg-shell-client-protocol.h" \
    || fail "wayland-scanner client-header failed"
wayland-scanner private-code "$XDG_SHELL_XML" "$WORK/xdg-shell-protocol.c" \
    || fail "wayland-scanner private-code failed"
wayland-scanner client-header "$PRESENTATION_XML" "$WORK/presentation-time-client-protocol.h" \
    || fail "wayland-scanner presentation client-header failed"
wayland-scanner private-code "$PRESENTATION_XML" "$WORK/presentation-time-protocol.c" \
    || fail "wayland-scanner presentation private-code failed"
gcc -o "$WORK/sevyn-test-client" "$CLIENT_SRC" "$WORK/xdg-shell-protocol.c" "$WORK/presentation-time-protocol.c" \
    -I"$WORK" $(pkg-config --cflags --libs wayland-client) \
    || fail "test client build failed"

# --- Parent stack: Xvfb -> Weston (headless, x11 backend) -----------------
export DISPLAY=":$DISPLAY_NUMBER"
export XDG_RUNTIME_DIR="$RUNTIME_DIR"
Xvfb ":$DISPLAY_NUMBER" -screen 0 1280x800x24 >"$XVFB_LOG" 2>&1 &
echo $! >"$WORK/xvfb.pid"
sleep 2
kill -0 "$(cat "$WORK/xvfb.pid")" 2>/dev/null || fail "Xvfb failed to start (see $XVFB_LOG)"

weston --backend=x11-backend.so --socket=wayland-1 --width=1280 --height=800 \
    >"$WESTON_LOG" 2>&1 &
echo $! >"$WORK/weston.pid"
# The socket is a file, so check for it directly.
deadline=$((SECONDS + TIMEOUT))
while [ ! -S "$RUNTIME_DIR/wayland-1" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
[ -S "$RUNTIME_DIR/wayland-1" ] || fail "weston socket never appeared (see $WESTON_LOG)"

# --- The nested compositor -------------------------------------------------
WAYLAND_DISPLAY=wayland-1 "$COMPOSITOR_BIN" --socket sevyn-nested-test --size 1280x800 \
    >"$COMPOSITOR_LOG" 2>"$COMPOSITOR_ERR" &
echo $! >"$WORK/compositor.pid"
COMPOSITOR_PID="$(cat "$WORK/compositor.pid")"
wait_for "$COMPOSITOR_LOG" "SEVYN READY socket=sevyn-nested-test" "compositor READY"
wait_for "$COMPOSITOR_LOG" "SEVYN FIRST FRAME PRESENTED" "first presented frame"

# --- The client -------------------------------------------------------------
rm -f "$CLIENT_HARDCODED_LOG"
WAYLAND_DISPLAY=sevyn-nested-test "$WORK/sevyn-test-client" >"$CLIENT_LOG" 2>&1 &
echo $! >"$WORK/client.pid"
wait_for "$CLIENT_HARDCODED_LOG" "M1CLIENT READY" "client READY"

assert_marker "SEVYN CLIENT CONNECTED pid="
assert_marker "SEVYN SURFACE CREATED id=1 session=runtime:"
assert_marker "SEVYN SESSION ASSOCIATED session=runtime:"
assert_marker "SEVYN SURFACE METADATA id=1 app_id=sevyn.m1.test title=sevyn-m1-test"
assert_marker "SEVYN SURFACE COMMITTED id=1 size=800x300"
assert_marker "SEVYN FOCUS KEYBOARD surface=1"
# Presentation-time feedback reaches the client (Stage 3): the client
# asked for feedback on its first commit and got a `presented` event.
grep -q "PRESENTATION presented" "$CLIENT_HARDCODED_LOG" \
    || fail "client never got presentation-time feedback (PRESENTATION presented)"

# --- Input: pointer sweep, click, key --------------------------------------
# Sweep the pointer across the output; the client must report entering one
# of its two subsurfaces (A left, B right) with surface-local coordinates.
# The nested window's position inside Weston is Weston's choice, so the
# sweep covers the whole output instead of assuming a position.
sweep_hit=""
deadline=$((SECONDS + TIMEOUT))
while [ "$SECONDS" -lt "$deadline" ] && [ -z "$sweep_hit" ]; do
    for x in 100 300 500 700 900 1100; do
        for y in 100 300 500 700; do
            xdotool mousemove "$x" "$y"
            sleep 0.1
            if grep -q "PTR ENTER" "$CLIENT_HARDCODED_LOG"; then
                sweep_hit="$x,$y"
                break 2
            fi
        done
    done
done
[ -n "$sweep_hit" ] || fail "pointer sweep never entered a client surface"
echo "nested-check: pointer entered a surface at output $sweep_hit"
assert_marker "SEVYN POINTER FOCUS surface=1"

# Click where the pointer entered, then press a key: both must reach the
# focused surface (the toplevel — Genesis focuses toplevels, not leaf
# subsurfaces; Weston focuses the leaf, see README).
xdotool mousemove "${sweep_hit%,*}" "${sweep_hit#*,}"
sleep 0.3
xdotool click 1
sleep 0.3
xdotool key a
sleep 0.5
grep -q "PTR BUTTON .* button=272 state=pressed" "$CLIENT_HARDCODED_LOG" \
    || fail "client never saw the pointer button press"
grep -q "KEY .* key=30 state=pressed" "$CLIENT_HARDCODED_LOG" \
    || fail "client never saw the key press (key=30 is 'a' as sent by Weston)"
grep -q "KEY FOCUS window" "$CLIENT_HARDCODED_LOG" \
    || fail "client never got keyboard focus on its toplevel"
assert_marker "SEVYN POINTER BUTTON button=272 state=pressed surface=1"
assert_marker "SEVYN KEY .* focus=1"

# --- Close from the Shell side, via the bridge ------------------------------
SESSION="$(grep -o "SESSION ASSOCIATED session=[^ ]*" "$COMPOSITOR_LOG" | head -1 | cut -d= -f2)"
[ -n "$SESSION" ] || fail "could not read the session id from the compositor log"
BRIDGE_RESPONSE="$(python3 - "$RUNTIME_DIR/sevyn-compositor.sock" "$SESSION" <<'PYEOF'
import json, socket, sys
path, session = sys.argv[1], sys.argv[2]
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.settimeout(5)
sock.connect(path)
sock.sendall(json.dumps({"v": 1, "op": "close", "session": session}).encode() + b"\n")
print(sock.recv(4096).decode().strip())
PYEOF
)"
echo "$BRIDGE_RESPONSE" | grep -q '"ok":true' \
    || fail "bridge close failed: $BRIDGE_RESPONSE"

deadline=$((SECONDS + TIMEOUT))
while kill -0 "$(cat "$WORK/client.pid")" 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do
    sleep 0.2
done
kill -0 "$(cat "$WORK/client.pid")" 2>/dev/null && fail "client did not exit after bridge close"
wait_for "$COMPOSITOR_LOG" "SEVYN SESSION STOPPED session=$SESSION surface=1 reason=closed" \
    "SESSION STOPPED (closed)"
wait_for "$COMPOSITOR_LOG" "SEVYN CLIENT DISCONNECTED pid=" "client disconnect"

# --- Clean shutdown, no orphans ---------------------------------------------
kill -TERM "$COMPOSITOR_PID"
deadline=$((SECONDS + TIMEOUT))
while kill -0 "$COMPOSITOR_PID" 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do
    sleep 0.2
done
kill -0 "$COMPOSITOR_PID" 2>/dev/null && fail "compositor did not exit on SIGTERM"
wait "$COMPOSITOR_PID"
COMPOSITOR_STATUS=$?
[ "$COMPOSITOR_STATUS" -eq 0 ] || fail "compositor exited with status $COMPOSITOR_STATUS, not 0"
assert_marker "SEVYN SHUTDOWN clean"
if pgrep -f "sevyn-test-clien[t]" >/dev/null 2>&1; then
    fail "test client processes remain after compositor shutdown"
fi
if pgrep -f "sevyn-compositor --socket sevyn-nested-test" >/dev/null 2>&1; then
    fail "compositor processes remain after shutdown"
fi

echo "nested-check: PASS — all Stage 2 milestones proven"
echo "nested-check: output created, surface accepted + associated, input routed, close + clean shutdown, no orphans"

# --- Stage 4 adoption (adoption-check.sh, nested backend) -------------------
# The Stage 4 harness boots its own parent stack (Xvfb :99 + Weston +
# nested compositor) and proves the adoption milestones. It fails on a
# Stage 3 tree by design (no adopt op yet) — see adoption-check.sh.
compositor/tests/adoption-check.sh || fail "adoption check (nested) failed"
echo "nested-check: PASS — Stage 4 adoption milestones proven (nested)"
