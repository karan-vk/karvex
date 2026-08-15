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
    CheckpointRecord, InterrogationRecord, NodeEvidence, RestorableCheckpoint, ReviewCycleRecord,
    ReviewFindingRecord, RunEdgeRecord, RunEventRecord, RunMemberRecord, RunNodeRecord, RunRecord,
    RunSummaryRecord, StoredGrowthLimit, StoredGrowthLimits, VersionRecord, WatchdogJournalEntry,
    WorkflowSummary, DEFAULT_NODE_HISTORY_RUNS,
};
use sha2::{Digest, Sha256};
use surrealdb::engine::local::{Db, Mem, SurrealKv};
use surrealdb::Surreal;

use crate::workflow::model::{
    canonical, is_reserved_path, Attention, CheckpointKind, Demand, EdgeKind, EdgePayload,
    Evidence, GrowthLimits, InstancePath, InterrogationId, InterviewMode, Isolation, Kvdag,
    KvdagEdge, KvdagNode, KvdagSpec, KvdagVersionId, NodeAssignment, NodeBinding, NodeKey,
    NodeKind, NodeStatus, NodeUsage, OutputSchema, RestoredRef, RestoredSeed, ReviewCycleId,
    ReviewCycleStatus, ReviewFindingSeed, RunEventKind, RunId, RunStatus, StoreWrite, Succession,
    SummaryNodeLine, WorkflowId, RESERVED_PATH_PREFIX,
};
use crate::workflow::tier::Assignment;
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
const TABLE_INTERROGATION: &str = "interrogation";
const TABLE_RUN_MEMBER: &str = "run_member";
const TABLE_REVIEW_CYCLE: &str = "review_cycle";

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
const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_init", include_str!("migrations/0001_init.surql")),
    (
        "0002_growth_and_history",
        include_str!("migrations/0002_growth_and_history.surql"),
    ),
    (
        "0003_node_identity",
        include_str!("migrations/0003_node_identity.surql"),
    ),
    (
        "0004_journal_time_and_interrogation",
        include_str!("migrations/0004_journal_time_and_interrogation.surql"),
    ),
    (
        "0005_lead_binding_and_projection",
        include_str!("migrations/0005_lead_binding_and_projection.surql"),
    ),
    (
        "0006_member_identity_and_review",
        include_str!("migrations/0006_member_identity_and_review.surql"),
    ),
];

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

/// Definition metadata that lives on the mutable `workflow` row rather than on
/// the immutable version, passed to
/// [`WorkflowStore::create_version_with_metadata`] so the row tracks its head
/// (`06-phase2-plan.md` H5 / §4 D16).
///
/// `kvdag_version` has no `description` column and deliberately gains none: it
/// is the immutable graph revision, and a second copy of the description would
/// be a second authority behind one `workflow.get`. `default_tier` is the same
/// story — Phase 1 already stores it on `workflow` (§4 D17), and the only gap
/// was that a new version never refreshed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionMetadata {
    pub description: String,
    pub default_tier: Tier,
}

/// The inputs of one run, checked against the version's limits on create: a run
/// narrows its version's growth limits and never widens them.
///
/// `Eq` dropped (kept through Phase 2): `restored: Vec<RestoredSeed>` carries
/// `serde_json::Value`, which is `PartialEq`-only.
#[derive(Debug, Clone, PartialEq)]
pub struct NewRun {
    pub workflow: WorkflowId,
    pub version: KvdagVersionId,
    pub tier: Tier,
    pub args: BTreeMap<String, String>,
    pub growth: GrowthLimits,
    /// When the run started, stamped **once** by the caller and bound
    /// explicitly by [`WorkflowStore::create_run`].
    ///
    /// `workflow_run.started_at` lost its `DEFAULT time::now()` in migration
    /// `0002`, so the database can no longer mint a competing clock: the run
    /// the journal describes and the run the live projection describes start at
    /// the same instant (`06-phase2-plan.md` H1 / §4 D15).
    pub started_at_unix_ms: u64,
    /// Every kvdag node's resolved `(model, effort, reason)`, **including
    /// templates**, produced once at run start by
    /// `crate::workflow::engine::graph::resolve_assignments`.
    ///
    /// [`WorkflowStore::create_run`] writes these verbatim — the store resolves
    /// no tiers of its own, so the DB row and the DAG view cannot disagree
    /// about which model a node ran on (`06-phase2-plan.md` §4 D9 / R-2). A
    /// scheduled node missing from this table is
    /// [`StoreError::Invariant`], not a silent fallback.
    pub assignments: BTreeMap<NodeKey, NodeAssignment>,
    pub context_runs: Vec<RunId>,
    /// Where the run's panes live, as the public API workspace id
    /// (`03-storage-schema.md` §4.2). Recorded at create time because it is a
    /// property of the run, not of the server that happens to be executing it:
    /// without it a run read back from the journal has no workspace binding at
    /// all.
    pub workspace_id: Option<String>,
    /// What this run's caller asked to restore, if anything — persisted
    /// verbatim into `workflow_run.restore_from`. Carries the full request
    /// (`run`, the selectors asked for, `allow_changed`), not just the
    /// source run id: `RestoredRef` on a seeded node records what
    /// *happened*, but a selector that was asked for and skipped has no
    /// other durable trace once the transient API response is gone.
    /// `RunRecord::restore_from_run` still exposes only the source id — the
    /// rest is audit trail, not a read path with a consumer today.
    pub restore_from: Option<RestoreFromRequest>,
    /// Restored node seeds, keyed by the **target** version's node key
    /// (`RestoredSeed::node_key`). Consumed by [`WorkflowStore::materialise_run_nodes`],
    /// which persists each seeded node's full terminal shape up front rather
    /// than waiting for engine-driven updates that a `Restored` node — which
    /// never runs — would never produce (`07-phase3-plan.md` §1 WS-B).
    pub restored: Vec<RestoredSeed>,
}

/// The durable half of a restore request (`07-phase3-plan.md` §1 WS-B,
/// judgment call approved by phase3-planner-f): what a run's caller asked
/// for, recorded alongside what actually happened (`RestoredRef` on each
/// seeded node). `nodes` holds the selectors as given — `Vec<String>`, not
/// `Vec<NodeKey>`, because an unknown selector is exactly the case this
/// exists to keep a record of, and typing it would imply a validation this
/// struct does not perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreFromRequest {
    pub run: RunId,
    pub nodes: Vec<String>,
    pub allow_changed: bool,
}

/// The destructured fields of [`StoreWrite::RunNodeCreated`], grouped so the
/// writer that consumes them is not an eleven-argument function.
///
/// Deliberately not a second public shape: it exists only between `write`'s
/// match arm and [`WorkflowStore::write_run_node_created`], and its field set
/// is the variant's field set.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunNodeCreate {
    run: RunId,
    key: NodeKey,
    path: InstancePath,
    label: String,
    inputs: BTreeMap<String, String>,
    parent: Option<InstancePath>,
    depth: u16,
    status: NodeStatus,
    demand: Demand,
    assignment: Assignment,
    assignment_reason: String,
    attempt: u8,
    proposal_id: String,
}

/// The destructured fields of [`StoreWrite::RunTaskProjected`], grouped for the
/// same reason as [`RunNodeCreate`]: the writer would otherwise be a
/// ten-argument function.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskProjection {
    run: RunId,
    path: InstancePath,
    node_key: NodeKey,
    task_id: String,
    subject: String,
    owner: String,
    status: NodeStatus,
    emergent: bool,
    blocked_by: Vec<InstancePath>,
    observed_at_unix_ms: u64,
}

/// The destructured fields of [`StoreWrite::RunMemberSnapshot`], grouped for
/// the same reason as [`TaskProjection`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct MemberSnapshot {
    run: RunId,
    name: String,
    agent_type: String,
    model: String,
    pane_id: Option<String>,
    backend_type: String,
    is_active: bool,
    cwd: Option<String>,
    /// The teammate identity migration `0006` (P7) added columns for
    /// (`phase4-retarget-plan.md` §3.3, S1); written with the
    /// never-regress-to-unknown idiom `write_run_member_snapshot` documents.
    session_id: Option<String>,
    transcript_path: Option<String>,
    last_state: Option<String>,
    last_state_at_unix_ms: Option<u64>,
    observed_at_unix_ms: u64,
}

/// A flat `string -> string` map as the JSON object the column holds. The same
/// shape `workflow_run.args` is written in, so `run_node.inputs` reads back the
/// way every other authored map does.
fn string_map_json(map: &BTreeMap<String, String>) -> serde_json::Value {
    serde_json::Value::Object(
        map.iter()
            .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
            .collect(),
    )
}

