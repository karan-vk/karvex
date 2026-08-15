//! The historical run projection: a past run, rehydrated read-only for the DAG
//! overlay (`docs/design/workflow-builder/07-phase3-plan.md` §1 WS-H).
//!
//! The run browser opens a *closed* run here. What comes back is a
//! [`HistoricalRunSnapshot`] on `AppState` — a `RunGraph` the overlay projects
//! exactly like the live one, plus the run's interrogation rows for the
//! detached lane. Nothing here is ever fed to the engine: a historical run is
//! not the active run, and every write path against it is refused server-side
//! by the existing not-the-active-run guard.
//!
//! **The data contract is the wire API, not private store reads.** The loader
//! dispatches `workflow.run.get` in-process, the same way the launcher
//! dispatches `workflow.list`, and rehydrates the model types from the wire
//! answer. That is the runtime/client boundary guardrail applied to a second
//! client surface (§3 frozen interface 10 states it for the browser; the DAG
//! view is the same client reading the same resource). A slim build therefore
//! needs no special case for the graph at all: `workflow.run.get` answers
//! `workflow_unavailable` and the caller shows that message.
//!
//! There is exactly **one** sanctioned exception, and it is the module's only
//! `#[cfg(feature = "workflow")]`: a run's interrogation rows. No API method
//! lists them by design, so [`App::historical_interrogations`] reads
//! `list_interrogations` in process — and because the store itself is
//! feature-gated, that one function is gated too, with a slim arm returning an
//! empty lane it can never actually be asked for.

use std::collections::BTreeMap;

use crate::api::schema::{
    ErrorBody, ErrorResponse, Method, ResponseResult, SuccessResponse, WorkflowEdgeKind,
    WorkflowEvidence, WorkflowInterrogationMode, WorkflowNodeStatus, WorkflowRunGraph,
    WorkflowRunInfo, WorkflowRunNodeInfo, WorkflowRunStatus, WorkflowRunTarget, WorkflowSuccession,
    WorkflowTier,
};
use crate::app::state::{
    DagViewState, HistoricalInterrogation, HistoricalRunSnapshot, Mode, ProjectedNodeFacts,
};
use crate::app::App;
use crate::workflow::model::{
    EdgeKind, EdgePayload, Evidence, GrowthLimits, InstancePath, KvdagVersionId, NodeKey,
    NodeResult, NodeStatus, NodeUsage, ProgressTracker, RestoredRef, RunEdge, RunGraph, RunId,
    RunNode, RunNodeIdx, RunStatus, Succession,
};
use crate::workflow::tier::{Assignment, Effort, ModelAlias, Tier};

impl App {
    /// Opens `run_id` in the DAG view as a read-only past run.
    ///
    /// On success the overlay is showing the snapshot; on failure nothing is
    /// mutated and the caller gets the message the API produced, which is
    /// already the user-facing wording for "pruned", "no such run", and "this
    /// build has no workflow store".
    // The caller is the run browser's `Enter` on a closed run (WS-F, step 2e).
    #[allow(dead_code)]
    pub(crate) fn load_historical_run(&mut self, run_id: &str) -> Result<(), String> {
        let response = self.dispatch_api_request(
            "tui.workflow.run.get",
            Method::WorkflowRunGet(WorkflowRunTarget {
                run_id: run_id.to_string(),
            }),
        );
        let Some(ResponseResult::WorkflowRunGet { run, graph }) = success_result(&response) else {
            return Err(
                error_message(&response).unwrap_or_else(|| format!("could not open run {run_id}"))
            );
        };

        let workflow_name = run.workflow_name.clone();
        // Taken before `graph` is consumed by the rehydration: the projection's
        // facts and the team's members are *not* the engine's `RunGraph` and
        // must not be smuggled into it (§3.4). They travel beside it on the
        // snapshot instead.
        let team_name = run.team_name.clone();
        let lead_pane_id = run.lead_pane_id.clone();
        let members = graph.members.clone();
        let projected = projected_facts(&graph);
        // Only a lead run can ever have a review cycle (§3.5: the trigger is
        // a Claude Code team lead's run reaching a terminal status), so an
        // engine-era run skips the round trip rather than asking a question
        // that can only ever answer `None`.
        let (review, review_findings) = if team_name.is_some() || lead_pane_id.is_some() {
            self.fetch_workflow_review(run_id)
        } else {
            (None, Vec::new())
        };
        let graph = rehydrate_run_graph(&run, graph);
        let interrogations = self.historical_interrogations(&graph.run_id);
        self.state.set_historical_run(Some(HistoricalRunSnapshot {
            graph: Box::new(graph),
            workflow_name,
            interrogations,
            team_name,
            lead_pane_id,
            projected,
            members,
            review,
            review_findings,
        }));
        self.state.mode = Mode::WorkflowDag;
        Ok(())
    }

    /// `workflow.review.get` for `run_id` — the DAG overlay's own read of the
    /// run's self-improvement review cycle, dispatched in-process exactly
    /// like `workflow.run.get` above it (`AGENTS.md`'s runtime/client
    /// boundary guardrail: a shared runtime fact, read through the JSON API,
    /// never a private store read from the TUI).
    ///
    /// A failure here degrades to "no review" rather than failing the whole
    /// load: the run graph is still a usable overlay without it, the same
    /// reasoning `load_historical_run` already applies to a missing summary.
    fn fetch_workflow_review(
        &mut self,
        run_id: &str,
    ) -> (
        Option<crate::api::schema::WorkflowReviewInfo>,
        Vec<crate::api::schema::WorkflowReviewFindingInfo>,
    ) {
        let response = self.dispatch_api_request(
            "tui.workflow.review.get",
            Method::WorkflowReviewGet(WorkflowRunTarget {
                run_id: run_id.to_string(),
            }),
        );
        match success_result(&response) {
            Some(ResponseResult::WorkflowReviewGet { review, findings }) => (review, findings),
            _ => {
                if let Some(message) = error_message(&response) {
                    tracing::debug!(%message, %run_id, "could not read this run's review cycle");
                }
                (None, Vec::new())
            }
        }
    }

    /// Refreshes only the review half of an already-open DAG snapshot, in
    /// response to a `workflow.review.*` event
    /// (`.local/prd/phase4-retarget-plan.md` §5 packet P13's contract:
    /// "refresh only on `workflow.review.*` and `workflow.node.watchdog`
    /// events").
    ///
    /// Deliberately narrower than [`Self::reload_open_lead_run`]: a review
    /// cycle runs on a run that has already reached a terminal status, so by
    /// the time it exists there is no live projection left to drive the
    /// poll-triggered reload — `poll_run_watchdog`'s "did anything change"
    /// edge never fires again once the run has closed. This is the seam that
    /// keeps a closed run's review cycle visible without inventing a second
    /// poll timer: the review's own event stream is the trigger.
    pub(crate) fn refresh_open_dag_review(&mut self, run_id: &str) -> bool {
        if self.state.mode != Mode::WorkflowDag {
            return false;
        }
        let open = self
            .state
            .historical_run()
            .is_some_and(|snapshot| snapshot.graph.run_id.as_str() == run_id);
        if !open {
            return false;
        }
        let (review, review_findings) = self.fetch_workflow_review(run_id);
        if let Some(snapshot) = self.state.historical_run.as_mut() {
            snapshot.review = review;
            snapshot.review_findings = review_findings;
        }
        true
    }

