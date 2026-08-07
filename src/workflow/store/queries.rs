//! Read methods that project stored rows into domain-typed records.
//!
//! Every method here is additive: it only ever reads, never mutates. The
//! mutating surface lives in `mod.rs`.
//!
//! Most of these are a single range scan, because the key layout in `db.rs`
//! already stores rows in the order the caller wants them. The two that sort in
//! memory say so, and why.

use std::collections::BTreeMap;

use crate::workflow::model::{
    CheckpointKind, Demand, Evidence, InstancePath, KvdagVersionId, NodeKey, NodeStatus,
    RunEventKind, RunId, RunStatus, Succession, WorkflowId,
};
use crate::workflow::tier::Tier;

use super::db::{self, RowReader as _};
use super::records::{self, parse_record_id, record_id_to_string};
use super::{
    parse_demand, parse_evidence, parse_node_status, parse_run_status, parse_succession,
    StoreError, WorkflowStore, TABLE_KVDAG_VERSION, TABLE_RUN_NODE, TABLE_WORKFLOW,
    TABLE_WORKFLOW_RUN,
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
    pub evidence: Option<Evidence>,
    pub succession: Option<Succession>,
    pub total_tokens: u64,
    pub tool_uses: u32,
    pub duration_ms: u64,
}

/// One `run_event` row.
#[derive(Debug, Clone, PartialEq)]
pub struct RunEventRecord {
    pub seq: u64,
    pub at: String,
    pub kind: RunEventKind,
    /// The full `run_node:...` id, not resolved to an `InstancePath`: some
    /// events (e.g. `run_started`) have none, and resolving the rest would
    /// cost a lookup per row for a value most callers already know.
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
        let key = parse_record_id(TABLE_WORKFLOW, workflow.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow id: {workflow}")))?;
        let read = self.read()?;
        let table = read.open_table(db::WORKFLOW).map_err(db::storage_error)?;
        let row: Option<records::WorkflowRow> = db::get_row(&table, &key)?;
        row.map(workflow_summary).transpose()
    }

    /// Every workflow, by name. Workflow keys are creation-ordered rather than
    /// name-ordered, so this is the one listing that sorts in memory.
    pub async fn list_workflows(&self) -> Result<Vec<WorkflowSummary>, StoreError> {
        let read = self.read()?;
        let table = read.open_table(db::WORKFLOW).map_err(db::storage_error)?;
        let mut rows: Vec<records::WorkflowRow> = db::scan_prefix(&table, "")?;
        rows.sort_by(|left, right| left.name.cmp(&right.name));
        rows.into_iter().map(workflow_summary).collect()
    }

