//! Pure self-improvement review core: who gets interviewed, in what mode, how
//! an interview's progress folds into a cycle, and how a synthesised finding
//! becomes a store seed.
//!
//! `phase4-retarget-plan.md` §3.5 and `00-overview.md` Feature 4. After a run
//! reaches a terminal status karvex revives each interesting teammate with
//! `claude --resume <session id> --fork-session`, puts its own *measured*
//! record to it, takes the answer back, synthesises findings, and — on
//! per-finding acceptance — compiles a new immutable definition version. This
//! module owns every decision in that sentence; `review_prompt` owns the two
//! documents, `binding::review` owns the argv, and the adapter owns the IO.
//!
//! Pure by construction, in the sense `workflow::model`, `workflow::tier`,
//! `workflow::lead_prompt`, and `workflow::projection` are: measured values
//! and prior state in, decisions out. No filesystem (the caller stats the
//! transcript and hands the boolean in), no store, no panes, no clock (every
//! entry point that needs one takes `now_unix_ms`).
//!
//! The rule the whole cycle exists to protect is attribution. A finding that
//! did not come out of a live interview is `evidence_only` and is never
//! presented as the teammate's own words — enforced here in
//! [`Attribution::resolve`] rather than trusted to a prompt, because a prompt
//! is a request and this is an invariant.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::workflow::model::{
    Attention, InstancePath, InterrogationId, InterviewMode, NodeKey, NodeStatus,
    ReviewFindingSeed, RunId, RunStatus,
};

// ── policy ─────────────────────────────────────────────────────────────────

/// How long an interview pane may go without producing a parsed answer before
/// the cycle stops waiting for it and degrades that member to evidence-only.
///
/// `review_interview_timeout` in §3.5's failure ladder. Not a config key: the
/// three keys §3.7 actually landed are `watchdog_enabled`,
/// `watchdog_tick_secs`, and `review_max_interviews`, so this is a constant
/// until a user asks for it to be one.
pub const DEFAULT_INTERVIEW_TIMEOUT_MS: u64 = 15 * 60 * 1000;

/// How long an interview pane may sit `blocked` before it is treated as
/// unanswerable.
///
/// `blocked` is a first-class interview outcome, not "slow" (spike S2, fact 2:
/// with the wrong permission arrangement the fork stalls on an approval dialog
/// forever and karvex reports `agent_status: blocked`). P6's argv carries
/// `--permission-mode acceptEdits` plus an allowed `Bash(kvx …:*)` so this
/// should not fire, but a human-gated pane is exactly the failure a review
/// cycle must survive rather than hang on. The grace exists because a human
/// *can* answer the dialog.
pub const DEFAULT_BLOCKED_GRACE_MS: u64 = 60 * 1000;

/// How many times the synthesis document may be attempted before the cycle
/// fails (§3.5: "a synthesis that dies twice fails the cycle").
pub const SYNTHESIS_MAX_ATTEMPTS: u32 = 2;

/// The tunable half of the cycle: everything the pure core needs that a user
/// can influence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewPolicy {
    /// `[workflow] review_max_interviews` (default 6). Zero means no member is
    /// interviewed at all, which the caller must surface as
    /// `workflow_review_no_interviewable_members` rather than as an empty
    /// success.
    pub max_interviews: usize,
    pub interview_timeout_ms: u64,
    pub blocked_grace_ms: u64,
}

impl Default for ReviewPolicy {
    fn default() -> Self {
        Self {
            max_interviews: 6,
            interview_timeout_ms: DEFAULT_INTERVIEW_TIMEOUT_MS,
            blocked_grace_ms: DEFAULT_BLOCKED_GRACE_MS,
        }
    }
}

impl ReviewPolicy {
    /// The one config-derived constructor, so the adapter never has to know
    /// which of the three fields is a config key and which is a constant.
    pub fn from_config(max_interviews: usize) -> Self {
        Self {
            max_interviews,
            ..Self::default()
        }
    }
}

// ── measured input ─────────────────────────────────────────────────────────

/// One owner change on a task: the measurable half of "what the lead did about
/// it".
///
/// Karvex cannot see the lead's reasoning, only that a task changed hands, so
/// that is all this records and all the interview prompt claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerChange {
    pub at_unix_ms: u64,
    /// Empty in the source data means *unclaimed*, which is a real state, so
    /// it is `None` here rather than an empty string.
    pub from: Option<String>,
    pub to: Option<String>,
}

/// One watchdog intervention as it was actually delivered (or not).
///
/// `delivered: false` is load-bearing, not cosmetic: after a server restart the
/// in-memory messaging endpoint is gone and a rung can be composed but never
/// reach anyone (S1). Telling a teammate "we nudged you" when nothing arrived
/// would be the exact dishonesty this cycle exists to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterventionEvidence {
    pub at_unix_ms: u64,
    /// 1..=4 on the ladder `workflow::watchdog` owns.
    pub rung: u8,
    /// The classification that produced it, free text mirroring whatever the
    /// watchdog journalled.
    pub kind: String,
    /// Which channel carried it (`message`, `prompt`, …). `None` when it was
    /// never delivered.
    pub channel: Option<String>,
    pub delivered: bool,
    /// The text karvex actually sent, so the interview can quote it verbatim
    /// instead of paraphrasing karvex's own message back at the teammate.
    pub text: Option<String>,
}

/// What karvex measured about one task of the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskEvidence {
    pub path: InstancePath,
    /// The definition node this task matched, or `None` for an emergent task
    /// the lead invented.
    pub node_key: Option<NodeKey>,
    pub subject: String,
    pub status: NodeStatus,
    pub emergent: bool,
    /// The watchdog's last opinion, never the projected status (D-10).
    pub attention: Option<Attention>,
    pub owner: Option<String>,
    pub owner_changes: Vec<OwnerChange>,
    /// Subjects of the tasks this one was still waiting on when the run ended.
    pub unresolved_blockers: Vec<String>,
    pub first_seen_at_unix_ms: u64,
    pub last_change_at_unix_ms: u64,
    /// Wall time the task spent `in_progress`.
    pub in_progress_ms: u64,
    /// Of that, how long the owning pane was idle — the number §3.5 wants put
    /// to the teammate, because it is the one karvex can defend.
    pub idle_while_in_progress_ms: u64,
    pub interventions: Vec<InterventionEvidence>,
}

impl TaskEvidence {
    pub fn completed(&self) -> bool {
        matches!(self.status, NodeStatus::Succeeded)
    }

    pub fn highest_rung(&self) -> u8 {
        self.interventions
            .iter()
            .map(|i| i.rung)
            .max()
            .unwrap_or_default()
    }
}

/// Who a member was and what karvex can still reach of it after the run.
///
/// `transcript_readable` is a boolean rather than a path check on purpose:
/// stat'ing the file is IO, so the adapter does it and this layer only decides
/// what it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberIdentity {
    pub name: String,
    pub is_lead: bool,
    pub model: String,
    pub backend_type: String,
    /// The member's own working directory, passed to the interview pane as
    /// `--add-dir` (S2 amendment 3: the pane's *cwd* is the cycle dir).
    pub cwd: Option<String>,
    pub session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub transcript_readable: bool,
}

