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
