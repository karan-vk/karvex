//! Store test plan (`docs/design/workflow-builder/03-storage-schema.md` §10).
//!
//! Cases 1-11 run against `kv-mem` (no disk, no PTY). Cases 12-13 need a real
//! on-disk `SurrealKv` lock and are marked `#[ignore]` by default.

use std::collections::BTreeMap;

use super::records::{self, parse_record_id, record_id_to_string};
use super::*;
use crate::workflow::model::{ArgSpec, InstancePath, KvdagError, Runner, SUMMARY_INSTANCE_PATH};
// The derive macro's generated `impl SurrealValue for ...` references the
// trait name unqualified regardless of how the derive itself is invoked, so
// it must be in scope wherever `#[derive(SurrealValue)]` is used (below, for
// small ad hoc row shapes this file needs that the store's own `records.rs`
// has no reason to define).
use surrealdb_types::SurrealValue;

// ── fixtures ─────────────────────────────────────────────────────────────

fn schema(required: &[&str]) -> OutputSchema {
    let required: Vec<serde_json::Value> = required
        .iter()
        .map(|name| serde_json::Value::String((*name).to_string()))
        .collect();
    OutputSchema::parse(serde_json::json!({"type": "object", "required": required}))
        .expect("valid schema")
}

fn node(key: &str, prompt: &str) -> KvdagNode {
    KvdagNode {
        key: NodeKey::new(key),
        label: key.to_string(),
        role: String::new(),
        kind: NodeKind::Agent,
        demand: Demand::Standard,
        runner: Runner::Agent,
        command: None,
        prompt_template: prompt.to_string(),
        system_contract: None,
        output_schema: schema(&["report"]),
        max_attempts: 2,
        timeout_ms: None,
        isolation: Isolation::None,
        is_template: false,
        expand_allow: Vec::new(),
        expand_max: 0,
    }
}

fn edge(from: &str, to: &str, port: Option<&str>) -> KvdagEdge {
    KvdagEdge {
        from: NodeKey::new(from),
        to: NodeKey::new(to),
        kind: if port.is_some() {
            EdgeKind::Data
        } else {
            EdgeKind::Sequence
        },
        condition: None,
        payload: EdgePayload::Summary,
        port: port.map(str::to_string),
    }
}

/// `version_id`/`workflow_id`/`version`/`parent` are placeholders:
/// `create_version` assigns the real identity, ignoring these.
fn base_spec(nodes: Vec<KvdagNode>, edges: Vec<KvdagEdge>) -> KvdagSpec {
    KvdagSpec {
        version_id: KvdagVersionId::new("kvdag_version:placeholder"),
        workflow_id: WorkflowId::new("workflow:placeholder"),
        version: 1,
        parent: None,
        contract: "reply only through result.json".to_string(),
        growth: GrowthLimits::default(),
        args: vec![ArgSpec {
            name: "goal".to_string(),
            required: true,
            default: None,
            description: "what to build".to_string(),
        }],
        nodes,
        edges,
    }
}

/// plan -> {left, right} -> join
fn diamond_spec() -> KvdagSpec {
    base_spec(
        vec![
            node("plan", "Plan for: {{goal}}"),
            node("left", "Left half of {{plan}}"),
            node("right", "Right half of {{plan}}"),
            node("join", "Merge {{left_out}} and {{right_out}}"),
        ],
        vec![
            edge("plan", "left", Some("plan")),
            edge("plan", "right", Some("plan")),
            edge("left", "join", Some("left_out")),
            edge("right", "join", Some("right_out")),
        ],
    )
}

fn single_node_spec() -> KvdagSpec {
    base_spec(vec![node("solo", "Solve {{goal}}")], Vec::new())
}

fn fanout_spec(children: usize) -> KvdagSpec {
    let mut nodes = vec![node("root", "Plan for: {{goal}}")];
    let mut edges = Vec::new();
    for index in 0..children {
        let key = format!("child{index}");
        nodes.push(node(&key, "Work on {{goal}}"));
        edges.push(edge("root", &key, None));
    }
    base_spec(nodes, edges)
}

async fn open_mem_store() -> WorkflowStore {
    WorkflowStore::open(StoreLocation::Memory)
        .await
        .expect("mem store opens")
}

async fn setup_workflow(store: &WorkflowStore) -> (WorkflowId, Kvdag) {
    let workflow = store
        .create_workflow("demo", "a demo workflow", Tier::Auto)
        .await
        .expect("create_workflow");
    let kvdag = store
        .create_version(&workflow, VersionOrigin::Authored, "v1", single_node_spec())
        .await
        .expect("create_version");
    store
        .set_head_version(&workflow, &kvdag.version_id)
        .await
        .expect("set_head_version");
    (workflow, kvdag)
}

async fn create_run(store: &WorkflowStore, workflow: &WorkflowId, kvdag: &Kvdag) -> RunId {
    create_run_in_workspace(store, workflow, kvdag, None).await
}

/// Stands in for `graph::resolve_assignments`: every node in the graph,
/// templates included, gets an entry, because `materialise_run_nodes` writes
/// the table verbatim and rejects a scheduled node that is missing from it.
fn assignments_for(kvdag: &Kvdag, tier: Tier) -> BTreeMap<NodeKey, NodeAssignment> {
    kvdag
        .nodes
        .iter()
        .map(|node| {
            (
                node.key.clone(),
                NodeAssignment::from_assignment(
                    crate::workflow::tier::resolve(tier, node.demand, None),
                    format!("tier/{}", tier.as_str()),
                ),
            )
        })
        .collect()
}

fn new_run(workflow: &WorkflowId, kvdag: &Kvdag) -> NewRun {
    NewRun {
        workflow: workflow.clone(),
        version: kvdag.version_id.clone(),
        tier: Tier::Auto,
        args: BTreeMap::new(),
        growth: GrowthLimits::default(),
        started_at_unix_ms: next_run_start_unix_ms(),
        assignments: assignments_for(kvdag, Tier::Auto),
        context_runs: Vec::new(),
        workspace_id: None,
        restore_from: None,
        restored: Vec::new(),
    }
}

/// A stamp with sub-second precision, so a round-trip that silently fell back
/// to `time::now()` cannot coincidentally match.
const FIRST_RUN_START_UNIX_MS: u64 = 1_700_000_123_456;

/// `create_run` binds `started_at` verbatim now, so two runs created in the
/// same millisecond would tie — and `prune_run_history` orders by exactly that
/// column. The fixture advances the clock a second per run so "most recent"
/// stays well defined without depending on wall time.
fn next_run_start_unix_ms() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(FIRST_RUN_START_UNIX_MS);
    NEXT.fetch_add(1_000, std::sync::atomic::Ordering::Relaxed)
}

/// One `workflow_run` and its single `run_node`, written directly in SQL for a
/// store deliberately opened at an older migration level.
///
/// [`WorkflowStore::create_run`] reads its rows back through `records::RunRow`
/// and `records::RunNodeRow`, and those structs always describe the *current*
/// schema — a column a later migration adds is a decode error against an
/// earlier one. Production never hits this (`WorkflowStore::open` migrates
/// before anything else runs), so it is only the migration tests that need a
/// writer that speaks the old schema.
///
/// Mirrors `single_node_spec`'s one `solo` node, and binds `started_at`
/// explicitly because migration `0002` removed its `DEFAULT time::now()`.
async fn create_run_row_directly(
    store: &WorkflowStore,
    workflow: &WorkflowId,
    kvdag: &Kvdag,
) -> RunId {
    let version_id =
        parse_record_id(TABLE_KVDAG_VERSION, kvdag.version_id.as_str()).expect("version id");
    let mut response = store
        .db
        .query("SELECT * FROM kvdag_node WHERE version = $version LIMIT 1")
        .bind(("version", version_id.clone()))
        .await
        .expect("select kvdag_node");
    let node_rows: Vec<records::KvdagNodeRow> = response.take(0).expect("decode kvdag_node");
    let kvdag_node_id = node_rows.into_iter().next().expect("the node exists").id;

    let mut response = store
        .db
        .query(
            "CREATE workflow_run SET workflow = $workflow, kvdag_version = $version, \
             tier = \"auto\", status = \"running\", max_depth = 3, max_nodes = 24, \
             started_at = time::from_millis($started_at_ms), nodes_total = 1 RETURN AFTER",
        )
        .bind((
            "workflow",
            parse_record_id(TABLE_WORKFLOW, workflow.as_str()).expect("workflow id"),
        ))
        .bind(("version", version_id))
        .bind(("started_at_ms", 1_700_000_000_000_i64))
        .await
        .expect("create workflow_run");
    let run_rows: Vec<IdOnly> = response.take(0).expect("decode workflow_run");
    let run_row_id = run_rows.into_iter().next().expect("the run row").id;

    let response = store
        .db
        .query(
            "CREATE run_node SET run = $run, kvdag_node = $kvdag_node, node_key = \"solo\", \
             instance_path = \"solo\", depth = 0, status = \"ready\", model = \"opus\", \
             effort = \"high\", demand = \"standard\"",
        )
        .bind(("run", run_row_id.clone()))
        .bind(("kvdag_node", kvdag_node_id))
        .await
        .expect("create run_node");
    response.check().expect("the pre-migration row is valid");

    RunId::new(record_id_to_string(&run_row_id))
}

async fn create_run_in_workspace(
    store: &WorkflowStore,
    workflow: &WorkflowId,
    kvdag: &Kvdag,
    workspace_id: Option<&str>,
) -> RunId {
    store
        .create_run(NewRun {
            workspace_id: workspace_id.map(str::to_string),
            ..new_run(workflow, kvdag)
        })
        .await
        .expect("create_run")
}

async fn setup_run(store: &WorkflowStore) -> (WorkflowId, Kvdag, RunId) {
    let (workflow, kvdag) = setup_workflow(store).await;
    let run = create_run(store, &workflow, &kvdag).await;
    (workflow, kvdag, run)
}

async fn seed_run_summary(store: &WorkflowStore, run: &RunId, version: &KvdagVersionId) {
    let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str()).expect("run id");
    let version_id = parse_record_id(TABLE_KVDAG_VERSION, version.as_str()).expect("version id");
    let response = store
        .db
        .query(
            "CREATE run_summary SET run = $run, kvdag_version = $version, \
             text = \"summary text\", outcome = \"ok\"",
        )
        .bind(("run", run_id))
        .bind(("version", version_id))
        .await
        .expect("create run_summary");
    response.check().expect("run_summary insert succeeds");
}

#[derive(Debug, Clone, SurrealValue)]
struct IdOnly {
    id: surrealdb_types::RecordId,
}

async fn seed_interrogation(
    store: &WorkflowStore,
    run_node_id: &surrealdb_types::RecordId,
) -> surrealdb_types::RecordId {
    let mut response = store
        .db
        .query(
            "CREATE interrogation SET run_node = $run_node, source_session_id = \"sess-1\", \
             forked_session_id = \"sess-1-fork\", cwd = \"/tmp\" RETURN AFTER",
        )
        .bind(("run_node", run_node_id.clone()))
        .await
        .expect("create interrogation");
    let rows: Vec<IdOnly> = response.take(0).expect("decode interrogation");
    rows.into_iter().next().expect("interrogation row").id
}

async fn seed_review_cycle(
    store: &WorkflowStore,
    run: &RunId,
    version: &KvdagVersionId,
) -> surrealdb_types::RecordId {
    let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str()).expect("run id");
    let version_id = parse_record_id(TABLE_KVDAG_VERSION, version.as_str()).expect("version id");
    let mut response = store
        .db
        .query(
            "CREATE review_cycle SET run = $run, kvdag_version = $version, \
             status = \"running\" RETURN AFTER",
        )
        .bind(("run", run_id))
        .bind(("version", version_id))
        .await
        .expect("create review_cycle");
    let rows: Vec<IdOnly> = response.take(0).expect("decode review_cycle");
    rows.into_iter().next().expect("review_cycle row").id
}

async fn seed_review_finding(
    store: &WorkflowStore,
    cycle: &surrealdb_types::RecordId,
    interrogation: &surrealdb_types::RecordId,
) {
    let response = store
        .db
        .query(
            "CREATE review_finding SET cycle = $cycle, node_key = \"solo\", \
             interview = $interview, interview_mode = \"resumed\", level = \"prompt\", \
             verdict = \"keep\", rationale = \"fine\"",
        )
        .bind(("cycle", cycle.clone()))
        .bind(("interview", interrogation.clone()))
        .await
        .expect("create review_finding");
    response.check().expect("review_finding insert succeeds");
}

fn unique_temp_dir(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "karvex-workflow-store-test-{label}-{}-{nanos}",
        std::process::id()
    ))
}