    /// Resolves a workflow-relative version *number* — which is what the wire
    /// carries — to the version's record id. The number is part of the version
    /// key, so this is a point lookup rather than a scan.
    pub async fn find_version_id(
        &self,
        workflow: &WorkflowId,
        version: u32,
    ) -> Result<Option<KvdagVersionId>, StoreError> {
        let workflow_key = parse_record_id(TABLE_WORKFLOW, workflow.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow id: {workflow}")))?;
        let key = db::version_key(&workflow_key, version);
        let read = self.read()?;
        let table = read
            .open_table(db::KVDAG_VERSION)
            .map_err(db::storage_error)?;
        if !table.row_exists(&key)? {
            return Ok(None);
        }
        Ok(Some(KvdagVersionId::new(record_id_to_string(
            TABLE_KVDAG_VERSION,
            &key,
        ))))
    }

    pub async fn get_run(&self, run: &RunId) -> Result<Option<RunRecord>, StoreError> {
        let key = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        self.select_run(&key)?.map(run_record).transpose()
    }

    /// Runs for a workflow, newest first (`03-storage-schema.md` §6). Run keys
    /// are creation-ordered, so "newest first" is the key range read backwards.
    pub async fn list_runs(
        &self,
        workflow: &WorkflowId,
        limit: u32,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let workflow_key = parse_record_id(TABLE_WORKFLOW, workflow.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow id: {workflow}")))?;
        let read = self.read()?;
        let table = read
            .open_table(db::WORKFLOW_RUN)
            .map_err(db::storage_error)?;
        let mut rows: Vec<records::RunRow> =
            db::scan_prefix(&table, &db::run_prefix(&workflow_key))?;
        rows.reverse();
        rows.truncate(limit as usize);
        rows.into_iter().map(run_record).collect()
    }

    /// Every `run_node` for a run, in scheduling order (`04` §3.1: depth, then
    /// instance path). Keys order by instance path alone, so depth is applied
    /// on top of that here.
    pub async fn list_run_nodes(&self, run: &RunId) -> Result<Vec<RunNodeRecord>, StoreError> {
        let key = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let read = self.read()?;
        let table = read.open_table(db::RUN_NODE).map_err(db::storage_error)?;
        let mut rows: Vec<records::RunNodeRow> = db::scan_prefix(&table, &db::child_prefix(&key))?;
        rows.sort_by(|left, right| {
            (left.depth, &left.instance_path).cmp(&(right.depth, &right.instance_path))
        });
        rows.into_iter().map(run_node_record).collect()
    }

    /// Replays a run's journal in order (`03-storage-schema.md` §6).
    pub async fn list_run_events(&self, run: &RunId) -> Result<Vec<RunEventRecord>, StoreError> {
        let key = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let read = self.read()?;
        let table = read.open_table(db::RUN_EVENT).map_err(db::storage_error)?;
        let rows: Vec<records::RunEventRow> = db::scan_prefix(&table, &db::child_prefix(&key))?;
        rows.into_iter().map(run_event_record).collect()
    }

    /// Every checkpoint for one node instance in one run, oldest first.
    pub async fn list_checkpoints(
        &self,
        run: &RunId,
        path: &InstancePath,
    ) -> Result<Vec<CheckpointRecord>, StoreError> {
        let key = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let read = self.read()?;
        let table = read
            .open_table(db::NODE_CHECKPOINT)
            .map_err(db::storage_error)?;
        let rows: Vec<records::CheckpointRow> =
            db::scan_prefix(&table, &db::checkpoint_prefix(&key, path.as_str()))?;
        rows.into_iter().map(checkpoint_record).collect()
    }

    /// Checkpoint restore source set (`03-storage-schema.md` §6): validated
    /// `result` checkpoints for the given node keys in one run.
    pub async fn find_restorable_checkpoints(
        &self,
        run: &RunId,
        node_keys: &[NodeKey],
    ) -> Result<Vec<CheckpointRecord>, StoreError> {
        let key = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let read = self.read()?;
        let table = read
            .open_table(db::NODE_CHECKPOINT)
            .map_err(db::storage_error)?;
        let mut rows: Vec<records::CheckpointRow> =
            db::scan_prefix(&table, &db::child_prefix(&key))?;
        rows.retain(|row| {
            row.kind == "result"
                && row.schema_valid
                && node_keys.iter().any(|key| key.as_str() == row.node_key)
        });
        // Checkpoints are keyed by instance path, and one node key can have
        // several instances, so node-key order is not the key order.
        rows.sort_by(|left, right| (&left.node_key, left.seq).cmp(&(&right.node_key, right.seq)));
        rows.into_iter().map(checkpoint_record).collect()
    }

    pub async fn get_run_summary(
        &self,
        run: &RunId,
    ) -> Result<Option<RunSummaryRecord>, StoreError> {
        let key = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let read = self.read()?;
        let table = read
            .open_table(db::RUN_SUMMARY)
            .map_err(db::storage_error)?;
        let row: Option<records::RunSummaryRow> = db::get_row(&table, &key)?;
        row.map(run_summary_record).transpose()
    }
}

fn workflow_summary(row: records::WorkflowRow) -> Result<WorkflowSummary, StoreError> {
    Ok(WorkflowSummary {
        id: WorkflowId::new(record_id_to_string(TABLE_WORKFLOW, &row.id)),
        name: row.name,
        description: row.description,
        head_version: row
            .head_version
            .as_deref()
            .map(|key| KvdagVersionId::new(record_id_to_string(TABLE_KVDAG_VERSION, key))),
        default_tier: Tier::parse(&row.default_tier)
            .ok_or_else(|| StoreError::Decode(format!("unknown tier {:?}", row.default_tier)))?,
        archived: row.archived,
        created_at_unix_ms: unix_ms(row.created_at),
        updated_at_unix_ms: unix_ms(row.updated_at),
    })
}

/// Rows carry signed unix milliseconds; the wire carries unsigned, so a
/// pre-epoch timestamp (which this store never writes) clamps to 0 rather than
/// wrapping.
fn unix_ms(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
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
        id: RunId::new(record_id_to_string(TABLE_WORKFLOW_RUN, &row.id)),
        workflow: WorkflowId::new(record_id_to_string(TABLE_WORKFLOW, &row.workflow)),
        version: KvdagVersionId::new(record_id_to_string(TABLE_KVDAG_VERSION, &row.kvdag_version)),
        tier: Tier::parse(&row.tier)
            .ok_or_else(|| StoreError::Decode(format!("unknown tier {:?}", row.tier)))?,
        status: parse_run_status(&row.status)?,
        args,
        context_runs: row
            .context_runs
            .iter()
            .map(|key| RunId::new(record_id_to_string(TABLE_WORKFLOW_RUN, key)))
            .collect(),
        max_depth: row.max_depth as u16,
        max_nodes: row.max_nodes as u16,
        workspace_id: row.workspace_id,
        tab_id: row.tab_id,
        nodes_total: row.nodes_total as u32,
        nodes_done: row.nodes_done as u32,
        total_tokens: row.total_tokens as u64,
        total_tool_uses: row.total_tool_uses as u32,
        started_at_unix_ms: unix_ms(row.started_at),
        ended_at_unix_ms: row.ended_at.map(unix_ms),
        failure: row.failure,
    })
}

fn run_node_record(row: records::RunNodeRow) -> Result<RunNodeRecord, StoreError> {
    Ok(RunNodeRecord {
        run: RunId::new(record_id_to_string(TABLE_WORKFLOW_RUN, &row.run)),
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
        evidence: row.evidence.as_deref().map(parse_evidence).transpose()?,
        succession: parse_succession(row.succession.as_deref(), row.blocker.as_ref())?,
        total_tokens: row.total_tokens as u64,
        tool_uses: row.tool_uses as u32,
        duration_ms: row.duration_ms as u64,
    })
}

fn run_event_record(row: records::RunEventRow) -> Result<RunEventRecord, StoreError> {
    Ok(RunEventRecord {
        seq: row.seq as u64,
        at: db::rfc3339_utc(row.at),
        kind: parse_run_event_kind(&row.kind)?,
        run_node: row
            .run_node
            .as_deref()
            .map(|key| record_id_to_string(TABLE_RUN_NODE, key)),
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
        run: RunId::new(record_id_to_string(TABLE_WORKFLOW_RUN, &row.run)),
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
