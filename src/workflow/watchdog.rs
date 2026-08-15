//! The pure watchdog: what karvex concludes from watching a run, and the
//! sentences it says about it.
//!
//! `phase4-retarget-plan.md` §3.4 and P4. Karvex does not execute a workflow
//! any more — Claude Code's team lead does — so the entire anti-stuck story is
//! *see, say, record*. This module is the "decide what to say" half: samples in
//! ([`ObservedNode`]), a class, a rung, and the exact agent-facing text out
//! ([`WatchdogVerdict`]). Every side effect — reading panes, delivering a
//! message, journalling, writing `run_node.attention`, emitting
//! `workflow.node.watchdog` — belongs to the adapter (`app/workflow_watchdog.rs`,
//! P9). Nothing here touches the filesystem, the store, a pane, or a clock.
//!
//! ## The three rules this file exists to keep
//!
//! 1. **Pane detection is the instrument; task status is the target.** Upstream
//!    documents that "teammates sometimes fail to mark tasks as completed"
//!    (`.local/prd/upstream/claude-agent-teams.md`, Limitations), so a lagging
//!    task status is never evidence on its own. The signal is the *disagreement*:
//!    a task that says `in_progress` while karvex's own evidence-based detection
//!    has watched the owner's pane sit idle. `classify` reads the pane first and
//!    the status second, always.
//! 2. **The ladder cannot restart anyone.** Rung 3 asks the *lead* to nudge,
//!    reassign, or respawn, because the lead is the only actor that can. The
//!    Phase-3 rung "write a checkpoint, kill the pane, respawn at attempt + 1"
//!    is gone with the engine that could do it, and is not coming back here.
//! 3. **A rung is only spent when the say actually happened.** Delivery can fail
//!    — no endpoint survives a karvex restart, and messaging is off on native
//!    Windows — so [`LadderState::after`] advances the rung only on
//!    [`DeliveryOutcome::Delivered`]. A nudge nobody received must never look
//!    like a nudge that was ignored.
//!
//! ## Single authority for the text
//!
//! Every `[karvex · watchdog]`-framed sentence karvex sends about a stuck node
//! is composed here, by [`nudge_text`], [`reprompt_text`], or
//! [`lead_escalation_text`]. No adapter, handler, or UI module may write its
//! own: the frame is a contract the skill file teaches agents to recognise, and
//! two authors for one voice is how a contract rots. Paths in these messages are
//! absolute or bare names, never `./`-relative — a relative path means nothing
//! to a session whose cwd karvex does not control.
//!
//! ## What it never does
//!
//! It never produces a [`NodeStatus`](crate::workflow::model::NodeStatus).
//! `run_node.status` holds Claude Code's own projected fact and
//! [`Attention`] is karvex's opinion about it, in its own column
//! (`phase4-retarget-plan.md` D-10). A verdict carries an `Attention` and never
//! a status, so there is no expression in this module that could overwrite one.

use serde::Serialize;

use crate::detect::AgentState;
use crate::workflow::model::{Attention, InstancePath};
use crate::workflow::projection::TaskStatus;

/// The frame every watchdog message opens with.
///
/// The receiving agent uses it to tell runtime steering from its human operator
/// — karvex's messages arrive on the same two channels a human's do (the
/// session's inbox socket, or typed into the pane), so the frame is the only
/// thing that separates them. `skills/karvex/SKILL.md` teaches it.
pub const WATCHDOG_FRAME: &str = "[karvex · watchdog]";

/// How karvex refers to the run's lead when talking to a teammate about it.
///
/// The same word Claude Code's own roster uses (`binding::lead::LEAD_TARGET_NAME`
/// is the addressing form of it), so a teammate told to "tell the team lead" can
/// act on it without a translation step.
const LEAD_ROLE_WORD: &str = "team lead";

// ── classification ─────────────────────────────────────────────────────────

/// What one watchdog sample concluded about one node.
///
/// Only [`ProgressClass::LocalLoop`] and [`ProgressClass::Unknown`] walk the
/// ladder. The other two are normal states of a healthy run and must never
/// produce a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressClass {
    /// The owner's pane is working, its state moved since the last sample, or
    /// its usage moved. Resets the streak *and* the ladder.
    Progressing,
    /// The owner's pane is idle while its task is still `in_progress`. This is
    /// the canonical stuck case and the only one a teammate is messaged about.
    LocalLoop,
    /// Somebody other than the teammate has to move first: the pane needs human
    /// input, the run's lead is blocked, or the task is `blockedBy` a task that
    /// has not finished. Surfaced, never nudged — a prompt cannot answer a
    /// permission dialog.
    ExternalWait,
    /// There is nobody to nudge: no owner, an owner that left the roster, an
    /// owner with no pane karvex can see, or a pane that is no longer running a
    /// recognised agent.
    Unknown,
}

impl ProgressClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Progressing => "progressing",
            Self::LocalLoop => "local_loop",
            Self::ExternalWait => "external_wait",
            Self::Unknown => "unknown",
        }
    }

    /// Whether this class accumulates the no-progress streak. `ExternalWait`
    /// holds the streak where it is rather than clearing it: the wait is not
    /// progress, but it is not the teammate's fault either.
    pub fn counts_toward_streak(self) -> bool {
        matches!(self, Self::LocalLoop | Self::Unknown)
    }
}

/// The escalation ladder, in order. `Ord` is the ladder: a rung is never
/// skipped downward, and `Unknown` uses it to jump *up* to
/// [`Escalation::EscalateToLead`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Escalation {
    /// Rung 1 — a short steer to the teammate naming its task, its status, and
    /// how long its pane has been idle.
    Nudge,
    /// Rung 2 — the structured re-prompt: names the exact disagreement between
    /// the task status and the pane, and asks for one of three concrete acts.
    Reprompt,
    /// Rung 3 — tell the lead, with the measurements, and name the two remedies
    /// upstream itself prescribes (reassign, or respawn). Karvex cannot do
    /// either; the lead can.
    EscalateToLead,
    /// Rung 4 — stop talking. `run_node.attention`, one notice, the node detail
    /// line. The run keeps going and nothing is killed.
    Surface,
}

impl Escalation {
    /// 1-based, the way §3.4 numbers the ladder and the way the journal payload
    /// records it.
    pub fn rung(self) -> u8 {
        match self {
            Self::Nudge => 1,
            Self::Reprompt => 2,
            Self::EscalateToLead => 3,
            Self::Surface => 4,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nudge => "nudge",
            Self::Reprompt => "reprompt",
            Self::EscalateToLead => "escalate_to_lead",
            Self::Surface => "surface",
        }
    }

    /// Claude Code's own inbox priority word for this rung.
    ///
    /// Returned as the word rather than a `messaging::Priority` so the pure
    /// layer keeps no dependency on the binding layer; the words are exactly the
    /// ones `binding::messaging::Priority::parse` accepts, which the tests pin.
    /// A nudge queues for the teammate's next turn (interrupting a running tool
    /// call to say "are you stuck" would be self-defeating); an escalation to
    /// the lead is read between tool calls, because the lead is the actor that
    /// can unblock the run.
    pub fn message_priority(self) -> &'static str {
        match self {
            Self::Nudge | Self::Reprompt => "next",
            Self::EscalateToLead => "now",
            Self::Surface => "later",
        }
    }
}

// ── inputs ─────────────────────────────────────────────────────────────────

/// What karvex's own detection says one pane is doing, plus how long it has
/// been saying it.
///
/// `state_age_ms` is measured from `last_agent_state_change_seq`'s observation,
/// not from the sample count: the sample count is a floor on the age (samples
/// are `watchdog_tick_secs` apart) and the wall-clock age is what a message can
/// honestly quote to an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneObservation {
    /// The public pane id, as the DAG and `kvx agent` name it.
    pub pane_id: String,
    pub state: AgentState,
    /// How long the pane has been in `state`.
    pub state_age_ms: u64,
    /// Whether the pane's agent state changed since the previous watchdog
    /// sample. A change is movement even when the new state is `idle`.
    pub changed_since_last_sample: bool,
}

