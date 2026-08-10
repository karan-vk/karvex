//! Engine facade: `apply(EngineInput) -> Vec<RunEffect>`.
//!
//! The whole engine contract is [`Engine::apply`]: no async, no I/O,
//! deterministic given a supplied clock. It runs inside the server's existing
//! single-threaded event loop, so there is no lock, no shared-mutable graph,
//! and no second scheduler
//! (`docs/design/workflow-builder/04-kvdag-and-execution.md` §2 and §9).

pub mod complete;
pub mod expand;
pub mod graph;
pub mod schedule;
pub mod watchdog;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Instant;

use serde_json::json;
use tracing::{debug, info};

use crate::detect::AgentState;
use crate::workflow::engine::complete::{Completion, SchemaViolation, Signal, SignalLedger};
use crate::workflow::engine::expand::ExpandProposal;
use crate::workflow::engine::graph::FIRST_ATTEMPT;
use crate::workflow::engine::schedule::TerminalBlocker;
use crate::workflow::model::{
    is_reserved_path, CheckpointKind, Demand, EngineInput, EpiloguePhase, EpilogueState,
    InstancePath, Kvdag, NodeBinding, NodeKey, NodeResult, NodeStatus, NodeToken, NodeUsage,
    NoticeLevel, OutputSchema, ProgressDelta, ProgressTracker, PublicPaneId, RawJson, RunEffect,
    RunEventKind, RunGraph, RunNode, RunNodeIdx, RunStatus, Runner, StoreWrite, Succession,
    SummaryNodeLine, UserNotice, WorkflowEvent, SUMMARY_INSTANCE_PATH,
};
use crate::workflow::tier;

/// Runtime knobs, sourced from the `[workflow]` config block.
///
/// `Clone` rather than `Copy` since [`Self::summary_command`] carries an argv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    pub max_parallel_nodes: usize,
    pub stuck_threshold: u16,
    pub drift_threshold: u16,
    /// Whether a finished run gets an end-of-run summary
    /// (`workflow.summary_enabled`, `07-phase3-plan.md` §4 D22).
    ///
    /// Off means no epilogue node is ever appended — not a summariser that runs
    /// and discards its output — so a run with summaries disabled is byte-for-
    /// byte the run Phase 2 produced.
    pub summary_enabled: bool,
    /// The declared argv override that binds the summariser to a command
    /// instead of `claude` (`KARVEX_WORKFLOW_SUMMARY_COMMAND`, §4 D2 / §6 A4).
    ///
    /// `None` is the production path: the epilogue runs as an agent. `Some`
    /// swaps it to [`Runner::Command`] — and the **engine** has to know, not
    /// just the spawn plan, because the runner decides which completion signals
    /// are admissible (defect D-1). WS-D populates this from the environment
    /// and rejects invalid argv loudly rather than falling back silently.
    pub summary_command: Option<Vec<String>>,
}

impl EngineConfig {
    /// How the epilogue is bound under this configuration. The single place the
    /// override becomes a [`Runner`], so the spawn plan and the engine's signal
    /// gating can never disagree about what the summariser is.
    fn epilogue_runner(&self) -> Runner {
        match self.summary_command {
            Some(_) => Runner::Command,
            None => Runner::Agent,
        }
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_parallel_nodes: 4,
            stuck_threshold: 3,
            drift_threshold: 5,
            summary_enabled: true,
            summary_command: None,
        }
    }
}

/// The end-of-run summary's `text` budget, in characters
/// (`03-storage-schema.md` §7). Stated in the prompt and enforced by
/// [`summary_output_schema`]'s `maxLength`, so an over-budget summary fails
/// validation and spends the one corrective re-prompt rather than being
/// silently truncated into the store.
pub const SUMMARY_TEXT_BUDGET: usize = 4_000;

/// The summary's one-line `outcome` budget, in characters.
pub const SUMMARY_OUTCOME_BUDGET: usize = 200;

/// The epilogue's demand, fixed by `07-phase3-plan.md` §4 D2 so a `--tier low`
/// run summarises cheaply by construction.
///
/// A constant rather than a field: unlike the epilogue's runner — which depends
/// on configuration and therefore has to be *recorded* per run
/// ([`EpilogueState::runner`]) — this never varies. It is `pub` because the
/// epilogue has **no kvdag node**, so every reader that would normally derive a
/// node's demand from the definition has to read it here instead. A reader that
/// falls back to `Demand::Standard` for a definition-less node is not wrong by a
/// little: it silently disagrees with the row the store holds, which is the
/// live-vs-durable field-loss class §4 D16 exists to catch.
pub const EPILOGUE_DEMAND: Demand = Demand::Light;

/// Why the epilogue's `model`/`effort` read what they read — the same
/// explanation every other node carries in `assignment_reason`.
///
/// A constant for the same reason [`EPILOGUE_DEMAND`] is one: `begin_epilogue`
/// writes it twice, once onto the live [`RunNode`] and once onto the
/// [`StoreWrite::RunNodeCreated`] row, and the two are compared field for field
/// across a restart (§4 D16). One value with two spellings is a live-vs-durable
/// disagreement waiting for someone to edit one of them.
const EPILOGUE_ASSIGNMENT_REASON: &str =
    "the end-of-run summariser runs at the run's tier on light demand";

/// Everything the binder needs to put the epilogue's summariser in a pane.
///
/// The epilogue has no kvdag node behind it, so its task text and output schema
/// come from here rather than from a definition lookup
/// (`07-phase3-plan.md` §3 rule 2). Pure and deterministic given a run graph, so
/// it is testable without a pane, a store, or a clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpilogueTaskSpec {
    /// Always [`SUMMARY_INSTANCE_PATH`].
    pub path: InstancePath,
    /// What the DAG view and the pane title call it.
    pub label: String,
    /// The **body** of the summariser's `task.md`: what to cover, the budget,
    /// and one evidence line per user node.
    ///
    /// Deliberately not the whole document. The engine is pure and knows
    /// nothing about node directories, so the binder renders this through the
    /// same [`crate::workflow::binding::spawn::TaskDocument`] every authored
    /// node goes through — which is what gives the epilogue the `## Reporting`
    /// contract. It used to be the finished document, which meant the
    /// summariser was the one node never told to write `result.json` or run
    /// `kvx workflow node complete`: it could only ever finish under a stub
    /// that already knew the protocol, and never under the default `claude`
    /// runner.
    pub task_body: String,
    /// [`summary_output_schema`], carried so the caller writes one file from one
    /// source.
    pub output_schema: serde_json::Value,
    /// The argv when [`EngineConfig::summary_command`] binds the summariser to a
    /// command, `None` when it runs as an agent.
    ///
    /// Carried here so the spawn plan reads the override from **one** authority
    /// rather than re-reading the environment itself. Two readers of the same
    /// environment variable is how the engine and the binder end up disagreeing
    /// about what the summariser is — which is precisely the defect this field
    /// closes (D-1).
    pub command: Option<Vec<String>>,
}

/// What the completion gate did with one reported result (§4.3).
///
/// The gate's decision used to be visible only as a node status change, so a
/// caller that delivered a schema-invalid result got a success envelope for a
/// result the engine had just rejected. The verdict is recorded here so the
/// report's own response can say what happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportVerdict {
    /// The result validated; the node succeeded.
    Accepted,
    /// Schema-invalid, first strike: the node keeps its pane and was issued the
    /// single documented corrective re-prompt.
    Corrected,
    /// Schema-invalid after the corrective re-prompt, or unvalidatable at all:
    /// the node was moved to `NeedsAttention`.
    Surfaced,
}

/// One report's verdict, kept until the node's next report replaces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportOutcome {
    pub path: InstancePath,
    pub verdict: ReportVerdict,
    /// One rendered output-schema violation per entry. Empty when the result
    /// validated, and empty when there was no schema to validate it against —
    /// so "the gate rejected this result on its schema" is exactly
    /// `!errors.is_empty()`.
    pub errors: Vec<String>,
    /// The line the engine journalled and surfaced for this report.
    pub reason: String,
}

impl ReportOutcome {
    pub fn accepted(&self) -> bool {
        matches!(self.verdict, ReportVerdict::Accepted)
    }
}

/// The two `NodeHistory` inputs the engine can state truthfully today
/// (`06-phase2-plan.md` §4 D8).
///
/// `tier::NodeHistory` has five fields and the `auto` policy reads four of
/// them, but only these two are facts the engine already holds: the other three
/// are Phase 4's watchdog counter, a token total `model.rs` documents as
/// permanently `0`, and an ordering-sensitive derivation the store computes over
/// several runs. Shipping "auto over `NodeHistory`" against an all-zero record
/// would be a silently inert feature, so these two are recorded per node and
/// the rest are left visibly absent rather than fabricated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NodeFacts {
    /// Attempt 1 reached `Succeeded` on the node's first reported result — no
    /// corrective re-prompt was spent. Assigned, not accumulated: a node that
    /// succeeds on a later attempt or a later report sets this back to false.
    pub first_pass_succeeded: bool,
    /// How many of this node's reported results the completion gate rejected on
    /// schema-class grounds, across every attempt. Cumulative for the life of
    /// the run: a retry does not refund a failure the node already made, which
    /// is the whole point of the measurement.
    pub schema_failures: u32,
}

/// A `PromptNode`/`SendKeys` delivery the runtime refused (§5).
///
/// The engine does not perform deliveries, so it cannot observe the refusal
/// itself; the caller hands it back through [`Engine::note_delivery_failure`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryFailureNote {
    /// The API method that was attempted, e.g. `pane.send_text`.
    pub method: String,
    /// The runtime's own reason, already user-facing.
    pub reason: String,
}

/// One run's state machine. The in-memory graph is authoritative during a run;
/// the journal is the durable record.
#[derive(Debug, Clone)]
pub struct Engine {
    config: EngineConfig,
    graph: Option<RunGraph>,
    /// The run's definition. `RunGraph` carries no output schemas, so without
    /// it the completion gate has nothing to validate against and no node can
    /// ever succeed.
    definition: Option<Kvdag>,
    signals: HashMap<InstancePath, SignalLedger>,
    /// How many results each node has had rejected by the completion gate, which
    /// is what limits it to exactly one corrective re-prompt.
    reports: HashMap<InstancePath, u8>,
    /// Whether each node's seed prompt is known to have registered, and whether
    /// it has already been re-delivered. See [`Engine::redeliver_seed`].
    seeds: HashMap<InstancePath, SeedState>,
    /// The verdict of the most recent reported result, per node. Read by the
    /// caller that delivered the report so its response can reflect a rejection
    /// instead of answering `ok` for a result the gate refused.
    reported: HashMap<InstancePath, ReportOutcome>,
    /// The last pane delivery the runtime could not make, per node. Surfaced
    /// state rather than a log line: a steer that never landed must not look
    /// identical to one that did.
    delivery_failures: HashMap<InstancePath, DeliveryFailureNote>,
    /// Per-node [`NodeFacts`], the measured half of what `tier::NodeHistory`
    /// aggregates across runs. Absent entries read as the default, so a node
    /// that never reported is "no first pass, no schema failures" rather than
    /// missing.
    facts: HashMap<InstancePath, NodeFacts>,
}