    /// `V` in the DAG view: the review ask's other half, dispatched off
    /// exactly what [`crate::app::state::DagViewState::review_status`] says —
    /// never automatic (`.local/prd/phase4-retarget-plan.md` §6 D19).
    ///
    /// No cycle yet on a finished lead run starts one; a cycle waiting on the
    /// human opens the findings overlay through the same path the global
    /// `keys.open_workflow_review` binding uses, so there is exactly one
    /// place that turns a `workflow.review.get` answer into
    /// [`crate::app::state::WorkflowReviewState`]. Anything else — still
    /// running, already decided, or no run open at all — is a quiet no-op:
    /// there is nothing this key could usefully start or open.
    pub(crate) fn handle_workflow_dag_review_key(&mut self) {
        let Some(run_id) = self
            .state
            .historical_run()
            .map(|snapshot| snapshot.graph.run_id.to_string())
        else {
            return;
        };
        let dag = &self.state.view.dag;
        match dag.review_status {
            None if dag.lead_run
                && dag
                    .run_status
                    .is_some_and(run_status_is_terminal_for_review) =>
            {
                self.start_review_from_dag(run_id);
            }
            Some(crate::api::schema::WorkflowReviewStatus::AwaitingUser) => {
                self.open_workflow_review();
            }
            _ => {}
        }
    }

    /// `workflow.review.start`, from the DAG's `V` key. Updates the open
    /// snapshot's review field directly from the response rather than
    /// re-fetching: `workflow.review.start`'s own answer already carries the
    /// cycle it just created.
    fn start_review_from_dag(&mut self, run_id: String) {
        let response = self.dispatch_api_request(
            "tui.workflow.review.start",
            Method::WorkflowReviewStart(WorkflowRunTarget {
                run_id: run_id.clone(),
            }),
        );
        match success_result(&response) {
            Some(ResponseResult::WorkflowReviewStarted { review }) => {
                if let Some(snapshot) = self.state.historical_run.as_mut() {
                    snapshot.review = Some(review);
                }
                self.show_workflow_notice(crate::workflow::model::UserNotice {
                    level: crate::workflow::model::NoticeLevel::Info,
                    run: Some(RunId::new(run_id)),
                    path: None,
                    message: "review started — interviewing the run's team".to_string(),
                });
            }
            _ => {
                let message = error_message(&response)
                    .unwrap_or_else(|| "could not start a review for this run".to_string());
                self.show_workflow_notice(crate::workflow::model::UserNotice {
                    level: crate::workflow::model::NoticeLevel::Warning,
                    run: Some(RunId::new(run_id)),
                    path: None,
                    message,
                });
            }
        }
    }

    /// Re-reads the run the overlay currently has open, when the run
    /// projection has just moved underneath it (§3.6 refresh).
    ///
    /// The historical path is load-once by design, which is right for a closed
    /// run and wrong for a *live* lead run: its tasks, owners, and members
    /// change every poll. Guarded three ways so a reload can neither surprise
    /// nor loop — the overlay must be the surface on screen, it must be showing
    /// a lead run, and `run_id` must be the run that actually changed. The
    /// caller only invokes this on a poll that reported a change, so the
    /// trigger is an edge rather than a level and cannot re-arm itself.
    pub(crate) fn reload_open_lead_run(&mut self, run_id: &str) -> bool {
        if self.state.mode != Mode::WorkflowDag {
            return false;
        }
        let open = self.state.historical_run().is_some_and(|snapshot| {
            snapshot.is_lead_run() && snapshot.graph.run_id.as_str() == run_id
        });
        if !open {
            return false;
        }
        if let Err(error) = self.load_historical_run(run_id) {
            // A reload that fails leaves the last good snapshot on screen: the
            // run is still openable from the browser, and blanking a graph the
            // user is watching because one refresh missed is the worse answer.
            tracing::debug!(%error, run = %run_id, "could not refresh the open lead run");
            return false;
        }
        true
    }

    /// Closes the historical projection, putting the overlay back on the live
    /// run (or on "no workflow run to show").
    ///
    /// Every exit from the DAG view goes through here rather than clearing the
    /// field inline: a snapshot left behind would keep a past run on screen the
    /// next time the overlay opens, which is the one failure mode a read-only
    /// view can still cause damage through — the user acts on a stale graph
    /// believing it is live.
    pub(crate) fn close_historical_run(&mut self) {
        self.state.set_historical_run(None);
    }

    /// The run's interrogation rows, for the detached lane.
    ///
    /// The one sanctioned exception to this module's wire-only rule (C-3):
    /// there is deliberately no API method for listing a run's interrogations,
    /// so the TUI reads `list_interrogations` in process.
    ///
    /// A failure here returns an empty lane rather than failing the load. The
    /// lane is an *addendum* to the graph; a run the user asked to see must
    /// not become unopenable because its interrogation rows would not decode.
    #[cfg(feature = "workflow")]
    fn historical_interrogations(&mut self, run: &RunId) -> Vec<HistoricalInterrogation> {
        let wanted = run.clone();
        let loaded = self
            .workflow_store
            .call(move |cx| cx.block_on(cx.store().list_interrogations(&wanted)));
        let records = match loaded {
            Ok(Ok(records)) => records,
            Ok(Err(error)) => {
                tracing::debug!(%error, %run, "could not list a past run's interrogations");
                return Vec::new();
            }
            Err(unavailable) => {
                tracing::debug!(
                    code = unavailable.code,
                    %run,
                    "the workflow store is unavailable; the interrogation lane stays empty"
                );
                return Vec::new();
            }
        };
        records
            .into_iter()
            .map(|record| {
                // A recorded pane id is not evidence that the pane is alive:
                // §4 D7 lets an interrogation pane outlive its run, and a
                // server restart takes every pane with it while the row's
                // `ended_at` — written on pane death — may never have been
                // stamped. `parse_pane_id` answering `None` *is* the liveness
                // check, so the lane offers a click only where there is
                // something to click.
                let live_pane = record
                    .pane_id
                    .clone()
                    .filter(|pane| self.parse_pane_id(pane).is_some());
                interrogation_row(record, live_pane)
            })
            .collect()
    }

