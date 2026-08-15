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
use crate::api::schema::{
    ErrorBody, KvdagEdgeInfo, KvdagNodeInfo, KvdagVersionDetail, KvdagVersionSummary,
    ResponseResult, WorkflowArgSpec, WorkflowDefinitionFormat, WorkflowDetail, WorkflowEdgePayload,
    WorkflowGrowthLimit, WorkflowGrowthLimitKind, WorkflowIsolation, WorkflowNodeKind,
    WorkflowRunEdgeInfo, WorkflowRunGraph, WorkflowRunInfo, WorkflowRunNodeInfo, WorkflowRunner,
    WorkflowSummary, WorkflowTier, WorkflowVersionOrigin,
};
use crate::api::schema::{
    WorkflowCreateParams, WorkflowNodeExpandParams, WorkflowNodeReportParams,
    WorkflowNodeSteerParams, WorkflowNodeTarget, WorkflowRunFinishParams, WorkflowRunListParams,
    WorkflowRunParams, WorkflowRunTarget, WorkflowSummaryListParams, WorkflowTarget,
    WorkflowVersionCreateParams, WorkflowVersionTarget,
};
#[cfg(feature = "workflow")]
use crate::api::schema::{
    WorkflowRestoreReport, WorkflowRestoreSkip, WorkflowRestoreSkipReason, WorkflowRestoredFrom,
};
#[cfg(feature = "workflow")]
use crate::app::workflow::{
    wire_blocker, wire_demand, wire_edge_kind, wire_evidence, wire_node_status, wire_run_status,
    wire_run_summary_record, wire_succession, wire_tier,
};
#[cfg(feature = "workflow")]
use crate::app::workflow_store::StoreUnavailable;
use crate::app::App;
#[cfg(feature = "workflow")]
use crate::workflow::definition::{Definition, DefinitionError};
#[cfg(feature = "workflow")]
use crate::workflow::model::{
    EdgePayload, InstancePath, Isolation, Kvdag, KvdagEdge, KvdagNode, KvdagVersionId, NodeKey,
    NodeKind, RestoredRef, RestoredSeed, RunId, Runner, WorkflowId,
};
#[cfg(feature = "workflow")]
use crate::workflow::store::error::WORKFLOW_NAME_TAKEN_CODE;
#[cfg(feature = "workflow")]
use crate::workflow::store::{
    NewRun, StoreError, StoredGrowthLimits, VersionMetadata, VersionOrigin, VersionRecord,
};
#[cfg(feature = "workflow")]
use crate::workflow::tier::{narrow_growth, resolve_assignments, HistoryIndex, Tier};

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
/// A message to one of a run's Claude Code sessions could not be handed over
/// (`09-agent-teams-rework.md` §3.5a). One code for every reason — messaging
/// switched off on this machine, no session identified yet, an unknown target,
/// a target with no inbox socket, an unusable message, or a socket that refused
/// the write — with the reason in the message text, matching the existing
/// single-code style. Never answered `ok` for a message that was not written:
/// a control surface that claims delivery it did not achieve is the exact
/// failure the deferred `m` verb was deferred to avoid.
#[cfg(feature = "workflow")]
const MESSAGE_REFUSED_CODE: &str = "workflow_run_message_refused";
/// The lead's end-of-run report was malformed: no summary, both spellings of
/// it, or a summary file that could not be read
/// (`09-agent-teams-rework.md` §3.3).
#[cfg(feature = "workflow")]
const INVALID_ARGUMENT_CODE: &str = "workflow_invalid_argument";
// Five codes were deleted here with the engine (`09-agent-teams-rework.md`
// §2), because nothing can produce them any more:
// `workflow_node_{not_running,delivery_failed,result_invalid}` belonged to the
// node contract's delivery and completion gates, `workflow_run_closed` to the
// four node verbs' closed-run guard, and `workflow_transcript_unavailable`
// lives on in `src/app/workflow_history.rs` alone now that interrogation is
// gone from the API.
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
/// A node verb that only the removed engine could serve
/// (`09-agent-teams-rework.md` §2). Its own code rather than a bare
/// `not_implemented`, so a client that still sends one of these can tell "this
/// server does not execute nodes any more" from "this method never existed".
#[cfg(feature = "workflow")]
const NODE_VERB_RETIRED_CODE: &str = "workflow_node_verb_retired";