/// What the engine knows about a node's seed prompt (§4.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SeedState {
    /// The agent has been observed working, so it read what it was seeded with.
    acted: bool,
    /// The seed has already been re-delivered once; it is not offered again.
    redelivered: bool,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self {
            config,
            graph: None,
            definition: None,
            signals: HashMap::new(),
            reports: HashMap::new(),
            seeds: HashMap::new(),
            reported: HashMap::new(),
            delivery_failures: HashMap::new(),
            facts: HashMap::new(),
        }
    }

    pub fn config(&self) -> EngineConfig {
        self.config.clone()
    }

    /// What this run has measured about `path` so far (§4 D8). An unknown node
    /// reads as the default rather than `None`: "nothing measured" and "no such
    /// node" are the same all-zero record to every consumer, and forcing a
    /// caller to unwrap the difference would invite a fabricated value.
    pub fn node_facts(&self, path: &InstancePath) -> NodeFacts {
        self.facts.get(path).copied().unwrap_or_default()
    }

    /// Records one schema-class rejection against `path`.
    ///
    /// Schema-class covers both an output-schema failure and a malformed
    /// `expand` (§4 D6): in each case the node said something the contract does
    /// not allow. A signal that arrived with no artifact at all is *not* one of
    /// these — there was nothing to validate — so `missing_result` never lands
    /// here.
    fn note_schema_failure(&mut self, path: &InstancePath) {
        let facts = self.facts.entry(path.clone()).or_default();
        facts.schema_failures = facts.schema_failures.saturating_add(1);
    }

    /// The verdict the completion gate reached on `path`'s most recent reported
    /// result, if it has reported one.
    pub fn report_outcome(&self, path: &InstancePath) -> Option<&ReportOutcome> {
        self.reported.get(path)
    }

    /// The last pane delivery this node's runtime refused, if any is
    /// outstanding. Cleared whenever the node gets a fresh pane, is restarted,
    /// or is steered/interrupted again.
    pub fn delivery_failure(&self, path: &InstancePath) -> Option<&DeliveryFailureNote> {
        self.delivery_failures.get(path)
    }

    /// Records a `PromptNode`/`SendKeys` effect the runtime could not deliver.
    ///
    /// The engine emits deliveries but never performs them, so a refusal is only
    /// ever known to the caller. Handing it back here is what turns a
    /// server-side log line into a journalled run event, a node-level marker
    /// readers can see, and a user notice — a steer the process never received
    /// must not be indistinguishable from one it did.
    pub fn note_delivery_failure(
        &mut self,
        path: &InstancePath,
        method: &str,
        reason: &str,
    ) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        let Some(idx) = self.graph.as_ref().and_then(|graph| graph.index_of(path)) else {
            debug!(path = %path, method, "workflow delivery failure for an unknown node");
            return effects;
        };
        self.delivery_failures.insert(
            path.clone(),
            DeliveryFailureNote {
                method: method.to_string(),
                reason: reason.to_string(),
            },
        );
        info!(
            path = %path,
            method,
            reason,
            "workflow node delivery failed; surfacing it on the node"
        );
        let Some(graph) = self.graph.as_mut() else {
            return effects;
        };
        let payload = json!({ "delivery_failed": true, "method": method, "reason": reason });
        effects.push(journal(
            graph,
            RunEventKind::Error,
            Some(path.clone()),
            payload,
        ));
        if let Some(status) = graph.node(idx).map(|node| node.status) {
            effects.push(RunEffect::Emit(WorkflowEvent::NodeUpdated {
                run: graph.run_id.clone(),
                path: path.clone(),
                status,
            }));
        }
        effects.push(RunEffect::Notify(UserNotice {
            level: NoticeLevel::Error,
            run: Some(graph.run_id.clone()),
            path: Some(path.clone()),
            message: format!("{method} to node {path} was not delivered: {reason}"),
        }));
        effects
    }

    /// Records one report's verdict, replacing whatever the node reported
    /// before.
    fn record_report(
        &mut self,
        path: &InstancePath,
        verdict: ReportVerdict,
        errors: Vec<String>,
        reason: &str,
    ) {
        self.reported.insert(
            path.clone(),
            ReportOutcome {
                path: path.clone(),
                verdict,
                errors,
                reason: reason.to_string(),
            },
        );
    }

    pub fn graph(&self) -> Option<&RunGraph> {
        self.graph.as_ref()
    }

    pub fn run_status(&self) -> Option<RunStatus> {
        self.graph.as_ref().map(|graph| graph.status)
    }

    pub fn definition(&self) -> Option<&Kvdag> {
        self.definition.as_ref()
    }

    /// Installs the definition the run graph was materialised from. Must happen
    /// before `EngineInput::Start`, which carries only the run graph.
    pub fn install_definition(&mut self, definition: Kvdag) {
        self.definition = Some(definition);
    }

    /// Nodes admitted to run now (§3.1). The caller turns these into spawns:
    /// building a `SpawnSpec` needs the run directory, the node token, and the
    /// agent session id, none of which the engine mints.
    ///
    /// **Deliberately not gated on the run's status.** The epilogue node is
    /// appended after `finish` has already set a terminal status, and it has to
    /// be admitted anyway — `settle`'s early return for a non-`Running`/`Paused`
    /// run is exactly why admission cannot ask the run whether it is live
    /// (`07-phase3-plan.md` §3 rule 2). Admission is a per-node question:
    /// `ready_set` reports the nodes whose status is `Ready`, and after a run
    /// finishes the only node that can be is the summariser.
    pub fn admissions(&self) -> Vec<RunNodeIdx> {
        self.graph
            .as_ref()
            .map(|graph| schedule::ready_set(graph, self.config.max_parallel_nodes))
            .unwrap_or_default()
    }

    /// Whether the engine still needs ticks to drive the end-of-run summariser.
    ///
    /// The app's only new liveness input (§7 R-1): a run whose epilogue is
    /// `Done`, `GaveUp`, or absent lets the workflow tick deadline lapse exactly
    /// as it did before Phase 3.
    pub fn epilogue_pending(&self) -> bool {
        self.graph
            .as_ref()
            .and_then(|graph| graph.epilogue)
            .is_some_and(|state| state.phase.is_pending())
    }

    /// The summariser's task and schema, or `None` when this run has no
    /// epilogue. The caller renders it into the epilogue node's directory the
    /// same way it renders an authored node's `task.md`.
    pub fn summary_task_spec(&self) -> Option<EpilogueTaskSpec> {
        let graph = self.graph.as_ref()?;
        graph.epilogue?;
        let definition = self.definition.as_ref()?;
        Some(summary_task_spec(
            graph,
            definition,
            self.config.summary_command.as_deref(),
        ))
    }

    /// The epilogue node's index while it still needs driving, or `None`.
    fn pending_epilogue_idx(&self) -> Option<RunNodeIdx> {
        self.graph
            .as_ref()
            .and_then(|graph| graph.epilogue)
            .filter(|state| state.phase.is_pending())
            .map(|state| state.node)
    }

    /// Whether `idx` is this run's epilogue node, whatever phase it is in.
    fn is_epilogue(&self, idx: RunNodeIdx) -> bool {
        self.graph
            .as_ref()
            .and_then(|graph| graph.epilogue)
            .is_some_and(|state| state.node == idx)
    }

    /// Appends the engine-owned summariser after the user graph's terminal
    /// status is decided (§4 D1).
    ///
    /// Runs *after* [`finish`], never inside it: a summariser inside
    /// `run_terminal_ready`'s conjunction wedges every failed run, because a
    /// `Failed` leaf never resolves its outbound edge and the summariser would
    /// sit `Pending` forever while the run paused instead of failing (§0.7).
    /// Appending it here means the run's status is already final and this
    /// function cannot change it.
    ///
    /// The node is created `Ready` with no inbound edges, so the very next
    /// [`Engine::admissions`] yields it. It is persisted through
    /// [`StoreWrite::RunNodeCreated`] with the reserved `.summary` path, which
    /// the store recognises as the one create allowed to carry no kvdag node
    /// behind it.
    fn begin_epilogue(&mut self, effects: &mut Vec<RunEffect>) {
        if !self.config.summary_enabled {
            return;
        }
        let Some(graph) = self.graph.as_mut() else {
            return;
        };
        // `Cancelled` never summarises (§4 D2), and an epilogue is appended at
        // most once per run.
        if graph.epilogue.is_some()
            || !matches!(graph.status, RunStatus::Succeeded | RunStatus::Failed)
        {
            return;
        }

        let idx = RunNodeIdx(graph.nodes.len());
        let path = InstancePath::new(SUMMARY_INSTANCE_PATH);
        let key = NodeKey::new(SUMMARY_INSTANCE_PATH);
        // Decided once, here, and recorded on the epilogue state: the engine
        // cannot re-derive it later because there is no kvdag node to ask, and
        // the `Agent` default it would fall back to lies whenever the summariser
        // is bound to a command (defect D-1).
        let runner = self.config.epilogue_runner();
        // Light demand through the run's own tier, so a `--tier low` run
        // summarises cheaply by construction (§4 D2).
        let assignment = tier::resolve(graph.tier, EPILOGUE_DEMAND, None);
        graph.nodes.push(RunNode {
            idx,
            key: key.clone(),
            path: path.clone(),
            label: EPILOGUE_LABEL.to_string(),
            inputs: BTreeMap::new(),
            parent: None,
            depth: 0,
            status: NodeStatus::Ready,
            assignment,
            assignment_reason: EPILOGUE_ASSIGNMENT_REASON.to_string(),
            attempt: FIRST_ATTEMPT,
            binding: None,
            result: None,
            usage: NodeUsage::default(),
            started_at_unix_ms: None,
            ended_at_unix_ms: None,
            progress: ProgressTracker::default(),
            succession: None,
            checkpoint_seq: 0,
            restored_from: None,
        });
        graph.epilogue = Some(EpilogueState {
            node: idx,
            phase: EpiloguePhase::Pending,
            runner,
        });

        effects.push(RunEffect::Persist(Box::new(StoreWrite::RunNodeCreated {
            run: graph.run_id.clone(),
            key,
            path: path.clone(),
            label: EPILOGUE_LABEL.to_string(),
            inputs: BTreeMap::new(),
            parent: None,
            depth: 0,
            status: NodeStatus::Ready,
            demand: EPILOGUE_DEMAND,
            assignment,
            assignment_reason: EPILOGUE_ASSIGNMENT_REASON.to_string(),
            attempt: FIRST_ATTEMPT,
            // `proposal_id: ""` and `parent: None` above are **unread on the
            // reserved-path branch** — the store's epilogue create ignores both,
            // because the epilogue is engine-owned and has neither a proposing
            // parent nor an `expand_proposed` entry to link back to. They are
            // the variant's required fields carrying "nothing", not values with
            // meaning; do not later assign either one a purpose here.
            proposal_id: String::new(),
        })));
        let payload = json!({ "epilogue": true });
        effects.push(journal(
            graph,
            RunEventKind::NodeCreated,
            Some(path.clone()),
            payload,
        ));
        // The DAG view learns about the summariser the same way it learns about
        // an expansion child, so it appears live rather than at the next reload.
        effects.push(RunEffect::Emit(WorkflowEvent::NodeCreated {
            run: graph.run_id.clone(),
            path,
        }));
        info!(run = %graph.run_id, "workflow run finished; the end-of-run summariser was appended");
    }

    /// The epilogue's happy path: a summary that validated against
    /// [`summary_output_schema`].
    ///
    /// Takes the place of [`Engine::succeed`] for the summariser, because none
    /// of what `succeed` does applies: there is no succession to resolve from
    /// outbound edges the epilogue does not have, no downstream to propagate to,
    /// and the durable artifact is a `run_summary` row rather than a node
    /// checkpoint.
    fn accept_summary(&mut self, idx: RunNodeIdx, result: NodeResult) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        let Some(graph) = self.graph.as_mut() else {
            return effects;
        };
        let (text, outcome, highlights, open_gaps) = summary_fields(&result.payload);
        let per_node = summary_per_node(&result.payload);
        let estimate = token_estimate(&result.payload);
        let text_len = text.chars().count();

        let Some(node) = graph.node_mut(idx) else {
            return effects;
        };
        node.status = NodeStatus::Succeeded;
        node.succession = Some(Succession::NoFollowup {
            evidence: "the run summary was written".to_string(),
        });
        node.result = Some(result);
        let Some(node) = graph.node(idx) else {
            return effects;
        };
        let path = node.path.clone();
        let pane = pane_of(graph, idx);

        effects.push(RunEffect::Persist(Box::new(StoreWrite::RunSummary {
            run: graph.run_id.clone(),
            kvdag_version: graph.version_id.clone(),
            text,
            outcome: outcome.clone(),
            highlights,
            open_gaps,
            per_node,
            token_estimate: estimate,
            generated_by_path: Some(path.clone()),
        })));
        let payload = json!({ "outcome": outcome, "text_len": text_len });
        effects.push(journal(
            graph,
            RunEventKind::Summary,
            Some(path.clone()),
            payload,
        ));
        record_status(graph, idx, &mut effects);
        effects.push(RunEffect::Emit(WorkflowEvent::RunSummarized {
            run: graph.run_id.clone(),
        }));
        // The summariser's pane has nothing left to do and the run is over; a
        // pane left open here would outlive every other pane the run created.
        if let Some(pane) = pane {
            effects.push(RunEffect::ClosePane { pane });
        }
        // The phase advances; the runner recorded at `begin_epilogue` is
        // preserved, because how the summariser was bound is a fact about this
        // epilogue and does not change as it resolves.
        if let Some(state) = graph.epilogue.as_mut() {
            state.phase = EpiloguePhase::Done;
        }
        effects
    }

    /// The bottom of the epilogue's bounded failure ladder (§4 D1).
    ///
    /// Every way the summariser can fail — schema-invalid after its one
    /// corrective re-prompt, a self-report with no artifact, a spawn failure, a
    /// pane that died, a cancel — converges here: journalled once, notified
    /// once, pane closed, **run status untouched**. `summary.get` answering
    /// `None` afterwards is a normal answer, not an error.
    ///
    /// Idempotent: a resolved epilogue gives up no further, so the notice
    /// cannot be delivered twice by two failure signals racing each other.
    fn give_up_epilogue(&mut self, idx: RunNodeIdx, reason: &str) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        if self.pending_epilogue_idx() != Some(idx) {
            return effects;
        }
        let Some(graph) = self.graph.as_mut() else {
            return effects;
        };
        let pane = pane_of(graph, idx);
        if let Some(node) = graph.node_mut(idx) {
            node.status = NodeStatus::Failed;
            // A recorded succession, so the epilogue node is not a
            // `SuccessionGap` waiting to be found by anything that walks the
            // graph later — even though `run_terminal_ready` already skips it.
            node.succession = Some(Succession::NoFollowup {
                evidence: reason.to_string(),
            });
        }
        let path = graph
            .node(idx)
            .map(|node| node.path.clone())
            .unwrap_or_else(|| InstancePath::new(SUMMARY_INSTANCE_PATH));
        let payload = json!({ "reason": "summary_failed", "detail": reason });
        effects.push(journal(
            graph,
            RunEventKind::Error,
            Some(path.clone()),
            payload,
        ));
        record_status(graph, idx, &mut effects);
        effects.push(RunEffect::Notify(UserNotice {
            level: NoticeLevel::Warning,
            run: Some(graph.run_id.clone()),
            path: Some(path),
            message: format!("the run summary could not be written: {reason}"),
        }));
        if let Some(pane) = pane {
            effects.push(RunEffect::ClosePane { pane });
        }
        // The phase advances; the runner recorded at `begin_epilogue` is
        // preserved, because how the summariser was bound is a fact about this
        // epilogue and does not change as it resolves.
        if let Some(state) = graph.epilogue.as_mut() {
            state.phase = EpiloguePhase::GaveUp;
        }
        info!(reason = %reason, "the end-of-run summariser gave up; the run's status is unchanged");
        effects
    }

    /// Records the pane binding of a node the caller has spawned and moves it to
    /// `Running`. This is the only way a node acquires a pane: `EngineInput`
    /// carries no spawn confirmation.
    pub fn bind_node(&mut self, path: &InstancePath, binding: NodeBinding) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        let Some(graph) = self.graph.as_mut() else {
            return effects;
        };
        let Some(idx) = graph.index_of(path) else {
            return effects;
        };
        if let Some(node) = graph.node_mut(idx) {
            node.binding = Some(binding);
            node.status = NodeStatus::Running;
        }
        // The epilogue's own phase follows its node into the pane, so
        // `epilogue_pending` distinguishes "waiting to be spawned" from
        // "working" without a second source of truth.
        if graph.epilogue.is_some_and(|state| state.node == idx) {
            if let Some(state) = graph.epilogue.as_mut() {
                state.phase = EpiloguePhase::Running;
            }
        }
        // A fresh pane is a fresh delivery channel, so whatever the previous one
        // refused is no longer outstanding.
        self.delivery_failures.remove(path);
        let payload = json!({});
        effects.push(journal(
            graph,
            RunEventKind::NodeStarted,
            Some(path.clone()),
            payload,
        ));
        record_status(graph, idx, &mut effects);
        effects
    }

    /// Records the transcript path a node's pane actually reported, replacing
    /// the pre-launch estimate (`07-phase3-plan.md` §4 D6, §0.5). Returns
    /// whether it changed anything. Touches `binding.transcript_path` and
    /// nothing else — unlike `bind_node`, which replaces the whole binding and
    /// moves the node to `Running`.
    ///
    /// The `bind_node` hazard in full: it also journals `NodeStarted` and clears
    /// the node's delivery-failure marker, because it exists for the moment a
    /// node *acquires* a pane. Learning a transcript path is none of those
    /// things — it happens to a node whose binding is already correct in every
    /// other respect — so routing it through `bind_node` would re-announce a
    /// start that already happened, resurrect a node that has since closed, and
    /// silently drop a refused delivery the user has not seen yet.
    ///
    /// Works on a node in **any** status, including a closed one: a session
    /// report can arrive after the node finished, and the stored path is what a
    /// *later* interrogation stats (§4 D6's stat-first rule). Refusing a late
    /// correction would preserve the stale estimate in exactly the historical-
    /// interrogation case §0.5 exists to fix.
    ///
    /// Returning `false` on an unchanged path lets the caller skip a durable
    /// write — the common case, since the pre-launch estimate is usually right.
    pub fn record_transcript_path(&mut self, path: &InstancePath, transcript: PathBuf) -> bool {
        let Some(graph) = self.graph.as_mut() else {
            return false;
        };
        let Some(idx) = graph.index_of(path) else {
            return false;
        };
        let Some(binding) = graph.node_mut(idx).and_then(|node| node.binding.as_mut()) else {
            return false;
        };
        if binding.transcript_path == transcript {
            return false;
        }
        binding.transcript_path = transcript;
        true
    }

    /// The durable counterpart of a node's current in-memory state, with **no**
    /// status journal and **no** `NodeUpdated` emit.
    ///
    /// [`record_transcript_path`](Self::record_transcript_path) changes one
    /// field of an already-announced node, so the caller needs a way to persist
    /// that field without re-announcing a status transition that did not
    /// happen. `record_status` is the wrong tool for the same reason
    /// `bind_node` is: it exists for transitions.
    ///
    /// Returns `None` for an unknown path. The write is the ordinary
    /// find-then-`UPDATE` [`StoreWrite::RunNode`], so the row must already
    /// exist — which it does for any node that has ever been bound.
    pub fn node_persist_effect(&self, path: &InstancePath) -> Option<RunEffect> {
        let graph = self.graph.as_ref()?;
        let node = graph.node_by_path(path)?;
        Some(RunEffect::Persist(Box::new(StoreWrite::RunNode {
            run: graph.run_id.clone(),
            path: node.path.clone(),
            status: node.status,
            attempt: node.attempt,
            binding: node.binding.clone(),
            usage: node.usage,
            evidence: node.result.as_ref().map(|result| result.evidence),
            succession: node.succession.clone(),
            started_at_unix_ms: node.started_at_unix_ms,
            ended_at_unix_ms: node.ended_at_unix_ms,
            restored_from: node.restored_from.clone(),
        })))
    }

    pub fn apply(&mut self, input: EngineInput) -> Vec<RunEffect> {
        match input {
            EngineInput::Start { graph } => self.start(*graph),
            EngineInput::NodeSelfReport {
                path,
                token,
                result,
            } => self.report(&path, &token, &result),
            EngineInput::ExpandProposed {
                path,
                token,
                proposals,
            } => self.expand_proposed(&path, &token, &proposals),
            EngineInput::TurnEnded { pane } => self.signal_from_pane(&pane, Signal::TurnEnd),
            EngineInput::AgentStatus { pane, state, at } => self.agent_status(&pane, state, at),
            EngineInput::ProgressObserved { path, delta } => self.progress(&path, &delta),
            EngineInput::PaneExited { pane, code } => self.pane_exited(&pane, code),
            EngineInput::SpawnFailed { path, reason } => self.spawn_failed(&path, &reason),
            EngineInput::Steer { path, text } => self.steer(&path, &text),
            EngineInput::Interrupt { path } => self.interrupt(&path),
            EngineInput::RestartNode { path } => self.restart(&path),
            EngineInput::CancelRun => self.cancel(),
            EngineInput::Tick { now } => self.tick(now),
        }
    }

    fn start(&mut self, mut graph: RunGraph) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        self.signals.clear();
        self.reports.clear();
        self.seeds.clear();
        self.reported.clear();
        self.delivery_failures.clear();
        self.facts.clear();

        graph.status = RunStatus::Running;
        let changed = schedule::propagate(&mut graph);

        effects.push(RunEffect::Persist(Box::new(StoreWrite::RunStatus {
            run: graph.run_id.clone(),
            status: RunStatus::Running,
            ended_at_unix_ms: None,
        })));
        let payload = json!({ "tier": graph.tier, "nodes": graph.nodes.len() });
        effects.push(journal(&mut graph, RunEventKind::RunStarted, None, payload));
        effects.push(RunEffect::Emit(WorkflowEvent::RunStarted {
            run: graph.run_id.clone(),
        }));

        let paths: Vec<InstancePath> = graph.nodes.iter().map(|node| node.path.clone()).collect();
        for path in paths {
            let payload = json!({});
            effects.push(journal(
                &mut graph,
                RunEventKind::NodeCreated,
                Some(path.clone()),
                payload,
            ));
            effects.push(RunEffect::Emit(WorkflowEvent::NodeCreated {
                run: graph.run_id.clone(),
                path,
            }));
        }
        // Every edge that is **already settled** at Start, not just the ones
        // this `propagate` moved.
        //
        // A restored node arrives from `materialise_with_restored` with its
        // result in place, and materialisation propagates before the engine ever
        // sees the graph — so by now its outbound edges are already fired and
        // `changed.edges` is empty. Recording only the delta would leave the
        // durable projection describing those edges as unfired forever, which is
        // exactly the failure `Propagation`'s doc warns about: a caller that
        // persists node statuses without their edges reads every restored edge
        // back unfired, and a restarted server would re-run the branch the first
        // run already satisfied.
        //
        // A no-op for a run with no restored nodes: no node can be terminal at
        // materialisation, so no edge can be settled before this point.
        let mut settled: Vec<usize> = changed.edges.clone();
        settled.extend(
            graph
                .edges
                .iter()
                .enumerate()
                .filter(|(_, edge)| edge.condition_result.is_some())
                .map(|(index, _)| index),
        );
        settled.sort_unstable();
        settled.dedup();
        record_edges(&graph, &settled, &mut effects);
        for idx in changed.nodes {
            record_status(&mut graph, idx, &mut effects);
        }

        self.graph = Some(graph);
        self.settle(&mut effects);
        effects
    }

    /// The self-report of §4.3 precedence 1. The per-node capability token is
    /// minted by the binder and checked by the API layer before the input
    /// reaches the engine, which never sees the mint and so cannot re-check it.
    fn report(
        &mut self,
        path: &InstancePath,
        _token: &NodeToken,
        result: &RawJson,
    ) -> Vec<RunEffect> {
        let Some(idx) = self.graph.as_ref().and_then(|graph| graph.index_of(path)) else {
            debug!(path = %path, "workflow node report for an unknown node");
            return Vec::new();
        };
        let Some(node) = self.graph.as_ref().and_then(|graph| graph.node(idx)) else {
            return Vec::new();
        };
        // A late duplicate report must not re-checkpoint a closed node or
        // resurrect a cancelled one.
        if node.status.is_terminal() {
            debug!(path = %path, "workflow node report for a node that already closed");
            return Vec::new();
        }
        let key = node.key.clone();
        let attempt = node.attempt;

        // §4.3: a self-report that carries no result artifact never completes a
        // node — it lands on `missing_result`, exactly like a sustained-idle
        // observation with nothing to validate. This is the only completion
        // signal a `Runner::Command` node has, so without it a node that cannot
        // produce `result.json` stalls `Running` with nothing to escalate.
        if result.0.is_null() {
            return match complete::missing_result(Signal::SelfReport) {
                Completion::NeedsAttention {
                    reason,
                    resume_when,
                } => {
                    info!(path = %path, reason = %reason, "workflow node report carried no result artifact");
                    // No schema violations: there was no artifact to validate,
                    // so the report itself is not what is wrong — the node is,
                    // and its status already says so.
                    self.record_report(path, ReportVerdict::Surfaced, Vec::new(), &reason);
                    self.needs_attention(idx, &reason, &resume_when)
                }
                Completion::Accepted(_) | Completion::Reprompt { .. } => Vec::new(),
            };
        }

        // §4 D6: `expand` is lifted out **before** anything treats this JSON as
        // a result. Everything downstream of here — validation,
        // `complete::node_result`, `summarise`, `digest`, the persisted
        // checkpoint — sees `stripped`, so the key can never reach the payload
        // Phase 3's restore compares digests over.
        let (stripped, proposed) = complete::strip_expand(result);
        let (proposals, expand_errors) = match proposed.as_ref().map(complete::parse_expand) {
            None => (Vec::new(), Vec::new()),
            Some(Ok(proposals)) => (proposals, Vec::new()),
            // A malformed `expand` is a schema-class violation of the result: it
            // rides through the same gate, spends the same single corrective
            // re-prompt, and names the field it is about.
            Some(Err(errors)) => (Vec::new(), errors),
        };

        let ledger = self.signals.entry(path.clone()).or_default();
        ledger.observe(Signal::SelfReport);
        let evidence = ledger.best().unwrap_or(Signal::SelfReport).evidence();

        let report_ordinal = {
            let count = self.reports.entry(path.clone()).or_insert(0);
            *count = count.saturating_add(1);
            *count
        };

        let Some(schema) = self.schema_for(&key) else {
            let reason =
                "the run's kvdag definition is not installed, so result.json cannot be validated";
            info!(path = %path, "workflow node report could not be validated: no installed definition");
            self.record_report(path, ReportVerdict::Surfaced, Vec::new(), reason);
            // Unlike the other blockers, nothing the node does next can clear
            // this one: without the definition there is no output_schema to
            // validate against, so the resume condition is a new run, not a
            // retry of this one.
            return self.needs_attention(
                idx,
                reason,
                "this run cannot validate any further results; cancel it with \
                 `kvx workflow run cancel <run_id>` and start a new run from the saved workflow \
                 with `kvx workflow run start <name|id>`",
            );
        };

        let completion = complete::accept_with(
            &schema,
            &stripped,
            evidence,
            report_ordinal,
            expand_errors.clone(),
        );
        if !proposals.is_empty() && !matches!(completion, Completion::Accepted(_)) {
            // §3.4 step 1: a node's **validated** result may propose. A result
            // the gate refused is not one, so its proposals go with it rather
            // than growing the graph from a payload the node is about to be
            // told to fix. Nothing is lost: the node re-proposes in the result
            // it gets right, and instance numbering is monotone either way.
            debug!(
                path = %path,
                proposals = proposals.len(),
                "expand proposals discarded with the result the completion gate refused"
            );
        }

        match completion {
            Completion::Accepted(accepted) => {
                self.reports.remove(path);
                self.record_report(
                    path,
                    ReportVerdict::Accepted,
                    Vec::new(),
                    "the reported result validated against the node's output schema",
                );
                // §4 D8's "first pass": the node's first attempt produced a
                // valid result on its first report, so no correction was spent.
                // `attempt` starts at 1 (`graph::FIRST_ATTEMPT`), so a retried
                // or restarted node can never satisfy this however clean its
                // later result is — which is exactly the measurement `auto`
                // wants.
                let first_pass = attempt <= 1 && report_ordinal <= complete::FIRST_REPORT;
                self.facts
                    .entry(path.clone())
                    .or_default()
                    .first_pass_succeeded = first_pass;

                // The expansion is committed **before** the parent succeeds:
                // `succeed` propagates and settles, and a run that finished on
                // its last node would otherwise close in the same call that was
                // supposed to grow it. Committing first means the parent→child
                // `sequence` edge is already in the graph when the parent's
                // success propagates through it.
                let mut effects = self.expand_proposals(idx, path, &proposals);
                effects.extend(self.succeed(idx, *accepted));
                effects
            }
            Completion::Reprompt { errors } => {
                self.note_schema_failure(path);
                let text = complete::corrective_prompt(&schema, &errors);
                let quoted: Vec<String> = errors.iter().map(SchemaViolation::quote).collect();
                let reason = format!(
                    "result.json does not validate against the node's output_schema: {}",
                    quoted.join("; ")
                );
                // §4.3's single corrective re-prompt applies to every runner
                // kind. A `Runner::Command` node has no interactive turn to
                // re-prompt, so the correction it can actually act on is the one
                // its own `kvx workflow node complete` call gets back — which is
                // why this verdict is recorded rather than only journalled.
                info!(
                    path = %path,
                    violations = quoted.len(),
                    "workflow node result rejected by its output schema; issuing the single corrective re-prompt"
                );
                self.record_report(path, ReportVerdict::Corrected, quoted, &reason);
                let mut effects = Vec::new();
                let Some(graph) = self.graph.as_mut() else {
                    return effects;
                };
                let payload = json!({ "schema_valid": false, "errors": errors.len() });
                effects.push(journal(
                    graph,
                    RunEventKind::NodeOutput,
                    Some(path.clone()),
                    payload,
                ));
                let delivered = match pane_of(graph, idx) {
                    Some(pane) => {
                        effects.push(RunEffect::PromptNode { pane, text });
                        true
                    }
                    None => false,
                };
                if !delivered {
                    // A node with no pane — restarted, or waiting on a respawn
                    // after its pane died — has nowhere to receive the
                    // correction. Spending its single re-prompt on a message
                    // that was never sent would send the next result straight
                    // to `NeedsAttention` with the agent never told what was
                    // wrong, so the strike is given back.
                    self.reports
                        .insert(path.clone(), report_ordinal.saturating_sub(1));
                }
                effects
            }
            Completion::NeedsAttention {
                reason,
                resume_when,
            } => {
                self.note_schema_failure(path);
                let mut quoted: Vec<String> = complete::validate(&schema, &stripped)
                    .err()
                    .unwrap_or_default()
                    .iter()
                    .map(SchemaViolation::quote)
                    .collect();
                quoted.extend(expand_errors.iter().map(SchemaViolation::quote));
                info!(
                    path = %path,
                    violations = quoted.len(),
                    "workflow node result still fails its output schema after the corrective re-prompt"
                );
                self.record_report(path, ReportVerdict::Surfaced, quoted, &reason);
                self.needs_attention(idx, &reason, &resume_when)
            }
        }
    }

    /// The mid-run proposal channel: `kvx workflow node expand`, which reaches
    /// the engine as [`EngineInput::ExpandProposed`].
    ///
    /// The per-node capability token is minted by the binder and checked by the
    /// API layer before the input reaches the engine, exactly as it is for
    /// [`Engine::report`] — the engine never sees the mint and so cannot
    /// re-check it.
    ///
    /// The other route into the same pipeline is the `expand` key lifted out of
    /// a reported result (§4 D6); that one settles inside `report` so the
    /// growth and the parent's success land in one effect vector.
    fn expand_proposed(
        &mut self,
        path: &InstancePath,
        _token: &NodeToken,
        proposals: &[ExpandProposal],
    ) -> Vec<RunEffect> {
        let Some(idx) = self.graph.as_ref().and_then(|graph| graph.index_of(path)) else {
            debug!(path = %path, "workflow expand proposal for an unknown node");
            return Vec::new();
        };
        // A node that has already closed cannot grow the graph: its children
        // would hang off a `sequence` edge from a settled parent, admitted
        // immediately, inside a run that may itself have finished.
        let closed = self
            .graph
            .as_ref()
            .and_then(|graph| graph.node(idx))
            .is_some_and(|node| node.status.is_terminal());
        if closed {
            debug!(path = %path, "workflow expand proposal for a node that already closed");
            return Vec::new();
        }

        let before = self.graph.as_ref().map_or(0, |graph| graph.nodes.len());
        let mut effects = self.expand_proposals(idx, path, proposals);
        let after = self.graph.as_ref().map_or(0, |graph| graph.nodes.len());
        // Only a graph that actually grew needs re-settling. A wholly rejected
        // proposal changed nothing, and settling anyway would let a run pause
        // or finish on the back of an input that did not move it.
        if after > before {
            self.settle(&mut effects);
        }
        effects
    }

    /// The §3.4 pipeline for one node's proposals: propose → guardrail →
    /// commit. **A node cannot create nodes; it proposes, and karvex decides.**
    ///
    /// Every proposal is journalled as `expand_proposed` before it is judged,
    /// whatever the verdict turns out to be — that entry is the proposal's home
    /// now that §4 D6 keeps it out of the result payload, and an audit trail
    /// that recorded only the accepted half would be the wrong half.
    ///
    /// Proposals are evaluated and committed **one at a time, in order**,
    /// because the guardrails are cumulative: `expand_max` counts every child
    /// the proposing node has already been granted and `max_nodes` counts the
    /// whole graph, so judging a batch against the graph as it stood before any
    /// of them landed would over-accept by exactly the size of the batch.
    fn expand_proposals(
        &mut self,
        proposer: RunNodeIdx,
        path: &InstancePath,
        proposals: &[ExpandProposal],
    ) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        for proposal in proposals {
            let Some(graph) = self.graph.as_mut() else {
                return effects;
            };
            let payload = json!({
                "template": proposal.template.as_str(),
                "label": proposal.label,
                "inputs": proposal.inputs,
                "count": proposal.count,
            });
            effects.push(journal(
                graph,
                RunEventKind::ExpandProposed,
                Some(path.clone()),
                payload,
            ));

            // Without the definition there is no template to instantiate and no
            // `expand_allow` to check against, so the proposal cannot be judged
            // at all. Journalled above and left at that: silently accepting it
            // would grow the graph from a node nobody validated.
            let Some(definition) = self.definition.as_ref() else {
                debug!(path = %path, "expand proposal not judged: the run's kvdag definition is not installed");
                continue;
            };
            let Some(graph) = self.graph.as_ref() else {
                return effects;
            };
            let outcome = expand::evaluate(graph, definition, proposer, proposal);
            if outcome.is_empty() {
                continue;
            }

            // Read while the definition is still borrowed, because `commit`
            // needs the graph mutably and `ExpandLimit::value_in` is the one
            // authority for "what number was hit".
            let run = graph.run_id.clone();
            let growth = graph.growth;
            let expand_max = graph
                .node(proposer)
                .and_then(|node| definition.node(&node.key))
                .map_or(0, |spec| spec.expand_max);

            let Some(graph) = self.graph.as_mut() else {
                return effects;
            };
            // `commit` journals `expand_accepted`/`expand_rejected`/
            // `growth_limited` and emits `NodeCreated`, and it enqueues
            // `RunNodeCreated` for a child before anything can update that
            // child — the ordering the app's bounded write queue depends on.
            effects.extend(expand::commit(graph, proposer, &outcome));

            for rejection in &outcome.rejected {
                // §4 D5: `workflow.growth.limited` is the one thing no client
                // can derive, and it is emitted for exactly the rejections a
                // guardrail produced. A validation refusal — unknown template,
                // not allowed, unknown input — is the node being wrong, not the
                // run running out of room.
                let Some(limit) = rejection.limit() else {
                    continue;
                };
                let limit_value = rejection
                    .limit_value()
                    .unwrap_or_else(|| limit.value_in(growth, expand_max));
                // Every rejection but `Truncated` created nothing, and the
                // count it was refused is the proposal's own; `count` defaults
                // to 1 (§4 D2).
                let (requested, accepted) = rejection
                    .counts()
                    .unwrap_or_else(|| (proposal.count.unwrap_or(1), 0));
                effects.push(RunEffect::Emit(WorkflowEvent::GrowthLimited {
                    run: run.clone(),
                    path: path.clone(),
                    template: rejection
                        .template()
                        .cloned()
                        .unwrap_or_else(|| proposal.template.clone()),
                    limit,
                    limit_value,
                    requested,
                    accepted,
                    message: rejection.message(Some(limit_value)),
                }));
            }
        }
        effects
    }

    /// Signals 2 and 3 arrive keyed by pane. Neither carries an artifact —
    /// reading `result.json` is I/O — so they are recorded as evidence and the
    /// artifact still has to arrive through a report.
    fn signal_from_pane(&mut self, pane: &PublicPaneId, signal: Signal) -> Vec<RunEffect> {
        let Some((idx, path)) = self.locate(pane) else {
            return Vec::new();
        };
        if !signal.available_for(self.runner_of(idx)) {
            return Vec::new();
        }
        self.signals.entry(path).or_default().observe(signal);
        Vec::new()
    }

    fn agent_status(
        &mut self,
        pane: &PublicPaneId,
        state: AgentState,
        _at: Instant,
    ) -> Vec<RunEffect> {
        let Some((idx, path)) = self.locate(pane) else {
            return Vec::new();
        };
        if !Signal::SustainedIdle.available_for(self.runner_of(idx)) {
            return Vec::new();
        }
        // An agent that is doing anything has read its seed. This is the fact
        // the re-delivery below turns on, so it is recorded before the idle
        // streak is consulted.
        if state == AgentState::Working {
            self.seeds.entry(path.clone()).or_default().acted = true;
        }
        let sustained = self
            .signals
            .entry(path.clone())
            .or_default()
            .observe_agent_state(state);
        if !sustained {
            return Vec::new();
        }

        let settled = self
            .graph
            .as_ref()
            .and_then(|graph| graph.node(idx))
            .is_some_and(|node| node.result.is_some() || node.status != NodeStatus::Running);
        if settled {
            return Vec::new();
        }

        if let Some(effects) = self.redeliver_seed(idx, &path) {
            return effects;
        }

        // §4.3: idle with no valid result never completes a node.
        match complete::missing_result(Signal::SustainedIdle) {
            Completion::NeedsAttention {
                reason,
                resume_when,
            } => self.needs_attention(idx, &reason, &resume_when),
            Completion::Accepted(_) | Completion::Reprompt { .. } => Vec::new(),
        }
    }

    /// §4.2's seed prompt is the `claude` argv's trailing positional, and a
    /// first-run workspace-trust dialog interrupts startup and consumes it —
    /// which happens on the first run in every fresh workspace. What karvex
    /// then observes is an agent that is up, settled at its prompt, and has
    /// never worked: `Running` forever with nothing to escalate and nothing to
    /// wait for.
    ///
    /// So the first sustained idle on a node that has never been observed
    /// working re-delivers the seed through `PromptNode`, which is the verified
    /// `agent.prompt` path for an `Agent` runner. Exactly once, in the spirit of
    /// §4.3's single corrective re-prompt: a second sustained idle with nothing
    /// done falls through to `NeedsAttention`, so a re-delivery that also fails
    /// surfaces instead of looping.
    ///
    /// Returns `None` when the node is not in that state, leaving the caller's
    /// normal completion path untouched.
    fn redeliver_seed(&mut self, idx: RunNodeIdx, path: &InstancePath) -> Option<Vec<RunEffect>> {
        let seed = self.seeds.entry(path.clone()).or_default();
        if seed.acted || seed.redelivered {
            return None;
        }
        // Both halves are resolved before anything is recorded: a node with no
        // pane has nowhere to receive the seed, and spending the single
        // re-delivery on a message that was never sent would leave the node
        // exactly as stuck with its one retry gone.
        let binding = self
            .graph
            .as_ref()
            .and_then(|graph| graph.node(idx))?
            .binding
            .as_ref()?;
        let text = crate::workflow::binding::spawn::seed_prompt_for(&binding.node_dir);
        let pane = binding.pane_id.clone();

        self.seeds.entry(path.clone()).or_default().redelivered = true;
        // The sustained-idle edge fires once per idle streak, so an agent that
        // silently swallows this re-delivery too would never produce a second
        // edge. Restarting the streak is what guarantees the fallback below is
        // eventually reached instead of the node hanging a second time.
        if let Some(ledger) = self.signals.get_mut(path) {
            ledger.reset_idle_streak();
        }
        debug!(path = %path, "workflow node seed prompt re-delivered");

        let mut effects = Vec::new();
        let graph = self.graph.as_mut()?;
        let payload = json!({ "text": text, "reason": "seed_not_registered" });
        effects.push(journal(
            graph,
            RunEventKind::MessageDelivered,
            Some(path.clone()),
            payload,
        ));
        effects.push(RunEffect::PromptNode { pane, text });
        Some(effects)
    }

    /// Materiality bookkeeping (§6.1). Phase 1 records the evidence; the
    /// Phase 4 watchdog is what acts on it, so no streak is incremented here.
    fn progress(&mut self, path: &InstancePath, delta: &ProgressDelta) -> Vec<RunEffect> {
        let Some(graph) = self.graph.as_mut() else {
            return Vec::new();
        };
        let Some(idx) = graph.index_of(path) else {
            return Vec::new();
        };
        let Some(node) = graph.node_mut(idx) else {
            return Vec::new();
        };

        let screen_changed = delta
            .screen_digest
            .as_ref()
            .is_some_and(|digest| node.progress.last_screen_digest.as_ref() != Some(digest));
        let material = delta.tool_calls > 0
            || delta.tokens > 0
            || delta.artifact_changes > 0
            || screen_changed;

        node.progress.tool_calls = node.progress.tool_calls.saturating_add(delta.tool_calls);
        node.progress.tokens = node.progress.tokens.saturating_add(delta.tokens);
        node.progress.artifact_changes = node
            .progress
            .artifact_changes
            .saturating_add(delta.artifact_changes);
        if let Some(digest) = delta.screen_digest.clone() {
            node.progress.last_screen_digest = Some(digest);
        }
        if material {
            node.progress.no_progress_streak = 0;
        }
        node.usage.total_tokens = node.usage.total_tokens.saturating_add(delta.tokens);
        node.usage.tool_uses = node.usage.tool_uses.saturating_add(delta.tool_calls);
        Vec::new()
    }

    /// §4.3: a pane that exits before a valid result fails the node, subject to
    /// the node's retry policy.
    fn pane_exited(&mut self, pane: &PublicPaneId, code: Option<i32>) -> Vec<RunEffect> {
        let Some((idx, path)) = self.locate(pane) else {
            return Vec::new();
        };
        let done = self
            .graph
            .as_ref()
            .and_then(|graph| graph.node(idx))
            .is_some_and(|node| node.status.is_terminal());
        if done {
            return Vec::new();
        }
        // A summariser whose pane died before it reported has nothing to retry
        // into: its run is over, and a second attempt would only reopen a pane
        // on a finished run. It gives up instead (§4 D1).
        if self.is_epilogue(idx) {
            let exit =
                code.map_or_else(|| "no exit code".to_string(), |code| format!("code {code}"));
            return self.give_up_epilogue(
                idx,
                &format!("the summariser's pane exited with {exit} before a summary was written"),
            );
        }

        let max_attempts = self.max_attempts_of(idx);
        // The retry below is a fresh attempt in a fresh pane, so it starts with
        // a fresh evidence ledger *and* a fresh corrective-re-prompt budget:
        // §4.3 gives every reported-result cycle one automatic re-prompt, and
        // an attempt that inherited the dead pane's strike would never get one.
        self.signals.remove(&path);
        self.reports.remove(&path);
        self.seeds.remove(&path);
        self.reported.remove(&path);
        self.delivery_failures.remove(&path);
        let mut effects = Vec::new();
        let exit = code.map_or_else(|| "no exit code".to_string(), |code| format!("code {code}"));
        let Some(graph) = self.graph.as_mut() else {
            return effects;
        };
        let retrying = graph
            .node(idx)
            .is_some_and(|node| node.attempt < max_attempts);

        if let Some(node) = graph.node_mut(idx) {
            if retrying {
                node.attempt = node.attempt.saturating_add(1);
                node.binding = None;
                node.status = NodeStatus::Ready;
            } else {
                node.status = NodeStatus::Failed;
                node.succession = Some(Succession::Blocked {
                    reason: format!("the node's pane exited with {exit} before a valid result"),
                    resume_when: "the node is restarted".to_string(),
                });
            }
        }
        let payload = json!({ "exit": exit, "retrying": retrying });
        effects.push(journal(
            graph,
            RunEventKind::Error,
            Some(path.clone()),
            payload,
        ));
        record_status(graph, idx, &mut effects);
        if !retrying {
            effects.push(RunEffect::Notify(UserNotice {
                level: NoticeLevel::Error,
                run: Some(graph.run_id.clone()),
                path: Some(path),
                message: format!("node failed: the pane exited with {exit}"),
            }));
        }
        self.settle(&mut effects);
        effects
    }

    /// The runtime exhausted its spawn attempts for an admitted node. The node
    /// leaves the ready set with the reason on it, which is what lets §3.2's
    /// conjunction pause the run instead of leaving it `Running` behind a node
    /// nothing will ever start.
    fn spawn_failed(&mut self, path: &InstancePath, reason: &str) -> Vec<RunEffect> {
        let Some(idx) = self.graph.as_ref().and_then(|graph| graph.index_of(path)) else {
            debug!(path = %path, "workflow spawn failure for an unknown node");
            return Vec::new();
        };
        let done = self
            .graph
            .as_ref()
            .and_then(|graph| graph.node(idx))
            .is_some_and(|node| node.status.is_terminal());
        if done {
            return Vec::new();
        }
        // The canonical blocker a first-time user meets: a run started with no
        // workspace has nowhere to put node panes, and the only way out is to
        // make one and restart the node.
        self.needs_attention(
            idx,
            &format!("the node's pane could not be started: {reason}"),
            "the run has a workspace to host node panes; create one with \
             `kvx workspace create`, then restart the node with \
             `kvx workflow node restart <run_id> <path>`",
        )
    }

    fn steer(&mut self, path: &InstancePath, text: &str) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        // This attempt answers for itself: a marker left by an earlier refused
        // delivery must not be read as this steer's outcome.
        self.delivery_failures.remove(path);
        let Some(graph) = self.graph.as_mut() else {
            return effects;
        };
        let Some(idx) = graph.index_of(path) else {
            return effects;
        };
        let pane = pane_of(graph, idx);
        let payload = json!({ "text": text });
        effects.push(journal(
            graph,
            RunEventKind::Steer,
            Some(path.clone()),
            payload,
        ));
        match pane {
            Some(pane) => effects.push(RunEffect::PromptNode {
                pane,
                text: text.to_string(),
            }),
            // A node with no pane has nowhere to receive the steer, and nothing
            // downstream will ever report a delivery failure for a delivery that
            // was never attempted. Recording it here is what stops the caller
            // believing the text landed.
            None => effects.extend(self.note_delivery_failure(
                path,
                "agent.prompt",
                "the node has no pane to deliver to",
            )),
        }
        effects
    }

    fn interrupt(&mut self, path: &InstancePath) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        self.delivery_failures.remove(path);
        let Some(idx) = self.graph.as_ref().and_then(|graph| graph.index_of(path)) else {
            return effects;
        };
        // §5's `agent.send_keys [Escape]` is the agent-runner interrupt: Escape
        // is what a `claude` TUI reads as "stop this turn". A `Runner::Command`
        // node is a plain process with no such convention, and Escape is a byte
        // it will simply ignore — the interrupt it can observe is the terminal's
        // own `ctrl+c`, which the line discipline turns into SIGINT. Same split
        // as the steer row: the primitive follows the runner.
        let keys = crate::workflow::binding::spawn::interrupt_keys(self.runner_of(idx));
        let Some(graph) = self.graph.as_mut() else {
            return effects;
        };
        let pane = pane_of(graph, idx);
        let payload = json!({ "keys": keys });
        effects.push(journal(
            graph,
            RunEventKind::Interrupt,
            Some(path.clone()),
            payload,
        ));
        match pane {
            Some(pane) => effects.push(RunEffect::SendKeys { pane, keys }),
            None => effects.extend(self.note_delivery_failure(
                path,
                "agent.send_keys",
                "the node has no pane to deliver to",
            )),
        }
        effects
    }

    /// §5: close the pane, `attempt += 1`, and hand the node back to the
    /// scheduler. Phase 1 always reseeds from `task.md`, because no `partial`
    /// checkpoint can exist before the Phase 4 watchdog writes them.
    fn restart(&mut self, path: &InstancePath) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        // A closed run has no scheduler left to collect a result: `settle`
        // returns immediately for any status outside `Running`/`Paused`. Handing
        // it a `Ready` node would spawn a pane nothing will ever read, inside a
        // run that already reported `cancelled`/`failed`/`succeeded` — a
        // terminal run silently containing a live process. Refuse instead.
        if let Some(status) = self.run_status() {
            if is_closed_run(status) {
                info!(
                    path = %path,
                    ?status,
                    "workflow node restart refused: the run has already closed"
                );
                return effects;
            }
        }
        self.signals.remove(path);
        self.reports.remove(path);
        self.seeds.remove(path);
        self.reported.remove(path);
        self.delivery_failures.remove(path);
        let Some(graph) = self.graph.as_mut() else {
            return effects;
        };
        let Some(idx) = graph.index_of(path) else {
            return effects;
        };
        if let Some(pane) = pane_of(graph, idx) {
            effects.push(RunEffect::ClosePane { pane });
        }
        if let Some(node) = graph.node_mut(idx) {
            node.attempt = node.attempt.saturating_add(1);
            node.binding = None;
            node.result = None;
            node.succession = None;
            node.progress = crate::workflow::model::ProgressTracker::default();
            // A restart is a fresh attempt: the previous attempt's timing is not
            // this one's, so `started_at`/`ended_at` reset alongside `binding`
            // and get restamped when the new attempt reaches `Running`/closes.
            node.started_at_unix_ms = None;
            node.ended_at_unix_ms = None;
            node.usage.duration_ms = 0;
            node.status = NodeStatus::Ready;
        }
        let payload = json!({ "restart": true });
        effects.push(journal(
            graph,
            RunEventKind::NodeStatus,
            Some(path.clone()),
            payload,
        ));
        record_status(graph, idx, &mut effects);
        // A restart is one of the explicit actions that clears a pause; the
        // node is `Ready` again, so `settle` is what notices and resumes.
        self.settle(&mut effects);
        effects
    }

    fn cancel(&mut self) -> Vec<RunEffect> {
        match self.graph.as_ref().map(|graph| is_closed_run(graph.status)) {
            None => return Vec::new(),
            // The user graph is already closed, but a summariser may still be
            // sitting in a live pane. Cancelling has to reach it: the alternative
            // is a pane working on a run the user just asked to stop, with
            // `epilogue_pending` keeping the tick alive behind it (§4 D1).
            Some(true) => {
                let Some(idx) = self.pending_epilogue_idx() else {
                    return Vec::new();
                };
                return self
                    .give_up_epilogue(idx, "the run was cancelled before the summary was written");
            }
            Some(false) => {}
        }

        let mut effects = Vec::new();
        let Some(graph) = self.graph.as_mut() else {
            return effects;
        };

        let live: Vec<RunNodeIdx> = graph
            .nodes
            .iter()
            .filter(|node| !node.status.is_terminal())
            .map(|node| node.idx)
            .collect();
        for idx in live {
            if let Some(pane) = pane_of(graph, idx) {
                effects.push(RunEffect::ClosePane { pane });
            }
            if let Some(node) = graph.node_mut(idx) {
                node.status = NodeStatus::Cancelled;
                node.succession = Some(Succession::NoFollowup {
                    evidence: "the run was cancelled".to_string(),
                });
            }
            record_status(graph, idx, &mut effects);
        }
        finish(graph, RunStatus::Cancelled, &mut effects);
        effects
    }

    fn tick(&mut self, _now: Instant) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        self.settle(&mut effects);
        effects
    }

    /// Re-settles the graph and decides the run's fate: the §3.2 conjunction
    /// finishes it, and a graph with nothing left to run pauses with the
    /// specific unmet conjunct instead of reporting success.
    fn settle(&mut self, effects: &mut Vec<RunEffect>) {
        let Some(graph) = self.graph.as_mut() else {
            return;
        };
        // A `Paused` run is still live: §3.2 pauses a run that stalled, it does
        // not close it. A steer, a late report, a retry, or a restart can hand
        // the graph a runnable node again, so a paused run has to keep settling
        // — otherwise no completion path could ever finish it, `RunFinished`
        // would never be emitted, and every later run would be refused as
        // in-flight.
        if !matches!(graph.status, RunStatus::Running | RunStatus::Paused) {
            return;
        }
        let settled = schedule::propagate(graph);
        record_edges(graph, &settled.edges, effects);
        for idx in settled.nodes {
            record_status(graph, idx, effects);
        }

        let mut just_finished = false;
        match schedule::run_terminal_ready(graph) {
            Ok(()) => {
                // §3.2 lets the conjunction hold with a `Blocked` node, because
                // the run may continue on other branches. Reporting that run as
                // `succeeded` would be the soft form of the false-completion
                // bug, so an unresolved blocker fails the run just as a `Failed`
                // node does.
                //
                // Engine-owned nodes are excluded for the same reason
                // `run_terminal_ready` excludes them (§4 D1): a summariser that
                // gave up must never turn a succeeded run into a failed one.
                let status = if graph
                    .nodes
                    .iter()
                    .filter(|node| !is_reserved_path(node.path.as_str()))
                    .any(|node| matches!(node.status, NodeStatus::Failed | NodeStatus::Blocked))
                {
                    RunStatus::Failed
                } else {
                    RunStatus::Succeeded
                };
                finish(graph, status, effects);
                just_finished = true;
            }
            Err(blocker) => {
                let live = graph
                    .nodes
                    .iter()
                    .any(|node| matches!(node.status, NodeStatus::Ready | NodeStatus::Running));
                match (live, graph.status) {
                    // The conjunct that stalled the run has a runnable node
                    // again, so the pause is over.
                    (true, RunStatus::Paused) => resume(graph, effects),
                    (false, RunStatus::Running) => pause(graph, &blocker, effects),
                    _ => {}
                }
            }
        }

        // Outside the borrow above, and outside `finish` itself: the epilogue is
        // appended only after the run's terminal status is decided and written,
        // so `workflow.run.finished` still precedes every summary effect.
        if just_finished {
            self.begin_epilogue(effects);
        }
    }

    fn succeed(&mut self, idx: RunNodeIdx, result: NodeResult) -> Vec<RunEffect> {
        // The summariser's accepted result is a `run_summary` row, not a node
        // checkpoint feeding downstream edges it does not have. Branching here
        // rather than at `report`'s call sites means no completion path can
        // reach the ordinary success machinery with the epilogue node.
        if self.is_epilogue(idx) {
            return self.accept_summary(idx, result);
        }
        let mut effects = Vec::new();
        let Some(graph) = self.graph.as_mut() else {
            return effects;
        };
        let Some(node) = graph.node_mut(idx) else {
            return effects;
        };
        node.status = NodeStatus::Succeeded;
        node.result = Some(result);
        // A node that succeeded is not blocked any more. `NeedsAttention` is
        // not terminal, and the resume condition the blocker itself prints —
        // steer the node until it writes a valid `result.json` — lands right
        // here without going through `restart`, which is the other place a
        // blocker is shed. `resolve_succession` below returns any explicitly
        // recorded succession verbatim, so leaving the blocker in place would
        // make it the succeeded node's permanent succession: a `resume:` line
        // for work that is already done, and a wire `succession` of `blocked`
        // on a node that succeeded. Only `Blocked` is cleared — it is the only
        // variant a non-terminal node can be carrying, and the "an explicit
        // succession wins" contract still holds for the rest.
        if matches!(node.succession, Some(Succession::Blocked { .. })) {
            node.succession = None;
        }
        // `node_checkpoint`'s unique index is `(run_node, seq)`, not
        // `(run, seq)` (`03` §4.3), so this counter is the node's own and
        // starts at 1 — the run journal cursor would make every node's first
        // checkpoint a different number.
        node.checkpoint_seq = node.checkpoint_seq.saturating_add(1);

        let Some(node) = graph.node(idx) else {
            return effects;
        };
        let path = node.path.clone();
        let seq = node.checkpoint_seq;
        let checkpoint = node.result.clone();
        if let Some(result) = checkpoint {
            effects.push(RunEffect::Persist(Box::new(StoreWrite::Checkpoint {
                run: graph.run_id.clone(),
                path: path.clone(),
                seq,
                kind: CheckpointKind::Result,
                schema_valid: true,
                payload: result.payload,
                summary: result.summary.clone(),
                artifact_paths: result.artifact_paths,
                digest: result.digest,
            })));
            effects.push(RunEffect::Emit(WorkflowEvent::NodeOutputCheckpoint {
                run: graph.run_id.clone(),
                path: path.clone(),
                seq,
                summary: result.summary,
            }));
        }

        let changed = schedule::propagate(graph);
        record_edges(graph, &changed.edges, &mut effects);
        match complete::resolve_succession(graph, idx) {
            Ok(succession) => {
                let payload = json!({ "succession": &succession });
                if let Some(node) = graph.node_mut(idx) {
                    node.succession = Some(succession);
                }
                effects.push(journal(
                    graph,
                    RunEventKind::Succession,
                    Some(path.clone()),
                    payload,
                ));
            }
            Err(gap) => {
                if let Some(node) = graph.node_mut(idx) {
                    node.status = NodeStatus::NeedsAttention;
                }
                effects.push(RunEffect::Notify(UserNotice {
                    level: NoticeLevel::Error,
                    run: Some(graph.run_id.clone()),
                    path: Some(path.clone()),
                    message: format!("succession gap: {}", gap.reason),
                }));
            }
        }
        record_status(graph, idx, &mut effects);
        for other in changed.nodes {
            if other != idx {
                record_status(graph, other, &mut effects);
            }
        }

        self.settle(&mut effects);
        effects
    }

    /// A node the run cannot make progress on without a human.
    ///
    /// `resume_when` is required, not optional: `NeedsAttention` pauses the run,
    /// and a paused run whose UI can only say *that* it is stuck is the failure
    /// mode `workflows.mdx` promises against. Recording it as a
    /// [`Succession::Blocked`] is what makes `run show`, `node show`, and the
    /// JSON API's `blocker` field non-empty — all three already render it and
    /// were reading a succession this function never set.
    ///
    /// It is set **before** [`record_status`], so the same call persists the
    /// blocker through `StoreWrite::RunNode` and emits a `NodeUpdated` carrying
    /// it. Recording a succession here does not risk letting a stalled run
    /// report success: `run_terminal_ready_with` rejects on `NeedsAttention`
    /// status before it ever looks at succession.
    fn needs_attention(
        &mut self,
        idx: RunNodeIdx,
        reason: &str,
        resume_when: &str,
    ) -> Vec<RunEffect> {
        // `NeedsAttention` means "the run is stalled until a human acts", and
        // the run the summariser belongs to is already over — there is nothing
        // to unstall and no one to wait for. The epilogue's ladder ends in
        // `GaveUp` instead (§4 D1). Branching here covers every route into this
        // function at once: a schema failure surviving the corrective
        // re-prompt, a self-report with no artifact, and a spawn failure.
        if self.is_epilogue(idx) {
            return self.give_up_epilogue(idx, reason);
        }
        let mut effects = Vec::new();
        let Some(graph) = self.graph.as_mut() else {
            return effects;
        };
        let Some(node) = graph.node_mut(idx) else {
            return effects;
        };
        node.status = NodeStatus::NeedsAttention;
        node.succession = Some(Succession::Blocked {
            reason: reason.to_string(),
            resume_when: resume_when.to_string(),
        });
        let path = node.path.clone();

        let payload = json!({ "reason": reason });
        effects.push(journal(
            graph,
            RunEventKind::Error,
            Some(path.clone()),
            payload,
        ));
        record_status(graph, idx, &mut effects);
        effects.push(RunEffect::Notify(UserNotice {
            level: NoticeLevel::Warning,
            run: Some(graph.run_id.clone()),
            path: Some(path),
            message: reason.to_string(),
        }));
        self.settle(&mut effects);
        effects
    }

    fn locate(&self, pane: &PublicPaneId) -> Option<(RunNodeIdx, InstancePath)> {
        self.graph
            .as_ref()
            .and_then(|graph| graph.node_by_pane(pane))
            .map(|node| (node.idx, node.path.clone()))
    }

    fn definition_node(&self, idx: RunNodeIdx) -> Option<&crate::workflow::model::KvdagNode> {
        let key = self
            .graph
            .as_ref()
            .and_then(|graph| graph.node(idx))
            .map(|node| &node.key)?;
        self.definition.as_ref().and_then(|kvdag| kvdag.node(key))
    }

    /// Without the definition the binding is unknown, and `Runner::Agent` is
    /// the definition's own default.
    ///
    /// The epilogue is the one node that has **no** definition by construction,
    /// so that default would be a guess rather than a fallback — and a wrong
    /// one whenever the summariser is bound to a command. It answers from the
    /// runner `begin_epilogue` recorded instead (defect D-1). This matters
    /// because the runner decides signal admissibility in `agent_status`: a
    /// command-bound epilogue must not accept sustained-idle as a completion
    /// signal, or karvex would re-deliver a seed prompt into a shell pane.
    fn runner_of(&self, idx: RunNodeIdx) -> Runner {
        if let Some(state) = self
            .graph
            .as_ref()
            .and_then(|graph| graph.epilogue)
            .filter(|state| state.node == idx)
        {
            return state.runner;
        }
        self.definition_node(idx)
            .map_or(Runner::Agent, |node| node.runner)
    }

    fn max_attempts_of(&self, idx: RunNodeIdx) -> u8 {
        self.definition_node(idx)
            .map_or(1, |node| node.max_attempts)
    }

    fn schema_for(&self, key: &NodeKey) -> Option<OutputSchema> {
        // The epilogue has no kvdag node behind it, so its contract is the
        // engine's own [`summary_output_schema`]. Without this the completion
        // gate would find no schema for `.summary` and surface the summariser
        // as unvalidatable on its very first report.
        if is_reserved_path(key.as_str()) {
            return OutputSchema::parse(summary_output_schema()).ok();
        }
        self.definition
            .as_ref()
            .and_then(|kvdag| kvdag.node(key))
            .map(|node| node.output_schema.clone())
    }
}

