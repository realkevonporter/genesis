// SPDX-License-Identifier: GPL-3.0-or-later
//! `sevyn-compositor` — the Genesis compositor (Stage 2: minimal nested).
//!
//! Runs nested inside a parent Wayland compositor (Weston during
//! development) via Smithay's winit backend, serving one output, `wl_shm`,
//! `wl_output`, `wl_seat` (pointer + keyboard), and `xdg-shell` to its
//! clients. See `COMPOSITOR_PLAN.md` (Stage 2) and `compositor/README.md`.
//!
//! Usage:
//!
//! ```text
//! sevyn-compositor [--socket <name>] [--size <WIDTHxHEIGHT>]
//! ```
//!
//! * `--socket` — the Wayland socket name in `$XDG_RUNTIME_DIR`
//!   (default `sevyn-compositor`). Clients connect with
//!   `WAYLAND_DISPLAY=<name>`.
//! * `--size` — the output size in logical pixels (default `1280x800`).
//!
//! Milestone markers go to stdout (`SEVYN ...` lines); diagnostics go to
//! stderr. SIGTERM / SIGINT shuts down cleanly (exit 0).

mod backend;
mod input;
mod logging;
mod output;
mod runtime_bridge;
mod server;
mod surface_registry;
mod xdg_shell;

use std::process::ExitCode;

use smithay::utils::{Logical, Size};

fn usage() -> &'static str {
    "usage: sevyn-compositor [--socket <name>] [--size <WIDTHxHEIGHT>]"
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
    let mut socket_name = "sevyn-compositor".to_string();
    let mut output_size = Size::from((1280, 800));

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{}", usage());
                return ExitCode::SUCCESS;
            }
            "--socket" => {
                socket_name = args.next().unwrap_or_else(|| {
                    eprintln!("sevyn-compositor: --socket needs a value");
                    std::process::exit(2);
                });
            }
            "--size" => {
                let text = args.next().unwrap_or_else(|| {
                    eprintln!("sevyn-compositor: --size needs a value");
                    std::process::exit(2);
                });
                match parse_size(&text) {
                    Ok(size) => output_size = size,
                    Err(err) => {
                        eprintln!("sevyn-compositor: {err}");
                        return ExitCode::from(2);
                    }
                }
            }
            other => {
                eprintln!("sevyn-compositor: unknown argument {other:?}\n{}", usage());
                return ExitCode::from(2);
            }
        }
    }

    match backend::run(&socket_name, output_size) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("sevyn-compositor: fatal: {err}");
            ExitCode::FAILURE
        }
    }
}
