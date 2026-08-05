//! No-progress streaks, materiality, and the escalation ladder.
//!
//! Phase 4 (`docs/design/workflow-builder/04-kvdag-and-execution.md` §6). The
//! types land now because Phase 1 already plumbs the progress evidence the
//! classifier reads.

/// Every watchdog tick classifies a node into exactly one of these. Only
/// `LocalLoop` is a bug; the other three are normal and must not escalate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressClass {
    /// Material progress observed; reset the streak.
    LegitimateIteration,
    /// A declared blocker with a resume condition, or a monitor reporting
    /// "checked, unchanged"; no streak increment, back off.
    ExternalWait,
    /// Progress, but none of it toward the output schema.
    GoalDrift,
    /// No material progress for `stuck_threshold` consecutive ticks.
    LocalLoop,
}

/// The escalation ladder, in order. Each step is journalled and increments the
/// node's intervention count, which is measured evidence for the review cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Escalation {
    /// A short steering prompt asking for state and the next concrete step.
    Nudge,
    /// Re-send the task plus the exact unfilled schema fields.
    StructuredReprompt,
    /// Write a partial checkpoint, close the pane, respawn at `attempt + 1`.
    Restart,
    /// Surface a blocker; the run continues on other branches and terminal
    /// readiness refuses to report success.
    Blocked,
}
