#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# real-chromium-adoption-check.sh — the Stage 5 real-Chromium adoption
# check (COMPOSITOR_PLAN.md, Stage 5; ~/workspace/sevynos-stage5-browser/SCOPE.md).
#
# The Stage 4 spike (chromium-spike.sh) proved real Chromium's dmabuf
# buffers land as textures, and adoption-check.sh (A1–A15) proved the
# adoption mechanism with a test client as the Chromium stand-in. This
# check proves the two together with the REAL client: a real Chromium
# window, adopted via the bridge `adopt` op into a Browser rect,
# renders its pixels inside that rect.
#
#   R0  real Chromium maps and paints: a capture before adoption shows
#       the page's solid colour somewhere on the output (its standalone
#       cascaded window) — the check never mistakes "Chromium never
#       rendered" for "adoption hid it". R0's bbox is kept: R1 asserts
#       it extended outside the rect, so adoption really moved it.
#   R1  adopt: bridge adopt ok:true + `ADOPTION ADOPTED`, then a
#       capture proves the SAME buffer relocated — the page's red
#       count matches R0 within 2% — with zero red, inactive-
#       titlebar, or infobar pixels outside the rect (a full-image
#       scan — that is the "no second window" and clipping proof in
#       one), and the union of the red/titlebar/infobar bboxes starts
#       at the rect origin within 2 px: the window's GEOMETRY lands
#       at the rect origin (see `GenesisState::adopted_buffer_origin`;
#       before the Stage 5 fix the union-min was the rect origin plus
#       Chrome's shadow offset (16,10)). The Browser chrome strip is
#       intact above the rect, and Chromium opened exactly one
#       toplevel. The page is a SUB-RECTANGLE of the Chromium window —
#       titlebar + infobar + shadow margins sit around it — so the
#       check never asserts the page "fills" the rect.
#   R2  resize: bridge resize to a narrower rect sends
#       xdg_toplevel.configure to real Chromium, which commits a new
#       size (any size other than the initial one — Chrome commits
#       its geometry, 600x338 for a 600x348 configure); a capture
#       proves the red count narrowed but stays large, nothing
#       red/titlebar/infobar falls outside the new rect, the window
#       did not move (union-min still the rect origin), and the freed
#       strip shows the Browser's own content colour.
#   R3  Chromium death: killing the Chromium process emits the
#       `chromium_died` bridge event + `ADOPTION CHROMIUM DIED`, the
#       content area empties back to the Browser's content colour
#       (no red pixel anywhere), and the Browser + compositor stay
#       alive (bridge ping ok:true).
#   R4  clean shutdown, no orphans (no chrome / test-client processes
#       remain).
#
# The Chromium window's own frame: with --no-sandbox Chromium shows
# an infobar (white, ~35 px tall) between its titlebar and the page,
# and its geometry is inset within its buffer by shadow margins
# (16,10) — the page's red rectangle is offset inside the window by
# that inset, and the window's visible size (768x306 for an 800x348
# window-size) is the geometry, not the buffer. The check measures
# all three parts (page, titlebar, infobar) and pins the window by
# their union-min, so it proves the whole window's placement, not
# just the page's.
#
# The rect geometry is derived from the captured pixels, not assumed:
# the Browser chrome strip's solid grey bounds the Browser window's
# output origin, and the rect is that origin + (0,152). The page is a
# local file with a solid #E02020 background — a colour nothing else
# in the scene uses (Browser chrome #404040, Browser content
# #182818, desktop clear ~(13,13,20)).
#
# What this check does NOT prove: the dmabuf path (the spike proves
# it; under this nested backend Chromium may render through SHM or
# llvmpipe — the pixels are the proof here, not the buffer type),
# drag/focus/close behaviour with the real client (adoption-check
# proves those with the stand-in; the GF65 gate proves them with the
# real Browser + Chromium), the DRM backend's pixels (no DRM device
# in CI — proven downstream by the QMP screendump oracle in the
# SevynOS m13 check), or 60 fps (the GF65's proof, never this
# harness's).
#
# Chromium source: a real Chromium snapshot zip (the same shape as
# the spike's /root/chrome-linux.zip). By default it downloads the
# PINNED snapshot revision 1738523 (Chromium 159.0.8150.0) — the same
# snapshot `sevyn browser` launches (sevynos
# v2/image/build-m5-image.sh pins the same revision), so the check
# runs the browser the product ships, not a drifting "latest".
# SEVYN_CHROMIUM_ZIP / SEVYN_CHROMIUM_ZIP_URL / SEVYN_CHROMIUM_BIN
# override it. The download is retried; a missing/unusable Chromium
# fails the run loudly, never silently.
#
# Usage:
#   compositor/tests/real-chromium-adoption-check.sh
#
# Environment (all optional):
#   SEVYN_COMPOSITOR_BIN  path to sevyn-compositor
#                          (default: compositor/target/debug/sevyn-compositor)
#   SEVYN_TEST_CLIENT_SRC path to test-client.c (default: compositor/tests/test-client.c)
#   SEVYN_CHROMIUM_BIN    path to an already-unpacked chrome binary
#   SEVYN_CHROMIUM_ZIP    path to a local chrome-linux.zip
#   SEVYN_CHROMIUM_ZIP_URL  URL to download chrome-linux.zip from
#                          (default: the download-chromium.appspot.com snapshot)
#   DISPLAY_NUMBER        X display number for Xvfb (default: 98)
#   REAL_CHROMIUM_TIMEOUT seconds per wait step (default: 30)
#   CHROMIUM_START_TIMEOUT seconds to wait for Chromium's surface (default: 180)
#   RESIZE_TIMEOUT        seconds to wait for Chromium's resize commit (default: 60)
#   SEVYN_CHECK_KEEP_DIR  when set, run artifacts (logs + every capture
#                          PNG) are copied there instead of being deleted
#                          with the work dir (debugging aid)
#
# Exit 0 when every case passes, non-zero otherwise. All processes
# are killed on exit (success or failure).

set -u
cd "$(dirname "$0")/../.." || exit 1

COMPOSITOR_BIN="${SEVYN_COMPOSITOR_BIN:-compositor/target/debug/sevyn-compositor}"
CLIENT_SRC="${SEVYN_TEST_CLIENT_SRC:-compositor/tests/test-client.c}"
CHROMIUM_BIN="${SEVYN_CHROMIUM_BIN:-}"
CHROMIUM_ZIP="${SEVYN_CHROMIUM_ZIP:-}"
CHROMIUM_ZIP_URL="${SEVYN_CHROMIUM_ZIP_URL:-https://commondatastorage.googleapis.com/chromium-browser-snapshots/Linux_x64/1738523/chrome-linux.zip}"
DISPLAY_NUMBER="${DISPLAY_NUMBER:-98}"
TIMEOUT="${REAL_CHROMIUM_TIMEOUT:-30}"
CHROMIUM_START_TIMEOUT="${CHROMIUM_START_TIMEOUT:-180}"
RESIZE_TIMEOUT="${RESIZE_TIMEOUT:-60}"
SOCKET_NAME="sevyn-real-chromium-test"
# The Browser stand-in's geometry (COMPOSITOR_PLAN.md, Stage 5).
BROWSER_W=800; BROWSER_H=500; CHROME_H=152
RECT_X=0; RECT_Y=152; RECT_W=800; RECT_H=348
RESIZED_W=600
OUT_W=1280; OUT_H=800
# The page's solid colour, and the scene's other known colours.
PAGE_R=224; PAGE_G=32; PAGE_B=32
PAGE_TOL=6
CHROME_GRAY_R=64; CHROME_GRAY_G=64; CHROME_GRAY_B=64
CONTENT_R=24; CONTENT_G=40; CONTENT_B=24
CLEAR_R=13; CLEAR_G=13; CLEAR_B=20
# Real Chromium's own window frame (pinned snapshot 159): the
# INACTIVE titlebar (221,227,233) — adoption leaves `Activated` on
# the Browser window, so the adopted Chromium renders its inactive
# titlebar — and the --no-sandbox infobar (white). Both are Chrome
# UI, not the page: they sit inside the adopted window above the
# page, and the check scans them to prove the WHOLE window (not just
# the page) renders inside the rect.
TITLEBAR_R=221; TITLEBAR_G=227; TITLEBAR_B=233
TITLEBAR_TOL=4
INFOBAR_R=255; INFOBAR_G=255; INFOBAR_B=255

