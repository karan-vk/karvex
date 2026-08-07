//! Read methods that project stored rows into domain-typed records.
//!
//! Every method here is additive: it only ever runs `SELECT`, never mutates.
//! The mutating surface lives in `mod.rs`.

use std::collections::BTreeMap;

use crate::workflow::model::{
    CheckpointKind, Demand, EdgeKind, Evidence, InstancePath, KvdagVersionId, NodeKey, NodeStatus,
    RunEventKind, RunId, RunStatus, Succession, WorkflowId,
};
use crate::workflow::tier::Tier;

use super::records::{self, parse_record_id, record_id_to_string};
use super::{
    parse_demand, parse_edge_kind, parse_evidence, parse_node_status, parse_run_status,
    parse_succession, query_error, StoreError, VersionOrigin, WorkflowStore, TABLE_KVDAG_VERSION,
    TABLE_WORKFLOW, TABLE_WORKFLOW_RUN,
};

/// One `workflow` row, projected for listing.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowSummary {
    pub id: WorkflowId,
    pub name: String,
    pub description: String,
    pub head_version: Option<KvdagVersionId>,
    pub default_tier: Tier,
    pub archived: bool,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

/// One `kvdag_version` row's metadata — everything about a version except its
/// nodes/edges, which `load_version` still owns because the engine needs the
/// full validated [`crate::workflow::model::Kvdag`]. Cheap enough to list a
/// workflow's whole version chain in one query (`workflow.get`'s `versions`).
#[derive(Debug, Clone, PartialEq)]
pub struct VersionRecord {
    pub version_id: KvdagVersionId,
    pub workflow: WorkflowId,
    pub version: u32,
    pub parent_version_id: Option<KvdagVersionId>,
    pub origin: VersionOrigin,
    pub change_summary: String,
    pub spec_digest: String,
    pub max_depth: u16,
    pub max_nodes: u16,
    pub created_at_unix_ms: u64,
}

/// One `workflow_run` row.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    pub id: RunId,
    pub workflow: WorkflowId,
    pub version: KvdagVersionId,
    pub tier: Tier,
    pub status: RunStatus,
    pub args: BTreeMap<String, String>,
    pub context_runs: Vec<RunId>,
    pub max_depth: u16,
    pub max_nodes: u16,
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub nodes_total: u32,
    pub nodes_done: u32,
    pub total_tokens: u64,
    pub total_tool_uses: u32,
    pub started_at_unix_ms: u64,
    pub ended_at_unix_ms: Option<u64>,
    pub failure: Option<serde_json::Value>,
}

/// One `run_node` row.
#[derive(Debug, Clone, PartialEq)]
pub struct RunNodeRecord {
    pub run: RunId,
    pub node_key: NodeKey,
    pub instance_path: InstancePath,
    pub depth: u16,
    pub status: NodeStatus,
    pub model: String,
    pub effort: String,
    /// The node's declared demand, stored on the row rather than re-derived
    /// from the kvdag: a run answered from the journal has no live definition
    /// to consult, and reporting every node as `Standard` would misreport it.
    pub demand: Demand,
    pub attempt: u8,
    pub pane_id: Option<String>,
    pub terminal_id: Option<String>,
    pub agent_session_id: Option<String>,
    /// The node's on-disk binding, written alongside the pane binding
    /// (`03-storage-schema.md` §4.2). Without these a restored node cannot be
    /// traced back to the `task.md`, `inputs/`, and `artifacts/` it produced.
    pub cwd: Option<String>,
    pub node_dir: Option<String>,
    pub evidence: Option<Evidence>,
    pub succession: Option<Succession>,
    pub total_tokens: u64,
    pub tool_uses: u32,
    pub duration_ms: u64,
    pub started_at_unix_ms: Option<u64>,
    pub ended_at_unix_ms: Option<u64>,
}

/// One `run_edge` relation, with both endpoints resolved to the instance paths
/// the rest of the run surface addresses nodes by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEdgeRecord {
    pub from: InstancePath,
    pub to: InstancePath,
    pub kind: EdgeKind,
    pub condition_result: Option<bool>,
    /// Whether the edge has been recorded as fired, i.e. whether `fired_at` is
    /// set. Written by `StoreWrite::RunEdge` every time the scheduler settles
    /// the edge, so a restored run reports the branches it actually took.
    pub fired: bool,
}