/// A run that has reached a final status. It is out of the live set for good:
/// `settle` never advances it again, so nothing will ever collect a result from
/// a node handed back to it.
pub fn is_closed_run(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled
    )
}

fn next_seq(graph: &mut RunGraph) -> u64 {
    graph.seq = graph.seq.saturating_add(1);
    graph.seq
}

/// Wall-clock epoch milliseconds for node `started_at`/`ended_at` stamps.
///
/// The engine's own contract is "deterministic given a supplied clock"
/// (module doc), which `EngineInput::AgentStatus`/`Tick`'s `Instant` honours for
/// scheduling; but `Instant` is monotonic and process-local, not an epoch, so it
/// cannot produce the wire's `_unix_ms` timestamps. `src/app/workflow.rs`'s
/// `ActiveRun::started_at_unix_ms`/`ended_at_unix_ms` already stamp the run
/// itself the same way, directly from the wall clock rather than a threaded
/// parameter — this mirrors that existing, accepted precedent for node-level
/// timestamps instead of introducing a second clock-injection mechanism.
pub(crate) fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn journal(
    graph: &mut RunGraph,
    kind: RunEventKind,
    path: Option<InstancePath>,
    payload: serde_json::Value,
) -> RunEffect {
    let seq = next_seq(graph);
    RunEffect::Persist(Box::new(StoreWrite::RunEvent {
        run: graph.run_id.clone(),
        seq,
        kind,
        path,
        payload,
        // Stamped here, not at store-flush time: the journal's timestamps are
        // the engine's facts, and a queued write applied minutes later must not
        // rewrite when the event happened (§4 D14).
        at_unix_ms: current_unix_ms(),
    }))
}