WORK="$(mktemp -d /tmp/sevyn-real-chromium-check.XXXXXX)"
RUNTIME_DIR="$WORK/run"
mkdir -p "$RUNTIME_DIR"
COMPOSITOR_LOG="$WORK/compositor.log"
COMPOSITOR_ERR="$WORK/compositor.err"
WESTON_LOG="$WORK/weston.log"
XVFB_LOG="$WORK/xvfb.log"
CHROMIUM_LOG="$WORK/chromium.log"
BRIDGE_IN="$WORK/bridge-in.txt"
BRIDGE_OUT="$WORK/bridge-out.txt"
: > "$BRIDGE_IN"; : > "$BRIDGE_OUT"
CAPTURE_DIR="$WORK/capture"
mkdir -p "$CAPTURE_DIR"
CAPTURE_PNG="$CAPTURE_DIR/latest.png"
BROWSER_LOG="$WORK/client-browser.log"

PASS_COUNT=0
FAIL_COUNT=0

ledger_pass() {
    PASS_COUNT=$((PASS_COUNT + 1))
    echo "real-chromium-adoption-check: $1: PASS"
}

ledger_fail() {
    FAIL_COUNT=$((FAIL_COUNT + 1))
    echo "real-chromium-adoption-check: $1: FAIL: $2" >&2
}

# fail <message>: infrastructure failure — abort the whole check.
fail() {
    echo "real-chromium-adoption-check: FAIL: $*" >&2
    echo "real-chromium-adoption-check: --- ledger ---" >&2
    echo "real-chromium-adoption-check: PASS=$PASS_COUNT FAIL=$FAIL_COUNT" >&2
    echo "real-chromium-adoption-check: --- compositor.log (tail) ---" >&2
    tail -40 "$COMPOSITOR_LOG" >&2 2>/dev/null || true
    echo "real-chromium-adoption-check: --- compositor.err (tail) ---" >&2
    tail -20 "$COMPOSITOR_ERR" >&2 2>/dev/null || true
    echo "real-chromium-adoption-check: --- chromium.log (tail) ---" >&2
    tail -40 "$CHROMIUM_LOG" >&2 2>/dev/null || true
    echo "real-chromium-adoption-check: --- weston.log (tail) ---" >&2
    tail -20 "$WESTON_LOG" >&2 2>/dev/null || true
    exit 1
}

