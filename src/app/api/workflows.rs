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
    WorkflowIsolation, WorkflowNodeKind, WorkflowRunGraph, WorkflowRunInfo, WorkflowRunNodeInfo,
    WorkflowRunner, WorkflowSummary, WorkflowTier, WorkflowVersionOrigin,
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
use crate::workflow::model::{
    EdgePayload, EngineInput, InstancePath, Isolation, Kvdag, KvdagEdge, KvdagNode, KvdagVersionId,
    NodeKind, RunGraph, RunId, Runner, WorkflowId,
};
#[cfg(feature = "workflow")]
use crate::workflow::store::{NewRun, StoreError, VersionOrigin};
#[cfg(feature = "workflow")]
use crate::workflow::tier::Tier;

use super::responses::encode_error;
#[cfg(feature = "workflow")]
use super::responses::{encode_error_body, encode_success};

/// Error code returned for every `workflow.*` call when the crate is built
/// with `--no-default-features` (the `workflow` cargo feature off).
#[cfg(not(feature = "workflow"))]
const WORKFLOW_UNAVAILABLE_CODE: &str = "workflow_unavailable";
#[cfg(not(feature = "workflow"))]
const WORKFLOW_UNAVAILABLE_MESSAGE: &str =
    "the workflow feature is not compiled into this server (built with --no-default-features)";

/// The definition document could not be parsed, or the graph it describes
/// fails `Kvdag::try_new`'s construction invariants.
#[cfg(feature = "workflow")]
const INVALID_DEFINITION_CODE: &str = "workflow_invalid_definition";
/// No workflow, version, run, or node with that id.
#[cfg(feature = "workflow")]
const NOT_FOUND_CODE: &str = "workflow_not_found";
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

/// Default page size for `workflow.run.list`.
#[cfg(feature = "workflow")]
const DEFAULT_RUN_LIST_LIMIT: u32 = 50;

#[cfg(not(feature = "workflow"))]
impl App {
    /// `--no-default-features` path (`05-phase-plan.md` W3 "Feature-off
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
        let workflow_id = WorkflowId::new(target.workflow_id.trim().to_string());
        let looked_up = self.workflow_store.call(move |cx| {
            let Some(workflow) = cx.block_on(cx.store().get_workflow(&workflow_id))? else {
                return Ok::<_, StoreError>(None);
            };
            let head = head_version_summary(cx, workflow.head_version.as_ref())?;
            let versions = head.iter().cloned().collect::<Vec<_>>();
            Ok(Some((
                wire_workflow_summary(workflow, head.as_ref()),
                versions,
            )))
        });
        match looked_up {
            Ok(Ok(Some((workflow, versions)))) => {
                encode_success(id, ResponseResult::WorkflowGet { workflow, versions })
            }
            Ok(Ok(None)) => encode_error(
                id,
                NOT_FOUND_CODE,
                format!("no workflow with id {}", target.workflow_id),
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
            Ok::<_, StoreError>((summary, kvdag))
        });

