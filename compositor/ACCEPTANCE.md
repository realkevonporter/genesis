# Acceptance — Stage 3 performance budget record

The budgets are from `COMPOSITOR_PLAN.md`, "Performance budget".
A stage that misses a budget does not ship until the budget is met
or Kevon explicitly re-scopes it. This file records the Stage 3
measurements, filled in from worker 3's standalone check run
(2026-11-20) — no numbers invented here.

## Budgets (Stage 3)

| Metric               | Budget                                                                           | How it is measured                                                                                       |
| -------------------- | -------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------- |
| Memory               | `sevyn-compositor` steady-state RSS ≤ **200 MB** with Shell + one Browser window | RSS of the compositor process once the Shell and one Browser window are up and idle (see commands below) |
| Startup: socket up   | Wayland socket up ≤ **2 s** from exec                                            | `socket_up_ms` in the `SEVYN READY` marker                                                               |
| Startup: first frame | First presented frame ≤ **3 s** from exec                                        | `first_frame_ms` in the `SEVYN FIRST FRAME PRESENTED` marker                                             |

Frame-time (p95 ≤ 16.67 ms) and input-latency budgets are Stage 4–5
items, not Stage 3 — they are not recorded here.

## Measured (worker 3 — standalone check, 2026-11-20)

Measured by `compositor/tests/standalone-check.sh` (run #8, the
official run) on the KVM box 45.77.162.169: QEMU/KVM guest, Ubuntu
24.04 cloud image, virtio-gpu (2D, no virgl — the GLES renderer runs
on Mesa llvmpipe in the guest), output 1280x800@75. Run log:
`/root/genesis-stage3-check/standalone-check-run.log` on the KVM box;
preserved logs in `/root/genesis-stage3-check/last-standalone-run/`
(`measurements.txt`, `compositor-gles.log`, `genesis.log`).

| Metric           | Measured                  | Machine / conditions                   | Date       | Result              |
| ---------------- | ------------------------- | -------------------------------------- | ---------- | ------------------- |
| Steady-state RSS | **125.8 MB** (128820 KiB) | QEMU virtio-gpu, KVM box 45.77.162.169 | 2026-11-20 | **PASS** vs 200 MB  |
| Socket up        | **102 ms**                | QEMU virtio-gpu, KVM box 45.77.162.169 | 2026-11-20 | **PASS** vs 2000 ms |
| First frame      | **184 ms**                | QEMU virtio-gpu, KVM box 45.77.162.169 | 2026-11-20 | **PASS** vs 3000 ms |

Two honest substitutions, recorded so the numbers are not
over-read:

- The budget says "Shell + one Browser window". The real Browser
  does not exist in this repo, so the RSS was measured with the
  Shell stand-in + the app stand-in (the C test client,
  `sevyn.shell` + `sevyn.app`, two 800x300 surfaces committed and
  idle) — the same two-surface load the budget intends, but with
  stand-ins, not a real browser.
- These are QEMU numbers, not GF65 numbers. The 60 fps bar is
  proven only on GF65 hardware; nothing here claims otherwise.

Repeat runs agreed: RSS 125.8–126.6 MB, socket-up 90–145 ms, first
frame 148–225 ms across the official runs (the earlier runs differ
only because the box was also running the coordinator's SevynOS
runner jobs during some of them).

The Pixman fallback is **NOT PROVEN** on this box (recorded in
`measurements.txt` as `pixman=NOT_PROVEN`): the virtio-gpu plane
advertises no explicit modifiers (Invalid only) and vkms advertises
LINEAR but Mesa GBM reports every buffer it creates on these virtual
drivers as modifier Invalid (direct gbm-probe evidence), while
Smithay's Pixman renderer refuses to bind anything but LINEAR — so
no frame can present through the Pixman path on any DRM device
available here. The compositor side is correct (it negotiates LINEAR
when the plane offers it; `READY renderer=pixman` is reached). The
check records this as NOT PROVEN and exits 1 rather than faking a
pass; real-GPU proof is owed (GF65).

## Exact commands to fill them in

