//! Embedded SurrealDB persistence for workflows, versions, runs, and the run
//! journal.
//!
//! Only this module talks to SurrealDB
//! (`docs/design/workflow-builder/03-storage-schema.md`). It is gated behind
//! the `workflow` cargo feature; with the feature off the subsystem reports
//! `workflow_unavailable` and nothing else in the crate changes shape.
//!
//! Two properties are structural, not conventional:
//!
//! - **Append-only by construction.** `kvdag_version`, `kvdag_node`,
//!   `kvdag_edge`, `node_checkpoint`, and `run_event` have no update and no
//!   delete method here. There is no API to call.
//! - **Whole-run retention only.** [`WorkflowStore::prune_run_history`] is the
//!   single deleting entry point and can only remove whole runs, never
//!   individual records inside a retained run.
//!
//! Step 2a adds `mod records;` and `mod queries;`, the migration files under
//! `migrations/`, and the read methods that return typed rows
//! (`get_workflow`, `list_workflows`, `get_run`, `list_runs`,
//! `list_run_nodes`, `list_run_events`, `list_checkpoints`,
//! `find_restorable_checkpoints`, `get_run_summary`). The names, the error
//! contract, and the write surface below are frozen.

pub mod error;
mod queries;
mod records;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use error::StoreError;
// Read-surface record types: no in-crate caller yet (the API layer that
// exposes them, `src/app/api/workflows.rs`, is a later step), but this is a
// binary crate, so nothing exempts a `pub use` from the unused-import lint on
// its own the way it would in a published library.
#[allow(unused_imports)]
pub use queries::{
    CheckpointRecord, RunEdgeRecord, RunEventRecord, RunNodeRecord, RunRecord, RunSummaryRecord,
    VersionRecord, WorkflowSummary,
};
use surrealdb::engine::local::{Db, Mem, SurrealKv};
use surrealdb::Surreal;

use crate::workflow::model::{
    CheckpointKind, Demand, EdgeKind, EdgePayload, Evidence, GrowthLimits, Isolation, Kvdag,
    KvdagEdge, KvdagNode, KvdagSpec, KvdagVersionId, NodeBinding, NodeKey, NodeKind, NodeStatus,
    NodeUsage, OutputSchema, RunEventKind, RunId, RunStatus, StoreWrite, Succession, WorkflowId,
};
use crate::workflow::tier::Tier;
use records::{
    parse_record_id, record_id_to_string, KvdagEdgeRow, KvdagNodeRow, KvdagVersionRow,
    SchemaMetaRow, WorkflowRow,
};

/// Overrides the database directory; primarily for tests and for pointing a
/// debug build at a release build's history.
pub const DB_PATH_ENV: &str = "KARVEX_WORKFLOW_DB_PATH";

pub const NAMESPACE: &str = "karvex";
pub const DATABASE: &str = "workflow";

const TABLE_WORKFLOW: &str = "workflow";
const TABLE_KVDAG_VERSION: &str = "kvdag_version";
const TABLE_KVDAG_NODE: &str = "kvdag_node";
const TABLE_WORKFLOW_RUN: &str = "workflow_run";
const TABLE_RUN_NODE: &str = "run_node";

/// Every statically materialised `run_node` sits at expansion depth 0; see
/// [`WorkflowStore::materialise_run_nodes`] for why this is not topological
/// depth.
const STATIC_NODE_DEPTH: u16 = 0;

/// The `run_node.status` values that close a node, mirroring
/// [`NodeStatus::is_terminal`] as the string set a query can compare against.
/// `nodes_done` is recomputed from this set, so the two must not drift;
/// `terminal_node_statuses_match_the_model` pins that.
const TERMINAL_NODE_STATUSES: &[NodeStatus] = &[
    NodeStatus::Succeeded,
    NodeStatus::Failed,
    NodeStatus::Skipped,
    NodeStatus::Restored,
    NodeStatus::Cancelled,
];

/// Payload budgets (`03-storage-schema.md` §7): token efficiency is a schema
/// property, enforced here rather than left to caller discipline.
const CHECKPOINT_PAYLOAD_BUDGET_BYTES: usize = 256 * 1024;
const RUN_EVENT_PAYLOAD_BUDGET_BYTES: usize = 16 * 1024;
const SUMMARY_BUDGET_CHARS: usize = 1_200;

/// Every embedded migration, applied in order and recorded in `schema_meta`.
/// The version string is both the `schema_meta.version` value and this
/// module's audit trail of what has ever shipped.
const MIGRATIONS: &[(&str, &str)] = &[("0001_init", include_str!("migrations/0001_init.surql"))];

/// Where a store's data lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreLocation {
    /// A `SurrealKv` directory. Holds an exclusive lock while open.
    OnDisk(PathBuf),
    /// A `kv-mem` database. Store and engine tests use this so they touch no
    /// disk; it is never a fallback for a locked on-disk store.
    Memory,
}

/// How a new `kvdag_version` came to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionOrigin {
    Authored,
    Imported,
    SelfImprovement,
    RestoreRewrite,
}

impl VersionOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authored => "authored",
            Self::Imported => "imported",
            Self::SelfImprovement => "self_improvement",
            Self::RestoreRewrite => "restore_rewrite",
        }
    }

    /// The inverse of [`Self::as_str`], for reading a stored `kvdag_version.origin`
    /// back into the enum (`queries::version_record`).
    pub fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "authored" => Ok(Self::Authored),
            "imported" => Ok(Self::Imported),
            "self_improvement" => Ok(Self::SelfImprovement),
            "restore_rewrite" => Ok(Self::RestoreRewrite),
            other => Err(StoreError::Decode(format!(
                "unknown kvdag_version origin {other:?}"
            ))),
        }
    }
}

/// The inputs of one run, checked against the version's limits on create: a run
/// narrows its version's growth limits and never widens them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRun {
    pub workflow: WorkflowId,
    pub version: KvdagVersionId,
    pub tier: Tier,
    pub args: BTreeMap<String, String>,
    pub growth: GrowthLimits,
    pub context_runs: Vec<RunId>,
    /// Where the run's panes live, as the public API workspace id
    /// (`03-storage-schema.md` §4.2). Recorded at create time because it is a
    /// property of the run, not of the server that happens to be executing it:
    /// without it a run read back from the journal has no workspace binding at
    /// all.
    pub workspace_id: Option<String>,
}

/// The workflow database. Opened lazily on first `workflow.*` use, so a karvex
/// that never touches workflows never pays the open cost.
#[derive(Debug)]
pub struct WorkflowStore {
    location: StoreLocation,
    db: Surreal<Db>,
}

impl WorkflowStore {
    /// `state_dir()` already appends the app directory name, so nothing extra
    /// is joined here. Debug builds resolve to `karvex-dev` and therefore use a
    /// different database from an installed release build.
    pub fn default_location() -> StoreLocation {
        match std::env::var_os(DB_PATH_ENV) {
            Some(path) if !path.is_empty() => StoreLocation::OnDisk(PathBuf::from(path)),
            _ => StoreLocation::OnDisk(crate::config::state_dir().join("workflow")),
        }
    }

    /// Opens the database and applies pending migrations in a transaction.
    /// Returns [`StoreError::Unavailable`] with reason
    /// [`error::STORE_LOCKED`] when another karvex server owns the directory.
    pub async fn open(location: StoreLocation) -> Result<Self, StoreError> {
        let db = connect(&location).await?;
        db.use_ns(NAMESPACE)
            .use_db(DATABASE)
            .await
            .map_err(query_error)?;
        let store = Self { location, db };
        store.migrate().await?;
        Ok(store)
    }

    pub fn location(&self) -> &StoreLocation {
        &self.location
    }

