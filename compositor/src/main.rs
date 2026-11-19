// SPDX-License-Identifier: GPL-3.0-or-later
//! sevyn-compositor — the Genesis compositor.
//!
//! The compositor is Rust end to end on Smithay 0.7 (see COMPOSITOR_PLAN.md,
//! "Language: Rust"). This stub exists so the cargo build and the smithay
//! dependency resolution are provable before any server code lands. It does
//! not start a server: Stage 2 (the minimal nested Smithay compositor) is
//! not yet implemented.

fn main() {
    eprintln!(
        "sevyn-compositor: Stage 2 (minimal nested Smithay compositor) is not yet implemented — see COMPOSITOR_PLAN.md"
    );
    std::process::exit(1);
}
