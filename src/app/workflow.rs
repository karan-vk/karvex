//! `App` glue for the workflow engine: the one place where a pure
//! [`Engine`] is pumped, its [`RunEffect`]s become runtime calls, and runtime
//! facts become [`EngineInput`]s
//! (`docs/design/workflow-builder/05-phase-plan.md` W4 step 3a,
//! `04-kvdag-and-execution.md` §2 and §9).
//!
//! [`WorkflowRuntimeState`] is pure data: it owns the engine, the identity of
//! the active run, the queue of durable writes the store task drains, and the
//! set of nodes admitted but not yet bound to a pane. Nothing in it touches a
//! PTY, the filesystem, or `tokio`, so every transition below is testable
//! without a runtime. The `App` half is the only part that dispatches.
//!
//! The engine runs inside the server's existing single-threaded event loop, so
//! there is no lock, no shared-mutable graph, and no second scheduler.

// The glue lands one step ahead of the API handlers (`src/app/api/workflows.rs`)
// and the spawn/observe bindings (`src/workflow/binding/*`) that call into it,
// so several entry points have no production caller yet. Remove once those
// steps land.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ratatui::layout::Direction;
use tracing::{debug, warn};

use crate::api::schema::{
    AgentPromptParams, AgentSendKeysParams, ErrorResponse, EventData, EventEnvelope, EventKind,
    Method, PaneSendKeysParams, PaneSendTextParams, WorkflowDemand, WorkflowEdgeKind,
    WorkflowEvidence, WorkflowGrowthLimit, WorkflowGrowthLimitKind, WorkflowNodeStatus,
    WorkflowRunEdgeInfo, WorkflowRunGraph, WorkflowRunInfo, WorkflowRunNodeInfo, WorkflowRunStatus,
    WorkflowSuccession, WorkflowTier,
};
use crate::app::state::{ToastKind, ToastNotification};
use crate::app::App;
use crate::events::WorkflowAppEvent;
use crate::workflow::binding::{interrogate, observe, spawn};
use crate::workflow::engine::expand::ExpandLimit;
use crate::workflow::engine::{DeliveryFailureNote, Engine, EngineConfig, EpilogueTaskSpec};
use crate::workflow::model::{
    is_reserved_path, Demand, EdgeKind, EdgePayload, EngineInput, Evidence, GrowthLimits,
    InstancePath, InterrogationId, Isolation, Kvdag, KvdagVersionId, NodeBinding, NodeStatus,
    NodeToken, NoticeLevel, OutputSchema, PublicPaneId, RunEffect, RunGraph, RunId, RunNode,
    RunNodeIdx, RunStatus, Runner, SpawnSpec, StoreWrite, Succession, UserNotice, WorkflowEvent,
    WorkflowId,
};
use crate::workflow::tier::Tier;

/// Engine clock cadence. `04-kvdag-and-execution.md` §6.3 states the watchdog's
/// `stuck_threshold = 3` ticks at a 20 s tick, so the tick length is part of the
/// documented thresholds rather than a free knob.
pub(crate) const WORKFLOW_TICK_INTERVAL: Duration = Duration::from_secs(20);

/// How many durable writes are held in memory before the oldest are dropped.
/// The in-memory run graph is authoritative during a run and the journal is the
/// durable record (`04` §9), so an undrained queue degrades persistence instead
/// of stalling or killing the run.
const PENDING_WRITE_BUDGET: usize = 4096;

/// How many times a node's pane spawn is retried before the failure is left on
/// the node instead of retried again. Matches the definition's own
/// `max_attempts` default, so a broken spawn cannot loop forever.
const SPAWN_ATTEMPT_BUDGET: u8 = 2;

/// Binds the end-of-run summariser to a command instead of `claude`, as a JSON
/// array of argv strings (`07-phase3-plan.md` §4 D2, §6 A4).
///
/// A **declared configuration**, not a test hook: it is the same first-class
/// command binding every fixture node already uses, and it is documented beside
/// `KARVEX_WORKFLOW_DB_PATH`. karvex reads it — unlike the `KARVEX_WORKFLOW_*`
/// variables in `binding::spawn`, which karvex *sets* on a node's process — and
/// reads it in exactly one place ([`engine_config`]).
pub(crate) const SUMMARY_COMMAND_ENV_VAR: &str = "KARVEX_WORKFLOW_SUMMARY_COMMAND";

/// Identity and wire-facing metadata of the run the engine is executing. The
/// engine's own events carry only ids, so this is what a `workflow.*` event
/// projection is built from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ActiveRun {
    pub(crate) run_id: RunId,
    pub(crate) workflow_id: WorkflowId,
    pub(crate) version_id: KvdagVersionId,
    pub(crate) tier: Tier,
    pub(crate) args: HashMap<String, String>,
    pub(crate) workspace_id: Option<String>,
    pub(crate) tab_id: Option<String>,
    pub(crate) started_at_unix_ms: u64,
    pub(crate) ended_at_unix_ms: Option<u64>,
    /// Past runs whose summaries this run was given (§4 D21). Recorded on the
    /// live projection as well as the row, so a client watching a run does not
    /// have to wait for it to close to see what history it started with.
    pub(crate) context_runs: Vec<RunId>,
    /// The run this one restored nodes from (§4 D4).
    pub(crate) restore_from_run: Option<RunId>,
    /// Absolute path of `context/prior-runs.md`, when this run was given one
    /// (§4 D21). Held on the run rather than on `AppState` because it is a fact
    /// about the run, not about what the TUI is showing — and because every
    /// node's spawn plan reads it, which is a runtime path, not a presentation
    /// one.
    pub(crate) prior_runs_path: Option<String>,
}

impl ActiveRun {
    pub(crate) fn new(
        run_id: RunId,
        workflow_id: WorkflowId,
        version_id: KvdagVersionId,
        tier: Tier,
    ) -> Self {
        Self {
            run_id,
            workflow_id,
            version_id,
            tier,
            args: HashMap::new(),
            workspace_id: None,
            tab_id: None,
            started_at_unix_ms: current_unix_ms(),
            ended_at_unix_ms: None,
            context_runs: Vec::new(),
            restore_from_run: None,
            prior_runs_path: None,
        }
    }

    pub(crate) fn with_args(mut self, args: HashMap<String, String>) -> Self {
        self.args = args;
        self
    }

    /// The two history facts recorded at run start (§4 D21, §4 D4). Both are
    /// decided before `create_run` and written to the row in the same call, so
    /// the live projection and the journal cannot disagree about them.
    pub(crate) fn with_history(
        mut self,
        context_runs: Vec<RunId>,
        restore_from_run: Option<RunId>,
    ) -> Self {
        self.context_runs = context_runs;
        self.restore_from_run = restore_from_run;
        self
    }

    pub(crate) fn with_prior_runs_path(mut self, prior_runs_path: Option<String>) -> Self {
        self.prior_runs_path = prior_runs_path;
        self
    }

    pub(crate) fn with_placement(
        mut self,
        workspace_id: Option<String>,
        tab_id: Option<String>,
    ) -> Self {
        self.workspace_id = workspace_id;
        self.tab_id = tab_id;
        self
    }
}

/// Why a run could not be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkflowStartError {
    /// Phase 1 executes one run at a time: the engine holds a single graph.
    RunInFlight,
}

impl WorkflowStartError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::RunInFlight => "workflow_run_in_flight",
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::RunInFlight => "another workflow run is still executing on this server",
        }
    }
}

/// The engine plus everything the runtime needs to keep it fed. Pure data:
/// construct it, drive it with [`EngineInput`]s, and read the effects back.
#[derive(Debug)]
pub(crate) struct WorkflowRuntimeState {
    config: EngineConfig,
    /// The app-enforced half of the `[workflow]` block (§4 D12, D21).
    policy: WorkflowPolicy,
    engine: Engine,
    run: Option<ActiveRun>,
    pending_writes: VecDeque<StoreWrite>,
    dropped_writes: u64,
    persistence_degraded: bool,
    /// Nodes the scheduler admitted that have no pane yet, in admission order.
    pending_spawns: Vec<InstancePath>,
    /// Pending spawns already handed to the binder, so a node is offered once
    /// per time it enters the ready set.
    claimed_spawns: HashSet<InstancePath>,
    spawn_failures: HashMap<InstancePath, u8>,
    /// The capability token minted for each spawned node. `NodeBinding` does not
    /// carry it, and it is what authenticates `workflow.node.report`.
    node_tokens: HashMap<InstancePath, NodeToken>,
    next_tick_at: Option<Instant>,
    /// The last pane delivery the runtime could not make. A control-plane call
    /// that answers `ok` for an interrupt or a steer whose keystrokes never
    /// reached the process is a lie about the system's state, so the API layer
    /// takes this after driving the engine and answers with the failure.
    delivery_failure: Option<DeliveryFailure>,
    /// The run status the user has already been told about. A run that
    /// succeeds, fails, is cancelled, or pauses is announced exactly once, and
    /// a run that never changes status is never announced again.
    announced_run_status: Option<RunStatus>,
    /// Nodes already announced as needing a human. Cleared per node when it
    /// leaves `NeedsAttention`, so a restarted node that gets stuck again is
    /// announced again rather than silently.
    announced_attention: HashSet<InstancePath>,
    /// The last growth guardrail each *proposing* node ran into, and the run's
    /// most recent one.
    ///
    /// A growth limit is a live fact with no `workflow_run`/`run_node` column
    /// to live in — the journal records it as a `growth_limited` event, which
    /// is an audit trail rather than a projection. Keeping it here is what
    /// makes §4 D11's guarantee ("a rejection is always surfaced") true on the
    /// API, the DAG overlay, and the CLI rather than only on the event stream,
    /// and it is the same shape as `Engine::delivery_failure`: state the run
    /// holds, mirrored into whatever is showing it.
    growth_limits: HashMap<InstancePath, WorkflowGrowthLimit>,
    /// The run-level view of the same fact: whichever limit was recorded last.
    last_growth_limit: Option<WorkflowGrowthLimit>,
    /// The run whose history has already been pruned, so retention fires once
    /// per run rather than on every tick a finished run still receives
    /// (§4 D12).
    pruned_history_for: Option<RunId>,
    /// Whether E-11's "summaries disabled" notice has already been shown. Once
    /// per server, not per run: the cause is a process-lifetime fact.
    summary_disabled_notified: bool,
    /// Interrogation panes this server is hosting (§4 D7-D8).
    ///
    /// Not cleared by [`Self::start`]: an interrogation belongs to the node it
    /// revived, not to the run this server happens to be executing, and closing
    /// one because an unrelated run started would strand its pane with no
    /// record able to stamp its end.
    interrogations: Vec<LiveInterrogation>,
}

/// A `RunEffect` delivery into a node's pane that the in-process API refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeliveryFailure {
    /// The API method that was attempted, e.g. `agent.send_keys`.
    pub(crate) method: String,
    pub(crate) pane: String,
    pub(crate) code: String,
    pub(crate) message: String,
}

impl DeliveryFailure {
    pub(crate) fn describe(&self) -> String {
        format!(
            "{} to pane {} failed: {}: {}",
            self.method, self.pane, self.code, self.message
        )
    }
}

/// Whether a buffered write brings a row into existence rather than updating
/// one. Creates are never evicted by queue overflow (§4 D7).
fn is_create_write(write: &StoreWrite) -> bool {
    matches!(
        write,
        StoreWrite::RunNodeCreated { .. }
            | StoreWrite::RunEdgeCreated { .. }
            // `07-phase3-plan.md` §3 rule 4: an interrogation's create is
            // addressed by an app-minted id that the later
            // `InterrogationUpdate` reuses. Evicting the create would leave
            // every update naming a row that does not exist — a permanent
            // failure, not one lost row.
            | StoreWrite::InterrogationStarted { .. }
    )
}

/// One interrogation pane this server is currently hosting
/// (`07-phase3-plan.md` §4 D7-D8).
///
/// Deliberately **not** a `RunNode` and deliberately not per-run state: an
/// interrogation of a run that finished last week can be open while a new run
/// executes, so [`WorkflowRuntimeState::start`] does not clear these the way it
/// clears everything keyed to the live run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LiveInterrogation {
    pub(crate) id: InterrogationId,
    /// The run the **source node** belongs to, which is not necessarily the run
    /// this server is executing.
    pub(crate) run: RunId,
    /// The source node's instance path.
    pub(crate) path: InstancePath,
    pub(crate) pane: PublicPaneId,
    pub(crate) source_session_id: String,
    /// Pre-assigned at spawn when `--session-id` combines with
    /// `--resume --fork-session` (the step-0 spike verified it does), and
    /// otherwise learned from the pane's session report (§6 A9).
    pub(crate) forked_session_id: Option<String>,
    pub(crate) transcript_path: Option<String>,
    pub(crate) cwd: String,
    pub(crate) reconstructed: bool,
    pub(crate) note: String,
    pub(crate) started_at_unix_ms: u64,
}

/// What `workflow.node.interrogate` resolved before anything was created: the
/// mode's precondition, already checked, turned into what the spawn needs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum InterrogationSeed {
    /// The source transcript and the recorded cwd both stat'd; fork it.
    Resumed {
        cwd: PathBuf,
        /// The path that was stat'd, recorded on the row so a later reader
        /// knows which transcript this fork came from.
        transcript_path: String,
    },
    /// No transcript to resume; stand in from this stored checkpoint, and say
    /// so (`00-overview.md` Feature 3).
    Reconstructed {
        cwd: PathBuf,
        checkpoint_seq: u64,
        summary: String,
        payload: String,
        original_task: Option<String>,
        label: String,
    },
}

/// Why an interrogation whose preconditions all passed could not get a **pane**.
///
/// Carries only the reason: the API code is fixed
/// (`workflow_interrogation_spawn_failed`, E-15) and is chosen by the handler,
/// because an interrogation is not a run node (§4 D8) and must not be reported
/// with the node-spawn codes even though it fails through the same pane
/// machinery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterrogationSpawnFailed(String);

impl InterrogationSpawnFailed {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    /// A [`spawn::SpawnError`] from the shared pane machinery, kept as prose:
    /// its own code names a *node* failure, which this is not.
    fn from_spawn(error: &spawn::SpawnError) -> Self {
        Self(error.to_string())
    }
}

impl std::fmt::Display for InterrogationSpawnFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The handler's resolved request, handed to the glue that creates the pane.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InterrogationRequest {
    pub(crate) run: RunId,
    pub(crate) path: InstancePath,
    pub(crate) workflow_name: String,
    /// The source run's workspace, when it recorded one.
    pub(crate) workspace_id: Option<String>,
    pub(crate) source_session_id: String,
    pub(crate) note: String,
    pub(crate) seed: InterrogationSeed,
}

/// The `[workflow]` knobs the **app** enforces, as opposed to the engine's.
///
/// Retention and history injection are policies about what the server does
/// around a run — prune afterwards, inject beforehand — not transitions inside
/// one, so they belong here rather than on [`EngineConfig`]. `App` does not
/// keep the loaded [`crate::config::Config`], so they are read once at
/// construction from the same value `engine_config` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorkflowPolicy {
    /// `workflow.retention_runs` (§4 D12).
    pub(crate) retention_runs: usize,
    /// `workflow.history_context_runs` (§4 D21, D22).
    pub(crate) history_context_runs: usize,
    /// Whether summaries are off because `KARVEX_WORKFLOW_SUMMARY_COMMAND` could
    /// not be parsed, as opposed to because the user set
    /// `workflow.summary_enabled = false` (E-11).
    ///
    /// The two are deliberately distinguished: switching summaries off is a
    /// choice that needs no announcement, while a typo'd override leaves the
    /// user believing their summariser is bound when it is not.
    pub(crate) summary_override_invalid: bool,
}

impl Default for WorkflowPolicy {
    /// The documented defaults, so a test-constructed runtime behaves like a
    /// server started with no `[workflow]` block at all.
    fn default() -> Self {
        Self {
            retention_runs: 50,
            history_context_runs: 3,
            summary_override_invalid: false,
        }
    }
}

/// Reads [`WorkflowPolicy`] off the `[workflow]` config block.
pub(crate) fn workflow_policy(config: &crate::config::Config) -> WorkflowPolicy {
    workflow_runtime_config(config).1
}

impl WorkflowRuntimeState {
    pub(crate) fn new(config: EngineConfig, policy: WorkflowPolicy) -> Self {
        Self {
            engine: Engine::new(config.clone()),
            config,
            policy,
            run: None,
            pending_writes: VecDeque::new(),
            dropped_writes: 0,
            persistence_degraded: false,
            pending_spawns: Vec::new(),
            claimed_spawns: HashSet::new(),
            spawn_failures: HashMap::new(),
            node_tokens: HashMap::new(),
            next_tick_at: None,
            delivery_failure: None,
            announced_run_status: None,
            announced_attention: HashSet::new(),
            growth_limits: HashMap::new(),
            last_growth_limit: None,
            pruned_history_for: None,
            summary_disabled_notified: false,
            interrogations: Vec::new(),
        }
    }

    /// Claims the one "summaries are disabled" notice this server gets (E-11).
    /// `true` at most once, and only when the disable came from an unusable
    /// override rather than from a deliberate config setting.
    pub(crate) fn claim_summary_disabled_notice(&mut self) -> bool {
        if !self.policy.summary_override_invalid || self.summary_disabled_notified {
            return false;
        }
        self.summary_disabled_notified = true;
        true
    }

    /// Claims the one retention pass this run gets. `true` exactly once per
    /// run id.
    pub(crate) fn mark_history_pruned(&mut self, run: &RunId) -> bool {
        if self.pruned_history_for.as_ref() == Some(run) {
            return false;
        }
        self.pruned_history_for = Some(run.clone());
        true
    }

    /// The live interrogation of this exact source node, if one is open.
    ///
    /// The authority for §4 D7's one-at-a-time rule is this list and **not**
    /// `interrogation` rows with no `ended_at`: a row left open by a server that
    /// died names a pane that no longer exists, and treating it as live would
    /// refuse the node forever.
    pub(crate) fn live_interrogation(
        &self,
        run: &RunId,
        path: &InstancePath,
    ) -> Option<&LiveInterrogation> {
        self.interrogations
            .iter()
            .find(|entry| &entry.run == run && &entry.path == path)
    }

    pub(crate) fn track_interrogation(&mut self, interrogation: LiveInterrogation) {
        self.interrogations.push(interrogation);
        // An interrogation can be opened with no run live at all, and `settle`
        // — the only other thing that arms the tick — never runs without one.
        // Without this the reconcile sweep would have no cadence to run on.
        self.rearm_tick(Instant::now());
    }

    pub(crate) fn interrogation_for_pane(&self, pane: &PublicPaneId) -> Option<&LiveInterrogation> {
        self.interrogations.iter().find(|entry| &entry.pane == pane)
    }

    /// Every interrogation pane, for the reconcile sweep to test against the
    /// layout.
    pub(crate) fn interrogation_panes(&self) -> Vec<PublicPaneId> {
        self.interrogations
            .iter()
            .map(|entry| entry.pane.clone())
            .collect()
    }

    /// Takes the interrogation whose pane went away. Removing it here is what
    /// makes the end stamp idempotent: two signals racing for the same dead
    /// pane produce one `InterrogationUpdate`, not two.
    pub(crate) fn end_interrogation_for_pane(
        &mut self,
        pane: &PublicPaneId,
    ) -> Option<LiveInterrogation> {
        let index = self
            .interrogations
            .iter()
            .position(|entry| &entry.pane == pane)?;
        Some(self.interrogations.remove(index))
    }

    /// Records a forked session id learned after the record was written (§6
    /// A9). Returns the interrogation when this is genuinely new information,
    /// so the caller enqueues exactly one `InterrogationUpdate`.
    pub(crate) fn learn_forked_session_id(
        &mut self,
        pane: &PublicPaneId,
        session_id: &str,
    ) -> Option<LiveInterrogation> {
        let entry = self
            .interrogations
            .iter_mut()
            .find(|entry| &entry.pane == pane)?;
        if entry.forked_session_id.as_deref() == Some(session_id) {
            return None;
        }
        entry.forked_session_id = Some(session_id.to_string());
        Some(entry.clone())
    }

    /// Records the growth guardrail `path`'s proposal ran into.
    ///
    /// Last-write-wins per node and for the run: a node that proposes twice has
    /// one current answer to "what stopped you", and the banner names the most
    /// recent breach rather than the first.
    pub(crate) fn record_growth_limit(&mut self, path: &InstancePath, limit: WorkflowGrowthLimit) {
        self.growth_limits.insert(path.clone(), limit.clone());
        self.last_growth_limit = Some(limit);
    }

    /// The last growth guardrail this node ran into as a proposer.
    pub(crate) fn node_growth_limit(&self, path: &InstancePath) -> Option<&WorkflowGrowthLimit> {
        self.growth_limits.get(path)
    }

    /// The run's most recent growth guardrail, whichever node hit it.
    pub(crate) fn last_growth_limit(&self) -> Option<&WorkflowGrowthLimit> {
        self.last_growth_limit.as_ref()
    }

    /// Everything the user has not been told yet about the run's current shape.
    ///
    /// The engine raises a notice for the failures it can see from inside a
    /// transition, but it has no notice at all for a run that *finishes* — the
    /// most common outcome — and none for a node that quietly lands in
    /// `needs_attention`. Reading it off the settled graph after each effect
    /// batch covers every path into those states without a second notification
    /// model: the caller shows these exactly the way it shows the engine's own
    /// [`UserNotice`]s.
    ///
    /// Ordered node-first, which is now just an ordering: the notice queue
    /// (`AppState::push_toast`, §4 D10) renders both, so this no longer has to
    /// choose which one survives a single slot.
    pub(crate) fn take_pending_announcements(&mut self) -> Vec<UserNotice> {
        let Some(graph) = self.engine.graph() else {
            return Vec::new();
        };
        let run = graph.run_id.clone();
        let status = graph.status;
        let mut announcements = Vec::new();

        let attention: HashSet<InstancePath> = graph
            .nodes
            .iter()
            .filter(|node| node.status == NodeStatus::NeedsAttention)
            .map(|node| node.path.clone())
            .collect();
        for node in &graph.nodes {
            if node.status != NodeStatus::NeedsAttention
                || self.announced_attention.contains(&node.path)
            {
                continue;
            }
            announcements.push(UserNotice {
                level: NoticeLevel::Warning,
                run: Some(run.clone()),
                path: Some(node.path.clone()),
                message: match &node.succession {
                    Some(Succession::Blocked { reason, .. }) => {
                        format!("needs attention: {reason}")
                    }
                    _ => "needs attention: the node is waiting for a human".to_string(),
                },
            });
        }
        // A node that recovered is forgotten, so getting stuck again is news.
        self.announced_attention = attention;

        if self.announced_run_status != Some(status) {
            let blocking: Vec<String> = graph
                .nodes
                .iter()
                .filter(|node| {
                    matches!(
                        node.status,
                        NodeStatus::NeedsAttention | NodeStatus::Blocked | NodeStatus::Failed
                    )
                })
                .map(|node| node.path.to_string())
                .collect();
            if let Some(notice) = run_status_notice(&run, status, &blocking) {
                announcements.push(notice);
            }
            self.announced_run_status = Some(status);
        }

        announcements
    }

    pub(crate) fn record_delivery_failure(&mut self, failure: DeliveryFailure) {
        self.delivery_failure = Some(failure);
    }

    /// Clears any delivery failure left by an earlier effect batch, so a call
    /// only ever answers for its own delivery.
    pub(crate) fn clear_delivery_failure(&mut self) {
        self.delivery_failure = None;
    }

    pub(crate) fn take_delivery_failure(&mut self) -> Option<DeliveryFailure> {
        self.delivery_failure.take()
    }

    /// Hands a refused pane delivery back to the engine, which journals it as a
    /// run event, marks the node, and raises a notice. Returns the effects for
    /// the caller to dispatch.
    pub(crate) fn note_delivery_failure(
        &mut self,
        path: &InstancePath,
        method: &str,
        reason: &str,
    ) -> Vec<RunEffect> {
        self.engine.note_delivery_failure(path, method, reason)
    }

    /// The last delivery this node's runtime refused, if one is outstanding.
    pub(crate) fn node_delivery_failure(
        &self,
        path: &InstancePath,
    ) -> Option<&DeliveryFailureNote> {
        self.engine.delivery_failure(path)
    }

    pub(crate) fn config(&self) -> EngineConfig {
        self.config.clone()
    }

    pub(crate) fn policy(&self) -> WorkflowPolicy {
        self.policy
    }

    pub(crate) fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Records the transcript path a node's pane reported, and returns the
    /// durable write for it (§4 D6, closing §0.5).
    ///
    /// The two engine calls are wrapped together here rather than exposed to the
    /// caller separately, so the in-memory correction and the row update cannot
    /// come apart: recording without persisting is precisely the live-vs-durable
    /// divergence this phase keeps having to design against. An unchanged path
    /// is `None` and costs no write.
    pub(crate) fn record_transcript_path(
        &mut self,
        path: &InstancePath,
        transcript: PathBuf,
    ) -> Option<RunEffect> {
        if !self.engine.record_transcript_path(path, transcript) {
            return None;
        }
        self.engine.node_persist_effect(path)
    }

    pub(crate) fn active_run(&self) -> Option<&ActiveRun> {
        self.run.as_ref()
    }

    pub(crate) fn graph(&self) -> Option<&RunGraph> {
        self.engine.graph()
    }

    pub(crate) fn definition(&self) -> Option<&Kvdag> {
        self.engine.definition()
    }

    /// How `run`'s epilogue was bound, when `run` is the run this server holds
    /// in memory.
    ///
    /// The reserved `.summary` path has **no kvdag node** (§4 D5), so every
    /// definition lookup misses for it and the caller that asks "what runner is
    /// this node?" has nowhere else to read the answer.
    /// `EpilogueState::runner` is the single authority D-1 established, and
    /// this is the by-path face of the by-pane read [`Self::runner_for_pane`]
    /// makes.
    ///
    /// `None` covers three cases the caller must treat alike as "unknown":
    /// there is no run in memory, the run in memory is a different one, or the
    /// run had no epilogue. Nothing persists the epilogue's runner — no
    /// `run_node` column carries it — so a run this server did not execute is
    /// always `None` here rather than answered from a re-derivation of the
    /// *current* server's `KARVEX_WORKFLOW_SUMMARY_COMMAND`, which would
    /// describe a configuration that run never had.
    pub(crate) fn epilogue_runner(&self, run: &RunId) -> Option<Runner> {
        let graph = self.engine.graph()?;
        if graph.run_id != *run {
            return None;
        }
        graph.epilogue.map(|state| state.runner)
    }

    pub(crate) fn run_status(&self) -> Option<RunStatus> {
        self.engine.graph().map(|graph| graph.status)
    }

    /// A run is live while it can still make progress. A finished run stays
    /// readable until the next one replaces it.
    ///
    /// Deliberately **not** widened by Phase 3 (`07-phase3-plan.md` M7): a
    /// succeeded run whose epilogue is still working is over, and everything
    /// keyed on `is_live` — restart guards, the DAG's live/closed split, the
    /// run's terminal status — must keep reading it that way. The two things
    /// that do have to outlive it are the *tick* and the run-start guard, and
    /// both say so explicitly ([`Self::needs_tick`],
    /// `App::handle_workflow_run`).
    pub(crate) fn is_live(&self) -> bool {
        matches!(
            self.run_status(),
            Some(RunStatus::Pending | RunStatus::Running | RunStatus::Paused)
        )
    }

