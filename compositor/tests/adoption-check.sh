#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# adoption-check.sh — the Stage 4 adoption milestone harness.
#
# Proves the Stage 4 adoption mechanism (COMPOSITOR_PLAN.md, Stage 4)
# with real Wayland clients: a Browser stand-in, a Chromium stand-in,
# clipboard + pointer-lock stand-ins, and a bridge-socket client
# speaking the adoption IPC. Two backends:
#
#   nested     (default) boots the nested compositor under headless
#              Weston (Xvfb + xdotool input) — the CI backend.
#   standalone boots a QEMU/KVM guest on the KVM box and drives the
#              standalone compositor there (QMP input) — the DRM proof.
#              Usage: adoption-check.sh --standalone (on the KVM box).
#
# Milestones (each recorded PASS / FAIL / NOT-PROVEN in the ledger at
# the end; exit 0 when nothing FAILED and no NOT-PROVEN falls outside
# the declared Track 3 gaps — A10 pointer lock — which is missing by
# design on this branch and named in the PR; any other NOT-PROVEN
# still fails the run, so a PASS regressing to NOT-PROVEN cannot slip
# through):
#
#   A1  clients up: Browser + Chromium stand-ins map, associate, commit
#   A2  hostile pre-adopt: malformed adopt, wrong version, missing
#       fields, unknown session, dead pid, rect outside the window —
#       all refused (ok:false), compositor stays alive, no ADOPTED
#   A3  app_id spoof: a client claiming app_id `chromium` with the
#       WRONG pid is never adopted; only the Runtime-declared pid is
#   A4  adopt: bridge adopt ok:true + `ADOPTION ADOPTED session=..
#       browser=.. chromium=.. pid=.. rect=..`
#   A5  geometry + clipping (hit-test): content-centre probe hits the
#       adopted surface with rect-relative local coords; chrome probes
#       hit the Browser; probes just outside the rect never hit the
#       adopted surface; the old standalone position no longer does
#   A6  move: bridge move changes the rect; the new centre hits the
#       adopted surface, the old centre no longer does
#   A7  resize: bridge resize sends xdg_toplevel.configure to the
#       adopted client, which resizes (CHROMIUM RESIZED)
#   A8  focus: click content -> adopted surface focus (keys reach it);
#       click chrome -> Browser focus (keys reach it)
#   A9  clipboard: copy/paste between two clients (wl_data_device)
#   A10 pointer lock + relative motion (pointer-constraints /
#       relative-pointer)
#   A11 manager: bind + list (Browser + adopted surface listed) +
#       direct manager activation of the adopted surface is IGNORED +
#       manager close of a throwaway window
#   A12 adopted-client death: chromium_died event, content area
#       empties (probes hit the Browser), Browser window survives
#   A13 re-adoption after death works
#   A14 close kills the adopted client: bridge close of the Browser
#       session exits both clients, both sessions recorded stopped
#   A15 Runtime death -> degraded: dropping the Runtime bridge
#       connection refuses new adoptions, the scene keeps presenting
#   A16 clean shutdown, no orphans
#
# What this check proves: adoption function + routing, not fps and not
# pixel-perfect clipping. Clipping is proven by hit-test (probes just
# outside the rect never reach the adopted surface) plus the adopted
# surface's configure size equalling the rect; a screendump pixel
# probe is Stage 5's genesis-check.mjs job, not this harness's. The
# same-frame / zero-drift property of a Browser-window move is
# structural (one scene, one output position — the plan's argument);
# the harness proves the rect update takes effect immediately after
# the move response, with no intermediate frame answering at the old
# position. The 60 fps bar is the GF65's proof, never this harness's.
#
# Contracts this harness asserts (from the task brief; the coordinator
# resolves drift against the adoption/manager tracks):
#
#   * Bridge IPC (v1, newline-delimited JSON on
#     $XDG_RUNTIME_DIR/sevyn-compositor.sock):
#       adopt   {"v":1,"op":"adopt","session":"..","chromium_pid":N,
#                "rect":{"x":0,"y":152,"w":800,"h":348}}
#       move    {"v":1,"op":"move","session":"..","rect":{...}}
#       resize  {"v":1,"op":"resize","session":"..","rect":{...}}
#       release {"v":1,"op":"release","session":".."}
#     Responses: {"v":1,"ok":true,"op":"..","session":".."} /
#                {"v":1,"ok":false,"op":"..","error":".."}
#     Events (on bridge connections): {"v":1,"event":"chromium_died",
#                "session":".."}
#   * Markers: `ADOPTION ADOPTED session=.. browser=.. chromium=..
#     pid=.. rect=..`, `ADOPTION RELEASED session=..`
#   * The harness holds ONE persistent bridge connection for the whole
#     run (bridge.py `runtime` mode) — that connection IS the Runtime.
#     One-shot connections are used only for the degraded-state test
#     (A15), where a fresh connection must be refused after the
#     Runtime's connection drops.
#   * Manager mode (manager track, stage4/manager @ 7dc862ca — the
#     REAL interface, not the assumed one): the test client's
#     `--manager` mode binds zwlr_foreign_toplevel_manager_v1 (v2)
#     + ext-foreign-toplevel-list-v1, lists the open toplevels, and
#     exercises one request per run on a target app_id
#     (SEVYN_MANAGER_TARGET_APP_ID / SEVYN_MANAGER_ACTION =
#     activate|close|...). Client markers: `MANAGER TOPLEVEL
#     app_id=.. title=.. state=..`, `MANAGER ACTIVATE|CLOSE
#     app_id=..`, `LIST TOPLEVEL identifier=.. title=.. app_id=..`,
#     `MANAGER DONE`. Compositor markers: `MANAGER ACTIVATE
#     id=..`, `MANAGER ACTIVATE IGNORED id=.. adopted`,
#     `MANAGER CLOSE id=.. ok=..`, `MANAGER TOPLEVEL CLOSED id=..`.
#     The harness builds the manager client from the wlr + ext-list
#     protocol sources generated out of the cargo registry
#     (wayland-protocols-wlr 0.3.12, wayland-protocols 0.32.13) with
#     -DHAVE_WLR_FOREIGN_TOPLEVEL; if the sources are unavailable,
#     A11 is NOT-PROVEN. SEAM (resolved at Stage 4 integration):
#     the manager's `is_adopted` is Track 1's real adoption
#     registry (adoption.rs), so a direct activation of an adopted
#     surface IS ignored on this tree — and if it goes through
#     instead, that is a FAIL, not a named gap.
#   * A10 NOT-PROVEN (not FAIL) when the compositor does not
#     advertise the protocol globals at all — a missing capability is
#     a named gap, not a wrong behaviour. Any other miss is a FAIL.
#     (A9 clipboard used to share this escape; it landed with the
#     data-device implementation, so a missing data-device global
#     is a FAIL now.)
#
# Usage:
#   compositor/tests/adoption-check.sh [--standalone]
#
# Environment (all optional):
#   SEVYN_COMPOSITOR_BIN  path to sevyn-compositor (nested: default
#                          compositor/target/debug/sevyn-compositor;
#                          standalone: compositor/target/release/...)
#   SEVYN_TEST_CLIENT_SRC path to test-client.c
#                          (default: compositor/tests/test-client.c)
#   ADOPTION_DISPLAY_NUMBER  X display for Xvfb (nested, default: 99)
#   ADOPTION_TIMEOUT      seconds per wait step (default: 30 nested,
#                          60 standalone)
#   SEVYN_CHECK_DIR       (standalone) host dir holding the cloud image
#                          + run logs (default: $HOME/genesis-stage3-check)
#   SEVYN_GUEST_QCOW2     (standalone) base guest disk
#   SEVYN_SSH_PORT        (standalone) host SSH port (default: 2225)
#   SEVYN_GUEST_MEM / SEVYN_GUEST_CPUS  (standalone, defaults 4096/4)
#
# Exit 0 when every milestone is proven, non-zero otherwise. Run logs
# are preserved in $SEVYN_CHECK_DIR/last-adoption-run/ (standalone)
# or printed on failure (nested).

set -u
cd "$(dirname "$0")/../.." || exit 1

BACKEND="nested"
if [ "${1:-}" = "--standalone" ]; then BACKEND="standalone"; shift; fi
if [ "${1:-}" = "--nested" ]; then shift; fi

if [ "$BACKEND" = "nested" ]; then
    COMPOSITOR_BIN="${SEVYN_COMPOSITOR_BIN:-compositor/target/debug/sevyn-compositor}"
else
    COMPOSITOR_BIN="${SEVYN_COMPOSITOR_BIN:-compositor/target/release/sevyn-compositor}"
fi
CLIENT_SRC="${SEVYN_TEST_CLIENT_SRC:-compositor/tests/test-client.c}"
DISPLAY_NUMBER="${ADOPTION_DISPLAY_NUMBER:-99}"
if [ "$BACKEND" = "nested" ]; then TIMEOUT="${ADOPTION_TIMEOUT:-30}"; else TIMEOUT="${ADOPTION_TIMEOUT:-60}"; fi
# Standalone host-side settings (mirrors standalone-check.sh).
CHECK_DIR="${SEVYN_CHECK_DIR:-$HOME/genesis-stage3-check}"
BASE_QCOW2="${SEVYN_GUEST_QCOW2:-$CHECK_DIR/noble.qcow2}"
SSH_PORT="${SEVYN_SSH_PORT:-2225}"
GUEST_MEM="${SEVYN_GUEST_MEM:-4096}"
GUEST_CPUS="${SEVYN_GUEST_CPUS:-4}"
SOCKET_NAME="sevyn-adoption-test"
# The Browser stand-in's geometry (COMPOSITOR_PLAN.md, Stage 5).
BROWSER_W=800; BROWSER_H=500; CHROME_H=152
RECT_X=0; RECT_Y=152; RECT_W=800; RECT_H=348
OUT_W=1280; OUT_H=800

WORK="$(mktemp -d /tmp/sevyn-adoption-check.XXXXXX)"
RUNTIME_DIR="$WORK/run"
mkdir -p "$RUNTIME_DIR"
COMPOSITOR_LOG="$WORK/compositor.log"
COMPOSITOR_ERR="$WORK/compositor.err"
BRIDGE_IN="$WORK/bridge-in.txt"
BRIDGE_OUT="$WORK/bridge-out.txt"
: > "$BRIDGE_IN"; : > "$BRIDGE_OUT"
# Milestone ledger: lines "A<n> PASS|FAIL|NOT-PROVEN <note>".
LEDGER="$WORK/ledger.txt"
: > "$LEDGER"
# Client logs (nested: local files; standalone: guest /tmp files —
# the same basenames, reached through the log_* helpers).
BROWSER_LOG_NAME="client-browser.log"
CHROMIUM_LOG_NAME="client-chromium.log"
SPOOF_LOG_NAME="client-spoof.log"
COPY_LOG_NAME="client-copy.log"
PASTE_LOG_NAME="client-paste.log"
LOCK_LOG_NAME="client-lock.log"
M1_LOG_NAME="client-m1.log"
CHROMIUM2_LOG_NAME="client-chromium2.log"
CHROMIUM3_LOG_NAME="client-chromium3.log"
BROWSER2_LOG_NAME="client-browser2.log"

record() { # <id> <PASS|FAIL|NOT-PROVEN> <note>
    echo "$1 $2 $3" >> "$LEDGER"
    echo "adoption-check: [$1] $2 — $3"
}

fail() {
    echo "adoption-check: FAIL: $*" >&2
    echo "adoption-check: --- ledger ---" >&2
    cat "$LEDGER" >&2 || true
    echo "adoption-check: --- compositor.log (tail) ---" >&2
    tail -40 "$COMPOSITOR_LOG" 2>/dev/null >&2 || true
    echo "adoption-check: --- compositor.err (tail) ---" >&2
    tail -20 "$COMPOSITOR_ERR" 2>/dev/null >&2 || true
    if [ "$BACKEND" = "standalone" ]; then preserve_logs; fi
    exit 1
}

