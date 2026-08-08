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
    ResponseResult, WorkflowArgSpec, WorkflowDefinitionFormat, WorkflowEdgePayload,
    WorkflowIsolation, WorkflowNodeKind, WorkflowRunEdgeInfo, WorkflowRunGraph, WorkflowRunInfo,
    WorkflowRunNodeInfo, WorkflowRunner, WorkflowSummary, WorkflowTier, WorkflowVersionOrigin,
};
use crate::api::schema::{
    WorkflowCreateParams, WorkflowNodeReportParams, WorkflowNodeSteerParams, WorkflowNodeTarget,
    WorkflowRunListParams, WorkflowRunParams, WorkflowRunTarget, WorkflowTarget,
    WorkflowVersionCreateParams, WorkflowVersionTarget,
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
use crate::workflow::definition::{Definition, DefinitionError};
#[cfg(feature = "workflow")]
use crate::workflow::engine::{is_closed_run, ReportOutcome, ReportVerdict};
#[cfg(feature = "workflow")]
use crate::workflow::model::{
    EdgePayload, EngineInput, InstancePath, Isolation, Kvdag, KvdagEdge, KvdagNode, KvdagVersionId,
    NodeKind, NodeStatus, RunGraph, RunId, RunStatus, Runner, WorkflowId,
};
#[cfg(feature = "workflow")]
use crate::workflow::store::{NewRun, StoreError, VersionOrigin, VersionRecord};
#[cfg(feature = "workflow")]
use crate::workflow::tier::Tier;

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
/// Restarting a node inside a closed run resurrects a process nothing will
/// collect a result from, so it is refused rather than performed.
#[cfg(feature = "workflow")]
const RUN_CLOSED_CODE: &str = "workflow_run_closed";

/// Default page size for `workflow.run.list`.
#[cfg(feature = "workflow")]
const DEFAULT_RUN_LIST_LIMIT: u32 = 50;

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
            Ok(LookupResult::Found((
                wire_workflow_summary(workflow, head.as_ref()),
                versions,
            )))
        });
        match looked_up {
            Ok(Ok(LookupResult::Found((workflow, versions)))) => {
                encode_success(id, ResponseResult::WorkflowGet { workflow, versions })
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

        let created = self.workflow_store.call(move |cx| {
            let workflow_id = match resolve_workflow_selector(cx, &selector)? {
                WorkflowSelector::Found(workflow_id) => workflow_id,
                WorkflowSelector::NotFound => return Ok::<_, StoreError>(LookupResult::NotFound),
                WorkflowSelector::Ambiguous => return Ok(LookupResult::Ambiguous),
            };
            let kvdag = cx.block_on(cx.store().create_version(
                &workflow_id,
                VersionOrigin::Authored,
                &change_summary,
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
        let growth = kvdag.growth;
        let ordered: BTreeMap<String, String> = args.clone().into_iter().collect();
        // Recorded on the run row at create time — a run's workspace binding is
        // a property of the run, not of whichever server happens to be
        // executing it, so a run read back from the journal keeps it too.
        let workspace_id = self.active_workspace_public_id();
        let created = self.workflow_store.call({
            let workspace_id = workspace_id.clone();
            move |cx| {
                cx.block_on(cx.store().create_run(NewRun {
                    workflow: workflow_id,
                    version: version_id,
                    tier,
                    args: ordered,
                    growth,
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

        let graph = RunGraph::materialise(&kvdag, run_id.clone(), tier);
        let active = ActiveRun::new(
            run_id.clone(),
            kvdag.workflow_id.clone(),
            kvdag.version_id.clone(),
            tier,
        )
        .with_args(args)
        .with_placement(workspace_id, None);

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
            Ok(LookupResult::Found(
                cx.block_on(cx.store().list_runs(&workflow_id, limit))?,
            ))
        });
        match listed {
            Ok(Ok(LookupResult::Found(runs))) => {
                // The live run's authoritative status is the engine's, not the
                // journal's, so the in-memory projection wins where it applies.
                let runs = runs
                    .into_iter()
                    .map(|record| {
                        self.workflow_run_info(&record.id)
                            .unwrap_or_else(|| wire_run_record(record))
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
        // A run that has closed will never settle again, so a node handed back
        // to it becomes a live process inside a `cancelled`/`failed`/`succeeded`
        // run that nothing will ever collect a result from — and whose pane
        // leaks. `apply_node_input` would happily report that as a success.
        let run = RunId::new(target.run_id.trim().to_string());
        if self.workflow_run_info(&run).is_none() {
            return not_the_active_run(id, &target.run_id);
        }
        if let Some(status) = self
            .workflow
            .run_status()
            .filter(|status| is_closed_run(*status))
        {
            return encode_error(
                id,
                RUN_CLOSED_CODE,
                format!(
                    "run {run} is already {}; a closed run cannot restart node {}. \
                     Start a new run with `kvx workflow run start <name|id>`.",
                    run_status_label(status),
                    target.path.trim()
                ),
            );
        }
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
            Ok(Some((record, nodes, edges)))
        });
        match loaded {
            Ok(Ok(Some((record, nodes, edges)))) => {
                let graph = WorkflowRunGraph {
                    nodes: nodes.into_iter().map(wire_run_node_record).collect(),
                    edges: edges.into_iter().map(wire_run_edge_record).collect(),
                };
                Ok(Some((wire_run_record(record), graph)))
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

#[cfg(feature = "workflow")]
fn wire_run_record(record: crate::workflow::store::RunRecord) -> WorkflowRunInfo {
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
    }
}

#[cfg(feature = "workflow")]
fn wire_run_node_record(record: crate::workflow::store::RunNodeRecord) -> WorkflowRunNodeInfo {
    WorkflowRunNodeInfo {
        path: record.instance_path.to_string(),
        node_key: record.node_key.to_string(),
        parent_path: None,
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
}