/// Who owns the task, and whether karvex can see them at all.
///
/// An enum rather than three loose fields because the illegal combinations
/// (a pane with no owner, an owner that is both vanished and observed) are the
/// ones that would produce a nonsense message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerState {
    /// The task file names no owner: nobody has claimed it.
    Unclaimed,
    /// The task names an owner who is no longer in the team roster.
    Vanished { name: String },
    /// In the roster, but karvex has no pane for it — an in-process teammate,
    /// or a pane that has since closed.
    NoPane { name: String },
    /// In the roster, with a pane karvex is watching.
    Observed { name: String, pane: PaneObservation },
}

impl OwnerState {
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Unclaimed => None,
            Self::Vanished { name } | Self::NoPane { name } | Self::Observed { name, .. } => {
                Some(name.as_str())
            }
        }
    }

    pub fn pane(&self) -> Option<&PaneObservation> {
        match self {
            Self::Observed { pane, .. } => Some(pane),
            _ => None,
        }
    }
}

/// Movement in the owner's transcript since the last sample.
///
/// Optional because karvex only has it once a member's session id and
/// transcript path are resolved (P8); a run where that never happens is still
/// fully watched, just on pane state alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageDelta {
    pub tool_uses: u32,
    pub tokens: u64,
}

impl UsageDelta {
    pub fn moved(self) -> bool {
        self.tool_uses > 0 || self.tokens > 0
    }
}

/// A task this node is `blockedBy` that has not finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockingTask {
    pub id: String,
    pub subject: String,
}

/// Everything one sample knows about one node. Assembled by the adapter from
/// the projection, the panes, and the node's own row; consumed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedNode {
    /// The node's instance path — `.lead` for the run's lead, `.task/<id>` for
    /// an emergent task, the definition key otherwise.
    pub path: InstancePath,
    /// Claude Code's task id, when this node is a projected task.
    pub task_id: Option<String>,
    /// The task subject, verbatim from the task file. This is what the teammate
    /// sees in its own task list, so it is what a message must name.
    pub subject: String,
    /// The projected status. Never written by the watchdog (D-10).
    pub status: TaskStatus,
    /// Whether this node is the run's lead rather than a member-owned task.
    /// The lead walks a shorter ladder: there is nobody above it to escalate to.
    pub is_lead: bool,
    pub owner: OwnerState,
    /// The lead's pane, for a member node. A blocked lead blocks the whole run,
    /// so every member inherits the fact instead of each discovering it.
    /// Ignored when `is_lead` — that pane is already in `owner`.
    pub lead_pane: Option<PaneObservation>,
    /// Only the blockers that have *not* finished.
    pub blocked_by: Vec<BlockingTask>,
    /// The node's authored `timeout_ms`, if the definition set one. Exceeding it
    /// is surfaced, never enforced: karvex has nothing left to kill.
    pub budget_ms: Option<u64>,
    /// How long this node has been in progress.
    pub elapsed_ms: u64,
    pub usage: Option<UsageDelta>,
    /// The ladder state carried from the previous sample.
    pub ladder: LadderState,
    /// Whether the run has closed. A closed run is never classified.
    pub run_closed: bool,
}

/// The ladder bookkeeping one node carries between samples.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LadderState {
    /// Consecutive samples with no material progress. Reset by progress and by
    /// every *delivered* intervention, so an intervention gets a full window to
    /// work before the next one fires.
    pub streak: u32,
    /// The highest rung actually delivered so far. `None` before the first one.
    pub rung: Option<Escalation>,
    /// Consecutive attempts at the current rung whose delivery was refused on
    /// every channel. Never advances the rung; two of them give up on saying
    /// anything and surface instead.
    pub undelivered: u32,
}

impl LadderState {
    /// The state after this sample, given what was decided and whether the say
    /// actually happened.
    ///
    /// The whole point of this function is the third arm: an undelivered rung is
    /// *not* consumed. It keeps the streak that earned it (so the same rung is
    /// retried on the next sample) and counts the failure, which is what
    /// eventually converts "karvex could not reach this session" into a surfaced
    /// fact rather than an infinite silent retry.
    pub fn after(self, verdict: &WatchdogDecision, outcome: DeliveryOutcome) -> Self {
        match (&verdict.action, outcome) {
            // Progress wipes the slate: streak, rung, and undelivered count.
            // Re-evaluation on every tick, not a one-way escalation.
            _ if verdict.class == ProgressClass::Progressing => Self::default(),
            (WatchdogAction::Hold, _) => Self {
                streak: verdict.streak,
                ..self
            },
            (WatchdogAction::Surface, _) => Self {
                streak: 0,
                rung: Some(Escalation::Surface),
                undelivered: 0,
            },
            (WatchdogAction::Say { rung, .. }, DeliveryOutcome::Delivered) => Self {
                streak: 0,
                rung: Some(*rung),
                undelivered: 0,
            },
            (WatchdogAction::Say { .. }, DeliveryOutcome::Undelivered) => Self {
                streak: verdict.streak,
                rung: self.rung,
                undelivered: self.undelivered.saturating_add(1),
            },
        }
    }
}

/// Whether the say actually happened, on either channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// The inbox socket accepted the frame, or karvex typed it into the pane.
    Delivered,
    /// Both channels refused. The rung is not spent.
    Undelivered,
}

// ── policy ─────────────────────────────────────────────────────────────────

/// The watchdog's half of `[workflow]`.
///
/// `stuck_threshold` and `drift_threshold` are published config keys that have
/// had no reader since the engine that used to own them was deleted
/// (`phase4-retarget-plan.md` D-8, WI-R2). This is the reader that makes them
/// mean something again, with their published names and defaults intact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchdogPolicy {
    /// `workflow.watchdog_enabled`. Off means nothing is classified at all — no
    /// messages, no `attention`, no journal.
    pub enabled: bool,
    /// `workflow.stuck_threshold`. Consecutive no-progress samples before
    /// rung 1.
    pub stuck_threshold: u32,
    /// `workflow.drift_threshold`. Consecutive no-progress samples between one
    /// delivered rung and the next — the documented task-status-lag window,
    /// which is why it is longer than `stuck_threshold`: after karvex has said
    /// something, the teammate deserves a wider window than it did before.
    pub drift_threshold: u32,
    /// `workflow.watchdog_tick_secs`. Sample cadence, quoted in messages so an
    /// agent can tell "idle for 4 minutes" from "sampled once".
    pub tick_secs: u64,
    /// How many consecutive undelivered attempts at one rung before the ladder
    /// stops trying to talk and surfaces instead. Not a config key: two is the
    /// point where "karvex cannot reach this session" is the finding.
    pub undelivered_limit: u32,
}

impl WatchdogPolicy {
    /// Two consecutive undelivered attempts jump to rung 4 (§3.4).
    pub const DEFAULT_UNDELIVERED_LIMIT: u32 = 2;

    /// Reads the published `[workflow]` knobs.
    ///
    /// A zero threshold would fire a rung on the very first sample of every
    /// node, which is a misconfiguration rather than an instruction, so both
    /// thresholds are floored at 1.
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            enabled: config.workflow.watchdog_enabled,
            stuck_threshold: threshold(config.workflow.stuck_threshold),
            drift_threshold: threshold(config.workflow.drift_threshold),
            tick_secs: config.workflow.watchdog_tick_secs.max(1),
            undelivered_limit: Self::DEFAULT_UNDELIVERED_LIMIT,
        }
    }
}

fn threshold(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX).max(1)
}

// ── outputs ────────────────────────────────────────────────────────────────

/// Who a watchdog message is addressed to.
///
/// The adapter resolves these against the run's own addressing table; the pure
/// layer only knows the role and the roster name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageTarget {
    /// The teammate that owns the task, by its team-config name.
    Member { name: String },
    /// The run's lead.
    Lead,
}

/// What the adapter should do about this node, this sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogAction {
    /// Nothing to say. `attention` on the verdict may still have changed.
    Hold,
    /// Say this, to them. `text` is final: the adapter delivers it verbatim.
    Say {
        rung: Escalation,
        target: MessageTarget,
        text: String,
    },
    /// Rung 4. No message; the attention column, one notice, and the node
    /// detail line carry it from here.
    Surface,
}

