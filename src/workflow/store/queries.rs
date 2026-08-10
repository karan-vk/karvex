//! Read methods that project stored rows into domain-typed records.
//!
//! Every method here is additive: it only ever runs `SELECT`, never mutates.
//! The mutating surface lives in `mod.rs`.

use std::collections::BTreeMap;

use crate::workflow::model::{
    CheckpointKind, Demand, EdgeKind, Evidence, InstancePath, InterrogationId, KvdagVersionId,
    NodeKey, NodeStatus, RestoredRef, RunEventKind, RunId, RunStatus, Succession, SummaryNodeLine,
    WorkflowId,
};
use crate::workflow::tier::{NodeHistory, Tier};

use super::records::{self, parse_record_id, record_id_to_string};
use super::{
    node_status_str, parse_demand, parse_edge_kind, parse_evidence, parse_node_status,
    parse_run_status, parse_succession, query_error, run_status_str, StoreError, VersionOrigin,
    WorkflowStore, TABLE_KVDAG_VERSION, TABLE_WORKFLOW, TABLE_WORKFLOW_RUN,
};

/// How many of a workflow's most recent closed runs
/// [`WorkflowStore::node_history`] measures by default — §7.3's "last N runs".
/// Ten is enough for §7.3 step 3's "≥ 3 prior runs" clause to have settled and
/// short enough that a node's recent behaviour is not diluted by a graph shape
/// it no longer has.
pub const DEFAULT_NODE_HISTORY_RUNS: usize = 10;

/// The `workflow_run.status` values that close a run. A run still in flight is
/// not a measurement.
const CLOSED_RUN_STATUSES: &[RunStatus] = &[
    RunStatus::Succeeded,
    RunStatus::Failed,
    RunStatus::Cancelled,
];

/// The `run_node.status` values that count as one measured execution of a node.
///
/// Deliberately narrower than the store's terminal set: `skipped`, `cancelled`,
/// and `restored` close a node without the node having run, and counting them
/// would read as first-pass failures — inventing evidence of poor performance
/// out of a dead branch.
const MEASURED_NODE_STATUSES: &[NodeStatus] = &[NodeStatus::Succeeded, NodeStatus::Failed];

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
    /// Denormalised alongside `workflow` so a cross-workflow listing
    /// (`Self::workflow` is `Option`-filtered by [`WorkflowStore::list_runs`])
    /// can label a row without an extra call per run (`07-phase3-plan.md` §4
    /// D9). Resolved by one batched name lookup, never a per-row join.
    pub workflow_name: String,
    pub version: KvdagVersionId,
    pub tier: Tier,
    pub status: RunStatus,
    pub args: BTreeMap<String, String>,
    pub context_runs: Vec<RunId>,
    /// The run this one restores from, if any (`07-phase3-plan.md` §4 D4).
    pub restore_from_run: Option<RunId>,
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
    /// The Claude Code session this run's team lead is, and what
    /// `claude --resume` takes (`09-agent-teams-rework.md` §3.1, §3.7).
    /// `None` for every pre-rework run, which karvex executed itself and which
    /// never had a lead — not a missing value, a truthful one.
    pub lead_session_id: Option<String>,
    /// The team name derived from the lead session id; it addresses
    /// `~/.claude/tasks/<team>/` and `~/.claude/teams/<team>/`.
    pub team_name: Option<String>,
    /// karvex's own public handles on the lead's pane, so "focus the lead"
    /// survives a server restart.
    pub lead_pane_id: Option<String>,
    pub lead_terminal_id: Option<String>,
    /// Which revision of the render contract (§3.2) produced the prompt this
    /// lead was launched with.
    pub lead_prompt_version: Option<u32>,
}

