// SPDX-License-Identifier: GPL-3.0-or-later
//! Milestone markers: the compositor's machine-readable progress log.
//!
//! Every line goes to stdout, starts with `SEVYN `, and is flushed
//! immediately so the nested check (`compositor/tests/nested-check.sh`)
//! can assert on it while the compositor is still running. Markers are the
//! compositor's proof surface: output created, client connected, surface
//! accepted, input routed, session stopped, clean shutdown. Diagnostics
//! that are not markers go to stderr via [`diag`].

use std::io::Write;

/// Emit one `SEVYN <marker>` line on stdout, flushed immediately.
pub fn marker(args: std::fmt::Arguments<'_>) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = writeln!(lock, "SEVYN {args}");
    let _ = lock.flush();
}

/// Emit a diagnostic line on stderr (not a marker; never asserted on).
pub fn diag(args: std::fmt::Arguments<'_>) {
    eprintln!("sevyn-compositor: {args}");
}

/// Convenience wrapper so call sites read `marker!("READY ...")`.
#[macro_export]
macro_rules! marker {
    ($($arg:tt)*) => {
        $crate::logging::marker(format_args!($($arg)*))
    };
}

/// Convenience wrapper so call sites read `diag!("...")`.
#[macro_export]
macro_rules! diag {
    ($($arg:tt)*) => {
        $crate::logging::diag(format_args!($($arg)*))
    };
}
