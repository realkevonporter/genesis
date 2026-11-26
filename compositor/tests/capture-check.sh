#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# capture-check.sh — the screen-capture (`capture` op) check.
#
# Boots the nested compositor under a headless parent compositor and
# proves the bridge `capture` op with a real Wayland client:
#
#   C1. capture writes a valid PNG with the compositor's ACTUAL pixels
#       (the test client's known solid colours at known positions:
#       subsurface A red at (100,100), B blue at (600,100), the empty
#       desktop's clear colour at (1000,700));
#   C2. a second capture overwrites the same single file (identical
#       bytes for an unchanged scene; different bytes once the scene
#       changes — the client is closed and the window is gone — and
#       still exactly one file: no history, S4);
#   C3. a sensitive layer stand-in (namespace `sevyn-lock`, the lock
#       screen's shape) visible → the capture is refused with
#       "refused: sensitive surface", never the sensitive pixels, and
#       the last good frame is untouched;
#   C4. a sensitive toplevel stand-in (app_id `sevyn.lock`, title
#       "Lock Screen") → the same refusal, never its pixels;
#   C5. malformed capture requests (bad JSON, wrong version, unknown
#       op, relative output path) leave the compositor alive and the
#       last good frame untouched.
#
# The PNG is decoded with a stdlib-only Python decoder (zlib +
# struct, all five PNG filter types) — no Pillow, no pngcheck — and
# the assertions are on IHDR dimensions and actual pixel values, not
# on file existence.
#
# What this check does NOT prove: the DRM path's pixels (no DRM
# device in CI — the DRM capture is wired through the same op and
# the same shared implementation, and its pixels are proven
# downstream by the QMP screendump oracle in the SevynOS m13 check),
# and the default output path `/data/screen/latest.png` (the CI user
# cannot write `/data` — the path override exercises the same write /
# overwrite / refusal logic, and the default path is proven on-device
# at the GF65 gate).
#
# The parent stack is the same as nested-check.sh: Xvfb -> headless
# Weston (x11 backend) -> nested compositor. No pointer input is
# needed here, so xdotool is not required.
#
# Usage:
#   compositor/tests/capture-check.sh
#
# Environment (all optional):
#   SEVYN_COMPOSITOR_BIN  path to sevyn-compositor
#                          (default: compositor/target/debug/sevyn-compositor)
#   SEVYN_TEST_CLIENT_SRC path to test-client.c (default: compositor/tests/test-client.c)
#   DISPLAY_NUMBER        X display number for Xvfb (default: 97)
#   CAPTURE_TIMEOUT       seconds per wait step (default: 30)
#
# Exit 0 when every case passes, non-zero otherwise. All processes
# are killed on exit (success or failure).

set -u
cd "$(dirname "$0")/../.." || exit 1

COMPOSITOR_BIN="${SEVYN_COMPOSITOR_BIN:-compositor/target/debug/sevyn-compositor}"
CLIENT_SRC="${SEVYN_TEST_CLIENT_SRC:-compositor/tests/test-client.c}"
DISPLAY_NUMBER="${DISPLAY_NUMBER:-97}"
TIMEOUT="${CAPTURE_TIMEOUT:-30}"

WORK="$(mktemp -d /tmp/sevyn-capture-check.XXXXXX)"
RUNTIME_DIR="$WORK/run"
mkdir -p "$RUNTIME_DIR"
COMPOSITOR_LOG="$WORK/compositor.log"
COMPOSITOR_ERR="$WORK/compositor.err"
WESTON_LOG="$WORK/weston.log"
XVFB_LOG="$WORK/xvfb.log"
CLIENT_LOG="$WORK/client.log"
CAPTURE_DIR="$WORK/capture"
mkdir -p "$CAPTURE_DIR"
CAPTURE_PNG="$CAPTURE_DIR/latest.png"
# The test client hard-codes its log path (see test-client.c).
CLIENT_HARDCODED_LOG="/tmp/sevyn-m1-client.log"
# The --layer client hard-codes its own log path (see test-client.c).
LAYER_HARDCODED_LOG="/tmp/sevyn-layer-client.log"
LOCK_CLIENT_LOG="/tmp/sevyn-lock-client.log"

PASS_COUNT=0
FAIL_COUNT=0

ledger_pass() {
    PASS_COUNT=$((PASS_COUNT + 1))
    echo "capture-check: $1: PASS"
}

ledger_fail() {
    FAIL_COUNT=$((FAIL_COUNT + 1))
    echo "capture-check: $1: FAIL: $2" >&2
}