cleanup() {
    if [ -n "${SEVYN_CHECK_KEEP_DIR:-}" ]; then
        mkdir -p "$SEVYN_CHECK_KEEP_DIR"
        for f in compositor.log compositor.err weston.log xvfb.log chromium.log bridge-out.txt bridge-in.txt bridge-runtime.log; do
            [ -f "$WORK/$f" ] && cp -a "$WORK/$f" "$SEVYN_CHECK_KEEP_DIR/$f" 2>/dev/null || true
        done
        cp -a "$CAPTURE_DIR/." "$SEVYN_CHECK_KEEP_DIR/" 2>/dev/null || true
        echo "real-chromium-adoption-check: run artifacts kept in $SEVYN_CHECK_KEEP_DIR"
    fi
    if [ -f "$WORK/bridge-runtime.pid" ]; then
        kill "$(cat "$WORK/bridge-runtime.pid")" 2>/dev/null || true
    fi
    pkill -x chrome 2>/dev/null || true
    pkill -f "sevyn-test-clien[t]" 2>/dev/null || true
    for pid_file in "$WORK"/xvfb.pid "$WORK"/weston.pid "$WORK"/compositor.pid; do
        if [ -f "$pid_file" ]; then kill "$(cat "$pid_file")" 2>/dev/null || true; fi
    done
    sleep 1
    pkill -9 -x chrome 2>/dev/null || true
    for pid_file in "$WORK"/xvfb.pid "$WORK"/weston.pid "$WORK"/compositor.pid; do
        if [ -f "$pid_file" ]; then kill -9 "$(cat "$pid_file")" 2>/dev/null || true; fi
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

# wait_count <file> <pattern> <n> [timeout] [description]: poll until the
# pattern appears at least <n> times in the file.
wait_count() {
    local file="$1" pattern="$2" n="$3" timeout="${4:-$TIMEOUT}" what="${5:-$pattern x$n}"
    local deadline=$((SECONDS + timeout))
    while [ "$SECONDS" -lt "$deadline" ]; do
        local c
        c="$(grep -c -- "$pattern" "$file" 2>/dev/null || true)"
        if [ "${c:-0}" -ge "$n" ] 2>/dev/null; then return 0; fi
        sleep 0.5
    done
    fail "timed out waiting for $what (pattern: $pattern in $file)"
}

[ -x "$COMPOSITOR_BIN" ] || fail "compositor binary not found: $COMPOSITOR_BIN (run cargo build first)"
command -v Xvfb >/dev/null || fail "Xvfb not found"
command -v weston >/dev/null || fail "weston not found"
command -v python3 >/dev/null || fail "python3 not found"
command -v gcc >/dev/null || fail "gcc not found"
command -v wayland-scanner >/dev/null || fail "wayland-scanner not found"
command -v curl >/dev/null || fail "curl not found"
command -v unzip >/dev/null || fail "unzip not found"
command -v sha256sum >/dev/null || fail "sha256sum not found"

# --- Chromium acquisition ---------------------------------------------------
# A real Chromium snapshot zip (the spike's shape): an unpacked
# binary, a local zip, or a download. The download is retried; a
# missing/unusable Chromium fails loudly, never silently.
if [ -n "$CHROMIUM_BIN" ]; then
    [ -x "$CHROMIUM_BIN" ] || fail "SEVYN_CHROMIUM_BIN is not executable: $CHROMIUM_BIN"
    CHROME="$CHROMIUM_BIN"
else
    CHROMIUM_DIR="$WORK/chrome-linux"
    if [ -n "$CHROMIUM_ZIP" ]; then
        [ -f "$CHROMIUM_ZIP" ] || fail "SEVYN_CHROMIUM_ZIP not found: $CHROMIUM_ZIP"
        echo "real-chromium-adoption-check: unzipping $CHROMIUM_ZIP ..."
        unzip -q "$CHROMIUM_ZIP" -d "$WORK" || fail "unzip of $CHROMIUM_ZIP failed"
    else
        echo "real-chromium-adoption-check: downloading Chromium snapshot ($CHROMIUM_ZIP_URL) ..."
        curl --retry 3 --retry-delay 5 -L -o "$WORK/chrome-linux.zip" "$CHROMIUM_ZIP_URL" \
            || fail "Chromium snapshot download failed: $CHROMIUM_ZIP_URL"
        unzip -q "$WORK/chrome-linux.zip" -d "$WORK" || fail "unzip of the downloaded snapshot failed"
    fi
    CHROME="$CHROMIUM_DIR/chrome"
    [ -x "$CHROME" ] || fail "no chrome binary in the Chromium snapshot ($CHROME)"
fi
echo "real-chromium-adoption-check: chromium: $("$CHROME" --version 2>/dev/null || echo 'version query failed')"
MISSING_LIBS="$(ldd "$CHROME" 2>/dev/null | grep "not found" || true)"
[ -z "$MISSING_LIBS" ] || fail "Chromium is missing runtime libraries (install them first): $MISSING_LIBS"

# --- Bridge client (bridge.py, runtime mode) --------------------------------
# Copied from adoption-check.sh: one persistent "Runtime" connection.
# The harness appends one JSON request line per request to the infile;
# response lines (and event lines such as chromium_died) are appended
# to the outfile. The compositor is launched with --runtime-pid=<this
# process's pid>, so the bridge admits ONLY this process (production
# shape: init passes the Runtime pid at launch).
cat >"$WORK/bridge.py" <<'PYEOF'
# SPDX-License-Identifier: GPL-3.0-or-later
"""bridge.py — the adoption harness's bridge-socket client (runtime mode).

Usage:
  bridge.py runtime <socket> <infile> <outfile>
      Hold one persistent connection (the Runtime). Append one JSON
      request line to <infile> per request; response lines (and event
      lines such as chromium_died) are appended to <outfile>.
"""
import json
import select
import socket
import sys
import time


def read_lines(sock, buf, timeout):
    """Read until a full line is available or timeout; returns lines."""
    end = time.time() + timeout
    lines = []
    while time.time() < end:
        if b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            lines.append(line.decode(errors="replace"))
            continue
        r, _, _ = select.select([sock], [], [], min(0.2, max(0.01, end - time.time())))
        if not r:
            continue
        try:
            chunk = sock.recv(4096)
        except OSError:
            break
        if not chunk:
            break
        buf += chunk
    return lines, buf


def main():
    sock_path, infile, outfile = sys.argv[2], sys.argv[3], sys.argv[4]

    def connect_retry():
        deadline = time.time() + 60
        while True:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(30)
            try:
                s.connect(sock_path)
                s.setblocking(False)
                return s
            except OSError:
                s.close()
                if time.time() > deadline:
                    raise
                time.sleep(0.2)

    sock = connect_retry()
    buf = b""
    offset = 0
    out = open(outfile, "a", buffering=1)
    while True:
        try:
            with open(infile, "r") as f:
                f.seek(offset)
                for line in f:
                    line = line.strip()
                    if not line:
                        continue
                    if sock is not None:
                        sock.sendall((line + "\n").encode())
                offset = f.tell()
        except FileNotFoundError:
            pass
        if sock is not None:
            lines, buf = read_lines(sock, buf, 0.2)
            for line in lines:
                out.write(line + "\n")
        time.sleep(0.05)


if __name__ == "__main__":
    sys.exit(main())
PYEOF

# bridge_req <json>: send one request over the persistent Runtime
# connection and wait for its response line. Prints the response.
# Returns 0 when a response arrived, 1 on timeout.
bridge_req() {
    local json="$1"
    local before
    before="$(wc -l < "$BRIDGE_OUT" | tr -d ' ')"
    printf '%s\n' "$json" >> "$BRIDGE_IN"
    local deadline=$((SECONDS + TIMEOUT))
    while [ "$SECONDS" -lt "$deadline" ]; do
        local line
        line="$(tail -n +"$((before + 1))" "$BRIDGE_OUT" 2>/dev/null | grep -m1 '"ok"' || true)"
        if [ -n "$line" ]; then
            printf '%s\n' "$line"
            return 0
        fi
        sleep 0.2
    done
    return 1
}
# bridge_req_ok <json>: send + assert the response is ok:true.
bridge_req_ok() {
    local resp
    resp="$(bridge_req "$1")" || return 1
    echo "$resp" | grep -q '"ok": *true' || return 1
    printf '%s\n' "$resp"
    return 0
}
# bridge_event_wait <pattern> [what]: poll the bridge outfile (which
# carries events on the Runtime connection) for the pattern.
bridge_event_wait() {
    local pattern="$1" what="${2:-$pattern}"
    local deadline=$((SECONDS + TIMEOUT))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if grep -q -- "$pattern" "$BRIDGE_OUT" 2>/dev/null; then return 0; fi
        sleep 0.5
    done
    return 1
}

# --- PNG decoder + scan -----------------------------------------------------
# pngcheck.py <path> info | pixel <x> <y> | pixel-near <x> <y> r g b tol
#                 | bbox <r> <g> <b> <tol>
#                 | scan <r> <g> <b> <tol> <rx> <ry> <rw> <rh>
# Stdlib only (zlib + struct): parses IHDR, concatenates IDAT,
# decompresses, and undoes all five PNG filter types. Asserts bit
# depth 8, colour type 6 (RGBA), non-interlaced — the shape the
# compositor's `png`-crate encoder writes. `scan` counts every pixel
# near the given colour in the whole image, reports its bounding box,
# and counts how many fall outside the given rect expanded by a 1 px
# grace ring (the "no second window / clipped" measurement). `bbox`
# reports the bounding box + count of pixels near a colour (the
# Browser chrome strip's box bounds the Browser window's origin).
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


def near(px, want, tol):
    return (
        abs(px[0] - want[0]) <= tol
        and abs(px[1] - want[1]) <= tol
        and abs(px[2] - want[2]) <= tol
        and px[3] == 255
    )


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
    if cmd in ("pixel", "pixel-near"):
        x, y = int(sys.argv[3]), int(sys.argv[4])
        if not (0 <= x < width and 0 <= y < height):
            print(f"pixel ({x},{y}) outside {width}x{height}", file=sys.stderr)
            return 1
        off = (y * width + x) * 4
        r, g, b, a = pixels[off:off + 4]
        if cmd == "pixel":
            print(f"{r} {g} {b} {a}")
            return 0
        want_r, want_g, want_b, tol = (int(v) for v in sys.argv[5:9])
        if near((r, g, b, a), (want_r, want_g, want_b), tol):
            return 0
        print(
            f"pixel ({x},{y}) is ({r},{g},{b},{a}), want ({want_r},{want_g},{want_b},255) ±{tol}",
            file=sys.stderr,
        )
        return 1
    if cmd == "bbox":
        want = tuple(int(v) for v in sys.argv[3:6])
        tol = int(sys.argv[6])
        count = 0
        minx, miny, maxx, maxy = width, height, -1, -1
        for y in range(height):
            for x in range(width):
                off = (y * width + x) * 4
                if near(pixels[off:off + 4], want, tol):
                    count += 1
                    minx, miny = min(minx, x), min(miny, y)
                    maxx, maxy = max(maxx, x), max(maxy, y)
        if count == 0:
            print("count=0 bbox=none")
        else:
            print(f"count={count} bbox={minx},{miny},{maxx},{maxy}")
        return 0
    if cmd == "scan":
        want = tuple(int(v) for v in sys.argv[3:6])
        tol = int(sys.argv[6])
        rx, ry, rw, rh = (int(v) for v in sys.argv[7:11])
        count = 0
        outside = 0
        minx, miny, maxx, maxy = width, height, -1, -1
        for y in range(height):
            for x in range(width):
                off = (y * width + x) * 4
                if near(pixels[off:off + 4], want, tol):
                    count += 1
                    minx, miny = min(minx, x), min(miny, y)
                    maxx, maxy = max(maxx, x), max(maxy, y)
                    if not (
                        rx - 1 <= x < rx + rw + 1 and ry - 1 <= y < ry + rh + 1
                    ):
                        outside += 1
        if count == 0:
            print("count=0 bbox=none outside=0")
        else:
            print(f"count={count} bbox={minx},{miny},{maxx},{maxy} outside={outside}")
        return 0
    print(f"unknown command {cmd}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
PYEOF

pngcheck() {
    python3 "$WORK/pngcheck.py" "$@"
}

# capture: one bridge `capture` op over the persistent Runtime
# connection, writing to $CAPTURE_PNG. The reply is deferred one loop
# iteration, so bridge_req waits for the ok:true line (up to TIMEOUT).
capture() {
    bridge_req "{\"v\": 1, \"op\": \"capture\", \"path\": \"$CAPTURE_PNG\"}"
}
# capture_png: capture + assert the reply is ok:true and the PNG is
# 1280x800. Echoes nothing on success; returns 1 on failure. With
# SEVYN_CHECK_KEEP_DIR set, each capture is also preserved as
# capture-<n>.png there (the bridge overwrites latest.png, so without
# this only the last frame survives for debugging).
CAPTURE_N=0
capture_png() {
    local reply
    reply="$(capture)" || return 1
    echo "$reply" | grep -q '"ok": *true' || return 1
    [ -f "$CAPTURE_PNG" ] || return 1
    [ "$(pngcheck "$CAPTURE_PNG" info)" = "$OUT_W $OUT_H" ] || return 1
    if [ -n "${SEVYN_CHECK_KEEP_DIR:-}" ]; then
        mkdir -p "$SEVYN_CHECK_KEEP_DIR"
        cp -a "$CAPTURE_PNG" "$SEVYN_CHECK_KEEP_DIR/capture-$CAPTURE_N.png" 2>/dev/null || true
    fi
    CAPTURE_N=$((CAPTURE_N + 1))
    return 0
}
# scan_value <scan-output> <key>: extract count=/outside= from a scan line.
scan_value() {
    echo "$1" | grep -o "$2=[0-9]*" | cut -d= -f2
}
# scan_bbox <scan-output>: the bbox= field, or "none".
scan_bbox() {
    echo "$1" | grep -o "bbox=[^ ]*" | cut -d= -f2
}
# bbox_field <bbox> <n>: the n-th comma field of a bbox (1=minx,
# 2=miny, 3=maxx, 4=maxy); empty when the bbox is "none".
bbox_field() {
    [ "$1" = "none" ] && return 0
    echo "$1" | cut -d, -f"$2"
}
# within_tol <value> <want> <tol>: true when |value-want| <= tol.
within_tol() {
    local d=$(( $1 - $2 ))
    d=${d#-}
    [ "$d" -le "$3" ]
}

# --- Build the test client -----------------------------------------------
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
deadline=$((SECONDS + TIMEOUT))
while [ ! -S "$RUNTIME_DIR/wayland-1" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
[ -S "$RUNTIME_DIR/wayland-1" ] || fail "weston socket never appeared (see $WESTON_LOG)"

# --- The nested compositor + the Runtime bridge connection -----------------
# The Runtime connection first (its pid is the Runtime's pid, passed
# to the compositor with --runtime-pid), then the compositor — the
# same shape as adoption-check.sh.
python3 "$WORK/bridge.py" runtime "$RUNTIME_DIR/sevyn-compositor.sock" "$BRIDGE_IN" "$BRIDGE_OUT" \
    >"$WORK/bridge-runtime.log" 2>&1 &
echo $! >"$WORK/bridge-runtime.pid"
sleep 0.5
kill -0 "$(cat "$WORK/bridge-runtime.pid")" 2>/dev/null \
    || fail "bridge runtime client died on start"
WAYLAND_DISPLAY=wayland-1 "$COMPOSITOR_BIN" --socket "$SOCKET_NAME" --size 1280x800 \
    --runtime-pid "$(cat "$WORK/bridge-runtime.pid")" \
    >"$COMPOSITOR_LOG" 2>"$COMPOSITOR_ERR" &
echo $! >"$WORK/compositor.pid"
COMPOSITOR_PID="$(cat "$WORK/compositor.pid")"
wait_for "$COMPOSITOR_LOG" "SEVYN READY socket=$SOCKET_NAME" "compositor READY"
wait_for "$COMPOSITOR_LOG" "SEVYN FIRST FRAME PRESENTED" "first presented frame"

# --- The Browser stand-in ----------------------------------------------------
rm -f "$BROWSER_LOG"
WAYLAND_DISPLAY="$SOCKET_NAME" SEVYN_CLIENT_MODE="browser" SEVYN_CLIENT_LOG="$BROWSER_LOG" \
    "$WORK/sevyn-test-client" >"$WORK/browser.stdout" 2>&1 &
echo $! >"$WORK/browser.pid"
BROWSER_PID="$(cat "$WORK/browser.pid")"
wait_for "$BROWSER_LOG" "BROWSER READY" "Browser READY"
wait_for "$COMPOSITOR_LOG" "SURFACE COMMITTED id=.* size=${BROWSER_W}x${BROWSER_H}" "Browser surface committed"
BROWSER_SESSION="$(grep -m1 "SESSION ASSOCIATED.*pid=$BROWSER_PID" "$COMPOSITOR_LOG" | sed 's/.*session=\([^ ]*\).*/\1/')"
[ -n "$BROWSER_SESSION" ] || fail "could not read the Browser session from the compositor log"
BROWSER_SURFACE="$(grep -m1 "SURFACE CREATED.*pid=$BROWSER_PID" "$COMPOSITOR_LOG" | grep -o "id=[0-9]*" | head -1 | cut -d= -f2)"
[ -n "$BROWSER_SURFACE" ] || fail "could not read the Browser surface id from the compositor log"
echo "real-chromium-adoption-check: browser pid=$BROWSER_PID session=$BROWSER_SESSION surface=$BROWSER_SURFACE"

# --- Real Chromium -----------------------------------------------------------
# The page: a local file, solid #E02020, no margins, no scrollbars.
cat >"$WORK/page.html" <<'HTMLEOF'
<!doctype html><html><head><meta charset="utf-8"><title>sevyn-red</title><style>html,body{margin:0;padding:0;height:100%;overflow:hidden;background:#E02020}</style></head><body></body></html>
HTMLEOF
WAYLAND_DISPLAY="$SOCKET_NAME" XDG_RUNTIME_DIR="$RUNTIME_DIR" "$CHROME" \
    --ozone-platform=wayland --no-sandbox --disable-dev-shm-usage \
    --no-first-run --disable-default-apps --disable-extensions \
    --disable-sync --disable-background-networking \
    --window-size=800,348 --app="file://$WORK/page.html" \
    --user-data-dir="$WORK/chromium-profile" \
    --enable-logging=stderr >"$CHROMIUM_LOG" 2>&1 &
echo $! >"$WORK/chromium-launcher.pid"
# Chromium cold start under llvmpipe is slow (the spike allows 180 s
# in the guest); wait for its surface in the compositor log. The
# signal is the compositor log (SURFACE CREATED), never the launcher
# pid — chrome forks and the launcher exits while the browser runs.
# NOTE: with --app=file://... Chromium's app_id is a derived
# `chrome-<hash>-Default`, not `chromium` (the spike's
# --app=about:blank gave `chromium`). That is fine — adoption matches
# by pid and the app_id is a sanity check only — and this check logs
# the live value rather than asserting it.
wait_count "$COMPOSITOR_LOG" "SURFACE CREATED" 2 "$CHROMIUM_START_TIMEOUT" "Chromium surface"
CHROMIUM_LINE="$(grep "SURFACE CREATED" "$COMPOSITOR_LOG" | grep -v "pid=$BROWSER_PID" | head -1)"
CHROMIUM_PID="$(echo "$CHROMIUM_LINE" | grep -o "pid=[0-9]*" | head -1 | cut -d= -f2)"
CHROMIUM_SURFACE="$(echo "$CHROMIUM_LINE" | grep -o "id=[0-9]*" | head -1 | cut -d= -f2)"
[ -n "$CHROMIUM_PID" ] && [ -n "$CHROMIUM_SURFACE" ] \
    || fail "could not read the Chromium surface/pid from the compositor log"
CHROMIUM_APP_ID="$(grep -m1 "SURFACE METADATA id=$CHROMIUM_SURFACE " "$COMPOSITOR_LOG" | grep -o "app_id=[^ ]*" | cut -d= -f2)"
echo "real-chromium-adoption-check: chromium pid=$CHROMIUM_PID surface=$CHROMIUM_SURFACE app_id=${CHROMIUM_APP_ID:-(not yet reported)}"
# Exactly one Chromium toplevel: a second window (first-run dialog,
# restore prompt, ...) would be a second surface for this pid. Folded
# into the R1 verdict below (not a separate ledger line, so one
# failure counts once).
CHROMIUM_WINDOWS="$(grep -c "SURFACE CREATED.*pid=$CHROMIUM_PID" "$COMPOSITOR_LOG" || true)"
R1_WINDOWS_OK=1
if [ "${CHROMIUM_WINDOWS:-0}" != 1 ]; then
    R1_WINDOWS_OK=0
    echo "real-chromium-adoption-check: R1: Chromium opened $CHROMIUM_WINDOWS toplevels, want exactly 1" >&2
else
    echo "real-chromium-adoption-check: Chromium opened exactly one toplevel"
fi
# The surface's committed size is the GEOMETRY size (the configure
# size minus Chrome's shadow margins): for --window-size=800,348 it
# is 768x306. R2 asserts the resized commit differs from it. Creation
# precedes the first buffer commit, so wait for the COMMITTED line
# rather than reading it immediately.
wait_count "$COMPOSITOR_LOG" "SURFACE COMMITTED id=$CHROMIUM_SURFACE size=" 1 60 "Chromium first commit"
CHROMIUM_SIZE="$(grep "SURFACE COMMITTED id=$CHROMIUM_SURFACE " "$COMPOSITOR_LOG" | tail -1 | grep -o "size=[0-9]*x[0-9]*" | cut -d= -f2)"
CHROMIUM_W="$(echo "$CHROMIUM_SIZE" | cut -dx -f1)"
CHROMIUM_H="$(echo "$CHROMIUM_SIZE" | cut -dx -f2)"
[ -n "$CHROMIUM_W" ] && [ -n "$CHROMIUM_H" ] \
    || fail "could not read Chromium's committed size (surface $CHROMIUM_SURFACE)"
echo "real-chromium-adoption-check: chromium committed size=${CHROMIUM_W}x${CHROMIUM_H}"
sleep 2

# --- R0: Chromium painted before adoption -------------------------------------
# R0's bbox is kept for R1: adoption must have MOVED the window, so
# the pre-adopt bbox has to extend outside the rect the window is
# adopted into (the rect origin is only known after R1's capture).
if capture_png; then
    R0_SCAN="$(pngcheck "$CAPTURE_PNG" scan $PAGE_R $PAGE_G $PAGE_B $PAGE_TOL 0 0 $OUT_W $OUT_H)"
    R0_COUNT="$(scan_value "$R0_SCAN" count)"
    R0_BBOX="$(scan_bbox "$R0_SCAN")"
    if [ "${R0_COUNT:-0}" -gt 100000 ] 2>/dev/null; then
        ledger_pass "R0 Chromium painted pre-adopt ($R0_COUNT red pixels, bbox $R0_BBOX)"
    else
        ledger_fail "R0" "too few red pixels pre-adopt (count=$R0_COUNT, want >100000) — Chromium did not paint"
    fi
else
    ledger_fail "R0" "capture before adoption failed"
fi

# --- R1: adopt + pixel proof ---------------------------------------------------
# What proves the adopted window: the SAME buffer, relocated — the
# page's red count is unchanged (the page is a sub-rectangle of the
# Chromium window: titlebar + infobar + shadow margins sit around
# it), no red/titlebar/infobar pixel falls outside the rect, and the
# union of the three bboxes starts at the rect origin — the geometry
# lands at the rect origin, not the buffer (see
# `GenesisState::adopted_buffer_origin`; before the Stage 5 fix the
# union-min was (16,162), the shadow offset, and the titlebar was
# misaligned with the rect).
R1_OK=1
ADOPT_REPLY="$(bridge_req "{\"v\": 1, \"op\": \"adopt\", \"session\": \"$BROWSER_SESSION\", \"chromium_pid\": $CHROMIUM_PID, \"rect\": {\"x\": $RECT_X, \"y\": $RECT_Y, \"w\": $RECT_W, \"h\": $RECT_H}}")" || ADOPT_REPLY=""
echo "$ADOPT_REPLY" | grep -q '"ok": *true' \
    || { R1_OK=0; echo "real-chromium-adoption-check: R1: adopt reply not ok: $ADOPT_REPLY" >&2; }
grep -q "ADOPTION ADOPTED session=$BROWSER_SESSION browser=$BROWSER_SURFACE chromium=$CHROMIUM_SURFACE pid=$CHROMIUM_PID rect=$RECT_X,$RECT_Y,$RECT_W,$RECT_H" "$COMPOSITOR_LOG" \
    || { R1_OK=0; echo "real-chromium-adoption-check: R1: ADOPTION ADOPTED marker missing" >&2; }
sleep 2
if capture_png; then
    # The Browser origin from the chrome strip's grey box (the strip
    # is the top 152 rows of the window, solid grey, unique in the
    # scene) — the rect in output coordinates is origin + (0,152).
    GRAY_BOX="$(pngcheck "$CAPTURE_PNG" bbox $CHROME_GRAY_R $CHROME_GRAY_G $CHROME_GRAY_B 0)"
    GRAY_BBOX="$(scan_bbox "$GRAY_BOX")"
    ORIGIN_X="$(echo "$GRAY_BBOX" | cut -d, -f1)"
    ORIGIN_Y="$(echo "$GRAY_BBOX" | cut -d, -f2)"
    if [ "$GRAY_BBOX" = "none" ] || [ -z "$ORIGIN_X" ] || [ -z "$ORIGIN_Y" ]; then
        R1_OK=0
        echo "real-chromium-adoption-check: R1: Browser chrome strip not found in the capture" >&2
    else
        echo "real-chromium-adoption-check: R1: browser origin=($ORIGIN_X,$ORIGIN_Y) chrome bbox=$GRAY_BBOX"
        RECT_OX=$((ORIGIN_X + RECT_X)); RECT_OY=$((ORIGIN_Y + RECT_Y))
        R1_SCAN="$(pngcheck "$CAPTURE_PNG" scan $PAGE_R $PAGE_G $PAGE_B $PAGE_TOL $RECT_OX $RECT_OY $RECT_W $RECT_H)"
        R1_COUNT="$(scan_value "$R1_SCAN" count)"
        R1_OUTSIDE="$(scan_value "$R1_SCAN" outside)"
        R1_BBOX="$(scan_bbox "$R1_SCAN")"
        R1_GRAY="$(pngcheck "$CAPTURE_PNG" scan $TITLEBAR_R $TITLEBAR_G $TITLEBAR_B $TITLEBAR_TOL $RECT_OX $RECT_OY $RECT_W $RECT_H)"
        R1_GRAY_COUNT="$(scan_value "$R1_GRAY" count)"
        R1_GRAY_OUTSIDE="$(scan_value "$R1_GRAY" outside)"
        R1_GRAY_BBOX="$(scan_bbox "$R1_GRAY")"
        R1_WHITE="$(pngcheck "$CAPTURE_PNG" scan $INFOBAR_R $INFOBAR_G $INFOBAR_B 0 $RECT_OX $RECT_OY $RECT_W $RECT_H)"
        R1_WHITE_COUNT="$(scan_value "$R1_WHITE" count)"
        R1_WHITE_OUTSIDE="$(scan_value "$R1_WHITE" outside)"
        R1_WHITE_BBOX="$(scan_bbox "$R1_WHITE")"
        echo "real-chromium-adoption-check: R1: red count=$R1_COUNT (R0=$R0_COUNT) outside=$R1_OUTSIDE bbox=$R1_BBOX"
        echo "real-chromium-adoption-check: R1: titlebar count=$R1_GRAY_COUNT outside=$R1_GRAY_OUTSIDE bbox=$R1_GRAY_BBOX"
        echo "real-chromium-adoption-check: R1: infobar count=$R1_WHITE_COUNT outside=$R1_WHITE_OUTSIDE bbox=$R1_WHITE_BBOX"
        # Same buffer relocated: the red count matches R0 within 2%.
        R0_LO=$((R0_COUNT * 98 / 100)); R0_HI=$((R0_COUNT * 102 / 100))
        { [ "${R1_COUNT:-0}" -ge "$R0_LO" ] && [ "${R1_COUNT:-0}" -le "$R0_HI" ]; } 2>/dev/null \
            || { R1_OK=0; echo "real-chromium-adoption-check: R1: red count $R1_COUNT is not R0's $R0_COUNT ±2% (same buffer, relocated)" >&2; }
        # Nothing outside the rect: no second window, clip holds.
        [ "${R1_OUTSIDE:-1}" = "0" ] 2>/dev/null \
            || { R1_OK=0; echo "real-chromium-adoption-check: R1: $R1_OUTSIDE red pixels outside the rect (second window or unclipped)" >&2; }
        [ "${R1_GRAY_COUNT:-0}" -gt 0 ] 2>/dev/null \
            || { R1_OK=0; echo "real-chromium-adoption-check: R1: no inactive-titlebar pixels inside the rect" >&2; }
        [ "${R1_GRAY_OUTSIDE:-1}" = "0" ] 2>/dev/null \
            || { R1_OK=0; echo "real-chromium-adoption-check: R1: $R1_GRAY_OUTSIDE titlebar pixels outside the rect" >&2; }
        [ "${R1_WHITE_COUNT:-0}" -gt 0 ] 2>/dev/null \
            || { R1_OK=0; echo "real-chromium-adoption-check: R1: no infobar pixels inside the rect" >&2; }
        [ "${R1_WHITE_OUTSIDE:-1}" = "0" ] 2>/dev/null \
            || { R1_OK=0; echo "real-chromium-adoption-check: R1: $R1_WHITE_OUTSIDE infobar pixels outside the rect" >&2; }
        # Geometry at the rect origin: the union of the window's
        # parts starts at (RECT_OX, RECT_OY) within 2 px (a 1 px
        # border may shift a colour's bbox by a pixel).
        UNION_MINX=""; UNION_MINY=""
        for B in "$R1_BBOX" "$R1_GRAY_BBOX" "$R1_WHITE_BBOX"; do
            BX="$(bbox_field "$B" 1)"; BY="$(bbox_field "$B" 2)"
            [ -n "$BX" ] || continue
            [ -z "$UNION_MINX" ] || [ "$BX" -lt "$UNION_MINX" ] && UNION_MINX="$BX"
            [ -z "$UNION_MINY" ] || [ "$BY" -lt "$UNION_MINY" ] && UNION_MINY="$BY"
        done
        echo "real-chromium-adoption-check: R1: window union-min=($UNION_MINX,$UNION_MINY) rect origin=($RECT_OX,$RECT_OY)"
        { within_tol "${UNION_MINX:-9999}" "$RECT_OX" 2 && within_tol "${UNION_MINY:-9999}" "$RECT_OY" 2; } \
            || { R1_OK=0; echo "real-chromium-adoption-check: R1: window does not start at the rect origin (union-min ($UNION_MINX,$UNION_MINY), want ($RECT_OX,$RECT_OY) ±2)" >&2; }
        # R0's window extended outside the rect — adoption moved it.
        R0_MAXX="$(bbox_field "${R0_BBOX:-none}" 3)"; R0_MINY="$(bbox_field "${R0_BBOX:-none}" 2)"
        { [ "${R0_MAXX:-0}" -gt $((RECT_OX + RECT_W)) ] || [ "${R0_MINY:-9999}" -lt "$RECT_OY" ]; } 2>/dev/null \
            || { R1_OK=0; echo "real-chromium-adoption-check: R1: pre-adopt bbox $R0_BBOX never extended outside the rect — adoption did not move the window" >&2; }
        # Chrome strip intact above the rect, desktop clear below the
        # window, desktop clear right of the window.
        pngcheck "$CAPTURE_PNG" pixel-near $((ORIGIN_X + 400)) $((ORIGIN_Y + 76)) $CHROME_GRAY_R $CHROME_GRAY_G $CHROME_GRAY_B 0 \
            || { R1_OK=0; echo "real-chromium-adoption-check: R1: chrome strip pixel wrong" >&2; }
        pngcheck "$CAPTURE_PNG" pixel-near $((ORIGIN_X + 400)) $((ORIGIN_Y + BROWSER_H + 10)) $CLEAR_R $CLEAR_G $CLEAR_B 2 \
            || { R1_OK=0; echo "real-chromium-adoption-check: R1: pixel below the Browser window is not the desktop clear colour" >&2; }
        pngcheck "$CAPTURE_PNG" pixel-near 1000 700 $CLEAR_R $CLEAR_G $CLEAR_B 2 \
            || { R1_OK=0; echo "real-chromium-adoption-check: R1: desktop pixel wrong" >&2; }
    fi
else
    R1_OK=0
    echo "real-chromium-adoption-check: R1: capture after adoption failed" >&2
fi
if [ "$R1_OK" = 1 ] && [ "$R1_WINDOWS_OK" = 1 ]; then
    ledger_pass "R1 adopt ok:true, same buffer relocated ($R1_COUNT red px, R0=$R0_COUNT), window at the rect origin, zero red/titlebar/infobar outside it, one Chromium toplevel"
else
    ledger_fail "R1" "see failures above"
fi

# --- R2: resize via compositor configure ---------------------------------------
# Real Chromium must honour the compositor configure and commit a new
# size (up to RESIZE_TIMEOUT; cold llvmpipe is slow). The committed
# size is NOT the configure size: Chrome commits its geometry, which
# is the configure size minus the shadow margins — for the pinned
# snapshot the resize configure 600x348 commits 600x338. So wait for
# ANY new COMMITTED line for the surface, read the size it reports,
# and assert only that it changed from the initial size and that its
# width is the configure width. A timeout is an R2 FAIL, not an
# abort — R3/R4 can still run.
R2_OK=1
ORIGIN_X="${ORIGIN_X:-0}"; ORIGIN_Y="${ORIGIN_Y:-0}"
RESIZE_REPLY="$(bridge_req "{\"v\": 1, \"op\": \"resize\", \"session\": \"$BROWSER_SESSION\", \"rect\": {\"x\": $RECT_X, \"y\": $RECT_Y, \"w\": $RESIZED_W, \"h\": $RECT_H}}")" || RESIZE_REPLY=""
echo "$RESIZE_REPLY" | grep -q '"ok": *true' \
    || { R2_OK=0; echo "real-chromium-adoption-check: R2: resize reply not ok: $RESIZE_REPLY" >&2; }
grep -q "ADOPTION RESIZED session=$BROWSER_SESSION size=${RESIZED_W}x${RECT_H} rect=$RECT_X,$RECT_Y,$RESIZED_W,$RECT_H" "$COMPOSITOR_LOG" \
    || { R2_OK=0; echo "real-chromium-adoption-check: R2: ADOPTION RESIZED marker missing" >&2; }
R2_COMMITTED=0
R2_DEADLINE=$((SECONDS + RESIZE_TIMEOUT))
while [ "$SECONDS" -lt "$R2_DEADLINE" ]; do
    R2_LAST="$(grep "SURFACE COMMITTED id=$CHROMIUM_SURFACE " "$COMPOSITOR_LOG" 2>/dev/null | tail -1 | grep -o "size=[0-9]*x[0-9]*" | cut -d= -f2)"
    if [ -n "$R2_LAST" ] && [ "$R2_LAST" != "${CHROMIUM_W}x${CHROMIUM_H}" ]; then R2_COMMITTED=1; break; fi
    sleep 0.5
done
if [ "$R2_COMMITTED" != 1 ]; then
    R2_OK=0
    echo "real-chromium-adoption-check: R2: Chromium never committed a new size after resize" >&2
else
    R2_SIZE="$(grep "SURFACE COMMITTED id=$CHROMIUM_SURFACE " "$COMPOSITOR_LOG" | tail -1 | grep -o "size=[0-9]*x[0-9]*" | cut -d= -f2)"
    R2_W="$(echo "$R2_SIZE" | cut -dx -f1)"; R2_H="$(echo "$R2_SIZE" | cut -dx -f2)"
    echo "real-chromium-adoption-check: R2: chromium committed size=${R2_SIZE} (initial ${CHROMIUM_W}x${CHROMIUM_H})"
    [ "$R2_SIZE" != "${CHROMIUM_W}x${CHROMIUM_H}" ] \
        || { R2_OK=0; echo "real-chromium-adoption-check: R2: committed size did not change after resize" >&2; }
    [ "${R2_W:-0}" = "$RESIZED_W" ] 2>/dev/null \
        || { R2_OK=0; echo "real-chromium-adoption-check: R2: committed width $R2_W is not the configure width $RESIZED_W" >&2; }
    { [ "${R2_H:-0}" -gt 0 ] && [ "${R2_H:-0}" -le "$RECT_H" ]; } 2>/dev/null \
        || { R2_OK=0; echo "real-chromium-adoption-check: R2: committed height $R2_H outside (0, $RECT_H]" >&2; }
fi
echo "real-chromium-adoption-check: R2: committed sizes for chromium surface $CHROMIUM_SURFACE:"
grep "SURFACE COMMITTED id=$CHROMIUM_SURFACE " "$COMPOSITOR_LOG" | tail -5 || true
sleep 2
if capture_png; then
    R2_SCAN="$(pngcheck "$CAPTURE_PNG" scan $PAGE_R $PAGE_G $PAGE_B $PAGE_TOL $((ORIGIN_X + RECT_X)) $((ORIGIN_Y + RECT_Y)) $RESIZED_W $RECT_H)"
    R2_COUNT="$(scan_value "$R2_SCAN" count)"
    R2_OUTSIDE="$(scan_value "$R2_SCAN" outside)"
    R2_BBOX="$(scan_bbox "$R2_SCAN")"
    R2_GRAY="$(pngcheck "$CAPTURE_PNG" scan $TITLEBAR_R $TITLEBAR_G $TITLEBAR_B $TITLEBAR_TOL $((ORIGIN_X + RECT_X)) $((ORIGIN_Y + RECT_Y)) $RESIZED_W $RECT_H)"
    R2_GRAY_COUNT="$(scan_value "$R2_GRAY" count)"
    R2_GRAY_OUTSIDE="$(scan_value "$R2_GRAY" outside)"
    R2_GRAY_BBOX="$(scan_bbox "$R2_GRAY")"
    R2_WHITE="$(pngcheck "$CAPTURE_PNG" scan $INFOBAR_R $INFOBAR_G $INFOBAR_B 0 $((ORIGIN_X + RECT_X)) $((ORIGIN_Y + RECT_Y)) $RESIZED_W $RECT_H)"
    R2_WHITE_OUTSIDE="$(scan_value "$R2_WHITE" outside)"
    R2_WHITE_BBOX="$(scan_bbox "$R2_WHITE")"
    echo "real-chromium-adoption-check: R2: red count=$R2_COUNT (R1=$R1_COUNT) outside=$R2_OUTSIDE bbox=$R2_BBOX"
    echo "real-chromium-adoption-check: R2: titlebar count=$R2_GRAY_COUNT outside=$R2_GRAY_OUTSIDE bbox=$R2_GRAY_BBOX"
    # The window narrowed: red count dropped from R1 but stays large,
    # nothing red/titlebar/infobar outside the new rect, and the
    # window still starts at the rect origin (resize must not move
    # it — the geometry-offset fix is placement, not size).
    { [ "${R2_COUNT:-0}" -lt "${R1_COUNT:-0}" ] && [ "${R2_COUNT:-0}" -gt 100000 ]; } 2>/dev/null \
        || { R2_OK=0; echo "real-chromium-adoption-check: R2: red count $R2_COUNT is not below R1's $R1_COUNT and above 100000" >&2; }
    [ "${R2_OUTSIDE:-1}" = "0" ] 2>/dev/null \
        || { R2_OK=0; echo "real-chromium-adoption-check: R2: $R2_OUTSIDE red pixels outside the resized rect" >&2; }
    [ "${R2_GRAY_OUTSIDE:-1}" = "0" ] 2>/dev/null \
        || { R2_OK=0; echo "real-chromium-adoption-check: R2: $R2_GRAY_OUTSIDE titlebar pixels outside the resized rect" >&2; }
    [ "${R2_WHITE_OUTSIDE:-1}" = "0" ] 2>/dev/null \
        || { R2_OK=0; echo "real-chromium-adoption-check: R2: $R2_WHITE_OUTSIDE infobar pixels outside the resized rect" >&2; }
    R2_MINX=""; R2_MINY=""
    for B in "$R2_BBOX" "$R2_GRAY_BBOX" "$R2_WHITE_BBOX"; do
        BX="$(bbox_field "$B" 1)"; BY="$(bbox_field "$B" 2)"
        [ -n "$BX" ] || continue
        [ -z "$R2_MINX" ] || [ "$BX" -lt "$R2_MINX" ] && R2_MINX="$BX"
        [ -z "$R2_MINY" ] || [ "$BY" -lt "$R2_MINY" ] && R2_MINY="$BY"
    done
    echo "real-chromium-adoption-check: R2: window union-min=($R2_MINX,$R2_MINY) rect origin=($((ORIGIN_X + RECT_X)),$((ORIGIN_Y + RECT_Y)))"
    { within_tol "${R2_MINX:-9999}" $((ORIGIN_X + RECT_X)) 2 && within_tol "${R2_MINY:-9999}" $((ORIGIN_Y + RECT_Y)) 2; } \
        || { R2_OK=0; echo "real-chromium-adoption-check: R2: window moved on resize (union-min ($R2_MINX,$R2_MINY))" >&2; }
    # The freed strip (x 600..800 of the content area) shows the
    # Browser's own content colour again; the kept half stays red.
    pngcheck "$CAPTURE_PNG" pixel-near $((ORIGIN_X + 700)) $((ORIGIN_Y + 326)) $CONTENT_R $CONTENT_G $CONTENT_B 0 \
        || { R2_OK=0; echo "real-chromium-adoption-check: R2: freed strip is not the Browser content colour" >&2; }
    pngcheck "$CAPTURE_PNG" pixel-near $((ORIGIN_X + 300)) $((ORIGIN_Y + 326)) $PAGE_R $PAGE_G $PAGE_B $PAGE_TOL \
        || { R2_OK=0; echo "real-chromium-adoption-check: R2: kept half of the rect is not red" >&2; }
else
    R2_OK=0
    echo "real-chromium-adoption-check: R2: capture after resize failed" >&2
fi
if [ "$R2_OK" = 1 ]; then
    ledger_pass "R2 resize via compositor configure ($R2_SIZE committed), red narrowed inside the new rect, window unmoved"
else
    ledger_fail "R2" "see failures above"
fi

# --- R3: Chromium death empties the content area -------------------------------
R3_OK=1
kill -TERM "$CHROMIUM_PID" 2>/dev/null || R3_OK=0
wait_for "$COMPOSITOR_LOG" "ADOPTION CHROMIUM DIED session=$BROWSER_SESSION chromium=$CHROMIUM_SURFACE pid=$CHROMIUM_PID" "ADOPTION CHROMIUM DIED" \
    || R3_OK=0
bridge_event_wait '"event": *"chromium_died"' "chromium_died bridge event" \
    || { R3_OK=0; echo "real-chromium-adoption-check: R3: no chromium_died event on the Runtime connection" >&2; }
sleep 1
if capture_png; then
    R3_SCAN="$(pngcheck "$CAPTURE_PNG" scan $PAGE_R $PAGE_G $PAGE_B $PAGE_TOL 0 0 $OUT_W $OUT_H)"
    R3_COUNT="$(scan_value "$R3_SCAN" count)"
    echo "real-chromium-adoption-check: R3: red count after death=$R3_COUNT"
    [ "${R3_COUNT:-1}" = "0" ] 2>/dev/null \
        || { R3_OK=0; echo "real-chromium-adoption-check: R3: $R3_COUNT red pixels remain after Chromium died" >&2; }
    pngcheck "$CAPTURE_PNG" pixel-near $((ORIGIN_X + 300)) $((ORIGIN_Y + 326)) $CONTENT_R $CONTENT_G $CONTENT_B 0 \
        || { R3_OK=0; echo "real-chromium-adoption-check: R3: content area did not empty back to the Browser content colour" >&2; }
else
    R3_OK=0
    echo "real-chromium-adoption-check: R3: capture after Chromium death failed" >&2
fi
PING_REPLY="$(bridge_req '{"v": 1, "op": "ping"}')" || PING_REPLY=""
echo "$PING_REPLY" | grep -q '"ok": *true' \
    || { R3_OK=0; echo "real-chromium-adoption-check: R3: bridge ping after Chromium death failed: $PING_REPLY" >&2; }
kill -0 "$BROWSER_PID" 2>/dev/null \
    || { R3_OK=0; echo "real-chromium-adoption-check: R3: Browser stand-in died with Chromium" >&2; }
kill -0 "$COMPOSITOR_PID" 2>/dev/null \
    || { R3_OK=0; echo "real-chromium-adoption-check: R3: compositor died with Chromium" >&2; }
if [ "$R3_OK" = 1 ]; then
    ledger_pass "R3 Chromium death: chromium_died event + marker, content area emptied, Browser + compositor alive"
else
    ledger_fail "R3" "see failures above"
fi

# --- R4: clean shutdown, no orphans --------------------------------------------
R4_OK=1
CLOSE_REPLY="$(bridge_req "{\"v\": 1, \"op\": \"close\", \"session\": \"$BROWSER_SESSION\"}")" || CLOSE_REPLY=""
echo "$CLOSE_REPLY" | grep -q '"ok": *true' \
    || { R4_OK=0; echo "real-chromium-adoption-check: R4: bridge close failed: $CLOSE_REPLY" >&2; }
deadline=$((SECONDS + TIMEOUT))
while kill -0 "$BROWSER_PID" 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
kill -0 "$BROWSER_PID" 2>/dev/null && { R4_OK=0; echo "real-chromium-adoption-check: R4: Browser stand-in did not exit after bridge close" >&2; }
kill -TERM "$COMPOSITOR_PID"
deadline=$((SECONDS + TIMEOUT))
while kill -0 "$COMPOSITOR_PID" 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
kill -0 "$COMPOSITOR_PID" 2>/dev/null && fail "compositor did not exit on SIGTERM"
wait "$COMPOSITOR_PID"
COMPOSITOR_STATUS=$?
[ "$COMPOSITOR_STATUS" -eq 0 ] \
    || { R4_OK=0; echo "real-chromium-adoption-check: R4: compositor exited with status $COMPOSITOR_STATUS, not 0" >&2; }
grep -q "SEVYN SHUTDOWN clean" "$COMPOSITOR_LOG" \
    || { R4_OK=0; echo "real-chromium-adoption-check: R4: missing SHUTDOWN clean marker" >&2; }
if pgrep -x chrome >/dev/null 2>&1; then
    R4_OK=0
    echo "real-chromium-adoption-check: R4: chrome processes remain after shutdown" >&2
fi
if pgrep -f "sevyn-test-clien[t]" >/dev/null 2>&1; then
    R4_OK=0
    echo "real-chromium-adoption-check: R4: test client processes remain after shutdown" >&2
fi
if [ "$R4_OK" = 1 ]; then
    ledger_pass "R4 clean shutdown, no orphans"
else
    ledger_fail "R4" "see failures above"
fi

echo "real-chromium-adoption-check: PASS=$PASS_COUNT FAIL=$FAIL_COUNT"
if [ "$FAIL_COUNT" -gt 0 ]; then
    echo "real-chromium-adoption-check: FAIL — $FAIL_COUNT case(s) failed" >&2
    exit 1
fi
echo "real-chromium-adoption-check: PASS — real Chromium adopted into the Browser rect (pixels inside, clipped, no second window), resized via compositor configure, death empties the content area"
