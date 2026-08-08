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
use std::path::PathBuf;
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
use crate::workflow::binding::{observe, spawn};
use crate::workflow::engine::expand::ExpandLimit;
use crate::workflow::engine::{DeliveryFailureNote, Engine, EngineConfig};
use crate::workflow::model::{
    Demand, EdgeKind, EdgePayload, EngineInput, Evidence, GrowthLimits, InstancePath, Kvdag,
    KvdagVersionId, NodeBinding, NodeStatus, NodeToken, NoticeLevel, OutputSchema, PublicPaneId,
    RunEffect, RunGraph, RunId, RunNode, RunNodeIdx, RunStatus, Runner, SpawnSpec, StoreWrite,
    Succession, UserNotice, WorkflowEvent, WorkflowId,
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
        }
    }

    pub(crate) fn with_args(mut self, args: HashMap<String, String>) -> Self {
        self.args = args;
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
        StoreWrite::RunNodeCreated { .. } | StoreWrite::RunEdgeCreated { .. }
    )
}

impl WorkflowRuntimeState {
    pub(crate) fn new(config: EngineConfig) -> Self {
        Self {
            config,
            engine: Engine::new(config),
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
        }
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
        self.config
    }

    pub(crate) fn engine(&self) -> &Engine {
        &self.engine
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

    pub(crate) fn run_status(&self) -> Option<RunStatus> {
        self.engine.graph().map(|graph| graph.status)
    }

    /// A run is live while it can still make progress. A finished run stays
    /// readable until the next one replaces it.
    pub(crate) fn is_live(&self) -> bool {
        matches!(
            self.run_status(),
            Some(RunStatus::Pending | RunStatus::Running | RunStatus::Paused)
        )
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
        self.engine = Engine::new(self.config);
        self.pending_spawns.clear();
        self.claimed_spawns.clear();
        self.spawn_failures.clear();
        self.node_tokens.clear();
        // Persistence health is a property of the run, not of the process: a
        // previous run's lost write must not leave the next one reporting
        // itself degraded.
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
        self.next_tick_at = match (live, self.next_tick_at) {
            (false, _) => None,
            (true, Some(deadline)) if deadline > now => Some(deadline),
            (true, _) => Some(now + WORKFLOW_TICK_INTERVAL),
        };
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
    pub(crate) fn runner_for_pane(&self, pane: &PublicPaneId) -> Runner {
        let runner = self.engine.graph().and_then(|graph| {
            let key = &graph.node_by_pane(pane)?.key;
            Some(self.engine.definition()?.node(key)?.runner)
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
fn task_input_sources(
    port: &str,
    contributions: &[spawn::PortContribution],
) -> Vec<(String, String)> {
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
        .map(|(stem, from)| {
            (
                (*from).to_string(),
                format!("{}/{stem}.json", spawn::port_dir_relative(port)),
            )
        })
        .collect()
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
    EngineConfig {
        max_parallel_nodes: config.workflow.max_parallel_nodes.max(1),
        stuck_threshold: u16::try_from(config.workflow.stuck_threshold).unwrap_or(u16::MAX),
        drift_threshold: u16::try_from(config.workflow.drift_threshold).unwrap_or(u16::MAX),
    }
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
        changed |= self.sample_workflow_agent_states(now);
        changed |= self.apply_workflow_engine_input_at(EngineInput::Tick { now }, now);
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
    pub(crate) fn observe_workflow_pane_exit(&mut self, pane_id: crate::layout::PaneId) -> bool {
        self.handle_workflow_app_event(WorkflowAppEvent::NodePaneExited {
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
        for effect in effects {
            changed |= self.dispatch_workflow_effect(effect);
        }
        self.flush_workflow_writes();
        self.mirror_workflow_run_graph();
        changed |= self.announce_workflow_progress();
        changed
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
        let input_ports: Vec<spawn::TaskInputPort> = inputs
            .iter()
            .map(|(port, contributions)| spawn::TaskInputPort {
                port: port.clone(),
                sources: task_input_sources(port, contributions),
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
        }
        .render();

        let run_dir = spawn::run_dir(&spawn::runs_root(), &run.run_id);
        let layout = spawn::NodeDirLayout::for_node(&run_dir, path);
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

    fn emit_workflow_event(&mut self, event: WorkflowEvent) {
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
        };
        let Some((event, data)) = envelope else {
            return;
        };
        self.emit_event(EventEnvelope { event, data });
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
            nodes_total: u32::try_from(graph.nodes.len()).unwrap_or(u32::MAX),
            nodes_done: u32::try_from(
                graph
                    .nodes
                    .iter()
                    .filter(|node| node.status.is_terminal())
                    .count(),
            )
            .unwrap_or(u32::MAX),
            failure: None,
            max_depth: u32::from(graph.growth.max_depth),
            max_nodes: u32::from(graph.growth.max_nodes),
            nodes_live: u32::from(GrowthLimits::live_node_count(&graph.nodes)),
            // §4 D11: the run's most recent guardrail breach, whichever node
            // hit it. A durable read of a finished run cannot report this —
            // `workflow_run` has no column for it — which is exactly why the
            // live projection wins for the active run.
            growth_limited: self.workflow.last_growth_limit().cloned(),
        })
    }

    /// Wire projection of one run node.
    pub(crate) fn workflow_node_info(&self, path: &InstancePath) -> Option<WorkflowRunNodeInfo> {
        let graph = self.workflow.graph()?;
        let node = graph.node_by_path(path)?;
        let demand = self
            .workflow
            .definition()
            .and_then(|definition| definition.node(&node.key))
            .map_or(Demand::Standard, |definition| definition.demand);
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
        let mut state = WorkflowRuntimeState::new(EngineConfig::default());
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
    }

    #[test]
    fn starting_a_run_admits_the_roots_and_arms_the_tick() {
        let now = Instant::now();
        let mut state = WorkflowRuntimeState::new(EngineConfig::default());
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

        assert_eq!(state.run_status(), Some(RunStatus::Succeeded));
        assert!(!state.is_live());
        assert_eq!(state.next_tick_deadline(), None);
        assert!(state.pending_spawns().is_empty());
        assert!(state
            .active_run()
            .and_then(|run| run.ended_at_unix_ms)
            .is_some());

        let definition = definition();
        let graph = graph_of(&definition);
        assert!(state
            .start(active_run(), definition, graph, Instant::now())
            .is_ok());
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

        let mut command = WorkflowRuntimeState::new(EngineConfig::default());
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
        let mut state = WorkflowRuntimeState::new(EngineConfig::default());
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
        let surfaced = surfaced_toasts(&app);
        let end = surfaced.last().expect("the run's end is shown");
        assert_eq!(end.title, "Workflow run");
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
        assert!(plan.task_markdown.contains("`plan`: `./inputs/plan.json`"));
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
            plan.task_markdown
                .contains("`fanout/worker/2`: `./inputs/shard/fanout-worker-2.json`"),
            "and to a file it can open on its own: {}",
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
        let mut state = WorkflowRuntimeState::new(EngineConfig::default());
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
        let mut state = WorkflowRuntimeState::new(EngineConfig::default());
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
