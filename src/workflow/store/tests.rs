//! Store test plan (`docs/design/workflow-builder/03-storage-schema.md` §10).
//!
//! Cases 1-11 run against redb's in-memory backend (no disk, no PTY). Cases
//! 12-13 need a real on-disk file lock and are marked `#[ignore]` by default.

use std::collections::BTreeMap;

use redb::TableHandle as _;

use super::db;
use super::records;
use super::*;
use crate::workflow::model::{ArgSpec, InstancePath, KvdagError, Runner};

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
    store
        .create_run(NewRun {
            workflow: workflow.clone(),
            version: kvdag.version_id.clone(),
            tier: Tier::Auto,
            args: BTreeMap::new(),
            growth: GrowthLimits::default(),
            context_runs: Vec::new(),
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
    store
        .create_run_summary(run, version, "summary text", "ok")
        .await
        .expect("create run_summary");
}

/// Every `interrogation` row still in the database. Read straight out of redb
/// because there is no engine writer — or reader — for the table yet, so it has
/// no projection on the public read surface to go through.
fn all_interrogations(store: &WorkflowStore) -> Vec<records::InterrogationRow> {
    let read = store.read().expect("read txn");
    let table = read.open_table(db::INTERROGATION).expect("interrogation");
    db::scan_prefix(&table, "").expect("decode interrogations")
}

fn all_review_findings(store: &WorkflowStore) -> Vec<records::ReviewFindingRow> {
    let read = store.read().expect("read txn");
    let table = read.open_table(db::REVIEW_FINDING).expect("review_finding");
    db::scan_prefix(&table, "").expect("decode findings")
}

fn run_summary_generated_by(store: &WorkflowStore, run: &RunId) -> Option<String> {
    let key = records::parse_record_id(TABLE_WORKFLOW_RUN, run.as_str()).expect("run id");
    let read = store.read().expect("read txn");
    let table = read.open_table(db::RUN_SUMMARY).expect("run_summary");
    let row: records::RunSummaryRow = db::get_row(&table, &key)
        .expect("decode summary")
        .expect("the summary exists");
    row.generated_by
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

// ── 1: migrations ────────────────────────────────────────────────────────

#[tokio::test]
async fn migrations_apply_cleanly_and_reapplying_is_a_noop() {
    let store = open_mem_store().await;
    let first = store.applied_migrations().expect("read schema_meta");
    assert_eq!(
        first,
        std::collections::BTreeSet::from(["0001_init".to_string()])
    );

    store.migrate().await.expect("re-migrate is a no-op");
    let second = store.applied_migrations().expect("read schema_meta");
    assert_eq!(first, second);
}

/// Migration 0001 exists to create every table, so that a read transaction on a
/// freshly opened database never has to handle a missing one.
#[tokio::test]
async fn every_table_exists_after_the_first_open() {
    let store = open_mem_store().await;
    let read = store.read().expect("read txn");
    for table in db::ROW_TABLES {
        read.open_table(*table)
            .unwrap_or_else(|error| panic!("{} is missing: {error}", table.name()));
    }
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

// ── 4: edge reload, including a fan-out of 12 ───────────────────────────

#[tokio::test]
async fn edges_reload_correctly_for_a_diamond_and_a_fanout_of_twelve() {
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

/// Two edges between the same ordered pair are legal as long as their inbound
/// port names differ, so the edge key has to keep them apart or reloading the
/// version silently loses one.
#[tokio::test]
async fn two_edges_between_the_same_pair_survive_a_reload() {
    let store = open_mem_store().await;
    let workflow = store
        .create_workflow("demo", "", Tier::Auto)
        .await
        .expect("create_workflow");

    let spec = base_spec(
        vec![
            node("plan", "Plan for: {{goal}}"),
            node("build", "Use {{outline}} and {{budget}}"),
        ],
        vec![
            edge("plan", "build", Some("outline")),
            edge("plan", "build", Some("budget")),
        ],
    );
    let kvdag = store
        .create_version(&workflow, VersionOrigin::Authored, "v1", spec)
        .await
        .expect("create_version");

    let mut ports: Vec<Option<String>> = kvdag
        .outbound_edges(&NodeKey::new("plan"))
        .map(|edge| edge.port.clone())
        .collect();
    ports.sort();
    assert_eq!(
        ports,
        vec![Some("budget".to_string()), Some("outline".to_string())]
    );
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
        "a duplicate seq must be rejected, not overwrite the journalled event"
    );

    // Sequence numbers past single digits still replay in numeric order, which
    // is only true because the key pads them.
    for seq in [9, 10, 11, 100] {
        store.write(event(&run, seq)).await.expect("append");
    }
    let seqs: Vec<u64> = store
        .list_run_events(&run)
        .await
        .expect("list_run_events")
        .iter()
        .map(|event| event.seq)
        .collect();
    assert_eq!(seqs, vec![0, 1, 2, 3, 9, 10, 11, 100]);
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

/// The summary is capped separately from the payload, with a visible marker: a
/// silently shortened summary would read as a complete one.
#[tokio::test]
async fn an_over_long_checkpoint_summary_is_truncated_with_a_marker() {
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
            summary: "s".repeat(SUMMARY_BUDGET_CHARS + 500),
            artifact_paths: Vec::new(),
            digest: "deadbeef".to_string(),
        })
        .await
        .expect("checkpoint writes");

    let checkpoints = store
        .list_checkpoints(&run, &InstancePath::new("solo"))
        .await
        .expect("list_checkpoints");
    assert!(checkpoints[0].summary.ends_with("[truncated]"));
    assert!(checkpoints[0].summary.chars().count() < SUMMARY_BUDGET_CHARS + 500);
}

// ── 8: review_finding replace requires a replacement ────────────────────

#[tokio::test]
async fn review_finding_replace_verdict_requires_a_replacement() {
    let store = open_mem_store().await;
    let (_, kvdag, run) = setup_run(&store).await;
    let cycle = store
        .create_review_cycle(&run, &kvdag.version_id, "running")
        .await
        .expect("create_review_cycle");

    let refused = store
        .create_review_finding(
            &cycle,
            &NodeKey::new("solo"),
            None,
            "prompt",
            "replace",
            "needs a rewrite",
            None,
        )
        .await;
    assert!(
        refused.is_err(),
        "a \"replace\" verdict with no replacement must be rejected"
    );

    store
        .create_review_finding(
            &cycle,
            &NodeKey::new("solo"),
            None,
            "prompt",
            "replace",
            "needs a rewrite",
            Some(serde_json::json!({"role": "a better teammate"})),
        )
        .await
        .expect("a \"replace\" verdict with a replacement is accepted");

    assert_eq!(all_review_findings(&store).len(), 1);
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

    let none = store
        .find_restorable_checkpoints(&run, &[NodeKey::new("someone-else")])
        .await
        .expect("find_restorable_checkpoints");
    assert!(
        none.is_empty(),
        "a node key nobody checkpointed matches none"
    );
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
    assert!(
        store
            .list_run_nodes(&runs[0])
            .await
            .expect("list_run_nodes")
            .is_empty(),
        "a pruned run keeps none of its nodes"
    );
    assert_eq!(
        store
            .list_run_nodes(&runs[2])
            .await
            .expect("list_run_nodes")
            .len(),
        1,
        "the retained run keeps all of its nodes"
    );
}

// ── 11: prune leaves no dangling reference ──────────────────────────────

#[tokio::test]
async fn prune_run_history_leaves_no_dangling_references() {
    let store = open_mem_store().await;
    let (workflow, kvdag) = setup_workflow(&store).await;

    let pruned_run = create_run(&store, &workflow, &kvdag).await;
    seed_run_summary(&store, &pruned_run, &kvdag.version_id).await;
    let interrogation = store
        .create_interrogation(
            &pruned_run,
            &InstancePath::new("solo"),
            "sess-1",
            "sess-1-fork",
            "/tmp",
        )
        .await
        .expect("create_interrogation");
    let cycle = store
        .create_review_cycle(&pruned_run, &kvdag.version_id, "running")
        .await
        .expect("create_review_cycle");
    store
        .create_review_finding(
            &cycle,
            &NodeKey::new("solo"),
            Some(&interrogation),
            "prompt",
            "keep",
            "fine",
            None,
        )
        .await
        .expect("create_review_finding");
    store
        .set_run_summary_generated_by(&pruned_run, &InstancePath::new("solo"))
        .await
        .expect("point the summary at this run's node");
    assert!(run_summary_generated_by(&store, &pruned_run).is_some());

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
    assert!(
        run_summary_generated_by(&store, &pruned_run).is_none(),
        "generated_by must be nulled, not left dangling"
    );
    assert!(
        all_interrogations(&store).is_empty(),
        "no interrogation row may reference a deleted run_node"
    );

    let findings = all_review_findings(&store);
    assert_eq!(findings.len(), 1);
    assert!(
        findings[0].interview.is_none(),
        "interview must be nulled once its interrogation is pruned"
    );
}

// ── 12: store-locked path (on-disk) ──────────────────────────────────────

#[tokio::test]
#[ignore = "touches disk: takes a real file lock"]
async fn opening_a_locked_file_reports_unavailable() {
    let dir = unique_temp_dir("locked");
    let path = dir.join("workflow.redb");
    let first = WorkflowStore::open(StoreLocation::OnDisk(path.clone()))
        .await
        .expect("first open succeeds");

    let second = WorkflowStore::open(StoreLocation::OnDisk(path.clone())).await;
    match second {
        Err(StoreError::Unavailable { reason, holder }) => {
            assert_eq!(reason, error::STORE_LOCKED);
            assert_eq!(
                holder,
                Some(format!("pid {}", std::process::id())),
                "the refusal must name whoever holds the lock"
            );
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
    let path = dir.join("workflow.redb");

    let (workflow, version_id, run) = {
        let store = WorkflowStore::open(StoreLocation::OnDisk(path.clone()))
            .await
            .expect("opens");
        let workflow = store
            .create_workflow("demo", "desc", Tier::Auto)
            .await
            .expect("create_workflow");
        let kvdag = store
            .create_version(&workflow, VersionOrigin::Authored, "v1", diamond_spec())
            .await
            .expect("create_version");
        store
            .set_head_version(&workflow, &kvdag.version_id)
            .await
            .expect("set_head_version");
        let run = create_run(&store, &workflow, &kvdag).await;
        store
            .write(StoreWrite::RunEvent {
                run: run.clone(),
                seq: 0,
                kind: RunEventKind::RunStarted,
                path: None,
                payload: serde_json::json!({"note": "started"}),
            })
            .await
            .expect("journal the start");
        (workflow, kvdag.version_id, run)
    };

    // The lock is released when the database handle drops, which happens on the
    // scope above closing — no retry loop needed, unlike an engine that tore
    // its lock down on a background task.
    let store = WorkflowStore::open(StoreLocation::OnDisk(path.clone()))
        .await
        .expect("reopens after the first handle dropped");
    let summary = store
        .get_workflow(&workflow)
        .await
        .expect("get_workflow")
        .expect("workflow persisted across close/reopen");
    assert_eq!(summary.name, "demo");
    assert_eq!(summary.head_version.as_ref(), Some(&version_id));

    let reloaded = store.load_version(&version_id).await.expect("load_version");
    assert_eq!(reloaded.nodes.len(), 4);
    assert_eq!(reloaded.edges.len(), 4);

    let events = store.list_run_events(&run).await.expect("list_run_events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, RunEventKind::RunStarted);
    assert!(
        events[0].at.ends_with('Z'),
        "a journalled timestamp reads as UTC: {}",
        events[0].at
    );

    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