/// Why a node was not classified at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// `workflow.watchdog_enabled = false`.
    Disabled,
    /// The run is closed. Nothing about a finished run needs a nudge.
    RunClosed,
    /// Claude Code marked the task completed. Whatever karvex thought it was
    /// watching, the owner says it is done.
    TaskCompleted,
    /// The task is not `in_progress` — nobody has started it, or its status is
    /// a word this karvex does not know. Watching a pending task would nudge a
    /// teammate about work it has not been given.
    NotInProgress { status: String },
}

impl SkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::RunClosed => "run_closed",
            Self::TaskCompleted => "task_completed",
            Self::NotInProgress { .. } => "not_in_progress",
        }
    }
}

/// One sample's conclusion about one watched node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchdogDecision {
    pub class: ProgressClass,
    /// The streak *including* this sample, which is the number the thresholds
    /// are compared against and the number the journal records.
    pub streak: u32,
    pub action: WatchdogAction,
    /// What `run_node.attention` should hold after this sample. `None` clears a
    /// previous value — this is a re-evaluation every tick, not a latch. Never a
    /// `NodeStatus`: the status column is Claude Code's (D-10).
    pub attention: Option<Attention>,
}

impl WatchdogDecision {
    /// The `kind: "watchdog"` journal payload from §3.4.
    pub fn journal_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "class": self.class.as_str(),
            "rung": match &self.action {
                WatchdogAction::Say { rung, .. } => Some(rung.rung()),
                WatchdogAction::Surface => Some(Escalation::Surface.rung()),
                WatchdogAction::Hold => None,
            },
            "streak": self.streak,
            "attention": self.attention.map(Attention::as_str),
        })
    }

    /// The rung this decision spends, if it spends one.
    pub fn rung(&self) -> Option<Escalation> {
        match &self.action {
            WatchdogAction::Say { rung, .. } => Some(*rung),
            WatchdogAction::Surface => Some(Escalation::Surface),
            WatchdogAction::Hold => None,
        }
    }
}

/// Whether a node was watched at all, and what came of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogVerdict {
    /// Out of scope. Nothing is said, journalled, or written — including
    /// `attention`, which keeps whatever it held.
    NotWatched(SkipReason),
    Watched(WatchdogDecision),
}

impl WatchdogVerdict {
    pub fn decision(&self) -> Option<&WatchdogDecision> {
        match self {
            Self::Watched(decision) => Some(decision),
            Self::NotWatched(_) => None,
        }
    }
}

// ── the judgement ──────────────────────────────────────────────────────────

/// What this sample makes of the node, ignoring the ladder.
///
/// Reads the pane before the task status, deliberately: the status is the
/// target of the whole feature, not its instrument.
pub fn classify(node: &ObservedNode) -> ProgressClass {
    let pane = node.owner.pane();

    // 1. Movement, from the most trusted evidence down. A state change counts
    //    even when the new state is idle: something happened.
    if pane.is_some_and(|pane| pane.state == AgentState::Working) {
        return ProgressClass::Progressing;
    }
    if pane.is_some_and(|pane| pane.changed_since_last_sample) {
        return ProgressClass::Progressing;
    }
    if node.usage.is_some_and(UsageDelta::moved) {
        return ProgressClass::Progressing;
    }

    // 2. Somebody else has to move first. The lead's own block comes first
    //    because it is the run-wide answer and every member inherits it.
    if !node.is_lead
        && node
            .lead_pane
            .as_ref()
            .is_some_and(|pane| pane.state == AgentState::Blocked)
    {
        return ProgressClass::ExternalWait;
    }
    if pane.is_some_and(|pane| pane.state == AgentState::Blocked) {
        return ProgressClass::ExternalWait;
    }
    if !node.blocked_by.is_empty() {
        return ProgressClass::ExternalWait;
    }

    // 3. Is there anybody to talk to at all?
    match &node.owner {
        OwnerState::Unclaimed | OwnerState::Vanished { .. } | OwnerState::NoPane { .. } => {
            ProgressClass::Unknown
        }
        OwnerState::Observed { pane, .. } => match pane.state {
            AgentState::Idle => ProgressClass::LocalLoop,
            // A pane running no recognised agent is not a teammate thinking; it
            // is a session that exited or a shell. Nobody to nudge.
            AgentState::Unknown => ProgressClass::Unknown,
            // Handled above; listed so a new `AgentState` variant fails to
            // compile rather than silently classifying as a loop.
            AgentState::Working | AgentState::Blocked => ProgressClass::Progressing,
        },
    }
}

/// The rung this sample earns, if any.
///
/// `streak` is the post-increment streak — the count *including* this sample —
/// so a threshold of 3 fires on the third consecutive no-progress sample.
pub fn next_rung(
    class: ProgressClass,
    node: &ObservedNode,
    policy: &WatchdogPolicy,
    streak: u32,
) -> Option<Escalation> {
    // An exceeded budget is a fact about the node, not about the teammate's
    // attention, so it jumps the queue and lands on rung 4 directly. Nothing is
    // killed: karvex has nothing to kill any more, it just says so.
    if node.budget_exceeded() {
        return (node.ladder.rung != Some(Escalation::Surface)).then_some(Escalation::Surface);
    }
    if !class.counts_toward_streak() {
        return None;
    }
    // Rung 4 is the end of the ladder. The attention column keeps saying it.
    if node.ladder.rung == Some(Escalation::Surface) {
        return None;
    }
    // Two attempts karvex could not deliver are a finding of their own. Surface
    // now rather than retrying into a channel that is not there.
    if node.ladder.undelivered >= policy.undelivered_limit {
        return Some(Escalation::Surface);
    }

    let mut candidate = match node.ladder.rung {
        None => Escalation::Nudge,
        Some(Escalation::Nudge) => Escalation::Reprompt,
        Some(Escalation::Reprompt) => Escalation::EscalateToLead,
        Some(Escalation::EscalateToLead) | Some(Escalation::Surface) => Escalation::Surface,
    };
    // Nobody to nudge: skip straight past the two teammate-facing rungs.
    if class == ProgressClass::Unknown && candidate < Escalation::EscalateToLead {
        candidate = Escalation::EscalateToLead;
    }
    // The lead has nobody above it. Its ladder is nudge, re-prompt, surface.
    if node.is_lead && candidate == Escalation::EscalateToLead {
        candidate = Escalation::Surface;
    }

    // The first rung uses the stuck window; every later one uses the drift
    // window, which is the wider "the status has not caught up yet" allowance.
    let threshold = if node.ladder.rung.is_none() {
        policy.stuck_threshold
    } else {
        policy.drift_threshold
    };
    (streak >= threshold).then_some(candidate)
}

/// The whole judgement for one node, one sample.
pub fn decide(node: &ObservedNode, policy: &WatchdogPolicy) -> WatchdogVerdict {
    if !policy.enabled {
        return WatchdogVerdict::NotWatched(SkipReason::Disabled);
    }
    if node.run_closed {
        return WatchdogVerdict::NotWatched(SkipReason::RunClosed);
    }
    if !node.is_lead {
        match &node.status {
            TaskStatus::Completed => return WatchdogVerdict::NotWatched(SkipReason::TaskCompleted),
            TaskStatus::InProgress => {}
            other => {
                return WatchdogVerdict::NotWatched(SkipReason::NotInProgress {
                    status: other.as_str().to_string(),
                })
            }
        }
    }

    let class = classify(node);
    let streak = if class.counts_toward_streak() {
        node.ladder.streak.saturating_add(1)
    } else if class == ProgressClass::Progressing {
        0
    } else {
        node.ladder.streak
    };

    let rung = next_rung(class, node, policy, streak);
    let action = match rung {
        None => WatchdogAction::Hold,
        Some(Escalation::Surface) => WatchdogAction::Surface,
        Some(rung @ (Escalation::Nudge | Escalation::Reprompt)) => {
            // Only a node with a named, observed owner ever reaches these two
            // rungs — `Unknown` jumped past them — but the target is built from
            // the name rather than assumed, so an owner-less node holds instead
            // of addressing a message to nobody.
            match node.owner.name() {
                Some(name) => WatchdogAction::Say {
                    rung,
                    target: MessageTarget::Member {
                        name: name.to_string(),
                    },
                    text: if rung == Escalation::Nudge {
                        nudge_text(node, policy)
                    } else {
                        reprompt_text(node, policy)
                    },
                },
                None => WatchdogAction::Hold,
            }
        }
        Some(Escalation::EscalateToLead) => WatchdogAction::Say {
            rung: Escalation::EscalateToLead,
            target: MessageTarget::Lead,
            text: lead_escalation_text(node, policy),
        },
    };

    WatchdogDecision {
        class,
        streak,
        action,
        attention: attention_for(node, class, rung),
    }
    .into()
}