/// One `run_node` row.
#[derive(Debug, Clone, PartialEq)]
pub struct RunNodeRecord {
    pub run: RunId,
    pub node_key: NodeKey,
    pub instance_path: InstancePath,
    /// What this instance is called: the authored kvdag label for a static
    /// node, the proposing node's `--label` for an expansion child. Read off
    /// the row rather than joined back to the definition, which can only ever
    /// answer with the template's name — the same name for every sibling of a
    /// generation.
    pub label: String,
    /// The accepted `--input k=v` slot overrides this instance was created
    /// with; empty for a static node.
    pub inputs: BTreeMap<String, String>,
    /// Spawn provenance, resolved from `run_node.parent` against this run's
    /// own rows: `Some` for an expansion child, `None` for a static node (or
    /// for a `parent` that does not resolve within this run, which should not
    /// happen today but is read as "no provenance" rather than an error).
    pub parent_path: Option<InstancePath>,
    pub depth: u16,
    pub status: NodeStatus,
    pub model: String,
    pub effort: String,
    /// The node's declared demand, stored on the row rather than re-derived
    /// from the kvdag: a run answered from the journal has no live definition
    /// to consult, and reporting every node as `Standard` would misreport it.
    pub demand: Demand,
    pub attempt: u8,
    /// Why [`Self::model`]/[`Self::effort`] read what they read — the §7.3
    /// reason string for `auto`, empty for a fixed tier. Written verbatim from
    /// the run's assignment table, so a finished run can still be explained
    /// (`06-phase2-plan.md` §4 D9).
    pub assignment_reason: String,
    pub pane_id: Option<String>,
    pub terminal_id: Option<String>,
    pub agent_session_id: Option<String>,
    /// The node's on-disk binding, written alongside the pane binding
    /// (`03-storage-schema.md` §4.2). Without these a restored node cannot be
    /// traced back to the `task.md`, `inputs/`, and `artifacts/` it produced.
    pub cwd: Option<String>,
    pub node_dir: Option<String>,
    /// The pane's session-reported transcript path (§4 D6), or the pre-launch
    /// estimate for a session that never reported one — the writer has always
    /// persisted this; this field only stops the reader from dropping it (the
    /// M2 fix, `07-phase3-plan.md` §1 WS-B).
    pub transcript_path: Option<String>,
    pub evidence: Option<Evidence>,
    pub succession: Option<Succession>,
    pub total_tokens: u64,
    pub tool_uses: u32,
    pub duration_ms: u64,
    pub started_at_unix_ms: Option<u64>,
    pub ended_at_unix_ms: Option<u64>,
    /// Set only for a node seeded by restore; `None` for every node this run
    /// actually executed (`07-phase3-plan.md` §4 D4).
    pub restored_from: Option<RestoredRef>,
    /// The projected Claude Code task id, e.g. `"7"` (§3.4). `None` for a
    /// planned node whose task the lead has not created yet — which is what
    /// makes "planned but never started" readable off the row.
    pub task_id: Option<String>,
    /// The observed task subject, verbatim. Distinct from [`Self::label`] on
    /// purpose: the lead may reword, split, or merge tasks, so the definition's
    /// name and the name the team worked under are two separate facts.
    pub subject: String,
    /// The claiming teammate's name; empty means unclaimed, which is a real
    /// state in the source data rather than a missing value.
    pub owner: String,
    /// A task the definition never planned. The drift is the record — §3.7's
    /// `workflow capture` promotes it back into a definition later.
    pub emergent: bool,
}

/// One `run_member` row: a member of the run's Claude Code team as the
/// projection last saw it (`09-agent-teams-rework.md` §3.4).
///
/// A snapshot, not a journal entry. `first_seen_at`/`last_seen_at` bracket the
/// window the member was visible in the team config; a member that vanishes
/// (or a whole config that Claude Code deletes at session end) keeps its row
/// and simply stops advancing `last_seen_at`, because these rows are the only
/// durable record that the run had teammates at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunMemberRecord {
    pub run: RunId,
    pub name: String,
    pub agent_type: String,
    pub model: String,
    /// The team config's `tmuxPaneId` — a karvex public pane id, handed back
    /// through Claude Code's team state. `None` for the in-process lead.
    pub pane_id: Option<String>,
    /// `tmux` for a split-pane teammate, `in-process` for the lead; it is what
    /// decides whether a member is separately resumable (§3.7).
    pub backend_type: String,
    pub is_active: bool,
    pub cwd: Option<String>,
    pub first_seen_at_unix_ms: u64,
    pub last_seen_at_unix_ms: u64,
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

/// One node's restorable checkpoint plus its source node's compatibility
/// digests (§3 rule 5, §4 D11), so the caller — the restore handler, WS-D —
/// can compare against the target version's node without a second store
/// round-trip per selector.
#[derive(Debug, Clone, PartialEq)]
pub struct RestorableCheckpoint {
    pub checkpoint: CheckpointRecord,
    pub prompt_digest: String,
    pub schema_digest: String,
}

/// One `interrogation` row, resolved to the source node's instance path
/// (`07-phase3-plan.md` §4 D8: an interrogation is not a run node, but it
/// names the source node it revived).
#[derive(Debug, Clone, PartialEq)]
pub struct InterrogationRecord {
    pub id: InterrogationId,
    pub path: InstancePath,
    pub source_session_id: String,
    pub forked_session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub cwd: String,
    pub pane_id: Option<String>,
    pub reconstructed: bool,
    pub note: String,
    pub started_at_unix_ms: u64,
    pub ended_at_unix_ms: Option<u64>,
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

/// One `run_summary` row, resolved for the wire's `WorkflowRunSummaryInfo`
/// (`07-phase3-plan.md` §WS-C): workflow identity, the summary body, and
/// whether the source run has since been pruned.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSummaryRecord {
    pub run: RunId,
    pub workflow: WorkflowId,
    /// Batched-resolved, same reasoning as `RunRecord::workflow_name`: a
    /// pruned run's summary is exactly the row the browser most needs
    /// labelled without an extra call.
    pub workflow_name: String,
    pub version: KvdagVersionId,
    pub text: String,
    pub outcome: String,
    pub highlights: Vec<String>,
    pub open_gaps: Vec<String>,
    pub per_node: Vec<SummaryNodeLine>,
    pub token_estimate: u32,
    pub generated_by_path: Option<InstancePath>,
    pub created_at_unix_ms: u64,
    /// Whether the summary's source `workflow_run` row still exists.
    /// `run_summary` is the one never-pruned table (`03-storage-schema.md`
    /// §9), so `true` here means the run's own history is gone but the
    /// summary survives it (`07-phase3-plan.md` §4 D9).
    pub run_pruned: bool,
}

