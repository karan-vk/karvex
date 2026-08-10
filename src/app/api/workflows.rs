//! `workflow.*` JSON API handlers.
//!
//! `docs/design/workflow-builder/05-phase-plan.md` W3: every `workflow.*`
//! method is routed here so the primary dispatch in `src/app/api.rs` never
//! falls through to its `not_implemented` catch-all (`05` §6).
//!
//! Wire params/results are declared in `src/api/schema/workflows.rs` and are
//! deliberately self-contained (no `crate::workflow::*`), so the schema
//! artifact has one canonical value with the feature on or off. Every
//! conversion between the wire vocabulary and the engine's own lives here,
//! behind `#[cfg(feature = "workflow")]`.
//!
//! **Where each method's answer comes from.** Definitions, versions, and the
//! run index are the store's (`03`); the *live* run is the engine's, and the
//! engine's in-memory graph is authoritative while a run is executing (`04`
//! §9). So `run.get`/`node.get` answer from `App` for the active run and fall
//! back to the store for any other, and the mutating node methods only ever
//! address the active run.

#[cfg(feature = "workflow")]
use std::collections::BTreeMap;
#[cfg(feature = "workflow")]
use std::path::PathBuf;

#[cfg(feature = "workflow")]
use crate::api::schema::{
    ErrorBody, KvdagEdgeInfo, KvdagNodeInfo, KvdagVersionDetail, KvdagVersionSummary,
    ResponseResult, WorkflowArgSpec, WorkflowDefinitionFormat, WorkflowDetail, WorkflowEdgePayload,
    WorkflowExpandRejection, WorkflowExpandRejectionReason, WorkflowGrowthLimit,
    WorkflowGrowthLimitKind, WorkflowIsolation, WorkflowNodeKind, WorkflowRunEdgeInfo,
    WorkflowRunGraph, WorkflowRunInfo, WorkflowRunNodeInfo, WorkflowRunner, WorkflowSummary,
    WorkflowTier, WorkflowVersionOrigin,
};
use crate::api::schema::{
    WorkflowCreateParams, WorkflowNodeExpandParams, WorkflowNodeInterrogateParams,
    WorkflowNodeReportParams, WorkflowNodeSteerParams, WorkflowNodeTarget, WorkflowRunFinishParams,
    WorkflowRunListParams, WorkflowRunParams, WorkflowRunTarget, WorkflowSummaryListParams,
    WorkflowTarget, WorkflowVersionCreateParams, WorkflowVersionTarget,
};
#[cfg(feature = "workflow")]
use crate::api::schema::{
    WorkflowInterrogationMode, WorkflowRestoreReport, WorkflowRestoreSkip,
    WorkflowRestoreSkipReason, WorkflowRestoredFrom,
};
#[cfg(feature = "workflow")]
use crate::app::workflow::{
    wire_blocker, wire_demand, wire_edge_kind, wire_evidence, wire_node_status, wire_run_status,
    wire_run_summary_record, wire_succession, wire_tier, InterrogationRequest, InterrogationSeed,
};
#[cfg(feature = "workflow")]
use crate::app::workflow_store::StoreUnavailable;
use crate::app::App;
#[cfg(feature = "workflow")]
use crate::workflow::binding::observe::ReportRejected;
#[cfg(feature = "workflow")]
use crate::workflow::definition::{Definition, DefinitionError};
#[cfg(feature = "workflow")]
use crate::workflow::engine::expand::{
    self, ExpandLimit, ExpandOutcome, ExpandProposal, ExpandRejection,
};
#[cfg(feature = "workflow")]
use crate::workflow::engine::graph::{narrow_growth, resolve_assignments};
#[cfg(feature = "workflow")]
use crate::workflow::engine::{is_closed_run, ReportOutcome, ReportVerdict};
#[cfg(feature = "workflow")]
use crate::workflow::model::{
    is_reserved_path, CheckpointKind, EdgePayload, EngineInput, GrowthLimits, InstancePath,
    Isolation, Kvdag, KvdagEdge, KvdagNode, KvdagVersionId, NodeKey, NodeKind, NodeStatus,
    NodeToken, RestoredRef, RestoredSeed, RunId, RunStatus, Runner, WorkflowId,
};
#[cfg(feature = "workflow")]
use crate::workflow::store::error::WORKFLOW_NAME_TAKEN_CODE;
#[cfg(feature = "workflow")]
use crate::workflow::store::{
    NewRun, StoreError, StoredGrowthLimits, VersionMetadata, VersionOrigin, VersionRecord,
};
#[cfg(feature = "workflow")]
use crate::workflow::tier::{HistoryIndex, Tier};

use super::responses::encode_error;
#[cfg(feature = "workflow")]
use super::responses::{encode_error_body, encode_success};

/// Error code returned for every `workflow.*` call when the crate is built
/// with `--no-default-features` (the `workflow` cargo feature off). `workflow`
/// is in `default`, so every shipped `kvx-<target>` binary has the subsystem;
/// this path exists for the MSVC cross-lint and slim source builds.
#[cfg(not(feature = "workflow"))]
const WORKFLOW_UNAVAILABLE_CODE: &str = "workflow_unavailable";
/// The message names the build that produced it, because a server answering
/// this way was deliberately built without the default feature set.
#[cfg(not(feature = "workflow"))]
const WORKFLOW_UNAVAILABLE_MESSAGE: &str =
    "the workflow feature is not compiled into this server (built with --no-default-features); \
     rebuild with --features workflow";

/// The definition document could not be parsed, or the graph it describes
/// fails `Kvdag::try_new`'s construction invariants.
#[cfg(feature = "workflow")]
const INVALID_DEFINITION_CODE: &str = "workflow_invalid_definition";
/// No workflow, version, run, or node with that id.
#[cfg(feature = "workflow")]
const NOT_FOUND_CODE: &str = "workflow_not_found";
/// A `<name|id>` selector's name half matched more than one workflow.
/// `workflow_name` carries a UNIQUE index (`migrations/0001_init.surql`), so
/// this is a defensive backstop rather than a reachable path in the current
/// schema — matching `agent_target_ambiguous`'s convention in
/// `src/app/agents.rs` for the same "target might not be unique" shape.
#[cfg(feature = "workflow")]
const AMBIGUOUS_NAME_CODE: &str = "workflow_target_ambiguous";
/// A node method addressed a run this server is not currently executing.
/// Phase 1 executes one run at a time and only the live run is steerable.
#[cfg(feature = "workflow")]
const NO_ACTIVE_RUN_CODE: &str = "workflow_run_not_active";
/// A required run argument was not supplied and has no default.
#[cfg(feature = "workflow")]
const MISSING_ARG_CODE: &str = "workflow_missing_arg";
/// The lead's end-of-run report was malformed: no summary, both spellings of
/// it, or a summary file that could not be read
/// (`09-agent-teams-rework.md` §3.3).
#[cfg(feature = "workflow")]
const INVALID_ARGUMENT_CODE: &str = "workflow_invalid_argument";
/// A node method that delivers into the node's pane addressed a node that has
/// none. `05-phase-plan.md` W5 scopes steering and interrupting to a running
/// node; answering with a success the node never received would be worse than
/// refusing.
#[cfg(feature = "workflow")]
const NODE_NOT_RUNNING_CODE: &str = "workflow_node_not_running";
/// A steer or interrupt reached the node's pane path and the runtime refused to
/// write it. `04-kvdag-and-execution.md` §5 makes these deliveries, not
/// requests: a control surface that answers `ok` for keystrokes the process
/// never saw is worse than one that fails loudly.
#[cfg(feature = "workflow")]
const DELIVERY_FAILED_CODE: &str = "workflow_node_delivery_failed";
/// A self-reported result the completion gate refused because it does not
/// validate against the node's `output_schema` (`04-kvdag-and-execution.md`
/// §4.3). The node's own process is the one that has to fix it, and its only
/// channel back is this response — answering `ok` for a result the engine just
/// rejected is what let a schema-invalid report stall a run silently.
#[cfg(feature = "workflow")]
const RESULT_INVALID_CODE: &str = "workflow_node_result_invalid";
/// A node method addressed a run that has already reached a final status.
/// A closed run will never settle again, so anything handed back to it —
/// a restart, a steer, an interrupt, or an expand proposal — is work nothing
/// will ever collect. Every one of them is refused rather than performed
/// (`06-phase2-plan.md` H2).
#[cfg(feature = "workflow")]
const RUN_CLOSED_CODE: &str = "workflow_run_closed";
/// The transcript an interrogation would resume is not reachable
/// (`07-phase3-plan.md` §3 rule 8). One code for every reason — a node that ran
/// as a command and never had a session, a transcript file that is gone, a
/// recorded cwd that no longer exists, a reconstruction with no checkpoint to
/// seed from — with the *reason* in the message text, matching the existing
/// single-code style. `03-storage-schema.md` §4.4's stat-first rule is what
/// makes this a structured answer instead of a pane that silently fails to
/// start.
///
/// `src/app/workflow_history.rs` carries its own copy of this literal rather
/// than importing this constant: that module's `interrogate_intent`/
/// `interrogate_outcome` compile unconditionally (its own module doc: only
/// `historical_interrogations` is `#[cfg(feature = "workflow")]`), and this
/// constant is not — it is gated off entirely in a `--no-default-features`
/// build, where an unconditional `use` of it would not compile. The two
/// literals' equality is asserted in
/// `all_workflow_error_codes_are_a_well_formed_disjoint_family` below so they
/// cannot silently diverge.
#[cfg(feature = "workflow")]
const TRANSCRIPT_UNAVAILABLE_CODE: &str = "workflow_transcript_unavailable";
/// The run's history has been pruned: only its `run_summary` row survives
/// (`03-storage-schema.md` §9, `07-phase3-plan.md` §4 D12). Restore and
/// interrogation are impossible, and `workflow.run.get` answers this rather
/// than a bare not-found so the surviving surface is *named*.
#[cfg(feature = "workflow")]
const RUN_PRUNED_CODE: &str = "workflow_run_pruned";
/// A `restore_from.nodes` selector matched no node in the **target** version.
/// A typo must not silently re-run a node the caller believed restored, which
/// is why this is a hard error while a selector that matches a node with no
/// usable checkpoint is only a reported skip (§4 D11).
#[cfg(feature = "workflow")]
const RESTORE_UNKNOWN_SELECTOR_CODE: &str = "workflow_restore_unknown_selector";
/// This node already has a live interrogation pane (§4 D7). Forking one
/// session twice concurrently is a footgun with no use case; sequential
/// re-interrogation is fine and each is its own record.
#[cfg(feature = "workflow")]
const INTERROGATION_ACTIVE_CODE: &str = "workflow_interrogation_active";
/// Every precondition passed but the interrogation's **pane** could not be
/// created (E-15).
///
/// Deliberately not `workflow_transcript_unavailable`: by the time this is
/// reachable the transcript and the recorded cwd have both been stat'd and
/// found, so telling the caller the transcript is unavailable would send them
/// to check a file that is sitting right there.
///
/// Deliberately not the node-spawn codes either, even though the underlying
/// failure is the same pane machinery: an interrogation is **not** a run node
/// anywhere (§4 D8), and answering `workflow.node.interrogate` with
/// `workflow_node_spawn_failed` would tell a client one of the run's nodes had
/// failed to start. One code with the reason in the message, matching the
/// single-code style the rest of this file uses.
#[cfg(feature = "workflow")]
const INTERROGATION_SPAWN_FAILED_CODE: &str = "workflow_interrogation_spawn_failed";

/// Default page size for `workflow.run.list`.
#[cfg(feature = "workflow")]
const DEFAULT_RUN_LIST_LIMIT: u32 = 50;

/// How many of a workflow's most recent closed runs `auto` measures a node
/// against (`04-kvdag-and-execution.md` §7.3 step 2's "last N runs"). Wide
/// enough for §7.3's own thresholds — "≥ 3 prior runs" for the downgrade and
/// "the last two runs" for the escalation — to be reachable without letting a
/// node's distant past outvote its recent record.
#[cfg(feature = "workflow")]
const NODE_HISTORY_WINDOW: usize = 10;

#[cfg(not(feature = "workflow"))]
impl App {
    /// Feature-off path (`05-phase-plan.md` W3 "Feature-off
    /// behaviour"): the schema types compile unconditionally, but with the
    /// `workflow` cargo feature off there is no engine to route to at all.
    fn workflow_unavailable(&mut self, id: String) -> String {
        encode_error(id, WORKFLOW_UNAVAILABLE_CODE, WORKFLOW_UNAVAILABLE_MESSAGE)
    }