impl From<WatchdogDecision> for WatchdogVerdict {
    fn from(decision: WatchdogDecision) -> Self {
        Self::Watched(decision)
    }
}

/// What `run_node.attention` should say after this sample.
///
/// Karvex's own opinion, in karvex's own column. It never becomes a
/// `NodeStatus`, and a node that starts moving again clears it.
fn attention_for(
    node: &ObservedNode,
    class: ProgressClass,
    rung: Option<Escalation>,
) -> Option<Attention> {
    if node.budget_exceeded() {
        return Some(Attention::BudgetExceeded);
    }
    match class {
        ProgressClass::Progressing => None,
        ProgressClass::ExternalWait => Some(external_wait_attention(node)),
        // The ladder is walking but has not finished: the node is not yet
        // "stuck", it is being talked to. Surfacing early would put a red mark
        // on every teammate that paused to think for a minute.
        ProgressClass::LocalLoop | ProgressClass::Unknown => {
            let surfaced =
                rung == Some(Escalation::Surface) || node.ladder.rung == Some(Escalation::Surface);
            if !surfaced {
                return None;
            }
            Some(match class {
                ProgressClass::Unknown => Attention::Unbound,
                _ => Attention::Stuck,
            })
        }
    }
}

fn external_wait_attention(node: &ObservedNode) -> Attention {
    if node.is_lead {
        return Attention::LeadBlocked;
    }
    let lead_blocked = node
        .lead_pane
        .as_ref()
        .is_some_and(|pane| pane.state == AgentState::Blocked);
    if lead_blocked {
        Attention::LeadBlocked
    } else {
        Attention::NeedsInput
    }
}

impl ObservedNode {
    /// Whether the node has outlived its authored `timeout_ms`.
    pub fn budget_exceeded(&self) -> bool {
        self.budget_ms
            .is_some_and(|budget| budget > 0 && self.elapsed_ms > budget)
    }
}

// ── the sentences ──────────────────────────────────────────────────────────

/// Rung 1, to the teammate: name the task, the status, and the measured idle
/// time, then ask for one of three concrete acts.
///
/// Deliberately specific. "Please continue" is noise an agent learns to ignore;
/// "task X is in_progress and your pane has been idle for 14 minutes" is a fact
/// it can act on or contradict.
pub fn nudge_text(node: &ObservedNode, policy: &WatchdogPolicy) -> String {
    let mut out = String::from(WATCHDOG_FRAME);
    out.push('\n');
    out.push_str(&format!(
        "{} and {}.\n",
        subject_clause(node),
        idle_clause(node, policy)
    ));
    out.push_str(
        "karvex is the terminal runtime around your session, not your human operator, and it \
         cannot edit your task file or answer for you.\n",
    );
    out.push_str("Do one of these now, in this session:\n");
    if node.is_lead {
        out.push_str("- still leading: say what the next concrete step is, then take it;\n");
        out.push_str(
            "- the run is finished: write the run summary and call `kvx workflow run finish`;\n",
        );
        out.push_str("- blocked on a human: say what you need, so it is visible in this pane.\n");
    } else {
        out.push_str("- still working: name the next concrete step, then take it;\n");
        out.push_str(&format!(
            "- finished: mark {} completed, then tell the {LEAD_ROLE_WORD} what landed;\n",
            task_noun(node)
        ));
        out.push_str(&format!(
            "- blocked: tell the {LEAD_ROLE_WORD} what is blocking you and what would clear it.\n"
        ));
    }
    out
}

/// Rung 2, to the teammate: name the disagreement itself.
///
/// This is the honest replacement for Phase 3's unfilled-schema re-prompt, and
/// it targets the failure upstream documents: a teammate that finished but never
/// marked the task completed looks exactly like one that stopped.
pub fn reprompt_text(node: &ObservedNode, policy: &WatchdogPolicy) -> String {
    let mut out = String::from(WATCHDOG_FRAME);
    out.push('\n');
    out.push_str(&format!(
        "Second notice, and nothing has moved. {} and {}.\n",
        subject_clause(node),
        idle_clause(node, policy)
    ));
    if node.is_lead {
        out.push_str(
            "A run whose lead has stopped is a run nobody will finish: karvex cannot close it for \
             you.\n",
        );
        out.push_str("Do exactly one of these, now:\n");
        out.push_str("- the run is done: write the summary and call `kvx workflow run finish`;\n");
        out.push_str(
            "- teammates are still working: say which task you are waiting on and check it;\n",
        );
        out.push_str("- you are blocked: say what you need in this pane.\n");
        out.push_str(
            "If this pane is still idle at the next check, karvex marks this run as needing \
             attention.\n",
        );
        return out;
    }
    out.push_str(
        "The task status and the pane disagree. Claude Code's own docs note that teammates \
         sometimes finish work and never mark the task completed, so karvex trusts the pane and \
         asks you to settle it.\n",
    );
    out.push_str("Do exactly one of these, now:\n");
    out.push_str(&format!(
        "- the work is done: mark {} completed, then send the {LEAD_ROLE_WORD} one line saying \
         what landed;\n",
        task_noun(node)
    ));
    out.push_str(&format!(
        "- you are blocked: send the {LEAD_ROLE_WORD} the blocker and what would clear it;\n"
    ));
    out.push_str("- you are still working: say what you are doing and take the next step.\n");
    out.push_str(&format!(
        "If this pane is still idle at the next check, karvex tells the {LEAD_ROLE_WORD} instead \
         of you.\n"
    ));
    out
}

/// Rung 3, to the lead: the measurements, and the two remedies upstream itself
/// prescribes.
///
/// Karvex cannot restart, reassign, or respawn anybody — that capability left
/// with the execution engine. The lead can do all three, so the rung that used
/// to be "restart from a checkpoint" is now "ask the only actor that can".
pub fn lead_escalation_text(node: &ObservedNode, policy: &WatchdogPolicy) -> String {
    let mut out = String::from(WATCHDOG_FRAME);
    out.push('\n');
    match node.owner.name() {
        Some(name) => out.push_str(&format!(
            "Teammate \"{name}\" is not making progress, and karvex could not get it to answer.\n"
        )),
        None => out.push_str("A task in this run has no owner karvex can see.\n"),
    }
    out.push_str(&format!(
        "What karvex measured: {}; {}; {}.\n",
        subject_clause(node),
        visibility_clause(node, policy),
        intervention_clause(node)
    ));
    out.push_str(
        "karvex cannot restart, reassign, or respawn a teammate — it only watches panes and \
         reports. You can do all three.\n",
    );
    out.push_str("Your options:\n");
    match node.owner.name() {
        Some(name) => {
            out.push_str(&format!(
                "- message {name} yourself and ask for a status;\n"
            ));
            out.push_str("- reassign this task to another teammate;\n");
            out.push_str(&format!("- respawn {name} and hand it the task again;\n"));
        }
        None => {
            out.push_str("- assign this task to a teammate that is running;\n");
            out.push_str("- take the work yourself;\n");
        }
    }
    out.push_str("- or, if this wait is expected, keep going and ignore this.\n");
    out.push_str(
        "This message is from the karvex runtime, not from your human operator, and it cannot be \
         replied to — act in your own session.\n",
    );
    out
}