    /// A slim build has no store to read, and never reaches here anyway:
    /// `workflow.run.get` answers `workflow_unavailable`, so
    /// [`Self::load_historical_run`] returns before the lane is built.
    #[cfg(not(feature = "workflow"))]
    fn historical_interrogations(&mut self, _run: &RunId) -> Vec<HistoricalInterrogation> {
        Vec::new()
    }
}

/// One durable interrogation row → the lane's view of it.
///
/// `live_pane` is the record's pane id **after** the liveness check, so this
/// stays a pure mapping: the caller owns "does this pane still exist", and the
/// display rule lives in one testable place.
#[cfg(feature = "workflow")]
fn interrogation_row(
    record: crate::workflow::store::InterrogationRecord,
    live_pane: Option<String>,
) -> HistoricalInterrogation {
    HistoricalInterrogation {
        id: record.id.to_string(),
        path: record.path.as_str().to_string(),
        // Ended when the row says so **or** when the pane is gone. The box's
        // only affordance is focusing its pane, so one with no pane is
        // finished as far as the user is concerned — drawing it teal-and-live
        // would promise a click that does nothing, which is the same broken
        // promise the footer's hint suppression exists to avoid.
        ended: record.ended_at_unix_ms.is_some() || live_pane.is_none(),
        pane_id: live_pane,
        reconstructed: record.reconstructed,
    }
}

// ── the interrogate keys (`i` resumes, `Shift+I` reconstructs) ──────────────

/// The error code the interrogate handler answers when there is no transcript
/// to fork and no checkpoint to reconstruct from.
///
/// Spelled here as a literal, not imported from the handler's own constant,
/// because this module's `interrogate_intent`/`interrogate_outcome` compile
/// unconditionally while the handler's constant is `#[cfg(feature =
/// "workflow")]` — importing it here would not compile in a
/// `--no-default-features` build. `pub(crate)` so
/// `all_workflow_error_codes_are_a_well_formed_disjoint_family`
/// (`src/app/api/workflows.rs`) can assert the two literals stay equal
/// instead of letting them drift the way a duplicated literal has before in
/// this phase. The overlay must classify the *code*, never the message: the
/// message names which of `cwd`/transcript was missing and is free to be
/// reworded.
pub(crate) const TRANSCRIPT_UNAVAILABLE_CODE: &str = "workflow_transcript_unavailable";

/// What an interrogate keypress on the selected node should do.
///
/// **Stateless by design.** The mode comes from *which key was pressed* — `i`
/// resumes, `Shift+I` reconstructs — so there is no pending-offer memory to
/// carry, go stale, or leak across an overlay close. The plan's original
/// two-step (press `i` twice) was dropped for this: a second press of the same
/// key silently meaning something else is hidden modal state, and it was
/// broken outright for anyone on `terminal`/`system` toast delivery, where the
/// "press again" notice arrives out of band — a quick second `i` would have
/// opened a reconstructed session the user was never told about. An explicit
/// second key serves 00 Feature 3's "never presented as the original" strictly
/// better, because escalation is always something the user chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InterrogateIntent {
    /// No run, or nothing selected: the key does nothing and says nothing —
    /// an empty overlay has no node to ask about.
    Ignore,
    /// Dispatch `workflow.node.interrogate`.
    Send {
        run_id: String,
        path: String,
        mode: WorkflowInterrogationMode,
    },
}

pub(crate) fn interrogate_intent(
    dag: &DagViewState,
    mode: WorkflowInterrogationMode,
) -> InterrogateIntent {
    if dag.run_id.is_empty() {
        return InterrogateIntent::Ignore;
    }
    let Some(node) = dag.selected_node() else {
        return InterrogateIntent::Ignore;
    };
    InterrogateIntent::Send {
        run_id: dag.run_id.clone(),
        path: node.path.clone(),
        mode,
    }
}

/// What the overlay does with the interrogate answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InterrogateOutcome {
    /// A pane opened. The pane is the feedback; there is nothing to say.
    Opened,
    /// There is no transcript to fork, but there *is* a next step. The notice
    /// text is the whole guide — it names the key, so nothing has to be
    /// remembered between presses.
    OfferReconstructed(String),
    /// Any refusal with no next step, shown as-is.
    Refused(String),
}

/// Classify the answer to an interrogate request.
///
/// The `Shift+I` pointer is offered **only** for a `resumed` attempt. A
/// `reconstructed` attempt that also answers `transcript_unavailable` means
/// there is no stored checkpoint to seed from either — pointing at the same
/// key again would send the user in a circle on a node that can never be
/// interrogated, which is the "never a silently failing pane" rule turned into
/// a silently failing keystroke.
pub(crate) fn interrogate_outcome(
    path: &str,
    mode: WorkflowInterrogationMode,
    response: &str,
) -> InterrogateOutcome {
    if matches!(
        success_result(response),
        Some(ResponseResult::WorkflowNodeInterrogated { .. })
    ) {
        return InterrogateOutcome::Opened;
    }
    let Some(error) = error_body(response) else {
        return InterrogateOutcome::Refused(format!("could not interrogate {path}"));
    };
    if error.code == TRANSCRIPT_UNAVAILABLE_CODE && mode == WorkflowInterrogationMode::Resumed {
        return InterrogateOutcome::OfferReconstructed(format!(
            "{} — press Shift+I for a reconstructed session",
            error.message
        ));
    }
    InterrogateOutcome::Refused(error.message)
}

/// Why steering a past run is refused, in the wording the server would use.
///
/// A historical run is not the active run, so `steer`/`interrupt`/`restart`
/// answer the not-the-active-run family — **not** `workflow_run_closed`, which
/// covers only the just-finished run that is still the active one
/// (`07-phase3-plan.md` §1 WS-H). Saying so client-side costs a round trip
/// nobody needs and gets the reason right.
pub(crate) fn historical_steer_refusal() -> String {
    "this is a past run, not the active one — steering is unavailable".to_string()
}

// ── wire → model rehydration ────────────────────────────────────────────────

/// The durable projection, back into the pure `RunGraph` the overlay lays out.
///
/// Indices are assigned by position and edges are resolved through a
/// path → index map: `RunNodeIdx` is a *live* engine handle with no durable
/// meaning, so reusing whatever the source run happened to allocate would be
/// inventing an identity the store never recorded. Instance paths are the
/// durable identity, and they are what every projection here keys on.
/// The projection's observations, keyed by instance path (§3.4).
///
/// Kept out of [`rehydrate_run_graph`] deliberately: `RunNode` is the engine's
/// model of a node and has no field for a task id, a subject, an owner, or
/// emergence — those are facts about what a Claude Code team did, and the
/// engine has no business describing them. An engine-era run's nodes carry
/// none of them, so this comes back empty and the overlay's merge is a no-op.
fn projected_facts(graph: &WorkflowRunGraph) -> BTreeMap<String, ProjectedNodeFacts> {
    graph
        .nodes
        .iter()
        .filter(|node| {
            node.task_id.is_some()
                || !node.subject.is_empty()
                || !node.owner.is_empty()
                || node.attention.is_some()
                || node.watchdog_interventions > 0
        })
        .map(|node| {
            (
                node.path.clone(),
                ProjectedNodeFacts {
                    task_id: node.task_id.clone(),
                    subject: node.subject.clone(),
                    owner: node.owner.clone(),
                    emergent: node.emergent,
                    attention: node.attention,
                    watchdog_interventions: node.watchdog_interventions,
                },
            )
        })
        .collect()
}