# --- Backend primitives -----------------------------------------------------
# log_path <basename>: where a log file lives for this backend.
# Nested: $WORK/<basename>. Standalone: /tmp/<basename> in the guest.
log_path() {
    if [ "$BACKEND" = "nested" ]; then echo "$WORK/$1"; else echo "/tmp/$1"; fi
}
# log_grep <basename> <pattern>: true when the pattern is in the log.
log_grep() {
    if [ "$BACKEND" = "nested" ]; then grep -q -- "$2" "$WORK/$1" 2>/dev/null;
    else ssh_guest "grep -q -- '$2' '/tmp/$1' 2>/dev/null"; fi
}
# log_count <basename> <pattern>: number of matching lines.
log_count() {
    # NB: grep -c prints the count (0) even when it finds nothing, so
    # the fallback must not echo a second 0 — `|| true` only swallows
    # the exit status.
    if [ "$BACKEND" = "nested" ]; then grep -c -- "$2" "$WORK/$1" 2>/dev/null || true;
    else ssh_guest "grep -c -- '$2' '/tmp/$1' 2>/dev/null || true" | tr -d ' '; fi
}
# log_last <basename> <pattern>: the last matching line (or empty).
log_last() {
    if [ "$BACKEND" = "nested" ]; then grep -- "$2" "$WORK/$1" 2>/dev/null | tail -1;
    else ssh_guest "grep -- '$2' '/tmp/$1' 2>/dev/null | tail -1"; fi
}
# log_fetch <basename> <local-dest>: pull a guest log to the workdir.
log_fetch() {
    if [ "$BACKEND" = "nested" ]; then cp -a "$WORK/$1" "$2" 2>/dev/null || true;
    else scp_from_guest "sevyn@127.0.0.1:/tmp/$1" "$2" 2>/dev/null || true; fi
}
# wait_log <basename> <pattern> [description]: poll until the pattern
# appears in the log (either backend).
wait_log() {
    local file="$1" pattern="$2" what="${3:-$pattern}"
    local deadline=$((SECONDS + TIMEOUT))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if log_grep "$file" "$pattern"; then return 0; fi
        sleep 0.5
    done
    return 1
}
# wait_log_new <basename> <pattern> <before-count> [description]: poll
# until the match count grows past before-count.
wait_log_new() {
    local file="$1" pattern="$2" before="$3" what="${4:-$pattern}"
    local deadline=$((SECONDS + TIMEOUT))
    while [ "$SECONDS" -lt "$deadline" ]; do
        local now
        now="$(log_count "$file" "$pattern")"
        now="${now:-0}"
        if [ "$now" -gt "$before" ] 2>/dev/null; then return 0; fi
        sleep 0.5
    done
    return 1
}

# --- Standalone (guest) plumbing --------------------------------------------
# Set once the guest is up; nested mode never calls these.
SSH_OPTS=""
ssh_guest() { ssh $SSH_OPTS sevyn@127.0.0.1 "$@"; }
scp_to_guest() { scp -P "$SSH_PORT" -i "$WORK/id_ed25519" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o LogLevel=ERROR "$@"; }
scp_from_guest() { scp -P "$SSH_PORT" -i "$WORK/id_ed25519" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o LogLevel=ERROR "$@"; }
preserve_logs() {
    local dest="$CHECK_DIR/last-adoption-run"
    mkdir -p "$dest"
    for f in qemu.log guest-serial.log compositor.log compositor.err compositor.exit bridge-out.txt ledger.txt measurements.txt; do
        [ -f "$WORK/$f" ] && cp -a "$WORK/$f" "$dest/$f" 2>/dev/null || true
    done
    for f in client-browser.log client-chromium.log client-spoof.log client-copy.log client-paste.log client-lock.log client-m1.log client-chromium2.log client-browser2.log client-chromium3.log manager-list.log manager-activate-chromium.log manager-activate-browser.log manager-close-m1.log; do
        log_fetch "$f" "$dest/$f"
    done
    scp_from_guest "sevyn@127.0.0.1:/tmp/compositor.log" "$dest/compositor.log" 2>/dev/null || true
    scp_from_guest "sevyn@127.0.0.1:/tmp/genesis.log" "$dest/genesis.log" 2>/dev/null || true
    echo "adoption-check: run logs preserved in $dest"
}