/// Opens an on-disk store, retrying briefly on `store_locked`. The local
/// engine's background router task (and the SurrealKv lock file it holds)
/// tears down asynchronously after the last `Surreal<Db>` handle drops, so a
/// reopen immediately after `drop` can race a lock that hasn't cleared yet.
async fn open_with_retry(dir: &std::path::Path) -> WorkflowStore {
    let mut last_error = None;
    for _ in 0..50 {
        match WorkflowStore::open(StoreLocation::OnDisk(dir.to_path_buf())).await {
            Ok(store) => return store,
            Err(StoreError::Unavailable { reason, .. }) if reason == error::STORE_LOCKED => {
                last_error = Some(reason);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(error) => panic!("reopen failed: {error:?}"),
        }
    }
    panic!("gave up waiting for the lock to clear: {last_error:?}");
}

// ── 1: migrations ────────────────────────────────────────────────────────

#[tokio::test]
async fn migrations_apply_cleanly_and_reapplying_is_a_noop() {
    let store = open_mem_store().await;
    let first = store.applied_migrations().await.expect("read schema_meta");
    assert_eq!(
        first,
        std::collections::BTreeSet::from([
            "0001_init".to_string(),
            "0002_growth_and_history".to_string(),
            "0003_node_identity".to_string(),
            "0004_journal_time_and_interrogation".to_string(),
            "0005_lead_binding_and_projection".to_string(),
        ])
    );

    store.migrate().await.expect("re-migrate is a no-op");
    let second = store.applied_migrations().await.expect("read schema_meta");
    assert_eq!(first, second);
}

// ── 2: create_version digest determinism + dedupe ───────────────────────

#[tokio::test]
async fn create_version_digest_is_deterministic_and_identical_graphs_dedupe() {
    let store = open_mem_store().await;
    let workflow = store
        .create_workflow("demo", "", Tier::Auto)
        .await
        .expect("create_workflow");

    let first = store
        .create_version(&workflow, VersionOrigin::Authored, "v1", diamond_spec())
        .await
        .expect("create_version");
    let second = store
        .create_version(
            &workflow,
            VersionOrigin::Authored,
            "v1 again",
            diamond_spec(),
        )
        .await
        .expect("create_version");

    assert_eq!(first.spec_digest, second.spec_digest);
    assert_eq!(
        first.version_id, second.version_id,
        "an identical graph must not write a new version"
    );
    assert_eq!(first.version, 1);
}

/// `spec_digest` covers only nodes and edges, so a revision that changes only
/// the contract, the args, or the growth limits must still write a version —
/// and a graph that reproduces an older shape must never hand the caller back
/// that older version, because the caller advances `head_version` to it.
#[tokio::test]
async fn create_version_dedupes_only_a_whole_no_op_against_the_chain_tip() {
    let store = open_mem_store().await;
    let workflow = store
        .create_workflow("demo", "", Tier::Auto)
        .await
        .expect("create_workflow");

    let v1 = store
        .create_version(&workflow, VersionOrigin::Authored, "v1", diamond_spec())
        .await
        .expect("v1");
    store
        .set_head_version(&workflow, &v1.version_id)
        .await
        .expect("head -> v1");

    let mut contract_only = diamond_spec();
    contract_only.contract = "reply only through result.json, and cite sources".to_string();
    let v2 = store
        .create_version(&workflow, VersionOrigin::Authored, "v2", contract_only)
        .await
        .expect("v2");
    assert_eq!(v2.spec_digest, v1.spec_digest, "the graph itself is equal");
    assert_ne!(
        v2.version_id, v1.version_id,
        "a contract-only revision is still a revision"
    );
    assert_eq!(v2.version, 2);
    assert_eq!(
        v2.contract,
        "reply only through result.json, and cite sources"
    );
    store
        .set_head_version(&workflow, &v2.version_id)
        .await
        .expect("head -> v2");

    // Reverting the graph shape back to v1's while the head is at v2 must not
    // resurrect v1: v1 carries the old contract, and returning it would walk
    // `head_version` backwards.
    let mut reverted = diamond_spec();
    reverted.contract = "reply only through result.json, and cite sources".to_string();
    reverted.args[0].description = "what to build, precisely".to_string();
    let v3 = store
        .create_version(&workflow, VersionOrigin::Authored, "v3", reverted)
        .await
        .expect("v3");
    assert_eq!(v3.version, 3);
    assert_eq!(v3.args[0].description, "what to build, precisely");

    // A byte-identical resubmission of the tip is the one real no-op.
    let mut same = diamond_spec();
    same.contract = "reply only through result.json, and cite sources".to_string();
    same.args[0].description = "what to build, precisely".to_string();
    let again = store
        .create_version(&workflow, VersionOrigin::Authored, "v3 again", same)
        .await
        .expect("no-op");
    assert_eq!(again.version_id, v3.version_id);
}

// ── 3: version chain + immutability ─────────────────────────────────────

#[tokio::test]
async fn version_chain_is_stable_and_old_versions_stay_immutable() {
    let store = open_mem_store().await;
    let workflow = store
        .create_workflow("demo", "", Tier::Auto)
        .await
        .expect("create_workflow");

    let v1 = store
        .create_version(&workflow, VersionOrigin::Authored, "v1", diamond_spec())
        .await
        .expect("v1");
    store
        .set_head_version(&workflow, &v1.version_id)
        .await
        .expect("head -> v1");

    let mut v2_spec = diamond_spec();
    v2_spec.nodes[1].prompt_template = "Other half of {{plan}}".to_string();
    let v2 = store
        .create_version(&workflow, VersionOrigin::Authored, "v2", v2_spec)
        .await
        .expect("v2");
    store
        .set_head_version(&workflow, &v2.version_id)
        .await
        .expect("head -> v2");

    let mut v3_spec = diamond_spec();
    v3_spec.nodes[1].prompt_template = "Yet another half of {{plan}}".to_string();
    let v3 = store
        .create_version(&workflow, VersionOrigin::Authored, "v3", v3_spec)
        .await
        .expect("v3");

    assert_eq!((v1.version, v2.version, v3.version), (1, 2, 3));
    assert_eq!(v2.parent.as_ref(), Some(&v1.version_id));
    assert_eq!(v3.parent.as_ref(), Some(&v2.version_id));

    let reloaded_v1 = store
        .load_version(&v1.version_id)
        .await
        .expect("load v1 after v3 exists");
    assert_eq!(reloaded_v1, v1, "an old version must be byte-identical");

    let v1_keys: Vec<_> = v1.nodes.iter().map(|node| node.key.clone()).collect();
    let v3_keys: Vec<_> = v3.nodes.iter().map(|node| node.key.clone()).collect();
    assert_eq!(v1_keys, v3_keys, "node_key is stable across versions");
}

/// `workflow.get`'s `versions` field walks the whole parent chain, not just
/// the head — this is the store-level guarantee the A7 fix depends on.
#[tokio::test]
async fn list_version_chain_walks_every_ancestor_with_real_metadata_newest_first() {
    let store = open_mem_store().await;
    let workflow = store
        .create_workflow("demo", "", Tier::Auto)
        .await
        .expect("create_workflow");

    let v1 = store
        .create_version(
            &workflow,
            VersionOrigin::Authored,
            "v1 summary",
            diamond_spec(),
        )
        .await
        .expect("v1");
    store
        .set_head_version(&workflow, &v1.version_id)
        .await
        .expect("head -> v1");

    let mut v2_spec = diamond_spec();
    v2_spec.nodes[1].prompt_template = "Other half of {{plan}}".to_string();
    let v2 = store
        .create_version(&workflow, VersionOrigin::Authored, "v2 summary", v2_spec)
        .await
        .expect("v2");
    store
        .set_head_version(&workflow, &v2.version_id)
        .await
        .expect("head -> v2");

    let chain = store
        .list_version_chain(&workflow, Some(&v2.version_id))
        .await
        .expect("list_version_chain");

    assert_eq!(
        chain.len(),
        2,
        "both versions must be observable, not just the head"
    );
    assert_eq!(chain[0].version_id, v2.version_id, "newest first");
    assert_eq!(chain[0].version, 2);
    assert_eq!(chain[0].parent_version_id.as_ref(), Some(&v1.version_id));
    assert_eq!(chain[0].origin, VersionOrigin::Authored);
    assert_eq!(chain[0].change_summary, "v2 summary");
    assert!(
        chain[0].created_at_unix_ms > 0,
        "created_at must be real, not fabricated"
    );

    assert_eq!(chain[1].version_id, v1.version_id);
    assert_eq!(chain[1].version, 1);
    assert_eq!(chain[1].parent_version_id, None, "v1 has no parent");
    assert_eq!(chain[1].change_summary, "v1 summary");
    assert!(chain[1].created_at_unix_ms > 0);
}

#[tokio::test]
async fn list_version_chain_is_empty_for_a_workflow_with_no_head() {
    let store = open_mem_store().await;
    let workflow = store
        .create_workflow("headless", "", Tier::Auto)
        .await
        .expect("create_workflow");

    let chain = store
        .list_version_chain(&workflow, None)
        .await
        .expect("list_version_chain");
    assert!(chain.is_empty());
}

#[tokio::test]
async fn get_version_record_reports_the_real_origin_change_summary_and_timestamp() {
    let store = open_mem_store().await;
    let (workflow, v1) = setup_workflow(&store).await;
    let v2 = store
        .create_version(
            &workflow,
            VersionOrigin::Authored,
            "hand-edited the prompt",
            fanout_spec(2),
        )
        .await
        .expect("v2");

    let record = store
        .get_version_record(&v2.version_id)
        .await
        .expect("get_version_record")
        .expect("v2 exists");
    assert_eq!(record.version_id, v2.version_id);
    assert_eq!(record.parent_version_id.as_ref(), Some(&v1.version_id));
    assert_eq!(record.origin, VersionOrigin::Authored);
    assert_eq!(record.change_summary, "hand-edited the prompt");
    assert_eq!(record.spec_digest, v2.spec_digest.as_str());
    assert!(record.created_at_unix_ms > 0);
}

#[tokio::test]
async fn get_version_record_reports_none_for_an_unknown_version() {
    let store = open_mem_store().await;
    let missing = KvdagVersionId::new("kvdag_version:does-not-exist");
    let record = store
        .get_version_record(&missing)
        .await
        .expect("get_version_record");
    assert_eq!(record, None);
}

#[tokio::test]
async fn find_workflows_by_name_matches_exactly_and_empty_is_not_an_error() {
    let store = open_mem_store().await;
    store
        .create_workflow("ship-feature", "", Tier::Auto)
        .await
        .expect("create_workflow");

    let matches = store
        .find_workflows_by_name("ship-feature")
        .await
        .expect("find_workflows_by_name");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].name, "ship-feature");

    let no_match = store
        .find_workflows_by_name("does-not-exist")
        .await
        .expect("find_workflows_by_name");
    assert!(no_match.is_empty());

    // Case-sensitive: the schema's UNIQUE index is exact-string, and this
    // resolver must not silently widen the match.
    let wrong_case = store
        .find_workflows_by_name("Ship-Feature")
        .await
        .expect("find_workflows_by_name");
    assert!(wrong_case.is_empty());
}

// ── 4: RELATE traversal, including a fan-out of 12 ──────────────────────

#[tokio::test]
async fn relate_edges_reload_correctly_for_a_diamond_and_a_fanout_of_twelve() {
    let store = open_mem_store().await;
    let workflow = store
        .create_workflow("demo", "", Tier::Auto)
        .await
        .expect("create_workflow");

    let diamond = store
        .create_version(
            &workflow,
            VersionOrigin::Authored,
            "diamond",
            diamond_spec(),
        )
        .await
        .expect("diamond");
    let plan = NodeKey::new("plan");
    let join = NodeKey::new("join");
    let mut downstream_of_plan: Vec<_> = diamond
        .outbound_edges(&plan)
        .map(|edge| edge.to.clone())
        .collect();
    downstream_of_plan.sort();
    assert_eq!(
        downstream_of_plan,
        vec![NodeKey::new("left"), NodeKey::new("right")]
    );
    let mut upstream_of_join: Vec<_> = diamond
        .inbound_edges(&join)
        .map(|edge| edge.from.clone())
        .collect();
    upstream_of_join.sort();
    assert_eq!(
        upstream_of_join,
        vec![NodeKey::new("left"), NodeKey::new("right")]
    );

    let fanout = store
        .create_version(
            &workflow,
            VersionOrigin::Authored,
            "fanout",
            fanout_spec(12),
        )
        .await
        .expect("fanout");
    let root = NodeKey::new("root");
    let children: Vec<_> = fanout
        .outbound_edges(&root)
        .map(|edge| edge.to.clone())
        .collect();
    assert_eq!(children.len(), 12);
    for index in 0..12 {
        assert!(children.contains(&NodeKey::new(format!("child{index}"))));
    }
}

// ── 4b: run-level persistence fidelity ──────────────────────────────────
//
// A run read back from the store has to carry the same facts the live
// projection does. Each case here covers one field that came back empty or
// disagreed after a server restart.

/// `name` rather than a fixed literal because `workflow_name` is UNIQUE, so a
/// test that sets up two runs needs two workflows.
async fn setup_diamond_run(
    store: &WorkflowStore,
    name: &str,
    workspace_id: Option<&str>,
) -> (WorkflowId, RunId) {
    let workflow = store
        .create_workflow(name, "", Tier::Auto)
        .await
        .expect("create_workflow");
    let kvdag = store
        .create_version(
            &workflow,
            VersionOrigin::Authored,
            "diamond",
            diamond_spec(),
        )
        .await
        .expect("create_version");
    let run = create_run_in_workspace(store, &workflow, &kvdag, workspace_id).await;
    (workflow, run)
}

fn node_status_write(run: &RunId, path: &str, status: NodeStatus) -> StoreWrite {
    StoreWrite::RunNode {
        run: run.clone(),
        path: InstancePath::new(path),
        status,
        attempt: 1,
        binding: None,
        usage: NodeUsage::default(),
        evidence: None,
        succession: None,
        started_at_unix_ms: None,
        ended_at_unix_ms: None,
        restored_from: None,
    }
}

/// C2: `nodes_done` had a schema field and a read path but no write site, so
/// every run read from the store reported `0` beside `status: succeeded`.
#[tokio::test]
async fn nodes_done_counts_the_runs_terminal_nodes() {
    let store = open_mem_store().await;
    let (_, run) = setup_diamond_run(&store, "nodes-done", None).await;

    let before = store
        .get_run(&run)
        .await
        .expect("get_run")
        .expect("the run exists");
    assert_eq!(before.nodes_total, 4);
    assert_eq!(before.nodes_done, 0, "no node has closed yet");

    store
        .write(node_status_write(&run, "plan", NodeStatus::Running))
        .await
        .expect("plan running");
    assert_eq!(
        store
            .get_run(&run)
            .await
            .expect("get_run")
            .map(|r| r.nodes_done),
        Some(0),
        "a running node has not closed"
    );

    store
        .write(node_status_write(&run, "plan", NodeStatus::Succeeded))
        .await
        .expect("plan succeeded");
    store
        .write(node_status_write(&run, "left", NodeStatus::Failed))
        .await
        .expect("left failed");
    store
        .write(node_status_write(&run, "right", NodeStatus::Skipped))
        .await
        .expect("right skipped");
    assert_eq!(
        store
            .get_run(&run)
            .await
            .expect("get_run")
            .map(|r| r.nodes_done),
        Some(3),
        "every terminal status closes a node, not just Succeeded"
    );

    // Restarting a node reopens it, so the counter has to come back down —
    // which an increment-on-write would never do.
    store
        .write(node_status_write(&run, "left", NodeStatus::Ready))
        .await
        .expect("left restarted");
    assert_eq!(
        store
            .get_run(&run)
            .await
            .expect("get_run")
            .map(|r| r.nodes_done),
        Some(2),
        "a node leaving a terminal status must decrement the counter"
    );
}

/// C3: the `run_edge` relations were written at run creation but never read
/// back, so a restored run had no topology at all.
#[tokio::test]
async fn run_edges_reload_with_both_endpoints_and_their_kind() {
    let store = open_mem_store().await;
    let (_, run) = setup_diamond_run(&store, "run-edges", None).await;

    let edges = store.list_run_edges(&run).await.expect("list_run_edges");
    let shape: Vec<(String, String, EdgeKind)> = edges
        .iter()
        .map(|edge| (edge.from.to_string(), edge.to.to_string(), edge.kind))
        .collect();
    assert_eq!(
        shape,
        vec![
            ("left".to_string(), "join".to_string(), EdgeKind::Data),
            ("plan".to_string(), "left".to_string(), EdgeKind::Data),
            ("plan".to_string(), "right".to_string(), EdgeKind::Data),
            ("right".to_string(), "join".to_string(), EdgeKind::Data),
        ],
        "the diamond's four edges must come back in (from, to) order"
    );
    assert!(
        edges.iter().all(|edge| !edge.fired),
        "a freshly materialised run has settled no edge yet"
    );
}

/// `run_edge.fired_at`/`condition_result` had a schema, a read path, and a
/// scheduler that settles them in memory, but no write site — so a run restored
/// after a server restart reported `fired: false` on every edge it had actually
/// taken.
#[tokio::test]
async fn run_edge_firing_state_round_trips_through_the_store() {
    let store = open_mem_store().await;
    let (_, run) = setup_diamond_run(&store, "run-edge-firing", None).await;

    let firing = |from: &str, to: &str, condition_result, fired| StoreWrite::RunEdge {
        run: run.clone(),
        from: InstancePath::new(from),
        to: InstancePath::new(to),
        kind: EdgeKind::Data,
        condition_result,
        fired,
    };
    store
        .write(firing("plan", "left", Some(true), true))
        .await
        .expect("left fired");
    store
        .write(firing("plan", "right", Some(false), false))
        .await
        .expect("right dead");

    let settled: Vec<(String, String, Option<bool>, bool)> = store
        .list_run_edges(&run)
        .await
        .expect("list_run_edges")
        .into_iter()
        .map(|edge| {
            (
                edge.from.to_string(),
                edge.to.to_string(),
                edge.condition_result,
                edge.fired,
            )
        })
        .collect();
    assert_eq!(
        settled,
        vec![
            ("left".to_string(), "join".to_string(), None, false),
            ("plan".to_string(), "left".to_string(), Some(true), true),
            ("plan".to_string(), "right".to_string(), Some(false), false),
            ("right".to_string(), "join".to_string(), None, false),
        ],
        "each edge must read back with the firing state it was settled at, and \
         a write must not touch the edges it does not address"
    );

    // §3.1 resolution is not one-way: restarting `plan` clears the result its
    // outbound edges resolved on, so the journal has to un-fire them rather
    // than keep a stamp describing a branch the run is no longer taking.
    store
        .write(firing("plan", "left", None, false))
        .await
        .expect("left un-fired");
    let left = store
        .list_run_edges(&run)
        .await
        .expect("list_run_edges")
        .into_iter()
        .find(|edge| edge.from.to_string() == "plan" && edge.to.to_string() == "left")
        .expect("the plan -> left edge survives");
    assert_eq!(
        (left.condition_result, left.fired),
        (None, false),
        "an edge whose source was restarted reads back unfired"
    );
}

/// C4: `workspace_id` is where a run's panes live; it was present live and
/// absent from every restored run.
#[tokio::test]
async fn create_run_persists_the_workspace_binding() {
    let store = open_mem_store().await;
    let (_, bound) = setup_diamond_run(&store, "bound", Some("w1")).await;
    assert_eq!(
        store
            .get_run(&bound)
            .await
            .expect("get_run")
            .and_then(|record| record.workspace_id),
        Some("w1".to_string())
    );

    let (_, unbound) = setup_diamond_run(&store, "unbound", None).await;
    assert_eq!(
        store
            .get_run(&unbound)
            .await
            .expect("get_run")
            .and_then(|record| record.workspace_id),
        None,
        "a run started with no active workspace records none"
    );
}

/// C5: `cwd` and `node_dir` were written with the pane binding but dropped on
/// the read path, so a restored node could not be traced to its node dir.
#[tokio::test]
async fn run_node_binding_reloads_its_filesystem_paths() {
    let store = open_mem_store().await;
    let (_, run) = setup_diamond_run(&store, "node-binding", Some("w1")).await;

    store
        .write(StoreWrite::RunNode {
            run: run.clone(),
            path: InstancePath::new("plan"),
            status: NodeStatus::Running,
            attempt: 1,
            binding: Some(NodeBinding {
                pane_id: crate::workflow::model::PublicPaneId::new("w1:p2"),
                terminal_id: crate::terminal::TerminalId::alloc(),
                agent_session_id: "457f6939".to_string(),
                transcript_path: std::path::PathBuf::from("/tmp/runs/r1/plan.jsonl"),
                node_dir: std::path::PathBuf::from("/tmp/runs/r1/plan"),
                cwd: std::path::PathBuf::from("/tmp/work"),
            }),
            usage: NodeUsage::default(),
            evidence: None,
            succession: None,
            started_at_unix_ms: None,
            ended_at_unix_ms: None,
            restored_from: None,
        })
        .await
        .expect("bind plan");

    let nodes = store.list_run_nodes(&run).await.expect("list_run_nodes");
    let plan = nodes
        .iter()
        .find(|node| node.instance_path == InstancePath::new("plan"))
        .expect("the plan node");
    assert_eq!(plan.node_dir.as_deref(), Some("/tmp/runs/r1/plan"));
    assert_eq!(plan.cwd.as_deref(), Some("/tmp/work"));
    assert_eq!(plan.pane_id.as_deref(), Some("w1:p2"));
    // M2 (`07-phase3-plan.md` §0.5, §1 WS-B): the writer has always persisted
    // this; the gap was `RunNodeRecord`/`run_node_record` dropping it on read.
    assert_eq!(
        plan.transcript_path.as_deref(),
        Some("/tmp/runs/r1/plan.jsonl")
    );

    let unbound = nodes
        .iter()
        .find(|node| node.instance_path == InstancePath::new("join"))
        .expect("the join node");
    assert_eq!(unbound.node_dir, None, "an unbound node has no node dir");
    assert_eq!(unbound.cwd, None);
    assert_eq!(unbound.transcript_path, None);
}