    /// Applies every unapplied migration in order, inside one transaction, and
    /// records the applied set in `schema_meta`. Re-applying is a no-op:
    /// already-applied versions are skipped.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        let applied = self.applied_migrations().await?;
        for (version, sql) in MIGRATIONS {
            if applied.contains(*version) {
                continue;
            }
            self.apply_migration(version, sql).await?;
        }
        Ok(())
    }

    async fn applied_migrations(&self) -> Result<std::collections::BTreeSet<String>, StoreError> {
        let mut response = self
            .db
            .query("SELECT * FROM schema_meta")
            .await
            .map_err(query_error)?;
        // On a brand-new database `schema_meta` itself doesn't exist yet — it
        // is defined BY migration 0001. `.query()` only fails per-statement on
        // `.take()`, not on the outer `.await`, so that's where this shows up.
        // Unlike a schemaless key scan, this embedded engine errors rather
        // than returning an empty set for a `SELECT` against an undefined
        // table; that specific failure means "no migrations applied yet".
        let rows: Vec<SchemaMetaRow> = match response.take(0) {
            Ok(rows) => rows,
            Err(error) if error.to_string().contains("does not exist") => Vec::new(),
            Err(error) => return Err(query_error(error)),
        };
        Ok(rows.into_iter().map(|row| row.version).collect())
    }

    /// A single `.query()` call is already one transaction (SurrealDB commits
    /// or cancels every statement in one request atomically); the DDL and the
    /// `schema_meta` marker travel together so a failed migration leaves no
    /// partial schema behind.
    async fn apply_migration(&self, version: &str, sql: &str) -> Result<(), StoreError> {
        let statement = format!("{sql}\nCREATE schema_meta SET version = $version;");
        let response = self
            .db
            .query(statement)
            .bind(("version", version.to_string()))
            .await
            .map_err(|error| migration_error(version, error))?;
        response
            .check()
            .map_err(|error| migration_error(version, error))?;
        Ok(())
    }

    pub async fn create_workflow(
        &self,
        name: &str,
        description: &str,
        default_tier: Tier,
    ) -> Result<WorkflowId, StoreError> {
        let mut response = self
            .db
            .query(
                "CREATE workflow SET name = $name, description = $description, \
                 default_tier = $default_tier RETURN AFTER",
            )
            .bind(("name", name.to_string()))
            .bind(("description", description.to_string()))
            .bind(("default_tier", default_tier.as_str().to_string()))
            .await
            .map_err(query_error)?;
        let rows: Vec<WorkflowRow> = response.take(0).map_err(query_error)?;
        let row = rows
            .into_iter()
            .next()
            .ok_or_else(|| StoreError::Query("create workflow returned no row".to_string()))?;
        Ok(WorkflowId::new(record_id_to_string(&row.id)))
    }

    /// Writes a new immutable version plus its nodes and edges, and returns the
    /// validated graph. An identical graph yields the same `spec_digest` and is
    /// not written again.
    pub async fn create_version(
        &self,
        workflow: &WorkflowId,
        origin: VersionOrigin,
        change_summary: &str,
        spec: KvdagSpec,
    ) -> Result<Kvdag, StoreError> {
        let workflow_id = workflow_record_id(workflow)?;
        let workflow_row =
            self.select_workflow(&workflow_id)
                .await?
                .ok_or_else(|| StoreError::NotFound {
                    table: TABLE_WORKFLOW,
                    id: workflow.to_string(),
                })?;

        // Identity fields (version number, parent) are the store's to assign;
        // the caller supplies content (contract/growth/args/nodes/edges) and,
        // optionally, an explicit parent override for a non-linear origin
        // (e.g. a future `restore_rewrite`).
        let latest = self.latest_version(&workflow_id).await?;
        let next_version = latest.as_ref().map_or(1, |(_, version)| version + 1);
        let explicit_parent = spec.parent.clone();
        let parent = explicit_parent.clone().or_else(|| {
            workflow_row
                .head_version
                .as_ref()
                .map(|id| KvdagVersionId::new(record_id_to_string(id)))
        });

        let probe_spec = KvdagSpec {
            version_id: KvdagVersionId::new(format!("{TABLE_KVDAG_VERSION}:probe")),
            workflow_id: workflow.clone(),
            version: next_version,
            parent: parent.clone(),
            ..spec
        };
        let validated = Kvdag::try_new(probe_spec)?;

        // A no-op revision is compared against the tip of the chain, and on
        // everything a version stores — `spec_digest` covers only nodes and
        // edges (`03` §5), while the row also carries the contract, the run
        // arguments, and the growth limits.
        //
        // Both halves matter. Matching any historical digest anywhere in the
        // workflow's history would return an *older* version, and the caller
        // advances `head_version` to whatever comes back, so a graph that
        // reproduced a past shape would walk the head backwards. Matching on
        // the digest alone would silently discard a caller's new
        // contract/args/limits and report success for a revision that was never
        // written. An explicit parent is a deliberate non-linear origin and is
        // never a no-op, so it skips the check entirely.
        if let Some((latest_id, _)) = latest.as_ref().filter(|_| explicit_parent.is_none()) {
            let existing = self.load_version(latest_id).await?;
            if existing.spec_digest == validated.spec_digest
                && existing.contract == validated.contract
                && existing.args == validated.args
                && existing.growth == validated.growth
            {
                return Ok(existing);
            }
        }

        let args_json = serde_json::to_value(&validated.args)
            .map_err(|error| StoreError::Query(error.to_string()))?;
        let mut response = self
            .db
            .query(
                "CREATE kvdag_version SET workflow = $workflow, version = $version, \
                 parent = $parent, origin = $origin, change_summary = $change_summary, \
                 contract = $contract, args = $args, max_depth = $max_depth, \
                 max_nodes = $max_nodes, spec_digest = $spec_digest RETURN AFTER",
            )
            .bind(("workflow", workflow_id.clone()))
            .bind((
                "parent",
                parent
                    .as_ref()
                    .and_then(|id| parse_record_id(TABLE_KVDAG_VERSION, id.as_str())),
            ))
            .bind(("version", i64::from(validated.version)))
            .bind(("origin", origin.as_str().to_string()))
            .bind(("change_summary", change_summary.to_string()))
            .bind(("contract", validated.contract.clone()))
            .bind(("args", args_json))
            .bind(("max_depth", i64::from(validated.growth.max_depth)))
            .bind(("max_nodes", i64::from(validated.growth.max_nodes)))
            .bind(("spec_digest", validated.spec_digest.to_string()))
            .await
            .map_err(query_error)?;
        let version_rows: Vec<KvdagVersionRow> = response.take(0).map_err(query_error)?;
        let version_row = version_rows
            .into_iter()
            .next()
            .ok_or_else(|| StoreError::Query("create kvdag_version returned no row".to_string()))?;

        let mut node_ids: BTreeMap<NodeKey, surrealdb_types::RecordId> = BTreeMap::new();
        for node in &validated.nodes {
            let node_id = self.insert_kvdag_node(&version_row.id, node).await?;
            node_ids.insert(node.key.clone(), node_id);
        }
        for edge in &validated.edges {
            let from_id = node_ids
                .get(&edge.from)
                .ok_or_else(|| StoreError::Query(format!("unknown edge source {}", edge.from)))?;
            let to_id = node_ids
                .get(&edge.to)
                .ok_or_else(|| StoreError::Query(format!("unknown edge target {}", edge.to)))?;
            self.insert_kvdag_edge(from_id, to_id, edge).await?;
        }

        self.load_version(&KvdagVersionId::new(record_id_to_string(&version_row.id)))
            .await
    }

    async fn insert_kvdag_node(
        &self,
        version_id: &surrealdb_types::RecordId,
        node: &KvdagNode,
    ) -> Result<surrealdb_types::RecordId, StoreError> {
        let output_schema = node.output_schema.as_value().clone();
        let mut response = self
            .db
            .query(
                "CREATE kvdag_node SET version = $version, node_key = $node_key, \
                 label = $label, role = $role, kind = $kind, runner = $runner, \
                 command = $command, demand = $demand, prompt_template = $prompt_template, \
                 system_contract = $system_contract, output_schema = $output_schema, \
                 max_attempts = $max_attempts, timeout_ms = $timeout_ms, \
                 isolation = $isolation, is_template = $is_template, \
                 expand_allow = $expand_allow, expand_max = $expand_max RETURN AFTER",
            )
            .bind(("version", version_id.clone()))
            .bind(("node_key", node.key.to_string()))
            .bind(("label", node.label.clone()))
            .bind(("role", node.role.clone()))
            .bind(("kind", node_kind_str(node.kind).to_string()))
            .bind(("runner", runner_str(node.runner).to_string()))
            .bind(("command", node.command.clone()))
            .bind(("demand", demand_str(node.demand).to_string()))
            .bind(("prompt_template", node.prompt_template.clone()))
            .bind(("system_contract", node.system_contract.clone()))
            .bind(("output_schema", output_schema))
            .bind(("max_attempts", i64::from(node.max_attempts)))
            .bind(("timeout_ms", node.timeout_ms.map(|ms| ms as i64)))
            .bind(("isolation", isolation_str(node.isolation).to_string()))
            .bind(("is_template", node.is_template))
            .bind((
                "expand_allow",
                node.expand_allow
                    .iter()
                    .map(NodeKey::to_string)
                    .collect::<Vec<_>>(),
            ))
            .bind(("expand_max", i64::from(node.expand_max)))
            .await
            .map_err(query_error)?;
        let rows: Vec<KvdagNodeRow> = response.take(0).map_err(query_error)?;
        let row = rows
            .into_iter()
            .next()
            .ok_or_else(|| StoreError::Query("create kvdag_node returned no row".to_string()))?;
        Ok(row.id)
    }

    async fn insert_kvdag_edge(
        &self,
        from: &surrealdb_types::RecordId,
        to: &surrealdb_types::RecordId,
        edge: &KvdagEdge,
    ) -> Result<(), StoreError> {
        let condition = edge
            .condition
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|error| StoreError::Query(error.to_string()))?;
        let response = self
            .db
            .query(
                "RELATE $from -> kvdag_edge -> $to SET kind = $kind, \
                 condition = $condition, payload = $payload, port = $port",
            )
            .bind(("from", from.clone()))
            .bind(("to", to.clone()))
            .bind(("kind", edge_kind_str(edge.kind).to_string()))
            .bind(("condition", condition))
            .bind(("payload", edge_payload_str(edge.payload).to_string()))
            .bind(("port", edge.port.clone()))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    /// Advances the workflow's head pointer. `workflow` is the one mutable
    /// definition table; the version it points at is immutable.
    pub async fn set_head_version(
        &self,
        workflow: &WorkflowId,
        version: &KvdagVersionId,
    ) -> Result<(), StoreError> {
        let workflow_id = workflow_record_id(workflow)?;
        let version_id = parse_record_id(TABLE_KVDAG_VERSION, version.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a kvdag_version id: {version}")))?;
        let response = self
            .db
            .query("UPDATE $workflow SET head_version = $version, updated_at = time::now()")
            .bind(("workflow", workflow_id))
            .bind(("version", version_id))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    /// Loads one version's full node and edge set back into a validated graph.
    pub async fn load_version(&self, version: &KvdagVersionId) -> Result<Kvdag, StoreError> {
        let version_id = parse_record_id(TABLE_KVDAG_VERSION, version.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a kvdag_version id: {version}")))?;
        let version_row = self
            .select_kvdag_version(&version_id)
            .await?
            .ok_or_else(|| StoreError::NotFound {
                table: TABLE_KVDAG_VERSION,
                id: version.to_string(),
            })?;

        let node_rows = self.select_kvdag_nodes(&version_id).await?;
        let edge_rows = self.select_kvdag_edges(&version_id).await?;

        let mut node_key_by_id: BTreeMap<String, NodeKey> = BTreeMap::new();
        let mut nodes = Vec::with_capacity(node_rows.len());
        for row in &node_rows {
            let key = NodeKey::new(row.node_key.clone());
            node_key_by_id.insert(record_id_to_string(&row.id), key.clone());
        }
        for row in node_rows {
            nodes.push(node_row_to_model(row)?);
        }

        let mut edges = Vec::with_capacity(edge_rows.len());
        for row in edge_rows {
            let from = node_key_by_id
                .get(&record_id_to_string(&row.r#in))
                .cloned()
                .ok_or_else(|| StoreError::Decode("edge references an unknown node".to_string()))?;
            let to = node_key_by_id
                .get(&record_id_to_string(&row.out))
                .cloned()
                .ok_or_else(|| StoreError::Decode("edge references an unknown node".to_string()))?;
            edges.push(edge_row_to_model(row, from, to)?);
        }

        let args: Vec<crate::workflow::model::ArgSpec> =
            serde_json::from_value(version_row.args)
                .map_err(|error| StoreError::Decode(error.to_string()))?;
        let parent = version_row
            .parent
            .as_ref()
            .map(|id| KvdagVersionId::new(record_id_to_string(id)));

        let spec = KvdagSpec {
            version_id: KvdagVersionId::new(record_id_to_string(&version_row.id)),
            workflow_id: WorkflowId::new(record_id_to_string(&version_row.workflow)),
            version: version_row.version as u32,
            parent,
            contract: version_row.contract,
            growth: GrowthLimits {
                max_depth: version_row.max_depth as u16,
                max_nodes: version_row.max_nodes as u16,
            },
            args,
            nodes,
            edges,
        };
        Ok(Kvdag::try_new(spec)?)
    }

    pub async fn create_run(&self, run: NewRun) -> Result<RunId, StoreError> {
        let workflow_id = workflow_record_id(&run.workflow)?;
        let version_id =
            parse_record_id(TABLE_KVDAG_VERSION, run.version.as_str()).ok_or_else(|| {
                StoreError::Decode(format!("not a kvdag_version id: {}", run.version))
            })?;
        let graph = self.load_version(&run.version).await?;

        let args_json = serde_json::Value::Object(
            run.args
                .into_iter()
                .map(|(key, value)| (key, serde_json::Value::String(value)))
                .collect(),
        );
        let context_runs: Vec<surrealdb_types::RecordId> = run
            .context_runs
            .iter()
            .filter_map(|id| parse_record_id(TABLE_WORKFLOW_RUN, id.as_str()))
            .collect();

        let mut response = self
            .db
            .query(
                "CREATE workflow_run SET workflow = $workflow, kvdag_version = $version, \
                 tier = $tier, status = \"pending\", args = $args, \
                 context_runs = $context_runs, max_depth = $max_depth, \
                 max_nodes = $max_nodes, workspace_id = $workspace_id, \
                 nodes_total = $nodes_total RETURN AFTER",
            )
            .bind(("workflow", workflow_id))
            .bind(("version", version_id.clone()))
            .bind(("tier", run.tier.as_str().to_string()))
            .bind(("args", args_json))
            .bind(("context_runs", context_runs))
            .bind(("max_depth", i64::from(run.growth.max_depth)))
            .bind(("max_nodes", i64::from(run.growth.max_nodes)))
            .bind(("workspace_id", run.workspace_id))
            .bind((
                "nodes_total",
                graph.nodes.iter().filter(|n| !n.is_template).count() as i64,
            ))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::RunRow> = response.take(0).map_err(query_error)?;
        let row = rows
            .into_iter()
            .next()
            .ok_or_else(|| StoreError::Query("create workflow_run returned no row".to_string()))?;
        let run_id = RunId::new(record_id_to_string(&row.id));

        self.materialise_run_nodes(&row.id, &version_id, run.tier, &graph)
            .await?;

        Ok(run_id)
    }

    /// The engine's durable write path. Never on the critical path of a node
    /// transition: a failure here degrades the run to `persistence_degraded`
    /// rather than killing it.
    pub async fn write(&self, write: StoreWrite) -> Result<(), StoreError> {
        match write {
            StoreWrite::RunEvent {
                run,
                seq,
                kind,
                path,
                payload,
            } => self.write_run_event(run, seq, kind, path, payload).await,
            StoreWrite::RunStatus {
                run,
                status,
                ended_at_unix_ms,
            } => self.write_run_status(run, status, ended_at_unix_ms).await,
            StoreWrite::RunNode {
                run,
                path,
                status,
                attempt,
                binding,
                usage,
                evidence,
                succession,
                started_at_unix_ms,
                ended_at_unix_ms,
            } => {
                self.write_run_node(
                    run,
                    path,
                    status,
                    attempt,
                    binding,
                    usage,
                    evidence,
                    succession,
                    started_at_unix_ms,
                    ended_at_unix_ms,
                )
                .await
            }
            StoreWrite::RunEdge {
                run,
                from,
                to,
                kind,
                condition_result,
                fired,
            } => {
                self.write_run_edge(run, from, to, kind, condition_result, fired)
                    .await
            }
            StoreWrite::Checkpoint {
                run,
                path,
                seq,
                kind,
                schema_valid,
                payload,
                summary,
                artifact_paths,
                digest,
            } => {
                self.write_checkpoint(
                    run,
                    path,
                    seq,
                    kind,
                    schema_valid,
                    payload,
                    summary,
                    artifact_paths,
                    digest,
                )
                .await
            }
        }
    }

    /// Deletes whole runs beyond the retention window and returns how many were
    /// removed. Every `run_summary` survives, and no dangling `run_node`
    /// reference is left behind.
    pub async fn prune_run_history(
        &self,
        workflow: &WorkflowId,
        keep_runs: usize,
    ) -> Result<u64, StoreError> {
        let workflow_id = workflow_record_id(workflow)?;
        let mut response = self
            .db
            .query(
                // ORDER BY requires the sort key in the projection, so this
                // selects the full row rather than just `id`.
                "SELECT * FROM workflow_run WHERE workflow = $workflow \
                 ORDER BY started_at DESC LIMIT 1000000",
            )
            .bind(("workflow", workflow_id))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::RunRow> = response.take(0).map_err(query_error)?;
        let to_prune: Vec<surrealdb_types::RecordId> =
            rows.into_iter().skip(keep_runs).map(|row| row.id).collect();

        let mut pruned = 0u64;
        for run_id in to_prune {
            self.prune_one_run(&run_id).await?;
            pruned += 1;
        }
        Ok(pruned)
    }

    async fn prune_one_run(&self, run: &surrealdb_types::RecordId) -> Result<(), StoreError> {
        // §9: the summary itself is exempt from pruning, but the identity of
        // the node that generated it is not worth retaining a whole run for.
        //
        // Order matters: `review_finding.interview` is nulled *before* the
        // `interrogation` rows it points at are deleted. Reversing this would
        // delete the interrogation first, so `interview.run_node.run` could no
        // longer resolve through the (now-gone) link and the finding would
        // never get nulled — leaving a dangling reference instead of the NONE
        // §9 requires.
        let response = self
            .db
            .query(
                "UPDATE run_summary SET generated_by = NONE WHERE run = $run;\
                 UPDATE review_finding SET interview = NONE WHERE interview.run_node.run = $run;\
                 DELETE interrogation WHERE run_node.run = $run;\
                 DELETE node_checkpoint WHERE run = $run;\
                 DELETE run_event WHERE run = $run;\
                 DELETE spawned WHERE run = $run;\
                 DELETE run_edge WHERE run = $run;\
                 DELETE run_node WHERE run = $run;\
                 DELETE $run;",
            )
            .bind(("run", run.clone()))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }
}

// ── connection + error classification ───────────────────────────────────────

async fn connect(location: &StoreLocation) -> Result<Surreal<Db>, StoreError> {
    match location {
        StoreLocation::Memory => Surreal::new::<Mem>(())
            .await
            .map_err(|error| StoreError::Query(error.to_string())),
        StoreLocation::OnDisk(path) => {
            std::fs::create_dir_all(path)?;
            let path_str = path.to_string_lossy().into_owned();
            Surreal::new::<SurrealKv>(path_str.as_str())
                .await
                .map_err(|error| classify_open_error(path, error))
        }
    }
}

/// SurrealKv's lock file (`LOCK`, inside the database directory) holds the PID
/// of whichever process currently owns it — written by the process that
/// acquired the lock, for exactly this kind of diagnostic read. A failed
/// acquisition never overwrites it, so on a lock conflict the file still names
/// the current holder.
fn classify_open_error(path: &Path, error: surrealdb::Error) -> StoreError {
    let message = error.to_string();
    if message.contains("already locked") {
        let holder = std::fs::read_to_string(path.join("LOCK"))
            .ok()
            .map(|contents| contents.trim().to_string())
            .filter(|pid| !pid.is_empty());
        StoreError::store_locked(holder)
    } else {
        StoreError::Query(message)
    }
}

fn query_error(error: surrealdb::Error) -> StoreError {
    StoreError::Query(error.to_string())
}

fn migration_error(version: &str, error: surrealdb::Error) -> StoreError {
    StoreError::Migration {
        version: version.to_string(),
        message: error.to_string(),
    }
}

fn workflow_record_id(id: &WorkflowId) -> Result<surrealdb_types::RecordId, StoreError> {
    parse_record_id(TABLE_WORKFLOW, id.as_str())
        .ok_or_else(|| StoreError::Decode(format!("not a workflow id: {id}")))
}

// ── domain <-> schema string conversions ────────────────────────────────────

fn node_kind_str(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Agent => "agent",
        NodeKind::Internal => "internal",
        NodeKind::Gate => "gate",
        NodeKind::Monitor => "monitor",
    }
}

fn parse_node_kind(value: &str) -> Result<NodeKind, StoreError> {
    match value {
        "agent" => Ok(NodeKind::Agent),
        "internal" => Ok(NodeKind::Internal),
        "gate" => Ok(NodeKind::Gate),
        "monitor" => Ok(NodeKind::Monitor),
        other => Err(StoreError::Decode(format!("unknown node kind {other:?}"))),
    }
}

fn runner_str(runner: crate::workflow::model::Runner) -> &'static str {
    match runner {
        crate::workflow::model::Runner::Agent => "agent",
        crate::workflow::model::Runner::Command => "command",
    }
}

fn parse_runner(value: &str) -> Result<crate::workflow::model::Runner, StoreError> {
    match value {
        "agent" => Ok(crate::workflow::model::Runner::Agent),
        "command" => Ok(crate::workflow::model::Runner::Command),
        other => Err(StoreError::Decode(format!("unknown runner {other:?}"))),
    }
}

fn demand_str(demand: Demand) -> &'static str {
    match demand {
        Demand::Peak => "peak",
        Demand::Critical => "critical",
        Demand::Standard => "standard",
        Demand::Light => "light",
    }
}

pub(super) fn parse_demand(value: &str) -> Result<Demand, StoreError> {
    match value {
        "peak" => Ok(Demand::Peak),
        "critical" => Ok(Demand::Critical),
        "standard" => Ok(Demand::Standard),
        "light" => Ok(Demand::Light),
        other => Err(StoreError::Decode(format!("unknown demand {other:?}"))),
    }
}

fn isolation_str(isolation: Isolation) -> &'static str {
    match isolation {
        Isolation::None => "none",
        Isolation::Worktree => "worktree",
    }
}