Run the standalone compositor (worker 3's QEMU/KVM check) and read
the markers from the log — the compositor prints both startup
numbers itself:

```sh
# 1. Boot the standalone compositor (QEMU/KVM or GF65).
#    The READY marker carries socket_up_ms; the FIRST FRAME marker
#    carries first_frame_ms. Both are also in /tmp/genesis.log.
sevyn-compositor --standalone
# ... wait for:
# SEVYN READY ... backend=drm ... socket_up_ms=<N>
# SEVYN FIRST FRAME PRESENTED ... first_frame_ms=<N>
```

```sh
# 2. RSS once the Shell + one Browser window are up and idle.
#    Use the compositor's own pid (not the Shell's, not the Browser's):
pidof sevyn-compositor
ps -o rss= -p <pid>          # KiB; divide by 1024 for MB
# or:
grep VmRSS /proc/<pid>/status
```

```sh
# 3. Cross-check from the log file (the numbers are also recorded there):
grep -E "SEVYN (READY|FIRST FRAME PRESENTED)" /tmp/genesis.log
```

## What was proven in this sandbox (Stage 3 worker 2)

These are the checks that _could_ run without DRM hardware. They
prove function, not the budget — the budget numbers above still need
worker 3's standalone run.

- `cargo build --locked`, `cargo clippy --locked -- -D warnings`,
  `cargo fmt --check`, `cargo test --locked` — all pass (10 tests).
- `compositor/tests/nested-check.sh` — PASS (all Stage 2 milestones).
- Protocol smoke run (scratch client in `/tmp/genesis-stage3-smoke`,
  not shipped): booted the nested compositor under Weston, connected
  a Wayland client, and confirmed all 9 Stage 3 globals advertised
  and bound — `zwlr_layer_shell_v1` v4, `wp_presentation` v2,
  `wp_viewporter` v1, `wp_content_type_manager_v1` v1,
  `wp_single_pixel_buffer_manager_v1` v1, `xdg_activation_v1` v1,
  `zwp_idle_inhibit_manager_v1` v1, `ext_idle_notifier_v1` v2,
  `wp_cursor_shape_manager_v1` v2 — plus the round-trips:
  presentation `presented` event, activation token `done`, idle
  `idled` then `resumed` after a pointer move, layer-surface
  `configure` (arranged 200x24), idle-inhibit create/destroy
  (`IDLE INHIBIT count=1` / `IDLE UNINHIBIT count=0` in the
  compositor log), and `/tmp/genesis.log` containing the markers.
  First layer configure arrived as 640x400 before the arranged
  200x24 configure — the initial configure carries the pre-arrange
  size; the arranged configure is the one clients must use.
- Touch routing verified by reading `src/input.rs`: touch down /
  motion / up call the same `hit_test` and the same raise +
  click-to-focus rule as pointer (no code change needed).
- Standalone proof is worker 3's job — done 2026-11-20 (see
  "Measured (worker 3)" above): all Stage 3 milestones proven on
  real DRM in QEMU/KVM except the Pixman fallback, which is NOT
  PROVEN on this box's virtual DRM stack (evidence in the check
  script header and `measurements.txt`); GF65 proof is still owed.

## Chromium 159 dmabuf spike (Stage 4, Track 4 — 2026-11-20)

Real Chromium as launched by `sevyn browser`, against the standalone
compositor on the KVM box (45.77.162.169, QEMU virtio-gpu guest).
Harness: `compositor/tests/chromium-spike.sh` (three phases per run:
`--app=about:blank`; same + `--use-gl=angle --use-angle=swiftshader
--v=1`; no `--app`). Evidence preserved on the box at
`/root/genesis-stage3-check/last-chromium-spike/` (`verdict.txt`,
`chromium-1/2/3.log` with `WAYLAND_DEBUG=1`, `compositor.log`).

Verdict: **SHM-ONLY — the zero-copy bar FAILS as measured in the
QEMU guest; Stage 5 must know before it is built.**

- Chromium: **159.0.8150.0** (CDP `/json/version` reachable in all
  three phases).