# --- Bridge client (bridge.py) ----------------------------------------------
# The harness's bridge-socket client: one persistent "Runtime"
# connection (`runtime` mode — reads JSON request lines from an infile,
# appends response/event lines to an outfile) plus one-shot requests
# for the degraded-state test. Generated into $WORK (nested) and
# scp'd to the guest (standalone).
cat >"$WORK/bridge.py" <<'PYEOF'
# SPDX-License-Identifier: GPL-3.0-or-later
"""bridge.py — the adoption harness's bridge-socket client.

Usage:
  bridge.py runtime <socket> <infile> <outfile>
      Hold one persistent connection (the Runtime). Append one JSON
      request line to <infile> per request; response lines (and event
      lines such as chromium_died) are appended to <outfile>.
  bridge.py req <socket> <json>
      One-shot: connect, send one request, print the first response
      line containing '"ok"' (or TIMEOUT / NO-RESPONSE).

A request line is one JSON object; the bridge speaks newline-delimited
JSON, protocol version 1 (see adoption-check.sh header).
"""
import json
import os
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
    mode = sys.argv[1]
    if mode == "runtime":
        sock_path, infile, outfile = sys.argv[2], sys.argv[3], sys.argv[4]
        # Control lines in the infile (handled locally, never
        # forwarded to the compositor):
        #   {"__bridge__": "drop"}       close the socket (the Runtime dies)
        #   {"__bridge__": "reconnect"}  open a fresh socket, same pid
        # The Runtime pid is stable across drop/reconnect, so a
        # compositor launched with --runtime-pid=<this pid> keeps
        # accepting this process as the Runtime while refusing any
        # other pid (the A15 degraded proof). Connect with retries:
        # the harness starts bridge.py before the compositor, so the
        # socket may not exist yet.
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
            # New requests in the infile?
            try:
                with open(infile, "r") as f:
                    f.seek(offset)
                    for line in f:
                        line = line.strip()
                        if not line:
                            continue
                        try:
                            ctl = json.loads(line)
                        except ValueError:
                            ctl = None
                        if isinstance(ctl, dict) and ctl.get("__bridge__") == "drop":
                            if sock is not None:
                                try:
                                    sock.shutdown(socket.SHUT_RDWR)
                                except OSError:
                                    pass
                                sock.close()
                                sock = None
                                buf = b""
                            continue
                        if isinstance(ctl, dict) and ctl.get("__bridge__") == "reconnect":
                            if sock is None:
                                sock = connect_retry()
                                buf = b""
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
    elif mode == "req":
        sock_path, request = sys.argv[2], sys.argv[3]
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(10)
        try:
            sock.connect(sock_path)
        except OSError as e:
            print("CONNECT-FAILED %s" % e)
            return 1
        sock.sendall((request + "\n").encode())
        buf = b""
        end = time.time() + 8
        while time.time() < end:
            lines, buf = read_lines(sock, buf, 0.5)
            for line in lines:
                if '"ok"' in line:
                    print(line)
                    return 0
                # Events ride the same connection; note and keep waiting.
                print("EVENT %s" % line, file=sys.stderr)
        print("NO-RESPONSE")
        return 1
    else:
        print("unknown mode: %s" % mode, file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
PYEOF

# bridge_req <json>: send one request over the persistent Runtime
# connection and wait for its response line. Prints the response.
# Returns 0 when a response arrived, 1 on timeout. The request is
# appended to the infile (local or guest, per backend).
bridge_req() {
    local json="$1"
    local before
    before="$(log_count_bridge_out)"
    if [ "$BACKEND" = "nested" ]; then
        printf '%s\n' "$json" >> "$BRIDGE_IN"
    else
        printf '%s\n' "$json" | ssh_guest "cat >> /tmp/bridge-in.txt"
    fi
    local deadline=$((SECONDS + TIMEOUT))
    while [ "$SECONDS" -lt "$deadline" ]; do
        local line
        line="$(bridge_out_new "$before")"
        if [ -n "$line" ]; then
            printf '%s\n' "$line"
            return 0
        fi
        sleep 0.2
    done
    return 1
}
# bridge_out_new <before-count>: first new '"ok"' line in the outfile.
bridge_out_new() {
    local before="$1"
    if [ "$BACKEND" = "nested" ]; then
        tail -n +"$((before + 1))" "$BRIDGE_OUT" 2>/dev/null | grep -m1 '"ok"' || true
    else
        ssh_guest "tail -n +$((before + 1)) /tmp/bridge-out.txt 2>/dev/null | grep -m1 '\"ok\"' || true"
    fi
}
# log_count_bridge_out: lines in the bridge outfile.
log_count_bridge_out() {
    if [ "$BACKEND" = "nested" ]; then wc -l < "$BRIDGE_OUT" | tr -d ' ';
    else ssh_guest "wc -l < /tmp/bridge-out.txt" | tr -d ' '; fi
}
# bridge_req_ok <json> [what]: send + assert the response is ok:true.
# Returns 0 on ok:true (and prints the response), 1 otherwise.
bridge_req_ok() {
    local resp
    resp="$(bridge_req "$1")" || return 1
    echo "$resp" | grep -q '"ok": *true' || return 1
    printf '%s\n' "$resp"
    return 0
}
# bridge_req_refused <json> [what]: send + assert ok:false.
# Returns 0 on ok:false (and prints the response), 1 otherwise.
bridge_req_refused() {
    local resp
    resp="$(bridge_req "$1")" || return 1
    echo "$resp" | grep -q '"ok": *false' || return 1
    printf '%s\n' "$resp"
    return 0
}
# bridge_control <drop|reconnect>: send a bridge.py control line over the
# persistent Runtime connection (handled locally by bridge.py, never
# forwarded to the compositor). drop closes the socket — the Runtime
# dies, same pid; reconnect opens a fresh socket from the same process,
# so a --runtime-pid compositor accepts it again. Produces no response:
# callers wait on compositor markers, not bridge_req.
bridge_control() {
    local line="{\"__bridge__\": \"$1\"}"
    if [ "$BACKEND" = "nested" ]; then
        printf '%s\n' "$line" >> "$BRIDGE_IN"
    else
        printf '%s\n' "$line" | ssh_guest "cat >> /tmp/bridge-in.txt"
    fi
}

# bridge_oneshot <json>: one-shot request on a fresh connection.
bridge_oneshot() {
    if [ "$BACKEND" = "nested" ]; then
        python3 "$WORK/bridge.py" req "$RUNTIME_DIR/sevyn-compositor.sock" "$1"
    else
        # NB: sudo — the bridge socket lives under root-owned 700
        # /tmp/sevyn-xdg, so an unprivileged probe dies with EACCES
        # before it reaches the compositor (standalone A15 proved
        # this the hard way: CONNECT-FAILED, not EOF). The probe
        # still connects as a DIFFERENT pid than the Runtime, so the
        # EOF refusal it observes is the compositor's accept-time
        # pid check, not a permission artefact.
        ssh_guest "sudo python3 /tmp/bridge.py req /tmp/sevyn-xdg/sevyn-compositor.sock '$1'"
    fi
}

# --- Input injection + clients ------------------------------------------------
# ptr_move <x> <y> / ptr_click / key_press <qcode>: backend-specific.
ptr_move() {
    if [ "$BACKEND" = "nested" ]; then
        xdotool mousemove "$1" "$2"
    else
        local ax=$(( $1 * 32767 / OUT_W )) ay=$(( $2 * 32767 / OUT_H ))
        qmp abs "$ax" "$ay"
    fi
}
ptr_click() {
    if [ "$BACKEND" = "nested" ]; then xdotool click 1; else qmp click; fi
}
key_press() {
    if [ "$BACKEND" = "nested" ]; then xdotool key "$1"; else qmp sendkey "$1"; fi
}
# start_client <mode> <log-basename> <pid-basename> [extra-env...]:
# launch one test-client instance; its pid lands in $WORK/<pid> (nested)
# or /tmp/<pid> in the guest (standalone).
start_client() {
    local mode="$1" log="$2" pidfile="$3"; shift 3
    if [ "$BACKEND" = "nested" ]; then
        rm -f "$WORK/$log"
        WAYLAND_DISPLAY="$SOCKET_NAME" SEVYN_CLIENT_MODE="$mode" \
            SEVYN_CLIENT_LOG="$WORK/$log" "$@" \
            "$WORK/sevyn-test-client" >"$WORK/$log.stdout" 2>&1 &
        echo $! > "$WORK/$pidfile"
    else
        ssh_guest "setsid nohup sudo bash -c 'rm -f /tmp/$log; echo \$\$ > /tmp/$pidfile; exec env XDG_RUNTIME_DIR=/tmp/sevyn-xdg WAYLAND_DISPLAY=$SOCKET_NAME SEVYN_CLIENT_MODE=$mode SEVYN_CLIENT_LOG=/tmp/$log $* /tmp/sevyn-test-client' >/dev/null 2>&1 &"
    fi
}
# run_manager_client <log-basename> [target-app-id] [action]: one-shot
# --manager run (Track 2's interface). The client lists the toplevels,
# optionally exercises one request on a target app_id, prints
# MANAGER DONE and exits on its own (~3 s: LIST_WAIT + ACTION_WAIT).
# Its markers land in $WORK/<log> (nested) or /tmp/<log> (guest) via
# SEVYN_MANAGER_LOG; if that file comes back empty, the captured
# stdout is used instead (same markers, same file).
run_manager_client() {
    local log="$1" target="${2:-}" action="${3:-none}"
    if [ "$BACKEND" = "nested" ]; then
        rm -f "$WORK/$log" "$WORK/$log.stdout"
        WAYLAND_DISPLAY="$SOCKET_NAME" SEVYN_MANAGER_LOG="$WORK/$log" \
            SEVYN_MANAGER_TARGET_APP_ID="$target" SEVYN_MANAGER_ACTION="$action" \
            "$WORK/sevyn-test-client-manager" --manager >"$WORK/$log.stdout" 2>&1
        if [ ! -s "$WORK/$log" ]; then cp "$WORK/$log.stdout" "$WORK/$log" 2>/dev/null || true; fi
    else
        ssh_guest "sudo bash -c 'rm -f /tmp/$log /tmp/$log.stdout; XDG_RUNTIME_DIR=/tmp/sevyn-xdg WAYLAND_DISPLAY=$SOCKET_NAME SEVYN_MANAGER_LOG=/tmp/$log SEVYN_MANAGER_TARGET_APP_ID=$target SEVYN_MANAGER_ACTION=$action /tmp/sevyn-test-client-manager --manager >/tmp/$log.stdout 2>&1; [ -s /tmp/$log ] || cp /tmp/$log.stdout /tmp/$log 2>/dev/null || true'"
    fi
}
# client_alive <pid-basename>: true when the client process is running.
client_alive() {
    if [ "$BACKEND" = "nested" ]; then
        [ -f "$WORK/$1" ] && kill -0 "$(cat "$WORK/$1")" 2>/dev/null
    else
        ssh_guest "[ -f /tmp/$1 ] && sudo kill -0 \$(cat /tmp/$1) 2>/dev/null"
    fi
}
# kill_client <pid-basename> [signal]: terminate a client.
kill_client() {
    local sig="${2:-TERM}"
    if [ "$BACKEND" = "nested" ]; then
        [ -f "$WORK/$1" ] && kill "-$sig" "$(cat "$WORK/$1")" 2>/dev/null || true
    else
        ssh_guest "[ -f /tmp/$1 ] && sudo kill -$sig \$(cat /tmp/$1) 2>/dev/null || true"
    fi
}
# wait_client_gone <pid-basename> [what]: poll until the process exits.
wait_client_gone() {
    local deadline=$((SECONDS + TIMEOUT))
    while client_alive "$1" && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    client_alive "$1" && return 1 || return 0
}

# --- Pointer probes -----------------------------------------------------------
# The grid covers the whole output, including the strips that are only
# one window wide (browser chrome y=20, browser bottom y=450, left
# strip x=20) — a coarse grid would never enter the Browser at all
# once Chromium overlaps it. The y=200 row covers the cascade band
# where the 400x100 stand-in windows land (count 3 -> y 144-244,
# count 4 -> y 192-292): without it the sweep never enters the paste
# or lock window, which sit between the y=100 and y=300 rows.
GRID_X="20 100 300 500 700 900 1100"
GRID_Y="20 100 200 300 450 600"
# sweep_find <log-basename>: move through the grid until the client's
# log gains a PTR ENTER line; echo "<out-x> <out-y> <local-x> <local-y>".
# Starts from a neutral point outside every window, so the first entry
# into the target is always a fresh PTR ENTER.
sweep_find() {
    local log="$1"
    ptr_move $((OUT_W - 20)) $((OUT_H - 20))
    sleep 0.2
    local before
    before="$(log_count "$log" "PTR ENTER")"
    for gx in $GRID_X; do
        for gy in $GRID_Y; do
            ptr_move "$gx" "$gy"
            sleep 0.15
            local now
            now="$(log_count "$log" "PTR ENTER")"
            now="${now:-0}"
            if [ "$now" -gt "$before" ] 2>/dev/null; then
                local last lx ly
                last="$(log_last "$log" "PTR ENTER")"
                lx="$(echo "$last" | awk '{print $(NF-1)}')"
                ly="$(echo "$last" | awk '{print $NF}')"
                echo "$gx $gy $lx $ly"
                return 0
            fi
        done
    done
    return 1
}
# probe_enter <log> <out-x> <out-y> <exp-lx> <exp-ly> [tol]: move to
# the output point and assert the client's log gains a PTR ENTER with
# local coords within tol (default 8) of expected. Echoes the coords.
# Moves to a neutral point first, so the probe is always a fresh enter
# (a move within the same surface generates no new PTR ENTER).
probe_enter() {
    local log="$1" ox="$2" oy="$3" ex="$4" ey="$5" tol="${6:-8}"
    ptr_move $((OUT_W - 20)) $((OUT_H - 20))
    sleep 0.2
    local before
    before="$(log_count "$log" "PTR ENTER")"
    ptr_move "$ox" "$oy"
    sleep 0.4
    local now last lx ly
    now="$(log_count "$log" "PTR ENTER")"
    now="${now:-0}"
    [ "$now" -gt "$before" ] 2>/dev/null || return 1
    last="$(log_last "$log" "PTR ENTER")"
    lx="$(echo "$last" | awk '{print $(NF-1)}')"
    ly="$(echo "$last" | awk '{print $NF}')"
    local dx=$((lx - ex)) dy=$((ly - ey))
    dx=${dx#-}; dy=${dy#-}
    if [ "$dx" -le "$tol" ] && [ "$dy" -le "$tol" ]; then
        echo "$lx $ly"
        return 0
    fi
    return 1
}
# probe_no_enter <log> <out-x> <out-y>: move there and assert the
# client's log gains NO new PTR ENTER line. Moves to a neutral point
# first (without it, a probe point inside the surface the pointer is
# already over would show no new enter and pass wrongly).
probe_no_enter() {
    local log="$1" ox="$2" oy="$3"
    ptr_move $((OUT_W - 20)) $((OUT_H - 20))
    sleep 0.2
    local before
    before="$(log_count "$log" "PTR ENTER")"
    ptr_move "$ox" "$oy"
    sleep 0.4
    local now
    now="$(log_count "$log" "PTR ENTER")"
    now="${now:-0}"
    [ "$now" -eq "$before" ] 2>/dev/null
}

# --- Setup: nested ------------------------------------------------------------
setup_nested() {
    [ -x "$COMPOSITOR_BIN" ] || fail "compositor binary not found: $COMPOSITOR_BIN (run cargo build first)"
    command -v Xvfb >/dev/null || fail "Xvfb not found"
    command -v weston >/dev/null || fail "weston not found"
    command -v xdotool >/dev/null || fail "xdotool not found"
    command -v python3 >/dev/null || fail "python3 not found"
    command -v gcc >/dev/null || fail "gcc not found"
    command -v wayland-scanner >/dev/null || fail "wayland-scanner not found"
    build_client
    export DISPLAY=":$DISPLAY_NUMBER"
    export XDG_RUNTIME_DIR="$RUNTIME_DIR"
    Xvfb ":$DISPLAY_NUMBER" -screen 0 1280x800x24 >"$WORK/xvfb.log" 2>&1 &
    echo $! >"$WORK/xvfb.pid"
    sleep 2
    kill -0 "$(cat "$WORK/xvfb.pid")" 2>/dev/null || fail "Xvfb failed to start (see $WORK/xvfb.log)"
    weston --backend=x11-backend.so --socket=wayland-1 --width=1280 --height=800 \
        >"$WORK/weston.log" 2>&1 &
    echo $! >"$WORK/weston.pid"
    local deadline=$((SECONDS + TIMEOUT))
    while [ ! -S "$RUNTIME_DIR/wayland-1" ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    [ -S "$RUNTIME_DIR/wayland-1" ] || fail "weston socket never appeared (see $WORK/weston.log)"
    # The Runtime connection: one persistent bridge connection for the
    # whole run (see the header contract). bridge.py starts FIRST and
    # retries its connect until the socket appears; its pid is the
    # Runtime's pid, passed to the compositor with --runtime-pid, so
    # the bridge admits ONLY this process (production shape: init
    # passes the Runtime pid at launch). Without --runtime-pid the
    # degraded state (A15) is untestable: any same-uid connection
    # counts as the Runtime reconnecting, so a fresh connection's
    # adopt would succeed instead of being refused.
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
    wait_log "compositor.log" "SEVYN READY socket=$SOCKET_NAME" "compositor READY" \
        || fail "compositor never became READY"
    wait_log "compositor.log" "SEVYN FIRST FRAME PRESENTED" "first presented frame" \
        || fail "compositor never presented a first frame"
}

# --- Setup: standalone (KVM guest) --------------------------------------------
# QMP helper (copied from standalone-check.sh — same protocol).
setup_qmp() {
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
    QMP_SOCK="$WORK/qmp.sock"
}
qmp() { python3 "$WORK/qmp.py" "$QMP_SOCK" "$@" || fail "QMP command failed: $*"; }

setup_standalone() {
    [ -x "$COMPOSITOR_BIN" ] || fail "compositor binary not found: $COMPOSITOR_BIN (run cargo build --release --locked in compositor/ first)"
    [ -f "$CLIENT_SRC" ] || fail "test client source not found: $CLIENT_SRC"
    [ -f "$BASE_QCOW2" ] || fail "guest base disk not found: $BASE_QCOW2"
    for tool in qemu-system-x86_64 qemu-img genisoimage python3 ssh scp ssh-keygen; do
        command -v "$tool" >/dev/null || fail "host tool not found: $tool"
    done
    [ -e /dev/kvm ] || fail "no /dev/kvm — the standalone backend needs the KVM box"
    if (exec 3<>/dev/tcp/127.0.0.1/"$SSH_PORT") 2>/dev/null; then
        exec 3>&- 3<&-
        fail "something already listens on 127.0.0.1:$SSH_PORT (stale QEMU? set SEVYN_SSH_PORT)"
    fi
    mkdir -p "$CHECK_DIR"
    setup_qmp
    SSH_OPTS="-p $SSH_PORT -i $WORK/id_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o LogLevel=ERROR"
    ssh-keygen -t ed25519 -f "$WORK/id_ed25519" -N "" -C "sevyn-adoption-check" >/dev/null \
        || fail "ssh-keygen failed"
    mkdir -p "$WORK/seed"
    cat >"$WORK/seed/meta-data" <<EOF
instance-id: sevyn-adoption-check
local-hostname: sevyn-adoption-check
EOF
    cat >"$WORK/seed/user-data" <<EOF
#cloud-config
hostname: sevyn-adoption-check
manage_etc_hosts: true
users:
  - name: sevyn
    sudo: ALL=(ALL) NOPASSWD:ALL
    shell: /bin/bash
    ssh_authorized_keys:
      - $(cat "$WORK/id_ed25519.pub")
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
    echo "adoption-check: waiting for guest SSH (up to 300 s)..."
    local deadline=$((SECONDS + 300))
    while ! ssh_guest true 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do sleep 5; done
    ssh_guest true 2>/dev/null || fail "guest SSH never came up"
    echo "adoption-check: waiting for cloud-init to finish (up to 660 s)..."
    timeout 660 ssh $SSH_OPTS sevyn@127.0.0.1 "sudo cloud-init status --wait" 2>/dev/null \
        || fail "cloud-init did not finish (guest package install incomplete?)"
    scp_to_guest "$COMPOSITOR_BIN" "sevyn@127.0.0.1:/tmp/sevyn-compositor" \
        || fail "scp compositor binary failed"
    scp_to_guest "$CLIENT_SRC" "sevyn@127.0.0.1:/tmp/test-client.c" \
        || fail "scp test client source failed"
    scp_to_guest "$WORK/bridge.py" "sevyn@127.0.0.1:/tmp/bridge.py" \
        || fail "scp bridge.py failed"
    # Manager protocol sources are generated on the host from the cargo
    # registry (the guest has no cargo) and scp'd over; the guest build
    # below compiles the --manager client from them when present.
    if gen_manager_sources "$WORK/manager-proto"; then
        for f in wlr-foreign-toplevel-management-unstable-v1-client-protocol.h wlr-foreign-toplevel-management-unstable-v1-protocol.c ext-foreign-toplevel-list-v1-client-protocol.h ext-foreign-toplevel-list-v1-protocol.c; do
            scp_to_guest "$WORK/manager-proto/$f" "sevyn@127.0.0.1:/tmp/$f" \
                || fail "scp manager protocol source $f failed"
        done
    fi
    ssh_guest "sudo chmod +x /tmp/sevyn-compositor" || fail "chmod in guest failed"
    echo "adoption-check: guest setup (seatd + test client build)..."
    ssh_guest "sudo bash -c '
set -e
export XDG_RUNTIME_DIR=/tmp/sevyn-xdg
mkdir -p \$XDG_RUNTIME_DIR; chmod 700 \$XDG_RUNTIME_DIR
if [ ! -S /run/seatd.sock ]; then
  seatd >/tmp/seatd.log 2>&1 </dev/null &
  for i in \$(seq 1 75); do [ -S /run/seatd.sock ] && break; sleep 0.2; done
  [ -S /run/seatd.sock ] || { echo guest: seatd socket never appeared >&2; exit 1; }
fi
BUILD_DIR=\$(mktemp -d /tmp/sevyn-client-build.XXXXXX)
for spec in \"xdg-shell stable/xdg-shell/xdg-shell\" \"presentation-time stable/presentation-time/presentation-time\" \"pointer-constraints-unstable-v1 unstable/pointer-constraints/pointer-constraints-unstable-v1\" \"relative-pointer-unstable-v1 unstable/relative-pointer/relative-pointer-unstable-v1\"; do
  set -- \$spec
  wayland-scanner client-header /usr/share/wayland-protocols/\$2.xml \$BUILD_DIR/\$1-client-protocol.h
  wayland-scanner private-code /usr/share/wayland-protocols/\$2.xml \$BUILD_DIR/\$1-protocol.c
done
gcc -o /tmp/sevyn-test-client /tmp/test-client.c \$BUILD_DIR/xdg-shell-protocol.c \$BUILD_DIR/presentation-time-protocol.c \$BUILD_DIR/pointer-constraints-unstable-v1-protocol.c \$BUILD_DIR/relative-pointer-unstable-v1-protocol.c -DSEVYN_HAVE_POINTER_CONSTRAINTS=1 -I\$BUILD_DIR \$(pkg-config --cflags --libs wayland-client)
if [ -f /tmp/wlr-foreign-toplevel-management-unstable-v1-protocol.c ] && [ -f /tmp/ext-foreign-toplevel-list-v1-protocol.c ]; then
  gcc -DHAVE_WLR_FOREIGN_TOPLEVEL -o /tmp/sevyn-test-client-manager /tmp/test-client.c \$BUILD_DIR/xdg-shell-protocol.c \$BUILD_DIR/presentation-time-protocol.c /tmp/wlr-foreign-toplevel-management-unstable-v1-protocol.c /tmp/ext-foreign-toplevel-list-v1-protocol.c -I\$BUILD_DIR -I/tmp \$(pkg-config --cflags --libs wayland-client) && echo guest: manager client built || echo guest: manager client build FAILED
fi
rm -rf \$BUILD_DIR
echo guest: seatd up, test client built
'" || fail "guest setup failed"
    ssh_guest "test -x /tmp/sevyn-test-client-manager" 2>/dev/null && MANAGER_CLIENT_OK=1 || MANAGER_CLIENT_OK=0
    # The Runtime connection, in the guest, as root. bridge.py starts
    # FIRST and retries its connect until the socket appears; its pid
    # is the Runtime's pid, passed to the compositor with
    # --runtime-pid (same shape as nested — see setup_nested).
    ssh_guest "sudo bash -c '
rm -f /tmp/bridge-in.txt /tmp/bridge-out.txt /tmp/bridge-runtime.pid; touch /tmp/bridge-in.txt /tmp/bridge-out.txt; chmod 666 /tmp/bridge-in.txt /tmp/bridge-out.txt
# The harness appends requests to bridge-in.txt over SSH as sevyn
# while bridge.py runs as root: the files must be world-writable.
setsid nohup bash -c \"python3 /tmp/bridge.py runtime /tmp/sevyn-xdg/sevyn-compositor.sock /tmp/bridge-in.txt /tmp/bridge-out.txt >/tmp/bridge-runtime.log 2>&1 & echo \\\$! > /tmp/bridge-runtime.pid; wait \\\$!\" >/dev/null 2>&1 &
'" || fail "could not start bridge runtime client (standalone)"
    local deadline=$((SECONDS + 30)) BRIDGE_PID=""
    while [ -z "$BRIDGE_PID" ] && [ "$SECONDS" -lt "$deadline" ]; do
        BRIDGE_PID="$(ssh_guest "cat /tmp/bridge-runtime.pid 2>/dev/null" | tr -d ' ')"
        [ -n "$BRIDGE_PID" ] || sleep 0.5
    done
    [ -n "$BRIDGE_PID" ] || fail "could not read bridge runtime pid (standalone)"
    ssh_guest "sudo bash -c '
export XDG_RUNTIME_DIR=/tmp/sevyn-xdg
rm -f /tmp/compositor.exit /tmp/compositor.log /tmp/compositor.err
setsid nohup bash -c \"XDG_RUNTIME_DIR=/tmp/sevyn-xdg /tmp/sevyn-compositor --standalone --socket $SOCKET_NAME --runtime-pid $BRIDGE_PID >/tmp/compositor.log 2>/tmp/compositor.err & echo \\\$! > /tmp/compositor.pid; wait \\\$!; echo \\\$? > /tmp/compositor.exit\" >/dev/null 2>&1 &
'" || fail "could not start compositor (standalone)"
    wait_log "compositor.log" "SEVYN READY" "compositor READY (standalone)" \
        || fail "compositor never became READY (standalone)"
    wait_log "compositor.log" "SEVYN FIRST FRAME PRESENTED" "first presented frame (standalone)" \
        || fail "compositor never presented a first frame (standalone)"
    # Output size (from the READY marker) drives the QMP scaling.
    local ready
    ready="$(log_last "compositor.log" "SEVYN READY")"
    OUT_W="$(echo "$ready" | grep -o "output=[0-9]*x[0-9]*" | cut -d= -f2 | cut -dx -f1)"
    OUT_H="$(echo "$ready" | grep -o "output=[0-9]*x[0-9]*" | cut -d= -f2 | cut -dx -f2)"
    [ -n "$OUT_W" ] && [ -n "$OUT_H" ] || fail "could not parse output size from READY: $ready"
    echo "adoption-check: standalone output ${OUT_W}x${OUT_H}"
    sleep 1
}

# --- Build the test client (nested) --------------------------------------------
# gen_manager_sources <dir>: generate the wlr foreign-toplevel-
# management + ext-foreign-toplevel-list client sources into <dir>
# from the cargo registry (manager track's interface:
# wayland-protocols-wlr 0.3.12, wayland-protocols 0.32.13). Returns 0
# on success; 1 when the XMLs are not in the registry (the manager
# track is not present in this tree) — a NOT-PROVEN cause, never a
# fatal one. The caller decides what to do with the failure.
gen_manager_sources() {
    local dir="$1"
    mkdir -p "$dir"
    local wlr="" list="" cand
    for cand in ~/.cargo/registry/src/*/wayland-protocols-wlr-0.3.12/wlr-protocols/unstable/wlr-foreign-toplevel-management-unstable-v1.xml; do
        [ -f "$cand" ] && wlr="$cand" && break
    done
    for cand in ~/.cargo/registry/src/*/wayland-protocols-0.32.13/protocols/staging/ext-foreign-toplevel-list/ext-foreign-toplevel-list-v1.xml; do
        [ -f "$cand" ] && list="$cand" && break
    done
    if [ -z "$wlr" ] || [ -z "$list" ]; then
        echo "adoption-check: manager protocol XMLs not in the cargo registry (wlr=$wlr list=$list)"
        return 1
    fi
    wayland-scanner client-header "$wlr" "$dir/wlr-foreign-toplevel-management-unstable-v1-client-protocol.h" || return 1
    wayland-scanner private-code "$wlr" "$dir/wlr-foreign-toplevel-management-unstable-v1-protocol.c" || return 1
    wayland-scanner client-header "$list" "$dir/ext-foreign-toplevel-list-v1-client-protocol.h" || return 1
    wayland-scanner private-code "$list" "$dir/ext-foreign-toplevel-list-v1-protocol.c" || return 1
    return 0
}
# MANAGER_CLIENT_OK: 1 when the --manager client binary built (set by
# build_client / the standalone guest setup; read by A11).
MANAGER_CLIENT_OK=0

build_client() {
    local b="$WORK/client-build"
    mkdir -p "$b"
    local xdg="/usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml"
    local pres="/usr/share/wayland-protocols/stable/presentation-time/presentation-time.xml"
    local pc="/usr/share/wayland-protocols/unstable/pointer-constraints/pointer-constraints-unstable-v1.xml"
    local rp="/usr/share/wayland-protocols/unstable/relative-pointer/relative-pointer-unstable-v1.xml"
    for spec in "$xdg:xdg-shell" "$pres:presentation-time" "$pc:pointer-constraints-unstable-v1" "$rp:relative-pointer-unstable-v1"; do
        local xml="${spec%%:*}" name="${spec##*:}"
        [ -f "$xml" ] || fail "protocol XML not found: $xml"
        wayland-scanner client-header "$xml" "$b/$name-client-protocol.h" \
            || fail "wayland-scanner client-header failed ($name)"
        wayland-scanner private-code "$xml" "$b/$name-protocol.c" \
            || fail "wayland-scanner private-code failed ($name)"
    done
    gcc -o "$WORK/sevyn-test-client" "$CLIENT_SRC" \
        "$b/xdg-shell-protocol.c" "$b/presentation-time-protocol.c" \
        "$b/pointer-constraints-unstable-v1-protocol.c" "$b/relative-pointer-unstable-v1-protocol.c" \
        -DSEVYN_HAVE_POINTER_CONSTRAINTS=1 -I"$b" $(pkg-config --cflags --libs wayland-client) \
        || fail "test client build failed"
    # Manager client (Track 2's --manager mode): only when the wlr +
    # ext-list protocol sources are available. Absent = A11 NOT-PROVEN.
    if gen_manager_sources "$b/manager"; then
        if gcc -DHAVE_WLR_FOREIGN_TOPLEVEL -o "$WORK/sevyn-test-client-manager" "$CLIENT_SRC" \
            "$b/xdg-shell-protocol.c" "$b/presentation-time-protocol.c" \
            "$b/manager/wlr-foreign-toplevel-management-unstable-v1-protocol.c" \
            "$b/manager/ext-foreign-toplevel-list-v1-protocol.c" \
            -I"$b" -I"$b/manager" $(pkg-config --cflags --libs wayland-client) \
            2>"$b/manager-build.err"; then
            MANAGER_CLIENT_OK=1
        else
            echo "adoption-check: manager client build failed (see $b/manager-build.err)"
        fi
    fi
}

# --- Teardown -------------------------------------------------------------------
cleanup() {
    # Kill the bridge runtime first (its connection dropping is the
    # Runtime dying — which is A15's test when done deliberately, and
    # harmless noise at teardown).
    if [ -f "$WORK/bridge-runtime.pid" ]; then
        kill "$(cat "$WORK/bridge-runtime.pid")" 2>/dev/null || true
    fi
    if [ "$BACKEND" = "standalone" ]; then
        ssh_guest "sudo pkill -f 'bridge.py runtime' 2>/dev/null; sudo pkill -f 'sevyn-test-clien[t]' 2>/dev/null; [ -f /tmp/compositor.pid ] && sudo kill -TERM \$(cat /tmp/compositor.pid) 2>/dev/null; true" 2>/dev/null || true
        sleep 1
        if [ -f "$WORK/qemu.pid" ]; then
            kill "$(cat "$WORK/qemu.pid")" 2>/dev/null || true
            sleep 2
            kill -9 "$(cat "$WORK/qemu.pid")" 2>/dev/null || true
        fi
        preserve_logs
        rm -rf "$WORK"
        return 0
    fi
    for pid_file in "$WORK"/xvfb.pid "$WORK"/weston.pid "$WORK"/compositor.pid; do
        if [ -f "$pid_file" ]; then kill "$(cat "$pid_file")" 2>/dev/null || true; fi
    done
    pkill -f "sevyn-test-clien[t]" 2>/dev/null || true
    sleep 1
    for pid_file in "$WORK"/xvfb.pid "$WORK"/weston.pid "$WORK"/compositor.pid; do
        if [ -f "$pid_file" ]; then kill -9 "$(cat "$pid_file")" 2>/dev/null || true; fi
    done
    rm -rf "$WORK"
}
trap cleanup EXIT

# --- Helpers for ids ------------------------------------------------------------
client_pid() { # <pid-basename>
    if [ "$BACKEND" = "nested" ]; then cat "$WORK/$1" 2>/dev/null | tr -d ' ';
    else ssh_guest "cat /tmp/$1 2>/dev/null" | tr -d ' '; fi
}
surface_line_for_pid() { # <pid>
    if [ "$BACKEND" = "nested" ]; then grep "SURFACE CREATED.*pid=$1" "$COMPOSITOR_LOG" 2>/dev/null | head -1;
    else ssh_guest "grep 'SURFACE CREATED.*pid=$1' /tmp/compositor.log 2>/dev/null | head -1"; fi
}
surface_id_for_pid() { surface_line_for_pid "$1" | grep -o "id=[0-9]*" | head -1 | cut -d= -f2; }
surface_session_for_pid() { surface_line_for_pid "$1" | grep -o "session=[^ ]*" | head -1 | cut -d= -f2; }
# dead_pid: a pid that has already exited (best effort; reuse in the
# test window is unlikely but noted where it is used).
dead_pid() {
    if [ "$BACKEND" = "nested" ]; then
        local p
        p="$(sh -c 'sleep 0.05 & echo $!')"
        sleep 0.3
        echo "$p"
    else
        ssh_guest "sh -c 'sleep 0.05 & echo \$!'; sleep 0.3" | tr -d ' ' | tail -1
    fi
}
# bridge_event_wait <pattern> [what]: poll the bridge outfile (which
# carries events on the Runtime connection) for the pattern.
bridge_event_wait() {
    local pattern="$1" what="${2:-$pattern}"
    local deadline=$((SECONDS + TIMEOUT))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if [ "$BACKEND" = "nested" ]; then
            grep -q -- "$pattern" "$BRIDGE_OUT" 2>/dev/null && return 0
        else
            ssh_guest "grep -q -- '$pattern' /tmp/bridge-out.txt 2>/dev/null" && return 0
        fi
        sleep 0.5
    done
    return 1
}
# compositor_alive: ping over the Runtime connection.
compositor_alive() {
    local resp
    resp="$(bridge_req '{"v":1,"op":"ping"}')" 2>/dev/null || return 1
    echo "$resp" | grep -q '"ok": *true'
}

# --- Main -----------------------------------------------------------------------
if [ "$BACKEND" = "nested" ]; then setup_nested; else setup_standalone; fi
echo "adoption-check: backend=$BACKEND compositor READY, bridge Runtime connected"

# --- A1: clients up -------------------------------------------------------------
start_client browser "$BROWSER_LOG_NAME" browser.pid
start_client chromium "$CHROMIUM_LOG_NAME" chromium.pid
wait_log "$BROWSER_LOG_NAME" "BROWSER READY" "Browser READY" \
    || fail "Browser stand-in never became READY"
wait_log "$CHROMIUM_LOG_NAME" "CHROMIUM READY" "Chromium READY" \
    || fail "Chromium stand-in never became READY"
BROWSER_PID="$(client_pid browser.pid)"
CHROMIUM_PID="$(client_pid chromium.pid)"
[ -n "$BROWSER_PID" ] && [ -n "$CHROMIUM_PID" ] || fail "could not read client pids"
BROWSER_SESSION="$(surface_session_for_pid "$BROWSER_PID")"
BROWSER_SURFACE="$(surface_id_for_pid "$BROWSER_PID")"
CHROMIUM_SESSION="$(surface_session_for_pid "$CHROMIUM_PID")"
CHROMIUM_SURFACE="$(surface_id_for_pid "$CHROMIUM_PID")"
[ -n "$BROWSER_SESSION" ] && [ -n "$BROWSER_SURFACE" ] && [ -n "$CHROMIUM_SURFACE" ] \
    || fail "could not read sessions/surfaces from the compositor log (browser session=$BROWSER_SESSION surface=$BROWSER_SURFACE chromium surface=$CHROMIUM_SURFACE)"
log_grep "compositor.log" "SEVYN SURFACE COMMITTED id=$BROWSER_SURFACE" \
    || fail "Browser surface $BROWSER_SURFACE never committed"
log_grep "compositor.log" "SEVYN SURFACE COMMITTED id=$CHROMIUM_SURFACE" \
    || fail "Chromium surface $CHROMIUM_SURFACE never committed"
record A1 PASS "Browser (session=$BROWSER_SESSION surface=$BROWSER_SURFACE pid=$BROWSER_PID) + Chromium (surface=$CHROMIUM_SURFACE pid=$CHROMIUM_PID) mapped, associated, committed"

# Discover the Browser window's output origin once (it never moves in
# this harness — only the adopted rect moves, via the bridge).
ORIGIN="$(sweep_find "$BROWSER_LOG_NAME")" || fail "pointer sweep never entered the Browser window"
BROWSER_OX="$(echo "$ORIGIN" | awk '{print $1 - $3}')"
BROWSER_OY="$(echo "$ORIGIN" | awk '{print $2 - $4}')"
CONTENT_CX=$((BROWSER_OX + 400)); CONTENT_CY=$((BROWSER_OY + 300))
CHROME_CX=$((BROWSER_OX + 400)); CHROME_CY=$((BROWSER_OY + 75))
NEUTRAL_X=$((OUT_W - 20)); NEUTRAL_Y=$((OUT_H - 20))
echo "adoption-check: Browser origin=($BROWSER_OX,$BROWSER_OY) content-centre=($CONTENT_CX,$CONTENT_CY) chrome-centre=($CHROME_CX,$CHROME_CY)"

# Neutral-point moves before probes: the pointer must leave a surface
# before re-entering it, or no new PTR ENTER is generated. probe_* do
# this internally; the raw moves here are for the milestones that
# click (they move directly onto their target).
probe_enter_neutral() { ptr_move "$NEUTRAL_X" "$NEUTRAL_Y"; sleep 0.2; }

# --- A2: hostile pre-adopt -------------------------------------------------------
A2_OK=1
bridge_req_refused 'not json' "A2 malformed adopt (not JSON)" || A2_OK=0
bridge_req_refused '{"v":2,"op":"ping"}' "A2 wrong version" || A2_OK=0
bridge_req_refused '{"v":1,"op":"adopt","session":"x"}' "A2 adopt missing fields" || A2_OK=0
bridge_req_refused "{\"v\":1,\"op\":\"adopt\",\"session\":\"no-such-session\",\"chromium_pid\":$CHROMIUM_PID,\"rect\":{\"x\":0,\"y\":152,\"w\":800,\"h\":348}}" "A2 adopt unknown session" || A2_OK=0
DEAD_PID="$(dead_pid)"
bridge_req_refused "{\"v\":1,\"op\":\"adopt\",\"session\":\"$BROWSER_SESSION\",\"chromium_pid\":$DEAD_PID,\"rect\":{\"x\":0,\"y\":152,\"w\":800,\"h\":348}}" "A2 adopt dead pid $DEAD_PID" || A2_OK=0
bridge_req_refused "{\"v\":1,\"op\":\"adopt\",\"session\":\"$BROWSER_SESSION\",\"chromium_pid\":$CHROMIUM_PID,\"rect\":{\"x\":900,\"y\":600,\"w\":100,\"h\":100}}" "A2 adopt rect outside window" || A2_OK=0
bridge_req_refused "{\"v\":1,\"op\":\"adopt\",\"session\":\"$BROWSER_SESSION\",\"chromium_pid\":$CHROMIUM_PID,\"rect\":{\"x\":700,\"y\":400,\"w\":200,\"h\":200}}" "A2 adopt rect partially outside window" || A2_OK=0
if log_grep "compositor.log" "ADOPTED session="; then
    record A2 FAIL "an ADOPTED marker appeared during the hostile cases"
    A2_OK=0
fi
compositor_alive || { record A2 FAIL "compositor stopped answering ping after the hostile cases"; A2_OK=0; }
[ "$A2_OK" = 1 ] && record A2 PASS "malformed/wrong-version/missing-fields/unknown-session/dead-pid/outside-rect adopts all refused (ok:false), no ADOPTED, compositor alive"

# --- A3/A4: spoof + adopt ---------------------------------------------------------
# A3 setup: a second client claiming app_id `chromium` with its own pid.
ADOPT_OK=0
start_client chromium "$SPOOF_LOG_NAME" spoof.pid
wait_log "$SPOOF_LOG_NAME" "CHROMIUM READY" "spoof Chromium READY" \
    || fail "spoof Chromium stand-in never became READY"
SPOOF_PID="$(client_pid spoof.pid)"
SPOOF_SURFACE="$(surface_id_for_pid "$SPOOF_PID")"
[ -n "$SPOOF_SURFACE" ] || fail "could not read the spoof surface id"
ADOPT_RESP="$(bridge_req "{\"v\":1,\"op\":\"adopt\",\"session\":\"$BROWSER_SESSION\",\"chromium_pid\":$CHROMIUM_PID,\"rect\":{\"x\":$RECT_X,\"y\":$RECT_Y,\"w\":$RECT_W,\"h\":$RECT_H}}")" \
    || ADOPT_RESP=""
if [ -z "$ADOPT_RESP" ]; then
    record A4 FAIL "adopt request timed out (no bridge response)"
    record A3 NOT-PROVEN "blocked: adopt failed (A4)"
elif ! echo "$ADOPT_RESP" | grep -q '"ok": *true'; then
    record A4 FAIL "adopt refused: $ADOPT_RESP"
    record A3 NOT-PROVEN "blocked: adopt failed (A4)"
else
    ADOPTED_LINE="$(log_last "compositor.log" "ADOPTED session=$BROWSER_SESSION")"
    if [ -z "$ADOPTED_LINE" ]; then
        record A4 FAIL "adopt answered ok:true but no ADOPTED marker appeared"
        record A3 NOT-PROVEN "blocked: adopt failed (A4)"
    elif ! echo "$ADOPTED_LINE" | grep -q "chromium=$CHROMIUM_SURFACE"; then
        record A4 FAIL "ADOPTED marker names the wrong surface: $ADOPTED_LINE (expected chromium=$CHROMIUM_SURFACE)"
        record A3 NOT-PROVEN "blocked: adopt failed (A4)"
    elif ! echo "$ADOPTED_LINE" | grep -q "pid=$CHROMIUM_PID"; then
        record A4 FAIL "ADOPTED marker names the wrong pid: $ADOPTED_LINE (expected pid=$CHROMIUM_PID)"
        record A3 NOT-PROVEN "blocked: adopt failed (A4)"
    elif ! echo "$ADOPTED_LINE" | grep -q "rect=0,152,800,348"; then
        record A4 FAIL "ADOPTED marker has the wrong rect: $ADOPTED_LINE"
        record A3 NOT-PROVEN "blocked: adopt failed (A4)"
    elif log_grep "compositor.log" "ADOPTED session=$BROWSER_SESSION browser=$BROWSER_SURFACE chromium=$SPOOF_SURFACE"; then
        record A3 FAIL "the spoof surface $SPOOF_SURFACE was adopted (app_id spoof with the wrong pid must never be adopted)"
        record A4 PASS "adopt ok:true; $ADOPTED_LINE"
        ADOPT_OK=1
    else
        record A3 PASS "spoof client (app_id=chromium, pid=$SPOOF_PID, surface=$SPOOF_SURFACE) NOT adopted; only the Runtime-declared pid $CHROMIUM_PID was"
        record A4 PASS "adopt ok:true; $ADOPTED_LINE"
        ADOPT_OK=1
    fi
fi
kill_client spoof.pid
wait_client_gone spoof.pid || echo "adoption-check: warning: spoof client did not exit after SIGTERM (continuing)"

# --- A5: geometry + clipping (hit-test) -------------------------------------------
if [ "$ADOPT_OK" != 1 ]; then
    record A5 NOT-PROVEN "blocked: adopt failed (A4)"
else
probe_enter_neutral
probe_enter "$CHROMIUM_LOG_NAME" "$CONTENT_CX" "$CONTENT_CY" 400 148 >/dev/null \
    || { record A5 FAIL "content-centre probe did not hit the adopted surface with rect-relative coords (400,148); last chromium enter: $(log_last "$CHROMIUM_LOG_NAME" "PTR ENTER")"; ADOPT_OK=0; }
if [ "$ADOPT_OK" = 1 ]; then
    probe_enter_neutral
    probe_enter "$BROWSER_LOG_NAME" "$CHROME_CX" "$CHROME_CY" 400 75 >/dev/null \
        || { record A5 FAIL "chrome-centre probe did not hit the Browser (400,75); last browser enter: $(log_last "$BROWSER_LOG_NAME" "PTR ENTER")"; ADOPT_OK=0; }
fi
if [ "$ADOPT_OK" = 1 ]; then
    # Just inside each rect edge -> the adopted surface, rect-relative.
    probe_enter_neutral
    probe_enter "$CHROMIUM_LOG_NAME" $((BROWSER_OX + 10)) $((BROWSER_OY + 300)) 10 148 >/dev/null \
        || { record A5 FAIL "left-edge-inside probe missed the adopted surface"; ADOPT_OK=0; }
    probe_enter_neutral
    probe_enter "$CHROMIUM_LOG_NAME" $((BROWSER_OX + 790)) $((BROWSER_OY + 300)) 790 148 >/dev/null \
        || { record A5 FAIL "right-edge-inside probe missed the adopted surface"; ADOPT_OK=0; }
    probe_enter_neutral
    probe_enter "$CHROMIUM_LOG_NAME" $((BROWSER_OX + 400)) $((BROWSER_OY + 160)) 400 8 >/dev/null \
        || { record A5 FAIL "top-edge-inside probe missed the adopted surface"; ADOPT_OK=0; }
    probe_enter_neutral
    probe_enter "$CHROMIUM_LOG_NAME" $((BROWSER_OX + 400)) $((BROWSER_OY + 490)) 400 338 >/dev/null \
        || { record A5 FAIL "bottom-edge-inside probe missed the adopted surface"; ADOPT_OK=0; }
fi
if [ "$ADOPT_OK" = 1 ]; then
    # Just outside the rect but inside the window -> the Browser, never
    # the adopted surface (this is the clipping proof this harness can
    # make without a screendump; see the header).
    probe_enter_neutral
    probe_no_enter "$CHROMIUM_LOG_NAME" $((BROWSER_OX + 400)) $((BROWSER_OY + 140)) \
        || { record A5 FAIL "probe just above the rect (chrome) reached the adopted surface — clipping failed"; ADOPT_OK=0; }
    probe_enter_neutral
    probe_enter "$BROWSER_LOG_NAME" $((BROWSER_OX + 400)) $((BROWSER_OY + 140)) 400 140 >/dev/null \
        || { record A5 FAIL "probe just above the rect did not hit the Browser chrome"; ADOPT_OK=0; }
    # The old standalone position (Chromium was cascaded at (48,48))
    # must no longer hit the adopted surface.
    probe_enter_neutral
    probe_no_enter "$CHROMIUM_LOG_NAME" $((BROWSER_OX + 60)) $((BROWSER_OY + 60)) \
        || { record A5 FAIL "the old standalone position still hits the adopted surface"; ADOPT_OK=0; }
fi
[ "$ADOPT_OK" = 1 ] && record A5 PASS "adopted surface inside the host rect with rect-relative coords; chrome hits the Browser; probes outside the rect never reach the adopted surface; old standalone position empty"
fi

# --- A6: move ---------------------------------------------------------------------
if [ "$ADOPT_OK" = 1 ]; then
    if ! bridge_req_ok "{\"v\":1,\"op\":\"move\",\"session\":\"$BROWSER_SESSION\",\"rect\":{\"x\":0,\"y\":152,\"w\":400,\"h\":348}}" "A6 move" >/dev/null; then
        record A6 FAIL "bridge move refused"
    else
    probe_enter_neutral
    if probe_enter "$CHROMIUM_LOG_NAME" $((BROWSER_OX + 200)) $((BROWSER_OY + 326)) 200 174 >/dev/null; then
        probe_enter_neutral
        if probe_no_enter "$CHROMIUM_LOG_NAME" "$CONTENT_CX" "$CONTENT_CY"; then
            if ! bridge_req_ok "{\"v\":1,\"op\":\"move\",\"session\":\"$BROWSER_SESSION\",\"rect\":{\"x\":0,\"y\":152,\"w\":800,\"h\":348}}" "A6 move back" >/dev/null; then
                record A6 FAIL "bridge move-back refused"
            else
            probe_enter_neutral
            if probe_enter "$CHROMIUM_LOG_NAME" "$CONTENT_CX" "$CONTENT_CY" 400 148 >/dev/null; then
                record A6 PASS "move changed the rect (new centre hits adopted, old centre no longer does); move back restored it"
            else
                record A6 FAIL "move back did not restore the adopted surface at the content centre"
            fi
            fi
        else
            record A6 FAIL "after move, the old content centre still hits the adopted surface (rect did not move)"
        fi
    else
        record A6 FAIL "after move, the new rect centre did not hit the adopted surface with rect-relative coords (200,174)"
    fi
    fi
else
    record A6 NOT-PROVEN "blocked: adopt/geometry failed (A5)"
fi

# --- A7: resize -------------------------------------------------------------------
if [ "$ADOPT_OK" = 1 ]; then
    if ! bridge_req_ok "{\"v\":1,\"op\":\"resize\",\"session\":\"$BROWSER_SESSION\",\"rect\":{\"x\":0,\"y\":152,\"w\":700,\"h\":300}}" "A7 resize" >/dev/null; then
        record A7 FAIL "bridge resize refused"
    else
    if wait_log "$CHROMIUM_LOG_NAME" "CHROMIUM RESIZED 700x300" "chromium resize to 700x300"; then
        probe_enter_neutral
        if probe_enter "$CHROMIUM_LOG_NAME" $((BROWSER_OX + 350)) $((BROWSER_OY + 302)) 350 150 >/dev/null; then
            if ! bridge_req_ok "{\"v\":1,\"op\":\"resize\",\"session\":\"$BROWSER_SESSION\",\"rect\":{\"x\":0,\"y\":152,\"w\":800,\"h\":348}}" "A7 resize back" >/dev/null; then
                record A7 FAIL "bridge resize-back refused"
            else
            if wait_log "$CHROMIUM_LOG_NAME" "CHROMIUM RESIZED 800x348" "chromium resize back to 800x348"; then
                # Release: the adopted surface leaves the rect (the
                # Browser shows through), stays alive, and a re-adopt
                # restores it — the release op must work before A8's
                # focus tests run against the adopted surface.
                if ! bridge_req_ok "{\"v\":1,\"op\":\"release\",\"session\":\"$BROWSER_SESSION\"}" "A7 release" >/dev/null; then
                    record A7 FAIL "bridge release refused"
                elif ! log_grep "compositor.log" "ADOPTION RELEASED session=$BROWSER_SESSION"; then
                    record A7 FAIL "no ADOPTION RELEASED marker after bridge release"
                elif ! client_alive chromium.pid; then
                    record A7 FAIL "the adopted client died on release (release must not kill it)"
                else
                    # No hit-test here: after release the Chromium
                    # window is a plain standalone toplevel again and
                    # may legitimately still overlap the Browser's
                    # content area, so a content-centre probe cannot
                    # tell "released" from "still adopted". The clean
                    # observable is state: a move for the released
                    # session must now be refused (no active adoption).
                    if ! bridge_req_refused "{\"v\":1,\"op\":\"move\",\"session\":\"$BROWSER_SESSION\",\"rect\":{\"x\":0,\"y\":152,\"w\":700,\"h\":300}}" "A7 post-release move" >/dev/null; then
                        record A7 FAIL "after release, a move for the released session was not refused (adoption state not cleaned up)"
                    elif ! bridge_req_ok "{\"v\":1,\"op\":\"adopt\",\"session\":\"$BROWSER_SESSION\",\"chromium_pid\":$CHROMIUM_PID,\"rect\":{\"x\":0,\"y\":152,\"w\":800,\"h\":348}}" "A7 re-adopt" >/dev/null; then
                        record A7 FAIL "re-adopt after release refused"
                    elif ! log_grep "compositor.log" "ADOPTED session=$BROWSER_SESSION browser=$BROWSER_SURFACE chromium=$CHROMIUM_SURFACE"; then
                        record A7 FAIL "re-adopt answered ok:true but no ADOPTED marker for surface $CHROMIUM_SURFACE"
                    else
                        probe_enter_neutral
                        if probe_enter "$CHROMIUM_LOG_NAME" "$CONTENT_CX" "$CONTENT_CY" 400 148 >/dev/null; then
                            record A7 PASS "resize sent xdg_toplevel.configure (client logged CHROMIUM RESIZED 700x300, then 800x348); new-centre probe hit with rect-relative coords; release emptied the adoption (ADOPTION RELEASED, client alive, post-release move refused) and re-adopt restored it"
                        else
                            record A7 FAIL "re-adopted surface is not hit at the content centre"
                        fi
                    fi
                fi
            else
                record A7 FAIL "resize back did not produce CHROMIUM RESIZED 800x348"
            fi
            fi
        else
            record A7 FAIL "after resize, the new rect centre did not hit the adopted surface with rect-relative coords (350,150)"
        fi
    else
        record A7 FAIL "no CHROMIUM RESIZED 700x300 after bridge resize (configure never reached the client); last chromium lines: $(log_last "$CHROMIUM_LOG_NAME" "CHROMIUM")"
    fi
    fi
else
    record A7 NOT-PROVEN "blocked: adopt/geometry failed (A5)"
fi

# --- A8: focus --------------------------------------------------------------------
if [ "$ADOPT_OK" = 1 ]; then
    A8_OK=1
    # Click content -> adopted surface focus, keys reach it.
    BEFORE_BTN="$(log_count "$CHROMIUM_LOG_NAME" "PTR BUTTON")"
    BEFORE_KEY="$(log_count "$CHROMIUM_LOG_NAME" "KEY .* key=30")"
    ptr_move "$CONTENT_CX" "$CONTENT_CY"; sleep 0.3
    ptr_click; sleep 0.3
    key_press a; sleep 0.5
    wait_log_new "$CHROMIUM_LOG_NAME" "PTR BUTTON .* button=272 state=pressed" "$BEFORE_BTN" "chromium click" \
        || { record A8 FAIL "click on the content did not reach the adopted surface (no PTR BUTTON)"; A8_OK=0; }
    if [ "$A8_OK" = 1 ]; then
        wait_log_new "$CHROMIUM_LOG_NAME" "KEY .* key=30 state=pressed" "$BEFORE_KEY" "chromium key 'a'" \
            || { record A8 FAIL "key 'a' after a content click did not reach the adopted surface"; A8_OK=0; }
    fi
    if [ "$A8_OK" = 1 ]; then
        # Focus may already be on the adopted surface (it takes focus
        # on map, and again when the spoof window died), so the click
        # need not emit a NEW focus line — assert the CURRENT focus
        # is the adopted surface, not a new line.
        if ! log_last "compositor.log" "SEVYN FOCUS KEYBOARD" | grep -q "surface=$CHROMIUM_SURFACE"; then
            record A8 FAIL "compositor did not focus the adopted surface after a content click"
            A8_OK=0
        fi
    fi
    # Click chrome -> Browser focus, keys reach it.
    if [ "$A8_OK" = 1 ]; then
        BEFORE_BTN="$(log_count "$BROWSER_LOG_NAME" "PTR BUTTON")"
        BEFORE_KEY="$(log_count "$BROWSER_LOG_NAME" "KEY .* key=48")"
        ptr_move "$CHROME_CX" "$CHROME_CY"; sleep 0.3
        ptr_click; sleep 0.3
        key_press b; sleep 0.5
        wait_log_new "$BROWSER_LOG_NAME" "PTR BUTTON .* button=272 state=pressed" "$BEFORE_BTN" "browser click" \
            && wait_log_new "$BROWSER_LOG_NAME" "KEY .* key=48 state=pressed" "$BEFORE_KEY" "browser key 'b'" \
            && log_last "compositor.log" "SEVYN FOCUS KEYBOARD" | grep -q "surface=$BROWSER_SURFACE" \
            || { record A8 FAIL "click on the chrome did not focus the Browser / key 'b' did not reach it"; A8_OK=0; }
    fi
    [ "$A8_OK" = 1 ] && record A8 PASS "click content -> adopted focus (keys reach the page); click chrome -> Browser focus (keys reach the app)"
else
    record A8 NOT-PROVEN "blocked: adopt/geometry failed (A5)"
fi

# --- A9: clipboard ------------------------------------------------------------------
start_client clipboard-copy "$COPY_LOG_NAME" copy.pid
start_client clipboard-paste "$PASTE_LOG_NAME" paste.pid
# A9 is a real gate now (clipboard landed): a client that never
# becomes READY fails — the "missing wl_data_device_manager"
# NOT-PROVEN escape died with the implementation.
if ! wait_log "$COPY_LOG_NAME" "CLIPBOARD READY" "copy client READY"; then
    record A9 FAIL "copy client never became READY"
    kill_client copy.pid; kill_client paste.pid
elif ! wait_log "$PASTE_LOG_NAME" "CLIPBOARD READY" "paste client READY"; then
    record A9 FAIL "paste client never became READY"
    kill_client copy.pid; kill_client paste.pid
else
    A9_OK=1
    # Focus the copy window (click it): keyboard enter carries the
    # serial the selection is set with. Click the top-left corner,
    # not the centre: the cascade overlaps the windows by 48 px and
    # the paste window (mapped later, on top) covers the copy
    # window's centre, so a centre click focuses paste instead and
    # copy never offers until paste's deadline kills it.
    COPY_ORIGIN="$(sweep_find "$COPY_LOG_NAME")" || A9_OK=0
    if [ "$A9_OK" = 1 ]; then
        COPY_OX="$(echo "$COPY_ORIGIN" | awk '{print $1 - $3}')"
        COPY_OY="$(echo "$COPY_ORIGIN" | awk '{print $2 - $4}')"
        ptr_move $((COPY_OX + 20)) $((COPY_OY + 20)); sleep 0.3
        ptr_click; sleep 0.5
        wait_log "$COPY_LOG_NAME" "CLIPBOARD COPY OFFERED" "copy offered" \
            || { record A9 FAIL "copy client never offered the selection after being focused"; A9_OK=0; }
    else
        record A9 FAIL "pointer sweep never entered the copy client window"
    fi
    if [ "$A9_OK" = 1 ]; then
        PASTE_ORIGIN="$(sweep_find "$PASTE_LOG_NAME")" || A9_OK=0
        if [ "$A9_OK" = 1 ]; then
            PASTE_OX="$(echo "$PASTE_ORIGIN" | awk '{print $1 - $3}')"
            PASTE_OY="$(echo "$PASTE_ORIGIN" | awk '{print $2 - $4}')"
            # Click the paste window's lower half, not its centre:
            # the copy click raised the copy window above paste, and
            # the copy window covers the paste window's centre, so a
            # centre click re-focuses copy instead of paste. The
            # lower half (local y 75) is below the copy window's
            # bottom edge, so it is paste-only.
            ptr_move $((PASTE_OX + 200)) $((PASTE_OY + 75)); sleep 0.3
            ptr_click; sleep 0.5
            if wait_log "$PASTE_LOG_NAME" "CLIPBOARD PASTE text=sevyn-clipboard-4" "paste received"; then
                wait_client_gone paste.pid || echo "adoption-check: warning: paste client did not exit after pasting (continuing)"
                record A9 PASS "copy offered sevyn-clipboard-4, paste received it (wl_data_device between two clients)"
            else
                record A9 FAIL "paste client never logged CLIPBOARD PASTE text=sevyn-clipboard-4; last paste lines: $(log_last "$PASTE_LOG_NAME" "CLIPBOARD")"
            fi
        else
            record A9 FAIL "pointer sweep never entered the paste client window"
        fi
    fi
    kill_client copy.pid
    wait_client_gone copy.pid || true
fi

# --- A10: pointer lock + relative motion ---------------------------------------------
start_client pointer-lock "$LOCK_LOG_NAME" lock.pid
if ! wait_log "$LOCK_LOG_NAME" "POINTERLOCK READY" "lock client READY"; then
    record A10 FAIL "pointer-lock client never became READY"
    kill_client lock.pid
elif log_grep "$LOCK_LOG_NAME" "POINTERLOCK error: missing pointer-constraints"; then
    record A10 NOT-PROVEN "compositor does not advertise pointer-constraints / relative-pointer (pointer lock not implemented)"
    kill_client lock.pid
else
    A10_OK=1
    LOCK_ORIGIN="$(sweep_find "$LOCK_LOG_NAME")" || A10_OK=0
    if [ "$A10_OK" = 1 ]; then
        LOCK_OX="$(echo "$LOCK_ORIGIN" | awk '{print $1 - $3}')"
        LOCK_OY="$(echo "$LOCK_ORIGIN" | awk '{print $2 - $4}')"
        # The sweep already entered the window (which requests the
        # lock); give the locked event a moment, then move.
        if ! wait_log "$LOCK_LOG_NAME" "POINTER LOCKED" "pointer locked"; then
            # The missing-globals error is only logged on pointer
            # enter, i.e. during the sweep above — so check for it
            # HERE, after the sweep, not only before it.
            if log_grep "$LOCK_LOG_NAME" "POINTERLOCK error: missing pointer-constraints"; then
                record A10 NOT-PROVEN "compositor does not advertise pointer-constraints / relative-pointer (pointer lock not implemented)"
            else
                record A10 FAIL "pointer never locked after entering the lock client window"
            fi
            A10_OK=0
        fi
    else
        record A10 FAIL "pointer sweep never entered the lock client window"
    fi
    if [ "$A10_OK" = 1 ]; then
        ptr_move $((LOCK_OX + 450)) $((LOCK_OY + 180)); sleep 0.3
        ptr_move $((LOCK_OX + 400)) $((LOCK_OY + 150)); sleep 0.5
        if wait_log "$LOCK_LOG_NAME" "RELATIVE MOTION" "relative motion"; then
            REL_LINE="$(log_last "$LOCK_LOG_NAME" "RELATIVE MOTION")"
            REL_DX="$(echo "$REL_LINE" | grep -o "total_dx=-*[0-9]*" | cut -d= -f2)"
            REL_DY="$(echo "$REL_LINE" | grep -o "total_dy=-*[0-9]*" | cut -d= -f2)"
            if [ "${REL_DX:-0}" != "0" ] || [ "${REL_DY:-0}" != "0" ]; then
                record A10 PASS "pointer locked (POINTER LOCKED) and relative motion flowed ($REL_LINE)"
            else
                record A10 FAIL "relative motion events arrived but totalled zero: $REL_LINE"
            fi
        else
            record A10 FAIL "no RELATIVE MOTION after pointer moves while locked"
        fi
    fi
    kill_client lock.pid
    wait_client_gone lock.pid || true
fi

# --- A11: manager (bind / list / activate-ignored / close) -----------------------------
# Track 2's real interface (stage4/manager @ 7dc862ca): the --manager
# client binds zwlr_foreign_toplevel_manager_v1 (v2) + the ext-list
# protocol, lists the open toplevels, and exercises one request per
# run on a target app_id. A throwaway M1 window is the manager-close
# target, so the close test cannot disturb the adoption scene.
# SEAM (resolved at Stage 4 integration): the manager's
# `is_adopted` is Track 1's real adoption registry, so a direct
# activation of the adopted surface IS ignored on this tree. The
# harness checks for the IGNORED marker; when the activation goes
# through instead, that is a FAIL — the seam is real now, and a
# wrong activation is wrong behaviour, not a named gap.
start_client "" "$M1_LOG_NAME" m1.pid
M1_OK=0
if wait_log "$M1_LOG_NAME" "M1CLIENT READY" "M1 client READY"; then
    M1_PID="$(client_pid m1.pid)"
    M1_SURFACE="$(surface_id_for_pid "$M1_PID")"
    M1_SESSION="$(surface_session_for_pid "$M1_PID")"
    [ -n "$M1_SURFACE" ] && M1_OK=1
fi
if [ "$M1_OK" != 1 ]; then
    record A11 FAIL "throwaway M1 client never became READY (manager close target missing)"
    kill_client m1.pid || true
elif [ "${MANAGER_CLIENT_OK:-0}" != 1 ]; then
    record A11 NOT-PROVEN "manager client did not build (wlr + ext-list protocol sources unavailable — see build output)"
    kill_client m1.pid
else
    A11_OK=1
    A11_IGNORED=0
    A11_NOTES=""
    # (1) List: the manager + list protocols enumerate the Browser,
    # the adopted Chromium surface, and the throwaway M1 window.
    run_manager_client "manager-list.log"
    if ! log_grep "manager-list.log" "MANAGER DONE"; then
        if log_grep "manager-list.log" "MANAGER error"; then
            record A11 NOT-PROVEN "manager client reported: $(log_last "manager-list.log" "MANAGER error")"
        else
            record A11 FAIL "manager client never finished its listing (no MANAGER DONE)"
        fi
        A11_OK=0
    else
        for want in "MANAGER TOPLEVEL app_id=sevyn.browser" "MANAGER TOPLEVEL app_id=chromium" "MANAGER TOPLEVEL app_id=sevyn.m1.test" "LIST TOPLEVEL identifier=.*app_id=sevyn.browser" "LIST TOPLEVEL identifier=.*app_id=chromium"; do
            if ! log_grep "manager-list.log" "$want"; then
                record A11 FAIL "manager listing is missing: $want"
                A11_OK=0
                break
            fi
        done
    fi
    # (2) Activate-ignored (needs a live adoption): a direct manager
    # activation of the adopted surface must be IGNORED.
    if [ "$A11_OK" = 1 ]; then
        if [ "$ADOPT_OK" != 1 ]; then
            A11_NOTES="adopted-activation NOT-PROVEN: blocked (adopt failed, A4)"
        else
            run_manager_client "manager-activate-chromium.log" "chromium" "activate"
            if log_grep "compositor.log" "MANAGER ACTIVATE IGNORED id=$CHROMIUM_SURFACE adopted"; then
                A11_IGNORED=1
            elif log_grep "compositor.log" "MANAGER ACTIVATE id=$CHROMIUM_SURFACE"; then
                record A11 FAIL "manager activate of the adopted surface went through (MANAGER ACTIVATE id=$CHROMIUM_SURFACE) — the adopted surface must be ignored; the is_adopted seam is real on this tree, so this is wrong behaviour, not a named gap"
                A11_OK=0
            else
                record A11 FAIL "manager activate of the adopted surface produced neither IGNORED nor ACTIVATE in the compositor log"
                A11_OK=0
            fi
            # Routing unchanged after the attempt, either way. Probe
            # the LEFT edge of the rect (20,300 -> local 20,148), not
            # the content centre: the throwaway M1 window (cascade
            # (96,96), 800x300) sits above the adopted surface and
            # covers the content centre, so a centre probe would hit
            # the M1 window, not the adopted surface. The left edge
            # is left of every cascaded window (min cascade x is 48)
            # and still proves the rect-relative routing.
            if [ "$A11_OK" = 1 ]; then
                probe_enter_neutral
                probe_enter "$CHROMIUM_LOG_NAME" $((BROWSER_OX + 20)) $((BROWSER_OY + 300)) 20 148 >/dev/null \
                    || { record A11 FAIL "after the manager activate attempt, the rect's left edge no longer hits the adopted surface"; A11_OK=0; }
            fi
        fi
    fi
    # (3) The Browser window still activates through the manager.
    if [ "$A11_OK" = 1 ]; then
        BEFORE_FOCUS="$(log_count "compositor.log" "SEVYN FOCUS KEYBOARD surface=$BROWSER_SURFACE")"
        run_manager_client "manager-activate-browser.log" "sevyn.browser" "activate"
        if ! log_grep "compositor.log" "MANAGER ACTIVATE id=$BROWSER_SURFACE"; then
            record A11 FAIL "manager activate of the Browser produced no MANAGER ACTIVATE id=$BROWSER_SURFACE"
            A11_OK=0
        elif ! wait_log_new "compositor.log" "SEVYN FOCUS KEYBOARD surface=$BROWSER_SURFACE" "$BEFORE_FOCUS" "keyboard focus on the Browser after manager activate"; then
            record A11 FAIL "manager activate of the Browser did not move keyboard focus to it"
            A11_OK=0
        fi
    fi
    # (4) Close the throwaway M1 window through the manager.
    if [ "$A11_OK" = 1 ]; then
        run_manager_client "manager-close-m1.log" "sevyn.m1.test" "close"
        if ! log_grep "manager-close-m1.log" "MANAGER CLOSE app_id=sevyn.m1.test"; then
            record A11 FAIL "manager client never attempted the close (no MANAGER CLOSE app_id=sevyn.m1.test)"
            A11_OK=0
        elif ! log_grep "compositor.log" "MANAGER CLOSE id=$M1_SURFACE ok=true"; then
            record A11 FAIL "compositor never confirmed the manager close (no MANAGER CLOSE id=$M1_SURFACE ok=true)"
            A11_OK=0
        elif ! wait_client_gone m1.pid; then
            record A11 FAIL "throwaway window did not exit after manager close"
            A11_OK=0
        elif ! log_grep "compositor.log" "SEVYN SESSION STOPPED session=$M1_SESSION.*reason=closed"; then
            record A11 FAIL "throwaway window exited after manager close but no SESSION STOPPED (closed) was recorded"
            A11_OK=0
        fi
    fi
    kill_client m1.pid || true
    if [ "$A11_OK" = 1 ]; then
        if [ "$A11_IGNORED" = 1 ]; then
            record A11 PASS "manager bound + listed (Browser, adopted Chromium, M1 via manager + list protocols); direct activation of the adopted surface IGNORED; Browser still activates via the manager; manager close exited the throwaway window"
        else
            record A11 NOT-PROVEN "manager list/activate/close proven (Browser activates via the manager, throwaway closes via the manager); $A11_NOTES"
        fi
    fi
fi

# --- A12: adopted-client death ---------------------------------------------------------
if [ "$ADOPT_OK" != 1 ]; then
    record A12 NOT-PROVEN "blocked: adopt failed (A4)"
    record A13 NOT-PROVEN "blocked: adopt failed (A4)"
    record A14 NOT-PROVEN "blocked: adopt failed (A4)"
else
    kill_client chromium.pid TERM
    # The event lands in the bridge outfile (nested: $BRIDGE_OUT;
    # standalone: /tmp/bridge-out.txt) — bridge_event_wait reads the
    # right one per backend. The event line carries the session.
    EVENT_OK=0
    if bridge_event_wait "chromium_died.*$BROWSER_SESSION" "chromium_died event for session $BROWSER_SESSION"; then
        EVENT_OK=1
    fi
    wait_client_gone chromium.pid || echo "adoption-check: warning: chromium client did not exit after SIGTERM (continuing)"
    if [ "$EVENT_OK" != 1 ]; then
        record A12 FAIL "no chromium_died event on the bridge after killing the adopted client"
    elif ! client_alive browser.pid; then
        record A12 FAIL "the Browser window died when the adopted client died (it must survive)"
    else
        probe_enter_neutral
        if probe_enter "$BROWSER_LOG_NAME" "$CONTENT_CX" "$CONTENT_CY" 400 300 >/dev/null; then
            record A12 PASS "chromium_died event received; content area emptied (probes hit the Browser at content coords 400,300); Browser window survived"
        else
            record A12 FAIL "after the adopted client died, the content centre did not hit the Browser (content area did not empty)"
        fi
    fi

    # --- A13: re-adoption after death -------------------------------------------------
    start_client chromium "$CHROMIUM2_LOG_NAME" chromium2.pid
    if ! wait_log "$CHROMIUM2_LOG_NAME" "CHROMIUM READY" "second Chromium READY"; then
        record A13 FAIL "second Chromium stand-in never became READY"
    else
        CHROMIUM2_PID="$(client_pid chromium2.pid)"
        CHROMIUM2_SURFACE="$(surface_id_for_pid "$CHROMIUM2_PID")"
        CHROMIUM2_SESSION="$(surface_session_for_pid "$CHROMIUM2_PID")"
        if [ -z "$CHROMIUM2_SURFACE" ]; then
            record A13 FAIL "could not read the second Chromium surface id"
        elif ! bridge_req_ok "{\"v\":1,\"op\":\"adopt\",\"session\":\"$BROWSER_SESSION\",\"chromium_pid\":$CHROMIUM2_PID,\"rect\":{\"x\":0,\"y\":152,\"w\":800,\"h\":348}}" "A13 re-adopt" >/dev/null; then
            record A13 FAIL "re-adopt refused after the first Chromium died"
        elif ! log_grep "compositor.log" "ADOPTED session=$BROWSER_SESSION browser=$BROWSER_SURFACE chromium=$CHROMIUM2_SURFACE"; then
            record A13 FAIL "re-adopt answered ok:true but no ADOPTED marker for surface $CHROMIUM2_SURFACE"
        else
            probe_enter_neutral
            if probe_enter "$CHROMIUM2_LOG_NAME" "$CONTENT_CX" "$CONTENT_CY" 400 148 >/dev/null; then
                record A13 PASS "re-adoption after death works (surface=$CHROMIUM2_SURFACE, content centre hits it)"
            else
                record A13 FAIL "re-adopted surface is not hit at the content centre"
            fi
        fi
    fi

    # --- A14: close kills the adopted client -------------------------------------------
    if ! bridge_req_ok "{\"v\":1,\"op\":\"close\",\"session\":\"$BROWSER_SESSION\"}" "A14 bridge close" >/dev/null; then
        record A14 FAIL "bridge close of the Browser session refused"
    else
        BROWSER_GONE=0; CHROMIUM_GONE=0
        wait_client_gone browser.pid && BROWSER_GONE=1
        wait_client_gone chromium2.pid && CHROMIUM_GONE=1
        if [ "$BROWSER_GONE" != 1 ]; then
            record A14 FAIL "Browser client did not exit after bridge close"
        elif [ "$CHROMIUM_GONE" != 1 ]; then
            record A14 FAIL "adopted Chromium client did not exit after the Browser was closed (close must kill the adopted client; no orphan)"
        elif log_grep "compositor.log" "SEVYN SESSION STOPPED session=$BROWSER_SESSION.*reason=closed" \
            && log_grep "compositor.log" "SEVYN SESSION STOPPED session=$CHROMIUM2_SESSION" ; then
            record A14 PASS "bridge close exited the Browser AND the adopted Chromium client; both sessions recorded stopped"
        else
            record A14 FAIL "both clients exited but the SESSION STOPPED records are missing"
        fi
    fi
fi

# --- A15: Runtime death -> degraded ----------------------------------------------------
# Fresh Browser + Chromium pair; the Runtime connection is dropped
# deliberately (same process, same pid — a bridge control line, not a
# kill), then the degraded contract is checked in three parts:
#   (1) a fresh connection from any OTHER pid is refused at accept
#       (EOF — bridge_oneshot sees NO-RESPONSE; the compositor logs
#       "bridge: rejected a connection from pid=" on stderr);
#   (2) the compositor stays degraded (no RUNTIME RECONNECTED marker)
#       and the scene keeps presenting;
#   (3) reconnecting from the Runtime pid clears degraded (RUNTIME
#       RECONNECTED) and the bridge validates normally again.
# The old expectation — an ok:false "runtime disconnected" answer on a
# fresh connection — is UNREACHABLE BY DESIGN and is NOT asserted:
# any connection accepted while degraded IS the Runtime's
# re-registration (there is no separate register op in v1), so the
# adopt handler's refusal is defense-in-depth and the refusal is
# enforced at accept time (wrong pid -> EOF), not in the handler.
# (Track 1's throwaway harness proved exactly these semantics
# standalone: DISCONNECTED marker -> wrong-pid EOF -> still degraded
# -> same-pid RECONNECTED -> bridge works again, 54/54.)
# NB: the third Chromium gets its OWN log + pid file
# (client-chromium3.log) — reusing the first Chromium's log would
# O_TRUNC the A5-A7 probe evidence.
start_client browser "$BROWSER2_LOG_NAME" browser2.pid
start_client chromium "$CHROMIUM3_LOG_NAME" chromium3.pid
A15_SKIP=""
if ! wait_log "$BROWSER2_LOG_NAME" "BROWSER READY" "second Browser READY"; then
    record A15 FAIL "second Browser stand-in never became READY"
    A15_SKIP=1
elif ! wait_log "$CHROMIUM3_LOG_NAME" "CHROMIUM READY" "third Chromium READY"; then
    record A15 FAIL "third Chromium stand-in never became READY"
    A15_SKIP=1
fi
if [ -z "$A15_SKIP" ]; then
    BROWSER2_PID="$(client_pid browser2.pid)"
    CHROMIUM3_PID="$(client_pid chromium3.pid)"
    BROWSER2_SESSION="$(surface_session_for_pid "$BROWSER2_PID")"
    if [ -z "$BROWSER2_SESSION" ] || [ -z "$CHROMIUM3_PID" ]; then
        record A15 FAIL "could not read the second Browser session / third Chromium pid"
    elif ! compositor_alive; then
        record A15 FAIL "compositor stopped answering ping before the Runtime-death test"
    else
        RECON_BEFORE="$(log_count "compositor.log" "RUNTIME RECONNECTED")"
        RECON_BEFORE="${RECON_BEFORE:-0}"
        # Drop the Runtime connection (control line; bridge.py stays
        # alive with the same pid, so its reconnect is the same Runtime).
        bridge_control drop
        if ! wait_log "compositor.log" "RUNTIME DISCONNECTED" "RUNTIME DISCONNECTED after Runtime drop"; then
            record A15 FAIL "no RUNTIME DISCONNECTED marker after dropping the Runtime connection"
        else
            # (1) Wrong-pid probe: a fresh connection must be refused
            # at accept (EOF, no response at all).
            ONESHOT_OUT="$(bridge_oneshot "{\"v\":1,\"op\":\"adopt\",\"session\":\"$BROWSER2_SESSION\",\"chromium_pid\":$CHROMIUM3_PID,\"rect\":{\"x\":0,\"y\":152,\"w\":800,\"h\":348}}" 2>/dev/null)" || true
            if [ "$ONESHOT_OUT" != "NO-RESPONSE" ]; then
                record A15 FAIL "wrong-pid connection was NOT refused at accept after Runtime death (expected EOF, got: $ONESHOT_OUT)"
            # (2) Still degraded: no RECONNECTED marker appeared (a
            # spurious reconnect would be immediate, so a short
            # settle + count comparison is enough — no need to burn
            # the full wait_log timeout), and the Browser window is
            # still presenting.
            elif sleep 3; [ "$(log_count "compositor.log" "RUNTIME RECONNECTED")" = "$RECON_BEFORE" ]; then
                if ! client_alive browser2.pid; then
                    record A15 FAIL "Browser died with the Runtime (the scene must keep presenting)"
                else
                    # (3) Reconnect from the Runtime pid: degraded
                    # clears and the bridge validates normally again.
                    bridge_control reconnect
                    if ! wait_log "compositor.log" "RUNTIME RECONNECTED" "RUNTIME RECONNECTED after Runtime reconnect"; then
                        record A15 FAIL "no RUNTIME RECONNECTED marker after the Runtime reconnected"
                    elif ! compositor_alive; then
                        record A15 FAIL "ping failed after the Runtime reconnected (bridge must work again)"
                    else
                        RESP3="$(bridge_req "{\"v\":1,\"op\":\"adopt\",\"session\":\"no-such-session\",\"chromium_pid\":$CHROMIUM3_PID,\"rect\":{\"x\":0,\"y\":152,\"w\":800,\"h\":348}}" 2>/dev/null)" || true
                        if echo "$RESP3" | grep -q '"ok": *false'; then
                            record A15 PASS "after Runtime death: wrong-pid connection refused at accept (EOF), still degraded with the scene presenting, same-pid reconnect cleared degraded and the bridge validates again ($RESP3)"
                        else
                            record A15 FAIL "bridge accepted an adopt for an unknown session after reconnect (validation must work again): $RESP3"
                        fi
                    fi
                fi
            else
                record A15 FAIL "a wrong-pid probe cleared degraded (RUNTIME RECONNECTED appeared without the Runtime reconnecting)"
            fi
        fi
    fi
    kill_client browser2.pid; kill_client chromium3.pid
fi

# --- A16: clean shutdown, no orphans ---------------------------------------------------
if [ "$BACKEND" = "nested" ]; then
    COMPOSITOR_PID="$(cat "$WORK/compositor.pid")"
    kill -TERM "$COMPOSITOR_PID"
    deadline=$((SECONDS + TIMEOUT))
    while kill -0 "$COMPOSITOR_PID" 2>/dev/null && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    if kill -0 "$COMPOSITOR_PID" 2>/dev/null; then
        record A16 FAIL "compositor did not exit on SIGTERM"
    else
        wait "$COMPOSITOR_PID"
        COMPOSITOR_STATUS=$?
        if [ "$COMPOSITOR_STATUS" -ne 0 ]; then
            record A16 FAIL "compositor exited with status $COMPOSITOR_STATUS, not 0"
        elif ! grep -q "SEVYN SHUTDOWN clean" "$COMPOSITOR_LOG"; then
            record A16 FAIL "no SEVYN SHUTDOWN clean marker"
        elif pgrep -f "sevyn-test-clien[t]" >/dev/null 2>&1; then
            echo "adoption-check: A16 leftovers:"; ps -ef | grep '[s]evyn-test-client' || true
            record A16 FAIL "test client processes remain after compositor shutdown"
        else
            record A16 PASS "compositor exited 0 on SIGTERM, SHUTDOWN clean, no orphans"
        fi
    fi
else
    ssh_guest "sudo kill -TERM \$(cat /tmp/compositor.pid) 2>/dev/null || true"
    deadline=$((SECONDS + TIMEOUT))
    while [ "$SECONDS" -lt "$deadline" ]; do
        ssh_guest "test -f /tmp/compositor.exit" 2>/dev/null && break
        sleep 1
    done
    EXIT_CODE="$(ssh_guest "cat /tmp/compositor.exit 2>/dev/null" | tr -d ' ')"
    log_fetch "compositor.log" "$WORK/compositor.log"
    if [ "$EXIT_CODE" != "0" ]; then
        record A16 FAIL "compositor exit code was ${EXIT_CODE:-(none)}, not 0"
    elif ! grep -q "SEVYN SHUTDOWN clean" "$WORK/compositor.log"; then
        record A16 FAIL "no SEVYN SHUTDOWN clean marker (standalone)"
    # NB: the [t] bracket trick (as in the nested branch) — without it
    # pgrep -f matches the remote shell running the pgrep itself
    # (its cmdline contains the literal pattern), a false positive
    # that failed standalone A16 twice while the ps dump came back
    # empty.
    elif ssh_guest "pgrep -f 'sevyn-test-clien[t]' >/dev/null 2>&1"; then
        echo "adoption-check: A16 leftovers in the guest:"
        ssh_guest "ps -ef | grep '[s]evyn-test-client'" 2>/dev/null || true
        record A16 FAIL "test client processes remain in the guest after compositor shutdown"
    else
        record A16 PASS "compositor exited 0 on SIGTERM, SHUTDOWN clean, no orphans (standalone)"
    fi
fi

# --- Ledger + exit ----------------------------------------------------------------------
echo "adoption-check: --- ledger ($BACKEND) ---"
cat "$LEDGER"
N_PASS="$(grep -c " PASS " "$LEDGER" || true)"
N_FAIL="$(grep -c " FAIL " "$LEDGER" || true)"
N_NP="$(grep -c " NOT-PROVEN " "$LEDGER" || true)"
echo "adoption-check: PASS=$N_PASS FAIL=$N_FAIL NOT-PROVEN=$N_NP"
if [ "$BACKEND" = "standalone" ]; then
    {
        echo "adoption-check measurements (QEMU virtio-gpu, KVM box 45.77.162.169, $(date -u +%Y-%m-%dT%H:%M:%SZ))"
        echo "backend=standalone output=${OUT_W}x${OUT_H}"
        echo "pass=$N_PASS fail=$N_FAIL not_proven=$N_NP"
        cat "$LEDGER"
    } >"$WORK/measurements.txt"
    preserve_logs
fi
# Exit status: any FAIL fails the run. NOT-PROVEN fails it too,
# EXCEPT the declared Track 3 gap (A10 pointer lock), which is
# missing by design on this branch and named in the PR — it is
# recorded in the ledger, not hidden. (A9 clipboard used to be a
# second gap; it landed with the data-device implementation, so a
# NOT-PROVEN there is unexpected now.) Any other NOT-PROVEN (e.g. a
# PASS regressing because a protocol stopped being advertised) still
# fails the run.
N_NP_UNEXPECTED="$(grep " NOT-PROVEN " "$LEDGER" | grep -vc "^A10 " || true)"
if [ "$N_FAIL" -gt 0 ] || [ "$N_NP_UNEXPECTED" -gt 0 ]; then
    echo "adoption-check: NOT GREEN — $N_FAIL failed, $N_NP not proven ($N_NP_UNEXPECTED unexpected; A10 is the declared Track 3 gap)"
    exit 1
fi
if [ "$N_NP" -gt 0 ]; then
    echo "adoption-check: PASS with $N_NP declared NOT-PROVEN (Track 3: A10 pointer lock)"
fi
echo "adoption-check: PASS — all Stage 4 adoption milestones proven ($BACKEND)"
exit 0