fn parse_isolation(value: &str) -> Result<Isolation, StoreError> {
    match value {
        "none" => Ok(Isolation::None),
        "worktree" => Ok(Isolation::Worktree),
        other => Err(StoreError::Decode(format!("unknown isolation {other:?}"))),
    }
}

fn edge_kind_str(kind: EdgeKind) -> &'static str {
    match kind {
        EdgeKind::Sequence => "sequence",
        EdgeKind::Data => "data",
        EdgeKind::Conditional => "conditional",
    }
}

pub(super) fn parse_edge_kind(value: &str) -> Result<EdgeKind, StoreError> {
    match value {
        "sequence" => Ok(EdgeKind::Sequence),
        "data" => Ok(EdgeKind::Data),
        "conditional" => Ok(EdgeKind::Conditional),
        other => Err(StoreError::Decode(format!("unknown edge kind {other:?}"))),
    }
}

fn edge_payload_str(payload: EdgePayload) -> &'static str {
    match payload {
        EdgePayload::None => "none",
        EdgePayload::Summary => "summary",
        EdgePayload::Full => "full",
    }
}

fn parse_edge_payload(value: &str) -> Result<EdgePayload, StoreError> {
    match value {
        "none" => Ok(EdgePayload::None),
        "summary" => Ok(EdgePayload::Summary),
        "full" => Ok(EdgePayload::Full),
        other => Err(StoreError::Decode(format!(
            "unknown edge payload {other:?}"
        ))),
    }
}