/// One `run_event` row.
#[derive(Debug, Clone, PartialEq)]
pub struct RunEventRecord {
    pub seq: u64,
    pub at: String,
    pub kind: RunEventKind,
    /// The full `run_node:...` id, not resolved to an `InstancePath`: some
    /// events (e.g. `run_started`) have none, and resolving the rest would
    /// cost a join per row for a value most callers already know.
    pub run_node: Option<String>,
    pub payload: serde_json::Value,
}

/// One `node_checkpoint` row.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointRecord {
    pub node_key: NodeKey,
    pub instance_path: InstancePath,
    pub seq: u64,
    pub kind: CheckpointKind,
    pub schema_valid: bool,
    pub payload: serde_json::Value,
    pub summary: String,
    pub artifact_paths: Vec<String>,
    pub digest: String,
}

/// One `run_summary` row.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSummaryRecord {
    pub run: RunId,
    pub text: String,
    pub outcome: String,
    pub highlights: Vec<String>,
    pub open_gaps: Vec<String>,
    pub token_estimate: u32,
}

impl WorkflowStore {
    pub async fn get_workflow(
        &self,
        workflow: &WorkflowId,
    ) -> Result<Option<WorkflowSummary>, StoreError> {
        let id = parse_record_id(TABLE_WORKFLOW, workflow.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow id: {workflow}")))?;
        let mut response = self
            .db
            .query("SELECT * FROM $id")
            .bind(("id", id))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::WorkflowRow> = response.take(0).map_err(query_error)?;
        rows.into_iter().next().map(workflow_summary).transpose()
    }

    pub async fn list_workflows(&self) -> Result<Vec<WorkflowSummary>, StoreError> {
        let mut response = self
            .db
            .query("SELECT * FROM workflow ORDER BY name")
            .await
            .map_err(query_error)?;
        let rows: Vec<records::WorkflowRow> = response.take(0).map_err(query_error)?;
        rows.into_iter().map(workflow_summary).collect()
    }

    /// Resolves a workflow-relative version *number* — which is what the wire
    /// carries — to the version's record id.
    pub async fn find_version_id(
        &self,
        workflow: &WorkflowId,
        version: u32,
    ) -> Result<Option<KvdagVersionId>, StoreError> {
        let id = parse_record_id(TABLE_WORKFLOW, workflow.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow id: {workflow}")))?;
        let mut response = self
            .db
            .query(
                "SELECT * FROM kvdag_version WHERE workflow = $workflow \
                 AND version = $version LIMIT 1",
            )
            .bind(("workflow", id))
            .bind(("version", i64::from(version)))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::KvdagVersionRow> = response.take(0).map_err(query_error)?;
        Ok(rows
            .into_iter()
            .next()
            .map(|row| KvdagVersionId::new(record_id_to_string(&row.id))))
    }

    /// Every workflow whose name matches exactly. `workflow_name` carries a
    /// UNIQUE index (`migrations/0001_init.surql`), so at most one row can
    /// ever come back — callers that resolve a `<name|id>` selector still
    /// treat more than one as ambiguous rather than assuming the constraint
    /// holds.
    pub async fn find_workflows_by_name(
        &self,
        name: &str,
    ) -> Result<Vec<WorkflowSummary>, StoreError> {
        let mut response = self
            .db
            .query("SELECT * FROM workflow WHERE name = $name")
            .bind(("name", name.to_string()))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::WorkflowRow> = response.take(0).map_err(query_error)?;
        rows.into_iter().map(workflow_summary).collect()
    }

    /// One version's metadata (origin/change_summary/created_at/digest/growth),
    /// without loading its nodes or edges.
    pub async fn get_version_record(
        &self,
        version: &KvdagVersionId,
    ) -> Result<Option<VersionRecord>, StoreError> {
        let id = parse_record_id(TABLE_KVDAG_VERSION, version.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a kvdag_version id: {version}")))?;
        let mut response = self
            .db
            .query("SELECT * FROM $id")
            .bind(("id", id))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::KvdagVersionRow> = response.take(0).map_err(query_error)?;
        rows.into_iter().next().map(version_record).transpose()
    }

    /// A workflow's version chain, walking the immutable `parent` link from
    /// `head` back to the root version. `kvdag_version` rows form a tree in
    /// general — an explicit non-linear `parent` override is reserved for a
    /// future, currently-unused origin — so this walks the ancestry `head`
    /// actually points through rather than returning every row that has ever
    /// existed for the workflow; an abandoned branch is not part of the
    /// observable history of the current head. Newest (`head`) first.
    pub async fn list_version_chain(
        &self,
        workflow: &WorkflowId,
        head: Option<&KvdagVersionId>,
    ) -> Result<Vec<VersionRecord>, StoreError> {
        let Some(head) = head else {
            return Ok(Vec::new());
        };
        let workflow_id = parse_record_id(TABLE_WORKFLOW, workflow.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow id: {workflow}")))?;
        let mut response = self
            .db
            .query("SELECT * FROM kvdag_version WHERE workflow = $workflow")
            .bind(("workflow", workflow_id))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::KvdagVersionRow> = response.take(0).map_err(query_error)?;
        let mut by_id: BTreeMap<String, records::KvdagVersionRow> = rows
            .into_iter()
            .map(|row| (record_id_to_string(&row.id), row))
            .collect();

        let mut chain = Vec::new();
        let mut cursor = Some(head.to_string());
        while let Some(current) = cursor {
            let Some(row) = by_id.remove(&current) else {
                // The head (or an ancestor) is not among this workflow's own
                // rows — a stale pointer, not a reason to fail the whole
                // chain read.
                break;
            };
            cursor = row.parent.as_ref().map(record_id_to_string);
            chain.push(version_record(row)?);
        }
        Ok(chain)
    }

    pub async fn get_run(&self, run: &RunId) -> Result<Option<RunRecord>, StoreError> {
        let id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let mut response = self
            .db
            .query("SELECT * FROM $id")
            .bind(("id", id))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::RunRow> = response.take(0).map_err(query_error)?;
        rows.into_iter().next().map(run_record).transpose()
    }

    /// Runs for a workflow, newest first (`03-storage-schema.md` §6).
    pub async fn list_runs(
        &self,
        workflow: &WorkflowId,
        limit: u32,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let id = parse_record_id(TABLE_WORKFLOW, workflow.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow id: {workflow}")))?;
        let mut response = self
            .db
            .query(
                "SELECT * FROM workflow_run WHERE workflow = $workflow \
                 ORDER BY started_at DESC LIMIT $limit",
            )
            .bind(("workflow", id))
            .bind(("limit", i64::from(limit)))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::RunRow> = response.take(0).map_err(query_error)?;
        rows.into_iter().map(run_record).collect()
    }

    /// Every `run_node` for a run, in scheduling order (`04` §3.1: depth, then
    /// instance path).
    pub async fn list_run_nodes(&self, run: &RunId) -> Result<Vec<RunNodeRecord>, StoreError> {
        let id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let mut response = self
            .db
            .query("SELECT * FROM run_node WHERE run = $run ORDER BY depth, instance_path")
            .bind(("run", id))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::RunNodeRow> = response.take(0).map_err(query_error)?;
        rows.into_iter().map(run_node_record).collect()
    }

    /// Every `run_edge` for a run, with both endpoints resolved to instance
    /// paths.
    ///
    /// `run_edge` is a `RELATE` whose `in`/`out` are `run_node` record ids, so
    /// the endpoints are resolved through this run's own `run_node` rows rather
    /// than with a graph traversal in the query: the node set is already needed
    /// beside the edges by every caller, and one extra `SELECT` per run is
    /// cheaper than a per-edge join. An edge whose endpoint is missing (a
    /// partially pruned run) is dropped rather than failing the whole read —
    /// half a topology is still more useful than none, and `prune_run_history`
    /// only ever removes whole runs.
    ///
    /// Ordered by `(from, to, kind)` so a restored graph is stable across
    /// reads.
    pub async fn list_run_edges(&self, run: &RunId) -> Result<Vec<RunEdgeRecord>, StoreError> {
        let id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let mut response = self
            .db
            .query("SELECT * FROM run_node WHERE run = $run")
            .bind(("run", id.clone()))
            .await
            .map_err(query_error)?;
        let node_rows: Vec<records::RunNodeRow> = response.take(0).map_err(query_error)?;
        let path_by_id: BTreeMap<String, InstancePath> = node_rows
            .into_iter()
            .map(|row| {
                (
                    record_id_to_string(&row.id),
                    InstancePath::new(row.instance_path),
                )
            })
            .collect();

        let mut response = self
            .db
            .query("SELECT * FROM run_edge WHERE run = $run")
            .bind(("run", id))
            .await
            .map_err(query_error)?;
        let edge_rows: Vec<records::RunEdgeRow> = response.take(0).map_err(query_error)?;

        let mut edges = Vec::with_capacity(edge_rows.len());
        for row in edge_rows {
            let (Some(from), Some(to)) = (
                path_by_id.get(&record_id_to_string(&row.r#in)).cloned(),
                path_by_id.get(&record_id_to_string(&row.out)).cloned(),
            ) else {
                continue;
            };
            edges.push(RunEdgeRecord {
                from,
                to,
                kind: parse_edge_kind(&row.kind)?,
                condition_result: row.condition_result,
                fired: row.fired_at.is_some(),
            });
        }
        edges.sort_by(|left, right| {
            left.from
                .cmp(&right.from)
                .then_with(|| left.to.cmp(&right.to))
                .then_with(|| edge_kind_order(left.kind).cmp(&edge_kind_order(right.kind)))
        });
        Ok(edges)
    }

    /// Replays a run's journal in order (`03-storage-schema.md` §6).
    pub async fn list_run_events(&self, run: &RunId) -> Result<Vec<RunEventRecord>, StoreError> {
        let id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let mut response = self
            .db
            .query("SELECT * FROM run_event WHERE run = $run ORDER BY seq")
            .bind(("run", id))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::RunEventRow> = response.take(0).map_err(query_error)?;
        rows.into_iter().map(run_event_record).collect()
    }

    /// Every checkpoint for one node instance in one run, oldest first.
    pub async fn list_checkpoints(
        &self,
        run: &RunId,
        path: &InstancePath,
    ) -> Result<Vec<CheckpointRecord>, StoreError> {
        let id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let mut response = self
            .db
            .query(
                "SELECT * FROM node_checkpoint WHERE run = $run AND instance_path = $path \
                 ORDER BY seq",
            )
            .bind(("run", id))
            .bind(("path", path.to_string()))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::CheckpointRow> = response.take(0).map_err(query_error)?;
        rows.into_iter().map(checkpoint_record).collect()
    }

    /// Checkpoint restore source set (`03-storage-schema.md` §6): validated
    /// `result` checkpoints for the given node keys in one run.
    pub async fn find_restorable_checkpoints(
        &self,
        run: &RunId,
        node_keys: &[NodeKey],
    ) -> Result<Vec<CheckpointRecord>, StoreError> {
        let id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let selectors: Vec<String> = node_keys.iter().map(NodeKey::to_string).collect();
        let mut response = self
            .db
            .query(
                "SELECT * FROM node_checkpoint WHERE run = $run AND kind = \"result\" \
                 AND schema_valid = true AND node_key IN $selectors ORDER BY node_key, seq",
            )
            .bind(("run", id))
            .bind(("selectors", selectors))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::CheckpointRow> = response.take(0).map_err(query_error)?;
        rows.into_iter().map(checkpoint_record).collect()
    }

    pub async fn get_run_summary(
        &self,
        run: &RunId,
    ) -> Result<Option<RunSummaryRecord>, StoreError> {
        let id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let mut response = self
            .db
            .query("SELECT * FROM run_summary WHERE run = $run LIMIT 1")
            .bind(("run", id))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::RunSummaryRow> = response.take(0).map_err(query_error)?;
        rows.into_iter().next().map(run_summary_record).transpose()
    }
}

fn workflow_summary(row: records::WorkflowRow) -> Result<WorkflowSummary, StoreError> {
    Ok(WorkflowSummary {
        id: WorkflowId::new(record_id_to_string(&row.id)),
        name: row.name,
        description: row.description,
        head_version: row
            .head_version
            .as_ref()
            .map(record_id_to_string)
            .map(KvdagVersionId::new),
        default_tier: Tier::parse(&row.default_tier)
            .ok_or_else(|| StoreError::Decode(format!("unknown tier {:?}", row.default_tier)))?,
        archived: row.archived,
        created_at_unix_ms: unix_ms(&row.created_at),
        updated_at_unix_ms: unix_ms(&row.updated_at),
    })
}

/// `surrealdb_types::Datetime` derefs to `chrono::DateTime<Utc>`; the wire
/// carries unsigned epoch milliseconds, so a pre-epoch timestamp (which this
/// store never writes) clamps to 0 rather than wrapping.
fn unix_ms(value: &surrealdb_types::Datetime) -> u64 {
    u64::try_from(value.timestamp_millis()).unwrap_or(0)
}

fn version_record(row: records::KvdagVersionRow) -> Result<VersionRecord, StoreError> {
    Ok(VersionRecord {
        version_id: KvdagVersionId::new(record_id_to_string(&row.id)),
        workflow: WorkflowId::new(record_id_to_string(&row.workflow)),
        version: row.version as u32,
        parent_version_id: row
            .parent
            .as_ref()
            .map(record_id_to_string)
            .map(KvdagVersionId::new),
        origin: VersionOrigin::parse(&row.origin)?,
        change_summary: row.change_summary,
        spec_digest: row.spec_digest,
        max_depth: row.max_depth as u16,
        max_nodes: row.max_nodes as u16,
        created_at_unix_ms: unix_ms(&row.created_at),
    })
}

fn run_record(row: records::RunRow) -> Result<RunRecord, StoreError> {
    let args = row
        .args
        .as_object()
        .map(|object| {
            object
                .iter()
                .map(|(key, value)| {
                    let value = value.as_str().map(str::to_string).unwrap_or_default();
                    (key.clone(), value)
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(RunRecord {
        id: RunId::new(record_id_to_string(&row.id)),
        workflow: WorkflowId::new(record_id_to_string(&row.workflow)),
        version: KvdagVersionId::new(record_id_to_string(&row.kvdag_version)),
        tier: Tier::parse(&row.tier)
            .ok_or_else(|| StoreError::Decode(format!("unknown tier {:?}", row.tier)))?,
        status: parse_run_status(&row.status)?,
        args,
        context_runs: row
            .context_runs
            .iter()
            .map(|id| RunId::new(record_id_to_string(id)))
            .collect(),
        max_depth: row.max_depth as u16,
        max_nodes: row.max_nodes as u16,
        workspace_id: row.workspace_id,
        tab_id: row.tab_id,
        nodes_total: row.nodes_total as u32,
        nodes_done: row.nodes_done as u32,
        total_tokens: row.total_tokens as u64,
        total_tool_uses: row.total_tool_uses as u32,
        started_at_unix_ms: unix_ms(&row.started_at),
        ended_at_unix_ms: row.ended_at.as_ref().map(unix_ms),
        failure: row.failure,
    })
}

fn run_node_record(row: records::RunNodeRow) -> Result<RunNodeRecord, StoreError> {
    Ok(RunNodeRecord {
        run: RunId::new(record_id_to_string(&row.run)),
        node_key: NodeKey::new(row.node_key),
        instance_path: InstancePath::new(row.instance_path),
        depth: row.depth as u16,
        status: parse_node_status(&row.status)?,
        model: row.model,
        effort: row.effort,
        demand: parse_demand(&row.demand)?,
        attempt: row.attempt as u8,
        pane_id: row.pane_id,
        terminal_id: row.terminal_id,
        agent_session_id: row.agent_session_id,
        cwd: row.cwd,
        node_dir: row.node_dir,
        evidence: row.evidence.as_deref().map(parse_evidence).transpose()?,
        succession: parse_succession(row.succession.as_deref(), row.blocker.as_ref())?,
        total_tokens: row.total_tokens as u64,
        tool_uses: row.tool_uses as u32,
        duration_ms: row.duration_ms as u64,
        started_at_unix_ms: row.started_at.as_ref().map(unix_ms),
        ended_at_unix_ms: row.ended_at.as_ref().map(unix_ms),
    })
}

/// A total order over [`EdgeKind`], which is a plain tag with no meaningful
/// ordering of its own; used only to break ties between two edges that share
/// both endpoints.
fn edge_kind_order(kind: EdgeKind) -> u8 {
    match kind {
        EdgeKind::Sequence => 0,
        EdgeKind::Data => 1,
        EdgeKind::Conditional => 2,
    }
}

fn run_event_record(row: records::RunEventRow) -> Result<RunEventRecord, StoreError> {
    Ok(RunEventRecord {
        seq: row.seq as u64,
        at: row.at.to_string(),
        kind: parse_run_event_kind(&row.kind)?,
        run_node: row.run_node.as_ref().map(record_id_to_string),
        payload: row.payload,
    })
}

fn checkpoint_record(row: records::CheckpointRow) -> Result<CheckpointRecord, StoreError> {
    Ok(CheckpointRecord {
        node_key: NodeKey::new(row.node_key),
        instance_path: InstancePath::new(row.instance_path),
        seq: row.seq as u64,
        kind: parse_checkpoint_kind(&row.kind)?,
        schema_valid: row.schema_valid,
        payload: row.payload,
        summary: row.summary,
        artifact_paths: row.artifact_paths,
        digest: row.digest,
    })
}

fn run_summary_record(row: records::RunSummaryRow) -> Result<RunSummaryRecord, StoreError> {
    Ok(RunSummaryRecord {
        run: RunId::new(record_id_to_string(&row.run)),
        text: row.text,
        outcome: row.outcome,
        highlights: row.highlights,
        open_gaps: row.open_gaps,
        token_estimate: row.token_estimate as u32,
    })
}

fn parse_run_event_kind(value: &str) -> Result<RunEventKind, StoreError> {
    match value {
        "run_started" => Ok(RunEventKind::RunStarted),
        "run_finished" => Ok(RunEventKind::RunFinished),
        "node_created" => Ok(RunEventKind::NodeCreated),
        "node_started" => Ok(RunEventKind::NodeStarted),
        "node_status" => Ok(RunEventKind::NodeStatus),
        "node_output" => Ok(RunEventKind::NodeOutput),
        "tool_activity" => Ok(RunEventKind::ToolActivity),
        "plan" => Ok(RunEventKind::Plan),
        "usage" => Ok(RunEventKind::Usage),
        "message_delivered" => Ok(RunEventKind::MessageDelivered),
        "steer" => Ok(RunEventKind::Steer),
        "interrupt" => Ok(RunEventKind::Interrupt),
        "expand_proposed" => Ok(RunEventKind::ExpandProposed),
        "expand_accepted" => Ok(RunEventKind::ExpandAccepted),
        "expand_rejected" => Ok(RunEventKind::ExpandRejected),
        "growth_limited" => Ok(RunEventKind::GrowthLimited),
        "watchdog" => Ok(RunEventKind::Watchdog),
        "checkpoint" => Ok(RunEventKind::Checkpoint),
        "succession" => Ok(RunEventKind::Succession),
        "error" => Ok(RunEventKind::Error),
        other => Err(StoreError::Decode(format!(
            "unknown run event kind {other:?}"
        ))),
    }
}

fn parse_checkpoint_kind(value: &str) -> Result<CheckpointKind, StoreError> {
    match value {
        "result" => Ok(CheckpointKind::Result),
        "partial" => Ok(CheckpointKind::Partial),
        "artifact_index" => Ok(CheckpointKind::ArtifactIndex),
        other => Err(StoreError::Decode(format!(
            "unknown checkpoint kind {other:?}"
        ))),
    }
}