// ── phrasing helpers ───────────────────────────────────────────────────────

/// `Task "wire the poller" (id 7c1a2b) is still marked in_progress`, or the
/// lead's equivalent.
fn subject_clause(node: &ObservedNode) -> String {
    if node.is_lead {
        return format!(
            "The run you are leading has been open for {}",
            human_ms(node.elapsed_ms)
        );
    }
    let mut clause = format!("Task \"{}\"", node.subject);
    if let Some(id) = &node.task_id {
        clause.push_str(&format!(" (id {id})"));
    }
    clause.push_str(&format!(
        " is still marked {} after {}",
        node.status.as_str(),
        human_ms(node.elapsed_ms)
    ));
    clause
}

/// How the teammate's own task is referred to in an instruction.
fn task_noun(node: &ObservedNode) -> String {
    match &node.task_id {
        Some(id) => format!("task {id}"),
        None => format!("task \"{}\"", node.subject),
    }
}

/// `karvex has watched your pane %3 sit idle for 14 minutes (3 samples, one
/// every 20s)`.
fn idle_clause(node: &ObservedNode, policy: &WatchdogPolicy) -> String {
    let samples = node.ladder.streak.saturating_add(1);
    match node.owner.pane() {
        Some(pane) => format!(
            "karvex has watched your pane {} sit {} for {} ({} consecutive samples, one every {}s)",
            pane.pane_id,
            state_word(pane.state),
            human_ms(pane.state_age_ms),
            samples,
            policy.tick_secs
        ),
        None => format!(
            "karvex has no pane to watch for it ({samples} consecutive samples, one every {}s)",
            policy.tick_secs
        ),
    }
}

/// The evidence sentence in the lead escalation: what karvex can and cannot
/// see about the owner.
fn visibility_clause(node: &ObservedNode, policy: &WatchdogPolicy) -> String {
    let samples = node.ladder.streak.saturating_add(1);
    match &node.owner {
        OwnerState::Observed { pane, .. } => format!(
            "its pane {} has been {} for {} ({samples} consecutive samples, one every {}s)",
            pane.pane_id,
            state_word(pane.state),
            human_ms(pane.state_age_ms),
            policy.tick_secs
        ),
        OwnerState::NoPane { name } => format!(
            "{name} has no pane karvex can watch, so karvex cannot tell whether it is working"
        ),
        OwnerState::Vanished { name } => {
            format!("{name} is no longer in the team roster, so nobody is working this task")
        }
        OwnerState::Unclaimed => {
            "the task file names no owner, so no teammate has claimed it".to_string()
        }
    }
}

/// What karvex already tried, so the lead is not told to repeat it.
fn intervention_clause(node: &ObservedNode) -> String {
    match node.ladder.rung {
        None => "karvex has sent it nothing (there was nobody to send it to)".to_string(),
        Some(Escalation::Nudge) => "karvex sent it one nudge and the pane did not move".to_string(),
        Some(Escalation::Reprompt) | Some(Escalation::EscalateToLead) => {
            "karvex sent it a nudge and a structured re-prompt and the pane did not move"
                .to_string()
        }
        Some(Escalation::Surface) => "karvex has stopped messaging it".to_string(),
    }
}

fn state_word(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "idle",
        AgentState::Working => "working",
        AgentState::Blocked => "waiting for input",
        AgentState::Unknown => "showing no recognised agent",
    }
}

/// Durations an agent can act on: seconds under a minute, whole minutes under
/// an hour, hours and minutes above it. Never a bare millisecond count.
fn human_ms(ms: u64) -> String {
    let seconds = ms / 1_000;
    if seconds < 60 {
        return plural(seconds, "second");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return plural(minutes, "minute");
    }
    let hours = minutes / 60;
    let rest = minutes % 60;
    if rest == 0 {
        return plural(hours, "hour");
    }
    format!("{} {}", plural(hours, "hour"), plural(rest, "minute"))
}