fn pane_of(graph: &RunGraph, idx: RunNodeIdx) -> Option<PublicPaneId> {
    graph
        .node(idx)?
        .binding
        .as_ref()
        .map(|binding| binding.pane_id.clone())
}

fn record_status(graph: &mut RunGraph, idx: RunNodeIdx, effects: &mut Vec<RunEffect>) {
    // Stamp timing before reading the node back for the store write, so the
    // persisted record and the in-memory node agree in the same call: a node
    // reaching `Running` gets `started_at` once, and a node reaching any
    // terminal status gets `ended_at` plus a `duration_ms` derived from the two
    // (`docs/design/workflow-builder/04-kvdag-and-execution.md` node lifecycle).
    if let Some(node) = graph.node_mut(idx) {
        if node.status == NodeStatus::Running && node.started_at_unix_ms.is_none() {
            node.started_at_unix_ms = Some(current_unix_ms());
        }
        if node.status.is_terminal() && node.ended_at_unix_ms.is_none() {
            let ended = current_unix_ms();
            node.ended_at_unix_ms = Some(ended);
            if let Some(started) = node.started_at_unix_ms {
                node.usage.duration_ms = ended.saturating_sub(started);
            }
        }
    }
    let Some(node) = graph.node(idx) else {
        return;
    };
    let path = node.path.clone();
    let status = node.status;
    effects.push(RunEffect::Persist(Box::new(StoreWrite::RunNode {
        run: graph.run_id.clone(),
        path: path.clone(),
        status,
        attempt: node.attempt,
        binding: node.binding.clone(),
        usage: node.usage,
        evidence: node.result.as_ref().map(|result| result.evidence),
        succession: node.succession.clone(),
        started_at_unix_ms: node.started_at_unix_ms,
        ended_at_unix_ms: node.ended_at_unix_ms,
        restored_from: node.restored_from.clone(),
    })));
    let payload = json!({ "status": status });
    effects.push(journal(
        graph,
        RunEventKind::NodeStatus,
        Some(path.clone()),
        payload,
    ));
    effects.push(RunEffect::Emit(WorkflowEvent::NodeUpdated {
        run: graph.run_id.clone(),
        path,
        status,
    }));
}

/// The edge counterpart of [`record_status`]: persists the firing state of the
/// edges a [`schedule::propagate`] pass settled.
///
/// Resolution is not one-way (§3.1), so this writes the *current* state rather
/// than only recording a first firing — a restarted source un-fires its
/// outbound edges, and a journal that kept the stale `fired` would describe a
/// branch the run is no longer taking.
fn record_edges(graph: &RunGraph, edges: &[usize], effects: &mut Vec<RunEffect>) {
    for index in edges {
        let Some(edge) = graph.edges.get(*index) else {
            continue;
        };
        let (Some(from), Some(to)) = (graph.node(edge.from), graph.node(edge.to)) else {
            continue;
        };
        effects.push(RunEffect::Persist(Box::new(StoreWrite::RunEdge {
            run: graph.run_id.clone(),
            from: from.path.clone(),
            to: to.path.clone(),
            kind: edge.kind,
            condition_result: edge.condition_result,
            fired: edge.fired,
        })));
    }
}

fn finish(graph: &mut RunGraph, status: RunStatus, effects: &mut Vec<RunEffect>) {
    graph.status = status;
    effects.push(RunEffect::Persist(Box::new(StoreWrite::RunStatus {
        run: graph.run_id.clone(),
        status,
        ended_at_unix_ms: Some(current_unix_ms()),
    })));
    let payload = json!({ "status": status });
    effects.push(journal(graph, RunEventKind::RunFinished, None, payload));
    effects.push(RunEffect::Emit(WorkflowEvent::RunFinished {
        run: graph.run_id.clone(),
        status,
    }));
    // A run reaching its end is announced to the user by
    // `app::workflow::run_status_notice`, which watches the run status and
    // fires exactly once per transition. Emitting a `Notify` here as well would
    // deliver the same completion twice.
}

/// Lifts a pause once the graph has a runnable node again. The counterpart of
/// [`pause`]: the run never left the live set, so this only restores the status
/// and tells the readers about it.
fn resume(graph: &mut RunGraph, effects: &mut Vec<RunEffect>) {
    graph.status = RunStatus::Running;
    effects.push(RunEffect::Persist(Box::new(StoreWrite::RunStatus {
        run: graph.run_id.clone(),
        status: RunStatus::Running,
        ended_at_unix_ms: None,
    })));
    effects.push(RunEffect::Emit(WorkflowEvent::RunUpdated {
        run: graph.run_id.clone(),
        status: RunStatus::Running,
    }));
}

fn pause(graph: &mut RunGraph, blocker: &TerminalBlocker, effects: &mut Vec<RunEffect>) {
    graph.status = RunStatus::Paused;
    effects.push(RunEffect::Persist(Box::new(StoreWrite::RunStatus {
        run: graph.run_id.clone(),
        status: RunStatus::Paused,
        ended_at_unix_ms: None,
    })));
    let payload = json!({ "blocker": blocker.to_string() });
    effects.push(journal(graph, RunEventKind::Error, None, payload));
    effects.push(RunEffect::Emit(WorkflowEvent::RunUpdated {
        run: graph.run_id.clone(),
        status: RunStatus::Paused,
    }));
    effects.push(RunEffect::Notify(UserNotice {
        level: NoticeLevel::Warning,
        run: Some(graph.run_id.clone()),
        path: None,
        message: format!("the run cannot report success: {blocker}"),
    }));
}

// ── the end-of-run epilogue (`07-phase3-plan.md` §4 D1) ─────────────────────

/// The summariser's output contract.
///
/// Declared here rather than authored per workflow because the summary is
/// karvex's artifact, not the user's: `run_summary`'s columns, the wire's
/// `WorkflowRunSummaryInfo`, and this schema are one shape, and a workflow
/// author cannot change it. The `maxLength` entries are what make the token
/// budget a property of the contract instead of a request in the prompt —
/// `complete::check` evaluates them, so an over-budget summary is a schema
/// violation like any other.
pub(crate) fn summary_output_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "required": ["text", "outcome", "highlights", "open_gaps", "per_node"],
        "properties": {
            "text": { "type": "string", "maxLength": SUMMARY_TEXT_BUDGET },
            "outcome": { "type": "string", "maxLength": SUMMARY_OUTCOME_BUDGET },
            "highlights": { "type": "array", "items": { "type": "string" } },
            "open_gaps": { "type": "array", "items": { "type": "string" } },
            "per_node": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["node_key", "verdict", "one_liner"],
                    "properties": {
                        "node_key": { "type": "string" },
                        "verdict": { "type": "string" },
                        "one_liner": { "type": "string" }
                    }
                }
            }
        }
    })
}

/// The karvex-authored prompt the summariser runs against: the fixed
/// what-to-cover text plus one evidence line per user node.
///
/// This is the task **body**, not the finished `task.md`. The binder wraps it
/// in the shared [`crate::workflow::binding::spawn::TaskDocument`], which
/// supplies the title and — the part that matters — the `## Reporting` section
/// telling the node to write `result.json` and run `kvx workflow node
/// complete`. Nothing here may restate that contract: a second copy is exactly
/// how the epilogue's contract went missing in the first place.
///
/// The evidence block is deliberately built from what the run already holds —
/// status, attempts, succession, and each node's checkpoint `summary`, which
/// `complete::SUMMARY_BUDGET` already caps at 1,200 characters — rather than
/// from transcripts or payloads. A summariser that had to read every node's full
/// output would cost more than the run it is summarising.
///
/// `kvdag` supplies each node's authored `role`, which is the one thing the run
/// graph does not carry and the only thing that explains *why* a node was in the
/// graph at all. A node with no definition behind it (an epilogue node, or a key
/// the version no longer has) simply contributes no role line.
pub fn summary_task_spec(
    graph: &RunGraph,
    kvdag: &Kvdag,
    command: Option<&[String]>,
) -> EpilogueTaskSpec {
    // No title line: `TaskDocument::render` writes the `# <label>` heading, and
    // a second one here would nest an H1 inside the document's own.
    let mut task = String::new();
    task.push_str(
        "You are karvex's end-of-run summariser. The run below has already finished; \
         its outcome is final and nothing you write can change it. Your job is to \
         leave behind something the next run of this workflow — and the person \
         reading the run history weeks from now — can actually use.\n\n",
    );
    task.push_str("Cover, in `text`:\n\n");
    task.push_str("- what the run set out to do and what it actually produced;\n");
    task.push_str("- what worked, and what had to be corrected or retried;\n");
    task.push_str("- what is still open, wrong, or unverified;\n");
    task.push_str("- anything a later run should know before repeating this work.\n\n");
    task.push_str(&format!(
        "Write `text` as prose, at most {SUMMARY_TEXT_BUDGET} characters — this is a \
         hard limit enforced by the output schema, not a suggestion, and an \
         over-budget summary is rejected. Keep `outcome` to a single line of at \
         most {SUMMARY_OUTCOME_BUDGET} characters. `highlights` and `open_gaps` are \
         short bullet strings. `per_node` needs one entry per node listed below, \
         with its `node_key`, a one-word `verdict`, and a `one_liner`.\n\n",
    ));
    task.push_str("Do not speculate about work you have no evidence for.\n\n");

    task.push_str("## The run\n\n");
    task.push_str(&format!(
        "- run: `{}`\n- workflow version: `{}`\n- status: `{:?}`\n- tier: `{:?}`\n\n",
        graph.run_id, graph.version_id, graph.status, graph.tier
    ));

    task.push_str("## Nodes\n\n");
    for node in graph
        .nodes
        .iter()
        .filter(|node| !is_reserved_path(node.path.as_str()))
    {
        task.push_str(&format!(
            "### `{}`\n\n- status: `{:?}`\n- attempts: {}\n",
            node.path, node.status, node.attempt
        ));
        if let Some(role) = kvdag
            .node(&node.key)
            .map(|definition| definition.role.trim())
            .filter(|role| !role.is_empty())
        {
            task.push_str(&format!("- role: {role}\n"));
        }
        match &node.succession {
            Some(Succession::Satisfied) => task.push_str("- succession: satisfied\n"),
            Some(Succession::Blocked {
                reason,
                resume_when,
            }) => task.push_str(&format!(
                "- blocked: {reason}\n- resume when: {resume_when}\n"
            )),
            Some(Succession::NoFollowup { evidence }) => {
                task.push_str(&format!("- no follow-up: {evidence}\n"));
            }
            None => task.push_str("- succession: none recorded\n"),
        }
        if let Some(result) = &node.result {
            task.push_str(&format!("- output summary: {}\n", result.summary));
        } else {
            task.push_str("- output summary: none — the node produced no validated result\n");
        }
        task.push('\n');
    }

    EpilogueTaskSpec {
        path: InstancePath::new(SUMMARY_INSTANCE_PATH),
        label: EPILOGUE_LABEL.to_string(),
        task_body: task,
        output_schema: summary_output_schema(),
        command: command.map(<[String]>::to_vec),
    }
}

/// The epilogue node's instance label. Short on purpose: it is what the DAG box
/// and the pane title read.
const EPILOGUE_LABEL: &str = "summary";

/// A rough token count for a written summary, for `run_summary.token_estimate`.
///
/// Four characters per token is the usual English approximation. It is an
/// estimate and named as one — no tokeniser runs in the engine, and the column
/// exists so a reader can see the order of magnitude of what history costs, not
/// to bill anybody.
fn token_estimate(payload: &serde_json::Value) -> u32 {
    let characters = payload.to_string().chars().count();
    u32::try_from(characters.div_ceil(4)).unwrap_or(u32::MAX)
}

