//! Embedded persistence for workflows, versions, runs, and the run journal.
//!
//! Only this module talks to the database
//! (`docs/design/workflow-builder/03-storage-schema.md`). The engine below it
//! and the API above it both see typed Rust values and never a key, a row, or a
//! transaction. The store is compiled unconditionally: there is one karvex
//! binary and the workflow subsystem is always in it.
//!
//! The backing store is [redb] — an embedded, pure-Rust, ACID key/value store
//! with no C toolchain, no network stack, and no TLS or JWT dependencies. The
//! key layout that stands in for the old SQL schema is documented in `db.rs`.
//!
//! Three properties are structural, not conventional:
//!
//! - **Append-only by construction.** `kvdag_version`, `kvdag_node`,
//!   `kvdag_edge`, `node_checkpoint`, and `run_event` have no update and no
//!   delete method here. There is no API to call.
//! - **Whole-run retention only.** [`WorkflowStore::prune_run_history`] is the
//!   single deleting entry point, and every row it can address is keyed by the
//!   run that owns it, so there is no key shape that removes part of a
//!   retained run.
//! - **One writer.** The database file is exclusively locked while open, so a
//!   second karvex server reports [`StoreError::Unavailable`] with reason
//!   [`error::STORE_LOCKED`] instead of racing the first one.
//!
//! Every method is `async` because the caller is
//! `src/app/workflow_store.rs`'s store thread, which drives them through
//! `block_on`; the work inside is synchronous local I/O.
//!
//! [redb]: https://www.redb.org

mod db;
pub mod error;
mod queries;
mod records;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub use error::StoreError;
// Read-surface record types: no in-crate caller for every one of them yet, but
// this is a binary crate, so nothing exempts a `pub use` from the unused-import
// lint on its own the way it would in a published library.
#[allow(unused_imports)]
pub use queries::{
    CheckpointRecord, RunEventRecord, RunNodeRecord, RunRecord, RunSummaryRecord, WorkflowSummary,
};

use crate::workflow::model::{
    CheckpointKind, Demand, EdgeKind, EdgePayload, Evidence, GrowthLimits, InstancePath, Isolation,
    Kvdag, KvdagEdge, KvdagNode, KvdagSpec, KvdagVersionId, NodeBinding, NodeKey, NodeKind,
    NodeStatus, NodeUsage, OutputSchema, RunEventKind, RunId, RunStatus, StoreWrite, Succession,
    WorkflowId,
};
use crate::workflow::tier::Tier;
use db::RowReader as _;
use records::{
    parse_record_id, record_id_to_string, CheckpointRow, InterrogationRow, KvdagEdgeRow,
    KvdagNodeRow, KvdagVersionRow, ReviewCycleRow, ReviewFindingRow, RunEdgeRow, RunEventRow,
    RunNodeRow, RunRow, RunSummaryRow, WorkflowRow,
};
use redb::ReadableTable as _;

/// Overrides the database file; primarily for tests and for pointing a debug
/// build at a release build's history.
pub const DB_PATH_ENV: &str = "KARVEX_WORKFLOW_DB_PATH";

/// The database file name under `state_dir()`. `0.9.0`'s directory-shaped
/// SurrealKv store is not read and not migrated; a workflow history from that
/// build is left where it is rather than half-converted.
const DB_FILE_NAME: &str = "workflow.redb";

/// Written next to the database while it is open, so a server that loses the
/// race for the lock can name the process holding it. Only ever read after the
/// lock has already been refused.
const OWNER_FILE_SUFFIX: &str = ".owner";

pub(super) const TABLE_WORKFLOW: &str = "workflow";
pub(super) const TABLE_KVDAG_VERSION: &str = "kvdag_version";
const TABLE_KVDAG_NODE: &str = "kvdag_node";
pub(super) const TABLE_WORKFLOW_RUN: &str = "workflow_run";
const TABLE_RUN_NODE: &str = "run_node";
const TABLE_RUN_SUMMARY: &str = "run_summary";
const TABLE_INTERROGATION: &str = "interrogation";
const TABLE_REVIEW_CYCLE: &str = "review_cycle";
const TABLE_REVIEW_FINDING: &str = "review_finding";

/// Payload budgets (`03-storage-schema.md` §7): token efficiency is a schema
/// property, enforced here rather than left to caller discipline.
const CHECKPOINT_PAYLOAD_BUDGET_BYTES: usize = 256 * 1024;
const RUN_EVENT_PAYLOAD_BUDGET_BYTES: usize = 16 * 1024;
const SUMMARY_BUDGET_CHARS: usize = 1_200;

type Migration = fn(&redb::WriteTransaction) -> Result<(), StoreError>;

/// Every migration, applied in order and recorded in `schema_meta`. The version
/// string is both the `schema_meta` key and this module's audit trail of what
/// has ever shipped.
const MIGRATIONS: &[(&str, Migration)] = &[("0001_init", migrate_0001_init)];

/// Brings every table into existence. redb creates a table the first time a
/// write transaction opens it, so this is what lets every later *read*
/// transaction assume its table is there.
fn migrate_0001_init(txn: &redb::WriteTransaction) -> Result<(), StoreError> {
    for table in db::ROW_TABLES {
        txn.open_table(*table).map_err(db::storage_error)?;
    }
    txn.open_table(db::SEQUENCE).map_err(db::storage_error)?;
    Ok(())
}

/// Where a store's data lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreLocation {
    /// A redb database file. Exclusively locked while open.
    OnDisk(PathBuf),
    /// A database on redb's in-memory backend. Store and engine tests use this
    /// so they touch no disk; it is never a fallback for a locked on-disk
    /// store.
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
}

/// The workflow database. Opened lazily on first `workflow.*` use, so a karvex
/// that never touches workflows never pays the open cost.
#[derive(Debug)]
pub struct WorkflowStore {
    location: StoreLocation,
    db: redb::Database,
}