/// What karvex measured about one member.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemberEvidence {
    pub name: String,
    /// Every task this member owned at any point, in the order the run saw
    /// them.
    pub tasks: Vec<TaskEvidence>,
    /// The member's last observed pane state, free text mirroring detection.
    pub last_state: Option<String>,
    pub last_state_at_unix_ms: Option<u64>,
}

impl MemberEvidence {
    pub fn interventions(&self) -> usize {
        self.tasks.iter().map(|task| task.interventions.len()).sum()
    }

    pub fn highest_rung(&self) -> u8 {
        self.tasks
            .iter()
            .map(TaskEvidence::highest_rung)
            .max()
            .unwrap_or_default()
    }

    pub fn idle_while_in_progress_ms(&self) -> u64 {
        self.tasks
            .iter()
            .map(|task| task.idle_while_in_progress_ms)
            .sum()
    }

    /// Reassignments that involved this member, in either direction. A task
    /// taken *away* from a member is as interesting as one dumped on it.
    pub fn owner_changes(&self) -> usize {
        self.tasks
            .iter()
            .flat_map(|task| task.owner_changes.iter())
            .filter(|change| {
                change.from.as_deref() == Some(self.name.as_str())
                    || change.to.as_deref() == Some(self.name.as_str())
            })
            .count()
    }

    pub fn emergent_tasks(&self) -> usize {
        self.tasks.iter().filter(|task| task.emergent).count()
    }

    pub fn unfinished_tasks(&self) -> usize {
        self.tasks.iter().filter(|task| !task.completed()).count()
    }
}

/// The whole measured record of one finished run, as the review cycle sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEvidence {
    pub run_id: RunId,
    pub workflow_name: String,
    /// The definition version the run executed; the parent of any version the
    /// cycle mints.
    pub kvdag_version: u32,
    pub status: RunStatus,
    pub started_at_unix_ms: u64,
    pub ended_at_unix_ms: Option<u64>,
    /// The lead's own end-of-run summary, when it wrote one.
    pub summary: Option<String>,
    /// The recorded failure reason, when the run has one.
    pub failure: Option<String>,
    pub members: Vec<MemberEvidence>,
    /// Tasks no member ever claimed. They belong to nobody's interview but are
    /// part of the synthesis document's picture of the run.
    pub unowned_tasks: Vec<TaskEvidence>,
}

impl RunEvidence {
    /// Whether the run ended the way it was supposed to. Deliberately narrow:
    /// a `succeeded` run that still carries a failure reason did not finish
    /// cleanly, and neither did one with an unfinished task.
    pub fn finished_cleanly(&self) -> bool {
        matches!(self.status, RunStatus::Succeeded)
            && self.failure.is_none()
            && self
                .members
                .iter()
                .all(|member| member.unfinished_tasks() == 0)
            && self.unowned_tasks.iter().all(TaskEvidence::completed)
    }

    pub fn member(&self, name: &str) -> Option<&MemberEvidence> {
        self.members.iter().find(|member| member.name == name)
    }
}

// ── the plan ───────────────────────────────────────────────────────────────

/// Why an interview is `evidence_only` rather than the teammate's own account.
///
/// Carried all the way into the prompt and the synthesis document, because
/// "we could not reach this teammate" and "we did not try" are different
/// admissions and a reader is entitled to know which one it is reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EvidenceOnlyReason {
    /// No claude session id was ever captured for this member.
    NoSessionId,
    /// A session id exists but its transcript is gone — claude owns that file
    /// and run history outlives claude's retention (`00` Feature 3).
    TranscriptUnreadable,
    /// The interview pane sat waiting for a human approval it never got.
    InterviewBlocked,
    /// The interview pane never produced a parsable answer in time.
    InterviewTimedOut,
    /// The interview pane died before answering.
    InterviewPaneGone,
}

impl EvidenceOnlyReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoSessionId => "no_session_id",
            Self::TranscriptUnreadable => "transcript_unreadable",
            Self::InterviewBlocked => "interview_blocked",
            Self::InterviewTimedOut => "interview_timed_out",
            Self::InterviewPaneGone => "interview_pane_gone",
        }
    }

    /// One sentence a human (or the synthesiser) can read without a decoder
    /// ring. Never phrased as the teammate's fault.
    pub fn sentence(self) -> &'static str {
        match self {
            Self::NoSessionId => {
                "karvex never captured a claude session id for this member, so there was \
                 nothing to resume"
            }
            Self::TranscriptUnreadable => {
                "the member's claude transcript is no longer readable, so the session could \
                 not be resumed"
            }
            Self::InterviewBlocked => {
                "the interview pane stopped on a permission prompt and never got past it"
            }
            Self::InterviewTimedOut => {
                "the interview pane did not produce a usable answer before the interview \
                 deadline"
            }
            Self::InterviewPaneGone => "the interview pane exited before it answered",
        }
    }
}

/// Resumed vs evidence-only, decided from the two facts that settle it.
///
/// Both halves matter: an id with no transcript cannot be resumed, and a
/// readable transcript with no id cannot be addressed.
pub fn decide_interview_mode(
    session_id: Option<&str>,
    transcript_readable: bool,
) -> (InterviewMode, Option<EvidenceOnlyReason>) {
    match session_id.map(str::trim).filter(|id| !id.is_empty()) {
        None => (
            InterviewMode::EvidenceOnly,
            Some(EvidenceOnlyReason::NoSessionId),
        ),
        Some(_) if !transcript_readable => (
            InterviewMode::EvidenceOnly,
            Some(EvidenceOnlyReason::TranscriptUnreadable),
        ),
        Some(_) => (InterviewMode::Resumed, None),
    }
}

/// The trouble ranking's arithmetic, kept as named weights so a change to the
/// ordering is a visible diff rather than a magic number moving.
///
/// Every input is something the tree already measures (§3.5): watchdog
/// interventions, the highest rung reached, idle-while-in-progress time, task
/// reassignments, emergent tasks owned, and whether the member's work finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TroubleScore {
    pub interventions: u32,
    pub highest_rung: u8,
    pub idle_minutes: u32,
    pub owner_changes: u32,
    pub emergent_tasks: u32,
    pub unfinished_tasks: u32,
    /// A member karvex has no task evidence for at all. Silence is a finding,
    /// not an absence of one.
    pub silent: bool,
}

impl TroubleScore {
    const W_INTERVENTION: u32 = 10;
    const W_RUNG: u32 = 15;
    const W_IDLE_MINUTE: u32 = 1;
    /// Idle time saturates: an hour idle and a day idle are the same finding.
    const MAX_IDLE_POINTS: u32 = 30;
    const W_OWNER_CHANGE: u32 = 8;
    const W_EMERGENT: u32 = 5;
    const W_UNFINISHED: u32 = 25;
    const W_SILENT: u32 = 12;

    pub fn measure(evidence: Option<&MemberEvidence>) -> Self {
        let Some(evidence) = evidence else {
            return Self {
                silent: true,
                ..Self::default()
            };
        };
        Self {
            interventions: evidence.interventions() as u32,
            highest_rung: evidence.highest_rung(),
            idle_minutes: (evidence.idle_while_in_progress_ms() / 60_000) as u32,
            owner_changes: evidence.owner_changes() as u32,
            emergent_tasks: evidence.emergent_tasks() as u32,
            unfinished_tasks: evidence.unfinished_tasks() as u32,
            silent: evidence.tasks.is_empty(),
        }
    }

