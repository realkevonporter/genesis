// SPDX-License-Identifier: GPL-3.0-or-later
//! Milestone markers + structured diagnostics: the compositor's
//! machine-readable progress log and its crash record.
//!
//! Every marker line goes to stdout, starts with `SEVYN `, and is
//! flushed immediately so the nested check
//! (`compositor/tests/nested-check.sh`) can assert on it while the
//! compositor is still running. Markers are the compositor's proof
//! surface: output created, client connected, surface accepted, input
//! routed, session stopped, clean shutdown. Diagnostics that are not
//! markers go to stderr via [`diag`].
//!
//! Stage 3 adds the file sink (plan item 10, "major crashes produce
//! useful logs"): [`init_logging`] opens [`LOG_PATH`] and every marker
//! and diagnostic is appended there as well as to stdout/stderr, so a
//! crash leaves a useful log even after the terminal is gone. The
//! recovery contract for that file lives in `compositor/RECOVERY.md`.
//!
//! ## Rotation (chosen: truncate on start)
//!
//! The log file is truncated when the compositor starts. One run's log
//! is bounded by what one run emits (markers are sparse — a line per
//! event, not per frame), and the supervisor owns history: if it needs
//! a previous run's log, it copies `/tmp/genesis.log` off before
//! restarting the compositor. There is no in-run size cap and no
//! rotation of old runs; that is the documented trade, not an
//! oversight — see `RECOVERY.md`.
//!
//! If the file cannot be opened (read-only `/tmp`, disk full), logging
//! to the file is skipped and the compositor keeps running with
//! stdout/stderr only; that failure is itself reported on stderr once.
//! Logging never fails startup.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    sync::{Mutex, OnceLock},
};

/// The log file's path (plan: `/tmp/genesis.log`, preserved for
/// recovery). The recovery console reads this file; init must not
/// delete it before the operator has had a chance to read it.
pub const LOG_PATH: &str = "/tmp/genesis.log";

/// The open log file, if any. `None` means the file could not be
/// opened (see the module docs) — stdout/stderr still work.
static LOG_FILE: OnceLock<Mutex<Option<File>>> = OnceLock::new();

/// Open (truncating) the log file and write the start banner. Call
/// once, at the top of `main`, before anything else that logs.
/// Safe to call more than once: later calls are no-ops.
pub fn init_logging() {
    let slot = LOG_FILE.get_or_init(|| Mutex::new(None));
    let mut guard = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_some() {
        return;
    }
    match OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(LOG_PATH)
    {
        Ok(mut file) => {
            let started = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            let _ = writeln!(
                file,
                "--- genesis log started (unix={started}) — truncated on start; see compositor/RECOVERY.md ---"
            );
            let _ = file.flush();
            *guard = Some(file);
        }
        Err(err) => {
            eprintln!(
                "sevyn-compositor: cannot open {LOG_PATH}: {err}; logging to stdout/stderr only"
            );
        }
    }
}

/// Append one line (without a trailing newline) to the log file, if
/// the file is open. Never panics: a poisoned lock is recovered, a
/// failed write is dropped silently (stdout/stderr already carry the
/// line).
fn write_to_file(line: &str) {
    let Some(slot) = LOG_FILE.get() else {
        return;
    };
    let Ok(mut guard) = slot.lock() else {
        return;
    };
    if let Some(file) = guard.as_mut() {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
}

/// Emit one `SEVYN <marker>` line on stdout (flushed immediately)
/// and append the same line to the log file.
pub fn marker(args: std::fmt::Arguments<'_>) {
    let line = format!("SEVYN {args}");
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = writeln!(lock, "{line}");
    let _ = lock.flush();
    drop(lock);
    write_to_file(&line);
}

/// Emit a diagnostic line on stderr (not a marker; never asserted on)
/// and append it to the log file.
pub fn diag(args: std::fmt::Arguments<'_>) {
    let line = format!("sevyn-compositor: {args}");
    eprintln!("{line}");
    write_to_file(&line);
}

/// Install the panic hook. Call once, at the top of `main`, after
/// [`init_logging`].
///
/// On a panic the hook:
/// * writes the panic message + location to stderr (the default
///   hook's job — this replaces it, so nothing is lost);
/// * emits a `SEVYN CRASH ...` marker on stdout and into the log
///   file, so the crash is greppable in both places;
/// * does NOT release DRM master itself — it cannot: the DRM device
///   is owned by the backend loop. The release happens because the
///   panic unwinds (`panic = "unwind"` is the default): the unwind
///   drops the backend state, which closes the DRM fd, which releases
///   DRM master (see `drm.rs`, "DRM master release"). Even `SIGKILL`
///   releases master the same way, via the kernel closing the fd.
///   The hook only makes sure the crash leaves a useful log line
///   (plan item 10).
///
/// The hook never panics itself: every write is best-effort.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("(unknown panic payload)");
        let location = info
            .location()
            .map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_else(|| "(unknown location)".to_string());
        eprintln!("sevyn-compositor: PANIC: {payload} at {location}");
        marker(format_args!(
            "CRASH panic={payload} at {location} (unwinding; DRM master released by fd close on drop)"
        ));
    }));
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
