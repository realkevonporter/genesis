// SPDX-License-Identifier: GPL-3.0-or-later
//! Idle policy + DPMS (Stage 3).
//!
//! Two halves, deliberately separate:
//!
//! * **Client idle notifications** (`ext-idle-notify-v1`) are
//!   Smithay's [`IdleNotifierState`]: a client asks for a notification
//!   with its own timeout and gets `idled` / `resumed` events. The
//!   compositor only has to feed it activity — every input event calls
//!   [`GenesisState::note_input_activity`], which forwards to
//!   [`IdleNotifierState::notify_activity`]. The notifier's timers run
//!   on a dedicated calloop loop owned by the state (see
//!   [`GenesisState::pump_idle_timers`]): Smithay's notifier needs a
//!   `LoopHandle<'static, GenesisState>`, and the backends' loops are
//!   `EventLoop<StandaloneLoop>` (standalone) or no calloop loop at
//!   all (nested), so neither can supply one. The dedicated loop holds
//!   only the notifier's timers and is pumped with a zero timeout on
//!   every backend iteration — no extra thread, no extra wakeups
//!   beyond the timers themselves.
//! * **DPMS** is the compositor's own policy, not the clients':
//!   after [`IDLE_DPMS_TIMEOUT`] of no input, and with no
//!   idle-inhibitor held, the display powers off; any input wakes it.
//!   A client's notification timeout does not drive DPMS — a video
//!   player asking for a 10 s idle notification does not power the
//!   panel off at 10 s. The fixed timeout is the policy; a Settings
//!   timeout UI is a follow-up.
//!
//! [`GenesisState::poll_idle`] is the backend-facing half: the
//! backends call it once per iteration and apply the returned
//! `Some(false)` (power off) / `Some(true)` (power on) through
//! `DrmOutput::set_dpms` (standalone) or log it as a no-op (nested).
//! The state only ever *requests* a transition, once per transition;
//! the backend owns the hardware call and its errors.
//!
//! Idle-inhibit (`zwp_idle_inhibit_manager_v1`) holds the display on:
//! [`IdleInhibitHandler::inhibit`] records the inhibiting surface and
//! wakes the display if it is already off; `uninhibit` drops it. The
//! inhibitor list is pruned of dead surfaces on every poll, so a
//! client that disconnects without destroying its inhibitor cannot
//! hold the display on forever — Smithay only calls `uninhibit` on
//! an explicit `Destroy` request, so the prune is the safety net.

use std::time::{Duration, Instant};

use smithay::reexports::wayland_server::Resource;
use smithay::{reexports::wayland_server::protocol::wl_surface::WlSurface, utils::IsAlive};

use crate::server::GenesisState;

/// No input for this long, with nothing inhibiting idle, powers the
/// display off (standalone) — the `swayidle`-style default, 5 minutes.
pub const IDLE_DPMS_TIMEOUT: Duration = Duration::from_secs(300);

impl GenesisState {
    /// Record input activity: reset the DPMS idle clock, tell the
    /// idle-notifier (so clients get `resumed` and their timers
    /// restart), and — if the display was powered off for idle —
    /// request a wake. Called from the input choke points in
    /// `input.rs` (pointer motion/button/axis, keys, touch), so both
    /// backends feed it through the same path.
    pub fn note_input_activity(&mut self) {
        self.last_activity = Instant::now();
        let seat = self.seat.clone();
        self.idle_notifier_state.notify_activity(&seat);
        if self.dpms_off {
            self.dpms_off = false;
            self.wants_dpms = Some(true);
        }
    }

    /// Pump the idle-notifier's timers. Call once per backend
    /// iteration (both backends); a zero-timeout dispatch, so it
    /// costs nothing when no timer is due.
    pub fn pump_idle_timers(&mut self) {
        let Some(mut idle_loop) = self.idle_loop.take() else {
            return;
        };
        let _ = idle_loop.dispatch(Some(Duration::ZERO), self);
        self.idle_loop = Some(idle_loop);
    }

    /// The next DPMS transition the backend should apply, if any:
    /// `Some(false)` powers the display off (idle), `Some(true)`
    /// powers it back on (input or a new inhibitor while off).
    /// Returns `None` when nothing changed. Each transition is
    /// reported exactly once; the backend applies it and owns errors.
    pub fn poll_idle(&mut self) -> Option<bool> {
        if let Some(on) = self.wants_dpms.take() {
            return Some(on);
        }
        self.prune_inhibitors();
        if !self.dpms_off
            && self.idle_inhibitors.is_empty()
            && self.last_activity.elapsed() >= IDLE_DPMS_TIMEOUT
        {
            self.dpms_off = true;
            return Some(false);
        }
        None
    }

    /// Record an idle-inhibitor surface (from
    /// `IdleInhibitHandler::inhibit`). Inhibiting wakes the display
    /// if it is already off, and holds it on while held.
    pub fn add_inhibitor(&mut self, surface: WlSurface) {
        if !self
            .idle_inhibitors
            .iter()
            .any(|inhibitor| inhibitor.id() == surface.id())
        {
            self.idle_inhibitors.push(surface);
        }
        self.idle_notifier_state.set_is_inhibited(true);
        if self.dpms_off {
            self.dpms_off = false;
            self.wants_dpms = Some(true);
        }
    }

    /// Drop an idle-inhibitor surface (from
    /// `IdleInhibitHandler::uninhibit`).
    pub fn remove_inhibitor(&mut self, surface: &WlSurface) {
        self.idle_inhibitors
            .retain(|inhibitor| inhibitor.id() != surface.id());
        self.prune_inhibitors();
    }

    /// Drop inhibitors whose surfaces are dead (client gone without
    /// destroying the inhibitor) and sync the notifier's inhibited
    /// flag with what is actually held. Called on every poll.
    fn prune_inhibitors(&mut self) {
        self.idle_inhibitors.retain(|surface| surface.alive());
        self.idle_notifier_state
            .set_is_inhibited(!self.idle_inhibitors.is_empty());
    }
}