impl WorkflowStore {
    /// `state_dir()` already appends the app directory name, so nothing extra
    /// is joined here. Debug builds resolve to `karvex-dev` and therefore use a
    /// different database from an installed release build.
    pub fn default_location() -> StoreLocation {
        match std::env::var_os(DB_PATH_ENV) {
            Some(path) if !path.is_empty() => StoreLocation::OnDisk(PathBuf::from(path)),
            _ => StoreLocation::OnDisk(crate::config::state_dir().join(DB_FILE_NAME)),
        }
    }

    /// Opens the database and applies pending migrations. Returns
    /// [`StoreError::Unavailable`] with reason [`error::STORE_LOCKED`] when
    /// another karvex server owns the file.
    pub async fn open(location: StoreLocation) -> Result<Self, StoreError> {
        let db = connect(&location)?;
        let store = Self { location, db };
        store.migrate().await?;
        Ok(store)
    }

    pub fn location(&self) -> &StoreLocation {
        &self.location
    }

    /// Applies every unapplied migration in order, each in its own transaction,
    /// and records the applied set in `schema_meta`. Re-applying is a no-op:
    /// already-applied versions are skipped.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        let applied = self.applied_migrations()?;
        for (version, apply) in MIGRATIONS {
            if applied.contains(*version) {
                continue;
            }
            self.apply_migration(version, *apply)?;
        }
        Ok(())
    }

    fn applied_migrations(&self) -> Result<BTreeSet<String>, StoreError> {
        let read = self.read()?;
        let table = match read.open_table(db::SCHEMA_META) {
            Ok(table) => table,
            // On a brand-new database `schema_meta` itself does not exist yet —
            // it is created BY migration 0001. That one failure means "no
            // migrations applied"; anything else is a real error.
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(BTreeSet::new()),
            Err(error) => return Err(db::storage_error(error)),
        };
        let mut applied = BTreeSet::new();
        for entry in table.iter().map_err(db::storage_error)? {
            let (version, _) = entry.map_err(db::storage_error)?;
            applied.insert(version.value().to_string());
        }
        Ok(applied)
    }

    /// The schema change and its `schema_meta` marker share one transaction, so
    /// a failed migration leaves no partial schema behind.
    fn apply_migration(&self, version: &str, apply: Migration) -> Result<(), StoreError> {
        let txn = self
            .db
            .begin_write()
            .map_err(|error| migration_error(version, error))?;
        apply(&txn).map_err(|error| migration_error(version, error))?;
        {
            let mut meta = txn
                .open_table(db::SCHEMA_META)
                .map_err(|error| migration_error(version, error))?;
            meta.insert(version, db::now_ms().unsigned_abs())
                .map_err(|error| migration_error(version, error))?;
        }
        txn.commit()
            .map_err(|error| migration_error(version, error))?;
        Ok(())
    }

    pub async fn create_workflow(
        &self,
        name: &str,
        description: &str,
        default_tier: Tier,
    ) -> Result<WorkflowId, StoreError> {
        if name.is_empty() {
            return Err(StoreError::Query("a workflow needs a name".to_string()));
        }
        // The old schema enforced this with a unique index on `name`. Nothing
        // in the key layout does, so the check is explicit — dropping it would
        // silently allow two workflows a user cannot tell apart.
        if self
            .list_workflows()
            .await?
            .iter()
            .any(|workflow| workflow.name == name)
        {
            return Err(StoreError::Query(format!(
                "a workflow named {name:?} already exists"
            )));
        }

        let now = db::now_ms();
        let txn = self.write_txn()?;
        let key = {
            let mut counters = txn.open_table(db::SEQUENCE).map_err(db::storage_error)?;
            db::workflow_key(db::next_counter(&mut counters, db::SEQ_WORKFLOW)?)
        };
        {
            let mut workflows = txn.open_table(db::WORKFLOW).map_err(db::storage_error)?;
            let row = WorkflowRow {
                id: key.clone(),
                name: name.to_string(),
                description: description.to_string(),
                head_version: None,
                default_tier: default_tier.as_str().to_string(),
                archived: false,
                created_at: now,
                updated_at: now,
            };
            db::insert_new(&mut workflows, &key, &row, TABLE_WORKFLOW)?;
        }
        commit(txn)?;
        Ok(WorkflowId::new(record_id_to_string(TABLE_WORKFLOW, &key)))
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
        let workflow_key = workflow_record_id(workflow)?;
        let workflow_row =
            self.select_workflow(&workflow_key)?
                .ok_or_else(|| StoreError::NotFound {
                    table: TABLE_WORKFLOW,
                    id: workflow.to_string(),
                })?;

        // Identity fields (version number, parent) are the store's to assign;
        // the caller supplies content (contract/growth/args/nodes/edges) and,
        // optionally, an explicit parent override for a non-linear origin
        // (e.g. a future `restore_rewrite`).
        let latest = self.latest_version(&workflow_key)?;
        let next_version = latest.as_ref().map_or(1, |(_, version)| version + 1);
        let explicit_parent = spec.parent.clone();
        let parent = explicit_parent.clone().or_else(|| {
            workflow_row
                .head_version
                .as_deref()
                .map(|key| KvdagVersionId::new(record_id_to_string(TABLE_KVDAG_VERSION, key)))
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
        let version_key = db::version_key(&workflow_key, validated.version);
        let now = db::now_ms();

        let txn = self.write_txn()?;
        {
            let mut versions = txn
                .open_table(db::KVDAG_VERSION)
                .map_err(db::storage_error)?;
            let row = KvdagVersionRow {
                id: version_key.clone(),
                workflow: workflow_key.clone(),
                version: i64::from(validated.version),
                parent: parent
                    .as_ref()
                    .and_then(|id| parse_record_id(TABLE_KVDAG_VERSION, id.as_str())),
                origin: origin.as_str().to_string(),
                change_summary: change_summary.to_string(),
                contract: validated.contract.clone(),
                args: args_json,
                max_depth: i64::from(validated.growth.max_depth),
                max_nodes: i64::from(validated.growth.max_nodes),
                spec_digest: validated.spec_digest.to_string(),
                created_at: now,
            };
            db::insert_new(&mut versions, &version_key, &row, TABLE_KVDAG_VERSION)?;
        }
        {
            let mut nodes = txn.open_table(db::KVDAG_NODE).map_err(db::storage_error)?;
            for node in &validated.nodes {
                let key = db::child_key(&version_key, node.key.as_str());
                let row = node_model_to_row(&key, &version_key, node);
                db::insert_new(&mut nodes, &key, &row, TABLE_KVDAG_NODE)?;
            }
        }
        {
            let mut edges = txn.open_table(db::KVDAG_EDGE).map_err(db::storage_error)?;
            for edge in &validated.edges {
                let key = kvdag_edge_key(&version_key, edge);
                let condition = edge
                    .condition
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()
                    .map_err(|error| StoreError::Query(error.to_string()))?;
                let row = KvdagEdgeRow {
                    id: key.clone(),
                    r#in: db::child_key(&version_key, edge.from.as_str()),
                    out: db::child_key(&version_key, edge.to.as_str()),
                    kind: edge_kind_str(edge.kind).to_string(),
                    condition,
                    payload: edge_payload_str(edge.payload).to_string(),
                    port: edge.port.clone(),
                };
                db::insert_new(&mut edges, &key, &row, "kvdag_edge")?;
            }
        }
        commit(txn)?;

        self.load_version(&KvdagVersionId::new(record_id_to_string(
            TABLE_KVDAG_VERSION,
            &version_key,
        )))
        .await
    }

    /// Advances the workflow's head pointer. `workflow` is the one mutable
    /// definition record; the version it points at is immutable.
    pub async fn set_head_version(
        &self,
        workflow: &WorkflowId,
        version: &KvdagVersionId,
    ) -> Result<(), StoreError> {
        let workflow_key = workflow_record_id(workflow)?;
        let version_key = parse_record_id(TABLE_KVDAG_VERSION, version.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a kvdag_version id: {version}")))?;
        let mut row = self
            .select_workflow(&workflow_key)?
            .ok_or_else(|| StoreError::NotFound {
                table: TABLE_WORKFLOW,
                id: workflow.to_string(),
            })?;
        row.head_version = Some(version_key);
        row.updated_at = db::now_ms();

        let txn = self.write_txn()?;
        {
            let mut workflows = txn.open_table(db::WORKFLOW).map_err(db::storage_error)?;
            db::put_row(&mut workflows, &workflow_key, &row)?;
        }
        commit(txn)
    }

    /// Loads one version's full node and edge set back into a validated graph.
    pub async fn load_version(&self, version: &KvdagVersionId) -> Result<Kvdag, StoreError> {
        let version_key = parse_record_id(TABLE_KVDAG_VERSION, version.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a kvdag_version id: {version}")))?;
        let read = self.read()?;
        let versions = read
            .open_table(db::KVDAG_VERSION)
            .map_err(db::storage_error)?;
        let version_row: KvdagVersionRow =
            db::get_row(&versions, &version_key)?.ok_or_else(|| StoreError::NotFound {
                table: TABLE_KVDAG_VERSION,
                id: version.to_string(),
            })?;

        // Both scans come back in key order, which is node key order for nodes
        // and (from, to, kind, port) for edges — deterministic, so a reloaded
        // version is byte-identical to the one `create_version` returned.
        let prefix = db::child_prefix(&version_key);
        let nodes_table = read.open_table(db::KVDAG_NODE).map_err(db::storage_error)?;
        let node_rows: Vec<KvdagNodeRow> = db::scan_prefix(&nodes_table, &prefix)?;
        let edges_table = read.open_table(db::KVDAG_EDGE).map_err(db::storage_error)?;
        let edge_rows: Vec<KvdagEdgeRow> = db::scan_prefix(&edges_table, &prefix)?;

        let mut node_key_by_id: BTreeMap<String, NodeKey> = BTreeMap::new();
        for row in &node_rows {
            node_key_by_id.insert(row.id.clone(), NodeKey::new(row.node_key.clone()));
        }
        let mut nodes = Vec::with_capacity(node_rows.len());
        for row in node_rows {
            nodes.push(node_row_to_model(row)?);
        }

        let mut edges = Vec::with_capacity(edge_rows.len());
        for row in edge_rows {
            let from = node_key_by_id
                .get(&row.r#in)
                .cloned()
                .ok_or_else(|| StoreError::Decode("edge references an unknown node".to_string()))?;
            let to = node_key_by_id
                .get(&row.out)
                .cloned()
                .ok_or_else(|| StoreError::Decode("edge references an unknown node".to_string()))?;
            edges.push(edge_row_to_model(row, from, to)?);
        }

        let args: Vec<crate::workflow::model::ArgSpec> =
            serde_json::from_value(version_row.args)
                .map_err(|error| StoreError::Decode(error.to_string()))?;

        let spec = KvdagSpec {
            version_id: KvdagVersionId::new(record_id_to_string(
                TABLE_KVDAG_VERSION,
                &version_row.id,
            )),
            workflow_id: WorkflowId::new(record_id_to_string(
                TABLE_WORKFLOW,
                &version_row.workflow,
            )),
            version: version_row.version as u32,
            parent: version_row
                .parent
                .as_deref()
                .map(|key| KvdagVersionId::new(record_id_to_string(TABLE_KVDAG_VERSION, key))),
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
        let workflow_key = workflow_record_id(&run.workflow)?;
        let version_key =
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
        let context_runs: Vec<String> = run
            .context_runs
            .iter()
            .filter_map(|id| parse_record_id(TABLE_WORKFLOW_RUN, id.as_str()))
            .collect();

        // Read the version's stored nodes and edges before opening the write
        // transaction: the run's materialised graph is built from what was
        // actually persisted, not from the caller's copy of it.
        let stored_nodes: BTreeMap<NodeKey, String> = {
            let read = self.read()?;
            let table = read.open_table(db::KVDAG_NODE).map_err(db::storage_error)?;
            let rows: Vec<KvdagNodeRow> = db::scan_prefix(&table, &db::child_prefix(&version_key))?;
            rows.into_iter()
                .map(|row| (NodeKey::new(row.node_key), row.id))
                .collect()
        };
        let stored_edges: BTreeSet<String> = {
            let read = self.read()?;
            let table = read.open_table(db::KVDAG_EDGE).map_err(db::storage_error)?;
            db::scan_prefix_keys(&table, &db::child_prefix(&version_key))?
                .into_iter()
                .collect()
        };

        let now = db::now_ms();
        let txn = self.write_txn()?;
        let run_key = {
            let mut counters = txn.open_table(db::SEQUENCE).map_err(db::storage_error)?;
            db::run_key(&workflow_key, db::next_counter(&mut counters, db::SEQ_RUN)?)
        };
        {
            let mut runs = txn
                .open_table(db::WORKFLOW_RUN)
                .map_err(db::storage_error)?;
            let row = RunRow {
                id: run_key.clone(),
                workflow: workflow_key,
                kvdag_version: version_key.clone(),
                tier: run.tier.as_str().to_string(),
                status: run_status_str(RunStatus::Pending).to_string(),
                args: args_json,
                context_runs,
                restore_from: None,
                max_depth: i64::from(run.growth.max_depth),
                max_nodes: i64::from(run.growth.max_nodes),
                workspace_id: None,
                tab_id: None,
                started_at: now,
                ended_at: None,
                total_tokens: 0,
                total_tool_uses: 0,
                nodes_total: graph.nodes.iter().filter(|node| !node.is_template).count() as i64,
                nodes_done: 0,
                failure: None,
            };
            db::insert_new(&mut runs, &run_key, &row, TABLE_WORKFLOW_RUN)?;
        }
        materialise_run_nodes(
            &txn,
            &run_key,
            &version_key,
            run.tier,
            &graph,
            &stored_nodes,
            &stored_edges,
        )?;
        commit(txn)?;

        Ok(RunId::new(record_id_to_string(
            TABLE_WORKFLOW_RUN,
            &run_key,
        )))
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
            } => self.write_run_event(run, seq, kind, path, payload),
            StoreWrite::RunStatus { run, status } => self.write_run_status(run, status),
            StoreWrite::RunNode {
                run,
                path,
                status,
                attempt,
                binding,
                usage,
                evidence,
                succession,
            } => self.write_run_node(
                run, path, status, attempt, binding, usage, evidence, succession,
            ),
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
            } => self.write_checkpoint(
                run,
                path,
                seq,
                kind,
                schema_valid,
                payload,
                summary,
                artifact_paths,
                digest,
            ),
        }
    }

    /// Deletes whole runs beyond the retention window and returns how many were
    /// removed. Every `run_summary` survives, and no dangling reference to a
    /// removed record is left behind.
    pub async fn prune_run_history(
        &self,
        workflow: &WorkflowId,
        keep_runs: usize,
    ) -> Result<u64, StoreError> {
        let workflow_key = workflow_record_id(workflow)?;
        let read = self.read()?;
        let runs = read
            .open_table(db::WORKFLOW_RUN)
            .map_err(db::storage_error)?;
        // Run keys sort in creation order, so reversing is `ORDER BY
        // started_at DESC` with a deterministic same-millisecond tiebreak.
        let mut keys = db::scan_prefix_keys(&runs, &db::run_prefix(&workflow_key))?;
        drop(runs);
        drop(read);
        keys.reverse();

        let mut pruned = 0u64;
        for run_key in keys.into_iter().skip(keep_runs) {
            self.prune_one_run(&run_key)?;
            pruned += 1;
        }
        Ok(pruned)
    }

    fn prune_one_run(&self, run_key: &str) -> Result<(), StoreError> {
        let child_prefix = db::child_prefix(run_key);

        // §9: the summary itself is exempt from pruning, but the identity of
        // the node that generated it is not worth retaining a whole run for.
        // Both back-references are rewritten *before* the records they point at
        // are removed, so no reader can ever observe one pointing at a deleted
        // record.
        let summary: Option<RunSummaryRow> = {
            let read = self.read()?;
            let table = read
                .open_table(db::RUN_SUMMARY)
                .map_err(db::storage_error)?;
            db::get_row(&table, run_key)?
        };
        let orphaned_findings: Vec<ReviewFindingRow> = {
            let read = self.read()?;
            let table = read
                .open_table(db::REVIEW_FINDING)
                .map_err(db::storage_error)?;
            let all: Vec<ReviewFindingRow> = db::scan_prefix(&table, "")?;
            all.into_iter()
                .filter(|finding| {
                    finding
                        .interview
                        .as_deref()
                        .is_some_and(|id| id.starts_with(&child_prefix))
                })
                .collect()
        };

        let txn = self.write_txn()?;
        if let Some(mut summary) = summary {
            summary.generated_by = None;
            let mut table = txn.open_table(db::RUN_SUMMARY).map_err(db::storage_error)?;
            db::put_row(&mut table, run_key, &summary)?;
        }
        if !orphaned_findings.is_empty() {
            let mut table = txn
                .open_table(db::REVIEW_FINDING)
                .map_err(db::storage_error)?;
            for mut finding in orphaned_findings {
                let key = finding.id.clone();
                finding.interview = None;
                db::put_row(&mut table, &key, &finding)?;
            }
        }
        for table in [
            db::INTERROGATION,
            db::NODE_CHECKPOINT,
            db::RUN_EVENT,
            db::RUN_EDGE,
            db::RUN_NODE,
        ] {
            let mut table = txn.open_table(table).map_err(db::storage_error)?;
            db::delete_prefix(&mut table, &child_prefix)?;
        }
        {
            let mut runs = txn
                .open_table(db::WORKFLOW_RUN)
                .map_err(db::storage_error)?;
            runs.remove(run_key).map_err(db::storage_error)?;
        }
        commit(txn)
    }
}

// ── run summary, interrogation, review (no Phase 1 engine writer) ───────────

impl WorkflowStore {
    /// Records a run's summary. `03` §9: a summary outlives the run it
    /// describes, so it is keyed by the run and exempt from pruning.
    pub(super) async fn create_run_summary(
        &self,
        run: &RunId,
        version: &KvdagVersionId,
        text: &str,
        outcome: &str,
    ) -> Result<(), StoreError> {
        let run_key = run_record_id(run)?;
        let version_key = parse_record_id(TABLE_KVDAG_VERSION, version.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a kvdag_version id: {version}")))?;
        let row = RunSummaryRow {
            id: run_key.clone(),
            run: run_key.clone(),
            kvdag_version: version_key,
            text: text.to_string(),
            outcome: outcome.to_string(),
            highlights: Vec::new(),
            open_gaps: Vec::new(),
            per_node: Vec::new(),
            token_estimate: 0,
            generated_by: None,
            created_at: db::now_ms(),
        };
        let txn = self.write_txn()?;
        {
            let mut table = txn.open_table(db::RUN_SUMMARY).map_err(db::storage_error)?;
            db::insert_new(&mut table, &run_key, &row, TABLE_RUN_SUMMARY)?;
        }
        commit(txn)
    }

    /// Points an existing summary at the node instance that produced it.
    pub(super) async fn set_run_summary_generated_by(
        &self,
        run: &RunId,
        path: &InstancePath,
    ) -> Result<(), StoreError> {
        let run_key = run_record_id(run)?;
        let mut row: RunSummaryRow = {
            let read = self.read()?;
            let table = read
                .open_table(db::RUN_SUMMARY)
                .map_err(db::storage_error)?;
            db::get_row(&table, &run_key)?.ok_or_else(|| StoreError::NotFound {
                table: TABLE_RUN_SUMMARY,
                id: run.to_string(),
            })?
        };
        row.generated_by = Some(db::child_key(&run_key, path.as_str()));
        let txn = self.write_txn()?;
        {
            let mut table = txn.open_table(db::RUN_SUMMARY).map_err(db::storage_error)?;
            db::put_row(&mut table, &run_key, &row)?;
        }
        commit(txn)?;
        Ok(())
    }

    /// Records one forked-session interrogation of a node. Keyed under the node
    /// it interrogates, so it is pruned with that node's run.
    pub(super) async fn create_interrogation(
        &self,
        run: &RunId,
        path: &InstancePath,
        source_session_id: &str,
        forked_session_id: &str,
        cwd: &str,
    ) -> Result<String, StoreError> {
        let run_node_key = self.find_run_node_key(run, path)?;
        let txn = self.write_txn()?;
        let key = {
            let mut counters = txn.open_table(db::SEQUENCE).map_err(db::storage_error)?;
            let counter = db::next_counter(&mut counters, db::SEQ_INTERROGATION)?;
            db::child_key(&run_node_key, &format!("i{counter:012x}"))
        };
        {
            let mut table = txn
                .open_table(db::INTERROGATION)
                .map_err(db::storage_error)?;
            let row = InterrogationRow {
                id: key.clone(),
                run_node: run_node_key,
                source_session_id: source_session_id.to_string(),
                forked_session_id: forked_session_id.to_string(),
                transcript_path: None,
                cwd: cwd.to_string(),
                pane_id: None,
                started_at: db::now_ms(),
                ended_at: None,
                note: String::new(),
                reconstructed: false,
                seeded_from: None,
            };
            db::insert_new(&mut table, &key, &row, TABLE_INTERROGATION)?;
        }
        commit(txn)?;
        Ok(key)
    }

    pub(super) async fn create_review_cycle(
        &self,
        run: &RunId,
        version: &KvdagVersionId,
        status: &str,
    ) -> Result<String, StoreError> {
        let run_key = run_record_id(run)?;
        let version_key = parse_record_id(TABLE_KVDAG_VERSION, version.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a kvdag_version id: {version}")))?;
        let txn = self.write_txn()?;
        let key = {
            let mut counters = txn.open_table(db::SEQUENCE).map_err(db::storage_error)?;
            let counter = db::next_counter(&mut counters, db::SEQ_REVIEW_CYCLE)?;
            format!("c{counter:012x}")
        };
        {
            let mut table = txn
                .open_table(db::REVIEW_CYCLE)
                .map_err(db::storage_error)?;
            let row = ReviewCycleRow {
                id: key.clone(),
                run: run_key,
                kvdag_version: version_key,
                status: status.to_string(),
                interviews: Vec::new(),
                started_at: db::now_ms(),
                ended_at: None,
            };
            db::insert_new(&mut table, &key, &row, TABLE_REVIEW_CYCLE)?;
        }
        commit(txn)?;
        Ok(key)
    }

    /// A `replace` verdict without a replacement is refused. The old schema
    /// enforced this with a table event; nothing in a key/value layout can, so
    /// it is a check here — a review that says "replace this node" and does not
    /// say what with is not a reviewable finding, it is a lost one.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn create_review_finding(
        &self,
        cycle: &str,
        node_key: &NodeKey,
        interview: Option<&str>,
        level: &str,
        verdict: &str,
        rationale: &str,
        replacement: Option<serde_json::Value>,
    ) -> Result<String, StoreError> {
        if verdict == "replace" && replacement.is_none() {
            return Err(StoreError::Query(
                "a \"replace\" verdict needs a replacement".to_string(),
            ));
        }
        let txn = self.write_txn()?;
        let key = {
            let mut counters = txn.open_table(db::SEQUENCE).map_err(db::storage_error)?;
            let counter = db::next_counter(&mut counters, db::SEQ_REVIEW_FINDING)?;
            db::child_key(cycle, &format!("f{counter:012x}"))
        };
        {
            let mut table = txn
                .open_table(db::REVIEW_FINDING)
                .map_err(db::storage_error)?;
            let row = ReviewFindingRow {
                id: key.clone(),
                cycle: cycle.to_string(),
                node_key: node_key.to_string(),
                interview: interview.map(str::to_string),
                interview_mode: if interview.is_some() {
                    "resumed".to_string()
                } else {
                    "evidence_only".to_string()
                },
                level: level.to_string(),
                verdict: verdict.to_string(),
                rationale: rationale.to_string(),
                replacement,
                evidence: serde_json::Value::Object(serde_json::Map::new()),
                proposed_change: serde_json::Value::Object(serde_json::Map::new()),
                accepted: false,
            };
            db::insert_new(&mut table, &key, &row, TABLE_REVIEW_FINDING)?;
        }
        commit(txn)?;
        Ok(key)
    }
}

// ── connection + error classification ───────────────────────────────────────

fn connect(location: &StoreLocation) -> Result<redb::Database, StoreError> {
    match location {
        StoreLocation::Memory => redb::Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .map_err(|error| StoreError::Query(error.to_string())),
        StoreLocation::OnDisk(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match redb::Database::create(path) {
                Ok(database) => {
                    record_lock_owner(path);
                    Ok(database)
                }
                Err(error) => Err(classify_open_error(path, &error)),
            }
        }
    }
}

fn owner_path(path: &Path) -> PathBuf {
    let mut owner = path.as_os_str().to_os_string();
    owner.push(OWNER_FILE_SUFFIX);
    PathBuf::from(owner)
}

/// redb's own lock is an advisory file lock with no payload, so the holder's
/// identity is recorded alongside it. Best-effort: failing to write it costs a
/// diagnostic, never the open.
fn record_lock_owner(path: &Path) {
    let _ = std::fs::write(owner_path(path), format!("pid {}", std::process::id()));
}

/// A refused lock is the one open failure that takes the whole subsystem out of
/// service with a name attached, so it is classified rather than reported as a
/// generic query failure. The owner file is only consulted here: a lock that
/// was refused is a lock somebody still holds, so what it names is current.
fn classify_open_error(path: &Path, error: &redb::DatabaseError) -> StoreError {
    if matches!(error, redb::DatabaseError::DatabaseAlreadyOpen) {
        let holder = std::fs::read_to_string(owner_path(path))
            .ok()
            .map(|contents| contents.trim().to_string())
            .filter(|holder| !holder.is_empty());
        StoreError::store_locked(holder)
    } else {
        StoreError::Query(error.to_string())
    }
}

fn migration_error(version: &str, error: impl std::fmt::Display) -> StoreError {
    StoreError::Migration {
        version: version.to_string(),
        message: error.to_string(),
    }
}

fn commit(txn: redb::WriteTransaction) -> Result<(), StoreError> {
    txn.commit().map_err(db::storage_error)
}

fn workflow_record_id(id: &WorkflowId) -> Result<String, StoreError> {
    parse_record_id(TABLE_WORKFLOW, id.as_str())
        .ok_or_else(|| StoreError::Decode(format!("not a workflow id: {id}")))
}

fn run_record_id(id: &RunId) -> Result<String, StoreError> {
    parse_record_id(TABLE_WORKFLOW_RUN, id.as_str())
        .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {id}")))
}

fn kvdag_edge_key(version_key: &str, edge: &KvdagEdge) -> String {
    // The node pair alone does not identify an edge: nothing forbids two edges
    // between the same ordered pair (only a target's inbound *port* names have
    // to be unique), so `kind` and `port` are part of the key or two edges
    // would collide on one record.
    db::edge_key(
        version_key,
        edge.from.as_str(),
        edge.to.as_str(),
        edge_kind_str(edge.kind),
        edge.port.as_deref().unwrap_or_default(),
    )
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

fn parse_edge_kind(value: &str) -> Result<EdgeKind, StoreError> {
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

fn node_model_to_row(key: &str, version_key: &str, node: &KvdagNode) -> KvdagNodeRow {
    KvdagNodeRow {
        id: key.to_string(),
        version: version_key.to_string(),
        node_key: node.key.to_string(),
        label: node.label.clone(),
        role: node.role.clone(),
        kind: node_kind_str(node.kind).to_string(),
        runner: runner_str(node.runner).to_string(),
        command: node.command.clone(),
        demand: demand_str(node.demand).to_string(),
        prompt_template: node.prompt_template.clone(),
        system_contract: node.system_contract.clone(),
        output_schema: node.output_schema.as_value().clone(),
        max_attempts: i64::from(node.max_attempts),
        timeout_ms: node.timeout_ms.map(|ms| ms as i64),
        isolation: isolation_str(node.isolation).to_string(),
        is_template: node.is_template,
        expand_allow: node.expand_allow.iter().map(NodeKey::to_string).collect(),
        expand_max: i64::from(node.expand_max),
        position: None,
    }
}

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

// ── reads and writes behind the public surface ──────────────────────────────

impl WorkflowStore {
    pub(super) fn read(&self) -> Result<redb::ReadTransaction, StoreError> {
        use redb::ReadableDatabase;
        self.db.begin_read().map_err(db::storage_error)
    }

    fn write_txn(&self) -> Result<redb::WriteTransaction, StoreError> {
        self.db.begin_write().map_err(db::storage_error)
    }

    fn select_workflow(&self, key: &str) -> Result<Option<WorkflowRow>, StoreError> {
        let read = self.read()?;
        let table = read.open_table(db::WORKFLOW).map_err(db::storage_error)?;
        db::get_row(&table, key)
    }

    /// The workflow's highest-numbered version, which is the tip of its chain.
    /// Version keys embed a zero-padded version number, so the tip is simply
    /// the last key in the workflow's range.
    fn latest_version(
        &self,
        workflow_key: &str,
    ) -> Result<Option<(KvdagVersionId, u32)>, StoreError> {
        let read = self.read()?;
        let table = read
            .open_table(db::KVDAG_VERSION)
            .map_err(db::storage_error)?;
        let keys = db::scan_prefix_keys(&table, &db::version_prefix(workflow_key))?;
        let Some(key) = keys.last() else {
            return Ok(None);
        };
        let row: KvdagVersionRow = db::get_row(&table, key)?
            .ok_or_else(|| StoreError::Decode(format!("kvdag_version {key} vanished mid-read")))?;
        Ok(Some((
            KvdagVersionId::new(record_id_to_string(TABLE_KVDAG_VERSION, &row.id)),
            row.version as u32,
        )))
    }

    pub(super) fn find_run_node_key(
        &self,
        run: &RunId,
        path: &InstancePath,
    ) -> Result<String, StoreError> {
        let run_key = run_record_id(run)?;
        let key = db::child_key(&run_key, path.as_str());
        let read = self.read()?;
        let table = read.open_table(db::RUN_NODE).map_err(db::storage_error)?;
        if table.row_exists(&key)? {
            Ok(key)
        } else {
            Err(StoreError::NotFound {
                table: TABLE_RUN_NODE,
                id: format!("{run}/{path}"),
            })
        }
    }

    fn write_run_event(
        &self,
        run: RunId,
        seq: u64,
        kind: RunEventKind,
        path: Option<InstancePath>,
        payload: serde_json::Value,
    ) -> Result<(), StoreError> {
        let run_key = run_record_id(&run)?;
        let run_node_key = match &path {
            Some(path) => Some(self.find_run_node_key(&run, path)?),
            None => None,
        };
        let payload = enforce_payload_budget(payload, RUN_EVENT_PAYLOAD_BUDGET_BYTES);
        let key = db::seq_key(&run_key, seq);
        let row = RunEventRow {
            id: key.clone(),
            run: run_key,
            seq: seq as i64,
            at: db::now_ms(),
            kind: run_event_kind_str(kind).to_string(),
            run_node: run_node_key,
            payload,
        };
        let txn = self.write_txn()?;
        {
            let mut table = txn.open_table(db::RUN_EVENT).map_err(db::storage_error)?;
            // The old schema's unique `(run, seq)` index, expressed as the key
            // itself: a repeated sequence number is refused rather than
            // silently overwriting the event already journalled under it.
            db::insert_new(&mut table, &key, &row, "run_event")?;
        }
        commit(txn)
    }

    fn write_run_status(&self, run: RunId, status: RunStatus) -> Result<(), StoreError> {
        let run_key = run_record_id(&run)?;
        let mut row = self
            .select_run(&run_key)?
            .ok_or_else(|| StoreError::NotFound {
                table: TABLE_WORKFLOW_RUN,
                id: run.to_string(),
            })?;
        row.status = run_status_str(status).to_string();
        if matches!(
            status,
            RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled
        ) {
            row.ended_at = Some(db::now_ms());
        }
        let txn = self.write_txn()?;
        {
            let mut table = txn
                .open_table(db::WORKFLOW_RUN)
                .map_err(db::storage_error)?;
            db::put_row(&mut table, &run_key, &row)?;
        }
        commit(txn)
    }

    // The parameters are the destructured fields of `StoreWrite::RunNode`, so
    // grouping them into a struct would only re-wrap the variant this method
    // exists to unwrap.
    #[allow(clippy::too_many_arguments)]
    fn write_run_node(
        &self,
        run: RunId,
        path: InstancePath,
        status: NodeStatus,
        attempt: u8,
        binding: Option<NodeBinding>,
        usage: NodeUsage,
        evidence: Option<Evidence>,
        succession: Option<Succession>,
    ) -> Result<(), StoreError> {
        let key = self.find_run_node_key(&run, &path)?;
        let mut row = self
            .select_run_node(&key)?
            .ok_or_else(|| StoreError::NotFound {
                table: TABLE_RUN_NODE,
                id: format!("{run}/{path}"),
            })?;

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

        row.status = node_status_str(status).to_string();
        row.attempt = i64::from(attempt);
        row.total_tokens = usage.total_tokens as i64;
        row.tool_uses = i64::from(usage.tool_uses);
        row.duration_ms = usage.duration_ms as i64;
        row.evidence = evidence.map(|value| evidence_str(value).to_string());
        row.succession = succession_str.map(str::to_string);
        row.blocker = blocker_json;

        if let Some(binding) = binding {
            row.pane_id = Some(binding.pane_id.to_string());
            row.terminal_id = Some(binding.terminal_id.to_string());
            row.agent_session_id = Some(binding.agent_session_id);
            row.transcript_path = Some(binding.transcript_path.to_string_lossy().into_owned());
            row.node_dir = Some(binding.node_dir.to_string_lossy().into_owned());
            row.cwd = Some(binding.cwd.to_string_lossy().into_owned());
            // First binding wins: a re-bind after a retry must not move the
            // node's start time forward and shrink its measured duration.
            row.started_at = row.started_at.or_else(|| Some(db::now_ms()));
        }

        let txn = self.write_txn()?;
        {
            let mut table = txn.open_table(db::RUN_NODE).map_err(db::storage_error)?;
            db::put_row(&mut table, &key, &row)?;
        }
        commit(txn)
    }

    // Same as `write_run_node`: these are the destructured fields of
    // `StoreWrite::Checkpoint`, not an argument list that grew by accident.
    #[allow(clippy::too_many_arguments)]
    fn write_checkpoint(
        &self,
        run: RunId,
        path: InstancePath,
        seq: u64,
        kind: CheckpointKind,
        schema_valid: bool,
        payload: serde_json::Value,
        summary: String,
        artifact_paths: Vec<String>,
        digest: String,
    ) -> Result<(), StoreError> {
        let run_key = run_record_id(&run)?;
        let run_node_key = self.find_run_node_key(&run, &path)?;
        let node_row =
            self.select_run_node(&run_node_key)?
                .ok_or_else(|| StoreError::NotFound {
                    table: TABLE_RUN_NODE,
                    id: format!("{run}/{path}"),
                })?;
        let run_row = self
            .select_run(&run_key)?
            .ok_or_else(|| StoreError::NotFound {
                table: TABLE_WORKFLOW_RUN,
                id: run.to_string(),
            })?;

        // §7: the inline payload is capped; a caller that already spilled a
        // large result to `artifact_paths` still gets that path stored, but
        // the store never lets an oversized payload land inline regardless.
        let payload = enforce_payload_budget(payload, CHECKPOINT_PAYLOAD_BUDGET_BYTES);
        let summary = truncate_chars(summary, SUMMARY_BUDGET_CHARS);

        let key = db::checkpoint_key(&run_key, path.as_str(), seq);
        let row = CheckpointRow {
            id: key.clone(),
            run: run_key,
            run_node: run_node_key,
            node_key: node_row.node_key,
            instance_path: path.to_string(),
            kvdag_version: run_row.kvdag_version,
            seq: seq as i64,
            kind: checkpoint_kind_str(kind).to_string(),
            schema_valid,
            payload,
            summary,
            artifact_paths,
            digest,
            created_at: db::now_ms(),
        };
        let txn = self.write_txn()?;
        {
            let mut table = txn
                .open_table(db::NODE_CHECKPOINT)
                .map_err(db::storage_error)?;
            db::insert_new(&mut table, &key, &row, "node_checkpoint")?;
        }
        commit(txn)
    }

    pub(super) fn select_run(&self, key: &str) -> Result<Option<RunRow>, StoreError> {
        let read = self.read()?;
        let table = read
            .open_table(db::WORKFLOW_RUN)
            .map_err(db::storage_error)?;
        db::get_row(&table, key)
    }

    pub(super) fn select_run_node(&self, key: &str) -> Result<Option<RunNodeRow>, StoreError> {
        let read = self.read()?;
        let table = read.open_table(db::RUN_NODE).map_err(db::storage_error)?;
        db::get_row(&table, key)
    }
}

/// Materialises the static run-node/run-edge set for a freshly created run:
/// one `run_node` per non-template `kvdag_node` (templates are only ever
/// instantiated by an accepted expand proposal — Phase 2), roots `Ready` and
/// everything else `Pending`, and one `run_edge` per `kvdag_edge` between two
/// materialised nodes. `depth` is the longest path from any root, computed in
/// one forward pass over `graph.nodes` because `Kvdag::try_new` already
/// topologically sorted them.
fn materialise_run_nodes(
    txn: &redb::WriteTransaction,
    run_key: &str,
    version_key: &str,
    tier: Tier,
    graph: &Kvdag,
    stored_nodes: &BTreeMap<NodeKey, String>,
    stored_edges: &BTreeSet<String>,
) -> Result<(), StoreError> {
    let scheduled: Vec<&KvdagNode> = graph
        .nodes
        .iter()
        .filter(|node| !node.is_template)
        .collect();
    let mut depth_by_key: BTreeMap<NodeKey, u16> = BTreeMap::new();
    let mut run_node_key_by_key: BTreeMap<NodeKey, String> = BTreeMap::new();
    let now = db::now_ms();

    {
        let mut table = txn.open_table(db::RUN_NODE).map_err(db::storage_error)?;
        for node in &scheduled {
            let inbound: Vec<&KvdagEdge> = graph.inbound_edges(&node.key).collect();
            let depth = inbound
                .iter()
                .filter_map(|edge| depth_by_key.get(&edge.from))
                .max()
                .map_or(0, |max| max + 1);
            depth_by_key.insert(node.key.clone(), depth);

            let status = if inbound.is_empty() {
                NodeStatus::Ready
            } else {
                NodeStatus::Pending
            };
            let assignment = crate::workflow::tier::resolve(tier, node.demand, None);
            let kvdag_node_key = stored_nodes.get(&node.key).ok_or_else(|| {
                StoreError::Decode(format!("node {} has no stored kvdag_node row", node.key))
            })?;

            let key = db::child_key(run_key, node.key.as_str());
            let row = RunNodeRow {
                id: key.clone(),
                run: run_key.to_string(),
                kvdag_node: kvdag_node_key.clone(),
                node_key: node.key.to_string(),
                instance_path: node.key.to_string(),
                parent: None,
                depth: i64::from(depth),
                status: node_status_str(status).to_string(),
                model: assignment.model.as_str().to_string(),
                effort: assignment.effort.as_str().to_string(),
                demand: demand_str(node.demand).to_string(),
                attempt: 1,
                pane_id: None,
                terminal_id: None,
                agent_session_id: None,
                transcript_path: None,
                cwd: None,
                node_dir: None,
                started_at: None,
                ended_at: None,
                total_tokens: 0,
                tool_uses: 0,
                duration_ms: 0,
                evidence: None,
                succession: None,
                blocker: None,
                restored_from: None,
                watchdog_interventions: 0,
            };
            db::insert_new(&mut table, &key, &row, TABLE_RUN_NODE)?;
            run_node_key_by_key.insert(node.key.clone(), key);
        }
    }

    let mut table = txn.open_table(db::RUN_EDGE).map_err(db::storage_error)?;
    for edge in &graph.edges {
        let (Some(from), Some(to)) = (
            run_node_key_by_key.get(&edge.from),
            run_node_key_by_key.get(&edge.to),
        ) else {
            // One endpoint is a template node, never materialised in Phase 1.
            continue;
        };
        let kvdag_edge_key = kvdag_edge_key(version_key, edge);
        let key = db::run_edge_key(
            run_key,
            edge.from.as_str(),
            edge.to.as_str(),
            edge_kind_str(edge.kind),
        );
        let row = RunEdgeRow {
            id: key.clone(),
            r#in: from.clone(),
            out: to.clone(),
            run: run_key.to_string(),
            kind: edge_kind_str(edge.kind).to_string(),
            kvdag_edge: stored_edges
                .contains(&kvdag_edge_key)
                .then_some(kvdag_edge_key),
            condition_result: None,
            fired_at: Some(now),
        };
        // Two edges between the same ordered pair with the same kind collapse
        // to one run edge; they differ only by port, which the run edge does
        // not carry, so the second is the same fact as the first.
        db::put_row(&mut table, &key, &row)?;
    }
    Ok(())
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
        assert!(path.ends_with(DB_FILE_NAME), "{path:?}");
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
    fn the_owner_file_sits_beside_the_database_not_inside_it() {
        let path = Path::new("/state/workflow.redb");
        assert_eq!(
            owner_path(path),
            PathBuf::from("/state/workflow.redb.owner")
        );
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod store_tests;
