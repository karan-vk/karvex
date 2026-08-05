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

use std::collections::HashMap;
use std::time::Instant;

use serde_json::json;
use tracing::debug;

use crate::detect::AgentState;
use crate::workflow::engine::complete::{Completion, Signal, SignalLedger};
use crate::workflow::engine::schedule::TerminalBlocker;
use crate::workflow::model::{
    CheckpointKind, EngineInput, InstancePath, Kvdag, NodeBinding, NodeKey, NodeResult, NodeStatus,
    NodeToken, NoticeLevel, OutputSchema, ProgressDelta, PublicPaneId, RawJson, RunEffect,
    RunEventKind, RunGraph, RunNodeIdx, RunStatus, Runner, StoreWrite, Succession, UserNotice,
    WorkflowEvent,
};

/// Runtime knobs, sourced from the `[workflow]` config block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineConfig {
    pub max_parallel_nodes: usize,
    pub stuck_threshold: u16,
    pub drift_threshold: u16,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_parallel_nodes: 4,
            stuck_threshold: 3,
            drift_threshold: 5,
        }
    }
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
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self {
            config,
            graph: None,
            definition: None,
            signals: HashMap::new(),
            reports: HashMap::new(),
        }
    }

    pub fn config(&self) -> EngineConfig {
        self.config
    }

    pub fn graph(&self) -> Option<&RunGraph> {
        self.graph.as_ref()
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
    pub fn admissions(&self) -> Vec<RunNodeIdx> {
        self.graph
            .as_ref()
            .map(|graph| schedule::ready_set(graph, self.config.max_parallel_nodes))
            .unwrap_or_default()
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

    pub fn apply(&mut self, input: EngineInput) -> Vec<RunEffect> {
        match input {
            EngineInput::Start { graph } => self.start(*graph),
            EngineInput::NodeSelfReport {
                path,
                token,
                result,
            } => self.report(&path, &token, &result),
            EngineInput::TurnEnded { pane } => self.signal_from_pane(&pane, Signal::TurnEnd),
            EngineInput::AgentStatus { pane, state, at } => self.agent_status(&pane, state, at),
            EngineInput::ProgressObserved { path, delta } => self.progress(&path, &delta),
            EngineInput::PaneExited { pane, code } => self.pane_exited(&pane, code),
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

        graph.status = RunStatus::Running;
        let changed = schedule::propagate(&mut graph);

        effects.push(RunEffect::Persist(Box::new(StoreWrite::RunStatus {
            run: graph.run_id.clone(),
            status: RunStatus::Running,
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
        for idx in changed {
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

        let ledger = self.signals.entry(path.clone()).or_default();
        ledger.observe(Signal::SelfReport);
        let evidence = ledger.best().unwrap_or(Signal::SelfReport).evidence();

        let report_ordinal = {
            let count = self.reports.entry(path.clone()).or_insert(0);
            *count = count.saturating_add(1);
            *count
        };

        let Some(schema) = self.schema_for(&key) else {
            return self.needs_attention(
                idx,
                "the run's kvdag definition is not installed, so result.json cannot be validated",
            );
        };

        match complete::accept(&schema, result, evidence, report_ordinal) {
            Completion::Accepted(accepted) => {
                self.reports.remove(path);
                self.succeed(idx, *accepted)
            }
            Completion::Reprompt { errors } => {
                let text = complete::corrective_prompt(&schema, &errors);
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
                if let Some(pane) = pane_of(graph, idx) {
                    effects.push(RunEffect::PromptNode { pane, text });
                }
                effects
            }
            Completion::NeedsAttention { reason } => self.needs_attention(idx, &reason),
        }
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
        let sustained = self
            .signals
            .entry(path)
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

        // §4.3: idle with no valid result never completes a node.
        match complete::missing_result(Signal::SustainedIdle) {
            Completion::NeedsAttention { reason } => self.needs_attention(idx, &reason),
            Completion::Accepted(_) | Completion::Reprompt { .. } => Vec::new(),
        }
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

        let max_attempts = self.max_attempts_of(idx);
        self.signals.remove(&path);
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

    fn steer(&mut self, path: &InstancePath, text: &str) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        let Some(graph) = self.graph.as_mut() else {
            return effects;
        };
        let Some(idx) = graph.index_of(path) else {
            return effects;
        };
        let payload = json!({ "text": text });
        effects.push(journal(
            graph,
            RunEventKind::Steer,
            Some(path.clone()),
            payload,
        ));
        if let Some(pane) = pane_of(graph, idx) {
            effects.push(RunEffect::PromptNode {
                pane,
                text: text.to_string(),
            });
        }
        effects
    }

    fn interrupt(&mut self, path: &InstancePath) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        let Some(graph) = self.graph.as_mut() else {
            return effects;
        };
        let Some(idx) = graph.index_of(path) else {
            return effects;
        };
        let payload = json!({});
        effects.push(journal(
            graph,
            RunEventKind::Interrupt,
            Some(path.clone()),
            payload,
        ));
        if let Some(pane) = pane_of(graph, idx) {
            effects.push(RunEffect::SendKeys {
                pane,
                keys: vec!["Escape".to_string()],
            });
        }
        effects
    }

    /// §5: close the pane, `attempt += 1`, and hand the node back to the
    /// scheduler. Phase 1 always reseeds from `task.md`, because no `partial`
    /// checkpoint can exist before the Phase 4 watchdog writes them.
    fn restart(&mut self, path: &InstancePath) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        self.signals.remove(path);
        self.reports.remove(path);
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
        // A restart is the explicit user action that clears a pause: the
        // conjunct that stalled the run now has a runnable node again.
        if graph.status == RunStatus::Paused {
            graph.status = RunStatus::Running;
            effects.push(RunEffect::Persist(Box::new(StoreWrite::RunStatus {
                run: graph.run_id.clone(),
                status: RunStatus::Running,
            })));
            effects.push(RunEffect::Emit(WorkflowEvent::RunUpdated {
                run: graph.run_id.clone(),
                status: RunStatus::Running,
            }));
        }
        self.settle(&mut effects);
        effects
    }

    fn cancel(&mut self) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        let Some(graph) = self.graph.as_mut() else {
            return effects;
        };
        if matches!(
            graph.status,
            RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled
        ) {
            return effects;
        }

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
        if graph.status != RunStatus::Running {
            return;
        }
        for idx in schedule::propagate(graph) {
            record_status(graph, idx, effects);
        }

        match schedule::run_terminal_ready(graph) {
            Ok(()) => {
                // §3.2 lets the conjunction hold with a `Blocked` node, because
                // the run may continue on other branches. Reporting that run as
                // `succeeded` would be the soft form of the false-completion
                // bug, so an unresolved blocker fails the run just as a `Failed`
                // node does.
                let status = if graph
                    .nodes
                    .iter()
                    .any(|node| matches!(node.status, NodeStatus::Failed | NodeStatus::Blocked))
                {
                    RunStatus::Failed
                } else {
                    RunStatus::Succeeded
                };
                finish(graph, status, effects);
            }
            Err(blocker) => {
                let live = graph
                    .nodes
                    .iter()
                    .any(|node| matches!(node.status, NodeStatus::Ready | NodeStatus::Running));
                if !live {
                    pause(graph, &blocker, effects);
                }
            }
        }
    }

    fn succeed(&mut self, idx: RunNodeIdx, result: NodeResult) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        let Some(graph) = self.graph.as_mut() else {
            return effects;
        };
        let Some(node) = graph.node_mut(idx) else {
            return effects;
        };
        node.status = NodeStatus::Succeeded;
        node.result = Some(result);
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
        for other in changed {
            if other != idx {
                record_status(graph, other, &mut effects);
            }
        }

        self.settle(&mut effects);
        effects
    }

    fn needs_attention(&mut self, idx: RunNodeIdx, reason: &str) -> Vec<RunEffect> {
        let mut effects = Vec::new();
        let Some(graph) = self.graph.as_mut() else {
            return effects;
        };
        let Some(node) = graph.node_mut(idx) else {
            return effects;
        };
        node.status = NodeStatus::NeedsAttention;
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
    fn runner_of(&self, idx: RunNodeIdx) -> Runner {
        self.definition_node(idx)
            .map_or(Runner::Agent, |node| node.runner)
    }

    fn max_attempts_of(&self, idx: RunNodeIdx) -> u8 {
        self.definition_node(idx)
            .map_or(1, |node| node.max_attempts)
    }

    fn schema_for(&self, key: &NodeKey) -> Option<OutputSchema> {
        self.definition
            .as_ref()
            .and_then(|kvdag| kvdag.node(key))
            .map(|node| node.output_schema.clone())
    }
}

fn next_seq(graph: &mut RunGraph) -> u64 {
    graph.seq = graph.seq.saturating_add(1);
    graph.seq
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

fn finish(graph: &mut RunGraph, status: RunStatus, effects: &mut Vec<RunEffect>) {
    graph.status = status;
    effects.push(RunEffect::Persist(Box::new(StoreWrite::RunStatus {
        run: graph.run_id.clone(),
        status,
    })));
    let payload = json!({ "status": status });
    effects.push(journal(graph, RunEventKind::RunFinished, None, payload));
    effects.push(RunEffect::Emit(WorkflowEvent::RunFinished {
        run: graph.run_id.clone(),
        status,
    }));
}

fn pause(graph: &mut RunGraph, blocker: &TerminalBlocker, effects: &mut Vec<RunEffect>) {
    graph.status = RunStatus::Paused;
    effects.push(RunEffect::Persist(Box::new(StoreWrite::RunStatus {
        run: graph.run_id.clone(),
        status: RunStatus::Paused,
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

    #[test]
    fn sustained_idle_without_a_result_needs_attention_and_never_succeeds() {
        let (mut engine, graph) = two_node_engine();
        engine.apply(EngineInput::Start {
            graph: Box::new(graph),
        });
        engine.bind_node(&InstancePath::new("plan"), binding("pane-1"));

        let pane = PublicPaneId::new("pane-1");
        let now = Instant::now();
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