/// The run's journalled growth limits, projected back into the shape the live
/// engine holds (`WorkflowState::growth_limits` / `last_growth_limit`,
/// `src/app/workflow.rs`): last-write-wins per proposing node, plus the run's
/// most recent limit overall (whichever node hit it). Read from the
/// `growth_limited` journal — nothing else durably records a growth
/// rejection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoredGrowthLimits {
    pub last: Option<StoredGrowthLimit>,
    pub by_path: BTreeMap<InstancePath, StoredGrowthLimit>,
}

/// One journalled `growth_limited` fact.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredGrowthLimit {
    /// `"expand_max" | "max_depth" | "max_nodes"` — `ExpandLimit::as_str`'s
    /// spelling, which is also what `commit` writes into the journal payload.
    pub kind: String,
    pub limit_value: u32,
    pub requested: u32,
    pub accepted: u32,
    /// Journal-fidelity, not engine-fidelity: the `run_event` row's own `at`,
    /// which can differ from the live `current_unix_ms()` reading by a few
    /// milliseconds. The only renderer is `HH:MM`, so the drift is invisible;
    /// noted here so a future reader does not "fix" it.
    pub at_unix_ms: u64,
    pub message: String,
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
        let Some(row) = rows.into_iter().next() else {
            return Ok(None);
        };
        let names = self
            .workflow_names_by_id(std::slice::from_ref(&row.workflow))
            .await?;
        let name = names
            .get(&record_id_to_string(&row.workflow))
            .cloned()
            .unwrap_or_default();
        run_record(row, name).map(Some)
    }

    /// Runs, newest first (`03-storage-schema.md` §6). `workflow: None` lists
    /// across every workflow — the run browser's cross-workflow view
    /// (`07-phase3-plan.md` §4 D9); a single-workflow caller still gets the
    /// original filtered form.
    pub async fn list_runs(
        &self,
        workflow: Option<&WorkflowId>,
        limit: u32,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let mut response = match workflow {
            Some(workflow) => {
                let id = parse_record_id(TABLE_WORKFLOW, workflow.as_str())
                    .ok_or_else(|| StoreError::Decode(format!("not a workflow id: {workflow}")))?;
                self.db
                    .query(
                        "SELECT * FROM workflow_run WHERE workflow = $workflow \
                         ORDER BY started_at DESC LIMIT $limit",
                    )
                    .bind(("workflow", id))
                    .bind(("limit", i64::from(limit)))
                    .await
                    .map_err(query_error)?
            }
            None => self
                .db
                .query("SELECT * FROM workflow_run ORDER BY started_at DESC LIMIT $limit")
                .bind(("limit", i64::from(limit)))
                .await
                .map_err(query_error)?,
        };
        let rows: Vec<records::RunRow> = response.take(0).map_err(query_error)?;
        let ids: Vec<_> = rows.iter().map(|row| row.workflow.clone()).collect();
        let names = self.workflow_names_by_id(&ids).await?;
        rows.into_iter()
            .map(|row| {
                let name = names
                    .get(&record_id_to_string(&row.workflow))
                    .cloned()
                    .unwrap_or_default();
                run_record(row, name)
            })
            .collect()
    }

    /// One batched `workflow.name` lookup for a set of run rows — the join
    /// [`Self::list_runs`]/[`Self::get_run`] need to fill
    /// `RunRecord::workflow_name` without a query per run
    /// (`07-phase3-plan.md` §4 D9).
    async fn workflow_names_by_id(
        &self,
        ids: &[surrealdb_types::RecordId],
    ) -> Result<BTreeMap<String, String>, StoreError> {
        if ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut response = self
            .db
            .query("SELECT * FROM $ids")
            .bind(("ids", ids.to_vec()))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::WorkflowRow> = response.take(0).map_err(query_error)?;
        Ok(rows
            .into_iter()
            .map(|row| (record_id_to_string(&row.id), row.name))
            .collect())
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
        // Same shape `list_run_edges` builds: the row id -> instance path map
        // that resolves `run_node.parent` (B1) without a join per row.
        let path_by_id: BTreeMap<String, InstancePath> = rows
            .iter()
            .map(|row| {
                (
                    record_id_to_string(&row.id),
                    InstancePath::new(row.instance_path.clone()),
                )
            })
            .collect();
        let checkpoint_ids: Vec<_> = rows
            .iter()
            .filter_map(|row| row.restored_from.clone())
            .collect();
        let restored_ref_by_checkpoint_id =
            self.restored_refs_by_checkpoint_id(checkpoint_ids).await?;
        rows.into_iter()
            .map(|row| run_node_record(row, &path_by_id, &restored_ref_by_checkpoint_id))
            .collect()
    }

    /// Batched `node_checkpoint` -> [`RestoredRef`] resolution for
    /// [`Self::list_run_nodes`]'s `restored_from` column: one query for the
    /// whole node set rather than a lookup per restored row.
    async fn restored_refs_by_checkpoint_id(
        &self,
        ids: Vec<surrealdb_types::RecordId>,
    ) -> Result<BTreeMap<String, RestoredRef>, StoreError> {
        if ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let mut response = self
            .db
            .query("SELECT * FROM $ids")
            .bind(("ids", ids))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::CheckpointRow> = response.take(0).map_err(query_error)?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    record_id_to_string(&row.id),
                    RestoredRef {
                        run: RunId::new(record_id_to_string(&row.run)),
                        node_key: NodeKey::new(row.node_key),
                        checkpoint_seq: row.seq as u64,
                    },
                )
            })
            .collect())
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

    /// [`Self::find_restorable_checkpoints`], paired with each checkpoint's
    /// source node's compatibility digests (§3 rule 5) — the restore
    /// handler's read path: resolve the candidates and their comparison
    /// inputs in one call, rather than a digest lookup per selector.
    pub async fn restore_source(
        &self,
        run: &RunId,
        node_keys: &[NodeKey],
    ) -> Result<Vec<RestorableCheckpoint>, StoreError> {
        let checkpoints = self.find_restorable_checkpoints(run, node_keys).await?;
        if checkpoints.is_empty() {
            return Ok(Vec::new());
        }
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let Some(run_row) = self.select_run_row(&run_id).await? else {
            return Ok(Vec::new());
        };
        let node_rows = self.select_kvdag_nodes(&run_row.kvdag_version).await?;
        let digests_by_key: BTreeMap<String, (String, String)> = node_rows
            .iter()
            .map(|row| (row.node_key.clone(), super::node_compat_digests(row)))
            .collect();
        Ok(checkpoints
            .into_iter()
            .map(|checkpoint| {
                let (prompt_digest, schema_digest) = digests_by_key
                    .get(checkpoint.node_key.as_str())
                    .cloned()
                    .unwrap_or_default();
                RestorableCheckpoint {
                    checkpoint,
                    prompt_digest,
                    schema_digest,
                }
            })
            .collect())
    }

    /// One node key's measured record across a workflow's most recent closed
    /// runs — the aggregation `tier::resolve`'s `auto` policy has always
    /// described (`04-kvdag-and-execution.md` §7.3) and never had.
    ///
    /// Windowed to the `window` most recently *started* runs that reached a
    /// closed status. Pruning is expected, not exceptional: `prune_run_history`
    /// removes whole runs, so `runs` counts the observations that survive and
    /// a fully pruned workflow reports an all-zero record — which
    /// [`crate::workflow::tier::resolve`] already documents as behaving like no
    /// history at all.
    ///
    /// Three of the five fields are honest about being dormant
    /// (`06-phase2-plan.md` §4 D8):
    ///
    /// - `watchdog_interventions` reads `run_node.watchdog_interventions`, the
    ///   column Phase 4 will write. It is `0` until then.
    /// - `mean_tokens` reads `total_tokens`, which
    ///   `model.rs` documents as permanently `0`. It is carried because the
    ///   field exists and is deliberately **not** consulted by `resolve_auto`.
    /// - `first_pass_successes`/`schema_failures` are the two that are truthful
    ///   today, written by `write_run_node` and `write_checkpoint`.
    pub async fn node_history(
        &self,
        workflow: &WorkflowId,
        node_key: &NodeKey,
        window: usize,
    ) -> Result<NodeHistory, StoreError> {
        let mut history = NodeHistory::default();
        if window == 0 {
            return Ok(history);
        }
        let workflow_id = parse_record_id(TABLE_WORKFLOW, workflow.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow id: {workflow}")))?;

        let closed: Vec<String> = CLOSED_RUN_STATUSES
            .iter()
            .map(|status| run_status_str(*status).to_string())
            .collect();
        // `LIMIT` takes a literal, so the window is applied in Rust; the same
        // shape `prune_run_history` uses. `ORDER BY` needs its sort key in the
        // projection, hence `SELECT *`.
        let mut response = self
            .db
            .query(
                "SELECT * FROM workflow_run WHERE workflow = $workflow \
                 AND status IN $closed ORDER BY started_at DESC LIMIT 1000000",
            )
            .bind(("workflow", workflow_id))
            .bind(("closed", closed))
            .await
            .map_err(query_error)?;
        let run_rows: Vec<records::RunRow> = response.take(0).map_err(query_error)?;

        let measured: Vec<String> = MEASURED_NODE_STATUSES
            .iter()
            .map(|status| node_status_str(*status).to_string())
            .collect();
        let mut total_tokens: u64 = 0;
        // Most recent first, so the §7.3 step 4 "last two runs" window is the
        // head of this list rather than a second query.
        let mut first_pass_by_recency: Vec<bool> = Vec::new();

        for run_row in run_rows.into_iter().take(window) {
            let mut response = self
                .db
                .query(
                    "SELECT * FROM run_node WHERE run = $run AND node_key = $node_key \
                     AND status IN $measured ORDER BY instance_path",
                )
                .bind(("run", run_row.id.clone()))
                .bind(("node_key", node_key.to_string()))
                .bind(("measured", measured.clone()))
                .await
                .map_err(query_error)?;
            let node_rows: Vec<records::RunNodeRow> = response.take(0).map_err(query_error)?;
            for row in node_rows {
                history.runs = history.runs.saturating_add(1);
                if row.first_pass_succeeded {
                    history.first_pass_successes = history.first_pass_successes.saturating_add(1);
                }
                history.schema_failures = history
                    .schema_failures
                    .saturating_add(u32::try_from(row.schema_failures).unwrap_or(u32::MAX));
                history.watchdog_interventions = history
                    .watchdog_interventions
                    .saturating_add(u32::try_from(row.watchdog_interventions).unwrap_or(u32::MAX));
                total_tokens =
                    total_tokens.saturating_add(u64::try_from(row.total_tokens).unwrap_or(0));
                first_pass_by_recency.push(row.first_pass_succeeded);
            }
        }

        history.mean_tokens = if history.runs == 0 {
            0
        } else {
            total_tokens / u64::from(history.runs)
        };
        history.recent_first_pass_failures = first_pass_by_recency
            .iter()
            .take(2)
            .filter(|succeeded| !**succeeded)
            .count() as u8;
        Ok(history)
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
        let mut records = self.run_summary_records(rows).await?;
        Ok(records.pop())
    }

    /// `run_summary` rows across a workflow (or every workflow, `workflow:
    /// None`), newest first (`07-phase3-plan.md` §4 D9 — the run browser's
    /// pruned-history listing; per-run `Self::get_run_summary` exists
    /// separately).
    ///
    /// The workflow filter cannot go through `run_summary.run`: that
    /// reference dangles by design once the run is pruned, which is exactly
    /// the row this listing most needs to return. The surviving route is
    /// `kvdag_version.workflow` — `kvdag_version` rows are never pruned and
    /// carry `workflow` — written as the two-step form migration `0004`'s
    /// `run_summary_version` index accelerates: resolve the workflow's
    /// version ids first, then filter `run_summary` by that set, rather than
    /// a per-row link traversal that would walk the record graph once per
    /// summary instead of hitting the index.
    pub async fn list_run_summaries(
        &self,
        workflow: Option<&WorkflowId>,
        limit: u32,
    ) -> Result<Vec<RunSummaryRecord>, StoreError> {
        let mut response = match workflow {
            Some(workflow) => {
                let workflow_id = super::workflow_record_id(workflow)?;
                let mut version_response = self
                    .db
                    .query("SELECT VALUE id FROM kvdag_version WHERE workflow = $workflow")
                    .bind(("workflow", workflow_id))
                    .await
                    .map_err(query_error)?;
                let version_ids: Vec<surrealdb_types::RecordId> =
                    version_response.take(0).map_err(query_error)?;
                self.db
                    .query(
                        "SELECT * FROM run_summary WHERE kvdag_version IN $versions \
                         ORDER BY created_at DESC LIMIT $limit",
                    )
                    .bind(("versions", version_ids))
                    .bind(("limit", i64::from(limit)))
                    .await
                    .map_err(query_error)?
            }
            None => self
                .db
                .query("SELECT * FROM run_summary ORDER BY created_at DESC LIMIT $limit")
                .bind(("limit", i64::from(limit)))
                .await
                .map_err(query_error)?,
        };
        let rows: Vec<records::RunSummaryRow> = response.take(0).map_err(query_error)?;
        self.run_summary_records(rows).await
    }

    /// The injection feed for a new run's prior-summaries context (§4 D21):
    /// a workflow's most recent summaries, excluding the run being started.
    pub async fn run_summaries_for_context(
        &self,
        workflow: &WorkflowId,
        excluding: &RunId,
        limit: u32,
    ) -> Result<Vec<RunSummaryRecord>, StoreError> {
        Ok(self
            .list_run_summaries(Some(workflow), limit.saturating_add(1))
            .await?
            .into_iter()
            .filter(|record| &record.run != excluding)
            .take(limit as usize)
            .collect())
    }

    /// Shared resolution for [`Self::get_run_summary`]/[`Self::list_run_summaries`]:
    /// one batched lookup per kind of reference for the whole row set —
    /// source-run survival, `generated_by`'s instance path, and the version's
    /// workflow identity/name — never one query per row.
    async fn run_summary_records(
        &self,
        rows: Vec<records::RunSummaryRow>,
    ) -> Result<Vec<RunSummaryRecord>, StoreError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }

        let run_ids: Vec<_> = rows.iter().map(|row| row.run.clone()).collect();
        let surviving = self.surviving_ids(&run_ids).await?;

        let version_ids: Vec<_> = rows.iter().map(|row| row.kvdag_version.clone()).collect();
        let mut version_response = self
            .db
            .query("SELECT * FROM $ids")
            .bind(("ids", version_ids))
            .await
            .map_err(query_error)?;
        let version_rows: Vec<records::KvdagVersionRow> =
            version_response.take(0).map_err(query_error)?;
        let version_by_id: BTreeMap<String, records::KvdagVersionRow> = version_rows
            .into_iter()
            .map(|row| (record_id_to_string(&row.id), row))
            .collect();

        let workflow_ids: Vec<_> = version_by_id
            .values()
            .map(|row| row.workflow.clone())
            .collect();
        let names = self.workflow_names_by_id(&workflow_ids).await?;

        let generated_by_ids: Vec<_> = rows
            .iter()
            .filter_map(|row| row.generated_by.clone())
            .collect();
        let mut node_response = self
            .db
            .query("SELECT * FROM $ids")
            .bind(("ids", generated_by_ids))
            .await
            .map_err(query_error)?;
        let node_rows: Vec<records::RunNodeRow> = node_response.take(0).map_err(query_error)?;
        let path_by_node_id: BTreeMap<String, InstancePath> = node_rows
            .into_iter()
            .map(|row| {
                (
                    record_id_to_string(&row.id),
                    InstancePath::new(row.instance_path),
                )
            })
            .collect();

        rows.into_iter()
            .map(|row| {
                let version_key = record_id_to_string(&row.kvdag_version);
                let workflow_id = version_by_id
                    .get(&version_key)
                    .map(|version| record_id_to_string(&version.workflow))
                    .unwrap_or_default();
                let workflow_name = names.get(&workflow_id).cloned().unwrap_or_default();
                let run_pruned = !surviving.contains(&record_id_to_string(&row.run));
                let generated_by_path = row
                    .generated_by
                    .as_ref()
                    .and_then(|id| path_by_node_id.get(&record_id_to_string(id)))
                    .cloned();
                Ok(RunSummaryRecord {
                    run: RunId::new(record_id_to_string(&row.run)),
                    workflow: WorkflowId::new(workflow_id),
                    workflow_name,
                    version: KvdagVersionId::new(version_key),
                    text: row.text,
                    outcome: row.outcome,
                    highlights: row.highlights,
                    open_gaps: row.open_gaps,
                    per_node: row
                        .per_node
                        .into_iter()
                        .filter_map(summary_node_line)
                        .collect(),
                    token_estimate: row.token_estimate as u32,
                    generated_by_path,
                    created_at_unix_ms: unix_ms(&row.created_at),
                    run_pruned,
                })
            })
            .collect()
    }

    /// Which of a set of record ids still resolve — one batched existence
    /// check. Used to flag a `run_summary` row whose source run has been
    /// pruned (`03-storage-schema.md` §9).
    async fn surviving_ids(
        &self,
        ids: &[surrealdb_types::RecordId],
    ) -> Result<std::collections::BTreeSet<String>, StoreError> {
        if ids.is_empty() {
            return Ok(std::collections::BTreeSet::new());
        }
        let mut response = self
            .db
            .query("SELECT VALUE id FROM $ids")
            .bind(("ids", ids.to_vec()))
            .await
            .map_err(query_error)?;
        let rows: Vec<surrealdb_types::RecordId> = response.take(0).map_err(query_error)?;
        Ok(rows.iter().map(record_id_to_string).collect())
    }

    /// A run's interrogations, in the order they were started. No production
    /// caller filters these further today: `load_historical_run` (WS-H) and
    /// the interrogate handler's active-interrogation check (WS-D) both want
    /// the whole set for one run.
    /// Every member snapshot the projection took for this run, oldest first.
    ///
    /// Ordered by `first_seen_at, name` — the order the team was assembled in,
    /// with the name as the tie-breaker for members that appeared in the same
    /// observation, so a listing is stable across calls.
    pub async fn list_run_members(&self, run: &RunId) -> Result<Vec<RunMemberRecord>, StoreError> {
        let id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let mut response = self
            .db
            .query("SELECT * FROM run_member WHERE run = $run ORDER BY first_seen_at, name")
            .bind(("run", id))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::RunMemberRow> = response.take(0).map_err(query_error)?;
        Ok(rows.into_iter().map(run_member_record).collect())
    }

    pub async fn list_interrogations(
        &self,
        run: &RunId,
    ) -> Result<Vec<InterrogationRecord>, StoreError> {
        let id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        // `interrogation` carries no `run` column of its own — it is
        // addressed through `run_node`, which does (§4 D8: an interrogation
        // is not a run node, but it names the source node it revived).
        let mut node_response = self
            .db
            .query("SELECT * FROM run_node WHERE run = $run")
            .bind(("run", id))
            .await
            .map_err(query_error)?;
        let node_rows: Vec<records::RunNodeRow> = node_response.take(0).map_err(query_error)?;
        let path_by_id: BTreeMap<String, InstancePath> = node_rows
            .iter()
            .map(|row| {
                (
                    record_id_to_string(&row.id),
                    InstancePath::new(row.instance_path.clone()),
                )
            })
            .collect();
        let node_ids: Vec<_> = node_rows.iter().map(|row| row.id.clone()).collect();
        if node_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut response = self
            .db
            .query("SELECT * FROM interrogation WHERE run_node IN $nodes ORDER BY started_at")
            .bind(("nodes", node_ids))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::InterrogationRow> = response.take(0).map_err(query_error)?;
        rows.into_iter()
            .map(|row| interrogation_record(row, &path_by_id))
            .collect()
    }

    /// The run's journalled growth limits (P2b). See [`StoredGrowthLimits`].
    pub async fn growth_limits(&self, run: &RunId) -> Result<StoredGrowthLimits, StoreError> {
        let id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;

        // Bound to `RunEventKind::GrowthLimited`'s own spelling rather than a
        // hand-typed literal, so the join cannot drift from
        // `run_event_kind_str`'s encoding.
        let mut response = self
            .db
            .query("SELECT * FROM run_event WHERE run = $run AND kind = $kind ORDER BY seq")
            .bind(("run", id.clone()))
            .bind(("kind", RunEventKind::GrowthLimited.as_str().to_string()))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::RunEventRow> = response.take(0).map_err(query_error)?;
        // Most runs never hit a guardrail, and the node map below only exists
        // to key `by_path`. Asking for it before knowing there is anything to
        // key would double the cost of the common case.
        if rows.is_empty() {
            return Ok(StoredGrowthLimits::default());
        }

        // Same shape `list_run_edges`/`list_run_nodes` build: the row id ->
        // instance path map that resolves `run_event.run_node` to the path the
        // rest of the run surface addresses nodes by.
        let mut response = self
            .db
            .query("SELECT * FROM run_node WHERE run = $run")
            .bind(("run", id))
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

        let mut limits = StoredGrowthLimits::default();
        for row in &rows {
            let Some(limit) = stored_growth_limit(row) else {
                continue;
            };
            // `commit` always journals `growth_limited` against the proposing
            // node (`expand.rs`), so a row with no `run_node`, or one that
            // resolves to nothing in this run, should not happen; when it
            // does, the limit still counts toward `last` but not `by_path`.
            if let Some(path) = row
                .run_node
                .as_ref()
                .and_then(|id| path_by_id.get(&record_id_to_string(id)))
            {
                limits.by_path.insert(path.clone(), limit.clone());
            }
            limits.last = Some(limit);
        }
        Ok(limits)
    }

    /// Each listed run's most recent journalled growth limit — the run-level
    /// half of [`StoredGrowthLimits`], for callers that answer about many runs
    /// at once.
    ///
    /// One query for the whole list rather than one per run: `run list`'s
    /// `limit` is caller-supplied and uncapped, so a per-run loop would be an
    /// unbounded N+1. `by_path` is deliberately not resolved here — it is the
    /// only part that needs each run's `run_node` rows, and no list surface
    /// reports per-node limits.
    pub async fn last_growth_limit_by_run(
        &self,
        runs: &[RunId],
    ) -> Result<BTreeMap<RunId, StoredGrowthLimit>, StoreError> {
        if runs.is_empty() {
            return Ok(BTreeMap::new());
        }
        let ids: Vec<_> = runs
            .iter()
            .filter_map(|run| parse_record_id(TABLE_WORKFLOW_RUN, run.as_str()))
            .collect();
        if ids.is_empty() {
            return Ok(BTreeMap::new());
        }

        let mut response = self
            .db
            .query("SELECT * FROM run_event WHERE kind = $kind AND run IN $runs ORDER BY seq")
            .bind(("runs", ids))
            .bind(("kind", RunEventKind::GrowthLimited.as_str().to_string()))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::RunEventRow> = response.take(0).map_err(query_error)?;

        // `seq` is per-run, so a global `ORDER BY seq` still leaves each run's
        // own rows in ascending order; last write wins per run.
        let mut last = BTreeMap::new();
        for row in &rows {
            let Some(limit) = stored_growth_limit(row) else {
                continue;
            };
            last.insert(RunId::new(record_id_to_string(&row.run)), limit);
        }
        Ok(last)
    }
}