    pub fn total(self) -> u32 {
        self.interventions.saturating_mul(Self::W_INTERVENTION)
            + u32::from(self.highest_rung).saturating_mul(Self::W_RUNG)
            + self
                .idle_minutes
                .saturating_mul(Self::W_IDLE_MINUTE)
                .min(Self::MAX_IDLE_POINTS)
            + self.owner_changes.saturating_mul(Self::W_OWNER_CHANGE)
            + self.emergent_tasks.saturating_mul(Self::W_EMERGENT)
            + self.unfinished_tasks.saturating_mul(Self::W_UNFINISHED)
            + if self.silent { Self::W_SILENT } else { 0 }
    }
}

/// One member karvex decided to interview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterviewAssignment {
    pub member: String,
    pub is_lead: bool,
    pub mode: InterviewMode,
    /// The reason the mode is evidence-only. `None` exactly when
    /// `mode == Resumed`.
    pub evidence_only_reason: Option<EvidenceOnlyReason>,
    /// The session `claude --resume … --fork-session` is pointed at. `Some`
    /// exactly when `mode == Resumed`.
    pub source_session_id: Option<String>,
    /// The member's own working directory, for `--add-dir`.
    pub member_cwd: Option<String>,
    pub trouble: TroubleScore,
}

impl InterviewAssignment {
    pub fn is_resumed(&self) -> bool {
        matches!(self.mode, InterviewMode::Resumed)
    }
}

/// Why a member of the roster is not being interviewed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Ranked below `review_max_interviews`. Not a judgement about the member,
    /// just the cap binding.
    OverCap,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedMember {
    pub member: String,
    pub reason: SkipReason,
    pub trouble: TroubleScore,
}

/// Which members get interviewed, in what order, in what mode.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewPlan {
    /// Ranked, most troubled first, already capped.
    pub assignments: Vec<InterviewAssignment>,
    pub skipped: Vec<SkippedMember>,
}

impl ReviewPlan {
    /// Ranks the roster and applies the cap.
    ///
    /// Ordering, in full: the lead sorts first when the run did not finish
    /// cleanly (§3.5 — a bad run is the lead's run), then by trouble score
    /// descending, then the lead ahead of a teammate on a tie, then by name so
    /// two identical runs plan identically. Determinism is not cosmetic here:
    /// the plan decides which panes get spawned and which findings can ever be
    /// attributed.
    ///
    /// Every member is a candidate. Nothing is filtered out for being boring —
    /// only the cap removes anyone, and it says so.
    pub fn build(
        members: &[MemberIdentity],
        evidence: &RunEvidence,
        policy: &ReviewPolicy,
    ) -> Self {
        let lead_first = !evidence.finished_cleanly();

        let mut ranked: Vec<(InterviewAssignment, bool)> = members
            .iter()
            .map(|identity| {
                let trouble = TroubleScore::measure(evidence.member(&identity.name));
                let (mode, reason) = decide_interview_mode(
                    identity.session_id.as_deref(),
                    identity.transcript_readable,
                );
                let source_session_id = match mode {
                    InterviewMode::Resumed => identity.session_id.clone(),
                    InterviewMode::EvidenceOnly => None,
                };
                let assignment = InterviewAssignment {
                    member: identity.name.clone(),
                    is_lead: identity.is_lead,
                    mode,
                    evidence_only_reason: reason,
                    source_session_id,
                    member_cwd: identity.cwd.clone(),
                    trouble,
                };
                let lead_priority = lead_first && identity.is_lead;
                (assignment, lead_priority)
            })
            .collect();

        ranked.sort_by(|left, right| {
            right
                .1
                .cmp(&left.1)
                .then_with(|| right.0.trouble.total().cmp(&left.0.trouble.total()))
                .then_with(|| right.0.is_lead.cmp(&left.0.is_lead))
                .then_with(|| left.0.member.cmp(&right.0.member))
        });

        let mut assignments = Vec::new();
        let mut skipped = Vec::new();
        for (assignment, _) in ranked {
            if assignments.len() < policy.max_interviews {
                assignments.push(assignment);
            } else {
                skipped.push(SkippedMember {
                    member: assignment.member,
                    reason: SkipReason::OverCap,
                    trouble: assignment.trouble,
                });
            }
        }

        Self {
            assignments,
            skipped,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.assignments.is_empty()
    }

    /// How many planned interviews are already degraded before a pane exists —
    /// the number `WorkflowReviewInfo.evidence_only_count` reports.
    pub fn evidence_only_count(&self) -> usize {
        self.assignments
            .iter()
            .filter(|assignment| !assignment.is_resumed())
            .count()
    }

    pub fn assignment(&self, member: &str) -> Option<&InterviewAssignment> {
        self.assignments
            .iter()
            .find(|assignment| assignment.member == member)
    }
}

// ── the cycle's live state ─────────────────────────────────────────────────

/// An interview pane's observed agent state, mirroring karvex's own detection
/// vocabulary.
///
/// Re-declared here rather than imported from the wire schema for the same
/// reason [`crate::workflow::projection::TaskStatus`] re-declares Claude Code's:
/// the pure layer must not depend on the API surface, and an unknown value has
/// to be survivable rather than a parse failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterviewPaneState {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl InterviewPaneState {
    pub fn parse(value: &str) -> Self {
        match value {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            _ => Self::Unknown,
        }
    }
}

/// Why an interview stopped without an answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterviewFailure {
    /// Stopped on a permission prompt for longer than the grace window.
    Blocked,
    TimedOut,
    PaneGone,
}

impl InterviewFailure {
    pub fn evidence_only_reason(self) -> EvidenceOnlyReason {
        match self {
            Self::Blocked => EvidenceOnlyReason::InterviewBlocked,
            Self::TimedOut => EvidenceOnlyReason::InterviewTimedOut,
            Self::PaneGone => EvidenceOnlyReason::InterviewPaneGone,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::TimedOut => "timed_out",
            Self::PaneGone => "pane_gone",
        }
    }
}

/// Where one interview is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterviewPhase {
    /// Planned; no pane observed yet.
    Pending,
    /// A pane is alive and has not answered.
    Running,
    /// A parsed answer was recorded. Terminal.
    Answered,
    /// Terminal and degraded: this member contributes evidence only.
    Failed(InterviewFailure),
}

impl InterviewPhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Answered | Self::Failed(_))
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Answered => "answered",
            Self::Failed(_) => "failed",
        }
    }
}

/// One interview's state inside a cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterviewState {
    pub member: String,
    /// The mode the plan chose. Unchanged by failure: what changes is the
    /// *attribution*, which [`Attribution`] derives from mode **and** phase.
    pub mode: InterviewMode,
    pub phase: InterviewPhase,
    pub started_at_unix_ms: Option<u64>,
    pub last_pane_state: Option<InterviewPaneState>,
    /// When the pane was first observed `blocked`, for the grace window.
    pub blocked_since_unix_ms: Option<u64>,
}

