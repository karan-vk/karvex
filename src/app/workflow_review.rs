//! The review cycle's IO adapter: plan, interview, collect, synthesise.
//!
//! `phase4-retarget-plan.md` §3.5 and §5 packet **P10**. The decisions are pure
//! and already landed in [`crate::workflow::review`] and
//! [`crate::workflow::review_prompt`] (packet P5): who is worth interviewing,
//! in which mode, what each interview is asked, and how an answer is
//! attributed. This module is the half that spawns the panes, seeds them,
//! watches them, and writes the `review_cycle` / `review_finding` rows the
//! store gained a writer for in P7.
//!
//! It exists as a file today so that P10 can land in a module nobody else owns.
//!
//! ## Why this poller is not gated on a live run
//!
//! A review interviews a run that has already ended — that is the whole point
//! of it — so by the time a cycle exists there is no `workflow_lead` left to
//! hang a poll off. [`crate::app::App::poll_run_projection`] therefore calls
//! this outside the live-run gate, and the event loop folds
//! [`crate::app::App::review_cycle_deadline`] into its own deadline so a server
//! with no live run still wakes for an interview that is running.
//!
//! Boundary note (`AGENTS.md`): a review cycle is a shared runtime fact, stored
//! and served over the JSON API. The overlay that displays one is the client's
//! business and does not belong here.

#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
impl crate::app::App {
    /// One tick over every review cycle this server is running.
    ///
    /// A no-op stub until P10, for the same reason
    /// [`crate::app::App::poll_run_watchdog`] is.
    pub(crate) fn poll_review_cycles(&mut self, now: std::time::Instant) -> bool {
        let _ = now;
        false
    }

    /// When the review poller next needs the loop to wake it.
    ///
    /// `None` means "nothing in flight", which is the permanent answer until
    /// P10 gives it cycles to run. Folded into the loop's min-of-all-deadlines
    /// like every other periodic task, and deliberately *not* gated on a
    /// connected client: a review is server-owned and runs headless.
    pub(crate) fn review_cycle_deadline(&self) -> Option<std::time::Instant> {
        None
    }
}