    /// Whether the engine's own summariser is still working (§4 D1). The
    /// run-start guard's second disjunct, so a `workflow.run` arriving during
    /// the epilogue cannot swap the engine out from under it (M7).
    pub(crate) fn epilogue_pending(&self) -> bool {
        self.engine.epilogue_pending()
    }

    /// Whether the workflow tick still has work to drive.
    ///
    /// Three independent reasons, and every one of them outlives
    /// [`Self::is_live`]: a live run's nodes, the epilogue's summariser after
    /// the run's status is already final (§4 D1 / §7 R-1), and interrogation
    /// panes, which belong to no run at all and are reconciled against the
    /// layout on the same cadence node panes are (§4 D8). Without the third, an
    /// interrogation opened while nothing is running would never have its end
    /// stamped by the sweep.
    fn needs_tick(&self) -> bool {
        self.is_live() || self.engine.epilogue_pending() || !self.interrogations.is_empty()
    }

    /// Re-arms or lapses the tick deadline from [`Self::needs_tick`].
    ///
    /// `settle` does this for everything that goes through the engine; this is
    /// the same arithmetic for the two things that do not — tracking an
    /// interrogation with no live run, and the tick that finds the last one
    /// gone.
    pub(crate) fn rearm_tick(&mut self, now: Instant) {
        self.next_tick_at = match (self.needs_tick(), self.next_tick_at) {
            (false, _) => None,
            (true, Some(deadline)) if deadline > now => Some(deadline),
            (true, _) => Some(now + WORKFLOW_TICK_INTERVAL),
        };
    }

    /// Installs the definition and starts the run. The definition has to be
    /// installed before `EngineInput::Start`, which carries only the run graph
    /// and therefore no output schemas to validate results against.
    pub(crate) fn start(
        &mut self,
        run: ActiveRun,
        definition: Kvdag,
        graph: RunGraph,
        now: Instant,
    ) -> Result<Vec<RunEffect>, WorkflowStartError> {
        if self.is_live() {
            return Err(WorkflowStartError::RunInFlight);
        }
        self.engine = Engine::new(self.config.clone());
        self.pending_spawns.clear();
        self.claimed_spawns.clear();
        self.spawn_failures.clear();
        self.node_tokens.clear();
        // Persistence health is a property of the run, not of the process: a
        // previous run's lost write must not leave the next one reporting
        // itself degraded.
        //
        // The caller (`App::start_workflow_run`) drains this to the store task
        // *before* calling in, so what is cleared here is only what the store
        // task had no room for — never an accepted summary that nothing else
        // would ever write (M7).
        self.pending_writes.clear();
        self.dropped_writes = 0;
        self.persistence_degraded = false;
        // Announcements are per run too: the previous run's terminal status
        // must not suppress this one's.
        self.announced_run_status = None;
        self.announced_attention.clear();
        // Growth is a property of the run too: the previous run's ceiling
        // breach must not banner this one.
        self.growth_limits.clear();
        self.last_growth_limit = None;
        self.run = Some(run);
        self.engine.install_definition(definition);
        let effects = self.engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        self.settle(now);
        Ok(effects)
    }

    pub(crate) fn apply(&mut self, input: EngineInput, now: Instant) -> Vec<RunEffect> {
        if self.run.is_none() {
            debug!("workflow engine input with no active run");
            return Vec::new();
        }
        // A restart is the user asking for a fresh attempt, so the node's spawn
        // attempts start over too — otherwise a node that had already burnt the
        // spawn budget would be given up on again the moment it is admitted.
        if let EngineInput::RestartNode { path } = &input {
            self.spawn_failures.remove(path);
        }
        let effects = self.engine.apply(input);
        self.settle(now);
        effects
    }

    /// Records the pane binding of a node the binder spawned, which is what
    /// moves it to `Running`.
    pub(crate) fn bind_node(
        &mut self,
        path: &InstancePath,
        binding: NodeBinding,
        now: Instant,
    ) -> Vec<RunEffect> {
        let effects = self.engine.bind_node(path, binding);
        self.settle(now);
        effects
    }

    /// Refreshes the derived state that depends on the graph: the admission
    /// queue, the tick deadline, and the run's end timestamp.
    fn settle(&mut self, now: Instant) {
        let admitted: Vec<InstancePath> = self
            .engine
            .admissions()
            .into_iter()
            .filter_map(|idx| self.engine.graph()?.node(idx).map(|node| node.path.clone()))
            .collect();
        self.claimed_spawns
            .retain(|path| admitted.iter().any(|admitted| admitted == path));
        self.pending_spawns = admitted;

        // A pending deadline is kept, so the clock is a fixed cadence rather
        // than an inactivity timer: the sustained-idle rule counts ticks, and a
        // busy sibling node must not push a stuck node's samples out forever.
        let live = self.is_live();
        self.rearm_tick(now);
        // Fallback only: a run that leaves the live set through a path the
        // engine did not close (so no `StoreWrite::RunStatus` carried a stamp)
        // still gets an end time. `queue_write` overwrites this with the
        // engine's stamp whenever there is one, so the live run and the journal
        // agree.
        if let Some(run) = self.run.as_mut() {
            if !live && run.ended_at_unix_ms.is_none() {
                run.ended_at_unix_ms = Some(current_unix_ms());
            }
        }
    }

    pub(crate) fn next_tick_deadline(&self) -> Option<Instant> {
        self.next_tick_at
    }

    pub(crate) fn tick_due(&self, now: Instant) -> bool {
        self.next_tick_at.is_some_and(|deadline| now >= deadline)
    }

    /// Buffers a durable write for the store task. Overflow drops the oldest
    /// *evictable* entry and marks the run's persistence degraded rather than
    /// blocking a node transition on I/O — see [`Self::evict_one_pending_write`]
    /// for why "evictable" is not simply "oldest".
    pub(crate) fn queue_write(&mut self, write: StoreWrite) {
        // The engine stamps the run's close time as it closes the run, and both
        // the live projection and the journal report *that* stamp. Without this
        // the live run kept the stamp `settle` took and the store took its own
        // when the queued write was finally applied, so the same run reported
        // two end times tens of milliseconds apart.
        if let StoreWrite::RunStatus {
            ended_at_unix_ms: Some(ended),
            ..
        } = &write
        {
            if let Some(run) = self.run.as_mut() {
                run.ended_at_unix_ms = Some(*ended);
            }
        }
        if self.pending_writes.len() >= PENDING_WRITE_BUDGET {
            self.evict_one_pending_write();
        }
        self.pending_writes.push_back(write);
    }

    /// Makes room for one more buffered write by dropping the oldest entry the
    /// journal can survive losing (`06-phase2-plan.md` §4 D7).
    ///
    /// A create is not such an entry. [`StoreWrite::RunNode`] and
    /// [`StoreWrite::RunEdge`] are find-then-`UPDATE` and error on a missing
    /// row, so dropping a [`StoreWrite::RunNodeCreated`] while keeping the
    /// updates queued behind it does not lose one row — it turns every later
    /// write naming that path into a permanent failure. Eviction therefore
    /// scans from the front for the first non-create entry.
    ///
    /// When the queue is *all* creates there is nothing safe to drop, so it
    /// grows past [`PENDING_WRITE_BUDGET`] and the run is marked persistence
    /// degraded instead. The bound is a memory guard; a corrupted journal is
    /// worse than a temporarily larger queue, and the in-memory [`RunGraph`]
    /// stays authoritative either way.
    fn evict_one_pending_write(&mut self) {
        let victim = self
            .pending_writes
            .iter()
            .position(|write| !is_create_write(write));
        match victim {
            Some(index) => {
                self.pending_writes.remove(index);
                self.dropped_writes = self.dropped_writes.saturating_add(1);
                self.persistence_degraded = true;
            }
            None => {
                if self.mark_persistence_degraded() {
                    warn!(
                        queued = self.pending_writes.len(),
                        "workflow write queue is over budget and holds only node/edge creates; \
                         growing it rather than dropping a create"
                    );
                }
            }
        }
    }

    /// Takes at most `limit` buffered writes, oldest first. The cap is what
    /// lets the caller stop handing work to a store thread that is behind, so
    /// the surplus stays here under [`PENDING_WRITE_BUDGET`].
    pub(crate) fn take_pending_writes(&mut self, limit: usize) -> Vec<StoreWrite> {
        let take = limit.min(self.pending_writes.len());
        self.pending_writes.drain(..take).collect()
    }

    pub(crate) fn pending_write_count(&self) -> usize {
        self.pending_writes.len()
    }

    pub(crate) fn dropped_write_count(&self) -> u64 {
        self.dropped_writes
    }

    pub(crate) fn persistence_degraded(&self) -> bool {
        self.persistence_degraded
    }

    /// Marks persistence degraded after a store write failed. Surfaced on the
    /// run rather than killing it (`04` §9). Returns whether this was the
    /// transition, so the surfacing happens once per run rather than once per
    /// lost write.
    pub(crate) fn mark_persistence_degraded(&mut self) -> bool {
        let newly = !self.persistence_degraded;
        self.persistence_degraded = true;
        newly
    }

    pub(crate) fn pending_spawns(&self) -> &[InstancePath] {
        &self.pending_spawns
    }

    /// Admitted nodes the binder has not been offered yet, marked as offered.
    /// A node re-enters this set only by leaving and re-entering the ready set,
    /// so a restart is offered again and a queued node is not offered twice.
    pub(crate) fn claim_spawns(&mut self) -> Vec<InstancePath> {
        let claimed: Vec<InstancePath> = self
            .pending_spawns
            .iter()
            .filter(|path| !self.claimed_spawns.contains(*path))
            .cloned()
            .collect();
        for path in &claimed {
            self.claimed_spawns.insert(path.clone());
        }
        claimed
    }

    /// Counts a failed spawn attempt and returns the node's running total.
    pub(crate) fn record_spawn_failure(&mut self, path: &InstancePath) -> u8 {
        let attempts = self.spawn_failures.entry(path.clone()).or_insert(0);
        *attempts = attempts.saturating_add(1);
        *attempts
    }

    /// Puts a node back in front of the binder after a failed attempt.
    pub(crate) fn release_spawn_claim(&mut self, path: &InstancePath) {
        self.claimed_spawns.remove(path);
    }

    pub(crate) fn spawn_failure_count(&self, path: &InstancePath) -> u8 {
        self.spawn_failures.get(path).copied().unwrap_or(0)
    }

    /// Remembers the token a node's process was handed, so its self-report can
    /// be authenticated. A respawn replaces the previous attempt's token.
    pub(crate) fn record_node_token(&mut self, path: &InstancePath, token: NodeToken) {
        self.node_tokens.insert(path.clone(), token);
    }

    pub(crate) fn node_token(&self, path: &InstancePath) -> Option<&NodeToken> {
        self.node_tokens.get(path)
    }

    pub(crate) fn node(&self, path: &InstancePath) -> Option<&RunNode> {
        self.engine.graph()?.node_by_path(path)
    }

    pub(crate) fn node_path_for_pane(&self, pane: &PublicPaneId) -> Option<InstancePath> {
        self.engine
            .graph()?
            .node_by_pane(pane)
            .map(|node| node.path.clone())
    }

    /// The binding primitive a node's pane accepts (`04` §5). Without a
    /// definition the binding is unknown, and `Runner::Agent` is the
    /// definition's own default.
    ///
    /// **The epilogue is asked about itself** (D-C). It has no kvdag node, so
    /// the definition lookup below can never resolve for it and the
    /// `Runner::Agent` fallback would be an outright lie whenever the summariser
    /// is bound to a command: its corrective re-prompt would go out as
    /// `agent.prompt` to a pane holding a plain process, fail
    /// `agent_not_found`, and silently kill the one re-prompt rung of the
    /// epilogue's bounded ladder (§4 D1). `EpilogueState::runner` is the single
    /// authority D-1 established for exactly this question — recorded once at
    /// `begin_epilogue`, read here rather than re-derived.
    pub(crate) fn runner_for_pane(&self, pane: &PublicPaneId) -> Runner {
        let runner = self.engine.graph().and_then(|graph| {
            let node = graph.node_by_pane(pane)?;
            if let Some(epilogue) = graph.epilogue.filter(|state| state.node == node.idx) {
                return Some(epilogue.runner);
            }
            Some(self.engine.definition()?.node(&node.key)?.runner)
        });
        runner.unwrap_or(Runner::Agent)
    }
}

/// Everything one node's spawn needs, resolved from the run graph and the
/// definition before anything is created.
#[derive(Debug, Clone)]
pub(crate) struct NodeSpawnPlan {
    pub(crate) spec: SpawnSpec,
    pub(crate) layout: spawn::NodeDirLayout,
    pub(crate) task_markdown: String,
    pub(crate) output_schema: OutputSchema,
    /// `port -> every upstream that fired into it`, in source-path order. A
    /// port is a list and not a payload because §3.4's inherited fan-in gives
    /// one port a whole generation of contributors.
    pub(crate) inputs: BTreeMap<String, Vec<spawn::PortContribution>>,
    pub(crate) transcript_path: PathBuf,
    /// What the node's pane is called in the sidebar and the pane header.
    /// Distinct from `spec.label`, which has to stay unique because it is the
    /// agent name; a pane title only has to be readable.
    pub(crate) pane_title: String,
}

/// What **this instance** is called, on every surface that names a node.
///
/// The run node's own label first — for an expansion child that is the `--label`
/// the proposing node was required to supply, and it is the only thing that
/// distinguishes one sibling of a generation from another. The definition's
/// authored label is the fallback for a node whose instance carries none (a run
/// graph restored by an older karvex, say), and the kvdag key is the last one.
/// Nothing here invents a name.
pub(crate) fn workflow_node_label<'a>(definition: &'a Kvdag, node: &'a RunNode) -> &'a str {
    let instance = node.label.trim();
    if !instance.is_empty() {
        return instance;
    }
    let authored = definition
        .node(&node.key)
        .map(|spec| spec.label.trim())
        .unwrap_or_default();
    if authored.is_empty() {
        node.key.as_str()
    } else {
        authored
    }
}

/// One port's per-contributor files as `task.md` lists them, empty for the
/// ordinary single-edge port whose file is exactly the upstream payload.
///
/// The paths come from the node's own [`spawn::NodeDirLayout`], so they are
/// absolute and spelled exactly as `materialise_node_dir` wrote them — a node's
/// cwd is the workspace directory, and a relative listing here would send it
/// looking for `inputs/` beside the user's own files.
fn task_input_sources(
    layout: &spawn::NodeDirLayout,
    port: &str,
    contributions: &[spawn::PortContribution],
) -> Vec<(String, PathBuf)> {
    if contributions.len() < 2 {
        return Vec::new();
    }
    let sources: Vec<&str> = contributions
        .iter()
        .map(|contribution| contribution.from.as_str())
        .collect();
    spawn::input_source_stems(&sources)
        .into_iter()
        .zip(sources.iter())
        .map(|(stem, from)| ((*from).to_string(), layout.input_source_file(port, &stem)))
        .collect()
}

/// The epilogue's `task.md`, rendered through the **same** document every
/// authored node's is (`07-phase3-plan.md` §3 rule 2).
///
/// The engine owns what the summariser is asked to cover; it does not own how a
/// node finishes, and it cannot — it is pure, and the reporting contract names
/// files only the binder knows about. Routing the body through
/// [`spawn::TaskDocument`] is what closes the defect this function exists for:
/// the epilogue used to carry a hand-built document that never grew a
/// `## Reporting` section, so the summariser was the one node never told to
/// write `result.json` or run `kvx workflow node complete`. It could therefore
/// only finish under a `KARVEX_WORKFLOW_SUMMARY_COMMAND` stub that already knew
/// the protocol, and never under the default `claude` runner — end-of-run
/// summaries were broken in exactly the configuration users have.
///
/// A second hand-maintained copy of the contract would have drifted the same
/// way, so there is one renderer and the epilogue simply passes through it. The
/// sections it has nothing for are left empty and `TaskDocument` omits them:
/// the epilogue has no author-supplied role, no workflow contract (D2 — the
/// author's instructions must not rewrite what karvex's own summariser is for),
/// no inbound ports, and no prior-runs pointer, because the evidence it needs
/// is already rendered into its body.
///
/// `node_dir` is the epilogue's own [`spawn::NodeDirLayout`] root, and passing
/// it is the whole point of sharing the renderer rather than copying it: the
/// summariser's cwd is the workspace directory like every other node's, so the
/// `## Reporting` section it now gets names `result.json` and
/// `output_schema.json` absolutely. A hand-built epilogue document would have
/// had to re-derive both facts, which is how the contract went missing in the
/// first place.
fn epilogue_task_markdown(spec: &EpilogueTaskSpec, node_dir: &Path) -> String {
    spawn::TaskDocument {
        label: &spec.label,
        role: "",
        contract: "",
        prompt: &spec.task_body,
        input_ports: &[],
        prior_runs: None,
        node_dir,
    }
    .render()
}

/// `SpawnSpec::label` becomes both `claude --name` and the karvex agent name, so
/// it has to be unique among the run's live agents. Node labels are not unique
/// by construction — a proposing node may hand its whole generation one label —
/// and the instance path always is.
fn workflow_agent_name(graph: &RunGraph, definition: &Kvdag, node: &RunNode) -> String {
    let label = workflow_node_label(definition, node);
    if label.is_empty() {
        return node.path.to_string();
    }
    let duplicated = graph
        .nodes
        .iter()
        .filter(|other| workflow_node_label(definition, other) == label)
        .count()
        > 1;
    if duplicated {
        node.path.to_string()
    } else {
        label.to_string()
    }
}

/// Reads the engine's knobs off the `[workflow]` config block. The config type
/// is `usize`-wide; the engine's streak counters are `u16`, so an out-of-range
/// value saturates instead of wrapping into a much smaller threshold.
pub(crate) fn engine_config(config: &crate::config::Config) -> EngineConfig {
    workflow_runtime_config(config).0
}

/// Both halves of the `[workflow]` block from **one** read of the environment.
///
/// `KARVEX_WORKFLOW_SUMMARY_COMMAND` is read exactly once per construction
/// (defect D-1), and both the engine's knobs and the app's policy are derived
/// from that single reading — including whether the override was *unusable*,
/// which only the app can surface (E-11) and only the engine can act on.
/// Deriving them in two functions would mean two readings of one variable, which
/// is the shape of the defect this rule exists to prevent.
pub(crate) fn workflow_runtime_config(
    config: &crate::config::Config,
) -> (EngineConfig, WorkflowPolicy) {
    // A malformed override disables the epilogue outright rather than quietly
    // reverting it to `claude`: running — and billing — an agent the caller
    // explicitly bound to a command is the failure this rejection exists to
    // prevent.
    let (summary_command, summary_override_invalid) = match summary_command_override() {
        SummaryCommand::Agent => (None, false),
        SummaryCommand::Argv(argv) => (Some(argv), false),
        SummaryCommand::Invalid => (None, true),
    };
    let engine = EngineConfig {
        max_parallel_nodes: config.workflow.max_parallel_nodes.max(1),
        stuck_threshold: u16::try_from(config.workflow.stuck_threshold).unwrap_or(u16::MAX),
        drift_threshold: u16::try_from(config.workflow.drift_threshold).unwrap_or(u16::MAX),
        summary_command,
        summary_enabled: config.workflow.summary_enabled && !summary_override_invalid,
    };
    let policy = WorkflowPolicy {
        retention_runs: config.workflow.retention_runs,
        history_context_runs: config.workflow.history_context_runs,
        summary_override_invalid,
    };
    (engine, policy)
}

/// `KARVEX_WORKFLOW_SUMMARY_COMMAND` — the declared binding override that runs
/// the epilogue as a command instead of `claude` (§4 D2 / §6 A4).
///
/// **Read exactly once, here** (defect D-1). The engine has to know which runner
/// the summariser is — the runner decides which completion signals are
/// admissible — and the spawn plan takes both runner and argv from the engine's
/// `summary_task_spec`. A second reader of this variable is how the two end up
/// disagreeing about what the summariser is.
///
/// Invalid content is **refused loudly**: the variable is set deliberately, so
/// a malformed value means the caller believes the summariser is bound to their
/// command when it is not. Falling back to `claude` silently would run — and
/// bill — the wrong thing.
fn summary_command_override() -> SummaryCommand {
    parse_summary_command(std::env::var(SUMMARY_COMMAND_ENV_VAR).ok().as_deref())
}

/// The env-free half of [`summary_command_override`], so the three outcomes are
/// testable without a process-global other tests would race on.
fn parse_summary_command(raw: Option<&str>) -> SummaryCommand {
    let Some(raw) = raw else {
        return SummaryCommand::Agent;
    };
    if raw.trim().is_empty() {
        return SummaryCommand::Agent;
    }
    match serde_json::from_str::<Vec<String>>(raw) {
        Ok(argv) if !argv.is_empty() => SummaryCommand::Argv(argv),
        Ok(_) => {
            warn!(
                "{SUMMARY_COMMAND_ENV_VAR} is an empty argv array; end-of-run summaries are \
                 disabled for this server rather than run as an agent the caller did not ask for"
            );
            SummaryCommand::Invalid
        }
        Err(error) => {
            warn!(
                error = %error,
                "{SUMMARY_COMMAND_ENV_VAR} is not a JSON array of strings; end-of-run summaries \
                 are disabled for this server rather than silently run as an agent"
            );
            SummaryCommand::Invalid
        }
    }
}

/// How `KARVEX_WORKFLOW_SUMMARY_COMMAND` resolved.
///
/// `Invalid` is deliberately **not** collapsed into `Agent`: the variable being
/// set at all means the caller has an opinion about what the summariser is, and
/// answering that opinion with "I ran `claude` instead" is the silent fallback
/// D-1 forbids.
#[derive(Debug)]
enum SummaryCommand {
    /// Unset or empty: the production path, the summariser runs as an agent.
    Agent,
    /// A declared command binding.
    Argv(Vec<String>),
    /// Set to something unusable. Summaries are switched off for this server.
    Invalid,
}

impl App {
    /// Deadline for the select loop's workflow tick arm.
    pub(crate) fn workflow_tick_deadline(&self) -> Option<Instant> {
        self.workflow.next_tick_deadline()
    }

    /// Starts a run and dispatches its opening effects.
    pub(crate) fn start_workflow_run(
        &mut self,
        run: ActiveRun,
        definition: Kvdag,
        graph: RunGraph,
    ) -> Result<(), WorkflowStartError> {
        let now = Instant::now();
        // E-11: a boot-time `warn!` is evidence nobody reads. If the summariser
        // was switched off because `KARVEX_WORKFLOW_SUMMARY_COMMAND` could not be
        // parsed, say so at the moment someone actually cares — the first run
        // that would otherwise have summarised — and say it exactly once per
        // server, not once per run.
        self.notice_summaries_disabled_once();
        // M7, second half. The run-start guard alone is not enough: an accepted
        // summary can be sitting in `pending_writes` after the epilogue is
        // already `Done`, and `WorkflowRuntimeState::start` clears that queue
        // outright — so the enqueued `StoreWrite::RunSummary` would die with the
        // engine swap even though the guard passed. Handing it to the store task
        // *first* is immediate and deterministic; the alternative, refusing
        // until the queue drains, can block indefinitely behind a backlogged
        // store task. Silent summary loss is the one outcome ruled out.
        self.flush_workflow_writes();
        let effects = self.workflow.start(run, definition, graph, now)?;
        self.dispatch_workflow_effects(effects);
        self.drive_workflow_spawns();
        Ok(())
    }

    /// Feeds one input to the engine and dispatches everything it produces.
    /// Returns whether the UI has to be redrawn.
    pub(crate) fn apply_workflow_engine_input(&mut self, input: EngineInput) -> bool {
        self.apply_workflow_engine_input_at(input, Instant::now())
    }

    /// The clock is supplied so the tick's own deadline arithmetic and the
    /// input it delivers agree on `now`.
    fn apply_workflow_engine_input_at(&mut self, input: EngineInput, now: Instant) -> bool {
        let effects = self.workflow.apply(input, now);
        let mut changed = self.dispatch_workflow_effects(effects);
        changed |= self.drive_workflow_spawns();
        changed
    }

    /// The select loop's tick arm: samples the detector for every running node,
    /// then advances the engine clock. The sampling is the tick's job because
    /// the sustained-idle rule counts detector *ticks*, and a pane that has been
    /// idle for three ticks produced exactly one state change.
    pub(crate) fn tick_workflow_engine(&mut self, now: Instant) -> bool {
        if !self.workflow.tick_due(now) {
            return false;
        }
        // Before sampling, because a node whose pane is gone has no agent state
        // to sample and must not be counted as idle for another tick.
        let mut changed = self.reconcile_workflow_pane_bindings();
        changed |= self.reconcile_interrogation_panes();
        changed |= self.sample_workflow_agent_states(now);
        changed |= self.apply_workflow_engine_input_at(EngineInput::Tick { now }, now);
        // `settle` re-arms whenever the input reached the engine; with no live
        // run it returns early and this is what keeps — or lapses — the
        // interrogation sweep's cadence.
        self.workflow.rearm_tick(now);
        changed
    }

    /// Fails, or retries, every node whose pane has left the layout without a
    /// `PaneExited` ever reaching the engine (`06-phase2-plan.md` §4 D14 / H6).
    ///
    /// Two paths report a pane's disappearance directly: `AppEvent::PaneDied`
    /// for a process that exited, and `App::close_pane` for the API verb and
    /// the TUI keybinding that routes through it. Neither covers bulk removal —
    /// `handle_tab_close` and `handle_workspace_close` drop every pane they own
    /// without telling anyone — and neither would cover a future path. So the
    /// live-run tick reconciles the engine's bindings against the layout
    /// instead of chasing call sites: detection is immediate for the direct
    /// paths and bounded at one [`WORKFLOW_TICK_INTERVAL`] for everything else.
    ///
    /// A node is reconciled at most once. `Engine::pane_exited` either clears
    /// the binding for a retry or leaves the node terminal, and both are
    /// filtered out below, so a run does not re-report the same dead pane every
    /// 20 seconds.
    pub(crate) fn reconcile_workflow_pane_bindings(&mut self) -> bool {
        let Some(graph) = self.workflow.graph() else {
            return false;
        };
        let orphaned: Vec<PublicPaneId> = graph
            .nodes
            .iter()
            .filter(|node| !node.status.is_terminal())
            .filter_map(|node| node.binding.as_ref().map(|binding| binding.pane_id.clone()))
            .filter(|pane| !self.workflow_pane_is_live(pane))
            .collect();

        let mut changed = false;
        for pane in orphaned {
            warn!(
                pane = %pane,
                "workflow node pane left the layout without a close event; failing the node"
            );
            changed |= self.apply_workflow_engine_input(observe::pane_exited(pane, None));
        }
        changed
    }

    /// Whether a bound public pane id still names *this* pane in this layout.
    ///
    /// Resolving the id is not enough in either direction. Public pane numbers
    /// are per workspace, so a stale id can parse into a different live pane —
    /// which would leave a dead node `running` forever. And a pane moved across
    /// workspaces keeps working under its previous id through
    /// `public_pane_id_aliases`, so demanding the *current* id would kill a node
    /// whose pane is perfectly alive. Both are checked.
    fn workflow_pane_is_live(&self, pane: &PublicPaneId) -> bool {
        let id = pane.as_str();
        let Some((ws_idx, pane_id)) = self.parse_pane_id(id) else {
            return false;
        };
        self.public_pane_id(ws_idx, pane_id).as_deref() == Some(id)
            || self.state.public_pane_id_aliases.get(id) == Some(&pane_id)
    }