/// What one poll saw of one interview pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedInterview {
    pub member: String,
    /// When the pane was launched. The interview deadline runs from here, not
    /// from the cycle's start: a queued interview is not a late one.
    pub started_at_unix_ms: u64,
    pub pane_alive: bool,
    pub pane_state: Option<InterviewPaneState>,
    /// Whether a *parsed* answer has been recorded for this member. The
    /// adapter parses with [`crate::workflow::review_prompt::parse_interview_answer`];
    /// an unparsable file is not an answer.
    pub answer_recorded: bool,
}

/// One interview changing phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterviewTransition {
    pub member: String,
    pub from: InterviewPhase,
    pub to: InterviewPhase,
}

/// What one [`ReviewCycleState::absorb`] changed. Empty when nothing did —
/// the same contract [`crate::workflow::projection::ProjectionDelta`] has, and
/// for the same reason: the caller turns it into store writes and events on a
/// poll loop.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewDelta {
    pub interviews: Vec<InterviewTransition>,
    /// Set on the poll where the last outstanding interview reached a terminal
    /// phase, i.e. the poll that unblocks synthesis. Never set twice.
    pub interviews_settled: bool,
    /// Observations for members the cycle has no interview for. Reported
    /// rather than dropped, because a pane karvex does not recognise is a bug
    /// worth seeing.
    pub unknown_members: Vec<String>,
}

impl ReviewDelta {
    pub fn is_empty(&self) -> bool {
        self.interviews.is_empty() && !self.interviews_settled && self.unknown_members.is_empty()
    }
}

/// Whether a failed synthesis attempt may be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynthesisOutcome {
    Retry,
    FailCycle,
}

/// The cycle's live state: one interview per planned member, plus the
/// synthesis attempt counter the failure ladder needs.
///
/// Deliberately not a mirror of `review_cycle.status`: the store row's status
/// is the durable, user-visible lifecycle and the adapter owns writing it.
/// This is the in-memory fold that decides *when* those writes happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewCycleState {
    interviews: BTreeMap<String, InterviewState>,
    policy: ReviewPolicy,
    synthesis_attempts: u32,
    settled_reported: bool,
}

impl ReviewCycleState {
    /// Starts a cycle from a plan. Every assignment begins
    /// [`InterviewPhase::Pending`], including evidence-only ones: an
    /// evidence-only interview still runs in a pane and still answers, it just
    /// answers as a reader of the record rather than as the teammate.
    pub fn from_plan(plan: &ReviewPlan, policy: ReviewPolicy) -> Self {
        let interviews = plan
            .assignments
            .iter()
            .map(|assignment| {
                (
                    assignment.member.clone(),
                    InterviewState {
                        member: assignment.member.clone(),
                        mode: assignment.mode,
                        phase: InterviewPhase::Pending,
                        started_at_unix_ms: None,
                        last_pane_state: None,
                        blocked_since_unix_ms: None,
                    },
                )
            })
            .collect();
        Self {
            interviews,
            policy,
            synthesis_attempts: 0,
            settled_reported: false,
        }
    }

    pub fn interviews(&self) -> impl Iterator<Item = &InterviewState> {
        self.interviews.values()
    }

    pub fn interview(&self, member: &str) -> Option<&InterviewState> {
        self.interviews.get(member)
    }

    /// Whether every interview has reached a terminal phase. An empty cycle is
    /// settled, which the caller must have refused before starting.
    pub fn interviews_settled(&self) -> bool {
        self.interviews
            .values()
            .all(|state| state.phase.is_terminal())
    }

    /// Folds one poll's observations in, returning only what changed.
    ///
    /// The precedence is deliberate and is the whole failure ladder: an answer
    /// beats everything (an interview that answered and then exited answered),
    /// then a dead pane, then a blocked pane past its grace, then the
    /// deadline. A terminal interview never moves again — degrading a member
    /// twice would double-count it in the synthesis document.
    pub fn absorb(&mut self, observed: &[ObservedInterview], now_unix_ms: u64) -> ReviewDelta {
        let mut delta = ReviewDelta::default();
        let mut seen: BTreeSet<&str> = BTreeSet::new();

        for observation in observed {
            let Some(state) = self.interviews.get_mut(&observation.member) else {
                delta.unknown_members.push(observation.member.clone());
                continue;
            };
            seen.insert(observation.member.as_str());
            state.started_at_unix_ms = Some(observation.started_at_unix_ms);
            state.last_pane_state = observation.pane_state;
            match observation.pane_state {
                Some(InterviewPaneState::Blocked) => {
                    state.blocked_since_unix_ms.get_or_insert(now_unix_ms);
                }
                _ => state.blocked_since_unix_ms = None,
            }

            if state.phase.is_terminal() {
                continue;
            }

            let next = if observation.answer_recorded {
                InterviewPhase::Answered
            } else if !observation.pane_alive {
                InterviewPhase::Failed(InterviewFailure::PaneGone)
            } else if state.blocked_since_unix_ms.is_some_and(|since| {
                now_unix_ms.saturating_sub(since) >= self.policy.blocked_grace_ms
            }) {
                InterviewPhase::Failed(InterviewFailure::Blocked)
            } else if now_unix_ms.saturating_sub(observation.started_at_unix_ms)
                >= self.policy.interview_timeout_ms
            {
                InterviewPhase::Failed(InterviewFailure::TimedOut)
            } else {
                InterviewPhase::Running
            };

            if next != state.phase {
                delta.interviews.push(InterviewTransition {
                    member: state.member.clone(),
                    from: state.phase,
                    to: next,
                });
                state.phase = next;
            }
        }

        // Determinism for the caller's store writes, exactly as the projection
        // promises: transitions come out by member name, not by poll order.
        delta.interviews.sort_by(|a, b| a.member.cmp(&b.member));
        delta.unknown_members.sort();
        delta.unknown_members.dedup();

        if !self.settled_reported && self.interviews_settled() {
            self.settled_reported = true;
            delta.interviews_settled = true;
        }

        delta
    }

    /// Records a synthesis attempt that produced nothing usable. §3.5's ladder:
    /// the first failure is a retry, the second fails the cycle.
    pub fn record_synthesis_failure(&mut self) -> SynthesisOutcome {
        self.synthesis_attempts = self.synthesis_attempts.saturating_add(1);
        if self.synthesis_attempts >= SYNTHESIS_MAX_ATTEMPTS {
            SynthesisOutcome::FailCycle
        } else {
            SynthesisOutcome::Retry
        }
    }

    pub fn synthesis_attempts(&self) -> u32 {
        self.synthesis_attempts
    }

    /// The attribution table for this cycle, given whatever `interrogation`
    /// rows the adapter actually created.
    ///
    /// This is where the honesty rule is enforced rather than requested: a
    /// member is `Resumed` only if it was *planned* resumed, its interview
    /// actually **answered**, and there is an interrogation row to point at. A
    /// blocked, timed-out, or dead interview degrades to evidence-only here,
    /// with the reason attached, no matter what the plan hoped for.
    pub fn attribution(&self, interviews: &BTreeMap<String, InterrogationId>) -> Attribution {
        let members = self
            .interviews
            .values()
            .map(|state| {
                let attribution = match (state.mode, state.phase) {
                    (InterviewMode::Resumed, InterviewPhase::Answered) => {
                        match interviews.get(&state.member) {
                            Some(id) => MemberAttribution::resumed(id.clone()),
                            // An answer with no provenance row cannot be shown
                            // as the teammate's own words: nothing links it.
                            None => {
                                MemberAttribution::evidence_only(EvidenceOnlyReason::NoSessionId)
                            }
                        }
                    }
                    (InterviewMode::Resumed, InterviewPhase::Failed(failure)) => {
                        MemberAttribution::evidence_only(failure.evidence_only_reason())
                    }
                    (InterviewMode::Resumed, _) => {
                        MemberAttribution::evidence_only(EvidenceOnlyReason::InterviewTimedOut)
                    }
                    (InterviewMode::EvidenceOnly, InterviewPhase::Failed(failure)) => {
                        MemberAttribution::evidence_only(failure.evidence_only_reason())
                    }
                    (InterviewMode::EvidenceOnly, _) => {
                        MemberAttribution::evidence_only(EvidenceOnlyReason::NoSessionId)
                    }
                };
                (state.member.clone(), attribution)
            })
            .collect();
        Attribution { members }
    }
}