pub(super) fn run_status_str(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Pending => "pending",
        RunStatus::Running => "running",
        RunStatus::Paused => "paused",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

pub(super) fn parse_run_status(value: &str) -> Result<RunStatus, StoreError> {
    match value {
        "pending" => Ok(RunStatus::Pending),
        "running" => Ok(RunStatus::Running),
        "paused" => Ok(RunStatus::Paused),
        "succeeded" => Ok(RunStatus::Succeeded),
        "failed" => Ok(RunStatus::Failed),
        "cancelled" => Ok(RunStatus::Cancelled),
        other => Err(StoreError::Decode(format!("unknown run status {other:?}"))),
    }
}

pub(super) fn node_status_str(status: NodeStatus) -> &'static str {
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

pub(super) fn parse_node_status(value: &str) -> Result<NodeStatus, StoreError> {
    match value {
        "pending" => Ok(NodeStatus::Pending),
        "ready" => Ok(NodeStatus::Ready),
        "running" => Ok(NodeStatus::Running),
        "needs_attention" => Ok(NodeStatus::NeedsAttention),
        "blocked" => Ok(NodeStatus::Blocked),
        "succeeded" => Ok(NodeStatus::Succeeded),
        "failed" => Ok(NodeStatus::Failed),
        "skipped" => Ok(NodeStatus::Skipped),
        "restored" => Ok(NodeStatus::Restored),
        "cancelled" => Ok(NodeStatus::Cancelled),
        other => Err(StoreError::Decode(format!("unknown node status {other:?}"))),
    }
}

fn evidence_str(evidence: Evidence) -> &'static str {
    match evidence {
        Evidence::SelfReport => "self_report",
        Evidence::Hook => "hook",
        Evidence::Detection => "detection",
        Evidence::Restored => "restored",
    }
}