    fn sample_workflow_agent_states(&mut self, now: Instant) -> bool {
        let Some(graph) = self.workflow.graph() else {
            return false;
        };
        let samples: Vec<(PublicPaneId, crate::detect::AgentState)> = graph
            .nodes
            .iter()
            .filter(|node| node.status == NodeStatus::Running)
            .filter_map(|node| {
                let binding = node.binding.as_ref()?;
                let state = self
                    .state
                    .terminals
                    .get(&binding.terminal_id)
                    .map_or(crate::detect::AgentState::Unknown, |terminal| {
                        terminal.state
                    });
                Some((binding.pane_id.clone(), state))
            })
            .collect();

        let mut changed = false;
        for (pane, state) in samples {
            changed |=
                self.apply_workflow_engine_input_at(observe::agent_status(pane, state, now), now);
        }
        changed
    }

    /// Runtime facts that arrive asynchronously (`04` §4.3). Pane ids are
    /// internal on the way in and public on the way to the engine; the
    /// translation itself lives in `workflow::binding::observe`.
    pub(crate) fn handle_workflow_app_event(&mut self, event: WorkflowAppEvent) -> bool {
        match event {
            WorkflowAppEvent::Tick => self.tick_workflow_engine(Instant::now()),
            WorkflowAppEvent::NodeHookReported {
                pane_id,
                source,
                agent_label,
                state,
            } => {
                let Some(pane) = self.workflow_public_pane_id(pane_id) else {
                    return false;
                };
                let report = observe::HookStateReport {
                    source: &source,
                    agent_label: &agent_label,
                    state,
                };
                let Some(input) = observe::turn_ended(pane, report) else {
                    return false;
                };
                self.apply_workflow_engine_input(input)
            }
            WorkflowAppEvent::NodeAgentStatus {
                pane_id,
                state,
                observed_at,
            } => {
                let Some(pane) = self.workflow_public_pane_id(pane_id) else {
                    return false;
                };
                self.apply_workflow_engine_input(observe::agent_status(pane, state, observed_at))
            }
            WorkflowAppEvent::NodePaneExited { pane_id, code } => {
                let Some(pane) = self.workflow_public_pane_id(pane_id) else {
                    return false;
                };
                self.apply_workflow_engine_input(observe::pane_exited(pane, code))
            }
        }
    }

    /// The `emit_pane_state_update` side of §4.3 signal 3. One call from the
    /// pane-state publisher is all the workflow runtime needs from it.
    pub(crate) fn observe_workflow_pane_update(
        &mut self,
        update: &crate::app::actions::PaneStateUpdate,
    ) -> bool {
        let Some(pane) = self.workflow_public_pane_id(update.pane_id) else {
            return false;
        };
        if self.workflow.node_path_for_pane(&pane).is_none() {
            return false;
        }
        let Some(input) = observe::agent_status_from_pane_update(pane, update, Instant::now())
        else {
            return false;
        };
        self.apply_workflow_engine_input(input)
    }

    /// `AppEvent::PaneDied` for a node's pane. The event carries no exit status,
    /// so the engine records the failure without a code.
    ///
    /// Also the interrogation end stamp's call site (§4 D7): the same two paths
    /// that report a node pane's disappearance report an interrogation pane's,
    /// which is what lets Phase 3 leave `src/events.rs` untouched. An
    /// interrogation pane is never a node pane, so the engine input below is a
    /// no-op for it and the two cannot double-handle one pane.
    pub(crate) fn observe_workflow_pane_exit(&mut self, pane_id: crate::layout::PaneId) -> bool {
        let mut changed = false;
        if let Some(pane) = self.workflow_public_pane_id(pane_id) {
            changed |= self.end_workflow_interrogation(&pane);
        }
        changed
            | self.handle_workflow_app_event(WorkflowAppEvent::NodePaneExited {
                pane_id,
                code: None,
            })
    }

    /// `workflow.node.report` (§4.3 signal 1). The token is authenticated
    /// against the one the binder minted for that node — the engine never sees
    /// the mint and so cannot re-check it.
    pub(crate) fn report_workflow_node(
        &mut self,
        path: &str,
        token: &str,
        result: Option<serde_json::Value>,
    ) -> Result<bool, observe::ReportRejected> {
        let expected = self
            .workflow
            .node_token(&InstancePath::new(path.trim()))
            .cloned();
        let input = observe::node_self_report(path, token, expected.as_ref(), result)?;
        Ok(self.apply_workflow_engine_input(input))
    }

    /// Records a spawned node's pane binding, which is what moves it to
    /// `Running`. Called by `src/workflow/binding/spawn.rs` once a pane exists.
    pub(crate) fn bind_workflow_node(&mut self, path: &InstancePath, binding: NodeBinding) -> bool {
        let now = Instant::now();
        let effects = self.workflow.bind_node(path, binding, now);
        let mut changed = self.dispatch_workflow_effects(effects);
        changed |= self.drive_workflow_spawns();
        changed
    }

    pub(crate) fn cancel_workflow_run(&mut self) -> bool {
        self.apply_workflow_engine_input(EngineInput::CancelRun)
    }

    fn workflow_public_pane_id(&self, pane_id: crate::layout::PaneId) -> Option<PublicPaneId> {
        let (ws_idx, _) = self.find_pane(pane_id)?;
        self.public_pane_id(ws_idx, pane_id).map(PublicPaneId::new)
    }

    fn dispatch_workflow_effects(&mut self, effects: Vec<RunEffect>) -> bool {
        let mut changed = false;
        // A run whose node set grew has to say so on the run stream. The engine
        // emits `RunUpdated` only from `pause()` and `resume()`, so before this
        // an expansion moved `nodes_total` from 2 to 12 without a single
        // `workflow.run.updated` — a subscriber tracking the run could see the
        // eleven `workflow.node.created` events and the final
        // `workflow.run.finished`, and nothing in between that carried the new
        // total.
        //
        // One synthetic event per effect batch that materialised nodes, not one
        // per node: a run start creates every static node in one batch, and a
        // client wants the settled total, not N intermediate ones.
        let mut grown_run: Option<RunId> = None;
        for effect in effects {
            match &effect {
                // The engine's own run update already re-reads the run
                // projection, so it carries the grown `nodes_total` itself.
                // Emitting a synthetic one immediately before it would be two
                // events for one fact.
                RunEffect::Emit(WorkflowEvent::RunUpdated { .. }) => {
                    grown_run = None;
                }
                // `RunFinished` is the last thing a subscriber hears about the
                // run, so the growth has to be announced before it — a client
                // watching only `workflow.run.updated` must not have to infer
                // the total from the terminal event.
                RunEffect::Emit(WorkflowEvent::RunFinished { .. }) => {
                    if let Some(run) = grown_run.take() {
                        changed |= self.emit_workflow_run_growth(&run);
                    }
                }
                _ => {}
            }
            let created_run = match &effect {
                RunEffect::Emit(WorkflowEvent::NodeCreated { run, .. }) => Some(run.clone()),
                _ => None,
            };
            changed |= self.dispatch_workflow_effect(effect);
            if let Some(run) = created_run {
                grown_run = Some(run);
            }
        }
        if let Some(run) = grown_run.take() {
            changed |= self.emit_workflow_run_growth(&run);
        }
        self.flush_workflow_writes();
        self.mirror_workflow_run_graph();
        changed |= self.announce_workflow_progress();
        self.prune_run_history_if_settled();
        changed
    }

    /// Fires retention once the run is genuinely over (§4 D12).
    ///
    /// "Over" means closed **and** its epilogue resolved (`Done`, `GaveUp`, or
    /// absent): pruning between `run.finished` and the summary landing could
    /// delete the very run whose summary is still being written. Fire-and-forget
    /// on the store task, and **never on a read path** — opening the run browser
    /// must not mutate history.
    ///
    /// Once per run: the guard is the run's own id, so the twenty ticks a
    /// finished run still receives while its panes close do not re-run the
    /// sweep twenty times.
    fn prune_run_history_if_settled(&mut self) {
        #[cfg(feature = "workflow")]
        {
            if self.workflow.is_live() || self.workflow.epilogue_pending() {
                return;
            }
            let Some(run) = self.workflow.active_run() else {
                return;
            };
            let workflow = run.workflow_id.clone();
            let run_id = run.run_id.clone();
            if !self.workflow.mark_history_pruned(&run_id) {
                return;
            }
            let keep = self.workflow.policy().retention_runs;
            self.workflow_store.submit(move |cx| {
                let pruned = cx.block_on(cx.store().prune_run_history(&workflow, keep))?;
                if pruned > 0 {
                    tracing::info!(
                        workflow = %workflow,
                        pruned,
                        keep,
                        "pruned run history down to the retention limit"
                    );
                }
                Ok(())
            });
        }
    }

    /// Tells the user what the settled graph now says. The engine notices only
    /// the failures it is in the middle of; a run that succeeds, is cancelled,
    /// or pauses reaches its terminal shape through `settle` with nothing said
    /// at all, which is why a finished run used to leave the screen
    /// byte-identical to an idle one.
    fn announce_workflow_progress(&mut self) -> bool {
        let announcements = self.workflow.take_pending_announcements();
        let announced = !announcements.is_empty();
        for notice in announcements {
            self.show_workflow_notice(notice);
        }
        announced
    }

    /// The bound `keys.open_workflow_dag` was pressed with nothing to show.
    pub(crate) fn notify_no_workflow_run(&mut self) {
        self.show_workflow_notice(UserNotice {
            level: NoticeLevel::Info,
            run: None,
            path: None,
            message: "no workflow run on this server — start one with kvx workflow run start"
                .to_string(),
        });
    }