// ── findings ───────────────────────────────────────────────────────────────

/// `review_finding.level`, the closed vocabulary the store ASSERTs
/// (`store/migrations/0001_init.surql:280-281`).
///
/// P1 left [`ReviewFindingSeed::level`] a raw `String` because this module —
/// the pure core that produces the value — owns naming it. This is that name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FindingLevel {
    /// The node's brief was wrong: wording, role, missing context.
    Prompt,
    /// The plan was wrong: the wrong demand, the wrong budget, the wrong node.
    Structural,
}

impl FindingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Structural => "structural",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "prompt" => Some(Self::Prompt),
            "structural" => Some(Self::Structural),
            _ => None,
        }
    }

    pub const ALL: [Self; 2] = [Self::Prompt, Self::Structural];
}

/// `review_finding.verdict`, the store's other closed vocabulary
/// (`0001_init.surql:282-283`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FindingVerdict {
    /// The node is fine. Recorded, because "we looked and it was fine" is
    /// evidence too.
    Keep,
    Improve,
    /// Fire and replace — and the finding must carry the replacement.
    Replace,
}

impl FindingVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Improve => "improve",
            Self::Replace => "replace",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "keep" => Some(Self::Keep),
            "improve" => Some(Self::Improve),
            "replace" => Some(Self::Replace),
            _ => None,
        }
    }

    pub const ALL: [Self; 3] = [Self::Keep, Self::Improve, Self::Replace];
}

/// How one member's findings may be attributed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberAttribution {
    pub mode: InterviewMode,
    pub interview: Option<InterrogationId>,
    pub reason: Option<EvidenceOnlyReason>,
}

impl MemberAttribution {
    pub fn resumed(interview: InterrogationId) -> Self {
        Self {
            mode: InterviewMode::Resumed,
            interview: Some(interview),
            reason: None,
        }
    }

    pub fn evidence_only(reason: EvidenceOnlyReason) -> Self {
        Self {
            mode: InterviewMode::EvidenceOnly,
            interview: None,
            reason: Some(reason),
        }
    }
}

/// Who may be quoted, and who may only be inferred about.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Attribution {
    members: BTreeMap<String, MemberAttribution>,
}

impl Attribution {
    pub fn new(members: BTreeMap<String, MemberAttribution>) -> Self {
        Self { members }
    }

    pub fn get(&self, member: &str) -> Option<&MemberAttribution> {
        self.members.get(member)
    }

    /// The honesty rule, in one function.
    ///
    /// A finding the synthesiser attributed to nobody, or to a member karvex
    /// never interviewed, is `evidence_only` with no interrogation row. There
    /// is no path through this function that produces `Resumed` without a
    /// named member whose interview answered and whose provenance row exists.
    pub fn resolve(&self, member: Option<&str>) -> MemberAttribution {
        let Some(member) = member.map(str::trim).filter(|name| !name.is_empty()) else {
            return MemberAttribution::evidence_only(EvidenceOnlyReason::NoSessionId);
        };
        match self.members.get(member) {
            Some(attribution) => attribution.clone(),
            None => MemberAttribution::evidence_only(EvidenceOnlyReason::NoSessionId),
        }
    }
}

/// One finding as the synthesiser reported it, before karvex decides what it
/// is allowed to claim about it.
///
/// Separate from [`ReviewFindingSeed`] on purpose: the seed carries
/// `interview_mode` and `interview`, and neither is the synthesiser's to
/// choose.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedFinding {
    pub node_key: NodeKey,
    /// Which member's account this came out of, when the synthesiser named
    /// one. Optional, and an absent value is honest: some findings are about
    /// the run, not about a teammate.
    pub source_member: Option<String>,
    pub level: FindingLevel,
    pub verdict: FindingVerdict,
    pub rationale: String,
    pub evidence: serde_json::Value,
    pub proposed_change: serde_json::Value,
    /// Mandatory when `verdict == Replace`; the parser refuses the document
    /// otherwise, so this is `Some` whenever the verdict demands it.
    pub replacement: Option<serde_json::Value>,
}

/// Turns a reported finding into the row seed, stamping the attribution karvex
/// — not the synthesiser — decided.
///
/// `evidence` is wrapped rather than passed through: the finding's own
/// evidence goes under `reported`, and karvex's attribution goes beside it
/// under `attribution`, so a reader of the stored row can always tell which
/// half an agent wrote.
pub fn finding_seed(
    finding: &ParsedFinding,
    attribution: &Attribution,
    run_nodes: &BTreeMap<NodeKey, InstancePath>,
) -> ReviewFindingSeed {
    let resolved = attribution.resolve(finding.source_member.as_deref());
    let mut attribution_json = serde_json::Map::new();
    attribution_json.insert(
        "member".into(),
        match &finding.source_member {
            Some(member) => serde_json::Value::String(member.clone()),
            None => serde_json::Value::Null,
        },
    );
    attribution_json.insert(
        "interview_mode".into(),
        serde_json::Value::String(resolved.mode.as_str().to_string()),
    );
    if let Some(reason) = resolved.reason {
        attribution_json.insert(
            "reason".into(),
            serde_json::Value::String(reason.as_str().to_string()),
        );
    }
    let evidence = serde_json::json!({
        "reported": finding.evidence.clone(),
        "attribution": serde_json::Value::Object(attribution_json),
    });

    ReviewFindingSeed {
        node_key: finding.node_key.clone(),
        run_node: run_nodes.get(&finding.node_key).cloned(),
        interview: resolved.interview.clone(),
        interview_mode: resolved.mode,
        level: finding.level.as_str().to_string(),
        verdict: finding.verdict.as_str().to_string(),
        rationale: finding.rationale.clone(),
        evidence,
        proposed_change: finding.proposed_change.clone(),
        replacement: finding.replacement.clone(),
    }
}

// ── formatting helpers shared with `review_prompt` ─────────────────────────