pub(super) fn parse_evidence(value: &str) -> Result<Evidence, StoreError> {
    match value {
        "self_report" => Ok(Evidence::SelfReport),
        "hook" => Ok(Evidence::Hook),
        "detection" => Ok(Evidence::Detection),
        "restored" => Ok(Evidence::Restored),
        other => Err(StoreError::Decode(format!("unknown evidence {other:?}"))),
    }
}

pub(super) fn parse_succession(
    succession: Option<&str>,
    blocker: Option<&serde_json::Value>,
) -> Result<Option<Succession>, StoreError> {
    match succession {
        None => Ok(None),
        Some("satisfied") => Ok(Some(Succession::Satisfied)),
        Some("blocked") => {
            let blocker = blocker.ok_or_else(|| {
                StoreError::Decode("blocked succession with no blocker".to_string())
            })?;
            let reason = blocker
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            let resume_when = blocker
                .get("resume_when")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(Some(Succession::Blocked {
                reason,
                resume_when,
            }))
        }
        Some("no_followup") => {
            let evidence = blocker
                .and_then(|value| value.get("evidence"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(Some(Succession::NoFollowup { evidence }))
        }
        Some(other) => Err(StoreError::Decode(format!("unknown succession {other:?}"))),
    }
}

fn checkpoint_kind_str(kind: CheckpointKind) -> &'static str {
    match kind {
        CheckpointKind::Result => "result",
        CheckpointKind::Partial => "partial",
        CheckpointKind::ArtifactIndex => "artifact_index",
    }
}

fn run_event_kind_str(kind: RunEventKind) -> &'static str {
    kind.as_str()
}

/// §7: a payload over budget is never stored inline; the full text stays only
/// wherever the caller already spilled it (`artifact_paths`, which this does
/// not touch).
fn enforce_payload_budget(payload: serde_json::Value, budget_bytes: usize) -> serde_json::Value {
    let size = serde_json::to_string(&payload)
        .map(|text| text.len())
        .unwrap_or(0);
    if size > budget_bytes {
        serde_json::json!({"truncated": true, "original_bytes": size})
    } else {
        payload
    }
}

/// §7: truncated with an explicit marker; the full text stays in `payload`.
fn truncate_chars(text: String, budget_chars: usize) -> String {
    if text.chars().count() <= budget_chars {
        return text;
    }
    let mut truncated: String = text.chars().take(budget_chars).collect();
    truncated.push_str(" …[truncated]");
    truncated
}

// ── row <-> model conversions ───────────────────────────────────────────────

fn node_row_to_model(row: KvdagNodeRow) -> Result<KvdagNode, StoreError> {
    Ok(KvdagNode {
        key: NodeKey::new(row.node_key),
        label: row.label,
        role: row.role,
        kind: parse_node_kind(&row.kind)?,
        demand: parse_demand(&row.demand)?,
        runner: parse_runner(&row.runner)?,
        command: row.command,
        prompt_template: row.prompt_template,
        system_contract: row.system_contract,
        output_schema: OutputSchema::parse(row.output_schema)
            .map_err(|error| StoreError::Decode(error.to_string()))?,
        max_attempts: row.max_attempts as u8,
        timeout_ms: row.timeout_ms.map(|ms| ms as u64),
        isolation: parse_isolation(&row.isolation)?,
        is_template: row.is_template,
        expand_allow: row.expand_allow.into_iter().map(NodeKey::new).collect(),
        expand_max: row.expand_max as u16,
    })
}

fn edge_row_to_model(
    row: KvdagEdgeRow,
    from: NodeKey,
    to: NodeKey,
) -> Result<KvdagEdge, StoreError> {
    let condition = row
        .condition
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| StoreError::Decode(error.to_string()))?;
    Ok(KvdagEdge {
        from,
        to,
        kind: parse_edge_kind(&row.kind)?,
        condition,
        payload: parse_edge_payload(&row.payload)?,
        port: row.port,
    })
}