fn plural(value: u64, unit: &str) -> String {
    if value == 1 {
        format!("1 {unit}")
    } else {
        format!("{value} {unit}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::binding::messaging::Priority;

    /// The default policy, with the published defaults from `[workflow]`.
    fn policy() -> WatchdogPolicy {
        WatchdogPolicy::from_config(&crate::config::Config::default())
    }

    fn pane(state: AgentState, age_ms: u64) -> PaneObservation {
        PaneObservation {
            pane_id: "w1:p2".to_string(),
            state,
            state_age_ms: age_ms,
            changed_since_last_sample: false,
        }
    }

    /// A teammate-owned task that is `in_progress` with an idle pane: the
    /// canonical stuck case every other case is a variation of.
    fn node() -> ObservedNode {
        ObservedNode {
            path: InstancePath("implement".to_string()),
            task_id: Some("7c1a2b".to_string()),
            subject: "wire the projection poller".to_string(),
            status: TaskStatus::InProgress,
            is_lead: false,
            owner: OwnerState::Observed {
                name: "backend".to_string(),
                pane: pane(AgentState::Idle, 14 * 60_000),
            },
            lead_pane: Some(pane(AgentState::Working, 3_000)),
            blocked_by: Vec::new(),
            budget_ms: None,
            elapsed_ms: 41 * 60_000,
            usage: None,
            ladder: LadderState::default(),
            run_closed: false,
        }
    }

    fn with_streak(streak: u32, rung: Option<Escalation>) -> ObservedNode {
        ObservedNode {
            ladder: LadderState {
                streak,
                rung,
                undelivered: 0,
            },
            ..node()
        }
    }

    fn say(verdict: &WatchdogVerdict) -> Option<(Escalation, MessageTarget, String)> {
        match &verdict.decision()?.action {
            WatchdogAction::Say { rung, target, text } => {
                Some((*rung, target.clone(), text.clone()))
            }
            _ => None,
        }
    }

    // ── the two published knobs ────────────────────────────────────────────

    #[test]
    fn the_policy_reads_the_two_published_dead_knobs_and_their_defaults() {
        let policy = policy();
        assert!(policy.enabled);
        assert_eq!(policy.stuck_threshold, 3, "workflow.stuck_threshold");
        assert_eq!(policy.drift_threshold, 5, "workflow.drift_threshold");
        assert_eq!(policy.tick_secs, 20);
        assert_eq!(policy.undelivered_limit, 2);
    }

    // ── scope: what is never classified at all ─────────────────────────────

    #[test]
    fn a_disabled_watchdog_a_closed_run_and_a_completed_task_are_never_classified() {
        let cases: &[(&str, ObservedNode, WatchdogPolicy, SkipReason)] = &[
            (
                "kill switch",
                with_streak(9, None),
                WatchdogPolicy {
                    enabled: false,
                    ..policy()
                },
                SkipReason::Disabled,
            ),
            (
                "closed run",
                ObservedNode {
                    run_closed: true,
                    ..with_streak(9, None)
                },
                policy(),
                SkipReason::RunClosed,
            ),
            (
                "completed task",
                ObservedNode {
                    status: TaskStatus::Completed,
                    ..with_streak(9, None)
                },
                policy(),
                SkipReason::TaskCompleted,
            ),
            (
                "pending task",
                ObservedNode {
                    status: TaskStatus::Pending,
                    ..with_streak(9, None)
                },
                policy(),
                SkipReason::NotInProgress {
                    status: "pending".to_string(),
                },
            ),
            (
                "a status this karvex does not know",
                ObservedNode {
                    status: TaskStatus::Unknown("deferred".to_string()),
                    ..with_streak(9, None)
                },
                policy(),
                SkipReason::NotInProgress {
                    status: "deferred".to_string(),
                },
            ),
        ];
        for (label, node, policy, expected) in cases {
            assert_eq!(
                decide(node, policy),
                WatchdogVerdict::NotWatched(expected.clone()),
                "{label}"
            );
        }
    }

    #[test]
    fn the_lead_is_watched_without_a_task_status() {
        // The `.lead` node has no task file, so its status is `Unknown`; the
        // status gate must not skip it.
        let lead = ObservedNode {
            path: InstancePath(".lead".to_string()),
            task_id: None,
            is_lead: true,
            status: TaskStatus::Unknown(String::new()),
            owner: OwnerState::Observed {
                name: "team-lead".to_string(),
                pane: pane(AgentState::Idle, 9 * 60_000),
            },
            lead_pane: None,
            ladder: LadderState {
                streak: 2,
                rung: None,
                undelivered: 0,
            },
            ..node()
        };
        let verdict = decide(&lead, &policy());
        let (rung, target, _) = say(&verdict).expect("the lead is nudged like anyone else");
        assert_eq!(rung, Escalation::Nudge);
        assert_eq!(
            target,
            MessageTarget::Member {
                name: "team-lead".into()
            }
        );
    }

    // ── classification ─────────────────────────────────────────────────────

    #[test]
    fn classification_reads_the_pane_before_the_task_status() {
        let blocker = BlockingTask {
            id: "b1".to_string(),
            subject: "land the migration".to_string(),
        };
        let cases: &[(&str, ObservedNode, ProgressClass)] = &[
            (
                "a working pane is progress no matter what the status says",
                ObservedNode {
                    owner: OwnerState::Observed {
                        name: "backend".into(),
                        pane: pane(AgentState::Working, 1_000),
                    },
                    ..node()
                },
                ProgressClass::Progressing,
            ),
            (
                "a state change since the last sample is movement, even into idle",
                ObservedNode {
                    owner: OwnerState::Observed {
                        name: "backend".into(),
                        pane: PaneObservation {
                            changed_since_last_sample: true,
                            ..pane(AgentState::Idle, 1_000)
                        },
                    },
                    ..node()
                },
                ProgressClass::Progressing,
            ),
            (
                "a usage delta is movement karvex cannot see on the screen",
                ObservedNode {
                    usage: Some(UsageDelta {
                        tool_uses: 2,
                        tokens: 0,
                    }),
                    ..node()
                },
                ProgressClass::Progressing,
            ),
            (
                "a zero usage delta is not movement",
                ObservedNode {
                    usage: Some(UsageDelta::default()),
                    ..node()
                },
                ProgressClass::LocalLoop,
            ),
            (
                "a pane waiting for input needs a human, not a nudge",
                ObservedNode {
                    owner: OwnerState::Observed {
                        name: "backend".into(),
                        pane: pane(AgentState::Blocked, 60_000),
                    },
                    ..node()
                },
                ProgressClass::ExternalWait,
            ),
            (
                "a blocked lead blocks every member under it",
                ObservedNode {
                    lead_pane: Some(pane(AgentState::Blocked, 60_000)),
                    ..node()
                },
                ProgressClass::ExternalWait,
            ),
            (
                "an unfinished blockedBy task is an external wait",
                ObservedNode {
                    blocked_by: vec![blocker.clone()],
                    ..node()
                },
                ProgressClass::ExternalWait,
            ),
            (
                "an idle pane while the task says in_progress is the stuck case",
                node(),
                ProgressClass::LocalLoop,
            ),
            (
                "an unclaimed task has nobody to nudge",
                ObservedNode {
                    owner: OwnerState::Unclaimed,
                    ..node()
                },
                ProgressClass::Unknown,
            ),
            (
                "an owner that left the roster has nobody to nudge",
                ObservedNode {
                    owner: OwnerState::Vanished {
                        name: "backend".into(),
                    },
                    ..node()
                },
                ProgressClass::Unknown,
            ),
            (
                "an owner with no pane cannot be watched",
                ObservedNode {
                    owner: OwnerState::NoPane {
                        name: "backend".into(),
                    },
                    ..node()
                },
                ProgressClass::Unknown,
            ),
            (
                "a pane running no recognised agent is a session that left",
                ObservedNode {
                    owner: OwnerState::Observed {
                        name: "backend".into(),
                        pane: pane(AgentState::Unknown, 60_000),
                    },
                    ..node()
                },
                ProgressClass::Unknown,
            ),
        ];
        for (label, node, expected) in cases {
            assert_eq!(classify(node), *expected, "{label}");
        }
    }

    // ── streak arithmetic and the thresholds ───────────────────────────────

    #[test]
    fn the_first_rung_fires_at_exactly_stuck_threshold() {
        let policy = policy();
        for prior in 0..policy.stuck_threshold + 2 {
            let verdict = decide(&with_streak(prior, None), &policy);
            let decision = verdict.decision().expect("watched");
            assert_eq!(decision.streak, prior + 1, "streak counts this sample");
            let fired = decision.rung().is_some();
            assert_eq!(
                fired,
                prior + 1 >= policy.stuck_threshold,
                "prior streak {prior} against stuck_threshold {}",
                policy.stuck_threshold
            );
        }
    }

    #[test]
    fn every_later_rung_fires_at_exactly_drift_threshold() {
        let policy = policy();
        for rung in [
            Escalation::Nudge,
            Escalation::Reprompt,
            Escalation::EscalateToLead,
        ] {
            for prior in 0..policy.drift_threshold + 2 {
                let verdict = decide(&with_streak(prior, Some(rung)), &policy);
                let fired = verdict.decision().expect("watched").rung().is_some();
                assert_eq!(
                    fired,
                    prior + 1 >= policy.drift_threshold,
                    "after {rung:?}, prior streak {prior}"
                );
            }
        }
    }

    #[test]
    fn external_wait_and_progress_never_earn_a_rung_however_long_they_last() {
        let policy = policy();
        let cases = [
            (
                "a pane waiting for input",
                ObservedNode {
                    owner: OwnerState::Observed {
                        name: "backend".into(),
                        pane: pane(AgentState::Blocked, 3 * 3_600_000),
                    },
                    ..with_streak(50, None)
                },
            ),
            (
                "a task blocked by an unfinished task",
                ObservedNode {
                    blocked_by: vec![BlockingTask {
                        id: "b1".into(),
                        subject: "land the migration".into(),
                    }],
                    ..with_streak(50, None)
                },
            ),
            (
                "a working pane",
                ObservedNode {
                    owner: OwnerState::Observed {
                        name: "backend".into(),
                        pane: pane(AgentState::Working, 3 * 3_600_000),
                    },
                    ..with_streak(50, None)
                },
            ),
        ];
        for (label, node) in cases {
            let verdict = decide(&node, &policy);
            let decision = verdict.decision().expect("watched");
            assert_eq!(decision.action, WatchdogAction::Hold, "{label}");
            assert_eq!(decision.rung(), None, "{label}");
        }
    }

    #[test]
    fn an_external_wait_holds_the_streak_and_progress_clears_the_whole_ladder() {
        let policy = policy();
        let waiting = ObservedNode {
            owner: OwnerState::Observed {
                name: "backend".into(),
                pane: pane(AgentState::Blocked, 60_000),
            },
            ..with_streak(4, Some(Escalation::Nudge))
        };
        let verdict = decide(&waiting, &policy);
        let decision = verdict.decision().expect("watched");
        assert_eq!(decision.streak, 4, "a wait neither counts nor clears");
        assert_eq!(
            waiting.ladder.after(decision, DeliveryOutcome::Delivered),
            LadderState {
                streak: 4,
                rung: Some(Escalation::Nudge),
                undelivered: 0,
            }
        );

        let working = ObservedNode {
            owner: OwnerState::Observed {
                name: "backend".into(),
                pane: pane(AgentState::Working, 1_000),
            },
            ..with_streak(4, Some(Escalation::Reprompt))
        };
        let verdict = decide(&working, &policy);
        let decision = verdict.decision().expect("watched");
        assert_eq!(decision.class, ProgressClass::Progressing);
        assert_eq!(decision.streak, 0);
        assert_eq!(decision.attention, None, "movement clears karvex's opinion");
        assert_eq!(
            working.ladder.after(decision, DeliveryOutcome::Delivered),
            LadderState::default(),
            "a working pane resets the ladder, not just the streak"
        );
    }

    // ── the ladder ─────────────────────────────────────────────────────────

    /// Walks a node forward one sample at a time and records every rung that
    /// was spent, which is the only honest way to test "never skips".
    fn walk(
        mut node: ObservedNode,
        policy: &WatchdogPolicy,
        samples: usize,
        outcome: DeliveryOutcome,
    ) -> Vec<Escalation> {
        let mut spent = Vec::new();
        for _ in 0..samples {
            let verdict = decide(&node, policy);
            let Some(decision) = verdict.decision() else {
                break;
            };
            if let Some(rung) = decision.rung() {
                // An undelivered *message* is not spent; a surface has no
                // message and always lands.
                if outcome == DeliveryOutcome::Delivered || rung == Escalation::Surface {
                    spent.push(rung);
                }
            }
            node.ladder = node.ladder.after(decision, outcome);
        }
        spent
    }

    #[test]
    fn the_ladder_walks_one_to_four_in_order_and_never_skips() {
        let policy = policy();
        let spent = walk(node(), &policy, 40, DeliveryOutcome::Delivered);
        assert_eq!(
            spent,
            vec![
                Escalation::Nudge,
                Escalation::Reprompt,
                Escalation::EscalateToLead,
                Escalation::Surface,
            ],
            "every rung once, in order, and then silence"
        );
    }

    #[test]
    fn an_exceeded_budget_jumps_straight_to_rung_four() {
        let policy = policy();
        let over = ObservedNode {
            budget_ms: Some(30 * 60_000),
            elapsed_ms: 41 * 60_000,
            ..node()
        };
        let verdict = decide(&over, &policy);
        let decision = verdict.decision().expect("watched");
        assert_eq!(decision.action, WatchdogAction::Surface);
        assert_eq!(decision.attention, Some(Attention::BudgetExceeded));
        assert_eq!(
            walk(over, &policy, 20, DeliveryOutcome::Delivered),
            vec![Escalation::Surface],
            "no teammate is nudged about a budget it cannot give back, and it surfaces once"
        );

        let inside = ObservedNode {
            budget_ms: Some(60 * 60_000),
            elapsed_ms: 41 * 60_000,
            ..node()
        };
        assert!(!inside.budget_exceeded());
        assert_eq!(
            decide(&inside, &policy)
                .decision()
                .expect("watched")
                .attention,
            None
        );
    }

    #[test]
    fn an_unknown_class_skips_the_two_teammate_rungs_and_goes_to_the_lead() {
        let policy = policy();
        let orphan = ObservedNode {
            owner: OwnerState::Vanished {
                name: "backend".into(),
            },
            ..node()
        };
        assert_eq!(
            walk(orphan.clone(), &policy, 40, DeliveryOutcome::Delivered),
            vec![Escalation::EscalateToLead, Escalation::Surface],
            "there is nobody to nudge, so rungs 1 and 2 are not spent on nobody"
        );
        let pending = ObservedNode {
            ladder: LadderState {
                streak: policy.stuck_threshold - 1,
                rung: None,
                undelivered: 0,
            },
            ..orphan
        };
        let verdict = decide(&pending, &policy);
        let (rung, target, text) = say(&verdict).expect("the lead is told");
        assert_eq!(rung, Escalation::EscalateToLead);
        assert_eq!(target, MessageTarget::Lead);
        assert!(text.contains("no longer in the team roster"), "{text}");
    }

    #[test]
    fn the_lead_has_nobody_above_it_so_its_ladder_ends_at_surface() {
        let policy = policy();
        let lead = ObservedNode {
            path: InstancePath(".lead".to_string()),
            task_id: None,
            is_lead: true,
            status: TaskStatus::Unknown(String::new()),
            owner: OwnerState::Observed {
                name: "team-lead".into(),
                pane: pane(AgentState::Idle, 20 * 60_000),
            },
            lead_pane: None,
            ..node()
        };
        assert_eq!(
            walk(lead, &policy, 40, DeliveryOutcome::Delivered),
            vec![Escalation::Nudge, Escalation::Reprompt, Escalation::Surface,],
            "escalating the lead to the lead would be a message to itself"
        );
    }

    #[test]
    fn a_delivered_intervention_resets_the_streak_so_it_gets_its_window() {
        let policy = policy();
        let node = with_streak(policy.stuck_threshold - 1, None);
        let verdict = decide(&node, &policy);
        let decision = verdict.decision().expect("watched");
        assert_eq!(decision.rung(), Some(Escalation::Nudge));
        let after = node.ladder.after(decision, DeliveryOutcome::Delivered);
        assert_eq!(
            after,
            LadderState {
                streak: 0,
                rung: Some(Escalation::Nudge),
                undelivered: 0,
            }
        );
    }

    // ── delivery honesty ───────────────────────────────────────────────────

    #[test]
    fn an_undelivered_rung_is_not_consumed_and_is_retried_verbatim() {
        let policy = policy();
        let node = with_streak(policy.stuck_threshold - 1, None);
        let verdict = decide(&node, &policy);
        let decision = verdict.decision().expect("watched");
        let after = node.ladder.after(decision, DeliveryOutcome::Undelivered);
        assert_eq!(
            after,
            LadderState {
                streak: policy.stuck_threshold,
                rung: None,
                undelivered: 1,
            },
            "the rung is not spent and the streak that earned it is kept"
        );

        // The very next sample tries the same rung again, not the next one.
        let retry = ObservedNode {
            ladder: after,
            ..node
        };
        let verdict = decide(&retry, &policy);
        assert_eq!(
            verdict.decision().expect("watched").rung(),
            Some(Escalation::Nudge),
            "a nudge nobody received must never look like a nudge that was ignored"
        );
    }

    #[test]
    fn two_undelivered_attempts_stop_talking_and_surface() {
        let policy = policy();
        let unreachable = ObservedNode {
            ladder: LadderState {
                streak: 1,
                rung: None,
                undelivered: policy.undelivered_limit,
            },
            ..node()
        };
        let verdict = decide(&unreachable, &policy);
        let decision = verdict.decision().expect("watched");
        assert_eq!(
            decision.action,
            WatchdogAction::Surface,
            "karvex cannot reach this session; that is the finding"
        );
        assert_eq!(decision.attention, Some(Attention::Stuck));
        assert_eq!(
            walk(node(), &policy, 40, DeliveryOutcome::Undelivered),
            vec![Escalation::Surface],
            "an unreachable session never spends a message rung"
        );
    }

    // ── attention: karvex's own column, never the projected status ─────────

    #[test]
    fn attention_is_karvexs_opinion_and_is_only_set_where_it_is_earned() {
        let policy = policy();
        let cases: &[(&str, ObservedNode, Option<Attention>)] = &[
            ("a walking ladder is not yet stuck", node(), None),
            (
                "a surfaced local loop is stuck",
                with_streak(50, Some(Escalation::EscalateToLead)),
                Some(Attention::Stuck),
            ),
            (
                "a surfaced unknown owner is unbound, not stuck",
                ObservedNode {
                    owner: OwnerState::NoPane {
                        name: "backend".into(),
                    },
                    ..with_streak(50, Some(Escalation::EscalateToLead))
                },
                Some(Attention::Unbound),
            ),
            (
                "a pane waiting for input needs input",
                ObservedNode {
                    owner: OwnerState::Observed {
                        name: "backend".into(),
                        pane: pane(AgentState::Blocked, 60_000),
                    },
                    ..node()
                },
                Some(Attention::NeedsInput),
            ),
            (
                "a blocked lead is inherited by name",
                ObservedNode {
                    lead_pane: Some(pane(AgentState::Blocked, 60_000)),
                    ..node()
                },
                Some(Attention::LeadBlocked),
            ),
        ];
        for (label, node, expected) in cases {
            assert_eq!(
                decide(node, &policy).decision().expect("watched").attention,
                *expected,
                "{label}"
            );
        }
    }

    #[test]
    fn a_verdict_never_carries_a_projected_task_status() {
        // D-10 in executable form: whatever the watchdog concludes, the only
        // status in the decision is the one it read, and it comes back
        // unchanged on the node it was given.
        let policy = policy();
        let node = with_streak(50, Some(Escalation::EscalateToLead));
        let verdict = decide(&node, &policy);
        assert_eq!(node.status, TaskStatus::InProgress);
        let payload = verdict.decision().expect("watched").journal_payload();
        assert_eq!(payload["class"], "local_loop");
        assert_eq!(payload["rung"], 4);
        assert_eq!(payload["attention"], "stuck");
        assert!(
            payload.get("status").is_none(),
            "the watchdog has no opinion to record about Claude Code's status column"
        );
    }

    // ── the text ───────────────────────────────────────────────────────────

    /// Every message any input can produce, for the properties that hold across
    /// all of them.
    fn every_message() -> Vec<(String, String)> {
        let policy = policy();
        let lead_node = ObservedNode {
            path: InstancePath(".lead".to_string()),
            task_id: None,
            is_lead: true,
            owner: OwnerState::Observed {
                name: "team-lead".into(),
                pane: pane(AgentState::Idle, 20 * 60_000),
            },
            lead_pane: None,
            ..node()
        };
        let owners = [
            OwnerState::Unclaimed,
            OwnerState::Vanished {
                name: "backend".into(),
            },
            OwnerState::NoPane {
                name: "backend".into(),
            },
            OwnerState::Observed {
                name: "backend".into(),
                pane: pane(AgentState::Idle, 14 * 60_000),
            },
        ];
        let mut out = Vec::new();
        for owner in owners {
            let node = ObservedNode { owner, ..node() };
            out.push(("nudge".to_string(), nudge_text(&node, &policy)));
            out.push(("reprompt".to_string(), reprompt_text(&node, &policy)));
            out.push((
                "escalation".to_string(),
                lead_escalation_text(&node, &policy),
            ));
        }
        out.push(("lead nudge".to_string(), nudge_text(&lead_node, &policy)));
        out.push((
            "lead reprompt".to_string(),
            reprompt_text(&lead_node, &policy),
        ));
        out
    }

    #[test]
    fn every_message_is_framed_and_names_the_runtime_rather_than_the_human() {
        for (label, text) in every_message() {
            assert!(
                text.starts_with(WATCHDOG_FRAME),
                "{label} is not framed: {text}"
            );
            assert!(
                text.contains("karvex"),
                "{label} does not say who is talking: {text}"
            );
            assert!(text.ends_with('\n'), "{label} should end with a newline");
        }
    }

    #[test]
    fn no_message_contains_a_relative_path() {
        // The `96674e80` rule: a `./` path means nothing to a session whose cwd
        // karvex does not control.
        for (label, text) in every_message() {
            assert!(
                !text.contains("./"),
                "{label} contains a relative path: {text}"
            );
            assert!(
                !text.contains("../"),
                "{label} contains a relative path: {text}"
            );
        }
    }

    #[test]
    fn no_message_is_a_content_free_please_continue() {
        for (label, text) in every_message() {
            let lowered = text.to_lowercase();
            assert!(
                !lowered.contains("please continue"),
                "{label} is noise: {text}"
            );
            assert!(
                text.len() > WATCHDOG_FRAME.len() + 120,
                "{label} is too thin to act on: {text}"
            );
        }
    }

    #[test]
    fn a_nudge_names_the_task_the_status_and_the_measured_idle_time() {
        let text = nudge_text(&with_streak(2, None), &policy());
        for needle in [
            "wire the projection poller",
            "id 7c1a2b",
            "in_progress",
            "w1:p2",
            "14 minutes",
            "3 consecutive samples",
            "one every 20s",
            "mark task 7c1a2b completed",
            "team lead",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    fn a_reprompt_names_the_disagreement_it_is_about() {
        let text = reprompt_text(&with_streak(4, Some(Escalation::Nudge)), &policy());
        assert!(text.contains("Second notice"), "{text}");
        assert!(text.contains("task status and the pane disagree"), "{text}");
        assert!(text.contains("mark task 7c1a2b completed"), "{text}");
        assert!(
            text.contains("karvex tells the team lead instead of you"),
            "the next rung is stated, so the ladder is legible: {text}"
        );
    }

    #[test]
    fn an_escalation_gives_the_lead_the_measurements_and_the_remedies_karvex_lacks() {
        let node = with_streak(4, Some(Escalation::Reprompt));
        let text = lead_escalation_text(&node, &policy());
        for needle in [
            "backend",
            "wire the projection poller",
            "id 7c1a2b",
            "w1:p2",
            "14 minutes",
            "41 minutes",
            "nudge and a structured re-prompt",
            "reassign this task to another teammate",
            "respawn backend",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        assert!(
            text.contains("cannot restart, reassign, or respawn"),
            "the one promise Phase 4 lost has to be said out loud: {text}"
        );
        assert!(
            !text.contains("checkpoint"),
            "karvex has nothing left to restart from: {text}"
        );
    }

    #[test]
    fn an_escalation_about_an_invisible_owner_says_what_karvex_cannot_see() {
        let policy = policy();
        let cases: &[(OwnerState, &str)] = &[
            (OwnerState::Unclaimed, "names no owner"),
            (
                OwnerState::Vanished {
                    name: "backend".into(),
                },
                "no longer in the team roster",
            ),
            (
                OwnerState::NoPane {
                    name: "backend".into(),
                },
                "no pane karvex can watch",
            ),
        ];
        for (owner, needle) in cases {
            let text = lead_escalation_text(
                &ObservedNode {
                    owner: owner.clone(),
                    ..node()
                },
                &policy,
            );
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    fn the_lead_is_told_the_verb_that_actually_closes_a_run() {
        let lead = ObservedNode {
            is_lead: true,
            task_id: None,
            owner: OwnerState::Observed {
                name: "team-lead".into(),
                pane: pane(AgentState::Idle, 9 * 60_000),
            },
            lead_pane: None,
            ..node()
        };
        let policy = policy();
        for text in [nudge_text(&lead, &policy), reprompt_text(&lead, &policy)] {
            assert!(
                text.contains("kvx workflow run finish"),
                "the lead's own completion contract: {text}"
            );
            assert!(
                !text.contains("in_progress"),
                "the lead has no task: {text}"
            );
        }
    }

    #[test]
    fn durations_are_stated_in_units_an_agent_can_act_on() {
        let cases: &[(u64, &str)] = &[
            (0, "0 seconds"),
            (1_000, "1 second"),
            (59_000, "59 seconds"),
            (60_000, "1 minute"),
            (14 * 60_000, "14 minutes"),
            (3_600_000, "1 hour"),
            (66 * 60_000, "1 hour 6 minutes"),
            (150 * 60_000, "2 hours 30 minutes"),
        ];
        for (ms, expected) in cases {
            assert_eq!(human_ms(*ms), *expected, "{ms}ms");
        }
    }

    // ── the seam to the delivery layer ─────────────────────────────────────

    #[test]
    fn every_rungs_priority_word_is_one_claude_codes_inbox_accepts() {
        for rung in [
            Escalation::Nudge,
            Escalation::Reprompt,
            Escalation::EscalateToLead,
            Escalation::Surface,
        ] {
            assert!(
                Priority::parse(rung.message_priority()).is_some(),
                "{rung:?} names a priority the messaging layer cannot send"
            );
        }
        assert_eq!(
            Escalation::EscalateToLead.message_priority(),
            "now",
            "the lead is the actor that can unblock the run"
        );
    }

    #[test]
    fn the_journal_payload_carries_the_three_fields_the_plan_names() {
        let policy = policy();
        let verdict = decide(&with_streak(2, None), &policy);
        let payload = verdict.decision().expect("watched").journal_payload();
        assert_eq!(payload["class"], "local_loop");
        assert_eq!(payload["rung"], 1);
        assert_eq!(payload["streak"], 3);
    }

    #[test]
    fn rung_numbers_and_words_are_stable() {
        assert_eq!(Escalation::Nudge.rung(), 1);
        assert_eq!(Escalation::Reprompt.rung(), 2);
        assert_eq!(Escalation::EscalateToLead.rung(), 3);
        assert_eq!(Escalation::Surface.rung(), 4);
        assert_eq!(Escalation::EscalateToLead.as_str(), "escalate_to_lead");
        assert_eq!(ProgressClass::LocalLoop.as_str(), "local_loop");
    }
}