fn rehydrate_run_graph(run: &WorkflowRunInfo, graph: WorkflowRunGraph) -> RunGraph {
    let index_of: std::collections::HashMap<String, RunNodeIdx> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(position, node)| (node.path.clone(), RunNodeIdx(position)))
        .collect();

    let nodes: Vec<RunNode> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(position, node)| rehydrate_node(RunNodeIdx(position), node, &index_of))
        .collect();

    let edges: Vec<RunEdge> = graph
        .edges
        .iter()
        .filter_map(|edge| {
            // An edge naming a node the projection does not carry is dropped
            // rather than pointed at nothing — the same rule the layout's
            // clipping applies, one layer earlier.
            let from = *index_of.get(&edge.from)?;
            let to = *index_of.get(&edge.to)?;
            Some(RunEdge {
                from,
                to,
                kind: edge_kind(edge.kind),
                condition: None,
                payload: EdgePayload::default(),
                port: None,
                condition_result: edge.condition_result,
                fired: edge.fired,
            })
        })
        .collect();

    RunGraph {
        run_id: RunId::new(run.run_id.clone()),
        version_id: KvdagVersionId::new(run.version_id.clone()),
        tier: engine_tier(run.tier),
        growth: GrowthLimits {
            max_depth: clamp_u16(run.max_depth),
            max_nodes: clamp_u16(run.max_nodes),
        },
        // Resolved assignments are a run-start artefact the durable projection
        // does not carry per key; each node already holds the assignment it
        // actually ran under, which is what the overlay reads.
        assignments: BTreeMap::new(),
        nodes,
        edges,
        status: run_status(run.status),
        // A past run is not being settled: there is no cursor to advance, and
        // a fabricated one would be a number nothing wrote.
        seq: 0,
        // The epilogue's phase is engine state, not a durable column. A closed
        // run's summariser is visible as a normal `.summary` node in the graph
        // above; the header's "summarising…" is a live-run affordance.
        epilogue: None,
    }
}

fn rehydrate_node(
    idx: RunNodeIdx,
    node: &WorkflowRunNodeInfo,
    index_of: &std::collections::HashMap<String, RunNodeIdx>,
) -> RunNode {
    RunNode {
        idx,
        key: NodeKey::new(node.node_key.clone()),
        path: InstancePath::new(node.path.clone()),
        label: node.label.clone(),
        // The `--input k=v` overrides are not on the wire node shape; they are
        // an expansion-time fact the DAG view does not draw.
        inputs: BTreeMap::new(),
        parent: node
            .parent_path
            .as_ref()
            .and_then(|path| index_of.get(path).copied()),
        depth: clamp_u16(node.depth),
        status: node_status(node.status),
        assignment: Assignment {
            model: parse_model(&node.model),
            effort: parse_effort(&node.effort),
        },
        assignment_reason: node.assignment_reason.clone(),
        attempt: u8::try_from(node.attempt).unwrap_or(u8::MAX),
        // Deliberately `None`, always. `NodeBinding` is a *live* binding — a
        // pane id, a terminal id, a transcript path — and a past run's panes
        // are gone. Rehydrating one would make `Enter` offer to focus a pane
        // that does not exist, which is exactly the "never a silent failure"
        // rule this view is built around. The node's own `pane_id` is
        // therefore absent from the projection and the footer drops `focus`.
        binding: None,
        result: node.succession.as_ref().map(|_| NodeResult {
            // The checkpoint payload is not on the node shape; the overlay
            // draws only the summary line, and inventing a payload here would
            // put a value in `result.payload` that no checkpoint holds.
            payload: serde_json::Value::Null,
            summary: String::new(),
            artifact_paths: Vec::new(),
            digest: String::new(),
            evidence: node.evidence.map(evidence).unwrap_or(Evidence::SelfReport),
        }),
        usage: NodeUsage {
            total_tokens: node.total_tokens,
            tool_uses: node.tool_uses,
            duration_ms: node.duration_ms,
        },
        started_at_unix_ms: node.started_at_unix_ms,
        ended_at_unix_ms: node.ended_at_unix_ms,
        progress: ProgressTracker::default(),
        succession: node.succession.as_ref().map(succession),
        checkpoint_seq: 0,
        restored_from: node.restored_from.as_ref().map(|source| RestoredRef {
            run: RunId::new(source.run_id.clone()),
            node_key: NodeKey::new(source.node_key.clone()),
            checkpoint_seq: source.checkpoint_seq,
        }),
    }
}

