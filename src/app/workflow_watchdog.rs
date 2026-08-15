//! The watchdog's IO adapter: sample the live run, classify, deliver, record.
//!
//! `phase4-retarget-plan.md` §3.4 and §5 packet **P9**. The decisions are pure
//! and already landed in [`crate::workflow::watchdog`] (packet P4) — the
//! four-way [`ProgressClass`](crate::workflow::watchdog::ProgressClass)
//! taxonomy, the escalation ladder, and the never-spend-an-undelivered-rung
//! rule. This module is the half that touches the world: it reads the live
//! run's panes and projected tasks, hands the pure layer an observation, and
//! turns the decision it gets back into a real message, a store write, a
//! journal entry and an event.
//!
//! It exists as a file today so that P9 can land in a module nobody else owns.
//! The seam is deliberately one call from the projection poll rather than a
//! timer of its own: the watchdog samples at `watchdog_tick_secs`, a multiple
//! of the 2 s projection cadence, over exactly the state that poll refreshed.
//!
//! Boundary note (`AGENTS.md`): everything here is a shared runtime fact —
//! `run_node.attention`, the `watchdog` journal kind, `workflow.node.watchdog`
//! — persisted through the store and served over the JSON API. Nothing here is
//! TUI-private.

#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
impl crate::app::App {
    /// One watchdog sample over the live lead run.
    ///
    /// A no-op stub until P9: it observes nothing, writes nothing, journals
    /// nothing, and emits nothing, so the ladder cannot half-exist. Returning
    /// `false` is the honest answer to "did anything change" for a poller that
    /// did not run.
    pub(crate) fn poll_run_watchdog(&mut self, now: std::time::Instant) -> bool {
        let _ = now;
        false
    }
}