/// C6: `depth` disagreed between the live graph (always 0) and the store
/// (longest path from a root). `depth` pairs with `run_node.parent` and is what
/// `04` §3.4 budgets with `max_depth`, so a statically declared graph consumes
/// none of it — the live reading is the correct one.
#[tokio::test]
async fn statically_materialised_run_nodes_all_sit_at_expansion_depth_zero() {
    let store = open_mem_store().await;
    let (_, run) = setup_diamond_run(&store, "depth", None).await;

    let nodes = store.list_run_nodes(&run).await.expect("list_run_nodes");
    assert_eq!(nodes.len(), 4);
    for node in &nodes {
        assert_eq!(
            node.depth, 0,
            "{} is statically declared, so it spends no expansion budget",
            node.instance_path
        );
    }

    // The same graph materialised in memory, which is what a live run answers
    // from. The two projections have to agree field for field.
    let kvdag = store
        .load_version(
            &store
                .get_run(&run)
                .await
                .expect("get_run")
                .expect("the run exists")
                .version,
        )
        .await
        .expect("load_version");
    let live = crate::workflow::model::RunGraph::materialise(&kvdag, run.clone(), Tier::Auto);
    for node in &nodes {
        let live_node = live
            .node_by_path(&node.instance_path)
            .unwrap_or_else(|| panic!("the live graph has {}", node.instance_path));
        assert_eq!(
            u32::from(node.depth),
            u32::from(live_node.depth),
            "depth disagrees for {}",
            node.instance_path
        );
    }
}

// ── 5: cycle rejection ───────────────────────────────────────────────────

#[tokio::test]
async fn create_version_rejects_a_cycle() {
    let store = open_mem_store().await;
    let workflow = store
        .create_workflow("demo", "", Tier::Auto)
        .await
        .expect("create_workflow");

    let mut spec = diamond_spec();
    spec.edges.push(edge("join", "plan", None));
    let error = store
        .create_version(&workflow, VersionOrigin::Authored, "cyclic", spec)
        .await
        .expect_err("a cyclic graph must be rejected");
    assert!(
        matches!(error, StoreError::InvalidGraph(KvdagError::Cycle(_))),
        "{error:?}"
    );
}

// ── 6: run_event seq uniqueness under concurrent appends ────────────────

#[tokio::test]
async fn run_event_seq_is_unique_and_survives_concurrent_appends() {
    let store = open_mem_store().await;
    let (_, _, run) = setup_run(&store).await;

    fn event(run: &RunId, seq: u64) -> StoreWrite {
        StoreWrite::RunEvent {
            run: run.clone(),
            seq,
            kind: RunEventKind::NodeStatus,
            path: None,
            payload: serde_json::json!({}),
            at_unix_ms: 1,
        }
    }

    let (first, second, third, fourth) = tokio::join!(
        store.write(event(&run, 0)),
        store.write(event(&run, 1)),
        store.write(event(&run, 2)),
        store.write(event(&run, 3)),
    );
    first.expect("seq 0");
    second.expect("seq 1");
    third.expect("seq 2");
    fourth.expect("seq 3");

    let events = store.list_run_events(&run).await.expect("list_run_events");
    let seqs: Vec<u64> = events.iter().map(|event| event.seq).collect();
    assert_eq!(seqs, vec![0, 1, 2, 3]);

    let duplicate = store.write(event(&run, 0)).await;
    assert!(
        duplicate.is_err(),
        "a duplicate seq must be rejected by the unique index"
    );
}

// ── 7: checkpoint spill ──────────────────────────────────────────────────

#[tokio::test]
async fn checkpoint_payload_over_budget_is_not_stored_inline() {
    let store = open_mem_store().await;
    let (_, _, run) = setup_run(&store).await;

    let huge_report = "x".repeat(512 * 1024);
    store
        .write(StoreWrite::Checkpoint {
            run: run.clone(),
            path: InstancePath::new("solo"),
            seq: 0,
            kind: CheckpointKind::Result,
            schema_valid: true,
            payload: serde_json::json!({"report": huge_report}),
            summary: "ok".to_string(),
            artifact_paths: vec!["nodes/solo/result.json".to_string()],
            digest: "deadbeef".to_string(),
        })
        .await
        .expect("checkpoint writes");

    let checkpoints = store
        .list_checkpoints(&run, &InstancePath::new("solo"))
        .await
        .expect("list_checkpoints");
    assert_eq!(checkpoints.len(), 1);
    let checkpoint = &checkpoints[0];
    let stored_bytes = serde_json::to_string(&checkpoint.payload)
        .expect("json")
        .len();
    assert!(
        stored_bytes < 1024,
        "an over-budget payload must not be stored inline, got {stored_bytes} bytes"
    );
    assert_eq!(
        checkpoint.artifact_paths,
        vec!["nodes/solo/result.json".to_string()],
        "the artifact path the caller already spilled to must still be recorded"
    );
}

// ── 8: review_finding replace requires a replacement ────────────────────

#[tokio::test]
async fn review_finding_replace_verdict_requires_a_replacement() {
    let store = open_mem_store().await;
    let response = store
        .db
        .query(
            "CREATE review_finding SET cycle = review_cycle:fake, node_key = \"plan\", \
             level = \"prompt\", verdict = \"replace\", rationale = \"needs a rewrite\"",
        )
        .await
        .expect("query executes");
    assert!(
        response.check().is_err(),
        "a \"replace\" verdict with no replacement must be rejected"
    );

    let response = store
        .db
        .query(
            "CREATE review_finding SET cycle = review_cycle:fake, node_key = \"plan\", \
             level = \"prompt\", verdict = \"replace\", rationale = \"needs a rewrite\", \
             replacement = { role: \"a better teammate\" }",
        )
        .await
        .expect("query executes");
    assert!(
        response.check().is_ok(),
        "a \"replace\" verdict with a replacement is accepted"
    );
}

// ── 9: restore query returns only valid results ──────────────────────────

#[tokio::test]
async fn find_restorable_checkpoints_returns_only_valid_results() {
    let store = open_mem_store().await;
    let (_, _, run) = setup_run(&store).await;

    store
        .write(StoreWrite::Checkpoint {
            run: run.clone(),
            path: InstancePath::new("solo"),
            seq: 0,
            kind: CheckpointKind::Result,
            schema_valid: true,
            payload: serde_json::json!({"report": "ok"}),
            summary: "ok".to_string(),
            artifact_paths: Vec::new(),
            digest: "valid-result".to_string(),
        })
        .await
        .expect("valid result checkpoint");
    store
        .write(StoreWrite::Checkpoint {
            run: run.clone(),
            path: InstancePath::new("solo"),
            seq: 1,
            kind: CheckpointKind::Result,
            schema_valid: false,
            payload: serde_json::json!({}),
            summary: "bad".to_string(),
            artifact_paths: Vec::new(),
            digest: "invalid-result".to_string(),
        })
        .await
        .expect("invalid result checkpoint");
    store
        .write(StoreWrite::Checkpoint {
            run: run.clone(),
            path: InstancePath::new("solo"),
            seq: 2,
            kind: CheckpointKind::Partial,
            schema_valid: true,
            payload: serde_json::json!({}),
            summary: "partial".to_string(),
            artifact_paths: Vec::new(),
            digest: "partial".to_string(),
        })
        .await
        .expect("partial checkpoint");

    let restorable = store
        .find_restorable_checkpoints(&run, &[NodeKey::new("solo")])
        .await
        .expect("find_restorable_checkpoints");
    assert_eq!(restorable.len(), 1);
    assert_eq!(restorable[0].digest, "valid-result");
}

// ── 10: prune deletes whole runs and preserves every run_summary ────────

#[tokio::test]
async fn prune_run_history_deletes_whole_runs_and_preserves_every_summary() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;

    let mut runs = Vec::new();
    for _ in 0..3 {
        let run = create_run(&store, &workflow, &kvdag).await;
        seed_run_summary(&store, &run, &kvdag.version_id).await;
        runs.push(run);
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }

    let pruned = store
        .prune_run_history(&workflow, 1)
        .await
        .expect("prune_run_history");
    assert_eq!(pruned, 2);

    for run in &runs {
        let summary = store.get_run_summary(run).await.expect("get_run_summary");
        assert!(summary.is_some(), "every run_summary must survive pruning");
    }
    assert!(
        store.get_run(&runs[0]).await.expect("get_run").is_none(),
        "the pruned run's own record must be gone"
    );
    assert!(
        store.get_run(&runs[2]).await.expect("get_run").is_some(),
        "the retained run must survive"
    );
}

// ── 11: prune leaves no dangling record<run_node> reference ─────────────

#[tokio::test]
async fn prune_run_history_leaves_no_dangling_references() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;

    let pruned_run = create_run(&store, &workflow, &kvdag).await;
    seed_run_summary(&store, &pruned_run, &kvdag.version_id).await;
    let run_node_id = store
        .find_run_node_id(&pruned_run, &InstancePath::new("solo"))
        .await
        .expect("find_run_node_id");
    let interrogation_id = seed_interrogation(&store, &run_node_id).await;
    let cycle_id = seed_review_cycle(&store, &pruned_run, &kvdag.version_id).await;
    seed_review_finding(&store, &cycle_id, &interrogation_id).await;

    let run_id = parse_record_id(TABLE_WORKFLOW_RUN, pruned_run.as_str()).expect("run id");
    let response = store
        .db
        .query("UPDATE run_summary SET generated_by = $node WHERE run = $run")
        .bind(("node", run_node_id.clone()))
        .bind(("run", run_id))
        .await
        .expect("point the summary at this run's node");
    response.check().expect("update succeeds");

    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    let kept_run = create_run(&store, &workflow, &kvdag).await;
    seed_run_summary(&store, &kept_run, &kvdag.version_id).await;

    let pruned = store
        .prune_run_history(&workflow, 1)
        .await
        .expect("prune_run_history");
    assert_eq!(pruned, 1);

    assert!(
        store
            .get_run_summary(&pruned_run)
            .await
            .expect("get_run_summary")
            .is_some(),
        "the summary itself survives"
    );

    #[derive(Debug, Clone, SurrealValue)]
    struct GeneratedBy {
        generated_by: Option<surrealdb_types::RecordId>,
    }
    let run_id = parse_record_id(TABLE_WORKFLOW_RUN, pruned_run.as_str()).expect("run id");
    let mut response = store
        .db
        .query("SELECT generated_by FROM run_summary WHERE run = $run")
        .bind(("run", run_id))
        .await
        .expect("select run_summary");
    let rows: Vec<GeneratedBy> = response.take(0).expect("decode");
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].generated_by.is_none(),
        "generated_by must be nulled, not left dangling"
    );

    let mut response = store
        .db
        .query("SELECT * FROM interrogation")
        .await
        .expect("select interrogation");
    let remaining: Vec<records::InterrogationRow> = response.take(0).expect("decode");
    assert!(
        remaining
            .iter()
            .all(|row| record_id_to_string(&row.run_node) != record_id_to_string(&run_node_id)),
        "no interrogation row may reference a deleted run_node"
    );

    #[derive(Debug, Clone, SurrealValue)]
    struct InterviewOnly {
        interview: Option<surrealdb_types::RecordId>,
    }
    let mut response = store
        .db
        .query("SELECT interview FROM review_finding WHERE cycle = $cycle")
        .bind(("cycle", cycle_id))
        .await
        .expect("select review_finding");
    let findings: Vec<InterviewOnly> = response.take(0).expect("decode");
    assert_eq!(findings.len(), 1);
    assert!(
        findings[0].interview.is_none(),
        "interview must be nulled once its interrogation is pruned"
    );
}

// ── 12: store-locked path (on-disk) ──────────────────────────────────────

