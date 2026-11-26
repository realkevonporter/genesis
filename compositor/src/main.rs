// SPDX-License-Identifier: GPL-3.0-or-later
//! `sevyn-compositor` — the Genesis compositor.
//!
//! Stage 2 (nested, the default): runs inside a parent Wayland
//! compositor (Weston during development) via Smithay's winit backend,
//! serving one output, `wl_shm`, `wl_output`, `wl_seat` (pointer +
//! keyboard), and `xdg-shell` to its clients.
//!
//! Stage 3 (standalone, `--standalone`): runs without a parent
//! compositor — DRM/KMS output at the panel's native mode, libinput
//! devices through the libseat session, GLES rendering with a Pixman
//! fallback. See `COMPOSITOR_PLAN.md` (Stage 3), `compositor/README.md`,
//! and `src/drm.rs`.
//!
//! Usage:
//!
//! ```text
//! sevyn-compositor [--socket <name>] [--size <WIDTHxHEIGHT>]
//!     [--runtime-pid <pid>]
//! sevyn-compositor --standalone [--socket <name>] [--scale N]
//!     [--rotate 0|90|180|270] [--no-gles] [--runtime-pid <pid>]
//! ```
//!
//! * `--socket` — the Wayland socket name in `$XDG_RUNTIME_DIR`
//!   (default `sevyn-compositor`). Clients connect with
//!   `WAYLAND_DISPLAY=<name>`.
//! * `--size` — nested mode only: the output size in logical pixels
//!   (default `1280x800`). Ignored in standalone mode, which uses the
//!   output's native mode.
//! * `--runtime-pid <pid>` — Stage 4: init passes the Runtime's pid at
//!   launch, and the bridge socket then admits ONLY that pid
//!   (`SO_PEERCRED` pid check). When unset, the Stage 2/3 same-uid
//!   auth stands (any process of the platform user may talk to the
//!   bridge — the development posture, not the shipping one).
//! * `--standalone` — run the DRM/KMS backend instead of the nested
//!   winit backend. Needs a DRM device and a libseat session (a VT
//!   session under logind/seatd); without one it fails cleanly with a
//!   named error, exit 1.
//! * `--scale N` — standalone only: integer output scale (default 1).
//! * `--rotate DEG` — standalone only: static output rotation,
//!   0/90/180/270 (default 0).
//! * `--no-gles` — standalone only: skip the GLES renderer and use the
//!   Pixman software renderer. `SEVYN_NO_GLES=1` does the same.
//!
//! Milestone markers go to stdout (`SEVYN ...` lines); diagnostics go to
//! stderr. Both are also appended to `/tmp/genesis.log` (truncated on
//! start; see `logging.rs` and `compositor/RECOVERY.md`). A panic emits
//! a `SEVYN CRASH ...` marker and unwinds, which drops the backend
//! state and releases DRM master. SIGTERM / SIGINT shuts down cleanly
//! (exit 0). Exit codes: 0 clean shutdown, 1 fatal startup/runtime
//! failure (including DRM output lost), 2 usage error, 101 panic.

mod adoption;
mod backend;
mod capture;
mod cursor;
mod drm;
mod foreign_toplevel;
mod idle;
mod input;
mod logging;
mod output;
mod render;
mod runtime_bridge;
mod server;
mod surface_registry;
mod xdg_shell;

use std::process::ExitCode;

use smithay::utils::{Logical, Size};

use drm::StandaloneConfig;

fn usage() -> &'static str {
    "usage: sevyn-compositor [--socket <name>] [--size <WIDTHxHEIGHT>] [--runtime-pid <pid>]\n       sevyn-compositor --standalone [--socket <name>] [--scale N] [--rotate 0|90|180|270] [--no-gles] [--runtime-pid <pid>]"
}

fn parse_size(text: &str) -> Result<Size<i32, Logical>, String> {
    let (width, height) = text
        .split_once('x')
        .ok_or_else(|| format!("bad --size {text:?}, expected WIDTHxHEIGHT"))?;
    let width: i32 = width
        .parse()
        .map_err(|_| format!("bad --size width in {text:?}"))?;
    let height: i32 = height
        .parse()
        .map_err(|_| format!("bad --size height in {text:?}"))?;
    if width <= 0 || height <= 0 {
        return Err(format!("bad --size {text:?}, dimensions must be positive"));
    }
    Ok(Size::from((width, height)))
}

fn main() -> ExitCode {
    logging::init_logging();
    logging::install_panic_hook();
    let mut socket_name = "sevyn-compositor".to_string();
    let mut output_size = Size::from((1280, 800));
    let mut standalone = false;
    let mut scale = 1;
    let mut rotate = smithay::utils::Transform::Normal;
    let mut no_gles = false;
    let mut size_given = false;
    let mut runtime_pid: Option<u32> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{}", usage());
                return ExitCode::SUCCESS;
            }
            "--standalone" => {
                standalone = true;
            }
            "--no-gles" => {
                no_gles = true;
            }
            "--runtime-pid" => {
                let text = args.next().unwrap_or_else(|| {
                    diag!("--runtime-pid needs a value");
                    std::process::exit(2);
                });
                match text.parse::<u32>() {
                    Ok(pid) if pid > 0 => runtime_pid = Some(pid),
                    _ => {
                        diag!("bad --runtime-pid {text:?}, expected a positive pid");
                        return ExitCode::from(2);
                    }
                }
            }
            "--socket" => {
                socket_name = args.next().unwrap_or_else(|| {
                    diag!("--socket needs a value");
                    std::process::exit(2);
                });
            }
            "--size" => {
                let text = args.next().unwrap_or_else(|| {
                    diag!("--size needs a value");
                    std::process::exit(2);
                });
                match parse_size(&text) {
                    Ok(size) => {
                        output_size = size;
                        size_given = true;
                    }
                    Err(err) => {
                        diag!("{err}");
                        return ExitCode::from(2);
                    }
                }
            }
            "--scale" => {
                let text = args.next().unwrap_or_else(|| {
                    diag!("--scale needs a value");
                    std::process::exit(2);
                });
                match drm::parse_scale(&text) {
                    Ok(parsed) => scale = parsed,
                    Err(err) => {
                        diag!("{err}");
                        return ExitCode::from(2);
                    }
                }
            }
            "--rotate" => {
                let text = args.next().unwrap_or_else(|| {
                    diag!("--rotate needs a value");
                    std::process::exit(2);
                });
                match drm::parse_rotate(&text) {
                    Ok(parsed) => rotate = parsed,
                    Err(err) => {
                        diag!("{err}");
                        return ExitCode::from(2);
                    }
                }
            }
            other => {
                diag!("unknown argument {other:?}\n{}", usage());
                return ExitCode::from(2);
            }
        }
    }

    if standalone {
        if size_given {
            diag!("ignoring --size in standalone mode (the output's native mode is used)");
        }
        let config = StandaloneConfig {
            socket_name,
            scale,
            rotate,
            no_gles,
            runtime_pid,
        };
        match drm::run_standalone(config) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                diag!("fatal: {err}");
                ExitCode::FAILURE
            }
        }
    } else {
        match backend::run(&socket_name, output_size, runtime_pid) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                diag!("fatal: {err}");
                ExitCode::FAILURE
            }
        }
    }
}