/// The wire's `u32` counts into the model's `u16` ceilings. Saturating rather
/// than wrapping: a projection is a read, and a read must never turn a large
/// number into a small one that reads as a plausible limit.
fn clamp_u16(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

/// The wire model/effort strings back into the enums.
///
/// Both derive snake_case `Deserialize`, so serde is the single mapping — a
/// hand-written match here would be a second one, free to drift from the
/// outbound direction the handler owns. An unrecognised value falls back to
/// the cheapest rung rather than failing the whole projection: one node's
/// assignment reading `sonnet · low` is a far smaller lie than the run
/// refusing to open.
fn parse_model(value: &str) -> ModelAlias {
    serde_json::from_value(serde_json::Value::String(value.to_string()))
        .unwrap_or(ModelAlias::Sonnet)
}

fn parse_effort(value: &str) -> Effort {
    serde_json::from_value(serde_json::Value::String(value.to_string())).unwrap_or(Effort::Low)
}

fn engine_tier(tier: WorkflowTier) -> Tier {
    match tier {
        WorkflowTier::Auto => Tier::Auto,
        WorkflowTier::Max => Tier::Max,
        WorkflowTier::High => Tier::High,
        WorkflowTier::Medium => Tier::Medium,
        WorkflowTier::Low => Tier::Low,
    }
}

fn run_status(status: WorkflowRunStatus) -> RunStatus {
    match status {
        WorkflowRunStatus::Pending => RunStatus::Pending,
        WorkflowRunStatus::Running => RunStatus::Running,
        WorkflowRunStatus::Paused => RunStatus::Paused,
        WorkflowRunStatus::Succeeded => RunStatus::Succeeded,
        WorkflowRunStatus::Failed => RunStatus::Failed,
        WorkflowRunStatus::Cancelled => RunStatus::Cancelled,
    }
}

fn node_status(status: WorkflowNodeStatus) -> NodeStatus {
    match status {
        WorkflowNodeStatus::Pending => NodeStatus::Pending,
        WorkflowNodeStatus::Ready => NodeStatus::Ready,
        WorkflowNodeStatus::Running => NodeStatus::Running,
        WorkflowNodeStatus::NeedsAttention => NodeStatus::NeedsAttention,
        WorkflowNodeStatus::Blocked => NodeStatus::Blocked,
        WorkflowNodeStatus::Succeeded => NodeStatus::Succeeded,
        WorkflowNodeStatus::Failed => NodeStatus::Failed,
        WorkflowNodeStatus::Skipped => NodeStatus::Skipped,
        WorkflowNodeStatus::Restored => NodeStatus::Restored,
        WorkflowNodeStatus::Cancelled => NodeStatus::Cancelled,
    }
}

fn edge_kind(kind: WorkflowEdgeKind) -> EdgeKind {
    match kind {
        WorkflowEdgeKind::Sequence => EdgeKind::Sequence,
        WorkflowEdgeKind::Data => EdgeKind::Data,
        WorkflowEdgeKind::Conditional => EdgeKind::Conditional,
    }
}

fn evidence(evidence: WorkflowEvidence) -> Evidence {
    match evidence {
        WorkflowEvidence::SelfReport => Evidence::SelfReport,
        WorkflowEvidence::Hook => Evidence::Hook,
        WorkflowEvidence::Detection => Evidence::Detection,
        WorkflowEvidence::Restored => Evidence::Restored,
    }
}

fn succession(succession: &WorkflowSuccession) -> Succession {
    match succession {
        WorkflowSuccession::Satisfied => Succession::Satisfied,
        WorkflowSuccession::Blocked {
            reason,
            resume_when,
        } => Succession::Blocked {
            reason: reason.clone(),
            resume_when: resume_when.clone(),
        },
        WorkflowSuccession::NoFollowup { evidence } => Succession::NoFollowup {
            evidence: evidence.clone(),
        },
    }
}

/// Whether a run has reached a status a review could start on
/// (`workflow.review.start`'s own precondition, §3.5). A deliberate small
/// copy of `app/workflow_review.rs`'s private `run_status_is_terminal` rather
/// than a cross-module dependency on another workstream's file — the same
/// call `ui/workflow_runs.rs` already makes for `run_status_color`/
/// `run_status_glyph`. This is a hint for when to *show* the ask; the
/// server's own precondition is what actually decides, so drift here can
/// only ever cost an extra refused `workflow.review.start`, never a wrong
/// acceptance.
fn run_status_is_terminal_for_review(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled
    )
}

fn success_result(response: &str) -> Option<ResponseResult> {
    serde_json::from_str::<SuccessResponse>(response)
        .ok()
        .map(|success| success.result)
}

fn error_message(response: &str) -> Option<String> {
    error_body(response).map(|error| error.message)
}