        match created {
            Ok(Ok((summary, kvdag))) => {
                let version = wire_version_summary(&kvdag, VersionOrigin::Authored, "");
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
        let workflow_id = WorkflowId::new(params.workflow_id.trim().to_string());
        let change_summary = params.change_summary.clone();

        let created = self.workflow_store.call(move |cx| {
            if cx
                .block_on(cx.store().get_workflow(&workflow_id))?
                .is_none()
            {
                return Ok::<_, StoreError>(None);
            }
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
            Ok(Some((summary, kvdag)))
        });

        match created {
            Ok(Ok(Some((summary, kvdag)))) => {
                let version =
                    wire_version_summary(&kvdag, VersionOrigin::Authored, &params.change_summary);
                encode_success(
                    id,
                    ResponseResult::WorkflowVersionCreated {
                        workflow: wire_workflow_summary(summary, Some(&version)),
                        version,
                    },
                )
            }
            Ok(Ok(None)) => encode_error(
                id,
                NOT_FOUND_CODE,
                format!("no workflow with id {}", params.workflow_id),
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
        let loaded = self
            .workflow_store
            .call(move |cx| cx.block_on(cx.store().load_version(&version_id)));
        match loaded {
            Ok(Ok(kvdag)) => encode_success(
                id,
                ResponseResult::WorkflowVersionGet {
                    version: wire_version_detail(&kvdag),
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
            return encode_error(id, refused.code(), refused.message());
        }
        let workflow_id = WorkflowId::new(params.workflow_id.trim().to_string());
        let requested_version = params.version;

        // The definition is resolved before the run row is created, so a run
        // whose graph is unusable never leaves a half-started record behind.
        let resolved = self.workflow_store.call(move |cx| {
            let Some(summary) = cx.block_on(cx.store().get_workflow(&workflow_id))? else {
                return Ok::<_, StoreError>(None);
            };
            let version_id = match requested_version {
                Some(number) => cx.block_on(cx.store().find_version_id(&workflow_id, number))?,
                None => summary.head_version,
            };
            let Some(version_id) = version_id else {
                return Ok(None);
            };
            let kvdag = cx.block_on(cx.store().load_version(&version_id))?;
            Ok(Some((summary.default_tier, kvdag)))
        });
        let (default_tier, kvdag) = match resolved {
            Ok(Ok(Some(resolved))) => resolved,
            Ok(Ok(None)) => {
                return encode_error(
                    id,
                    NOT_FOUND_CODE,
                    format!(
                        "no runnable kvdag version for workflow {}",
                        params.workflow_id
                    ),
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
        let created = self.workflow_store.call(move |cx| {
            cx.block_on(cx.store().create_run(NewRun {
                workflow: workflow_id,
                version: version_id,
                tier,
                args: ordered,
                growth,
                context_runs: Vec::new(),
            }))
        });
        let run_id = match created {
            Ok(Ok(run_id)) => run_id,
            Ok(Err(error)) => return self.store_error(id, &error),
            Err(unavailable) => return unavailable_response(id, &unavailable),
        };

        let graph = RunGraph::materialise(&kvdag, run_id.clone(), tier);
        let active = ActiveRun::new(
            run_id.clone(),
            kvdag.workflow_id.clone(),
            kvdag.version_id.clone(),
            tier,
        )
        .with_args(args)
        .with_placement(self.active_workspace_public_id(), None);

        if let Err(error) = self.start_workflow_run(active, kvdag, graph) {
            return encode_error(id, error.code(), error.message());
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
        let workflow_id = WorkflowId::new(params.workflow_id.trim().to_string());
        let limit = params.limit.unwrap_or(DEFAULT_RUN_LIST_LIMIT).max(1);
        let listed = self
            .workflow_store
            .call(move |cx| cx.block_on(cx.store().list_runs(&workflow_id, limit)));
        match listed {
            Ok(Ok(runs)) => {
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
        self.apply_workflow_engine_input(input);
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
            Ok(Some((record, nodes)))
        });
        match loaded {
            Ok(Ok(Some((record, nodes)))) => {
                let graph = WorkflowRunGraph {
                    nodes: nodes.into_iter().map(wire_run_node_record).collect(),
                    // Run edges are a `RELATE` projection the Phase 1 read
                    // surface does not expose; the live graph carries them, and
                    // a finished run's node statuses already encode the result.
                    edges: Vec::new(),
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
    match cx.block_on(cx.store().load_version(head)) {
        Ok(kvdag) => Ok(Some(wire_version_summary(
            &kvdag,
            VersionOrigin::Authored,
            "",
        ))),
        // A workflow whose head version cannot be loaded is still listable;
        // dropping the whole list because one pointer is stale would hide every
        // healthy workflow behind one broken one.
        Err(StoreError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error),
    }
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
fn wire_version_summary(
    kvdag: &Kvdag,
    origin: VersionOrigin,
    change_summary: &str,
) -> KvdagVersionSummary {
    KvdagVersionSummary {
        version_id: kvdag.version_id.to_string(),
        workflow_id: kvdag.workflow_id.to_string(),
        version: kvdag.version,
        parent_version_id: kvdag.parent.as_ref().map(std::string::ToString::to_string),
        origin: wire_origin(origin),
        change_summary: change_summary.to_string(),
        spec_digest: kvdag.spec_digest.as_str().to_string(),
        max_depth: u32::from(kvdag.growth.max_depth),
        max_nodes: u32::from(kvdag.growth.max_nodes),
        created_at_unix_ms: 0,
    }
}

#[cfg(feature = "workflow")]
fn wire_version_detail(kvdag: &Kvdag) -> KvdagVersionDetail {
    KvdagVersionDetail {
        version_id: kvdag.version_id.to_string(),
        workflow_id: kvdag.workflow_id.to_string(),
        version: kvdag.version,
        parent_version_id: kvdag.parent.as_ref().map(std::string::ToString::to_string),
        origin: WorkflowVersionOrigin::Authored,
        change_summary: String::new(),
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
        created_at_unix_ms: 0,
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
        cwd: None,
        node_dir: None,
        started_at_unix_ms: None,
        ended_at_unix_ms: None,
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
}