impl WorkflowStore {
    async fn select_workflow(
        &self,
        id: &surrealdb_types::RecordId,
    ) -> Result<Option<WorkflowRow>, StoreError> {
        let mut response = self
            .db
            .query("SELECT * FROM $id")
            .bind(("id", id.clone()))
            .await
            .map_err(query_error)?;
        let rows: Vec<WorkflowRow> = response.take(0).map_err(query_error)?;
        Ok(rows.into_iter().next())
    }

    async fn select_kvdag_version(
        &self,
        id: &surrealdb_types::RecordId,
    ) -> Result<Option<KvdagVersionRow>, StoreError> {
        let mut response = self
            .db
            .query("SELECT * FROM $id")
            .bind(("id", id.clone()))
            .await
            .map_err(query_error)?;
        let rows: Vec<KvdagVersionRow> = response.take(0).map_err(query_error)?;
        Ok(rows.into_iter().next())
    }

    async fn select_kvdag_nodes(
        &self,
        version_id: &surrealdb_types::RecordId,
    ) -> Result<Vec<KvdagNodeRow>, StoreError> {
        let mut response = self
            .db
            .query("SELECT * FROM kvdag_node WHERE version = $version ORDER BY node_key")
            .bind(("version", version_id.clone()))
            .await
            .map_err(query_error)?;
        response.take(0).map_err(query_error)
    }

    async fn select_kvdag_edges(
        &self,
        version_id: &surrealdb_types::RecordId,
    ) -> Result<Vec<KvdagEdgeRow>, StoreError> {
        let mut response = self
            .db
            .query("SELECT * FROM kvdag_edge WHERE in.version = $version ORDER BY in, out, port")
            .bind(("version", version_id.clone()))
            .await
            .map_err(query_error)?;
        response.take(0).map_err(query_error)
    }

    /// The workflow's highest-numbered version, which is the tip of its chain.
    async fn latest_version(
        &self,
        workflow_id: &surrealdb_types::RecordId,
    ) -> Result<Option<(KvdagVersionId, u32)>, StoreError> {
        let mut response = self
            .db
            .query(
                "SELECT * FROM kvdag_version WHERE workflow = $workflow \
                 ORDER BY version DESC LIMIT 1",
            )
            .bind(("workflow", workflow_id.clone()))
            .await
            .map_err(query_error)?;
        let rows: Vec<KvdagVersionRow> = response.take(0).map_err(query_error)?;
        Ok(rows.into_iter().next().map(|row| {
            (
                KvdagVersionId::new(record_id_to_string(&row.id)),
                row.version as u32,
            )
        }))
    }

    /// Materialises the static run-node/run-edge set for a freshly created run:
    /// one `run_node` per non-template `kvdag_node` (templates are only ever
    /// instantiated by an accepted expand proposal — Phase 2), roots `Ready`
    /// and everything else `Pending`, and one `run_edge` `RELATE` per
    /// `kvdag_edge` between two materialised nodes.
    ///
    /// `depth` is **expansion** depth, not topological depth: it pairs with
    /// `run_node.parent` (`03-storage-schema.md` §4.2, both `NONE`/`0` by
    /// default) and it is what `04-kvdag-and-execution.md` §3.4 guards with
    /// `parent.depth + 1 <= growth.max_depth` when an accepted expand proposal
    /// instantiates a template under a proposing node. A statically declared
    /// graph consumes none of that budget — with `max_depth` defaulting to 3,
    /// numbering a five-node chain 0..4 would report a legal graph as already
    /// past its own growth ceiling — so every statically materialised node is
    /// at depth 0, exactly as [`crate::workflow::model::RunGraph::materialise`]
    /// records it in memory.
    async fn materialise_run_nodes(
        &self,
        run_id: &surrealdb_types::RecordId,
        version_id: &surrealdb_types::RecordId,
        tier: Tier,
        graph: &Kvdag,
    ) -> Result<(), StoreError> {
        let scheduled: Vec<&KvdagNode> = graph.nodes.iter().filter(|n| !n.is_template).collect();
        let node_record_ids = self.select_kvdag_nodes(version_id).await?;
        let node_record_id_by_key: BTreeMap<NodeKey, surrealdb_types::RecordId> = node_record_ids
            .into_iter()
            .map(|row| (NodeKey::new(row.node_key.clone()), row.id))
            .collect();

        let mut run_node_id_by_key: BTreeMap<NodeKey, surrealdb_types::RecordId> = BTreeMap::new();

        for node in &scheduled {
            let inbound: Vec<&KvdagEdge> = graph.inbound_edges(&node.key).collect();
            let status = if inbound.is_empty() {
                NodeStatus::Ready
            } else {
                NodeStatus::Pending
            };
            let assignment = crate::workflow::tier::resolve(tier, node.demand, None);
            let kvdag_node_id = node_record_id_by_key.get(&node.key).ok_or_else(|| {
                StoreError::Decode(format!("node {} has no stored kvdag_node row", node.key))
            })?;

            let mut response = self
                .db
                .query(
                    "CREATE run_node SET run = $run, kvdag_node = $kvdag_node, \
                     node_key = $node_key, instance_path = $instance_path, \
                     depth = $depth, status = $status, model = $model, \
                     effort = $effort, demand = $demand RETURN AFTER",
                )
                .bind(("run", run_id.clone()))
                .bind(("kvdag_node", kvdag_node_id.clone()))
                .bind(("node_key", node.key.to_string()))
                .bind(("instance_path", node.key.to_string()))
                .bind(("depth", i64::from(STATIC_NODE_DEPTH)))
                .bind(("status", node_status_str(status).to_string()))
                .bind(("model", assignment.model.as_str().to_string()))
                .bind(("effort", assignment.effort.as_str().to_string()))
                .bind(("demand", demand_str(node.demand).to_string()))
                .await
                .map_err(query_error)?;
            let rows: Vec<records::RunNodeRow> = response.take(0).map_err(query_error)?;
            let row = rows
                .into_iter()
                .next()
                .ok_or_else(|| StoreError::Query("create run_node returned no row".to_string()))?;
            run_node_id_by_key.insert(node.key.clone(), row.id);
        }

        for edge in &graph.edges {
            let (Some(from_id), Some(to_id)) = (
                run_node_id_by_key.get(&edge.from),
                run_node_id_by_key.get(&edge.to),
            ) else {
                // One endpoint is a template node, never materialised in Phase 1.
                continue;
            };
            let kvdag_edge_id = self
                .find_kvdag_edge_id(&node_record_id_by_key, edge)
                .await?;
            let response = self
                .db
                .query(
                    "RELATE $from -> run_edge -> $to SET run = $run, kind = $kind, \
                     kvdag_edge = $kvdag_edge",
                )
                .bind(("from", from_id.clone()))
                .bind(("to", to_id.clone()))
                .bind(("run", run_id.clone()))
                .bind(("kind", edge_kind_str(edge.kind).to_string()))
                .bind(("kvdag_edge", kvdag_edge_id))
                .await
                .map_err(query_error)?;
            response.check().map_err(query_error)?;
        }
        Ok(())
    }

    async fn find_kvdag_edge_id(
        &self,
        node_record_id_by_key: &BTreeMap<NodeKey, surrealdb_types::RecordId>,
        edge: &KvdagEdge,
    ) -> Result<Option<surrealdb_types::RecordId>, StoreError> {
        let (Some(from), Some(to)) = (
            node_record_id_by_key.get(&edge.from),
            node_record_id_by_key.get(&edge.to),
        ) else {
            return Ok(None);
        };
        // The node pair alone does not identify an edge: nothing forbids two
        // edges between the same ordered pair (only a target's inbound *port*
        // names have to be unique), so `kind` and `port` are part of the match
        // or a run edge's provenance link can point at the wrong `kvdag_edge`.
        let mut response = self
            .db
            .query(
                "SELECT * FROM kvdag_edge WHERE in = $from AND out = $to \
                 AND kind = $kind AND port = $port LIMIT 1",
            )
            .bind(("from", from.clone()))
            .bind(("to", to.clone()))
            .bind(("kind", edge_kind_str(edge.kind).to_string()))
            .bind(("port", edge.port.clone()))
            .await
            .map_err(query_error)?;
        let rows: Vec<KvdagEdgeRow> = response.take(0).map_err(query_error)?;
        Ok(rows.into_iter().next().map(|row| row.id))
    }