/// The inverse of [`string_map_json`]. A value that is not a string is rendered
/// as its JSON text rather than dropped: a slot override that reached the
/// column in an unexpected shape should still reach the prompt, because the
/// alternative is the silent discard this column exists to end.
fn string_map_from_json(value: &serde_json::Value) -> BTreeMap<String, String> {
    value
        .as_object()
        .map(|object| {
            object
                .iter()
                .map(|(key, value)| {
                    let text = match value {
                        serde_json::Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    (key.clone(), text)
                })
                .collect()
        })
        .unwrap_or_default()
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

    /// Opens a store with only the first `count` migrations applied, so a test
    /// can build rows the way an older karvex did and then watch `migrate()`
    /// bring them forward. Nothing in production selects a migration subset.
    #[cfg(test)]
    pub(super) async fn open_with_migrations(
        location: StoreLocation,
        count: usize,
    ) -> Result<Self, StoreError> {
        let db = connect(&location).await?;
        db.use_ns(NAMESPACE)
            .use_db(DATABASE)
            .await
            .map_err(query_error)?;
        let store = Self { location, db };
        for (version, sql) in MIGRATIONS.iter().take(count) {
            store.apply_migration(version, sql).await?;
        }
        Ok(store)
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
            .map_err(|error| create_workflow_error(name, error))?;
        let rows: Vec<WorkflowRow> = response
            .take(0)
            .map_err(|error| create_workflow_error(name, error))?;
        let row = rows
            .into_iter()
            .next()
            .ok_or_else(|| StoreError::Query("create workflow returned no row".to_string()))?;
        Ok(WorkflowId::new(record_id_to_string(&row.id)))
    }

    /// Writes a new immutable version plus its nodes and edges, and returns the
    /// validated graph. An identical graph yields the same `spec_digest` and is
    /// not written again.
    ///
    /// The metadata-free entry point, kept so a caller that is only revising
    /// the graph does not have to invent a description. Callers that hold the
    /// authored document — every `workflow.create`/`workflow.update` path —
    /// use [`Self::create_version_with_metadata`] instead, so the `workflow`
    /// row tracks its head (H5).
    pub async fn create_version(
        &self,
        workflow: &WorkflowId,
        origin: VersionOrigin,
        change_summary: &str,
        spec: KvdagSpec,
    ) -> Result<Kvdag, StoreError> {
        self.create_version_with_metadata(workflow, origin, change_summary, spec, None)
            .await
    }

    /// [`Self::create_version`] plus the H5 head-metadata refresh.
    ///
    /// `kvdag_version` stores neither `description` nor `default_tier`, so
    /// after a `workflow.update` that changed either, `workflow.get` used to
    /// report v1's metadata beside `head_version: 2`. The fix is to make the
    /// mutable `workflow` row track its head rather than to add a second
    /// authority to the immutable revision (`06-phase2-plan.md` §4 D16).
    ///
    /// The refresh happens **before** the no-op-revision early return on
    /// purpose: an update that changes only the description leaves the graph
    /// digest identical, which is exactly the case that used to go missing.
    /// It happens **after** `Kvdag::try_new` for the opposite reason: a
    /// rejected document must not leave the workflow row describing itself with
    /// metadata from a revision that was never written.
    pub async fn create_version_with_metadata(
        &self,
        workflow: &WorkflowId,
        origin: VersionOrigin,
        change_summary: &str,
        spec: KvdagSpec,
        metadata: Option<&VersionMetadata>,
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

        // Only now: everything above this line is a read or a pure validation,
        // so a document that fails the gate leaves the workflow exactly as it
        // was found. Before this moved, a rejected `workflow.update` still
        // rewrote the row's `description`/`default_tier` to describe a revision
        // that was never written.
        if let Some(metadata) = metadata {
            self.refresh_workflow_metadata(&workflow_id, metadata)
                .await?;
        }

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

    /// Writes the authored document's `description` and `default_tier` onto
    /// the `workflow` row (H5). Idempotent, and a no-op for a workflow id that
    /// does not resolve.
    async fn refresh_workflow_metadata(
        &self,
        workflow_id: &surrealdb_types::RecordId,
        metadata: &VersionMetadata,
    ) -> Result<(), StoreError> {
        let response = self
            .db
            .query(
                "UPDATE $workflow SET description = $description, \
                 default_tier = $default_tier, updated_at = time::now()",
            )
            .bind(("workflow", workflow_id.clone()))
            .bind(("description", metadata.description.clone()))
            .bind(("default_tier", metadata.default_tier.as_str().to_string()))
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

    /// Creates a run and materialises its static node/edge set.
    ///
    /// Two invariants are enforced here rather than left to caller discipline:
    ///
    /// - **A run narrows, never widens** (`04-kvdag-and-execution.md` §3.4). The
    ///   run's growth limits must be `<=` the version's on both axes, which is
    ///   what makes `workflow_run.max_nodes <= kvdag_version.max_nodes` a true
    ///   invariant rather than a comment. `NewRun`'s own doc has claimed this
    ///   since Phase 1; before Phase 2 nothing checked it.
    /// - **`started_at` has one authority** (§4 D15 / H1): it is bound
    ///   explicitly from `NewRun.started_at_unix_ms`, and migration `0002`
    ///   removed the column's `DEFAULT time::now()` so the database cannot mint
    ///   a second, later clock.
    pub async fn create_run(&self, run: NewRun) -> Result<RunId, StoreError> {
        let workflow_id = workflow_record_id(&run.workflow)?;
        let version_id =
            parse_record_id(TABLE_KVDAG_VERSION, run.version.as_str()).ok_or_else(|| {
                StoreError::Decode(format!("not a kvdag_version id: {}", run.version))
            })?;
        let graph = self.load_version(&run.version).await?;

        if run.growth.max_depth > graph.growth.max_depth
            || run.growth.max_nodes > graph.growth.max_nodes
        {
            return Err(StoreError::Invariant(format!(
                "run growth (max_depth {}, max_nodes {}) widens version {} \
                 (max_depth {}, max_nodes {}); a run narrows, never widens",
                run.growth.max_depth,
                run.growth.max_nodes,
                run.version,
                graph.growth.max_depth,
                graph.growth.max_nodes,
            )));
        }

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
        // The full request, not just the source run id: `restored_from` on a
        // seeded node records what happened, but a selector that was asked
        // for and skipped has no other durable trace once the transient API
        // response is gone. `run_record` still parses only `"run"` back into
        // `RunRecord::restore_from_run` — `nodes`/`allow_changed` are audit
        // trail with no reader today.
        let restore_from_json = run.restore_from.as_ref().map(|request| {
            serde_json::json!({
                "run": request.run.to_string(),
                "nodes": request.nodes,
                "allow_changed": request.allow_changed,
            })
        });

        let mut response = self
            .db
            .query(
                "CREATE workflow_run SET workflow = $workflow, kvdag_version = $version, \
                 tier = $tier, status = \"pending\", args = $args, \
                 context_runs = $context_runs, restore_from = $restore_from, \
                 max_depth = $max_depth, \
                 max_nodes = $max_nodes, workspace_id = $workspace_id, \
                 started_at = time::from_millis($started_at_ms), \
                 nodes_total = $nodes_total RETURN AFTER",
            )
            .bind(("workflow", workflow_id))
            .bind(("version", version_id.clone()))
            .bind(("started_at_ms", run.started_at_unix_ms as i64))
            .bind(("tier", run.tier.as_str().to_string()))
            .bind(("args", args_json))
            .bind(("context_runs", context_runs))
            .bind(("restore_from", restore_from_json))
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

        self.materialise_run_nodes(
            &row.id,
            &version_id,
            &run.assignments,
            &graph,
            run.started_at_unix_ms,
            &run.restored,
        )
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
                at_unix_ms,
            } => {
                self.write_run_event(run, seq, kind, path, payload, at_unix_ms)
                    .await
            }
            StoreWrite::RunStatus {
                run,
                status,
                ended_at_unix_ms,
            } => self.write_run_status(run, status, ended_at_unix_ms).await,
            StoreWrite::RunFailed {
                run,
                ended_at_unix_ms,
                failure,
            } => self.write_run_failure(run, ended_at_unix_ms, failure).await,
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
                restored_from,
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
                    restored_from,
                )
                .await
            }
            StoreWrite::RunNodeCreated {
                run,
                key,
                path,
                label,
                inputs,
                parent,
                depth,
                status,
                demand,
                assignment,
                assignment_reason,
                attempt,
                proposal_id,
            } => {
                self.write_run_node_created(RunNodeCreate {
                    run,
                    key,
                    path,
                    label,
                    inputs,
                    parent,
                    depth,
                    status,
                    demand,
                    assignment,
                    assignment_reason,
                    attempt,
                    proposal_id,
                })
                .await
            }
            StoreWrite::RunEdgeCreated {
                run,
                from,
                to,
                kind,
                kvdag_edge,
                condition_result,
                fired,
            } => {
                self.write_run_edge_created(
                    run,
                    from,
                    to,
                    kind,
                    kvdag_edge,
                    condition_result,
                    fired,
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
            StoreWrite::RunSummary {
                run,
                kvdag_version,
                text,
                outcome,
                highlights,
                open_gaps,
                per_node,
                token_estimate,
                generated_by_path,
            } => {
                self.write_run_summary(
                    run,
                    kvdag_version,
                    text,
                    outcome,
                    highlights,
                    open_gaps,
                    per_node,
                    token_estimate,
                    generated_by_path,
                )
                .await
            }
            StoreWrite::InterrogationStarted {
                id,
                run,
                path,
                source_session_id,
                forked_session_id,
                transcript_path,
                cwd,
                pane_id,
                reconstructed,
                seeded_from_seq,
                note,
                started_at_unix_ms,
            } => {
                self.write_interrogation_started(
                    id,
                    run,
                    path,
                    source_session_id,
                    forked_session_id,
                    transcript_path,
                    cwd,
                    pane_id,
                    reconstructed,
                    seeded_from_seq,
                    note,
                    started_at_unix_ms,
                )
                .await
            }
            StoreWrite::InterrogationUpdate {
                id,
                forked_session_id,
                transcript_path,
                ended_at_unix_ms,
            } => {
                self.write_interrogation_update(
                    id,
                    forked_session_id,
                    transcript_path,
                    ended_at_unix_ms,
                )
                .await
            }
            StoreWrite::RunLeadPane {
                run,
                lead_pane_id,
                lead_terminal_id,
                lead_prompt_version,
            } => {
                self.write_run_lead_pane(run, lead_pane_id, lead_terminal_id, lead_prompt_version)
                    .await
            }
            StoreWrite::RunLeadBinding {
                run,
                lead_session_id,
                team_name,
                lead_pane_id,
                lead_terminal_id,
                lead_prompt_version,
            } => {
                self.write_run_lead_binding(
                    run,
                    lead_session_id,
                    team_name,
                    lead_pane_id,
                    lead_terminal_id,
                    lead_prompt_version,
                )
                .await
            }
            StoreWrite::RunTaskProjected {
                run,
                path,
                node_key,
                task_id,
                subject,
                owner,
                status,
                emergent,
                blocked_by,
                observed_at_unix_ms,
            } => {
                self.write_run_task_projected(TaskProjection {
                    run,
                    path,
                    node_key,
                    task_id,
                    subject,
                    owner,
                    status,
                    emergent,
                    blocked_by,
                    observed_at_unix_ms,
                })
                .await
            }
            StoreWrite::RunMemberSnapshot {
                run,
                name,
                agent_type,
                model,
                pane_id,
                backend_type,
                is_active,
                cwd,
                session_id,
                transcript_path,
                last_state,
                last_state_at_unix_ms,
                observed_at_unix_ms,
            } => {
                self.write_run_member_snapshot(MemberSnapshot {
                    run,
                    name,
                    agent_type,
                    model,
                    pane_id,
                    backend_type,
                    is_active,
                    cwd,
                    session_id,
                    transcript_path,
                    last_state,
                    last_state_at_unix_ms,
                    observed_at_unix_ms,
                })
                .await
            }
            // Landed in `phase4-retarget-plan.md` P7, once migration `0006`
            // gave `run_node.attention` a column to write and the review
            // tables (schema present since `0001_init.surql`) a writer.
            StoreWrite::RunNodeAttention {
                run,
                path,
                attention,
                intervened,
                observed_at_unix_ms: _,
            } => {
                self.write_run_node_attention(run, path, attention, intervened)
                    .await
            }
            StoreWrite::ReviewCycleStarted {
                id,
                run,
                kvdag_version,
                started_at_unix_ms,
            } => {
                self.write_review_cycle_started(id, run, kvdag_version, started_at_unix_ms)
                    .await
            }
            StoreWrite::ReviewCycleUpdate {
                id,
                status,
                ended_at_unix_ms,
                resulting_version,
            } => {
                self.write_review_cycle_update(id, status, ended_at_unix_ms, resulting_version)
                    .await
            }
            StoreWrite::ReviewFindings { cycle, findings } => {
                self.write_review_findings(cycle, findings).await
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
            .bind(("workflow", workflow_id.clone()))
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
        if pruned > 0 {
            self.bump_pruned_runs_counter(&workflow_id, pruned).await?;
            tracing::info!(
                workflow = %workflow,
                pruned,
                keep_runs,
                "pruned workflow run history"
            );
        }
        Ok(pruned)
    }

    /// §9's "journalled at the workflow level": a `pruned_runs` counter bump
    /// plus `updated_at` refresh on the `workflow` row, rather than a
    /// dedicated journal table — a prune is one number, and the summary rows
    /// it leaves behind are their own record (§4 D12).
    async fn bump_pruned_runs_counter(
        &self,
        workflow_id: &surrealdb_types::RecordId,
        pruned: u64,
    ) -> Result<(), StoreError> {
        let response = self
            .db
            .query(
                "UPDATE $workflow SET pruned_runs = \
                 (IF pruned_runs = NONE THEN 0 ELSE pruned_runs END) + $pruned, \
                 updated_at = time::now()",
            )
            .bind(("workflow", workflow_id.clone()))
            .bind(("pruned", pruned as i64))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
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
                 DELETE run_member WHERE run = $run;\
                 DELETE $run;",
            )
            .bind(("run", run.clone()))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    /// Marks every non-terminal run `failed { reason: "interrupted" }` at
    /// store open, and sweeps their non-terminal nodes to `cancelled` (§4
    /// D13). A server restart drops any in-memory `Engine`, so a
    /// `pending`/`running`/`paused` row left over from the previous process
    /// is a lie the moment the new server starts — nothing will ever move it
    /// forward again. `Paused` is not offered (04 §9 assumed resume machinery
    /// that does not exist); `failed/interrupted` is honest and terminal, and
    /// Phase 3's restore is the recovery path this surfaces (the run
    /// browser's `R`).
    ///
    /// `now_unix_ms` is caller-supplied rather than read here: `time::now()`
    /// would reintroduce the store-flush second clock §4 D14 exists to kill.
    /// A swept node's `evidence` is left untouched rather than claiming a
    /// completion signal that never arrived — the node didn't fail, the
    /// server left.
    ///
    /// Safe because the store's exclusive `LOCK` guarantees no other server
    /// is executing these runs, and the current server opens the store
    /// before it can start one. Idempotent: a second call finds nothing
    /// non-terminal and is a no-op.
    pub async fn mark_interrupted_runs(&self, now_unix_ms: u64) -> Result<u64, StoreError> {
        let non_terminal_run: Vec<String> = ["pending", "running", "paused"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut response = self
            .db
            .query(
                "UPDATE workflow_run SET status = \"failed\", \
                 failure = { reason: \"interrupted\", \
                             detail: \"server restarted while the run was live\" }, \
                 ended_at = time::from_millis($now_ms) \
                 WHERE status IN $non_terminal RETURN AFTER",
            )
            .bind(("non_terminal", non_terminal_run))
            .bind(("now_ms", now_unix_ms as i64))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::RunRow> = response.take(0).map_err(query_error)?;
        let run_ids: Vec<surrealdb_types::RecordId> = rows.into_iter().map(|row| row.id).collect();
        if run_ids.is_empty() {
            return Ok(0);
        }

        let terminal_node: Vec<String> = TERMINAL_NODE_STATUSES
            .iter()
            .map(|status| node_status_str(*status).to_string())
            .collect();
        let response = self
            .db
            .query(
                "UPDATE run_node SET status = \"cancelled\" \
                 WHERE run IN $runs AND status NOT IN $terminal",
            )
            .bind(("runs", run_ids.clone()))
            .bind(("terminal", terminal_node))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;

        Ok(run_ids.len() as u64)
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

/// The UNIQUE index `workflow.name` carries
/// (`migrations/0001_init.surql`). Named here so the conflict sniffer below
/// stays tied to the migration that defines it.
const INDEX_WORKFLOW_NAME: &str = "workflow_name";

/// A failed `CREATE workflow` is a name collision far more often than it is
/// anything else, and SurrealDB reports that collision as a generic query
/// error carrying its own index message ("Database index `workflow_name`
/// already contains 'x', with record workflow:…"). Forwarding that string to
/// the caller leaked a database internal as the user-facing error, so it is
/// recognised here and named instead. Anything unrecognised stays a plain
/// [`StoreError::Query`] — this narrows the message, it never swallows an
/// unrelated failure.
fn create_workflow_error(name: &str, error: surrealdb::Error) -> StoreError {
    let message = error.to_string();
    if is_workflow_name_conflict(&message) {
        return StoreError::NameTaken {
            name: name.to_string(),
        };
    }
    StoreError::Query(message)
}

fn is_workflow_name_conflict(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains(INDEX_WORKFLOW_NAME) && lowered.contains("already contains")
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

pub(super) fn parse_attention(value: &str) -> Result<Attention, StoreError> {
    match value {
        "stuck" => Ok(Attention::Stuck),
        "budget_exceeded" => Ok(Attention::BudgetExceeded),
        "needs_input" => Ok(Attention::NeedsInput),
        "lead_blocked" => Ok(Attention::LeadBlocked),
        "unbound" => Ok(Attention::Unbound),
        other => Err(StoreError::Decode(format!("unknown attention {other:?}"))),
    }
}

pub(super) fn parse_review_cycle_status(value: &str) -> Result<ReviewCycleStatus, StoreError> {
    match value {
        "running" => Ok(ReviewCycleStatus::Running),
        "awaiting_user" => Ok(ReviewCycleStatus::AwaitingUser),
        "applied" => Ok(ReviewCycleStatus::Applied),
        "declined" => Ok(ReviewCycleStatus::Declined),
        "failed" => Ok(ReviewCycleStatus::Failed),
        other => Err(StoreError::Decode(format!(
            "unknown review_cycle status {other:?}"
        ))),
    }
}

pub(super) fn parse_interview_mode(value: &str) -> Result<InterviewMode, StoreError> {
    match value {
        "resumed" => Ok(InterviewMode::Resumed),
        "evidence_only" => Ok(InterviewMode::EvidenceOnly),
        other => Err(StoreError::Decode(format!(
            "unknown interview_mode {other:?}"
        ))),
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

/// Cross-version restore compatibility digests for one kvdag node (§3 rule 5,
/// §4 D11): `sha256(prompt_template)` and `sha256(canonical(output_schema))`.
/// Computed on demand from the immutable `kvdag_node` row — no stored digest
/// column, so this is the one place either digest is computed, for both a
/// caller's restore decision ([`WorkflowStore::node_compat_digests_for`]) and
/// the store's own re-validation of a restored node's re-persisted checkpoint
/// ([`WorkflowStore::create_restored_run_node`]).
fn node_compat_digests(row: &KvdagNodeRow) -> (String, String) {
    let prompt_digest = format!("{:x}", Sha256::digest(row.prompt_template.as_bytes()));
    let schema_digest = format!(
        "{:x}",
        Sha256::digest(canonical(&row.output_schema).to_string().as_bytes())
    );
    (prompt_digest, schema_digest)
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
    ///
    /// `assignments` is written **verbatim**. This method used to call
    /// `tier::resolve` itself, which made it a second resolver beside
    /// `engine::graph`'s and left the durable row and the DAG view agreeing
    /// only by accident (`06-phase2-plan.md` §4 D9 / R-2). A scheduled node
    /// with no entry in the table is [`StoreError::Invariant`]: falling back to
    /// a locally resolved assignment is precisely the second authority this
    /// change removes.
    async fn materialise_run_nodes(
        &self,
        run_id: &surrealdb_types::RecordId,
        version_id: &surrealdb_types::RecordId,
        assignments: &BTreeMap<NodeKey, NodeAssignment>,
        graph: &Kvdag,
        started_at_unix_ms: u64,
        restored: &[RestoredSeed],
    ) -> Result<(), StoreError> {
        let scheduled: Vec<&KvdagNode> = graph.nodes.iter().filter(|n| !n.is_template).collect();
        let node_rows = self.select_kvdag_nodes(version_id).await?;
        let node_record_id_by_key: BTreeMap<NodeKey, surrealdb_types::RecordId> = node_rows
            .iter()
            .map(|row| (NodeKey::new(row.node_key.clone()), row.id.clone()))
            .collect();
        let node_row_by_key: BTreeMap<NodeKey, &KvdagNodeRow> = node_rows
            .iter()
            .map(|row| (NodeKey::new(row.node_key.clone()), row))
            .collect();
        let seed_by_key: BTreeMap<&NodeKey, &RestoredSeed> =
            restored.iter().map(|seed| (&seed.node_key, seed)).collect();

        let mut run_node_id_by_key: BTreeMap<NodeKey, surrealdb_types::RecordId> = BTreeMap::new();

        for node in &scheduled {
            let assignment = assignments.get(&node.key).ok_or_else(|| {
                StoreError::Invariant(format!(
                    "node {} has no resolved assignment; \
                     the run's assignment table must cover every scheduled node",
                    node.key
                ))
            })?;
            let kvdag_node_id = node_record_id_by_key.get(&node.key).ok_or_else(|| {
                StoreError::Decode(format!("node {} has no stored kvdag_node row", node.key))
            })?;

            if let Some(seed) = seed_by_key.get(&node.key) {
                let target_row = node_row_by_key.get(&node.key).copied();
                let row_id = self
                    .create_restored_run_node(
                        run_id,
                        version_id,
                        kvdag_node_id,
                        node,
                        assignment,
                        seed,
                        target_row,
                        started_at_unix_ms,
                    )
                    .await?;
                run_node_id_by_key.insert(node.key.clone(), row_id);
                continue;
            }

            let inbound: Vec<&KvdagEdge> = graph.inbound_edges(&node.key).collect();
            let status = if inbound.is_empty() {
                NodeStatus::Ready
            } else {
                NodeStatus::Pending
            };

            let mut response = self
                .db
                .query(
                    "CREATE run_node SET run = $run, kvdag_node = $kvdag_node, \
                     node_key = $node_key, instance_path = $instance_path, \
                     label = $label, depth = $depth, status = $status, model = $model, \
                     effort = $effort, demand = $demand, \
                     assignment_reason = $assignment_reason RETURN AFTER",
                )
                .bind(("run", run_id.clone()))
                .bind(("kvdag_node", kvdag_node_id.clone()))
                .bind(("node_key", node.key.to_string()))
                .bind(("instance_path", node.key.to_string()))
                // A static node's instance label is the authored one; `inputs`
                // is left to its `{}` default, since nothing proposed it.
                .bind(("label", node.label.clone()))
                .bind(("depth", i64::from(STATIC_NODE_DEPTH)))
                .bind(("status", node_status_str(status).to_string()))
                .bind(("model", assignment.model.as_str().to_string()))
                .bind(("effort", assignment.effort.as_str().to_string()))
                .bind(("demand", demand_str(node.demand).to_string()))
                .bind(("assignment_reason", assignment.reason.clone()))
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

    /// Persists one restored node's full terminal shape at creation
    /// (`07-phase3-plan.md` §1 WS-B). A `Restored` node never runs, so unlike
    /// a scheduled node it gets no follow-up [`Self::write_run_node`] to fill
    /// in status/evidence/succession — everything is written here, up front.
    /// `started_at`/`ended_at` both read the run's own start stamp (the
    /// restore instant — §4 D4): the caller supplies one explicit clock
    /// reading for the whole run (§4 D14's rule against a second, store-side
    /// clock), and every node materialised in the same `create_run` call
    /// shares it.
    ///
    /// Also re-persists the seed as the node's own seq-1 `result` checkpoint
    /// in the **new** run, so the new run's durable projection is
    /// self-contained and survives later pruning of the source run.
    /// `schema_valid` is recomputed here rather than trusted from the caller:
    /// [`RestoredSeed`] carries no such flag (compatibility was already
    /// decided once, by the handler, to choose *whether* to restore at all —
    /// §3 rule 5), so the store independently compares the source and target
    /// node's digests to decide whether *this* checkpoint may itself seed a
    /// later restore.
    async fn create_restored_run_node(
        &self,
        run_id: &surrealdb_types::RecordId,
        version_id: &surrealdb_types::RecordId,
        kvdag_node_id: &surrealdb_types::RecordId,
        node: &KvdagNode,
        assignment: &NodeAssignment,
        seed: &RestoredSeed,
        target_row: Option<&KvdagNodeRow>,
        started_at_unix_ms: u64,
    ) -> Result<surrealdb_types::RecordId, StoreError> {
        let restored_from_id = self.resolve_checkpoint_id(&seed.source).await?;

        let response = self
            .db
            .query(
                "CREATE run_node SET run = $run, kvdag_node = $kvdag_node, \
                 node_key = $node_key, instance_path = $instance_path, \
                 label = $label, depth = $depth, status = $status, model = $model, \
                 effort = $effort, demand = $demand, assignment_reason = $assignment_reason, \
                 evidence = $evidence, succession = $succession, restored_from = $restored_from, \
                 started_at = time::from_millis($started_at_ms), \
                 ended_at = time::from_millis($started_at_ms) RETURN AFTER",
            )
            .bind(("run", run_id.clone()))
            .bind(("kvdag_node", kvdag_node_id.clone()))
            .bind(("node_key", node.key.to_string()))
            .bind(("instance_path", node.key.to_string()))
            .bind(("label", node.label.clone()))
            .bind(("depth", i64::from(STATIC_NODE_DEPTH)))
            .bind(("status", node_status_str(NodeStatus::Restored).to_string()))
            .bind(("model", assignment.model.as_str().to_string()))
            .bind(("effort", assignment.effort.as_str().to_string()))
            .bind(("demand", demand_str(node.demand).to_string()))
            .bind(("assignment_reason", assignment.reason.clone()))
            .bind(("evidence", evidence_str(Evidence::Restored).to_string()))
            .bind(("succession", "satisfied".to_string()))
            .bind(("restored_from", restored_from_id))
            .bind(("started_at_ms", started_at_unix_ms as i64))
            .await
            .map_err(query_error)?;
        let mut response = response.check().map_err(query_error)?;
        let rows: Vec<records::RunNodeRow> = response.take(0).map_err(query_error)?;
        let row = rows.into_iter().next().ok_or_else(|| {
            StoreError::Query(format!(
                "create restored run_node {} returned no row",
                node.key
            ))
        })?;

        let schema_valid = match (self.source_node_row(&seed.source).await?, target_row) {
            (Some(source_row), Some(target_row)) => {
                node_compat_digests(&source_row) == node_compat_digests(target_row)
            }
            _ => false,
        };

        let checkpoint_response = self
            .db
            .query(
                "CREATE node_checkpoint SET run = $run, run_node = $run_node, \
                 node_key = $node_key, instance_path = $instance_path, \
                 kvdag_version = $kvdag_version, seq = 1, kind = \"result\", \
                 schema_valid = $schema_valid, payload = $payload, summary = $summary, \
                 artifact_paths = $artifact_paths, digest = $digest",
            )
            .bind(("run", run_id.clone()))
            .bind(("run_node", row.id.clone()))
            .bind(("node_key", node.key.to_string()))
            .bind(("instance_path", node.key.to_string()))
            .bind(("kvdag_version", version_id.clone()))
            .bind(("schema_valid", schema_valid))
            .bind(("payload", seed.payload.clone()))
            .bind(("summary", seed.summary.clone()))
            .bind(("artifact_paths", seed.artifact_paths.clone()))
            .bind(("digest", seed.digest.clone()))
            .await
            .map_err(query_error)?;
        checkpoint_response.check().map_err(query_error)?;

        Ok(row.id)
    }

    /// The source node's own `kvdag_node` row for a [`RestoredRef`], resolved
    /// via the source run's `kvdag_version` (a `RestoredRef` names a node key,
    /// not a version). `None` for a source run or node that no longer
    /// resolves — treated as "cannot verify compatibility", not an error.
    async fn source_node_row(
        &self,
        source: &RestoredRef,
    ) -> Result<Option<KvdagNodeRow>, StoreError> {
        let Some(run_id) = parse_record_id(TABLE_WORKFLOW_RUN, source.run.as_str()) else {
            return Ok(None);
        };
        let Some(run_row) = self.select_run_row(&run_id).await? else {
            return Ok(None);
        };
        let rows = self.select_kvdag_nodes(&run_row.kvdag_version).await?;
        Ok(rows
            .into_iter()
            .find(|row| row.node_key == source.node_key.as_str()))
    }

    /// Public accessor for [`node_compat_digests`]: resolves the node by
    /// `(version, key)` first, for a caller (the restore handler, WS-D) that
    /// only has the node's key and a version, not a row. `None` when the
    /// version has no node under that key, so an unknown selector is a plain
    /// comparison input rather than a special case for every caller
    /// (`07-phase3-plan.md` §3 rule 5, §4 D11).
    pub async fn node_compat_digests_for(
        &self,
        version: &KvdagVersionId,
        key: &NodeKey,
    ) -> Result<Option<(String, String)>, StoreError> {
        let version_id = parse_record_id(TABLE_KVDAG_VERSION, version.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a kvdag_version id: {version}")))?;
        let rows = self.select_kvdag_nodes(&version_id).await?;
        Ok(rows
            .iter()
            .find(|row| row.node_key == key.as_str())
            .map(node_compat_digests))
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
        at_unix_ms: u64,
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
                 run_node = $run_node, payload = $payload, at = time::from_millis($at_ms)",
            )
            .bind(("run", run_id))
            .bind(("seq", seq as i64))
            .bind(("kind", run_event_kind_str(kind).to_string()))
            .bind(("run_node", run_node_id))
            .bind(("payload", payload))
            .bind(("at_ms", at_unix_ms as i64))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    /// Closes a run as `failed` and records *why* on the run row's `failure`
    /// column, which `workflow.run.get` already publishes.
    ///
    /// Separate from [`Self::write_run_status`] rather than an extra parameter
    /// on it: a status write happens on every ordinary transition and carries no
    /// reason, and threading an always-`None` argument through all of them would
    /// make the one case that *does* have a reason harder to find, not easier.
    async fn write_run_failure(
        &self,
        run: RunId,
        ended_at_unix_ms: u64,
        failure: serde_json::Value,
    ) -> Result<(), StoreError> {
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let response = self
            .db
            .query(
                "UPDATE $run SET status = \"failed\", failure = $failure, \
                 ended_at = time::from_millis($ended_at_ms)",
            )
            .bind(("run", run_id))
            .bind(("failure", failure))
            .bind(("ended_at_ms", ended_at_unix_ms as i64))
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
        restored_from: Option<RestoredRef>,
    ) -> Result<(), StoreError> {
        let run_node_id = self.find_run_node_id(&run, &path).await?;
        let restored_from_id = match &restored_from {
            Some(source) => self.resolve_checkpoint_id(source).await?,
            None => None,
        };
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
                 restored_from = $restored_from, \
                 first_pass_succeeded = IF $settles THEN $first_pass \
                                        ELSE first_pass_succeeded END, \
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
            .bind(("restored_from", restored_from_id))
            // `first_pass_succeeded` is one of the two `NodeHistory` inputs
            // that can be truthful today (§4 D8), and it is a property of how
            // the node *closed*, not of any intermediate transition — so it is
            // only rewritten on a terminal status. A restart that succeeds on
            // attempt 2 therefore reads `false`, which is what the `auto`
            // policy needs it to mean.
            .bind(("settles", status.is_terminal()))
            .bind((
                "first_pass",
                status == NodeStatus::Succeeded && attempt <= 1,
            ))
            .bind(("started_at_ms", started_at_unix_ms.map(|ms| ms as i64)))
            .bind(("ended_at_ms", ended_at_unix_ms.map(|ms| ms as i64)))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;

        if let Some(binding) = binding {
            let response = self
                .db
                .query(
                    // An empty transcript path is not a path: it is a node
                    // whose session has not written one yet (the reserved
                    // `.lead` node is the case that reaches this — a lead can
                    // be identified a poll or two before its transcript
                    // exists). Stored as `NONE` so a reader cannot mistake it
                    // for a file it could open.
                    "UPDATE $id SET pane_id = $pane_id, terminal_id = $terminal_id, \
                     agent_session_id = $agent_session_id, \
                     transcript_path = IF $transcript_path = \"\" THEN NONE \
                                        ELSE $transcript_path END, \
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

    /// Resolves a [`RestoredRef`] to the source `node_checkpoint` record it
    /// names.
    ///
    /// Addressed by `(run, node_key, seq)` rather than `run_node`'s
    /// `checkpoint_seq` index (`run_node, seq`): the engine only carries the
    /// source node's *key*, not its store row id, across a restore. Missing is
    /// not an error — a checkpoint pruned away between restore and this write
    /// decodes back as `None` rather than failing the write
    /// (`07-phase3-plan.md` §1, `StoreWrite::RunNode.restored_from` doc).
    async fn resolve_checkpoint_id(
        &self,
        source: &RestoredRef,
    ) -> Result<Option<surrealdb_types::RecordId>, StoreError> {
        let Some(run_id) = parse_record_id(TABLE_WORKFLOW_RUN, source.run.as_str()) else {
            return Ok(None);
        };
        let mut response = self
            .db
            .query(
                "SELECT * FROM node_checkpoint WHERE run = $run AND node_key = $node_key \
                 AND seq = $seq AND kind = \"result\" ORDER BY instance_path LIMIT 1",
            )
            .bind(("run", run_id))
            .bind(("node_key", source.node_key.to_string()))
            .bind(("seq", source.checkpoint_seq as i64))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::CheckpointRow> = response.take(0).map_err(query_error)?;
        Ok(rows.into_iter().next().map(|row| row.id))
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
                 WHERE run = $run AND status IN $terminal \
                 AND !string::starts_with(instance_path, $reserved_prefix)))",
            )
            .bind(("run", run_id))
            .bind(("terminal", terminal))
            .bind(("reserved_prefix", RESERVED_PATH_PREFIX))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    /// Re-derives both run-level node counters from the run's own `run_node`
    /// rows.
    ///
    /// `nodes_total` was a create-time constant in Phase 1 because the node set
    /// was. Dynamic growth makes it a moving number, and a progress counter
    /// whose denominator ignores expansion children under-reports the run —
    /// so both halves are recomputed from the rows rather than incremented.
    async fn refresh_run_node_counters(&self, run: &RunId) -> Result<(), StoreError> {
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let terminal: Vec<String> = TERMINAL_NODE_STATUSES
            .iter()
            .map(|status| node_status_str(*status).to_string())
            .collect();
        let response = self
            .db
            .query(
                "UPDATE $run SET \
                 nodes_total = array::len((SELECT VALUE id FROM run_node WHERE run = $run \
                 AND !string::starts_with(instance_path, $reserved_prefix))), \
                 nodes_done = array::len((SELECT VALUE id FROM run_node \
                 WHERE run = $run AND status IN $terminal \
                 AND !string::starts_with(instance_path, $reserved_prefix)))",
            )
            .bind(("run", run_id))
            .bind(("terminal", terminal))
            .bind(("reserved_prefix", RESERVED_PATH_PREFIX))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    /// Creates a `run_node` that did not exist when the run started, together
    /// with the `spawned` relation recording which node proposed it.
    ///
    /// This is the first writer the `spawned` table has ever had: before
    /// Phase 2 its only Rust reference was the `DELETE` in
    /// [`Self::prune_run_history`]. The `CREATE` and the `RELATE` travel in one
    /// `.query()` — one request is one transaction — so a child can never exist
    /// without its provenance.
    async fn write_run_node_created(&self, create: RunNodeCreate) -> Result<(), StoreError> {
        // The engine-owned epilogue is created through this same variant
        // (`begin_epilogue`, `engine/mod.rs`), keyed `.summary`, with no
        // authored `kvdag_node` behind it — the reserved-path branch is the
        // one create allowed to bind `kvdag_node: NONE` (§4 D5, D15).
        if is_reserved_path(create.path.as_str()) {
            return self.write_epilogue_node_created(create).await;
        }

        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, create.run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {}", create.run)))?;
        let run_row = self
            .select_run_row(&run_id)
            .await?
            .ok_or_else(|| StoreError::NotFound {
                table: TABLE_WORKFLOW_RUN,
                id: create.run.to_string(),
            })?;
        let kvdag_node_id = self
            .find_kvdag_node_id(&run_row.kvdag_version, &create.key)
            .await?;
        let parent_id = match &create.parent {
            Some(parent) => Some(self.find_run_node_id(&create.run, parent).await?),
            None => None,
        };

        // `$child` is the array `CREATE ... RETURN AFTER` yields; the relation
        // is drawn to its single element.
        const CREATE_NODE: &str = "LET $child = (CREATE run_node SET run = $run, \
             kvdag_node = $kvdag_node, node_key = $node_key, instance_path = $instance_path, \
             label = $label, inputs = $inputs, \
             parent = $parent, depth = $depth, status = $status, model = $model, \
             effort = $effort, demand = $demand, attempt = $attempt, \
             assignment_reason = $assignment_reason RETURN AFTER);";
        let statement = if parent_id.is_some() {
            // `RELATE` wants a plain record target, so the created id is
            // hoisted into its own binding rather than indexed inline.
            format!(
                "{CREATE_NODE}\
                 LET $child_id = $child[0].id;\
                 RELATE $parent -> spawned -> $child_id SET run = $run, \
                 template_key = $template_key, proposal_id = $proposal_id;\
                 RETURN $child;"
            )
        } else {
            format!("{CREATE_NODE}RETURN $child;")
        };

        let response = self
            .db
            .query(statement)
            .bind(("run", run_id))
            .bind(("kvdag_node", kvdag_node_id))
            .bind(("node_key", create.key.to_string()))
            .bind(("instance_path", create.path.to_string()))
            .bind(("label", create.label.clone()))
            // Bound as JSON rather than as a typed map: the column is a flat
            // `string -> string` object and this is the one writer, so the
            // shape is decided here and read back the same way.
            .bind(("inputs", string_map_json(&create.inputs)))
            .bind(("parent", parent_id))
            .bind(("depth", i64::from(create.depth)))
            .bind(("status", node_status_str(create.status).to_string()))
            .bind(("model", create.assignment.model.as_str().to_string()))
            .bind(("effort", create.assignment.effort.as_str().to_string()))
            .bind(("demand", demand_str(create.demand).to_string()))
            .bind(("attempt", i64::from(create.attempt)))
            .bind(("assignment_reason", create.assignment_reason))
            .bind(("template_key", create.key.to_string()))
            .bind(("proposal_id", create.proposal_id))
            .await
            .map_err(query_error)?;
        let mut response = response.check().map_err(query_error)?;
        // The `RETURN` is the last statement either way; its index differs
        // because the parented form carries the `RELATE` in between.
        let index = if create.parent.is_some() { 3 } else { 1 };
        let rows: Vec<records::RunNodeRow> = response.take(index).map_err(query_error)?;
        if rows.is_empty() {
            return Err(StoreError::Query(format!(
                "create run_node {} returned no row",
                create.path
            )));
        }

        self.refresh_run_node_counters(&create.run).await
    }

    /// Creates the `.summary` epilogue's `run_node` row — the one create
    /// allowed to leave `kvdag_node` `NONE` (migration `0004` loosens the
    /// column for exactly this case; §4 D5, D15).
    ///
    /// Never routed to directly from [`Self::write`]: [`Self::write_run_node_created`]
    /// dispatches here on [`is_reserved_path`]. Asserting the reserved path
    /// again here — rather than trusting the caller — is what makes the
    /// invariant testable on its own: a store test calls this directly with a
    /// non-reserved path and asserts [`StoreError::Invariant`], proving the
    /// loosened column can never be reached by an ordinary node write.
    ///
    /// The epilogue has no `parent` (it is engine-owned, not an expansion
    /// child) and is excluded from `nodes_total`/`nodes_done` (§4 D5), so
    /// unlike [`Self::write_run_node_created`] this never touches
    /// [`Self::refresh_run_node_counters`].
    async fn write_epilogue_node_created(&self, create: RunNodeCreate) -> Result<(), StoreError> {
        if !is_reserved_path(create.path.as_str()) {
            return Err(StoreError::Invariant(format!(
                "write_epilogue_node_created called for non-reserved path {}; \
                 only the \".\"-prefixed namespace may create a run_node with no kvdag_node",
                create.path
            )));
        }
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, create.run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {}", create.run)))?;

        let response = self
            .db
            .query(
                "CREATE run_node SET run = $run, kvdag_node = NONE, node_key = $node_key, \
                 instance_path = $instance_path, label = $label, inputs = $inputs, \
                 parent = NONE, depth = $depth, status = $status, model = $model, \
                 effort = $effort, demand = $demand, attempt = $attempt, \
                 assignment_reason = $assignment_reason RETURN AFTER",
            )
            .bind(("run", run_id))
            .bind(("node_key", create.key.to_string()))
            .bind(("instance_path", create.path.to_string()))
            .bind(("label", create.label.clone()))
            .bind(("inputs", string_map_json(&create.inputs)))
            .bind(("depth", i64::from(create.depth)))
            .bind(("status", node_status_str(create.status).to_string()))
            .bind(("model", create.assignment.model.as_str().to_string()))
            .bind(("effort", create.assignment.effort.as_str().to_string()))
            .bind(("demand", demand_str(create.demand).to_string()))
            .bind(("attempt", i64::from(create.attempt)))
            .bind(("assignment_reason", create.assignment_reason))
            .await
            .map_err(query_error)?;
        let mut response = response.check().map_err(query_error)?;
        let rows: Vec<records::RunNodeRow> = response.take(0).map_err(query_error)?;
        if rows.is_empty() {
            return Err(StoreError::Query(format!(
                "create epilogue run_node {} returned no row",
                create.path
            )));
        }
        Ok(())
    }

    /// Creates a `run_edge` that did not exist when the run started: the
    /// parent→child `sequence` edge an accepted proposal adds, or the child's
    /// inherited copy of one of its parent's outbound edges (§4 D4).
    ///
    /// The create-shaped sibling of [`Self::write_run_edge`], which is
    /// find-then-`UPDATE` and errors on a missing row. `kvdag_edge` stays
    /// `NONE` for the synthetic sequence edge, which has no authored
    /// counterpart — the column is `option<record<kvdag_edge>>` for exactly
    /// this case.
    #[allow(clippy::too_many_arguments)]
    async fn write_run_edge_created(
        &self,
        run: RunId,
        from: InstancePath,
        to: InstancePath,
        kind: EdgeKind,
        kvdag_edge: Option<(NodeKey, NodeKey)>,
        condition_result: Option<bool>,
        fired: bool,
    ) -> Result<(), StoreError> {
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let from_id = self.find_run_node_id(&run, &from).await?;
        let to_id = self.find_run_node_id(&run, &to).await?;

        let kvdag_edge_id = match kvdag_edge {
            None => None,
            Some((source, target)) => {
                let run_row =
                    self.select_run_row(&run_id)
                        .await?
                        .ok_or_else(|| StoreError::NotFound {
                            table: TABLE_WORKFLOW_RUN,
                            id: run.to_string(),
                        })?;
                self.find_kvdag_edge_id_by_keys(&run_row.kvdag_version, &source, &target, kind)
                    .await?
            }
        };

        let response = self
            .db
            .query(
                "RELATE $from -> run_edge -> $to SET run = $run, kind = $kind, \
                 kvdag_edge = $kvdag_edge, condition_result = $condition_result, \
                 fired_at = IF $fired THEN time::now() ELSE NONE END",
            )
            .bind(("from", from_id))
            .bind(("to", to_id))
            .bind(("run", run_id))
            .bind(("kind", edge_kind_str(kind).to_string()))
            .bind(("kvdag_edge", kvdag_edge_id))
            .bind(("condition_result", condition_result))
            .bind(("fired", fired))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    async fn select_run_row(
        &self,
        run_id: &surrealdb_types::RecordId,
    ) -> Result<Option<records::RunRow>, StoreError> {
        let mut response = self
            .db
            .query("SELECT * FROM $id")
            .bind(("id", run_id.clone()))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::RunRow> = response.take(0).map_err(query_error)?;
        Ok(rows.into_iter().next())
    }

    async fn find_kvdag_node_id(
        &self,
        version_id: &surrealdb_types::RecordId,
        key: &NodeKey,
    ) -> Result<surrealdb_types::RecordId, StoreError> {
        let mut response = self
            .db
            .query(
                "SELECT * FROM kvdag_node WHERE version = $version \
                 AND node_key = $node_key LIMIT 1",
            )
            .bind(("version", version_id.clone()))
            .bind(("node_key", key.to_string()))
            .await
            .map_err(query_error)?;
        let rows: Vec<KvdagNodeRow> = response.take(0).map_err(query_error)?;
        rows.into_iter()
            .next()
            .map(|row| row.id)
            .ok_or_else(|| StoreError::NotFound {
                table: TABLE_KVDAG_NODE,
                id: format!("{}/{key}", record_id_to_string(version_id)),
            })
    }

    /// The `kvdag_edge` behind an inherited run edge, addressed by the authored
    /// endpoints' node keys.
    ///
    /// [`Self::find_kvdag_edge_id`] resolves the same link from a live
    /// [`KvdagEdge`] during `create_run`; an expansion child's inherited edge
    /// arrives from the pure engine as a key pair instead, because the store
    /// write carries no `KvdagEdge`. Missing is not an error: provenance is a
    /// link, and an edge whose authored counterpart cannot be found is still a
    /// real edge in the run.
    async fn find_kvdag_edge_id_by_keys(
        &self,
        version_id: &surrealdb_types::RecordId,
        from: &NodeKey,
        to: &NodeKey,
        kind: EdgeKind,
    ) -> Result<Option<surrealdb_types::RecordId>, StoreError> {
        let mut response = self
            .db
            .query(
                "SELECT * FROM kvdag_edge WHERE in.version = $version \
                 AND in.node_key = $from AND out.node_key = $to AND kind = $kind LIMIT 1",
            )
            .bind(("version", version_id.clone()))
            .bind(("from", from.to_string()))
            .bind(("to", to.to_string()))
            .bind(("kind", edge_kind_str(kind).to_string()))
            .await
            .map_err(query_error)?;
        let rows: Vec<KvdagEdgeRow> = response.take(0).map_err(query_error)?;
        Ok(rows.into_iter().next().map(|row| row.id))
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
            .bind(("run_node", run_node_id.clone()))
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

        // The second truthful `NodeHistory` input (§4 D8). A checkpoint is the
        // only place the store ever learns that a result failed its schema, so
        // the counter is derived here rather than added to `StoreWrite::RunNode`
        // — the wire carries no such field and does not need one.
        if !schema_valid {
            let response = self
                .db
                .query("UPDATE $id SET schema_failures = schema_failures + 1")
                .bind(("id", run_node_id))
                .await
                .map_err(query_error)?;
            response.check().map_err(query_error)?;
        }
        Ok(())
    }

    /// A node instance's own checkpoint, addressed the way `list_checkpoints`
    /// reports it: `(run, instance_path, seq)`, the `checkpoint_by_instance`
    /// index. Used for a reconstructed interrogation's seed, which is always a
    /// checkpoint of the node being interrogated in the run being read —
    /// unlike [`Self::resolve_checkpoint_id`], which crosses runs by
    /// `node_key` for a cross-version restore.
    async fn find_checkpoint_id(
        &self,
        run: &RunId,
        path: &InstancePath,
        seq: u64,
    ) -> Result<Option<surrealdb_types::RecordId>, StoreError> {
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let mut response = self
            .db
            .query(
                "SELECT * FROM node_checkpoint WHERE run = $run AND instance_path = $path \
                 AND seq = $seq LIMIT 1",
            )
            .bind(("run", run_id))
            .bind(("path", path.to_string()))
            .bind(("seq", seq as i64))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::CheckpointRow> = response.take(0).map_err(query_error)?;
        Ok(rows.into_iter().next().map(|row| row.id))
    }

    /// The end-of-run summary the epilogue produced. Idempotent-rejected on
    /// the `run_summary_run` UNIQUE index: a second write for a run surfaces
    /// as a plain [`StoreError::Query`], never a panic — `run_summary` is
    /// meant to be written exactly once per run.
    #[allow(clippy::too_many_arguments)]
    async fn write_run_summary(
        &self,
        run: RunId,
        kvdag_version: KvdagVersionId,
        text: String,
        outcome: String,
        highlights: Vec<String>,
        open_gaps: Vec<String>,
        per_node: Vec<SummaryNodeLine>,
        token_estimate: u32,
        generated_by_path: Option<InstancePath>,
    ) -> Result<(), StoreError> {
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let version_id =
            parse_record_id(TABLE_KVDAG_VERSION, kvdag_version.as_str()).ok_or_else(|| {
                StoreError::Decode(format!("not a kvdag_version id: {kvdag_version}"))
            })?;
        // `None` is tolerated: a summary without a resolvable producer (the
        // `.summary` node itself failed to persist, or was pruned mid-flight)
        // is still a summary.
        let generated_by = match &generated_by_path {
            Some(path) => self.find_run_node_id(&run, path).await.ok(),
            None => None,
        };
        let per_node_json: Vec<serde_json::Value> = per_node
            .into_iter()
            .map(|line| {
                serde_json::json!({
                    "node_key": line.node_key,
                    "verdict": line.verdict,
                    "one_liner": line.one_liner,
                })
            })
            .collect();

        let response = self
            .db
            .query(
                "CREATE run_summary SET run = $run, kvdag_version = $kvdag_version, \
                 text = $text, outcome = $outcome, highlights = $highlights, \
                 open_gaps = $open_gaps, per_node = $per_node, \
                 token_estimate = $token_estimate, generated_by = $generated_by",
            )
            .bind(("run", run_id))
            .bind(("kvdag_version", version_id))
            .bind(("text", text))
            .bind(("outcome", outcome))
            .bind(("highlights", highlights))
            .bind(("open_gaps", open_gaps))
            .bind(("per_node", per_node_json))
            .bind(("token_estimate", i64::from(token_estimate)))
            .bind(("generated_by", generated_by))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    /// A past node's session was revived in a pane. A create, addressed at the
    /// app-allocated [`InterrogationId`] so the later
    /// [`Self::write_interrogation_update`] can find the same row without a
    /// read-back (`07-phase3-plan.md` §3 rule 4).
    #[allow(clippy::too_many_arguments)]
    async fn write_interrogation_started(
        &self,
        id: InterrogationId,
        run: RunId,
        path: InstancePath,
        source_session_id: String,
        forked_session_id: Option<String>,
        transcript_path: Option<String>,
        cwd: String,
        pane_id: crate::workflow::model::PublicPaneId,
        reconstructed: bool,
        seeded_from_seq: Option<u64>,
        note: String,
        started_at_unix_ms: u64,
    ) -> Result<(), StoreError> {
        let interrogation_id = parse_record_id(TABLE_INTERROGATION, id.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not an interrogation id: {id}")))?;
        let run_node_id = self.find_run_node_id(&run, &path).await?;
        let seeded_from_id = match seeded_from_seq {
            Some(seq) => self.find_checkpoint_id(&run, &path, seq).await?,
            None => None,
        };

        let response = self
            .db
            .query(
                "CREATE $id SET run_node = $run_node, \
                 source_session_id = $source_session_id, \
                 forked_session_id = $forked_session_id, \
                 transcript_path = $transcript_path, cwd = $cwd, pane_id = $pane_id, \
                 started_at = time::from_millis($started_at_ms), note = $note, \
                 reconstructed = $reconstructed, seeded_from = $seeded_from",
            )
            .bind(("id", interrogation_id))
            .bind(("run_node", run_node_id))
            .bind(("source_session_id", source_session_id))
            .bind(("forked_session_id", forked_session_id))
            .bind(("transcript_path", transcript_path))
            .bind(("cwd", cwd))
            .bind(("pane_id", pane_id.to_string()))
            .bind(("started_at_ms", started_at_unix_ms as i64))
            .bind(("note", note))
            .bind(("reconstructed", reconstructed))
            .bind(("seeded_from", seeded_from_id))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    /// The two things learned about an interrogation after its record exists.
    /// `None` on either field means "no change", not "clear": once known, the
    /// forked session id never goes back to unknown, and an end stamp is never
    /// un-set (§4 D7).
    async fn write_interrogation_update(
        &self,
        id: InterrogationId,
        forked_session_id: Option<String>,
        transcript_path: Option<String>,
        ended_at_unix_ms: Option<u64>,
    ) -> Result<(), StoreError> {
        let interrogation_id = parse_record_id(TABLE_INTERROGATION, id.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not an interrogation id: {id}")))?;
        let response = self
            .db
            .query(
                "UPDATE $id SET \
                 forked_session_id = IF $forked_session_id = NONE THEN forked_session_id \
                                      ELSE $forked_session_id END, \
                 transcript_path = IF $transcript_path = NONE THEN transcript_path \
                                    ELSE $transcript_path END, \
                 ended_at = IF $ended_at_ms = NONE THEN ended_at \
                            ELSE time::from_millis($ended_at_ms) END",
            )
            .bind(("id", interrogation_id))
            .bind(("forked_session_id", forked_session_id))
            .bind(("transcript_path", transcript_path))
            .bind(("ended_at_ms", ended_at_unix_ms.map(|ms| ms as i64)))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    // ── the agent-teams rework: lead binding and projection ──────────────

    /// Records the pane karvex launched a run's team lead into, before that
    /// lead has said anything about itself (§3.1a).
    ///
    /// Deliberately writes only the three columns karvex already knows and
    /// leaves `lead_session_id` and `team_name` untouched: they are the lead's
    /// to assert, and a placeholder in either would make an unbound run
    /// indistinguishable from a bound one. The later
    /// [`Self::write_run_lead_binding`] overwrites the same pane columns with
    /// the identical values, which is why this one can be an unconditional
    /// `UPDATE` too.
    async fn write_run_lead_pane(
        &self,
        run: RunId,
        lead_pane_id: String,
        lead_terminal_id: String,
        lead_prompt_version: u32,
    ) -> Result<(), StoreError> {
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let response = self
            .db
            .query(
                "UPDATE $run SET lead_pane_id = $lead_pane_id, \
                 lead_terminal_id = $lead_terminal_id, \
                 lead_prompt_version = $lead_prompt_version",
            )
            .bind(("run", run_id))
            .bind(("lead_pane_id", lead_pane_id))
            .bind(("lead_terminal_id", lead_terminal_id))
            .bind(("lead_prompt_version", i64::from(lead_prompt_version)))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    /// Binds the run to the Claude Code team-lead session it spawned
    /// (`09-agent-teams-rework.md` §3.1 step 4).
    ///
    /// A plain `UPDATE`, not a create: the run row exists from `create_run`
    /// onward and the session id only appears once the pane's `claude` has
    /// registered itself. Every column is written unconditionally, so
    /// re-learning the same binding after a server restart is a no-op rather
    /// than a second row.
    async fn write_run_lead_binding(
        &self,
        run: RunId,
        lead_session_id: String,
        team_name: String,
        lead_pane_id: Option<String>,
        lead_terminal_id: Option<String>,
        lead_prompt_version: u32,
    ) -> Result<(), StoreError> {
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let response = self
            .db
            .query(
                "UPDATE $run SET lead_session_id = $lead_session_id, team_name = $team_name, \
                 lead_pane_id = $lead_pane_id, lead_terminal_id = $lead_terminal_id, \
                 lead_prompt_version = $lead_prompt_version",
            )
            .bind(("run", run_id))
            .bind(("lead_session_id", lead_session_id))
            .bind(("team_name", team_name))
            .bind(("lead_pane_id", lead_pane_id))
            .bind(("lead_terminal_id", lead_terminal_id))
            .bind(("lead_prompt_version", i64::from(lead_prompt_version)))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    /// Projects one observed Claude Code task onto its `run_node` row (§3.4).
    ///
    /// Upsert on `(run, instance_path)` — the pair the `run_node_instance`
    /// UNIQUE index already covers — because the projection re-observes every
    /// task on every poll and an append would turn a 2s cadence into unbounded
    /// row growth.
    ///
    /// The two branches are not symmetric:
    ///
    /// - A **planned** task lands on a row `create_run` materialised, so it is
    ///   always the `UPDATE` branch and never needs a create.
    /// - An **emergent** task has no `kvdag_node` behind it, so its create
    ///   binds `kvdag_node = NONE` — the same loosened column migration `0004`
    ///   opened for the epilogue. The reserved-path assertion from
    ///   [`Self::write_epilogue_node_created`] is repeated here for exactly the
    ///   same reason: it is what keeps the loosened column unreachable from an
    ///   ordinary node write, and it is asserted here rather than trusted from
    ///   the caller so a store test can prove it directly.
    ///
    /// `observed_at_unix_ms` is the only clock a projected task has — the
    /// source files carry no timestamps — so it stamps `started_at` the first
    /// time the task is seen anywhere past `pending` and `ended_at` the first
    /// time it is seen terminal. Neither is ever re-stamped: the run's history
    /// records when karvex first saw the transition, not when it last looked.
    async fn write_run_task_projected(&self, task: TaskProjection) -> Result<(), StoreError> {
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, task.run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {}", task.run)))?;
        let started_ms =
            (task.status != NodeStatus::Pending).then_some(task.observed_at_unix_ms as i64);
        let ended_ms = task
            .status
            .is_terminal()
            .then_some(task.observed_at_unix_ms as i64);

        let existing = self.select_run_node_row(&run_id, &task.path).await?;
        let node_id = match existing {
            Some(row) => {
                let response = self
                    .db
                    .query(
                        "UPDATE $id SET task_id = $task_id, subject = $subject, owner = $owner, \
                         status = $status, emergent = $emergent, \
                         started_at = IF $started_ms = NONE THEN started_at \
                             ELSE (IF started_at = NONE THEN time::from_millis($started_ms) \
                                   ELSE started_at END) END, \
                         ended_at = IF $ended_ms = NONE THEN ended_at \
                             ELSE (IF ended_at = NONE THEN time::from_millis($ended_ms) \
                                   ELSE ended_at END) END",
                    )
                    .bind(("id", row.id.clone()))
                    .bind(("task_id", task.task_id.clone()))
                    .bind(("subject", task.subject.clone()))
                    .bind(("owner", task.owner.clone()))
                    .bind(("status", node_status_str(task.status).to_string()))
                    .bind(("emergent", task.emergent))
                    .bind(("started_ms", started_ms))
                    .bind(("ended_ms", ended_ms))
                    .await
                    .map_err(query_error)?;
                response.check().map_err(query_error)?;
                row.id
            }
            None => {
                if !is_reserved_path(task.path.as_str()) {
                    return Err(StoreError::Invariant(format!(
                        "projected task would create run_node {} with no kvdag_node; \
                         only the \".\"-prefixed namespace may hold an emergent task",
                        task.path
                    )));
                }
                let mut response = self
                    .db
                    .query(
                        "CREATE run_node SET run = $run, kvdag_node = NONE, node_key = $node_key, \
                         instance_path = $instance_path, label = $subject, inputs = {}, \
                         parent = NONE, depth = 0, status = $status, model = \"\", \
                         effort = \"\", demand = $demand, attempt = 1, \
                         assignment_reason = \"\", task_id = $task_id, subject = $subject, \
                         owner = $owner, emergent = $emergent, \
                         started_at = IF $started_ms = NONE THEN NONE \
                                      ELSE time::from_millis($started_ms) END, \
                         ended_at = IF $ended_ms = NONE THEN NONE \
                                    ELSE time::from_millis($ended_ms) END RETURN AFTER",
                    )
                    .bind(("run", run_id.clone()))
                    .bind(("node_key", task.node_key.to_string()))
                    .bind(("instance_path", task.path.to_string()))
                    // The DAG view names a node by `label`, and an emergent task
                    // has no authored name to use — its subject is the only name
                    // it has ever had.
                    .bind(("subject", task.subject.clone()))
                    .bind(("status", node_status_str(task.status).to_string()))
                    .bind(("demand", demand_str(Demand::Standard).to_string()))
                    .bind(("task_id", task.task_id.clone()))
                    .bind(("owner", task.owner.clone()))
                    .bind(("emergent", task.emergent))
                    .bind(("started_ms", started_ms))
                    .bind(("ended_ms", ended_ms))
                    .await
                    .map_err(query_error)?
                    .check()
                    .map_err(query_error)?;
                let rows: Vec<records::RunNodeRow> = response.take(0).map_err(query_error)?;
                rows.into_iter()
                    .next()
                    .ok_or_else(|| {
                        StoreError::Query(format!(
                            "create emergent run_node {} returned no row",
                            task.path
                        ))
                    })?
                    .id
            }
        };

        self.project_blocked_by(&run_id, &node_id, &task.blocked_by)
            .await?;
        // An emergent node lives in the reserved namespace, which both counters
        // already exclude (`refresh_run_node_counters` filters on the prefix),
        // so this is only ever moving `nodes_done` for a planned task.
        self.refresh_nodes_done(&task.run).await
    }

    /// Materialises a projected task's `blockedBy` list as `sequence`
    /// `run_edge` relations (§3.4: `blockedBy` is the edge structure).
    ///
    /// Four rules make this safe to run on every poll:
    ///
    /// - **Idempotent.** An edge that already exists between the same two
    ///   endpoints is left alone, whatever its kind, so re-observing the same
    ///   task neither accumulates parallel relations nor shadows an authored
    ///   `data` edge with a redundant `sequence` one.
    /// - **Self-healing rather than ordering-dependent.** A blocker whose own
    ///   task has not been projected yet has no `run_node` row to point at; the
    ///   edge is skipped rather than erroring, and the next poll — which sees
    ///   both tasks — draws it. The projection has no ordering guarantee to
    ///   offer, so the writer must not need one.
    /// - **Withdrawing is part of observing.** A lead may drop a `blockedBy`,
    ///   and an edge that outlived the dependency it described would make the
    ///   DAG lie about what the run is waiting on. Edges into this node that
    ///   the projection itself drew and no longer observes are removed.
    /// - **Only its own edges.** Withdrawal is scoped to `kvdag_edge = NONE`,
    ///   so an edge the definition authored and `create_run` materialised is
    ///   never removed by an observation that merely failed to mention it. The
    ///   plan's structure is the plan's to state; only the observed structure
    ///   is the projection's to retract.
    async fn project_blocked_by(
        &self,
        run_id: &surrealdb_types::RecordId,
        node_id: &surrealdb_types::RecordId,
        blocked_by: &[InstancePath],
    ) -> Result<(), StoreError> {
        let mut observed: Vec<surrealdb_types::RecordId> = Vec::with_capacity(blocked_by.len());
        for blocker in blocked_by {
            let Some(blocker_row) = self.select_run_node_row(run_id, blocker).await? else {
                continue;
            };
            observed.push(blocker_row.id.clone());
            let mut response = self
                .db
                .query(
                    "SELECT VALUE id FROM run_edge \
                     WHERE run = $run AND in = $from AND out = $to LIMIT 1",
                )
                .bind(("run", run_id.clone()))
                .bind(("from", blocker_row.id.clone()))
                .bind(("to", node_id.clone()))
                .await
                .map_err(query_error)?;
            let existing: Vec<surrealdb_types::RecordId> = response.take(0).map_err(query_error)?;
            if !existing.is_empty() {
                continue;
            }
            let response = self
                .db
                .query(
                    "RELATE $from -> run_edge -> $to SET run = $run, kind = $kind, \
                     kvdag_edge = NONE, condition_result = NONE, fired_at = NONE",
                )
                .bind(("from", blocker_row.id))
                .bind(("to", node_id.clone()))
                .bind(("run", run_id.clone()))
                .bind(("kind", edge_kind_str(EdgeKind::Sequence).to_string()))
                .await
                .map_err(query_error)?;
            response.check().map_err(query_error)?;
        }

        let response = self
            .db
            .query(
                "DELETE run_edge WHERE run = $run AND out = $to AND kvdag_edge = NONE \
                 AND !array::any($observed, |$blocker| $blocker = in)",
            )
            .bind(("run", run_id.clone()))
            .bind(("to", node_id.clone()))
            .bind(("observed", observed))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    /// The `Option`-returning sibling of [`Self::find_run_node_id`], which
    /// errors on a missing row. Both upsert-shaped projection writers need
    /// "does this row exist yet" as an answer rather than as an error.
    async fn select_run_node_row(
        &self,
        run_id: &surrealdb_types::RecordId,
        path: &InstancePath,
    ) -> Result<Option<records::RunNodeRow>, StoreError> {
        let mut response = self
            .db
            .query("SELECT * FROM run_node WHERE run = $run AND instance_path = $path LIMIT 1")
            .bind(("run", run_id.clone()))
            .bind(("path", path.to_string()))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::RunNodeRow> = response.take(0).map_err(query_error)?;
        Ok(rows.into_iter().next())
    }

    /// Snapshots one member of the run's team (§3.4).
    ///
    /// Upsert on `(run, name)` — the `run_member_name` UNIQUE index — because
    /// the projection re-reads the whole team config on every poll. First sight
    /// creates with `first_seen_at == last_seen_at`; every later sighting
    /// rewrites the mutable half in place and advances `last_seen_at` only, so
    /// the pair brackets the window the member was visible in.
    ///
    /// Both stamps are bound from the observation's own
    /// `observed_at_unix_ms`. The database never mints one: a `time::now()`
    /// here would be the flush-time second clock migrations `0002` and `0004`
    /// exist to keep out of this schema.
    ///
    /// `session_id`/`transcript_path`/`last_state`/`last_state_at_unix_ms`
    /// (migration `0006`) follow the same non-regressing idiom
    /// [`Self::write_interrogation_update`] uses for `forked_session_id`:
    /// `None` on an incoming observation leaves the stored value untouched
    /// rather than clearing it. A poll that has not (yet) resolved a
    /// teammate's session id must never erase one an earlier poll already
    /// learned — S1's "absent ⇒ evidence_only forever" is about the identity
    /// never having been resolvable at all, not about one poll's silence.
    async fn write_run_member_snapshot(&self, member: MemberSnapshot) -> Result<(), StoreError> {
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, member.run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {}", member.run)))?;
        let observed_ms = member.observed_at_unix_ms as i64;
        let last_state_at_ms = member.last_state_at_unix_ms.map(|ms| ms as i64);

        let mut response = self
            .db
            .query("SELECT VALUE id FROM run_member WHERE run = $run AND name = $name LIMIT 1")
            .bind(("run", run_id.clone()))
            .bind(("name", member.name.clone()))
            .await
            .map_err(query_error)?;
        let existing: Vec<surrealdb_types::RecordId> = response.take(0).map_err(query_error)?;

        let response = match existing.into_iter().next() {
            Some(id) => {
                self.db
                    .query(
                        "UPDATE $id SET agent_type = $agent_type, model = $model, \
                         pane_id = $pane_id, backend_type = $backend_type, \
                         is_active = $is_active, cwd = $cwd, \
                         session_id = IF $session_id = NONE THEN session_id \
                                       ELSE $session_id END, \
                         transcript_path = IF $transcript_path = NONE THEN transcript_path \
                                            ELSE $transcript_path END, \
                         last_state = IF $last_state = NONE THEN last_state \
                                       ELSE $last_state END, \
                         last_state_at = IF $last_state_at_ms = NONE THEN last_state_at \
                                          ELSE time::from_millis($last_state_at_ms) END, \
                         last_seen_at = time::from_millis($observed_ms)",
                    )
                    .bind(("id", id))
                    .bind(("agent_type", member.agent_type))
                    .bind(("model", member.model))
                    .bind(("pane_id", member.pane_id))
                    .bind(("backend_type", member.backend_type))
                    .bind(("is_active", member.is_active))
                    .bind(("cwd", member.cwd))
                    .bind(("session_id", member.session_id))
                    .bind(("transcript_path", member.transcript_path))
                    .bind(("last_state", member.last_state))
                    .bind(("last_state_at_ms", last_state_at_ms))
                    .bind(("observed_ms", observed_ms))
                    .await
            }
            None => {
                self.db
                    .query(
                        "CREATE run_member SET run = $run, name = $name, \
                         agent_type = $agent_type, model = $model, pane_id = $pane_id, \
                         backend_type = $backend_type, is_active = $is_active, cwd = $cwd, \
                         session_id = $session_id, transcript_path = $transcript_path, \
                         last_state = $last_state, \
                         last_state_at = IF $last_state_at_ms = NONE THEN NONE \
                                          ELSE time::from_millis($last_state_at_ms) END, \
                         first_seen_at = time::from_millis($observed_ms), \
                         last_seen_at = time::from_millis($observed_ms)",
                    )
                    .bind(("run", run_id))
                    .bind(("name", member.name))
                    .bind(("agent_type", member.agent_type))
                    .bind(("model", member.model))
                    .bind(("pane_id", member.pane_id))
                    .bind(("backend_type", member.backend_type))
                    .bind(("is_active", member.is_active))
                    .bind(("cwd", member.cwd))
                    .bind(("session_id", member.session_id))
                    .bind(("transcript_path", member.transcript_path))
                    .bind(("last_state", member.last_state))
                    .bind(("last_state_at_ms", last_state_at_ms))
                    .bind(("observed_ms", observed_ms))
                    .await
            }
        }
        .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    /// Karvex's own opinion about one node's need for intervention (§3.6,
    /// D-10). `attention` is written unconditionally, `None` included — this
    /// is a re-evaluation on every watchdog tick, not a one-way escalation,
    /// and the caller (the watchdog adapter) decides a node has stopped
    /// needing attention.
    ///
    /// `watchdog_interventions` accumulates by exactly one on every write
    /// whose caller passes `intervened = true` — never on `attention` being
    /// `Some` (`phase4-retarget-plan.md` amendment log, WI-R5). The two used
    /// to be the same test, which undercounted: P4's ladder holds `attention`
    /// at `None` through rungs 1–3 (`Say`) and only sets it once the class is
    /// surfaced or a node is already `ExternalWait`/over budget, so a column
    /// meant to count every rung karvex actually sent counted opinions
    /// instead. The caller (the watchdog adapter) decides `intervened`;
    /// before this write-arm existed, nothing on this branch ever bound the
    /// column at all (`write_run_node` never has), so every
    /// `run_node.watchdog_interventions` value in the field today is a stale
    /// `0` from `DEFAULT 0`. Written here rather than in `write_run_node`
    /// because attention and status are evaluated on two different cadences
    /// (§3.1: a 20s watchdog sample over the 2s projection poll) and must
    /// stay two independent writes. A write that only clears attention
    /// (`None`) is never an intervention — clearing is the watchdog observing
    /// the node moving again, not karvex acting on it — and the adapter never
    /// sets `intervened` on one.
    ///
    /// `observed_at_unix_ms` is not persisted by this write: migration
    /// `0006` adds no timestamp column for `attention`, on purpose (§3.7:
    /// "nothing else"). The moment of observation is what the `watchdog`
    /// journal's own `run_event.at` already records, via the sibling
    /// `StoreWrite::RunEvent` write the watchdog adapter issues alongside
    /// this one for a rung it actually walks.
    async fn write_run_node_attention(
        &self,
        run: RunId,
        path: InstancePath,
        attention: Option<Attention>,
        intervened: bool,
    ) -> Result<(), StoreError> {
        let run_node_id = self.find_run_node_id(&run, &path).await?;
        let attention_str = attention.map(|value| value.as_str().to_string());
        let response = self
            .db
            .query(
                "UPDATE $id SET attention = $attention, \
                 watchdog_interventions = IF $intervened THEN watchdog_interventions + 1 \
                                           ELSE watchdog_interventions END",
            )
            .bind(("id", run_node_id))
            .bind(("attention", attention_str))
            .bind(("intervened", intervened))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    // ── the agent-teams rework: self-improvement review (`phase4-retarget-plan.md` P7) ──

    /// Starts a review cycle. A create, so the app's bounded `pending_writes`
    /// queue must never evict it — [`Self::write_review_cycle_update`] and
    /// [`Self::write_review_findings`] address this row by id and would have
    /// nothing to update (the same rule [`Self::write_interrogation_started`]
    /// follows).
    async fn write_review_cycle_started(
        &self,
        id: ReviewCycleId,
        run: RunId,
        kvdag_version: KvdagVersionId,
        started_at_unix_ms: u64,
    ) -> Result<(), StoreError> {
        let cycle_id = parse_record_id(TABLE_REVIEW_CYCLE, id.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a review_cycle id: {id}")))?;
        let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a workflow_run id: {run}")))?;
        let version_id =
            parse_record_id(TABLE_KVDAG_VERSION, kvdag_version.as_str()).ok_or_else(|| {
                StoreError::Decode(format!("not a kvdag_version id: {kvdag_version}"))
            })?;
        let response = self
            .db
            .query(
                "CREATE $id SET run = $run, kvdag_version = $kvdag_version, \
                 status = \"running\", started_at = time::from_millis($started_ms)",
            )
            .bind(("id", cycle_id))
            .bind(("run", run_id))
            .bind(("kvdag_version", version_id))
            .bind(("started_ms", started_at_unix_ms as i64))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    /// What changes on a review cycle after it starts. `None` on any field
    /// means "no change", the same convention
    /// [`Self::write_interrogation_update`] uses: a status that never
    /// regresses to unknown, an end stamp that is never un-set, a resulting
    /// version that is never un-linked once compiled.
    async fn write_review_cycle_update(
        &self,
        id: ReviewCycleId,
        status: Option<ReviewCycleStatus>,
        ended_at_unix_ms: Option<u64>,
        resulting_version: Option<KvdagVersionId>,
    ) -> Result<(), StoreError> {
        let cycle_id = parse_record_id(TABLE_REVIEW_CYCLE, id.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a review_cycle id: {id}")))?;
        let resulting_version_id = match &resulting_version {
            Some(version) => Some(
                parse_record_id(TABLE_KVDAG_VERSION, version.as_str()).ok_or_else(|| {
                    StoreError::Decode(format!("not a kvdag_version id: {version}"))
                })?,
            ),
            None => None,
        };
        let response = self
            .db
            .query(
                "UPDATE $id SET \
                 status = IF $status = NONE THEN status ELSE $status END, \
                 ended_at = IF $ended_ms = NONE THEN ended_at \
                            ELSE time::from_millis($ended_ms) END, \
                 resulting_version = IF $resulting_version = NONE THEN resulting_version \
                                      ELSE $resulting_version END",
            )
            .bind(("id", cycle_id))
            .bind(("status", status.map(|value| value.as_str().to_string())))
            .bind(("ended_ms", ended_at_unix_ms.map(|ms| ms as i64)))
            .bind(("resulting_version", resulting_version_id))
            .await
            .map_err(query_error)?;
        response.check().map_err(query_error)?;
        Ok(())
    }

    /// Writes one review cycle's findings and append-merges the interviews
    /// they cite into `review_cycle.interviews` — the same append-merge shape
    /// [`Self::write_interrogation_update`] uses for a single field, widened
    /// here to a whole batch landing in one call (§3.5: findings are written
    /// together once every interview, or its `evidence_only` fallback, is
    /// in).
    ///
    /// `review_finding.run_node` is resolved against the cycle's own run
    /// (read back from the `review_cycle` row itself, so this write does not
    /// need the caller to carry `RunId` a second time). A `verdict =
    /// "replace"` finding with no `replacement` is rejected by the schema's
    /// own `review_finding_replace_requires_replacement` event
    /// (`0001_init.surql`) — `query_error` surfaces that as a typed
    /// [`StoreError`], never a panic, satisfying this packet's own contract.
    async fn write_review_findings(
        &self,
        cycle: ReviewCycleId,
        findings: Vec<ReviewFindingSeed>,
    ) -> Result<(), StoreError> {
        let cycle_id = parse_record_id(TABLE_REVIEW_CYCLE, cycle.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a review_cycle id: {cycle}")))?;
        let mut run_response = self
            .db
            .query("SELECT VALUE run FROM $cycle")
            .bind(("cycle", cycle_id.clone()))
            .await
            .map_err(query_error)?;
        let runs: Vec<surrealdb_types::RecordId> = run_response.take(0).map_err(query_error)?;
        let run_id = runs
            .into_iter()
            .next()
            .ok_or_else(|| StoreError::Decode(format!("review_cycle {cycle} has no run")))?;

        let mut interview_ids: Vec<surrealdb_types::RecordId> = Vec::new();
        for seed in findings {
            let run_node_id = match &seed.run_node {
                Some(path) => self
                    .select_run_node_row(&run_id, path)
                    .await?
                    .map(|row| row.id),
                None => None,
            };
            let interview_id = match &seed.interview {
                Some(interview) => {
                    let id = parse_record_id(TABLE_INTERROGATION, interview.as_str()).ok_or_else(
                        || StoreError::Decode(format!("not an interrogation id: {interview}")),
                    )?;
                    interview_ids.push(id.clone());
                    Some(id)
                }
                None => None,
            };
            let response = self
                .db
                .query(
                    "CREATE review_finding SET cycle = $cycle, run_node = $run_node, \
                     node_key = $node_key, interview = $interview, \
                     interview_mode = $interview_mode, level = $level, verdict = $verdict, \
                     rationale = $rationale, evidence = $evidence, \
                     proposed_change = $proposed_change, replacement = $replacement",
                )
                .bind(("cycle", cycle_id.clone()))
                .bind(("run_node", run_node_id))
                .bind(("node_key", seed.node_key.to_string()))
                .bind(("interview", interview_id))
                .bind(("interview_mode", seed.interview_mode.as_str().to_string()))
                .bind(("level", seed.level))
                .bind(("verdict", seed.verdict))
                .bind(("rationale", seed.rationale))
                .bind(("evidence", seed.evidence))
                .bind(("proposed_change", seed.proposed_change))
                .bind(("replacement", seed.replacement))
                .await
                .map_err(query_error)?;
            response.check().map_err(query_error)?;
        }

        if !interview_ids.is_empty() {
            let response = self
                .db
                .query(
                    "UPDATE $cycle SET \
                     interviews = array::distinct(array::concat(interviews, $new_interviews))",
                )
                .bind(("cycle", cycle_id))
                .bind(("new_interviews", interview_ids))
                .await
                .map_err(query_error)?;
            response.check().map_err(query_error)?;
        }
        Ok(())
    }

    /// Accepts a set of findings out of a review cycle: `accepted = true` and
    /// `applied_in = version` for every `review_finding` in `cycle` whose
    /// `node_key` is in `keys`. Returns how many findings were marked, so the
    /// caller can tell "nothing matched" from "the apply happened" without a
    /// second read.
    pub async fn finding_mark_applied(
        &self,
        cycle: &ReviewCycleId,
        keys: &[NodeKey],
        version: &KvdagVersionId,
    ) -> Result<u64, StoreError> {
        let cycle_id = parse_record_id(TABLE_REVIEW_CYCLE, cycle.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a review_cycle id: {cycle}")))?;
        let version_id = parse_record_id(TABLE_KVDAG_VERSION, version.as_str())
            .ok_or_else(|| StoreError::Decode(format!("not a kvdag_version id: {version}")))?;
        let key_strings: Vec<String> = keys.iter().map(NodeKey::to_string).collect();
        let mut response = self
            .db
            .query(
                "UPDATE review_finding SET accepted = true, applied_in = $version \
                 WHERE cycle = $cycle AND node_key IN $keys RETURN AFTER",
            )
            .bind(("cycle", cycle_id))
            .bind(("version", version_id))
            .bind(("keys", key_strings))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::ReviewFindingRow> = response.take(0).map_err(query_error)?;
        Ok(rows.len() as u64)
    }

    /// Marks every `running` `review_cycle` `failed` at store open, sparing
    /// `awaiting_user` — the review-cycle sibling of
    /// [`Self::mark_interrupted_runs`], for the identical reason: a server
    /// restart drops the in-memory interview machinery (the interview panes,
    /// the synthesis step), so a `running` cycle left over from the previous
    /// process is a lie the moment the new server starts. An `awaiting_user`
    /// cycle is different: nothing was in flight, a human's per-finding
    /// accept is the only thing that was ever going to move it, and that
    /// decision is still there to make through the same row after a restart
    /// — sweeping it would destroy real, still-actionable work for no
    /// reason.
    ///
    /// `review_cycle` has no `failure`/`detail` column of its own (`0006`
    /// added none — §3.7's own "nothing else"), so `status = "failed"` is the
    /// most honest terminal state this schema can currently express; a human
    /// reading the run's review history sees a cycle that did not reach a
    /// resolution, which is the truthful summary of "the server that was
    /// running it restarted".
    pub async fn mark_interrupted_reviews(&self, now_unix_ms: u64) -> Result<u64, StoreError> {
        let mut response = self
            .db
            .query(
                "UPDATE review_cycle SET status = \"failed\", \
                 ended_at = time::from_millis($now_ms) \
                 WHERE status = \"running\" RETURN AFTER",
            )
            .bind(("now_ms", now_unix_ms as i64))
            .await
            .map_err(query_error)?;
        let rows: Vec<records::ReviewCycleRow> = response.take(0).map_err(query_error)?;
        Ok(rows.len() as u64)
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