    pub(super) fn handle_workflow_list(&mut self, id: String) -> String {
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_get(&mut self, id: String, target: WorkflowTarget) -> String {
        if let Some(error) = require_non_empty(&id, "workflow_id", &target.workflow_id) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_create(
        &mut self,
        id: String,
        params: WorkflowCreateParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "definition.text", &params.definition.text) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_version_create(
        &mut self,
        id: String,
        params: WorkflowVersionCreateParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "workflow_id", &params.workflow_id) {
            return error;
        }
        if let Some(error) = require_non_empty(&id, "definition.text", &params.definition.text) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_version_get(
        &mut self,
        id: String,
        target: WorkflowVersionTarget,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "version_id", &target.version_id) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_run(&mut self, id: String, params: WorkflowRunParams) -> String {
        if let Some(error) = require_non_empty(&id, "workflow_id", &params.workflow_id) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_run_finish(
        &mut self,
        id: String,
        params: WorkflowRunFinishParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &params.run_id) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_run_get(
        &mut self,
        id: String,
        target: WorkflowRunTarget,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &target.run_id) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_run_list(
        &mut self,
        id: String,
        _params: WorkflowRunListParams,
    ) -> String {
        // Both fields are optional from Phase 3 on (§4 D9: `workflow_id: None`
        // lists across every workflow), so there is nothing to validate before
        // the subsystem's own answer.
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_run_cancel(
        &mut self,
        id: String,
        target: WorkflowRunTarget,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &target.run_id) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_node_get(
        &mut self,
        id: String,
        target: WorkflowNodeTarget,
    ) -> String {
        if let Some(error) = require_node_target(&id, &target) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_node_steer(
        &mut self,
        id: String,
        params: WorkflowNodeSteerParams,
    ) -> String {
        if let Some(error) = require_steer_params(&id, &params) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_node_interrupt(
        &mut self,
        id: String,
        target: WorkflowNodeTarget,
    ) -> String {
        if let Some(error) = require_node_target(&id, &target) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_node_report(
        &mut self,
        id: String,
        params: WorkflowNodeReportParams,
    ) -> String {
        if let Some(error) = require_report_params(&id, &params) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_node_restart(
        &mut self,
        id: String,
        target: WorkflowNodeTarget,
    ) -> String {
        if let Some(error) = require_node_target(&id, &target) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_node_expand(
        &mut self,
        id: String,
        params: WorkflowNodeExpandParams,
    ) -> String {
        if let Some(error) = require_expand_params(&id, &params) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_node_interrogate(
        &mut self,
        id: String,
        params: WorkflowNodeInterrogateParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &params.run_id) {
            return error;
        }
        if let Some(error) = require_non_empty(&id, "path", &params.path) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_summary_get(
        &mut self,
        id: String,
        target: WorkflowRunTarget,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &target.run_id) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_summary_list(
        &mut self,
        id: String,
        _params: WorkflowSummaryListParams,
    ) -> String {
        // Both fields are optional (§4 D9: `workflow_id: None` lists across
        // every workflow), so there is nothing to validate before the
        // subsystem's own answer.
        self.workflow_unavailable(id)
    }
}

#[cfg(feature = "workflow")]
impl App {
    pub(super) fn handle_workflow_list(&mut self, id: String) -> String {
        let listed = match self.workflow_store.call(|cx| {
            let workflows = cx.block_on(cx.store().list_workflows())?;
            let mut summaries = Vec::with_capacity(workflows.len());
            for workflow in workflows {
                let head = head_version_summary(cx, workflow.head_version.as_ref())?;
                summaries.push(wire_workflow_summary(workflow, head.as_ref()));
            }
            Ok::<_, StoreError>(summaries)
        }) {
            Ok(Ok(workflows)) => workflows,
            Ok(Err(error)) => return self.store_error(id, &error),
            Err(unavailable) => return unavailable_response(id, &unavailable),
        };
        encode_success(id, ResponseResult::WorkflowList { workflows: listed })
    }

    pub(super) fn handle_workflow_get(&mut self, id: String, target: WorkflowTarget) -> String {
        if let Some(error) = require_non_empty(&id, "workflow_id", &target.workflow_id) {
            return error;
        }
        let selector = target.workflow_id.trim().to_string();
        let looked_up = self.workflow_store.call(move |cx| {
            let workflow_id = match resolve_workflow_selector(cx, &selector)? {
                WorkflowSelector::Found(workflow_id) => workflow_id,
                WorkflowSelector::NotFound => return Ok::<_, StoreError>(LookupResult::NotFound),
                WorkflowSelector::Ambiguous => return Ok(LookupResult::Ambiguous),
            };
            let Some(workflow) = cx.block_on(cx.store().get_workflow(&workflow_id))? else {
                return Ok(LookupResult::NotFound);
            };
            // Walks the immutable parent chain from the head down, so `versions`
            // is the workflow's whole observable history, not just the head —
            // `05-phase-plan.md`'s `workflow.get`: "one workflow + its version
            // chain summary".
            let records = cx.block_on(
                cx.store()
                    .list_version_chain(&workflow_id, workflow.head_version.as_ref()),
            )?;
            let versions: Vec<KvdagVersionSummary> =
                records.iter().map(wire_version_summary).collect();
            let head = versions.first().cloned();
            // The head document, so `workflow.get` can describe the graph the
            // workflow currently *is* and not only its revision list. A head
            // pointer that will not load is survivable — the workflow is still
            // listable and its version chain still readable — so the detail
            // arrives with empty node/edge/arg sets rather than failing the
            // whole call.
            let head_kvdag = match workflow.head_version.clone() {
                Some(version_id) => cx.block_on(cx.store().load_version(&version_id)).ok(),
                None => None,
            };
            Ok(LookupResult::Found((
                wire_workflow_summary(workflow, head.as_ref()),
                versions,
                head_kvdag,
            )))
        });
        match looked_up {
            Ok(Ok(LookupResult::Found((workflow, versions, head_kvdag)))) => {
                // H3 / §4 D16: **one** projection. The human renderer and the
                // `--json` path both read `detail`, so the two cannot describe
                // different field sets. `description` comes from the `workflow`
                // row, which `create_version_with_metadata` keeps equal to the
                // head document (H5) — `kvdag_version` has no `description`
                // column to read instead.
                let detail = workflow_detail(&workflow, &versions, head_kvdag.as_ref());
                encode_success(
                    id,
                    ResponseResult::WorkflowGet {
                        workflow,
                        versions,
                        detail: Some(detail),
                    },
                )
            }
            Ok(Ok(LookupResult::NotFound)) => encode_error(
                id,
                NOT_FOUND_CODE,
                format!("no workflow with id {}", target.workflow_id),
            ),
            Ok(Ok(LookupResult::Ambiguous)) => encode_error(
                id,
                AMBIGUOUS_NAME_CODE,
                format!("multiple workflows matched name {}", target.workflow_id),
            ),
            Ok(Err(error)) => self.store_error(id, &error),
            Err(unavailable) => unavailable_response(id, &unavailable),
        }
    }

    pub(super) fn handle_workflow_create(
        &mut self,
        id: String,
        params: WorkflowCreateParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "definition.text", &params.definition.text) {
            return error;
        }
        let definition = match parse_definition(&params.definition) {
            Ok(definition) => definition,
            Err(error) => return encode_error(id, INVALID_DEFINITION_CODE, error.to_string()),
        };
        // Before the first write, not after it. `create_workflow` and
        // `create_version` are two separate commits, so a graph the second one
        // rejects — a cycle, a duplicate node key, an edge naming a node that
        // does not exist — used to leave the first one behind as a version-less
        // workflow that permanently squatted the name, with no `workflow
        // delete` to recover it. Hoisting the store's own gate
        // (`Definition::validate_graph`, the same `Kvdag::try_new`) in front of
        // the first write is what makes this all-or-nothing; a
        // write-then-roll-back would still lose the race against a crash
        // between the two.
        if let Err(error) = definition.validate_graph() {
            return encode_error(id, INVALID_DEFINITION_CODE, error.to_string());
        }

        let name = definition.name.trim().to_string();
        // H5: this path holds the authored document, so the `workflow` row's
        // description/tier come from it — which is also what gives an adopted
        // row (below) the new document's metadata instead of the empty
        // placeholder the abandoned create left on it.
        let metadata = VersionMetadata {
            description: definition.description.clone(),
            default_tier: definition.tier(),
        };
        let created = self.workflow_store.call(move |cx| {
            let workflow_id = match create_target(cx, &definition)? {
                CreateTarget::Fresh(workflow_id) | CreateTarget::Adopted(workflow_id) => {
                    workflow_id
                }
                CreateTarget::NameTaken => return Ok::<_, StoreError>(CreateResult::NameTaken),
            };
            let kvdag = cx.block_on(cx.store().create_version_with_metadata(
                &workflow_id,
                VersionOrigin::Authored,
                "",
                definition.spec(&workflow_id),
                Some(&metadata),
            ))?;
            cx.block_on(cx.store().set_head_version(&workflow_id, &kvdag.version_id))?;
            let summary = cx
                .block_on(cx.store().get_workflow(&workflow_id))?
                .ok_or_else(|| StoreError::NotFound {
                    table: "workflow",
                    id: workflow_id.to_string(),
                })?;
            let version_record = cx
                .block_on(cx.store().get_version_record(&kvdag.version_id))?
                .ok_or_else(|| StoreError::NotFound {
                    table: "kvdag_version",
                    id: kvdag.version_id.to_string(),
                })?;
            Ok(CreateResult::Created((summary, version_record)))
        });

        match created {
            Ok(Ok(CreateResult::Created((summary, version_record)))) => {
                let version = wire_version_summary(&version_record);
                encode_success(
                    id,
                    ResponseResult::WorkflowCreated {
                        workflow: wire_workflow_summary(summary, Some(&version)),
                        version,
                    },
                )
            }
            Ok(Ok(CreateResult::NameTaken)) => encode_error(
                id,
                WORKFLOW_NAME_TAKEN_CODE,
                format!(
                    "a workflow named {name} already exists; \
                     pick another name, or author a new version of it \
                     (`kvx workflow update`, `workflow.version.create`)"
                ),
            ),
            Ok(Err(error)) => self.store_error(id, &error),
            Err(unavailable) => unavailable_response(id, &unavailable),
        }
    }

    pub(super) fn handle_workflow_version_create(
        &mut self,
        id: String,
        params: WorkflowVersionCreateParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "workflow_id", &params.workflow_id) {
            return error;
        }
        if let Some(error) = require_non_empty(&id, "definition.text", &params.definition.text) {
            return error;
        }
        let definition = match parse_definition(&params.definition) {
            Ok(definition) => definition,
            Err(error) => return encode_error(id, INVALID_DEFINITION_CODE, error.to_string()),
        };
        let selector = params.workflow_id.trim().to_string();
        let change_summary = params.change_summary.clone();
        // H5, caller side. `kvdag_version` carries neither `description` nor
        // `default_tier`, so an update that changed either used to leave
        // `workflow.get` reporting v1's metadata beside `head_version: 2`. The
        // store makes the mutable `workflow` row track its head; this is where
        // the head's metadata comes from (§4 D16/D17).
        let metadata = VersionMetadata {
            description: definition.description.clone(),
            default_tier: definition.tier(),
        };

        let created = self.workflow_store.call(move |cx| {
            let workflow_id = match resolve_workflow_selector(cx, &selector)? {
                WorkflowSelector::Found(workflow_id) => workflow_id,
                WorkflowSelector::NotFound => return Ok::<_, StoreError>(LookupResult::NotFound),
                WorkflowSelector::Ambiguous => return Ok(LookupResult::Ambiguous),
            };
            let kvdag = cx.block_on(cx.store().create_version_with_metadata(
                &workflow_id,
                VersionOrigin::Authored,
                &change_summary,
                definition.spec(&workflow_id),
                Some(&metadata),
            ))?;
            cx.block_on(cx.store().set_head_version(&workflow_id, &kvdag.version_id))?;
            let summary = cx
                .block_on(cx.store().get_workflow(&workflow_id))?
                .ok_or_else(|| StoreError::NotFound {
                    table: "workflow",
                    id: workflow_id.to_string(),
                })?;
            let version_record = cx
                .block_on(cx.store().get_version_record(&kvdag.version_id))?
                .ok_or_else(|| StoreError::NotFound {
                    table: "kvdag_version",
                    id: kvdag.version_id.to_string(),
                })?;
            Ok(LookupResult::Found((summary, version_record)))
        });

        match created {
            Ok(Ok(LookupResult::Found((summary, version_record)))) => {
                let version = wire_version_summary(&version_record);
                encode_success(
                    id,
                    ResponseResult::WorkflowVersionCreated {
                        workflow: wire_workflow_summary(summary, Some(&version)),
                        version,
                    },
                )
            }
            Ok(Ok(LookupResult::NotFound)) => encode_error(
                id,
                NOT_FOUND_CODE,
                format!("no workflow with id {}", params.workflow_id),
            ),
            Ok(Ok(LookupResult::Ambiguous)) => encode_error(
                id,
                AMBIGUOUS_NAME_CODE,
                format!("multiple workflows matched name {}", params.workflow_id),
            ),
            Ok(Err(error)) => self.store_error(id, &error),
            Err(unavailable) => unavailable_response(id, &unavailable),
        }
    }

    pub(super) fn handle_workflow_version_get(
        &mut self,
        id: String,
        target: WorkflowVersionTarget,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "version_id", &target.version_id) {
            return error;
        }
        let version_id = KvdagVersionId::new(target.version_id.trim().to_string());
        let loaded = self.workflow_store.call(move |cx| {
            let kvdag = cx.block_on(cx.store().load_version(&version_id))?;
            let record = cx
                .block_on(cx.store().get_version_record(&version_id))?
                .ok_or_else(|| StoreError::NotFound {
                    table: "kvdag_version",
                    id: version_id.to_string(),
                })?;
            Ok::<_, StoreError>((kvdag, record))
        });
        match loaded {
            Ok(Ok((kvdag, record))) => encode_success(
                id,
                ResponseResult::WorkflowVersionGet {
                    version: wire_version_detail(&kvdag, &record),
                },
            ),
            Ok(Err(StoreError::NotFound { .. })) => encode_error(
                id,
                NOT_FOUND_CODE,
                format!("no kvdag version with id {}", target.version_id),
            ),
            Ok(Err(error)) => self.store_error(id, &error),
            Err(unavailable) => unavailable_response(id, &unavailable),
        }
    }

    pub(super) fn handle_workflow_run(&mut self, id: String, params: WorkflowRunParams) -> String {
        if let Some(error) = require_non_empty(&id, "workflow_id", &params.workflow_id) {
            return error;
        }
        // Phase 1 executes one run at a time. Refusing here rather than after
        // `create_run` is what keeps a refused start from leaving an orphan
        // `workflow_run` row that no engine will ever advance.
        //
        // **The epilogue disjunct is not cosmetic** (`07-phase3-plan.md` M7). A
        // `Succeeded` run is not `is_live()`, so without it a `workflow.run`
        // arriving while the summariser is still working would pass this check,
        // `start()` a fresh engine, clear `node_tokens`, and silently orphan the
        // summariser's report — the summary lost with no surface at all.
        if self.workflow.is_live() || self.workflow.epilogue_pending() || self.lead_run_is_live() {
            let refused = crate::app::workflow::WorkflowStartError::RunInFlight;
            let message = if self.workflow.is_live() {
                self.workflow_run_in_flight_message()
            } else {
                "the previous run's end-of-run summary is still being written; \
                 it finishes or gives up on its own, and the next run can start then"
                    .to_string()
            };
            return encode_error(id, refused.code(), message);
        }
        let selector = params.workflow_id.trim().to_string();
        let requested_version = params.version;

        // The definition is resolved before the run row is created, so a run
        // whose graph is unusable never leaves a half-started record behind.
        let resolved = self.workflow_store.call(move |cx| {
            let workflow_id = match resolve_workflow_selector(cx, &selector)? {
                WorkflowSelector::Found(workflow_id) => workflow_id,
                WorkflowSelector::NotFound => return Ok::<_, StoreError>(LookupResult::NotFound),
                WorkflowSelector::Ambiguous => return Ok(LookupResult::Ambiguous),
            };
            let Some(summary) = cx.block_on(cx.store().get_workflow(&workflow_id))? else {
                return Ok(LookupResult::NotFound);
            };
            let version_id = match requested_version {
                Some(number) => cx.block_on(cx.store().find_version_id(&workflow_id, number))?,
                None => summary.head_version,
            };
            let Some(version_id) = version_id else {
                return Ok(LookupResult::NotFound);
            };
            let kvdag = cx.block_on(cx.store().load_version(&version_id))?;
            Ok(LookupResult::Found((
                summary.default_tier,
                summary.name,
                kvdag,
            )))
        });
        let (default_tier, workflow_name, kvdag) = match resolved {
            Ok(Ok(LookupResult::Found(resolved))) => resolved,
            Ok(Ok(LookupResult::NotFound)) => {
                return encode_error(
                    id,
                    NOT_FOUND_CODE,
                    format!(
                        "no runnable kvdag version for workflow {}",
                        params.workflow_id
                    ),
                )
            }
            Ok(Ok(LookupResult::Ambiguous)) => {
                return encode_error(
                    id,
                    AMBIGUOUS_NAME_CODE,
                    format!("multiple workflows matched name {}", params.workflow_id),
                )
            }
            Ok(Err(StoreError::NotFound { .. })) => {
                return encode_error(
                    id,
                    NOT_FOUND_CODE,
                    format!(
                        "no runnable kvdag version for workflow {}",
                        params.workflow_id
                    ),
                )
            }
            Ok(Err(error)) => return self.store_error(id, &error),
            Err(unavailable) => return unavailable_response(id, &unavailable),
        };

        let args = match resolve_run_args(&kvdag, &params.args) {
            Ok(args) => args,
            Err(message) => return encode_error(id, MISSING_ARG_CODE, message),
        };
        let tier = params.tier.map_or(default_tier, engine_tier);

        let workflow_id = kvdag.workflow_id.clone();
        let version_id = kvdag.version_id.clone();
        // §4 D4 / R-3: the tier narrows the version's ceilings, and the run row
        // has to persist the *effective* limits the `RunGraph` enforces —
        // `materialise_with` narrows the same way. Narrowing here is what keeps
        // a `--tier low` run's banner from contradicting its own database row.
        // `narrow_growth` is idempotent, so narrowing once and re-narrowing
        // downstream cannot drift.
        let growth = narrow_growth(kvdag.growth, tier);
        let ordered: BTreeMap<String, String> = args.clone().into_iter().collect();
        // Recorded on the run row at create time — a run's workspace binding is
        // a property of the run, not of whichever server happens to be
        // executing it, so a run read back from the journal keeps it too.
        let workspace_id = self.active_workspace_public_id();
        // §4 D15 / H1: stamped **once** here and handed to both `NewRun` (which
        // binds `workflow_run.started_at` explicitly) and `ActiveRun`, so the
        // run the journal describes and the run the live projection describes
        // start at the same instant.
        let started_at_unix_ms = unix_now_ms();
        // §4 D9: one resolver for the whole subsystem. The table is resolved
        // here, once, and handed to **both** `NewRun` (which
        // `materialise_run_nodes` writes verbatim) and `RunGraph::materialise_with`
        // — so the DAG view and the durable row cannot disagree about which
        // model a node ran on.
        let history = match self.node_history_index(&kvdag, tier) {
            Ok(history) => history,
            Err(response) => return response(id),
        };
        let assignments = resolve_assignments(&kvdag, tier, &history);

        // §3.1 step 5 / §4's last risk row. The preflight runs before
        // `create_run` for the same reason the definition resolution does: a
        // hard error after the row exists leaves an orphan run nothing will
        // ever advance. A `claude` too old for agent teams starts fine and
        // then silently never spawns a teammate, which is the failure this
        // turns into a clear message.
        if let Err(error) = self.preflight_claude_for_lead() {
            return encode_error(id, error.code(), error.to_string());
        }

        // §4 D11: restore is resolved **before** `create_run`, for the same
        // reason the definition is. An unknown selector is a hard error, and a
        // hard error after the row exists would leave an orphan `workflow_run`
        // no engine will ever advance.
        let restore = match params.restore_from.as_ref() {
            Some(request) => match self.resolve_restore(request, &kvdag) {
                Ok(plan) => Some(plan),
                Err(response) => return response(id),
            },
            None => None,
        };
        // §4 D21: absent means true. The default lives here, in the handler, and
        // exactly here — the wire type carries `Option<bool>` precisely so the
        // policy is not duplicated into every client.
        let include_prior_summaries = params.include_prior_summaries.unwrap_or(true);
        let prior_summaries = if include_prior_summaries {
            match self.prior_run_summaries(&kvdag.workflow_id) {
                Ok(summaries) => summaries,
                Err(response) => return response(id),
            }
        } else {
            Vec::new()
        };
        let context_runs: Vec<RunId> = prior_summaries
            .iter()
            .map(|summary| RunId::new(summary.run_id.clone()))
            .collect();
        let restore_from_run = restore.as_ref().map(|plan| plan.source.clone());
        let restore_request = restore.as_ref().map(|plan| plan.request.clone());
        let seeds = restore
            .as_ref()
            .map(|plan| plan.seeds.clone())
            .unwrap_or_default();

        let created = self.workflow_store.call({
            let workspace_id = workspace_id.clone();
            let assignments = assignments.clone();
            let context_runs = context_runs.clone();
            let restore_from = restore_request;
            let restored = seeds.clone();
            move |cx| {
                cx.block_on(cx.store().create_run(NewRun {
                    workflow: workflow_id,
                    version: version_id,
                    tier,
                    args: ordered,
                    growth,
                    started_at_unix_ms,
                    assignments,
                    context_runs,
                    workspace_id,
                    restore_from,
                    restored,
                }))
            }
        });
        let run_id = match created {
            Ok(Ok(run_id)) => run_id,
            Ok(Err(error)) => return self.store_error(id, &error),
            Err(unavailable) => return unavailable_response(id, &unavailable),
        };

        // The DAG overlay heads the run with the name the author gave the
        // workflow; the run graph itself only carries record ids.
        self.state.set_workflow_run_name(workflow_name.clone());

        // §4 D21 / §6 A8: one file in the run dir, written before the first node
        // spawns so its `task.md` pointer can never name a file that is not
        // there yet. A write failure is a warning, not a failed run — the run's
        // work does not depend on its history, and refusing to start over a
        // missing context file would be a worse answer than starting without it.
        let prior_runs_path = if prior_summaries.is_empty() {
            None
        } else {
            self.write_prior_runs_context(&run_id, &workflow_name, &prior_summaries)
        };

        // 09-agent-teams-rework.md §3.1. The run's execution is a Claude Code
        // team lead in a pane from here on, not this server's engine. The
        // engine's materialisation is deliberately not built: `create_run`
        // already wrote the planned `run_node` rows, and the projection
        // (§3.4) is what moves them.
        let _ = (&assignments, &seeds, &context_runs, &restore_from_run);
        let _ = (workspace_id, prior_runs_path);

        let ws_idx = match self.state.active.filter(|ws_idx| {
            self.state
                .workspaces
                .get(*ws_idx)
                .is_some_and(|workspace| !workspace.tabs.is_empty())
        }) {
            Some(ws_idx) => ws_idx,
            None => {
                let error = crate::workflow::binding::lead::LeadSpawnError::NoTargetPane;
                return encode_error(id, error.code(), error.to_string());
            }
        };
        let cwd = self.workflow_node_cwd_for(ws_idx);
        // The cwd is only known once the workspace is resolved, so this half of
        // the preflight lands here rather than beside the version check. It
        // still runs before anything is spawned.
        if let Err(error) = self.preflight_cwd_trust_for_lead(&cwd) {
            return encode_error(id, error.code(), error.to_string());
        }
        let run_dir = crate::workflow::binding::spawn::run_dir(
            &crate::workflow::binding::spawn::runs_root(),
            &run_id,
        );
        // The lead orchestrates rather than implements, so it is resolved at
        // the run tier's `critical` row: the judgment calls it makes about
        // splitting, merging, and finishing the plan are the expensive ones.
        let lead_assignment =
            crate::workflow::tier::resolve(tier, crate::workflow::model::Demand::Critical, None);
        let spec = crate::workflow::binding::lead::LeadSpawnSpec {
            run_id: run_id.clone(),
            workflow_name: workflow_name.clone(),
            run_dir,
            cwd,
            assignment: lead_assignment,
        };
        let summary_path = spec.summary_path().to_string_lossy().into_owned();
        let ordered_args: std::collections::BTreeMap<String, String> =
            args.clone().into_iter().collect();
        let prompt = crate::workflow::lead_prompt::render_lead_prompt(
            &crate::workflow::lead_prompt::LeadPromptInput {
                run_id: &run_id,
                workflow_name: &workflow_name,
                kvdag: &kvdag,
                tier,
                args: &ordered_args,
                history: &history,
                summary_path: &summary_path,
            },
        );
        if let Err(error) = self.write_lead_prompt(&spec, &prompt) {
            return encode_error(id, error.code(), error.to_string());
        }
        let (lead_pane_id, lead_terminal_id) = match self.spawn_lead_pane(ws_idx, &spec) {
            Ok(spawned) => spawned,
            Err(error) => return encode_error(id, error.code(), error.to_string()),
        };
        self.bind_lead_run(
            run_id.clone(),
            &kvdag,
            &spec,
            lead_pane_id,
            lead_terminal_id,
            started_at_unix_ms,
        );
        // `create_run` writes `pending`; the lead is live the moment its pane
        // is, and nothing else will move the row off `pending` now that no
        // engine is watching it.
        self.mark_lead_run_running(&run_id);

        match self.stored_run(&run_id) {
            Ok(Some((run, _graph))) => encode_success(
                id,
                ResponseResult::WorkflowRunStarted {
                    run,
                    restore: restore.map(|plan| plan.report),
                },
            ),
            Ok(None) => encode_error(
                id,
                NOT_FOUND_CODE,
                "the run was created but cannot be read back",
            ),
            Err(response) => response(id),
        }
    }

    /// The team lead's self-report (`09-agent-teams-rework.md` §3.3).
    ///
    /// This one call replaces the entire summariser subsystem: the lead writes
    /// its own summary and says it is done, and karvex records both. There is
    /// no engine-judged verdict any more — if the run failed, the lead still
    /// calls this and says so in `outcome`, because the truth lives in the
    /// transcript one click away.
    ///
    /// Authorisation is possession of the run id, which karvex handed the lead
    /// through `KARVEX_WORKFLOW_RUN_ID` in its pane.
    pub(super) fn handle_workflow_run_finish(
        &mut self,
        id: String,
        params: WorkflowRunFinishParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &params.run_id) {
            return error;
        }
        let run_id = RunId::new(params.run_id.trim().to_string());

        let text = match (params.summary.as_deref(), params.summary_file.as_deref()) {
            (Some(text), None) => text.to_string(),
            (None, Some(path)) => match std::fs::read_to_string(path) {
                Ok(text) => text,
                Err(error) => {
                    return encode_error(
                        id,
                        INVALID_ARGUMENT_CODE,
                        format!("the summary file {path} could not be read: {error}"),
                    )
                }
            },
            (Some(_), Some(_)) => {
                return encode_error(
                    id,
                    INVALID_ARGUMENT_CODE,
                    "pass either --summary or --summary-file, not both",
                )
            }
            (None, None) => {
                return encode_error(
                    id,
                    INVALID_ARGUMENT_CODE,
                    "a run summary is required: pass --summary-file <path> or --summary <text>",
                )
            }
        };
        if text.trim().is_empty() {
            return encode_error(
                id,
                INVALID_ARGUMENT_CODE,
                "the run summary is empty; say what the run did before finishing it",
            );
        }
        let outcome = params
            .outcome
            .as_deref()
            .map(str::trim)
            .filter(|outcome| !outcome.is_empty())
            .unwrap_or("succeeded")
            .to_string();

        // The version is read off the run row rather than taken from the
        // caller: the summary outlives the run, and a lead that named the
        // wrong version would corrupt the per-workflow summary listing.
        let version = match self.stored_run(&run_id) {
            Ok(Some((run, _graph))) => KvdagVersionId::new(run.version_id),
            Ok(None) => {
                return encode_error(id, NOT_FOUND_CODE, format!("no run {run_id}"));
            }
            Err(response) => return response(id),
        };

        let ended_at_unix_ms = unix_now_ms();
        self.persist_lead_run_summary(&run_id, &version, text, outcome);
        self.finish_lead_run(&run_id, ended_at_unix_ms);

        let summary = match self.stored_run_summary_info(&run_id) {
            Some(summary) => summary,
            None => {
                return encode_error(
                    id,
                    NOT_FOUND_CODE,
                    "the run summary was accepted but cannot be read back",
                )
            }
        };
        self.emit_event(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::WorkflowRunSummarized,
            data: crate::api::schema::EventData::WorkflowRunSummarized {
                run_id: run_id.to_string(),
                summary: summary.clone(),
            },
        });
        match self.stored_run(&run_id) {
            Ok(Some((run, _graph))) => {
                self.emit_event(crate::api::schema::EventEnvelope {
                    event: crate::api::schema::EventKind::WorkflowRunFinished,
                    data: crate::api::schema::EventData::WorkflowRunFinished { run: run.clone() },
                });
                encode_success(id, ResponseResult::WorkflowRunFinished { run, summary })
            }
            Ok(None) => encode_error(id, NOT_FOUND_CODE, format!("no run {run_id}")),
            Err(response) => response(id),
        }
    }

    pub(super) fn handle_workflow_run_get(
        &mut self,
        id: String,
        target: WorkflowRunTarget,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &target.run_id) {
            return error;
        }
        let run_id = RunId::new(target.run_id.trim().to_string());
        if let (Some(run), Some(graph)) = (
            self.workflow_run_info(&run_id),
            self.workflow_run_graph_info(&run_id),
        ) {
            return encode_success(id, ResponseResult::WorkflowRunGet { run, graph });
        }
        // m9: a pruned run whose summary survives answers `workflow_run_pruned`
        // naming `workflow.summary.get`, not the bare not-found — the surviving
        // surface is stated rather than implied.
        match self.stored_run_or_pruned(&run_id) {
            Ok(Some((run, graph))) => {
                encode_success(id, ResponseResult::WorkflowRunGet { run, graph })
            }
            Ok(None) => encode_error(
                id,
                NOT_FOUND_CODE,
                format!("no run with id {}", target.run_id),
            ),
            Err(response) => response(id),
        }
    }

    pub(super) fn handle_workflow_run_list(
        &mut self,
        id: String,
        params: WorkflowRunListParams,
    ) -> String {
        // §4 D9: `None` — and, for the callers that have always sent it, an
        // empty string — lists across every workflow, newest first. Every
        // published client sends a workflow id, so widening the *request* field
        // is compatible in the direction clients actually use.
        let selector = params
            .workflow_id
            .map(|workflow_id| workflow_id.trim().to_string())
            .filter(|workflow_id| !workflow_id.is_empty());
        let limit = params.limit.unwrap_or(DEFAULT_RUN_LIST_LIMIT).max(1);
        // `kvx workflow run list <name|id>` takes the same selector as `show`,
        // `update`, and `run start`, so it resolves it the same way; listing a
        // workflow's runs by the name the user gave it must not be the one
        // verb that only speaks record ids.
        let unresolved = selector.clone().unwrap_or_default();
        let listed = self.workflow_store.call(move |cx| {
            let workflow_id = match selector {
                Some(selector) => match resolve_workflow_selector(cx, &selector)? {
                    WorkflowSelector::Found(workflow_id) => Some(workflow_id),
                    WorkflowSelector::NotFound => {
                        return Ok::<_, StoreError>(LookupResult::NotFound)
                    }
                    WorkflowSelector::Ambiguous => return Ok(LookupResult::Ambiguous),
                },
                None => None,
            };
            let records = cx.block_on(cx.store().list_runs(workflow_id.as_ref(), limit))?;
            // One batched query for the whole page rather than one per run:
            // `limit` is caller-supplied and uncapped, so a per-run call would
            // be an unbounded N+1. Without it `run.list` would report
            // `growth_limited: null` for the same run `run.get` reports a
            // limit for, which is the inconsistency B2 exists to remove.
            let ids: Vec<_> = records.iter().map(|record| record.id.clone()).collect();
            let limits = cx.block_on(cx.store().last_growth_limit_by_run(&ids))?;
            Ok(LookupResult::Found((records, limits)))
        });
        match listed {
            Ok(Ok(LookupResult::Found((records, limits)))) => {
                // The live run's authoritative status is the engine's, not the
                // journal's, so the in-memory projection wins where it applies.
                let runs = records
                    .into_iter()
                    .map(|record| {
                        self.workflow_run_info(&record.id).unwrap_or_else(|| {
                            let limits = StoredGrowthLimits {
                                last: limits.get(&record.id).cloned(),
                                by_path: BTreeMap::new(),
                            };
                            wire_run_record(record, &limits)
                        })
                    })
                    .collect();
                encode_success(id, ResponseResult::WorkflowRunList { runs })
            }
            Ok(Ok(LookupResult::NotFound)) => encode_error(
                id,
                NOT_FOUND_CODE,
                format!("no workflow with id {unresolved}"),
            ),
            Ok(Ok(LookupResult::Ambiguous)) => encode_error(
                id,
                AMBIGUOUS_NAME_CODE,
                format!("multiple workflows matched name {unresolved}"),
            ),
            Ok(Err(error)) => self.store_error(id, &error),
            Err(unavailable) => unavailable_response(id, &unavailable),
        }
    }

    pub(super) fn handle_workflow_run_cancel(
        &mut self,
        id: String,
        target: WorkflowRunTarget,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &target.run_id) {
            return error;
        }
        let run_id = RunId::new(target.run_id.trim().to_string());
        // §3.3: a lead run cancels by closing the lead's pane. Checked before
        // the engine path, because a lead run has no live `ActiveRun` and
        // would otherwise be refused as "not the run this server is
        // executing".
        if self.is_live_lead_run(&run_id) {
            self.cancel_lead_run(&run_id, unix_now_ms());
            return match self.stored_run(&run_id) {
                Ok(Some((run, _graph))) => {
                    encode_success(id, ResponseResult::WorkflowRunCancelled { run })
                }
                Ok(None) => encode_error(id, NOT_FOUND_CODE, format!("no run {run_id}")),
                Err(response) => response(id),
            };
        }
        if self.workflow_run_info(&run_id).is_none() {
            return encode_error(
                id,
                NO_ACTIVE_RUN_CODE,
                format!("run {} is not the run this server is executing", run_id),
            );
        }
        // B3: the same H2 closed-run guard `steer`/`interrupt`/`restart`/
        // `expand` already use. A run that is already closed will never
        // settle again, so cancelling it a second time answered `ok` with an
        // envelope literally named `workflow_run_cancelled` for a run whose
        // status stayed whatever it already was — a lie the retest flagged.
        if let Some(error) = self.require_open_run(&id, &run_id, "be cancelled") {
            return error;
        }
        self.cancel_workflow_run();
        match self.workflow_run_info(&run_id) {
            Some(run) => encode_success(id, ResponseResult::WorkflowRunCancelled { run }),
            None => encode_error(
                id,
                NO_ACTIVE_RUN_CODE,
                format!("run {} is no longer the active run", run_id),
            ),
        }
    }

    pub(super) fn handle_workflow_node_get(
        &mut self,
        id: String,
        target: WorkflowNodeTarget,
    ) -> String {
        if let Some(error) = require_node_target(&id, &target) {
            return error;
        }
        let run_id = RunId::new(target.run_id.trim().to_string());
        let path = InstancePath::new(target.path.trim().to_string());
        if self.workflow_run_info(&run_id).is_some() {
            return match self.workflow_node_info(&path) {
                Some(node) => encode_success(id, ResponseResult::WorkflowNodeGet { node }),
                None => node_not_found(id, &target),
            };
        }
        match self.stored_run(&run_id) {
            Ok(Some((_, graph))) => match graph
                .nodes
                .into_iter()
                .find(|node| node.path == path.as_str())
            {
                Some(node) => encode_success(id, ResponseResult::WorkflowNodeGet { node }),
                None => node_not_found(id, &target),
            },
            Ok(None) => encode_error(
                id,
                NOT_FOUND_CODE,
                format!("no run with id {}", target.run_id),
            ),
            Err(response) => response(id),
        }
    }

    pub(super) fn handle_workflow_node_steer(
        &mut self,
        id: String,
        params: WorkflowNodeSteerParams,
    ) -> String {
        if let Some(error) = require_steer_params(&id, &params) {
            return error;
        }
        let path = InstancePath::new(params.path.trim().to_string());
        self.apply_node_input(
            id,
            &params.run_id,
            &params.path,
            EngineInput::Steer {
                path: path.clone(),
                text: params.text.clone(),
            },
            &path,
            |node| ResponseResult::WorkflowNodeSteered { node },
        )
    }

    pub(super) fn handle_workflow_node_interrupt(
        &mut self,
        id: String,
        target: WorkflowNodeTarget,
    ) -> String {
        if let Some(error) = require_node_target(&id, &target) {
            return error;
        }
        let path = InstancePath::new(target.path.trim().to_string());
        self.apply_node_input(
            id,
            &target.run_id,
            &target.path,
            EngineInput::Interrupt { path: path.clone() },
            &path,
            |node| ResponseResult::WorkflowNodeInterrupted { node },
        )
    }

    pub(super) fn handle_workflow_node_report(
        &mut self,
        id: String,
        params: WorkflowNodeReportParams,
    ) -> String {
        if let Some(error) = require_report_params(&id, &params) {
            return error;
        }
        let run_id = RunId::new(params.run_id.trim().to_string());
        if self.workflow_run_info(&run_id).is_none() {
            return not_the_active_run(id, &params.run_id);
        }
        let path = InstancePath::new(params.path.trim().to_string());
        if let Err(rejected) = self.report_workflow_node(
            params.path.trim(),
            params.token.trim(),
            Some(params.result.clone()),
        ) {
            return encode_error(id, rejected.code(), rejected.message());
        }
        // The completion gate has already run. A result it refused on the node's
        // own `output_schema` must not come back as `workflow_node_reported`:
        // for a `Runner::Command` node the response *is* the corrective channel,
        // and a success envelope leaves the script believing it finished while
        // the node sits `Running` with nothing left to report.
        let rejection = self
            .workflow
            .engine()
            .report_outcome(&path)
            .filter(|outcome| !outcome.errors.is_empty())
            .map(describe_rejected_report);
        if let Some(message) = rejection {
            return encode_error(id, RESULT_INVALID_CODE, message);
        }
        match self.workflow_node_info(&path) {
            Some(node) => encode_success(id, ResponseResult::WorkflowNodeReported { node }),
            None => node_not_found(
                id,
                &WorkflowNodeTarget {
                    run_id: params.run_id,
                    path: params.path,
                },
            ),
        }
    }

    pub(super) fn handle_workflow_node_restart(
        &mut self,
        id: String,
        target: WorkflowNodeTarget,
    ) -> String {
        if let Some(error) = require_node_target(&id, &target) {
            return error;
        }
        // The closed-run guard is `apply_node_input`'s now (H2): a run that has
        // closed will never settle again, so a node handed back to it becomes a
        // live process inside a `cancelled`/`failed`/`succeeded` run that
        // nothing will ever collect a result from — and the same is true of a
        // steer or an interrupt delivered into it.
        let path = InstancePath::new(target.path.trim().to_string());
        self.apply_node_input(
            id,
            &target.run_id,
            &target.path,
            EngineInput::RestartNode { path: path.clone() },
            &path,
            |node| ResponseResult::WorkflowNodeRestarted { node },
        )
    }

    /// `workflow.node.expand` — a node proposing new nodes
    /// (`04-kvdag-and-execution.md` §3.4). **A node cannot create nodes; it
    /// proposes, and karvex decides.**
    ///
    /// Token-authenticated exactly like `workflow.node.report`, because an
    /// expand proposal is a node speaking and not an operator. A *rejected*
    /// proposal is a **successful** response carrying the rejection: the run
    /// continues, and the caller learns exactly which guardrail it hit. Only a
    /// bad run, path, or token — or a closed run — is an error.
    pub(super) fn handle_workflow_node_expand(
        &mut self,
        id: String,
        params: WorkflowNodeExpandParams,
    ) -> String {
        if let Some(error) = require_expand_params(&id, &params) {
            return error;
        }
        let run = RunId::new(params.run_id.trim().to_string());
        if self.workflow_run_info(&run).is_none() {
            return not_the_active_run(id, &params.run_id);
        }
        let path_text = params.path.trim().to_string();
        if let Some(error) =
            self.require_open_run(&id, &run, &format!("expand from node {path_text}"))
        {
            return error;
        }
        let path = InstancePath::new(path_text.clone());
        if self.workflow_node_info(&path).is_none() {
            return node_not_found(
                id,
                &WorkflowNodeTarget {
                    run_id: params.run_id.clone(),
                    path: params.path.clone(),
                },
            );
        }
        let token = match self.authenticate_node_token(&path_text, params.token.trim()) {
            Ok(token) => token,
            Err(rejected) => return encode_error(id, rejected.code(), rejected.message()),
        };
        // A node that has already closed cannot grow the graph: its children
        // would hang off a `sequence` edge from a settled parent. The engine
        // refuses it silently, and a silent refusal is exactly what §3.4
        // forbids — so the refusal is named here instead.
        if self
            .workflow
            .node(&path)
            .is_some_and(|node| node.status.is_terminal())
        {
            return encode_error(
                id,
                NODE_NOT_RUNNING_CODE,
                format!(
                    "node {path_text} has already closed; a closed node cannot propose new nodes"
                ),
            );
        }
        let proposal = match expand_proposal(&params) {
            Ok(proposal) => proposal,
            Err(message) => return encode_error(id, "invalid_params", message),
        };

        // The verdict is computed **before** the engine applies the same input.
        // `expand::evaluate` is pure and deterministic, and nothing mutates the
        // graph between these two lines, so the engine reaches the identical
        // outcome — this is one evaluator read twice, not a second policy. It
        // is read here because the engine reports its verdict through effects,
        // and this response is the proposing node's only channel back.
        let Some((outcome, growth, expand_max)) = self.evaluate_expansion(&path, &proposal) else {
            return encode_error(
                id,
                NO_ACTIVE_RUN_CODE,
                format!("run {run} has no installed definition to judge a proposal against"),
            );
        };

        self.apply_workflow_engine_input(EngineInput::ExpandProposed {
            path: path.clone(),
            token,
            proposals: vec![proposal.clone()],
        });

        // Reported children are confirmed against the graph the engine actually
        // produced, so the response can never claim a node that does not exist.
        let mut accepted = Vec::with_capacity(outcome.accepted.len());
        for child in &outcome.accepted {
            let exists = self
                .workflow
                .graph()
                .is_some_and(|graph| graph.index_of(&child.path).is_some());
            if exists {
                accepted.push(child.path.to_string());
            } else {
                tracing::warn!(
                    path = %child.path,
                    "expansion child was accepted but is not in the run graph"
                );
            }
        }
        let at_unix_ms = unix_now_ms();
        let rejected = outcome
            .rejected
            .iter()
            .map(|rejection| {
                wire_expand_rejection(rejection, &proposal, growth, expand_max, at_unix_ms)
            })
            .collect();
        encode_success(
            id,
            ResponseResult::WorkflowNodeExpanded { accepted, rejected },
        )
    }

    /// `workflow.node.interrogate` (`07-phase3-plan.md` §WS-D, §4 D7).
    ///
    /// Revives a **finished** node's Claude session in a pane, forked so the
    /// source transcript is never mutated. The whole point of the precondition
    /// ladder below is `00-overview.md` Feature 3's guarantee: the caller
    /// either gets a working pane or a structured refusal that says which fact
    /// was missing — **never** a pane that silently fails to start. Every
    /// refusal path returns before anything is created, so a refused call
    /// leaves the workspace with exactly as many panes as it had.
    pub(super) fn handle_workflow_node_interrogate(
        &mut self,
        id: String,
        params: WorkflowNodeInterrogateParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &params.run_id) {
            return error;
        }
        if let Some(error) = require_non_empty(&id, "path", &params.path) {
            return error;
        }
        let run_id = RunId::new(params.run_id.trim().to_string());
        let path = InstancePath::new(params.path.trim().to_string());
        let reconstructed = matches!(params.mode, WorkflowInterrogationMode::Reconstructed);
        let note = params.note.unwrap_or_default();

        // 1. The run. A pruned run has no `workflow_run` row at all, so this is
        //    also where "summary-only" is distinguished from "never existed":
        //    the caller is pointed at the surface that survived rather than
        //    told the run is unknown.
        let (run, node) = match self.interrogation_target(&run_id, &path) {
            Ok(Some(target)) => target,
            Ok(None) => {
                return node_not_found(
                    id,
                    &WorkflowNodeTarget {
                        run_id: params.run_id.clone(),
                        path: params.path.clone(),
                    },
                )
            }
            Err(response) => return response(id),
        };

        // 2. A session to fork.
        //
        //    The gate is the node's **runner**, not the presence of an
        //    `agent_session_id`. `07-phase3-plan.md` §WS-D says "a `runner:
        //    command` node does not [have one]", and that premise does not hold
        //    against the tree: `workflow_spawn_plan` derives
        //    `SpawnSpec::agent_session_id` for *every* node it plans,
        //    `Runner::Command` included, `spawn_workflow_node` copies it onto
        //    the binding, and `write_run_node` persists it. So a command node's
        //    binding carries an id for a Claude session that was never created.
        //    Gating on the id would let a command node through to the stat and,
        //    if any file happened to sit at the estimated path, hand `--resume`
        //    a session id `claude` has never heard of.
        //
        //    The message says which of the two things is true, because "the
        //    transcript is unavailable" alone sends the caller looking for a
        //    file that was never going to be there.
        match self.interrogation_runner(&run, &node) {
            Some(Runner::Command) => {
                return encode_error(
                    id,
                    TRANSCRIPT_UNAVAILABLE_CODE,
                    format!(
                        "node {path} ran as a command, not an agent, so it has no session \
                         transcript to resume"
                    ),
                )
            }
            // An unresolvable runner falls through to the stat, which is the
            // arbiter anyway (`03-storage-schema.md` §4.4). Refusing on a lookup
            // miss would block a perfectly forkable node over a definition row
            // that has since been pruned.
            Some(Runner::Agent) | None => {}
        }
        let Some(source_session_id) = node.agent_session_id.clone() else {
            return encode_error(
                id,
                TRANSCRIPT_UNAVAILABLE_CODE,
                format!(
                    "node {path} never started a session — it has no pane binding to \
                     resume from"
                ),
            );
        };

        // 3. One live interrogation per source node (§4 D7). Keyed on the live
        //    tracker rather than on `interrogation` rows with no `ended_at`: a
        //    row left open by a server that died names a pane that is long gone,
        //    and treating it as live would refuse this node forever.
        if let Some(active) = self.workflow.live_interrogation(&run_id, &path) {
            return encode_error(
                id,
                INTERROGATION_ACTIVE_CODE,
                format!(
                    "node {path} is already being interrogated in pane {}; close it before \
                     starting another",
                    active.pane
                ),
            );
        }

        // 4. The mode's own precondition. Stat-first (`03-storage-schema.md`
        //    §4.4): a wrong path is discovered here, as an answer, rather than
        //    by a pane that starts and dies.
        let seed =
            match self.interrogation_seed(&run_id, &path, &node, &source_session_id, reconstructed)
            {
                Ok(seed) => seed,
                Err(InterrogationRefusal::Unavailable(message)) => {
                    return encode_error(id, TRANSCRIPT_UNAVAILABLE_CODE, message)
                }
                Err(InterrogationRefusal::Store(response)) => return response(id),
            };

        // Everything below creates. Nothing above did.
        match self.spawn_interrogation(InterrogationRequest {
            run: run_id,
            path,
            workflow_name: run.workflow_name,
            workspace_id: run.workspace_id,
            source_session_id,
            note,
            seed,
        }) {
            Ok(info) => encode_success(
                id,
                ResponseResult::WorkflowNodeInterrogated {
                    interrogation: info,
                },
            ),
            // Not `workflow_transcript_unavailable`, and not a node-spawn code
            // either — see [`INTERROGATION_SPAWN_FAILED_CODE`]. The specific
            // reason rides in the message.
            Err(failed) => encode_error(
                id,
                INTERROGATION_SPAWN_FAILED_CODE,
                format!("the interrogation's pane could not be created: {failed}"),
            ),
        }
    }

    /// `workflow.summary.get` — the run's end-of-run summary, or `None`.
    ///
    /// `None` is a normal answer and never an error (§4 D1): a run whose
    /// epilogue was disabled, cancelled, or gave up simply has no summary, and
    /// every consumer treats that as "no summary". The summary outlives its run,
    /// so this answers for a pruned run too — which is exactly what
    /// `workflow.run.get`'s `workflow_run_pruned` message points at.
    pub(super) fn handle_workflow_summary_get(
        &mut self,
        id: String,
        target: WorkflowRunTarget,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &target.run_id) {
            return error;
        }
        let run_id = RunId::new(target.run_id.trim().to_string());
        let loaded = self
            .workflow_store
            .call(move |cx| cx.block_on(cx.store().get_run_summary(&run_id)));
        match loaded {
            Ok(Ok(summary)) => encode_success(
                id,
                ResponseResult::WorkflowSummaryGet {
                    summary: summary.map(wire_run_summary_record),
                },
            ),
            Ok(Err(error)) => self.store_error(id, &error),
            Err(unavailable) => unavailable_response(id, &unavailable),
        }
    }

    /// `workflow.summary.list` — summaries newest-first, across one workflow or
    /// all of them (§4 D9).
    ///
    /// `run_summary` is the one never-pruned table, so this is the only listing
    /// that still returns a pruned run's history; each row says so through
    /// `run_pruned`.
    pub(super) fn handle_workflow_summary_list(
        &mut self,
        id: String,
        params: WorkflowSummaryListParams,
    ) -> String {
        let limit = params.limit.unwrap_or(DEFAULT_RUN_LIST_LIMIT).max(1);
        let selector = params
            .workflow_id
            .map(|workflow_id| workflow_id.trim().to_string())
            .filter(|workflow_id| !workflow_id.is_empty());
        let listed = self.workflow_store.call(move |cx| {
            let workflow_id = match selector {
                // The same `<name|id>` selector every other workflow verb takes:
                // listing summaries by the name the user gave the workflow must
                // not be the one verb that only speaks record ids.
                Some(selector) => match resolve_workflow_selector(cx, &selector)? {
                    WorkflowSelector::Found(workflow_id) => Some(workflow_id),
                    WorkflowSelector::NotFound => {
                        return Ok::<_, StoreError>(LookupResult::NotFound)
                    }
                    WorkflowSelector::Ambiguous => return Ok(LookupResult::Ambiguous),
                },
                None => None,
            };
            let summaries =
                cx.block_on(cx.store().list_run_summaries(workflow_id.as_ref(), limit))?;
            Ok(LookupResult::Found(summaries))
        });
        match listed {
            Ok(Ok(LookupResult::Found(summaries))) => encode_success(
                id,
                ResponseResult::WorkflowSummaryList {
                    summaries: summaries.into_iter().map(wire_run_summary_record).collect(),
                },
            ),
            Ok(Ok(LookupResult::NotFound)) => {
                encode_error(id, NOT_FOUND_CODE, "no workflow with that id".to_string())
            }
            Ok(Ok(LookupResult::Ambiguous)) => encode_error(
                id,
                AMBIGUOUS_NAME_CODE,
                "multiple workflows matched that name".to_string(),
            ),
            Ok(Err(error)) => self.store_error(id, &error),
            Err(unavailable) => unavailable_response(id, &unavailable),
        }
    }

    /// Authenticates a node-token-bearing call that is not a report, returning
    /// the minted token the engine input has to carry.
    ///
    /// Delegates to the binder's `node_self_report`, whose constant-time
    /// comparison is the subsystem's one token check — a second implementation
    /// here would be a second thing to get wrong, and a capability check is the
    /// wrong place to keep two. The `EngineInput` it builds is discarded: the
    /// call *is* the check, and it performs no effects.
    fn authenticate_node_token(
        &self,
        path: &str,
        token: &str,
    ) -> Result<NodeToken, ReportRejected> {
        let expected = self
            .workflow
            .node_token(&InstancePath::new(path.trim()))
            .cloned();
        crate::workflow::binding::observe::node_self_report(
            path,
            token,
            expected.as_ref(),
            Some(serde_json::Value::Null),
        )?;
        // `node_self_report` refuses a node with no minted token, so the check
        // above already established this is `Some`.
        expected.ok_or(ReportRejected::UnknownNode)
    }

    /// Judges one proposal against the live run, returning the outcome plus the
    /// two numbers a limit's `limit_value` is read from — the run's effective
    /// [`GrowthLimits`] and the proposing node's own `expand_max`.
    ///
    /// `None` when the run has no graph or no installed definition, which is
    /// the one state in which the engine cannot judge a proposal at all.
    fn evaluate_expansion(
        &self,
        path: &InstancePath,
        proposal: &ExpandProposal,
    ) -> Option<(ExpandOutcome, GrowthLimits, u16)> {
        let graph = self.workflow.graph()?;
        let definition = self.workflow.definition()?;
        let proposer = graph.index_of(path)?;
        let expand_max = graph
            .node(proposer)
            .and_then(|node| definition.node(&node.key))
            .map_or(0, |spec| spec.expand_max);
        let outcome = expand::evaluate(graph, definition, proposer, proposal);
        Some((outcome, graph.growth, expand_max))
    }

    /// H2 — the one closed-run guard, applied to every node method that hands
    /// work back to a run: `steer`, `interrupt`, `restart`, and `expand`.
    ///
    /// A closed run will never settle again, so anything delivered into it is
    /// answered `ok` for work nothing will ever collect. `action` completes the
    /// sentence "a closed run cannot …".
    fn require_open_run(&self, id: &str, run: &RunId, action: &str) -> Option<String> {
        let status = self
            .workflow
            .run_status()
            .filter(|status| is_closed_run(*status))?;
        Some(encode_error(
            id.to_string(),
            RUN_CLOSED_CODE,
            format!(
                "run {run} is already {}; a closed run cannot {action}. \
                 Start a new run with `kvx workflow run start <name|id>`.",
                run_status_label(status),
            ),
        ))
    }

    /// `workflow_run_in_flight`, told truthfully. A *paused* run is not
    /// executing: it is waiting for a human, and the old wording sent the user
    /// looking for a busy run that does not exist. Names the blocking run, its
    /// status, the node it is stuck on, and both ways out.
    fn workflow_run_in_flight_message(&self) -> String {
        let Some(run) = self.workflow.active_run().map(|run| run.run_id.to_string()) else {
            return crate::app::workflow::WorkflowStartError::RunInFlight
                .message()
                .to_string();
        };
        let status = self
            .workflow
            .run_status()
            .map_or("running", run_status_label);
        let blocking: Vec<(String, &'static str)> = self
            .workflow
            .graph()
            .map(|graph| {
                graph
                    .nodes
                    .iter()
                    .filter(|node| {
                        matches!(
                            node.status,
                            NodeStatus::NeedsAttention | NodeStatus::Blocked | NodeStatus::Failed
                        )
                    })
                    .map(|node| (node.path.to_string(), node_status_label(node.status)))
                    .collect()
            })
            .unwrap_or_default();

        let mut message = format!("another workflow run is still in flight: run {run} is {status}");
        if let Some((path, node_status)) = blocking.first() {
            message.push_str(&format!(", blocked on node \"{path}\" ({node_status})"));
            if blocking.len() > 1 {
                message.push_str(&format!(" and {} other node(s)", blocking.len() - 1));
            }
        }
        message.push('.');
        if matches!(self.workflow.run_status(), Some(RunStatus::Paused)) {
            message.push_str(" A paused run is not executing — it is waiting for a human.");
        }
        match blocking.first() {
            Some((path, _)) => message.push_str(&format!(
                " Resume it with `kvx workflow node restart {run} {path}` or \
                 `kvx workflow node steer {run} {path} <text>`, or end it with \
                 `kvx workflow run cancel {run}`."
            )),
            None => message.push_str(&format!(
                " Wait for it to finish, or end it with `kvx workflow run cancel {run}`."
            )),
        }
        message
    }

    /// The shared body of every node method that drives the engine: the run has
    /// to be the live one, the node has to exist in it, and the answer is the
    /// node as it stands after the input was applied.
    fn apply_node_input(
        &mut self,
        id: String,
        run_id: &str,
        path_text: &str,
        input: EngineInput,
        path: &InstancePath,
        result: impl FnOnce(WorkflowRunNodeInfo) -> ResponseResult,
    ) -> String {
        let run = RunId::new(run_id.trim().to_string());
        if self.workflow_run_info(&run).is_none() {
            return not_the_active_run(id, run_id);
        }
        // H2: one guard for all three verbs. It runs before the node lookup so
        // a closed run is reported as closed rather than as a path problem —
        // the ordering `restart` established when it was the only guarded verb.
        if let Some(error) = self.require_open_run(&id, &run, &closed_run_action(&input, path_text))
        {
            return error;
        }
        if self.workflow_node_info(path).is_none() {
            return node_not_found(
                id,
                &WorkflowNodeTarget {
                    run_id: run_id.to_string(),
                    path: path_text.to_string(),
                },
            );
        }
        // Steer and interrupt are deliveries into the node's pane; the engine
        // emits nothing at all for a node that has no binding. Reporting that
        // as a success would tell the caller their text landed when it was
        // silently dropped.
        let delivers_to_pane = matches!(
            input,
            EngineInput::Steer { .. } | EngineInput::Interrupt { .. }
        );
        if delivers_to_pane
            && self
                .workflow
                .node(path)
                .is_none_or(|node| node.binding.is_none())
        {
            return encode_error(
                id,
                NODE_NOT_RUNNING_CODE,
                format!("node {path_text} has no pane to deliver to"),
            );
        }
        if delivers_to_pane {
            self.workflow.clear_delivery_failure();
        }
        self.apply_workflow_engine_input(input);
        // A pane delivery the runtime refused is not a success. Reporting one
        // would tell the caller their interrupt landed on a process that never
        // saw it — the control surface lying about the system's state.
        if delivers_to_pane {
            if let Some(failure) = self.workflow.take_delivery_failure() {
                return encode_error(id, DELIVERY_FAILED_CODE, failure.describe());
            }
        }
        match self.workflow_node_info(path) {
            Some(node) => encode_success(id, result(node)),
            None => node_not_found(
                id,
                &WorkflowNodeTarget {
                    run_id: run_id.to_string(),
                    path: path_text.to_string(),
                },
            ),
        }
    }

    /// A run this server is not executing, read back from the journal. The
    /// error arm is returned as a response builder so the caller keeps
    /// ownership of the request id.
    #[allow(clippy::type_complexity)]
    fn stored_run(
        &mut self,
        run: &RunId,
    ) -> Result<Option<(WorkflowRunInfo, WorkflowRunGraph)>, Box<dyn FnOnce(String) -> String>>
    {
        let wanted = run.clone();
        let loaded = self.workflow_store.call(move |cx| {
            let Some(record) = cx.block_on(cx.store().get_run(&wanted))? else {
                return Ok::<_, StoreError>(None);
            };
            let nodes = cx.block_on(cx.store().list_run_nodes(&wanted))?;
            // The `run_edge` relations are written when the run is
            // materialised and re-settled by `StoreWrite::RunEdge` on every
            // propagation, so a restored run carries both its topology and the
            // branches it actually took.
            let edges = cx.block_on(cx.store().list_run_edges(&wanted))?;
            // B2: one extra round trip in the same context rather than a
            // second `workflow_store.call`, since `run.get`/`node.get` need it
            // alongside the nodes/edges this closure already loaded.
            let limits = cx.block_on(cx.store().growth_limits(&wanted))?;
            // §3.4: the team's members are part of the run's record, not a
            // live read — Claude Code deletes the team config at session end,
            // so this is the only place they survive.
            let members = cx.block_on(cx.store().list_run_members(&wanted))?;
            Ok(Some((record, nodes, edges, limits, members)))
        });
        match loaded {
            Ok(Ok(Some((record, nodes, edges, limits, members)))) => {
                let graph = WorkflowRunGraph {
                    nodes: nodes
                        .into_iter()
                        .map(|node| wire_run_node_record(node, &limits))
                        .collect(),
                    edges: edges.into_iter().map(wire_run_edge_record).collect(),
                    members: members.into_iter().map(wire_run_member_record).collect(),
                };
                Ok(Some((wire_run_record(record, &limits), graph)))
            }
            Ok(Ok(None)) => Ok(None),
            Ok(Err(error)) => {
                let code = error.api_code();
                let message = error.to_string();
                Err(Box::new(move |id| encode_error(id, code, message)))
            }
            Err(unavailable) => Err(Box::new(move |id| unavailable_response(id, &unavailable))),
        }
    }

    /// Resolves a `restore_from` request into seeds and a report (§4 D11).
    ///
    /// The one hard error is an **unknown selector**: a selector naming no node
    /// in the *target* version is a typo, and silently re-running a node the
    /// caller believed restored is precisely what that error exists to prevent.
    /// Every other outcome is a reported skip, because "you asked for something
    /// that doesn't exist" and "it exists but can't be restored" are different
    /// answers and only the first is the caller's mistake.
    ///
    /// Runs entirely before `create_run`, so a hard error leaves no run row.
    #[allow(clippy::type_complexity)]
    fn resolve_restore(
        &mut self,
        request: &crate::api::schema::WorkflowRestoreRequest,
        kvdag: &Kvdag,
    ) -> Result<RestorePlan, Box<dyn FnOnce(String) -> String>> {
        let source = RunId::new(request.run_id.trim().to_string());
        // A pruned source is `workflow_run_pruned`, not "not found": its
        // checkpoints are gone with the run, but its summary is not, and the
        // caller deserves to be told which of those two things happened.
        if self.stored_run_or_pruned(&source)?.is_none() {
            let named = source.clone();
            return Err(Box::new(move |id| {
                encode_error(id, NOT_FOUND_CODE, format!("no run with id {named}"))
            }));
        }

        // Templates are never materialised at run start (§3.4), so they are not
        // in the selector namespace: a template has no instance to restore into.
        let target_keys: Vec<NodeKey> = kvdag
            .nodes
            .iter()
            .filter(|node| !node.is_template)
            .map(|node| node.key.clone())
            .collect();

        // D18: the bare form means "everything restorable". Forcing an explicit
        // selector list for the common case — re-run this, keeping what
        // succeeded — would make the short spelling useless, and the report
        // lists both sets either way, so it is never silent about what it
        // skipped.
        let selectors: Vec<NodeKey> = if request.nodes.is_empty() {
            target_keys.clone()
        } else {
            let mut selectors = Vec::with_capacity(request.nodes.len());
            for selector in &request.nodes {
                let key = NodeKey::new(selector.trim().to_string());
                if !target_keys.contains(&key) {
                    let selector = selector.clone();
                    let version = kvdag.version_id.clone();
                    return Err(Box::new(move |id| {
                        encode_error(
                            id,
                            RESTORE_UNKNOWN_SELECTOR_CODE,
                            format!(
                                "version {version} has no restorable node named {selector}; \
                                 restore selectors name nodes of the version being started, \
                                 not of the run being restored from"
                            ),
                        )
                    }));
                }
                selectors.push(key);
            }
            selectors
        };

        let version = kvdag.version_id.clone();
        let wanted = source.clone();
        let wanted_selectors = selectors.clone();
        // One store job for the whole resolution: the source checkpoints and the
        // target version's digests together, rather than a round trip per
        // selector.
        let loaded = self.workflow_store.call(move |cx| {
            let restorable = cx.block_on(cx.store().restore_source(&wanted, &wanted_selectors))?;
            let mut target_digests: BTreeMap<NodeKey, (String, String)> = BTreeMap::new();
            for key in &wanted_selectors {
                if let Some(digests) =
                    cx.block_on(cx.store().node_compat_digests_for(&version, key))?
                {
                    target_digests.insert(key.clone(), digests);
                }
            }
            Ok::<_, StoreError>((restorable, target_digests))
        });
        let (restorable, target_digests) = match loaded {
            Ok(Ok(loaded)) => loaded,
            Ok(Err(error)) => {
                let code = error.api_code();
                let message = error.to_string();
                return Err(Box::new(move |id| encode_error(id, code, message)));
            }
            Err(unavailable) => {
                return Err(Box::new(move |id| unavailable_response(id, &unavailable)))
            }
        };

        // Newest checkpoint per node key: `restore_source` orders by seq, and a
        // node that checkpointed twice restores from what it ended with.
        let mut newest: BTreeMap<NodeKey, crate::workflow::store::RestorableCheckpoint> =
            BTreeMap::new();
        for candidate in restorable {
            let key = candidate.checkpoint.node_key.clone();
            match newest.get(&key) {
                Some(existing) if existing.checkpoint.seq >= candidate.checkpoint.seq => {}
                _ => {
                    newest.insert(key, candidate);
                }
            }
        }

        let mut plan = RestorePlan {
            request: crate::workflow::store::RestoreFromRequest {
                run: source.clone(),
                nodes: request.nodes.clone(),
                allow_changed: request.allow_changed,
            },
            source,
            seeds: Vec::new(),
            report: WorkflowRestoreReport {
                restored: Vec::new(),
                skipped: Vec::new(),
            },
        };
        for key in selectors {
            let Some(candidate) = newest.remove(&key) else {
                plan.report.skipped.push(WorkflowRestoreSkip {
                    selector: key.to_string(),
                    reason: WorkflowRestoreSkipReason::NoCheckpoint,
                    message: format!("the source run has no validated result checkpoint for {key}"),
                });
                continue;
            };
            // D19: the store discards over-budget payloads and keeps a
            // `{"truncated": true}` stub. Restoring the stub would hand
            // downstream nodes a lie labelled as data, so this is skipped even
            // with `allow_changed` — the payload is not "changed", it is absent.
            if is_truncated_payload(&candidate.checkpoint.payload) {
                plan.report.skipped.push(WorkflowRestoreSkip {
                    selector: key.to_string(),
                    reason: WorkflowRestoreSkipReason::PayloadTruncated,
                    message: format!(
                        "{key}'s checkpoint payload exceeded the store's size budget and was \
                         not kept; there is nothing to restore even with allow_changed"
                    ),
                });
                continue;
            }
            let target = target_digests.get(&key);
            let compatible = target.is_some_and(|(prompt, schema)| {
                prompt == &candidate.prompt_digest && schema == &candidate.schema_digest
            });
            if !compatible && !request.allow_changed {
                plan.report.skipped.push(WorkflowRestoreSkip {
                    selector: key.to_string(),
                    reason: WorkflowRestoreSkipReason::DefinitionChanged,
                    message: format!(
                        "{key}'s prompt or output schema differs from the version it ran under, \
                         so its stored result may not answer the question this version asks; \
                         re-run it, or pass allow_changed to restore it anyway"
                    ),
                });
                continue;
            }
            plan.report.restored.push(key.to_string());
            plan.seeds.push(RestoredSeed {
                node_key: key,
                payload: candidate.checkpoint.payload,
                summary: candidate.checkpoint.summary,
                artifact_paths: candidate.checkpoint.artifact_paths,
                digest: candidate.checkpoint.digest,
                source: RestoredRef {
                    run: plan.source.clone(),
                    node_key: candidate.checkpoint.node_key,
                    checkpoint_seq: candidate.checkpoint.seq,
                },
            });
        }
        Ok(plan)
    }

    /// The injection feed: this workflow's most recent summaries (§4 D21).
    #[allow(clippy::type_complexity)]
    fn prior_run_summaries(
        &mut self,
        workflow: &WorkflowId,
    ) -> Result<Vec<crate::api::schema::WorkflowRunSummaryInfo>, Box<dyn FnOnce(String) -> String>>
    {
        let limit = u32::try_from(self.workflow.policy().history_context_runs).unwrap_or(u32::MAX);
        if limit == 0 {
            return Ok(Vec::new());
        }
        let workflow = workflow.clone();
        let loaded = self.workflow_store.call(move |cx| {
            cx.block_on(cx.store().run_summaries_for_context(
                &workflow,
                // The run being started does not exist yet — it is created from
                // what this returns — so there is nothing of its own to exclude.
                // The parameter is for callers re-deriving context for a run
                // that already has an id.
                &RunId::new(String::new()),
                limit,
            ))
        });
        match loaded {
            Ok(Ok(summaries)) => Ok(summaries.into_iter().map(wire_run_summary_record).collect()),
            Ok(Err(error)) => {
                let code = error.api_code();
                let message = error.to_string();
                Err(Box::new(move |id| encode_error(id, code, message)))
            }
            Err(unavailable) => Err(Box::new(move |id| unavailable_response(id, &unavailable))),
        }
    }

    /// Writes `<run dir>/context/prior-runs.md` and returns its path.
    ///
    /// A failure is a warning, never a failed run: the run's work does not
    /// depend on its history, and refusing to start over a context file the
    /// nodes are explicitly told they may ignore would be a worse answer than
    /// starting without one.
    fn write_prior_runs_context(
        &mut self,
        run: &RunId,
        workflow_name: &str,
        summaries: &[crate::api::schema::WorkflowRunSummaryInfo],
    ) -> Option<String> {
        let sections: Vec<crate::workflow::binding::spawn::PriorRunSection<'_>> = summaries
            .iter()
            .map(|summary| crate::workflow::binding::spawn::PriorRunSection {
                run: &summary.run_id,
                outcome: &summary.outcome,
                text: &summary.text,
                highlights: &summary.highlights,
                open_gaps: &summary.open_gaps,
            })
            .collect();
        let body = crate::workflow::binding::spawn::render_prior_runs(workflow_name, &sections);
        let run_dir = crate::workflow::binding::spawn::run_dir(
            &crate::workflow::binding::spawn::runs_root(),
            run,
        );
        match crate::workflow::binding::spawn::write_run_context(&run_dir, &body) {
            Ok(path) => Some(path.display().to_string()),
            Err(error) => {
                tracing::warn!(
                    run = %run,
                    error = %error,
                    "the run's prior-runs context could not be written; the run starts without it"
                );
                None
            }
        }
    }

    /// Everything `workflow.node.interrogate` needs about the node it was
    /// pointed at: the run's name and workspace, and the node's own projection.
    ///
    /// Live run first, then the durable projection — the same precedence
    /// `run.get`/`node.get` use, and the reason an interrogation of the
    /// just-finished run reads the engine's `NodeBinding` rather than a row the
    /// store task may not have applied yet.
    ///
    /// `Ok(None)` means the run exists but has no such node. A run that has been
    /// *pruned* is not `Ok(None)`: it is refused with `workflow_run_pruned`
    /// naming `workflow.summary.get`, because "no such run" would be a lie
    /// about a run whose summary is sitting in the database.
    #[allow(clippy::type_complexity)]
    fn interrogation_target(
        &mut self,
        run: &RunId,
        path: &InstancePath,
    ) -> Result<Option<(WorkflowRunInfo, WorkflowRunNodeInfo)>, Box<dyn FnOnce(String) -> String>>
    {
        if let Some(info) = self.workflow_run_info(run) {
            let Some(node) = self.workflow_node_info(path) else {
                return Ok(None);
            };
            return Ok(Some((info, node)));
        }
        let Some((info, graph)) = self.stored_run_or_pruned(run)? else {
            return Ok(None);
        };
        Ok(graph
            .nodes
            .into_iter()
            .find(|node| node.path == path.as_str())
            .map(|node| (info, node)))
    }

    /// How the node was bound, which is what decides whether a transcript could
    /// ever exist for it.
    ///
    /// The live definition when the run is this server's, and the run's own
    /// kvdag version otherwise — one extra store read on a user-initiated path,
    /// not a hot loop. `None` means the version no longer resolves, which the
    /// caller treats as "let the stat decide" rather than as a refusal.
    ///
    /// **A reserved path is asked about differently.** The `.summary` epilogue
    /// has no kvdag node (§4 D5), so neither lookup below can ever resolve for
    /// it: both would answer `None`, the caller would fall through to the stat,
    /// and a command-bound summariser — which never had a session — would be
    /// refused with "the transcript is not on disk" instead of "it ran as a
    /// command". That is the wrong-answer shape the runner gate exists to
    /// prevent, and it is the same defect as D-C
    /// (`WorkflowRuntimeState::runner_for_pane`) reached by path instead of by
    /// pane. `EpilogueState::runner` is the one authority, read through
    /// `WorkflowRuntimeState::epilogue_runner`.
    ///
    /// It answers only for the run this server has in memory, because nothing
    /// persists the epilogue's runner. A stored run's epilogue therefore still
    /// resolves to `None` and is decided by the stat — the same policy this
    /// function already applies to a node whose definition version has been
    /// pruned, and better than re-deriving the binding from *this* server's
    /// `KARVEX_WORKFLOW_SUMMARY_COMMAND`, which is not the configuration that
    /// run summarised under.
    fn interrogation_runner(
        &mut self,
        run: &WorkflowRunInfo,
        node: &WorkflowRunNodeInfo,
    ) -> Option<Runner> {
        if is_reserved_path(node.path.as_str()) {
            return self
                .workflow
                .epilogue_runner(&RunId::new(run.run_id.clone()));
        }
        let key = NodeKey::new(node.node_key.clone());
        if let Some(definition) = self.workflow.definition() {
            if definition.version_id.as_str() == run.version_id {
                return definition.node(&key).map(|node| node.runner);
            }
        }
        let version = KvdagVersionId::new(run.version_id.clone());
        let loaded = self
            .workflow_store
            .call(move |cx| cx.block_on(cx.store().load_version(&version)));
        match loaded {
            Ok(Ok(kvdag)) => kvdag.node(&key).map(|node| node.runner),
            _ => None,
        }
    }

    /// [`Self::stored_run`] plus m9's pruned-run distinction.
    ///
    /// A pruned run has no `workflow_run` row at all — `prune_one_run` deletes
    /// it and keeps only `run_summary` — so the bare not-found the store hands
    /// back is indistinguishable from a run id that never existed. Checking for
    /// the surviving summary is what turns those two into different answers,
    /// and the error message names `workflow.summary.get` rather than implying
    /// it.
    #[allow(clippy::type_complexity)]
    fn stored_run_or_pruned(
        &mut self,
        run: &RunId,
    ) -> Result<Option<(WorkflowRunInfo, WorkflowRunGraph)>, Box<dyn FnOnce(String) -> String>>
    {
        if let Some(found) = self.stored_run(run)? {
            return Ok(Some(found));
        }
        let wanted = run.clone();
        let summarised = self
            .workflow_store
            .call(move |cx| cx.block_on(cx.store().get_run_summary(&wanted)));
        match summarised {
            Ok(Ok(Some(_))) => {
                let run = run.clone();
                Err(Box::new(move |id| {
                    encode_error(
                        id,
                        RUN_PRUNED_CODE,
                        format!(
                            "run {run} has been pruned from the run history; its summary \
                             survives and is readable with workflow.summary.get"
                        ),
                    )
                }))
            }
            Ok(Ok(None)) => Ok(None),
            Ok(Err(error)) => {
                let code = error.api_code();
                let message = error.to_string();
                Err(Box::new(move |id| encode_error(id, code, message)))
            }
            Err(unavailable) => Err(Box::new(move |id| unavailable_response(id, &unavailable))),
        }
    }

    /// The mode's own precondition, resolved into what the spawn needs.
    ///
    /// `resumed` stats **both** the transcript and the recorded cwd and names
    /// whichever is missing: a fork whose cwd is gone starts in the wrong
    /// project directory and silently fails to find the session, which is the
    /// failure `03-storage-schema.md` §4.4's stat-first rule exists to convert
    /// into an answer. `reconstructed` needs no transcript by definition — it
    /// needs a stored `result` checkpoint, and with none there is nothing to
    /// stand in for.
    fn interrogation_seed(
        &mut self,
        run: &RunId,
        path: &InstancePath,
        node: &WorkflowRunNodeInfo,
        source_session_id: &str,
        reconstructed: bool,
    ) -> Result<InterrogationSeed, InterrogationRefusal> {
        let recorded_cwd = node
            .cwd
            .as_deref()
            .map(str::trim)
            .filter(|cwd| !cwd.is_empty())
            .map(PathBuf::from);

        if reconstructed {
            let checkpoint = self.latest_result_checkpoint(run, path)?.ok_or_else(|| {
                InterrogationRefusal::Unavailable(format!(
                    "node {path} has no stored result checkpoint to reconstruct from, and its \
                     transcript is not being resumed"
                ))
            })?;
            // The recorded cwd is preferred but not required here: a
            // reconstruction reads a karvex-authored file and does not depend
            // on the project directory the original ran in. Falling back keeps
            // the degraded path available when the workspace has moved.
            let cwd = recorded_cwd
                .filter(|cwd| cwd.is_dir())
                .or_else(|| self.interrogation_fallback_cwd())
                .ok_or_else(|| {
                    InterrogationRefusal::Unavailable(
                        "there is no directory to start the reconstructed session in".to_string(),
                    )
                })?;
            let original_task = node
                .node_dir
                .as_deref()
                .map(|dir| PathBuf::from(dir).join(crate::workflow::binding::spawn::TASK_FILE))
                .and_then(|task| std::fs::read_to_string(task).ok());
            return Ok(InterrogationSeed::Reconstructed {
                cwd,
                checkpoint_seq: checkpoint.seq,
                summary: checkpoint.summary,
                payload: serde_json::to_string_pretty(&checkpoint.payload)
                    .unwrap_or_else(|_| checkpoint.payload.to_string()),
                original_task,
                label: node.label.clone(),
            });
        }

        let cwd = recorded_cwd.ok_or_else(|| {
            InterrogationRefusal::Unavailable(format!(
                "node {path} has no recorded working directory, so its session cannot be \
                 resumed from where it ran"
            ))
        })?;
        if !cwd.is_dir() {
            return Err(InterrogationRefusal::Unavailable(format!(
                "the directory node {path} ran in no longer exists: {}",
                cwd.display()
            )));
        }
        // §4 D6: the hook-reported path when one was recorded, else the
        // pre-launch estimate. The estimate is recomputed rather than assumed
        // absent, so a node whose row predates the read-back still resolves.
        let transcript = node
            .transcript_path
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .map_or_else(
                || {
                    crate::workflow::binding::spawn::transcript_path(&cwd, source_session_id)
                        .map_err(|err| {
                            InterrogationRefusal::Unavailable(format!(
                                "the transcript path for node {path} is unknown: {err}"
                            ))
                        })
                },
                Ok,
            )?;
        if !transcript.is_file() {
            return Err(InterrogationRefusal::Unavailable(format!(
                "the transcript for node {path} is not on disk: {}",
                transcript.display()
            )));
        }
        Ok(InterrogationSeed::Resumed {
            cwd,
            transcript_path: transcript.display().to_string(),
        })
    }

    /// The node's newest schema-valid `result` checkpoint, which is what a
    /// reconstruction stands in for.
    fn latest_result_checkpoint(
        &mut self,
        run: &RunId,
        path: &InstancePath,
    ) -> Result<Option<crate::workflow::store::CheckpointRecord>, InterrogationRefusal> {
        let wanted_run = run.clone();
        let wanted_path = path.clone();
        let loaded = self
            .workflow_store
            .call(move |cx| cx.block_on(cx.store().list_checkpoints(&wanted_run, &wanted_path)));
        match loaded {
            Ok(Ok(checkpoints)) => Ok(checkpoints
                .into_iter()
                .filter(|checkpoint| {
                    checkpoint.kind == CheckpointKind::Result && checkpoint.schema_valid
                })
                .max_by_key(|checkpoint| checkpoint.seq)),
            Ok(Err(error)) => {
                let code = error.api_code();
                let message = error.to_string();
                Err(InterrogationRefusal::Store(Box::new(move |id| {
                    encode_error(id, code, message)
                })))
            }
            Err(unavailable) => Err(InterrogationRefusal::Store(Box::new(move |id| {
                unavailable_response(id, &unavailable)
            }))),
        }
    }

    fn store_error(&mut self, id: String, error: &StoreError) -> String {
        encode_error(id, error.api_code(), error.to_string())
    }

    /// The measured history the single resolver reads (§4 D9), one entry per
    /// kvdag node key — templates included, so an expansion child resolves from
    /// the same table with no mid-run query.
    ///
    /// Only `auto` consults it: `tier::resolve`'s four fixed tiers are table
    /// lookups that ignore `history` entirely (`tier.rs`'s own doc), so a fixed
    /// tier is answered with an empty index rather than one store round trip
    /// per node for a value nothing reads.
    // The error arm is a response builder so the caller keeps ownership of the
    // request id — the same shape `stored_run` uses, and the reason the return
    // type is nested.
    #[allow(clippy::type_complexity)]
    fn node_history_index(
        &mut self,
        kvdag: &Kvdag,
        tier: Tier,
    ) -> Result<HistoryIndex, Box<dyn FnOnce(String) -> String>> {
        if tier != Tier::Auto {
            return Ok(HistoryIndex::new());
        }
        let workflow = kvdag.workflow_id.clone();
        let keys: Vec<NodeKey> = kvdag.nodes.iter().map(|node| node.key.clone()).collect();
        let loaded = self.workflow_store.call(move |cx| {
            let mut index = HistoryIndex::new();
            for key in keys {
                let history = cx.block_on(cx.store().node_history(
                    &workflow,
                    &key,
                    NODE_HISTORY_WINDOW,
                ))?;
                index.insert(key, history);
            }
            Ok::<_, StoreError>(index)
        });
        match loaded {
            Ok(Ok(index)) => Ok(index),
            Ok(Err(error)) => {
                let code = error.api_code();
                let message = error.to_string();
                Err(Box::new(move |id| encode_error(id, code, message)))
            }
            Err(unavailable) => Err(Box::new(move |id| unavailable_response(id, &unavailable))),
        }
    }

    /// Where a reconstructed interrogation starts when the node's own recorded
    /// directory is gone: the active workspace's directory.
    ///
    /// Only the reconstructed path falls back. A *resumed* fork must start in
    /// the directory the session ran in, because that is what decides which
    /// `~/.claude/projects/<slug>/` Claude looks in — starting it elsewhere
    /// would find no transcript and fail silently, which is the whole failure
    /// the stat-first rule exists to prevent.
    fn interrogation_fallback_cwd(&self) -> Option<PathBuf> {
        let ws_idx = self.state.active?;
        Some(self.workflow_node_cwd_for(ws_idx))
    }

    /// `ActiveRun.workspace_id` is what pins a run's panes to one workspace, so
    /// a run started over the API records the workspace it was started from.
    fn active_workspace_public_id(&self) -> Option<String> {
        let ws_idx = self.state.active?;
        Some(self.public_workspace_id(ws_idx))
    }
}

/// What a resolved `restore_from` request decided (§4 D11).
#[cfg(feature = "workflow")]
struct RestorePlan {
    source: RunId,
    /// What the caller asked for, verbatim, recorded on the run row beside what
    /// actually happened. An unknown selector never reaches here — that is a
    /// hard error — but a *skipped* one does, and the row is the only durable
    /// trace of it once the response is gone.
    request: crate::workflow::store::RestoreFromRequest,
    seeds: Vec<RestoredSeed>,
    report: WorkflowRestoreReport,
}

/// Whether a checkpoint payload is the store's over-budget stub rather than
/// real data (§4 D19).
///
/// `enforce_payload_budget` replaces a payload above 256 KB with
/// `{"truncated": true}`. Handing that to a downstream node would be a lie
/// labelled as data, so it is matched here and skipped — deliberately by
/// *shape*, because the stub is what the store writes and there is no separate
/// column saying it did.
#[cfg(feature = "workflow")]
fn is_truncated_payload(payload: &serde_json::Value) -> bool {
    payload
        .as_object()
        .is_some_and(|object| object.get("truncated") == Some(&serde_json::Value::Bool(true)))
}

/// Why an interrogation was refused before anything was created.
///
/// The two arms are genuinely different answers: the caller can act on
/// `Unavailable` (delete nothing, ask for `mode: reconstructed` instead), while
/// `Store` is the subsystem failing and carries the store's own code.
#[cfg(feature = "workflow")]
enum InterrogationRefusal {
    /// `workflow_transcript_unavailable`, with the reason in the message.
    Unavailable(String),
    /// The store answered with an error or is unavailable; its response,
    /// deferred so the caller keeps ownership of the request id.
    Store(Box<dyn FnOnce(String) -> String>),
}

#[cfg(feature = "workflow")]
fn unavailable_response(id: String, unavailable: &StoreUnavailable) -> String {
    encode_error_body(
        id,
        ErrorBody {
            code: unavailable.code.to_string(),
            message: unavailable.message.clone(),
        },
    )
}

/// Wall-clock now, in milliseconds. One reading per call site: a run's start
/// instant and a growth limit's timestamp are both stamped exactly once (§4
/// D15).
#[cfg(feature = "workflow")]
fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Completes the sentence "a closed run cannot …" for the three verbs that
/// hand work back to a run through [`App::apply_node_input`].
#[cfg(feature = "workflow")]
fn closed_run_action(input: &EngineInput, path: &str) -> String {
    let verb = match input {
        EngineInput::Steer { .. } => "steer",
        EngineInput::Interrupt { .. } => "interrupt",
        EngineInput::RestartNode { .. } => "restart",
        // No other input reaches `apply_node_input`; naming the node is still
        // the truthful half of the sentence.
        _ => "deliver to",
    };
    format!("{verb} node {}", path.trim())
}

/// The wire proposal, narrowed to the engine's vocabulary.
///
/// `count` is `u32` on the wire and `u16` in [`ExpandProposal`], so an
/// out-of-range value is **refused** rather than truncated — silently turning
/// `70000` into `4464` is exactly the kind of quiet reinterpretation §3.4
/// forbids. An explicit `0` is normalised to 1, which is what
/// `ExpandProposal::requested` means by it: a proposal is a request for at
/// least one child.
#[cfg(feature = "workflow")]
fn expand_proposal(params: &WorkflowNodeExpandParams) -> Result<ExpandProposal, String> {
    let count = match params.count {
        Some(count) => Some(u16::try_from(count.max(1)).map_err(|_| {
            format!(
                "count {count} is larger than the {} a proposal can ask for",
                u16::MAX
            )
        })?),
        None => None,
    };
    Ok(ExpandProposal {
        template: NodeKey::new(params.template.trim().to_string()),
        label: params.label.trim().to_string(),
        inputs: params
            .inputs
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        count,
    })
}

/// One engine rejection in the wire's vocabulary.
///
/// The two vocabularies are declared separately on purpose
/// (`src/api/schema/workflows.rs`'s module doc), and this is the only place
/// that maps between them, so a new engine variant cannot reach the wire under
/// an invented name. `growth`/`expand_max` are the run's effective ceilings and
/// the proposing node's own budget: [`ExpandLimit::value_in`] is the single
/// authority for "what number was hit", which is what keeps this response, the
/// `workflow.growth.limited` event, the journal, and the CLI in agreement.
#[cfg(feature = "workflow")]
fn wire_expand_rejection(
    rejection: &ExpandRejection,
    proposal: &ExpandProposal,
    growth: GrowthLimits,
    expand_max: u16,
    at_unix_ms: u64,
) -> WorkflowExpandRejection {
    let reason = match rejection {
        ExpandRejection::NotAllowed { .. } => WorkflowExpandRejectionReason::NotAllowed,
        ExpandRejection::UnknownTemplate { .. } => WorkflowExpandRejectionReason::UnknownTemplate,
        ExpandRejection::NotATemplate { .. } => WorkflowExpandRejectionReason::NotATemplate,
        ExpandRejection::ExpandMaxReached { .. } => WorkflowExpandRejectionReason::ExpandMaxReached,
        ExpandRejection::MaxDepthReached { .. } => WorkflowExpandRejectionReason::MaxDepthReached,
        ExpandRejection::MaxNodesReached { .. } => WorkflowExpandRejectionReason::MaxNodesReached,
        ExpandRejection::Truncated { .. } => WorkflowExpandRejectionReason::Truncated,
        ExpandRejection::UnknownInput { .. } => WorkflowExpandRejectionReason::UnknownInput,
    };
    // Every rejection but `Truncated` created nothing, and the count it was
    // refused is the proposal's own.
    let (requested, accepted) = rejection
        .counts()
        .unwrap_or_else(|| (proposal.requested(), 0));
    let limit = rejection.limit().map(|limit| {
        let limit_value = rejection
            .limit_value()
            .unwrap_or_else(|| limit.value_in(growth, expand_max));
        WorkflowGrowthLimit {
            kind: wire_growth_limit_kind(limit),
            limit_value: u32::from(limit_value),
            requested: u32::from(requested),
            accepted: u32::from(accepted),
            at_unix_ms,
            message: rejection.message(Some(limit_value)),
        }
    });
    WorkflowExpandRejection {
        template: rejection
            .template()
            .cloned()
            .unwrap_or_else(|| proposal.template.clone())
            .to_string(),
        reason,
        message: limit
            .as_ref()
            .map_or_else(|| rejection.message(None), |limit| limit.message.clone()),
        limit,
        requested: u32::from(requested),
        accepted: u32::from(accepted),
    }
}

#[cfg(feature = "workflow")]
fn wire_growth_limit_kind(limit: ExpandLimit) -> WorkflowGrowthLimitKind {
    match limit {
        ExpandLimit::ExpandMax => WorkflowGrowthLimitKind::ExpandMax,
        ExpandLimit::MaxDepth => WorkflowGrowthLimitKind::MaxDepth,
        ExpandLimit::MaxNodes => WorkflowGrowthLimitKind::MaxNodes,
    }
}

/// H3 / §4 D16 — the **one** `workflow.get` projection. Both the human
/// renderer and `--json` read this, so the two cannot describe different
/// node/edge/arg sets.
///
/// `workflow` is the `workflow` row's own summary, which `create_version`'s
/// metadata refresh keeps equal to the head document (H5); `nodes`/`edges`/
/// `args` come from the head version's kvdag. A head pointer that would not
/// load yields the summary and the version chain with empty graph sets, rather
/// than failing a call whose other half is perfectly readable.
#[cfg(feature = "workflow")]
fn workflow_detail(
    workflow: &WorkflowSummary,
    versions: &[KvdagVersionSummary],
    head: Option<&Kvdag>,
) -> WorkflowDetail {
    WorkflowDetail {
        workflow: workflow.clone(),
        nodes: head
            .map(|head| head.nodes.iter().map(wire_node_info).collect())
            .unwrap_or_default(),
        edges: head
            .map(|head| head.edges.iter().map(wire_edge_info).collect())
            .unwrap_or_default(),
        args: head
            .map(|head| {
                head.args
                    .iter()
                    .map(|arg| WorkflowArgSpec {
                        name: arg.name.clone(),
                        required: arg.required,
                        default: arg.default.clone(),
                        description: arg.description.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        versions: versions.to_vec(),
    }
}

#[cfg(feature = "workflow")]
fn not_the_active_run(id: String, run_id: &str) -> String {
    encode_error(
        id,
        NO_ACTIVE_RUN_CODE,
        format!("run {run_id} is not the run this server is executing"),
    )
}

/// The rejection message a node's own process reads back from
/// `kvx workflow node complete`. It quotes every schema violation and names the
/// next move, which is the only correction a `Runner::Command` node can act on.
#[cfg(feature = "workflow")]
fn describe_rejected_report(outcome: &ReportOutcome) -> String {
    let next = match outcome.verdict {
        ReportVerdict::Corrected => {
            "this was the node's one corrective re-prompt: fix result.json and run \
             `kvx workflow node complete` again"
        }
        ReportVerdict::Surfaced => {
            "the corrective re-prompt is already spent, so the node is now \
             needs_attention: fix result.json and restart the node"
        }
        // Not reachable: an accepted result has no violations to report.
        ReportVerdict::Accepted => "the node's result was accepted",
    };
    let mut message =
        String::from("result.json does not validate against the node's output_schema:");
    for violation in &outcome.errors {
        message.push_str("\n  - ");
        message.push_str(violation);
    }
    message.push('\n');
    message.push_str(next);
    message
}

/// Wire spelling of a run status, for messages the user reads.
#[cfg(feature = "workflow")]
fn run_status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Pending => "pending",
        RunStatus::Running => "running",
        RunStatus::Paused => "paused",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

#[cfg(feature = "workflow")]
fn node_status_label(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Pending => "pending",
        NodeStatus::Ready => "ready",
        NodeStatus::Running => "running",
        NodeStatus::NeedsAttention => "needs_attention",
        NodeStatus::Blocked => "blocked",
        NodeStatus::Succeeded => "succeeded",
        NodeStatus::Failed => "failed",
        NodeStatus::Skipped => "skipped",
        NodeStatus::Restored => "restored",
        NodeStatus::Cancelled => "cancelled",
    }
}

#[cfg(feature = "workflow")]
fn node_not_found(id: String, target: &WorkflowNodeTarget) -> String {
    encode_error(
        id,
        NOT_FOUND_CODE,
        format!("run {} has no node at path {}", target.run_id, target.path),
    )
}

#[cfg(feature = "workflow")]
fn parse_definition(
    document: &crate::api::schema::WorkflowDefinitionDocument,
) -> Result<Definition, DefinitionError> {
    match document.format {
        WorkflowDefinitionFormat::Toml => Definition::parse_toml(&document.text),
        WorkflowDefinitionFormat::Json => Definition::parse_json(&document.text),
    }
}

/// `05-phase-plan.md` §4: `workflow.run` rejects a run that omits a required
/// arg with no default. Declared args with a default are filled in here, so the
/// prompt renderer never has to distinguish "absent" from "defaulted".
#[cfg(feature = "workflow")]
fn resolve_run_args(
    kvdag: &Kvdag,
    supplied: &std::collections::HashMap<String, String>,
) -> Result<std::collections::HashMap<String, String>, String> {
    let mut resolved = std::collections::HashMap::with_capacity(kvdag.args.len());
    for arg in &kvdag.args {
        match supplied.get(&arg.name).or(arg.default.as_ref()) {
            Some(value) => {
                resolved.insert(arg.name.clone(), value.clone());
            }
            None if arg.required => {
                return Err(format!(
                    "run argument \"{}\" is required and has no default",
                    arg.name
                ))
            }
            None => {}
        }
    }
    Ok(resolved)
}

#[cfg(feature = "workflow")]
fn head_version_summary(
    cx: &crate::app::workflow_store::StoreContext<'_>,
    head: Option<&KvdagVersionId>,
) -> Result<Option<KvdagVersionSummary>, StoreError> {
    let Some(head) = head else {
        return Ok(None);
    };
    match cx.block_on(cx.store().get_version_record(head)) {
        Ok(Some(record)) => Ok(Some(wire_version_summary(&record))),
        // A workflow whose head version cannot be loaded is still listable;
        // dropping the whole list because one pointer is stale would hide every
        // healthy workflow behind one broken one.
        Ok(None) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The outcome of resolving a `<name|id>` selector against the store, shared
/// by every `workflow.*` handler that accepts one (`workflow.get`,
/// `workflow.version.create`, `workflow.run` — `05-phase-plan.md`'s
/// `kvx workflow show|update|run start <name|id>`). A value already shaped
/// like a `workflow:<key>` record id is used as that id directly; anything
/// else is looked up by exact name — the same "resolve the target server-side"
/// convention `src/app/agents.rs` uses for `agent.get`/`agent.focus`.
#[cfg(feature = "workflow")]
enum WorkflowSelector {
    Found(WorkflowId),
    NotFound,
    Ambiguous,
}

#[cfg(feature = "workflow")]
fn resolve_workflow_selector(
    cx: &crate::app::workflow_store::StoreContext<'_>,
    selector: &str,
) -> Result<WorkflowSelector, StoreError> {
    let candidate = WorkflowId::new(selector.to_string());
    match cx.block_on(cx.store().get_workflow(&candidate)) {
        Ok(Some(_)) => return Ok(WorkflowSelector::Found(candidate)),
        Ok(None) => return Ok(WorkflowSelector::NotFound),
        // Not shaped like a `workflow:<key>` id at all — the selector is a
        // name candidate, not a broken id.
        Err(StoreError::Decode(message)) if message.contains("not a workflow id") => {}
        Err(error) => return Err(error),
    }
    let mut matches = cx
        .block_on(cx.store().find_workflows_by_name(selector))?
        .into_iter();
    match (matches.next(), matches.next()) {
        (None, _) => Ok(WorkflowSelector::NotFound),
        (Some(found), None) => Ok(WorkflowSelector::Found(found.id)),
        (Some(_), Some(_)) => Ok(WorkflowSelector::Ambiguous),
    }
}

/// Which `workflow` row `workflow.create` is going to hang its first version
/// off.
#[cfg(feature = "workflow")]
enum CreateTarget {
    /// The ordinary path: no workflow held that name, so one was created.
    Fresh(WorkflowId),
    /// A row holding the name that has no head version and no version rows at
    /// all — the residue a half-committed create left behind before
    /// `workflow.create` pre-validated (see `handle_workflow_create`). Such a
    /// row cannot be shown, run, or revised in any meaningful way; it only
    /// squats the name, and there is no `workflow delete` to clear it. So a
    /// create naming it fills it in rather than refusing, which is what makes
    /// names burned by 0.12.0 recoverable without a new command.
    Adopted(WorkflowId),
    /// A real workflow — one with a version — already holds the name.
    NameTaken,
}

/// `workflow.create`'s two outcomes past the store boundary. `NameTaken` is
/// carried back as data rather than as a [`StoreError`] because nothing failed
/// in the store: the answer is a refusal the handler renders itself.
#[cfg(feature = "workflow")]
enum CreateResult<T> {
    Created(T),
    NameTaken,
}

/// Resolves [`CreateTarget`], creating the row on the ordinary path.
#[cfg(feature = "workflow")]
fn create_target(
    cx: &crate::app::workflow_store::StoreContext<'_>,
    definition: &Definition,
) -> Result<CreateTarget, StoreError> {
    let name = definition.name.trim();
    let mut existing = cx
        .block_on(cx.store().find_workflows_by_name(name))?
        .into_iter();
    if let Some(found) = existing.next() {
        // `workflow_name` is UNIQUE, so a second match is not reachable under
        // the current schema; if one ever is, adopting *either* row would be a
        // guess, and refusing is the safe answer.
        if existing.next().is_none() && is_versionless(cx, &found)? {
            tracing::warn!(
                workflow_id = %found.id,
                name,
                "adopting a version-less workflow row left behind by an abandoned create"
            );
            return Ok(CreateTarget::Adopted(found.id));
        }
        return Ok(CreateTarget::NameTaken);
    }
    let workflow_id = cx.block_on(cx.store().create_workflow(
        name,
        &definition.description,
        definition.tier(),
    ))?;
    Ok(CreateTarget::Fresh(workflow_id))
}

/// Whether a workflow row has no version behind it at all.
///
/// Both halves are checked: `head_version` is cleared on nothing and set
/// immediately after v1 is written, so a missing head is the usual marker —
/// but a create that died between `create_version` and `set_head_version` would
/// leave a v1 with no head, and adopting *that* would write a second v1-shaped
/// version beside an orphan. Version 1 is asked for directly because it is the
/// first number the store ever assigns.
#[cfg(feature = "workflow")]
fn is_versionless(
    cx: &crate::app::workflow_store::StoreContext<'_>,
    workflow: &crate::workflow::store::WorkflowSummary,
) -> Result<bool, StoreError> {
    if workflow.head_version.is_some() {
        return Ok(false);
    }
    Ok(cx
        .block_on(cx.store().find_version_id(&workflow.id, 1))?
        .is_none())
}

/// A store lookup that first resolves a `<name|id>` selector: `NotFound`/
/// `Ambiguous` short-circuit before whatever the handler would otherwise have
/// gone on to read.
#[cfg(feature = "workflow")]
enum LookupResult<T> {
    Found(T),
    NotFound,
    Ambiguous,
}

// ── wire conversions ────────────────────────────────────────────────────────

#[cfg(feature = "workflow")]
fn wire_workflow_summary(
    summary: crate::workflow::store::WorkflowSummary,
    head: Option<&KvdagVersionSummary>,
) -> WorkflowSummary {
    WorkflowSummary {
        workflow_id: summary.id.to_string(),
        name: summary.name,
        description: summary.description,
        default_tier: wire_tier(summary.default_tier),
        archived: summary.archived,
        head_version_id: summary
            .head_version
            .as_ref()
            .map(std::string::ToString::to_string),
        head_version: head.map(|head| head.version),
        created_at_unix_ms: summary.created_at_unix_ms,
        updated_at_unix_ms: summary.updated_at_unix_ms,
    }
}

#[cfg(feature = "workflow")]
fn wire_version_summary(record: &VersionRecord) -> KvdagVersionSummary {
    KvdagVersionSummary {
        version_id: record.version_id.to_string(),
        workflow_id: record.workflow.to_string(),
        version: record.version,
        parent_version_id: record
            .parent_version_id
            .as_ref()
            .map(std::string::ToString::to_string),
        origin: wire_origin(record.origin),
        change_summary: record.change_summary.clone(),
        spec_digest: record.spec_digest.clone(),
        max_depth: u32::from(record.max_depth),
        max_nodes: u32::from(record.max_nodes),
        created_at_unix_ms: record.created_at_unix_ms,
    }
}

#[cfg(feature = "workflow")]
fn wire_version_detail(kvdag: &Kvdag, record: &VersionRecord) -> KvdagVersionDetail {
    KvdagVersionDetail {
        version_id: kvdag.version_id.to_string(),
        workflow_id: kvdag.workflow_id.to_string(),
        version: kvdag.version,
        parent_version_id: kvdag.parent.as_ref().map(std::string::ToString::to_string),
        origin: wire_origin(record.origin),
        change_summary: record.change_summary.clone(),
        contract: kvdag.contract.clone(),
        args: kvdag
            .args
            .iter()
            .map(|arg| WorkflowArgSpec {
                name: arg.name.clone(),
                required: arg.required,
                default: arg.default.clone(),
                description: arg.description.clone(),
            })
            .collect(),
        max_depth: u32::from(kvdag.growth.max_depth),
        max_nodes: u32::from(kvdag.growth.max_nodes),
        spec_digest: kvdag.spec_digest.as_str().to_string(),
        created_at_unix_ms: record.created_at_unix_ms,
        nodes: kvdag.nodes.iter().map(wire_node_info).collect(),
        edges: kvdag.edges.iter().map(wire_edge_info).collect(),
    }
}

#[cfg(feature = "workflow")]
fn wire_node_info(node: &KvdagNode) -> KvdagNodeInfo {
    KvdagNodeInfo {
        node_key: node.key.to_string(),
        label: node.label.clone(),
        role: node.role.clone(),
        kind: match node.kind {
            NodeKind::Agent => WorkflowNodeKind::Agent,
            NodeKind::Internal => WorkflowNodeKind::Internal,
            NodeKind::Gate => WorkflowNodeKind::Gate,
            NodeKind::Monitor => WorkflowNodeKind::Monitor,
        },
        runner: match node.runner {
            Runner::Agent => WorkflowRunner::Agent,
            Runner::Command => WorkflowRunner::Command,
        },
        command: node.command.clone(),
        demand: wire_demand(node.demand),
        prompt_template: node.prompt_template.clone(),
        system_contract: node.system_contract.clone(),
        output_schema: node.output_schema.as_json().clone(),
        max_attempts: u32::from(node.max_attempts),
        timeout_ms: node.timeout_ms,
        isolation: match node.isolation {
            Isolation::None => WorkflowIsolation::None,
            Isolation::Worktree => WorkflowIsolation::Worktree,
        },
        is_template: node.is_template,
        expand_allow: node
            .expand_allow
            .iter()
            .map(std::string::ToString::to_string)
            .collect(),
        expand_max: u32::from(node.expand_max),
    }
}

#[cfg(feature = "workflow")]
fn wire_edge_info(edge: &KvdagEdge) -> KvdagEdgeInfo {
    KvdagEdgeInfo {
        from: edge.from.to_string(),
        to: edge.to.to_string(),
        kind: wire_edge_kind(edge.kind),
        condition: edge
            .condition
            .as_ref()
            .and_then(|condition| serde_json::to_value(condition).ok()),
        payload: match edge.payload {
            EdgePayload::None => WorkflowEdgePayload::None,
            EdgePayload::Summary => WorkflowEdgePayload::Summary,
            EdgePayload::Full => WorkflowEdgePayload::Full,
        },
        port: edge.port.clone(),
    }
}

/// Maps one journalled limit's kind spelling back to the wire enum. Fail
/// closed: a string `commit` never wrote (a future kind this build does not
/// know about) is reported as no limit at all rather than guessed, and never
/// panics.
#[cfg(feature = "workflow")]
fn wire_stored_growth_limit_kind(kind: &str) -> Option<WorkflowGrowthLimitKind> {
    match kind {
        "expand_max" => Some(WorkflowGrowthLimitKind::ExpandMax),
        "max_depth" => Some(WorkflowGrowthLimitKind::MaxDepth),
        "max_nodes" => Some(WorkflowGrowthLimitKind::MaxNodes),
        _ => None,
    }
}

#[cfg(feature = "workflow")]
fn wire_stored_growth_limit(
    limit: &crate::workflow::store::StoredGrowthLimit,
) -> Option<WorkflowGrowthLimit> {
    Some(WorkflowGrowthLimit {
        kind: wire_stored_growth_limit_kind(&limit.kind)?,
        limit_value: limit.limit_value,
        requested: limit.requested,
        accepted: limit.accepted,
        at_unix_ms: limit.at_unix_ms,
        message: limit.message.clone(),
    })
}

#[cfg(feature = "workflow")]
pub(crate) fn wire_run_record(
    record: crate::workflow::store::RunRecord,
    limits: &StoredGrowthLimits,
) -> WorkflowRunInfo {
    WorkflowRunInfo {
        run_id: record.id.to_string(),
        workflow_id: record.workflow.to_string(),
        version_id: record.version.to_string(),
        tier: wire_tier(record.tier),
        status: wire_run_status(record.status),
        args: record.args.into_iter().collect(),
        workspace_id: record.workspace_id,
        tab_id: record.tab_id,
        started_at_unix_ms: record.started_at_unix_ms,
        ended_at_unix_ms: record.ended_at_unix_ms,
        total_tokens: record.total_tokens,
        total_tool_uses: u64::from(record.total_tool_uses),
        nodes_total: record.nodes_total,
        nodes_done: record.nodes_done,
        failure: record.failure,
        max_depth: u32::from(record.max_depth),
        max_nodes: u32::from(record.max_nodes),
        // Every materialised node counts against `max_nodes` regardless of
        // status, which is exactly what `nodes_total` records.
        nodes_live: record.nodes_total,
        // B2: recovered from the `growth_limited` journal (`limits.last` is
        // whichever node's rejection landed last), not from a `workflow_run`
        // column — the run row still carries none.
        growth_limited: limits.last.as_ref().and_then(wire_stored_growth_limit),
        // §4 D9: denormalised onto the row by one batched lookup server-side, so
        // a cross-workflow listing labels every run without N client calls.
        workflow_name: record.workflow_name,
        // §4 D21: which past runs' summaries this run was given. Recorded per
        // run rather than derived, because `history_context_runs` and the
        // workflow's summary set both change afterwards.
        context_runs: record.context_runs.iter().map(RunId::to_string).collect(),
        restore_from_run: record.restore_from_run.as_ref().map(RunId::to_string),
        // §3.1: the lead binding, learned once the spawned `claude` created
        // its team. Absent on every run from before the rework.
        lead_session_id: record.lead_session_id,
        team_name: record.team_name,
        lead_pane_id: record.lead_pane_id,
        lead_prompt_version: record.lead_prompt_version,
    }
}

#[cfg(feature = "workflow")]
pub(crate) fn wire_run_node_record(
    record: crate::workflow::store::RunNodeRecord,
    limits: &StoredGrowthLimits,
) -> WorkflowRunNodeInfo {
    WorkflowRunNodeInfo {
        path: record.instance_path.to_string(),
        node_key: record.node_key.to_string(),
        // Persisted per instance on `run_node.label`, so a run read back after
        // a restart still names its children the way their proposals did.
        label: record.label.clone(),
        // B1: resolved from `run_node.parent` against this run's own rows
        // (`run_node_record`), rather than dropped on the floor.
        parent_path: record.parent_path.as_ref().map(InstancePath::to_string),
        depth: u32::from(record.depth),
        status: wire_node_status(record.status),
        demand: wire_demand(record.demand),
        model: record.model,
        effort: record.effort,
        attempt: u32::from(record.attempt),
        pane_id: record.pane_id,
        terminal_id: record.terminal_id,
        agent_session_id: record.agent_session_id,
        cwd: record.cwd,
        node_dir: record.node_dir,
        started_at_unix_ms: record.started_at_unix_ms,
        ended_at_unix_ms: record.ended_at_unix_ms,
        total_tokens: record.total_tokens,
        tool_uses: record.tool_uses,
        duration_ms: record.duration_ms,
        evidence: record.evidence.map(wire_evidence),
        succession: record.succession.as_ref().map(wire_succession),
        blocker: record.succession.as_ref().and_then(wire_blocker),
        watchdog_interventions: 0,
        // Written verbatim from the run's assignment table (§4 D9), so a
        // finished run can still explain why a node ran on the model it did.
        // Empty for a fixed tier, whose §7.1/§7.2 row *is* the explanation.
        assignment_reason: record.assignment_reason,
        // Delivery failures live only on the engine — the durable projection
        // has no column for one and cannot report it.
        delivery_failure: None,
        // B2: this node's own last journalled limit, as a *proposer* — the
        // same key the live projection uses (`by_path`, keyed by instance
        // path).
        growth_limited: limits
            .by_path
            .get(&record.instance_path)
            .and_then(wire_stored_growth_limit),
        // M2: written by `write_run_node` since Phase 1 and dropped by the
        // reader until now, which is why historical interrogation after a
        // restart always answered `transcript_unavailable` no matter what was
        // on disk (§4 D6).
        transcript_path: record.transcript_path,
        // §4 D4: provenance, not timestamps — a restored node's own stamps are
        // the restore instant, and this is what says where its result came
        // from.
        restored_from: record.restored_from.as_ref().map(wire_restored_from),
    }
}

/// A restored node's provenance, on the wire (§4 D4).
#[cfg(feature = "workflow")]
fn wire_restored_from(source: &RestoredRef) -> WorkflowRestoredFrom {
    WorkflowRestoredFrom {
        run_id: source.run.to_string(),
        node_key: source.node_key.to_string(),
        checkpoint_seq: source.checkpoint_seq,
    }
}

#[cfg(feature = "workflow")]
fn wire_run_edge_record(record: crate::workflow::store::RunEdgeRecord) -> WorkflowRunEdgeInfo {
    WorkflowRunEdgeInfo {
        from: record.from.to_string(),
        to: record.to.to_string(),
        kind: wire_edge_kind(record.kind),
        condition_result: record.condition_result,
        fired: record.fired,
    }
}

#[cfg(feature = "workflow")]
fn engine_tier(tier: WorkflowTier) -> Tier {
    match tier {
        WorkflowTier::Auto => Tier::Auto,
        WorkflowTier::Max => Tier::Max,
        WorkflowTier::High => Tier::High,
        WorkflowTier::Medium => Tier::Medium,
        WorkflowTier::Low => Tier::Low,
    }
}

#[cfg(feature = "workflow")]
fn wire_origin(origin: VersionOrigin) -> WorkflowVersionOrigin {
    match origin {
        VersionOrigin::Authored => WorkflowVersionOrigin::Authored,
        VersionOrigin::Imported => WorkflowVersionOrigin::Imported,
        VersionOrigin::SelfImprovement => WorkflowVersionOrigin::SelfImprovement,
        VersionOrigin::RestoreRewrite => WorkflowVersionOrigin::RestoreRewrite,
    }
}

fn require_non_empty(id: &str, field: &str, value: &str) -> Option<String> {
    if value.trim().is_empty() {
        Some(encode_error(
            id.to_string(),
            "invalid_params",
            format!("{field} must not be empty"),
        ))
    } else {
        None
    }
}

fn require_node_target(id: &str, target: &WorkflowNodeTarget) -> Option<String> {
    require_non_empty(id, "run_id", &target.run_id)
        .or_else(|| require_non_empty(id, "path", &target.path))
}

fn require_steer_params(id: &str, params: &WorkflowNodeSteerParams) -> Option<String> {
    require_non_empty(id, "run_id", &params.run_id)
        .or_else(|| require_non_empty(id, "path", &params.path))
        .or_else(|| require_non_empty(id, "text", &params.text))
}

fn require_report_params(id: &str, params: &WorkflowNodeReportParams) -> Option<String> {
    require_non_empty(id, "run_id", &params.run_id)
        .or_else(|| require_non_empty(id, "path", &params.path))
        .or_else(|| require_non_empty(id, "token", &params.token))
}

fn require_expand_params(id: &str, params: &WorkflowNodeExpandParams) -> Option<String> {
    require_non_empty(id, "run_id", &params.run_id)
        .or_else(|| require_non_empty(id, "path", &params.path))
        .or_else(|| require_non_empty(id, "token", &params.token))
        .or_else(|| require_non_empty(id, "template", &params.template))
}

/// One `run_member` row, as the wire sees it.
#[cfg(feature = "workflow")]
fn wire_run_member_record(
    record: crate::workflow::store::RunMemberRecord,
) -> crate::api::schema::WorkflowRunMemberInfo {
    crate::api::schema::WorkflowRunMemberInfo {
        name: record.name,
        agent_type: record.agent_type,
        model: record.model,
        pane_id: record.pane_id,
        backend_type: record.backend_type,
        is_active: record.is_active,
        cwd: record.cwd,
        first_seen_at_unix_ms: record.first_seen_at_unix_ms,
        last_seen_at_unix_ms: record.last_seen_at_unix_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{Method, WorkflowDefinitionDocument, WorkflowDefinitionFormat};
    use crate::config::Config;
    use std::collections::HashMap;

    fn app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        #[cfg_attr(not(feature = "workflow"), allow(unused_mut))]
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        // Never let a unit test open — or lock — the user's real workflow
        // database. With the feature off there is no store to redirect.
        #[cfg(feature = "workflow")]
        {
            app.workflow_store = crate::app::workflow_store::WorkflowStoreHandle::in_memory();
        }
        app
    }

    fn definition() -> WorkflowDefinitionDocument {
        WorkflowDefinitionDocument {
            format: WorkflowDefinitionFormat::Toml,
            text: "name = \"ship-feature\"\n".to_string(),
        }
    }

    fn error_code(response: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(response).unwrap();
        value["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn empty_workflow_id_is_rejected_before_the_engine_boundary() {
        let mut app = app();
        let response = app.handle_workflow_get(
            "req".into(),
            WorkflowTarget {
                workflow_id: String::new(),
            },
        );
        assert_eq!(error_code(&response), "invalid_params");
    }

    #[test]
    fn empty_definition_text_is_rejected() {
        let mut app = app();
        let response = app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: WorkflowDefinitionDocument {
                    format: WorkflowDefinitionFormat::Toml,
                    text: String::new(),
                },
            },
        );
        assert_eq!(error_code(&response), "invalid_params");
    }

    #[test]
    fn empty_node_path_is_rejected() {
        let mut app = app();
        let response = app.handle_workflow_node_steer(
            "req".into(),
            WorkflowNodeSteerParams {
                run_id: "workflow_run:1".into(),
                path: String::new(),
                text: "keep going".into(),
            },
        );
        assert_eq!(error_code(&response), "invalid_params");
    }

    /// `05-phase-plan.md` §6: the primary dispatch in `src/app/api.rs` ends
    /// in a catch-all that answers `not_implemented`, and the compiler does
    /// not catch a forgotten `workflow.*` arm. This sweep is the safety net.
    #[test]
    fn no_workflow_method_falls_through_to_not_implemented() {
        let mut app = app();
        let methods = vec![
            Method::WorkflowList(crate::api::schema::EmptyParams::default()),
            Method::WorkflowGet(WorkflowTarget {
                workflow_id: "workflow:1".into(),
            }),
            Method::WorkflowCreate(WorkflowCreateParams {
                definition: definition(),
            }),
            Method::WorkflowVersionCreate(WorkflowVersionCreateParams {
                workflow_id: "workflow:1".into(),
                definition: definition(),
                change_summary: String::new(),
            }),
            Method::WorkflowVersionGet(WorkflowVersionTarget {
                version_id: "kvdag_version:1".into(),
            }),
            Method::WorkflowRun(WorkflowRunParams {
                workflow_id: "workflow:1".into(),
                version: None,
                tier: None,
                args: HashMap::new(),
                restore_from: None,
                include_prior_summaries: None,
            }),
            Method::WorkflowRunGet(WorkflowRunTarget {
                run_id: "workflow_run:1".into(),
            }),
            Method::WorkflowRunList(WorkflowRunListParams {
                workflow_id: Some("workflow:1".into()),
                limit: None,
            }),
            Method::WorkflowRunCancel(WorkflowRunTarget {
                run_id: "workflow_run:1".into(),
            }),
            Method::WorkflowNodeGet(WorkflowNodeTarget {
                run_id: "workflow_run:1".into(),
                path: "plan".into(),
            }),
            Method::WorkflowNodeSteer(WorkflowNodeSteerParams {
                run_id: "workflow_run:1".into(),
                path: "plan".into(),
                text: "keep going".into(),
            }),
            Method::WorkflowNodeInterrupt(WorkflowNodeTarget {
                run_id: "workflow_run:1".into(),
                path: "plan".into(),
            }),
            Method::WorkflowNodeReport(WorkflowNodeReportParams {
                run_id: "workflow_run:1".into(),
                path: "plan".into(),
                token: "node-token".into(),
                result: serde_json::json!({}),
            }),
            Method::WorkflowNodeRestart(WorkflowNodeTarget {
                run_id: "workflow_run:1".into(),
                path: "plan".into(),
            }),
            Method::WorkflowNodeExpand(crate::api::schema::WorkflowNodeExpandParams {
                run_id: "workflow_run:1".into(),
                path: "plan".into(),
                token: "node-token".into(),
                template: "worker".into(),
                label: String::new(),
                inputs: HashMap::new(),
                count: None,
            }),
            // Phase 3 additions (`07-phase3-plan.md` §WS-C, §WS-D).
            Method::WorkflowNodeInterrogate(WorkflowNodeInterrogateParams {
                run_id: "workflow_run:1".into(),
                path: "plan".into(),
                mode: crate::api::schema::WorkflowInterrogationMode::Resumed,
                note: None,
            }),
            Method::WorkflowSummaryGet(WorkflowRunTarget {
                run_id: "workflow_run:1".into(),
            }),
            Method::WorkflowSummaryList(WorkflowSummaryListParams {
                workflow_id: None,
                limit: None,
            }),
        ];

        for method in methods {
            let response = app.dispatch_api_request("req", method.clone());
            let code = error_code(&response);
            assert_ne!(
                code, "not_implemented",
                "{method:?} fell through to the not_implemented catch-all"
            );
        }
    }

    /// `workflow` is a default feature, so every shipped binary answers from
    /// the store; a `--no-default-features` build is the one configuration
    /// that reaches this path — the MSVC cross-lint and slim source builds.
    /// The sweep above only rules out `not_implemented`; this pins the exact
    /// documented code so a feature-off build can never start answering
    /// `workflow.*` with something a client cannot recognise.
    #[cfg(not(feature = "workflow"))]
    #[test]
    fn every_workflow_method_reports_workflow_unavailable_with_the_feature_off() {
        let mut app = app();

        // Valid params throughout: an invalid-params rejection happens before
        // the engine boundary and would hide the code under test.
        let methods = vec![
            Method::WorkflowList(crate::api::schema::EmptyParams::default()),
            Method::WorkflowGet(WorkflowTarget {
                workflow_id: "workflow:1".into(),
            }),
            Method::WorkflowCreate(WorkflowCreateParams {
                definition: definition(),
            }),
            Method::WorkflowRun(WorkflowRunParams {
                workflow_id: "workflow:1".into(),
                version: None,
                tier: None,
                args: HashMap::new(),
                restore_from: None,
                include_prior_summaries: None,
            }),
            Method::WorkflowRunList(WorkflowRunListParams {
                workflow_id: Some("workflow:1".into()),
                limit: None,
            }),
            Method::WorkflowNodeSteer(WorkflowNodeSteerParams {
                run_id: "workflow_run:1".into(),
                path: "plan".into(),
                text: "keep going".into(),
            }),
            Method::WorkflowNodeExpand(crate::api::schema::WorkflowNodeExpandParams {
                run_id: "workflow_run:1".into(),
                path: "plan".into(),
                token: "node-token".into(),
                template: "worker".into(),
                label: String::new(),
                inputs: HashMap::new(),
                count: None,
            }),
            // Phase 3 additions (`07-phase3-plan.md` §WS-D "Tested"): a
            // feature-off build must answer these with the documented code too,
            // not with the catch-all the sweep above only rules out.
            Method::WorkflowNodeInterrogate(WorkflowNodeInterrogateParams {
                run_id: "workflow_run:1".into(),
                path: "plan".into(),
                mode: crate::api::schema::WorkflowInterrogationMode::Resumed,
                note: None,
            }),
            Method::WorkflowSummaryGet(WorkflowRunTarget {
                run_id: "workflow_run:1".into(),
            }),
            Method::WorkflowSummaryList(WorkflowSummaryListParams {
                workflow_id: None,
                limit: None,
            }),
        ];

        for method in methods {
            let response = app.dispatch_api_request("req", method.clone());
            assert_eq!(
                error_code(&response),
                "workflow_unavailable",
                "{method:?} did not report workflow_unavailable"
            );
        }
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn a_created_workflow_is_listed_and_readable() {
        let mut app = app();
        let created = app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: WorkflowDefinitionDocument {
                    format: WorkflowDefinitionFormat::Toml,
                    text: r#"
name = "handler-test"
[[arg]]
name = "goal"
required = true
[[node]]
key = "only"
label = "Only"
runner = "command"
command = ["/bin/true"]
prompt_template = "do {{goal}}"
output_schema = { type = "object" }
"#
                    .to_string(),
                },
            },
        );
        let value: serde_json::Value = serde_json::from_str(&created).unwrap();
        assert_eq!(
            value["result"]["type"], "workflow_created",
            "unexpected: {value}"
        );
        let workflow_id = value["result"]["workflow"]["workflow_id"]
            .as_str()
            .expect("a created workflow carries an id")
            .to_string();
        assert_eq!(value["result"]["version"]["version"], 1);

        let listed = app.handle_workflow_list("req".into());
        let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
        let workflows = listed["result"]["workflows"].as_array().unwrap();
        assert_eq!(workflows.len(), 1);
        assert_eq!(workflows[0]["name"], "handler-test");
        assert_eq!(workflows[0]["head_version"], 1);

        let fetched = app.handle_workflow_get(
            "req".into(),
            WorkflowTarget {
                workflow_id: workflow_id.clone(),
            },
        );
        let fetched: serde_json::Value = serde_json::from_str(&fetched).unwrap();
        assert_eq!(fetched["result"]["workflow"]["workflow_id"], workflow_id);
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn a_definition_that_is_not_a_valid_kvdag_is_rejected_as_such() {
        let mut app = app();
        let response = app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: WorkflowDefinitionDocument {
                    format: WorkflowDefinitionFormat::Toml,
                    // `{{missing}}` resolves to neither an inbound edge port
                    // nor a declared arg, which `Kvdag::try_new` rejects.
                    text: r#"
name = "bad-template"
[[node]]
key = "only"
label = "Only"
prompt_template = "do {{missing}}"
output_schema = { type = "object" }
"#
                    .to_string(),
                },
            },
        );
        // Not "either code": the graph validators run before the first write
        // now, so which of the two reported it is no longer a coin toss.
        assert_eq!(error_code(&response), INVALID_DEFINITION_CODE, "{response}");
    }

    /// One TOML document per graph-level validator that only `Kvdag::try_new`
    /// owns — `Definition::check` passes all three, which is exactly why they
    /// used to reach the store with the `workflow` row already committed.
    #[cfg(feature = "workflow")]
    fn graph_invalid_definitions() -> Vec<(&'static str, String)> {
        fn node(key: &str) -> String {
            format!(
                "[[node]]\nkey = \"{key}\"\nlabel = \"{key}\"\nrunner = \"command\"\n\
                 command = [\"/bin/true\"]\nprompt_template = \"do it\"\n\
                 output_schema = {{ type = \"object\" }}\n"
            )
        }
        vec![
            (
                "cycle",
                format!(
                    "name = \"zombie-test\"\n{}{}\
                     [[edge]]\nfrom = \"a\"\nto = \"b\"\n\
                     [[edge]]\nfrom = \"b\"\nto = \"a\"\n",
                    node("a"),
                    node("b")
                ),
            ),
            (
                "duplicate node key",
                format!("name = \"zombie-test\"\n{}{}", node("a"), node("a")),
            ),
            (
                "unknown edge endpoint",
                format!(
                    "name = \"zombie-test\"\n{}[[edge]]\nfrom = \"a\"\nto = \"ghost\"\n",
                    node("a")
                ),
            ),
        ]
    }

    #[cfg(feature = "workflow")]
    fn create_toml(app: &mut App, text: &str) -> String {
        app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: WorkflowDefinitionDocument {
                    format: WorkflowDefinitionFormat::Toml,
                    text: text.to_string(),
                },
            },
        )
    }

    #[cfg(feature = "workflow")]
    fn listed_workflows(app: &mut App) -> Vec<serde_json::Value> {
        let listed = app.handle_workflow_list("req".into());
        let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
        listed["result"]["workflows"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// The zombie-row regression. `create_workflow` and `create_version` are
    /// two commits; a graph the second one rejects used to leave the first one
    /// behind as a version-less workflow that squatted the name forever,
    /// because there is no `workflow delete`.
    #[cfg(feature = "workflow")]
    #[test]
    fn a_graph_invalid_create_writes_nothing_and_leaves_the_name_usable() {
        for (label, text) in graph_invalid_definitions() {
            let mut app = app();
            let response = create_toml(&mut app, &text);
            assert_eq!(
                error_code(&response),
                INVALID_DEFINITION_CODE,
                "{label}: {response}"
            );
            assert!(
                listed_workflows(&mut app).is_empty(),
                "{label}: a rejected create left a workflow row behind: {:?}",
                listed_workflows(&mut app)
            );

            // …and the name it was rejected under is still available.
            let valid = create_toml(
                &mut app,
                r#"
name = "zombie-test"
[[node]]
key = "only"
label = "Only"
runner = "command"
command = ["/bin/true"]
prompt_template = "do it"
output_schema = { type = "object" }
"#,
            );
            let valid: serde_json::Value = serde_json::from_str(&valid).unwrap();
            assert_eq!(
                valid["result"]["type"], "workflow_created",
                "{label}: the name stayed burned: {valid}"
            );
        }
    }

    /// The other half of the same incident: the retry after a burned name
    /// answered with SurrealDB's own index message ("Database index
    /// workflow_name already contains …"). A genuine collision now has a code
    /// of its own and a message written for a human.
    #[cfg(feature = "workflow")]
    #[test]
    fn creating_a_workflow_under_a_taken_name_is_a_named_refusal() {
        let definition = r#"
name = "taken"
[[node]]
key = "only"
label = "Only"
runner = "command"
command = ["/bin/true"]
prompt_template = "do it"
output_schema = { type = "object" }
"#;
        let mut app = app();
        let first: serde_json::Value = serde_json::from_str(&create_toml(&mut app, definition))
            .expect("the first create is valid json");
        assert_eq!(first["result"]["type"], "workflow_created", "{first}");

        let response = create_toml(&mut app, definition);
        assert_eq!(
            error_code(&response),
            WORKFLOW_NAME_TAKEN_CODE,
            "{response}"
        );
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        let message = value["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains("taken"), "{message}");
        assert!(
            !message.to_ascii_lowercase().contains("database index"),
            "the raw store message leaked through: {message}"
        );
        assert_eq!(listed_workflows(&mut app).len(), 1, "{response}");
    }

    /// Names already burned by a 0.12.0 zombie stay recoverable: a row with no
    /// head and no versions is filled in by the next create rather than
    /// refusing it, which is the only escape hatch available without a new
    /// `workflow delete` command.
    #[cfg(feature = "workflow")]
    #[test]
    fn a_create_adopts_a_version_less_row_that_is_squatting_the_name() {
        let mut app = app();
        // Exactly the residue the old two-commit path left: the row, and
        // nothing else.
        let zombie = app
            .workflow_store
            .call(|cx| cx.block_on(cx.store().create_workflow("squatted", "", Tier::High)))
            .expect("the in-memory store is available")
            .expect("creating the row succeeds");

        let created: serde_json::Value = serde_json::from_str(&create_toml(
            &mut app,
            r#"
name = "squatted"
description = "the real one"
default_tier = "low"
[[node]]
key = "only"
label = "Only"
runner = "command"
command = ["/bin/true"]
prompt_template = "do it"
output_schema = { type = "object" }
"#,
        ))
        .unwrap();
        assert_eq!(
            created["result"]["type"], "workflow_created",
            "the burned name is still unusable: {created}"
        );
        // Adopted, not duplicated — the unique index would have refused a
        // second row anyway, so a second listing entry would mean the id moved.
        assert_eq!(
            created["result"]["workflow"]["workflow_id"],
            zombie.as_str()
        );
        assert_eq!(created["result"]["version"]["version"], 1);
        let workflows = listed_workflows(&mut app);
        assert_eq!(workflows.len(), 1, "{workflows:?}");
        // H5: the adopted row describes the document that filled it in, not
        // the empty placeholder it was created with.
        assert_eq!(workflows[0]["description"], "the real one");
        assert_eq!(workflows[0]["default_tier"], "low");
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn a_run_that_omits_a_required_argument_is_refused() {
        let mut app = app();
        let created = app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: WorkflowDefinitionDocument {
                    format: WorkflowDefinitionFormat::Toml,
                    text: r#"
name = "needs-arg"
[[arg]]
name = "goal"
required = true
[[node]]
key = "only"
label = "Only"
runner = "command"
command = ["/bin/true"]
prompt_template = "do {{goal}}"
output_schema = { type = "object" }
"#
                    .to_string(),
                },
            },
        );
        let created: serde_json::Value = serde_json::from_str(&created).unwrap();
        let workflow_id = created["result"]["workflow"]["workflow_id"]
            .as_str()
            .unwrap()
            .to_string();

        let response = app.handle_workflow_run(
            "req".into(),
            WorkflowRunParams {
                workflow_id,
                version: None,
                tier: None,
                args: HashMap::new(),
                restore_from: None,
                include_prior_summaries: None,
            },
        );
        assert_eq!(error_code(&response), MISSING_ARG_CODE);
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn node_methods_refuse_a_run_this_server_is_not_executing() {
        let mut app = app();
        for response in [
            app.handle_workflow_node_steer(
                "req".into(),
                WorkflowNodeSteerParams {
                    run_id: "workflow_run:ghost".into(),
                    path: "plan".into(),
                    text: "keep going".into(),
                },
            ),
            app.handle_workflow_node_interrupt(
                "req".into(),
                WorkflowNodeTarget {
                    run_id: "workflow_run:ghost".into(),
                    path: "plan".into(),
                },
            ),
            app.handle_workflow_node_restart(
                "req".into(),
                WorkflowNodeTarget {
                    run_id: "workflow_run:ghost".into(),
                    path: "plan".into(),
                },
            ),
            app.handle_workflow_run_cancel(
                "req".into(),
                WorkflowRunTarget {
                    run_id: "workflow_run:ghost".into(),
                },
            ),
        ] {
            assert_eq!(error_code(&response), NO_ACTIVE_RUN_CODE, "{response}");
        }
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn an_unknown_run_is_reported_as_not_found_rather_than_empty() {
        let mut app = app();
        let response = app.handle_workflow_run_get(
            "req".into(),
            WorkflowRunTarget {
                run_id: "workflow_run:ghost".into(),
            },
        );
        assert_eq!(error_code(&response), NOT_FOUND_CODE, "{response}");
    }

    #[cfg(feature = "workflow")]
    fn single_node_definition(name: &str, prompt: &str) -> WorkflowDefinitionDocument {
        WorkflowDefinitionDocument {
            format: WorkflowDefinitionFormat::Toml,
            text: format!(
                r#"
name = "{name}"
[[node]]
key = "only"
label = "Only"
runner = "command"
command = ["/bin/true"]
prompt_template = "{prompt}"
output_schema = {{ type = "object" }}
"#
            ),
        }
    }

    #[cfg(feature = "workflow")]
    fn required_arg_definition(name: &str) -> WorkflowDefinitionDocument {
        WorkflowDefinitionDocument {
            format: WorkflowDefinitionFormat::Toml,
            text: format!(
                r#"
name = "{name}"
[[arg]]
name = "goal"
required = true
[[node]]
key = "only"
label = "Only"
runner = "command"
command = ["/bin/true"]
prompt_template = "do {{{{goal}}}}"
output_schema = {{ type = "object" }}
"#
            ),
        }
    }

    /// A7: `workflow.get`'s plural `versions` field must expose the whole
    /// immutable parent chain — including v1 with its real `origin` and
    /// `change_summary` — not just a copy of the head.
    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_get_returns_the_full_version_chain_with_real_metadata() {
        let mut app = app();
        let created = app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: single_node_definition("ship-feature", "v1 prompt"),
            },
        );
        let created: serde_json::Value = serde_json::from_str(&created).unwrap();
        let workflow_id = created["result"]["workflow"]["workflow_id"]
            .as_str()
            .unwrap()
            .to_string();

        let updated = app.handle_workflow_version_create(
            "req".into(),
            WorkflowVersionCreateParams {
                workflow_id: workflow_id.clone(),
                definition: single_node_definition("ship-feature", "v2 prompt"),
                change_summary: "widened the prompt".into(),
            },
        );
        let updated: serde_json::Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(
            updated["result"]["type"], "workflow_version_created",
            "unexpected: {updated}"
        );
        assert_eq!(updated["result"]["version"]["version"], 2);

        let shown = app.handle_workflow_get(
            "req".into(),
            WorkflowTarget {
                workflow_id: workflow_id.clone(),
            },
        );
        let shown: serde_json::Value = serde_json::from_str(&shown).unwrap();
        let versions = shown["result"]["versions"].as_array().unwrap_or_else(|| {
            panic!("`versions` must be an array: {shown}");
        });
        assert_eq!(
            versions.len(),
            2,
            "v1 must stay listable after v2 exists, not just the head: {shown}"
        );

        assert_eq!(versions[0]["version"], 2, "newest first");
        assert_eq!(versions[0]["origin"], "authored");
        assert_eq!(versions[0]["change_summary"], "widened the prompt");
        assert_eq!(
            versions[0]["parent_version_id"], versions[1]["version_id"],
            "the chain must actually be parent-linked"
        );

        assert_eq!(versions[1]["version"], 1);
        assert_eq!(versions[1]["origin"], "authored");
        assert_eq!(
            versions[1]["change_summary"], "",
            "v1 has no change_summary (none was supplied at create time)"
        );
        assert_ne!(
            versions[0]["version_id"], versions[1]["version_id"],
            "v1 and v2 must be distinct, inspectable versions"
        );
    }

    /// A8: `created_at_unix_ms` must be the store's real timestamp, not the
    /// hardcoded `0` every create/update/show response used to carry.
    #[cfg(feature = "workflow")]
    #[test]
    fn created_at_unix_ms_is_populated_on_create_update_and_show() {
        let mut app = app();
        let created = app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: single_node_definition("timestamped", "v1"),
            },
        );
        let created: serde_json::Value = serde_json::from_str(&created).unwrap();
        let created_at = created["result"]["version"]["created_at_unix_ms"]
            .as_u64()
            .expect("created_at_unix_ms must be a number");
        assert!(created_at > 0, "unexpected: {created}");

        let workflow_id = created["result"]["workflow"]["workflow_id"]
            .as_str()
            .unwrap()
            .to_string();

        let updated = app.handle_workflow_version_create(
            "req".into(),
            WorkflowVersionCreateParams {
                workflow_id: workflow_id.clone(),
                definition: single_node_definition("timestamped", "v2"),
                change_summary: String::new(),
            },
        );
        let updated: serde_json::Value = serde_json::from_str(&updated).unwrap();
        let updated_at = updated["result"]["version"]["created_at_unix_ms"]
            .as_u64()
            .expect("created_at_unix_ms must be a number");
        assert!(updated_at > 0, "unexpected: {updated}");

        let shown = app.handle_workflow_get("req".into(), WorkflowTarget { workflow_id });
        let shown: serde_json::Value = serde_json::from_str(&shown).unwrap();
        for version in shown["result"]["versions"].as_array().unwrap() {
            assert!(
                version["created_at_unix_ms"].as_u64().unwrap_or(0) > 0,
                "unexpected: {version}"
            );
        }
    }

    /// A9: `workflow show <name|id>` — the CLI passes the selector through
    /// verbatim (`src/cli/workflow.rs`'s `parse_workflow_show_args`), so the
    /// name has to resolve here, server-side.
    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_get_resolves_a_unique_name_to_its_id() {
        let mut app = app();
        let created = app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: single_node_definition("ship-feature", "v1"),
            },
        );
        let created: serde_json::Value = serde_json::from_str(&created).unwrap();
        let workflow_id = created["result"]["workflow"]["workflow_id"]
            .as_str()
            .unwrap()
            .to_string();

        let fetched = app.handle_workflow_get(
            "req".into(),
            WorkflowTarget {
                workflow_id: "ship-feature".into(),
            },
        );
        let fetched: serde_json::Value = serde_json::from_str(&fetched).unwrap();
        assert_eq!(
            fetched["result"]["workflow"]["workflow_id"], workflow_id,
            "unexpected: {fetched}"
        );
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_get_reports_not_found_for_an_unknown_name() {
        let mut app = app();
        let response = app.handle_workflow_get(
            "req".into(),
            WorkflowTarget {
                workflow_id: "does-not-exist".into(),
            },
        );
        assert_eq!(error_code(&response), NOT_FOUND_CODE, "{response}");
    }

    /// A9: `workflow update <name|id>` — same server-side resolution as show.
    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_version_create_resolves_a_unique_name_to_its_id() {
        let mut app = app();
        app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: single_node_definition("ship-feature", "v1"),
            },
        );