#[tokio::test]
#[ignore = "touches disk: opens a real SurrealKv lock"]
async fn opening_a_locked_directory_reports_unavailable() {
    let dir = unique_temp_dir("locked");
    let first = WorkflowStore::open(StoreLocation::OnDisk(dir.clone()))
        .await
        .expect("first open succeeds");

    let second = WorkflowStore::open(StoreLocation::OnDisk(dir.clone())).await;
    match second {
        Err(StoreError::Unavailable { reason, .. }) => {
            assert_eq!(reason, error::STORE_LOCKED);
        }
        other => panic!("expected a store_locked error, got {other:?}"),
    }

    drop(first);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── 13: on-disk round trip (on-disk) ─────────────────────────────────────

#[tokio::test]
#[ignore = "touches disk"]
async fn on_disk_round_trip_survives_close_and_reopen() {
    let dir = unique_temp_dir("roundtrip");

    let workflow = {
        let store = WorkflowStore::open(StoreLocation::OnDisk(dir.clone()))
            .await
            .expect("opens");
        let workflow = store
            .create_workflow("demo", "desc", Tier::Auto)
            .await
            .expect("create_workflow");
        store
            .create_version(&workflow, VersionOrigin::Authored, "v1", diamond_spec())
            .await
            .expect("create_version");
        workflow
    };

    // The local engine tears down its background router task (and, with it,
    // the SurrealKv lock file) asynchronously after the last `Surreal<Db>`
    // handle drops, so reopening immediately can race a lock that hasn't
    // been released yet. Retry briefly rather than sleeping a fixed amount.
    let store = open_with_retry(&dir).await;
    let summary = store
        .get_workflow(&workflow)
        .await
        .expect("get_workflow")
        .expect("workflow persisted across close/reopen");
    assert_eq!(summary.name, "demo");

    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fix-wave requirement (B1+B2 combined, restart durability): a run whose
/// growth was limited and that has expanded children must report the same
/// `growth_limited` objects (run + node) and `parent_path` values after a
/// real close/reopen of the on-disk store — not just `kv-mem`, which every
/// other test in this file uses and which never round-trips through a
/// SurrealKv file at all. `list_run_nodes`/`growth_limits` are exactly what
/// `src/app/api/workflows.rs::stored_run` calls for `run.get`/`node.get` once
/// this server is not the one executing the run, which a restart always
/// leaves it as.
#[tokio::test]
#[ignore = "touches disk"]
async fn a_restarted_store_reports_the_same_growth_limited_and_parent_path() {
    let dir = unique_temp_dir("restart");

    let (run, parent_path_before, limits_before) = {
        let store = WorkflowStore::open(StoreLocation::OnDisk(dir.clone()))
            .await
            .expect("opens");
        let (_, _, run) = setup_expandable_run(&store, "restart-durability").await;

        store
            .write(node_created_write(
                &run,
                "worker",
                "root/worker/1",
                Some("root"),
            ))
            .await
            .expect("create the child");

        store
            .write(StoreWrite::RunEvent {
                run: run.clone(),
                seq: 0,
                kind: RunEventKind::GrowthLimited,
                path: Some(InstancePath::new("root")),
                payload: serde_json::json!({
                    "reason": "max_nodes_reached",
                    "message": "max_nodes 3 reached; no nodes created",
                    "limit": "max_nodes",
                    "limit_value": 3,
                    "requested": 1,
                    "accepted": 0,
                }),
                at_unix_ms: 1,
            })
            .await
            .expect("growth_limited event");

        let nodes = store.list_run_nodes(&run).await.expect("list_run_nodes");
        let child = nodes
            .iter()
            .find(|record| record.instance_path.as_str() == "root/worker/1")
            .expect("the child is in the run");
        let parent_path_before = child.parent_path.clone();
        let limits_before = store.growth_limits(&run).await.expect("growth_limits");
        (run, parent_path_before, limits_before)
    };

    let store = open_with_retry(&dir).await;

    let nodes = store.list_run_nodes(&run).await.expect("list_run_nodes");
    let child = nodes
        .iter()
        .find(|record| record.instance_path.as_str() == "root/worker/1")
        .expect("the child survives the restart");
    assert_eq!(
        child.parent_path, parent_path_before,
        "parent_path must survive a real close/reopen of the store, not just kv-mem"
    );
    assert_eq!(
        child.parent_path,
        Some(InstancePath::new("root")),
        "and it must actually be the value the live server had, not just a stable None"
    );

    let limits_after = store.growth_limits(&run).await.expect("growth_limits");
    assert_eq!(
        limits_after, limits_before,
        "growth_limited must survive a real close/reopen of the store, not just kv-mem"
    );
    assert!(
        limits_after.last.is_some(),
        "and it must actually still carry the limit the live server recorded"
    );

    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── 14: Phase 2 — growth, create paths, assignments, node history ────────
//
// `06-phase2-plan.md` WS-C. Each case pins one of the four things the store
// gained: a create path that is not `create_run`, the invariants `NewRun`'s
// own doc has claimed since Phase 1, the single tier authority, and the
// `NodeHistory` aggregation `auto` has always described and never had.

/// The `spawned` relation, read back raw. `records.rs` has no row for it
/// because until Phase 2 the table had no writer at all — only the `DELETE`
/// in `prune_run_history`.
#[derive(Debug, Clone, SurrealValue)]
struct SpawnedRow {
    r#in: surrealdb_types::RecordId,
    out: surrealdb_types::RecordId,
    run: surrealdb_types::RecordId,
    template_key: String,
    proposal_id: String,
}

/// `root` fans out into the `worker` template; `report` is the fan-in point an
/// expansion child inherits (§4 D4).
fn expandable_spec() -> KvdagSpec {
    let mut template = node("worker", "Work on {{goal}}");
    template.is_template = true;
    let mut root = node("root", "Plan for: {{goal}}");
    root.expand_allow = vec![NodeKey::new("worker")];
    root.expand_max = 4;
    base_spec(
        vec![root, node("report", "Report on {{work}}"), template],
        vec![edge("root", "report", Some("work"))],
    )
}

async fn setup_expandable_run(store: &WorkflowStore, name: &str) -> (WorkflowId, Kvdag, RunId) {
    let workflow = store
        .create_workflow(name, "", Tier::Auto)
        .await
        .expect("create_workflow");
    let kvdag = store
        .create_version(
            &workflow,
            VersionOrigin::Authored,
            "expandable",
            expandable_spec(),
        )
        .await
        .expect("create_version");
    let run = create_run(store, &workflow, &kvdag).await;
    (workflow, kvdag, run)
}

fn node_created_write(run: &RunId, key: &str, path: &str, parent: Option<&str>) -> StoreWrite {
    labelled_node_created_write(run, key, path, parent, "", &[])
}

/// [`node_created_write`] with the two facts that describe *this* child rather
/// than the template it is cut from: the proposing node's `--label` and its
/// accepted `--input k=v` overrides (`04-kvdag-and-execution.md` §3.4).
fn labelled_node_created_write(
    run: &RunId,
    key: &str,
    path: &str,
    parent: Option<&str>,
    label: &str,
    inputs: &[(&str, &str)],
) -> StoreWrite {
    StoreWrite::RunNodeCreated {
        run: run.clone(),
        key: NodeKey::new(key),
        path: InstancePath::new(path),
        label: label.to_string(),
        inputs: inputs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect(),
        parent: parent.map(InstancePath::new),
        depth: 1,
        status: NodeStatus::Ready,
        demand: Demand::Standard,
        assignment: crate::workflow::tier::Assignment {
            model: crate::workflow::tier::ModelAlias::Sonnet,
            effort: crate::workflow::tier::Effort::Xhigh,
        },
        assignment_reason: "auto/downgrade-standard".to_string(),
        attempt: 1,
        proposal_id: "proposal-1".to_string(),
    }
}

// H1 / §4 D15 — one authority for `started_at`.

#[tokio::test]
async fn create_run_binds_started_at_verbatim_so_a_reload_reports_the_apps_stamp() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;

    let stamp = 1_699_000_000_777;
    let run = store
        .create_run(NewRun {
            started_at_unix_ms: stamp,
            ..new_run(&workflow, &kvdag)
        })
        .await
        .expect("create_run");

    // Reading the run back goes through the stored row, which is the only
    // thing a restarted server has. Before migration `0002` this reported
    // `time::now()` at queue-drain time instead — a second clock, 2-3 ms
    // later than the one `ActiveRun` carries.
    let reloaded = store
        .get_run(&run)
        .await
        .expect("get_run")
        .expect("the run exists");
    assert_eq!(reloaded.started_at_unix_ms, stamp);

    let listed = store
        .list_runs(Some(&workflow), 10)
        .await
        .expect("list_runs")
        .into_iter()
        .find(|record| record.id == run)
        .expect("the run is listed");
    assert_eq!(
        listed.started_at_unix_ms, stamp,
        "every projection of the run reports the one stamp it was created with"
    );
}

// §4 D14/D3 narrowing — `workflow_run.max_nodes <= kvdag_version.max_nodes`.

#[tokio::test]
async fn create_run_rejects_growth_wider_than_its_version_on_either_axis() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;
    let version_growth = kvdag.growth;

    for widened in [
        GrowthLimits {
            max_depth: version_growth.max_depth + 1,
            max_nodes: version_growth.max_nodes,
        },
        GrowthLimits {
            max_depth: version_growth.max_depth,
            max_nodes: version_growth.max_nodes + 1,
        },
    ] {
        let error = store
            .create_run(NewRun {
                growth: widened,
                ..new_run(&workflow, &kvdag)
            })
            .await
            .expect_err("a run may narrow its version's limits, never widen them");
        assert!(
            matches!(error, StoreError::Invariant(_)),
            "expected an invariant violation, got {error:?}"
        );
    }
}

#[tokio::test]
async fn create_run_accepts_growth_equal_to_or_narrower_than_its_version() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;
    let version_growth = kvdag.growth;

    for accepted in [
        version_growth,
        GrowthLimits {
            max_depth: version_growth.max_depth.saturating_sub(1),
            max_nodes: version_growth.max_nodes.saturating_sub(6),
        },
    ] {
        let run = store
            .create_run(NewRun {
                growth: accepted,
                ..new_run(&workflow, &kvdag)
            })
            .await
            .expect("narrowing is the whole point of the run-level limits");
        let record = store
            .get_run(&run)
            .await
            .expect("get_run")
            .expect("the run exists");
        assert_eq!(record.max_depth, accepted.max_depth);
        assert_eq!(record.max_nodes, accepted.max_nodes);
    }
}

// §4 D9 — one resolver. The store writes the run's table verbatim.

#[tokio::test]
async fn materialise_run_nodes_writes_the_runs_assignment_table_verbatim() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;

    // Deliberately not what `tier::resolve(Auto, Standard, None)` would
    // produce: if the store still resolved tiers of its own, it would
    // overwrite these and the assertion below would read the policy's answer
    // instead of the run's.
    let mut assignments = BTreeMap::new();
    assignments.insert(
        NodeKey::new("solo"),
        NodeAssignment {
            model: crate::workflow::tier::ModelAlias::Fable,
            effort: crate::workflow::tier::Effort::Low,
            reason: "auto/escalate-recent-failures".to_string(),
        },
    );

    let run = store
        .create_run(NewRun {
            assignments,
            ..new_run(&workflow, &kvdag)
        })
        .await
        .expect("create_run");

    let nodes = store.list_run_nodes(&run).await.expect("list_run_nodes");
    let solo = nodes.first().expect("the run has one node");
    assert_eq!(solo.model, "fable");
    assert_eq!(solo.effort, "low");
    assert_eq!(solo.assignment_reason, "auto/escalate-recent-failures");
}

#[tokio::test]
async fn create_run_rejects_a_scheduled_node_with_no_resolved_assignment() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;

    let error = store
        .create_run(NewRun {
            assignments: BTreeMap::new(),
            ..new_run(&workflow, &kvdag)
        })
        .await
        .expect_err("a missing assignment is an invariant violation, not a fallback");
    assert!(
        matches!(error, StoreError::Invariant(_)),
        "expected an invariant violation, got {error:?}"
    );
}

// §4 D7 — the create paths.

#[tokio::test]
async fn a_created_run_node_round_trips_with_its_parent_depth_and_spawned_relation() {
    let store = open_mem_store().await;
    let (_, _, run) = setup_expandable_run(&store, "created-node").await;

    let before = store.list_run_nodes(&run).await.expect("list_run_nodes");
    assert_eq!(
        before.len(),
        2,
        "the template is not materialised until a proposal instantiates it"
    );

    store
        .write(node_created_write(
            &run,
            "worker",
            "root/worker/1",
            Some("root"),
        ))
        .await
        .expect("the create path exists now");

    let after = store.list_run_nodes(&run).await.expect("list_run_nodes");
    let child = after
        .iter()
        .find(|record| record.instance_path.as_str() == "root/worker/1")
        .expect("the child is in the run");
    assert_eq!(child.node_key.as_str(), "worker");
    assert_eq!(child.depth, 1, "first-generation children are depth 1");
    assert_eq!(child.status, NodeStatus::Ready);
    assert_eq!(child.model, "sonnet");
    assert_eq!(child.effort, "xhigh");
    assert_eq!(child.assignment_reason, "auto/downgrade-standard");
    // B-T1 (P2b, restart durability): `run_node.parent` is a real column, but
    // `run_node_record` used to drop it on the floor rather than resolving it
    // to the instance path a restarted server can report.
    assert_eq!(
        child.parent_path,
        Some(InstancePath::new("root")),
        "the child's provenance resolves to its proposing parent's path"
    );
    let root = before
        .iter()
        .find(|record| record.instance_path.as_str() == "root")
        .expect("the static root node is in the run");
    assert_eq!(
        root.parent_path, None,
        "a static node has no proposer to resolve"
    );

    // The run's denominator moves with the graph; a progress counter that
    // ignored expansion children would under-report every growing run.
    let record = store
        .get_run(&run)
        .await
        .expect("get_run")
        .expect("the run exists");
    assert_eq!(record.nodes_total, 3);

    let mut response = store
        .db
        .query("SELECT * FROM spawned")
        .await
        .expect("select spawned");
    let rows: Vec<SpawnedRow> = response.take(0).expect("decode spawned");
    let relation = rows.first().expect("the child carries its provenance");
    assert_eq!(relation.template_key, "worker");
    assert_eq!(relation.proposal_id, "proposal-1");
    assert_eq!(record_id_to_string(&relation.run), run.to_string());

    let mut ids = BTreeMap::new();
    for record in &after {
        let id = store
            .find_run_node_id(&run, &record.instance_path)
            .await
            .expect("every listed node resolves");
        ids.insert(record.instance_path.to_string(), record_id_to_string(&id));
    }
    assert_eq!(
        record_id_to_string(&relation.r#in),
        ids["root"],
        "the relation runs from the proposing parent"
    );
    assert_eq!(record_id_to_string(&relation.out), ids["root/worker/1"]);
}

/// B-T2 (P2b, restart durability): `growth_limited` is journalled as a
/// `run_event` row keyed to the proposing node, but before this fix nothing
/// read it back — a restarted server reported `growth_limited: null`
/// regardless of what had actually happened.
#[tokio::test]
async fn a_growth_limited_journal_entry_projects_back_as_a_run_and_node_fact() {
    let store = open_mem_store().await;
    let (_, _, run) = setup_expandable_run(&store, "growth-projects").await;

    store
        .write(node_created_write(
            &run,
            "worker",
            "root/worker/1",
            Some("root"),
        ))
        .await
        .expect("create the child");

    store
        .write(StoreWrite::RunEvent {
            run: run.clone(),
            seq: 0,
            kind: RunEventKind::GrowthLimited,
            path: Some(InstancePath::new("root")),
            payload: serde_json::json!({
                "reason": "max_nodes_reached",
                "message": "max_nodes 3 reached; no nodes created",
                "limit": "max_nodes",
                "limit_value": 3,
                "requested": 1,
                "accepted": 0,
            }),
            at_unix_ms: 1,
        })
        .await
        .expect("first growth_limited event");

    store
        .write(StoreWrite::RunEvent {
            run: run.clone(),
            seq: 1,
            kind: RunEventKind::GrowthLimited,
            path: Some(InstancePath::new("root/worker/1")),
            payload: serde_json::json!({
                "reason": "expand_max_reached",
                "message": "expand_max 2 reached; no nodes created",
                "limit": "expand_max",
                "limit_value": 2,
                "requested": 1,
                "accepted": 0,
            }),
            at_unix_ms: 1,
        })
        .await
        .expect("second growth_limited event");

    let limits = store.growth_limits(&run).await.expect("growth_limits");

    let last = limits.last.expect("the run's most recent limit");
    assert_eq!(last.kind, "expand_max");
    assert_eq!(last.limit_value, 2);
    assert_eq!(last.requested, 1);
    assert_eq!(last.accepted, 0);
    assert_eq!(last.message, "expand_max 2 reached; no nodes created");

    let by_root = limits
        .by_path
        .get(&InstancePath::new("root"))
        .expect("the first proposer's own limit is keyed by path");
    assert_eq!(by_root.kind, "max_nodes");
    assert_eq!(by_root.limit_value, 3);
    assert_eq!(by_root.requested, 1);
    assert_eq!(by_root.accepted, 0);

    let by_child = limits
        .by_path
        .get(&InstancePath::new("root/worker/1"))
        .expect("the second proposer's own limit is keyed by path");
    assert_eq!(by_child.kind, "expand_max");
    assert_eq!(by_child.limit_value, 2);
}

/// `workflow.run.list` answers about a whole page of runs, and its `limit` is
/// caller-supplied and uncapped, so it cannot afford one `growth_limits` call
/// per listed run. Without a batched read it would keep reporting
/// `growth_limited: null` for exactly the runs `workflow.run.get` now reports a
/// limit for — the same fact disagreeing with itself depending on which verb
/// asked. This also pins the `run IN $runs` filter, the one piece of SurrealQL
/// in the store with no other caller to catch a syntax drift.
#[tokio::test]
async fn listed_runs_report_the_same_last_growth_limit_that_run_get_does() {
    let store = open_mem_store().await;
    let (_, _, limited) = setup_expandable_run(&store, "growth-listed-a").await;
    let (_, _, untouched) = setup_expandable_run(&store, "growth-listed-b").await;

    for (seq, limit_value) in [(0, 3), (1, 5)] {
        store
            .write(StoreWrite::RunEvent {
                run: limited.clone(),
                seq,
                kind: RunEventKind::GrowthLimited,
                path: Some(InstancePath::new("root")),
                payload: serde_json::json!({
                    "reason": "max_nodes_reached",
                    "message": format!("max_nodes {limit_value} reached; no nodes created"),
                    "limit": "max_nodes",
                    "limit_value": limit_value,
                    "requested": 1,
                    "accepted": 0,
                }),
                at_unix_ms: 1,
            })
            .await
            .expect("growth_limited event");
    }

    let listed = store
        .last_growth_limit_by_run(&[limited.clone(), untouched.clone()])
        .await
        .expect("last_growth_limit_by_run");

    let last = listed
        .get(&limited)
        .expect("a run that hit a guardrail reports it when listed, not only when fetched");
    assert_eq!(
        last.limit_value, 5,
        "the run's most recent limit wins, the same last-write-wins rule `growth_limits` uses"
    );
    assert_eq!(
        Some(last),
        store
            .growth_limits(&limited)
            .await
            .expect("growth_limits")
            .last
            .as_ref(),
        "the batched read and the single-run read must not disagree"
    );
    assert!(
        !listed.contains_key(&untouched),
        "a run that never hit a guardrail contributes nothing, and must not pick up \
         another run's limit"
    );
    assert!(store
        .last_growth_limit_by_run(&[])
        .await
        .expect("an empty page is not an error")
        .is_empty());
}

/// R-4: `commit` emits the create before any update for the same path, and the
/// queue drains FIFO. The update must therefore land on the created row rather
/// than error against a row that does not exist yet.
#[tokio::test]
async fn a_create_then_update_for_the_same_path_lands_the_update() {
    let store = open_mem_store().await;
    let (_, _, run) = setup_expandable_run(&store, "create-then-update").await;

    for write in [
        node_created_write(&run, "worker", "root/worker/1", Some("root")),
        node_status_write(&run, "root/worker/1", NodeStatus::Running),
        node_status_write(&run, "root/worker/1", NodeStatus::Succeeded),
    ] {
        store.write(write).await.expect("drained in order");
    }

    let nodes = store.list_run_nodes(&run).await.expect("list_run_nodes");
    let child = nodes
        .iter()
        .find(|record| record.instance_path.as_str() == "root/worker/1")
        .expect("the child survived both writes");
    assert_eq!(child.status, NodeStatus::Succeeded);
    assert_eq!(
        store
            .get_run(&run)
            .await
            .expect("get_run")
            .expect("the run exists")
            .nodes_done,
        1
    );
}

#[tokio::test]
async fn created_run_edges_carry_authored_provenance_only_when_there_is_any() {
    let store = open_mem_store().await;
    let (_, _, run) = setup_expandable_run(&store, "created-edges").await;

    store
        .write(node_created_write(
            &run,
            "worker",
            "root/worker/1",
            Some("root"),
        ))
        .await
        .expect("create the child");

    // (a) the synthetic parent -> child sequence edge, which has no authored
    // counterpart, and (b) the child's inherited copy of its parent's
    // outbound data edge, which does.
    store
        .write(StoreWrite::RunEdgeCreated {
            run: run.clone(),
            from: InstancePath::new("root"),
            to: InstancePath::new("root/worker/1"),
            kind: EdgeKind::Sequence,
            kvdag_edge: None,
            condition_result: None,
            fired: false,
        })
        .await
        .expect("synthetic sequence edge");
    store
        .write(StoreWrite::RunEdgeCreated {
            run: run.clone(),
            from: InstancePath::new("root/worker/1"),
            to: InstancePath::new("report"),
            kind: EdgeKind::Data,
            kvdag_edge: Some((NodeKey::new("root"), NodeKey::new("report"))),
            condition_result: Some(true),
            fired: true,
        })
        .await
        .expect("inherited outbound edge");

    let edges = store.list_run_edges(&run).await.expect("list_run_edges");
    let inherited = edges
        .iter()
        .find(|edge| edge.from.as_str() == "root/worker/1")
        .expect("the fan-in point survives expansion");
    assert_eq!(inherited.to.as_str(), "report");
    assert_eq!(inherited.kind, EdgeKind::Data);
    assert_eq!(inherited.condition_result, Some(true));
    assert!(inherited.fired);

    let mut response = store
        .db
        .query("SELECT * FROM run_edge WHERE run = $run")
        .bind((
            "run",
            parse_record_id(TABLE_WORKFLOW_RUN, run.as_str()).expect("run id"),
        ))
        .await
        .expect("select run_edge");
    let rows: Vec<records::RunEdgeRow> = response.take(0).expect("decode run_edge");
    let synthetic = rows
        .iter()
        .find(|row| row.kind == "sequence")
        .expect("the sequence edge exists");
    assert!(
        synthetic.kvdag_edge.is_none(),
        "the parent -> child sequence edge has no authored counterpart"
    );
    let inherited_row = rows
        .iter()
        .find(|row| row.kind == "data" && row.condition_result == Some(true))
        .expect("the inherited edge exists");
    assert!(
        inherited_row.kvdag_edge.is_some(),
        "an inherited edge keeps the authored edge it copies"
    );
}

// H5 — the workflow row tracks its head.

#[tokio::test]
async fn create_version_with_metadata_refreshes_the_workflow_row_even_for_a_no_op_graph() {
    let store = open_mem_store().await;
    let workflow = store
        .create_workflow("head-metadata", "the original description", Tier::Auto)
        .await
        .expect("create_workflow");
    let first = store
        .create_version(&workflow, VersionOrigin::Authored, "v1", diamond_spec())
        .await
        .expect("create_version");
    store
        .set_head_version(&workflow, &first.version_id)
        .await
        .expect("set_head_version");

    // The graph is byte-identical, so this is the no-op-revision path — which
    // is exactly the update that used to leave the description stale, because
    // `kvdag_version` has no description column to read instead.
    let metadata = VersionMetadata {
        description: "the updated description".to_string(),
        default_tier: Tier::Low,
    };
    let second = store
        .create_version_with_metadata(
            &workflow,
            VersionOrigin::Authored,
            "v1 again",
            diamond_spec(),
            Some(&metadata),
        )
        .await
        .expect("create_version_with_metadata");
    assert_eq!(
        second.version_id, first.version_id,
        "an identical graph must still not write a new version"
    );

    let summary = store
        .get_workflow(&workflow)
        .await
        .expect("get_workflow")
        .expect("the workflow exists");
    assert_eq!(summary.description, "the updated description");
    assert_eq!(summary.default_tier, Tier::Low);
}

#[tokio::test]
async fn create_version_without_metadata_leaves_the_workflow_row_alone() {
    let store = open_mem_store().await;
    let workflow = store
        .create_workflow("no-metadata", "the original description", Tier::High)
        .await
        .expect("create_workflow");
    store
        .create_version(&workflow, VersionOrigin::Authored, "v1", diamond_spec())
        .await
        .expect("create_version");

    let summary = store
        .get_workflow(&workflow)
        .await
        .expect("get_workflow")
        .expect("the workflow exists");
    assert_eq!(summary.description, "the original description");
    assert_eq!(summary.default_tier, Tier::High);
}

// §4 D8 — `NodeHistory` gets a producer.

/// Closes `solo` in one run and closes the run, so `node_history` has a
/// measurement to find. `attempt > 1` is how a restart reads: the node
/// succeeded, but not on its first pass.
async fn record_solo_run(
    store: &WorkflowStore,
    workflow: &WorkflowId,
    kvdag: &Kvdag,
    status: NodeStatus,
    attempt: u8,
    schema_failures: u32,
) -> RunId {
    let run = create_run(store, workflow, kvdag).await;
    for seq in 0..schema_failures {
        store
            .write(StoreWrite::Checkpoint {
                run: run.clone(),
                path: InstancePath::new("solo"),
                seq: u64::from(seq) + 1,
                kind: CheckpointKind::Result,
                schema_valid: false,
                payload: serde_json::json!({"report": "malformed"}),
                summary: "did not match the schema".to_string(),
                artifact_paths: Vec::new(),
                digest: format!("digest-{seq}"),
            })
            .await
            .expect("checkpoint");
    }
    store
        .write(StoreWrite::RunNode {
            run: run.clone(),
            path: InstancePath::new("solo"),
            status,
            attempt,
            binding: None,
            usage: NodeUsage::default(),
            evidence: None,
            succession: None,
            started_at_unix_ms: None,
            ended_at_unix_ms: None,
            restored_from: None,
        })
        .await
        .expect("close the node");
    store
        .write(StoreWrite::RunStatus {
            run: run.clone(),
            status: if status == NodeStatus::Succeeded {
                RunStatus::Succeeded
            } else {
                RunStatus::Failed
            },
            ended_at_unix_ms: None,
        })
        .await
        .expect("close the run");
    run
}

#[tokio::test]
async fn node_history_aggregates_the_workflows_closed_runs() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;

    // Oldest first: a clean first pass, then a restart that only succeeded on
    // its second attempt after one schema failure, then an outright failure.
    record_solo_run(&store, &workflow, &kvdag, NodeStatus::Succeeded, 1, 0).await;
    record_solo_run(&store, &workflow, &kvdag, NodeStatus::Succeeded, 2, 1).await;
    record_solo_run(&store, &workflow, &kvdag, NodeStatus::Failed, 1, 0).await;

    let history = store
        .node_history(&workflow, &NodeKey::new("solo"), 10)
        .await
        .expect("node_history");
    assert_eq!(history.runs, 3);
    assert_eq!(history.first_pass_successes, 1);
    assert_eq!(history.schema_failures, 1);
    assert_eq!(
        history.recent_first_pass_failures, 2,
        "the two most recent runs both missed on the first pass"
    );
    assert!(
        (history.first_pass_success_rate() - 1.0 / 3.0).abs() < f64::EPSILON,
        "{} is not what the runs say",
        history.first_pass_success_rate()
    );
    assert_eq!(
        history.watchdog_interventions, 0,
        "the column exists but Phase 4 owns its writer; 0 is documented, not fabricated"
    );
    assert_eq!(
        history.mean_tokens, 0,
        "run_node.total_tokens is permanently 0, which is why resolve_auto never reads it"
    );
}