/// The one answer every retired node verb gives. `action` completes the
/// sentence "karvex no longer executes nodes, so it cannot …".
#[cfg(feature = "workflow")]
fn node_verb_retired(id: String, action: &str) -> String {
    encode_error(
        id,
        NODE_VERB_RETIRED_CODE,
        format!(
            "karvex no longer executes workflow nodes, so it cannot {action}. A run is a Claude \
             Code team lead in a pane: open the node's pane to steer it, or message the lead."
        ),
    )
}

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

    /// The identity hook's callback, with no runs to report to.
    ///
    /// Answers the same `workflow_unavailable` every other workflow method
    /// does, rather than a success: a hook that got a cheerful `role: ignored`
    /// from a server with no workflow subsystem would look like a session that
    /// was considered and rejected, which is a different fact. The hook itself
    /// exits 0 regardless — it must never fail a session's startup.
    pub(super) fn handle_workflow_run_report_session(
        &mut self,
        id: String,
        params: crate::api::schema::WorkflowRunReportSessionParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &params.run_id) {
            return error;
        }
        self.workflow_unavailable(id)
    }

    pub(super) fn handle_workflow_run_message(
        &mut self,
        id: String,
        params: crate::api::schema::WorkflowRunMessageParams,
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
        params: crate::api::schema::WorkflowNodeInterrogateParams,
    ) -> String {
        if let Some(error) = require_interrogate_params(&id, &params) {
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
        // D-6: `isolation = "worktree"` has no lead-path implementation —
        // nothing binds a node's pane to a worktree any more, so accepting it
        // would author a promise the run can never keep. Rejected here, at
        // authoring time, before either write, for the same "no orphaned
        // write" reason `validate_graph` is hoisted in front of them.
        if let Err(message) = reject_worktree_isolation(&definition) {
            return encode_error(id, INVALID_DEFINITION_CODE, message);
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
        // D-6: same authoring-time rejection as `workflow.create` — see its
        // comment. A new version is a single write, so there is no "orphaned
        // workflow row" risk to hoist this in front of, but the document
        // still must not reach the store with a promise the lead path cannot
        // keep.
        if let Err(message) = reject_worktree_isolation(&definition) {
            return encode_error(id, INVALID_DEFINITION_CODE, message);
        }
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
        // One team lead at a time. Refusing here rather than after `create_run`
        // is what keeps a refused start from leaving an orphan `workflow_run`
        // row that no lead will ever advance.
        if self.lead_run_is_live() {
            let refused = crate::app::workflow::WorkflowStartError::RunInFlight;
            return encode_error(id, refused.code(), self.workflow_run_in_flight_message());
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

        // §4 D11: restore is resolved **before** `create_run`, for the same
        // reason the definition is. An unknown selector is a hard error, and a
        // hard error after the row exists would leave an orphan `workflow_run`
        // no lead will ever pick up.
        //
        // It is also resolved before the `claude` preflight below. Both are
        // pre-`create_run` refusals, so the order only decides which one a
        // caller who is wrong twice hears about — and a malformed request is
        // the caller's to fix whatever this machine has installed, while
        // checking the request second would mask a restore typo behind
        // `workflow_lead_unavailable` on every host without agent teams.
        let restore = match params.restore_from.as_ref() {
            Some(request) => match self.resolve_restore(request, &kvdag) {
                Ok(plan) => Some(plan),
                Err(response) => return response(id),
            },
            None => None,
        };

        // §3.1 step 5 / §4's last risk row. The preflight runs before
        // `create_run` for the same reason the definition resolution does: a
        // hard error after the row exists leaves an orphan run nothing will
        // ever advance. A `claude` too old for agent teams starts fine and
        // then silently never spawns a teammate, which is the failure this
        // turns into a clear message.
        if let Err(error) = self.preflight_claude_for_lead() {
            return encode_error(id, error.code(), error.to_string());
        }
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
        let _ = workspace_id;
        // WI-R1 (`phase4-retarget-plan.md` amendment log, "Orchestrator
        // findings"): `--restore-from` used to resolve `seeds` here and then
        // discard them — `kvx workflow run --restore-from ...` and the run
        // browser's `r` verb silently started an ordinary fresh run while
        // still looking like a restore, because prior-run *summaries* still
        // reached the lead via `context/prior-runs.md` regardless. Honoured,
        // not refused: karvex owns the render contract, so the resolved
        // selection is handed to the lead below instead of thrown away.
        // Counted here, before `restore` is consumed by the response below.
        let restore_skipped = restore
            .as_ref()
            .map(|plan| plan.report.skipped.len())
            .unwrap_or(0);

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
        let restore_context = restore_from_run.as_ref().map(|source_run| {
            crate::workflow::lead_prompt::RestoreContext {
                source_run,
                seeds: &seeds,
                skipped: restore_skipped,
            }
        });
        let prompt = crate::workflow::lead_prompt::render_lead_prompt(
            &crate::workflow::lead_prompt::LeadPromptInput {
                run_id: &run_id,
                workflow_name: &workflow_name,
                kvdag: &kvdag,
                tier,
                args: &ordered_args,
                history: &history,
                summary_path: &summary_path,
                // §3.6: `include_prior_summaries` writes the file whether or
                // not anything reads it, so naming it here is what makes the
                // parameter mean something to a lead.
                prior_runs_path: prior_runs_path.as_deref(),
                // D-6: a concurrency hint, not a cap karvex enforces — there
                // is no engine left to enforce it against.
                max_parallel_nodes: self.workflow_policy.max_parallel_nodes,
                // WI-R1: what `--restore-from` resolved, if anything.
                restore: restore_context,
            },
        );
        if let Err(error) = self.write_lead_run_files(&spec, &prompt) {
            return encode_error(id, error.code(), error.to_string());
        }
        // Not a launch gate: a run whose sessions cannot be messaged is still a
        // run. Resolved here so the answer is a recorded property of the run
        // rather than something re-derived per message, and so a documented
        // kill switch is reported once, loudly, instead of turning the message
        // verb into a silent no-op (`09-agent-teams-rework.md` §3.5a).
        let messaging = self.preflight_messaging_for_lead();
        // Keyed off `Available`, not off `is_available()`: a *suspected* kill
        // switch counts as available on purpose (S3 proved the variable does
        // nothing on an account whose feature flags are already cached), and
        // gating the warning on availability meant the one case karvex cannot
        // check was also the one case nobody was ever told about.
        if !matches!(
            messaging,
            crate::workflow::binding::messaging::MessagingSupport::Available
        ) {
            tracing::warn!(
                run = %run_id,
                reason = messaging.code(),
                blocking = messaging.blocks_messaging(),
                "{messaging}"
            );
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
            messaging,
        );
        // `create_run` writes `pending`; the lead is live the moment its pane
        // is, and nothing else will move the row off `pending` now that no
        // engine is watching it.
        self.mark_lead_run_running(&run_id);

        match self.stored_run(&run_id) {
            Ok(Some((run, _graph))) => {
                // The run exists and is `running`; say so to whoever subscribed
                // (§3.6 keeps `workflow.run.started` on the wire) and to the run
                // browser if it is open.
                self.emit_workflow_run_event(
                    crate::api::schema::EventKind::WorkflowRunStarted,
                    crate::api::schema::EventData::WorkflowRunStarted { run: run.clone() },
                );
                encode_success(
                    id,
                    ResponseResult::WorkflowRunStarted {
                        run,
                        restore: restore.map(|plan| plan.report),
                    },
                )
            }
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
        self.emit_workflow_run_event(
            crate::api::schema::EventKind::WorkflowRunSummarized,
            crate::api::schema::EventData::WorkflowRunSummarized {
                run_id: run_id.to_string(),
                summary: summary.clone(),
            },
        );
        match self.stored_run(&run_id) {
            Ok(Some((run, _graph))) => {
                self.emit_workflow_run_event(
                    crate::api::schema::EventKind::WorkflowRunFinished,
                    crate::api::schema::EventData::WorkflowRunFinished { run: run.clone() },
                );
                encode_success(id, ResponseResult::WorkflowRunFinished { run, summary })
            }
            Ok(None) => encode_error(id, NOT_FOUND_CODE, format!("no run {run_id}")),
            Err(response) => response(id),
        }
    }

    /// One of the run's Claude Code sessions identifying itself
    /// (`09-agent-teams-rework.md` §3.1a).
    ///
    /// Authorised exactly like `workflow.run.finish` — possession of the run
    /// id, which karvex baked into the hook command in the run's own settings
    /// file — and then checked against the pane id karvex put in that pane's
    /// environment. A report that turns out not to be this run's is a
    /// *successful* response carrying `role: ignored`: the same hook fires in
    /// every session that inherits the run's settings, and a hook that gets an
    /// error back would retry or log noise for something entirely normal.
    pub(super) fn handle_workflow_run_report_session(
        &mut self,
        id: String,
        params: crate::api::schema::WorkflowRunReportSessionParams,
    ) -> String {
        if params.run_id.trim().is_empty() || params.session_id.trim().is_empty() {
            return encode_error(
                id,
                INVALID_ARGUMENT_CODE,
                "a session report needs both a run id and a session id",
            );
        }
        let report = crate::workflow::binding::identity::SessionReport {
            run_id: params.run_id.clone(),
            pane_id: params.pane_id.clone(),
            session_id: params.session_id.clone(),
            transcript_path: params.transcript_path.clone(),
            cwd: params.cwd.clone(),
            source: params.source.clone(),
            messaging_socket: params.messaging_socket.clone(),
            messaging_token: params.messaging_token.clone(),
            agent_id: params.agent_id.clone(),
        };
        self.record_run_session_report(&report);

        // Answered from the run's own view rather than from the classifier a
        // second time, so the response cannot claim a role the server did not
        // actually record.
        let (role, team_name, addressable) = self.run_session_report_outcome(&params.session_id);
        encode_success(
            id,
            ResponseResult::WorkflowRunSessionReported {
                run_id: params.run_id,
                role,
                team_name,
                addressable,
            },
        )
    }

    /// Sends text into one of a live run's Claude Code sessions
    /// (`09-agent-teams-rework.md` §3.5a).
    pub(super) fn handle_workflow_run_message(
        &mut self,
        id: String,
        params: crate::api::schema::WorkflowRunMessageParams,
    ) -> String {
        use crate::workflow::binding::messaging::Priority;

        let run_id = crate::workflow::model::RunId::new(params.run_id.clone());
        if !self.is_live_lead_run(&run_id) {
            return encode_error(
                id,
                NO_ACTIVE_RUN_CODE,
                format!("{run_id} is not the run live on this server, so it cannot be messaged"),
            );
        }
        let Some(priority) = params
            .priority
            .as_deref()
            .map_or(Some(Priority::default()), Priority::parse)
        else {
            return encode_error(
                id,
                INVALID_ARGUMENT_CODE,
                "priority must be one of now, next, or later",
            );
        };
        match self.message_run_session(&params.target, &params.text, priority) {
            Ok(receipt) => encode_success(
                id,
                ResponseResult::WorkflowRunMessaged {
                    receipt: crate::api::schema::WorkflowRunMessageReceipt {
                        run_id: params.run_id,
                        target: receipt.target,
                        session_id: receipt.session_id,
                        pane_id: receipt.pane_id,
                        channel: receipt.channel.as_str().to_string(),
                    },
                },
            ),
            Err(error) => encode_error(id, MESSAGE_REFUSED_CODE, error.to_string()),
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
                // Every run — live or historical — is read back from the
                // journal now: the run projection writes what the team does as
                // it happens, so there is no second, in-memory truth to prefer.
                let runs = records
                    .into_iter()
                    .map(|record| {
                        let limits = StoredGrowthLimits {
                            last: limits.get(&record.id).cloned(),
                            by_path: BTreeMap::new(),
                        };
                        wire_run_record(record, &limits)
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
        // §3.3: a run cancels by closing its lead's pane. Teammates belong to
        // the lead, so there is no task-level kill choreography.
        if !self.is_live_lead_run(&run_id) {
            return not_the_active_run(id, run_id.as_str());
        }
        self.cancel_lead_run(&run_id, unix_now_ms());
        match self.stored_run(&run_id) {
            Ok(Some((run, _graph))) => {
                // Cancelling is terminal, so it is a `run.finished` on the
                // wire: there is no separate cancelled event kind, and a client
                // that only watches finishes must not miss this one.
                self.emit_workflow_run_event(
                    crate::api::schema::EventKind::WorkflowRunFinished,
                    crate::api::schema::EventData::WorkflowRunFinished { run: run.clone() },
                );
                encode_success(id, ResponseResult::WorkflowRunCancelled { run })
            }
            Ok(None) => encode_error(id, NOT_FOUND_CODE, format!("no run {run_id}")),
            Err(response) => response(id),
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

    /// `workflow.node.steer` — no longer implementable.
    ///
    /// A node is a Claude Code teammate in its own pane now
    /// (`09-agent-teams-rework.md` §3.5): steering one is clicking its pane and
    /// typing, or messaging the lead. The op is answered rather than routed to
    /// the dispatcher's `not_implemented` catch-all so a client that still
    /// sends it learns why it stopped working.
    pub(super) fn handle_workflow_node_steer(
        &mut self,
        id: String,
        params: WorkflowNodeSteerParams,
    ) -> String {
        if let Some(error) = require_steer_params(&id, &params) {
            return error;
        }
        node_verb_retired(id, "steer a node")
    }

    /// `workflow.node.interrupt` — no longer implementable. See
    /// [`Self::handle_workflow_node_steer`].
    pub(super) fn handle_workflow_node_interrupt(
        &mut self,
        id: String,
        target: WorkflowNodeTarget,
    ) -> String {
        if let Some(error) = require_node_target(&id, &target) {
            return error;
        }
        node_verb_retired(id, "interrupt a node")
    }

    /// `workflow.node.report` — no longer implementable.
    ///
    /// The node contract that minted a token and validated a result against a
    /// node's `output_schema` went with the engine (§2). A teammate reports
    /// through Claude Code's shared task list, which the run projection reads.
    pub(super) fn handle_workflow_node_report(
        &mut self,
        id: String,
        params: WorkflowNodeReportParams,
    ) -> String {
        if let Some(error) = require_report_params(&id, &params) {
            return error;
        }
        node_verb_retired(id, "report a node result")
    }

    /// `workflow.node.restart` — no longer implementable. The lead reassigns or
    /// respawns a task; karvex records what it did rather than deciding it.
    pub(super) fn handle_workflow_node_restart(
        &mut self,
        id: String,
        target: WorkflowNodeTarget,
    ) -> String {
        if let Some(error) = require_node_target(&id, &target) {
            return error;
        }
        node_verb_retired(id, "restart a node")
    }

    /// `workflow.node.expand` — no longer implementable. The lead creates tasks
    /// freely and karvex records emergent nodes, instead of judging proposals
    /// against growth guardrails (§2).
    pub(super) fn handle_workflow_node_expand(
        &mut self,
        id: String,
        params: WorkflowNodeExpandParams,
    ) -> String {
        if let Some(error) = require_expand_params(&id, &params) {
            return error;
        }
        node_verb_retired(id, "expand a node")
    }

    /// `workflow.node.interrogate` — no longer implementable.
    ///
    /// It forked or reconstructed a Claude session karvex's own engine owned
    /// (`binding/interrogate.rs`), and 09 §3.6 makes its absence deliberate:
    /// Phase E's interrogate resumes a *member's* session id out of the run
    /// snapshot, which is a different mechanism. The method itself stays on the
    /// wire until the protocol bump removes it, so it is answered here rather
    /// than left to the dispatcher's `not_implemented` catch-all — a client
    /// told "not implemented yet" would reasonably keep retrying.
    pub(super) fn handle_workflow_node_interrogate(
        &mut self,
        id: String,
        params: crate::api::schema::WorkflowNodeInterrogateParams,
    ) -> String {
        if let Some(error) = require_interrogate_params(&id, &params) {
            return error;
        }
        node_verb_retired(id, "interrogate a node")
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

    /// `workflow_run_in_flight`, told truthfully: which run is holding the
    /// server and how to end it. There is no node-level detail any more — the
    /// blocking node is whatever the lead's team is working on, which the DAG
    /// shows and this message would only ever guess at.
    fn workflow_run_in_flight_message(&self) -> String {
        let Some(run) = self
            .workflow_lead
            .as_ref()
            .map(|lead| lead.run_id.to_string())
        else {
            return crate::app::workflow::WorkflowStartError::RunInFlight
                .message()
                .to_string();
        };
        format!(
            "another workflow run is still in flight: run {run}'s team lead is live. \
             Steer it in its own pane, or end the run with `kvx workflow run cancel {run}`."
        )
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
                    // Live-only, and deliberately attached here rather than
                    // stored: a session's inbox socket is unlinked when its
                    // process exits, so a persisted one would be a durable
                    // record of something that stopped being true.
                    messaging: self.run_messaging_info(run),
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
        let limit = u32::try_from(self.workflow_policy.history_context_runs).unwrap_or(u32::MAX);
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

/// The one wording for "this server is not executing that run".
///
/// `workflow.run.cancel` is the only caller left: the node verbs that shared it
/// answer `workflow_node_verb_retired` now (§2). It stays a function rather
/// than an inline `format!` because the CLI matches on this sentence and the
/// duplicate literal is exactly how the two would drift apart.
#[cfg(feature = "workflow")]
fn not_the_active_run(id: String, run_id: &str) -> String {
    encode_error(
        id,
        NO_ACTIVE_RUN_CODE,
        format!("run {run_id} is not the run this server is executing"),
    )
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

/// D-6: `isolation = "worktree"` used to parse cleanly and then be a silent
/// no-op — nothing in the lead path binds a node's pane to a worktree, so
/// authoring it promised isolation the run could never deliver. Both
/// authoring entry points (`workflow.create`, `workflow.version.create`)
/// call this before their first write; `isolation = "none"` (the default)
/// is unaffected.
///
/// The rule itself lives in `workflow::definition` — the pure layer — because
/// `workflow.review.apply` compiles a definition with no document in hand and
/// has to pass exactly the same gate (P11): a version karvex minted from a
/// review must never be one it would have refused from a human.
#[cfg(feature = "workflow")]
fn reject_worktree_isolation(definition: &Definition) -> Result<(), String> {
    match crate::workflow::definition::worktree_isolation_rejection(&definition.node) {
        Some(rejection) => Err(rejection.message),
        None => Ok(()),
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
        watchdog_interventions: record.watchdog_interventions,
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
        // §3.4: what the run projection observed. Empty/absent for every
        // engine-era run, which never had a task list behind it.
        task_id: record.task_id,
        subject: record.subject,
        owner: record.owner,
        emergent: record.emergent,
        // §3.6/D-10: karvex's own opinion about the node, in its own column,
        // never the projected `status` beside it. Written by the watchdog
        // adapter (packet P9) and read back verbatim here — the wave-0 shape
        // this replaced reported `None` for every node because nothing wrote
        // the column yet.
        attention: record.attention.map(wire_attention),
    }
}

/// `run_node.attention` on the wire.
///
/// A total match rather than a string passthrough, so a new [`Attention`]
/// variant fails to compile here instead of reaching a client as a word its
/// schema does not know.
#[cfg(feature = "workflow")]
fn wire_attention(
    attention: crate::workflow::model::Attention,
) -> crate::api::schema::WorkflowAttention {
    use crate::api::schema::WorkflowAttention;
    use crate::workflow::model::Attention;
    match attention {
        Attention::Stuck => WorkflowAttention::Stuck,
        Attention::BudgetExceeded => WorkflowAttention::BudgetExceeded,
        Attention::NeedsInput => WorkflowAttention::NeedsInput,
        Attention::LeadBlocked => WorkflowAttention::LeadBlocked,
        Attention::Unbound => WorkflowAttention::Unbound,
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

fn require_interrogate_params(
    id: &str,
    params: &crate::api::schema::WorkflowNodeInterrogateParams,
) -> Option<String> {
    require_non_empty(id, "run_id", &params.run_id)
        .or_else(|| require_non_empty(id, "path", &params.path))
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
        // Captured while the run was alive by the projection poll
        // (`.local/prd/phase4-retarget-plan.md` §3.3, packet P8) and read
        // straight off the row, so a run whose panes are long gone still
        // answers with the session ids that make it interviewable. `None`
        // means karvex never resolved one — an honest, visible outcome that
        // makes that member `evidence_only` in a review.
        session_id: record.session_id,
        last_state: record
            .last_state
            .as_deref()
            .map(crate::api::schema::WorkflowMemberState::from_stored),
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
            Method::WorkflowNodeInterrogate(crate::api::schema::WorkflowNodeInterrogateParams {
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
            // Phase 4 additions (`.local/prd/phase4-retarget-plan.md` §5
            // packet P3): the review cycle's five methods. Stubs today
            // (`workflow_review.rs`, `workflow_review_apply.rs`), but a stub
            // answering `workflow_review_not_found` is still not
            // `not_implemented`.
            Method::WorkflowReviewStart(WorkflowRunTarget {
                run_id: "workflow_run:1".into(),
            }),
            Method::WorkflowReviewGet(WorkflowRunTarget {
                run_id: "workflow_run:1".into(),
            }),
            Method::WorkflowReviewApply(crate::api::schema::WorkflowReviewApplyParams {
                run_id: "workflow_run:1".into(),
                accept: Vec::new(),
            }),
            Method::WorkflowReviewAnswer(crate::api::schema::WorkflowReviewAnswerParams {
                run_id: "workflow_run:1".into(),
                member: "research".into(),
                answer: Some(serde_json::json!({"account": "done"})),
                answer_file: None,
            }),
            Method::WorkflowReviewReport(crate::api::schema::WorkflowReviewReportParams {
                run_id: "workflow_run:1".into(),
                findings: Some(serde_json::json!([])),
                findings_file: None,
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
            Method::WorkflowNodeInterrogate(crate::api::schema::WorkflowNodeInterrogateParams {
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
            // The agent-teams rework's two run-session methods (§3.1a, §3.5a).
            // The identity hook in particular reaches a server karvex did not
            // choose the build of, so its answer here has to be the documented
            // code rather than the catch-all.
            Method::WorkflowRunReportSession(crate::api::schema::WorkflowRunReportSessionParams {
                run_id: "workflow_run:1".into(),
                session_id: "51ea857f-cb96-4372-ae75-bab1640c8428".into(),
                transcript_path: None,
                pane_id: Some("w1:p2".into()),
                cwd: None,
                source: Some("startup".into()),
                messaging_socket: None,
                messaging_token: None,
                agent_id: None,
            }),
            Method::WorkflowRunMessage(crate::api::schema::WorkflowRunMessageParams {
                run_id: "workflow_run:1".into(),
                target: "team-lead".into(),
                text: "rebase before you continue".into(),
                priority: None,
            }),
            // Phase 4 additions (`.local/prd/phase4-retarget-plan.md` §5
            // packet P3): the review cycle's five methods answer
            // `workflow_unavailable` with the feature off too, not
            // `workflow_review_not_found` — that code is specific to the
            // feature-on stub and would misreport why the request failed.
            Method::WorkflowReviewStart(WorkflowRunTarget {
                run_id: "workflow_run:1".into(),
            }),
            Method::WorkflowReviewGet(WorkflowRunTarget {
                run_id: "workflow_run:1".into(),
            }),
            Method::WorkflowReviewApply(crate::api::schema::WorkflowReviewApplyParams {
                run_id: "workflow_run:1".into(),
                accept: Vec::new(),
            }),
            Method::WorkflowReviewAnswer(crate::api::schema::WorkflowReviewAnswerParams {
                run_id: "workflow_run:1".into(),
                member: "research".into(),
                answer: Some(serde_json::json!({"account": "done"})),
                answer_file: None,
            }),
            Method::WorkflowReviewReport(crate::api::schema::WorkflowReviewReportParams {
                run_id: "workflow_run:1".into(),
                findings: Some(serde_json::json!([])),
                findings_file: None,
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

    /// D-6: `isolation = "worktree"` has no lead-path implementation, so
    /// authoring it must never reach the store — accepting it would promise
    /// isolation the run can never deliver. Refused before the first write,
    /// same as an invalid graph, and the name stays usable afterwards.
    #[cfg(feature = "workflow")]
    #[test]
    fn worktree_isolation_is_rejected_at_authoring_time_on_create() {
        let mut app = app();
        let response = app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: WorkflowDefinitionDocument {
                    format: WorkflowDefinitionFormat::Toml,
                    text: r#"
name = "wants-a-worktree"
[[node]]
key = "only"
label = "Only"
runner = "command"
command = ["/bin/true"]
prompt_template = "do it"
output_schema = { type = "object" }
isolation = "worktree"
"#
                    .to_string(),
                },
            },
        );
        assert_eq!(error_code(&response), INVALID_DEFINITION_CODE, "{response}");
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        let message = value["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("only"),
            "names the offending node: {message}"
        );
        assert!(message.contains("worktree"), "{message}");

        // The name is still usable: the rejection left no orphan row.
        let retried = app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: single_node_definition("wants-a-worktree", "do it"),
            },
        );
        let retried: serde_json::Value = serde_json::from_str(&retried).unwrap();
        assert_eq!(
            retried["result"]["type"], "workflow_created",
            "the burned name is still usable once isolation is fixed: {retried}"
        );
    }

    /// Same rejection, the other authoring entry point: a new version of an
    /// existing workflow must not adopt `isolation = "worktree"` either.
    #[cfg(feature = "workflow")]
    #[test]
    fn worktree_isolation_is_rejected_at_authoring_time_on_version_create() {
        let mut app = app();
        let created = app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: single_node_definition("isolation-version", "do it"),
            },
        );
        let created: serde_json::Value = serde_json::from_str(&created).unwrap();
        let workflow_id = created["result"]["workflow"]["workflow_id"]
            .as_str()
            .expect("the workflow was created")
            .to_string();

        let response = app.handle_workflow_version_create(
            "req".into(),
            WorkflowVersionCreateParams {
                workflow_id,
                definition: WorkflowDefinitionDocument {
                    format: WorkflowDefinitionFormat::Toml,
                    text: r#"
name = "isolation-version"
[[node]]
key = "only"
label = "Only"
runner = "command"
command = ["/bin/true"]
prompt_template = "do it"
output_schema = { type = "object" }
isolation = "worktree"
"#
                    .to_string(),
                },
                change_summary: String::new(),
            },
        );
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

    /// The retired node verbs and the guard that survived them.
    ///
    /// `workflow.node.{steer,interrupt,restart}` are gone for good — a node is
    /// a Claude Code teammate in its own pane now
    /// (`09-agent-teams-rework.md` §3.5) — so they name the *retirement*
    /// whatever run id they are handed, and never the missing run: a client
    /// that retries against a live run would otherwise be told to keep trying.
    /// `workflow.run.cancel` survives the rework, so it still answers the
    /// not-the-active-run guard for a run this server is not executing.
    #[cfg(feature = "workflow")]
    #[test]
    fn the_retired_node_verbs_name_their_retirement_and_cancel_still_guards_the_run() {
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
        ] {
            assert_eq!(error_code(&response), NODE_VERB_RETIRED_CODE, "{response}");
            let value: serde_json::Value = serde_json::from_str(&response).unwrap();
            let message = value["error"]["message"].as_str().unwrap_or_default();
            assert!(
                message.contains("pane"),
                "a retired verb names the affordance that replaced it: {message}"
            );
        }

        let cancelled = app.handle_workflow_run_cancel(
            "req".into(),
            WorkflowRunTarget {
                run_id: "workflow_run:ghost".into(),
            },
        );
        assert_eq!(error_code(&cancelled), NO_ACTIVE_RUN_CODE, "{cancelled}");
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

    /// A live lead run, minus the lead.
    ///
    /// It replaces `app_with_a_bound_command_node`, which bound an engine node
    /// and minted it a result token; nodes are Claude Code tasks now and karvex
    /// records them rather than binding them (`09-agent-teams-rework.md` §2).
    /// The state is built by `App::test_bind_a_live_lead_run`, beside the
    /// production `bind_lead_run` it mirrors, so the overlay tests in
    /// `src/app/input/` build the same run these handler tests do.
    #[cfg(feature = "workflow")]
    fn app_with_a_live_lead_run() -> (App, String) {
        let mut app = app();
        let created = app.handle_workflow_create(
            "req".into(),
            WorkflowCreateParams {
                definition: single_node_definition("lead-run", "run it"),
            },
        );
        let created: serde_json::Value = serde_json::from_str(&created).unwrap();
        let workflow_id = created["result"]["workflow"]["workflow_id"]
            .as_str()
            .expect("the workflow was created")
            .to_string();
        let run_id = app.test_bind_a_live_lead_run(&workflow_id, "lead-run");
        (app, run_id.to_string())
    }

    /// B-T5 kept, re-aimed. `run cancel` on an already-closed run used to
    /// answer `ok` with an envelope literally named `workflow_run_cancelled`
    /// while the run's status stayed whatever it already was. It is still a
    /// refusal — but the code changed with the engine: there is no
    /// `workflow_run_closed` any more, because the only run this server can
    /// cancel is the one whose lead is live (§3.3), and a closed one is simply
    /// not that run.
    #[cfg(feature = "workflow")]
    #[test]
    fn cancelling_a_closed_run_is_refused_rather_than_answered_ok() {
        let (mut app, run_id) = app_with_a_live_lead_run();

        let first = app.handle_workflow_run_cancel(
            "req".into(),
            WorkflowRunTarget {
                run_id: run_id.clone(),
            },
        );
        let value: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(
            value["result"]["type"], "workflow_run_cancelled",
            "the first cancel is the one that closes the run: {value}"
        );
        assert_eq!(value["result"]["run"]["status"], "cancelled", "{value}");
        assert!(
            !app.lead_run_is_live(),
            "cancelling releases the single-live-run guard"
        );

        let second = app.handle_workflow_run_cancel(
            "req".into(),
            WorkflowRunTarget {
                run_id: run_id.clone(),
            },
        );
        assert_eq!(error_code(&second), NO_ACTIVE_RUN_CODE, "{second}");
        let value: serde_json::Value = serde_json::from_str(&second).unwrap();
        let message = value["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains(&run_id),
            "the refusal names the run it refused: {message}"
        );

        // And the refusal is a refusal, not a second mutation.
        let read_back = app.handle_workflow_run_get(
            "req".into(),
            WorkflowRunTarget {
                run_id: run_id.clone(),
            },
        );
        let value: serde_json::Value = serde_json::from_str(&read_back).unwrap();
        assert_eq!(value["result"]["run"]["status"], "cancelled", "{value}");
    }

    /// 2.14 kept, re-aimed. `workflow_run_in_flight` used to claim the blocking
    /// run was "still executing" when it was in fact paused, and named neither
    /// the run nor a way out. There is no paused state and no blocking node to
    /// name any more — the lead decides what its team works on — so what the
    /// message still owes the user is the run that is holding the server and
    /// the two ways to get past it.
    #[cfg(feature = "workflow")]
    #[test]
    fn run_in_flight_names_the_live_lead_run_and_the_way_out() {
        let (mut app, run_id) = app_with_a_live_lead_run();
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
            message.contains("team lead is live"),
            "the refusal says what is holding the server: {message}"
        );
        assert!(
            message.contains(&format!("kvx workflow run cancel {run_id}")),
            "the refusal names the way out: {message}"
        );

        // The guard refuses before `create_run`, so a refused start leaves no
        // orphan row behind — the half that made 2.14 worth pinning.
        assert_eq!(
            run_row_count(&mut app, "second-workflow"),
            0,
            "a refused start must create no run row"
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

    // Phase 2's expansion coverage — `an_accepted_expand_proposal_creates_the
    // _children_it_reports`, the rejected/truncated/wrong-token/out-of-range
    // proposals, `steer_interrupt_and_expand_are_refused_once_the_run_has
    // _closed`, and `the_persisted_growth_limits_are_the_ones_the_run_graph
    // _enforces` — went with the engine (`09-agent-teams-rework.md` §2).
    //
    // What left the product with them: `workflow.node.expand`'s proposal
    // protocol (token, template allow-list, count ceiling, truncation report)
    // and the growth guardrails it was judged against. The lead creates tasks
    // freely now and karvex records emergent nodes instead of judging them.
    // `steer`/`interrupt`/`restart`/`expand` no longer have a closed-run arm
    // because they no longer have an open-run arm — see
    // `the_retired_node_verbs_name_their_retirement_and_cancel_still_guards_the_run`
    // above. The tier narrowing those growth ceilings came from is still pinned,
    // as a pure function, in `src/workflow/tier.rs`'s
    // `a_tier_narrows_the_versions_ceilings_and_never_widens_them`.

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
        use crate::workflow::model::{Demand, NodeStatus, RunStatus};
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
            watchdog_interventions: 4,
            attention: None,
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
        // Wire-honesty sweep (`.local/prd/phase4-retarget-plan.md` §5 packet
        // P3): `watchdog_interventions` used to be hardcoded to `0` here
        // regardless of what the row carried.
        assert_eq!(
            wired_node.watchdog_interventions, 4,
            "watchdog_interventions survives the durable projection instead of \
             being hardcoded to 0"
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

    // ── Phase 3: restore and summaries ─────────────────────────────────────

    // Phase 3's interrogation coverage — the command-node refusal, the missing
    // transcript, the checkpoint-less reconstruction, and the frozen-fork argv
    // — went with `binding/interrogate.rs` (`09-agent-teams-rework.md` §3.6).
    //
    // What left the product with it: `workflow.node.interrogate`, its two modes,
    // and the `workflow_interrogation_*` codes. Phase E's interrogate is a
    // different mechanism (resume a *member's* session id out of the run's
    // snapshot), and 09 §3.6 is explicit that a seam shaped like the old one
    // would be a wrong-shaped hole, so there is nothing here to keep ignored
    // against it.

    /// `workflow.summary.get` on a run with no summary is a **success** with
    /// `summary: null`, never an error (§4 D1). The epilogue subsystem that
    /// used to produce one went with the engine; the lead writes its own
    /// summary through `workflow.run.finish` (§3.3), so "no summary yet" is now
    /// the normal state of every run that has not finished.
    #[cfg(feature = "workflow")]
    #[test]
    fn a_run_with_no_summary_answers_null_rather_than_an_error() {
        let (mut app, run_id) = app_with_a_live_lead_run();
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
    ///
    /// The restore request is resolved before the `claude` preflight as well as
    /// before `create_run`: a malformed request is the caller's to fix whatever
    /// the machine has installed, and resolving it second would mask a typo
    /// behind `workflow_lead_unavailable` on any host without agent teams.
    #[cfg(feature = "workflow")]
    #[test]
    fn an_unknown_restore_selector_is_refused_and_starts_no_run() {
        let (mut app, run_id) = app_with_a_live_lead_run();
        // Close the run so the single-live-run guard is not what refuses this.
        app.handle_workflow_run_cancel(
            "req".into(),
            WorkflowRunTarget {
                run_id: run_id.clone(),
            },
        );
        let runs_before = run_row_count(&mut app, "lead-run");

        let response = app.handle_workflow_run(
            "req".into(),
            WorkflowRunParams {
                workflow_id: "lead-run".into(),
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
            run_row_count(&mut app, "lead-run"),
            runs_before,
            "a hard restore error must be raised before create_run, or it leaves \
             an orphan run row no lead will ever pick up"
        );
    }

    /// A pruned run's own row is gone, but its `run_summary` outlives it
    /// (`03-storage-schema.md` §9), so `workflow.run.get` and a restore that
    /// names it must say **pruned**, not not-found: the caller is told which of
    /// "gone" and "never existed" happened, and the surviving surface is named
    /// rather than implied. Ported down from the engine-era headless suite,
    /// which had to run a whole workflow to reach a prunable run.
    #[cfg(feature = "workflow")]
    #[test]
    fn a_pruned_run_answers_pruned_rather_than_not_found() {
        let (mut app, run_id) = app_with_a_live_lead_run();
        // Finish it, which is what writes the `run_summary` that survives the
        // prune, and closes the run so a second start is not refused.
        app.handle_workflow_run_finish(
            "req".into(),
            WorkflowRunFinishParams {
                run_id: run_id.clone(),
                summary: Some("did the thing".into()),
                summary_file: None,
                outcome: None,
            },
        );

        let workflow = WorkflowId::new(
            serde_json::from_str::<serde_json::Value>(&app.handle_workflow_list("req".into()))
                .unwrap()["result"]["workflows"][0]["workflow_id"]
                .as_str()
                .expect("the workflow exists")
                .to_string(),
        );
        let pruned = app
            .workflow_store
            .call(move |cx| cx.block_on(cx.store().prune_run_history(&workflow, 0)))
            .expect("the in-memory store is available")
            .expect("the prune runs");
        assert_eq!(pruned, 1, "the finished run is the one that was pruned");

        let response = app.handle_workflow_run_get(
            "req".into(),
            WorkflowRunTarget {
                run_id: run_id.clone(),
            },
        );
        assert_eq!(error_code(&response), RUN_PRUNED_CODE, "{response}");
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        let message = value["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("summary"),
            "the refusal names the surface that survived: {message}"
        );

        // The summary itself still answers, which is what makes that message
        // true rather than merely encouraging.
        let summary = app.handle_workflow_summary_get(
            "req".into(),
            WorkflowRunTarget {
                run_id: run_id.clone(),
            },
        );
        let summary: serde_json::Value = serde_json::from_str(&summary).unwrap();
        assert_eq!(summary["result"]["summary"]["text"], "did the thing");

        // And a restore naming it is refused as pruned too — its checkpoints
        // went with the run.
        let response = app.handle_workflow_run(
            "req".into(),
            WorkflowRunParams {
                workflow_id: "lead-run".into(),
                version: None,
                tier: None,
                args: HashMap::new(),
                restore_from: Some(crate::api::schema::WorkflowRestoreRequest {
                    run_id,
                    nodes: Vec::new(),
                    allow_changed: false,
                }),
                include_prior_summaries: None,
            },
        );
        assert_eq!(error_code(&response), RUN_PRUNED_CODE, "{response}");
    }

    /// **Known gap, not a passing test.** `workflow.retention_runs` is a
    /// documented, published config knob (`website/src/data/config-reference
    /// .json`) and `WorkflowStore::prune_run_history` implements it correctly —
    /// but the engine's epilogue was the only thing that ever called it, so on
    /// this branch nothing prunes and the knob is a silent no-op. That is the
    /// same shape of dishonesty the rework audit's §2.4 catalogues, newly
    /// created rather than inherited.
    ///
    /// The natural owner is `workflow.run.finish`, which is where a run's
    /// history last changes — but retention deletes user data, so wiring it
    /// blind is not the right move inside a lint-and-test pass. Named here so
    /// the next change to this file has to decide.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "gap: workflow.retention_runs is published but nothing calls prune_run_history since the engine's epilogue went; needs an owner, probably run.finish"]
    fn finishing_a_run_prunes_history_past_the_retention_limit() {
        let (mut app, run_id) = app_with_a_live_lead_run();
        app.handle_workflow_run_finish(
            "req".into(),
            WorkflowRunFinishParams {
                run_id: run_id.clone(),
                summary: Some("did the thing".into()),
                summary_file: None,
                outcome: None,
            },
        );
        // With `retention_runs` honoured at all, a limit of zero would leave no
        // `workflow_run` row behind.
        let response = app.handle_workflow_run_get("req".into(), WorkflowRunTarget { run_id });
        assert_eq!(error_code(&response), RUN_PRUNED_CODE, "{response}");
    }

    /// Restoring from a run that has neither a row nor a surviving summary is
    /// a plain not-found, told apart from `workflow_run_pruned` — "gone" and
    /// "never existed" are different answers to the caller.
    #[cfg(feature = "workflow")]
    #[test]
    fn restoring_from_an_unknown_run_is_not_found_not_pruned() {
        let (mut app, run_id) = app_with_a_live_lead_run();
        app.handle_workflow_run_cancel("req".into(), WorkflowRunTarget { run_id });

        let response = app.handle_workflow_run(
            "req".into(),
            WorkflowRunParams {
                workflow_id: "lead-run".into(),
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
    /// code (E-15), and a keybind collision of the same shape (E-7) shipped
    /// and had to be fixed later.
    ///
    /// Values are pulled from the handlers themselves — the `_CODE` constants
    /// and the `WorkflowStartError::code()` method — never hand-copied, so a
    /// changed constant changes this list for free. Only the *domain* label
    /// per entry is hand-maintained, and
    /// `every_workflow_error_code_literal_in_source_is_inventoried` below
    /// greps the source tree so a brand-new code (or constant) left off this
    /// list fails loudly instead of silently going unchecked the way all of
    /// these did before.
    ///
    /// Nine codes left with the engine (`09-agent-teams-rework.md` §2): the
    /// node-contract refusals `workflow_node_{not_running,delivery_failed,
    /// result_invalid}` and `workflow_node_report_*`, the spawn family
    /// `workflow_node_spawn_*`, `workflow_run_closed`, and the interrogation
    /// pair `workflow_interrogation_{active,spawn_failed}`. None of them has a
    /// producer any more, and the grep below is what proves it: a code left in
    /// this list with nothing defining it fails just as loudly as a new one
    /// missing from it.
    #[cfg(feature = "workflow")]
    fn all_workflow_error_codes() -> Vec<(&'static str, &'static str)> {
        use crate::app::workflow::WorkflowStartError;
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
            // Owned by `workflow_history.rs` now: that module's
            // `interrogate_outcome` compiles unconditionally and is the only
            // remaining producer, so the duplicate copy this file used to carry
            // (and pin against divergence) is gone with the handlers that used
            // it.
            (
                "transcript_unavailable",
                crate::app::workflow_history::TRANSCRIPT_UNAVAILABLE_CODE,
            ),
            // A message to one of a live run's Claude Code sessions was not
            // handed over (§3.5a).
            ("run_message_refused", MESSAGE_REFUSED_CODE),
            ("run_pruned", RUN_PRUNED_CODE),
            ("restore_unknown_selector", RESTORE_UNKNOWN_SELECTOR_CODE),
            // §2: a node verb the removed engine was the only possible server
            // of. Its own domain, not `not_found`: the run may be perfectly
            // alive and the verb still gone.
            ("node_verb_retired", NODE_VERB_RETIRED_CODE),
            ("store_error", WORKFLOW_STORE_ERROR_CODE),
            ("run_in_flight", WorkflowStartError::RunInFlight.code()),
            // Phase 4 additions (`.local/prd/phase4-retarget-plan.md` §5
            // packet P3): the review cycle's own failure domains. Handlers
            // are wave-0 stubs today (`workflow_review.rs`,
            // `workflow_review_apply.rs`); the codes are inventoried here
            // ahead of wave 2b's real orchestration so the wire contract is
            // frozen once, not amended alongside the behaviour.
            (
                "review_not_found",
                crate::app::api::workflow_review::WORKFLOW_REVIEW_NOT_FOUND_CODE,
            ),
            (
                "review_in_flight",
                crate::app::api::workflow_review::WORKFLOW_REVIEW_IN_FLIGHT_CODE,
            ),
            (
                "review_not_awaiting",
                crate::app::api::workflow_review::WORKFLOW_REVIEW_NOT_AWAITING_CODE,
            ),
            (
                "review_no_interviewable_members",
                crate::app::api::workflow_review::WORKFLOW_REVIEW_NO_INTERVIEWABLE_MEMBERS_CODE,
            ),
            // Its own domain, deliberately not `definition_invalid`: nothing
            // was authored and nothing was written — the accepted findings did
            // not compile, and the cycle is still there to decide with a
            // smaller set (P11).
            (
                "review_compile_failed",
                crate::app::api::workflow_review_apply::WORKFLOW_REVIEW_COMPILE_FAILED_CODE,
            ),
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
            23,
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