/// Parses one `growth_limited` journal row's payload back into a
/// [`StoredGrowthLimit`]. `commit` (`src/workflow/engine/expand.rs`) always
/// writes `limit`/`limit_value`/`requested`/`accepted`/`message` for a row of
/// this kind (B2 made the counts unconditional); a row missing `limit` is not
/// one `commit` wrote and is skipped rather than guessed.
fn stored_growth_limit(row: &records::RunEventRow) -> Option<StoredGrowthLimit> {
    let kind = row.payload.get("limit")?.as_str()?.to_string();
    let limit_value = row
        .payload
        .get("limit_value")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;
    let requested = row
        .payload
        .get("requested")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;
    let accepted = row
        .payload
        .get("accepted")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;
    let message = row
        .payload
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some(StoredGrowthLimit {
        kind,
        limit_value,
        requested,
        accepted,
        at_unix_ms: unix_ms(&row.at),
        message,
    })
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

fn run_record(row: records::RunRow, workflow_name: String) -> Result<RunRecord, StoreError> {
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
    // `{"run": "<id>"}`, the shape `WorkflowStore::create_run` writes
    // (`07-phase3-plan.md` §4 D4); anything else decodes as "no restore
    // provenance" rather than failing the whole row.
    let restore_from_run = row
        .restore_from
        .as_ref()
        .and_then(|value| value.get("run"))
        .and_then(serde_json::Value::as_str)
        .map(RunId::new);
    Ok(RunRecord {
        id: RunId::new(record_id_to_string(&row.id)),
        workflow: WorkflowId::new(record_id_to_string(&row.workflow)),
        workflow_name,
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
        restore_from_run,
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
        lead_session_id: row.lead_session_id,
        team_name: row.team_name,
        lead_pane_id: row.lead_pane_id,
        lead_terminal_id: row.lead_terminal_id,
        lead_prompt_version: row.lead_prompt_version.map(|version| version as u32),
    })
}

fn run_node_record(
    row: records::RunNodeRow,
    path_by_id: &BTreeMap<String, InstancePath>,
    restored_ref_by_checkpoint_id: &BTreeMap<String, RestoredRef>,
) -> Result<RunNodeRecord, StoreError> {
    // Resolved against this run's own rows rather than a live `Kvdag` — a
    // restored run has no live definition to consult, and `parent` only ever
    // points at another `run_node` row in the same run.
    let parent_path = row
        .parent
        .as_ref()
        .and_then(|parent| path_by_id.get(&record_id_to_string(parent)))
        .cloned();
    let restored_from = row
        .restored_from
        .as_ref()
        .and_then(|checkpoint| restored_ref_by_checkpoint_id.get(&record_id_to_string(checkpoint)))
        .cloned();
    Ok(RunNodeRecord {
        run: RunId::new(record_id_to_string(&row.run)),
        node_key: NodeKey::new(row.node_key),
        instance_path: InstancePath::new(row.instance_path),
        label: row.label,
        inputs: super::string_map_from_json(&row.inputs),
        parent_path,
        depth: row.depth as u16,
        status: parse_node_status(&row.status)?,
        model: row.model,
        effort: row.effort,
        demand: parse_demand(&row.demand)?,
        attempt: row.attempt as u8,
        assignment_reason: row.assignment_reason,
        pane_id: row.pane_id,
        terminal_id: row.terminal_id,
        agent_session_id: row.agent_session_id,
        cwd: row.cwd,
        node_dir: row.node_dir,
        transcript_path: row.transcript_path,
        evidence: row.evidence.as_deref().map(parse_evidence).transpose()?,
        succession: parse_succession(row.succession.as_deref(), row.blocker.as_ref())?,
        total_tokens: row.total_tokens as u64,
        tool_uses: row.tool_uses as u32,
        duration_ms: row.duration_ms as u64,
        started_at_unix_ms: row.started_at.as_ref().map(unix_ms),
        ended_at_unix_ms: row.ended_at.as_ref().map(unix_ms),
        restored_from,
        task_id: row.task_id,
        subject: row.subject,
        owner: row.owner,
        emergent: row.emergent,
    })
}

fn run_member_record(row: records::RunMemberRow) -> RunMemberRecord {
    RunMemberRecord {
        run: RunId::new(record_id_to_string(&row.run)),
        name: row.name,
        agent_type: row.agent_type,
        model: row.model,
        pane_id: row.pane_id,
        backend_type: row.backend_type,
        is_active: row.is_active,
        cwd: row.cwd,
        first_seen_at_unix_ms: unix_ms(&row.first_seen_at),
        last_seen_at_unix_ms: unix_ms(&row.last_seen_at),
    }
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

fn interrogation_record(
    row: records::InterrogationRow,
    path_by_id: &BTreeMap<String, InstancePath>,
) -> Result<InterrogationRecord, StoreError> {
    let path = path_by_id
        .get(&record_id_to_string(&row.run_node))
        .cloned()
        .ok_or_else(|| {
            StoreError::Decode(format!(
                "interrogation {} references a run_node not in this run",
                record_id_to_string(&row.id)
            ))
        })?;
    Ok(InterrogationRecord {
        id: InterrogationId::new(record_id_to_string(&row.id)),
        path,
        source_session_id: row.source_session_id,
        forked_session_id: row.forked_session_id,
        transcript_path: row.transcript_path,
        cwd: row.cwd,
        pane_id: row.pane_id,
        reconstructed: row.reconstructed,
        note: row.note,
        started_at_unix_ms: unix_ms(&row.started_at),
        ended_at_unix_ms: row.ended_at.as_ref().map(unix_ms),
    })
}

/// Parses one `run_summary.per_node` entry (`{node_key, verdict, one_liner}`)
/// back into a [`SummaryNodeLine`]. A malformed entry is dropped rather than
/// failing the whole summary — the store never wrote a summary with a
/// per-node array unless the epilogue's result already passed
/// `summary_output_schema()`, so a decode miss here means the shape drifted,
/// not that the summary itself is unusable.
fn summary_node_line(value: serde_json::Value) -> Option<SummaryNodeLine> {
    let node_key = value.get("node_key")?.as_str()?.to_string();
    let verdict = value.get("verdict")?.as_str()?.to_string();
    let one_liner = value.get("one_liner")?.as_str()?.to_string();
    Some(SummaryNodeLine {
        node_key,
        verdict,
        one_liner,
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
        "summary" => Ok(RunEventKind::Summary),
        "member" => Ok(RunEventKind::Member),
        "task" => Ok(RunEventKind::Task),
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