#[tokio::test]
async fn node_history_windows_to_the_most_recent_runs_and_ignores_open_ones() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;

    record_solo_run(&store, &workflow, &kvdag, NodeStatus::Failed, 1, 0).await;
    record_solo_run(&store, &workflow, &kvdag, NodeStatus::Succeeded, 1, 0).await;
    // Still running, so not a measurement.
    create_run(&store, &workflow, &kvdag).await;

    let windowed = store
        .node_history(&workflow, &NodeKey::new("solo"), 1)
        .await
        .expect("node_history");
    assert_eq!(windowed.runs, 1, "one run's worth of window");
    assert_eq!(
        windowed.first_pass_successes, 1,
        "the window takes the most recent closed run, not the oldest"
    );

    let whole = store
        .node_history(&workflow, &NodeKey::new("solo"), 10)
        .await
        .expect("node_history");
    assert_eq!(whole.runs, 2, "the still-open run is not a measurement");
}

#[tokio::test]
async fn node_history_reports_no_runs_once_every_run_has_been_pruned() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;
    record_solo_run(&store, &workflow, &kvdag, NodeStatus::Succeeded, 1, 0).await;
    record_solo_run(&store, &workflow, &kvdag, NodeStatus::Succeeded, 1, 0).await;

    let pruned = store
        .prune_run_history(&workflow, 0)
        .await
        .expect("prune_run_history");
    assert_eq!(pruned, 2);

    let history = store
        .node_history(&workflow, &NodeKey::new("solo"), 10)
        .await
        .expect("node_history tolerates a fully pruned workflow");
    assert_eq!(history, crate::workflow::tier::NodeHistory::default());
}

#[tokio::test]
async fn node_history_is_empty_for_an_unknown_node_or_a_zero_window() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;
    record_solo_run(&store, &workflow, &kvdag, NodeStatus::Succeeded, 1, 0).await;

    let unknown = store
        .node_history(&workflow, &NodeKey::new("never-existed"), 10)
        .await
        .expect("node_history");
    assert_eq!(unknown.runs, 0);

    let no_window = store
        .node_history(&workflow, &NodeKey::new("solo"), 0)
        .await
        .expect("node_history");
    assert_eq!(no_window.runs, 0);
}

// Migration 0002 on top of a 0001-only database.

#[tokio::test]
async fn migration_0002_applies_over_a_0001_only_database_and_backfills_its_columns() {
    let store = WorkflowStore::open_with_migrations(StoreLocation::Memory, 1)
        .await
        .expect("a 0001-only database opens");
    assert_eq!(
        store.applied_migrations().await.expect("read schema_meta"),
        std::collections::BTreeSet::from(["0001_init".to_string()])
    );

    // Rows written the way a pre-Phase-2 karvex wrote them: no
    // `assignment_reason`, no `first_pass_succeeded`, no `schema_failures`.
    // `workflow`/`kvdag_version`/`kvdag_node` are untouched by `0002`, so
    // their normal writers are fine here.
    let (workflow, kvdag) = setup_workflow(&store).await;
    let version_id =
        parse_record_id(TABLE_KVDAG_VERSION, kvdag.version_id.as_str()).expect("version id");
    let mut response = store
        .db
        .query("SELECT * FROM kvdag_node WHERE version = $version LIMIT 1")
        .bind(("version", version_id.clone()))
        .await
        .expect("select kvdag_node");
    let node_rows: Vec<records::KvdagNodeRow> = response.take(0).expect("decode kvdag_node");
    let kvdag_node_id = node_rows.into_iter().next().expect("the node exists").id;

    let mut response = store
        .db
        .query(
            "CREATE workflow_run SET workflow = $workflow, kvdag_version = $version, \
             tier = \"auto\", status = \"succeeded\", max_depth = 3, max_nodes = 24, \
             nodes_total = 1 RETURN AFTER",
        )
        .bind((
            "workflow",
            parse_record_id(TABLE_WORKFLOW, workflow.as_str()).expect("workflow id"),
        ))
        .bind(("version", version_id))
        .await
        .expect("create workflow_run");
    let run_rows: Vec<IdOnly> = response.take(0).expect("decode workflow_run");
    let run_row_id = run_rows.into_iter().next().expect("the run row").id;
    let run = RunId::new(record_id_to_string(&run_row_id));

    let response = store
        .db
        .query(
            "CREATE run_node SET run = $run, kvdag_node = $kvdag_node, node_key = \"solo\", \
             instance_path = \"solo\", depth = 0, status = \"succeeded\", model = \"opus\", \
             effort = \"high\", demand = \"standard\"",
        )
        .bind(("run", run_row_id))
        .bind(("kvdag_node", kvdag_node_id))
        .await
        .expect("create run_node");
    response.check().expect("the 0001 row is valid");

    store
        .migrate()
        .await
        .expect("0002 through 0005 apply on top of 0001");
    assert_eq!(
        store.applied_migrations().await.expect("read schema_meta"),
        std::collections::BTreeSet::from([
            "0001_init".to_string(),
            "0002_growth_and_history".to_string(),
            "0003_node_identity".to_string(),
            "0004_journal_time_and_interrogation".to_string(),
            "0005_lead_binding_and_projection".to_string(),
        ])
    );

    // The read path decodes the pre-migration row, which is only true because
    // `0002` and `0003` backfill: a `DEFAULT` is applied at write time, not at
    // read time.
    let nodes = store
        .list_run_nodes(&run)
        .await
        .expect("a pre-migration run_node still decodes");
    let solo = nodes.first().expect("the node survived the migration");
    assert_eq!(solo.assignment_reason, "");
    assert_eq!(solo.model, "opus");
    assert_eq!(
        solo.label, "",
        "a row that predates the column carries no instance label; the renderers fall back"
    );
    assert!(solo.inputs.is_empty());

    let history = store
        .node_history(&workflow, &NodeKey::new("solo"), 10)
        .await
        .expect("node_history");
    assert_eq!(history.runs, 1);
    assert_eq!(
        history.first_pass_successes, 0,
        "a row that predates the column has no first-pass evidence to report"
    );
}

/// The two facts that describe an expansion child rather than the template it
/// is cut from have to survive the process that created them: a run read back
/// after a restart must still know which shard `worker/1` was and what it was
/// told to work on. Before this column pair, both lived only in the
/// `expand_accepted` journal payload and nothing on the read path could see
/// them.
#[tokio::test]
async fn a_created_run_node_persists_its_label_and_input_overrides() {
    let store = open_mem_store().await;
    let (_, _, run) = setup_expandable_run(&store, "created-node-identity").await;

    for (path, label, focus) in [
        ("root/worker/1", "Shard: auth", "src/auth"),
        ("root/worker/2", "Shard: ui", "src/ui"),
    ] {
        store
            .write(labelled_node_created_write(
                &run,
                "worker",
                path,
                Some("root"),
                label,
                &[("focus", focus)],
            ))
            .await
            .expect("the create carries the proposal's description");
    }

    let described: Vec<(String, String, String)> = store
        .list_run_nodes(&run)
        .await
        .expect("list_run_nodes")
        .into_iter()
        .filter(|record| record.instance_path.as_str().starts_with("root/worker/"))
        .map(|record| {
            (
                record.instance_path.to_string(),
                record.label.clone(),
                record
                    .inputs
                    .get("focus")
                    .cloned()
                    .unwrap_or_else(|| "<none>".to_string()),
            )
        })
        .collect();
    assert_eq!(
        described,
        vec![
            (
                "root/worker/1".to_string(),
                "Shard: auth".to_string(),
                "src/auth".to_string()
            ),
            (
                "root/worker/2".to_string(),
                "Shard: ui".to_string(),
                "src/ui".to_string()
            ),
        ],
        "two siblings of one generation must read back apart"
    );

    // A static node's row carries the authored kvdag label, so one column
    // answers for both kinds of node.
    let root = store
        .list_run_nodes(&run)
        .await
        .expect("list_run_nodes")
        .into_iter()
        .find(|record| record.instance_path.as_str() == "root")
        .expect("the static proposer is in the run");
    assert_eq!(
        root.label, "root",
        "the fixture's authored label; the point is that the column is written at all"
    );
    assert!(root.inputs.is_empty(), "nothing proposed a static node");
}

/// The variant allows a create with no provenance. It is not what expansion
/// produces, but it is a different SQL shape — no `RELATE`, one fewer
/// statement in the batch — so it gets its own case rather than being assumed.
#[tokio::test]
async fn a_created_run_node_without_a_parent_writes_no_spawned_relation() {
    let store = open_mem_store().await;
    let (_, _, run) = setup_expandable_run(&store, "created-node-orphan").await;

    store
        .write(node_created_write(&run, "worker", "worker/1", None))
        .await
        .expect("a create with no provenance is still a create");

    let nodes = store.list_run_nodes(&run).await.expect("list_run_nodes");
    assert!(nodes
        .iter()
        .any(|record| record.instance_path.as_str() == "worker/1"));

    let mut response = store
        .db
        .query("SELECT * FROM spawned")
        .await
        .expect("select spawned");
    let rows: Vec<SpawnedRow> = response.take(0).expect("decode spawned");
    assert!(rows.is_empty(), "no parent, no provenance edge");
}

// ── Phase 3, WS-B: migration 0004 ───────────────────────────────────────

#[tokio::test]
async fn migration_0004_applies_over_a_0001_to_0003_database() {
    let store = WorkflowStore::open_with_migrations(StoreLocation::Memory, 3)
        .await
        .expect("a 0001..0003 database opens");
    assert_eq!(
        store.applied_migrations().await.expect("read schema_meta"),
        std::collections::BTreeSet::from([
            "0001_init".to_string(),
            "0002_growth_and_history".to_string(),
            "0003_node_identity".to_string(),
        ])
    );

    let (workflow, kvdag) = setup_workflow(&store).await;
    // Not `create_run`: it decodes through `records::RunNodeRow`, which always
    // describes the *current* schema, so it cannot run against a deliberately
    // older migration level (migration `0005` adds three non-option `run_node`
    // columns). The 0002 test above writes its pre-migration rows in SQL for
    // the same reason.
    let run = create_run_row_directly(&store, &workflow, &kvdag).await;

    store
        .migrate()
        .await
        .expect("0004 and 0005 apply on top of 0001..0003, including over existing rows");
    assert_eq!(
        store.applied_migrations().await.expect("read schema_meta"),
        std::collections::BTreeSet::from([
            "0001_init".to_string(),
            "0002_growth_and_history".to_string(),
            "0003_node_identity".to_string(),
            "0004_journal_time_and_interrogation".to_string(),
            "0005_lead_binding_and_projection".to_string(),
        ])
    );

    // A pre-0004 row still reads back: the OVERWRITE statements touched
    // `run_event.at`/`run_node.kvdag_node`/`interrogation.forked_session_id`,
    // none of which this run has rows for yet, but the migration itself must
    // not choke applying `OVERWRITE` over a schemafull table with rows.
    let reloaded = store
        .get_run(&run)
        .await
        .expect("get_run")
        .expect("run exists");
    assert_eq!(reloaded.id, run);

    // `workflow.pruned_runs` (D15 point 5) is now writable.
    let pruned = store
        .prune_run_history(&workflow, 0)
        .await
        .expect("prune_run_history");
    assert_eq!(pruned, 1);

    #[derive(Debug, Clone, SurrealValue)]
    struct PrunedRuns {
        pruned_runs: i64,
    }
    let mut response = store
        .db
        .query("SELECT pruned_runs FROM $workflow")
        .bind((
            "workflow",
            parse_record_id(TABLE_WORKFLOW, workflow.as_str()).expect("workflow id"),
        ))
        .await
        .expect("select workflow");
    let rows: Vec<PrunedRuns> = response.take(0).expect("decode workflow");
    assert_eq!(rows.first().expect("workflow row exists").pruned_runs, 1);
}

