#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# standalone-guest.sh — guest-side phases for the Stage 3 standalone check.
#
# Runs INSIDE the QEMU guest (Ubuntu cloud image) on the KVM box, invoked
# over SSH by compositor/tests/standalone-check.sh (the host driver).
# The host driver owns the assertions; this script only performs the
# guest-side steps and prints what it did. Run as root (the host driver
# invokes it via sudo).
#
# Stages:
#   setup                  seatd up, test client built
#   start-compositor <mode> <socket>
#                          mode: gles | pixman | panic (SEVYN_TEST_PANIC=1)
#                          launches detached; exit code lands in
#                          /tmp/compositor.exit when the process ends
#   start-clients <socket> Shell + app stand-ins (distinct app_ids)
#   start-one-client <socket> <app_id> <title> <log> <pidfile>
#   bridge-close <session> close one session via the bridge socket
#   rss                    compositor RSS in KiB
#   stop-compositor        SIGTERM, wait for the exit code, print it
#   status                 what is still running in the guest
#
# Paths (all in the guest): compositor /tmp/sevyn-compositor, client
# /tmp/sevyn-test-client, client source /tmp/test-client.c, logs
# /tmp/compositor.log, /tmp/client-shell.log, /tmp/client-app.log,
# /tmp/genesis.log (the compositor's own log file).

set -u

XDG_RUNTIME_DIR=/tmp/sevyn-xdg
export XDG_RUNTIME_DIR
COMPOSITOR_BIN=/tmp/sevyn-compositor
CLIENT_BIN=/tmp/sevyn-test-client
CLIENT_SRC=/tmp/test-client.c

stage="${1:-}"
case "$stage" in
setup)
    # Optional second arg: `vkms` — load the virtual KMS driver so
    # this guest's only DRM device is vkms (the Pixman guest; see
    # standalone-check.sh). virtio-gpu's plane advertises no explicit
    # modifiers, vkms advertises LINEAR for all its formats.
    if [ "${2:-}" = "vkms" ]; then
        # The cloud image's kernel does not ship vkms (it lives in
        # linux-modules-extra); install the extra modules for the
        # running kernel first if the module is missing.
        if ! modprobe vkms 2>/dev/null; then
            echo "guest: vkms module missing; installing linux-modules-extra-$(uname -r)..."
            apt-get update -qq 2>&1 | tail -2 || true
            apt-get install -y "linux-modules-extra-$(uname -r)" \
                || { echo "guest: could not install linux-modules-extra" >&2; exit 1; }
            modprobe vkms || { echo "guest: modprobe vkms failed" >&2; exit 1; }
        fi
        deadline=$((SECONDS + 15))
        while [ ! -e /dev/dri/card0 ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
        [ -e /dev/dri/card0 ] || { echo "guest: no /dev/dri/card0 after modprobe vkms" >&2; exit 1; }
        echo "guest: vkms loaded, /dev/dri/card0 is vkms"
    fi
    mkdir -p "$XDG_RUNTIME_DIR"
    chmod 700 "$XDG_RUNTIME_DIR"
    command -v gcc >/dev/null || { echo "guest: gcc not found" >&2; exit 1; }
    command -v wayland-scanner >/dev/null || { echo "guest: wayland-scanner not found" >&2; exit 1; }
    command -v python3 >/dev/null || { echo "guest: python3 not found" >&2; exit 1; }
    # seatd: the libseat session the standalone backend needs. It
    # daemonizes itself; the socket is /run/seatd.sock.
    if [ ! -S /run/seatd.sock ]; then
        command -v seatd >/dev/null || { echo "guest: seatd not found" >&2; exit 1; }
        seatd >/tmp/seatd.log 2>&1 </dev/null &
        deadline=$((SECONDS + 15))
        while [ ! -S /run/seatd.sock ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
        [ -S /run/seatd.sock ] || { echo "guest: seatd socket never appeared" >&2; exit 1; }
    fi
    echo "guest: seatd up, socket /run/seatd.sock"
    # Build the test client (same wayland-scanner steps as nested-check.sh).
    XDG_SHELL_XML="/usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml"
    PRESENTATION_XML="/usr/share/wayland-protocols/stable/presentation-time/presentation-time.xml"
    [ -f "$XDG_SHELL_XML" ] || { echo "guest: xdg-shell.xml not found" >&2; exit 1; }
    [ -f "$PRESENTATION_XML" ] || { echo "guest: presentation-time.xml not found" >&2; exit 1; }
    BUILD_DIR="$(mktemp -d /tmp/sevyn-client-build.XXXXXX)"
    wayland-scanner client-header "$XDG_SHELL_XML" "$BUILD_DIR/xdg-shell-client-protocol.h" \
        || { echo "guest: xdg-shell client-header failed" >&2; exit 1; }
    wayland-scanner private-code "$XDG_SHELL_XML" "$BUILD_DIR/xdg-shell-protocol.c" \
        || { echo "guest: xdg-shell private-code failed" >&2; exit 1; }
    wayland-scanner client-header "$PRESENTATION_XML" "$BUILD_DIR/presentation-time-client-protocol.h" \
        || { echo "guest: presentation client-header failed" >&2; exit 1; }
    wayland-scanner private-code "$PRESENTATION_XML" "$BUILD_DIR/presentation-time-protocol.c" \
        || { echo "guest: presentation private-code failed" >&2; exit 1; }
    gcc -o "$CLIENT_BIN" "$CLIENT_SRC" "$BUILD_DIR/xdg-shell-protocol.c" \
        "$BUILD_DIR/presentation-time-protocol.c" \
        -I"$BUILD_DIR" $(pkg-config --cflags --libs wayland-client) \
        || { echo "guest: test client build failed" >&2; exit 1; }
    rm -rf "$BUILD_DIR"
    echo "guest: test client built at $CLIENT_BIN"
    ls -l /dev/dri/card* 2>&1 || echo "guest: WARNING no /dev/dri/card* (compositor start will fail cleanly)"
    ls /dev/input/event* 2>&1 | head -8
    ;;
start-compositor)
    mode="${2:-gles}"
    socket="${3:-sevyn-standalone-test}"
    [ -x "$COMPOSITOR_BIN" ] || { echo "guest: compositor binary not found: $COMPOSITOR_BIN" >&2; exit 1; }
    rm -f /tmp/compositor.exit /tmp/compositor.log /tmp/compositor.err
    case "$mode" in
    gles) extra_args="" ; panic_env="" ;;
    pixman) extra_args="--no-gles" ; panic_env="" ;;
    panic) extra_args="" ; panic_env="SEVYN_TEST_PANIC=1" ;;
    *) echo "guest: unknown compositor mode: $mode" >&2; exit 1 ;;
    esac
    # Detached wrapper: runs the compositor in the background, records
    # its real pid (the process name is 17 chars, longer than the
    # kernel's 15-char comm, so pgrep -x can never match it — the pid
    # file is the reliable handle), waits for it, and records its exit
    # code. setsid + nohup + closed stdio so the process survives the
    # SSH session that started it.
    # shellcheck disable=SC2086
    setsid nohup bash -c "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR $panic_env $COMPOSITOR_BIN --standalone --socket $socket $extra_args >/tmp/compositor.log 2>/tmp/compositor.err & echo \$! > /tmp/compositor.pid; wait \$!; echo \$? > /tmp/compositor.exit" >/dev/null 2>&1 &
    echo "guest: compositor started mode=$mode socket=$socket"
    ;;