    /// Copies the engine's graph into `AppState` so the DAG overlay — which is
    /// computed and drawn from `AppState` alone — sees the live run. Once per
    /// effect batch, not per effect: a batch is one engine input, and the graph
    /// is tens of nodes.
    fn mirror_workflow_run_graph(&mut self) {
        let graph = self.workflow.graph().cloned();
        // Labels are mirrored **per instance**, keyed by instance path, because
        // a generation cut from one template shares a key: keying by key drew
        // six identically labelled boxes for six children the proposing node had
        // deliberately named apart. The definition's labels are still mirrored
        // under their keys, as the fallback for a node the graph does not carry
        // — a static node's path *is* its key, so the two never disagree.
        let mut labels: std::collections::HashMap<String, String> = self
            .workflow
            .definition()
            .map(|kvdag| {
                kvdag
                    .nodes
                    .iter()
                    .map(|node| (node.key.as_str().to_string(), node.label.clone()))
                    .collect()
            })
            .unwrap_or_default();
        if let (Some(graph), Some(definition)) = (self.workflow.graph(), self.workflow.definition())
        {
            for node in &graph.nodes {
                labels.insert(
                    node.path.to_string(),
                    workflow_node_label(definition, node).to_string(),
                );
            }
        }
        // A refused delivery lives on the engine, not in the run graph, so it
        // is mirrored the same way the labels are — otherwise the only surface
        // that shows a node's state cannot show that its last steer was
        // dropped.
        let delivery_failures = self
            .workflow
            .graph()
            .map(|graph| {
                graph
                    .nodes
                    .iter()
                    .filter_map(|node| {
                        let failure = self.workflow.node_delivery_failure(&node.path)?;
                        Some((
                            node.path.to_string(),
                            format!("{}: {}", failure.method, failure.reason),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // A growth limit lives on the runtime for the same reason a refused
        // delivery does — no column carries it — so the overlay gets it the
        // same way: one banner for the run and one notice on the node that
        // proposed (§4 D11, WS-G).
        let growth_banner = self.workflow.last_growth_limit().map(format_growth_banner);
        let growth_notices = self
            .workflow
            .graph()
            .map(|graph| {
                graph
                    .nodes
                    .iter()
                    .filter_map(|node| {
                        let limit = self.workflow.node_growth_limit(&node.path)?;
                        Some((node.path.to_string(), format_growth_notice(limit)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.state.set_workflow_run_graph(graph);
        self.state.set_workflow_node_labels(labels);
        self.state.set_workflow_delivery_failures(delivery_failures);
        self.state
            .set_workflow_growth(growth_banner, growth_notices);
    }

    /// Hands whatever `RunEffect::Persist` queued to the store. Every effect
    /// batch ends here, so the journal never lags the in-memory graph by more
    /// than one batch. With the `workflow` feature off there is no store, and
    /// the queue is simply left to its own budget.
    fn flush_workflow_writes(&mut self) {
        #[cfg(feature = "workflow")]
        self.drain_workflow_store_writes();
    }

    fn dispatch_workflow_effect(&mut self, effect: RunEffect) -> bool {
        match effect {
            // The engine drives spawns through `admissions()` rather than this
            // effect, because it cannot mint a run directory, a session id, or a
            // token. The arm still spawns, so an engine that starts emitting it
            // does not silently do nothing.
            RunEffect::SpawnNode { path, spec } => {
                debug!(path = %path, runner = ?spec.runner, "workflow spawn requested");
                match self.spawn_workflow_node(&path) {
                    Ok(()) => true,
                    Err(reason) => self.fail_workflow_spawn(&path, &reason),
                }
            }
            RunEffect::PromptNode { pane, text } => self.deliver_workflow_text(&pane, &text),
            RunEffect::SendKeys { pane, keys } => self.deliver_workflow_keys(&pane, keys),
            RunEffect::ClosePane { pane } => {
                let response = self.runtime_pane_close("workflow.node.close", pane.to_string());
                // A close is not a delivery into the node's turn, so it is
                // logged and kept but never surfaced as a delivery marker.
                let _ = self.record_workflow_api_error("pane.close", &pane, &response);
                true
            }
            RunEffect::Persist(write) => {
                self.workflow.queue_write(*write);
                false
            }
            RunEffect::Emit(event) => {
                self.emit_workflow_event(event);
                true
            }
            RunEffect::Notify(notice) => {
                self.show_workflow_notice(notice);
                true
            }
        }
    }

    /// `04` §5: an `Agent` node is steered with `agent.prompt`, which verifies
    /// the live foreground process still matches the expected agent and handles
    /// the Enter-submit race; a `Command` node has no agent to verify, so its
    /// pane takes the text raw.
    fn deliver_workflow_text(&mut self, pane: &PublicPaneId, text: &str) -> bool {
        let (method, label) = match self.workflow.runner_for_pane(pane) {
            Runner::Agent => (
                Method::AgentPrompt(AgentPromptParams {
                    target: pane.to_string(),
                    text: text.to_string(),
                    wait: None,
                }),
                "agent.prompt",
            ),
            Runner::Command => (
                Method::PaneSendText(PaneSendTextParams {
                    pane_id: pane.to_string(),
                    text: text.to_string(),
                }),
                "pane.send_text",
            ),
        };
        let response = self.dispatch_api_request("workflow.node.deliver", method);
        self.surface_workflow_delivery_failure(label, pane, &response);
        true
    }

    /// The interrupt half of `04` §5. `agent.send_keys` verifies the pane still
    /// hosts the expected agent before writing, which is exactly right for a
    /// `Runner::Agent` node and exactly wrong for a `Runner::Command` one: a
    /// plain process is by construction not a detected agent, so that path
    /// answers `agent_not_ready` and the keystroke never reaches the PTY. A
    /// command node's keys go through `pane.send_keys`, which writes to the
    /// terminal itself — that is what makes `ctrl+c` a real SIGINT.
    fn deliver_workflow_keys(&mut self, pane: &PublicPaneId, keys: Vec<String>) -> bool {
        let (method, label) = match self.workflow.runner_for_pane(pane) {
            Runner::Agent => (
                Method::AgentSendKeys(AgentSendKeysParams {
                    target: pane.to_string(),
                    keys,
                }),
                "agent.send_keys",
            ),
            Runner::Command => (
                Method::PaneSendKeys(PaneSendKeysParams {
                    pane_id: pane.to_string(),
                    keys,
                }),
                "pane.send_keys",
            ),
        };
        let response = self.dispatch_api_request("workflow.node.send_keys", method);
        self.surface_workflow_delivery_failure(label, pane, &response);
        true
    }

    /// A pane delivery the runtime refused, made visible instead of only
    /// logged. The API caller that asked for it learns through
    /// `take_delivery_failure`, but the DAG view's steer row and the engine's
    /// own re-prompts have no such caller — before this, a steer that never
    /// reached the process looked exactly like one that did. The engine turns
    /// it into a journalled run event, a node-level marker, and a user notice.
    fn surface_workflow_delivery_failure(
        &mut self,
        method: &str,
        pane: &PublicPaneId,
        response: &str,
    ) {
        let Some(failure) = self.record_workflow_api_error(method, pane, response) else {
            return;
        };
        let Some(path) = self.workflow.node_path_for_pane(pane) else {
            return;
        };
        let reason = format!("{}: {}", failure.code, failure.message);
        let effects = self.workflow.note_delivery_failure(&path, method, &reason);
        self.dispatch_workflow_effects(effects);
    }

    /// An in-process API call answers with the same envelope a client would
    /// get, so a failed delivery is logged *and* kept: the caller that asked for
    /// the delivery has to be able to tell that it did not happen.
    /// Returns the failure it recorded, so a caller that also has to surface it
    /// does not have to re-parse the envelope.
    fn record_workflow_api_error(
        &mut self,
        method: &str,
        pane: &PublicPaneId,
        response: &str,
    ) -> Option<DeliveryFailure> {
        let failure = workflow_api_error(method, pane, response)?;
        warn!(
            method,
            pane = %pane,
            code = %failure.code,
            message = %failure.message,
            "workflow effect delivery failed"
        );
        self.workflow.record_delivery_failure(failure.clone());
        Some(failure)
    }

    /// Puts every admitted node into a pane through `workflow::binding::spawn`,
    /// then records the binding, which is what moves the node to `Running`.
    fn drive_workflow_spawns(&mut self) -> bool {
        let claimed = self.workflow.claim_spawns();
        let mut changed = false;
        for path in claimed {
            match self.spawn_workflow_node(&path) {
                Ok(()) => changed = true,
                Err(reason) => changed |= self.fail_workflow_spawn(&path, &reason),
            }
        }
        changed
    }

    /// A failed spawn keeps the node `Ready` and retries it on a later
    /// admission until the attempt budget runs out; the give-up then goes back
    /// to the engine as [`EngineInput::SpawnFailed`], which takes the node out
    /// of the ready set so §3.2 can pause the run. Both the retry and the
    /// give-up are surfaced: a node that silently never starts is the failure
    /// this design exists to prevent.
    fn fail_workflow_spawn(&mut self, path: &InstancePath, reason: &str) -> bool {
        let attempts = self.workflow.record_spawn_failure(path);
        let retrying = attempts < SPAWN_ATTEMPT_BUDGET;
        warn!(path = %path, attempts, retrying, reason, "workflow node spawn failed");
        if retrying {
            self.workflow.release_spawn_claim(path);
        }
        let run = self.workflow.active_run().map(|run| run.run_id.clone());
        self.show_workflow_notice(UserNotice {
            level: if retrying {
                NoticeLevel::Warning
            } else {
                NoticeLevel::Error
            },
            run,
            path: Some(path.clone()),
            message: if retrying {
                format!("node spawn failed, retrying: {reason}")
            } else {
                format!("node spawn failed: {reason}")
            },
        });
        if !retrying {
            // Deliberately not `apply_workflow_engine_input`: this already runs
            // inside `drive_workflow_spawns`, and re-driving spawns from here
            // would recurse once per node whose spawn is failing.
            let effects = self.workflow.apply(
                EngineInput::SpawnFailed {
                    path: path.clone(),
                    reason: reason.to_string(),
                },
                Instant::now(),
            );
            self.dispatch_workflow_effects(effects);
        }
        true
    }

    /// One node → one pane (`04` §4.1 and §4.2): node directory, argv/env,
    /// `split_pane_argv_command`, managed-agent confirmation for `Runner::Agent`
    /// only, then the binding.
    fn spawn_workflow_node(&mut self, path: &InstancePath) -> Result<(), String> {
        // A node that already holds a pane must never be spawned again: the
        // second binding would orphan the first pane, leaving a live process no
        // status refers to.
        if self
            .workflow
            .node(path)
            .is_some_and(|node| node.binding.is_some())
        {
            return Ok(());
        }
        let ws_idx = self
            .workflow_run_workspace()
            .ok_or_else(|| "no workspace to host the node's pane".to_string())?;
        let target_pane = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.focused_pane_id())
            .ok_or_else(|| "the run's workspace has no pane to split".to_string())?;
        let cwd = self.workflow_node_cwd(ws_idx);
        let plan = self.workflow_spawn_plan(path, cwd)?;

        spawn::materialise_node_dir(
            &plan.layout,
            &spawn::NodeDirPlan {
                task_markdown: &plan.task_markdown,
                output_schema: &plan.output_schema,
                inputs: &plan.inputs,
            },
        )
        .map_err(|err| format!("the node directory could not be written: {err}"))?;

        let (rows, cols) = self.state.estimate_pane_size();
        let context = spawn::PaneSpawnContext {
            target_pane,
            direction: Direction::Horizontal,
            rows,
            cols,
            scrollback_limit_bytes: self.state.pane_scrollback_limit_bytes,
            host_terminal_theme: self.state.host_terminal_theme,
            host_terminal_appearance: self.state.host_terminal_appearance,
            // A run fans out over several panes; stealing focus per node would
            // fight the user, who watches the run through the DAG view.
            focus: false,
        };
        let workspace = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .ok_or_else(|| "the run's workspace disappeared".to_string())?;
        let (tab_idx, new_pane) = spawn::spawn_node_pane(workspace, context, &plan.spec)
            .map_err(|err| format!("{}: {err}", err.code()))?;

        let mut terminal = new_pane.terminal;
        let terminal_id = terminal.id.clone();
        // The pane's own label, set through the same mechanism `pane.rename`
        // uses. A `Runner::Command` node emits no OSC title, so this is the
        // only thing that ever names its pane.
        terminal.set_manual_label(plan.pane_title.clone());
        spawn::confirm_managed_agent(&mut terminal, &plan.spec, Instant::now());
        self.terminal_runtimes
            .insert(terminal_id.clone(), new_pane.runtime);
        self.state
            .remove_alias_shadowed_by_new_pane(new_pane.pane_id);
        self.state.terminals.insert(terminal_id.clone(), terminal);
        self.schedule_session_save();
        if let Some(pane) = self.pane_info(ws_idx, new_pane.pane_id) {
            self.emit_event(EventEnvelope {
                event: EventKind::PaneCreated,
                data: EventData::PaneCreated { pane },
            });
        }
        self.emit_layout_updated_event(ws_idx, tab_idx);

        let pane_id = self
            .public_pane_id(ws_idx, new_pane.pane_id)
            .ok_or_else(|| "the node's pane has no public id".to_string())?;
        let binding = NodeBinding {
            pane_id: PublicPaneId::new(pane_id),
            terminal_id,
            agent_session_id: plan.spec.agent_session_id.clone(),
            transcript_path: plan.transcript_path,
            node_dir: plan.layout.root.clone(),
            cwd: plan.spec.cwd.clone(),
        };
        self.workflow.record_node_token(path, plan.spec.token);
        self.bind_workflow_node(path, binding);
        Ok(())
    }

    /// Creates an interrogation's pane and records it (`07-phase3-plan.md` §4
    /// D7).
    ///
    /// Every precondition was checked by the handler before this was called, so
    /// the only failures left are the ones creating a pane can produce. That
    /// ordering is the "never a silent pane" guarantee: a refusal happens with
    /// nothing created, and a failure here is reported with nothing left behind
    /// either.
    pub(crate) fn spawn_interrogation(
        &mut self,
        request: InterrogationRequest,
    ) -> Result<crate::api::schema::WorkflowInterrogationInfo, InterrogationSpawnFailed> {
        let ws_idx = self
            .interrogation_workspace(request.workspace_id.as_deref())
            .ok_or_else(|| {
                InterrogationSpawnFailed::new("there is no workspace to host the pane")
            })?;
        let target_pane = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.focused_pane_id())
            .ok_or_else(|| InterrogationSpawnFailed::new("the workspace has no pane to split"))?;

        let id = interrogate::mint_interrogation_id();
        // Pre-assigned rather than learned: the step-0 spike verified that
        // `--session-id` combines with `--resume --fork-session`, which is §4
        // D7's preferred path — the row carries the fork's identity from the
        // moment it is created. The async learn below stays as belt-and-braces
        // for the case where a future `claude` stops honouring it (§6 A9).
        let forked_session_id = interrogate::mint_forked_session_id();
        let reconstructed = matches!(request.seed, InterrogationSeed::Reconstructed { .. });

        let (cwd, argv, seeded_from_seq, source_transcript) = match &request.seed {
            InterrogationSeed::Resumed {
                cwd,
                transcript_path,
            } => (
                cwd.clone(),
                interrogate::resumed_argv(&request.source_session_id, &forked_session_id),
                None,
                Some(transcript_path.clone()),
            ),
            InterrogationSeed::Reconstructed {
                cwd,
                checkpoint_seq,
                summary,
                payload,
                original_task,
                label,
            } => {
                let dir = interrogate::interrogation_dir(
                    &spawn::run_dir(&spawn::runs_root(), &request.run),
                    &id,
                );
                let markdown = interrogate::ReconstructedSeed {
                    workflow_name: &request.workflow_name,
                    run: request.run.as_str(),
                    path: request.path.as_str(),
                    label,
                    checkpoint_seq: *checkpoint_seq,
                    summary,
                    payload,
                    original_task: original_task.as_deref(),
                    note: &request.note,
                }
                .render();
                let seed_file = interrogate::materialise_interrogation_dir(&dir, &markdown)
                    .map_err(|err| {
                        InterrogationSpawnFailed::new(format!(
                            "the interrogation seed could not be written: {err}"
                        ))
                    })?;
                (
                    cwd.clone(),
                    interrogate::reconstructed_argv(&forked_session_id, &dir, &seed_file),
                    Some(*checkpoint_seq),
                    None,
                )
            }
        };

        // D6/§0.5's estimate formula, over the **spawn** cwd and the **minted**
        // session id — the fork's own transcript, not the source's. Recorded at
        // spawn so a later reader can find the forked transcript the same way
        // it finds a node's.
        let transcript_path = spawn::transcript_path(&cwd, &forked_session_id)
            .ok()
            .map(|path| path.display().to_string());

        let (rows, cols) = self.state.estimate_pane_size();
        let context = spawn::PaneSpawnContext {
            target_pane,
            direction: Direction::Horizontal,
            rows,
            cols,
            scrollback_limit_bytes: self.state.pane_scrollback_limit_bytes,
            host_terminal_theme: self.state.host_terminal_theme,
            host_terminal_appearance: self.state.host_terminal_appearance,
            // Unlike a node's pane: an interrogation is something the human just
            // asked for and is about to type into, so it takes focus.
            focus: true,
        };
        let workspace = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .ok_or_else(|| InterrogationSpawnFailed::new("the workspace disappeared"))?;
        let (tab_idx, new_pane) =
            interrogate::spawn_interrogation_pane(workspace, context, &argv, &cwd)
                .map_err(|err| InterrogationSpawnFailed::from_spawn(&err))?;

        let mut terminal = new_pane.terminal;
        let terminal_id = terminal.id.clone();
        let pane_title = interrogate::interrogation_pane_title(
            &request.workflow_name,
            request.path.as_str(),
            reconstructed,
        );
        terminal.set_manual_label(pane_title);
        // The same managed-agent confirmation a node's pane gets: an
        // interrogation *is* a `claude` process, and the detector has to know
        // that as surely as it does for a node (§3 rule 7).
        terminal.begin_managed_agent(
            interrogate::interrogation_agent_name(&request.path, &id),
            crate::detect::Agent::Claude,
            Instant::now(),
            spawn::NODE_AGENT_SETTLE_DELAY,
            spawn::NODE_AGENT_LAUNCH_WINDOW,
        );
        self.terminal_runtimes
            .insert(terminal_id.clone(), new_pane.runtime);
        self.state
            .remove_alias_shadowed_by_new_pane(new_pane.pane_id);
        self.state.terminals.insert(terminal_id, terminal);
        self.schedule_session_save();
        if let Some(pane) = self.pane_info(ws_idx, new_pane.pane_id) {
            self.emit_event(EventEnvelope {
                event: EventKind::PaneCreated,
                data: EventData::PaneCreated { pane },
            });
        }
        self.emit_layout_updated_event(ws_idx, tab_idx);

        let pane = PublicPaneId::new(
            self.public_pane_id(ws_idx, new_pane.pane_id)
                .ok_or_else(|| InterrogationSpawnFailed::new("the pane has no public id"))?,
        );
        let started_at_unix_ms = current_unix_ms();
        let live = LiveInterrogation {
            id: id.clone(),
            run: request.run.clone(),
            path: request.path.clone(),
            pane,
            source_session_id: request.source_session_id.clone(),
            forked_session_id: Some(forked_session_id),
            // The row records the transcript this fork will write, and — for a
            // resumed fork — the source it was taken from is already named by
            // `source_session_id`, so nothing is lost by carrying only one.
            transcript_path: transcript_path.or(source_transcript),
            cwd: cwd.display().to_string(),
            reconstructed,
            note: request.note.clone(),
            started_at_unix_ms,
        };
        self.workflow.queue_write(StoreWrite::InterrogationStarted {
            id: id.clone(),
            run: live.run.clone(),
            path: live.path.clone(),
            source_session_id: live.source_session_id.clone(),
            forked_session_id: live.forked_session_id.clone(),
            transcript_path: live.transcript_path.clone(),
            cwd: live.cwd.clone(),
            pane_id: live.pane.clone(),
            reconstructed,
            seeded_from_seq,
            note: live.note.clone(),
            started_at_unix_ms,
        });
        let info = wire_interrogation_info(&live, None);
        self.workflow.track_interrogation(live);
        self.flush_workflow_writes();
        self.emit_event(EventEnvelope {
            event: EventKind::WorkflowInterrogationStarted,
            data: EventData::WorkflowInterrogationStarted {
                interrogation: info.clone(),
            },
        });
        Ok(info)
    }

    /// Stamps an interrogation's end when its pane goes away.
    ///
    /// Called from the two paths that already report a pane's disappearance —
    /// `AppEvent::PaneDied` and `App::close_pane` — through
    /// [`App::observe_workflow_pane_exit`], which is why Phase 3 adds no
    /// `AppEvent` variant (§4 D7, and `07-phase3-plan.md` §0's
    /// uncommitted-diff rule).
    pub(crate) fn end_workflow_interrogation(&mut self, pane: &PublicPaneId) -> bool {
        let Some(ended) = self.workflow.end_interrogation_for_pane(pane) else {
            return false;
        };
        let ended_at_unix_ms = current_unix_ms();
        self.workflow.queue_write(StoreWrite::InterrogationUpdate {
            id: ended.id.clone(),
            forked_session_id: None,
            ended_at_unix_ms: Some(ended_at_unix_ms),
        });
        self.flush_workflow_writes();
        self.emit_event(EventEnvelope {
            event: EventKind::WorkflowInterrogationEnded,
            data: EventData::WorkflowInterrogationEnded {
                interrogation: wire_interrogation_info(&ended, Some(ended_at_unix_ms)),
            },
        });
        true
    }

    /// The interrogation half of the reconcile sweep.
    ///
    /// Deliberately a **parallel** sweep rather than an extension of
    /// [`App::reconcile_workflow_pane_bindings`] (§4 D8): that one fails the
    /// *node* whose pane vanished, and an interrogation has no node to fail —
    /// running it through there would report a dead node pane for a node that
    /// is perfectly fine. What it shares is the reason it exists: bulk closes
    /// (`handle_tab_close`, `handle_workspace_close`) drop panes without telling
    /// anyone, so the tick reconciles against the layout instead of chasing
    /// call sites.
    pub(crate) fn reconcile_interrogation_panes(&mut self) -> bool {
        let orphaned: Vec<PublicPaneId> = self
            .workflow
            .interrogation_panes()
            .into_iter()
            .filter(|pane| !self.workflow_pane_is_live(pane))
            .collect();
        let mut changed = false;
        for pane in orphaned {
            debug!(pane = %pane, "interrogation pane left the layout; stamping its end");
            changed |= self.end_workflow_interrogation(&pane);
        }
        changed
    }

    /// Surfaces, once, that end-of-run summaries are off because the argv
    /// override could not be read (E-11, strengthening §4 D2 / §6 A4).
    ///
    /// Only for the *malformed-override* case. A user who set
    /// `workflow.summary_enabled = false` chose that and does not need telling;
    /// a user whose environment variable is a typo believes their summariser is
    /// bound and would otherwise find out by noticing summaries never appear.
    fn notice_summaries_disabled_once(&mut self) {
        if !self.workflow.claim_summary_disabled_notice() {
            return;
        }
        self.show_workflow_notice(UserNotice {
            level: NoticeLevel::Warning,
            run: None,
            path: None,
            message: format!(
                "summaries disabled: {SUMMARY_COMMAND_ENV_VAR} is not a valid argv array"
            ),
        });
    }

    /// Writes back the transcript path a workflow pane's session report carried
    /// (§4 D6), replacing the pre-launch estimate.
    ///
    /// `spawn::transcript_path` derives the stored path from
    /// `(claude_dir, slug(cwd), session_id)` before the process starts, and its
    /// own docstring says to prefer the reported path "once it arrives" —
    /// nothing ever read it back, so a node whose real transcript lived
    /// elsewhere answered `transcript_unavailable` no matter what was on disk
    /// (§0.5). This is the read-back.
    ///
    /// A **strict no-op** for any pane that is not a workflow node's: the two
    /// call sites are the general-purpose `pane.report_agent*` handlers, which
    /// every agent pane in the session reaches, and the workflow runtime has no
    /// business reacting to panes it does not own.
    pub(crate) fn observe_workflow_transcript_path(
        &mut self,
        pane_id: crate::layout::PaneId,
        source: &str,
        agent_label: &str,
        agent_session_path: Option<&str>,
    ) -> bool {
        // Cheapest discriminator first: almost every report reaching here is for
        // a pane no run owns.
        let Some(reported) =
            observe::reported_transcript_path(source, agent_label, agent_session_path)
        else {
            return false;
        };
        let Some(pane) = self.workflow_public_pane_id(pane_id) else {
            return false;
        };
        let Some(path) = self.workflow.node_path_for_pane(&pane) else {
            return false;
        };
        let Some(effect) = self.workflow.record_transcript_path(&path, reported) else {
            return false;
        };
        debug!(path = %path, "a workflow node's session reported its transcript path");
        // Dispatched rather than queued directly: this is a `RunEffect` like any
        // other, and the batch dispatcher is what drains it to the store task.
        self.dispatch_workflow_effects(vec![effect])
    }

    /// Learns an interrogation's forked session id from its pane's session
    /// report (§6 A9).
    ///
    /// Belt-and-braces: the id is pre-assigned at spawn because the step-0 spike
    /// verified `--session-id` combines with `--resume --fork-session`. This
    /// exists so a future `claude` that stops honouring the pre-assignment
    /// degrades to a late-but-correct record rather than a permanently null one.
    pub(crate) fn observe_interrogation_session_id(
        &mut self,
        pane: &PublicPaneId,
        session_id: &str,
    ) -> bool {
        let Some(learned) = self.workflow.learn_forked_session_id(pane, session_id) else {
            return false;
        };
        self.workflow.queue_write(StoreWrite::InterrogationUpdate {
            id: learned.id,
            forked_session_id: learned.forked_session_id,
            ended_at_unix_ms: None,
        });
        self.flush_workflow_writes();
        true
    }

    /// Which workspace hosts an interrogation's pane: the source run's own when
    /// it recorded one and it still resolves, else the active workspace.
    ///
    /// Unlike a node's pane, falling back is right here: the run being
    /// interrogated may have finished in a workspace the user has since closed,
    /// and refusing to open the pane at all would be a worse answer than opening
    /// it where the user is looking.
    fn interrogation_workspace(&self, workspace_id: Option<&str>) -> Option<usize> {
        let named = workspace_id.and_then(|workspace_id| self.parse_workspace_id(workspace_id));
        named.or(self.state.active).filter(|ws_idx| {
            self.state
                .workspaces
                .get(*ws_idx)
                .is_some_and(|workspace| !workspace.tabs.is_empty())
        })
    }

    /// The run's own workspace when it named one, else the active workspace.
    fn workflow_run_workspace(&self) -> Option<usize> {
        let named = self
            .workflow
            .active_run()
            .and_then(|run| run.workspace_id.clone())
            .and_then(|workspace_id| self.parse_workspace_id(&workspace_id));
        named.or(self.state.active).filter(|ws_idx| {
            self.state
                .workspaces
                .get(*ws_idx)
                .is_some_and(|workspace| !workspace.tabs.is_empty())
        })
    }

    /// [`Self::workflow_node_cwd`] for callers outside this module — the
    /// interrogation handler's fallback directory for a reconstructed session
    /// whose node directory is gone.
    pub(crate) fn workflow_node_cwd_for(&self, ws_idx: usize) -> PathBuf {
        self.workflow_node_cwd(ws_idx)
    }

    /// Phase 1 runs every node in the workspace's own directory.
    /// `Isolation::Worktree` routes through `src/worktree.rs` in a later phase.
    fn workflow_node_cwd(&self, ws_idx: usize) -> PathBuf {
        let follow = self.state.workspaces.get(ws_idx).and_then(|workspace| {
            workspace.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
        });
        self.resolve_new_terminal_cwd(follow)
    }

    /// Everything the binder needs for one node, built from the run graph and
    /// the definition. Reads state and touches no runtime, so the argv, the
    /// node directory layout, and the rendered `task.md` are all testable
    /// without spawning anything.
    fn workflow_spawn_plan(
        &self,
        path: &InstancePath,
        cwd: PathBuf,
    ) -> Result<NodeSpawnPlan, String> {
        let run = self
            .workflow
            .active_run()
            .ok_or_else(|| "no active run".to_string())?;
        let graph = self
            .workflow
            .graph()
            .ok_or_else(|| "the run has no graph".to_string())?;
        let definition = self
            .workflow
            .definition()
            .ok_or_else(|| "the run's kvdag definition is not installed".to_string())?;
        let node = graph
            .node_by_path(path)
            .ok_or_else(|| format!("no node at {path}"))?;
        // The epilogue has no kvdag node behind it, so the lookup below would
        // fail for it. Its whole plan comes from the engine instead (§3 rule 2),
        // which is also the single authority for whether it is bound to an agent
        // or to a command (defect D-1).
        if is_reserved_path(path.as_str()) {
            return self.epilogue_spawn_plan(run, graph, node, cwd);
        }
        let spec_node = definition
            .node(&node.key)
            .ok_or_else(|| format!("the definition has no node {}", node.key))?;

        // Every upstream that fired into a port, not just the last one. §3.4's
        // inherited fan-in gives one port a whole generation of sources, and
        // keying by port alone made each child's result overwrite its sibling's
        // — `collect` saw one report and the other N-1 were lost with nothing
        // said on any surface.
        let mut inputs: BTreeMap<String, Vec<(RunNodeIdx, spawn::PortContribution)>> =
            BTreeMap::new();
        for edge in graph.edges.iter().filter(|edge| edge.to == node.idx) {
            let (Some(port), true) = (edge.port.as_ref(), edge.fired) else {
                continue;
            };
            let Some(from) = graph.node(edge.from) else {
                continue;
            };
            let Some(result) = from.result.as_ref() else {
                continue;
            };
            let payload = match edge.payload {
                EdgePayload::Full => result.payload.clone(),
                EdgePayload::Summary => serde_json::Value::String(result.summary.clone()),
                EdgePayload::None => continue,
            };
            inputs.entry(port.clone()).or_default().push((
                from.idx,
                spawn::PortContribution {
                    from: from.path.to_string(),
                    label: workflow_node_label(definition, from).to_string(),
                    payload,
                },
            ));
        }
        // Node-creation order — the order a generation was proposed in, and the
        // order the DAG lays it out. Sorting by instance path would read
        // `worker/10` before `worker/2`, and edge order alone is an
        // implementation detail of how the graph was built.
        let inputs: BTreeMap<String, Vec<spawn::PortContribution>> = inputs
            .into_iter()
            .map(|(port, mut contributions)| {
                contributions.sort_by_key(|(idx, _)| idx.0);
                (
                    port,
                    contributions
                        .into_iter()
                        .map(|(_, contribution)| contribution)
                        .collect(),
                )
            })
            .collect();

        let mut slots: BTreeMap<String, String> = BTreeMap::new();
        for arg in &definition.args {
            if let Some(default) = arg.default.as_ref() {
                slots.insert(arg.name.clone(), default.clone());
            }
        }
        for (name, value) in &run.args {
            slots.insert(name.clone(), value.clone());
        }
        // An inbound port beats a run argument of the same name: the graph's own
        // data flow is the more specific source.
        for (port, contributions) in &inputs {
            slots.insert(port.clone(), spawn::port_slot_text(contributions));
        }
        // The proposal's `--input k=v` wins over both (`06-phase2-plan.md` §4
        // D3): it is the one channel by which a proposing node can give each of
        // its children different work, and it was already validated against this
        // template's declared slots when the proposal was accepted.
        for (name, value) in &node.inputs {
            slots.insert(name.clone(), value.clone());
        }

        let contract = match spec_node.system_contract.as_deref() {
            Some(node_contract) if !node_contract.trim().is_empty() => {
                format!("{}\n\n{node_contract}", definition.contract)
            }
            _ => definition.contract.clone(),
        };
        let prompt = spawn::fill_slots(&spec_node.prompt_template, &slots);
        // The layout is resolved before the task document, not after: every
        // karvex-owned path `task.md` names is rendered from it, so the node is
        // told where its files actually are rather than where they would be if
        // its cwd were its node directory.
        let run_dir = spawn::run_dir(&spawn::runs_root(), &run.run_id);
        let layout = spawn::NodeDirLayout::for_node(&run_dir, path);
        let input_ports: Vec<spawn::TaskInputPort> = inputs
            .iter()
            .map(|(port, contributions)| spawn::TaskInputPort {
                port: port.clone(),
                sources: task_input_sources(&layout, port, contributions),
            })
            .collect();
        let node_label = workflow_node_label(definition, node);
        let task_markdown = spawn::TaskDocument {
            // The instance's own label, not the template's: a fan-out's whole
            // point is that its children are told apart, and `task.md`'s title
            // is the first place a teammate reads its own name.
            label: node_label,
            role: &spec_node.role,
            contract: &contract,
            prompt: &prompt,
            input_ports: &input_ports,
            // §4 D21: two lines pointing at the run's one context file, and
            // absent entirely when the run has none — which is what keeps every
            // Phase 1–2 `task.md` byte-identical (§7 R-7).
            prior_runs: run.prior_runs_path.as_deref(),
            node_dir: &layout.root,
        }
        .render();

        let agent_session_id = spawn::derive_agent_session_id(&run.run_id, path, node.attempt);
        let transcript_path = spawn::transcript_path(&cwd, &agent_session_id)
            .map_err(|err| format!("the node's transcript path is unknown: {err}"))?;
        let label = workflow_agent_name(graph, definition, node);
        // The pane title names the workflow and the node, so a run's splits are
        // told apart from each other and from the user's own panes. The
        // instance's label is what the author — or the proposing node — called
        // *this* node; the key is the honest fallback.
        let node_title = node_label;
        let pane_title = spawn::node_pane_title(
            self.state
                .workflow_run_presentation()
                .workflow_name
                .as_str(),
            node_title,
        );

        Ok(NodeSpawnPlan {
            spec: SpawnSpec {
                run_id: run.run_id.clone(),
                path: path.clone(),
                label,
                runner: spec_node.runner,
                command: spec_node.command.clone(),
                assignment: node.assignment,
                agent_session_id,
                node_dir: layout.root.clone(),
                cwd,
                isolation: spec_node.isolation,
                contract,
                seed_prompt: spawn::seed_prompt_for(&layout.root),
                token: spawn::mint_node_token(),
            },
            output_schema: spec_node.output_schema.clone(),
            layout,
            task_markdown,
            inputs,
            transcript_path,
            pane_title,
        })
    }

    /// The end-of-run summariser's spawn plan (§4 D1, D2, §3 rule 2).
    ///
    /// Every field that would come from a kvdag node comes from the engine's
    /// [`crate::workflow::engine::Engine::summary_task_spec`] instead — task
    /// text, output schema, **and the argv override**. The override is read from
    /// the spec and never from the environment here: two readers of
    /// `KARVEX_WORKFLOW_SUMMARY_COMMAND` is exactly how the engine's signal
    /// gating and the binder's argv end up disagreeing about what the
    /// summariser is (defect D-1).
    fn epilogue_spawn_plan(
        &self,
        run: &ActiveRun,
        graph: &RunGraph,
        node: &RunNode,
        cwd: PathBuf,
    ) -> Result<NodeSpawnPlan, String> {
        let spec = self
            .workflow
            .engine()
            .summary_task_spec()
            .ok_or_else(|| "the run has no epilogue to spawn".to_string())?;
        let runner = graph
            .epilogue
            .map(|state| state.runner)
            .ok_or_else(|| "the run graph has no epilogue state".to_string())?;
        let run_dir = spawn::run_dir(&spawn::runs_root(), &run.run_id);
        let layout = spawn::NodeDirLayout::for_node(&run_dir, &node.path);
        let agent_session_id =
            spawn::derive_agent_session_id(&run.run_id, &node.path, node.attempt);
        let transcript_path = spawn::transcript_path(&cwd, &agent_session_id)
            .map_err(|err| format!("the summariser's transcript path is unknown: {err}"))?;
        // Rendered before the plan is built, for the same reason an authored
        // node's is: the document names every karvex-owned path from the node's
        // own directory, so the layout has to exist first. The epilogue is a
        // node like any other here — its `result.json` is watched in its own
        // node directory, and its cwd is the workspace, not that directory.
        let task_markdown = epilogue_task_markdown(&spec, &layout.root);
        Ok(NodeSpawnPlan {
            spec: SpawnSpec {
                run_id: run.run_id.clone(),
                path: node.path.clone(),
                label: spec.label.clone(),
                runner,
                command: spec.command.clone(),
                assignment: node.assignment,
                agent_session_id,
                node_dir: layout.root.clone(),
                cwd,
                // The summariser reads a file the engine wrote and writes one
                // back; it has no reason to hold a worktree of its own.
                isolation: Isolation::None,
                // No workflow contract: the epilogue is karvex's node, not the
                // author's, and appending the author's contract would let a
                // workflow's own instructions rewrite what the summariser is
                // for.
                contract: String::new(),
                seed_prompt: spawn::seed_prompt_for(&layout.root),
                // A real node token: the summariser completes through the
                // ordinary `NodeSelfReport` path (§3 rule 2), unlike an
                // interrogation, which gets none because it is not a node.
                token: spawn::mint_node_token(),
            },
            // Authored by `summary_output_schema`, so it validates by
            // construction; a parse failure here would mean the engine's own
            // built-in schema is malformed, which is a bug to surface rather
            // than a run to fail silently.
            output_schema: OutputSchema::parse(spec.output_schema.clone())
                .map_err(|err| format!("the built-in summary schema is invalid: {err}"))?,
            layout,
            task_markdown,
            // The epilogue has no inbound edges: the evidence it summarises is
            // rendered into its `task.md` body by `summary_task_spec`, not
            // carried on ports.
            inputs: BTreeMap::new(),
            transcript_path,
            pane_title: spawn::node_pane_title(
                self.state
                    .workflow_run_presentation()
                    .workflow_name
                    .as_str(),
                &spec.label,
            ),
        })
    }

    /// `04` §9: a store write failure degrades the run to
    /// `persistence_degraded` and that degradation is surfaced. The run itself
    /// is unaffected — the in-memory graph is authoritative while it executes —
    /// so this is a warning, not a failure, and it is shown once per run.
    pub(crate) fn mark_workflow_persistence_degraded(&mut self) {
        if !self.workflow.mark_persistence_degraded() {
            return;
        }
        let run = self.workflow.active_run().map(|run| run.run_id.clone());
        warn!("workflow run persistence degraded: part of the journal was not written");
        self.show_workflow_notice(UserNotice {
            level: NoticeLevel::Warning,
            run,
            path: None,
            message: "the run's journal is incomplete: a durable write could not be stored"
                .to_string(),
        });
    }

    /// Shows one workflow notice through whichever delivery the user
    /// configured.
    ///
    /// This used to answer only for `ToastDelivery::Karvex` and drop the notice
    /// on the floor for every other setting, so a user who had deliberately
    /// asked for terminal or desktop notifications got *fewer* workflow
    /// notifications than the default. The escalation reuses exactly the
    /// notifier the agent-state path uses; no second notification model.
    pub(crate) fn show_workflow_notice(&mut self, notice: UserNotice) {
        let kind = match notice.level {
            NoticeLevel::Info => ToastKind::Finished,
            NoticeLevel::Warning | NoticeLevel::Error => ToastKind::NeedsAttention,
        };
        let title = match notice.path.as_ref() {
            Some(path) => format!("Workflow node {path}"),
            None => "Workflow run".to_string(),
        };
        match self.state.toast_config.delivery {
            crate::config::ToastDelivery::Karvex => {
                // One rendered slot, but a workflow batch routinely raises a
                // per-node notice immediately followed by the run-level one,
                // and assigning the slot destroyed the first (H4). `push_toast`
                // shows this one now or queues it behind whatever is showing;
                // the expiry pop in `App::expire_toast_or_show_next` is what
                // drains it. The `Terminal`/`System` arms below fire one OS
                // notification per notice and never contended for the slot, so
                // they are unchanged.
                if self.state.push_toast(ToastNotification {
                    kind,
                    title,
                    context: notice.message,
                    position: None,
                    target: None,
                }) {
                    self.arm_toast_deadline();
                }
            }
            crate::config::ToastDelivery::Terminal | crate::config::ToastDelivery::System
                if self.local_terminal_notifications =>
            {
                let notify = match self.state.toast_config.delivery {
                    crate::config::ToastDelivery::Terminal => {
                        crate::terminal_notify::show_notification
                    }
                    _ => crate::platform::show_desktop_notification,
                };
                let _ = notify(&title, Some(&notice.message));
            }
            _ => {}
        }
    }

    /// Announces the run's refreshed shape after a batch materialised nodes.
    ///
    /// Silently does nothing for a run that is no longer the active one —
    /// `workflow_run_info` is the same guard every other run event goes
    /// through, and a stale effect must not emit an event about a run this
    /// server no longer holds.
    fn emit_workflow_run_growth(&mut self, run: &RunId) -> bool {
        let Some(info) = self.workflow_run_info(run) else {
            return false;
        };
        self.emit_event(EventEnvelope {
            event: EventKind::WorkflowRunUpdated,
            data: EventData::WorkflowRunUpdated { run: info },
        });
        true
    }

    fn emit_workflow_event(&mut self, event: WorkflowEvent) {
        // C-4 / §6 A7: the run browser refreshes on run-level event arrivals
        // rather than polling, which is what keeps `src/app/runtime.rs` and
        // `app/mod.rs` out of Phase 3's diff entirely. The *trigger* lives here,
        // with the component that owns event emission; the *behaviour* lives in
        // WS-F's `refresh_workflow_runs_overlay`, which no-ops unless the
        // browser is open.
        //
        // Cloned only for the four run-level kinds, so the common case — a node
        // event, several per node per run — pays nothing. Deliberately called
        // *after* the match rather than before: `RunSummarized`'s arm flushes
        // the summary write to the store task, and a refresh that re-read the
        // list before that flush would show the row it is refreshing *for* as
        // still missing.
        let refresh = matches!(
            event,
            WorkflowEvent::RunStarted { .. }
                | WorkflowEvent::RunUpdated { .. }
                | WorkflowEvent::RunFinished { .. }
                | WorkflowEvent::RunSummarized { .. }
        )
        .then(|| event.clone());
        let envelope = match event {
            WorkflowEvent::RunStarted { run } => self.workflow_run_info(&run).map(|run| {
                (
                    EventKind::WorkflowRunStarted,
                    EventData::WorkflowRunStarted { run },
                )
            }),
            WorkflowEvent::RunUpdated { run, .. } => self.workflow_run_info(&run).map(|run| {
                (
                    EventKind::WorkflowRunUpdated,
                    EventData::WorkflowRunUpdated { run },
                )
            }),
            WorkflowEvent::RunFinished { run, .. } => self.workflow_run_info(&run).map(|run| {
                (
                    EventKind::WorkflowRunFinished,
                    EventData::WorkflowRunFinished { run },
                )
            }),
            WorkflowEvent::NodeCreated { run, path } => {
                self.workflow_node_info(&path).map(|node| {
                    (
                        EventKind::WorkflowNodeCreated,
                        EventData::WorkflowNodeCreated {
                            run_id: run.to_string(),
                            node,
                        },
                    )
                })
            }
            WorkflowEvent::NodeUpdated { run, path, .. } => {
                self.workflow_node_info(&path).map(|node| {
                    (
                        EventKind::WorkflowNodeUpdated,
                        EventData::WorkflowNodeUpdated {
                            run_id: run.to_string(),
                            node,
                        },
                    )
                })
            }
            WorkflowEvent::NodeOutputCheckpoint {
                run,
                path,
                seq,
                summary,
            } => Some((
                EventKind::WorkflowNodeOutputCheckpoint,
                EventData::WorkflowNodeOutputCheckpoint {
                    run_id: run.to_string(),
                    path: path.to_string(),
                    seq,
                    summary,
                },
            )),
            WorkflowEvent::GrowthLimited {
                run,
                path,
                template,
                limit,
                limit_value,
                requested,
                accepted,
                message,
            } => {
                // §4 D11: a growth rejection lands on three independent
                // surfaces and the toast is the fourth, optional one. It is
                // raised here rather than as a `RunEffect::Notify` because the
                // engine's `commit` deliberately does not know the proposal's
                // template — the caller that still holds it builds both the
                // wire event and the notice from the same facts.
                //
                // Recorded before it is emitted, so the run and node
                // projections below — which the DAG banner, `run show`, and
                // `node show` all read — already carry the limit by the time
                // any client asks. An event is a notification; the projection
                // is the fact, and a client that connects after the event was
                // sent must still be able to see it.
                self.workflow.record_growth_limit(
                    &path,
                    WorkflowGrowthLimit {
                        kind: wire_growth_limit_kind(limit),
                        limit_value: u32::from(limit_value),
                        requested: u32::from(requested),
                        accepted: u32::from(accepted),
                        at_unix_ms: current_unix_ms(),
                        message: message.clone(),
                    },
                );
                self.show_workflow_notice(UserNotice {
                    level: NoticeLevel::Warning,
                    run: Some(run.clone()),
                    path: Some(path.clone()),
                    message: message.clone(),
                });
                Some((
                    EventKind::WorkflowGrowthLimited,
                    EventData::WorkflowGrowthLimited {
                        run_id: run.to_string(),
                        path: path.to_string(),
                        template: template.as_str().to_string(),
                        limit: wire_growth_limit_kind(limit),
                        limit_value: u32::from(limit_value),
                        requested: u32::from(requested),
                        accepted: u32::from(accepted),
                        message,
                    },
                ))
            }
            // The engine's variant carries only enough to decide that something
            // needs re-reading; the full `run_summary` row is read back from the
            // store here, exactly as `NodeUpdated` re-reads the node's
            // projection. There is no second `run.finished` and no
            // `run.updated`: the run's status was final before the summariser
            // started (§4 D1's post-`RunFinished` contract).
            WorkflowEvent::RunSummarized { run, .. } => {
                self.stored_run_summary(&run).map(|summary| {
                    (
                        EventKind::WorkflowRunSummarized,
                        EventData::WorkflowRunSummarized {
                            run_id: run.to_string(),
                            summary,
                        },
                    )
                })
            }
        };
        if let Some((event, data)) = envelope {
            self.emit_event(EventEnvelope { event, data });
        }
        // Runs even when the envelope was `None` — a run-level event with no
        // wire projection still means the run set may have moved, and the
        // browser is showing that set.
        if let Some(event) = refresh {
            self.refresh_workflow_runs_overlay(&event);
        }
    }

    /// Reads back the summary the epilogue just wrote, for the
    /// `workflow.run.summarized` event's payload.
    ///
    /// The `StoreWrite::RunSummary` is still in the pending queue when the
    /// engine's `RunSummarized` effect is dispatched — the batch's flush runs
    /// after every effect — so this flushes first and then reads. The store
    /// thread serves its jobs in order, which is what makes the read see the
    /// write submitted immediately before it.
    ///
    /// `None` under a backlogged store task means the write had no room in the
    /// thread's queue yet: the row is not lost (it stays in the engine's own
    /// bounded queue and lands later) and `workflow.summary.get` answers it, so
    /// the miss costs the notification, not the summary. That is D10's split —
    /// the event is a notification, the read method is the contract.
    fn stored_run_summary(
        &mut self,
        run: &RunId,
    ) -> Option<crate::api::schema::WorkflowRunSummaryInfo> {
        self.flush_workflow_writes();
        #[cfg(feature = "workflow")]
        {
            let wanted = run.clone();
            match self
                .workflow_store
                .call(move |cx| cx.block_on(cx.store().get_run_summary(&wanted)))
            {
                Ok(Ok(Some(record))) => return Some(wire_run_summary_record(record)),
                Ok(Ok(None)) => {
                    warn!(run = %run, "the run summary is not readable back yet; \
                          workflow.run.summarized is not emitted for it");
                }
                Ok(Err(error)) => {
                    warn!(run = %run, error = %error, "reading back the run summary failed");
                }
                Err(unavailable) => {
                    warn!(run = %run, code = unavailable.code, "the workflow store is unavailable");
                }
            }
        }
        #[cfg(not(feature = "workflow"))]
        let _ = run;
        None
    }

    /// Wire projection of the active run. Returns `None` for a run id that is
    /// not the active one, which is what keeps a stale effect from emitting an
    /// event about a run this server no longer holds.
    pub(crate) fn workflow_run_info(&self, run: &RunId) -> Option<WorkflowRunInfo> {
        let active = self.workflow.active_run()?;
        if &active.run_id != run {
            return None;
        }
        let graph = self.workflow.graph()?;
        let (total_tokens, total_tool_uses) =
            graph.nodes.iter().fold((0_u64, 0_u64), |acc, node| {
                (
                    acc.0.saturating_add(node.usage.total_tokens),
                    acc.1.saturating_add(u64::from(node.usage.tool_uses)),
                )
            });
        Some(WorkflowRunInfo {
            run_id: active.run_id.to_string(),
            workflow_id: active.workflow_id.to_string(),
            version_id: active.version_id.to_string(),
            tier: wire_tier(active.tier),
            status: wire_run_status(graph.status),
            args: active.args.clone(),
            workspace_id: active.workspace_id.clone(),
            tab_id: active.tab_id.clone(),
            started_at_unix_ms: active.started_at_unix_ms,
            ended_at_unix_ms: active.ended_at_unix_ms,
            total_tokens,
            total_tool_uses,
            // §4 D5: these counters mean "the run's **declared** work", so the
            // engine-owned `.summary` epilogue is excluded from both. Including
            // it would make every summarised run report `nodes_done <
            // nodes_total` at the instant `run.finished` fires — the epilogue
            // has not run yet — which is an ordering lie no client could work
            // around.
            //
            // The store's `refresh_nodes_done`/`refresh_run_node_counters`
            // apply the same reserved-path filter (WS-B). The two halves are
            // inseparable: the live and durable projections are compared field
            // for field by `a_finished_run_reads_back_field_equal_to_its_live_
            // projection`, so filtering one side alone is a permanent
            // disagreement, not a partial fix.
            //
            // Counts only. The summariser stays **visible** — the DAG's own
            // `run_counts` and its node box are deliberately unfiltered (§4 D1's
            // post-`RunFinished` contract): a succeeded run showing a still-
            // working summariser is the truthful picture.
            nodes_total: u32::try_from(
                graph
                    .nodes
                    .iter()
                    .filter(|node| !is_reserved_path(node.path.as_str()))
                    .count(),
            )
            .unwrap_or(u32::MAX),
            nodes_done: u32::try_from(
                graph
                    .nodes
                    .iter()
                    .filter(|node| !is_reserved_path(node.path.as_str()))
                    .filter(|node| node.status.is_terminal())
                    .count(),
            )
            .unwrap_or(u32::MAX),
            failure: None,
            max_depth: u32::from(graph.growth.max_depth),
            max_nodes: u32::from(graph.growth.max_nodes),
            nodes_live: u32::from(GrowthLimits::live_node_count(&graph.nodes)),
            // §4 D11: the run's most recent guardrail breach, whichever node
            // hit it. `workflow_run` still has no column for it; a run read
            // back after a restart recovers the same fact from its own
            // `growth_limited` journal instead
            // (`src/workflow/store/queries.rs::growth_limits`), so the two
            // projections agree rather than one of them going quiet.
            growth_limited: self.workflow.last_growth_limit().cloned(),
            // §4 D9: the name the author gave the workflow, which the run graph
            // itself only knows as a record id. Read from the same presentation
            // state the DAG overlay heads the run with, so the live row and the
            // overlay cannot disagree about what the run is called.
            workflow_name: self
                .state
                .workflow_run_presentation()
                .workflow_name
                .to_string(),
            // §4 D21 / §4 D4: recorded on the run at start and carried on the
            // live projection too, so a client does not have to wait for the
            // run to close before it can see what history it was given and what
            // it restored from.
            context_runs: active.context_runs.iter().map(RunId::to_string).collect(),
            restore_from_run: active.restore_from_run.as_ref().map(RunId::to_string),
        })
    }

    /// Wire projection of one run node.
    pub(crate) fn workflow_node_info(&self, path: &InstancePath) -> Option<WorkflowRunNodeInfo> {
        let graph = self.workflow.graph()?;
        let node = graph.node_by_path(path)?;
        // The epilogue has **no kvdag node behind it** (§4 D5), so the lookup
        // below cannot succeed for it and its `Standard` fallback would report a
        // demand the engine never gave it: `begin_epilogue` creates the
        // summariser at `Demand::Light` and persists exactly that, so the live
        // projection said `standard` while the row said `light` for the same
        // node. That is the live-vs-durable disagreement D16 exists to catch.
        //
        // The pin is `epilogue_node_shape` inside
        // `a_restored_and_summarised_run_reads_back_field_equal_to_its_live_
        // projection`, which compares this node's `demand`/`model`/`effort`/
        // `label`/`assignment_reason` across a restart once the summariser is
        // terminal. Not `run_shape`: that projection now filters reserved paths
        // out entirely — the epilogue is still live at the instant a run reports
        // finished — so it cannot see this field at all.
        let demand = if is_reserved_path(node.path.as_str()) {
            // One constant, two readers (E-13): `begin_epilogue` resolves the
            // summariser's tier from it and writes it on the row, and this reads
            // it back for the wire. A literal here would be a second magic value
            // free to drift from the engine's.
            crate::workflow::engine::EPILOGUE_DEMAND
        } else {
            self.workflow
                .definition()
                .and_then(|definition| definition.node(&node.key))
                .map_or(Demand::Standard, |definition| definition.demand)
        };
        let binding = node.binding.as_ref();
        Some(WorkflowRunNodeInfo {
            path: node.path.to_string(),
            node_key: node.key.to_string(),
            // Resolved the same way every other naming surface resolves it:
            // instance label first, authored kvdag label second, key last.
            label: self
                .workflow
                .definition()
                .map(|definition| workflow_node_label(definition, node))
                .unwrap_or(node.key.as_str())
                .to_string(),
            parent_path: node
                .parent
                .and_then(|parent| graph.node(parent))
                .map(|parent| parent.path.to_string()),
            depth: u32::from(node.depth),
            status: wire_node_status(node.status),
            demand: wire_demand(demand),
            model: node.assignment.model.as_str().to_string(),
            effort: node.assignment.effort.as_str().to_string(),
            attempt: u32::from(node.attempt),
            pane_id: binding.map(|binding| binding.pane_id.to_string()),
            terminal_id: binding.map(|binding| binding.terminal_id.to_string()),
            agent_session_id: binding.map(|binding| binding.agent_session_id.clone()),
            cwd: binding.map(|binding| binding.cwd.display().to_string()),
            node_dir: binding.map(|binding| binding.node_dir.display().to_string()),
            started_at_unix_ms: node.started_at_unix_ms,
            ended_at_unix_ms: node.ended_at_unix_ms,
            total_tokens: node.usage.total_tokens,
            tool_uses: node.usage.tool_uses,
            duration_ms: node.usage.duration_ms,
            evidence: node
                .result
                .as_ref()
                .map(|result| wire_evidence(result.evidence)),
            succession: node.succession.as_ref().map(wire_succession),
            blocker: node.succession.as_ref().and_then(wire_blocker),
            watchdog_interventions: u32::from(node.progress.interventions),
            assignment_reason: node.assignment_reason.clone(),
            // A shared runtime fact that used to be reachable only through the
            // private TUI path; the DAG overlay reads the same map.
            delivery_failure: self
                .workflow
                .node_delivery_failure(&node.path)
                .map(|failure| format!("{}: {}", failure.method, failure.reason)),
            // The limit this node ran into *as a proposer*, so a reader can
            // attribute the run-level breach to the node that caused it
            // without replaying the event stream.
            growth_limited: self.workflow.node_growth_limit(&node.path).cloned(),
            // §4 D6: the session-reported path once the hook has reported one,
            // and the pre-launch estimate until then — the binding holds
            // whichever is current, so this reports what interrogation would
            // actually stat.
            transcript_path: binding.map(|binding| binding.transcript_path.display().to_string()),
            // §4 D4: `None` for every node this run executed; set only on a node
            // seeded by restore.
            restored_from: node.restored_from.as_ref().map(|source| {
                crate::api::schema::WorkflowRestoredFrom {
                    run_id: source.run.to_string(),
                    node_key: source.node_key.to_string(),
                    checkpoint_seq: source.checkpoint_seq,
                }
            }),
        })
    }

    /// Wire projection of the whole active run graph, which is what
    /// `workflow.run.get` returns alongside the run record.
    pub(crate) fn workflow_run_graph_info(&self, run: &RunId) -> Option<WorkflowRunGraph> {
        let active = self.workflow.active_run()?;
        if &active.run_id != run {
            return None;
        }
        let graph = self.workflow.graph()?;
        let nodes = graph
            .nodes
            .iter()
            .filter_map(|node| self.workflow_node_info(&node.path))
            .collect();
        let edges = graph
            .edges
            .iter()
            .filter_map(|edge| {
                Some(WorkflowRunEdgeInfo {
                    from: graph.node(edge.from)?.path.to_string(),
                    to: graph.node(edge.to)?.path.to_string(),
                    kind: wire_edge_kind(edge.kind),
                    condition_result: edge.condition_result,
                    fired: edge.fired,
                })
            })
            .collect();
        Some(WorkflowRunGraph { nodes, edges })
    }
}

/// Wire projection of one interrogation.
///
/// `ended_at_unix_ms` is supplied by the caller rather than carried on
/// [`LiveInterrogation`]: the tracker only ever holds *live* entries, and the
/// one moment an ended projection is needed is the instant the entry is taken
/// out of it.
pub(crate) fn wire_interrogation_info(
    interrogation: &LiveInterrogation,
    ended_at_unix_ms: Option<u64>,
) -> crate::api::schema::WorkflowInterrogationInfo {
    crate::api::schema::WorkflowInterrogationInfo {
        id: interrogation.id.to_string(),
        run_id: interrogation.run.to_string(),
        path: interrogation.path.to_string(),
        source_session_id: interrogation.source_session_id.clone(),
        forked_session_id: interrogation.forked_session_id.clone(),
        pane_id: Some(interrogation.pane.to_string()),
        reconstructed: interrogation.reconstructed,
        transcript_path: interrogation.transcript_path.clone(),
        cwd: interrogation.cwd.clone(),
        started_at_unix_ms: interrogation.started_at_unix_ms,
        ended_at_unix_ms,
        note: interrogation.note.clone(),
    }
}

/// Wire projection of one stored `run_summary` row.
///
/// Lives here rather than beside the handlers because both the
/// `workflow.summary.*` responses and the `workflow.run.summarized` event
/// project the same record, and two spellings of one mapping is how a field
/// starts appearing on one surface and not the other.
///
/// Feature-gated because its *input* is a store type: only `src/workflow/store`
/// is behind the `workflow` feature, and a slim build has no `RunSummaryRecord`
/// to project. The wire type it produces compiles unconditionally, which is what
/// keeps the schema artifact single-valued on both legs.
#[cfg(feature = "workflow")]
pub(crate) fn wire_run_summary_record(
    record: crate::workflow::store::RunSummaryRecord,
) -> crate::api::schema::WorkflowRunSummaryInfo {
    crate::api::schema::WorkflowRunSummaryInfo {
        run_id: record.run.to_string(),
        workflow_id: record.workflow.to_string(),
        workflow_name: record.workflow_name,
        version_id: record.version.to_string(),
        text: record.text,
        outcome: record.outcome,
        highlights: record.highlights,
        open_gaps: record.open_gaps,
        per_node: record
            .per_node
            .into_iter()
            .map(|line| crate::api::schema::WorkflowSummaryNodeLine {
                node_key: line.node_key,
                verdict: line.verdict,
                one_liner: line.one_liner,
            })
            .collect(),
        token_estimate: record.token_estimate,
        generated_by_path: record
            .generated_by_path
            .as_ref()
            .map(InstancePath::to_string),
        created_at_unix_ms: record.created_at_unix_ms,
        run_pruned: record.run_pruned,
    }
}

/// An in-process API call answers with the same envelope a client would get, so
/// a failed delivery is recoverable from the response instead of being dropped.
/// What to tell the user when a run reaches `status`, or `None` for the
/// statuses that are simply progress.
///
/// A run that pauses names what it is waiting on: "paused" alone sends the user
/// to the CLI to find out which node stopped it.
fn run_status_notice(run: &RunId, status: RunStatus, blocking: &[String]) -> Option<UserNotice> {
    let (level, message) = match status {
        RunStatus::Succeeded => (
            NoticeLevel::Info,
            "the run finished: every node succeeded".to_string(),
        ),
        RunStatus::Failed => (
            NoticeLevel::Error,
            match blocking.first() {
                Some(path) => format!("the run failed at node {path}"),
                None => "the run failed".to_string(),
            },
        ),
        RunStatus::Cancelled => (NoticeLevel::Warning, "the run was cancelled".to_string()),
        RunStatus::Paused => (
            NoticeLevel::Warning,
            match blocking.split_first() {
                Some((path, [])) => format!("the run is paused on node {path}"),
                Some((path, rest)) => {
                    format!("the run is paused on node {path} and {} more", rest.len())
                }
                None => "the run is paused and is waiting for a human".to_string(),
            },
        ),
        RunStatus::Pending | RunStatus::Running => return None,
    };
    Some(UserNotice {
        level,
        run: Some(run.clone()),
        path: None,
        message,
    })
}

/// The user-facing message in a `workflow.node.steer` response, or `None` when
/// the steer was accepted.
///
/// The envelope is the only place the TUI learns that a delivery was refused —
/// `workflow_node_delivery_failed` for a pane that would not take the text and
/// `workflow_node_not_running` for a node with no pane at all.
pub(crate) fn steer_failure_message(response: &str) -> Option<String> {
    let error = serde_json::from_str::<ErrorResponse>(response).ok()?;
    Some(format!("steer not delivered: {}", error.error.message))
}

fn workflow_api_error(
    method: &str,
    pane: &PublicPaneId,
    response: &str,
) -> Option<DeliveryFailure> {
    let error = serde_json::from_str::<ErrorResponse>(response).ok()?;
    Some(DeliveryFailure {
        method: method.to_string(),
        pane: pane.to_string(),
        code: error.error.code,
        message: error.error.message,
    })
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

pub(crate) fn wire_tier(tier: Tier) -> WorkflowTier {
    match tier {
        Tier::Auto => WorkflowTier::Auto,
        Tier::Max => WorkflowTier::Max,
        Tier::High => WorkflowTier::High,
        Tier::Medium => WorkflowTier::Medium,
        Tier::Low => WorkflowTier::Low,
    }
}

pub(crate) fn wire_demand(demand: Demand) -> WorkflowDemand {
    match demand {
        Demand::Peak => WorkflowDemand::Peak,
        Demand::Critical => WorkflowDemand::Critical,
        Demand::Standard => WorkflowDemand::Standard,
        Demand::Light => WorkflowDemand::Light,
    }
}

pub(crate) fn wire_edge_kind(kind: EdgeKind) -> WorkflowEdgeKind {
    match kind {
        EdgeKind::Sequence => WorkflowEdgeKind::Sequence,
        EdgeKind::Data => WorkflowEdgeKind::Data,
        EdgeKind::Conditional => WorkflowEdgeKind::Conditional,
    }
}

pub(crate) fn wire_run_status(status: RunStatus) -> WorkflowRunStatus {
    match status {
        RunStatus::Pending => WorkflowRunStatus::Pending,
        RunStatus::Running => WorkflowRunStatus::Running,
        RunStatus::Paused => WorkflowRunStatus::Paused,
        RunStatus::Succeeded => WorkflowRunStatus::Succeeded,
        RunStatus::Failed => WorkflowRunStatus::Failed,
        RunStatus::Cancelled => WorkflowRunStatus::Cancelled,
    }
}

pub(crate) fn wire_node_status(status: NodeStatus) -> WorkflowNodeStatus {
    match status {
        NodeStatus::Pending => WorkflowNodeStatus::Pending,
        NodeStatus::Ready => WorkflowNodeStatus::Ready,
        NodeStatus::Running => WorkflowNodeStatus::Running,
        NodeStatus::NeedsAttention => WorkflowNodeStatus::NeedsAttention,
        NodeStatus::Blocked => WorkflowNodeStatus::Blocked,
        NodeStatus::Succeeded => WorkflowNodeStatus::Succeeded,
        NodeStatus::Failed => WorkflowNodeStatus::Failed,
        NodeStatus::Skipped => WorkflowNodeStatus::Skipped,
        NodeStatus::Restored => WorkflowNodeStatus::Restored,
        NodeStatus::Cancelled => WorkflowNodeStatus::Cancelled,
    }
}

/// The wire spelling of a growth guardrail. The engine's [`ExpandLimit`] and
/// the wire's `WorkflowGrowthLimitKind` are declared separately on purpose —
/// `src/api/schema` names no `crate::workflow` type — so this is the one place
/// the two vocabularies are joined.
pub(crate) fn wire_growth_limit_kind(limit: ExpandLimit) -> WorkflowGrowthLimitKind {
    match limit {
        ExpandLimit::ExpandMax => WorkflowGrowthLimitKind::ExpandMax,
        ExpandLimit::MaxDepth => WorkflowGrowthLimitKind::MaxDepth,
        ExpandLimit::MaxNodes => WorkflowGrowthLimitKind::MaxNodes,
    }
}

/// The guardrail's name as the author spelled it in the kvdag, which is also
/// its wire spelling. Kept beside [`wire_growth_limit_kind`] rather than read
/// back off `ExpandLimit::as_str` so the notice a user reads and the `kind` a
/// client parses cannot drift.
fn growth_limit_kind_str(kind: WorkflowGrowthLimitKind) -> &'static str {
    match kind {
        WorkflowGrowthLimitKind::ExpandMax => "expand_max",
        WorkflowGrowthLimitKind::MaxDepth => "max_depth",
        WorkflowGrowthLimitKind::MaxNodes => "max_nodes",
    }
}

/// The DAG's run banner: one line naming the ceiling and the shortfall
/// (`06-phase2-plan.md` §1 WS-G).
fn format_growth_banner(limit: &WorkflowGrowthLimit) -> String {
    format!(
        "growth limited · {} {} reached · {} of {} requested nodes created",
        growth_limit_kind_str(limit.kind),
        limit.limit_value,
        limit.accepted,
        limit.requested
    )
}

/// The per-node notice drawn inside the proposing node's box. Shorter than the
/// banner because it shares a box with the node's own status, and truncated by
/// the renderer when even that does not fit.
fn format_growth_notice(limit: &WorkflowGrowthLimit) -> String {
    format!(
        "growth limited: {} {} · {} of {}",
        growth_limit_kind_str(limit.kind),
        limit.limit_value,
        limit.accepted,
        limit.requested
    )
}

pub(crate) fn wire_evidence(evidence: Evidence) -> WorkflowEvidence {
    match evidence {
        Evidence::SelfReport => WorkflowEvidence::SelfReport,
        Evidence::Hook => WorkflowEvidence::Hook,
        Evidence::Detection => WorkflowEvidence::Detection,
        Evidence::Restored => WorkflowEvidence::Restored,
    }
}

pub(crate) fn wire_succession(succession: &Succession) -> WorkflowSuccession {
    match succession {
        Succession::Satisfied => WorkflowSuccession::Satisfied,
        Succession::Blocked {
            reason,
            resume_when,
        } => WorkflowSuccession::Blocked {
            reason: reason.clone(),
            resume_when: resume_when.clone(),
        },
        Succession::NoFollowup { evidence } => WorkflowSuccession::NoFollowup {
            evidence: evidence.clone(),
        },
    }
}

/// `Succession::Blocked` is the only succession that carries a blocker; the
/// wire keeps it in its own field so a client can render it without matching on
/// the succession shape.
pub(crate) fn wire_blocker(succession: &Succession) -> Option<serde_json::Value> {
    match succession {
        Succession::Blocked {
            reason,
            resume_when,
        } => Some(serde_json::json!({
            "reason": reason,
            "resume_when": resume_when,
        })),
        Succession::Satisfied | Succession::NoFollowup { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::EventHub;
    use crate::config::Config;
    use crate::workflow::model::{
        ArgSpec, EdgeKind, EdgePayload, GrowthLimits, Isolation, KvdagEdge, KvdagNode, KvdagSpec,
        NodeKey, NodeKind, NodeToken, OutputSchema, RawJson, RunEventKind,
    };

    fn definition_node(key: &str, runner: Runner, required: &str) -> KvdagNode {
        KvdagNode {
            key: NodeKey::new(key),
            label: key.to_string(),
            role: String::new(),
            kind: NodeKind::Agent,
            demand: Demand::Critical,
            runner,
            command: match runner {
                Runner::Agent => None,
                Runner::Command => Some(vec!["true".to_string()]),
            },
            prompt_template: format!("do {key}"),
            system_contract: None,
            output_schema: OutputSchema::parse(serde_json::json!({
                "type": "object",
                "required": [required],
            }))
            .expect("fixture schema parses"),
            max_attempts: 2,
            timeout_ms: None,
            isolation: Isolation::None,
            is_template: false,
            expand_allow: Vec::new(),
            expand_max: 0,
        }
    }

    /// `plan → implement`, the smallest graph with a real dependency.
    fn definition_with(runner: Runner) -> Kvdag {
        Kvdag::try_new(KvdagSpec {
            version_id: KvdagVersionId::new("kvdag_version:test"),
            workflow_id: WorkflowId::new("workflow:test"),
            version: 1,
            parent: None,
            contract: "Reply only through result.json.".to_string(),
            growth: GrowthLimits::default(),
            args: vec![ArgSpec {
                name: "goal".to_string(),
                required: false,
                default: None,
                description: String::new(),
            }],
            nodes: vec![
                definition_node("plan", runner, "plan"),
                definition_node("implement", runner, "report"),
            ],
            edges: vec![KvdagEdge {
                from: NodeKey::new("plan"),
                to: NodeKey::new("implement"),
                kind: EdgeKind::Sequence,
                condition: None,
                payload: EdgePayload::Summary,
                port: None,
            }],
        })
        .expect("fixture kvdag is valid")
    }

    fn definition() -> Kvdag {
        definition_with(Runner::Agent)
    }

    /// One node with one attempt, so a dead pane is terminal: the pane-exit
    /// tests want the engine's verdict, not a retry that spawns a real PTY.
    fn single_attempt_definition() -> Kvdag {
        let mut plan = definition_node("plan", Runner::Agent, "plan");
        plan.max_attempts = 1;
        Kvdag::try_new(KvdagSpec {
            version_id: KvdagVersionId::new("kvdag_version:test"),
            workflow_id: WorkflowId::new("workflow:test"),
            version: 1,
            parent: None,
            contract: "Reply only through result.json.".to_string(),
            growth: GrowthLimits::default(),
            args: Vec::new(),
            nodes: vec![plan],
            edges: Vec::new(),
        })
        .expect("fixture kvdag is valid")
    }

    /// `plan --(port "plan")--> implement`, so the downstream node's template
    /// slot resolves to the upstream checkpoint instead of a run argument.
    fn ported_definition() -> Kvdag {
        let mut implement = definition_node("implement", Runner::Agent, "report");
        implement.prompt_template = "Implement {{plan}}".to_string();
        Kvdag::try_new(KvdagSpec {
            version_id: KvdagVersionId::new("kvdag_version:test"),
            workflow_id: WorkflowId::new("workflow:test"),
            version: 1,
            parent: None,
            contract: "Reply only through result.json.".to_string(),
            growth: GrowthLimits::default(),
            args: Vec::new(),
            nodes: vec![definition_node("plan", Runner::Agent, "plan"), implement],
            edges: vec![KvdagEdge {
                from: NodeKey::new("plan"),
                to: NodeKey::new("implement"),
                kind: EdgeKind::Data,
                condition: None,
                payload: EdgePayload::Summary,
                port: Some("plan".to_string()),
            }],
        })
        .expect("fixture kvdag is valid")
    }

    fn graph_of(definition: &Kvdag) -> RunGraph {
        RunGraph::materialise(definition, RunId::new("workflow_run:test"), Tier::High)
    }

    fn active_run() -> ActiveRun {
        ActiveRun::new(
            RunId::new("workflow_run:test"),
            WorkflowId::new("workflow:test"),
            KvdagVersionId::new("kvdag_version:test"),
            Tier::High,
        )
        .with_args(HashMap::from([("goal".to_string(), "ship it".to_string())]))
    }

    fn binding_for(pane: &str) -> NodeBinding {
        NodeBinding {
            pane_id: PublicPaneId::new(pane),
            terminal_id: crate::terminal::TerminalId::alloc(),
            agent_session_id: "session-1".to_string(),
            transcript_path: PathBuf::from("transcript.jsonl"),
            node_dir: PathBuf::from("/runs/test/plan"),
            cwd: PathBuf::from("/repo"),
        }
    }

    fn started_state() -> WorkflowRuntimeState {
        let mut state =
            WorkflowRuntimeState::new(EngineConfig::default(), WorkflowPolicy::default());
        let definition = definition();
        let graph = graph_of(&definition);
        state
            .start(active_run(), definition, graph, Instant::now())
            .expect("the first run starts");
        state
    }

    fn report(raw: &str) -> RawJson {
        RawJson(serde_json::from_str(raw).expect("test json parses"))
    }

    fn test_app_with_hub(event_hub: EventHub) -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(&Config::default(), true, None, api_rx, event_hub)
    }

    /// Every notice the user will see, in the order they will see it: the
    /// rendered slot first, then whatever is waiting behind it. Before the
    /// notice queue only the last notice of a batch survived, so a test could
    /// read `state.toast` and call that "the notification"; now the batch is
    /// the answer.
    fn surfaced_toasts(app: &App) -> Vec<ToastNotification> {
        app.state
            .toast
            .iter()
            .chain(app.state.toast_queue.iter())
            .cloned()
            .collect()
    }

    fn surfaced_notices(app: &App) -> Vec<String> {
        surfaced_toasts(app)
            .into_iter()
            .map(|toast| toast.context)
            .collect()
    }

    fn status_of(state: &WorkflowRuntimeState, path: &str) -> NodeStatus {
        state
            .node(&InstancePath::new(path))
            .map(|node| node.status)
            .expect("the node exists")
    }

    #[test]
    fn engine_config_reads_the_workflow_config_block() {
        let config = Config::default();
        let engine = engine_config(&config);
        assert_eq!(engine.max_parallel_nodes, 4);
        assert_eq!(engine.stuck_threshold, 3);
        assert_eq!(engine.drift_threshold, 5);
        // §4 D22: summaries default on, and the production path is the agent
        // binding — `summary_command: None` — unless the environment declares
        // an override.
        assert!(engine.summary_enabled);
        let policy = workflow_policy(&config);
        assert_eq!(policy.retention_runs, 50);
        assert_eq!(policy.history_context_runs, 3);
    }

    /// The three ways `KARVEX_WORKFLOW_SUMMARY_COMMAND` resolves (§4 D2, defect
    /// D-1). Written against the parser rather than the environment so it does
    /// not fight other tests over a process-global.
    #[test]
    fn a_malformed_summary_command_disables_summaries_rather_than_reverting_to_claude() {
        // Unset/empty is the production path: run as an agent.
        assert!(matches!(parse_summary_command(None), SummaryCommand::Agent));
        assert!(matches!(
            parse_summary_command(Some("   ")),
            SummaryCommand::Agent
        ));
        // A declared command binding.
        match parse_summary_command(Some(r#"["/bin/echo","hi"]"#)) {
            SummaryCommand::Argv(argv) => assert_eq!(argv, vec!["/bin/echo", "hi"]),
            other => panic!("expected an argv binding, got {other:?}"),
        }
        // Set to something unusable. The caller has an opinion about what the
        // summariser is; answering it with "I ran `claude` instead" is the
        // silent fallback D-1 forbids, so summaries switch off entirely.
        assert!(matches!(
            parse_summary_command(Some("not json")),
            SummaryCommand::Invalid
        ));
        assert!(matches!(
            parse_summary_command(Some("[]")),
            SummaryCommand::Invalid
        ));
        assert!(matches!(
            parse_summary_command(Some(r#"{"argv":["x"]}"#)),
            SummaryCommand::Invalid
        ));
    }

    /// E-11: a boot-time log line is evidence nobody reads, so the disable is
    /// surfaced at the first run that would have summarised — and exactly once,
    /// because the cause is a process-lifetime fact, not a per-run one.
    #[test]
    fn a_disabled_summariser_is_announced_once_per_server() {
        let mut state = WorkflowRuntimeState::new(
            EngineConfig {
                summary_enabled: false,
                ..EngineConfig::default()
            },
            WorkflowPolicy {
                summary_override_invalid: true,
                ..WorkflowPolicy::default()
            },
        );
        assert!(
            state.claim_summary_disabled_notice(),
            "the first run says so"
        );
        assert!(
            !state.claim_summary_disabled_notice(),
            "and no run after it repeats the same standing fact"
        );
    }

    /// The *deliberate* off-switch is silent: a user who set
    /// `workflow.summary_enabled = false` chose that and needs no warning. Only
    /// an override that could not be read is news.
    #[test]
    fn summaries_switched_off_by_config_are_not_announced() {
        let mut state = WorkflowRuntimeState::new(
            EngineConfig {
                summary_enabled: false,
                ..EngineConfig::default()
            },
            WorkflowPolicy::default(),
        );
        assert!(!state.claim_summary_disabled_notice());
    }

    /// §4 D6: only the bundled Claude hook's absolute path is taken as a
    /// transcript. Anything else leaves the pre-launch estimate in place, which
    /// the stat-first rule already degrades correctly.
    #[test]
    fn only_the_claude_hooks_absolute_path_is_read_back_as_a_transcript() {
        use crate::workflow::binding::observe::{
            reported_transcript_path, CLAUDE_AGENT_LABEL, CLAUDE_HOOK_SOURCE,
        };

        assert_eq!(
            reported_transcript_path(
                CLAUDE_HOOK_SOURCE,
                CLAUDE_AGENT_LABEL,
                Some("/home/u/.claude/projects/-repo/s1.jsonl"),
            ),
            Some(PathBuf::from("/home/u/.claude/projects/-repo/s1.jsonl"))
        );
        assert_eq!(
            reported_transcript_path(CLAUDE_HOOK_SOURCE, CLAUDE_AGENT_LABEL, None),
            None
        );
        assert_eq!(
            reported_transcript_path(
                CLAUDE_HOOK_SOURCE,
                CLAUDE_AGENT_LABEL,
                Some("relative.jsonl")
            ),
            None,
            "the stat happens in the server's cwd, not the pane's"
        );
        assert_eq!(
            reported_transcript_path("karvex:codex", "codex", Some("/t/s.jsonl")),
            None,
            "karvex does not know another agent's transcript layout"
        );
    }

    /// §4 D8: an interrogation belongs to the node it revived, not to the run
    /// this server happens to be executing, so a new run must not strand it.
    #[test]
    fn an_interrogation_survives_the_next_run_starting() {
        let mut state = started_state();
        let interrogation = live_interrogation("w1:p7", "workflow_run:old", "plan");
        state.track_interrogation(interrogation.clone());

        state.apply(EngineInput::CancelRun, Instant::now());
        let definition = definition();
        let graph = graph_of(&definition);
        state
            .start(active_run(), definition, graph, Instant::now())
            .expect("the cancelled run does not block the next one");

        assert_eq!(
            state.interrogation_for_pane(&PublicPaneId::new("w1:p7")),
            Some(&interrogation),
            "`start` clears per-run state; an interrogation is not per-run state"
        );
    }

    /// The one-at-a-time rule (§4 D7) is keyed on `(run, path)`, so the same
    /// node in a different run — and a different node in the same run — are
    /// both still interrogable.
    #[test]
    fn one_live_interrogation_per_source_node() {
        let mut state =
            WorkflowRuntimeState::new(EngineConfig::default(), WorkflowPolicy::default());
        state.track_interrogation(live_interrogation("w1:p7", "workflow_run:a", "plan"));

        assert!(state
            .live_interrogation(&RunId::new("workflow_run:a"), &InstancePath::new("plan"))
            .is_some());
        assert!(
            state
                .live_interrogation(&RunId::new("workflow_run:a"), &InstancePath::new("review"))
                .is_none(),
            "a different node of the same run is not blocked"
        );
        assert!(
            state
                .live_interrogation(&RunId::new("workflow_run:b"), &InstancePath::new("plan"))
                .is_none(),
            "the same node key in a different run is not blocked"
        );
    }

    /// Taking the entry out is what makes the end stamp idempotent: `PaneDied`
    /// and the reconcile sweep can both name the same dead pane, and only the
    /// first produces an `InterrogationUpdate`.
    #[test]
    fn an_interrogation_ends_exactly_once() {
        let mut state =
            WorkflowRuntimeState::new(EngineConfig::default(), WorkflowPolicy::default());
        state.track_interrogation(live_interrogation("w1:p7", "workflow_run:a", "plan"));

        let pane = PublicPaneId::new("w1:p7");
        assert!(state.end_interrogation_for_pane(&pane).is_some());
        assert!(
            state.end_interrogation_for_pane(&pane).is_none(),
            "a second signal for the same dead pane must not stamp a second end"
        );
    }

    /// §6 A9: the id is pre-assigned at spawn, so the async learn is only news
    /// when it actually differs.
    #[test]
    fn learning_a_forked_session_id_is_only_news_once() {
        let mut state =
            WorkflowRuntimeState::new(EngineConfig::default(), WorkflowPolicy::default());
        let mut entry = live_interrogation("w1:p7", "workflow_run:a", "plan");
        entry.forked_session_id = None;
        state.track_interrogation(entry);
        let pane = PublicPaneId::new("w1:p7");

        let learned = state
            .learn_forked_session_id(&pane, "fork-1")
            .expect("the first report is news");
        assert_eq!(learned.forked_session_id.as_deref(), Some("fork-1"));
        assert!(
            state.learn_forked_session_id(&pane, "fork-1").is_none(),
            "re-reporting the same id must not enqueue a second update"
        );
    }

    /// §7 R-1 / §4 D8: the tick outlives `is_live()` for two independent
    /// reasons, and lapses when neither holds.
    #[test]
    fn the_tick_outlives_the_run_for_a_live_interrogation() {
        let now = Instant::now();
        let mut state =
            WorkflowRuntimeState::new(EngineConfig::default(), WorkflowPolicy::default());
        state.rearm_tick(now);
        assert_eq!(
            state.next_tick_deadline(),
            None,
            "an idle runtime with nothing to drive lets the deadline lapse"
        );

        state.track_interrogation(live_interrogation("w1:p7", "workflow_run:a", "plan"));
        assert!(
            state.next_tick_deadline().is_some(),
            "an interrogation can be opened with no run live at all, and the \
             reconcile sweep needs a cadence to run on"
        );

        state.end_interrogation_for_pane(&PublicPaneId::new("w1:p7"));
        state.rearm_tick(now + WORKFLOW_TICK_INTERVAL);
        assert_eq!(
            state.next_tick_deadline(),
            None,
            "and it lapses again once the last one is gone"
        );
    }

    /// §3 rule 4: an interrogation's create is addressed by an app-minted id
    /// the later update reuses, so evicting it would leave every update naming
    /// a row that does not exist.
    #[test]
    fn an_interrogation_create_is_never_evicted() {
        assert!(is_create_write(&StoreWrite::InterrogationStarted {
            id: InterrogationId::new("interrogation:1"),
            run: RunId::new("workflow_run:a"),
            path: InstancePath::new("plan"),
            source_session_id: "source".into(),
            forked_session_id: None,
            transcript_path: None,
            cwd: "/repo".into(),
            pane_id: PublicPaneId::new("w1:p7"),
            reconstructed: false,
            seeded_from_seq: None,
            note: String::new(),
            started_at_unix_ms: 1,
        }));
        assert!(
            !is_create_write(&StoreWrite::InterrogationUpdate {
                id: InterrogationId::new("interrogation:1"),
                forked_session_id: None,
                ended_at_unix_ms: Some(2),
            }),
            "the update is not a create; it is exactly what eviction may drop"
        );
    }

    fn live_interrogation(pane: &str, run: &str, path: &str) -> LiveInterrogation {
        LiveInterrogation {
            id: InterrogationId::new(format!("interrogation:{pane}")),
            run: RunId::new(run),
            path: InstancePath::new(path),
            pane: PublicPaneId::new(pane),
            source_session_id: "source-sid".to_string(),
            forked_session_id: Some("fork-sid".to_string()),
            transcript_path: Some("/t/fork.jsonl".to_string()),
            cwd: "/repo".to_string(),
            reconstructed: false,
            note: String::new(),
            started_at_unix_ms: 1,
        }
    }

    #[test]
    fn starting_a_run_admits_the_roots_and_arms_the_tick() {
        let now = Instant::now();
        let mut state =
            WorkflowRuntimeState::new(EngineConfig::default(), WorkflowPolicy::default());
        let definition = definition();
        let graph = graph_of(&definition);
        let effects = state
            .start(active_run(), definition, graph, now)
            .expect("the run starts");

        assert_eq!(state.run_status(), Some(RunStatus::Running));
        assert!(state.is_live());
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, RunEffect::Emit(WorkflowEvent::RunStarted { .. }))));
        assert_eq!(
            state.pending_spawns(),
            &[InstancePath::new("plan")],
            "only the root is admitted"
        );
        assert_eq!(
            state.next_tick_deadline(),
            Some(now + WORKFLOW_TICK_INTERVAL)
        );
        assert!(!state.tick_due(now));
        assert!(state.tick_due(now + WORKFLOW_TICK_INTERVAL));
    }

    #[test]
    fn a_second_run_is_refused_while_one_is_live() {
        let mut state = started_state();
        let definition = definition();
        let graph = graph_of(&definition);

        let error = state
            .start(active_run(), definition, graph, Instant::now())
            .expect_err("one run at a time");
        assert_eq!(error, WorkflowStartError::RunInFlight);
        assert_eq!(state.run_status(), Some(RunStatus::Running));
    }

    /// 2.1: a run that *finishes* is the most common outcome and the engine has
    /// no notice for it at all, so the screen used to be byte-identical to an
    /// idle one. It is announced once, and only once.
    #[test]
    fn a_run_that_finishes_is_announced_exactly_once() {
        let mut state = started_state();
        assert!(
            state.take_pending_announcements().is_empty(),
            "a running run is progress, not news"
        );

        for (path, result) in [
            ("plan", r#"{"plan":"do it"}"#),
            ("implement", r#"{"report":"shipped"}"#),
        ] {
            state.bind_node(&InstancePath::new(path), binding_for(path), Instant::now());
            state.apply(
                EngineInput::NodeSelfReport {
                    path: InstancePath::new(path),
                    token: NodeToken::new("token"),
                    result: report(result),
                },
                Instant::now(),
            );
        }
        assert_eq!(state.run_status(), Some(RunStatus::Succeeded));

        let announced = state.take_pending_announcements();
        assert_eq!(announced.len(), 1, "{announced:?}");
        assert_eq!(announced[0].level, NoticeLevel::Info);
        assert_eq!(announced[0].path, None);
        assert!(
            announced[0].message.contains("finished"),
            "{}",
            announced[0].message
        );
        assert!(
            state.take_pending_announcements().is_empty(),
            "the same terminal status is not announced twice"
        );
    }

    /// 2.1: a cancelled run is a terminal state the user asked for and still
    /// has to be told about — it is the state that frees the server.
    #[test]
    fn a_cancelled_run_is_announced() {
        let mut state = started_state();
        let _ = state.take_pending_announcements();
        state.apply(EngineInput::CancelRun, Instant::now());

        let announced = state.take_pending_announcements();
        assert_eq!(announced.len(), 1, "{announced:?}");
        assert_eq!(announced[0].level, NoticeLevel::Warning);
        assert!(
            announced[0].message.contains("cancelled"),
            "{}",
            announced[0].message
        );
    }

    /// 2.1: the node that stops the run is named, so "paused" does not send the
    /// user to the CLI to find out which node it is waiting on.
    #[test]
    fn a_node_that_needs_a_human_is_announced_and_names_the_run_it_paused() {
        let mut state = started_state();
        let path = InstancePath::new("plan");
        state.bind_node(&path, binding_for("pane-1"), Instant::now());
        let _ = state.take_pending_announcements();

        // Two schema-invalid results: the first is re-prompted, the second
        // hands the node to a human.
        for _ in 0..2 {
            state.apply(
                EngineInput::NodeSelfReport {
                    path: path.clone(),
                    token: NodeToken::new("token"),
                    result: report(r#"{"wrong":1}"#),
                },
                Instant::now(),
            );
        }
        assert_eq!(status_of(&state, "plan"), NodeStatus::NeedsAttention);

        let announced = state.take_pending_announcements();
        let node_notice = announced
            .iter()
            .find(|notice| notice.path.as_ref() == Some(&path))
            .expect("the node that needs a human is announced");
        assert_eq!(node_notice.level, NoticeLevel::Warning);
        assert!(
            node_notice.message.contains("needs attention"),
            "{}",
            node_notice.message
        );
        assert_eq!(state.run_status(), Some(RunStatus::Paused));
        let run_notice = announced
            .iter()
            .find(|notice| notice.path.is_none())
            .expect("the paused run is announced too");
        assert!(
            run_notice.message.contains("plan"),
            "the pause names the node it is waiting on: {}",
            run_notice.message
        );
        assert!(
            state
                .take_pending_announcements()
                .iter()
                .all(|notice| notice.path.as_ref() != Some(&path)),
            "the same node is not announced twice"
        );
    }

    /// The message the DAG view's steer row shows when the API refused the
    /// delivery. 2.15: this is the envelope that used to be discarded.
    #[test]
    fn a_refused_steer_response_becomes_a_message() {
        let refused = r#"{"id":"1","error":{"code":"workflow_node_delivery_failed","message":"pane.send_text to pane w1:pD failed: pane_not_found: no such pane"}}"#;
        let message = steer_failure_message(refused).expect("a refusal is a message");
        assert!(message.starts_with("steer not delivered:"), "{message}");
        assert!(message.contains("pane_not_found"), "{message}");

        let accepted = r#"{"id":"1","result":{"type":"workflow_node_steered"}}"#;
        assert_eq!(steer_failure_message(accepted), None);
    }

    #[test]
    fn a_finished_run_releases_the_tick_and_accepts_the_next_run() {
        let mut state = started_state();
        state.bind_node(
            &InstancePath::new("plan"),
            binding_for("pane-1"),
            Instant::now(),
        );
        for (path, result) in [
            ("plan", r#"{"plan":"do it"}"#),
            ("implement", r#"{"report":"shipped"}"#),
        ] {
            state.bind_node(&InstancePath::new(path), binding_for(path), Instant::now());
            state.apply(
                EngineInput::NodeSelfReport {
                    path: InstancePath::new(path),
                    token: NodeToken::new("token"),
                    result: report(result),
                },
                Instant::now(),
            );
        }

        // ── the run's own work is over ──────────────────────────────────
        assert_eq!(state.run_status(), Some(RunStatus::Succeeded));
        assert!(!state.is_live(), "the user graph is closed");
        assert!(state
            .active_run()
            .and_then(|run| run.ended_at_unix_ms)
            .is_some());

        // ── but the epilogue holds the tick and the next run ─────────────
        //
        // §7 R-1: `epilogue_pending()` is the only new liveness input, and this
        // is its pin. A succeeded run leaves exactly one thing to spawn — the
        // engine-owned `.summary` node (§4 D1) — and until it resolves the
        // workflow tick must stay alive to drive it, and the next run must be
        // refused rather than silently orphaning the summariser (M7).
        let summary = InstancePath::new(crate::workflow::model::SUMMARY_INSTANCE_PATH);
        assert_eq!(state.pending_spawns(), std::slice::from_ref(&summary));
        assert!(
            state.next_tick_deadline().is_some(),
            "a pending epilogue keeps the tick alive; without it nothing would \
             ever drive the summariser to a conclusion"
        );
        // The *admission* refusal lives one layer up, in `handle_workflow_run`
        // (`is_live() || epilogue_pending()`), together with the
        // `pending_writes` drain that keeps an accepted summary from dying with
        // the engine swap — see this module's `start` doc. At this layer `start`
        // is deliberately unguarded, so it is not asserted here; the e2e
        // `a_node_whose_pane_exits_without_a_result_fails_and_closes_the_run`
        // covers the refusal end to end.

        // ── and a give-up releases both ─────────────────────────────────
        //
        // The other half of R-1: the ladder ends on its own, so the deadline
        // lapses and admission reopens without anyone intervening. A `GaveUp`
        // epilogue that kept ticking would be an unbounded liveness leak.
        state.bind_node(&summary, binding_for("pane-summary"), Instant::now());
        state.apply(
            EngineInput::PaneExited {
                pane: crate::workflow::model::PublicPaneId::new("pane-summary"),
                code: Some(1),
            },
            Instant::now(),
        );
        assert!(
            !state.engine().epilogue_pending(),
            "a summariser whose pane died before reporting gives up"
        );
        assert_eq!(
            state.run_status(),
            Some(RunStatus::Succeeded),
            "and giving up never changes the run's outcome"
        );
        assert_eq!(
            state.next_tick_deadline(),
            None,
            "a resolved epilogue lets the deadline lapse"
        );
        assert!(
            state
                .start(
                    active_run(),
                    definition(),
                    graph_of(&definition()),
                    Instant::now()
                )
                .is_ok(),
            "and the next run is admitted"
        );
    }

    #[test]
    fn a_spawn_is_offered_once_per_admission() {
        let mut state = started_state();
        assert_eq!(state.claim_spawns(), vec![InstancePath::new("plan")]);
        assert!(
            state.claim_spawns().is_empty(),
            "a queued node is not offered to the binder twice"
        );

        state.bind_node(
            &InstancePath::new("plan"),
            binding_for("pane-1"),
            Instant::now(),
        );
        assert_eq!(status_of(&state, "plan"), NodeStatus::Running);
        assert!(state.pending_spawns().is_empty());

        // A restart puts the node back in the ready set, so it is offered again.
        state.apply(
            EngineInput::RestartNode {
                path: InstancePath::new("plan"),
            },
            Instant::now(),
        );
        assert_eq!(status_of(&state, "plan"), NodeStatus::Ready);
        assert_eq!(state.claim_spawns(), vec![InstancePath::new("plan")]);
    }

    #[test]
    fn durable_writes_are_buffered_and_overflow_degrades_persistence() {
        let mut state = started_state();
        assert!(!state.persistence_degraded());

        let write = || StoreWrite::RunEvent {
            run: RunId::new("workflow_run:test"),
            seq: 1,
            kind: RunEventKind::NodeStatus,
            path: None,
            payload: serde_json::json!({}),
            at_unix_ms: 1,
        };
        for _ in 0..PENDING_WRITE_BUDGET {
            state.queue_write(write());
        }
        assert_eq!(state.pending_write_count(), PENDING_WRITE_BUDGET);
        assert!(!state.persistence_degraded());

        state.queue_write(write());
        assert_eq!(state.pending_write_count(), PENDING_WRITE_BUDGET);
        assert_eq!(state.dropped_write_count(), 1);
        assert!(state.persistence_degraded());

        assert_eq!(
            state.take_pending_writes(8).len(),
            8,
            "the drain is capped by what the store thread has room for"
        );
        assert_eq!(state.pending_write_count(), PENDING_WRITE_BUDGET - 8);
        assert_eq!(
            state.take_pending_writes(usize::MAX).len(),
            PENDING_WRITE_BUDGET - 8
        );
        assert_eq!(state.pending_write_count(), 0);
    }

    /// D-C: the summariser is asked about **itself**, not about a kvdag node it
    /// does not have.
    ///
    /// With `KARVEX_WORKFLOW_SUMMARY_COMMAND` set, the epilogue is a plain
    /// process. Resolving its runner through the definition — which has no
    /// `.summary` node — fell back to `Runner::Agent`, so the one corrective
    /// re-prompt of §4 D1's bounded ladder went out as `agent.prompt`, failed
    /// `agent_not_found`, and the rung was dead: an over-budget summary went
    /// straight to `GaveUp` with no second chance. Production was unaffected
    /// (a real `claude` epilogue *is* an agent), which is exactly why only the
    /// override path could expose it.
    #[test]
    fn a_command_bound_epilogue_reports_its_own_runner_not_the_definitions() {
        let mut state = WorkflowRuntimeState::new(
            EngineConfig {
                // The declared command binding (§4 D2 / §6 A4).
                summary_command: Some(vec!["/bin/true".to_string()]),
                ..EngineConfig::default()
            },
            WorkflowPolicy::default(),
        );
        // A definition whose *own* nodes are agents, so a runner read from the
        // definition would answer `Agent` — the exact wrong answer.
        let definition = definition();
        let graph = graph_of(&definition);
        state
            .start(active_run(), definition, graph, Instant::now())
            .expect("the run starts");

        // Drive the user graph to success so the epilogue is appended.
        for (path, result) in [
            ("plan", r#"{"plan":"do it"}"#),
            ("implement", r#"{"report":"shipped"}"#),
        ] {
            state.bind_node(&InstancePath::new(path), binding_for(path), Instant::now());
            state.apply(
                EngineInput::NodeSelfReport {
                    path: InstancePath::new(path),
                    token: NodeToken::new("token"),
                    result: report(result),
                },
                Instant::now(),
            );
        }
        assert_eq!(state.run_status(), Some(RunStatus::Succeeded));

        let summary = InstancePath::new(crate::workflow::model::SUMMARY_INSTANCE_PATH);
        state.bind_node(&summary, binding_for("pane-summary"), Instant::now());

        assert_eq!(
            state.runner_for_pane(&PublicPaneId::new("pane-summary")),
            Runner::Command,
            "the epilogue's runner is the one `begin_epilogue` recorded (D-1's \
             single authority), not the `Agent` default a definition lookup \
             falls back to for a node the definition does not contain"
        );
        // The sibling nodes still resolve through the definition, so the
        // reserved-path branch is a special case and not a new default.
        assert_eq!(
            state.runner_for_pane(&PublicPaneId::new("plan")),
            Runner::Agent
        );
    }

    /// The same node, with the summariser left on its production binding: the
    /// epilogue is an agent, and the reserved-path branch answers that too.
    #[test]
    fn an_agent_bound_epilogue_still_reports_agent() {
        let mut state =
            WorkflowRuntimeState::new(EngineConfig::default(), WorkflowPolicy::default());
        let definition = definition_with(Runner::Command);
        let graph = graph_of(&definition);
        state
            .start(active_run(), definition, graph, Instant::now())
            .expect("the run starts");
        for (path, result) in [
            ("plan", r#"{"plan":"do it"}"#),
            ("implement", r#"{"report":"shipped"}"#),
        ] {
            state.bind_node(&InstancePath::new(path), binding_for(path), Instant::now());
            state.apply(
                EngineInput::NodeSelfReport {
                    path: InstancePath::new(path),
                    token: NodeToken::new("token"),
                    result: report(result),
                },
                Instant::now(),
            );
        }
        let summary = InstancePath::new(crate::workflow::model::SUMMARY_INSTANCE_PATH);
        state.bind_node(&summary, binding_for("pane-summary"), Instant::now());
        assert_eq!(
            state.runner_for_pane(&PublicPaneId::new("pane-summary")),
            Runner::Agent,
            "with no override the summariser is a real `claude` pane, and the \
             definition's own `Command` nodes must not decide that for it"
        );
    }

    /// The defect this whole seam exists for: the summariser's `task.md` was
    /// hand-built by `summary_task_spec` and never went through
    /// [`spawn::TaskDocument`], so it was the one node document with no
    /// `## Reporting` section. It told the node what to cover and never how to
    /// finish — no `result.json`, no `kvx workflow node complete` — which the
    /// `KARVEX_WORKFLOW_SUMMARY_COMMAND` stubs hid completely, because a stub
    /// hardcodes the protocol instead of reading it. Under the default
    /// `claude` runner the epilogue could only idle until the watchdog gave up.
    #[test]
    fn the_summarisers_task_document_tells_it_how_to_report_completion() {
        let mut state =
            WorkflowRuntimeState::new(EngineConfig::default(), WorkflowPolicy::default());
        let definition = definition();
        let graph = graph_of(&definition);
        state
            .start(active_run(), definition, graph, Instant::now())
            .expect("the run starts");
        for (path, result) in [
            ("plan", r#"{"plan":"do it"}"#),
            ("implement", r#"{"report":"shipped"}"#),
        ] {
            state.bind_node(&InstancePath::new(path), binding_for(path), Instant::now());
            state.apply(
                EngineInput::NodeSelfReport {
                    path: InstancePath::new(path),
                    token: NodeToken::new("token"),
                    result: report(result),
                },
                Instant::now(),
            );
        }

        let spec = state
            .engine()
            .summary_task_spec()
            .expect("a succeeded run has an epilogue");
        let node_dir = PathBuf::from(if cfg!(windows) {
            r"C:\runs\r1\.summary"
        } else {
            "/runs/r1/.summary"
        });
        let rendered = epilogue_task_markdown(&spec, &node_dir);

        assert!(
            rendered.contains("## Reporting"),
            "the epilogue is rendered through the shared task document:\n{rendered}"
        );
        assert!(
            rendered.contains("result.json"),
            "the summariser is told which file to write:\n{rendered}"
        );
        assert!(
            rendered.contains("output_schema.json"),
            "…which schema it has to validate against:\n{rendered}"
        );
        assert!(
            rendered.contains("kvx workflow node complete"),
            "…and the only way this node ever finishes:\n{rendered}"
        );

        // Sharing the renderer means the epilogue inherits the absolute-path
        // contract too, and it needs it for the same reason every other node
        // does: the summariser's cwd is the workspace directory, so a
        // `./result.json` would send it writing where nothing is watching.
        for file in ["result.json", "output_schema.json"] {
            let expected = node_dir.join(file);
            assert!(
                rendered.contains(&expected.display().to_string()),
                "the summariser is given {} absolutely:\n{rendered}",
                expected.display()
            );
        }
        assert!(
            !rendered.contains("`./"),
            "the epilogue may not name a karvex file relative to its cwd:\n{rendered}"
        );

        // The engine's evidence survives the wrap, and the document has exactly
        // one title.
        assert!(rendered.starts_with("# summary\n"), "{rendered}");
        assert_eq!(
            rendered.matches("\n# ").count() + 1,
            1,
            "a single H1, not the engine's plus the document's:\n{rendered}"
        );
        assert!(rendered.contains("### `plan`"), "{rendered}");
        assert!(rendered.contains("### `implement`"), "{rendered}");

        // The author's own contract stays out: the epilogue is karvex's node,
        // and a workflow must not be able to rewrite what its summariser is
        // for (§4 D2).
        assert!(!rendered.contains("## Contract"), "{rendered}");
        assert!(!rendered.contains("## Inputs"), "{rendered}");
        assert!(!rendered.contains("## Prior runs"), "{rendered}");
    }

    #[test]
    fn the_delivery_primitive_follows_the_node_runner() {
        let mut agent = started_state();
        agent.bind_node(
            &InstancePath::new("plan"),
            binding_for("pane-1"),
            Instant::now(),
        );
        assert_eq!(
            agent.runner_for_pane(&PublicPaneId::new("pane-1")),
            Runner::Agent
        );

        let mut command =
            WorkflowRuntimeState::new(EngineConfig::default(), WorkflowPolicy::default());
        let definition = definition_with(Runner::Command);
        let graph = graph_of(&definition);
        command
            .start(active_run(), definition, graph, Instant::now())
            .expect("the run starts");
        command.bind_node(
            &InstancePath::new("plan"),
            binding_for("pane-2"),
            Instant::now(),
        );
        assert_eq!(
            command.runner_for_pane(&PublicPaneId::new("pane-2")),
            Runner::Command
        );
        assert_eq!(
            command.node_path_for_pane(&PublicPaneId::new("pane-2")),
            Some(InstancePath::new("plan"))
        );
        assert_eq!(
            command.runner_for_pane(&PublicPaneId::new("pane-unknown")),
            Runner::Agent,
            "an unbound pane falls back to the definition's default runner"
        );
    }

    /// E4: `agent.send_keys` verifies the pane still hosts the expected agent,
    /// which a plain process never is — the keystroke was being dropped before
    /// it reached the PTY. A command node's interrupt goes through
    /// `pane.send_keys` as `ctrl+c`, which the line discipline turns into
    /// SIGINT, and a delivery the runtime refuses is *recorded* rather than
    /// only logged, so the caller can be told it did not happen.
    #[test]
    fn a_command_node_interrupt_is_a_signal_and_a_failed_delivery_is_recorded() {
        let mut app = test_app_with_hub(EventHub::default());
        let definition = definition_with(Runner::Command);
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");
        app.bind_workflow_node(&InstancePath::new("plan"), binding_for("w9:p9"));

        let effects = app.workflow.apply(
            EngineInput::Interrupt {
                path: InstancePath::new("plan"),
            },
            Instant::now(),
        );
        let keys = effects
            .iter()
            .find_map(|effect| match effect {
                RunEffect::SendKeys { pane, keys } => Some((pane.clone(), keys.clone())),
                _ => None,
            })
            .expect("an interrupt on a bound node sends keys");
        assert_eq!(keys.0, PublicPaneId::new("w9:p9"));
        assert_eq!(
            keys.1,
            vec!["ctrl+c".to_string()],
            "Escape is a claude TUI convention a plain process ignores"
        );

        // `App::new(no_session)` has no workspace, so the pane cannot be
        // written to and the delivery fails.
        app.workflow.clear_delivery_failure();
        app.apply_workflow_engine_input(EngineInput::Interrupt {
            path: InstancePath::new("plan"),
        });
        let failure = app
            .workflow
            .take_delivery_failure()
            .expect("a refused delivery is recorded, not just logged");
        assert_eq!(failure.method, "pane.send_keys");
        assert_eq!(failure.pane, "w9:p9");
        assert!(
            failure.describe().contains("pane.send_keys"),
            "unexpected description: {}",
            failure.describe()
        );
        assert!(
            app.workflow.take_delivery_failure().is_none(),
            "taking the failure clears it, so a later call answers for itself"
        );
    }

    /// D4: the server owns completion. A self-report with no result artifact is
    /// `NeedsAttention` (§4.3) — never a client-side veto that leaves the node
    /// `Running` with the server never told the node tried to finish.
    #[test]
    fn a_report_with_a_null_result_needs_attention_instead_of_stalling() {
        let mut app = test_app_with_hub(EventHub::default());
        let definition = definition_with(Runner::Command);
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");
        app.bind_workflow_node(&InstancePath::new("plan"), binding_for("pane-1"));
        app.workflow
            .record_node_token(&InstancePath::new("plan"), NodeToken::new("node-token"));
        let _ = app.workflow.take_pending_writes(usize::MAX);

        app.report_workflow_node("plan", "node-token", Some(serde_json::Value::Null))
            .expect("a report with no result still reaches the engine");

        let node = app
            .workflow_node_info(&InstancePath::new("plan"))
            .expect("the node projects onto the wire");
        assert_eq!(node.status, WorkflowNodeStatus::NeedsAttention);
        assert_eq!(
            node.evidence, None,
            "a node with no result artifact records no completion evidence"
        );
        assert!(
            app.workflow
                .take_pending_writes(usize::MAX)
                .iter()
                .any(|write| match write {
                    StoreWrite::RunEvent {
                        kind: RunEventKind::Error,
                        payload,
                        ..
                    } => payload["reason"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("no result.json"),
                    _ => false,
                }),
            "the journal records why the node was surfaced"
        );

        // The wire shape is the only way to say "no result"; an internal caller
        // that passes nothing at all is still a shape error.
        assert_eq!(
            app.report_workflow_node("plan", "node-token", None),
            Err(observe::ReportRejected::MissingResult)
        );
    }

    #[test]
    fn an_input_without_a_run_is_a_no_op() {
        let mut state =
            WorkflowRuntimeState::new(EngineConfig::default(), WorkflowPolicy::default());
        let effects = state.apply(
            EngineInput::Tick {
                now: Instant::now(),
            },
            Instant::now(),
        );
        assert!(effects.is_empty());
        assert_eq!(state.run_status(), None);
        assert_eq!(state.next_tick_deadline(), None);
    }

    #[test]
    fn starting_a_run_emits_the_run_and_node_events() {
        let event_hub = EventHub::default();
        let mut app = test_app_with_hub(event_hub.clone());
        let definition = definition();
        let graph = graph_of(&definition);

        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");

        let kinds: Vec<EventKind> = event_hub
            .events_after(0)
            .into_iter()
            .map(|(_, event)| event.event)
            .collect();
        assert!(kinds.contains(&EventKind::WorkflowRunStarted));
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == EventKind::WorkflowNodeCreated)
                .count(),
            2,
            "one node.created per materialised node"
        );

        let started = event_hub
            .events_after(0)
            .into_iter()
            .find_map(|(_, event)| match event.data {
                EventData::WorkflowRunStarted { run } => Some(run),
                _ => None,
            })
            .expect("workflow.run.started carries the run");
        assert_eq!(started.run_id, "workflow_run:test");
        assert_eq!(started.status, WorkflowRunStatus::Running);
        assert_eq!(started.tier, WorkflowTier::High);
        assert_eq!(started.nodes_total, 2);
        assert_eq!(started.nodes_done, 0);
        assert_eq!(
            started.args.get("goal").map(String::as_str),
            Some("ship it")
        );
    }

    #[test]
    fn a_validated_self_report_updates_the_node_and_finishes_the_run() {
        let event_hub = EventHub::default();
        let mut app = test_app_with_hub(event_hub.clone());
        let definition = definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");

        for (path, result) in [
            ("plan", r#"{"plan":"do it"}"#),
            ("implement", r#"{"report":"shipped"}"#),
        ] {
            app.bind_workflow_node(&InstancePath::new(path), binding_for(path));
            let cursor = event_hub.current_sequence();
            app.apply_workflow_engine_input(EngineInput::NodeSelfReport {
                path: InstancePath::new(path),
                token: NodeToken::new("token"),
                result: report(result),
            });
            let kinds: Vec<EventKind> = event_hub
                .events_after(cursor)
                .into_iter()
                .map(|(_, event)| event.event)
                .collect();
            assert!(kinds.contains(&EventKind::WorkflowNodeOutputCheckpoint));
            assert!(kinds.contains(&EventKind::WorkflowNodeUpdated));
        }

        assert_eq!(app.workflow.run_status(), Some(RunStatus::Succeeded));
        let finished = event_hub
            .events_after(0)
            .into_iter()
            .find_map(|(_, event)| match event.data {
                EventData::WorkflowRunFinished { run } => Some(run),
                _ => None,
            })
            .expect("workflow.run.finished carries the run");
        assert_eq!(finished.status, WorkflowRunStatus::Succeeded);
        assert_eq!(finished.nodes_done, 2);
        assert!(finished.ended_at_unix_ms.is_some());

        let node = app
            .workflow_node_info(&InstancePath::new("plan"))
            .expect("the node projects onto the wire");
        assert_eq!(node.status, WorkflowNodeStatus::Succeeded);
        assert_eq!(node.evidence, Some(WorkflowEvidence::SelfReport));
        assert_eq!(node.demand, WorkflowDemand::Critical);
        assert_eq!(node.model, "opus");
        assert_eq!(node.pane_id.as_deref(), Some("plan"));
        assert_eq!(node.attempt, 1);
        assert!(node.blocker.is_none());
    }

    #[test]
    fn durable_writes_reach_the_store_queue_instead_of_the_event_stream() {
        let mut app = test_app_with_hub(EventHub::default());
        let definition = definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");

        assert!(
            app.workflow.pending_write_count() > 0,
            "run and node writes are journalled off the critical path"
        );
        assert!(!app.workflow.persistence_degraded());
        let writes = app.workflow.take_pending_writes(usize::MAX);
        assert!(writes
            .iter()
            .any(|write| matches!(write, StoreWrite::RunStatus { .. })));
    }

    #[test]
    fn an_idle_report_without_a_result_never_succeeds_the_run() {
        let mut app = test_app_with_hub(EventHub::default());
        let definition = definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");
        app.bind_workflow_node(&InstancePath::new("plan"), binding_for("pane-1"));

        let now = Instant::now();
        // An agent that has worked read its seed prompt, so its idle is the
        // "went quiet with nothing to show" case rather than the swallowed-seed
        // one the engine answers with a re-delivery.
        app.apply_workflow_engine_input(EngineInput::AgentStatus {
            pane: PublicPaneId::new("pane-1"),
            state: crate::detect::AgentState::Working,
            at: now,
        });
        for _ in 0..3 {
            app.apply_workflow_engine_input(EngineInput::AgentStatus {
                pane: PublicPaneId::new("pane-1"),
                state: crate::detect::AgentState::Idle,
                at: now,
            });
        }

        let node = app
            .workflow_node_info(&InstancePath::new("plan"))
            .expect("the node projects onto the wire");
        assert_eq!(node.status, WorkflowNodeStatus::NeedsAttention);
        assert_ne!(app.workflow.run_status(), Some(RunStatus::Succeeded));
    }

    #[test]
    fn a_node_report_is_authenticated_against_the_minted_token() {
        let mut app = test_app_with_hub(EventHub::default());
        let definition = definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");
        app.workflow
            .record_node_token(&InstancePath::new("plan"), NodeToken::new("node-token"));

        assert_eq!(
            app.report_workflow_node("implement", "node-token", Some(serde_json::json!({}))),
            Err(observe::ReportRejected::UnknownNode),
            "a node with no minted token cannot report"
        );
        assert_eq!(
            app.report_workflow_node("plan", "guessed", Some(serde_json::json!({}))),
            Err(observe::ReportRejected::InvalidToken)
        );
        assert_eq!(
            app.report_workflow_node("plan", "node-token", None),
            Err(observe::ReportRejected::MissingResult)
        );

        app.report_workflow_node(
            "plan",
            "node-token",
            Some(serde_json::json!({"plan": "do it"})),
        )
        .expect("an authenticated report reaches the engine");
        assert_eq!(
            app.workflow_node_info(&InstancePath::new("plan"))
                .map(|node| node.status),
            Some(WorkflowNodeStatus::Succeeded)
        );
    }

    #[test]
    fn only_the_bundled_claude_stop_hook_counts_as_a_turn_end() {
        let mut app = test_app_with_hub(EventHub::default());
        let definition = definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");

        // Neither reaches the engine, but for different reasons: the pane is
        // unknown here, and `observe::turn_ended` rejects a foreign reporter
        // before the pane is even consulted.
        assert!(
            !app.handle_workflow_app_event(WorkflowAppEvent::NodeHookReported {
                pane_id: crate::layout::PaneId::from_raw(4242),
                source: "some-other-tool".to_string(),
                agent_label: "claude".to_string(),
                state: crate::detect::AgentState::Idle,
            })
        );
        assert_eq!(app.workflow.run_status(), Some(RunStatus::Running));
    }

    #[test]
    fn a_pane_event_for_an_unknown_pane_is_ignored() {
        let mut app = test_app_with_hub(EventHub::default());
        let definition = definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");

        let unknown = crate::layout::PaneId::from_raw(4242);
        assert!(
            !app.handle_workflow_app_event(WorkflowAppEvent::NodeHookReported {
                pane_id: unknown,
                source: "karvex:claude".to_string(),
                agent_label: "claude".to_string(),
                state: crate::detect::AgentState::Idle,
            })
        );
        assert!(
            !app.handle_workflow_app_event(WorkflowAppEvent::NodePaneExited {
                pane_id: unknown,
                code: Some(1),
            })
        );
        assert_eq!(app.workflow.run_status(), Some(RunStatus::Running));
    }

    #[test]
    fn the_tick_arm_only_fires_when_the_clock_is_due() {
        let mut app = test_app_with_hub(EventHub::default());
        assert_eq!(app.workflow_tick_deadline(), None);
        assert!(!app.tick_workflow_engine(Instant::now()));

        let definition = definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");
        let deadline = app
            .workflow_tick_deadline()
            .expect("a live run arms the tick");

        assert!(!app.tick_workflow_engine(deadline - Duration::from_secs(1)));
        assert_eq!(app.workflow_tick_deadline(), Some(deadline));
        app.tick_workflow_engine(deadline);
        assert!(
            app.workflow_tick_deadline()
                .is_some_and(|next| next > deadline),
            "a fired tick rearms the clock"
        );
        // `App::new(no_session)` has no workspace, so the tick's spawn retry
        // exhausts the node's attempt budget and the run stalls. A paused run
        // is still live, which is exactly why the clock stays armed.
        assert_eq!(app.workflow.run_status(), Some(RunStatus::Paused));
        assert!(app.workflow.is_live());
    }

    #[test]
    fn a_notice_becomes_a_toast_only_where_karvex_owns_the_toast() {
        let notice = || UserNotice {
            level: NoticeLevel::Error,
            run: Some(RunId::new("workflow_run:test")),
            path: Some(InstancePath::new("plan")),
            message: "node failed: the pane exited with code 1".to_string(),
        };

        let mut app = test_app_with_hub(EventHub::default());
        app.state.toast_config.delivery = crate::config::ToastDelivery::Off;
        app.show_workflow_notice(notice());
        assert!(
            app.state.toast.is_none(),
            "the in-app toast follows the configured delivery, like every other toast"
        );

        app.state.toast_config.delivery = crate::config::ToastDelivery::Karvex;
        app.show_workflow_notice(notice());

        let toast = app.state.toast.clone().expect("the notice surfaces");
        assert_eq!(toast.kind, ToastKind::NeedsAttention);
        assert!(toast.title.contains("plan"));
        assert_eq!(toast.context, "node failed: the pane exited with code 1");
        assert!(app.toast_deadline.is_some());
    }

    #[test]
    fn a_spawn_plan_carries_the_node_directory_the_argv_and_the_env() {
        let mut app = test_app_with_hub(EventHub::default());
        let definition = definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");

        let plan = app
            .workflow_spawn_plan(&InstancePath::new("plan"), PathBuf::from("/repo"))
            .expect("the root node plans a spawn");

        assert_eq!(plan.spec.runner, Runner::Agent);
        assert_eq!(plan.spec.command, None);
        assert_eq!(plan.spec.label, "plan");
        assert_eq!(plan.spec.cwd, PathBuf::from("/repo"));
        assert!(plan.layout.root.ends_with("plan"));
        assert_eq!(plan.spec.node_dir, plan.layout.root);
        assert!(plan
            .transcript_path
            .ends_with(format!("{}.jsonl", plan.spec.agent_session_id)));
        assert!(plan.task_markdown.contains("## Task"));
        assert!(plan.task_markdown.contains("do plan"));
        assert!(plan
            .task_markdown
            .contains("Reply only through result.json."));
        assert!(plan.inputs.is_empty(), "a root node has no inbound ports");

        let argv = spawn::argv_for(&plan.spec).expect("the agent argv builds");
        assert_eq!(argv.first().map(String::as_str), Some("claude"));
        assert!(argv.contains(&"--session-id".to_string()));
        assert!(argv.contains(&plan.spec.agent_session_id.clone()));
        assert!(argv.contains(&"opus".to_string()), "critical at tier high");

        let env = spawn::node_env(&plan.spec);
        let names: Vec<&str> = env.iter().map(|(name, _)| name.as_str()).collect();
        for expected in [
            spawn::RUN_ID_ENV_VAR,
            spawn::NODE_PATH_ENV_VAR,
            spawn::NODE_DIR_ENV_VAR,
            spawn::NODE_TOKEN_ENV_VAR,
        ] {
            assert!(names.contains(&expected), "{expected} is in the node env");
        }
    }

    /// 2.20: the pane a node runs in is titled from the workflow and the node,
    /// so a run's splits are told apart from each other and from the user's own
    /// panes. `spec.label` cannot do this job — it is the agent name and has to
    /// stay unique.
    #[test]
    fn a_spawn_plan_titles_the_node_pane_after_the_workflow_and_the_node() {
        let mut app = test_app_with_hub(EventHub::default());
        app.state.set_workflow_run_name("ux-dag-probe");
        let definition = definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");

        let plan = app
            .workflow_spawn_plan(&InstancePath::new("plan"), PathBuf::from("/repo"))
            .expect("the root node plans a spawn");
        assert_eq!(plan.pane_title, "ux-dag-probe · plan");
    }

    /// 2.1: a run that reaches a terminal state used to leave the screen
    /// byte-identical to an idle one, because the engine has no notice for a
    /// run that simply finishes.
    #[test]
    fn a_run_that_finishes_reaches_the_user_as_a_notification() {
        let mut app = test_app_with_hub(EventHub::default());
        app.state.toast_config.delivery = crate::config::ToastDelivery::Karvex;
        let definition = definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");
        // The spawn failures the workspace-less fixture produces are notices
        // like any other, so both the slot and the queue behind it are cleared.
        app.state.toast = None;
        app.state.toast_queue.clear();

        for (path, result) in [
            ("plan", r#"{"plan":"do it"}"#),
            ("implement", r#"{"report":"shipped"}"#),
        ] {
            app.bind_workflow_node(&InstancePath::new(path), binding_for(path));
            app.apply_workflow_engine_input(EngineInput::NodeSelfReport {
                path: InstancePath::new(path),
                token: NodeToken::new("token"),
                result: report(result),
            });
        }

        assert_eq!(app.workflow.run_status(), Some(RunStatus::Succeeded));
        // The run notice is the last of the batch and no longer has to win a
        // single slot to be seen: the node notices ahead of it render first and
        // it follows as each expires (§4 D10).
        // Phase 3 adds one notice *after* the run's: this fixture has no
        // workspace, so the `.summary` epilogue cannot be spawned and gives up
        // with a notice of its own (`07-phase3-plan.md` §4 D1). That never
        // changes the run's outcome, and the run's own notice is still
        // delivered — which is what this test is about.
        let surfaced = surfaced_toasts(&app);
        let end = surfaced
            .iter()
            .find(|toast| toast.title == "Workflow run")
            .expect("the run's end is shown");
        assert!(end.context.contains("finished"), "{:?}", end.context);
    }

    #[test]
    fn a_downstream_plan_carries_the_upstream_port_into_inputs_and_the_prompt() {
        let mut app = test_app_with_hub(EventHub::default());
        let definition = ported_definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");
        app.bind_workflow_node(&InstancePath::new("plan"), binding_for("plan"));
        app.apply_workflow_engine_input(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: report(r#"{"plan":"ship the api","summary":"ship the api"}"#),
        });

        let plan = app
            .workflow_spawn_plan(&InstancePath::new("implement"), PathBuf::from("/repo"))
            .expect("the downstream node plans a spawn");
        assert_eq!(
            plan.inputs
                .get("plan")
                .and_then(|contributions| contributions.first())
                .and_then(|contribution| contribution.payload.as_str()),
            Some("ship the api"),
            "a summary edge writes the upstream summary to inputs/<port>.json"
        );
        assert!(plan.task_markdown.contains("Implement ship the api"));
        assert!(plan.task_markdown.contains(&format!(
            "`plan`: `{}`",
            plan.layout.input_file("plan").display()
        )));
        // The node's cwd is `/repo`, not its node directory, so a `./` path in
        // `task.md` names a file in the workspace: the defect that made nodes
        // write `result.json` where nothing was watching for it.
        assert!(
            !plan.task_markdown.contains("`./"),
            "task.md may not name a karvex file relative to the node's cwd: {}",
            plan.task_markdown
        );
        assert!(plan
            .task_markdown
            .contains(&format!("`{}`", plan.layout.result.display())));
        assert!(plan
            .task_markdown
            .contains(&format!("`{}`", plan.layout.output_schema.display())));
    }

    /// `fanout → collect` on a `shard` data edge, with a `worker` template
    /// `fanout` may instantiate. The same shape as
    /// `tests/fixtures/workflow/expand.toml`, small enough to drive a spawn
    /// plan directly.
    fn expandable_definition() -> Kvdag {
        let mut fanout = definition_node("fanout", Runner::Agent, "plan");
        fanout.label = "Fan out".to_string();
        fanout.expand_allow = vec![NodeKey::new("worker")];
        fanout.expand_max = 4;

        let mut worker = definition_node("worker", Runner::Agent, "report");
        worker.label = "Worker".to_string();
        worker.is_template = true;
        worker.prompt_template = "Work one shard of: {{goal}}".to_string();

        let mut collect = definition_node("collect", Runner::Agent, "report");
        collect.label = "Collect".to_string();
        collect.prompt_template = "Collect the reports:\n{{shard}}".to_string();

        Kvdag::try_new(KvdagSpec {
            version_id: KvdagVersionId::new("kvdag_version:test"),
            workflow_id: WorkflowId::new("workflow:test"),
            version: 1,
            parent: None,
            contract: "Reply only through result.json.".to_string(),
            growth: GrowthLimits::default(),
            args: vec![ArgSpec {
                name: "goal".to_string(),
                required: false,
                default: None,
                description: String::new(),
            }],
            nodes: vec![fanout, worker, collect],
            edges: vec![KvdagEdge {
                from: NodeKey::new("fanout"),
                to: NodeKey::new("collect"),
                kind: EdgeKind::Data,
                condition: None,
                payload: EdgePayload::Summary,
                port: Some("shard".to_string()),
            }],
        })
        .expect("fixture kvdag is valid")
    }

    /// Proposes one child of `worker` with its own label and slot override,
    /// exactly as `kvx workflow node expand --label … --input k=v` does.
    fn expand_child(app: &mut App, label: &str, goal: &str) {
        app.apply_workflow_engine_input(EngineInput::ExpandProposed {
            path: InstancePath::new("fanout"),
            token: NodeToken::new("token"),
            proposals: vec![crate::workflow::engine::expand::ExpandProposal {
                template: NodeKey::new("worker"),
                label: label.to_string(),
                inputs: BTreeMap::from([("goal".to_string(), goal.to_string())]),
                count: Some(1),
            }],
        });
    }

    /// The retest's first two P0s, at the surface a teammate actually reads:
    /// the accepted `--input` has to reach the child's rendered `task.md`, and
    /// the required `--label` has to be the child's name in its own prompt and
    /// on its pane. Before the fix both were validated, journalled, and then
    /// dropped, so two children of one parent were byte-identical documents
    /// under one name.
    #[test]
    fn an_expansion_childs_task_carries_its_own_label_and_input_override() {
        let mut app = test_app_with_hub(EventHub::default());
        app.state.set_workflow_run_name("dgprobe");
        let definition = expandable_definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");

        expand_child(&mut app, "Shard: auth", "just src/auth");
        expand_child(&mut app, "Shard: ui", "just src/ui");

        let first = app
            .workflow_spawn_plan(
                &InstancePath::new("fanout/worker/1"),
                PathBuf::from("/repo"),
            )
            .expect("the first child plans a spawn");
        let second = app
            .workflow_spawn_plan(
                &InstancePath::new("fanout/worker/2"),
                PathBuf::from("/repo"),
            )
            .expect("the second child plans a spawn");

        assert!(
            first
                .task_markdown
                .contains("Work one shard of: just src/auth"),
            "the accepted --input must fill the template slot: {}",
            first.task_markdown
        );
        assert!(
            second
                .task_markdown
                .contains("Work one shard of: just src/ui"),
            "{}",
            second.task_markdown
        );
        assert_ne!(
            first.task_markdown, second.task_markdown,
            "a proposing node with no way to differentiate its children has no fan-out"
        );
        assert!(
            first.task_markdown.starts_with("# Shard: auth\n"),
            "a child's task.md is titled with its own label: {}",
            first.task_markdown
        );
        assert_eq!(first.pane_title, "dgprobe · Shard: auth");
        assert_eq!(second.pane_title, "dgprobe · Shard: ui");
        assert_eq!(
            first.spec.label, "Shard: auth",
            "the agent name is the child's label, not the template's"
        );
    }

    /// The run argument is the default and the proposal's `--input` is the
    /// override (`06-phase2-plan.md` §4 D3). A child that renders the run's
    /// `goal` where its parent asked for a shard is the retest's finding
    /// exactly.
    #[test]
    fn an_expansion_input_overrides_the_run_argument_of_the_same_name() {
        let mut app = test_app_with_hub(EventHub::default());
        let definition = expandable_definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");
        expand_child(&mut app, "Shard", "just src/auth");

        let plan = app
            .workflow_spawn_plan(
                &InstancePath::new("fanout/worker/1"),
                PathBuf::from("/repo"),
            )
            .expect("the child plans a spawn");
        assert!(plan.task_markdown.contains("just src/auth"));
        assert!(
            !plan.task_markdown.contains("ship it"),
            "the run argument must not win over the child's own override: {}",
            plan.task_markdown
        );
    }

    /// The DAG's label map used to be keyed by kvdag node *key*, so a whole
    /// generation cut from one template drew as N boxes labelled alike. It is
    /// keyed by instance path now, and the per-key entries stay as the
    /// fallback for anything the graph does not carry.
    #[test]
    fn the_mirrored_label_map_names_each_expansion_child_individually() {
        let mut app = test_app_with_hub(EventHub::default());
        let definition = expandable_definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");
        expand_child(&mut app, "Shard: auth", "just src/auth");
        expand_child(&mut app, "Shard: ui", "just src/ui");

        let labels = &app.state.workflow_run_presentation().node_labels;
        assert_eq!(
            labels.get("fanout/worker/1").map(String::as_str),
            Some("Shard: auth")
        );
        assert_eq!(
            labels.get("fanout/worker/2").map(String::as_str),
            Some("Shard: ui")
        );
        assert_eq!(
            labels.get("fanout").map(String::as_str),
            Some("Fan out"),
            "a static node's path is its key, so both lookups agree"
        );
        assert_eq!(
            labels.get("worker").map(String::as_str),
            Some("Worker"),
            "the template's own authored label stays as the fallback"
        );
    }

    /// The third P0: every child inherits its parent's outbound edge, so
    /// `collect`'s one `shard` port carries the parent's result *and* the whole
    /// generation's. Keyed by port alone the last writer won and the rest were
    /// lost with nothing said on any surface.
    #[test]
    fn a_fan_in_node_receives_every_upstream_that_fired_into_one_port() {
        let mut app = test_app_with_hub(EventHub::default());
        let definition = expandable_definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");
        expand_child(&mut app, "Shard: auth", "just src/auth");
        expand_child(&mut app, "Shard: ui", "just src/ui");

        for (path, summary) in [
            ("fanout", "the plan"),
            ("fanout/worker/1", "auth report"),
            ("fanout/worker/2", "ui report"),
        ] {
            app.bind_workflow_node(&InstancePath::new(path), binding_for(path));
            app.apply_workflow_engine_input(EngineInput::NodeSelfReport {
                path: InstancePath::new(path),
                token: NodeToken::new("token"),
                result: report(&format!(
                    r#"{{"plan":"p","report":"r","summary":"{summary}"}}"#
                )),
            });
        }

        let plan = app
            .workflow_spawn_plan(&InstancePath::new("collect"), PathBuf::from("/repo"))
            .expect("the fan-in node plans a spawn");
        let shard = plan
            .inputs
            .get("shard")
            .expect("the fan-in port carries its contributions");
        assert_eq!(
            shard
                .iter()
                .map(|contribution| contribution.from.as_str())
                .collect::<Vec<&str>>(),
            vec!["fanout", "fanout/worker/1", "fanout/worker/2"],
            "all three upstreams fired into `shard`; none may overwrite another"
        );
        for expected in ["the plan", "auth report", "ui report"] {
            assert!(
                plan.task_markdown.contains(expected),
                "the fan-in prompt must carry {expected}: {}",
                plan.task_markdown
            );
        }
        assert!(
            plan.task_markdown
                .contains("[from fanout/worker/1 · Shard: auth]"),
            "each contribution is attributed to the node that produced it: {}",
            plan.task_markdown
        );
        assert!(
            plan.task_markdown.contains(&format!(
                "`fanout/worker/2`: `{}`",
                plan.layout
                    .input_source_file("shard", "fanout-worker-2")
                    .display()
            )),
            "and to a file it can open on its own, named absolutely because the \
             node's cwd is not its node directory: {}",
            plan.task_markdown
        );
    }

    #[test]
    fn a_spawn_failure_is_retried_then_stalls_the_run_on_the_node() {
        let mut app = test_app_with_hub(EventHub::default());
        app.state.toast_config.delivery = crate::config::ToastDelivery::Karvex;
        let definition = definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");

        // `App::new(no_session)` has no workspace, so every spawn fails.
        let path = InstancePath::new("plan");
        assert_eq!(app.workflow.spawn_failure_count(&path), 1);
        assert_eq!(
            app.workflow.pending_spawns(),
            std::slice::from_ref(&path),
            "a failed spawn leaves the node admitted so a later tick retries it"
        );
        assert!(
            surfaced_notices(&app)
                .iter()
                .any(|notice| notice.contains("retrying")),
            "{:?}",
            surfaced_notices(&app)
        );

        app.tick_workflow_engine(
            app.workflow_tick_deadline()
                .expect("a live run arms the tick"),
        );
        assert_eq!(
            app.workflow.spawn_failure_count(&path),
            SPAWN_ATTEMPT_BUDGET
        );
        // The give-up notice is the newest one the user will see. It no longer
        // has to *replace* the retry notice to be surfaced — that is the point
        // of the queue (§4 D10).
        let surfaced = surfaced_notices(&app);
        assert!(
            surfaced
                .last()
                .is_some_and(|notice| !notice.contains("retrying")),
            "{surfaced:?}"
        );

        // A node nothing will ever start must not sit `Ready` forever: it takes
        // the failure as a status, which lets §3.2's conjunction stall the run
        // with a surfaced reason instead of leaving it `Running` — and, because
        // a live run blocks every later `workflow.run`, wedging the subsystem.
        assert_eq!(
            app.workflow.node(&path).map(|node| node.status),
            Some(NodeStatus::NeedsAttention)
        );
        assert_eq!(app.workflow.run_status(), Some(RunStatus::Paused));
        assert!(
            app.workflow.pending_spawns().is_empty(),
            "the node is no longer admitted"
        );

        app.tick_workflow_engine(
            app.workflow_tick_deadline()
                .expect("a paused run is still live, so the clock stays armed"),
        );
        assert_eq!(
            app.workflow.spawn_failure_count(&path),
            SPAWN_ATTEMPT_BUDGET,
            "the attempt budget stops the retry loop"
        );

        // A restart is the way out: the node is admitted again with a fresh
        // spawn budget and the run leaves the pause.
        app.apply_workflow_engine_input(EngineInput::RestartNode { path: path.clone() });
        assert_eq!(
            app.workflow.spawn_failure_count(&path),
            1,
            "the restart cleared the budget, and the immediate retry spent one"
        );
        assert_eq!(
            app.workflow.pending_spawns(),
            std::slice::from_ref(&path),
            "the node is admitted again"
        );
        assert_eq!(app.workflow.run_status(), Some(RunStatus::Running));
    }

    #[test]
    fn a_run_info_projection_is_scoped_to_the_active_run() {
        let mut app = test_app_with_hub(EventHub::default());
        let definition = definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");

        assert!(app
            .workflow_run_info(&RunId::new("workflow_run:test"))
            .is_some());
        assert!(
            app.workflow_run_info(&RunId::new("workflow_run:other"))
                .is_none(),
            "an effect for a run this server no longer holds emits nothing"
        );
        assert!(app
            .workflow_node_info(&InstancePath::new("missing"))
            .is_none());
    }

    fn edge_create(to: &str) -> StoreWrite {
        StoreWrite::RunEdgeCreated {
            run: RunId::new("workflow_run:test"),
            from: InstancePath::new("plan"),
            to: InstancePath::new(to),
            kind: EdgeKind::Sequence,
            kvdag_edge: None,
            condition_result: None,
            fired: false,
        }
    }

    fn node_status_write(path: &str) -> StoreWrite {
        StoreWrite::RunNode {
            run: RunId::new("workflow_run:test"),
            path: InstancePath::new(path),
            status: NodeStatus::Running,
            attempt: 1,
            binding: None,
            usage: crate::workflow::model::NodeUsage::default(),
            evidence: None,
            succession: None,
            started_at_unix_ms: None,
            ended_at_unix_ms: None,
            restored_from: None,
        }
    }

    fn queued_paths(state: &WorkflowRuntimeState) -> Vec<String> {
        state
            .pending_writes
            .iter()
            .map(|write| match write {
                StoreWrite::RunEdgeCreated { to, .. } => format!("create:{to}"),
                StoreWrite::RunNode { path, .. } => format!("update:{path}"),
                other => format!("other:{other:?}"),
            })
            .collect()
    }

    /// §4 D7. `write_run_edge`/`write_run_node` are find-then-`UPDATE` and error
    /// on a missing row, so evicting the oldest write blindly would drop a
    /// create and turn every later write for that path into a permanent
    /// failure. The oldest *update* goes instead.
    #[test]
    fn write_queue_overflow_evicts_an_update_and_never_a_create() {
        let mut state =
            WorkflowRuntimeState::new(EngineConfig::default(), WorkflowPolicy::default());
        state.queue_write(edge_create("child/1"));
        state.queue_write(node_status_write("oldest"));
        state.queue_write(edge_create("child/2"));
        while state.pending_write_count() < PENDING_WRITE_BUDGET {
            state.queue_write(node_status_write("filler"));
        }

        state.queue_write(node_status_write("newest"));

        assert_eq!(state.pending_write_count(), PENDING_WRITE_BUDGET);
        assert_eq!(state.dropped_write_count(), 1);
        assert!(state.persistence_degraded());
        let queued = queued_paths(&state);
        assert_eq!(
            queued.first().map(String::as_str),
            Some("create:child/1"),
            "the create at the head survives: {queued:?}",
        );
        assert!(
            queued.contains(&"create:child/2".to_string()),
            "every create survives: {queued:?}",
        );
        assert!(
            !queued.contains(&"update:oldest".to_string()),
            "the oldest evictable write is the one that goes: {queued:?}",
        );
    }

    /// §4 D7's other half: with nothing safe to drop the queue grows past its
    /// budget and says so, because a corrupted journal is worse than a larger
    /// queue and the in-memory graph is authoritative either way.
    #[test]
    fn a_write_queue_of_only_creates_grows_past_budget_instead_of_dropping_one() {
        let mut state =
            WorkflowRuntimeState::new(EngineConfig::default(), WorkflowPolicy::default());
        for index in 0..PENDING_WRITE_BUDGET {
            state.queue_write(edge_create(&format!("child/{index}")));
        }
        assert!(!state.persistence_degraded());

        state.queue_write(edge_create("child/overflow"));

        assert_eq!(state.pending_write_count(), PENDING_WRITE_BUDGET + 1);
        assert_eq!(
            state.dropped_write_count(),
            0,
            "a create is never dropped, so nothing was dropped"
        );
        assert!(
            state.persistence_degraded(),
            "the run reports the overflow instead of hiding it"
        );
        let queued = queued_paths(&state);
        assert_eq!(queued.first().map(String::as_str), Some("create:child/0"));
        assert_eq!(
            queued.last().map(String::as_str),
            Some("create:child/overflow")
        );
    }

    /// H6 / §4 D14. `handle_tab_close` and `handle_workspace_close` drop every
    /// pane they own without telling the engine, so the live-run tick
    /// reconciles bindings against the layout. Exactly once per dead pane.
    #[test]
    fn a_bound_pane_missing_from_the_layout_is_reconciled_once() {
        let mut app = test_app_with_hub(EventHub::default());
        let definition = definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");
        let path = InstancePath::new("plan");
        // `App::new(no_session)` has no workspace at all, so this binding names
        // a pane the layout cannot resolve — exactly the state a bulk close
        // leaves behind.
        app.bind_workflow_node(&path, binding_for("w1:p1"));
        assert_eq!(
            app.workflow.node(&path).map(|node| node.status),
            Some(NodeStatus::Running)
        );

        assert!(
            app.reconcile_workflow_pane_bindings(),
            "the orphaned binding is reported to the engine"
        );
        let node = app.workflow.node(&path).expect("the node exists").clone();
        assert!(
            node.binding.is_none() || node.status.is_terminal(),
            "a reconciled node either retries in a fresh pane or fails; it never stays running \
             with a dead binding: {node:?}",
        );
        assert_ne!(node.status, NodeStatus::Running);

        assert!(
            !app.reconcile_workflow_pane_bindings(),
            "a dead pane is reported once, not every tick"
        );
        assert!(!app.reconcile_workflow_pane_bindings());
    }

    /// The retry budget is 2, so the second dead pane is terminal — the node
    /// reaches a settled status rather than sitting `running` forever (H6).
    #[test]
    fn a_node_whose_pane_keeps_disappearing_reaches_a_terminal_status() {
        let mut app = test_app_with_hub(EventHub::default());
        let definition = definition();
        let graph = graph_of(&definition);
        app.start_workflow_run(active_run(), definition, graph)
            .expect("the run starts");
        let path = InstancePath::new("plan");

        for attempt in 0..4 {
            app.bind_workflow_node(&path, binding_for(&format!("w1:p{attempt}")));
            app.reconcile_workflow_pane_bindings();
        }

        let status = app
            .workflow
            .node(&path)
            .map(|node| node.status)
            .expect("the node exists");
        assert!(
            status.is_terminal(),
            "the node settled instead of staying running: {status:?}"
        );
    }

    /// The bulk closes are exactly what `close_pane` does *not* cover
    /// (§4 D14): `handle_tab_close` and `handle_workspace_close` drop every
    /// pane they own without telling anyone, so the backstop has to catch them
    /// within one tick.
    #[tokio::test]
    async fn a_bulk_close_is_reconciled_within_one_tick() {
        for (label, close) in [
            (
                "workspace",
                Method::WorkspaceClose(crate::api::schema::WorkspaceTarget {
                    workspace_id: "closed".to_string(),
                }),
            ),
            (
                "tab",
                Method::TabClose(crate::api::schema::TabTarget {
                    tab_id: "closed:t1".to_string(),
                }),
            ),
        ] {
            let mut app = test_app_with_hub(EventHub::default());
            app.state.workspaces = vec![crate::workspace::Workspace::test_new("bulk")];
            app.state.workspaces[0].id = "closed".to_string();
            app.state.ensure_test_terminals();
            app.state.active = Some(0);
            let pane_id = app.state.workspaces[0].tabs[0].root_pane;
            let public = app
                .public_pane_id(0, pane_id)
                .expect("the test workspace has a root pane");

            let definition = single_attempt_definition();
            let graph = graph_of(&definition);
            let path = InstancePath::new("plan");
            app.workflow
                .start(active_run(), definition, graph, Instant::now())
                .expect("the run starts");
            app.workflow
                .bind_node(&path, binding_for(&public), Instant::now());

            app.handle_api_request(crate::api::schema::Request {
                id: "close".into(),
                method: close,
            });
            assert_eq!(
                app.workflow.node(&path).map(|node| node.status),
                Some(NodeStatus::Running),
                "{label} close does not tell the engine anything by itself"
            );

            let deadline = app
                .workflow_tick_deadline()
                .expect("a live run arms the tick");
            app.tick_workflow_engine(deadline);

            assert_eq!(
                app.workflow.node(&path).map(|node| node.status),
                Some(NodeStatus::Failed),
                "the {label} close is caught by the next tick"
            );
        }
    }

    /// A pane that is still in the layout is never reconciled away.
    #[test]
    fn a_live_pane_binding_is_left_alone() {
        let mut app = test_app_with_hub(EventHub::default());
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("reconcile")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let public = app
            .public_pane_id(0, pane_id)
            .expect("the test workspace has a root pane");

        // The engine is driven directly: this test is about the layout lookup,
        // and `start_workflow_run` would try to spawn a real pane in the
        // workspace it needs.
        let definition = definition();
        let graph = graph_of(&definition);
        let path = InstancePath::new("plan");
        app.workflow
            .start(active_run(), definition, graph, Instant::now())
            .expect("the run starts");
        app.workflow
            .bind_node(&path, binding_for(&public), Instant::now());

        assert!(!app.reconcile_workflow_pane_bindings());
        assert_eq!(
            app.workflow.node(&path).map(|node| node.status),
            Some(NodeStatus::Running),
            "a live pane leaves its node alone"
        );
    }

    /// H6's direct path (§4 D14). A pane that is *closed* rather than dying
    /// used to leave its node `running` forever, because only
    /// `AppEvent::PaneDied` ever reached the engine. `App::close_pane` now
    /// reports the exit itself, which covers `pane.close` and the TUI
    /// keybinding that routes through it.
    // `pane.close` shuts down the pane's terminal runtime, which needs a
    // reactor.
    #[tokio::test]
    async fn closing_a_node_pane_through_the_api_fails_the_node() {
        let mut app = test_app_with_hub(EventHub::default());
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("close")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let public = app
            .public_pane_id(0, pane_id)
            .expect("the test workspace has a root pane");

        // The engine is driven directly and the node gets one attempt, so the
        // close is the whole story: no spawn retry, no real PTY.
        let definition = single_attempt_definition();
        let graph = graph_of(&definition);
        let path = InstancePath::new("plan");
        app.workflow
            .start(active_run(), definition, graph, Instant::now())
            .expect("the run starts");
        app.workflow
            .bind_node(&path, binding_for(&public), Instant::now());
        assert_eq!(
            app.workflow.node(&path).map(|node| node.status),
            Some(NodeStatus::Running)
        );

        app.handle_api_request(crate::api::schema::Request {
            id: "close".into(),
            method: Method::PaneClose(crate::api::schema::PaneTarget {
                pane_id: public.clone(),
            }),
        });

        let node = app.workflow.node(&path).expect("the node exists").clone();
        assert_eq!(
            node.status,
            NodeStatus::Failed,
            "a closed pane is as fatal to the node as a dead one: {node:?}"
        );
        assert!(
            !app.reconcile_workflow_pane_bindings(),
            "the direct call already reported it, so the tick backstop has nothing to do"
        );
    }

    /// P1's second half: a run whose node set grows has to say so on the run
    /// stream. The engine emits `RunUpdated` only from `pause()`/`resume()`, so
    /// an expansion used to move `nodes_total` without a single
    /// `workflow.run.updated` — a subscriber saw the node events and then a
    /// finished run, and nothing carrying the new total in between.
    #[test]
    fn an_expansion_emits_a_run_updated_before_the_run_finishes() {
        let event_hub = EventHub::default();
        let mut app = test_app_with_hub(event_hub.clone());
        let definition = definition();
        let graph = graph_of(&definition);
        let run_id = RunId::new("workflow_run:test");
        app.workflow
            .start(active_run(), definition, graph, Instant::now())
            .expect("the run starts");
        let cursor = event_hub.current_sequence();

        app.dispatch_workflow_effects(vec![
            RunEffect::Emit(WorkflowEvent::NodeCreated {
                run: run_id.clone(),
                path: InstancePath::new("plan"),
            }),
            RunEffect::Emit(WorkflowEvent::NodeCreated {
                run: run_id.clone(),
                path: InstancePath::new("implement"),
            }),
            RunEffect::Emit(WorkflowEvent::RunFinished {
                run: run_id.clone(),
                status: RunStatus::Succeeded,
            }),
        ]);

        let emitted: Vec<_> = event_hub.events_after(cursor).into_iter().collect();
        let kinds: Vec<EventKind> = emitted.iter().map(|(_, event)| event.event).collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::WorkflowNodeCreated,
                EventKind::WorkflowNodeCreated,
                EventKind::WorkflowRunUpdated,
                EventKind::WorkflowRunFinished,
            ],
            "exactly one run.updated per batch that materialised nodes, and it lands \
             before the run.finished it precedes"
        );

        let EventData::WorkflowRunUpdated { run } = emitted[2].1.data.clone() else {
            panic!("unexpected event data");
        };
        assert_eq!(
            run.nodes_total, 2,
            "the synthetic update carries the refreshed total, which is the whole point of it"
        );
        assert_eq!(run.run_id, "workflow_run:test");
    }

    /// The growth flag is cleared by the engine's own `RunUpdated` rather than
    /// firing alongside it: a paused run must not get two updates for one fact.
    #[test]
    fn a_batch_that_already_carries_a_run_update_does_not_get_a_second_one() {
        let event_hub = EventHub::default();
        let mut app = test_app_with_hub(event_hub.clone());
        let definition = definition();
        let graph = graph_of(&definition);
        let run_id = RunId::new("workflow_run:test");
        app.workflow
            .start(active_run(), definition, graph, Instant::now())
            .expect("the run starts");
        let cursor = event_hub.current_sequence();

        app.dispatch_workflow_effects(vec![
            RunEffect::Emit(WorkflowEvent::NodeCreated {
                run: run_id.clone(),
                path: InstancePath::new("plan"),
            }),
            RunEffect::Emit(WorkflowEvent::RunUpdated {
                run: run_id.clone(),
                status: RunStatus::Paused,
            }),
        ]);

        let kinds: Vec<EventKind> = event_hub
            .events_after(cursor)
            .into_iter()
            .map(|(_, event)| event.event)
            .collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::WorkflowNodeCreated,
                EventKind::WorkflowRunUpdated,
            ],
            "the engine's own run update already carries the grown total, so the batch \
             does not also get a synthetic one"
        );
    }

    /// §4 D5 + D11: the one new wire event, plus the toast that is its fourth
    /// and only optional surface.
    #[test]
    fn a_growth_limit_reaches_the_wire_and_the_toast() {
        let event_hub = EventHub::default();
        let mut app = test_app_with_hub(event_hub.clone());
        app.state.toast_config.delivery = crate::config::ToastDelivery::Karvex;
        let cursor = event_hub.current_sequence();

        app.emit_workflow_event(WorkflowEvent::GrowthLimited {
            run: RunId::new("workflow_run:test"),
            path: InstancePath::new("plan"),
            template: NodeKey::new("reviewer"),
            limit: ExpandLimit::MaxNodes,
            limit_value: 12,
            requested: 4,
            accepted: 2,
            message: "max_nodes reached".to_string(),
        });

        let data = event_hub
            .events_after(cursor)
            .into_iter()
            .find_map(|(_, event)| {
                (event.event == EventKind::WorkflowGrowthLimited).then_some(event.data)
            })
            .expect("workflow.growth.limited is emitted");
        let EventData::WorkflowGrowthLimited {
            run_id,
            path,
            template,
            limit,
            limit_value,
            requested,
            accepted,
            message,
        } = data
        else {
            panic!("unexpected event data");
        };
        assert_eq!(run_id, "workflow_run:test");
        assert_eq!(path, "plan");
        assert_eq!(template, "reviewer");
        assert_eq!(limit, WorkflowGrowthLimitKind::MaxNodes);
        assert_eq!((limit_value, requested, accepted), (12, 4, 2));
        assert_eq!(message, "max_nodes reached");

        let toast = app.state.toast.as_ref().expect("the notice surfaces");
        assert_eq!(toast.kind, ToastKind::NeedsAttention);
        assert!(toast.title.contains("plan"));
        assert_eq!(toast.context, "max_nodes reached");
        assert!(app.toast_deadline.is_some());
    }

    /// §4 D11's other two non-optional surfaces. The event is a notification —
    /// a client that connects after it was sent has missed it — so the limit
    /// has to survive as a *fact* on the run and on the node that proposed,
    /// which is what `run show`, `node show`, and the DAG banner all read. The
    /// durable store has no column for it, so the live projection is the only
    /// authority while the run is active.
    #[test]
    fn a_growth_limit_becomes_a_fact_on_the_run_the_node_and_the_overlay() {
        let mut app = test_app_with_hub(EventHub::default());
        let definition = definition();
        let graph = graph_of(&definition);
        let run_id = RunId::new("workflow_run:test");
        let path = InstancePath::new("plan");
        app.workflow
            .start(active_run(), definition, graph, Instant::now())
            .expect("the run starts");
        assert!(
            app.workflow_run_info(&run_id)
                .expect("the run projects")
                .growth_limited
                .is_none(),
            "an unlimited run reports no limit rather than a reassuring zero"
        );

        app.emit_workflow_event(WorkflowEvent::GrowthLimited {
            run: run_id.clone(),
            path: path.clone(),
            template: NodeKey::new("reviewer"),
            limit: ExpandLimit::MaxNodes,
            limit_value: 12,
            requested: 4,
            accepted: 2,
            message: "max_nodes 12 reached; 2 of 4 requested nodes created".to_string(),
        });
        app.mirror_workflow_run_graph();

        let run_limit = app
            .workflow_run_info(&run_id)
            .expect("the run projects")
            .growth_limited
            .expect("the run reports the limit it hit");
        assert_eq!(run_limit.kind, WorkflowGrowthLimitKind::MaxNodes);
        assert_eq!(
            (
                run_limit.limit_value,
                run_limit.requested,
                run_limit.accepted
            ),
            (12, 4, 2)
        );
        assert!(
            run_limit.at_unix_ms > 0,
            "the breach is stamped: {run_limit:?}"
        );

        let node_limit = app
            .workflow_node_info(&path)
            .expect("the node projects")
            .growth_limited
            .expect("the limit is attributed to the node that proposed");
        assert_eq!(node_limit, run_limit);
        assert!(
            app.workflow_node_info(&InstancePath::new("implement"))
                .expect("a sibling projects")
                .growth_limited
                .is_none(),
            "a node that never proposed is not blamed for the run's ceiling"
        );

        let presentation = app.state.workflow_run_presentation();
        assert_eq!(
            presentation.growth_banner.as_deref(),
            Some("growth limited · max_nodes 12 reached · 2 of 4 requested nodes created"),
            "the DAG banner names the ceiling and the shortfall"
        );
        assert_eq!(
            presentation.growth_notices.get("plan").map(String::as_str),
            Some("growth limited: max_nodes 12 · 2 of 4"),
            "and the proposing node carries its own notice: {presentation:?}"
        );
    }

    /// H4's data half seen from the producer: a node notice and the run notice
    /// raised in one batch both survive. `App::expire_toast_or_show_next` is
    /// what drains the queue, and `src/app/mod.rs` pins that across an expiry.
    #[test]
    fn two_workflow_notices_in_one_batch_both_survive() {
        let mut app = test_app_with_hub(EventHub::default());
        app.state.toast_config.delivery = crate::config::ToastDelivery::Karvex;

        app.show_workflow_notice(UserNotice {
            level: NoticeLevel::Warning,
            run: Some(RunId::new("workflow_run:test")),
            path: Some(InstancePath::new("plan")),
            message: "needs attention: the node is waiting for a human".to_string(),
        });
        app.show_workflow_notice(UserNotice {
            level: NoticeLevel::Info,
            run: Some(RunId::new("workflow_run:test")),
            path: None,
            message: "the run finished".to_string(),
        });

        let toast = app.state.toast.as_ref().expect("the node notice shows");
        assert!(toast.title.contains("plan"));
        assert_eq!(
            app.state
                .toast_queue
                .front()
                .map(|queued| queued.context.as_str()),
            Some("the run finished"),
            "the run-level notice waits instead of destroying the node notice"
        );
    }

    /// The escalating deliveries never touched the slot, so they must not start
    /// queueing either.
    #[test]
    fn a_non_karvex_delivery_queues_nothing() {
        let mut app = test_app_with_hub(EventHub::default());
        app.state.toast_config.delivery = crate::config::ToastDelivery::Off;

        for index in 0..3 {
            app.show_workflow_notice(UserNotice {
                level: NoticeLevel::Warning,
                run: None,
                path: None,
                message: format!("notice {index}"),
            });
        }

        assert!(app.state.toast.is_none());
        assert!(app.state.toast_queue.is_empty());
        assert!(app.toast_deadline.is_none());
    }
}