// ── Phase 3, WS-B: the epilogue's reserved-path create (§4 D5, D15) ────────

#[tokio::test]
async fn epilogue_node_created_with_no_kvdag_node_and_excluded_from_counters() {
    let store = open_mem_store().await;
    let (_, _, run) = setup_run(&store).await;

    let before = store
        .get_run(&run)
        .await
        .expect("get_run")
        .expect("run exists");
    assert_eq!(before.nodes_total, 1);

    store
        .write(StoreWrite::RunNodeCreated {
            run: run.clone(),
            key: NodeKey::new(SUMMARY_INSTANCE_PATH),
            path: InstancePath::new(SUMMARY_INSTANCE_PATH),
            label: "summary".to_string(),
            inputs: BTreeMap::new(),
            parent: None,
            depth: 0,
            status: NodeStatus::Ready,
            demand: Demand::Light,
            assignment: crate::workflow::tier::Assignment {
                model: crate::workflow::tier::ModelAlias::Sonnet,
                effort: crate::workflow::tier::Effort::Xhigh,
            },
            assignment_reason: "the end-of-run summariser".to_string(),
            attempt: 1,
            proposal_id: String::new(),
        })
        .await
        .expect("the reserved path is the one create allowed to bind kvdag_node: NONE");

    let nodes = store.list_run_nodes(&run).await.expect("list_run_nodes");
    let epilogue = nodes
        .iter()
        .find(|record| record.instance_path.as_str() == SUMMARY_INSTANCE_PATH)
        .expect("the epilogue row exists");
    assert_eq!(epilogue.status, NodeStatus::Ready);

    let after_create = store
        .get_run(&run)
        .await
        .expect("get_run")
        .expect("run exists");
    assert_eq!(
        after_create.nodes_total, 1,
        "the epilogue must never inflate nodes_total (§4 D5)"
    );

    store
        .write(StoreWrite::RunNode {
            run: run.clone(),
            path: InstancePath::new(SUMMARY_INSTANCE_PATH),
            status: NodeStatus::Succeeded,
            attempt: 1,
            binding: None,
            usage: NodeUsage::default(),
            evidence: Some(Evidence::SelfReport),
            succession: Some(Succession::Satisfied),
            started_at_unix_ms: None,
            ended_at_unix_ms: None,
            restored_from: None,
        })
        .await
        .expect("epilogue status update");

    let after_finish = store
        .get_run(&run)
        .await
        .expect("get_run")
        .expect("run exists");
    assert_eq!(
        after_finish.nodes_done, 0,
        "the epilogue reaching a terminal status must not move nodes_done (§4 D5)"
    );
}

#[tokio::test]
async fn write_epilogue_node_created_rejects_a_non_reserved_path() {
    let store = open_mem_store().await;
    let (_, _, run) = setup_run(&store).await;

    let create = RunNodeCreate {
        run: run.clone(),
        key: NodeKey::new("solo"),
        path: InstancePath::new("solo"),
        label: String::new(),
        inputs: BTreeMap::new(),
        parent: None,
        depth: 0,
        status: NodeStatus::Ready,
        demand: Demand::Light,
        assignment: crate::workflow::tier::Assignment {
            model: crate::workflow::tier::ModelAlias::Sonnet,
            effort: crate::workflow::tier::Effort::Xhigh,
        },
        assignment_reason: String::new(),
        attempt: 1,
        proposal_id: String::new(),
    };

    let error = store
        .write_epilogue_node_created(create)
        .await
        .expect_err("a non-reserved path must never write a NULL kvdag_node");
    assert!(
        matches!(error, StoreError::Invariant(_)),
        "got {error:?}, want StoreError::Invariant — the loosened column must stay unreachable \
         for ordinary node writes"
    );
}

// ── Phase 3, WS-B: RunSummary (D16 field-for-field) ─────────────────────

#[tokio::test]
async fn a_run_summary_round_trips_every_field_through_the_production_read_path() {
    let store = open_mem_store().await;
    let (workflow, kvdag, run) = setup_run(&store).await;

    store
        .write(StoreWrite::RunSummary {
            run: run.clone(),
            kvdag_version: kvdag.version_id.clone(),
            text: "the run succeeded".to_string(),
            outcome: "succeeded".to_string(),
            highlights: vec!["did the thing".to_string()],
            open_gaps: vec!["nothing left".to_string()],
            per_node: vec![SummaryNodeLine {
                node_key: "solo".to_string(),
                verdict: "good".to_string(),
                one_liner: "solved it".to_string(),
            }],
            token_estimate: 42,
            generated_by_path: Some(InstancePath::new("solo")),
        })
        .await
        .expect("write_run_summary");

    let record = store
        .get_run_summary(&run)
        .await
        .expect("get_run_summary")
        .expect("the summary exists");
    assert_eq!(record.run, run);
    assert_eq!(record.workflow, workflow);
    assert_eq!(record.workflow_name, "demo");
    assert_eq!(record.version, kvdag.version_id);
    assert_eq!(record.text, "the run succeeded");
    assert_eq!(record.outcome, "succeeded");
    assert_eq!(record.highlights, vec!["did the thing".to_string()]);
    assert_eq!(record.open_gaps, vec!["nothing left".to_string()]);
    assert_eq!(record.per_node.len(), 1);
    assert_eq!(record.per_node[0].node_key, "solo");
    assert_eq!(record.per_node[0].verdict, "good");
    assert_eq!(record.per_node[0].one_liner, "solved it");
    assert_eq!(record.token_estimate, 42);
    assert_eq!(record.generated_by_path, Some(InstancePath::new("solo")));
    assert!(!record.run_pruned);
}

#[tokio::test]
async fn a_second_run_summary_write_for_the_same_run_is_rejected_not_panicked() {
    let store = open_mem_store().await;
    let (_, kvdag, run) = setup_run(&store).await;
    let write = || StoreWrite::RunSummary {
        run: run.clone(),
        kvdag_version: kvdag.version_id.clone(),
        text: "t".to_string(),
        outcome: "o".to_string(),
        highlights: Vec::new(),
        open_gaps: Vec::new(),
        per_node: Vec::new(),
        token_estimate: 0,
        generated_by_path: None,
    };
    store
        .write(write())
        .await
        .expect("the first write succeeds");
    let second = store.write(write()).await;
    assert!(
        second.is_err(),
        "run_summary_run is UNIQUE; a second write for the same run must error, not panic"
    );
}

#[tokio::test]
async fn list_run_summaries_orders_newest_first_respects_limit_and_flags_pruned() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;

    let mut runs = Vec::new();
    for index in 0..3 {
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
        let run = create_run(&store, &workflow, &kvdag).await;
        store
            .write(StoreWrite::RunSummary {
                run: run.clone(),
                kvdag_version: kvdag.version_id.clone(),
                text: format!("run {index}"),
                outcome: "succeeded".to_string(),
                highlights: Vec::new(),
                open_gaps: Vec::new(),
                per_node: Vec::new(),
                token_estimate: 0,
                generated_by_path: None,
            })
            .await
            .expect("write_run_summary");
        runs.push(run);
    }

    // Prune the oldest run outright; its summary must survive (M9 — the
    // dangling `run_summary.run` route would silently drop exactly this row).
    let pruned_id = parse_record_id(TABLE_WORKFLOW_RUN, runs[0].as_str()).expect("pruned run id");
    store
        .prune_one_run(&pruned_id)
        .await
        .expect("prune_one_run");

    let summaries = store
        .list_run_summaries(Some(&workflow), 10)
        .await
        .expect("list_run_summaries");
    assert_eq!(
        summaries.len(),
        3,
        "kvdag_version.workflow finds a pruned run's summary too"
    );
    assert_eq!(summaries[0].text, "run 2", "newest first");
    assert!(
        summaries
            .iter()
            .any(|record| record.run == runs[0] && record.run_pruned),
        "the pruned run's summary is flagged"
    );
    assert!(summaries
        .iter()
        .filter(|record| record.run != runs[0])
        .all(|record| !record.run_pruned));

    let limited = store
        .list_run_summaries(Some(&workflow), 2)
        .await
        .expect("list_run_summaries limited");
    assert_eq!(limited.len(), 2);
}

// ── Phase 3, WS-B: interrogation (D16 field-for-field) ──────────────────

#[tokio::test]
async fn an_interrogation_round_trips_every_field_through_the_production_read_path() {
    let store = open_mem_store().await;
    let (_, _, run) = setup_run(&store).await;

    let id = InterrogationId::new("interrogation:test-1");
    store
        .write(StoreWrite::InterrogationStarted {
            id: id.clone(),
            run: run.clone(),
            path: InstancePath::new("solo"),
            source_session_id: "sess-source".to_string(),
            forked_session_id: None,
            transcript_path: Some("/tmp/sess.jsonl".to_string()),
            cwd: "/tmp/work".to_string(),
            pane_id: crate::workflow::model::PublicPaneId::new("pane-1"),
            reconstructed: false,
            seeded_from_seq: None,
            note: "checking on it".to_string(),
            started_at_unix_ms: 1_700_000_000_000,
        })
        .await
        .expect("write_interrogation_started");

    let started = store
        .list_interrogations(&run)
        .await
        .expect("list_interrogations");
    assert_eq!(started.len(), 1);
    let record = &started[0];
    assert_eq!(record.id, id);
    assert_eq!(record.path, InstancePath::new("solo"));
    assert_eq!(record.source_session_id, "sess-source");
    assert_eq!(
        record.forked_session_id, None,
        "not pre-assigned: None until the fork's id is learned (§4 D7)"
    );
    assert_eq!(record.transcript_path, Some("/tmp/sess.jsonl".to_string()));
    assert_eq!(record.cwd, "/tmp/work");
    assert_eq!(record.pane_id.as_deref(), Some("pane-1"));
    assert!(!record.reconstructed);
    assert_eq!(record.note, "checking on it");
    assert_eq!(record.started_at_unix_ms, 1_700_000_000_000);
    assert_eq!(record.ended_at_unix_ms, None);

    store
        .write(StoreWrite::InterrogationUpdate {
            id: id.clone(),
            forked_session_id: Some("sess-fork-1".to_string()),
            ended_at_unix_ms: Some(1_700_000_060_000),
        })
        .await
        .expect("write_interrogation_update");

    let updated = store
        .list_interrogations(&run)
        .await
        .expect("list_interrogations");
    let record = &updated[0];
    assert_eq!(
        record.forked_session_id,
        Some("sess-fork-1".to_string()),
        "learned later from the pane's session report"
    );
    assert_eq!(record.ended_at_unix_ms, Some(1_700_000_060_000));
    assert_eq!(
        record.started_at_unix_ms, 1_700_000_000_000,
        "an update never rewrites the start stamp"
    );
}

#[tokio::test]
async fn prune_run_history_leaves_no_dangling_references_for_rows_from_the_new_writers() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;

    let pruned_run = create_run(&store, &workflow, &kvdag).await;
    store
        .write(StoreWrite::RunSummary {
            run: pruned_run.clone(),
            kvdag_version: kvdag.version_id.clone(),
            text: "t".to_string(),
            outcome: "o".to_string(),
            highlights: Vec::new(),
            open_gaps: Vec::new(),
            per_node: Vec::new(),
            token_estimate: 0,
            generated_by_path: Some(InstancePath::new("solo")),
        })
        .await
        .expect("write_run_summary");

    let interrogation_id = InterrogationId::new("interrogation:prune-test");
    store
        .write(StoreWrite::InterrogationStarted {
            id: interrogation_id.clone(),
            run: pruned_run.clone(),
            path: InstancePath::new("solo"),
            source_session_id: "sess-source".to_string(),
            forked_session_id: Some("sess-fork".to_string()),
            transcript_path: None,
            cwd: "/tmp".to_string(),
            pane_id: crate::workflow::model::PublicPaneId::new("pane-1"),
            reconstructed: false,
            seeded_from_seq: None,
            note: String::new(),
            started_at_unix_ms: 1_700_000_000_000,
        })
        .await
        .expect("write_interrogation_started");

    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    let kept_run = create_run(&store, &workflow, &kvdag).await;
    store
        .write(StoreWrite::RunSummary {
            run: kept_run.clone(),
            kvdag_version: kvdag.version_id.clone(),
            text: "kept".to_string(),
            outcome: "o".to_string(),
            highlights: Vec::new(),
            open_gaps: Vec::new(),
            per_node: Vec::new(),
            token_estimate: 0,
            generated_by_path: None,
        })
        .await
        .expect("write_run_summary for the kept run");

    let pruned = store
        .prune_run_history(&workflow, 1)
        .await
        .expect("prune_run_history");
    assert_eq!(pruned, 1);

    let summary = store
        .get_run_summary(&pruned_run)
        .await
        .expect("get_run_summary")
        .expect("the summary survives");
    assert!(summary.run_pruned);
    assert_eq!(
        summary.generated_by_path, None,
        "generated_by is nulled on prune, not left dangling"
    );

    let mut response = store
        .db
        .query("SELECT * FROM interrogation WHERE id = $id")
        .bind((
            "id",
            parse_record_id(TABLE_INTERROGATION, interrogation_id.as_str())
                .expect("interrogation id"),
        ))
        .await
        .expect("select interrogation");
    let rows: Vec<IdOnly> = response.take(0).expect("decode");
    assert!(
        rows.is_empty(),
        "prune deletes interrogations for the pruned run's nodes"
    );
}

// ── Phase 3, WS-B: RunEvent.at_unix_ms (§4 D14) ─────────────────────────

#[tokio::test]
async fn a_run_event_stamps_the_producers_at_unix_ms_not_a_store_flush_clock() {
    let store = open_mem_store().await;
    let (_, _, run) = setup_run(&store).await;
    let run_id = parse_record_id(TABLE_WORKFLOW_RUN, run.as_str()).expect("run id");

    let stamp = 1_700_000_555_123u64;
    store
        .write(StoreWrite::RunEvent {
            run: run.clone(),
            seq: 1,
            kind: RunEventKind::RunStarted,
            path: None,
            payload: serde_json::json!({}),
            at_unix_ms: stamp,
        })
        .await
        .expect("write_run_event");

    let mut response = store
        .db
        .query("SELECT * FROM run_event WHERE run = $run LIMIT 1")
        .bind(("run", run_id))
        .await
        .expect("select run_event");
    let rows: Vec<records::RunEventRow> = response.take(0).expect("decode run_event");
    let row = rows.into_iter().next().expect("the event row exists");
    assert_eq!(
        row.at.timestamp_millis() as u64,
        stamp,
        "run_event.at is the producer's own stamp, not a store-flush time::now() (§4 D14)"
    );
}

#[tokio::test]
async fn run_event_kind_summary_round_trips_through_list_run_events() {
    let store = open_mem_store().await;
    let (_, _, run) = setup_run(&store).await;

    store
        .write(StoreWrite::RunEvent {
            run: run.clone(),
            seq: 1,
            kind: RunEventKind::Summary,
            path: None,
            payload: serde_json::json!({"reason": "summary_failed"}),
            at_unix_ms: 1_700_000_000_000,
        })
        .await
        .expect("write_run_event");

    let events = store.list_run_events(&run).await.expect("list_run_events");
    assert_eq!(
        events.first().expect("the event exists").kind,
        RunEventKind::Summary
    );
}

// ── Phase 3, WS-B: orphan sweep (§4 D13) ────────────────────────────────