start-clients)
    socket="${2:-sevyn-standalone-test}"
    "$0" start-one-client "$socket" sevyn.shell "Sevyn Shell" /tmp/client-shell.log /tmp/client-shell.pid
    "$0" start-one-client "$socket" sevyn.app "Sevyn App" /tmp/client-app.log /tmp/client-app.pid
    ;;
start-one-client)
    socket="$2" app_id="$3" title="$4" log="$5" pidfile="$6"
    [ -x "$CLIENT_BIN" ] || { echo "guest: test client not found: $CLIENT_BIN" >&2; exit 1; }
    rm -f "$log"
    # echo $$ then exec: the pid file holds the client's own pid.
    setsid nohup bash -c "echo \$\$ > $pidfile; exec env XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR WAYLAND_DISPLAY=$socket SEVYN_CLIENT_APP_ID=$app_id SEVYN_CLIENT_TITLE=\"$title\" SEVYN_CLIENT_LOG=$log $CLIENT_BIN" >/dev/null 2>&1 &
    echo "guest: client started app_id=$app_id log=$log"
    ;;
bridge-close)
    session="$2"
    python3 - "$XDG_RUNTIME_DIR/sevyn-compositor.sock" "$session" <<'PYEOF'
import json, socket, sys
path, session = sys.argv[1], sys.argv[2]
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.settimeout(5)
sock.connect(path)
sock.sendall(json.dumps({"v": 1, "op": "close", "session": session}).encode() + b"\n")
print(sock.recv(4096).decode().strip())
PYEOF
    ;;
rss)
    [ -f /tmp/compositor.pid ] || { echo "guest: no compositor pid file" >&2; exit 1; }
    pid="$(cat /tmp/compositor.pid)"
    ps -o rss= -p "$pid" || { echo "guest: no sevyn-compositor process $pid" >&2; exit 1; }
    ;;
stop-compositor)
    [ -f /tmp/compositor.pid ] || { echo "guest: no compositor pid file" >&2; exit 1; }
    pid="$(cat /tmp/compositor.pid)"
    if ! kill -0 "$pid" 2>/dev/null; then
        echo "guest: compositor already gone"
        [ -f /tmp/compositor.exit ] && cat /tmp/compositor.exit || echo "no exit file"
        exit 0
    fi
    kill -TERM "$pid"
    deadline=$((SECONDS + 30))
    while [ ! -f /tmp/compositor.exit ] && [ "$SECONDS" -lt "$deadline" ]; do sleep 0.2; done
    if [ -f /tmp/compositor.exit ]; then
        cat /tmp/compositor.exit
    else
        echo "guest: compositor did not exit on SIGTERM" >&2
        exit 1
    fi
    ;;
status)
    echo "guest: compositor pid file: $(cat /tmp/compositor.pid 2>/dev/null || echo none)"
    echo "guest: test clients: $(pgrep -f sevyn-test-client | tr '\n' ' ')"
    echo "guest: weston: $(pgrep -x weston | tr '\n' ' ')"
    echo "guest: Xorg: $(pgrep -x Xorg | tr '\n' ' ')"
    echo "guest: DISPLAY=${DISPLAY-unset}"
    ;;
*)
    echo "guest: unknown stage: $stage" >&2
    exit 1
    ;;
esac
