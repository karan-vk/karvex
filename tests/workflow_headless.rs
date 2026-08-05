//! Headless end-to-end tests for the workflow (kvdag) runtime.
//!
//! `docs/design/workflow-builder/05-phase-plan.md` W7. Three scenarios against
//! a real `kvx server`, over the real JSON API socket:
//!
//! 1. a two-node kvdag runs to `succeeded`, each node visibly executing in its
//!    own pane, with `evidence: self_report` on both;
//! 2. a node that reaches turn end **without a valid result** ends in
//!    `NeedsAttention` and the run does **not** report success — the single
//!    most important behavioural guarantee in the design
//!    (`00-overview.md` D7, `04-kvdag-and-execution.md` §4.3);
//! 3. `workflow.node.steer` delivers text into the node's pane, asserted
//!    through `pane.read`.
//!
//! **Why the fixtures use `runner = "command"`.** W7 is explicit: the
//! managed-agent path is closed to a stub by construction —
//! `begin_managed_agent` takes a `crate::detect::Agent` with no generic
//! variant, and `agent.prompt` answers `agent_not_ready` unless the pane's
//! foreground job resolves to a detected agent. So a fake `claude` would be
//! reported as a spawn failure rather than exercising anything. `runner =
//! "command"` is a declared node field and a first-class binding
//! (`04` §4.2), not a test-only escape hatch: the nodes here are plain
//! processes that write `result.json` and call `kvx workflow node complete`.
//! The `runner = "agent"` path is exercised by the manual real-`claude` run
//! only. Nothing here needs the `claude` binary, a network, or an API key.
//!
//! **What is asserted where.** karvex has no library target, so an integration
//! test cannot link `WorkflowStore`. Per W7 this file asserts only
//! API-observable facts — run status, node status, evidence, pane creation, and
//! the `events.subscribe` stream. Checkpoint contents and the contiguity of the
//! `run_event` journal's `seq` are asserted in `src/workflow/store/tests.rs`,
//! which can link the store directly.

// Pane-driven and event-stream tests are gated off the macOS CI leg, matching
// `tests/api_ping.rs`'s event-subscription tests and `tests/cli.rs`.
#![cfg(not(target_os = "macos"))]

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};

use support::jsonrpc::{
    drain_events, error_code, event_kinds, events_of_kind, first_event_of_kind, open_subscription,
    poll_until, position_of_kind, request_ok, send_request, wait_for_event,
    wait_for_event_matching, JsonLineReader,
};
use support::{
    app_dir_name, cleanup_test_base, register_runtime_dir, register_spawned_karvex_pid,
    unregister_spawned_karvex_pid,
};

/// Node spawn plus a `/bin/sh` stub reporting itself is fast, but the engine
/// pump, the store write, and the pane runtime all sit in between.
const SETTLE: Duration = Duration::from_secs(45);
/// How long a run is watched to prove it does *not* reach `succeeded`.
const NON_SUCCESS_WINDOW: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct WorkflowServer {
    base: PathBuf,
    socket: PathBuf,
    _master: Option<Box<dyn MasterPty + Send>>,
    child: Box<dyn Child + Send + Sync>,
}

impl WorkflowServer {
    fn socket(&self) -> &Path {
        &self.socket
    }

    fn shutdown(self) {
        drop(self);
    }
}

/// All teardown lives here rather than in `shutdown`, so a panicking assertion
/// still reaps the server, its node panes, and the base directory.
impl Drop for WorkflowServer {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        drop(self._master.take());

        if let Some(pid) = pid {
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let mut status = 0;
                let result =
                    unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
                if result == pid as libc::pid_t || result == -1 {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        unregister_spawned_karvex_pid(pid);
        cleanup_test_base(&self.base);
    }
}

fn unique_base(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/karvex-workflow-e2e-{label}-{}-{nanos}",
        std::process::id()
    ))
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("workflow")
}

/// Reads a fixture definition and points its `@STUB@` placeholders at the real
/// stub script. Node dirs live under `KARVEX_WORKFLOW_RUNS_DIR`, so the stub is
/// read from the repo and never written to.
fn definition_text(fixture: &str) -> String {
    let stub = fixture_dir().join("node_stub.sh");
    assert!(stub.exists(), "missing stub script at {}", stub.display());
    let path = fixture_dir().join(fixture);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let replaced = text.replace("@STUB@", &stub.to_string_lossy());
    assert!(
        !replaced.contains("@STUB@"),
        "unresolved placeholder in {fixture}"
    );
    replaced
}