    async fn find_run_node_id(
        &self,
        run: &RunId,
        path: &crate::workflow::model::InstancePath,
    ) -> Result<surrealdb_types::RecordId, StoreError> {
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let mut response = self
            .db
            .query("SELECT * FROM run_node WHERE run = $run AND instance_path = $path LIMIT 1")
            .bind(("run", run_id))
            .bind(("path", path.to_string()))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::RunNodeRow> = response.take(0).map_err(query_error)?;
        rows.into_iter()
            .next()
            .map(|row| row.id)
            .ok_or_else(|| StoreError::NotFound {
                table: TABLE_RUN_NODE,
                id: format!("{run}/{path}"),
            })
    }

    async fn write_run_event(
        &self,
        run: RunId,
        seq: u64,
        kind: RunEventKind,
        path: Option<crate::workflow::model::InstancePath>,
        payload: serde_json::Value,
    ) -> Result<(), StoreError> {
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let run_node_id = match &path {
            Some(path) => Some(self.find_run_node_id(&run, path).await?),
            None => None,
        };
        let payload = enforce_payload_budget(payload, RUN_EVENT_PAYLOAD_BUDGET_BYTES);
        let response = self
            .db
            .query(
                "CREATE run_event SET run = $run, seq = $seq, kind = $kind, \
                 run_node = $run_node, payload = $payload",
            )
            .bind(("run", run_id))
            .bind(("seq", seq as i64))
            .bind(("kind", run_event_kind_str(kind).to_string()))
            .bind(("run_node", run_node_id))
            .bind(("payload", payload))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    /// `ended_at_unix_ms` is the engine's own close stamp, which is also what
    /// the live run reports — the journal and the live projection must not
    /// describe the same run as ending at two different times. `time::now()`
    /// stays as the fallback so a terminal status that somehow arrives without
    /// a stamp still records *an* end time rather than none.
    async fn write_run_status(
        &self,
        run: RunId,
        status: RunStatus,
        ended_at_unix_ms: Option<u64>,
    ) -> Result<(), StoreError> {
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let terminal = matches!(
            status,
            RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled
        );
        let statement = if terminal {
            "UPDATE $run SET status = $status, \
             ended_at = IF $ended_at_ms = NONE THEN time::now() \
                        ELSE time::from_millis($ended_at_ms) END"
        } else {
            "UPDATE $run SET status = $status"
        };
        let response = self
            .db
            .query(statement)
            .bind(("run", run_id))
            .bind(("status", run_status_str(status).to_string()))
            .bind(("ended_at_ms", ended_at_unix_ms.map(|ms| ms as i64)))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    // The parameters are the destructured fields of `StoreWrite::RunNode`, so
    // grouping them into a struct would only re-wrap the variant this method
    // exists to unwrap.
    #[allow(clippy::too_many_arguments)]
    async fn write_run_node(
        &self,
        run: RunId,
        path: crate::workflow::model::InstancePath,
        status: NodeStatus,
        attempt: u8,
        binding: Option<NodeBinding>,
        usage: NodeUsage,
        evidence: Option<Evidence>,
        succession: Option<Succession>,
        started_at_unix_ms: Option<u64>,
        ended_at_unix_ms: Option<u64>,
    ) -> Result<(), StoreError> {
        let run_node_id = self.find_run_node_id(&run, &path).await?;
        let (succession_str, blocker_json) = match &succession {
            None => (None, None),
            Some(Succession::Satisfied) => (Some("satisfied"), None),
            Some(Succession::Blocked {
                reason,
                resume_when,
            }) => (
                Some("blocked"),
                Some(serde_json::json!({"reason": reason, "resume_when": resume_when})),
            ),
            Some(Succession::NoFollowup { evidence }) => (
                Some("no_followup"),
                Some(serde_json::json!({"evidence": evidence})),
            ),
        };

        let response = self
            .db
            .query(
                "UPDATE $id SET status = $status, attempt = $attempt, \
                 total_tokens = $total_tokens, tool_uses = $tool_uses, \
                 duration_ms = $duration_ms, evidence = $evidence, \
                 succession = $succession, blocker = $blocker, \
                 started_at = IF $started_at_ms = NONE THEN started_at \
                              ELSE time::from_millis($started_at_ms) END, \
                 ended_at = IF $ended_at_ms = NONE THEN ended_at \
                            ELSE time::from_millis($ended_at_ms) END",
            )
            .bind(("id", run_node_id.clone()))
            .bind(("status", node_status_str(status).to_string()))
            .bind(("attempt", i64::from(attempt)))
            .bind(("total_tokens", usage.total_tokens as i64))
            .bind(("tool_uses", i64::from(usage.tool_uses)))
            .bind(("duration_ms", usage.duration_ms as i64))
            .bind((
                "evidence",
                evidence.map(|value| evidence_str(value).to_string()),
            ))
            .bind(("succession", succession_str.map(str::to_string)))
            .bind(("blocker", blocker_json))
            .bind(("started_at_ms", started_at_unix_ms.map(|ms| ms as i64)))
            .bind(("ended_at_ms", ended_at_unix_ms.map(|ms| ms as i64)))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;

        if let Some(binding) = binding {
            let response = self
                .db
                .query(
                    "UPDATE $id SET pane_id = $pane_id, terminal_id = $terminal_id, \
                     agent_session_id = $agent_session_id, transcript_path = $transcript_path, \
                     node_dir = $node_dir, cwd = $cwd",
                )
                .bind(("id", run_node_id))
                .bind(("pane_id", binding.pane_id.to_string()))
                .bind(("terminal_id", binding.terminal_id.to_string()))
                .bind(("agent_session_id", binding.agent_session_id))
                .bind((
                    "transcript_path",
                    binding.transcript_path.to_string_lossy().into_owned(),
                ))
                .bind(("node_dir", binding.node_dir.to_string_lossy().into_owned()))
                .bind(("cwd", binding.cwd.to_string_lossy().into_owned()))
                .await
                .map_err(query_error)?;
            response.check().map_err(query_error)?;
        }

        self.refresh_nodes_done(&run).await
    }

    /// Re-derives `workflow_run.nodes_done` from the run's own `run_node`
    /// statuses.
    ///
    /// The counter is materialised on the run row rather than computed on read
    /// because `03-storage-schema.md` §4.2 declares it a stored `int` and §6's
    /// run-list projection selects it straight off `workflow_run` with no join
    /// to `run_node` — a run list has to stay one row read per run. Every node
    /// write refreshes it from the authoritative statuses instead of
    /// incrementing, so a node leaving a terminal status (a restart) or a
    /// replayed write can never leave the counter above or below the truth.
    async fn refresh_nodes_done(&self, run: &RunId) -> Result<(), StoreError> {
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let terminal: Vec<String> = TERMINAL_NODE_STATUSES
            .iter()
            .map(|status| node_status_str(*status).to_string())
            .collect();
        let response = self
            .db
            .query(
                "UPDATE $run SET nodes_done = array::len((SELECT VALUE id FROM run_node \
                 WHERE run = $run AND status IN $terminal))",
            )
            .bind(("run", run_id))
            .bind(("terminal", terminal))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    /// Settles one materialised edge's firing record.
    ///
    /// The edge is addressed by `(run, from, to, kind)` — the same identity
    /// `list_run_edges` reports it back under — rather than by its relation id,
    /// which the engine's in-memory graph never learns.
    ///
    /// `fired_at` is the *instant* of the firing, so a re-settled edge keeps
    /// the one it already has; an edge that goes back to unfired (§3.1 resolves
    /// a `Data` edge only while its source holds a validated result, and a
    /// restart clears that) drops the stamp instead of keeping one that no
    /// longer describes the run.
    async fn write_run_edge(
        &self,
        run: RunId,
        from: crate::workflow::model::InstancePath,
        to: crate::workflow::model::InstancePath,
        kind: EdgeKind,
        condition_result: Option<bool>,
        fired: bool,
    ) -> Result<(), StoreError> {
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let from_id = self.find_run_node_id(&run, &from).await?;
        let to_id = self.find_run_node_id(&run, &to).await?;
        let response = self
            .db
            .query(
                "UPDATE run_edge SET condition_result = $condition_result, \
                 fired_at = IF $fired THEN \
                     (IF fired_at = NONE THEN time::now() ELSE fired_at END) \
                 ELSE NONE END \
                 WHERE run = $run AND in = $from AND out = $to AND kind = $kind",
            )
            .bind(("run", run_id))
            .bind(("from", from_id))
            .bind(("to", to_id))
            .bind(("kind", edge_kind_str(kind).to_string()))
            .bind(("condition_result", condition_result))
            .bind(("fired", fired))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    // Same as `write_run_node`: these are the destructured fields of
    // `StoreWrite::Checkpoint`, not an argument list that grew by accident.
    #[allow(clippy::too_many_arguments)]
    async fn write_checkpoint(
        &self,
        run: RunId,
        path: crate::workflow::model::InstancePath,
        seq: u64,
        kind: CheckpointKind,
        schema_valid: bool,
        payload: serde_json::Value,
        summary: String,
        artifact_paths: Vec<String>,
        digest: String,
    ) -> Result<(), StoreError> {
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let run_node_id = self.find_run_node_id(&run, &path).await?;

        let mut response = self
            .db
            .query("SELECT * FROM $id")
            .bind(("id", run_node_id.clone()))
            .await
            .map_err(query_error)?;
        let node_rows: Vec<records::RunNodeRow> = response.take(0).map_err(query_error)?;
        let node_row = node_rows
            .into_iter()
            .next()
            .ok_or_else(|| StoreError::NotFound {
                table: TABLE_RUN_NODE,
                id: format!("{run}/{path}"),
            })?;

        let mut response = self
            .db
            .query("SELECT * FROM $id")
            .bind(("id", run_id.clone()))
            .await
            .map_err(query_error)?;
        let run_rows: Vec<records::RunRow> = response.take(0).map_err(query_error)?;
        let run_row = run_rows
            .into_iter()
            .next()
            .ok_or_else(|| StoreError::NotFound {
                table: TABLE_WORKFLOW_RUN,
                id: run.to_string(),
            })?;

        // §7: the inline payload is capped; a caller that already spilled a
        // large result to `artifact_paths` still gets that path stored, but
        // the store never lets an oversized payload land inline regardless.
        let payload = enforce_payload_budget(payload, CHECKPOINT_PAYLOAD_BUDGET_BYTES);
        let summary = truncate_chars(summary, SUMMARY_BUDGET_CHARS);

        let response = self
            .db
            .query(
                "CREATE node_checkpoint SET run = $run, run_node = $run_node, \
                 node_key = $node_key, instance_path = $instance_path, \
                 kvdag_version = $kvdag_version, seq = $seq, kind = $kind, \
                 schema_valid = $schema_valid, payload = $payload, summary = $summary, \
                 artifact_paths = $artifact_paths, digest = $digest",
            )
            .bind(("run", run_id))
            .bind(("run_node", run_node_id))
            .bind(("node_key", node_row.node_key))
            .bind(("instance_path", path.to_string()))
            .bind(("kvdag_version", run_row.kvdag_version))
            .bind(("seq", seq as i64))
            .bind(("kind", checkpoint_kind_str(kind).to_string()))
            .bind(("schema_valid", schema_valid))
            .bind(("payload", payload))
            .bind(("summary", summary))
            .bind(("artifact_paths", artifact_paths))
            .bind(("digest", digest))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_location_is_under_the_state_dir() {
        let StoreLocation::OnDisk(path) = WorkflowStore::default_location() else {
            panic!("the default location is on disk");
        };
        if std::env::var_os(DB_PATH_ENV).is_some() {
            return;
        }
        assert!(path.ends_with("workflow"), "{path:?}");
        assert_eq!(
            path.parent(),
            Some(crate::config::state_dir().as_path()),
            "state_dir() already carries the app directory name"
        );
    }

    #[test]
    fn version_origin_strings_match_the_schema_assertion() {
        assert_eq!(VersionOrigin::Authored.as_str(), "authored");
        assert_eq!(VersionOrigin::SelfImprovement.as_str(), "self_improvement");
        assert_eq!(VersionOrigin::RestoreRewrite.as_str(), "restore_rewrite");
    }

    #[test]
    fn version_origin_parse_round_trips_every_variant() {
        for origin in [
            VersionOrigin::Authored,
            VersionOrigin::Imported,
            VersionOrigin::SelfImprovement,
            VersionOrigin::RestoreRewrite,
        ] {
            let parsed = VersionOrigin::parse(origin.as_str()).expect("known origin string");
            assert_eq!(parsed, origin);
        }
    }

    #[test]
    fn version_origin_parse_rejects_garbage() {
        assert!(VersionOrigin::parse("not-a-real-origin").is_err());
    }

    /// `nodes_done` is recomputed with a `status IN $terminal` comparison, so
    /// the string set it binds has to be exactly the statuses the model calls
    /// terminal. A new `NodeStatus` variant that closes a node and is not
    /// added here would silently stop being counted.
    #[test]
    fn terminal_node_statuses_match_the_model() {
        // Deliberately exhaustive rather than a slice of variants: adding a
        // `NodeStatus` stops this compiling until someone decides whether the
        // new status closes a node, which is exactly the decision
        // `TERMINAL_NODE_STATUSES` encodes.
        const EVERY_NODE_STATUS: [NodeStatus; 10] = [
            NodeStatus::Pending,
            NodeStatus::Ready,
            NodeStatus::Running,
            NodeStatus::NeedsAttention,
            NodeStatus::Blocked,
            NodeStatus::Succeeded,
            NodeStatus::Failed,
            NodeStatus::Skipped,
            NodeStatus::Restored,
            NodeStatus::Cancelled,
        ];
        fn closes_a_node(status: NodeStatus) -> bool {
            match status {
                NodeStatus::Succeeded
                | NodeStatus::Failed
                | NodeStatus::Skipped
                | NodeStatus::Restored
                | NodeStatus::Cancelled => true,
                NodeStatus::Pending
                | NodeStatus::Ready
                | NodeStatus::Running
                | NodeStatus::NeedsAttention
                | NodeStatus::Blocked => false,
            }
        }
        for status in EVERY_NODE_STATUS {
            assert_eq!(
                closes_a_node(status),
                status.is_terminal(),
                "{status:?} disagrees with NodeStatus::is_terminal"
            );
            assert_eq!(
                TERMINAL_NODE_STATUSES.contains(&status),
                status.is_terminal(),
                "{status:?} is missing from the nodes_done terminal set"
            );
        }
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod store_tests;