# fail <message>: infrastructure failure — abort the whole check.
fail() {
    echo "capture-check: FAIL: $*" >&2
    echo "capture-check: --- compositor.log ---" >&2
    cat "$COMPOSITOR_LOG" >&2 || true
    echo "capture-check: --- compositor.err ---" >&2
    tail -20 "$COMPOSITOR_ERR" >&2 || true
    echo "capture-check: --- weston.log ---" >&2
    tail -20 "$WESTON_LOG" >&2 || true
    echo "capture-check: --- client log ---" >&2
    cat "$CLIENT_HARDCODED_LOG" >&2 || true
    echo "capture-check: --- layer client log ---" >&2
    cat "$LAYER_HARDCODED_LOG" >&2 || true
    echo "capture-check: --- lock client log ---" >&2
    cat "$LOCK_CLIENT_LOG" >&2 || true
    exit 1
}

cleanup() {
    for pid_file in "$WORK"/xvfb.pid "$WORK"/weston.pid "$WORK"/compositor.pid "$WORK"/client.pid "$WORK"/layer-client.pid "$WORK"/lock-client.pid; do
        if [ -f "$pid_file" ]; then
            kill "$(cat "$pid_file")" 2>/dev/null || true
        fi
    done
    # The clients are started by path; match that exact path only
    # (see nested-check.sh for why -x cannot match them).
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

[ -x "$COMPOSITOR_BIN" ] || fail "compositor binary not found: $COMPOSITOR_BIN (run cargo build first)"
command -v Xvfb >/dev/null || fail "Xvfb not found"
command -v weston >/dev/null || fail "weston not found"
command -v python3 >/dev/null || fail "python3 not found"
command -v gcc >/dev/null || fail "gcc not found"
command -v wayland-scanner >/dev/null || fail "wayland-scanner not found"
command -v sha256sum >/dev/null || fail "sha256sum not found"

# --- Bridge helper ----------------------------------------------------------
# bridge.py <socket> <raw-line>: send one request line, print the one
# reply line. The capture reply is deferred one loop iteration (the
# handler records the request, the backend resolves it after the
# next render), so the read waits up to 10 s.
cat >"$WORK/bridge.py" <<'PYEOF'
import socket
import sys

path, payload = sys.argv[1], sys.argv[2]
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.settimeout(10)
sock.connect(path)
sock.sendall(payload.encode() + b"\n")
buf = b""
while b"\n" not in buf:
    chunk = sock.recv(4096)
    if not chunk:
        print("EOF (no reply)", file=sys.stderr)
        sys.exit(1)
    buf += chunk
print(buf.decode().strip())
PYEOF

bridge() {
    python3 "$WORK/bridge.py" "$RUNTIME_DIR/sevyn-compositor.sock" "$1"
}

capture() {
    bridge "{\"v\": 1, \"op\": \"capture\", \"path\": \"$CAPTURE_PNG\"}"
}

# --- PNG decoder ------------------------------------------------------------
# pngcheck.py <path> info | pixel <x> <y> | pixel-near <x> <y> r g b tol
# Stdlib only (zlib + struct): parses IHDR, concatenates IDAT,
# decompresses, and undoes all five PNG filter types. Asserts bit
# depth 8, colour type 6 (RGBA), non-interlaced — the shape the
# compositor's `png`-crate encoder writes.
cat >"$WORK/pngcheck.py" <<'PYEOF'
import struct
import sys
import zlib


def paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    if pb <= pc:
        return b
    return c


def decode(path):
    with open(path, "rb") as f:
        data = f.read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError("not a PNG file")
    pos = 8
    width = height = bit_depth = color_type = interlace = None
    idat = b""
    while pos < len(data):
        length, ctype = struct.unpack(">I4s", data[pos:pos + 8])
        chunk = data[pos + 8:pos + 8 + length]
        if ctype == b"IHDR":
            width, height, bit_depth, color_type, _, _, interlace = struct.unpack(
                ">IIBBBBB", chunk
            )
        elif ctype == b"IDAT":
            idat += chunk
        elif ctype == b"IEND":
            break
        pos += 12 + length
    if width is None:
        raise ValueError("no IHDR chunk")
    if (bit_depth, color_type, interlace) != (8, 6, 0):
        raise ValueError(
            f"unexpected IHDR: bit_depth={bit_depth} color_type={color_type} "
            f"interlace={interlace} (want 8 / 6 / 0)"
        )
    raw = zlib.decompress(idat)
    stride = width * 4
    if len(raw) != (stride + 1) * height:
        raise ValueError(f"unexpected pixel data length {len(raw)}")
    pixels = bytearray()
    prev = bytes(stride)
    for y in range(height):
        ftype = raw[y * (stride + 1)]
        row = bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        if ftype == 0:
            pass
        elif ftype == 1:
            for i in range(4, stride):
                row[i] = (row[i] + row[i - 4]) & 0xFF
        elif ftype == 2:
            for i in range(stride):
                row[i] = (row[i] + prev[i]) & 0xFF
        elif ftype == 3:
            for i in range(stride):
                a = row[i - 4] if i >= 4 else 0
                row[i] = (row[i] + ((a + prev[i]) >> 1)) & 0xFF
        elif ftype == 4:
            for i in range(stride):
                a = row[i - 4] if i >= 4 else 0
                b = prev[i]
                c = prev[i - 4] if i >= 4 else 0
                row[i] = (row[i] + paeth(a, b, c)) & 0xFF
        else:
            raise ValueError(f"unknown filter type {ftype}")
        pixels += row
        prev = bytes(row)
    return width, height, bytes(pixels)


def main():
    path, cmd = sys.argv[1], sys.argv[2]
    try:
        width, height, pixels = decode(path)
    except (OSError, ValueError, zlib.error) as err:
        print(f"invalid PNG {path}: {err}", file=sys.stderr)
        return 1
    if cmd == "info":
        print(f"{width} {height}")
        return 0
    x, y = int(sys.argv[3]), int(sys.argv[4])
    if not (0 <= x < width and 0 <= y < height):
        print(f"pixel ({x},{y}) outside {width}x{height}", file=sys.stderr)
        return 1
    off = (y * width + x) * 4
    r, g, b, a = pixels[off:off + 4]
    if cmd == "pixel":
        print(f"{r} {g} {b} {a}")
        return 0
    if cmd == "pixel-near":
        want_r, want_g, want_b, tol = (int(v) for v in sys.argv[5:9])
        if (
            abs(r - want_r) <= tol
            and abs(g - want_g) <= tol
            and abs(b - want_b) <= tol
            and a == 255
        ):
            return 0
        print(
            f"pixel ({x},{y}) is ({r},{g},{b},{a}), want ({want_r},{want_g},{want_b},255) ±{tol}",
            file=sys.stderr,
        )
        return 1
    print(f"unknown command {cmd}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
PYEOF

pngcheck() {
    python3 "$WORK/pngcheck.py" "$@"
}

# --- Build the test client -----------------------------------------------
# Same lookup as nested-check.sh: xdg-shell + presentation-time from
# the installed wayland-protocols package, else the vendored crate
# copy.
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

# The layer client (a second binary: the layer mode is compiled in
# only with -DHAVE_WLR_LAYER_SHELL). The wlr-layer-shell protocol
# ships in the wayland-protocols-wlr crate.
LAYER_SHELL_XML="/usr/share/wayland-protocols/unstable/wlr-layer-shell-unstable-v1.xml"
if [ ! -f "$LAYER_SHELL_XML" ]; then
    LAYER_SHELL_XML="$(ls ~/.cargo/registry/src/*/wayland-protocols-wlr-*/wlr-protocols/unstable/wlr-layer-shell-unstable-v1.xml 2>/dev/null | head -1)"
fi
[ -n "$LAYER_SHELL_XML" ] && [ -f "$LAYER_SHELL_XML" ] || fail "wlr-layer-shell-unstable-v1.xml not found"
wayland-scanner client-header "$LAYER_SHELL_XML" "$WORK/wlr-layer-shell-unstable-v1-client-protocol.h" \
    || fail "wayland-scanner layer-shell client-header failed"
wayland-scanner private-code "$LAYER_SHELL_XML" "$WORK/wlr-layer-shell-unstable-v1-protocol.c" \
    || fail "wayland-scanner layer-shell private-code failed"
gcc -DHAVE_WLR_LAYER_SHELL -o "$WORK/sevyn-test-client-layer" "$CLIENT_SRC" \
    "$WORK/xdg-shell-protocol.c" "$WORK/presentation-time-protocol.c" \
    "$WORK/wlr-layer-shell-unstable-v1-protocol.c" \
    -I"$WORK" $(pkg-config --cflags --libs wayland-client) \
    || fail "layer test client build failed"

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
# The M1 window (800x300 at the cascade origin (0,0)): subsurface A
# (left half) is solid 0xFF402020, B (right half) solid 0xFF202040.
rm -f "$CLIENT_HARDCODED_LOG"
WAYLAND_DISPLAY=sevyn-nested-test "$WORK/sevyn-test-client" >"$CLIENT_LOG" 2>&1 &
echo $! >"$WORK/client.pid"
wait_for "$CLIENT_HARDCODED_LOG" "M1CLIENT READY" "client READY"
wait_for "$COMPOSITOR_LOG" "SEVYN SURFACE COMMITTED id=1 size=800x300" "surface committed"
sleep 0.5

# --- C1: capture writes a valid PNG with the actual pixels -----------------
C1_REPLY="$(capture)" || C1_REPLY=""
echo "$C1_REPLY" | grep -q '"ok":true' || ledger_fail "C1" "capture reply not ok: $C1_REPLY"
if [ -f "$CAPTURE_PNG" ]; then
    C1_INFO="$(pngcheck "$CAPTURE_PNG" info)" || C1_INFO=""
    if [ "$C1_INFO" = "1280 800" ]; then
        C1_PIXELS_OK=1
        pngcheck "$CAPTURE_PNG" pixel-near 100 100 64 32 32 0 \
            || { C1_PIXELS_OK=0; echo "capture-check: C1: subsurface A pixel wrong" >&2; }
        pngcheck "$CAPTURE_PNG" pixel-near 600 100 32 32 64 0 \
            || { C1_PIXELS_OK=0; echo "capture-check: C1: subsurface B pixel wrong" >&2; }
        # The empty desktop's clear colour (0.05, 0.05, 0.08) is
        # (13, 13, 20) after 8-bit quantisation; ±2 covers rounding.
        pngcheck "$CAPTURE_PNG" pixel-near 1000 700 13 13 20 2 \
            || { C1_PIXELS_OK=0; echo "capture-check: C1: background pixel wrong" >&2; }
        if [ "$C1_PIXELS_OK" = 1 ] && grep -q "CAPTURE OK width=1280 height=800 path=$CAPTURE_PNG" "$COMPOSITOR_LOG"; then
            ledger_pass "C1 valid PNG with actual pixels (A red, B blue, desktop clear colour)"
        else
            ledger_fail "C1" "pixel values or CAPTURE OK marker wrong"
        fi
    else
        ledger_fail "C1" "PNG info wrong (want '1280 800', got '$C1_INFO')"
    fi
else
    ledger_fail "C1" "no PNG written at $CAPTURE_PNG"
fi
HASH_C1="$(sha256sum "$CAPTURE_PNG" 2>/dev/null | cut -d' ' -f1)"

# --- C2: a second capture overwrites the same single file ------------------
if [ -n "$HASH_C1" ]; then
    C2_REPLY="$(capture)" || C2_REPLY=""
    HASH_C2="$(sha256sum "$CAPTURE_PNG" 2>/dev/null | cut -d' ' -f1)"
    if echo "$C2_REPLY" | grep -q '"ok":true' && [ "$HASH_C2" = "$HASH_C1" ] \
        && [ "$(find "$CAPTURE_DIR" -type f | wc -l)" = 1 ]; then
        ledger_pass "C2 repeat capture overwrites one file (identical bytes, still one file)"
    else
        ledger_fail "C2" "repeat capture reply=$C2_REPLY hash changed or file count wrong"
    fi
else
    ledger_fail "C2" "skipped (C1 produced no PNG)"
fi

# Close the M1 client via the bridge, then capture the changed scene:
# the bytes must change (the window is gone) and there must still be
# exactly one file.
SESSION="$(grep -o "SESSION ASSOCIATED session=[^ ]*" "$COMPOSITOR_LOG" | head -1 | cut -d= -f2)"
if [ -n "$SESSION" ]; then
    CLOSE_REPLY="$(bridge "{\"v\": 1, \"op\": \"close\", \"session\": \"$SESSION\"}")" || CLOSE_REPLY=""
    echo "$CLOSE_REPLY" | grep -q '"ok":true' || fail "bridge close failed: $CLOSE_REPLY"
    deadline=$((SECONDS + TIMEOUT))
    while kill -0 "$(cat "$WORK/client.pid")" 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do
        sleep 0.2
    done
    kill -0 "$(cat "$WORK/client.pid")" 2>/dev/null && fail "client did not exit after bridge close"
    sleep 0.5
    C2B_REPLY="$(capture)" || C2B_REPLY=""
    HASH_C2B="$(sha256sum "$CAPTURE_PNG" 2>/dev/null | cut -d' ' -f1)"
    if echo "$C2B_REPLY" | grep -q '"ok":true' && [ -n "$HASH_C2B" ] && [ "$HASH_C2B" != "$HASH_C1" ] \
        && [ "$(find "$CAPTURE_DIR" -type f | wc -l)" = 1 ] \
        && pngcheck "$CAPTURE_PNG" pixel-near 100 100 13 13 20 2; then
        ledger_pass "C2b scene change rewrites the one file (bytes change, window gone, still one file)"
    else
        ledger_fail "C2b" "post-close capture reply=$C2B_REPLY hash=$HASH_C2B (was $HASH_C1)"
    fi
else
    ledger_fail "C2b" "skipped (no session id in the compositor log)"
fi
HASH_GOOD="$HASH_C2B"

# --- C3: sensitive layer stand-in → refused, never its pixels --------------
rm -f "$LAYER_HARDCODED_LOG"
WAYLAND_DISPLAY=sevyn-nested-test SEVYN_LAYER_NAMESPACE=sevyn-lock \
    "$WORK/sevyn-test-client-layer" --layer >"$WORK/layer-client.log" 2>&1 &
echo $! >"$WORK/layer-client.pid"
wait_for "$LAYER_HARDCODED_LOG" "LAYER CONFIGURE width=1280 height=800" "layer client configured fullscreen"
wait_for "$COMPOSITOR_LOG" "SEVYN LAYER SURFACE namespace=sevyn-lock layer=Background mapped=true" "sensitive layer mapped"
C3_REPLY="$(capture)" || C3_REPLY=""
HASH_C3="$(sha256sum "$CAPTURE_PNG" 2>/dev/null | cut -d' ' -f1)"
if echo "$C3_REPLY" | grep -q '"ok":false' \
    && echo "$C3_REPLY" | grep -q 'refused: sensitive surface' \
    && [ "$HASH_C3" = "$HASH_GOOD" ] \
    && grep -q "CAPTURE REFUSED reason=sensitive-surface detail=layer namespace=sevyn-lock" "$COMPOSITOR_LOG"; then
    ledger_pass "C3 sensitive layer refused (no pixels, last good frame untouched)"
else
    ledger_fail "C3" "reply=$C3_REPLY hash=$HASH_C3 (was $HASH_GOOD)"
fi
# The layer client must be gone before C4, or its refusal would mask
# the toplevel's.
kill "$(cat "$WORK/layer-client.pid")" 2>/dev/null || true
deadline=$((SECONDS + TIMEOUT))
while kill -0 "$(cat "$WORK/layer-client.pid")" 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do
    sleep 0.2
done
kill -0 "$(cat "$WORK/layer-client.pid")" 2>/dev/null && fail "layer client did not exit"
wait_for "$COMPOSITOR_LOG" "SEVYN LAYER SURFACE DESTROYED namespace=sevyn-lock" "sensitive layer destroyed"

# --- C4: sensitive toplevel stand-in → refused, never its pixels -----------
rm -f "$LOCK_CLIENT_LOG"
WAYLAND_DISPLAY=sevyn-nested-test SEVYN_CLIENT_APP_ID=sevyn.lock \
    SEVYN_CLIENT_TITLE="Lock Screen" SEVYN_CLIENT_LOG="$LOCK_CLIENT_LOG" \
    "$WORK/sevyn-test-client" >"$WORK/lock-client.log" 2>&1 &
echo $! >"$WORK/lock-client.pid"
wait_for "$LOCK_CLIENT_LOG" "M1CLIENT READY" "lock client READY"
wait_for "$COMPOSITOR_LOG" "SEVYN SURFACE METADATA .* app_id=sevyn.lock title=Lock Screen" "lock surface metadata"
sleep 0.5
C4_REPLY="$(capture)" || C4_REPLY=""
HASH_C4="$(sha256sum "$CAPTURE_PNG" 2>/dev/null | cut -d' ' -f1)"
if echo "$C4_REPLY" | grep -q '"ok":false' \
    && echo "$C4_REPLY" | grep -q 'refused: sensitive surface' \
    && [ "$HASH_C4" = "$HASH_GOOD" ] \
    && grep -q "CAPTURE REFUSED reason=sensitive-surface detail=toplevel app_id=sevyn.lock title=Lock Screen" "$COMPOSITOR_LOG"; then
    ledger_pass "C4 sensitive toplevel refused (no pixels, last good frame untouched)"
else
    ledger_fail "C4" "reply=$C4_REPLY hash=$HASH_C4 (was $HASH_GOOD)"
fi
# Close the lock client again through the bridge (its session is the
# last ASSOCIATED line); the compositor must stay alive for C5.
LOCK_SESSION="$(grep -o "SESSION ASSOCIATED session=[^ ]*" "$COMPOSITOR_LOG" | tail -1 | cut -d= -f2)"
if [ -n "$LOCK_SESSION" ]; then
    CLOSE_REPLY="$(bridge "{\"v\": 1, \"op\": \"close\", \"session\": \"$LOCK_SESSION\"}")" || CLOSE_REPLY=""
    echo "$CLOSE_REPLY" | grep -q '"ok":true' || fail "bridge close of lock client failed: $CLOSE_REPLY"
    deadline=$((SECONDS + TIMEOUT))
    while kill -0 "$(cat "$WORK/lock-client.pid")" 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do
        sleep 0.2
    done
    kill -0 "$(cat "$WORK/lock-client.pid")" 2>/dev/null && fail "lock client did not exit after bridge close"
fi

# --- C5: malformed requests leave the compositor alive ----------------------
C5_OK=1
C5_BAD_JSON="$(bridge 'this is not json')" || C5_BAD_JSON=""
echo "$C5_BAD_JSON" | grep -q '"ok":false' && echo "$C5_BAD_JSON" | grep -q 'malformed request' \
    || { C5_OK=0; echo "capture-check: C5: bad JSON reply wrong: $C5_BAD_JSON" >&2; }
C5_VERSION="$(bridge '{"v": 2, "op": "capture"}')" || C5_VERSION=""
echo "$C5_VERSION" | grep -q '"ok":false' && echo "$C5_VERSION" | grep -q 'unsupported protocol version' \
    || { C5_OK=0; echo "capture-check: C5: wrong-version reply wrong: $C5_VERSION" >&2; }
C5_UNKNOWN="$(bridge '{"v": 1, "op": "bogus"}')" || C5_UNKNOWN=""
echo "$C5_UNKNOWN" | grep -q '"ok":false' && echo "$C5_UNKNOWN" | grep -q 'unknown op' \
    || { C5_OK=0; echo "capture-check: C5: unknown-op reply wrong: $C5_UNKNOWN" >&2; }
C5_RELATIVE="$(bridge '{"v": 1, "op": "capture", "path": "relative/latest.png"}')" || C5_RELATIVE=""
echo "$C5_RELATIVE" | grep -q '"ok":false' && echo "$C5_RELATIVE" | grep -q 'malformed request' \
    || { C5_OK=0; echo "capture-check: C5: relative-path reply wrong: $C5_RELATIVE" >&2; }
C5_PING="$(bridge '{"v": 1, "op": "ping"}')" || C5_PING=""
echo "$C5_PING" | grep -q '"ok":true' \
    || { C5_OK=0; echo "capture-check: C5: ping after malformed requests failed: $C5_PING" >&2; }
kill -0 "$COMPOSITOR_PID" 2>/dev/null \
    || { C5_OK=0; echo "capture-check: C5: compositor died" >&2; }
HASH_C5="$(sha256sum "$CAPTURE_PNG" 2>/dev/null | cut -d' ' -f1)"
[ "$HASH_C5" = "$HASH_GOOD" ] \
    || { C5_OK=0; echo "capture-check: C5: last good frame changed (was $HASH_GOOD, now $HASH_C5)" >&2; }
if [ "$C5_OK" = 1 ]; then
    ledger_pass "C5 malformed requests refused, compositor alive, last good frame untouched"
else
    ledger_fail "C5" "see failures above"
fi

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
grep -q "SEVYN SHUTDOWN clean" "$COMPOSITOR_LOG" || fail "missing SHUTDOWN clean marker"
if pgrep -f "sevyn-test-clien[t]" >/dev/null 2>&1; then
    fail "test client processes remain after compositor shutdown"
fi

echo "capture-check: PASS=$PASS_COUNT FAIL=$FAIL_COUNT"
if [ "$FAIL_COUNT" -gt 0 ]; then
    echo "capture-check: FAIL — $FAIL_COUNT case(s) failed" >&2
    exit 1
fi
echo "capture-check: PASS — screen capture proven (valid PNG with real pixels, overwrite/no-history, S3 refusal for layer + toplevel, malformed requests harmless)"