        let response = app.handle_workflow_version_create(
            "req".into(),
            WorkflowVersionCreateParams {
                workflow_id: "ship-feature".into(),
                definition: single_node_definition("ship-feature", "v2"),
                change_summary: "by name".into(),
            },
        );
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            response["result"]["type"], "workflow_version_created",
            "unexpected: {response}"
        );
        assert_eq!(response["result"]["version"]["version"], 2);
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_version_create_reports_not_found_for_an_unknown_name() {
        let mut app = app();
        let response = app.handle_workflow_version_create(
            "req".into(),
            WorkflowVersionCreateParams {
                workflow_id: "does-not-exist".into(),
                definition: single_node_definition("does-not-exist", "v1"),
                change_summary: String::new(),
            },
        );
        assert_eq!(error_code(&response), NOT_FOUND_CODE, "{response}");
    }

    /// A9: `workflow run start <name|id>` — reaching the missing-argument
    /// check (rather than `workflow_not_found`) proves the name resolved to
    /// the workflow just created, without this test having to drive a real
    /// pane spawn.
    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_run_resolves_a_unique_name_to_its_id() {
        let mut app = app();
        app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: required_arg_definition("ship-feature"),
            },
        );

        let response = app.handle_workflow_run(
            "req".into(),
            WorkflowRunParams {
                workflow_id: "ship-feature".into(),
                version: None,
                tier: None,
                args: HashMap::new(),
                restore_from: None,
                include_prior_summaries: None,
            },
        );
        assert_eq!(error_code(&response), MISSING_ARG_CODE, "{response}");
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_run_reports_not_found_for_an_unknown_name() {
        let mut app = app();
        let response = app.handle_workflow_run(
            "req".into(),
            WorkflowRunParams {
                workflow_id: "does-not-exist".into(),
                version: None,
                tier: None,
                args: HashMap::new(),
                restore_from: None,
                include_prior_summaries: None,
            },
        );
        assert_eq!(error_code(&response), NOT_FOUND_CODE, "{response}");
    }

    /// A9: `workflow run list <name|id>` takes the same selector every other
    /// targeted verb does. It was the one left speaking record ids only, so a
    /// user who created and listed a workflow by name could not list its runs
    /// by that name — the store answered `not a workflow id: <name>`.
    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_run_list_resolves_a_unique_name_to_its_id() {
        let mut app = app();
        app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: single_node_definition("ship-feature", "v1"),
            },
        );

        let response = app.handle_workflow_run_list(
            "req".into(),
            WorkflowRunListParams {
                workflow_id: Some("ship-feature".into()),
                limit: None,
            },
        );
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            response["result"]["type"], "workflow_run_list",
            "unexpected: {response}"
        );
        assert_eq!(
            response["result"]["runs"].as_array().map(Vec::len),
            Some(0),
            "a freshly created workflow has no runs yet: {response}"
        );
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_run_list_reports_not_found_for_an_unknown_name() {
        let mut app = app();
        let response = app.handle_workflow_run_list(
            "req".into(),
            WorkflowRunListParams {
                workflow_id: Some("does-not-exist".into()),
                limit: None,
            },
        );
        assert_eq!(error_code(&response), NOT_FOUND_CODE, "{response}");
    }

    /// Starts a single-node `runner = "command"` workflow and binds its node to
    /// `pane`, so the node methods have a live run and a bound node to address.
    /// The pane need not exist: what the delivery path does with a pane it
    /// cannot write to is exactly what these tests are about.
    #[cfg(feature = "workflow")]
    fn app_with_a_bound_command_node(pane: &str) -> (App, String) {
        use crate::workflow::model::{NodeBinding, NodeToken};
        use std::path::PathBuf;

        let mut app = app();
        let created = app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: WorkflowDefinitionDocument {
                    format: WorkflowDefinitionFormat::Toml,
                    text: r#"
name = "control-surface"
[[node]]
key = "only"
label = "Only"
runner = "command"
command = ["/bin/true"]
prompt_template = "run it"
output_schema = { type = "object", required = ["done"] }
"#
                    .to_string(),
                },
            },
        );
        let created: serde_json::Value = serde_json::from_str(&created).unwrap();
        let workflow_id = created["result"]["workflow"]["workflow_id"]
            .as_str()
            .expect("the workflow was created")
            .to_string();

        let started = app.handle_workflow_run(
            "req".into(),
            WorkflowRunParams {
                workflow_id,
                version: None,
                tier: None,
                args: HashMap::new(),
                restore_from: None,
                include_prior_summaries: None,
            },
        );
        let started: serde_json::Value = serde_json::from_str(&started).unwrap();
        let run_id = started["result"]["run"]["run_id"]
            .as_str()
            .unwrap_or_else(|| panic!("the run started: {started}"))
            .to_string();

        app.workflow
            .record_node_token(&InstancePath::new("only"), NodeToken::new("node-token"));
        app.bind_workflow_node(
            &InstancePath::new("only"),
            NodeBinding {
                pane_id: crate::workflow::model::PublicPaneId::new(pane),
                terminal_id: crate::terminal::TerminalId::alloc(),
                agent_session_id: "session-1".to_string(),
                transcript_path: PathBuf::from("transcript.jsonl"),
                node_dir: PathBuf::from("/runs/test/only"),
                cwd: PathBuf::from("/repo"),
            },
        );
        (app, run_id)
    }

    /// E4: `workflow.node.interrupt` is a *delivery*. When the runtime refuses
    /// to write the keystroke — here because the bound pane does not exist —
    /// answering `workflow_node_interrupted` would tell the caller their
    /// interrupt landed on a process that never saw it.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn an_interrupt_that_was_not_delivered_is_not_reported_as_success() {
        let (mut app, run_id) = app_with_a_bound_command_node("w9:p9");

        let response = app.handle_workflow_node_interrupt(
            "req".into(),
            WorkflowNodeTarget {
                run_id: run_id.clone(),
                path: "only".into(),
            },
        );
        assert_eq!(error_code(&response), DELIVERY_FAILED_CODE, "{response}");
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        let message = value["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("pane.send_keys"),
            "the error names the delivery that failed: {message}"
        );

        // A steer is the same shape of delivery and must fail the same way.
        let response = app.handle_workflow_node_steer(
            "req".into(),
            WorkflowNodeSteerParams {
                run_id,
                path: "only".into(),
                text: "keep going".into(),
            },
        );
        assert_eq!(error_code(&response), DELIVERY_FAILED_CODE, "{response}");
    }

    /// D4: `kvx workflow node complete` reports `null` when it cannot read or
    /// parse `result.json`, and the server — not the client — is what turns
    /// that into `NeedsAttention`. A `runner = "command"` node has no other
    /// completion signal, so without this it stalls `Running` forever.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn a_report_with_no_result_reaches_needs_attention_through_the_server() {
        let (mut app, run_id) = app_with_a_bound_command_node("w9:p9");
        assert_eq!(
            app.workflow_node_info(&InstancePath::new("only"))
                .map(|node| node.status),
            Some(crate::api::schema::WorkflowNodeStatus::Running)
        );

        let response = app.handle_workflow_node_report(
            "req".into(),
            WorkflowNodeReportParams {
                run_id,
                path: "only".into(),
                token: "node-token".into(),
                result: serde_json::Value::Null,
            },
        );
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            value["result"]["type"], "workflow_node_reported",
            "the report is accepted by the server, not refused: {value}"
        );
        assert_eq!(
            value["result"]["node"]["status"], "needs_attention",
            "a report with no result artifact never completes a node: {value}"
        );
        assert!(
            value["result"]["node"]["evidence"].is_null(),
            "a node with no result artifact records no completion evidence: {value}"
        );
    }

    /// 2.12: a `runner = "command"` node wrote `{"wrong_field":123}` against a
    /// schema requiring `done`, and `kvx workflow node complete` printed a
    /// `workflow_node_reported` success envelope and exited 0 while the node sat
    /// `Running`. The response is the only correction channel a command node
    /// has, so it has to carry the refusal and the violations.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn a_schema_invalid_report_is_answered_with_the_rejection_not_a_success() {
        let (mut app, run_id) = app_with_a_bound_command_node("w9:p9");

        let response = app.handle_workflow_node_report(
            "req".into(),
            WorkflowNodeReportParams {
                run_id: run_id.clone(),
                path: "only".into(),
                token: "node-token".into(),
                result: serde_json::json!({ "wrong_field": 123 }),
            },
        );
        assert_eq!(error_code(&response), RESULT_INVALID_CODE, "{response}");
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        let message = value["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("output_schema")
                && message.contains("missing required field \"done\""),
            "the rejection quotes the schema violation the node has to fix: {message}"
        );
        assert!(
            message.contains("kvx workflow node complete"),
            "the rejection names the next move: {message}"
        );
        assert_eq!(
            app.workflow_node_info(&InstancePath::new("only"))
                .map(|node| node.status),
            Some(crate::api::schema::WorkflowNodeStatus::Running),
            "the first invalid result still earns the documented corrective re-prompt"
        );

        // Second strike: still refused, and now the node is surfaced rather than
        // left `Running` with nothing to wait for.
        let response = app.handle_workflow_node_report(
            "req".into(),
            WorkflowNodeReportParams {
                run_id,
                path: "only".into(),
                token: "node-token".into(),
                result: serde_json::json!({ "wrong_field": 456 }),
            },
        );
        assert_eq!(error_code(&response), RESULT_INVALID_CODE, "{response}");
        assert_eq!(
            app.workflow_node_info(&InstancePath::new("only"))
                .map(|node| node.status),
            Some(crate::api::schema::WorkflowNodeStatus::NeedsAttention),
            "a node never stays Running after its corrective re-prompt is spent"
        );
    }

    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn a_valid_report_is_still_answered_as_a_success() {
        let (mut app, run_id) = app_with_a_bound_command_node("w9:p9");
        let response = app.handle_workflow_node_report(
            "req".into(),
            WorkflowNodeReportParams {
                run_id,
                path: "only".into(),
                token: "node-token".into(),
                result: serde_json::json!({ "done": true }),
            },
        );
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            value["result"]["type"], "workflow_node_reported",
            "unexpected: {value}"
        );
        assert_eq!(value["result"]["node"]["status"], "succeeded");
    }

    /// 2.13: after `run cancel`, `workflow.node.restart` succeeded — the run
    /// reported `cancelled` while the node it restarted reported `running`, in a
    /// pane nothing would ever collect a result from.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn node_restart_is_refused_once_the_run_has_been_cancelled() {
        let (mut app, run_id) = app_with_a_bound_command_node("w9:p9");
        app.handle_workflow_run_cancel(
            "req".into(),
            WorkflowRunTarget {
                run_id: run_id.clone(),
            },
        );
        assert_eq!(
            app.workflow.run_status(),
            Some(RunStatus::Cancelled),
            "the fixture run is cancelled before the restart"
        );

        let response = app.handle_workflow_node_restart(
            "req".into(),
            WorkflowNodeTarget {
                run_id,
                path: "only".into(),
            },
        );
        assert_eq!(error_code(&response), RUN_CLOSED_CODE, "{response}");
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        let message = value["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("cancelled") && message.contains("kvx workflow run start"),
            "the refusal names the run's status and what to do instead: {message}"
        );
        assert_eq!(
            app.workflow_node_info(&InstancePath::new("only"))
                .map(|node| node.status),
            Some(crate::api::schema::WorkflowNodeStatus::Cancelled),
            "the node is not resurrected"
        );
    }

    /// B-T5 (P2b decision): `run cancel` on an already-closed run used to
    /// answer `ok` with an envelope literally named `workflow_run_cancelled`
    /// while the run's status stayed whatever it already was — inconsistent
    /// with `steer`/`interrupt`/`restart`, which all reject a closed run with
    /// `workflow_run_closed`.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn cancelling_a_closed_run_is_refused_like_steer_and_interrupt() {
        let (mut app, run_id) = app_with_a_bound_command_node("w9:p9");
        app.handle_workflow_run_cancel(
            "req".into(),
            WorkflowRunTarget {
                run_id: run_id.clone(),
            },
        );
        assert_eq!(
            app.workflow.run_status(),
            Some(RunStatus::Cancelled),
            "the fixture run is cancelled before the second cancel"
        );

        let response = app.handle_workflow_run_cancel(
            "req".into(),
            WorkflowRunTarget {
                run_id: run_id.clone(),
            },
        );
        assert_eq!(error_code(&response), RUN_CLOSED_CODE, "{response}");
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        let message = value["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("cancelled") && message.contains("kvx workflow run start"),
            "the refusal names the run's status and what to do instead: {message}"
        );
        assert_eq!(
            app.workflow.run_status(),
            Some(RunStatus::Cancelled),
            "the refusal does not reopen or otherwise mutate the run"
        );
    }

    /// 2.14: `workflow_run_in_flight` claimed the blocking run was "still
    /// executing" when it was in fact paused waiting for a human, and named
    /// neither the run nor a way out.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn run_in_flight_names_the_paused_run_the_blocking_node_and_the_remedies() {
        let (mut app, run_id) = app_with_a_bound_command_node("w9:p9");
        // Two invalid results spend the corrective re-prompt, surface the node,
        // and leave the run paused with nothing runnable.
        for _ in 0..2 {
            app.handle_workflow_node_report(
                "req".into(),
                WorkflowNodeReportParams {
                    run_id: run_id.clone(),
                    path: "only".into(),
                    token: "node-token".into(),
                    result: serde_json::json!({ "wrong_field": 1 }),
                },
            );
        }
        assert_eq!(app.workflow.run_status(), Some(RunStatus::Paused));

        app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: single_node_definition("second-workflow", "run it"),
            },
        );
        let response = app.handle_workflow_run(
            "req".into(),
            WorkflowRunParams {
                workflow_id: "second-workflow".into(),
                version: None,
                tier: None,
                args: HashMap::new(),
                restore_from: None,
                include_prior_summaries: None,
            },
        );

        assert_eq!(
            error_code(&response),
            "workflow_run_in_flight",
            "{response}"
        );
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        let message = value["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains(&run_id),
            "the refusal names the blocking run: {message}"
        );
        assert!(
            message.contains("paused") && message.contains("waiting for a human"),
            "the refusal says the run is not executing: {message}"
        );
        assert!(
            message.contains("blocked on node \"only\""),
            "the refusal names the node the run is stuck on: {message}"
        );
        assert!(
            message.contains(&format!("kvx workflow run cancel {run_id}"))
                && message.contains(&format!("kvx workflow node restart {run_id} only")),
            "the refusal names both ways out: {message}"
        );
    }

    // ── Phase 2 (WS-E) ─────────────────────────────────────────────────────

    /// A root that may fan out into one template, plus a downstream node the
    /// children inherit the parent's edge to — the shape §3.4's "the fan-in
    /// point is preserved" is about.
    #[cfg(feature = "workflow")]
    fn expanding_definition(max_nodes: Option<u16>) -> WorkflowDefinitionDocument {
        let growth = max_nodes.map_or(String::new(), |value| format!("max_nodes = {value}\n"));
        WorkflowDefinitionDocument {
            format: WorkflowDefinitionFormat::Toml,
            text: format!(
                r#"
name = "grower"
description = "v1 description"
{growth}
[[arg]]
name = "topic"
default = "widgets"

[[node]]
key = "plan"
label = "Plan"
runner = "command"
command = ["/bin/true"]
prompt_template = "plan {{{{topic}}}}"
output_schema = {{ type = "object" }}
expand_allow = ["worker"]
expand_max = 2

[[node]]
key = "worker"
label = "Worker"
runner = "command"
command = ["/bin/true"]
prompt_template = "work on {{{{topic}}}}"
output_schema = {{ type = "object" }}
is_template = true

[[node]]
key = "review"
label = "Review"
runner = "command"
command = ["/bin/true"]
prompt_template = "review {{{{summary}}}}"
output_schema = {{ type = "object" }}
# One attempt, so a dead pane is a failure rather than a retry — which is what
# lets a test reach `RunStatus::Failed` without a second spawn.
max_attempts = 1

[[edge]]
from = "plan"
to = "review"
kind = "data"
payload = "summary"
port = "summary"
"#
            ),
        }
    }

    /// Starts the expanding workflow and binds `plan`, so it is a live,
    /// token-holding node that can propose.
    #[cfg(feature = "workflow")]
    fn app_with_an_expanding_run(tier: Option<WorkflowTier>) -> (App, String) {
        use crate::workflow::model::{NodeBinding, NodeToken};
        use std::path::PathBuf;

        let mut app = app();
        let created = app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: expanding_definition(None),
            },
        );
        let created: serde_json::Value = serde_json::from_str(&created).unwrap();
        let workflow_id = created["result"]["workflow"]["workflow_id"]
            .as_str()
            .unwrap_or_else(|| panic!("the workflow was created: {created}"))
            .to_string();

        let started = app.handle_workflow_run(
            "req".into(),
            WorkflowRunParams {
                workflow_id,
                version: None,
                tier,
                args: HashMap::new(),
                restore_from: None,
                include_prior_summaries: None,
            },
        );
        let started: serde_json::Value = serde_json::from_str(&started).unwrap();
        let run_id = started["result"]["run"]["run_id"]
            .as_str()
            .unwrap_or_else(|| panic!("the run started: {started}"))
            .to_string();

        app.workflow
            .record_node_token(&InstancePath::new("plan"), NodeToken::new("node-token"));
        app.bind_workflow_node(
            &InstancePath::new("plan"),
            NodeBinding {
                pane_id: crate::workflow::model::PublicPaneId::new("w9:p9"),
                terminal_id: crate::terminal::TerminalId::alloc(),
                agent_session_id: "session-1".to_string(),
                transcript_path: PathBuf::from("transcript.jsonl"),
                node_dir: PathBuf::from("/runs/test/plan"),
                cwd: PathBuf::from("/repo"),
            },
        );
        (app, run_id)
    }

    /// Settles `plan` with a result that fills the `summary` port, which is
    /// what admits `review`.
    #[cfg(feature = "workflow")]
    fn report_plan(app: &mut App, run_id: &str) {
        app.handle_workflow_node_report(
            "req".into(),
            WorkflowNodeReportParams {
                run_id: run_id.to_string(),
                path: "plan".into(),
                token: "node-token".into(),
                result: serde_json::json!({ "summary": "done" }),
            },
        );
    }

    /// Binds the downstream node to its own pane, the step that moves an
    /// admitted node from `Ready` to `Running`.
    #[cfg(feature = "workflow")]
    fn bind_review(app: &mut App) {
        use crate::workflow::model::{NodeBinding, NodeToken};
        use std::path::PathBuf;

        app.workflow
            .record_node_token(&InstancePath::new("review"), NodeToken::new("node-token"));
        app.bind_workflow_node(
            &InstancePath::new("review"),
            NodeBinding {
                pane_id: crate::workflow::model::PublicPaneId::new("w9:p8"),
                terminal_id: crate::terminal::TerminalId::alloc(),
                agent_session_id: "session-2".to_string(),
                transcript_path: PathBuf::from("transcript.jsonl"),
                node_dir: PathBuf::from("/runs/test/review"),
                cwd: PathBuf::from("/repo"),
            },
        );
    }

    #[cfg(feature = "workflow")]
    fn expand_params(run_id: &str, template: &str, count: Option<u32>) -> WorkflowNodeExpandParams {
        WorkflowNodeExpandParams {
            run_id: run_id.to_string(),
            path: "plan".into(),
            token: "node-token".into(),
            template: template.to_string(),
            label: String::new(),
            inputs: HashMap::new(),
            count,
        }
    }

    /// An accepted proposal creates the children it names, at the §3 frozen
    /// instance-path grammar, and reports exactly those paths.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn an_accepted_expand_proposal_creates_the_children_it_reports() {
        let (mut app, run_id) = app_with_an_expanding_run(None);
        let response = app
            .handle_workflow_node_expand("req".into(), expand_params(&run_id, "worker", Some(2)));
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            value["result"]["type"], "workflow_node_expanded",
            "unexpected: {value}"
        );
        let accepted: Vec<&str> = value["result"]["accepted"]
            .as_array()
            .unwrap_or_else(|| panic!("accepted is an array: {value}"))
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert_eq!(accepted, vec!["plan/worker/1", "plan/worker/2"], "{value}");
        assert!(
            value["result"]["rejected"]
                .as_array()
                .is_some_and(Vec::is_empty),
            "a wholly accepted proposal reports no rejection: {value}"
        );
        for path in accepted {
            assert!(
                app.workflow_node_info(&InstancePath::new(path)).is_some(),
                "the response only names children that exist: {path}"
            );
        }
    }

    /// The headline guarantee, handler-side: a refused proposal is a
    /// **successful** response carrying the refusal, not an error — the run
    /// continues and the node learns exactly what it hit.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn a_rejected_expand_proposal_is_a_success_response_carrying_the_rejection() {
        let (mut app, run_id) = app_with_an_expanding_run(None);
        let before = app.workflow.graph().map(|graph| graph.nodes.len());

        let response =
            app.handle_workflow_node_expand("req".into(), expand_params(&run_id, "review", None));
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            value["result"]["type"], "workflow_node_expanded",
            "a rejection is not an error: {value}"
        );
        assert!(
            value["result"]["accepted"]
                .as_array()
                .is_some_and(Vec::is_empty),
            "{value}"
        );
        let rejected = value["result"]["rejected"].as_array().unwrap();
        assert_eq!(rejected.len(), 1, "{value}");
        assert_eq!(rejected[0]["reason"], "not_allowed");
        assert_eq!(rejected[0]["template"], "review");
        assert_eq!(rejected[0]["requested"], 1);
        assert_eq!(rejected[0]["accepted"], 0);
        assert!(
            rejected[0]["limit"].is_null(),
            "a validation refusal is not a guardrail: {value}"
        );
        assert!(
            rejected[0]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("expand_allow"),
            "{value}"
        );
        assert_eq!(
            app.workflow.graph().map(|graph| graph.nodes.len()),
            before,
            "a refused proposal creates nothing"
        );
    }

    /// §4 D2: partial acceptance is the interesting case. Four asked for, two
    /// created, and the shortfall reported with the exact guardrail — never
    /// accept-all and never a silent truncation.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn a_truncated_expand_proposal_reports_both_halves() {
        let (mut app, run_id) = app_with_an_expanding_run(None);
        let response = app
            .handle_workflow_node_expand("req".into(), expand_params(&run_id, "worker", Some(4)));
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(
            value["result"]["accepted"].as_array().map(Vec::len),
            Some(2),
            "expand_max 2 caps the acceptance: {value}"
        );
        let rejected = value["result"]["rejected"].as_array().unwrap();
        assert_eq!(rejected.len(), 1, "{value}");
        assert_eq!(rejected[0]["reason"], "truncated");
        assert_eq!(rejected[0]["requested"], 4);
        assert_eq!(rejected[0]["accepted"], 2);
        assert_eq!(rejected[0]["limit"]["kind"], "expand_max");
        assert_eq!(rejected[0]["limit"]["limit_value"], 2);
        assert_eq!(rejected[0]["limit"]["requested"], 4);
        assert_eq!(rejected[0]["limit"]["accepted"], 2);
        assert!(
            rejected[0]["limit"]["at_unix_ms"].as_u64().unwrap_or(0) > 0,
            "a growth limit is stamped when it is hit: {value}"
        );
        assert!(
            rejected[0]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("2 of 4"),
            "{value}"
        );
    }

    /// The token is the node's capability, exactly as it is for
    /// `workflow.node.report`: an operator holding the run id cannot grow
    /// someone else's graph.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn an_expand_proposal_with_the_wrong_token_is_refused() {
        let (mut app, run_id) = app_with_an_expanding_run(None);
        let mut params = expand_params(&run_id, "worker", Some(1));
        params.token = "not-the-token".into();
        let response = app.handle_workflow_node_expand("req".into(), params);
        assert_eq!(
            error_code(&response),
            "workflow_node_token_invalid",
            "{response}"
        );
        assert!(
            app.workflow_node_info(&InstancePath::new("plan/worker/1"))
                .is_none(),
            "an unauthenticated proposal creates nothing"
        );
    }

    /// `count` is `u32` on the wire and `u16` in the engine, so an
    /// out-of-range value is refused rather than truncated to a number the
    /// caller never asked for.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn an_out_of_range_expand_count_is_refused_rather_than_truncated() {
        let (mut app, run_id) = app_with_an_expanding_run(None);
        let response = app.handle_workflow_node_expand(
            "req".into(),
            expand_params(&run_id, "worker", Some(u32::from(u16::MAX) + 1)),
        );
        assert_eq!(error_code(&response), "invalid_params", "{response}");
        assert!(app
            .workflow_node_info(&InstancePath::new("plan/worker/1"))
            .is_none());
    }

    /// H2 — the closed-run guard is one helper applied to all four verbs.
    /// Three terminal statuses × three verbs, each naming the status it
    /// refused for.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn steer_interrupt_and_expand_are_refused_once_the_run_has_closed() {
        use crate::workflow::model::PublicPaneId;

        for (label, close) in [
            (
                "cancelled",
                Box::new(|app: &mut App, run_id: &str| {
                    app.handle_workflow_run_cancel(
                        "req".into(),
                        WorkflowRunTarget {
                            run_id: run_id.to_string(),
                        },
                    );
                }) as Box<dyn Fn(&mut App, &str)>,
            ),
            (
                "succeeded",
                Box::new(|app: &mut App, run_id: &str| {
                    // Every node reports a valid result, so the run reaches its
                    // own terminal status rather than being cancelled into one.
                    report_plan(app, run_id);
                    bind_review(app);
                    app.handle_workflow_node_report(
                        "req".into(),
                        WorkflowNodeReportParams {
                            run_id: run_id.to_string(),
                            path: "review".into(),
                            token: "node-token".into(),
                            result: serde_json::json!({ "summary": "reviewed" }),
                        },
                    );
                }),
            ),
            (
                "failed",
                Box::new(|app: &mut App, run_id: &str| {
                    // The last node's pane dies before a result arrives, which
                    // §4.3 makes a failure with the exit code; every node is
                    // then terminal and one of them failed.
                    report_plan(app, run_id);
                    bind_review(app);
                    app.apply_workflow_engine_input(EngineInput::PaneExited {
                        pane: PublicPaneId::new("w9:p8"),
                        code: Some(1),
                    });
                }),
            ),
        ] {
            let (mut app, run_id) = app_with_an_expanding_run(None);
            close(&mut app, &run_id);
            let status = app.workflow.run_status();
            let nodes: Vec<(String, NodeStatus, bool)> = app
                .workflow
                .graph()
                .map(|graph| {
                    graph
                        .nodes
                        .iter()
                        .map(|node| {
                            (
                                node.path.to_string(),
                                node.status,
                                node.succession.is_some(),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            assert!(
                status.is_some_and(is_closed_run),
                "the {label} fixture closes the run, got {status:?} with {nodes:?}"
            );

            let responses = [
                (
                    "steer",
                    app.handle_workflow_node_steer(
                        "req".into(),
                        WorkflowNodeSteerParams {
                            run_id: run_id.clone(),
                            path: "plan".into(),
                            text: "keep going".into(),
                        },
                    ),
                ),
                (
                    "interrupt",
                    app.handle_workflow_node_interrupt(
                        "req".into(),
                        WorkflowNodeTarget {
                            run_id: run_id.clone(),
                            path: "plan".into(),
                        },
                    ),
                ),
                (
                    "restart",
                    app.handle_workflow_node_restart(
                        "req".into(),
                        WorkflowNodeTarget {
                            run_id: run_id.clone(),
                            path: "plan".into(),
                        },
                    ),
                ),
                (
                    "expand",
                    app.handle_workflow_node_expand(
                        "req".into(),
                        expand_params(&run_id, "worker", Some(1)),
                    ),
                ),
            ];

            for (verb, response) in responses {
                assert_eq!(
                    error_code(&response),
                    RUN_CLOSED_CODE,
                    "{verb} on a {label} run: {response}"
                );
                let value: serde_json::Value = serde_json::from_str(&response).unwrap();
                let message = value["error"]["message"].as_str().unwrap_or_default();
                assert!(
                    message.contains(label) && message.contains("kvx workflow run start"),
                    "{verb} on a {label} run names the status and the way out: {message}"
                );
            }
            assert!(
                app.workflow_node_info(&InstancePath::new("plan/worker/1"))
                    .is_none(),
                "a closed run cannot grow ({label})"
            );
        }
    }

    /// §4 D4 / R-3: the tier narrows the version's ceilings **once**, at run
    /// create, so what the `RunGraph` enforces is what the run row persists.
    /// Without the narrowing a `--tier low` run's row says 30 while its graph
    /// says 12.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn the_persisted_growth_limits_are_the_ones_the_run_graph_enforces() {
        for (tier, expected) in [
            (WorkflowTier::Max, 30_u16),
            (WorkflowTier::High, 30),
            (WorkflowTier::Auto, 30),
            (WorkflowTier::Medium, 24),
            (WorkflowTier::Low, 12),
        ] {
            let mut app = app();
            let created = app.handle_workflow_create(
                "req".into(),
                WorkflowCreateParams {
                    definition: expanding_definition(Some(30)),
                },
            );
            let created: serde_json::Value = serde_json::from_str(&created).unwrap();
            let workflow_id = created["result"]["workflow"]["workflow_id"]
                .as_str()
                .unwrap_or_else(|| panic!("{created}"))
                .to_string();
            let started = app.handle_workflow_run(
                "req".into(),
                WorkflowRunParams {
                    workflow_id,
                    version: None,
                    tier: Some(tier),
                    args: HashMap::new(),
                    restore_from: None,
                    include_prior_summaries: None,
                },
            );
            let started: serde_json::Value = serde_json::from_str(&started).unwrap();
            let run_id = RunId::new(
                started["result"]["run"]["run_id"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{started}"))
                    .to_string(),
            );

            let enforced = app
                .workflow
                .graph()
                .map(|graph| graph.growth.max_nodes)
                .expect("the run graph is live");
            assert_eq!(enforced, expected, "{tier:?} narrows the graph's ceiling");
            assert_eq!(
                started["result"]["run"]["max_nodes"].as_u64(),
                Some(u64::from(expected)),
                "{tier:?}: the projection reports the effective ceiling: {started}"
            );

            let wanted = run_id.clone();
            let persisted = app
                .workflow_store
                .call(move |cx| cx.block_on(cx.store().get_run(&wanted)))
                .expect("the in-memory store is available")
                .expect("the run row reads back")
                .expect("the run row exists");
            assert_eq!(
                persisted.max_nodes, enforced,
                "{tier:?}: the run row and the run graph must not disagree"
            );
            assert_eq!(persisted.max_depth, 3, "{tier:?}: depth is never narrowed");
        }
    }

    /// H3 / §4 D16: one projection. `detail` is what both the human renderer
    /// and `--json` read, so it has to describe the head document's whole
    /// node/edge/arg set beside the summary and the version chain.
    #[cfg(feature = "workflow")]
    #[test]
    fn workflow_get_returns_one_detail_projection_of_the_head_document() {
        let mut app = app();
        app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: expanding_definition(None),
            },
        );
        let shown = app.handle_workflow_get(
            "req".into(),
            WorkflowTarget {
                workflow_id: "grower".into(),
            },
        );
        let shown: serde_json::Value = serde_json::from_str(&shown).unwrap();
        let detail = &shown["result"]["detail"];
        assert!(!detail.is_null(), "workflow.get carries a detail: {shown}");
        assert_eq!(
            detail["workflow"], shown["result"]["workflow"],
            "the detail's summary is the response's own: {shown}"
        );
        assert_eq!(
            detail["versions"], shown["result"]["versions"],
            "the detail's chain is the response's own: {shown}"
        );
        let nodes = detail["nodes"]
            .as_array()
            .unwrap_or_else(|| panic!("{shown}"));
        let mut keys: Vec<&str> = nodes
            .iter()
            .filter_map(|node| node["node_key"].as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["plan", "review", "worker"],
            "every node of the head document, templates included: {shown}"
        );
        assert_eq!(
            detail["edges"].as_array().map(Vec::len),
            Some(1),
            "the head document's edges: {shown}"
        );
        assert_eq!(
            detail["args"][0]["name"], "topic",
            "the head document's args: {shown}"
        );
        let worker = nodes
            .iter()
            .find(|node| node["node_key"] == "worker")
            .unwrap_or_else(|| panic!("{shown}"));
        assert_eq!(
            worker["is_template"], true,
            "a template is described as one: {shown}"
        );
        assert_eq!(worker["expand_allow"].as_array().map(Vec::len), Some(0));
    }

    /// H5: `create_version` writes the new document's metadata onto the
    /// `workflow` row, so `workflow.get` cannot report v1's description beside
    /// `head_version: 2`.
    #[cfg(feature = "workflow")]
    #[test]
    fn an_update_that_changes_the_description_is_what_workflow_get_reports() {
        let mut app = app();
        app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: expanding_definition(None),
            },
        );
        let mut updated_definition = expanding_definition(None);
        updated_definition.text = updated_definition
            .text
            .replace(
                "description = \"v1 description\"",
                "description = \"v2 description\"\ndefault_tier = \"low\"",
            )
            // The graph has to move too, or the store recognises the revision
            // as a no-op and the head stays at v1 — which is the very case that
            // proves the metadata refresh is not riding on a new version row.
            .replace("review {{summary}}", "review it: {{summary}}");
        let updated = app.handle_workflow_version_create(
            "req".into(),
            WorkflowVersionCreateParams {
                workflow_id: "grower".into(),
                definition: updated_definition,
                change_summary: "reworded".into(),
            },
        );
        let updated: serde_json::Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(
            updated["result"]["type"], "workflow_version_created",
            "unexpected: {updated}"
        );

        let shown = app.handle_workflow_get(
            "req".into(),
            WorkflowTarget {
                workflow_id: "grower".into(),
            },
        );
        let shown: serde_json::Value = serde_json::from_str(&shown).unwrap();
        assert_eq!(shown["result"]["workflow"]["head_version"], 2, "{shown}");
        assert_eq!(
            shown["result"]["workflow"]["description"], "v2 description",
            "the head's description, not v1's: {shown}"
        );
        assert_eq!(
            shown["result"]["workflow"]["default_tier"], "low",
            "the head's tier, not v1's: {shown}"
        );
        assert_eq!(
            shown["result"]["detail"]["workflow"]["description"], "v2 description",
            "the one projection reports the same thing: {shown}"
        );
    }

    /// B-T4 (P2a/P2b): the durable projection hardcoded `parent_path: None`
    /// and `growth_limited: None` regardless of what `stored_run` read back,
    /// even though both facts are recoverable from already-persisted data
    /// (`run_node.parent` and the `growth_limited` journal, B1/B2).
    #[cfg(feature = "workflow")]
    #[test]
    fn the_durable_projection_reports_parent_path_and_growth_limited() {
        use crate::workflow::model::Demand;
        use crate::workflow::store::{RunNodeRecord, RunRecord, StoredGrowthLimit};

        let node = RunNodeRecord {
            run: RunId::new("workflow_run:t4"),
            node_key: NodeKey::new("worker"),
            instance_path: InstancePath::new("root/worker/1"),
            label: "worker".into(),
            inputs: BTreeMap::new(),
            parent_path: Some(InstancePath::new("root")),
            depth: 1,
            status: NodeStatus::Succeeded,
            // Phase 3 (M2/§4 D4): both are read back by the production reader
            // now, so the projection has to carry them.
            transcript_path: Some("/home/u/.claude/projects/-repo/s1.jsonl".into()),
            restored_from: None,
            model: "sonnet".into(),
            effort: "high".into(),
            demand: Demand::Standard,
            attempt: 1,
            assignment_reason: String::new(),
            pane_id: None,
            terminal_id: None,
            agent_session_id: None,
            cwd: None,
            node_dir: None,
            evidence: None,
            succession: None,
            total_tokens: 0,
            tool_uses: 0,
            duration_ms: 0,
            started_at_unix_ms: None,
            ended_at_unix_ms: None,
            task_id: None,
            subject: String::new(),
            owner: String::new(),
            emergent: false,
        };

        let limit = StoredGrowthLimit {
            kind: "max_nodes".into(),
            limit_value: 3,
            requested: 4,
            accepted: 1,
            at_unix_ms: 1_700_000_000_000,
            message: "max_nodes 3 reached; 1 of 4 requested nodes created".into(),
        };
        let mut limits = StoredGrowthLimits::default();
        limits
            .by_path
            .insert(InstancePath::new("root/worker/1"), limit.clone());
        limits.last = Some(limit);

        let wired_node = wire_run_node_record(node, &limits);
        assert_eq!(
            wired_node.parent_path,
            Some("root".to_string()),
            "spawn provenance survives the durable projection"
        );
        let node_limit = wired_node
            .growth_limited
            .expect("this node's own journalled growth limit survives the durable projection");
        assert_eq!(node_limit.kind, WorkflowGrowthLimitKind::MaxNodes);
        assert_eq!(node_limit.limit_value, 3);
        assert_eq!(node_limit.requested, 4);
        assert_eq!(node_limit.accepted, 1);

        let run = RunRecord {
            id: RunId::new("workflow_run:t4"),
            workflow: WorkflowId::new("workflow:t4"),
            workflow_name: "t4".into(),
            version: KvdagVersionId::new("kvdag_version:t4"),
            tier: Tier::Auto,
            status: RunStatus::Succeeded,
            args: BTreeMap::new(),
            context_runs: Vec::new(),
            restore_from_run: None,
            max_depth: 3,
            max_nodes: 12,
            workspace_id: None,
            tab_id: None,
            nodes_total: 2,
            nodes_done: 2,
            total_tokens: 0,
            total_tool_uses: 0,
            started_at_unix_ms: 1_700_000_000_000,
            ended_at_unix_ms: Some(1_700_000_001_000),
            failure: None,
            lead_session_id: None,
            team_name: None,
            lead_pane_id: None,
            lead_terminal_id: None,
            lead_prompt_version: None,
        };
        let wired_run = wire_run_record(run, &limits);
        let run_limit = wired_run.growth_limited.expect(
            "the run's most recent journalled growth limit survives the durable projection",
        );
        assert_eq!(run_limit.kind, WorkflowGrowthLimitKind::MaxNodes);
    }

    // ── Phase 3: interrogation, restore, summaries ─────────────────────────

    #[cfg(feature = "workflow")]
    fn pane_count(app: &App) -> usize {
        app.state
            .workspaces
            .iter()
            .map(|workspace| {
                workspace
                    .tabs
                    .iter()
                    .map(|tab| tab.layout.pane_count())
                    .sum::<usize>()
            })
            .sum()
    }

    /// The agent-runner twin of [`app_with_a_bound_command_node`].
    ///
    /// The interrogation ladder gates on the node's **runner**, so the command
    /// fixture can only ever exercise the first refusal; everything past it
    /// needs a node the definition declares as an agent.
    #[cfg(feature = "workflow")]
    fn app_with_a_bound_agent_node(pane: &str, cwd: PathBuf, transcript: PathBuf) -> (App, String) {
        let mut app = app();
        let created = app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: WorkflowDefinitionDocument {
                    format: WorkflowDefinitionFormat::Toml,
                    text: r#"
name = "interrogable"
[[node]]
key = "only"
label = "Only"
runner = "agent"
prompt_template = "do it"
output_schema = { type = "object", required = ["done"] }
"#
                    .to_string(),
                },
            },
        );
        let created: serde_json::Value = serde_json::from_str(&created).unwrap();
        let workflow_id = created["result"]["workflow"]["workflow_id"]
            .as_str()
            .expect("the workflow was created")
            .to_string();

        let started = app.handle_workflow_run(
            "req".into(),
            WorkflowRunParams {
                workflow_id,
                version: None,
                tier: None,
                args: HashMap::new(),
                restore_from: None,
                include_prior_summaries: None,
            },
        );
        let started: serde_json::Value = serde_json::from_str(&started).unwrap();
        let run_id = started["result"]["run"]["run_id"]
            .as_str()
            .unwrap_or_else(|| panic!("the run started: {started}"))
            .to_string();

        app.bind_workflow_node(
            &InstancePath::new("only"),
            crate::workflow::model::NodeBinding {
                pane_id: crate::workflow::model::PublicPaneId::new(pane),
                terminal_id: crate::terminal::TerminalId::alloc(),
                agent_session_id: "00000000-0000-4000-8000-000000000001".to_string(),
                transcript_path: transcript,
                node_dir: PathBuf::from("/runs/test/only"),
                cwd,
            },
        );
        (app, run_id)
    }

    #[cfg(feature = "workflow")]
    fn interrogate(app: &mut App, run_id: &str, path: &str, reconstructed: bool) -> String {
        app.handle_workflow_node_interrogate(
            "req".into(),
            WorkflowNodeInterrogateParams {
                run_id: run_id.to_string(),
                path: path.to_string(),
                mode: if reconstructed {
                    WorkflowInterrogationMode::Reconstructed
                } else {
                    WorkflowInterrogationMode::Resumed
                },
                note: None,
            },
        )
    }

    /// The "never a silent pane" pin (`00-overview.md` Feature 3,
    /// `07-phase3-plan.md` §WS-D "Tested").
    ///
    /// A `runner: command` node never had a Claude session, so its transcript
    /// can never exist. The refusal has to be structured *and* has to happen
    /// before anything is created — a pane that starts and dies would be the
    /// exact failure the stat-first rule exists to prevent, so the pane count
    /// is asserted, not just the error code.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn interrogating_a_command_node_is_refused_and_creates_no_pane() {
        let (mut app, run_id) = app_with_a_bound_command_node("w9:p9");
        let before = pane_count(&app);

        let response = interrogate(&mut app, &run_id, "only", false);

        assert_eq!(error_code(&response), "workflow_transcript_unavailable");
        let body: serde_json::Value = serde_json::from_str(&response).unwrap();
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("command"),
            "the message has to say *why* there is no transcript, or the caller \
             goes looking for a file that was never going to exist: {message}"
        );
        assert_eq!(
            pane_count(&app),
            before,
            "a refused interrogation must leave the workspace exactly as it was"
        );
    }

    /// A node that *does* have a session but whose transcript is not on disk
    /// takes the same code with a different reason, and still creates nothing.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn interrogating_a_node_whose_transcript_is_gone_names_the_missing_file() {
        let (mut app, run_id) = app_with_a_bound_agent_node(
            "w9:p9",
            std::env::temp_dir(),
            PathBuf::from("/nonexistent/transcript.jsonl"),
        );
        let before = pane_count(&app);

        let response = interrogate(&mut app, &run_id, "only", false);

        assert_eq!(error_code(&response), "workflow_transcript_unavailable");
        let body: serde_json::Value = serde_json::from_str(&response).unwrap();
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("/nonexistent/transcript.jsonl"),
            "the refusal names the path it stat'd, so the caller can check it: {message}"
        );
        assert_eq!(pane_count(&app), before);
    }

    /// A reconstruction with nothing to reconstruct from is refused too: the
    /// degraded path is a stand-in for a stored result, and with no checkpoint
    /// there is nothing to stand in for.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn a_reconstruction_with_no_checkpoint_is_refused() {
        let (mut app, run_id) = app_with_a_bound_agent_node(
            "w9:p9",
            std::env::temp_dir(),
            PathBuf::from("/nonexistent/transcript.jsonl"),
        );
        let before = pane_count(&app);

        let response = interrogate(&mut app, &run_id, "only", true);

        assert_eq!(error_code(&response), "workflow_transcript_unavailable");
        assert_eq!(pane_count(&app), before);
    }

    /// §WS-D "Tested": with the transcript actually on disk the ladder gets all
    /// the way through, and the two things CI can check about a real fork are
    /// checked — the argv is exactly the frozen six tokens, and the source
    /// transcript's bytes are untouched by the call.
    ///
    /// The spawn itself fails here (a headless `AppState` has no pane to split),
    /// which is the point: the failure is *after* every precondition passed, so
    /// what is asserted is the argv the binder built and the file it did not
    /// write to. The real-fork non-mutation check is in the manual list (§5).
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn a_resumable_node_builds_the_frozen_fork_argv_and_never_writes_the_source() {
        use crate::workflow::binding::interrogate;

        let dir = std::env::temp_dir().join(format!(
            "karvex-interrogate-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).expect("the fixture directory");
        let transcript = dir.join("source.jsonl");
        let contents = b"{\"type\":\"user\"}\n";
        std::fs::write(&transcript, contents).expect("the fixture transcript");

        let (mut app, run_id) =
            app_with_a_bound_agent_node("w9:p9", dir.clone(), transcript.clone());
        let before = pane_count(&app);

        let response = interrogate(&mut app, &run_id, "only", false);

        // Every precondition passed, so the failure is the pane and it says so
        // (E-15) — not "there was nothing to fork", which would send the caller
        // to check a file that is sitting right there.
        assert_eq!(
            error_code(&response),
            INTERROGATION_SPAWN_FAILED_CODE,
            "the transcript and cwd both stat; the ladder must be past them: {response}"
        );
        assert_eq!(
            std::fs::read(&transcript).expect("the source transcript survives"),
            contents,
            "`--fork-session` is what makes this non-mutating; nothing on the \
             interrogate path may write the source transcript"
        );
        assert_eq!(
            pane_count(&app),
            before,
            "this fixture has no pane to split, so the spawn fails and leaves nothing"
        );

        // The frozen argv itself (§3 rule 7), asserted at the binder rather than
        // through a pane CI cannot create.
        let argv = interrogate::resumed_argv("source-sid", "forked-sid");
        assert_eq!(
            argv[1..],
            [
                "--session-id".to_string(),
                "forked-sid".to_string(),
                "--resume".to_string(),
                "source-sid".to_string(),
                "--fork-session".to_string(),
            ]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unknown run is a plain not-found, and — the m9 half — is answered
    /// before any node lookup, since a run that does not exist has no nodes.
    #[cfg(feature = "workflow")]
    #[test]
    fn interrogating_an_unknown_run_is_not_found() {
        let mut app = app();
        let response = interrogate(&mut app, "workflow_run:missing", "plan", false);
        assert_eq!(error_code(&response), "workflow_not_found");
    }

    /// `workflow.summary.get` on a run with no summary is a **success** with
    /// `summary: null`, never an error (§4 D1): a run whose epilogue was
    /// disabled, cancelled, or gave up simply has none.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn a_run_with_no_summary_answers_null_rather_than_an_error() {
        let (mut app, run_id) = app_with_a_bound_command_node("w9:p9");
        let response = app.handle_workflow_summary_get(
            "req".into(),
            WorkflowRunTarget {
                run_id: run_id.clone(),
            },
        );
        let body: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert!(
            body["error"].is_null(),
            "an absent summary is a normal answer: {body}"
        );
        assert!(
            body["result"]["summary"].is_null(),
            "and it is spelled `null`, not an empty object: {body}"
        );
    }

    /// `workflow.summary.list` with no `workflow_id` lists across every
    /// workflow (§4 D9) rather than rejecting the absent selector.
    #[cfg(feature = "workflow")]
    #[test]
    fn summary_list_without_a_workflow_id_is_a_cross_workflow_read() {
        let mut app = app();
        let response = app.handle_workflow_summary_list(
            "req".into(),
            WorkflowSummaryListParams {
                workflow_id: None,
                limit: None,
            },
        );
        let body: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert!(body["error"].is_null(), "{body}");
        assert_eq!(
            body["result"]["summaries"].as_array().map(Vec::len),
            Some(0),
            "an empty database lists nothing, and that is not an error: {body}"
        );
    }

    /// The same back-compat pin one layer down from WS-C's serde test:
    /// `workflow.run.list` with no `workflow_id` must not be rejected as a
    /// missing parameter, which is what the pre-Phase-3 handler did.
    #[cfg(feature = "workflow")]
    #[test]
    fn run_list_without_a_workflow_id_lists_across_workflows() {
        let mut app = app();
        let response = app.handle_workflow_run_list(
            "req".into(),
            WorkflowRunListParams {
                workflow_id: None,
                limit: None,
            },
        );
        let body: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert!(
            body["error"].is_null(),
            "an absent workflow_id is `all workflows`, not invalid params: {body}"
        );
    }

    /// §4 D11's typo protection: a selector naming no node of the **target**
    /// version is a hard error, and — the half that matters — the run is not
    /// created, so a refused restore leaves no orphan `workflow_run` row.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn an_unknown_restore_selector_is_refused_and_starts_no_run() {
        let (mut app, run_id) = app_with_a_bound_command_node("w9:p9");
        // Close the first run so the in-flight guard is not what refuses this.
        app.cancel_workflow_run();
        let workflow_id = {
            let listed = app.handle_workflow_list("req".into());
            let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
            listed["result"]["workflows"][0]["workflow_id"]
                .as_str()
                .expect("the workflow exists")
                .to_string()
        };
        let runs_before = run_row_count(&mut app, &workflow_id);

        let response = app.handle_workflow_run(
            "req".into(),
            WorkflowRunParams {
                workflow_id: workflow_id.clone(),
                version: None,
                tier: None,
                args: HashMap::new(),
                restore_from: Some(crate::api::schema::WorkflowRestoreRequest {
                    run_id: run_id.clone(),
                    nodes: vec!["typo".into()],
                    allow_changed: false,
                }),
                include_prior_summaries: None,
            },
        );

        assert_eq!(
            error_code(&response),
            "workflow_restore_unknown_selector",
            "{response}"
        );
        assert_eq!(
            run_row_count(&mut app, &workflow_id),
            runs_before,
            "a hard restore error must be raised before create_run, or it leaves \
             an orphan run row no engine will ever advance"
        );
    }

    /// Restoring from a run that has been pruned answers `workflow_run_pruned`
    /// — its checkpoints went with the run, and the caller is told which of
    /// "gone" and "never existed" happened.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn restoring_from_an_unknown_run_is_not_found_not_pruned() {
        let (mut app, _) = app_with_a_bound_command_node("w9:p9");
        app.cancel_workflow_run();
        let workflow_id = {
            let listed = app.handle_workflow_list("req".into());
            let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
            listed["result"]["workflows"][0]["workflow_id"]
                .as_str()
                .expect("the workflow exists")
                .to_string()
        };

        let response = app.handle_workflow_run(
            "req".into(),
            WorkflowRunParams {
                workflow_id,
                version: None,
                tier: None,
                args: HashMap::new(),
                restore_from: Some(crate::api::schema::WorkflowRestoreRequest {
                    run_id: "workflow_run:nosuchrun".into(),
                    nodes: Vec::new(),
                    allow_changed: false,
                }),
                include_prior_summaries: None,
            },
        );

        assert_eq!(
            error_code(&response),
            "workflow_not_found",
            "a run with neither a row nor a summary never existed: {response}"
        );
    }

    /// How many `workflow_run` rows a workflow has, read through the
    /// production listing path.
    #[cfg(feature = "workflow")]
    fn run_row_count(app: &mut App, workflow_id: &str) -> usize {
        let listed = app.handle_workflow_run_list(
            "req".into(),
            WorkflowRunListParams {
                workflow_id: Some(workflow_id.to_string()),
                limit: None,
            },
        );
        let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
        listed["result"]["runs"]
            .as_array()
            .map(Vec::len)
            .unwrap_or_default()
    }

    /// Every `workflow_*` error code the crate can hand back over the wire —
    /// the cross-phase inventory this exists to build. Error codes are
    /// contract: scripts match on them, the CLI renders them, and the docs
    /// list them. But until now they lived as scattered string constants and
    /// match-arm literals across six files with no naming guard and no
    /// collision check anywhere — the naming guard in
    /// `src/api/schema/workflows.rs` has only ever covered method names,
    /// event names, enum variants, and struct fields, never error codes. Two
    /// "absent check" incidents already came out of that gap: Phase 3 nearly
    /// shipped `workflow_interrogation_spawn_failed` reusing a node-spawn
    /// code (E-15, formerly pinned locally in
    /// `the_interrogation_spawn_code_follows_the_family_convention`, now
    /// subsumed by the general check below), and a keybind collision of the
    /// same shape (E-7) shipped and had to be fixed later.
    ///
    /// Values are pulled from the handlers themselves — the `_CODE` constants
    /// and the `SpawnError`/`ReportRejected`/`WorkflowStartError` `code()`
    /// methods — never hand-copied, so a changed constant changes this list
    /// for free. Only the *domain* label per entry is hand-maintained, and
    /// `every_workflow_error_code_literal_in_source_is_inventoried` below
    /// greps the source tree so a brand-new code (or constant) left off this
    /// list fails loudly instead of silently going unchecked the way all of
    /// these did before.
    #[cfg(feature = "workflow")]
    fn all_workflow_error_codes() -> Vec<(&'static str, &'static str)> {
        use crate::app::workflow::WorkflowStartError;
        use crate::workflow::binding::spawn::SpawnError;
        use crate::workflow::store::error::{
            WORKFLOW_INVALID_DEFINITION_CODE, WORKFLOW_NAME_TAKEN_CODE, WORKFLOW_STORE_ERROR_CODE,
            WORKFLOW_UNAVAILABLE_CODE,
        };

        vec![
            // `Definition::check` and `Kvdag::try_new` are two validators for
            // the same "the document you authored did not validate" fact
            // (`src/workflow/store/error.rs`'s own doc comment), so they
            // deliberately share both a domain label and a code.
            ("definition_invalid", INVALID_DEFINITION_CODE),
            ("definition_invalid", WORKFLOW_INVALID_DEFINITION_CODE),
            ("subsystem_unavailable", WORKFLOW_UNAVAILABLE_CODE),
            ("not_found", NOT_FOUND_CODE),
            // A create colliding with an existing workflow's name. Its own
            // domain: the document validated fine (so not
            // `definition_invalid`), the store did not misbehave (so not
            // `store_error`), and no selector was ambiguous (so not
            // `target_ambiguous`).
            ("name_taken", WORKFLOW_NAME_TAKEN_CODE),
            ("target_ambiguous", AMBIGUOUS_NAME_CODE),
            ("run_not_active", NO_ACTIVE_RUN_CODE),
            ("missing_arg", MISSING_ARG_CODE),
            // §3.3: the lead's end-of-run report was malformed.
            ("invalid_argument", INVALID_ARGUMENT_CODE),
            // §3.1: the two halves of a refused lead launch — the machine's
            // `claude` cannot run agent teams, or the pane could not be made.
            (
                "lead_spawn_failed",
                crate::workflow::binding::lead::LEAD_SPAWN_FAILED_CODE,
            ),
            (
                "lead_unavailable",
                crate::workflow::binding::lead::LEAD_UNAVAILABLE_CODE,
            ),
            ("node_not_running", NODE_NOT_RUNNING_CODE),
            ("node_delivery_failed", DELIVERY_FAILED_CODE),
            ("node_result_invalid", RESULT_INVALID_CODE),
            ("run_closed", RUN_CLOSED_CODE),
            ("transcript_unavailable", TRANSCRIPT_UNAVAILABLE_CODE),
            ("run_pruned", RUN_PRUNED_CODE),
            ("restore_unknown_selector", RESTORE_UNKNOWN_SELECTOR_CODE),
            ("interrogation_active", INTERROGATION_ACTIVE_CODE),
            (
                "interrogation_spawn_failed",
                INTERROGATION_SPAWN_FAILED_CODE,
            ),
            ("store_error", WORKFLOW_STORE_ERROR_CODE),
            (
                "node_report_unknown_node",
                ReportRejected::UnknownNode.code(),
            ),
            (
                "node_report_invalid_token",
                ReportRejected::InvalidToken.code(),
            ),
            (
                "node_report_missing_result",
                ReportRejected::MissingResult.code(),
            ),
            (
                "node_spawn_missing_command",
                SpawnError::MissingCommand(InstancePath::new(String::new())).code(),
            ),
            (
                "node_spawn_invalid_argument",
                SpawnError::InvalidArgument(String::new()).code(),
            ),
            (
                "node_spawn_target_pane_not_found",
                SpawnError::TargetPaneNotFound.code(),
            ),
            (
                "node_spawn_failed",
                SpawnError::PaneLaunchFailed(String::new()).code(),
            ),
            ("run_in_flight", WorkflowStartError::RunInFlight.code()),
        ]
    }

    /// The contract check: every code is well-formed, and no two *distinct*
    /// failure domains share a code. This is the general form of E-15 (an
    /// interrogation-pane spawn failure is not a run-node spawn failure) and
    /// would have caught it, or the E-7 keybind collision's "absent check"
    /// shape in general, before either shipped.
    #[cfg(feature = "workflow")]
    #[test]
    fn all_workflow_error_codes_are_a_well_formed_disjoint_family() {
        use crate::api::schema::workflows::tests::BANNED_UI_SURFACE_WORDS;

        let codes = all_workflow_error_codes();
        assert_eq!(
            codes.len(),
            29,
            "the inventory grew or shrank; update this count alongside the list itself"
        );

        for (domain, code) in &codes {
            assert!(code.starts_with("workflow_"), "{domain}: {code}");
            assert_eq!(
                *code,
                code.to_ascii_lowercase(),
                "{domain}: snake_case only: {code}"
            );
            assert!(
                !code.contains(' ') && !code.contains('-'),
                "{domain}: snake_case only: {code}"
            );
            for word in code.split('_') {
                assert!(
                    !BANNED_UI_SURFACE_WORDS.contains(&word),
                    "{domain}: {code} uses a UI-surface word"
                );
            }
        }

        let mut domain_by_code: std::collections::BTreeMap<&str, &str> =
            std::collections::BTreeMap::new();
        for (domain, code) in &codes {
            if let Some(existing) = domain_by_code.insert(code, domain) {
                assert_eq!(
                    existing, *domain,
                    "{code} is claimed by two different failure domains \
                     ({existing:?} and {domain:?}); either they are the same failure \
                     (give them the same domain label above and say why) or one of \
                     them needs its own code"
                );
            }
        }

        // `workflow_history.rs` cannot import `TRANSCRIPT_UNAVAILABLE_CODE`
        // (it is `#[cfg(feature = "workflow")]`-gated here, but that module's
        // `interrogate_outcome` compiles unconditionally) and so carries its
        // own copy of the literal instead. That copy is not in
        // `all_workflow_error_codes()` — it is the same value under a
        // different name, not a second domain — so it is pinned here
        // directly, keeping the one duplicate this file can't eliminate from
        // silently drifting the way a duplicated literal already has before
        // in this phase.
        assert_eq!(
            TRANSCRIPT_UNAVAILABLE_CODE,
            crate::app::workflow_history::TRANSCRIPT_UNAVAILABLE_CODE
        );
    }

    /// [`all_workflow_error_codes`] is hand-maintained — the values come from
    /// the handlers, but a *new* code (or a whole new file defining one)
    /// still has to be added to that list by a human. This closes that gap by
    /// grepping the tree for the two literal shapes every existing code is
    /// written in — a `..._CODE: &str = "workflow_..."` constant, or a
    /// `Self::Variant => "workflow_...",` match arm — and failing if either
    /// side has something the other does not.
    ///
    /// Deliberately not a blanket `"workflow_` search: that also matches wire
    /// type tags (`workflow_get`), event names (`workflow_run_started`),
    /// field names (`workflow_id`), and the `workflow_run` table name, none
    /// of which are error codes, and hand-allowlisting every one of those
    /// would be its own unbounded maintenance burden with no payoff.
    #[cfg(feature = "workflow")]
    #[test]
    fn every_workflow_error_code_literal_in_source_is_inventoried() {
        use std::collections::BTreeSet;
        use std::path::{Path, PathBuf};

        fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    rust_files(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }

        let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src_dir, &mut files);
        assert!(
            !files.is_empty(),
            "sanity: found no source files under {src_dir:?}"
        );

        let const_re = regex::Regex::new(r#"_CODE:\s*&str\s*=\s*"(workflow_[a-z_]+)""#).unwrap();
        let match_arm_re = regex::Regex::new(r#"=>\s*"(workflow_[a-z_]+)""#).unwrap();

        let mut found: BTreeSet<String> = BTreeSet::new();
        for file in &files {
            let Ok(text) = std::fs::read_to_string(file) else {
                continue;
            };
            for re in [&const_re, &match_arm_re] {
                for capture in re.captures_iter(&text) {
                    found.insert(capture[1].to_string());
                }
            }
        }

        let known: BTreeSet<String> = all_workflow_error_codes()
            .into_iter()
            .map(|(_, code)| code.to_string())
            .collect();

        let missing_from_inventory: Vec<&String> = found.difference(&known).collect();
        assert!(
            missing_from_inventory.is_empty(),
            "source defines workflow error code(s) not in all_workflow_error_codes(): \
             {missing_from_inventory:?}"
        );
        let stale_in_inventory: Vec<&String> = known.difference(&found).collect();
        assert!(
            stale_in_inventory.is_empty(),
            "all_workflow_error_codes() lists code(s) no longer defined anywhere in source: \
             {stale_in_inventory:?}"
        );
    }

    /// §4 D19: the store's over-budget stub is not data, and restoring it
    /// would hand a downstream node a lie labelled as data. Recognised by
    /// shape, because the shape is all the store writes.
    #[cfg(feature = "workflow")]
    #[test]
    fn the_truncated_payload_stub_is_recognised_and_nothing_else_is() {
        assert!(is_truncated_payload(
            &serde_json::json!({"truncated": true})
        ));
        assert!(!is_truncated_payload(&serde_json::json!({
            "truncated": false
        })));
        assert!(
            !is_truncated_payload(&serde_json::json!({"truncated": "yes"})),
            "only the literal boolean stub the store writes counts"
        );
        assert!(!is_truncated_payload(&serde_json::json!({"steps": 3})));
        assert!(!is_truncated_payload(&serde_json::json!(null)));
    }
}