- **app_id is `chromium-browser` in all three phases** — the plan's
  assumed `chromium` is wrong for Chromium 159 as launched by
  `sevyn browser`, with or without `--app`. The adoption track must
  match on the Runtime-declared pid, never on the app_id string
  (the harness's A3 spoof case proves exactly that).
- Chromium binds `zwp_linux_dmabuf_v1` (v4) and reads the format
  table + tranche, but **creates zero dmabuf buffers in every phase**
  (`dmabuf_buffer_creates=0`, `dmabuf_bind=1`) and presents frames
  on SHM buffers only (`shm_buffer_creates=4` per phase). The
  compositor committed the surface (e.g. `SEVYN SURFACE COMMITTED
id=1 size=1018x738`), so the SHM path renders — it is the
  zero-copy path that does not happen.
- Whether the cause is the guest's software GL (llvmpipe) or the
  compositor's dmabuf feedback is **not established** by this spike:
  the GPU process made no Wayland connection of its own, and the
  `--v=1` phase-2 log carries no decisive GL/ANGLE fallback line
  (only binding-version warnings). A GF65 run (real GPU) is the test
  that would settle it; until then, Stage 5 must be built assuming
  SHM fallback is possible, not assuming dmabuf textures.
- The spike itself took six runs to land (v1–v6): v1's verdict logic
  was wrong (launcher-pid aliveness, `@` vs `#` object ids); v2/v3
  died because `pkill -f` matched the launcher's own command line
  (the launch line re-introduces the literal chrome path — fixed
  with `pkill -x chrome`); v4–v6 died on quoting in the remote
  launch block (a bare paren, then an apostrophe, inside a
  single-quoted remote `bash -c` script are live metacharacters
  through the two shell parsings — the rule is now: no apostrophe
  and no bare paren in any comment inside a remote `bash -c` block,
  audited clean in both check scripts).

# Acceptance — Stage 4 adoption milestones (Track 1)

The plan's Stage 4 "Adoption: the mechanism" items, and how each is
proven. Latency rows (frame p95 ≤ 16.67 ms, pointer ≤ 1 frame, key
≤ 3 frames) belong to the input track and are left blank here.

## Milestones

| Item                                                                        | How it is proven                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| --------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| adopt via bridge (`adopt` op, exact shape in README)                        | Bridge request returns `ok:true`; `ADOPTION ADOPTED` marker names the Browser surface, the Chromium surface, and the pid                                                                                                                                                                                                                                                                                                                                                   |
| Hostile adopt rejected: malformed JSON / missing fields / non-positive rect | `ok:false`, error `"malformed request"`                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| Hostile adopt rejected: dead Chromium pid                                   | `ok:false`, error `"dead chromium pid"`                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| Hostile adopt rejected: rect outside the Browser window                     | `ok:false`, error `"rect outside browser window"`                                                                                                                                                                                                                                                                                                                                                                                                                          |
| Hostile adopt rejected: unknown session                                     | `ok:false`, error `"unknown session"`                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| Hostile adopt rejected: app_id spoof (claims `chromium`, wrong pid)         | Never adopted (pid match fails first); the spoof window stays a standalone, focusable window                                                                                                                                                                                                                                                                                                                                                                               |
| Already adopted                                                             | Second `adopt` for the same session → `ok:false`, error `"already adopted"`                                                                                                                                                                                                                                                                                                                                                                                                |
| Move same-frame, no configure                                               | `move` returns `ok:true`, `ADOPTION MOVED` marker, Chromium receives **no** `toplevel configure`; hit-test follows the new rect in the same frame                                                                                                                                                                                                                                                                                                                          |
| Resize configures Chromium                                                  | `resize` returns `ok:true`, `ADOPTION RESIZED` marker, Chromium receives `toplevel configure size=<rect.w>x<rect.h>`                                                                                                                                                                                                                                                                                                                                                       |
| Clipping (both backends)                                                    | Screendump pixels: content area shows Chromium colours, chrome shows Browser colours, below the window is the compositor background, the spoof strip still paints — Chromium pixels never appear outside the rect                                                                                                                                                                                                                                                          |
| Focus routing: click content → Chromium keys                                | Pointer click inside the rect, then a key: the Chromium client logs the key, the Browser client does not                                                                                                                                                                                                                                                                                                                                                                   |
| Focus routing: click chrome → Browser keys                                  | Pointer click outside the rect, then a key: the Browser client logs the key, the Chromium client does not                                                                                                                                                                                                                                                                                                                                                                  |
| Chromium death                                                              | SIGKILL Chromium → `chromium_died` event on the bridge, `ADOPTION CHROMIUM DIED` marker, Browser window stays open                                                                                                                                                                                                                                                                                                                                                         |
| Runtime death / reconnect (degraded)                                        | Bridge disconnect → `RUNTIME DISCONNECTED` marker; a fresh connection from any OTHER pid is refused at accept (EOF) and the compositor stays degraded; reconnect from the Runtime pid → `RUNTIME RECONNECTED` marker, bridge works again. (The adopt handler's `"runtime disconnected"` refusal is defense-in-depth and is unreachable through the bridge — a connection accepted while degraded IS the Runtime's re-registration; A15 asserts the accept-time semantics.) |
| Release                                                                     | `release` returns `ok:true`, `ADOPTION RELEASED` marker; Chromium repaints as a standalone window, takes keys there, re-adopt works, release-again → `"unknown session"`                                                                                                                                                                                                                                                                                                   |
| Browser close drops the adoption                                            | Bridge `close` for the Browser → `ADOPTION DROPPED ... reason=browser-closed`, and the adopted Chromium is asked to close with it (a close is not a release — release is the only path that leaves it standalone); both sessions recorded stopped                                                                                                                                                                                                                          |
| `--runtime-pid` binding                                                     | A connection from any pid other than the Runtime's gets EOF and a `bridge: rejected a connection from pid=...` diagnostic; the Runtime's own connection works                                                                                                                                                                                                                                                                                                              |
| Memory budget (RSS ≤ 200 MB)                                                | Compositor steady-state RSS with the adoption live, same method as Stage 3                                                                                                                                                                                                                                                                                                                                                                                                 |

## Measured (Track 1)

Nested proof (sandbox, 2026-11-20): `cargo build --locked`,
`cargo clippy --locked -- -D warnings`, `cargo fmt --check`,
`cargo test --locked` all pass (14 tests); `nested-check.sh` PASS;
throwaway harness `/tmp/adoption-harness/adoption_harness.py`
**59/59 checks PASS** (final run `run4.log`) — every row above
except the standalone-only items, under Weston (nested backend).
Proof logs preserved at `~/workspace/stage4-adoption-proof/`
(`nested-harness-run.log`, `nested-compositor.log`, `shot.png`).

Standalone proof (KVM box 45.77.162.169, QEMU virtio-gpu, GLES on
llvmpipe, output 1280x800): **PASS 2026-11-20** — the throwaway
harness (`adoption-host.sh` + `adoption-guest.sh` +
`adoption-runtime.py`, preserved in
`~/workspace/stage4-adoption-proof/`; run log on the box at
`/root/genesis-stage4-adoption-check/last-adoption-run/`) drove the
full sequence through the bridge with QMP pointer/keys and
screendump pixels: **54/54 runtime checks PASS, 0 FAIL** — every
row of the milestones table above, including all hostile adopt
cases, move (no configure), resize (configure 800x148), clipping
by pixels (content + chrome + below-window + spoof strip),
focus routing both ways, Chromium-death event + Browser survival,
Runtime degraded/reconnect, release + re-adopt, and Browser close
dropping the adoption. Measurements from the same run:
steady-state RSS **127.0 MB** (130088 KiB, budget 200 MB), socket
up **154 ms** (budget 2000 ms), first frame **270 ms** (budget
3000 ms).

Three real bugs were found and fixed by this proof, all in the
compositor or the harness (not the spec):

- **Standalone bridge wake-up** (`drm.rs`, `runtime_bridge.rs`):
  the calloop loop only woke on the bridge _listener_ fd, so a
  request on an existing connection sat until some other source
  fired. Fixed with one calloop source per live connection
  (`sync_bridge_sources`, dups from
  `connection_watch_fds`), keyed by the dup's own fd number.
- **Reply stuck in the outbox** (`runtime_bridge.rs`): a reply was
  queued and only flushed on the _next_ poll. `poll` now flushes
  a reply in the same call that handled its request
  (`flush_connection`).
- **Harness reader timeout** (throwaway `adoption-runtime.py`):
  the bridge socket's 10 s timeout killed the reader thread after
  10 s of silence (`socket.timeout` is an `OSError`), so later
  replies were never seen. The socket is blocking after connect;
  `close()` shuts it down first to wake the reader (closing the
  makefile first deadlocks against the blocked reader). The same
  timeout bug existed in the nested harness's `Bridge` class — it
  passed there only because the gaps stayed under 10 s.
- **Harness commit-order race** (throwaway `adoption-guest.sh`):
  three parallel client launches race for cascade positions; a
  Browser committing third lands at (96,96) and every fixed
  coordinate is wrong. Client launches are now serialised on each
  client's first commit.

## What is NOT proven

- Real Shell/Chromium embedding is sevynos-side Stage 5 work — the
  proof here uses the C test client as the Chromium stand-in (a
  green/yellow variant so pixels tell it apart from the red/blue
  Browser stand-in) and a Python Runtime stand-in driving the
  bridge.
- GF65 hardware proof is owed; the standalone numbers above are
  QEMU numbers.
- The test-client stand-in logs the `toplevel configure` on resize
  but keeps its fixed 800x300 geometry (it never resizes its
  buffer); resize proof is the configure event, not a client resize.
- No window-move API exists in Stage 2/3, so "Browser window moves
  → same delta, same frame" is proven only via the bridge `move`
  op (a rect change), not via an actual `Space` relocation; the
  derived-location design makes both the same render-time code path.

## Measured (integration — Tracks 1+2+4, 2026-11-20)

`adoption-check.sh` run against the REAL adoption code on the
integrated tree (`stage4/integration`), after fixing three harness
bugs the integration run exposed plus one compositor gap:

- **A4/A7/A13 marker contract**: the harness grepped for the assumed
  marker `ADOPTED session=.. surface=..`, but the real marker is
  `ADOPTION ADOPTED session=.. browser=.. chromium=.. pid=..
rect=..` — `browser=` sits between `session=` and `chromium=`, so
  the old greps could never match (and the A7 spoof grep was a
  false-negative hole: a real spoof adoption would have gone
  undetected). Fixed to the full marker shape.
- **A15 degraded semantics**: the adopt handler's `"runtime
disconnected"` refusal is unreachable through the bridge — a
  connection accepted while degraded IS the Runtime's
  re-registration (v1 has no separate register op) — so the refusal
  is enforced at accept time via `--runtime-pid`. `bridge.py`
  gained retry-connect plus `__bridge__` drop/reconnect control
  lines; both setups launch the compositor with
  `--runtime-pid=<bridge pid>`; A15 asserts wrong-pid EOF, still
  degraded with the scene presenting, same-pid reconnect clearing
  degraded. `adoption.rs` documents the handler refusal as
  defense-in-depth.
- **A14 compositor fix**: `reconcile_adoptions` dropped the adoption
  when the Browser closed but never asked the adopted Chromium to
  close — the window lingered as an orphan. It now sends close to
  the adopted toplevel (a close is not a release; release remains
  the only path that leaves Chromium standalone).
- **Standalone A15 probe privilege**: the one-shot probe ran as
  `sevyn` in the guest, but the bridge socket lives under
  root-owned 700 `/tmp/sevyn-xdg`, so it died with EACCES before
  reaching the compositor. It now runs via sudo (the Runtime's
  privilege); the EOF it observes is still the compositor's
  accept-time pid check.
- **Standalone A16 pgrep self-match**: the standalone `pgrep -f
sevyn-test-client` matched the remote shell running it (the `ps`
  dump on the failure path came back empty). Now uses the `[t]`
  bracket trick, as the nested branch already did.
- **Lesson**: `cargo check` / `clippy` / `test` do not re-link the
  binary — a nested re-run after the A14 fix first ran against the
  stale binary and failed identically. `cargo build` explicitly
  before any harness run.

**Nested** (sandbox, Weston): **14 PASS, 0 FAIL, 2 NOT-PROVEN**
(A9 clipboard, A10 pointer lock — Track 3, still missing).

**Standalone** (KVM box 45.77.162.169, headless QEMU virtio-gpu,
output 1280x800): **14 PASS, 0 FAIL, 2 NOT-PROVEN** (same two).
Run logs preserved on the box at
`/root/genesis-stage3-check/last-adoption-run/` (run 3,
2026-11-20T23:50:24Z).

**DRM page-flip question — NOT REPRODUCED.** Track 4's standalone
runs hit a DRM page-flip EINVAL starvation (10 errors, adopt late).
Both integrated standalone runs on this tree: **zero**
page-flip/EINVAL errors in either guest log, adopt prompt in both;
Track 1's 54/54 standalone proof was also clean. The starvation
appears specific to Track 4's harness/guest setup, not the
compositor. The exact trigger in Track 4's runs was not isolated,
so this is a non-reproduction with two clean runs behind it, not a
root-caused exoneration.
