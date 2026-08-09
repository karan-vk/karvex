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
    WorkflowExpandRejection, WorkflowExpandRejectionReason, WorkflowGrowthLimit,
    WorkflowGrowthLimitKind, WorkflowIsolation, WorkflowNodeKind, WorkflowRunEdgeInfo,
    WorkflowRunGraph, WorkflowRunInfo, WorkflowRunNodeInfo, WorkflowRunner, WorkflowSummary,
    WorkflowTier, WorkflowVersionOrigin,
};
use crate::api::schema::{
    WorkflowCreateParams, WorkflowNodeExpandParams, WorkflowNodeReportParams,
    WorkflowNodeSteerParams, WorkflowNodeTarget, WorkflowRunListParams, WorkflowRunParams,
    WorkflowRunTarget, WorkflowTarget, WorkflowVersionCreateParams, WorkflowVersionTarget,
};
#[cfg(feature = "workflow")]
use crate::app::workflow::{
    wire_blocker, wire_demand, wire_edge_kind, wire_evidence, wire_node_status, wire_run_status,
    wire_succession, wire_tier, ActiveRun,
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
    EdgePayload, EngineInput, GrowthLimits, InstancePath, Isolation, Kvdag, KvdagEdge, KvdagNode,
    KvdagVersionId, NodeKey, NodeKind, NodeStatus, NodeToken, RunGraph, RunId, RunStatus, Runner,
    WorkflowId,
};
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
        params: WorkflowRunListParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "workflow_id", &params.workflow_id) {
            return error;
        }
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

        let created = self.workflow_store.call(move |cx| {
            let workflow_id = cx.block_on(cx.store().create_workflow(
                definition.name.trim(),
                &definition.description,
                definition.tier(),
            ))?;
            let kvdag = cx.block_on(cx.store().create_version(
                &workflow_id,
                VersionOrigin::Authored,
                "",
                definition.spec(&workflow_id),
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
            Ok::<_, StoreError>((summary, version_record))
        });

        match created {
            Ok(Ok((summary, version_record))) => {
                let version = wire_version_summary(&version_record);
                encode_success(
                    id,
                    ResponseResult::WorkflowCreated {
                        workflow: wire_workflow_summary(summary, Some(&version)),
                        version,
                    },
                )
            }
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
        if self.workflow.is_live() {
            let refused = crate::app::workflow::WorkflowStartError::RunInFlight;
            let message = self.workflow_run_in_flight_message();
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
        let created = self.workflow_store.call({
            let workspace_id = workspace_id.clone();
            let assignments = assignments.clone();
            move |cx| {
                cx.block_on(cx.store().create_run(NewRun {
                    workflow: workflow_id,
                    version: version_id,
                    tier,
                    args: ordered,
                    growth,
                    started_at_unix_ms,
                    assignments,
                    context_runs: Vec::new(),
                    workspace_id,
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
        self.state.set_workflow_run_name(workflow_name);

        let graph = RunGraph::materialise_with(&kvdag, run_id.clone(), tier, &assignments);
        let mut active = ActiveRun::new(
            run_id.clone(),
            kvdag.workflow_id.clone(),
            kvdag.version_id.clone(),
            tier,
        )
        .with_args(args)
        .with_placement(workspace_id, None);
        // The last of the three clocks H1 describes. `ActiveRun::new` stamps
        // its own `now`, which is a second reading of the same instant; the run
        // row was bound from `started_at_unix_ms`, so the live projection is
        // moved onto that one value rather than left milliseconds apart from
        // the journal (§4 D15).
        active.started_at_unix_ms = started_at_unix_ms;

        if let Err(error) = self.start_workflow_run(active, kvdag, graph) {
            let message = match error {
                crate::app::workflow::WorkflowStartError::RunInFlight => {
                    self.workflow_run_in_flight_message()
                }
            };
            return encode_error(id, error.code(), message);
        }
        match self.workflow_run_info(&run_id) {
            Some(run) => encode_success(id, ResponseResult::WorkflowRunStarted { run }),
            None => encode_error(
                id,
                NO_ACTIVE_RUN_CODE,
                "the run was created but is no longer the active run",
            ),
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
        match self.stored_run(&run_id) {
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
        if let Some(error) = require_non_empty(&id, "workflow_id", &params.workflow_id) {
            return error;
        }
        let selector = params.workflow_id.trim().to_string();
        let limit = params.limit.unwrap_or(DEFAULT_RUN_LIST_LIMIT).max(1);
        // `kvx workflow run list <name|id>` takes the same selector as `show`,
        // `update`, and `run start`, so it resolves it the same way; listing a
        // workflow's runs by the name the user gave it must not be the one
        // verb that only speaks record ids.
        let listed = self.workflow_store.call(move |cx| {
            let workflow_id = match resolve_workflow_selector(cx, &selector)? {
                WorkflowSelector::Found(workflow_id) => workflow_id,
                WorkflowSelector::NotFound => return Ok::<_, StoreError>(LookupResult::NotFound),
                WorkflowSelector::Ambiguous => return Ok(LookupResult::Ambiguous),
            };
            let records = cx.block_on(cx.store().list_runs(&workflow_id, limit))?;
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

    pub(super) fn handle_workflow_run_cancel(
        &mut self,
        id: String,
        target: WorkflowRunTarget,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &target.run_id) {
            return error;
        }
        let run_id = RunId::new(target.run_id.trim().to_string());
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
            Ok(Some((record, nodes, edges, limits)))
        });
        match loaded {
            Ok(Ok(Some((record, nodes, edges, limits)))) => {
                let graph = WorkflowRunGraph {
                    nodes: nodes
                        .into_iter()
                        .map(|node| wire_run_node_record(node, &limits))
                        .collect(),
                    edges: edges.into_iter().map(wire_run_edge_record).collect(),
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
fn wire_run_record(
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
    }
}

#[cfg(feature = "workflow")]
fn wire_run_node_record(
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
            }),
            Method::WorkflowRunGet(WorkflowRunTarget {
                run_id: "workflow_run:1".into(),
            }),
            Method::WorkflowRunList(WorkflowRunListParams {
                workflow_id: "workflow:1".into(),
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
            }),
            Method::WorkflowRunList(WorkflowRunListParams {
                workflow_id: "workflow:1".into(),
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
        let code = error_code(&response);
        assert!(
            code == INVALID_DEFINITION_CODE || code == "workflow_store_error",
            "unexpected code {code}: {response}"
        );
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
                workflow_id: "ship-feature".into(),
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
                workflow_id: "does-not-exist".into(),
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
            version: KvdagVersionId::new("kvdag_version:t4"),
            tier: Tier::Auto,
            status: RunStatus::Succeeded,
            args: BTreeMap::new(),
            context_runs: Vec::new(),
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
        };
        let wired_run = wire_run_record(run, &limits);
        let run_limit = wired_run.growth_limited.expect(
            "the run's most recent journalled growth limit survives the durable projection",
        );
        assert_eq!(run_limit.kind, WorkflowGrowthLimitKind::MaxNodes);
    }
}