#[tokio::test]
async fn mark_interrupted_runs_sweeps_non_terminal_runs_and_their_nodes_once() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;
    let run = create_run(&store, &workflow, &kvdag).await;

    // `create_run` leaves the row "pending" and the root node "ready" — both
    // non-terminal, exactly what a server restart finds mid-run.
    let swept = store
        .mark_interrupted_runs(1_700_000_900_000)
        .await
        .expect("mark_interrupted_runs");
    assert_eq!(swept, 1);

    let reloaded = store
        .get_run(&run)
        .await
        .expect("get_run")
        .expect("run exists");
    assert_eq!(reloaded.status, RunStatus::Failed);
    assert_eq!(
        reloaded
            .failure
            .as_ref()
            .and_then(|failure| failure.get("reason"))
            .and_then(serde_json::Value::as_str),
        Some("interrupted")
    );
    assert_eq!(reloaded.ended_at_unix_ms, Some(1_700_000_900_000));

    let nodes = store.list_run_nodes(&run).await.expect("list_run_nodes");
    let solo = nodes
        .iter()
        .find(|node| node.instance_path.as_str() == "solo")
        .expect("the node exists");
    assert_eq!(solo.status, NodeStatus::Cancelled);
    assert_eq!(
        solo.evidence, None,
        "the node didn't fail, the server left — evidence stays untouched"
    );

    let second = store
        .mark_interrupted_runs(1_700_000_999_000)
        .await
        .expect("a second sweep is a no-op");
    assert_eq!(second, 0);
    let reloaded_again = store
        .get_run(&run)
        .await
        .expect("get_run")
        .expect("run exists");
    assert_eq!(
        reloaded_again.ended_at_unix_ms,
        Some(1_700_000_900_000),
        "a terminal run's ended_at is not touched again"
    );
}

#[tokio::test]
async fn mark_interrupted_runs_leaves_terminal_runs_untouched() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;
    let run = create_run(&store, &workflow, &kvdag).await;
    store
        .write(StoreWrite::RunStatus {
            run: run.clone(),
            status: RunStatus::Succeeded,
            ended_at_unix_ms: Some(1_700_000_500_000),
        })
        .await
        .expect("write_run_status");

    let swept = store
        .mark_interrupted_runs(1_700_000_900_000)
        .await
        .expect("mark_interrupted_runs");
    assert_eq!(swept, 0);

    let reloaded = store
        .get_run(&run)
        .await
        .expect("get_run")
        .expect("run exists");
    assert_eq!(reloaded.status, RunStatus::Succeeded);
    assert_eq!(reloaded.ended_at_unix_ms, Some(1_700_000_500_000));
}

// ── Phase 3, WS-B: restore (§1 WS-B, §4 D4, D11) ────────────────────────

#[tokio::test]
async fn a_restored_node_persists_its_full_terminal_shape_and_reseeds_its_checkpoint() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;
    let source_run = create_run(&store, &workflow, &kvdag).await;

    store
        .write(StoreWrite::Checkpoint {
            run: source_run.clone(),
            path: InstancePath::new("solo"),
            seq: 1,
            kind: CheckpointKind::Result,
            schema_valid: true,
            payload: serde_json::json!({"report": "done"}),
            summary: "solved it".to_string(),
            artifact_paths: vec!["artifacts/out.md".to_string()],
            digest: "abc123".to_string(),
        })
        .await
        .expect("write the source run's checkpoint");

    let seed = RestoredSeed {
        node_key: NodeKey::new("solo"),
        payload: serde_json::json!({"report": "done"}),
        summary: "solved it".to_string(),
        artifact_paths: vec!["artifacts/out.md".to_string()],
        digest: "abc123".to_string(),
        source: RestoredRef {
            run: source_run.clone(),
            node_key: NodeKey::new("solo"),
            checkpoint_seq: 1,
        },
    };

    let restore_stamp = next_run_start_unix_ms();
    let restored_run = store
        .create_run(NewRun {
            started_at_unix_ms: restore_stamp,
            restore_from: Some(RestoreFromRequest {
                run: source_run.clone(),
                nodes: vec!["solo".to_string()],
                allow_changed: false,
            }),
            restored: vec![seed],
            ..new_run(&workflow, &kvdag)
        })
        .await
        .expect("create_run with a restored seed");

    let nodes = store
        .list_run_nodes(&restored_run)
        .await
        .expect("list_run_nodes");
    let solo = nodes
        .iter()
        .find(|record| record.instance_path.as_str() == "solo")
        .expect("the restored node exists");
    assert_eq!(solo.status, NodeStatus::Restored);
    assert_eq!(solo.evidence, Some(Evidence::Restored));
    assert_eq!(solo.succession, Some(Succession::Satisfied));
    assert_eq!(solo.started_at_unix_ms, Some(restore_stamp));
    assert_eq!(
        solo.ended_at_unix_ms,
        Some(restore_stamp),
        "the restore instant, not the source run's (§4 D4)"
    );
    assert_eq!(
        solo.restored_from,
        Some(RestoredRef {
            run: source_run.clone(),
            node_key: NodeKey::new("solo"),
            checkpoint_seq: 1,
        })
    );

    let checkpoints = store
        .list_checkpoints(&restored_run, &InstancePath::new("solo"))
        .await
        .expect("list_checkpoints");
    let checkpoint = checkpoints
        .first()
        .expect("the re-persisted checkpoint exists");
    assert_eq!(checkpoint.seq, 1);
    assert_eq!(checkpoint.kind, CheckpointKind::Result);
    assert!(
        checkpoint.schema_valid,
        "a digest-equal restore validates against the target version's schema"
    );
    assert_eq!(checkpoint.payload, serde_json::json!({"report": "done"}));
    assert_eq!(checkpoint.summary, "solved it");
    assert_eq!(checkpoint.digest, "abc123");

    let reloaded = store
        .get_run(&restored_run)
        .await
        .expect("get_run")
        .expect("run exists");
    assert_eq!(reloaded.restore_from_run, Some(source_run.clone()));

    // The full request is persisted, not just the source id: a selector that
    // was asked for and skipped has no other durable trace once the
    // transient API response is gone (judgment call approved by
    // phase3-planner-f).
    let restored_run_id =
        parse_record_id(TABLE_WORKFLOW_RUN, restored_run.as_str()).expect("run id");
    let mut response = store
        .db
        .query("SELECT * FROM $id")
        .bind(("id", restored_run_id))
        .await
        .expect("select workflow_run");
    let rows: Vec<records::RunRow> = response.take(0).expect("decode workflow_run");
    let restore_from = rows
        .into_iter()
        .next()
        .expect("the run row exists")
        .restore_from
        .expect("restore_from is set");
    assert_eq!(
        restore_from.get("run").and_then(|v| v.as_str()),
        Some(source_run.as_str())
    );
    assert_eq!(
        restore_from.get("nodes"),
        Some(&serde_json::json!(["solo"]))
    );
    assert_eq!(
        restore_from.get("allow_changed"),
        Some(&serde_json::json!(false))
    );
}

#[tokio::test]
async fn a_restored_node_across_a_changed_definition_reseeds_with_schema_valid_false() {
    let store = open_mem_store().await;
    let workflow = store
        .create_workflow("cross-version", "", Tier::Auto)
        .await
        .expect("create_workflow");
    let v1 = store
        .create_version(&workflow, VersionOrigin::Authored, "v1", single_node_spec())
        .await
        .expect("create_version v1");
    store
        .set_head_version(&workflow, &v1.version_id)
        .await
        .expect("set_head_version");
    let source_run = create_run(&store, &workflow, &v1).await;

    store
        .write(StoreWrite::Checkpoint {
            run: source_run.clone(),
            path: InstancePath::new("solo"),
            seq: 1,
            kind: CheckpointKind::Result,
            schema_valid: true,
            payload: serde_json::json!({"report": "done"}),
            summary: "solved it".to_string(),
            artifact_paths: Vec::new(),
            digest: "abc123".to_string(),
        })
        .await
        .expect("write the source run's checkpoint");

    let mut changed = single_node_spec();
    changed.nodes[0].prompt_template = "Solve {{goal}} differently".to_string();
    let v2 = store
        .create_version(&workflow, VersionOrigin::Authored, "v2", changed)
        .await
        .expect("create_version v2");
    store
        .set_head_version(&workflow, &v2.version_id)
        .await
        .expect("set_head_version");

    let seed = RestoredSeed {
        node_key: NodeKey::new("solo"),
        payload: serde_json::json!({"report": "done"}),
        summary: "solved it".to_string(),
        artifact_paths: Vec::new(),
        digest: "abc123".to_string(),
        source: RestoredRef {
            run: source_run.clone(),
            node_key: NodeKey::new("solo"),
            checkpoint_seq: 1,
        },
    };

    let restored_run = store
        .create_run(NewRun {
            restore_from: Some(RestoreFromRequest {
                run: source_run.clone(),
                nodes: vec!["solo".to_string()],
                allow_changed: true,
            }),
            restored: vec![seed],
            ..new_run(&workflow, &v2)
        })
        .await
        .expect("create_run restoring across versions (allow_changed)");

    let checkpoints = store
        .list_checkpoints(&restored_run, &InstancePath::new("solo"))
        .await
        .expect("list_checkpoints");
    let checkpoint = checkpoints.first().expect("the checkpoint exists");
    assert!(
        !checkpoint.schema_valid,
        "an allow_changed cross-version restore was never validated against the target \
         schema, and must not validate onward restore of unvalidated data"
    );
}

#[tokio::test]
async fn node_compat_digests_for_differs_when_the_prompt_template_changes() {
    let store = open_mem_store().await;
    let workflow = store
        .create_workflow("digests", "", Tier::Auto)
        .await
        .expect("create_workflow");
    let v1 = store
        .create_version(&workflow, VersionOrigin::Authored, "v1", single_node_spec())
        .await
        .expect("create_version v1");

    let mut changed = single_node_spec();
    changed.nodes[0].prompt_template = "a different prompt".to_string();
    let v2 = store
        .create_version(&workflow, VersionOrigin::Authored, "v2", changed)
        .await
        .expect("create_version v2");

    let d1 = store
        .node_compat_digests_for(&v1.version_id, &NodeKey::new("solo"))
        .await
        .expect("digests v1")
        .expect("the node exists");
    let d2 = store
        .node_compat_digests_for(&v2.version_id, &NodeKey::new("solo"))
        .await
        .expect("digests v2")
        .expect("the node exists");
    assert_ne!(
        d1.0, d2.0,
        "the prompt digest changes with the prompt template"
    );
    assert_eq!(
        d1.1, d2.1,
        "the schema digest is unaffected by a prompt-only change"
    );

    let unknown = store
        .node_compat_digests_for(&v1.version_id, &NodeKey::new("ghost"))
        .await
        .expect("an unknown key is a plain comparison input, not an error");
    assert!(unknown.is_none());
}

#[tokio::test]
async fn restore_source_returns_only_valid_result_checkpoints_with_source_digests() {
    let store = open_mem_store().await;
    let (_, kvdag, run) = setup_run(&store).await;

    store
        .write(StoreWrite::Checkpoint {
            run: run.clone(),
            path: InstancePath::new("solo"),
            seq: 1,
            kind: CheckpointKind::Result,
            schema_valid: true,
            payload: serde_json::json!({"report": "v1"}),
            summary: "s1".to_string(),
            artifact_paths: Vec::new(),
            digest: "d1".to_string(),
        })
        .await
        .expect("write checkpoint 1");
    store
        .write(StoreWrite::Checkpoint {
            run: run.clone(),
            path: InstancePath::new("solo"),
            seq: 2,
            kind: CheckpointKind::Result,
            schema_valid: false,
            payload: serde_json::json!({"report": "v2-invalid"}),
            summary: "s2".to_string(),
            artifact_paths: Vec::new(),
            digest: "d2".to_string(),
        })
        .await
        .expect("write checkpoint 2 (schema-invalid)");
    store
        .write(StoreWrite::Checkpoint {
            run: run.clone(),
            path: InstancePath::new("solo"),
            seq: 3,
            kind: CheckpointKind::Partial,
            schema_valid: true,
            payload: serde_json::json!({"progress": "half"}),
            summary: "s3".to_string(),
            artifact_paths: Vec::new(),
            digest: "d3".to_string(),
        })
        .await
        .expect("write checkpoint 3 (partial)");

    let restorable = store
        .restore_source(&run, &[NodeKey::new("solo")])
        .await
        .expect("restore_source");
    assert_eq!(
        restorable.len(),
        1,
        "only the schema-valid result checkpoint is restorable"
    );
    assert_eq!(restorable[0].checkpoint.seq, 1);
    assert_eq!(restorable[0].checkpoint.digest, "d1");
    let (expected_prompt, expected_schema) = store
        .node_compat_digests_for(&kvdag.version_id, &NodeKey::new("solo"))
        .await
        .expect("digests")
        .expect("the node exists");
    assert_eq!(restorable[0].prompt_digest, expected_prompt);
    assert_eq!(restorable[0].schema_digest, expected_schema);
}

// ── Phase 3, WS-B: cross-workflow run listing (§4 D9) ───────────────────

#[tokio::test]
async fn list_runs_with_no_workflow_filter_lists_across_every_workflow() {
    let store = open_mem_store().await;
    let (workflow_a, kvdag_a) = setup_workflow(&store).await;
    let run_a = create_run(&store, &workflow_a, &kvdag_a).await;

    let workflow_b = store
        .create_workflow("second", "", Tier::Auto)
        .await
        .expect("create_workflow");
    let kvdag_b = store
        .create_version(
            &workflow_b,
            VersionOrigin::Authored,
            "v1",
            single_node_spec(),
        )
        .await
        .expect("create_version");
    store
        .set_head_version(&workflow_b, &kvdag_b.version_id)
        .await
        .expect("set_head_version");
    let run_b = create_run(&store, &workflow_b, &kvdag_b).await;

    let all = store
        .list_runs(None, 10)
        .await
        .expect("list_runs across every workflow");
    assert!(all
        .iter()
        .any(|record| record.id == run_a && record.workflow_name == "demo"));
    assert!(all
        .iter()
        .any(|record| record.id == run_b && record.workflow_name == "second"));
}

// ── 14: create atomicity and name collisions ─────────────────────────────

/// A name collision came back as `StoreError::Query` carrying SurrealDB's own
/// index message, which `workflow.create` then handed to the user verbatim.
#[tokio::test]
async fn creating_a_workflow_under_a_taken_name_is_a_named_store_error() {
    let store = open_mem_store().await;
    store
        .create_workflow("demo", "", Tier::Auto)
        .await
        .expect("create_workflow");

    let error = store
        .create_workflow("demo", "", Tier::Auto)
        .await
        .expect_err("the UNIQUE workflow_name index refuses the second row");
    assert!(
        matches!(&error, StoreError::NameTaken { name } if name == "demo"),
        "unexpected error: {error:?}"
    );
    assert_eq!(error.api_code(), error::WORKFLOW_NAME_TAKEN_CODE);
    assert!(
        !error.to_string().to_ascii_lowercase().contains("index"),
        "the raw index message leaked: {error}"
    );
}

/// The sniffer that recognises the collision. Pinned against the message shape
/// actually observed, so an upstream rewording fails here rather than silently
/// reverting the leak.
#[test]
fn the_unique_name_index_message_is_recognised_and_nothing_else_is() {
    assert!(is_workflow_name_conflict(
        "Database index `workflow_name` already contains 'test-cycle', \
         with record `workflow:abc`"
    ));
    assert!(is_workflow_name_conflict(
        "Database index workflow_name already contains 'x', with record workflow:abc"
    ));
    assert!(!is_workflow_name_conflict(
        "Database index `run_workflow` already contains 'x'"
    ));
    assert!(!is_workflow_name_conflict(
        "There was a problem with the database"
    ));
}

/// A version the graph validators reject must leave the workflow row exactly as
/// it was: the H5 metadata refresh used to run *before* `Kvdag::try_new`, so a
/// rejected update still rewrote the row's description and tier to describe a
/// revision that was never written.
#[tokio::test]
async fn a_rejected_version_leaves_the_workflow_metadata_untouched() {
    let store = open_mem_store().await;
    let (workflow, _) = setup_workflow(&store).await;
    let before = store
        .get_workflow(&workflow)
        .await
        .expect("get_workflow")
        .expect("the workflow exists");

    // Two nodes with the same key: rejected by `Kvdag::try_new`, never by any
    // check that runs earlier.
    let duplicate = base_spec(
        vec![
            node("same", "Work on {{goal}}"),
            node("same", "Work on {{goal}}"),
        ],
        Vec::new(),
    );
    let error = store
        .create_version_with_metadata(
            &workflow,
            VersionOrigin::Authored,
            "rejected",
            duplicate,
            Some(&VersionMetadata {
                description: "should never be written".to_string(),
                default_tier: Tier::Low,
            }),
        )
        .await
        .expect_err("a duplicate node key is refused");
    assert!(
        matches!(
            error,
            StoreError::InvalidGraph(KvdagError::DuplicateNodeKey(_))
        ),
        "unexpected error: {error:?}"
    );

    let after = store
        .get_workflow(&workflow)
        .await
        .expect("get_workflow")
        .expect("the workflow still exists");
    assert_eq!(after.description, before.description);
    assert_eq!(after.default_tier, before.default_tier);
}

// ── the agent-teams rework: lead binding, task projection, members ──────

/// The `run_member` shape this file needs to look at the raw row, where
/// `RunMemberRecord` only exposes millisecond stamps.
#[derive(Debug, Clone, SurrealValue)]
struct MemberStamps {
    first_seen_at: surrealdb_types::Datetime,
    last_seen_at: surrealdb_types::Datetime,
}