fn error_body(response: &str) -> Option<ErrorBody> {
    serde_json::from_str::<ErrorResponse>(response)
        .ok()
        .map(|error| error.error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{WorkflowDemand, WorkflowRestoredFrom, WorkflowRunEdgeInfo};

    fn wire_node(path: &str, key: &str) -> WorkflowRunNodeInfo {
        WorkflowRunNodeInfo {
            path: path.to_string(),
            node_key: key.to_string(),
            label: String::new(),
            parent_path: None,
            depth: 0,
            status: WorkflowNodeStatus::Succeeded,
            demand: WorkflowDemand::Standard,
            model: "opus".to_string(),
            effort: "high".to_string(),
            attempt: 1,
            pane_id: Some("pane-9".to_string()),
            terminal_id: Some("term-9".to_string()),
            agent_session_id: Some("sid".to_string()),
            cwd: None,
            node_dir: None,
            started_at_unix_ms: Some(1_000),
            ended_at_unix_ms: Some(4_000),
            total_tokens: 12,
            tool_uses: 3,
            duration_ms: 3_000,
            evidence: Some(WorkflowEvidence::SelfReport),
            succession: Some(WorkflowSuccession::Satisfied),
            blocker: None,
            watchdog_interventions: 0,
            assignment_reason: "measured".to_string(),
            delivery_failure: None,
            growth_limited: None,
            transcript_path: None,
            restored_from: None,
            task_id: None,
            subject: String::new(),
            owner: String::new(),
            emergent: false,
            attention: None,
        }
    }

    fn wire_run() -> WorkflowRunInfo {
        WorkflowRunInfo {
            run_id: "workflow_run:past".to_string(),
            workflow_id: "workflow:1".to_string(),
            version_id: "kvdag_version:1".to_string(),
            tier: WorkflowTier::High,
            status: WorkflowRunStatus::Succeeded,
            args: Default::default(),
            workspace_id: None,
            tab_id: None,
            started_at_unix_ms: 100,
            ended_at_unix_ms: Some(900),
            total_tokens: 12,
            total_tool_uses: 3,
            nodes_total: 2,
            nodes_done: 2,
            failure: None,
            max_depth: 3,
            max_nodes: 24,
            nodes_live: 2,
            growth_limited: None,
            workflow_name: "ux-dag-probe".to_string(),
            context_runs: Vec::new(),
            restore_from_run: None,
            lead_session_id: None,
            team_name: None,
            lead_pane_id: None,
            lead_prompt_version: None,
        }
    }

    fn wire_graph() -> WorkflowRunGraph {
        let mut child = wire_node("plan/impl", "impl");
        child.parent_path = Some("plan".to_string());
        child.depth = 1;
        WorkflowRunGraph {
            nodes: vec![wire_node("plan", "plan"), child],
            edges: vec![WorkflowRunEdgeInfo {
                from: "plan".to_string(),
                to: "plan/impl".to_string(),
                kind: WorkflowEdgeKind::Data,
                condition_result: None,
                fired: true,
            }],
            members: Vec::new(),
            messaging: None,
        }
    }

    /// Indices are positional and edges resolve through instance paths — the
    /// durable identity — so the rehydrated graph is connected the way the
    /// store recorded it, not the way some past engine happened to number it.
    #[test]
    fn the_projection_resolves_edges_through_instance_paths() {
        let graph = rehydrate_run_graph(&wire_run(), wire_graph());
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].from, RunNodeIdx(0));
        assert_eq!(graph.edges[0].to, RunNodeIdx(1));
        assert_eq!(graph.edges[0].kind, EdgeKind::Data);
        assert!(graph.edges[0].fired);
        assert_eq!(graph.nodes[1].parent, Some(RunNodeIdx(0)));
        assert_eq!(graph.nodes[1].depth, 1);
        assert_eq!(graph.run_id.as_str(), "workflow_run:past");
        assert_eq!(graph.status, RunStatus::Succeeded);
        assert_eq!(graph.tier, Tier::High);
        assert_eq!(graph.growth.max_nodes, 24);
    }

    /// §3.4: the projection's observations travel beside the rehydrated graph,
    /// not inside it. An engine-era run has none, so the map comes back empty
    /// and the overlay's merge is a no-op — which is what keeps such a run
    /// rendering exactly as it did before the rework.
    #[test]
    fn projected_task_facts_are_kept_beside_the_graph_and_only_for_observed_nodes() {
        assert!(
            projected_facts(&wire_graph()).is_empty(),
            "an engine-era run has no projection to carry"
        );

        let mut wire = wire_graph();
        wire.nodes[0].task_id = Some("1".to_string());
        wire.nodes[0].subject = "plan the work".to_string();
        wire.nodes[0].owner = "research".to_string();
        wire.nodes[1].task_id = Some("7".to_string());
        wire.nodes[1].subject = "retest the parser".to_string();
        wire.nodes[1].emergent = true;

        let facts = projected_facts(&wire);
        assert_eq!(facts.len(), 2);
        let planned = facts.get("plan").expect("keyed by instance path");
        assert_eq!(planned.owner, "research");
        assert_eq!(planned.subject, "plan the work");
        assert!(!planned.emergent);
        let emergent = facts.get("plan/impl").expect("keyed by instance path");
        assert!(emergent.emergent);
        assert_eq!(
            emergent.owner, "",
            "an unclaimed task keeps its empty owner rather than being defaulted"
        );

        // And the graph itself is untouched: `RunNode` has no room for any of
        // this, and inventing room would make the engine describe a projection
        // it never produced.
        let graph = rehydrate_run_graph(&wire_run(), wire);
        assert_eq!(graph.nodes.len(), 2);
    }

    /// A node the watchdog has an opinion about is projected even when the
    /// team recorded nothing else for it — a stuck node with no claimed task
    /// yet must still be visible.
    #[test]
    fn watchdog_facts_alone_are_enough_to_project_a_node() {
        let mut wire = wire_graph();
        wire.nodes[0].watchdog_interventions = 2;
        wire.nodes[0].attention = Some(crate::api::schema::WorkflowAttention::Stuck);

        let facts = projected_facts(&wire);
        let stuck = facts
            .get("plan")
            .expect("watchdog facts alone still project");
        assert_eq!(stuck.watchdog_interventions, 2);
        assert_eq!(
            stuck.attention,
            Some(crate::api::schema::WorkflowAttention::Stuck)
        );
        assert!(stuck.owner.is_empty(), "nothing else was observed for it");
    }

    /// An edge naming a node the projection does not carry is dropped, not
    /// pointed at nothing.
    #[test]
    fn an_edge_to_a_missing_node_is_dropped() {
        let mut wire = wire_graph();
        wire.edges.push(WorkflowRunEdgeInfo {
            from: "plan".to_string(),
            to: "gone".to_string(),
            kind: WorkflowEdgeKind::Sequence,
            condition_result: None,
            fired: false,
        });
        let graph = rehydrate_run_graph(&wire_run(), wire);
        assert_eq!(graph.edges.len(), 1);
    }

    /// A past run's panes are gone, so no node carries a binding — otherwise
    /// `Enter` would offer to focus a pane that has not existed for days.
    #[test]
    fn a_historical_node_never_carries_a_live_binding() {
        let graph = rehydrate_run_graph(&wire_run(), wire_graph());
        for node in &graph.nodes {
            assert!(node.binding.is_none(), "{:?}", node.path);
        }
    }

    /// Restore provenance survives the round trip: it is the one fact that
    /// explains why a node has a result it never computed (§4 D4).
    #[test]
    fn restore_provenance_survives_the_projection() {
        let mut wire = wire_graph();
        wire.nodes[0].status = WorkflowNodeStatus::Restored;
        wire.nodes[0].evidence = Some(WorkflowEvidence::Restored);
        wire.nodes[0].restored_from = Some(WorkflowRestoredFrom {
            run_id: "workflow_run:older".to_string(),
            node_key: "plan".to_string(),
            checkpoint_seq: 7,
        });
        let graph = rehydrate_run_graph(&wire_run(), wire);
        assert_eq!(graph.nodes[0].status, NodeStatus::Restored);
        let source = graph.nodes[0].restored_from.as_ref().expect("provenance");
        assert_eq!(source.run.as_str(), "workflow_run:older");
        assert_eq!(source.node_key.as_str(), "plan");
        assert_eq!(source.checkpoint_seq, 7);
        assert_eq!(
            graph.nodes[0].result.as_ref().map(|result| result.evidence),
            Some(Evidence::Restored)
        );
    }

    /// Model and effort come back through the same serde mapping the outbound
    /// direction uses, and an unrecognised value degrades one node instead of
    /// failing the run.
    #[test]
    fn assignments_round_trip_and_degrade_rather_than_fail() {
        let graph = rehydrate_run_graph(&wire_run(), wire_graph());
        assert_eq!(graph.nodes[0].assignment.model, ModelAlias::Opus);
        assert_eq!(graph.nodes[0].assignment.effort, Effort::High);
        assert_eq!(graph.nodes[0].assignment_reason, "measured");

        let mut wire = wire_graph();
        wire.nodes[0].model = "a-model-from-the-future".to_string();
        wire.nodes[0].effort = "ludicrous".to_string();
        let graph = rehydrate_run_graph(&wire_run(), wire);
        assert_eq!(graph.nodes[0].assignment.model, ModelAlias::Sonnet);
        assert_eq!(graph.nodes[0].assignment.effort, Effort::Low);
    }

    /// The wire's `u32` ceilings saturate into `u16` rather than wrapping: a
    /// projection that turned `70_000` into `4_464` would report a limit the
    /// run never had.
    #[test]
    fn oversized_counts_saturate_instead_of_wrapping() {
        let mut run = wire_run();
        run.max_nodes = 70_000;
        run.max_depth = 70_000;
        let mut wire = wire_graph();
        wire.nodes[0].depth = 70_000;
        let graph = rehydrate_run_graph(&run, wire);
        assert_eq!(graph.growth.max_nodes, u16::MAX);
        assert_eq!(graph.growth.max_depth, u16::MAX);
        assert_eq!(graph.nodes[0].depth, u16::MAX);
    }

    // ── the interrogate keys ────────────────────────────────────────────────

    fn dag_with(paths: &[&str]) -> DagViewState {
        let mut dag = DagViewState {
            run_id: "workflow_run:past".to_string(),
            ..DagViewState::default()
        };
        dag.nodes = paths
            .iter()
            .enumerate()
            .map(|(position, path)| crate::app::state::DagNodeView {
                idx: RunNodeIdx(position),
                path: (*path).to_string(),
                label: (*path).to_string(),
                status: NodeStatus::Succeeded,
                model: "opus".to_string(),
                effort: "high".to_string(),
                attempt: 1,
                usage: NodeUsage::default(),
                duration_ms: 0,
                summary: None,
                delivery_failure: None,
                growth_notice: None,
                depth: 0,
                parent: None,
                blocker: None,
                pane_id: None,
                owner: String::new(),
                subject: String::new(),
                emergent: false,
                owner_pane_id: None,
                agent_state: None,
                successors: Vec::new(),
                predecessors: Vec::new(),
                attention: None,
                watchdog_interventions: 0,
            })
            .collect();
        dag.selected = Some(RunNodeIdx(0));
        dag
    }

    fn interrogated_ok() -> String {
        r#"{"id":"1","type":"success","result":{"type":"workflow_node_interrogated",
            "interrogation":{"id":"interrogation:1","run_id":"workflow_run:past","path":"plan",
            "source_session_id":"sid","forked_session_id":null,"pane_id":"pane-2",
            "reconstructed":false,"transcript_path":null,"cwd":"/tmp",
            "started_at_unix_ms":1,"ended_at_unix_ms":null,"note":""}}}"#
            .to_string()
    }

    fn refusal(code: &str, message: &str) -> String {
        serde_json::json!({
            "id": "1",
            "type": "error",
            "error": { "code": code, "message": message },
        })
        .to_string()
    }

    /// The key chooses the mode; nothing is remembered between presses.
    #[test]
    fn the_key_chooses_the_mode() {
        let dag = dag_with(&["plan", "impl"]);
        assert_eq!(
            interrogate_intent(&dag, WorkflowInterrogationMode::Resumed),
            InterrogateIntent::Send {
                run_id: "workflow_run:past".to_string(),
                path: "plan".to_string(),
                mode: WorkflowInterrogationMode::Resumed,
            }
        );
        assert_eq!(
            interrogate_intent(&dag, WorkflowInterrogationMode::Reconstructed),
            InterrogateIntent::Send {
                run_id: "workflow_run:past".to_string(),
                path: "plan".to_string(),
                mode: WorkflowInterrogationMode::Reconstructed,
            }
        );
    }

    /// The intent always names the *selected* node, so an escalation can never
    /// land on a node other than the one on screen.
    #[test]
    fn the_intent_names_the_selected_node() {
        let mut dag = dag_with(&["plan", "impl"]);
        dag.selected = Some(RunNodeIdx(1));
        assert_eq!(
            interrogate_intent(&dag, WorkflowInterrogationMode::Reconstructed),
            InterrogateIntent::Send {
                run_id: "workflow_run:past".to_string(),
                path: "impl".to_string(),
                mode: WorkflowInterrogationMode::Reconstructed,
            }
        );
    }

    #[test]
    fn interrogate_is_ignored_with_no_run_and_with_no_selection() {
        let mut dag = dag_with(&["plan"]);
        dag.selected = None;
        assert_eq!(
            interrogate_intent(&dag, WorkflowInterrogationMode::Resumed),
            InterrogateIntent::Ignore
        );

        let mut dag = dag_with(&["plan"]);
        dag.run_id.clear();
        assert_eq!(
            interrogate_intent(&dag, WorkflowInterrogationMode::Resumed),
            InterrogateIntent::Ignore
        );
    }

    /// `i` on a node with no transcript names the key that *does* work, and
    /// that notice is the entire mechanism — there is no hidden state behind
    /// it, which is why it still works under out-of-band toast delivery.
    #[test]
    fn a_missing_transcript_points_at_shift_i() {
        let outcome = interrogate_outcome(
            "plan",
            WorkflowInterrogationMode::Resumed,
            &refusal(
                TRANSCRIPT_UNAVAILABLE_CODE,
                "no transcript at /home/x/.claude/projects/p/sid.jsonl",
            ),
        );
        let InterrogateOutcome::OfferReconstructed(message) = outcome else {
            panic!("an unavailable transcript must point at the reconstruction key");
        };
        assert!(message.contains("Shift+I"), "{message}");
        assert!(message.contains("no transcript at"), "{message}");

        // And the escalation itself opens a pane.
        assert_eq!(
            interrogate_outcome(
                "plan",
                WorkflowInterrogationMode::Reconstructed,
                &interrogated_ok()
            ),
            InterrogateOutcome::Opened
        );
    }

    /// A reconstruction that *also* has nothing to work from is a plain
    /// refusal. Pointing at `Shift+I` again would send the user in a circle on
    /// a node that can never be interrogated.
    #[test]
    fn a_failed_reconstruction_is_refused_not_offered_again() {
        let outcome = interrogate_outcome(
            "plan",
            WorkflowInterrogationMode::Reconstructed,
            &refusal(TRANSCRIPT_UNAVAILABLE_CODE, "no stored checkpoint"),
        );
        assert_eq!(
            outcome,
            InterrogateOutcome::Refused("no stored checkpoint".to_string())
        );
    }

    /// Every other refusal is shown as-is: an active interrogation, a pruned
    /// run, and a command-runner node are all facts the user needs, and none
    /// of them is fixed by reconstructing anything.
    #[test]
    fn other_refusals_are_shown_without_an_offer() {
        for (code, message) in [
            (
                "workflow_interrogation_active",
                "already interrogating plan",
            ),
            ("workflow_run_pruned", "history pruned"),
            ("workflow_unavailable", "this build has no workflow store"),
        ] {
            assert_eq!(
                interrogate_outcome(
                    "plan",
                    WorkflowInterrogationMode::Resumed,
                    &refusal(code, message)
                ),
                InterrogateOutcome::Refused(message.to_string()),
                "{code}"
            );
        }
        // An answer that is neither a success nor a decodable error still says
        // something rather than nothing.
        let outcome = interrogate_outcome("plan", WorkflowInterrogationMode::Resumed, "not json");
        assert_eq!(
            outcome,
            InterrogateOutcome::Refused("could not interrogate plan".to_string())
        );
    }

    /// A box is drawn live only when there is a pane to click. A row whose
    /// `ended_at` was never stamped — the server restarted and took the pane
    /// with it, so pane-death never fired — must still read as ended, or the
    /// lane promises a click that does nothing.
    #[cfg(feature = "workflow")]
    #[test]
    fn an_interrogation_is_live_only_while_its_pane_is() {
        use crate::workflow::store::InterrogationRecord;

        let record = |ended_at: Option<u64>, reconstructed: bool| InterrogationRecord {
            id: crate::workflow::model::InterrogationId::new("interrogation:1"),
            path: InstancePath::new("plan"),
            source_session_id: "sid".to_string(),
            forked_session_id: None,
            transcript_path: None,
            cwd: "/tmp".to_string(),
            pane_id: Some("w1:p7".to_string()),
            reconstructed,
            note: String::new(),
            started_at_unix_ms: 1,
            ended_at_unix_ms: ended_at,
        };

        // Live row, live pane.
        let row = interrogation_row(record(None, false), Some("w1:p7".to_string()));
        assert!(!row.ended);
        assert_eq!(row.pane_id.as_deref(), Some("w1:p7"));
        assert_eq!(row.path, "plan");
        assert!(!row.reconstructed);

        // Live row, dead pane — the orphan a restart leaves behind.
        let row = interrogation_row(record(None, false), None);
        assert!(row.ended, "no pane to click means the box is not live");
        assert_eq!(row.pane_id, None);

        // Ended row keeps its ended-ness whatever the pane says.
        let row = interrogation_row(record(Some(9), true), Some("w1:p7".to_string()));
        assert!(row.ended);
        assert!(row.reconstructed, "the degraded kind must survive the map");
    }

    /// m7: a past run is refused by the *not-the-active-run* guard, never by
    /// `workflow_run_closed`, and the client-side wording has to match.
    #[test]
    fn the_steer_refusal_names_the_right_reason() {
        let refusal = historical_steer_refusal();
        assert!(refusal.contains("past run"), "{refusal}");
        assert!(!refusal.contains("closed"), "{refusal}");
    }

    /// Every node status has a projection — the compiler enforces the match,
    /// and this pins that no arm silently collapses two statuses into one.
    #[test]
    fn every_wire_node_status_maps_to_a_distinct_engine_status() {
        let all = [
            (WorkflowNodeStatus::Pending, NodeStatus::Pending),
            (WorkflowNodeStatus::Ready, NodeStatus::Ready),
            (WorkflowNodeStatus::Running, NodeStatus::Running),
            (
                WorkflowNodeStatus::NeedsAttention,
                NodeStatus::NeedsAttention,
            ),
            (WorkflowNodeStatus::Blocked, NodeStatus::Blocked),
            (WorkflowNodeStatus::Succeeded, NodeStatus::Succeeded),
            (WorkflowNodeStatus::Failed, NodeStatus::Failed),
            (WorkflowNodeStatus::Skipped, NodeStatus::Skipped),
            (WorkflowNodeStatus::Restored, NodeStatus::Restored),
            (WorkflowNodeStatus::Cancelled, NodeStatus::Cancelled),
        ];
        for (wire, engine) in all {
            assert_eq!(node_status(wire), engine, "{wire:?}");
        }
    }

    // ── the DAG's `V` key and the review refresh seam (packet P13) ─────────

    fn test_app() -> App {
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        )
    }

    /// Opens the DAG on a bare lead-run snapshot for `run_id`, without ever
    /// having actually run anything server-side — enough to exercise the
    /// `V` key's dispatch and `refresh_open_dag_review`'s gating without the
    /// cost of a full review cycle (panes, interviews, synthesis).
    fn open_dag_on(app: &mut App, run_id: &str) {
        app.state.mode = Mode::WorkflowDag;
        app.state.set_historical_run(Some(HistoricalRunSnapshot {
            graph: Box::new(RunGraph {
                run_id: RunId::new(run_id.to_string()),
                version_id: KvdagVersionId::new("kvdag_version:1"),
                tier: Tier::Auto,
                growth: GrowthLimits::default(),
                assignments: BTreeMap::new(),
                nodes: Vec::new(),
                edges: Vec::new(),
                status: RunStatus::Succeeded,
                seq: 0,
                epilogue: None,
            }),
            workflow_name: "demo".to_string(),
            interrogations: Vec::new(),
            team_name: Some("session-213aa9bf".to_string()),
            lead_pane_id: None,
            projected: BTreeMap::new(),
            members: Vec::new(),
            review: None,
            review_findings: Vec::new(),
        }));
    }

    #[test]
    fn refresh_open_dag_review_no_ops_outside_dag_mode() {
        let mut app = test_app();
        assert!(!app.refresh_open_dag_review("workflow_run:none"));
    }

    #[test]
    fn refresh_open_dag_review_no_ops_for_a_run_that_is_not_open() {
        let mut app = test_app();
        open_dag_on(&mut app, "workflow_run:a");
        assert!(!app.refresh_open_dag_review("workflow_run:b"));
    }

    #[test]
    fn refresh_open_dag_review_reloads_the_open_run_even_with_no_server_side_cycle() {
        let mut app = test_app();
        open_dag_on(&mut app, "workflow_run:a");
        assert!(app.refresh_open_dag_review("workflow_run:a"));
        assert!(
            app.state
                .historical_run()
                .expect("still open")
                .review
                .is_none(),
            "no cycle on this server, so the honest answer is none"
        );
    }

    #[test]
    fn v_does_nothing_with_no_dag_run_open() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowDag;
        app.handle_workflow_dag_review_key();
        assert_eq!(app.state.mode, Mode::WorkflowDag);
    }

    #[test]
    fn v_does_nothing_while_a_review_is_already_running() {
        let mut app = test_app();
        open_dag_on(&mut app, "workflow_run:a");
        app.state.view.dag.lead_run = true;
        app.state.view.dag.run_status = Some(RunStatus::Succeeded);
        app.state.view.dag.review_status = Some(crate::api::schema::WorkflowReviewStatus::Running);
        app.handle_workflow_dag_review_key();
        assert_eq!(
            app.state.mode,
            Mode::WorkflowDag,
            "a running cycle offers nothing new to press"
        );
    }

    #[test]
    fn v_does_nothing_on_a_run_that_has_not_finished() {
        let mut app = test_app();
        open_dag_on(&mut app, "workflow_run:a");
        app.state.view.dag.lead_run = true;
        app.state.view.dag.run_status = Some(RunStatus::Running);
        app.state.view.dag.review_status = None;
        app.handle_workflow_dag_review_key();
        assert_eq!(app.state.mode, Mode::WorkflowDag);
    }

    #[test]
    fn v_opens_the_findings_overlay_when_the_cached_status_says_awaiting_user() {
        let mut app = test_app();
        open_dag_on(&mut app, "workflow_run:a");
        app.state.view.dag.lead_run = true;
        app.state.view.dag.run_status = Some(RunStatus::Succeeded);
        app.state.view.dag.review_status =
            Some(crate::api::schema::WorkflowReviewStatus::AwaitingUser);
        app.handle_workflow_dag_review_key();
        assert_eq!(app.state.mode, Mode::WorkflowReview);
    }
}