/// `1h 04m 12s`, or `-` for zero. One author for every duration the review
/// cycle shows a human or an agent, so two documents never disagree about how
/// long something took.
pub fn format_duration_ms(ms: u64) -> String {
    if ms == 0 {
        return "-".to_string();
    }
    let total_secs = ms / 1000;
    let (hours, minutes, seconds) = (total_secs / 3600, (total_secs % 3600) / 60, total_secs % 60);
    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

impl fmt::Display for EvidenceOnlyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for FindingLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for FindingVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::{InstancePath, NodeKey, RunId};

    fn task(subject: &str) -> TaskEvidence {
        TaskEvidence {
            path: InstancePath(subject.to_string()),
            node_key: Some(NodeKey(subject.to_string())),
            subject: subject.to_string(),
            status: NodeStatus::Succeeded,
            emergent: false,
            attention: None,
            owner: None,
            owner_changes: Vec::new(),
            unresolved_blockers: Vec::new(),
            first_seen_at_unix_ms: 0,
            last_change_at_unix_ms: 0,
            in_progress_ms: 0,
            idle_while_in_progress_ms: 0,
            interventions: Vec::new(),
        }
    }

    fn member(name: &str, tasks: Vec<TaskEvidence>) -> MemberEvidence {
        MemberEvidence {
            name: name.to_string(),
            tasks,
            last_state: None,
            last_state_at_unix_ms: None,
        }
    }

    fn identity(name: &str) -> MemberIdentity {
        MemberIdentity {
            name: name.to_string(),
            is_lead: false,
            model: "sonnet".into(),
            backend_type: "tmux".into(),
            cwd: Some("/abs/project".into()),
            session_id: Some(format!("sid-{name}")),
            transcript_path: Some(format!("/abs/{name}.jsonl")),
            transcript_readable: true,
        }
    }

    fn run(members: Vec<MemberEvidence>) -> RunEvidence {
        RunEvidence {
            run_id: RunId("workflow_run:1".into()),
            workflow_name: "ship it".into(),
            kvdag_version: 4,
            status: RunStatus::Succeeded,
            started_at_unix_ms: 1_000,
            ended_at_unix_ms: Some(61_000),
            summary: None,
            failure: None,
            members,
            unowned_tasks: Vec::new(),
        }
    }

    #[test]
    fn interview_mode_needs_both_an_id_and_a_readable_transcript() {
        assert_eq!(
            decide_interview_mode(None, true),
            (
                InterviewMode::EvidenceOnly,
                Some(EvidenceOnlyReason::NoSessionId)
            ),
        );
        assert_eq!(
            decide_interview_mode(Some("   "), true),
            (
                InterviewMode::EvidenceOnly,
                Some(EvidenceOnlyReason::NoSessionId)
            ),
            "a blank session id is no session id",
        );
        assert_eq!(
            decide_interview_mode(Some("sid"), false),
            (
                InterviewMode::EvidenceOnly,
                Some(EvidenceOnlyReason::TranscriptUnreadable)
            ),
        );
        assert_eq!(
            decide_interview_mode(Some("sid"), true),
            (InterviewMode::Resumed, None),
        );
    }

    #[test]
    fn a_member_without_a_session_id_is_always_evidence_only() {
        let mut nameless = identity("ghost");
        nameless.session_id = None;
        let evidence = run(vec![member("ghost", vec![task("a")])]);
        let plan = ReviewPlan::build(&[nameless], &evidence, &ReviewPolicy::default());
        let assignment = &plan.assignments[0];
        assert_eq!(assignment.mode, InterviewMode::EvidenceOnly);
        assert_eq!(
            assignment.evidence_only_reason,
            Some(EvidenceOnlyReason::NoSessionId)
        );
        assert!(
            assignment.source_session_id.is_none(),
            "an evidence-only assignment must never carry a session to resume",
        );
        assert_eq!(plan.evidence_only_count(), 1);
    }

    #[test]
    fn ranking_puts_the_troubled_members_first() {
        let calm = member("calm", vec![task("a")]);
        let mut troubled_task = task("b");
        troubled_task.status = NodeStatus::Blocked;
        troubled_task.interventions = vec![InterventionEvidence {
            at_unix_ms: 10,
            rung: 3,
            kind: "local_loop".into(),
            channel: Some("message".into()),
            delivered: true,
            text: Some("[karvex · watchdog] you have been idle".into()),
        }];
        let troubled = member("troubled", vec![troubled_task]);
        let mildly = member(
            "mildly",
            vec![TaskEvidence {
                emergent: true,
                ..task("c")
            }],
        );

        let evidence = run(vec![calm, troubled, mildly]);
        let plan = ReviewPlan::build(
            &[identity("calm"), identity("troubled"), identity("mildly")],
            &evidence,
            &ReviewPolicy::default(),
        );
        let order: Vec<&str> = plan
            .assignments
            .iter()
            .map(|assignment| assignment.member.as_str())
            .collect();
        assert_eq!(order, vec!["troubled", "mildly", "calm"]);
        assert!(plan.assignments[2].trouble.total() == 0);
    }

    #[test]
    fn the_cap_binds_and_says_who_it_dropped() {
        let members: Vec<MemberEvidence> = (0..5)
            .map(|index| {
                let mut owned = task(&format!("t{index}"));
                owned.interventions = (0..index)
                    .map(|_| InterventionEvidence {
                        at_unix_ms: 0,
                        rung: 1,
                        kind: "local_loop".into(),
                        channel: None,
                        delivered: false,
                        text: None,
                    })
                    .collect();
                member(&format!("m{index}"), vec![owned])
            })
            .collect();
        let identities: Vec<MemberIdentity> =
            (0..5).map(|index| identity(&format!("m{index}"))).collect();
        let evidence = run(members);

        let plan = ReviewPlan::build(&identities, &evidence, &ReviewPolicy::from_config(2));
        assert_eq!(
            plan.assignments
                .iter()
                .map(|a| a.member.as_str())
                .collect::<Vec<_>>(),
            vec!["m4", "m3"],
        );
        assert_eq!(
            plan.skipped
                .iter()
                .map(|s| (s.member.as_str(), s.reason))
                .collect::<Vec<_>>(),
            vec![
                ("m2", SkipReason::OverCap),
                ("m1", SkipReason::OverCap),
                ("m0", SkipReason::OverCap),
            ],
        );
    }

    #[test]
    fn a_zero_cap_plans_nothing_rather_than_pretending() {
        let evidence = run(vec![member("a", vec![task("x")])]);
        let plan = ReviewPlan::build(&[identity("a")], &evidence, &ReviewPolicy::from_config(0));
        assert!(plan.is_empty());
        assert_eq!(plan.skipped.len(), 1);
    }

    #[test]
    fn the_lead_is_ranked_first_only_when_the_run_did_not_finish_cleanly() {
        let mut lead_identity = identity("lead");
        lead_identity.is_lead = true;
        let identities = vec![lead_identity, identity("busy")];

        let mut busy_task = task("busy-task");
        busy_task.interventions = vec![InterventionEvidence {
            at_unix_ms: 0,
            rung: 4,
            kind: "external_wait".into(),
            channel: None,
            delivered: false,
            text: None,
        }];
        let members = vec![
            member("lead", vec![task("lead-task")]),
            member("busy", vec![busy_task]),
        ];

        let clean = run(members.clone());
        assert!(clean.finished_cleanly());
        let plan = ReviewPlan::build(&identities, &clean, &ReviewPolicy::default());
        assert_eq!(
            plan.assignments[0].member, "busy",
            "trouble wins on a clean run"
        );

        let mut dirty = run(members);
        dirty.status = RunStatus::Failed;
        assert!(!dirty.finished_cleanly());
        let plan = ReviewPlan::build(&identities, &dirty, &ReviewPolicy::default());
        assert_eq!(
            plan.assignments[0].member, "lead",
            "a run that did not finish cleanly is the lead's run",
        );
    }

    #[test]
    fn an_unfinished_task_means_the_run_did_not_finish_cleanly() {
        let mut unfinished = task("open");
        unfinished.status = NodeStatus::Running;
        let evidence = run(vec![member("a", vec![unfinished])]);
        assert_eq!(evidence.status, RunStatus::Succeeded);
        assert!(!evidence.finished_cleanly());
    }

    #[test]
    fn trouble_score_saturates_idle_time() {
        let mut idle = task("slow");
        idle.idle_while_in_progress_ms = 10 * 60 * 60 * 1000;
        let evidence = member("slow", vec![idle]);
        let score = TroubleScore::measure(Some(&evidence));
        assert_eq!(score.idle_minutes, 600);
        assert_eq!(
            score.total(),
            TroubleScore::MAX_IDLE_POINTS,
            "an hour idle and a day idle are the same finding",
        );
    }

    #[test]
    fn a_member_with_no_evidence_at_all_still_scores() {
        let score = TroubleScore::measure(None);
        assert!(score.silent);
        assert!(
            score.total() > 0,
            "silence is a finding, not an absence of one"
        );
    }

    fn plan_of(members: &[(&str, InterviewMode)]) -> ReviewPlan {
        ReviewPlan {
            assignments: members
                .iter()
                .map(|(name, mode)| InterviewAssignment {
                    member: (*name).to_string(),
                    is_lead: false,
                    mode: *mode,
                    evidence_only_reason: match mode {
                        InterviewMode::Resumed => None,
                        InterviewMode::EvidenceOnly => Some(EvidenceOnlyReason::NoSessionId),
                    },
                    source_session_id: match mode {
                        InterviewMode::Resumed => Some(format!("sid-{name}")),
                        InterviewMode::EvidenceOnly => None,
                    },
                    member_cwd: None,
                    trouble: TroubleScore::default(),
                })
                .collect(),
            skipped: Vec::new(),
        }
    }

    fn observation(member: &str, alive: bool, state: InterviewPaneState) -> ObservedInterview {
        ObservedInterview {
            member: member.to_string(),
            started_at_unix_ms: 0,
            pane_alive: alive,
            pane_state: Some(state),
            answer_recorded: false,
        }
    }

    #[test]
    fn absorb_is_empty_when_nothing_changed() {
        let plan = plan_of(&[("a", InterviewMode::Resumed)]);
        let mut state = ReviewCycleState::from_plan(&plan, ReviewPolicy::default());
        let observed = vec![observation("a", true, InterviewPaneState::Working)];

        let first = state.absorb(&observed, 1_000);
        assert_eq!(first.interviews.len(), 1);
        assert_eq!(first.interviews[0].to, InterviewPhase::Running);

        let second = state.absorb(&observed, 2_000);
        assert!(
            second.is_empty(),
            "a 2s poll must not re-emit its whole view"
        );
    }

    #[test]
    fn an_answer_beats_every_other_outcome() {
        let plan = plan_of(&[("a", InterviewMode::Resumed)]);
        let mut state = ReviewCycleState::from_plan(&plan, ReviewPolicy::default());
        let mut observed = observation("a", false, InterviewPaneState::Blocked);
        observed.answer_recorded = true;
        // Well past the deadline, pane dead, pane blocked: it still answered.
        let delta = state.absorb(&[observed], DEFAULT_INTERVIEW_TIMEOUT_MS * 10);
        assert_eq!(delta.interviews[0].to, InterviewPhase::Answered);
        assert!(delta.interviews_settled);
    }

    #[test]
    fn a_blocked_pane_is_a_first_class_outcome_after_its_grace() {
        let plan = plan_of(&[("a", InterviewMode::Resumed)]);
        let mut state = ReviewCycleState::from_plan(&plan, ReviewPolicy::default());
        let observed = vec![observation("a", true, InterviewPaneState::Blocked)];

        state.absorb(&observed, 0);
        assert_eq!(
            state.interview("a").expect("interview").phase,
            InterviewPhase::Running,
            "a human may still answer the dialog",
        );

        let delta = state.absorb(&observed, DEFAULT_BLOCKED_GRACE_MS);
        assert_eq!(
            delta.interviews[0].to,
            InterviewPhase::Failed(InterviewFailure::Blocked),
        );
    }

    #[test]
    fn a_pane_that_recovers_from_blocked_keeps_its_grace_window_fresh() {
        let plan = plan_of(&[("a", InterviewMode::Resumed)]);
        let mut state = ReviewCycleState::from_plan(&plan, ReviewPolicy::default());
        state.absorb(&[observation("a", true, InterviewPaneState::Blocked)], 0);
        state.absorb(&[observation("a", true, InterviewPaneState::Working)], 10);
        let delta = state.absorb(
            &[observation("a", true, InterviewPaneState::Blocked)],
            DEFAULT_BLOCKED_GRACE_MS,
        );
        assert!(
            delta.is_empty(),
            "the grace window restarts when the pane starts working again",
        );
    }

    #[test]
    fn the_deadline_and_a_dead_pane_both_degrade_the_interview() {
        let plan = plan_of(&[
            ("late", InterviewMode::Resumed),
            ("dead", InterviewMode::Resumed),
        ]);
        let mut state = ReviewCycleState::from_plan(&plan, ReviewPolicy::default());
        let delta = state.absorb(
            &[
                observation("late", true, InterviewPaneState::Working),
                observation("dead", false, InterviewPaneState::Unknown),
            ],
            DEFAULT_INTERVIEW_TIMEOUT_MS,
        );
        assert_eq!(
            delta
                .interviews
                .iter()
                .map(|t| (t.member.as_str(), t.to))
                .collect::<Vec<_>>(),
            vec![
                ("dead", InterviewPhase::Failed(InterviewFailure::PaneGone)),
                ("late", InterviewPhase::Failed(InterviewFailure::TimedOut)),
            ],
            "transitions come out by member name, not poll order",
        );
        assert!(delta.interviews_settled);
    }

    #[test]
    fn a_terminal_interview_never_moves_again_and_settles_once() {
        let plan = plan_of(&[("a", InterviewMode::Resumed)]);
        let mut state = ReviewCycleState::from_plan(&plan, ReviewPolicy::default());
        let mut answered = observation("a", true, InterviewPaneState::Idle);
        answered.answer_recorded = true;
        let first = state.absorb(&[answered], 10);
        assert!(first.interviews_settled);

        let later = state.absorb(&[observation("a", false, InterviewPaneState::Unknown)], 20);
        assert!(
            later.is_empty(),
            "degrading a member twice would double-count it in the synthesis document",
        );
    }

    #[test]
    fn an_observation_for_an_unplanned_member_is_reported_not_dropped() {
        let plan = plan_of(&[("a", InterviewMode::Resumed)]);
        let mut state = ReviewCycleState::from_plan(&plan, ReviewPolicy::default());
        let delta = state.absorb(
            &[observation("stranger", true, InterviewPaneState::Idle)],
            5,
        );
        assert_eq!(delta.unknown_members, vec!["stranger".to_string()]);
        assert!(!delta.is_empty());
    }

    #[test]
    fn synthesis_may_be_retried_once_and_then_fails_the_cycle() {
        let plan = plan_of(&[("a", InterviewMode::Resumed)]);
        let mut state = ReviewCycleState::from_plan(&plan, ReviewPolicy::default());
        assert_eq!(state.record_synthesis_failure(), SynthesisOutcome::Retry);
        assert_eq!(
            state.record_synthesis_failure(),
            SynthesisOutcome::FailCycle
        );
        assert_eq!(state.synthesis_attempts(), SYNTHESIS_MAX_ATTEMPTS);
    }

    fn interview_ids(names: &[&str]) -> BTreeMap<String, InterrogationId> {
        names
            .iter()
            .map(|name| {
                (
                    (*name).to_string(),
                    InterrogationId(format!("interrogation:{name}")),
                )
            })
            .collect()
    }

    #[test]
    fn only_an_answered_resumed_interview_with_provenance_is_attributable() {
        let plan = plan_of(&[
            ("answered", InterviewMode::Resumed),
            ("orphan", InterviewMode::Resumed),
            ("blocked", InterviewMode::Resumed),
            ("reader", InterviewMode::EvidenceOnly),
        ]);
        let mut state = ReviewCycleState::from_plan(&plan, ReviewPolicy::default());

        let mut answered = observation("answered", true, InterviewPaneState::Idle);
        answered.answer_recorded = true;
        let mut orphan = observation("orphan", true, InterviewPaneState::Idle);
        orphan.answer_recorded = true;
        let mut reader = observation("reader", true, InterviewPaneState::Idle);
        reader.answer_recorded = true;
        state.absorb(&[answered, orphan, reader], 10);
        state.absorb(
            &[observation("blocked", true, InterviewPaneState::Blocked)],
            10,
        );
        state.absorb(
            &[observation("blocked", true, InterviewPaneState::Blocked)],
            10 + DEFAULT_BLOCKED_GRACE_MS,
        );

        let attribution = state.attribution(&interview_ids(&["answered", "blocked", "reader"]));

        assert_eq!(
            attribution.resolve(Some("answered")).mode,
            InterviewMode::Resumed,
        );
        assert_eq!(
            attribution.resolve(Some("orphan")).mode,
            InterviewMode::EvidenceOnly,
            "an answer with no interrogation row cannot be shown as the teammate's words",
        );
        let blocked = attribution.resolve(Some("blocked"));
        assert_eq!(blocked.mode, InterviewMode::EvidenceOnly);
        assert_eq!(blocked.reason, Some(EvidenceOnlyReason::InterviewBlocked));
        assert!(blocked.interview.is_none());
        assert_eq!(
            attribution.resolve(Some("reader")).mode,
            InterviewMode::EvidenceOnly,
            "an evidence-only interview stays evidence-only however well it answers",
        );
        assert_eq!(
            attribution.resolve(None).mode,
            InterviewMode::EvidenceOnly,
            "a finding attributed to nobody is attributed to nobody",
        );
        assert_eq!(
            attribution.resolve(Some("never-interviewed")).mode,
            InterviewMode::EvidenceOnly,
        );
    }

    fn parsed(node_key: &str, source: Option<&str>) -> ParsedFinding {
        ParsedFinding {
            node_key: NodeKey(node_key.to_string()),
            source_member: source.map(str::to_string),
            level: FindingLevel::Prompt,
            verdict: FindingVerdict::Improve,
            rationale: "because".into(),
            evidence: serde_json::json!({"idle_ms": 5}),
            proposed_change: serde_json::json!({"prompt_template": "better"}),
            replacement: None,
        }
    }

    #[test]
    fn a_seed_carries_the_attribution_karvex_decided_not_the_one_claimed() {
        let attribution = Attribution::new(BTreeMap::from([
            (
                "real".to_string(),
                MemberAttribution::resumed(InterrogationId("interrogation:1".into())),
            ),
            (
                "reader".to_string(),
                MemberAttribution::evidence_only(EvidenceOnlyReason::TranscriptUnreadable),
            ),
        ]));
        let run_nodes = BTreeMap::from([(NodeKey("verify".into()), InstancePath("verify".into()))]);

        let seed = finding_seed(&parsed("verify", Some("real")), &attribution, &run_nodes);
        assert_eq!(seed.interview_mode, InterviewMode::Resumed);
        assert_eq!(
            seed.interview,
            Some(InterrogationId("interrogation:1".into()))
        );
        assert_eq!(seed.run_node, Some(InstancePath("verify".into())));
        assert_eq!(seed.level, "prompt");
        assert_eq!(seed.verdict, "improve");
        assert_eq!(seed.evidence["reported"], serde_json::json!({"idle_ms": 5}));
        assert_eq!(seed.evidence["attribution"]["member"], "real");
        assert_eq!(seed.evidence["attribution"]["interview_mode"], "resumed");

        let seed = finding_seed(&parsed("verify", Some("reader")), &attribution, &run_nodes);
        assert_eq!(seed.interview_mode, InterviewMode::EvidenceOnly);
        assert!(seed.interview.is_none());
        assert_eq!(
            seed.evidence["attribution"]["reason"],
            "transcript_unreadable",
        );

        let seed = finding_seed(&parsed("verify", Some("liar")), &attribution, &run_nodes);
        assert_eq!(
            seed.interview_mode,
            InterviewMode::EvidenceOnly,
            "naming a member karvex never interviewed does not make a finding attributable",
        );
        assert!(seed.evidence["attribution"]["member"] == "liar");

        let seed = finding_seed(&parsed("missing", None), &attribution, &run_nodes);
        assert_eq!(seed.interview_mode, InterviewMode::EvidenceOnly);
        assert!(
            seed.run_node.is_none(),
            "an unmatched node key resolves to no run node"
        );
        assert!(seed.evidence["attribution"]["member"].is_null());
    }

    #[test]
    fn the_finding_vocabularies_are_the_ones_the_store_asserts() {
        assert_eq!(
            FindingLevel::ALL.map(FindingLevel::as_str),
            ["prompt", "structural"],
        );
        assert_eq!(
            FindingVerdict::ALL.map(FindingVerdict::as_str),
            ["keep", "improve", "replace"],
        );
        for level in FindingLevel::ALL {
            assert_eq!(FindingLevel::parse(level.as_str()), Some(level));
        }
        for verdict in FindingVerdict::ALL {
            assert_eq!(FindingVerdict::parse(verdict.as_str()), Some(verdict));
        }
        assert!(FindingLevel::parse("Prompt").is_none());
        assert!(FindingVerdict::parse("").is_none());
    }

    #[test]
    fn durations_read_the_same_everywhere() {
        assert_eq!(format_duration_ms(0), "-");
        assert_eq!(format_duration_ms(1_000), "1s");
        assert_eq!(format_duration_ms(62_000), "1m 02s");
        assert_eq!(format_duration_ms(3_872_000), "1h 04m 32s");
    }

    #[test]
    fn interview_pane_states_survive_an_unknown_word() {
        assert_eq!(
            InterviewPaneState::parse("blocked"),
            InterviewPaneState::Blocked
        );
        assert_eq!(InterviewPaneState::parse("done"), InterviewPaneState::Done);
        assert_eq!(
            InterviewPaneState::parse("compacting"),
            InterviewPaneState::Unknown,
        );
    }
}