/// Spawns a real headless server whose config, state, workflow database, and
/// run directories all live inside the test's own base directory, and whose
/// `PATH` carries the `kvx` under test so a node's
/// `kvx workflow node complete` resolves to it.
fn spawn_workflow_server(label: &str) -> WorkflowServer {
    let base = unique_base(label);
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let state_home = base.join("state");
    let bin_dir = base.join("bin");
    let socket = runtime_dir.join("karvex.sock");

    fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(&state_home).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();
    register_runtime_dir(&runtime_dir);
    fs::write(
        config_home.join(app_dir_name()).join("config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    // The node stub calls plain `kvx`, exactly as the prompt contract in
    // `04` §4.3 documents; give it the binary under test rather than whatever
    // the developer happens to have installed.
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_kvx"), bin_dir.join("kvx")).unwrap();
    let path_override = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_kvx"));
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", &config_home);
    cmd.env("XDG_RUNTIME_DIR", &runtime_dir);
    cmd.env("XDG_STATE_HOME", &state_home);
    cmd.env("KARVEX_SOCKET_PATH", &socket);
    cmd.env("KARVEX_WORKFLOW_DB_PATH", base.join("workflow-db"));
    cmd.env("KARVEX_WORKFLOW_RUNS_DIR", base.join("workflow-runs"));
    cmd.env("PATH", &path_override);
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("KARVEX_CLIENT_SOCKET_PATH");
    cmd.env_remove("KARVEX_ENV");

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_karvex_pid(child.process_id());
    drop(pair.slave);

    let server = WorkflowServer {
        base,
        socket,
        _master: Some(pair.master),
        child,
    };
    wait_for_socket(server.socket(), Duration::from_secs(20));
    server
}

fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && std::os::unix::net::UnixStream::connect(path).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("socket did not appear at {}", path.display());
}

enum Readiness {
    /// The server can serve `workflow.*`.
    Ready,
    /// A `--no-default-features` build: the schema types compile
    /// unconditionally but there is no engine at all (`05` W3 "Feature-off
    /// behaviour"). `just check-no-workflow` runs the whole suite this way, so
    /// these scenarios legitimately have nothing to assert.
    FeatureOff,
    /// The `workflow` feature is compiled in but the handlers still resolve to
    /// the placeholder engine handle.
    EngineUnwired,
}

fn workflow_readiness(socket: &Path) -> Readiness {
    let response = send_request(socket, &request("probe", "workflow.list", json!({})));
    match error_code(&response).as_str() {
        "workflow_unavailable" => Readiness::FeatureOff,
        "workflow_engine_not_ready" => Readiness::EngineUnwired,
        _ => Readiness::Ready,
    }
}

/// Returns `false` when the scenario cannot run at all (feature-off build).
/// Panics with the exact remaining wiring when the engine is merely unwired —
/// answering `not ready` is a state the phase has to leave behind, and a test
/// that skipped over it would reproduce precisely the silent-pass failure mode
/// `05-phase-plan.md` §6 exists to prevent.
fn require_workflow_api(socket: &Path) -> bool {
    match workflow_readiness(socket) {
        Readiness::Ready => true,
        Readiness::FeatureOff => {
            eprintln!("skipping: server built with --no-default-features");
            false
        }
        Readiness::EngineUnwired => panic!(
            "the server answers workflow.* with `workflow_engine_not_ready`.\n\
             `src/app/api/workflows.rs` still resolves every handler to \
             `WorkflowEngineHandle::Unwired`; the engine, store, and runtime \
             glue it needs all exist:\n  \
             - definition document -> `KvdagSpec` -> `WorkflowStore::create_workflow` \
             / `create_version` (workflow.create, workflow.version.create)\n  \
             - `WorkflowStore::create_run` + `App::start_workflow_run` (workflow.run)\n  \
             - `App::workflow_run_info` / `workflow_node_info` (workflow.run.get, \
             workflow.node.get)\n  \
             - `App::apply_workflow_engine_input(EngineInput::{{Steer,Interrupt,RestartNode}})`\n  \
             - `App::report_workflow_node` (workflow.node.report)\n  \
             - `App::cancel_workflow_run` (workflow.run.cancel)\n\
             Widening `WorkflowEngineHandle` past `Unwired` is what turns this \
             end-to-end suite on."
        ),
    }
}

fn request(id: &str, method: &str, params: Value) -> String {
    json!({ "id": id, "method": method, "params": params }).to_string()
}

fn subscribe(socket: &Path) -> (JsonLineReader, Vec<Value>) {
    let mut reader = open_subscription(
        socket,
        &request(
            "sub_workflow",
            "events.subscribe",
            json!({
                "subscriptions": [
                    { "type": "workflow.run.started" },
                    { "type": "workflow.run.updated" },
                    { "type": "workflow.run.finished" },
                    { "type": "workflow.node.created" },
                    { "type": "workflow.node.updated" },
                    { "type": "workflow.node.output_checkpoint" },
                    { "type": "pane.created" },
                ]
            }),
        ),
    );
    let ack = reader.read_json_line(Duration::from_secs(5));
    assert_eq!(ack["id"], "sub_workflow", "unexpected subscribe ack: {ack}");
    assert_eq!(
        ack["result"]["type"], "subscription_started",
        "subscribe was rejected: {ack}"
    );
    (reader, Vec::new())
}

/// Every run needs somewhere to put its panes; create the workspace explicitly
/// so the run's cwd is the test's own base directory.
fn create_workspace(socket: &Path, cwd: &Path) -> String {
    let result = request_ok(
        socket,
        &request(
            "req_ws",
            "workspace.create",
            json!({ "cwd": cwd.to_string_lossy(), "focus": true }),
        ),
    );
    result["workspace"]["workspace_id"]
        .as_str()
        .expect("workspace.create returned no workspace_id")
        .to_string()
}

fn create_workflow(socket: &Path, fixture: &str) -> String {
    let result = request_ok(
        socket,
        &request(
            "req_create",
            "workflow.create",
            json!({
                "definition": { "format": "toml", "text": definition_text(fixture) }
            }),
        ),
    );
    assert_eq!(result["type"], "workflow_created", "unexpected: {result}");
    result["workflow"]["workflow_id"]
        .as_str()
        .expect("workflow.create returned no workflow_id")
        .to_string()
}

fn start_run(socket: &Path, workflow_id: &str, goal: &str) -> String {
    let result = request_ok(
        socket,
        &request(
            "req_run",
            "workflow.run",
            json!({ "workflow_id": workflow_id, "args": { "goal": goal } }),
        ),
    );
    assert_eq!(
        result["type"], "workflow_run_started",
        "unexpected: {result}"
    );
    result["run"]["run_id"]
        .as_str()
        .expect("workflow.run returned no run_id")
        .to_string()
}

fn run_get(socket: &Path, run_id: &str) -> Value {
    request_ok(
        socket,
        &request(
            "req_run_get",
            "workflow.run.get",
            json!({ "run_id": run_id }),
        ),
    )
}

fn node_get(socket: &Path, run_id: &str, path: &str) -> Value {
    let response = send_request(
        socket,
        &request(
            "req_node_get",
            "workflow.node.get",
            json!({ "run_id": run_id, "path": path }),
        ),
    );
    assert!(
        response.get("error").is_none(),
        "workflow.node.get {path} failed: {response}"
    );
    response["result"]["node"].clone()
}

fn run_status(socket: &Path, run_id: &str) -> String {
    run_get(socket, run_id)["run"]["status"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn pane_text(socket: &Path, pane_id: &str) -> String {
    let result = request_ok(
        socket,
        &request(
            "req_pane_read",
            "pane.read",
            // Unwrapped, so a marker that straddles the pane's right edge is
            // still one contiguous substring.
            json!({
                "pane_id": pane_id,
                "source": "recent_unwrapped",
                "lines": 200,
                "format": "text",
                "strip_ansi": true,
            }),
        ),
    );
    result["read"]["text"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| panic!("pane.read returned no text: {result}"))
}

/// The instance paths named in the stream's `workflow.node.created` events.
fn created_node_paths(events: &[Value]) -> BTreeSet<String> {
    events_of_kind(events, "workflow_node_created")
        .into_iter()
        .filter_map(|event| event["data"]["node"]["path"].as_str())
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------
// 1. Two-node kvdag runs to completion
// ---------------------------------------------------------------------------

#[test]
fn two_node_command_kvdag_runs_to_succeeded_with_a_pane_per_node() {
    let server = spawn_workflow_server("two-node");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe(&socket);

    let workflow_id = create_workflow(&socket, "two_node_command.toml");
    let run_id = start_run(&socket, &workflow_id, "add dark mode");

    // ── the event stream ────────────────────────────────────────────────────
    let started = wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_run_started",
        SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );
    assert_eq!(started["data"]["run"]["workflow_id"], workflow_id.as_str());

    let finished = wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_run_finished",
        SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );
    assert_eq!(
        finished["data"]["run"]["status"], "succeeded",
        "the run must report success, not merely finish: {finished}"
    );
    drain_events(&mut reader, &mut seen, Duration::from_millis(500));

    assert_eq!(
        created_node_paths(&seen),
        BTreeSet::from(["plan".to_string(), "implement".to_string()]),
        "both nodes must be announced; saw {:?}",
        event_kinds(&seen)
    );
    assert!(
        position_of_kind(&seen, "workflow_run_started")
            < position_of_kind(&seen, "workflow_node_created"),
        "run.started must precede node.created; saw {:?}",
        event_kinds(&seen)
    );
    assert!(
        position_of_kind(&seen, "workflow_node_created")
            < position_of_kind(&seen, "workflow_run_finished"),
        "node.created must precede run.finished; saw {:?}",
        event_kinds(&seen)
    );
    assert!(
        !events_of_kind(&seen, "workflow_node_updated").is_empty(),
        "node status changes must be published; saw {:?}",
        event_kinds(&seen)
    );

    // Checkpoint sequences are per run node and start at 1
    // (`03-storage-schema.md` §4.3 `checkpoint_seq`). Contiguity of the
    // `run_event` journal is asserted in the in-crate store tests, which can
    // link the store; here only the checkpoint stream is observable.
    for path in ["plan", "implement"] {
        let mut seqs: Vec<u64> = events_of_kind(&seen, "workflow_node_output_checkpoint")
            .into_iter()
            .filter(|event| event["data"]["path"] == path)
            .filter_map(|event| event["data"]["seq"].as_u64())
            .collect();
        seqs.sort_unstable();
        assert!(
            !seqs.is_empty(),
            "node {path} produced no output checkpoint; saw {:?}",
            event_kinds(&seen)
        );
        let expected: Vec<u64> = (1..=seqs.len() as u64).collect();
        assert_eq!(
            seqs, expected,
            "node {path} checkpoint seq is not contiguous"
        );
    }

    // ── the run and its nodes, read back over the API ───────────────────────
    let run = run_get(&socket, &run_id);
    assert_eq!(run["run"]["status"], "succeeded");
    assert_eq!(run["run"]["run_id"], run_id.as_str());

    let graph_nodes = run["graph"]["nodes"]
        .as_array()
        .expect("workflow.run.get returned no run graph")
        .clone();
    assert_eq!(graph_nodes.len(), 2, "expected two run nodes: {run}");

    let mut pane_ids = BTreeSet::new();
    for path in ["plan", "implement"] {
        let node = node_get(&socket, &run_id, path);
        assert_eq!(node["status"], "succeeded", "node {path}: {node}");
        assert_eq!(
            node["evidence"], "self_report",
            "node {path} must complete through the self-report signal, not a \
             weaker one: {node}"
        );
        let pane_id = node["pane_id"]
            .as_str()
            .unwrap_or_else(|| panic!("node {path} has no pane binding: {node}"));
        assert!(
            pane_ids.insert(pane_id.to_string()),
            "node {path} reuses another node's pane {pane_id}"
        );
    }
    assert_eq!(pane_ids.len(), 2, "each node must get its own pane");

    // The panes really were created by the runtime, not just recorded.
    let announced: BTreeSet<String> = events_of_kind(&seen, "pane_created")
        .into_iter()
        .filter_map(|event| event["data"]["pane"]["pane_id"].as_str())
        .map(str::to_string)
        .collect();
    for pane_id in &pane_ids {
        assert!(
            announced.contains(pane_id),
            "pane {pane_id} was bound to a node but never announced as created; \
             announced {announced:?}"
        );
        // A live pane the API can still read is the visible-teammate promise.
        let _ = pane_text(&socket, pane_id);
    }

    // The run is durable enough to list, which is the API-observable half of
    // "the whole run is persisted".
    let listed = request_ok(
        &socket,
        &request(
            "req_run_list",
            "workflow.run.list",
            json!({ "workflow_id": workflow_id }),
        ),
    );
    let runs = listed["runs"].as_array().expect("no runs array");
    assert!(
        runs.iter().any(|run| run["run_id"] == run_id.as_str()),
        "the finished run is missing from workflow.run.list: {listed}"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 2. No valid result ⇒ NeedsAttention, and the run does not succeed
// ---------------------------------------------------------------------------

#[test]
fn node_without_a_valid_result_ends_needs_attention_and_the_run_does_not_succeed() {
    let server = spawn_workflow_server("no-result");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe(&socket);

    let workflow_id = create_workflow(&socket, "no_valid_result.toml");
    let run_id = start_run(&socket, &workflow_id, "add dark mode");

    wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_run_started",
        SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );

    // The node reports twice with a document that does not satisfy its output
    // schema. The first report earns exactly one corrective re-prompt; the
    // second must not be allowed to complete the node (`04` §4.3).
    let attention = wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_node_updated",
        SETTLE,
        |event| {
            event["data"]["node"]["path"] == "stalled"
                && event["data"]["node"]["status"] == "needs_attention"
        },
    );
    assert_eq!(attention["data"]["run_id"], run_id.as_str());

    let stalled = node_get(&socket, &run_id, "stalled");
    assert_eq!(stalled["status"], "needs_attention", "{stalled}");
    assert!(
        stalled["evidence"].is_null(),
        "a node with no valid result must record no completion evidence: {stalled}"
    );

    // The downstream node must not evaporate *or* advance: its inbound data
    // edge never resolved (`04` §3.1).
    let after = node_get(&socket, &run_id, "after");
    assert!(
        matches!(after["status"].as_str(), Some("pending") | Some("blocked")),
        "the downstream node must stay unscheduled while its input is unmet: {after}"
    );

    // `run_terminal_ready` is a conjunction (`04` §3.2): a node in
    // NeedsAttention refuses the run success, so the run must never report
    // `succeeded` — not now and not after it stops making progress.
    let deadline = Instant::now() + NON_SUCCESS_WINDOW;
    let mut last = String::new();
    while Instant::now() < deadline {
        last = run_status(&socket, &run_id);
        assert_ne!(
            last, "succeeded",
            "the run reported success with a node in NeedsAttention"
        );
        thread::sleep(Duration::from_millis(250));
    }
    assert!(
        matches!(last.as_str(), "running" | "paused"),
        "expected the run to stall as running or paused, got {last}"
    );

    drain_events(&mut reader, &mut seen, Duration::from_millis(500));
    for finished in events_of_kind(&seen, "workflow_run_finished") {
        assert_ne!(
            finished["data"]["run"]["status"], "succeeded",
            "a run.finished event claimed success: {finished}"
        );
    }
    assert!(
        events_of_kind(&seen, "workflow_node_output_checkpoint")
            .into_iter()
            .all(|event| event["data"]["path"] != "stalled"),
        "an invalid result must not be checkpointed as a node output; saw {:?}",
        event_kinds(&seen)
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 3. Steering reaches the node's pane
// ---------------------------------------------------------------------------

#[test]
fn node_steer_delivers_text_into_the_nodes_pane() {
    let server = spawn_workflow_server("steer");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe(&socket);

    let workflow_id = create_workflow(&socket, "steerable.toml");
    let run_id = start_run(&socket, &workflow_id, "add dark mode");

    wait_for_event(&mut reader, &mut seen, "workflow_run_started", SETTLE);

    // Wait for the node to actually be bound to a pane; steering an unbound
    // node would journal without delivering.
    let pane_id = poll_until(
        "the node to be bound to a pane",
        SETTLE,
        Duration::from_millis(200),
        || {
            node_get(&socket, &run_id, "worker")["pane_id"]
                .as_str()
                .map(str::to_string)
        },
    );

    let steer_text = "karvex-e2e-steer-marker";
    let steered = request_ok(
        &socket,
        &request(
            "req_steer",
            "workflow.node.steer",
            json!({ "run_id": run_id, "path": "worker", "text": steer_text }),
        ),
    );
    assert_eq!(steered["type"], "workflow_node_steered", "{steered}");

    // `04` §5: a `runner = "command"` node is steered with `pane.send_text`,
    // so the text lands in the pane itself and `pane.read` can see it.
    poll_until(
        "the steered text to reach the pane",
        Duration::from_secs(20),
        Duration::from_millis(200),
        || {
            pane_text(&socket, &pane_id)
                .contains(steer_text)
                .then_some(())
        },
    );

    // Steering is a delivery, not a completion: the node keeps running.
    let node = node_get(&socket, &run_id, "worker");
    assert_eq!(
        node["status"], "running",
        "a steer must not complete the node: {node}"
    );
    assert_ne!(run_status(&socket, &run_id), "succeeded");

    drain_events(&mut reader, &mut seen, Duration::from_millis(300));
    let _ = first_event_of_kind(&seen, "workflow_node_created");

    server.shutdown();
}
