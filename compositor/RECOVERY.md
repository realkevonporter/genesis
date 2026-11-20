# Recovery contract — Genesis compositor

What happens when Genesis fails, what it guarantees, and what init
(the sevynos image's supervisor) must provide. This is the
"Failure without fallback" section of `COMPOSITOR_PLAN.md`, made
concrete for Stage 3.

## The rule (locked)

Genesis is the one and only shipping compositor. If it fails, the
product does **not** fall back to Weston — it fails safe to a
terminal/recovery environment. "Never a bricked desktop" means a
Genesis failure always lands on a usable console with the `sevyn`
CLI, never a hung black screen.

## What the compositor guarantees on failure

| Failure                                                             | Compositor behavior                                                                                                                                                                                                                                                                                                   | How you can tell                                          |
| ------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------- |
| No DRM device / no usable connector / no libseat session at startup | Fails cleanly with a named error on stderr and exit **1**. Never a panic, never a hung black screen.                                                                                                                                                                                                                  | stderr: `sevyn-compositor: fatal: ...`; exit code 1       |
| Bad CLI argument                                                    | Usage error on stderr and exit **2**.                                                                                                                                                                                                                                                                                 | stderr; exit code 2                                       |
| Claimed DRM device removed while running                            | Logs `SEVYN DRM OUTPUT LOST`, asks its surfaces to close, exits **1**.                                                                                                                                                                                                                                                | `SEVYN DRM OUTPUT LOST` in stdout / `/tmp/genesis.log`    |
| SIGTERM / SIGINT                                                    | Clean shutdown, exit **0**: every surface is asked to close first, then `SEVYN SHUTDOWN clean`.                                                                                                                                                                                                                       | `SEVYN SHUTDOWN clean`; exit code 0                       |
| Panic (bug, e.g. an internal invariant)                             | Emits `SEVYN CRASH panic=... at <file:line:col> (unwinding; DRM master released by fd close on drop)` on stdout and in `/tmp/genesis.log`, plus the panic message on stderr; unwinds (default `panic = "unwind"`); exits **101** (Rust default).                                                                      | `SEVYN CRASH ...` marker; exit code 101                   |
| SIGKILL (supervisor kill, OOM)                                      | No marker — the process is gone without running any code. The kernel closes the process's fds, which releases DRM master.                                                                                                                                                                                             | Process gone; no `SHUTDOWN clean`                         |
| Deliberate test panic (`SEVYN_TEST_PANIC=1`)                        | **Test-only fault injection** (Stage 3 milestone 10): the compositor panics just after `READY` + first frame. Same crash record as a real panic (`SEVYN CRASH` marker, exit 101, DRM master released by the unwind). Never set in production — see `src/drm.rs`; only `compositor/tests/standalone-check.sh` sets it. | `SEVYN CRASH panic=SEVYN_TEST_PANIC=1 ...`; exit code 101 |

### DRM master release (guaranteed)

DRM master is held by the open DRM fd and released when that fd
closes. Every exit path closes it: clean shutdown drops the device
explicitly; a panic unwinds through the same drops; SIGKILL lets the
kernel close the fd. **What is not guaranteed is console/VT state
after a crash mid-modeset** — the kernel VT switch handing the
console back to the recovery environment is the plan's guarantee,
not a compositor one.

### Logs (guaranteed)

- Every marker (`SEVYN ...`, stdout, flushed immediately) and every
  diagnostic (`sevyn-compositor: ...`, stderr) is also appended to
  **`/tmp/genesis.log`**. The file is truncated on start (rotation:
  one run's log; see `src/logging.rs`).
- The log is the crash record: a panic's `SEVYN CRASH` marker, the
  last `READY` / `DPMS` / `LAYER SURFACE` / `IDLE` markers, and the
  named fatal error are all in it. The recovery console's first job
  is to preserve it (see below) — init must not delete it before the
  operator has had a chance to read it.
- If `/tmp/genesis.log` cannot be opened (read-only `/tmp`, disk
  full), the compositor keeps running on stdout/stderr only and says
  so once on stderr. Logging never fails startup.

### Exit codes (summary)

| Code | Meaning                                               |
| ---- | ----------------------------------------------------- |
| 0    | Clean shutdown (signal or window close)               |
| 1    | Fatal startup/runtime failure (named error on stderr) |
| 2    | Usage error                                           |
| 101  | Panic (preceded by a `SEVYN CRASH` marker)            |

## What init must provide (sevynos-image work, not this repo)

The compositor never restarts itself and never falls back to another
compositor. The supervisor owns all of this:

1. **Supervision.** Start `sevyn-compositor --standalone` with
   `$XDG_RUNTIME_DIR` set and a VT session available (logind/seatd).
   Wait for the `SEVYN READY` marker (socket up) before starting
   clients; if `READY` does not appear within the socket-wait
   timeout, treat the start as failed.
2. **Socket-wait timeout → recovery console.** If the Wayland socket
   is not up within the timeout (suggested: 5 s; the budget is
   socket-up ≤ 2 s from exec), or the compositor exits non-zero, or
   it stops responding, stop waiting and bring up the recovery
   console — do not keep waiting, do not retry forever without
   surfacing the failure. The exact timeout is init's choice; the
   contract is only that a timeout lands on the console, never on a
   hung black screen.
3. **Restart policy.** On a non-zero exit, init may restart the
   compositor (with a backoff; a crash loop must surface, not spin).
   Before restarting, **copy `/tmp/genesis.log` off** (e.g. to a
   timestamped file) — the next start truncates it.
4. **`sevyn.recovery=1` direct entry.** The boot path must offer a
   direct entry to the recovery console (kernel cmdline
   `sevyn.recovery=1`), skipping the compositor entirely, for the
   case where the compositor cannot start at all.
5. **The recovery console itself.** A usable console with the `sevyn`
   CLI (busybox `sh` + `sevyn` CLI): enough to read
   `/tmp/genesis.log`, inspect the system, and restart or repair.
   **The console itself is sevynos-image work, not this repo** — it
   is named here as owed, not claimed: nothing in the genesis repo
   builds or ships it. The image must also ensure the console gets
   the VT back after a compositor crash mid-modeset (the case the
   compositor cannot guarantee; see above).

## What is NOT in this repo (owed elsewhere)

- **The recovery console** (busybox `sh` + `sevyn` CLI): sevynos
  image work. Owed, not claimed.
- **`sevyn.recovery=1` boot handling**: sevynos image / bootloader
  work. Owed, not claimed.
- **Init supervision / socket-wait timeout / restart policy**:
  sevynos init work. The contract above is the interface; the
  implementation lives in the image.
- **Session restore** (relaunch-on-login): a Shell/Runtime
  follow-up; the compositor's part is stable session/device handling
  across a restart (no lost outputs, no stuck seats).
