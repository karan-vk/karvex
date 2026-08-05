//! Store test plan (`docs/design/workflow-builder/03-storage-schema.md` §10).
//!
//! Cases 1-11 run against `kv-mem` (no disk, no PTY). Cases 12-13 need a real
//! on-disk `SurrealKv` lock and are marked `#[ignore]` by default.

use std::collections::BTreeMap;

use super::records::{self, parse_record_id, record_id_to_string};
use super::*;
use crate::workflow::model::{ArgSpec, InstancePath, KvdagError, Runner};
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
        std::collections::BTreeSet::from(["0001_init".to_string()])
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