fn member_snapshot(run: &RunId, name: &str, model: &str, observed_at_unix_ms: u64) -> StoreWrite {
    StoreWrite::RunMemberSnapshot {
        run: run.clone(),
        name: name.to_string(),
        agent_type: "Explore".to_string(),
        model: model.to_string(),
        pane_id: Some("w3:p1P".to_string()),
        backend_type: "tmux".to_string(),
        is_active: true,
        cwd: Some("/home/karan/code/karvex".to_string()),
        observed_at_unix_ms,
    }
}

fn task_projection(
    run: &RunId,
    path: &str,
    node_key: &str,
    task_id: &str,
    subject: &str,
    owner: &str,
    status: NodeStatus,
    emergent: bool,
    observed_at_unix_ms: u64,
) -> StoreWrite {
    StoreWrite::RunTaskProjected {
        run: run.clone(),
        path: InstancePath::new(path),
        node_key: NodeKey::new(node_key),
        task_id: task_id.to_string(),
        subject: subject.to_string(),
        owner: owner.to_string(),
        status,
        emergent,
        blocked_by: Vec::new(),
        observed_at_unix_ms,
    }
}

#[tokio::test]
async fn a_lead_binding_round_trips_through_get_run() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;
    let run = create_run(&store, &workflow, &kvdag).await;

    let before = store
        .get_run(&run)
        .await
        .expect("get_run")
        .expect("run exists");
    assert_eq!(
        before.lead_session_id, None,
        "a run has no lead until its pane's claude registers a session"
    );

    store
        .write(StoreWrite::RunLeadBinding {
            run: run.clone(),
            lead_session_id: "213aa9bf-2652-45ca-ac73-1cf00b493ef3".to_string(),
            team_name: "session-213aa9bf".to_string(),
            lead_pane_id: Some("w1:p2A".to_string()),
            lead_terminal_id: Some("t7".to_string()),
            lead_prompt_version: 1,
        })
        .await
        .expect("write lead binding");

    let bound = store
        .get_run(&run)
        .await
        .expect("get_run")
        .expect("run exists");
    assert_eq!(
        bound.lead_session_id.as_deref(),
        Some("213aa9bf-2652-45ca-ac73-1cf00b493ef3")
    );
    assert_eq!(bound.team_name.as_deref(), Some("session-213aa9bf"));
    assert_eq!(bound.lead_pane_id.as_deref(), Some("w1:p2A"));
    assert_eq!(bound.lead_terminal_id.as_deref(), Some("t7"));
    assert_eq!(bound.lead_prompt_version, Some(1));

    // Re-learning the same binding after a restart is an update, never a
    // second row.
    store
        .write(StoreWrite::RunLeadBinding {
            run: run.clone(),
            lead_session_id: "213aa9bf-2652-45ca-ac73-1cf00b493ef3".to_string(),
            team_name: "session-213aa9bf".to_string(),
            lead_pane_id: Some("w1:p9Z".to_string()),
            lead_terminal_id: None,
            lead_prompt_version: 2,
        })
        .await
        .expect("re-write lead binding");
    let rebound = store
        .get_run(&run)
        .await
        .expect("get_run")
        .expect("run exists");
    assert_eq!(rebound.lead_pane_id.as_deref(), Some("w1:p9Z"));
    assert_eq!(rebound.lead_terminal_id, None);
    assert_eq!(rebound.lead_prompt_version, Some(2));
}

#[tokio::test]
async fn a_planned_task_projection_updates_the_run_node_row_it_already_has() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;
    let run = create_run(&store, &workflow, &kvdag).await;

    store
        .write(task_projection(
            &run,
            "solo",
            "solo",
            "1",
            "1. Solve the thing",
            "solo-worker",
            NodeStatus::Running,
            false,
            1_700_000_000_000,
        ))
        .await
        .expect("project a planned task");

    let nodes = store.list_run_nodes(&run).await.expect("list_run_nodes");
    assert_eq!(
        nodes.len(),
        1,
        "a planned task must not create a second row"
    );
    let node = &nodes[0];
    assert_eq!(node.task_id.as_deref(), Some("1"));
    assert_eq!(node.subject, "1. Solve the thing");
    assert_eq!(node.owner, "solo-worker");
    assert_eq!(node.status, NodeStatus::Running);
    assert!(!node.emergent);
    // `label` is the authored name and stays the authored name even though the
    // lead reworded the task.
    assert_eq!(node.label, "solo");
    assert_eq!(node.started_at_unix_ms, Some(1_700_000_000_000));
    assert_eq!(node.ended_at_unix_ms, None);

    store
        .write(task_projection(
            &run,
            "solo",
            "solo",
            "1",
            "1. Solve the thing, revised",
            "someone-else",
            NodeStatus::Succeeded,
            false,
            1_700_000_005_000,
        ))
        .await
        .expect("re-project the same task");

    let nodes = store.list_run_nodes(&run).await.expect("list_run_nodes");
    assert_eq!(nodes.len(), 1, "re-observing a task is an update");
    let node = &nodes[0];
    assert_eq!(node.subject, "1. Solve the thing, revised");
    assert_eq!(node.owner, "someone-else");
    assert_eq!(node.status, NodeStatus::Succeeded);
    assert_eq!(
        node.started_at_unix_ms,
        Some(1_700_000_000_000),
        "the first non-pending sighting is never re-stamped"
    );
    assert_eq!(node.ended_at_unix_ms, Some(1_700_000_005_000));

    let reloaded = store
        .get_run(&run)
        .await
        .expect("get_run")
        .expect("run exists");
    assert_eq!(reloaded.nodes_done, 1, "a closed task closes its node");
}

#[tokio::test]
async fn an_emergent_task_projection_creates_one_reserved_path_row_however_often_it_is_seen() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;
    let run = create_run(&store, &workflow, &kvdag).await;

    for (owner, status, observed) in [
        ("", NodeStatus::Pending, 1_700_000_000_000_u64),
        ("scout", NodeStatus::Running, 1_700_000_001_000),
        ("scout", NodeStatus::Succeeded, 1_700_000_002_000),
    ] {
        store
            .write(task_projection(
                &run,
                ".task/7",
                "task-7",
                "7",
                "7. Something the plan never mentioned",
                owner,
                status,
                true,
                observed,
            ))
            .await
            .expect("project an emergent task");
    }

    let nodes = store.list_run_nodes(&run).await.expect("list_run_nodes");
    let emergent: Vec<_> = nodes.iter().filter(|node| node.emergent).collect();
    assert_eq!(
        emergent.len(),
        1,
        "re-projecting the same emergent task is idempotent"
    );
    let node = emergent[0];
    assert_eq!(node.instance_path.as_str(), ".task/7");
    assert_eq!(node.task_id.as_deref(), Some("7"));
    assert_eq!(node.owner, "scout");
    assert_eq!(node.status, NodeStatus::Succeeded);
    assert_eq!(
        node.label, "7. Something the plan never mentioned",
        "an emergent node's only name is its subject"
    );
    assert_eq!(
        node.started_at_unix_ms,
        Some(1_700_000_001_000),
        "the pending first sighting does not start the node"
    );
    assert_eq!(node.ended_at_unix_ms, Some(1_700_000_002_000));

    // No kvdag node behind it — the loosened `0004` column, reached only
    // through the reserved namespace.
    let mut response = store
        .db
        .query("SELECT VALUE id FROM run_node WHERE run = $run AND kvdag_node = NONE")
        .bind((
            "run",
            parse_record_id(TABLE_WORKFLOW_RUN, run.as_str()).expect("run id"),
        ))
        .await
        .expect("select run_node");
    let without_kvdag: Vec<surrealdb_types::RecordId> =
        response.take(0).expect("decode run_node ids");
    assert_eq!(without_kvdag.len(), 1);

    // Reserved paths stay out of both run counters.
    let reloaded = store
        .get_run(&run)
        .await
        .expect("get_run")
        .expect("run exists");
    assert_eq!(reloaded.nodes_total, 1);
    assert_eq!(reloaded.nodes_done, 0);
}

#[tokio::test]
async fn an_emergent_task_outside_the_reserved_namespace_is_rejected() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;
    let run = create_run(&store, &workflow, &kvdag).await;

    let error = store
        .write(task_projection(
            &run,
            "not-in-the-definition",
            "not-in-the-definition",
            "9",
            "9. Off-plan",
            "scout",
            NodeStatus::Running,
            true,
            1_700_000_000_000,
        ))
        .await
        .expect_err("a create with no kvdag node outside the reserved namespace is refused");
    assert!(
        matches!(error, StoreError::Invariant(_)),
        "unexpected error: {error:?}"
    );

    let nodes = store.list_run_nodes(&run).await.expect("list_run_nodes");
    assert_eq!(nodes.len(), 1, "the refused create wrote nothing");
}

#[tokio::test]
async fn a_projected_blocked_by_becomes_one_run_edge_once_both_tasks_exist() {
    let store = open_mem_store().await;
    let workflow = store
        .create_workflow("fanout", "two children", Tier::Auto)
        .await
        .expect("create_workflow");
    let kvdag = store
        .create_version(&workflow, VersionOrigin::Authored, "v1", fanout_spec(1))
        .await
        .expect("create_version");
    let run = create_run(&store, &workflow, &kvdag).await;

    // `child0`'s blocker is projected before `root`'s own task is: the
    // projection has no ordering guarantee, so the edge is skipped rather than
    // erroring, and the next poll draws it.
    let blocked = |observed: u64| StoreWrite::RunTaskProjected {
        run: run.clone(),
        path: InstancePath::new("child0"),
        node_key: NodeKey::new("child0"),
        task_id: "2".to_string(),
        subject: "2. Child".to_string(),
        owner: String::new(),
        status: NodeStatus::Pending,
        emergent: false,
        blocked_by: vec![InstancePath::new(".task/1")],
        observed_at_unix_ms: observed,
    };

    store
        .write(blocked(1_700_000_000_000))
        .await
        .expect("write");
    let edges = store.list_run_edges(&run).await.expect("list_run_edges");
    let projected: Vec<_> = edges
        .iter()
        .filter(|edge| edge.from.as_str() == ".task/1")
        .collect();
    assert!(
        projected.is_empty(),
        "a blocker with no row yet is skipped, not an error"
    );

    store
        .write(task_projection(
            &run,
            ".task/1",
            "task-1",
            "1",
            "1. The blocker",
            "lead",
            NodeStatus::Running,
            true,
            1_700_000_001_000,
        ))
        .await
        .expect("project the blocker");

    // Two more polls; the edge appears once and stays once.
    store
        .write(blocked(1_700_000_002_000))
        .await
        .expect("write");
    store
        .write(blocked(1_700_000_003_000))
        .await
        .expect("write");

    let edges = store.list_run_edges(&run).await.expect("list_run_edges");
    let projected: Vec<_> = edges
        .iter()
        .filter(|edge| edge.from.as_str() == ".task/1" && edge.to.as_str() == "child0")
        .collect();
    assert_eq!(projected.len(), 1, "blockedBy edges do not accumulate");
    assert_eq!(projected[0].kind, EdgeKind::Sequence);
}

#[tokio::test]
async fn member_snapshots_upsert_in_place_and_list_in_first_seen_order() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;
    let run = create_run(&store, &workflow, &kvdag).await;

    store
        .write(member_snapshot(
            &run,
            "solo-worker",
            "sonnet",
            1_700_000_000_000,
        ))
        .await
        .expect("first sighting");

    let members = store
        .list_run_members(&run)
        .await
        .expect("list_run_members");
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].name, "solo-worker");
    assert_eq!(members[0].model, "sonnet");
    assert_eq!(
        members[0].first_seen_at_unix_ms, members[0].last_seen_at_unix_ms,
        "a member first seen is a member seen once"
    );
    assert_eq!(members[0].first_seen_at_unix_ms, 1_700_000_000_000);

    // A later observation with a changed model and pane rewrites in place.
    store
        .write(StoreWrite::RunMemberSnapshot {
            run: run.clone(),
            name: "solo-worker".to_string(),
            agent_type: "Explore".to_string(),
            model: "opus".to_string(),
            pane_id: Some("w3:p4Q".to_string()),
            backend_type: "tmux".to_string(),
            is_active: false,
            cwd: Some("/home/karan/code/karvex".to_string()),
            observed_at_unix_ms: 1_700_000_009_000,
        })
        .await
        .expect("second sighting");

    let members = store
        .list_run_members(&run)
        .await
        .expect("list_run_members");
    assert_eq!(members.len(), 1, "a re-observed member is one row");
    assert_eq!(members[0].model, "opus");
    assert_eq!(members[0].pane_id.as_deref(), Some("w3:p4Q"));
    assert!(!members[0].is_active);
    assert_eq!(
        members[0].first_seen_at_unix_ms, 1_700_000_000_000,
        "first_seen_at is never moved"
    );
    assert_eq!(members[0].last_seen_at_unix_ms, 1_700_000_009_000);

    // Ordering: `first_seen_at` first, then `name`. `zz` is seen before `aa`.
    store
        .write(member_snapshot(
            &run,
            "zz-later",
            "sonnet",
            1_700_000_010_000,
        ))
        .await
        .expect("third member");
    store
        .write(member_snapshot(
            &run,
            "aa-latest",
            "sonnet",
            1_700_000_011_000,
        ))
        .await
        .expect("fourth member");
    let members = store
        .list_run_members(&run)
        .await
        .expect("list_run_members");
    let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, vec!["solo-worker", "zz-later", "aa-latest"]);

    // The raw stamps, so this is not just a projection artefact.
    let mut response = store
        .db
        .query("SELECT first_seen_at, last_seen_at FROM run_member WHERE name = $name")
        .bind(("name", "solo-worker".to_string()))
        .await
        .expect("select run_member");
    let rows: Vec<MemberStamps> = response.take(0).expect("decode run_member");
    assert_eq!(rows.len(), 1);
    assert_ne!(rows[0].first_seen_at, rows[0].last_seen_at);

    // Pruning a run takes its members with it: no dangling snapshot.
    store
        .prune_run_history(&workflow, 0)
        .await
        .expect("prune_run_history");
    let members = store
        .list_run_members(&run)
        .await
        .expect("list_run_members");
    assert!(members.is_empty(), "a pruned run leaves no member rows");
}

// ── the agent-teams rework: migration 0005 ──────────────────────────────

#[tokio::test]
async fn migration_0005_applies_over_a_0001_to_0004_database() {
    let store = WorkflowStore::open_with_migrations(StoreLocation::Memory, 4)
        .await
        .expect("a 0001..0004 database opens");
    assert_eq!(
        store.applied_migrations().await.expect("read schema_meta"),
        std::collections::BTreeSet::from([
            "0001_init".to_string(),
            "0002_growth_and_history".to_string(),
            "0003_node_identity".to_string(),
            "0004_journal_time_and_interrogation".to_string(),
        ])
    );

    // A run and its node, written the way a pre-rework karvex wrote them: no
    // lead binding, no task columns. Written in SQL rather than through
    // `create_run`, which decodes `records::RunNodeRow` and therefore requires
    // the columns this migration is about to add.
    let (workflow, kvdag) = setup_workflow(&store).await;
    let run = create_run_row_directly(&store, &workflow, &kvdag).await;

    store
        .migrate()
        .await
        .expect("0005 applies on top of 0001..0004, including over existing rows");
    assert_eq!(
        store.applied_migrations().await.expect("read schema_meta"),
        std::collections::BTreeSet::from([
            "0001_init".to_string(),
            "0002_growth_and_history".to_string(),
            "0003_node_identity".to_string(),
            "0004_journal_time_and_interrogation".to_string(),
            "0005_lead_binding_and_projection".to_string(),
        ])
    );

    // The point of the test: a row written before 0005 still decodes through
    // the widened `RunRow`/`RunNodeRow`. The `option` lead columns read back
    // as `None`; the three non-option task columns read back as the values
    // the migration's backfill `UPDATE` put there, not as a decode error.
    let reloaded = store
        .get_run(&run)
        .await
        .expect("a pre-0005 workflow_run row still decodes")
        .expect("run exists");
    assert_eq!(reloaded.id, run);
    assert_eq!(reloaded.lead_session_id, None);
    assert_eq!(reloaded.team_name, None);
    assert_eq!(reloaded.lead_prompt_version, None);

    let nodes = store
        .list_run_nodes(&run)
        .await
        .expect("a pre-0005 run_node row still decodes");
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].task_id, None);
    assert_eq!(nodes[0].subject, "");
    assert_eq!(nodes[0].owner, "");
    assert!(!nodes[0].emergent);

    // And the new write paths work against the migrated database.
    store
        .write(StoreWrite::RunLeadBinding {
            run: run.clone(),
            lead_session_id: "s1".to_string(),
            team_name: "session-s1".to_string(),
            lead_pane_id: None,
            lead_terminal_id: None,
            lead_prompt_version: 1,
        })
        .await
        .expect("the lead binding is writable after 0005");
    store
        .write(member_snapshot(
            &run,
            "solo-worker",
            "opus",
            1_700_000_000_000,
        ))
        .await
        .expect("run_member is writable after 0005");
    assert_eq!(
        store
            .list_run_members(&run)
            .await
            .expect("list_run_members")
            .len(),
        1
    );
}