/// Pulls the summary's fields out of a payload the built-in schema has already
/// validated.
///
/// Total by construction: the schema guarantees the shape, and every read here
/// still falls back rather than unwrapping, so a future schema relaxation
/// degrades to an emptier summary instead of a panic.
fn summary_fields(payload: &serde_json::Value) -> (String, String, Vec<String>, Vec<String>) {
    let string = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let strings = |key: &str| {
        payload
            .get(key)
            .and_then(serde_json::Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    (
        string("text"),
        string("outcome"),
        strings("highlights"),
        strings("open_gaps"),
    )
}

fn summary_per_node(payload: &serde_json::Value) -> Vec<SummaryNodeLine> {
    payload
        .get("per_node")
        .and_then(serde_json::Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .map(|entry| {
                    let field = |key: &str| {
                        entry
                            .get(key)
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    };
                    SummaryNodeLine {
                        node_key: field("node_key"),
                        verdict: field("verdict"),
                        one_liner: field("one_liner"),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests_support;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::engine::tests_support::{kvdag_of, spec_edge, spec_node, TestNode};
    use crate::workflow::model::{EdgeKind, RunId};
    use crate::workflow::tier::Tier;

    fn two_node_engine() -> (Engine, RunGraph) {
        let definition = kvdag_of(
            vec![
                spec_node(&TestNode::requiring("plan", &["plan"])),
                spec_node(&TestNode::requiring("implement", &["report"])),
            ],
            vec![spec_edge("plan", "implement", EdgeKind::Sequence)],
        );
        let graph = RunGraph::materialise(&definition, RunId::new("workflow_run:1"), Tier::High);
        let mut engine = Engine::new(EngineConfig::default());
        engine.install_definition(definition);
        (engine, graph)
    }

    fn binding(pane: &str) -> NodeBinding {
        NodeBinding {
            pane_id: PublicPaneId::new(pane),
            terminal_id: crate::terminal::TerminalId::alloc(),
            agent_session_id: "session".to_string(),
            transcript_path: std::path::PathBuf::from("transcript.jsonl"),
            node_dir: std::path::PathBuf::from("node"),
            cwd: std::path::PathBuf::from("."),
        }
    }

    fn report(raw: &str) -> RawJson {
        RawJson(serde_json::from_str(raw).expect("test json parses"))
    }

    fn status_of(engine: &Engine, path: &str) -> NodeStatus {
        engine
            .graph()
            .and_then(|graph| graph.node_by_path(&InstancePath::new(path)))
            .map(|node| node.status)
            .expect("the node exists")
    }

    fn succession_of(engine: &Engine, path: &str) -> Option<Succession> {
        engine
            .graph()
            .and_then(|graph| graph.node_by_path(&InstancePath::new(path)))
            .expect("the node exists")
            .succession
            .clone()
    }

    /// The blocker as the store would receive it: the last `StoreWrite::RunNode`
    /// for `path` in an effect batch is what the durable projection reads back.
    fn persisted_succession(effects: &[RunEffect], path: &str) -> Option<Succession> {
        let wanted = InstancePath::new(path);
        effects
            .iter()
            .rev()
            .find_map(|effect| match effect {
                RunEffect::Persist(write) => match write.as_ref() {
                    StoreWrite::RunNode {
                        path: written,
                        succession,
                        ..
                    } if written == &wanted => Some(succession.clone()),
                    _ => None,
                },
                _ => None,
            })
            .expect("the node's status was persisted")
    }

    #[test]
    fn default_config_matches_the_documented_defaults() {
        let config = EngineConfig::default();
        assert_eq!(config.max_parallel_nodes, 4);
        assert_eq!(config.stuck_threshold, 3);
        assert_eq!(config.drift_threshold, 5);
    }

    #[test]
    fn start_installs_the_graph_and_admits_the_roots() {
        let (mut engine, graph) = two_node_engine();
        let effects = engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });

        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Running)
        );
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, RunEffect::Emit(WorkflowEvent::RunStarted { .. }))));
        assert_eq!(status_of(&engine, "plan"), NodeStatus::Ready);
        assert_eq!(status_of(&engine, "implement"), NodeStatus::Pending);
        assert_eq!(engine.admissions(), vec![RunNodeIdx(0)]);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, RunEffect::Emit(WorkflowEvent::RunFinished { .. }))),
            "a run with work left never finishes at start"
        );
    }

    #[test]
    fn a_run_reaches_succeeded_only_through_validated_results() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

        engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: report(r#"{"plan":"do it"}"#),
        });
        assert_eq!(status_of(&engine, "plan"), NodeStatus::Succeeded);
        assert_eq!(status_of(&engine, "implement"), NodeStatus::Ready);
        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Running)
        );

        engine.bind_node(&InstancePath::new("implement"), binding("pane-2"));
        let effects = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("implement"),
            token: NodeToken::new("token"),
            result: report(r#"{"report":"shipped"}"#),
        });

        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Succeeded)
        );
        assert!(effects.iter().any(|effect| matches!(
            effect,
            RunEffect::Emit(WorkflowEvent::RunFinished {
                status: RunStatus::Succeeded,
                ..
            })
        )));
    }

    #[test]
    fn a_schema_invalid_result_reprompts_once_then_needs_attention() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

        let first = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: report(r#"{"notes":"oops"}"#),
        });
        let prompts: Vec<&RunEffect> = first
            .iter()
            .filter(|effect| matches!(effect, RunEffect::PromptNode { .. }))
            .collect();
        assert_eq!(prompts.len(), 1, "exactly one corrective re-prompt");
        assert_eq!(status_of(&engine, "plan"), NodeStatus::Running);

        let second = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: report(r#"{"notes":"oops again"}"#),
        });
        assert!(
            !second
                .iter()
                .any(|effect| matches!(effect, RunEffect::PromptNode { .. })),
            "the corrective re-prompt happens exactly once"
        );
        assert_eq!(status_of(&engine, "plan"), NodeStatus::NeedsAttention);
        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Paused),
            "a run with a node in NeedsAttention and nothing runnable pauses"
        );
    }

    /// Drives `pane` through one full sustained-idle streak.
    fn idle_streak(engine: &mut Engine, pane: &PublicPaneId, now: Instant) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        for _ in 0..3 {
            effects.extend(engine.apply(EngineInput::AgentStatus {
                pane: pane.clone(),
                state: AgentState::Idle,
                at: now,
            }));
        }
        effects
    }

    #[test]
    fn sustained_idle_without_a_result_needs_attention_and_never_succeeds() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

        let pane = PublicPaneId::new("pane-1");
        let now = Instant::now();
        // An agent that has been working read its seed, so idling afterwards is
        // the "went quiet with nothing to show" case, not the swallowed-seed one.
        engine.apply(EngineInput::AgentStatus {
            pane: pane.clone(),
            state: AgentState::Working,
            at: now,
        });
        for _ in 0..2 {
            engine.apply(EngineInput::AgentStatus {
                pane: pane.clone(),
                state: AgentState::Idle,
                at: now,
            });
            assert_eq!(status_of(&engine, "plan"), NodeStatus::Running);
        }
        engine.apply(EngineInput::AgentStatus {
            pane: pane.clone(),
            state: AgentState::Idle,
            at: now,
        });

        assert_eq!(status_of(&engine, "plan"), NodeStatus::NeedsAttention);
        assert_ne!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Succeeded)
        );
    }

    /// F9: `claude`'s first-run workspace-trust dialog interrupts startup and
    /// consumes the argv's trailing positional, so the agent comes up having
    /// never been told anything. It settles idle with no work done — the one
    /// state that distinguishes a swallowed seed from a finished turn — and the
    /// seed is re-delivered once through the verified `agent.prompt` path.
    #[test]
    fn an_agent_that_never_saw_its_seed_prompt_is_reseeded_once_then_surfaced() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

        let pane = PublicPaneId::new("pane-1");
        let now = Instant::now();
        let effects = idle_streak(&mut engine, &pane, now);

        let text = effects
            .iter()
            .find_map(|effect| match effect {
                RunEffect::PromptNode { pane, text } if *pane == PublicPaneId::new("pane-1") => {
                    Some(text.clone())
                }
                _ => None,
            })
            .expect("the seed is re-delivered rather than left swallowed");
        assert_eq!(
            text,
            crate::workflow::binding::spawn::seed_prompt_for(&std::path::PathBuf::from("node")),
            "the re-delivery is the seed itself, by absolute node-dir path"
        );
        assert_eq!(
            status_of(&engine, "plan"),
            NodeStatus::Running,
            "a node that has just been reseeded has not failed"
        );

        // Once, not in a loop: a second streak with still nothing done surfaces
        // the node instead of reseeding it again.
        let effects = idle_streak(&mut engine, &pane, now);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, RunEffect::PromptNode { .. })),
            "the seed is offered exactly once"
        );
        assert_eq!(status_of(&engine, "plan"), NodeStatus::NeedsAttention);
    }

    /// The re-delivery is for a seed that never registered. An agent that has
    /// been observed working read it, so its idle is a completion question and
    /// must not be answered by re-sending the task.
    #[test]
    fn an_agent_that_worked_is_never_reseeded() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

        let pane = PublicPaneId::new("pane-1");
        let now = Instant::now();
        engine.apply(EngineInput::AgentStatus {
            pane: pane.clone(),
            state: AgentState::Working,
            at: now,
        });
        let effects = idle_streak(&mut engine, &pane, now);

        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, RunEffect::PromptNode { .. })),
            "an agent that worked already had its seed"
        );
        assert_eq!(status_of(&engine, "plan"), NodeStatus::NeedsAttention);
    }

    /// D4: the only completion signal a `Runner::Command` node has is its own
    /// report, so a report that carries no result artifact has to be the thing
    /// that surfaces it. Reaching `NeedsAttention` here is what makes
    /// `kvx workflow node complete` safe to run unconditionally.
    #[test]
    fn a_self_report_with_no_result_artifact_needs_attention() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

        let effects = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: RawJson(serde_json::Value::Null),
        });

        assert_eq!(status_of(&engine, "plan"), NodeStatus::NeedsAttention);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, RunEffect::PromptNode { .. })),
            "there is no result to correct, so no corrective re-prompt is spent"
        );
        assert_ne!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Succeeded)
        );
    }

    /// An interrupt is only useful if the process can observe it. Escape is a
    /// `claude` TUI convention; a plain process needs `ctrl+c`.
    #[test]
    fn the_interrupt_key_follows_the_node_runner() {
        for (runner, expected) in [(Runner::Agent, "Escape"), (Runner::Command, "ctrl+c")] {
            let definition = kvdag_of(
                vec![spec_node(&TestNode {
                    runner,
                    ..TestNode::requiring("plan", &["plan"])
                })],
                Vec::new(),
            );
            let graph =
                RunGraph::materialise(&definition, RunId::new("workflow_run:1"), Tier::High);
            let mut engine = Engine::new(EngineConfig::default());
            engine.install_definition(definition);
            engine.apply(EngineInput::Start {
                graph: Box::new(graph),
            });
            engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

            let effects = engine.apply(EngineInput::Interrupt {
                path: InstancePath::new("plan"),
            });
            let keys = effects
                .iter()
                .find_map(|effect| match effect {
                    RunEffect::SendKeys { keys, .. } => Some(keys.clone()),
                    _ => None,
                })
                .expect("an interrupt on a bound node sends keys");
            assert_eq!(keys, vec![expected.to_string()], "runner {runner:?}");
        }
    }

    #[test]
    fn a_pane_exit_before_a_result_retries_then_fails_the_node() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

        engine.apply(EngineInput::PaneExited {
            pane: PublicPaneId::new("pane-1"),
            code: Some(1),
        });
        assert_eq!(
            status_of(&engine, "plan"),
            NodeStatus::Ready,
            "max_attempts 2 buys one retry"
        );

        engine.bind_node(&InstancePath::new("plan"), binding("pane-1b"));
        engine.apply(EngineInput::PaneExited {
            pane: PublicPaneId::new("pane-1b"),
            code: Some(1),
        });
        assert_eq!(status_of(&engine, "plan"), NodeStatus::Failed);
        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Paused),
            "a downstream node left Pending blocks the terminal conjunction"
        );
    }

    #[test]
    fn steer_and_interrupt_reach_the_bound_pane_and_are_journalled() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

        let steer = engine.apply(EngineInput::Steer {
            path: InstancePath::new("plan"),
            text: "focus on the API".to_string(),
        });
        assert!(steer.iter().any(|effect| matches!(
            effect,
            RunEffect::PromptNode { text, .. } if text == "focus on the API"
        )));
        assert!(steer.iter().any(|effect| matches!(
            effect,
            RunEffect::Persist(write)
                if matches!(**write, StoreWrite::RunEvent { kind: RunEventKind::Steer, .. })
        )));

        let interrupt = engine.apply(EngineInput::Interrupt {
            path: InstancePath::new("plan"),
        });
        assert!(interrupt.iter().any(|effect| matches!(
            effect,
            RunEffect::SendKeys { keys, .. } if keys == &vec!["Escape".to_string()]
        )));
    }

    #[test]
    fn restart_closes_the_pane_and_hands_the_node_back_to_the_scheduler() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

        let effects = engine.apply(EngineInput::RestartNode {
            path: InstancePath::new("plan"),
        });
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, RunEffect::ClosePane { .. })));
        assert_eq!(status_of(&engine, "plan"), NodeStatus::Ready);
        let node = engine
            .graph()
            .and_then(|graph| graph.node_by_path(&InstancePath::new("plan")))
            .expect("the node exists");
        assert_eq!(node.attempt, 2);
        assert!(node.binding.is_none());
    }

    fn node_of<'a>(engine: &'a Engine, path: &str) -> &'a crate::workflow::model::RunNode {
        engine
            .graph()
            .and_then(|graph| graph.node_by_path(&InstancePath::new(path)))
            .expect("the node exists")
    }

    /// Regression for B10: `started_at_unix_ms`/`ended_at_unix_ms`/`duration_ms`
    /// used to stay unset/zero for the whole run, on both the in-memory node
    /// and the persisted `StoreWrite::RunNode` effect.
    #[test]
    fn bind_node_stamps_started_at_and_leaves_ended_at_unset() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        assert!(
            node_of(&engine, "plan").started_at_unix_ms.is_none(),
            "a node that has not run yet has no started_at"
        );

        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

        let node = node_of(&engine, "plan");
        assert!(
            node.started_at_unix_ms.is_some(),
            "binding a node to a pane moves it to Running and stamps started_at"
        );
        assert!(
            node.ended_at_unix_ms.is_none(),
            "a still-running node has no ended_at"
        );
        assert_eq!(node.usage.duration_ms, 0);
    }

    #[test]
    fn a_succeeded_node_gets_ended_at_and_a_duration_derived_from_the_two_stamps() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));
        let started = node_of(&engine, "plan")
            .started_at_unix_ms
            .expect("stamped on bind");

        let effects = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: report(r#"{"plan":"do it"}"#),
        });

        let node = node_of(&engine, "plan");
        assert_eq!(node.status, NodeStatus::Succeeded);
        let ended = node
            .ended_at_unix_ms
            .expect("a terminal node has ended_at set");
        assert!(ended >= started);
        assert_eq!(
            node.usage.duration_ms,
            ended - started,
            "duration_ms is derived from ended_at - started_at"
        );

        // The persisted write for this node carries the same stamps the
        // in-memory node holds — B10 was also missing on the persisted path.
        let persisted = effects.iter().find_map(|effect| match effect {
            RunEffect::Persist(write) => match write.as_ref() {
                StoreWrite::RunNode {
                    path,
                    started_at_unix_ms,
                    ended_at_unix_ms,
                    ..
                } if path == &InstancePath::new("plan") => {
                    Some((*started_at_unix_ms, *ended_at_unix_ms))
                }
                _ => None,
            },
            _ => None,
        });
        assert_eq!(
            persisted,
            Some((Some(started), Some(ended))),
            "the StoreWrite::RunNode effect for the closing status change carries \
             the same started_at/ended_at the in-memory node just got stamped with"
        );
    }

    /// `run_edge.fired`/`condition_result` had no write site: the scheduler
    /// settled them in memory and nothing ever persisted them, so a restored
    /// run reported `fired: false` on the edges it had actually taken.
    #[test]
    fn settling_an_edge_persists_its_firing_state() {
        fn edge_writes(effects: &[RunEffect]) -> Vec<(String, String, Option<bool>, bool)> {
            effects
                .iter()
                .filter_map(|effect| match effect {
                    RunEffect::Persist(write) => match write.as_ref() {
                        StoreWrite::RunEdge {
                            from,
                            to,
                            condition_result,
                            fired,
                            ..
                        } => Some((from.to_string(), to.to_string(), *condition_result, *fired)),
                        _ => None,
                    },
                    _ => None,
                })
                .collect()
        }

        let (mut engine, graph) = two_node_engine();
        let started = engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        assert!(
            edge_writes(&started).is_empty(),
            "no edge has settled at run start, so there is nothing to persist"
        );

        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));
        let effects = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: report(r#"{"plan":"do it"}"#),
        });

        assert_eq!(
            edge_writes(&effects),
            vec![(
                "plan".to_string(),
                "implement".to_string(),
                Some(true),
                true
            )],
            "the edge the succeeding node just fired must be persisted once, \
             carrying the same state the in-memory graph holds"
        );
        let edge = engine
            .graph()
            .and_then(|graph| graph.edges.first())
            .expect("the fixture has one edge");
        assert_eq!((edge.condition_result, edge.fired), (Some(true), true));
    }

    fn checkpoint_of(effects: &[RunEffect]) -> (serde_json::Value, String, String, Vec<String>) {
        effects
            .iter()
            .find_map(|effect| match effect {
                RunEffect::Persist(write) => match write.as_ref() {
                    StoreWrite::Checkpoint {
                        payload,
                        summary,
                        digest,
                        artifact_paths,
                        ..
                    } => Some((
                        payload.clone(),
                        summary.clone(),
                        digest.clone(),
                        artifact_paths.clone(),
                    )),
                    _ => None,
                },
                _ => None,
            })
            .expect("a succeeding node checkpoints its validated result")
    }

    fn report_plan(engine: &mut Engine, raw: &str) -> Vec<RunEffect> {
        engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: report(raw),
        })
    }

    fn started_engine() -> Engine {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));
        engine
    }

    /// **The Phase 3 restore-compatibility guard** (§4 D6). `complete::check`
    /// never rejects unknown top-level keys, so an `expand` left in the result
    /// would validate by accident and then flow into the payload, the summary,
    /// the artifact index, and the `digest` that Phase 3's restore compares
    /// across versions. A quiet Phase 2 choice would become a Phase 3
    /// correctness bug, so the checkpoint a node produces must be *byte*
    /// identical with and without the key.
    #[test]
    fn an_expand_key_never_reaches_the_payload_summary_artifacts_or_digest() {
        let plain = checkpoint_of(&report_plan(
            &mut started_engine(),
            r#"{"plan":"do it","summary":"planned","artifacts":["out/a.md"]}"#,
        ));
        let expanding = checkpoint_of(&report_plan(
            &mut started_engine(),
            r#"{"plan":"do it","summary":"planned","artifacts":["out/a.md"],
                "expand":[{"template":"worker","label":"w","inputs":{"focus":"api"},"count":2}]}"#,
        ));

        assert_eq!(
            plain, expanding,
            "the checkpointed payload, summary, digest and artifact index must not \
             record that the node also proposed an expansion"
        );

        let (payload, _, _, _) = expanding;
        assert!(
            payload.get("expand").is_none(),
            "the proposal's home is the expand_proposed journal entry, not the payload"
        );
    }

    /// The node prompt/output contract is unchanged (§3 frozen interface 11):
    /// `expand` is an optional *additional* key, so a schema that has never
    /// heard of it still passes a result that carries it.
    #[test]
    fn an_expand_array_validates_against_a_schema_that_never_mentions_it() {
        let mut engine = started_engine();
        let effects = report_plan(
            &mut engine,
            r#"{"plan":"do it","expand":[{"template":"worker","label":"w"}]}"#,
        );

        assert_eq!(status_of(&engine, "plan"), NodeStatus::Succeeded);
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, RunEffect::PromptNode { .. })),
            "a well-formed expand costs the node nothing"
        );
        assert_eq!(
            engine
                .node_facts(&InstancePath::new("plan"))
                .schema_failures,
            0
        );
    }

    /// A proposal is journalled before it is judged, whatever the verdict: with
    /// the key stripped from the payload (§4 D6), `expand_proposed` is the only
    /// durable record that the node asked at all.
    #[test]
    fn a_proposal_is_journalled_as_expand_proposed_before_it_is_judged() {
        let mut engine = started_engine();
        let effects = report_plan(
            &mut engine,
            r#"{"plan":"do it","expand":[{"template":"worker","label":"w","count":2}]}"#,
        );

        let payload = effects
            .iter()
            .find_map(|effect| match effect {
                RunEffect::Persist(write) => match write.as_ref() {
                    StoreWrite::RunEvent {
                        kind: RunEventKind::ExpandProposed,
                        path,
                        payload,
                        ..
                    } if path.as_ref() == Some(&InstancePath::new("plan")) => Some(payload.clone()),
                    _ => None,
                },
                _ => None,
            })
            .expect("the proposal is journalled");

        assert_eq!(
            payload.get("template").and_then(|v| v.as_str()),
            Some("worker")
        );
        assert_eq!(payload.get("label").and_then(|v| v.as_str()), Some("w"));
        assert_eq!(
            payload.get("count").and_then(serde_json::Value::as_u64),
            Some(2)
        );
    }

    /// A malformed `expand` is a schema-class violation: it spends the node's
    /// single corrective re-prompt and no more, and the correction names the
    /// field rather than sending the node to a schema that never saw the key.
    #[test]
    fn a_malformed_expand_spends_exactly_one_reprompt_then_needs_attention() {
        let mut engine = started_engine();

        let first = report_plan(&mut engine, r#"{"plan":"do it","expand":"just do it"}"#);
        let prompts: Vec<&String> = first
            .iter()
            .filter_map(|effect| match effect {
                RunEffect::PromptNode { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(prompts.len(), 1, "exactly one corrective re-prompt");
        assert!(
            prompts[0].contains("expand"),
            "the correction names the field it is about: {}",
            prompts[0]
        );
        assert_eq!(
            status_of(&engine, "plan"),
            NodeStatus::Running,
            "a correctable node keeps its pane"
        );
        assert_eq!(
            engine
                .report_outcome(&InstancePath::new("plan"))
                .map(|outcome| outcome.verdict),
            Some(ReportVerdict::Corrected)
        );

        let second = report_plan(&mut engine, r#"{"plan":"do it","expand":"just do it"}"#);
        assert!(
            !second
                .iter()
                .any(|effect| matches!(effect, RunEffect::PromptNode { .. })),
            "the corrective re-prompt happens exactly once"
        );
        assert_eq!(status_of(&engine, "plan"), NodeStatus::NeedsAttention);
        assert_eq!(
            engine
                .node_facts(&InstancePath::new("plan"))
                .schema_failures,
            2,
            "both rejections are counted"
        );
        assert!(
            !engine
                .node_facts(&InstancePath::new("plan"))
                .first_pass_succeeded
        );
    }

    /// §4 D8: the two `NodeHistory` inputs the engine can state truthfully.
    #[test]
    fn first_pass_succeeded_is_true_only_without_a_spent_correction() {
        let mut clean = started_engine();
        report_plan(&mut clean, r#"{"plan":"do it"}"#);
        let facts = clean.node_facts(&InstancePath::new("plan"));
        assert!(facts.first_pass_succeeded);
        assert_eq!(facts.schema_failures, 0);

        let mut corrected = started_engine();
        report_plan(&mut corrected, r#"{"notes":"oops"}"#);
        assert_eq!(status_of(&corrected, "plan"), NodeStatus::Running);
        report_plan(&mut corrected, r#"{"plan":"do it"}"#);

        assert_eq!(status_of(&corrected, "plan"), NodeStatus::Succeeded);
        let facts = corrected.node_facts(&InstancePath::new("plan"));
        assert!(
            !facts.first_pass_succeeded,
            "the node reached Succeeded, but only after the correction"
        );
        assert_eq!(facts.schema_failures, 1);
    }

    /// A retry is a second attempt by definition, so however clean its result
    /// is it is not a first pass. Failures are cumulative across attempts —
    /// a respawn does not refund a rejection the node already earned.
    #[test]
    fn a_retried_node_never_reports_a_first_pass_and_keeps_its_failure_count() {
        let mut engine = started_engine();
        report_plan(&mut engine, r#"{"notes":"oops"}"#);
        assert_eq!(
            engine
                .node_facts(&InstancePath::new("plan"))
                .schema_failures,
            1
        );

        engine.apply(EngineInput::PaneExited {
            pane: PublicPaneId::new("pane-1"),
            code: Some(1),
        });
        assert_eq!(status_of(&engine, "plan"), NodeStatus::Ready);
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1b"));
        report_plan(&mut engine, r#"{"plan":"do it"}"#);

        assert_eq!(status_of(&engine, "plan"), NodeStatus::Succeeded);
        let facts = engine.node_facts(&InstancePath::new("plan"));
        assert!(!facts.first_pass_succeeded, "attempt 2 is not a first pass");
        assert_eq!(
            facts.schema_failures, 1,
            "a fresh pane does not refund the rejection the node already earned"
        );
    }

    /// A signal with no artifact is not a schema failure: there was nothing to
    /// validate, so counting it would inflate the measurement `auto` reads.
    #[test]
    fn a_report_with_no_artifact_is_not_counted_as_a_schema_failure() {
        let mut engine = started_engine();
        engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: RawJson(serde_json::Value::Null),
        });

        assert_eq!(status_of(&engine, "plan"), NodeStatus::NeedsAttention);
        assert_eq!(
            engine
                .node_facts(&InstancePath::new("plan"))
                .schema_failures,
            0
        );
    }

    /// The run's facts are the run's own: a second `Start` must not report the
    /// previous run's measurements.
    #[test]
    fn starting_a_run_clears_the_previous_runs_facts() {
        let mut engine = started_engine();
        report_plan(&mut engine, r#"{"notes":"oops"}"#);
        assert_eq!(
            engine
                .node_facts(&InstancePath::new("plan"))
                .schema_failures,
            1
        );

        let (_, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        assert_eq!(
            engine.node_facts(&InstancePath::new("plan")),
            NodeFacts::default()
        );
    }

    /// The mid-run channel (`kvx workflow node expand`) reaches the same
    /// pipeline as the result key, and a node that has already closed cannot
    /// use it — its children would hang off a settled parent inside a run that
    /// may itself have finished.
    #[test]
    fn a_mid_run_proposal_is_journalled_and_a_closed_node_cannot_make_one() {
        let mut engine = started_engine();
        let proposals = vec![crate::workflow::engine::expand::ExpandProposal {
            template: NodeKey::new("worker"),
            label: "w".to_string(),
            inputs: std::collections::BTreeMap::new(),
            count: None,
        }];

        let effects = engine.apply(EngineInput::ExpandProposed {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            proposals: proposals.clone(),
        });
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                RunEffect::Persist(write)
                    if matches!(**write, StoreWrite::RunEvent { kind: RunEventKind::ExpandProposed, .. })
            )),
            "a mid-run proposal is journalled like a result-carried one"
        );

        report_plan(&mut engine, r#"{"plan":"do it"}"#);
        assert_eq!(status_of(&engine, "plan"), NodeStatus::Succeeded);
        assert!(
            engine
                .apply(EngineInput::ExpandProposed {
                    path: InstancePath::new("plan"),
                    token: NodeToken::new("token"),
                    proposals: proposals.clone(),
                })
                .is_empty(),
            "a closed node cannot grow the graph"
        );
        assert!(
            engine
                .apply(EngineInput::ExpandProposed {
                    path: InstancePath::new("nonexistent"),
                    token: NodeToken::new("token"),
                    proposals,
                })
                .is_empty(),
            "and neither can a node that is not in the graph"
        );
    }

    /// `fanout` may expand the `worker` template four times; `collect` is the
    /// §3.4 fan-in point, drawn from `fanout` so a child inherits it.
    fn expanding_engine(expand_max: u16) -> (Engine, RunGraph) {
        let mut fanout = spec_node(&TestNode::requiring("fanout", &["plan"]));
        fanout.expand_allow = vec![NodeKey::new("worker")];
        fanout.expand_max = expand_max;
        let mut worker = spec_node(&TestNode::requiring("worker", &["report"]));
        worker.is_template = true;

        let definition = kvdag_of(
            vec![
                fanout,
                worker,
                spec_node(&TestNode::requiring("collect", &["report"])),
            ],
            vec![
                spec_edge("fanout", "worker", EdgeKind::Sequence),
                spec_edge("worker", "collect", EdgeKind::Sequence),
                spec_edge("fanout", "collect", EdgeKind::Sequence),
            ],
        );
        let graph = RunGraph::materialise(&definition, RunId::new("workflow_run:1"), Tier::High);
        let mut engine = Engine::new(EngineConfig::default());
        engine.install_definition(definition);
        (engine, graph)
    }

    fn expanding_report(engine: &mut Engine, count: u16) -> Vec<RunEffect> {
        engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("fanout"),
            token: NodeToken::new("token"),
            result: report(&format!(
                r#"{{"plan":"fan out","expand":[{{"template":"worker","label":"w","count":{count}}}]}}"#
            )),
        })
    }

    fn started_expanding_engine(expand_max: u16) -> Engine {
        let (mut engine, graph) = expanding_engine(expand_max);
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("fanout"), binding("pane-1"));
        engine
    }

    fn created_paths(effects: &[RunEffect]) -> Vec<InstancePath> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                RunEffect::Persist(write) => match write.as_ref() {
                    StoreWrite::RunNodeCreated { path, .. } => Some(path.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// D7: `pending_writes` is a bounded, drop-oldest queue, and
    /// `StoreWrite::RunNode` is find-then-`UPDATE` that errors on a missing
    /// row. A create that landed after its own update — or that the queue could
    /// evict while keeping the update — is a permanent decode error for that
    /// node, so the create must be first in the vector, not merely present.
    fn assert_creates_precede_their_updates(effects: &[RunEffect]) {
        let mut created: HashMap<InstancePath, usize> = HashMap::new();
        for (index, effect) in effects.iter().enumerate() {
            if let RunEffect::Persist(write) = effect {
                if let StoreWrite::RunNodeCreated { path, .. } = write.as_ref() {
                    created.entry(path.clone()).or_insert(index);
                }
            }
        }
        assert!(
            !created.is_empty(),
            "the fixture must create at least one node for this invariant to mean anything"
        );
        for (index, effect) in effects.iter().enumerate() {
            if let RunEffect::Persist(write) = effect {
                if let StoreWrite::RunNode { path, .. } = write.as_ref() {
                    if let Some(create) = created.get(path) {
                        assert!(
                            *create < index,
                            "RunNodeCreated for {path} is at {create} but a RunNode update \
                             for it is at {index}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn an_accepted_proposal_creates_its_children_before_it_updates_them() {
        let mut engine = started_expanding_engine(4);
        let effects = expanding_report(&mut engine, 2);

        assert_eq!(
            created_paths(&effects),
            vec![
                InstancePath::new("fanout/worker/1"),
                InstancePath::new("fanout/worker/2"),
            ],
            "instance paths are <parent>/<template>/<n>, 1-based"
        );
        assert_creates_precede_their_updates(&effects);
        assert_eq!(status_of(&engine, "fanout"), NodeStatus::Succeeded);
        assert_eq!(
            status_of(&engine, "fanout/worker/1"),
            NodeStatus::Ready,
            "the parent→child sequence edge fired when the parent succeeded"
        );
    }

    /// The run must not close in the very call that grew it: `succeed`
    /// propagates and settles, so an expansion committed after the parent's
    /// success would arrive at an already-finished run.
    #[test]
    fn expanding_on_the_last_result_keeps_the_run_alive() {
        let mut engine = started_expanding_engine(4);
        let effects = expanding_report(&mut engine, 1);

        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Running)
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, RunEffect::Emit(WorkflowEvent::RunFinished { .. }))),
            "a run that just grew has not finished"
        );
    }

    /// §4 D2: never accept-all, never reject-all, never silently truncated.
    #[test]
    fn a_proposal_over_budget_is_partially_accepted_and_reports_the_shortfall() {
        let mut engine = started_expanding_engine(2);
        let effects = expanding_report(&mut engine, 5);

        assert_eq!(
            created_paths(&effects).len(),
            2,
            "the two that fit are created"
        );
        let limited = effects
            .iter()
            .find_map(|effect| match effect {
                RunEffect::Emit(WorkflowEvent::GrowthLimited {
                    template,
                    limit,
                    limit_value,
                    requested,
                    accepted,
                    message,
                    ..
                }) => Some((
                    template.clone(),
                    *limit,
                    *limit_value,
                    *requested,
                    *accepted,
                    message.clone(),
                )),
                _ => None,
            })
            .expect("the shortfall is reported, never silently truncated");

        assert_eq!(limited.0, NodeKey::new("worker"));
        assert_eq!(
            limited.1,
            crate::workflow::engine::expand::ExpandLimit::ExpandMax
        );
        assert_eq!(limited.2, 2, "the ceiling that was hit");
        assert_eq!((limited.3, limited.4), (5, 2));
        assert!(limited.5.contains("2 of 5"), "message: {}", limited.5);
    }

    /// A guardrail refusal is not an error: the node succeeds, the run
    /// continues, and the refusal is reported.
    #[test]
    fn a_wholly_refused_proposal_still_lets_the_node_succeed() {
        let mut engine = started_expanding_engine(0);
        let effects = expanding_report(&mut engine, 2);

        assert!(created_paths(&effects).is_empty());
        assert_eq!(status_of(&engine, "fanout"), NodeStatus::Succeeded);
        assert!(effects.iter().any(|effect| matches!(
            effect,
            RunEffect::Emit(WorkflowEvent::GrowthLimited { limit_value: 0, .. })
        )));
    }

    /// A validation refusal is the node being wrong, not the run running out of
    /// room, so it produces no `workflow.growth.limited`.
    #[test]
    fn a_validation_refusal_reports_no_growth_limit() {
        let mut engine = started_expanding_engine(4);
        let effects = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("fanout"),
            token: NodeToken::new("token"),
            result: report(r#"{"plan":"x","expand":[{"template":"nobody","label":"w"}]}"#),
        });

        assert!(created_paths(&effects).is_empty());
        assert!(
            !effects.iter().any(|effect| matches!(
                effect,
                RunEffect::Emit(WorkflowEvent::GrowthLimited { .. })
            )),
            "an unknown template is a validation failure, not a growth limit"
        );
        assert!(effects.iter().any(|effect| matches!(
            effect,
            RunEffect::Persist(write)
                if matches!(**write, StoreWrite::RunEvent { kind: RunEventKind::ExpandRejected, .. })
        )));
        assert_eq!(status_of(&engine, "fanout"), NodeStatus::Succeeded);
    }

    /// The mid-run verb reaches the same guardrails as the result key, and
    /// `expand_max` is cumulative across every proposal a node makes.
    #[test]
    fn expand_max_is_cumulative_across_a_nodes_proposals() {
        let mut engine = started_expanding_engine(2);
        let proposal = |count: Option<u16>| crate::workflow::engine::expand::ExpandProposal {
            template: NodeKey::new("worker"),
            label: "w".to_string(),
            inputs: std::collections::BTreeMap::new(),
            count,
        };

        let first = engine.apply(EngineInput::ExpandProposed {
            path: InstancePath::new("fanout"),
            token: NodeToken::new("token"),
            proposals: vec![proposal(None), proposal(None)],
        });
        assert_eq!(created_paths(&first).len(), 2);
        assert_creates_precede_their_updates(&first);

        let third = engine.apply(EngineInput::ExpandProposed {
            path: InstancePath::new("fanout"),
            token: NodeToken::new("token"),
            proposals: vec![proposal(None)],
        });
        assert!(
            created_paths(&third).is_empty(),
            "the third child would exceed expand_max 2"
        );
        assert!(third
            .iter()
            .any(|effect| matches!(effect, RunEffect::Emit(WorkflowEvent::GrowthLimited { .. }))));
    }

    #[test]
    fn restart_clears_timing_so_the_next_attempt_gets_fresh_stamps() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));
        assert!(node_of(&engine, "plan").started_at_unix_ms.is_some());

        engine.apply(EngineInput::RestartNode {
            path: InstancePath::new("plan"),
        });

        let node = node_of(&engine, "plan");
        assert!(
            node.started_at_unix_ms.is_none(),
            "restart hands the node back to the scheduler, and its previous \
             attempt's started_at is not this attempt's"
        );
        assert!(node.ended_at_unix_ms.is_none());
        assert_eq!(node.usage.duration_ms, 0);
    }

    #[test]
    fn a_run_that_closes_with_a_blocked_node_never_reports_success() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));
        engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: report(r#"{"plan":"do it"}"#),
        });

        let idx = engine
            .graph()
            .and_then(|graph| graph.index_of(&InstancePath::new("implement")))
            .expect("the node exists");
        if let Some(node) = engine.graph.as_mut().and_then(|graph| graph.node_mut(idx)) {
            node.status = NodeStatus::Blocked;
            node.succession = Some(Succession::Blocked {
                reason: "waiting on a release approval".to_string(),
                resume_when: "the approval lands".to_string(),
            });
        }
        engine.apply(EngineInput::Tick {
            now: Instant::now(),
        });

        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Failed)
        );
    }

    #[test]
    fn restarting_a_node_clears_a_paused_run() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));
        for _ in 0..2 {
            engine.apply(EngineInput::NodeSelfReport {
                path: InstancePath::new("plan"),
                token: NodeToken::new("token"),
                result: report(r#"{"notes":"oops"}"#),
            });
        }
        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Paused)
        );

        engine.apply(EngineInput::RestartNode {
            path: InstancePath::new("plan"),
        });
        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Running)
        );
        assert_eq!(status_of(&engine, "plan"), NodeStatus::Ready);
    }

    /// A pause is a stall, not a close: `Paused` stays in the live set, so the
    /// run has to be able to leave it *and* finish. Without this the run never
    /// emits `RunFinished`, never sets `ended_at`, and blocks every later
    /// `workflow.run` with `run_in_flight` until the server restarts.
    #[test]
    fn a_paused_run_that_gets_a_valid_result_resumes_and_finishes() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));
        for _ in 0..2 {
            engine.apply(EngineInput::NodeSelfReport {
                path: InstancePath::new("plan"),
                token: NodeToken::new("token"),
                result: report(r#"{"notes":"oops"}"#),
            });
        }
        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Paused)
        );

        // The user steers the node and it finally reports something valid.
        let resumed = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: report(r#"{"plan":"do it"}"#),
        });
        assert_eq!(status_of(&engine, "plan"), NodeStatus::Succeeded);
        assert_eq!(status_of(&engine, "implement"), NodeStatus::Ready);
        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Running),
            "a runnable node lifts the pause"
        );
        assert!(resumed.iter().any(|effect| matches!(
            effect,
            RunEffect::Emit(WorkflowEvent::RunUpdated {
                status: RunStatus::Running,
                ..
            })
        )));
        assert_eq!(
            engine.admissions(),
            vec![RunNodeIdx(1)],
            "the resumed run schedules again"
        );

        engine.bind_node(&InstancePath::new("implement"), binding("pane-2"));
        let effects = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("implement"),
            token: NodeToken::new("token"),
            result: report(r#"{"report":"shipped"}"#),
        });
        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Succeeded)
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, RunEffect::Emit(WorkflowEvent::RunFinished { .. }))),
            "a run that recovered from a pause still finishes"
        );
    }

    /// §3.1 resolves a `Data` edge only while its source is `Succeeded` *with* a
    /// validated result. Restarting a succeeded node clears that result, so its
    /// outbound edges must stop counting — otherwise a fan-in node runs with the
    /// restarted branch's port missing.
    #[test]
    fn restarting_a_succeeded_node_unfires_its_edges_and_holds_the_fan_in() {
        let definition = kvdag_of(
            vec![
                spec_node(&TestNode::requiring("plan", &["plan"])),
                spec_node(&TestNode::requiring("left", &["report"])),
                spec_node(&TestNode::requiring("right", &["report"])),
                spec_node(&TestNode::requiring("join", &["report"])),
            ],
            vec![
                spec_edge("plan", "left", EdgeKind::Data),
                spec_edge("plan", "right", EdgeKind::Data),
                spec_edge("left", "join", EdgeKind::Data),
                spec_edge("right", "join", EdgeKind::Data),
            ],
        );
        let graph = RunGraph::materialise(&definition, RunId::new("workflow_run:1"), Tier::High);
        let mut engine = Engine::new(EngineConfig::default());
        engine.install_definition(definition);
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });

        let succeed = |engine: &mut Engine, path: &str, pane: &str, body: &str| {
            engine.bind_node(&InstancePath::new(path), binding(pane));
            engine.apply(EngineInput::NodeSelfReport {
                path: InstancePath::new(path),
                token: NodeToken::new("token"),
                result: report(body),
            });
        };
        succeed(&mut engine, "plan", "pane-plan", r#"{"plan":"do it"}"#);
        succeed(&mut engine, "left", "pane-left", r#"{"report":"left"}"#);
        assert_eq!(status_of(&engine, "join"), NodeStatus::Pending);

        engine.apply(EngineInput::RestartNode {
            path: InstancePath::new("left"),
        });
        assert_eq!(status_of(&engine, "left"), NodeStatus::Ready);
        let left_to_join = engine
            .graph()
            .and_then(|graph| graph.edges.iter().find(|edge| edge.to == RunNodeIdx(3)))
            .expect("the fan-in edge exists");
        assert!(
            !left_to_join.fired && left_to_join.condition_result.is_none(),
            "a restarted source leaves its outbound edge unresolved"
        );

        succeed(&mut engine, "right", "pane-right", r#"{"report":"right"}"#);
        assert_eq!(
            status_of(&engine, "join"),
            NodeStatus::Pending,
            "the fan-in waits for the branch that is running again"
        );
        assert!(!engine.admissions().contains(&RunNodeIdx(3)));

        succeed(&mut engine, "left", "pane-left-2", r#"{"report":"left"}"#);
        assert_eq!(status_of(&engine, "join"), NodeStatus::Ready);
    }

    /// The re-prompt budget is per reported-result cycle (§4.3). A node whose
    /// pane died is retried in a fresh pane, so it starts that attempt with its
    /// one corrective re-prompt intact.
    #[test]
    fn a_retry_after_a_pane_exit_gets_its_corrective_reprompt_back() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));
        engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: report(r#"{"notes":"oops"}"#),
        });

        engine.apply(EngineInput::PaneExited {
            pane: PublicPaneId::new("pane-1"),
            code: Some(1),
        });
        assert_eq!(status_of(&engine, "plan"), NodeStatus::Ready);
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1b"));

        let effects = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: report(r#"{"notes":"oops again"}"#),
        });
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, RunEffect::PromptNode { .. }))
                .count(),
            1,
            "the fresh attempt still gets its one corrective re-prompt"
        );
        assert_eq!(status_of(&engine, "plan"), NodeStatus::Running);
    }

    /// A correction that had nowhere to go was never delivered, so it must not
    /// count against the node's single re-prompt.
    #[test]
    fn a_reprompt_with_no_pane_does_not_spend_the_correction() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        // No `bind_node`: the node is admitted but holds no pane.
        let dropped = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: report(r#"{"notes":"oops"}"#),
        });
        assert!(
            !dropped
                .iter()
                .any(|effect| matches!(effect, RunEffect::PromptNode { .. })),
            "there is no pane to prompt"
        );

        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));
        let effects = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: report(r#"{"notes":"still oops"}"#),
        });
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, RunEffect::PromptNode { .. }))
                .count(),
            1,
            "the undeliverable correction did not burn the budget"
        );
        assert_eq!(status_of(&engine, "plan"), NodeStatus::Running);
    }

    /// A node the runtime could not put in a pane has no `PaneExited` to report.
    /// Without a status of its own it would sit `Ready` forever, leaving the run
    /// `Running` behind a node nothing will ever start.
    #[test]
    fn a_spawn_that_never_happened_takes_the_node_out_of_the_ready_set() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });

        engine.apply(EngineInput::SpawnFailed {
            path: InstancePath::new("plan"),
            reason: "no workspace to host the node's pane".to_string(),
        });
        assert_eq!(status_of(&engine, "plan"), NodeStatus::NeedsAttention);
        assert!(engine.admissions().is_empty());
        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Paused),
            "the run stalls with a surfaced reason instead of running forever"
        );
    }

    /// The canonical `needs_attention` trigger: a run started with no workspace
    /// to host the node's pane. `workflows.mdx` promises `run show` and
    /// `node show` name the blocker reason *and* the resume condition, and both
    /// renderers read `Succession::Blocked` — so the engine has to record one.
    #[test]
    fn a_spawn_that_never_happened_records_a_blocker_reason_and_resume_condition() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });

        engine.apply(EngineInput::SpawnFailed {
            path: InstancePath::new("plan"),
            reason: "no workspace to host the node's pane".to_string(),
        });

        assert_eq!(status_of(&engine, "plan"), NodeStatus::NeedsAttention);
        let Some(Succession::Blocked {
            reason,
            resume_when,
        }) = succession_of(&engine, "plan")
        else {
            panic!("a node that needs attention carries a structured blocker");
        };
        assert!(
            reason.contains("no workspace to host the node's pane"),
            "the blocker names why the node is stuck: {reason}"
        );
        assert!(
            resume_when.contains("kvx workspace create")
                && resume_when.contains("kvx workflow node restart"),
            "the resume condition names the commands that unstick the run: {resume_when}"
        );
    }

    /// A result that is still invalid after its one corrective re-prompt is the
    /// other blocker path a user meets, and it must be as actionable as the
    /// spawn failure — the fix is wired generically, not only for spawns.
    #[test]
    fn a_result_that_still_fails_its_schema_records_a_blocker() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

        for _ in 0..2 {
            engine.apply(EngineInput::NodeSelfReport {
                path: InstancePath::new("plan"),
                token: NodeToken::new("token"),
                result: report(r#"{"notes":"oops"}"#),
            });
        }

        assert_eq!(status_of(&engine, "plan"), NodeStatus::NeedsAttention);
        let Some(Succession::Blocked {
            reason,
            resume_when,
        }) = succession_of(&engine, "plan")
        else {
            panic!("a schema-blocked node carries a structured blocker");
        };
        assert!(
            reason.contains("output schema") || reason.contains("result.json"),
            "the blocker names the failing artifact: {reason}"
        );
        assert!(
            resume_when.contains("kvx workflow node"),
            "the resume condition names a command the user can run: {resume_when}"
        );
    }

    /// The other half of recording a blocker: shedding it. `NeedsAttention` is
    /// not terminal, and the resume condition the blocker itself prints — steer
    /// the node, let it write a valid `result.json` — brings the node back
    /// through `report`/`succeed` *without* a restart. `resolve_succession`
    /// returns any explicitly recorded succession verbatim, so a blocker left
    /// behind becomes the succeeded node's permanent succession: `node show`
    /// keeps printing a `resume:` line for work that is already done, and the
    /// wire's `succession` reads `blocked` on a node that succeeded.
    #[test]
    fn a_node_that_recovers_from_needing_attention_sheds_its_blocker() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

        // Two invalid results: one corrective re-prompt, then the blocker.
        for _ in 0..2 {
            engine.apply(EngineInput::NodeSelfReport {
                path: InstancePath::new("plan"),
                token: NodeToken::new("token"),
                result: report(r#"{"notes":"oops"}"#),
            });
        }
        assert!(
            matches!(
                succession_of(&engine, "plan"),
                Some(Succession::Blocked { .. })
            ),
            "the blocked node is the precondition for this test"
        );

        // The documented way out, taken without a restart.
        let effects = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: report(r#"{"plan":"ship it"}"#),
        });

        assert_eq!(status_of(&engine, "plan"), NodeStatus::Succeeded);
        assert_eq!(
            succession_of(&engine, "plan"),
            Some(Succession::Satisfied),
            "a node that succeeded records why it succeeded, not the obstruction it cleared"
        );
        assert_eq!(
            persisted_succession(&effects, "plan"),
            Some(Succession::Satisfied),
            "and the durable projection must not keep printing a resume condition for \
             work that is already done"
        );
    }

    /// The hand-off that makes the blocker durable: the same `record_status`
    /// call that flips the node to `NeedsAttention` has to carry the blocker
    /// into `StoreWrite::RunNode`, or the durable projection reads back `None`
    /// after a server restart and `node show` goes quiet again.
    #[test]
    fn a_needs_attention_blocker_survives_a_store_round_trip() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });

        let effects = engine.apply(EngineInput::SpawnFailed {
            path: InstancePath::new("plan"),
            reason: "no workspace to host the node's pane".to_string(),
        });

        let persisted = persisted_succession(&effects, "plan");
        assert!(
            matches!(persisted, Some(Succession::Blocked { .. })),
            "the persisted node record carries the blocker: {persisted:?}"
        );
    }

    /// A restart is the documented way out of a blocker, so the blocker must not
    /// outlive the attempt it described. This already held before the blocker
    /// was recorded at all; it is pinned so a later refactor cannot leave a
    /// stale `resume:` line on a node that is running again.
    #[test]
    fn restarting_a_node_clears_its_blocker() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.apply(EngineInput::SpawnFailed {
            path: InstancePath::new("plan"),
            reason: "no workspace to host the node's pane".to_string(),
        });

        engine.apply(EngineInput::RestartNode {
            path: InstancePath::new("plan"),
        });

        assert_eq!(status_of(&engine, "plan"), NodeStatus::Ready);
        assert_eq!(
            succession_of(&engine, "plan"),
            None,
            "a fresh attempt sheds the previous attempt's blocker"
        );
    }

    #[test]
    fn cancel_never_overwrites_a_finished_run() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        for (path, result) in [
            ("plan", r#"{"plan":"do it"}"#),
            ("implement", r#"{"report":"shipped"}"#),
        ] {
            engine.bind_node(&InstancePath::new(path), binding(path));
            engine.apply(EngineInput::NodeSelfReport {
                path: InstancePath::new(path),
                token: NodeToken::new("token"),
                result: report(result),
            });
        }
        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Succeeded)
        );

        // Phase 3: the user graph is closed, but the epilogue appended by
        // `finish` is still pending, and cancelling has to reach it — otherwise
        // a summariser keeps working on a run the user just asked to stop
        // (`07-phase3-plan.md` §4 D1). What cancel must *never* do is move the
        // run's status, which is what this test is about.
        let effects = engine.apply(EngineInput::CancelRun);
        assert!(
            effects.iter().all(|effect| !matches!(
                effect,
                RunEffect::Emit(WorkflowEvent::RunFinished { .. })
            )),
            "cancelling the epilogue re-decides nothing about the run: {effects:?}"
        );
        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Succeeded)
        );
        assert_eq!(
            engine
                .graph()
                .and_then(|graph| graph.epilogue)
                .map(|state| state.phase),
            Some(EpiloguePhase::GaveUp)
        );

        // With the epilogue resolved there is nothing left for a cancel to do.
        assert!(engine.apply(EngineInput::CancelRun).is_empty());
        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Succeeded)
        );
        assert!(
            engine
                .apply(EngineInput::NodeSelfReport {
                    path: InstancePath::new("plan"),
                    token: NodeToken::new("token"),
                    result: report(r#"{"plan":"again"}"#),
                })
                .is_empty(),
            "a duplicate report never re-checkpoints a closed node"
        );
    }

    #[test]
    fn cancel_closes_every_live_pane_and_records_a_succession() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

        let effects = engine.apply(EngineInput::CancelRun);
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, RunEffect::ClosePane { .. })));
        assert_eq!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Cancelled)
        );
        for path in ["plan", "implement"] {
            let node = engine
                .graph()
                .and_then(|graph| graph.node_by_path(&InstancePath::new(path)))
                .expect("the node exists");
            assert_eq!(node.status, NodeStatus::Cancelled);
            assert!(node.succession.is_some());
        }
    }

    #[test]
    fn progress_is_folded_into_the_node_without_touching_its_status() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

        engine.apply(EngineInput::ProgressObserved {
            path: InstancePath::new("plan"),
            delta: ProgressDelta {
                tool_calls: 2,
                tokens: 400,
                artifact_changes: 1,
                screen_digest: Some("abc".to_string()),
            },
        });
        let node = engine
            .graph()
            .and_then(|graph| graph.node_by_path(&InstancePath::new("plan")))
            .expect("the node exists");
        assert_eq!(node.progress.tool_calls, 2);
        assert_eq!(node.progress.tokens, 400);
        assert_eq!(node.usage.total_tokens, 400);
        assert_eq!(node.progress.last_screen_digest.as_deref(), Some("abc"));
        assert_eq!(node.status, NodeStatus::Running);
    }

    #[test]
    fn without_a_definition_no_report_can_complete_a_node() {
        let definition = kvdag_of(vec![spec_node(&TestNode::new("only"))], Vec::new());
        let graph = RunGraph::materialise(&definition, RunId::new("workflow_run:1"), Tier::High);
        let mut engine = Engine::new(EngineConfig::default());
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("only"), binding("pane-1"));

        engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("only"),
            token: NodeToken::new("token"),
            result: report("{}"),
        });
        assert_eq!(status_of(&engine, "only"), NodeStatus::NeedsAttention);
        assert_ne!(
            engine.graph().map(|graph| graph.status),
            Some(RunStatus::Succeeded)
        );
    }

    /// The two sequences are deliberately separate: `run_event`'s unique index
    /// is `(run, seq)` and `node_checkpoint`'s is `(run_node, seq)`
    /// (`03-storage-schema.md` §4.3). Sharing one counter would still satisfy
    /// both indexes while making a node's first checkpoint an arbitrary number.
    #[test]
    fn journal_sequence_numbers_are_contiguous_and_monotonic() {
        let (mut engine, graph) = two_node_engine();
        let mut effects = engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        effects.extend(engine.bind_node(&InstancePath::new("plan"), binding("pane-1")));
        effects.extend(engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: report(r#"{"plan":"do it"}"#),
        }));

        let mut journal = 0;
        let mut checkpoints: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        for effect in &effects {
            let RunEffect::Persist(write) = effect else {
                continue;
            };
            match write.as_ref() {
                StoreWrite::RunEvent { seq, .. } => {
                    journal += 1;
                    assert_eq!(*seq, journal, "the run journal cursor is contiguous");
                }
                StoreWrite::Checkpoint { path, seq, .. } => {
                    let next = checkpoints.entry(path.to_string()).or_default();
                    *next += 1;
                    assert_eq!(
                        *seq, *next,
                        "checkpoint seq is per run node and starts at 1"
                    );
                }
                _ => continue,
            }
        }
        assert!(journal > 0);
        assert_eq!(checkpoints.get("plan").copied(), Some(1));
    }

    /// 2.12: a `runner = "command"` node wrote a result that does not validate,
    /// called `kvx workflow node complete`, and got a `workflow_node_reported`
    /// success envelope back while the node sat `Running` forever. The gate's
    /// verdict existed only as a status change, so the caller had nothing to
    /// answer with. It is recorded now — and the errors it carries are what the
    /// node's own process is told.
    #[test]
    fn the_completion_gate_records_a_verdict_the_report_can_be_answered_with() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));
        let path = InstancePath::new("plan");
        assert!(
            engine.report_outcome(&path).is_none(),
            "a node that has not reported has no verdict"
        );

        engine.apply(EngineInput::NodeSelfReport {
            path: path.clone(),
            token: NodeToken::new("token"),
            result: report(r#"{"wrong_field":123}"#),
        });
        let first = engine
            .report_outcome(&path)
            .expect("the first report leaves a verdict");
        assert_eq!(first.verdict, ReportVerdict::Corrected);
        assert!(!first.accepted());
        assert_eq!(
            first.errors,
            vec!["missing required field \"plan\"".to_string()],
            "the verdict carries the violations the node has to fix"
        );
        assert_eq!(status_of(&engine, "plan"), NodeStatus::Running);

        engine.apply(EngineInput::NodeSelfReport {
            path: path.clone(),
            token: NodeToken::new("token"),
            result: report(r#"{"wrong_field":456}"#),
        });
        let second = engine
            .report_outcome(&path)
            .expect("the second report replaces the verdict");
        assert_eq!(second.verdict, ReportVerdict::Surfaced);
        assert!(
            !second.errors.is_empty(),
            "the second rejection still quotes the schema violations: {second:?}"
        );
        assert_eq!(status_of(&engine, "plan"), NodeStatus::NeedsAttention);

        engine.apply(EngineInput::RestartNode { path: path.clone() });
        engine.bind_node(&path, binding("pane-1b"));
        engine.apply(EngineInput::NodeSelfReport {
            path: path.clone(),
            token: NodeToken::new("token"),
            result: report(r#"{"plan":"do it"}"#),
        });
        let accepted = engine
            .report_outcome(&path)
            .expect("a valid result leaves a verdict too");
        assert!(accepted.accepted());
        assert!(
            accepted.errors.is_empty(),
            "an accepted result carries no violations"
        );
    }

    /// 2.12: a self-report that carries no artifact is not a *result* the gate
    /// refused — there was nothing to validate. The node's status already says
    /// what is wrong, so the verdict must carry no schema violations and the
    /// report must not be answered as an invalid result.
    #[test]
    fn a_report_with_no_artifact_records_no_schema_violations() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

        engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("plan"),
            token: NodeToken::new("token"),
            result: RawJson(serde_json::Value::Null),
        });
        let outcome = engine
            .report_outcome(&InstancePath::new("plan"))
            .expect("the report leaves a verdict");
        assert_eq!(outcome.verdict, ReportVerdict::Surfaced);
        assert!(outcome.errors.is_empty(), "unexpected: {outcome:?}");
        assert_eq!(status_of(&engine, "plan"), NodeStatus::NeedsAttention);
    }

    /// 2.13: after `run cancel`, restarting a node succeeded — the run reported
    /// `cancelled` while the node reported `running`, in a fresh pane nothing
    /// would ever collect a result from. A closed run never settles again, so
    /// the restart is refused instead.
    #[test]
    fn a_closed_run_never_restarts_a_node() {
        for close in [Closer::Cancel, Closer::Succeed] {
            let (mut engine, graph) = two_node_engine();
            engine.apply(EngineInput::Start {
                graph: Box::new(graph),
            });
            engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

            let closed_status = match close {
                Closer::Cancel => {
                    engine.apply(EngineInput::CancelRun);
                    RunStatus::Cancelled
                }
                Closer::Succeed => {
                    for (path, result) in [
                        ("plan", r#"{"plan":"do it"}"#),
                        ("implement", r#"{"report":"shipped"}"#),
                    ] {
                        engine.bind_node(&InstancePath::new(path), binding(path));
                        engine.apply(EngineInput::NodeSelfReport {
                            path: InstancePath::new(path),
                            token: NodeToken::new("token"),
                            result: report(result),
                        });
                    }
                    RunStatus::Succeeded
                }
            };
            assert_eq!(engine.run_status(), Some(closed_status));
            let before = status_of(&engine, "plan");

            let effects = engine.apply(EngineInput::RestartNode {
                path: InstancePath::new("plan"),
            });

            assert!(
                effects.is_empty(),
                "a refused restart emits nothing at all — no ClosePane, no spawn, \
                 no journal entry: {effects:?}"
            );
            assert_eq!(
                status_of(&engine, "plan"),
                before,
                "the node keeps the status the closed run left it with"
            );
            assert_eq!(
                engine.run_status(),
                Some(closed_status),
                "a closed run stays closed"
            );
            // Phase 3 admits exactly one thing into a closed run — the
            // engine-owned `.summary` node, which only exists *because* the run
            // closed (§3 rule 2). No node the user authored comes back.
            let readmitted: Vec<InstancePath> = engine
                .admissions()
                .into_iter()
                .filter_map(|idx| engine.graph().and_then(|graph| graph.node(idx)))
                .map(|node| node.path.clone())
                .filter(|path| !is_reserved_path(path.as_str()))
                .collect();
            assert!(
                readmitted.is_empty(),
                "no authored node is admitted back into a closed run: {readmitted:?}"
            );
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum Closer {
        Cancel,
        Succeed,
    }

    /// 2.15: a steer the runtime refused produced a single server-side WARN and
    /// nothing else, so the user was left believing it had been delivered. The
    /// refusal is handed back to the engine, which journals it, marks the node,
    /// and raises a notice a client can render.
    #[test]
    fn a_refused_delivery_is_journalled_marked_and_surfaced() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));
        let path = InstancePath::new("plan");

        engine.apply(EngineInput::Steer {
            path: path.clone(),
            text: "please rerun".to_string(),
        });
        assert!(
            engine.delivery_failure(&path).is_none(),
            "a steer that has not failed leaves no marker"
        );

        let effects =
            engine.note_delivery_failure(&path, "pane.send_text", "pane_not_found: no such pane");

        let note = engine
            .delivery_failure(&path)
            .expect("the refusal is surfaced on the node, not only logged");
        assert_eq!(note.method, "pane.send_text");
        assert!(note.reason.contains("pane_not_found"));
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                RunEffect::Persist(write)
                    if matches!(**write, StoreWrite::RunEvent { kind: RunEventKind::Error, .. })
            )),
            "the refusal is a run event, so it survives in the journal: {effects:?}"
        );
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                RunEffect::Emit(WorkflowEvent::NodeUpdated { path: updated, .. })
                    if updated == &path
            )),
            "clients are told to re-read the node: {effects:?}"
        );
        let message = effects
            .iter()
            .find_map(|effect| match effect {
                RunEffect::Notify(notice) => Some(notice.clone()),
                _ => None,
            })
            .expect("the user is told the delivery did not happen");
        assert_eq!(message.level, NoticeLevel::Error);
        assert_eq!(message.path, Some(path.clone()));
        assert!(
            message.message.contains("pane.send_text") && message.message.contains("not delivered"),
            "unexpected notice: {}",
            message.message
        );

        // The marker answers for the latest attempt only: a fresh steer, a fresh
        // pane, or a restart all clear it.
        engine.apply(EngineInput::Steer {
            path: path.clone(),
            text: "again".to_string(),
        });
        assert!(engine.delivery_failure(&path).is_none());
        engine.note_delivery_failure(&path, "pane.send_text", "pane_not_found: no such pane");
        engine.bind_node(&path, binding("pane-2"));
        assert!(engine.delivery_failure(&path).is_none());
    }

    /// The other half of 2.15: a node with no pane at all never produces a
    /// delivery for the runtime to refuse, so nothing downstream could report
    /// one. The engine records it directly instead of emitting silence.
    #[test]
    fn steering_a_node_with_no_pane_is_surfaced_rather_than_dropped() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        let path = InstancePath::new("plan");

        let effects = engine.apply(EngineInput::Steer {
            path: path.clone(),
            text: "please rerun".to_string(),
        });
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, RunEffect::PromptNode { .. })),
            "there is no pane to prompt"
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, RunEffect::Notify(_))),
            "the caller is told the steer was not delivered: {effects:?}"
        );
        assert!(
            engine
                .delivery_failure(&path)
                .is_some_and(|note| note.reason.contains("no pane")),
            "the node carries the marker a reader can see"
        );

        let effects = engine.apply(EngineInput::Interrupt { path: path.clone() });
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, RunEffect::Notify(_))),
            "an interrupt with nowhere to go is surfaced the same way: {effects:?}"
        );
    }

    /// §4 D6 / §0.5: the stored `transcript_path` is a pre-launch *estimate*
    /// derived from `(claude_dir, slug(cwd), session_id)` and was never
    /// corrected, so a historical interrogation could answer
    /// `transcript_unavailable` for a session whose transcript exists at the
    /// path the hook actually reported.
    #[test]
    fn a_reported_transcript_path_replaces_the_estimate_and_touches_nothing_else() {
        let mut engine = started_engine();
        let path = InstancePath::new("plan");

        // Close the node first: a session report can arrive after the node
        // finished, and the stored path is what a later interrogation stats.
        report_plan(&mut engine, r#"{"plan":"done"}"#);
        let before = node_of(&engine, "plan").clone();
        assert_eq!(before.status, NodeStatus::Succeeded);

        let reported = std::path::PathBuf::from("/home/u/.claude/projects/real/abc.jsonl");
        assert!(
            engine.record_transcript_path(&path, reported.clone()),
            "the reported path differs from the estimate, so it is taken"
        );

        let after = node_of(&engine, "plan").clone();
        assert_eq!(
            after.binding.as_ref().map(|b| b.transcript_path.clone()),
            Some(reported.clone())
        );
        // Exactly one field moved. Reconstructing `before` with only the
        // transcript path swapped must reproduce the node byte for byte, which
        // catches any status change, stamp, or cleared field the mutator might
        // have touched on the way past.
        let mut expected = before.clone();
        if let Some(binding) = expected.binding.as_mut() {
            binding.transcript_path = reported.clone();
        }
        assert_eq!(after, expected, "only transcript_path may change");
        // The *run* is still `Running` — only `plan` closed, `implement` has
        // not — and learning a transcript path must not move it either.
        assert_eq!(engine.run_status(), Some(RunStatus::Running));

        // Idempotent: the same path again is not a change, so the caller skips
        // the durable write.
        assert!(
            !engine.record_transcript_path(&path, reported),
            "recording the path already stored reports no change"
        );
        // And an unknown node, or one that never bound a pane, is a no-op
        // rather than a panic.
        assert!(!engine.record_transcript_path(
            &InstancePath::new("nonesuch"),
            std::path::PathBuf::from("/tmp/x.jsonl")
        ));
        assert!(!engine.record_transcript_path(
            &InstancePath::new("implement"),
            std::path::PathBuf::from("/tmp/y.jsonl")
        ));
    }

    /// The §0.5 case, pinned on its own: the report that corrects a node's
    /// transcript path can arrive **after** that node has closed, and that is
    /// precisely when it matters. A historical interrogation stats the stored
    /// path (§4 D6's stat-first rule), so refusing a late correction would leave
    /// the wrong estimate in place for the one caller that reads it.
    #[test]
    fn a_late_session_report_corrects_a_closed_nodes_transcript_path() {
        let mut engine = started_engine();
        report_plan(&mut engine, r#"{"plan":"done"}"#);
        assert!(
            status_of(&engine, "plan").is_terminal(),
            "the node is closed before the report arrives"
        );

        let reported = std::path::PathBuf::from("/home/u/.claude/projects/real/late.jsonl");
        assert!(
            engine.record_transcript_path(&InstancePath::new("plan"), reported.clone()),
            "a closed node still accepts the corrected path"
        );
        assert_eq!(
            node_of(&engine, "plan")
                .binding
                .as_ref()
                .map(|binding| binding.transcript_path.clone()),
            Some(reported)
        );
        assert!(
            status_of(&engine, "plan").is_terminal(),
            "and recording it did not reopen the node"
        );
    }

    /// The durable half: the caller persists the corrected path through an
    /// ordinary `RunNode` update carrying the node's current state, with no
    /// status journal and no `NodeUpdated` emit — nothing transitioned.
    #[test]
    fn the_persist_effect_carries_the_corrected_path_and_journals_nothing() {
        let mut engine = started_engine();
        let path = InstancePath::new("plan");
        let reported = std::path::PathBuf::from("/home/u/.claude/projects/real/abc.jsonl");
        assert!(engine.record_transcript_path(&path, reported.clone()));

        let effect = engine
            .node_persist_effect(&path)
            .expect("a bound node has a durable counterpart");
        match &effect {
            RunEffect::Persist(write) => match write.as_ref() {
                StoreWrite::RunNode {
                    path: written,
                    binding,
                    status,
                    ..
                } => {
                    assert_eq!(written, &path);
                    assert_eq!(
                        binding.as_ref().map(|b| b.transcript_path.clone()),
                        Some(reported),
                        "the durable row keeps pace with the live copy"
                    );
                    assert_eq!(*status, NodeStatus::Running, "no status was invented");
                }
                other => panic!("expected a RunNode write, got {other:?}"),
            },
            other => panic!("expected a Persist effect, got {other:?}"),
        }

        assert!(engine
            .node_persist_effect(&InstancePath::new("nonesuch"))
            .is_none());
    }

    // ── the end-of-run epilogue (`07-phase3-plan.md` §4 D1) ────────────────

    /// Runs the two-node fixture to `Succeeded`, leaving the epilogue appended
    /// and waiting to be spawned.
    fn engine_at_end_of_run() -> Engine {
        let mut engine = started_engine();
        report_plan(&mut engine, r#"{"plan":"done"}"#);
        engine.bind_node(&InstancePath::new("implement"), binding("pane-2"));
        engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("implement"),
            token: NodeToken::new("token"),
            result: report(r#"{"report":"done"}"#),
        });
        engine
    }

    fn epilogue_phase(engine: &Engine) -> Option<EpiloguePhase> {
        engine
            .graph()
            .and_then(|graph| graph.epilogue)
            .map(|state| state.phase)
    }

    /// Binds the summariser to a pane the way the app would once `admissions`
    /// has yielded it.
    fn spawn_summariser(engine: &mut Engine) -> Vec<RunEffect> {
        engine.bind_node(&InstancePath::new(SUMMARY_INSTANCE_PATH), binding("pane-s"))
    }

    fn valid_summary(text: &str) -> String {
        json!({
            "text": text,
            "outcome": "the run succeeded",
            "highlights": ["it worked"],
            "open_gaps": [],
            "per_node": [{ "node_key": "plan", "verdict": "ok", "one_liner": "planned" }],
        })
        .to_string()
    }

    fn run_finished_count(effects: &[RunEffect]) -> usize {
        effects
            .iter()
            .filter(|effect| matches!(effect, RunEffect::Emit(WorkflowEvent::RunFinished { .. })))
            .count()
    }

    #[test]
    fn a_finished_run_appends_exactly_one_summariser_and_keeps_its_status() {
        let engine = engine_at_end_of_run();

        assert_eq!(engine.run_status(), Some(RunStatus::Succeeded));
        assert_eq!(epilogue_phase(&engine), Some(EpiloguePhase::Pending));
        let graph = engine.graph().expect("a graph");
        let epilogue: Vec<&RunNode> = graph
            .nodes
            .iter()
            .filter(|node| is_reserved_path(node.path.as_str()))
            .collect();
        assert_eq!(epilogue.len(), 1, "exactly one engine-owned node");
        assert_eq!(epilogue[0].path, InstancePath::new(SUMMARY_INSTANCE_PATH));
        assert_eq!(epilogue[0].status, NodeStatus::Ready);
        assert_eq!(epilogue[0].label, "summary");

        // §4 D5: the counters mean "the run's declared work", and the epilogue
        // is not part of it. Every user-facing count filters on the same
        // predicate the store's counter refresh does.
        assert_eq!(
            graph
                .nodes
                .iter()
                .filter(|node| !is_reserved_path(node.path.as_str()))
                .count(),
            2,
            "the summariser is not one of the run's declared nodes"
        );
    }

    /// §3 rule 2: `admissions` is a per-node question, so the summariser is
    /// admitted even though the run's status is already terminal. Without this
    /// the epilogue would never be spawned and `epilogue_pending` would stay
    /// true forever.
    #[test]
    fn the_post_finish_summariser_reaches_admissions() {
        let engine = engine_at_end_of_run();
        let admitted: Vec<&str> = engine
            .admissions()
            .into_iter()
            .filter_map(|idx| engine.graph().and_then(|graph| graph.node(idx)))
            .map(|node| node.path.as_str())
            .collect();
        assert_eq!(admitted, vec![SUMMARY_INSTANCE_PATH]);
        assert!(engine.epilogue_pending());
    }

    /// R-1: `finish` is never re-entered on the epilogue's account. A finished
    /// run keeps ticking while the summariser works, and not one of those ticks
    /// may emit a second `RunFinished` or move the run's status.
    #[test]
    fn a_pending_epilogue_never_re_enters_finish() {
        let mut engine = engine_at_end_of_run();
        spawn_summariser(&mut engine);

        for _ in 0..5 {
            let effects = engine.apply(EngineInput::Tick {
                now: Instant::now(),
            });
            assert_eq!(
                run_finished_count(&effects),
                0,
                "a tick during the epilogue re-decides nothing"
            );
            assert_eq!(engine.run_status(), Some(RunStatus::Succeeded));
        }

        let effects = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new(SUMMARY_INSTANCE_PATH),
            token: NodeToken::new("token"),
            result: report(&valid_summary("the run planned and implemented")),
        });
        assert_eq!(
            run_finished_count(&effects),
            0,
            "accepting the summary does not finish the run a second time"
        );
        assert_eq!(engine.run_status(), Some(RunStatus::Succeeded));
        assert_eq!(epilogue_phase(&engine), Some(EpiloguePhase::Done));
        assert!(!engine.epilogue_pending());
    }

    #[test]
    fn an_accepted_summary_writes_the_row_journals_it_and_closes_the_pane() {
        let mut engine = engine_at_end_of_run();
        spawn_summariser(&mut engine);
        assert_eq!(epilogue_phase(&engine), Some(EpiloguePhase::Running));

        let effects = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new(SUMMARY_INSTANCE_PATH),
            token: NodeToken::new("token"),
            result: report(&valid_summary("plan then implement; both landed")),
        });

        let summary = effects
            .iter()
            .find_map(|effect| match effect {
                RunEffect::Persist(write) => match write.as_ref() {
                    StoreWrite::RunSummary {
                        text,
                        outcome,
                        highlights,
                        per_node,
                        token_estimate,
                        generated_by_path,
                        ..
                    } => Some((
                        text.clone(),
                        outcome.clone(),
                        highlights.clone(),
                        per_node.clone(),
                        *token_estimate,
                        generated_by_path.clone(),
                    )),
                    _ => None,
                },
                _ => None,
            })
            .expect("an accepted summary is persisted");
        assert_eq!(summary.0, "plan then implement; both landed");
        assert_eq!(summary.1, "the run succeeded");
        assert_eq!(summary.2, vec!["it worked".to_string()]);
        assert_eq!(summary.3.len(), 1);
        assert_eq!(summary.3[0].node_key, "plan");
        assert!(summary.4 > 0, "the token estimate is filled in");
        assert_eq!(
            summary.5,
            Some(InstancePath::new(SUMMARY_INSTANCE_PATH)),
            "the summary names the node that produced it"
        );

        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                RunEffect::Persist(write)
                    if matches!(write.as_ref(), StoreWrite::RunEvent { kind: RunEventKind::Summary, .. })
            )),
            "the journal gets its `summary` entry: {effects:?}"
        );
        assert!(
            effects.iter().any(|effect| matches!(
                effect,
                RunEffect::Emit(WorkflowEvent::RunSummarized { .. })
            )),
            "the app is told to re-read and publish the summary"
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, RunEffect::ClosePane { pane } if pane.as_str() == "pane-s")),
            "the summariser's pane does not outlive the run"
        );
    }

    /// The budget is a property of the contract, not a request in the prompt:
    /// an over-budget `text` fails `maxLength`, spends the one corrective
    /// re-prompt, and then the ladder ends — without touching the run.
    #[test]
    fn an_over_budget_summary_reprompts_once_then_gives_up_without_touching_the_run() {
        let mut engine = engine_at_end_of_run();
        spawn_summariser(&mut engine);
        let over_budget = valid_summary(&"x".repeat(SUMMARY_TEXT_BUDGET + 1));

        let first = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new(SUMMARY_INSTANCE_PATH),
            token: NodeToken::new("token"),
            result: report(&over_budget),
        });
        assert!(
            first
                .iter()
                .any(|effect| matches!(effect, RunEffect::PromptNode { .. })),
            "the single corrective re-prompt is delivered: {first:?}"
        );
        assert_eq!(epilogue_phase(&engine), Some(EpiloguePhase::Running));
        assert_eq!(engine.run_status(), Some(RunStatus::Succeeded));

        let second = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new(SUMMARY_INSTANCE_PATH),
            token: NodeToken::new("token"),
            result: report(&over_budget),
        });
        assert_eq!(epilogue_phase(&engine), Some(EpiloguePhase::GaveUp));
        assert_eq!(
            engine.run_status(),
            Some(RunStatus::Succeeded),
            "a summariser that gave up never changes the run's outcome"
        );
        assert_eq!(run_finished_count(&second), 0);
        let journalled = second.iter().any(|effect| match effect {
            RunEffect::Persist(write) => match write.as_ref() {
                StoreWrite::RunEvent {
                    kind: RunEventKind::Error,
                    payload,
                    ..
                } => payload["reason"] == "summary_failed",
                _ => false,
            },
            _ => false,
        });
        assert!(journalled, "the give-up is journalled: {second:?}");
        assert_eq!(
            second
                .iter()
                .filter(|effect| matches!(effect, RunEffect::Notify(_)))
                .count(),
            1,
            "notified once, not once per failure signal"
        );
        assert!(second
            .iter()
            .any(|effect| matches!(effect, RunEffect::ClosePane { .. })));
        assert!(!engine.epilogue_pending());
    }

    /// A within-budget summary is accepted, so the `maxLength` gate is not
    /// rejecting everything.
    #[test]
    fn a_summary_at_the_budget_is_accepted() {
        let mut engine = engine_at_end_of_run();
        spawn_summariser(&mut engine);
        let effects = engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new(SUMMARY_INSTANCE_PATH),
            token: NodeToken::new("token"),
            result: report(&valid_summary(&"x".repeat(SUMMARY_TEXT_BUDGET))),
        });
        assert_eq!(epilogue_phase(&engine), Some(EpiloguePhase::Done));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            RunEffect::Persist(write) if matches!(write.as_ref(), StoreWrite::RunSummary { .. })
        )));
    }

    #[test]
    fn a_summariser_pane_that_dies_before_reporting_gives_up() {
        let mut engine = engine_at_end_of_run();
        spawn_summariser(&mut engine);

        let effects = engine.apply(EngineInput::PaneExited {
            pane: PublicPaneId::new("pane-s"),
            code: Some(1),
        });
        assert_eq!(epilogue_phase(&engine), Some(EpiloguePhase::GaveUp));
        assert_eq!(engine.run_status(), Some(RunStatus::Succeeded));
        assert_eq!(run_finished_count(&effects), 0);
        assert_eq!(
            status_of(&engine, SUMMARY_INSTANCE_PATH),
            NodeStatus::Failed,
            "the summariser is not retried into a fresh pane on a finished run"
        );
    }

    #[test]
    fn a_summariser_that_cannot_be_spawned_gives_up() {
        let mut engine = engine_at_end_of_run();
        let effects = engine.apply(EngineInput::SpawnFailed {
            path: InstancePath::new(SUMMARY_INSTANCE_PATH),
            reason: "no workspace".to_string(),
        });
        assert_eq!(epilogue_phase(&engine), Some(EpiloguePhase::GaveUp));
        assert_eq!(engine.run_status(), Some(RunStatus::Succeeded));
        assert_eq!(run_finished_count(&effects), 0);
    }

    #[test]
    fn a_summariser_self_report_with_no_artifact_gives_up() {
        let mut engine = engine_at_end_of_run();
        spawn_summariser(&mut engine);
        engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new(SUMMARY_INSTANCE_PATH),
            token: NodeToken::new("token"),
            result: RawJson(serde_json::Value::Null),
        });
        assert_eq!(epilogue_phase(&engine), Some(EpiloguePhase::GaveUp));
        assert_eq!(engine.run_status(), Some(RunStatus::Succeeded));
    }

    #[test]
    fn cancelling_mid_epilogue_closes_the_summariser_pane_and_gives_up() {
        let mut engine = engine_at_end_of_run();
        spawn_summariser(&mut engine);

        let effects = engine.apply(EngineInput::CancelRun);
        assert_eq!(epilogue_phase(&engine), Some(EpiloguePhase::GaveUp));
        assert_eq!(
            engine.run_status(),
            Some(RunStatus::Succeeded),
            "cancelling a summariser does not retroactively cancel a finished run"
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, RunEffect::ClosePane { pane } if pane.as_str() == "pane-s")),
            "the pane is closed: {effects:?}"
        );

        // Idempotent: a second cancel has nothing left to give up on.
        assert!(engine.apply(EngineInput::CancelRun).is_empty());
    }

    /// §4 D2: a cancelled run has no outcome worth summarising, and a
    /// summariser spawned after a cancel would be a pane the user just asked not
    /// to have.
    #[test]
    fn a_cancelled_run_appends_no_epilogue() {
        let mut engine = started_engine();
        engine.apply(EngineInput::CancelRun);

        assert_eq!(engine.run_status(), Some(RunStatus::Cancelled));
        assert!(engine.graph().and_then(|graph| graph.epilogue).is_none());
        assert!(!engine.epilogue_pending());
        assert!(engine
            .graph()
            .expect("a graph")
            .nodes
            .iter()
            .all(|node| !is_reserved_path(node.path.as_str())));
    }

    /// A failed run gets a summary too — that is when the history is worth the
    /// most.
    #[test]
    fn a_failed_run_still_gets_a_summariser() {
        let mut engine = started_engine();
        report_plan(&mut engine, r#"{"plan":"done"}"#);

        // The leaf's pane dies through both of its attempts, so it fails with
        // every other node already terminal — which is what makes the *run*
        // fail rather than pause on an outstanding node.
        engine.bind_node(&InstancePath::new("implement"), binding("pane-2"));
        engine.apply(EngineInput::PaneExited {
            pane: PublicPaneId::new("pane-2"),
            code: Some(1),
        });
        engine.bind_node(&InstancePath::new("implement"), binding("pane-3"));
        engine.apply(EngineInput::PaneExited {
            pane: PublicPaneId::new("pane-3"),
            code: Some(1),
        });

        assert_eq!(engine.run_status(), Some(RunStatus::Failed));
        assert_eq!(
            epilogue_phase(&engine),
            Some(EpiloguePhase::Pending),
            "a failed run is exactly when its history is worth the most"
        );
    }

    /// Runs the two-node fixture to `Succeeded` under a given engine config, so
    /// the epilogue's binding can be varied without duplicating the run.
    fn engine_at_end_of_run_with(config: EngineConfig) -> Engine {
        let (engine, graph) = two_node_engine();
        let mut engine = Engine { config, ..engine };
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));
        report_plan(&mut engine, r#"{"plan":"done"}"#);
        engine.bind_node(&InstancePath::new("implement"), binding("pane-2"));
        engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("implement"),
            token: NodeToken::new("token"),
            result: report(r#"{"report":"done"}"#),
        });
        engine
    }

    fn command_config() -> EngineConfig {
        EngineConfig {
            summary_command: Some(vec!["summarise.sh".to_string()]),
            ..EngineConfig::default()
        }
    }

    /// **Defect D-1.** The epilogue has no kvdag node, so `runner_of` used to
    /// fall through to its `Agent` default — which is a lie whenever
    /// `KARVEX_WORKFLOW_SUMMARY_COMMAND` binds the summariser to a script. It
    /// mattered because `Agent` makes sustained idle an admissible completion
    /// signal, and the first sustained idle on a node never seen working
    /// re-delivers the seed prompt through `PromptNode` — i.e. karvex would type
    /// the summariser's prompt into a **shell**.
    #[test]
    fn a_command_bound_summariser_never_takes_the_agent_only_idle_path() {
        let mut engine = engine_at_end_of_run_with(command_config());
        spawn_summariser(&mut engine);
        assert_eq!(
            engine
                .graph()
                .and_then(|graph| graph.epilogue)
                .map(|state| state.runner),
            Some(Runner::Command)
        );

        // Well past `SUSTAINED_IDLE_TICKS`, which for an agent-bound node would
        // have re-delivered the seed and then given up.
        let mut effects = Vec::new();
        for _ in 0..(complete::SUSTAINED_IDLE_TICKS * 3) {
            effects.extend(engine.apply(EngineInput::AgentStatus {
                pane: PublicPaneId::new("pane-s"),
                state: AgentState::Idle,
                at: Instant::now(),
            }));
        }

        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, RunEffect::PromptNode { .. })),
            "no seed prompt is ever typed into a command pane: {effects:?}"
        );
        assert_eq!(
            epilogue_phase(&engine),
            Some(EpiloguePhase::Running),
            "an inadmissible signal is ignored, not treated as a give-up trigger"
        );
        assert_eq!(engine.run_status(), Some(RunStatus::Succeeded));

        // Self-report is the one signal a command runner has, and it still
        // completes the epilogue normally.
        engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new(SUMMARY_INSTANCE_PATH),
            token: NodeToken::new("token"),
            result: report(&valid_summary("the command wrote a summary")),
        });
        assert_eq!(epilogue_phase(&engine), Some(EpiloguePhase::Done));
    }

    /// The other half of the ladder still holds for a command-bound summariser:
    /// a pane that dies before reporting gives up rather than hanging, so the
    /// override cannot strand `epilogue_pending` forever.
    #[test]
    fn a_command_bound_summariser_that_dies_before_reporting_still_gives_up() {
        let mut engine = engine_at_end_of_run_with(command_config());
        spawn_summariser(&mut engine);
        engine.apply(EngineInput::PaneExited {
            pane: PublicPaneId::new("pane-s"),
            code: Some(2),
        });

        assert_eq!(epilogue_phase(&engine), Some(EpiloguePhase::GaveUp));
        assert!(!engine.epilogue_pending());
        assert_eq!(engine.run_status(), Some(RunStatus::Succeeded));
    }

    /// Without the override the summariser is agent-bound, so the existing
    /// agent-only ladder is unchanged — including the seed re-delivery that is
    /// correct for a real `claude` pane.
    #[test]
    fn an_agent_bound_summariser_keeps_its_agent_semantics() {
        let mut engine = engine_at_end_of_run();
        spawn_summariser(&mut engine);
        assert_eq!(
            engine
                .graph()
                .and_then(|graph| graph.epilogue)
                .map(|state| state.runner),
            Some(Runner::Agent)
        );

        let mut effects = Vec::new();
        for _ in 0..complete::SUSTAINED_IDLE_TICKS {
            effects.extend(engine.apply(EngineInput::AgentStatus {
                pane: PublicPaneId::new("pane-s"),
                state: AgentState::Idle,
                at: Instant::now(),
            }));
        }
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, RunEffect::PromptNode { .. })),
            "an agent that never worked gets its one seed re-delivery: {effects:?}"
        );
    }

    /// The argv reaches the spawn plan through the engine, so the binder never
    /// re-reads the environment and the two can never disagree about what the
    /// summariser is (§3 item 2, as amended).
    #[test]
    fn the_task_spec_carries_the_override_argv() {
        let engine = engine_at_end_of_run_with(command_config());
        let spec = engine.summary_task_spec().expect("a run with an epilogue");
        assert_eq!(spec.command, Some(vec!["summarise.sh".to_string()]));

        let agent = engine_at_end_of_run();
        let spec = agent.summary_task_spec().expect("a run with an epilogue");
        assert_eq!(
            spec.command, None,
            "no override means the summariser runs as an agent"
        );
    }

    #[test]
    fn summaries_disabled_means_no_epilogue_node_at_all() {
        let (mut engine, graph) = two_node_engine();
        engine = Engine {
            config: EngineConfig {
                summary_enabled: false,
                ..EngineConfig::default()
            },
            ..engine
        };
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));
        report_plan(&mut engine, r#"{"plan":"done"}"#);
        engine.bind_node(&InstancePath::new("implement"), binding("pane-2"));
        engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("implement"),
            token: NodeToken::new("token"),
            result: report(r#"{"report":"done"}"#),
        });

        assert_eq!(engine.run_status(), Some(RunStatus::Succeeded));
        assert!(engine.graph().and_then(|graph| graph.epilogue).is_none());
        assert_eq!(engine.graph().expect("a graph").nodes.len(), 2);
    }

    /// §4 D14: `run_event.at` is the producer's fact. A single unstamped
    /// producer would put the store's flush-time clock back in the read path, so
    /// the assertion is over *every* journal write a whole run emits.
    #[test]
    fn every_journal_write_a_run_emits_is_stamped_by_its_producer() {
        let mut engine = started_engine();
        let mut effects = engine.apply(EngineInput::Steer {
            path: InstancePath::new("plan"),
            text: "keep going".to_string(),
        });
        effects.extend(report_plan(&mut engine, r#"{"nope":true}"#));
        effects.extend(report_plan(&mut engine, r#"{"plan":"done"}"#));
        effects.extend(engine.bind_node(&InstancePath::new("implement"), binding("pane-2")));
        effects.extend(engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new("implement"),
            token: NodeToken::new("token"),
            result: report(r#"{"report":"done"}"#),
        }));
        effects.extend(spawn_summariser(&mut engine));
        effects.extend(engine.apply(EngineInput::NodeSelfReport {
            path: InstancePath::new(SUMMARY_INSTANCE_PATH),
            token: NodeToken::new("token"),
            result: report(&valid_summary("it went fine")),
        }));

        let stamps: Vec<(RunEventKind, u64)> = effects
            .iter()
            .filter_map(|effect| match effect {
                RunEffect::Persist(write) => match write.as_ref() {
                    StoreWrite::RunEvent {
                        kind, at_unix_ms, ..
                    } => Some((*kind, *at_unix_ms)),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert!(
            stamps.len() > 10,
            "the run emitted a representative journal: {stamps:?}"
        );
        for (kind, at) in &stamps {
            assert!(
                *at > 0,
                "{kind:?} reached the store with no producer timestamp"
            );
        }
        assert!(
            stamps
                .iter()
                .any(|(kind, _)| *kind == RunEventKind::Summary),
            "the new `summary` kind is among them: {stamps:?}"
        );
    }

    #[test]
    fn the_summary_schema_is_a_valid_output_schema_and_enforces_the_budget() {
        let schema =
            OutputSchema::parse(summary_output_schema()).expect("the built-in schema is valid");
        assert!(complete::validate(&schema, &report(&valid_summary("short and sweet"))).is_ok());

        let errors = complete::validate(
            &schema,
            &report(&valid_summary(&"x".repeat(SUMMARY_TEXT_BUDGET + 1))),
        )
        .expect_err("an over-budget summary is invalid");
        assert!(
            errors
                .iter()
                .any(|violation| violation.at == "text" && violation.message.contains("maxLength")),
            "the violation names the field and the limit: {errors:?}"
        );
    }

    #[test]
    fn the_summary_task_states_the_budget_and_covers_only_the_user_nodes() {
        let engine = engine_at_end_of_run();
        let spec = engine.summary_task_spec().expect("a run with an epilogue");

        assert_eq!(spec.path, InstancePath::new(SUMMARY_INSTANCE_PATH));
        assert_eq!(spec.output_schema, summary_output_schema());
        assert!(
            spec.task_body.contains(&SUMMARY_TEXT_BUDGET.to_string()),
            "the prompt states the budget the schema enforces"
        );
        assert!(spec.task_body.contains("### `plan`"));
        assert!(spec.task_body.contains("### `implement`"));
        assert!(
            !spec.task_body.contains(SUMMARY_INSTANCE_PATH),
            "the summariser is not asked to summarise itself"
        );
        // The body is a body, not a document: it carries no title of its own,
        // because the binder's `TaskDocument` writes one — and it must not
        // restate the reporting contract, because a second copy is how the
        // epilogue's contract went missing in the first place.
        assert!(
            !spec.task_body.starts_with('#') || spec.task_body.starts_with("##"),
            "the body must not open with its own H1: {}",
            spec.task_body
        );
        assert!(
            !spec.task_body.contains("kvx workflow node complete"),
            "the reporting contract has exactly one author, and it is not this \
             function: {}",
            spec.task_body
        );
    }

    /// `05-phase-plan.md` W2: the engine must not reference `App`,
    /// `TerminalRuntime`, SurrealDB, or ratatui, and must stay synchronous.
    /// Only production code is scanned — the rule is about what the engine
    /// depends on, and fixtures are free to build whatever they need.
    #[test]
    fn the_engine_stays_pure() {
        let sources = [
            ("mod.rs", include_str!("mod.rs")),
            ("graph.rs", include_str!("graph.rs")),
            ("schedule.rs", include_str!("schedule.rs")),
            ("complete.rs", include_str!("complete.rs")),
            ("expand.rs", include_str!("expand.rs")),
            ("watchdog.rs", include_str!("watchdog.rs")),
        ];
        let identifiers = [
            "App",
            "AppState",
            "TerminalRuntime",
            "PaneRuntime",
            "Workspace",
            "ratatui",
            "surrealdb",
            "tokio",
        ];
        let fragments = [".await", "async fn", "std::fs", "std::process", "unwrap()"];

        for (name, source) in sources {
            let production = source.split("#[cfg(test)]").next().unwrap_or_default();
            let code = strip_comments(production);
            // A scanner that matches nothing would pass this test vacuously.
            assert!(
                mentions(&code, "pub") && !code.is_empty(),
                "src/workflow/engine/{name} produced no scannable production code"
            );
            for identifier in identifiers {
                assert!(
                    !mentions(&code, identifier),
                    "src/workflow/engine/{name} references {identifier}"
                );
            }
            for fragment in fragments {
                assert!(
                    !code.contains(fragment),
                    "src/workflow/engine/{name} contains {fragment}"
                );
            }
        }
    }

    fn strip_comments(source: &str) -> String {
        source
            .lines()
            .map(|line| line.find("//").map_or(line, |at| &line[..at]))
            .collect::<Vec<&str>>()
            .join("\n")
    }

    fn mentions(source: &str, identifier: &str) -> bool {
        source.match_indices(identifier).any(|(at, _)| {
            let before = source[..at].chars().next_back();
            let after = source[at + identifier.len()..].chars().next();
            let boundary = |value: Option<char>| {
                !value.is_some_and(|found| found.is_alphanumeric() || found == '_')
            };
            boundary(before) && boundary(after)
        })
    }
}
