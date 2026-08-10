//! Headless end-to-end tests for the workflow (kvdag) runtime.
//!
//! `docs/design/workflow-builder/05-phase-plan.md` W7. Five scenarios against
//! a real `kvx server`, over the real JSON API socket:
//!
//! 1. a two-node kvdag runs to `succeeded`, each node visibly executing in its
//!    own pane, with `evidence: self_report` on both;
//! 2. a node that reaches turn end **without a valid result** ends in
//!    `NeedsAttention` and the run does **not** report success — the single
//!    most important behavioural guarantee in the design
//!    (`00-overview.md` D7, `04-kvdag-and-execution.md` §4.3);
//! 3. a node whose pane exits before a valid result ends `failed` and the run
//!    reaches a terminal status instead of staying live forever (`04` §4.3);
//! 4. `workflow.node.steer` delivers text into the node's pane, asserted
//!    through `pane.read`;
//! 5. an agent node whose seed prompt is swallowed at startup is re-seeded
//!    with an absolute `task.md` path instead of hanging forever (`04` §4.2);
//! 6. a finished run, re-read from a *restarted* server that never executed it,
//!    describes the same run field for field — the store's read path is not
//!    allowed to know less than the engine did.
//!
//! Phase 2 (`06-phase2-plan.md` WS-J) adds three more, all against
//! `tests/fixtures/workflow/expand.toml`:
//!
//! 7. an accepted expansion creates children that inherit their parent's
//!    outbound edges, so the downstream fan-in node waits for the whole
//!    generation (`04` §3.4, §4 D4);
//! 8. a `--tier low` run whose proposal does not fit creates what fits and
//!    **reports the shortfall** on the event stream, on the run projection, and
//!    to the proposing node — the phase's headline guarantee, which is an e2e
//!    and not a unit test precisely because it is a claim about surfaces
//!    (§4 D2, §5 R-6);
//! 9. a proposal naming a template outside the proposer's `expand_allow` is
//!    refused with no side effects at all.
//!
//! Phase 2's ordering fix adds a tenth:
//!
//! 10. one `events.subscribe` connection carrying every `workflow.*` type
//!     delivers a fast expansion run in **causal** order — a node's
//!     `workflow.node.created` before that node's updates and checkpoints, and
//!     `workflow.run.finished` after every node event of the run — plus a
//!     `workflow.run.updated` that carries the grown `nodes_total`.
//!
//! **On event ordering.** `events.subscribe` gives every event-log-backed
//! subscription on a connection **one shared cursor** over the hub's global
//! sequence, and each poll pass drains the whole backlog in that order. So a
//! single connection's stream position *is* causal order: a node's
//! `workflow.node.created` precedes its updates and checkpoints, and
//! `workflow.run.finished` follows the node events it summarises. Before this
//! each subscribed `type` had its own cursor and the loop yielded at most one
//! event per subscription per pass, so a run with eleven queued node events and
//! one queued `run.finished` delivered the finish first — the P1 regression
//! scenario 10 pins. Assertions written against *payloads* rather than stream
//! position are still fine, and most of the older scenarios below keep doing
//! that; scenario 10 is the one that asserts the guarantee itself.
//!
//! **Why almost every fixture uses `runner = "command"`.** `runner =
//! "command"` is a declared node field and a first-class binding (`04` §4.2),
//! not a test-only escape hatch: the nodes here are plain processes that write
//! `result.json` and call `kvx workflow node complete`. That makes the
//! completion, steering, and persistence scenarios deterministic and offline.
//!
//! **The one `runner = "agent"` scenario.** W7 assumed the managed-agent path
//! was closed to a stub. It is not: `confirm_managed_agent` calls
//! `begin_managed_agent` for *every* `Runner::Agent` node without inspecting
//! the binary, agent detection is driven entirely by what the pane renders,
//! and `agent_argv` resolves `claude` through `PATH`. So
//! `tests/fixtures/workflow/agent_stub.sh` — installed as `claude` on the
//! server's `PATH` by the harness — is a real managed agent as far as the
//! runtime is concerned, and `agent_seed.toml` exercises the seed-prompt path
//! end to end. Still no network, no API key, and no real `claude`; the full
//! real-`claude` behaviours (tool use, transcripts, tier resolution) remain a
//! manual run.
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
    /// Set only while a restart is in flight, so the base directory (and with
    /// it the workflow database) outlives the process that owned it.
    keep_base: bool,
    /// The `config.toml` body and extra environment this server was brought up
    /// with, kept so [`WorkflowServer::restart`] can bring its replacement up
    /// the same way. A restart that quietly dropped
    /// `KARVEX_WORKFLOW_SUMMARY_COMMAND` would read a summarised run back
    /// through a server that cannot summarise.
    extra_config: String,
    extra_env: Vec<(String, String)>,
}

impl WorkflowServer {
    fn socket(&self) -> &Path {
        &self.socket
    }

    fn shutdown(self) {
        drop(self);
    }

    /// Stops this server and brings a fresh one up on the same base directory,
    /// so the replacement reopens the same workflow database.
    ///
    /// `server.stop` rather than a signal: the store's SurrealKv directory is
    /// held under an exclusive lock, and the replacement can only take it once
    /// the previous owner has really gone, so the exit is waited for rather
    /// than assumed.
    fn restart(mut self) -> WorkflowServer {
        let base = self.base.clone();
        let socket = self.socket.clone();
        let extra_config = self.extra_config.clone();
        let extra_env = self.extra_env.clone();
        send_server_stop(&socket);

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut exited = false;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                exited = true;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(exited, "the server did not exit after server.stop");

        self.keep_base = true;
        drop(self);
        // A stopped server unlinks its own socket; removing it here keeps a
        // leftover file from making `wait_for_socket` connect to nothing.
        let _ = fs::remove_file(&socket);
        let borrowed: Vec<(&str, &str)> = extra_env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        spawn_workflow_server_at_with(base, &extra_config, &borrowed)
    }
}

/// Sends `server.stop` without waiting for a reply: the server tears the socket
/// down as it shuts down, so reading a response back is a race, not a signal.
fn send_server_stop(socket: &Path) {
    use std::io::Write;
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return;
    };
    let line = format!("{}\n", request("req_stop", "server.stop", json!({})));
    let _ = stream.write_all(line.as_bytes());
    let _ = stream.flush();
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
        if !self.keep_base {
            cleanup_test_base(&self.base);
        }
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
    spawn_workflow_server_at(unique_base(label))
}

/// [`spawn_workflow_server`] with extra `config.toml` lines appended, mirroring
/// `tests/cli/workflow.rs`'s convention.
///
/// The supported way for a fixture to turn a documented knob off — in practice
/// `workflow.summary_enabled`, for a test that is not about summaries and must
/// not sit inside the epilogue's admission window (`07-phase3-plan.md` §4 D1's
/// M8 contract: `run.finished` finalises the *outcome*, not the admission of the
/// next run).
///
/// Deliberately **not** the default. Every test that leaves `summary_enabled`
/// at its documented `true` also pins the real epilogue path for free —
/// including give-up-when-`claude`-is-missing — which is exactly the coverage
/// §4 D1's failure ladder wants, so the knob is turned off only where the
/// epilogue is incidental to what the test is asserting.
fn spawn_workflow_server_with_config(label: &str, extra_config: &str) -> WorkflowServer {
    spawn_workflow_server_at_with(unique_base(label), extra_config, &[])
}

/// [`spawn_workflow_server_with_config`] plus extra environment for the server
/// process itself.
///
/// The one variable this exists for is `KARVEX_WORKFLOW_SUMMARY_COMMAND`
/// (`07-phase3-plan.md` §4 D2 / §6 A4): a declared binding override, read once
/// at engine-config time, that runs the epilogue as a command instead of
/// `claude`. It is what makes the summariser's whole lifecycle observable in CI
/// — the alternative is the `claude` on `PATH`, which here is `agent_stub.sh`
/// and resolves only through the give-up ladder on a schedule no assertion
/// should depend on.
fn spawn_workflow_server_with_env(
    label: &str,
    extra_config: &str,
    extra_env: &[(&str, &str)],
) -> WorkflowServer {
    spawn_workflow_server_at_with(unique_base(label), extra_config, extra_env)
}

/// Brings a server up on an explicit base directory. Split out of
/// [`spawn_workflow_server`] so a restart can reuse one — every path the server
/// is given, including `KARVEX_WORKFLOW_DB_PATH`, is derived from the base, so
/// re-spawning on the same base is what makes the replacement read the same
/// store.
fn spawn_workflow_server_at(base: PathBuf) -> WorkflowServer {
    spawn_workflow_server_at_with(base, "", &[])
}

fn spawn_workflow_server_at_with(
    base: PathBuf,
    extra_config: &str,
    extra_env: &[(&str, &str)],
) -> WorkflowServer {
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
        format!("onboarding = false\n{extra_config}"),
    )
    .unwrap();

    // The node stub calls plain `kvx`, exactly as the prompt contract in
    // `04` §4.3 documents; give it the binary under test rather than whatever
    // the developer happens to have installed.
    // Removed first so re-spawning on an existing base (a restart) is not a
    // `symlink` conflict.
    let _ = fs::remove_file(bin_dir.join("kvx"));
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_kvx"), bin_dir.join("kvx")).unwrap();

    // `agent_argv` resolves the agent runner's executable through `PATH`
    // (`crate::detect::interactive_agent_executable`), so the `claude` an
    // `runner = "agent"` node launches is this stub. It is placed for every
    // server, not just the agent test: what it does is decided entirely by the
    // fixture that spawns it, and only `agent_seed.toml` uses `runner =
    // "agent"` at all.
    let _ = fs::remove_file(bin_dir.join("claude"));
    std::os::unix::fs::symlink(fixture_dir().join("agent_stub.sh"), bin_dir.join("claude"))
        .unwrap();

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
    // Inherited by the node pane the agent runner spawns, so the stub records
    // what it was launched with and what was later delivered into it.
    cmd.env("AGENT_STUB_LOG", agent_stub_log(&base));
    cmd.env("PATH", &path_override);
    cmd.env("SHELL", "/bin/sh");
    cmd.env_remove("KARVEX_CLIENT_SOCKET_PATH");
    cmd.env_remove("KARVEX_ENV");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_karvex_pid(child.process_id());
    drop(pair.slave);

    let server = WorkflowServer {
        base,
        socket,
        _master: Some(pair.master),
        child,
        keep_base: false,
        extra_config: extra_config.to_string(),
        extra_env: extra_env
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect(),
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
    /// The store itself refused to open — this binary only exists with the
    /// `workflow` feature on (`[[test]] required-features` in `Cargo.toml`),
    /// so this is a genuine runtime failure (a lock held by another process,
    /// or a corrupt/unwritable database directory), never an expected build
    /// configuration.
    StoreUnavailable,
    /// The store is reachable but the handlers still resolve to the
    /// placeholder engine handle.
    EngineUnwired,
}

fn workflow_readiness(socket: &Path) -> Readiness {
    let response = send_request(socket, &request("probe", "workflow.list", json!({})));
    match error_code(&response).as_str() {
        "workflow_unavailable" => Readiness::StoreUnavailable,
        "workflow_engine_not_ready" => Readiness::EngineUnwired,
        _ => Readiness::Ready,
    }
}

/// Always returns `true`; the `bool` is kept so callers read as a guard.
/// Panics with the exact remaining wiring when the engine is merely unwired —
/// answering `not ready` is a state the phase has to leave behind, and a test
/// that skipped over it would reproduce precisely the silent-pass failure mode
/// `05-phase-plan.md` §6 exists to prevent. A store that will not open is
/// failed the same way rather than skipped: this suite only compiles with the
/// `workflow` feature on, so there is no configuration in which having nothing
/// to assert is correct.
fn require_workflow_api(socket: &Path) -> bool {
    match workflow_readiness(socket) {
        Readiness::Ready => true,
        Readiness::StoreUnavailable => panic!(
            "the server answers workflow.* with `workflow_unavailable`.\n\
             This binary is only built with the `workflow` feature on, so this \
             is a real store failure, not a feature-off build: `WorkflowStore` \
             could not open its SurrealKV database. Check for another karvex \
             server holding the lock on `$KARVEX_WORKFLOW_DB_PATH` (default \
             `<state_dir>/workflow`), and for a corrupt or read-only database \
             directory."
        ),
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
    create_workflow_from_text(socket, &definition_text(fixture))
}

/// [`create_workflow`] for a definition the caller has already substituted.
///
/// The expansion fixture is parameterised (see [`expand_definition_text`]), so
/// its three scenarios are three documents built from one file rather than
/// three near-identical files.
fn create_workflow_from_text(socket: &Path, text: &str) -> String {
    let result = request_ok(
        socket,
        &request(
            "req_create",
            "workflow.create",
            json!({
                "definition": { "format": "toml", "text": text }
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
    start_run_at_tier(socket, workflow_id, goal, None)
}

/// [`start_run`] with an explicit tier.
///
/// `tier` is what narrows the run's growth ceilings (`06-phase2-plan.md` §4
/// D17 / `engine/graph.rs::narrow_growth`), so a truncation scenario is a
/// property of the run rather than of the document it runs.
fn start_run_at_tier(socket: &Path, workflow_id: &str, goal: &str, tier: Option<&str>) -> String {
    let mut params = json!({ "workflow_id": workflow_id, "args": { "goal": goal } });
    if let (Some(tier), Some(object)) = (tier, params.as_object_mut()) {
        object.insert("tier".into(), json!(tier));
    }
    let result = request_ok(socket, &request("req_run", "workflow.run", params));
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
// Expansion harness (`06-phase2-plan.md` WS-J)
// ---------------------------------------------------------------------------

/// Expansion fans out real panes, and `max_parallel_nodes` defaults to 4, so
/// the truncation scenario's generation settles in waves rather than at once.
const EXPANSION_SETTLE: Duration = Duration::from_secs(120);

/// The proposing node of `expand.toml`, and the template it is allowed to
/// instantiate. Named once because every expansion assertion is written against
/// them.
const PROPOSER: &str = "fanout";
const TEMPLATE: &str = "worker";
/// A real template that is deliberately outside the proposer's `expand_allow`.
const DISALLOWED_TEMPLATE: &str = "quarantined";

/// One proposal for `count` children under a single label — the shape a
/// ceiling is reached by, and therefore the shape the truncation and refusal
/// scenarios use.
const BATCH: &str = "batch";
/// `count` proposals of one child each, every one with its own `--label` and
/// `--input goal=…`. §3.4's fan-out: a node proposes children *and tells each
/// of them what to work on*.
const SHARDS: &str = "shards";

/// `expand.toml` with its placeholders resolved: the three stub scripts, the
/// proposal mode, the template the proposing node names, and the `--count` it
/// asks for.
///
/// One fixture, four scenarios — the graph is identical and only the proposal
/// differs, so an assertion about the accepted case and one about the refused
/// case are comparing the same run shape.
fn expand_definition_text(mode: &str, template: &str, count: u16) -> String {
    let fanout_stub = fixture_dir().join("expand_stub.sh");
    let worker_stub = fixture_dir().join("expand_worker_stub.sh");
    for stub in [&fanout_stub, &worker_stub] {
        assert!(stub.exists(), "missing stub at {}", stub.display());
    }
    let text = definition_text("expand.toml")
        .replace("@FANOUT@", &fanout_stub.to_string_lossy())
        .replace("@WORKER@", &worker_stub.to_string_lossy())
        .replace("@MODE@", mode)
        .replace("@TEMPLATE@", template)
        .replace("@COUNT@", &count.to_string());
    for placeholder in ["@FANOUT@", "@WORKER@", "@MODE@", "@TEMPLATE@", "@COUNT@"] {
        assert!(
            !text.contains(placeholder),
            "unresolved {placeholder} in expand.toml"
        );
    }
    text
}

/// The verdict the *proposing node* was handed, read back from the file
/// `expand_stub.sh` wrote into its own node directory.
///
/// The expand response goes to the node, not to the operator — §3 frozen
/// interface 7 makes a rejection a **success** response on that one channel —
/// so this is the only way to assert what the node was actually told. Read as
/// the whole response envelope, exactly as `--json` printed it.
fn expand_verdict(socket: &Path, run_id: &str) -> Value {
    expand_verdict_file(socket, run_id, "expand.json")
}

/// The verdict of the `n`th one-child proposal a `shards`-mode proposer made.
fn expand_shard_verdict(socket: &Path, run_id: &str, n: u16) -> Value {
    expand_verdict_file(socket, run_id, &format!("expand.{n}.json"))
}

fn expand_verdict_file(socket: &Path, run_id: &str, file: &str) -> Value {
    let verdict_path = Path::new(&node_dir_of(socket, run_id, PROPOSER)).join(file);
    let text = poll_until(
        &format!("the proposal verdict at {}", verdict_path.display()),
        SETTLE,
        Duration::from_millis(100),
        || fs::read_to_string(&verdict_path).ok(),
    );
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("the verdict is not JSON ({err}): {text}"))
}

/// A node's own directory (`04` §4.1), which is where the karvex-written half
/// of its contract lives: `task.md`, `inputs/`, `output_schema.json`.
fn node_dir_of(socket: &Path, run_id: &str, path: &str) -> String {
    let node = node_get(socket, run_id, path);
    node["node_dir"]
        .as_str()
        .unwrap_or_else(|| panic!("node {path} has no node_dir: {node}"))
        .to_string()
}

/// The `task.md` karvex rendered for a node: the prompt a teammate actually
/// reads, with its label as the title and every `{{slot}}` filled.
fn task_markdown(socket: &Path, run_id: &str, path: &str) -> String {
    let task = Path::new(&node_dir_of(socket, run_id, path)).join("task.md");
    fs::read_to_string(&task)
        .unwrap_or_else(|err| panic!("node {path} has no task.md at {}: {err}", task.display()))
}

/// Every pane's `label` in the run's workspace, keyed by public pane id. The
/// pane title is where a node's label reaches the user outside the DAG view.
fn pane_labels(socket: &Path) -> std::collections::BTreeMap<String, String> {
    let result = request_ok(socket, &request("req_pane_list", "pane.list", json!({})));
    result["panes"]
        .as_array()
        .unwrap_or_else(|| panic!("pane.list returned no panes: {result}"))
        .iter()
        .filter_map(|pane| {
            Some((
                pane["pane_id"].as_str()?.to_string(),
                pane["label"].as_str()?.to_string(),
            ))
        })
        .collect()
}

/// Accepted instance paths, in the order the response listed them.
fn accepted_paths(verdict: &Value) -> Vec<String> {
    verdict["result"]["accepted"]
        .as_array()
        .unwrap_or_else(|| panic!("the verdict carries no accepted list: {verdict}"))
        .iter()
        .filter_map(|path| path.as_str())
        .map(str::to_string)
        .collect()
}

fn rejections(verdict: &Value) -> Vec<Value> {
    verdict["result"]["rejected"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// A subscription carrying everything the expansion scenarios assert on,
/// including the one event kind Phase 2 adds.
///
/// Separate from [`subscribe`] so the Phase 1 scenarios keep the exact stream
/// they were written against.
fn subscribe_expansion(socket: &Path) -> (JsonLineReader, Vec<Value>) {
    let mut reader = open_subscription(
        socket,
        &request(
            "sub_expand",
            "events.subscribe",
            json!({
                "subscriptions": [
                    { "type": "workflow.run.started" },
                    { "type": "workflow.run.finished" },
                    { "type": "workflow.node.created" },
                    { "type": "workflow.growth.limited" },
                ]
            }),
        ),
    );
    let ack = reader.read_json_line(Duration::from_secs(5));
    assert_eq!(ack["id"], "sub_expand", "unexpected subscribe ack: {ack}");
    assert_eq!(
        ack["result"]["type"], "subscription_started",
        "subscribe was rejected: {ack}"
    );
    (reader, Vec::new())
}

/// One connection carrying every `workflow.*` event type, which is what a
/// client migrating off polling actually opens — and the only shape in which
/// the cross-type ordering guarantee is observable.
fn subscribe_all_workflow_events(socket: &Path) -> (JsonLineReader, Vec<Value>) {
    let mut reader = open_subscription(
        socket,
        &request(
            "sub_order",
            "events.subscribe",
            json!({
                "subscriptions": [
                    { "type": "workflow.run.started" },
                    { "type": "workflow.run.updated" },
                    { "type": "workflow.run.finished" },
                    { "type": "workflow.node.created" },
                    { "type": "workflow.node.updated" },
                    { "type": "workflow.node.output_checkpoint" },
                    { "type": "workflow.growth.limited" },
                ]
            }),
        ),
    );
    let ack = reader.read_json_line(Duration::from_secs(5));
    assert_eq!(ack["id"], "sub_order", "unexpected subscribe ack: {ack}");
    assert_eq!(
        ack["result"]["type"], "subscription_started",
        "subscribe was rejected: {ack}"
    );
    (reader, Vec::new())
}

/// The instance path an event is about, however that event spells it.
fn event_node_path(event: &Value) -> Option<String> {
    event["data"]["node"]["path"]
        .as_str()
        .or_else(|| event["data"]["path"].as_str())
        .map(str::to_string)
}

/// Every `workflow.node.created` event announcing a child of the proposer.
fn spawned_child_events(events: &[Value]) -> Vec<&Value> {
    events_of_kind(events, "workflow_node_created")
        .into_iter()
        .filter(|event| event["data"]["node"]["parent_path"] == PROPOSER)
        .collect()
}

/// The run graph's edges as `from -> to` pairs.
fn edge_pairs(run: &Value) -> BTreeSet<String> {
    run["graph"]["edges"]
        .as_array()
        .unwrap_or_else(|| panic!("workflow.run.get returned no run graph: {run}"))
        .iter()
        .filter_map(|edge| {
            Some(format!(
                "{} -> {}",
                edge["from"].as_str()?,
                edge["to"].as_str()?
            ))
        })
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
        // `.summary` is the engine-owned epilogue, announced through the same
        // `workflow.node.created` every other node uses so the DAG shows the
        // summariser live (`07-phase3-plan.md` §1 WS-H). The set stays exact:
        // the epilogue's visibility is pinned here, not waved through.
        BTreeSet::from([
            "plan".to_string(),
            "implement".to_string(),
            ".summary".to_string(),
        ]),
        "both nodes must be announced; saw {:?}",
        event_kinds(&seen)
    );
    // Causal order is asserted from the payloads, not from stream position.
    // `events.subscribe` gives every requested `type` its own cursor and
    // `stream_subscriptions` yields at most one event per subscription per poll
    // pass, so two events of *different* kinds have no guaranteed relative
    // position on the wire — only events of the same kind do.
    assert_eq!(
        started["data"]["run"]["nodes_done"], 0,
        "run.started is published before any node has closed: {started}"
    );
    assert_eq!(
        started["data"]["run"]["nodes_total"], 2,
        "the graph is materialised before run.started: {started}"
    );
    assert_eq!(
        finished["data"]["run"]["nodes_done"], 2,
        "run.finished reports every node closed: {finished}"
    );
    assert!(
        position_of_kind(&seen, "workflow_node_created")
            < position_of_kind(&seen, "workflow_node_updated"),
        "within one subscription the stream is ordered; saw {:?}",
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
    // §4 D1's post-`RunFinished` contract (M8): `workflow.run.get` on a
    // finished run returns the terminal status *and* a graph containing the
    // `.summary` node in its live state — a succeeded run with a still-working
    // summariser is the truthful picture. The count is the two declared nodes
    // plus the epilogue; `nodes_total`/`nodes_done` above still read 2, which is
    // the D5 split (excluded from counts, never from sight).
    assert_eq!(
        graph_nodes.len(),
        3,
        "expected two run nodes plus the epilogue: {run}"
    );

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
// 3. A node's pane exiting before a valid result fails the node
// ---------------------------------------------------------------------------

/// `04` §4.3: `PaneExited` before a valid result is a node failure once the
/// retry policy is exhausted. The engine only learns this from the runtime, so
/// this scenario is really about the wiring — an unwired pane exit leaves the
/// node `Running` forever, which never pauses, never finishes, and refuses
/// every later `workflow.run` with `workflow_run_in_flight`.
#[test]
fn a_node_whose_pane_exits_without_a_result_fails_and_closes_the_run() {
    // Not a summary test: it starts a second run immediately after the first
    // closes, which lands inside the epilogue's admission window and draws a
    // `workflow_run_in_flight` refusal (§4 D1/M8 — `run.finished` finalises the
    // outcome, not the admission of the next run). Turning the epilogue off is
    // the honest fix: retrying here would test the guard, not the pane-exit
    // failure path this test exists for. The give-up ladder stays pinned by the
    // default-`true` tests.
    let server =
        spawn_workflow_server_with_config("pane-exit", "[workflow]\nsummary_enabled = false\n");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe(&socket);

    let workflow_id = create_workflow(&socket, "pane_exit.toml");
    let run_id = start_run(&socket, &workflow_id, "add dark mode");

    wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_run_started",
        SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );

    let node = poll_until(
        "the node to record its pane's exit",
        SETTLE,
        Duration::from_millis(200),
        || {
            let node = node_get(&socket, &run_id, "doomed");
            (node["status"] == "failed").then_some(node)
        },
    );
    assert!(
        node["evidence"].is_null(),
        "a node that never produced a result records no completion evidence: {node}"
    );
    assert!(
        node["blocker"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("pane exited")),
        "the failure is recorded as the node's succession: {node}"
    );

    // The run has to *close*. A run left live blocks the whole subsystem.
    let status = poll_until(
        "the run to reach a terminal status",
        SETTLE,
        Duration::from_millis(200),
        || {
            let status = run_status(&socket, &run_id);
            matches!(status.as_str(), "failed" | "succeeded" | "cancelled").then_some(status)
        },
    );
    assert_eq!(
        status, "failed",
        "a run with a failed node is not a success"
    );

    let run = run_get(&socket, &run_id);
    assert!(
        run["run"]["ended_at_unix_ms"].is_u64(),
        "a closed run records when it ended: {run}"
    );

    // The proof that the subsystem is not wedged: another run starts.
    let second = request_ok(
        &socket,
        &request(
            "req_run_again",
            "workflow.run",
            json!({ "workflow_id": workflow_id, "args": { "goal": "again" } }),
        ),
    );
    assert_eq!(second["type"], "workflow_run_started", "{second}");

    drain_events(&mut reader, &mut seen, Duration::from_millis(500));
    for finished in events_of_kind(&seen, "workflow_run_finished") {
        assert_ne!(
            finished["data"]["run"]["status"], "succeeded",
            "a run.finished event claimed success: {finished}"
        );
    }

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 4. Steering reaches the node's pane
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

// ---------------------------------------------------------------------------
// 4b. An interrupt is a signal the process can actually observe
// ---------------------------------------------------------------------------

/// `04` §5. The interrupt was being delivered as `agent.send_keys [Escape]`
/// for every runner. A `runner = "command"` node is by construction not a
/// detected agent, so that path answered `agent_not_ready` and the keystroke
/// never reached the PTY — while `workflow.node.interrupt` answered
/// `workflow_node_interrupted` and journalled it. A control surface reporting
/// success for a no-op is worse than one that fails.
///
/// The stub traps SIGINT and prints on receipt, so this asserts the interrupt
/// against the process's own acknowledgement rather than against the response.
#[test]
fn node_interrupt_is_observed_by_the_nodes_process() {
    let server = spawn_workflow_server("interrupt");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe(&socket);

    let workflow_id = create_workflow(&socket, "interruptible.toml");
    let run_id = start_run(&socket, &workflow_id, "add dark mode");

    wait_for_event(&mut reader, &mut seen, "workflow_run_started", SETTLE);

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
    // The trap has to be installed before the interrupt, or the default
    // disposition kills the shell and this proves nothing about delivery.
    poll_until(
        "the stub to install its SIGINT trap",
        SETTLE,
        Duration::from_millis(200),
        || {
            pane_text(&socket, &pane_id)
                .contains("node-stub trapping")
                .then_some(())
        },
    );

    let interrupted = request_ok(
        &socket,
        &request(
            "req_interrupt",
            "workflow.node.interrupt",
            json!({ "run_id": run_id, "path": "worker" }),
        ),
    );
    assert_eq!(
        interrupted["type"], "workflow_node_interrupted",
        "{interrupted}"
    );

    poll_until(
        "the node's process to acknowledge the interrupt",
        Duration::from_secs(20),
        Duration::from_millis(200),
        || {
            pane_text(&socket, &pane_id)
                .contains("node-stub interrupted")
                .then_some(())
        },
    );

    // An interrupt is a delivery, not a completion.
    let node = node_get(&socket, &run_id, "worker");
    assert_eq!(
        node["status"], "running",
        "an interrupt must not complete the node: {node}"
    );
    assert_ne!(run_status(&socket, &run_id), "succeeded");

    drain_events(&mut reader, &mut seen, Duration::from_millis(300));
    server.shutdown();
}

// ---------------------------------------------------------------------------
// 4c. A report with no result.json reaches NeedsAttention through the server
// ---------------------------------------------------------------------------

/// `04` §4.3. `kvx workflow node complete` was parsing `result.json`
/// client-side and exiting before it contacted the server, so a node that
/// could not produce the file never reported at all: it sat `Running` forever
/// with the server never told the node had tried to finish. A
/// `runner = "command"` node has no second completion signal to fall back on,
/// so the report itself has to be what surfaces it.
#[test]
fn a_report_with_no_result_file_reaches_needs_attention() {
    let server = spawn_workflow_server("missing-result");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe(&socket);

    let workflow_id = create_workflow(&socket, "missing_result.toml");
    let run_id = start_run(&socket, &workflow_id, "add dark mode");

    wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_run_started",
        SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );

    let attention = wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_node_updated",
        SETTLE,
        |event| {
            event["data"]["node"]["path"] == "empty"
                && event["data"]["node"]["status"] == "needs_attention"
        },
    );
    assert_eq!(attention["data"]["run_id"], run_id.as_str());

    let node = node_get(&socket, &run_id, "empty");
    assert_eq!(node["status"], "needs_attention", "{node}");
    assert!(
        node["evidence"].is_null(),
        "a node with no result artifact records no completion evidence: {node}"
    );

    // The node dir really has no result — the report, not the file, is what
    // surfaced it.
    let node_dir = node["node_dir"]
        .as_str()
        .unwrap_or_else(|| panic!("a bound node reports its node dir: {node}"));
    assert!(
        !Path::new(node_dir).join("result.json").exists(),
        "the fixture must not have written a result: {node_dir}"
    );

    assert_ne!(
        run_status(&socket, &run_id),
        "succeeded",
        "a run with a node in NeedsAttention never reports success"
    );

    drain_events(&mut reader, &mut seen, Duration::from_millis(300));
    server.shutdown();
}

// ---------------------------------------------------------------------------
// 4d. An agent node's swallowed seed prompt is re-delivered
// ---------------------------------------------------------------------------

/// Where the agent stub records its argv and everything delivered into it.
fn agent_stub_log(base: &Path) -> PathBuf {
    base.join("agent-stub.log")
}

fn read_agent_stub_log(base: &Path) -> String {
    fs::read_to_string(agent_stub_log(base)).unwrap_or_default()
}

/// Lines the stub tagged with `tag`, in order.
fn agent_stub_lines(log: &str, tag: &str) -> Vec<String> {
    let prefix = format!("{tag}\t");
    log.lines()
        .filter_map(|line| line.strip_prefix(prefix.as_str()))
        .map(str::to_string)
        .collect()
}

/// `04` §4.2's seed prompt, end to end against a real server, and the only
/// scenario here that exercises `runner = "agent"`.
///
/// Two defects meet in this one path, and both are server-only — the TUI event
/// loop always did these and the headless loop did not, so neither could be
/// caught by an engine unit test:
///
///   * the seed named `./task.md`, which is relative to the node's *cwd* (the
///     workspace) while `task.md` lives in the *node dir*, so the path did not
///     resolve even when the prompt survived; and
///   * a seed consumed by claude's first-run workspace-trust dialog was never
///     re-delivered, so the node sat `Running` forever with nothing to
///     escalate — the sustained-idle rule counts detector *ticks*, and the
///     server never advanced the engine clock, so the streak never reached
///     three. Reaching the re-delivery then needs the managed agent to leave
///     `Pending`, which the server also never did, so `agent.prompt` answered
///     `agent_not_ready`.
///
/// The `claude` on the server's PATH is `agent_stub.sh`, which swallows its
/// seed and renders what claude's real detection manifest matches. The
/// assertions are on what that *process* received, not on what the API
/// reported: an interrupt or a prompt that is only acknowledged by the control
/// plane is exactly the failure being tested for.
#[test]
fn an_agent_that_never_saw_its_seed_prompt_is_reseeded_with_an_absolute_path() {
    let server = spawn_workflow_server("agent-seed");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let workflow_id = create_workflow(&socket, "agent_seed.toml");
    let run_id = start_run(&socket, &workflow_id, "add dark mode");

    // Wait for the node to be bound to its pane, which is when the stub has
    // been execed and has logged the argv it was given.
    let deadline = Instant::now() + SETTLE;
    let mut node = node_get(&socket, &run_id, "solo");
    while Instant::now() < deadline && node["node_dir"].as_str().is_none() {
        thread::sleep(Duration::from_millis(100));
        node = node_get(&socket, &run_id, "solo");
    }
    let node_dir = node["node_dir"]
        .as_str()
        .unwrap_or_else(|| panic!("the agent node never bound to a pane: {node}"))
        .to_string();
    let task_md = Path::new(&node_dir).join("task.md");
    assert!(
        task_md.exists(),
        "the node dir must carry the task the seed points at: {}",
        task_md.display()
    );

    // The seed prompt karvex actually execed.
    let mut log = read_agent_stub_log(&server.base);
    let seed_deadline = Instant::now() + SETTLE;
    while Instant::now() < seed_deadline && agent_stub_lines(&log, "SEED").is_empty() {
        thread::sleep(Duration::from_millis(100));
        log = read_agent_stub_log(&server.base);
    }
    let seeds = agent_stub_lines(&log, "SEED");
    let seed = seeds
        .first()
        .unwrap_or_else(|| panic!("the agent stub never recorded a seed prompt:\n{log}"));
    assert!(
        seed.contains(&task_md.display().to_string()),
        "the seed prompt must name task.md by absolute path, got {seed:?}"
    );
    assert!(
        !seed.contains("./task.md"),
        "a cwd-relative task.md does not resolve from the node's cwd: {seed:?}"
    );
    assert_eq!(
        agent_stub_lines(&log, "SWALLOWED_SEED").len(),
        0,
        "SWALLOWED_SEED carries no payload and is matched as a bare line"
    );
    assert!(
        log.contains("SWALLOWED_SEED"),
        "the stub must have swallowed its seed for this to be the tested case:\n{log}"
    );

    // The node never worked, so karvex must re-deliver the seed into the pane.
    // This is asserted from the stub's own stdin, not from an API response.
    let redeliver_deadline = Instant::now() + SETTLE;
    while Instant::now() < redeliver_deadline && agent_stub_lines(&log, "STDIN").is_empty() {
        thread::sleep(Duration::from_millis(250));
        log = read_agent_stub_log(&server.base);
    }
    let delivered = agent_stub_lines(&log, "STDIN");
    assert!(
        !delivered.is_empty(),
        "the swallowed seed was never re-delivered; the node hangs forever:\n{log}"
    );
    assert!(
        delivered
            .iter()
            .any(|line| line.contains(&task_md.display().to_string())),
        "the re-delivery must carry the absolute task.md path, got {delivered:?}"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 5. A finished run reads back from the store field-equal to its live projection
// ---------------------------------------------------------------------------

/// A run answered from the journal has to describe the same run the engine
/// described while it was live. It did not: `nodes_done` came back `0` beside
/// `status: "succeeded"`, `graph.edges` came back empty, and the run's
/// workspace binding and every node's `cwd`/`node_dir` were dropped, while
/// `depth` disagreed outright between the two paths.
///
/// The comparison is a whole-projection equality rather than a list of
/// individual field assertions, so a field that starts diverging later fails
/// here even though nobody thought to assert it.
///
/// The compared shape includes each edge's `fired`/`condition_result`: an edge
/// that fired live and reads back unfired is the run telling two different
/// stories about which branch it took.
#[test]
fn a_finished_run_reads_back_field_equal_to_its_live_projection() {
    let server = spawn_workflow_server("restart-fidelity");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    let workspace_id = create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe(&socket);

    let workflow_id = create_workflow(&socket, "two_node_command.toml");
    let run_id = start_run(&socket, &workflow_id, "add dark mode");

    let finished = wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_run_finished",
        SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );
    assert_eq!(
        finished["data"]["run"]["status"], "succeeded",
        "the fixture must reach success before its persistence is compared: {finished}"
    );

    let live = run_get(&socket, &run_id);
    let live_shape = run_shape(&live);

    // The live projection is the reference, so it has to carry the facts the
    // restored one is being checked for — otherwise "equal" could mean "both
    // empty".
    assert_eq!(live["run"]["nodes_done"], 2, "live run: {live}");
    assert_eq!(live["run"]["nodes_total"], 2, "live run: {live}");
    assert_eq!(
        live["run"]["workspace_id"],
        workspace_id.as_str(),
        "live run: {live}"
    );
    assert_eq!(
        live["graph"]["edges"].as_array().map(Vec::len),
        Some(1),
        "live graph: {live}"
    );
    for edge in live["graph"]["edges"].as_array().expect("live edges") {
        assert_eq!(
            (&edge["fired"], &edge["condition_result"]),
            (&json!(true), &json!(true)),
            "the fixture's only edge must have fired live, or comparing the \
             firing state proves nothing: {edge}"
        );
    }
    for node in live["graph"]["nodes"].as_array().expect("live nodes") {
        assert!(
            node["cwd"].is_string() && node["node_dir"].is_string(),
            "a bound node knows where it ran: {node}"
        );
    }

    // Durable writes are queued onto the store thread, which serves its jobs in
    // order; a completed `workflow.run.list` is therefore a barrier proving
    // every write submitted before it has already been applied. Without this
    // the restart could race the last node's write.
    let listed = request_ok(
        &socket,
        &request(
            "req_run_list_barrier",
            "workflow.run.list",
            json!({ "workflow_id": workflow_id }),
        ),
    );
    assert!(
        listed["runs"]
            .as_array()
            .is_some_and(|runs| runs.iter().any(|run| run["run_id"] == run_id.as_str())),
        "the run must be in the store before the restart: {listed}"
    );

    // ── the same run, from a server that never executed it ──────────────────
    let server = server.restart();
    let socket = server.socket().to_path_buf();
    require_workflow_api(&socket);

    let restored = run_get(&socket, &run_id);
    assert_eq!(
        restored["run"]["run_id"],
        run_id.as_str(),
        "the restarted server must answer for the same run: {restored}"
    );
    assert_eq!(
        run_shape(&restored),
        live_shape,
        "a run read back from the store must describe the same run the engine \
         did.\nlive:     {live}\nrestored: {restored}"
    );

    // `ended_at_unix_ms` is inside `run_shape` from Phase 3 on, but it keeps its
    // own assertion because equality alone would also be satisfied by two
    // `null`s: this field used to be stamped twice — once by the app when the
    // run left the live set and once by the store when the queued write was
    // applied — so the same finished run reported two different end times tens
    // of milliseconds apart.
    assert!(
        live["run"]["ended_at_unix_ms"].is_u64(),
        "a finished run has an end time: {live}"
    );
    assert_eq!(
        restored["run"]["ended_at_unix_ms"], live["run"]["ended_at_unix_ms"],
        "the run's close time is stamped once, by the engine that closed it.\n\
         live:     {live}\nrestored: {restored}"
    );

    server.shutdown();
}

/// The comparable shape of a `workflow.run.get` result: the run facts and the
/// per-node/per-edge facts that both the live engine and the store are supposed
/// to know, normalised into a stable order.
///
/// Deliberately excludes what genuinely cannot survive a restart (the pane
/// behind `pane_id` is gone) and what nothing persists yet
/// (`watchdog_interventions`).
///
/// **Phase 3 widened it** (`07-phase3-plan.md` §WS-J scenario 5 / §4 D16). The
/// original shape omitted exactly the field classes the 0.10.2 P1 family hit —
/// timestamps, `growth_limited`, and the instance's own `label` — so a decoder
/// that dropped one of them read back "equal". Every Phase 3 durable field is
/// here too (`transcript_path`, `restored_from`, `context_runs`,
/// `restore_from_run`, `workflow_name`), which is what makes this the e2e face
/// of D16's per-field rule rather than a shape that happens to match.
///
/// **Reserved-namespace nodes are excluded.** The `.summary` epilogue node is a
/// live node at the instant a run reports finished (§4 D1's M8 contract), so
/// comparing it across a restart would compare a snapshot against a moving
/// target. The epilogue's own read-back is asserted where it is deterministic:
/// [`a_restored_and_summarised_run_reads_back_field_equal_to_its_live_projection`],
/// which waits for `workflow.run.summarized` first.
fn run_shape(response: &Value) -> Value {
    let run = &response["run"];
    let mut nodes: Vec<Value> = response["graph"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter(|node| !is_reserved_path(node["path"].as_str().unwrap_or_default()))
        .map(|node| {
            json!({
                "path": node["path"],
                "node_key": node["node_key"],
                "label": node["label"],
                "depth": node["depth"],
                "status": node["status"],
                "demand": node["demand"],
                "model": node["model"],
                "effort": node["effort"],
                "attempt": node["attempt"],
                "cwd": node["cwd"],
                "node_dir": node["node_dir"],
                "evidence": node["evidence"],
                "succession": node["succession"],
                "started_at_unix_ms": node["started_at_unix_ms"],
                "ended_at_unix_ms": node["ended_at_unix_ms"],
                "duration_ms": node["duration_ms"],
                "growth_limited": node["growth_limited"],
                "transcript_path": node["transcript_path"],
                "restored_from": node["restored_from"],
            })
        })
        .collect();
    nodes.sort_by_key(|node| node["path"].as_str().unwrap_or_default().to_string());

    let mut edges: Vec<Value> = response["graph"]["edges"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|edge| {
            json!({
                "from": edge["from"],
                "to": edge["to"],
                "kind": edge["kind"],
                "fired": edge["fired"],
                "condition_result": edge["condition_result"],
            })
        })
        .collect();
    edges.sort_by_key(|edge| {
        format!(
            "{}->{}",
            edge["from"].as_str().unwrap_or_default(),
            edge["to"].as_str().unwrap_or_default()
        )
    });

    json!({
        "status": run["status"],
        "tier": run["tier"],
        "args": run["args"],
        "workspace_id": run["workspace_id"],
        "tab_id": run["tab_id"],
        "nodes_total": run["nodes_total"],
        "nodes_done": run["nodes_done"],
        "started_at_unix_ms": run["started_at_unix_ms"],
        "ended_at_unix_ms": run["ended_at_unix_ms"],
        "growth_limited": run["growth_limited"],
        "workflow_name": run["workflow_name"],
        "context_runs": run["context_runs"],
        "restore_from_run": run["restore_from_run"],
        "nodes": nodes,
        "edges": edges,
    })
}

/// Whether an instance path is engine-owned (`07-phase3-plan.md` §3 rule 3:
/// authored node keys may not begin with `.`, so the reserved namespace is
/// karvex's alone).
fn is_reserved_path(path: &str) -> bool {
    path.starts_with('.')
}

// ---------------------------------------------------------------------------
// 7. An accepted expansion: children are created, inherit the fan-in point,
//    and the downstream node waits for the whole generation
// ---------------------------------------------------------------------------

/// `04-kvdag-and-execution.md` §3.4 / `06-phase2-plan.md` WS-J scenario 1.
///
/// The proposing node asks for two children of a template it is allowed to
/// instantiate. Both are created, both are announced as `workflow.node.created`
/// with the proposer as `parent_path` and `depth: 1` — §4 D5's reason for not
/// adding a second "spawned" event — both run, and the downstream fan-in node
/// starts only once all three of its upstreams have closed.
///
/// The fan-in half is the interesting one: a child inherits a copy of its
/// parent's *outbound* edges, so `collect` acquires two inbound edges it was
/// not authored with, and `propagate` admits it only when every inbound edge
/// has settled. Without inheritance the run would still succeed — `collect`
/// would simply start early — so the assertion is on the edge set and the
/// ordering, not on the status.
///
/// **The payload half is asserted here too**, because a topology that is right
/// while the data underneath it is wrong is the shape the v0.10.0 retest found:
/// `--input` was validated and discarded, the *required* `--label` was
/// discarded, and every inherited edge kept the parent's port so one
/// `inputs/<port>.json` held one contribution and the rest were lost silently.
/// Each of the three now has an assertion on the surface a user or a teammate
/// actually reads — the child's rendered `task.md`, its pane title, and the
/// fan-in node's `inputs/` directory and prompt.
#[test]
fn an_accepted_expansion_creates_children_that_inherit_the_fan_in_point() {
    let server = spawn_workflow_server("expand-accepted");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe_expansion(&socket);

    let workflow_id =
        create_workflow_from_text(&socket, &expand_definition_text(SHARDS, TEMPLATE, 2));
    let run_id = start_run(&socket, &workflow_id, "ship the thing");

    let started = wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_run_started",
        SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );
    assert_eq!(
        started["data"]["run"]["nodes_total"], 2,
        "a template is never materialised as a static node, so the run starts \
         with the proposer and the fan-in node only: {started}"
    );

    let finished = wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_run_finished",
        EXPANSION_SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );
    assert_eq!(
        finished["data"]["run"]["status"], "succeeded",
        "a run whose expansion was wholly accepted must still succeed: {finished}"
    );
    drain_events(&mut reader, &mut seen, Duration::from_millis(500));

    // ── the proposing node's own channel ────────────────────────────────────
    // Two one-child proposals, each with its own label and its own override,
    // so the response names one child each and the numbering continues.
    let mut accepted = Vec::new();
    for shard in 1..=2 {
        let verdict = expand_shard_verdict(&socket, &run_id, shard);
        assert!(
            rejections(&verdict).is_empty(),
            "nothing was refused, so nothing may be reported as refused: {verdict}"
        );
        accepted.extend(accepted_paths(&verdict));
    }
    assert_eq!(
        accepted,
        vec!["fanout/worker/1".to_string(), "fanout/worker/2".to_string()],
        "the response names the children it created, in `<parent>/<template>/<n>` \
         form with `n` 1-based (§3 frozen interface 8)"
    );

    // ── the event stream ────────────────────────────────────────────────────
    assert_eq!(
        created_node_paths(&seen),
        BTreeSet::from([
            "fanout".to_string(),
            "collect".to_string(),
            "fanout/worker/1".to_string(),
            "fanout/worker/2".to_string(),
            // The end-of-run summariser (§4 D1), announced like any node.
            ".summary".to_string(),
        ]),
        "every node that entered the run graph is announced on the one existing \
         event kind; saw {:?}",
        event_kinds(&seen)
    );
    let children = spawned_child_events(&seen);
    assert_eq!(
        children.len(),
        2,
        "both children must be announced as children of the proposer; saw {:?}",
        event_kinds(&seen)
    );
    for event in &children {
        assert_eq!(
            event["data"]["node"]["depth"], 1,
            "static nodes are depth 0, so a first generation child is depth 1 \
             (§4 D13): {event}"
        );
        assert_eq!(
            event["data"]["node"]["node_key"], TEMPLATE,
            "a child's kvdag key is the template it instantiates: {event}"
        );
    }
    assert!(
        events_of_kind(&seen, "workflow_growth_limited").is_empty(),
        "no guardrail was reached, so nothing may claim one was; saw {:?}",
        event_kinds(&seen)
    );

    // ── the run graph, read back over the API ───────────────────────────────
    let run = run_get(&socket, &run_id);
    assert_eq!(run["run"]["status"], "succeeded", "run: {run}");
    assert_eq!(
        run["run"]["nodes_total"], 4,
        "two static nodes plus two children: {run}"
    );
    assert_eq!(
        run["run"]["nodes_live"], 4,
        "`max_nodes` counts every materialised node regardless of status \
         (§4 D12): {run}"
    );

    // Two synthetic `sequence` edges parent -> child, plus the authored data
    // edge, plus one inherited copy of it per child. The inherited pair is what
    // preserves the fan-in point §3.4 requires.
    assert_eq!(
        edge_pairs(&run),
        BTreeSet::from([
            "fanout -> collect".to_string(),
            "fanout -> fanout/worker/1".to_string(),
            "fanout -> fanout/worker/2".to_string(),
            "fanout/worker/1 -> collect".to_string(),
            "fanout/worker/2 -> collect".to_string(),
        ]),
        "a child inherits a copy of its parent's outbound edges (§4 D4): {run}"
    );

    let mut pane_ids = BTreeSet::new();
    let mut upstream_ended = 0_u64;
    for path in ["fanout", "fanout/worker/1", "fanout/worker/2"] {
        let node = node_get(&socket, &run_id, path);
        assert_eq!(node["status"], "succeeded", "node {path}: {node}");
        assert_eq!(
            node["evidence"], "self_report",
            "node {path} completes through its own report: {node}"
        );
        let pane_id = node["pane_id"]
            .as_str()
            .unwrap_or_else(|| panic!("node {path} has no pane binding: {node}"));
        assert!(
            pane_ids.insert(pane_id.to_string()),
            "node {path} reuses another node's pane {pane_id}"
        );
        let ended = node["ended_at_unix_ms"]
            .as_u64()
            .unwrap_or_else(|| panic!("node {path} has no end time: {node}"));
        upstream_ended = upstream_ended.max(ended);
    }
    assert_eq!(
        pane_ids.len(),
        3,
        "each child is a visible teammate in its own pane"
    );

    let collect = node_get(&socket, &run_id, "collect");
    assert_eq!(collect["status"], "succeeded", "collect: {collect}");
    let collect_started = collect["started_at_unix_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("collect never started: {collect}"));
    assert!(
        collect_started >= upstream_ended,
        "the fan-in node must wait for the whole generation, not just for its \
         authored upstream: it started at {collect_started} and its last \
         upstream closed at {upstream_ended}"
    );

    // ── P0 1: the accepted `--input` reaches the child's rendered prompt ─────
    let first_task = task_markdown(&socket, &run_id, "fanout/worker/1");
    let second_task = task_markdown(&socket, &run_id, "fanout/worker/2");
    assert!(
        first_task.contains("Work one shard of: shard-1"),
        "the child's `{{goal}}` slot must be filled from its proposal's \
         `--input goal=shard-1`, not from the run argument: {first_task}"
    );
    assert!(
        second_task.contains("Work one shard of: shard-2"),
        "{second_task}"
    );
    for task in [&first_task, &second_task] {
        assert!(
            !task.contains("ship the thing"),
            "the run argument must not survive the child's own override: {task}"
        );
    }
    assert_ne!(
        first_task, second_task,
        "a proposing node that cannot give its children different work has no fan-out"
    );

    // ── P0 2: the required `--label` is the child's name ─────────────────────
    assert!(
        first_task.starts_with("# Shard 1\n"),
        "a child's task.md is titled with its own label: {first_task}"
    );
    assert!(second_task.starts_with("# Shard 2\n"), "{second_task}");
    let labels = pane_labels(&socket);
    let pane_label_of = |path: &str| -> String {
        let pane = node_get(&socket, &run_id, path)["pane_id"]
            .as_str()
            .unwrap_or_else(|| panic!("node {path} has no pane"))
            .to_string();
        labels
            .get(&pane)
            .unwrap_or_else(|| panic!("pane {pane} of {path} has no label: {labels:?}"))
            .clone()
    };
    let first_pane = pane_label_of("fanout/worker/1");
    let second_pane = pane_label_of("fanout/worker/2");
    assert!(
        first_pane.ends_with("Shard 1") && second_pane.ends_with("Shard 2"),
        "each child's pane is titled with its own label, not the template's: \
         {first_pane:?} / {second_pane:?}"
    );
    assert_ne!(first_pane, second_pane);

    // A node's name is a shared runtime fact, so it has to be readable over
    // the JSON API and not only through the TUI's private mirror — otherwise
    // `run show`/`node show` name a whole generation `worker` while the DAG
    // names them apart.
    let named: Vec<String> = ["fanout/worker/1", "fanout/worker/2"]
        .into_iter()
        .map(|path| node_get(&socket, &run_id, path)["label"].to_string())
        .collect();
    assert_eq!(
        named,
        vec!["\"Shard 1\"".to_string(), "\"Shard 2\"".to_string()],
        "workflow.node.get must carry each child's own label"
    );
    let run_nodes = run_get(&socket, &run_id);
    let listed: BTreeSet<String> = run_nodes["graph"]["nodes"]
        .as_array()
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|node| Some(node["label"].as_str()?.to_string()))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        listed.contains("Shard 1") && listed.contains("Shard 2"),
        "workflow.run.get lists every node's label too: {listed:?}"
    );

    // ── P0 3: every inherited edge's payload reaches the fan-in node ─────────
    let collect_dir = PathBuf::from(node_dir_of(&socket, &run_id, "collect"));
    let contributors: BTreeSet<String> = fs::read_dir(collect_dir.join("inputs").join("shard"))
        .unwrap_or_else(|err| {
            panic!(
                "the fanned-in port has no per-contributor directory at {}: {err}",
                collect_dir.join("inputs").join("shard").display()
            )
        })
        .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
        .collect();
    assert_eq!(
        contributors,
        BTreeSet::from([
            "fanout.json".to_string(),
            "fanout-worker-1.json".to_string(),
            "fanout-worker-2.json".to_string(),
        ]),
        "all three upstreams fired into the one `shard` port; one file each, and \
         no contribution may overwrite another"
    );
    let index: Value = serde_json::from_str(
        &fs::read_to_string(collect_dir.join("inputs").join("shard.json"))
            .expect("the port's own file"),
    )
    .expect("the port index is JSON");
    assert_eq!(
        index.as_array().map(Vec::len),
        Some(3),
        "`inputs/<port>.json` indexes the whole generation: {index}"
    );

    let collect_task = task_markdown(&socket, &run_id, "collect");
    for expected in [
        "fanout/worker/1 reporting on shard-1",
        "fanout/worker/2 reporting on shard-2",
    ] {
        assert!(
            collect_task.contains(expected),
            "the fan-in prompt must carry every child's result, not the last \
             writer's: {expected} missing from {collect_task}"
        );
    }
    assert!(
        collect_task.contains("[from fanout/worker/1 · Shard 1]"),
        "each contribution is attributed to the node that produced it: {collect_task}"
    );
    // A node's cwd is the workspace directory, not its node directory, so a
    // `./result.json` in `task.md` named a file in the workspace: the node did
    // the work, wrote its result where nothing was watching, and failed its
    // completion gate. Every karvex-owned path the document names is absolute.
    assert!(
        !collect_task.contains("`./"),
        "task.md may not name a node file relative to the node's cwd: {collect_task}"
    );
    for file in ["result.json", "output_schema.json"] {
        let expected = collect_dir.join(file);
        assert!(
            collect_task.contains(&expected.display().to_string()),
            "task.md must name {} by its absolute path: {collect_task}",
            expected.display()
        );
    }
    assert!(
        collect_task.contains(
            &collect_dir
                .join("inputs")
                .join("shard.json")
                .display()
                .to_string()
        ),
        "and the inputs it lists too: {collect_task}"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 8. A truncated expansion is surfaced — the phase's headline guarantee
// ---------------------------------------------------------------------------

/// `06-phase2-plan.md` WS-J scenario 2, §4 D2, §5 R-6.
///
/// A `--tier low` run narrows `max_nodes` from the version's 24 to 12
/// (`narrow_growth`), leaving room for ten children beside the two static
/// nodes. The proposal asks for twelve. Accept-all would violate the ceiling
/// and reject-all would waste the budget, so ten are created and the shortfall
/// is **reported** — that reporting is the guarantee, and it is asserted here
/// end to end rather than in a unit test because the surfaces it has to reach
/// are the event stream, the run projection, and the proposing node's own
/// response.
///
/// **`growth_limited` on the run and on the node is the assertion that is
/// currently unmet**, and it is left in rather than softened. §4 D11 puts the
/// guarantee on three surfaces so that it does not rest on the one that is
/// off by default, and two of the three — the DAG banner and `kvx workflow run
/// show` — read the run's `growth_limited` rather than the event stream. With
/// it permanently `None` the guarantee is carried by the event alone, which is
/// exactly the single-channel failure §5 R-6 exists to prevent. Both
/// projections say the other one owns it: `src/app/api/workflows.rs`'s
/// `wire_run_record` defers to "the live projection in `src/app/workflow.rs`",
/// and `App::workflow_run_info` / `App::workflow_node_info` in that file
/// hardcode `growth_limited: None` under a comment deferring back to WS-E. The
/// limit is already in hand where it needs to be recorded: the
/// `WorkflowEvent::GrowthLimited` arm of `App::emit_workflow_event` builds the
/// whole `WorkflowGrowthLimit` a line above, next to the `show_workflow_notice`
/// call.
#[test]
fn a_truncated_expansion_is_surfaced_and_the_run_still_succeeds() {
    /// The version ceiling narrowed by `low`.
    const LOW_TIER_MAX_NODES: u64 = 12;
    /// `fanout` and `collect`.
    const STATIC_NODES: u64 = 2;
    const REQUESTED: u16 = 12;
    const EXPECTED_ACCEPTED: u64 = LOW_TIER_MAX_NODES - STATIC_NODES;

    let server = spawn_workflow_server("expand-truncated");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe_expansion(&socket);

    let workflow_id =
        create_workflow_from_text(&socket, &expand_definition_text(BATCH, TEMPLATE, REQUESTED));
    let run_id = start_run_at_tier(&socket, &workflow_id, "shard the corpus", Some("low"));

    // ── the one event no client can derive ──────────────────────────────────
    let limited = wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_growth_limited",
        EXPANSION_SETTLE,
        |event| event["data"]["run_id"] == run_id.as_str(),
    );
    assert_eq!(
        limited["data"]["path"], PROPOSER,
        "the event names the node that proposed, not the children: {limited}"
    );
    assert_eq!(
        limited["data"]["template"], TEMPLATE,
        "the event names the template that was truncated: {limited}"
    );
    assert_eq!(
        limited["data"]["limit"], "max_nodes",
        "the run-level ceiling is what ran out here; the node's own expand_max \
         is deliberately set above it in the fixture so the reported cause is \
         unambiguous: {limited}"
    );
    assert_eq!(
        limited["data"]["limit_value"],
        json!(LOW_TIER_MAX_NODES),
        "the exact ceiling is on the event so no reader has to look it up: \
         {limited}"
    );
    assert_eq!(
        limited["data"]["requested"],
        json!(REQUESTED),
        "the event reports what was asked for: {limited}"
    );
    assert_eq!(
        limited["data"]["accepted"],
        json!(EXPECTED_ACCEPTED),
        "and how many of it fit — a shortfall reported is what makes partial \
         acceptance legitimate (§4 D2): {limited}"
    );
    assert!(
        limited["data"]["message"]
            .as_str()
            .is_some_and(|message| !message.trim().is_empty()),
        "the event carries a human-readable reason: {limited}"
    );

    let finished = wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_run_finished",
        EXPANSION_SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );
    assert_eq!(
        finished["data"]["run"]["status"], "succeeded",
        "a truncated expansion is a reported shortfall, not a failure: {finished}"
    );
    drain_events(&mut reader, &mut seen, Duration::from_millis(500));

    // ── the proposing node's own channel ────────────────────────────────────
    let verdict = expand_verdict(&socket, &run_id);
    assert_eq!(
        accepted_paths(&verdict).len() as u64,
        EXPECTED_ACCEPTED,
        "some children are created — reject-all would waste the budget: {verdict}"
    );
    let rejected = rejections(&verdict);
    assert_eq!(
        rejected.len(),
        1,
        "one proposal produced one shortfall: {verdict}"
    );
    let truncation = &rejected[0];
    assert_eq!(truncation["reason"], "truncated", "{truncation}");
    assert_eq!(truncation["template"], TEMPLATE, "{truncation}");
    assert_eq!(truncation["requested"], json!(REQUESTED), "{truncation}");
    assert_eq!(
        truncation["accepted"],
        json!(EXPECTED_ACCEPTED),
        "{truncation}"
    );
    assert_eq!(truncation["limit"]["kind"], "max_nodes", "{truncation}");
    assert_eq!(
        truncation["limit"]["limit_value"],
        json!(LOW_TIER_MAX_NODES),
        "{truncation}"
    );

    // ── the run projection ──────────────────────────────────────────────────
    let run = run_get(&socket, &run_id);
    assert_eq!(
        run["run"]["tier"], "low",
        "the run really did start at the narrowing tier: {run}"
    );
    assert_eq!(
        run["run"]["max_nodes"],
        json!(LOW_TIER_MAX_NODES),
        "one authority: what the run graph enforced is what the run reports, \
         narrowed from the version's 24 (§5 R-3): {run}"
    );
    assert_eq!(
        run["run"]["nodes_total"],
        json!(LOW_TIER_MAX_NODES),
        "the budget is exactly exhausted, never exceeded: {run}"
    );
    assert_eq!(
        run["run"]["nodes_live"],
        json!(LOW_TIER_MAX_NODES),
        "`nodes_live` counts every materialised node regardless of status: {run}"
    );

    let children = spawned_child_events(&seen);
    assert_eq!(
        children.len() as u64,
        EXPECTED_ACCEPTED,
        "every child that was created is announced; saw {:?}",
        event_kinds(&seen)
    );

    // Read now, asserted after the shutdown below. This run holds a whole
    // generation of panes, and a panic here would skip `shutdown` and leave
    // them — and their server — alive for however long the harness watchdog
    // takes to notice, which is long enough to starve the sibling tests this
    // file runs in parallel with. A failing assertion has to fail this test
    // only.
    let run_growth = run["run"]["growth_limited"].clone();
    let node_growth = node_get(&socket, &run_id, PROPOSER)["growth_limited"].clone();

    server.shutdown();

    // §4 D11 — the guarantee lands on the API as a durable *fact* about the run
    // and about the node that ran into the ceiling, not only as a transient
    // event. Two of the three non-optional surfaces (the DAG banner and
    // `kvx workflow run show`) read these fields and never the event stream, so
    // without them the "always surfaced" guarantee rests on one channel again.
    // See this test's doc comment for where the projection is missing.
    assert_eq!(
        run_growth["kind"], "max_nodes",
        "the run must report the limit it hit: {run_growth}"
    );
    assert_eq!(
        run_growth["limit_value"],
        json!(LOW_TIER_MAX_NODES),
        "{run_growth}"
    );
    assert_eq!(run_growth["requested"], json!(REQUESTED), "{run_growth}");
    assert_eq!(
        run_growth["accepted"],
        json!(EXPECTED_ACCEPTED),
        "{run_growth}"
    );
    assert_eq!(
        node_growth["kind"], "max_nodes",
        "the limit is also a fact about the node that proposed, so the DAG can \
         badge that node without replaying the stream: {node_growth}"
    );
}

// ---------------------------------------------------------------------------
// 9. A disallowed template is refused, and refusing it changes nothing
// ---------------------------------------------------------------------------

/// `06-phase2-plan.md` WS-J scenario 3.
///
/// The proposal names a template that exists and is a real template, but is not
/// in the proposing node's `expand_allow`. It is refused, no node is created,
/// the run graph is byte-for-byte the graph it started with, and the run still
/// succeeds — a node being wrong about what it may instantiate is not a run
/// failure.
///
/// **Drift from the plan's bullet, and why the code is right.** WS-J scenario 3
/// as written also expects a `workflow.growth.limited` event. §4 D5 — and the
/// engine that implements it (`engine/mod.rs`, the `ExpandProposed` arm) —
/// says the opposite in as many words: that event is emitted for "exactly the
/// rejections a guardrail produced", and "a validation refusal — unknown
/// template, not allowed, unknown input — is the node being wrong, not the run
/// running out of room". The two statements are in the same document and the
/// decision is the more specific of them, so this asserts the *absence* of the
/// event. Emitting it here would tell every client the run had hit a ceiling it
/// has not come near, and `run.growth_limited` would then be permanently set on
/// a run that never lost a single node to a guardrail.
#[test]
fn a_disallowed_template_is_refused_and_creates_nothing() {
    let server = spawn_workflow_server("expand-refused");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe_expansion(&socket);

    let workflow_id = create_workflow_from_text(
        &socket,
        &expand_definition_text(BATCH, DISALLOWED_TEMPLATE, 2),
    );
    let run_id = start_run(&socket, &workflow_id, "try to escape the allowlist");

    let started = wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_run_started",
        SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );
    let nodes_at_start = started["data"]["run"]["nodes_total"].clone();
    assert_eq!(nodes_at_start, json!(2), "started: {started}");

    let finished = wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_run_finished",
        EXPANSION_SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );
    assert_eq!(
        finished["data"]["run"]["status"], "succeeded",
        "a refused proposal is the node being wrong, not the run failing: \
         {finished}"
    );
    drain_events(&mut reader, &mut seen, Duration::from_millis(500));

    // ── the refusal reached the node that proposed ──────────────────────────
    let verdict = expand_verdict(&socket, &run_id);
    assert!(
        accepted_paths(&verdict).is_empty(),
        "a refused proposal creates nothing: {verdict}"
    );
    let rejected = rejections(&verdict);
    assert_eq!(rejected.len(), 1, "one refusal: {verdict}");
    assert_eq!(
        rejected[0]["reason"], "not_allowed",
        "the reason names the allowlist, not a guardrail: {verdict}"
    );
    assert_eq!(
        rejected[0]["template"], DISALLOWED_TEMPLATE,
        "the refusal names the template that was asked for: {verdict}"
    );
    assert!(
        rejected[0]["limit"].is_null(),
        "no guardrail was reached, so there is no limit to report: {verdict}"
    );
    assert!(
        rejected[0]["message"]
            .as_str()
            .is_some_and(|message| message.contains("expand_allow")),
        "the message says what to change: {verdict}"
    );

    // ── and changed nothing else ────────────────────────────────────────────
    assert_eq!(
        created_node_paths(&seen),
        // The refused proposal created nothing; `.summary` is the epilogue,
        // which every finished run gets (§4 D1).
        BTreeSet::from([
            "fanout".to_string(),
            "collect".to_string(),
            ".summary".to_string(),
        ]),
        "no node entered the run graph; saw {:?}",
        event_kinds(&seen)
    );
    assert!(
        spawned_child_events(&seen).is_empty(),
        "the proposer has no children; saw {:?}",
        event_kinds(&seen)
    );
    assert!(
        events_of_kind(&seen, "workflow_growth_limited").is_empty(),
        "a validation refusal is not a guardrail; see this test's doc comment. \
         Saw {:?}",
        event_kinds(&seen)
    );

    let run = run_get(&socket, &run_id);
    assert_eq!(
        run["run"]["nodes_total"], nodes_at_start,
        "`nodes_total` is unchanged by a refused proposal: {run}"
    );
    assert_eq!(run["run"]["nodes_live"], json!(2), "run: {run}");
    assert!(
        run["run"]["growth_limited"].is_null(),
        "the run never ran out of room, so nothing may say it did: {run}"
    );
    assert_eq!(
        edge_pairs(&run),
        BTreeSet::from(["fanout -> collect".to_string()]),
        "the graph is the one the author drew: {run}"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 10. The event stream's causal ordering guarantee
// ---------------------------------------------------------------------------

/// The P1 regression, end to end, over the surface it broke on.
///
/// A stub expansion run finishes in well under a second, so every event of the
/// run is queued on the hub before the subscription's poll loop next wakes.
/// That backlog is exactly the condition under which per-type cursors used to
/// deliver `workflow.run.finished` ahead of the `workflow.node.created` events
/// it summarises, and a node's `workflow.node.output_checkpoint` ahead of that
/// node's own creation — an event-driven client cache saw a finished run before
/// its children existed. `06-phase2-plan.md:229` states the write-ordering
/// invariant ("RunNodeCreated before any RunNode update for the same path");
/// this asserts the *delivery* order a client actually observes.
///
/// Written against stream position on purpose. Every other scenario in this
/// file deliberately asserts from payloads instead, because payload assertions
/// survive a reordering — which is precisely why none of them caught this.
#[test]
fn event_stream_delivers_cross_type_events_in_causal_order() {
    let server = spawn_workflow_server("eorder");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe_all_workflow_events(&socket);

    let workflow_id =
        create_workflow_from_text(&socket, &expand_definition_text(SHARDS, TEMPLATE, 3));
    let run_id = start_run(&socket, &workflow_id, "ship the thing");

    let started = wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_run_started",
        SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );
    let nodes_at_start = started["data"]["run"]["nodes_total"]
        .as_u64()
        .unwrap_or_else(|| panic!("run.started carries no nodes_total: {started}"));

    wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_run_finished",
        EXPANSION_SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );
    // Anything still queued behind the finish belongs to the same run and has
    // to be counted, or a stream that delivered the finish first would look
    // ordered simply because the test stopped reading.
    drain_events(&mut reader, &mut seen, Duration::from_secs(2));

    let kinds = event_kinds(&seen);
    let created_at: std::collections::BTreeMap<String, usize> = seen
        .iter()
        .enumerate()
        .filter(|(_, event)| event["event"] == "workflow_node_created")
        .filter_map(|(index, event)| Some((event_node_path(event)?, index)))
        .fold(
            std::collections::BTreeMap::new(),
            |mut acc, (path, index)| {
                acc.entry(path).or_insert(index);
                acc
            },
        );
    assert!(
        created_at.len() >= 5,
        "two static nodes plus three children must each be announced; saw {kinds:?}"
    );

    // ── a node is created before anything else refers to it ─────────────────
    for (index, event) in seen.iter().enumerate() {
        let kind = event["event"].as_str().unwrap_or_default();
        if kind != "workflow_node_updated" && kind != "workflow_node_output_checkpoint" {
            continue;
        }
        let Some(path) = event_node_path(event) else {
            continue;
        };
        let created = created_at.get(&path).copied().unwrap_or_else(|| {
            panic!("{kind} for {path} with no workflow_node_created at all; saw {kinds:?}")
        });
        assert!(
            created < index,
            "{kind} for {path} arrived at line {index}, before that node's \
             workflow_node_created at line {created}; saw {kinds:?}"
        );
    }

    // ── the run finishes after every node event of the run ──────────────────
    let finished_at = seen
        .iter()
        .position(|event| event["event"] == "workflow_run_finished")
        .unwrap_or_else(|| panic!("no workflow_run_finished on the stream; saw {kinds:?}"));
    // Same D1/M8 narrowing as the `last_node_event` scan below, at the second
    // site the invariant is expressed: reserved-namespace nodes legitimately
    // appear *after* `run_finished`, because the epilogue is created by `finish`
    // itself. Every node the author declared still has to exist before the run
    // reports finished — that is the client-cache guarantee this protects.
    for (path, created) in created_at.iter().filter(|(path, _)| !path.starts_with('.')) {
        assert!(
            *created < finished_at,
            "workflow_run_finished arrived at line {finished_at}, before \
             workflow_node_created for {path} at line {created} — a client cache \
             would see a finished run whose children do not exist yet; saw {kinds:?}"
        );
    }
    // Reserved-namespace events legitimately follow `run_finished` by D1/M8
    // design — this narrowing is intent, not accident.
    //
    // Scoped to the **user graph**. `workflow_run_finished` is still the last
    // word on the run's own work — that is what this invariant protects — but
    // §4 D1 deliberately runs the engine-owned summariser *after* the run's
    // terminal status is decided, and M8 requires the DAG to show it live, so
    // `.summary`'s own node events must be able to follow it. Excluding the
    // reserved namespace keeps the original assertion exactly as strong for
    // every node the author declared, rather than loosening it for all of them.
    let last_node_event = seen
        .iter()
        .rposition(|event| {
            matches!(
                event["event"].as_str(),
                Some(
                    "workflow_node_created"
                        | "workflow_node_updated"
                        | "workflow_node_output_checkpoint"
                )
            ) && event_node_path(event).is_none_or(|path| !path.starts_with('.'))
        })
        .unwrap_or_else(|| panic!("no node events at all; saw {kinds:?}"));
    assert!(
        last_node_event < finished_at,
        "workflow_run_finished must be the last word on the run's declared work, \
         but a user-node event followed it at line {last_node_event}; saw {kinds:?}"
    );

    // ── the run's growth is trackable from the run stream alone ─────────────
    let grew = seen[..finished_at]
        .iter()
        .filter(|event| event["event"] == "workflow_run_updated")
        .filter_map(|event| event["data"]["run"]["nodes_total"].as_u64())
        .max()
        .unwrap_or_else(|| {
            panic!(
                "no workflow.run.updated before the run finished, so a subscriber \
                 cannot track nodes_total growth from run events; saw {kinds:?}"
            )
        });
    assert!(
        grew > nodes_at_start,
        "the run grew from {nodes_at_start} nodes, but no workflow.run.updated \
         reported more than {grew}; saw {kinds:?}"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// Phase 3 (`07-phase3-plan.md` WS-J): end-of-run summaries, prior-run context,
// restore, pruned history, and the widened restart-fidelity projection
// ---------------------------------------------------------------------------

/// The argv `KARVEX_WORKFLOW_SUMMARY_COMMAND` takes: a JSON array of strings
/// (§4 D2). Points the epilogue at `summary_stub.sh` in the given mode.
fn summary_command_argv(mode: &str) -> String {
    let stub = fixture_dir().join("summary_stub.sh");
    assert!(stub.exists(), "missing stub script at {}", stub.display());
    serde_json::to_string(&["/bin/sh", &stub.to_string_lossy(), mode])
        .expect("argv is serialisable")
}

/// A server whose epilogue is bound to [`summary_command_argv`] and whose
/// `[workflow]` block carries `extra_config`.
///
/// Summaries are left **on** here — that is the whole point of these scenarios —
/// which is why every one of them waits for `workflow.run.summarized` before
/// starting the next run: `run.finished` finalises the run's *outcome*, not the
/// admission of the next run, and starting one inside the epilogue window is
/// answered with `workflow_run_in_flight` (§4 D1/M7, and the E-12 contract the
/// docs state).
fn spawn_summary_server(label: &str, mode: &str, extra_config: &str) -> WorkflowServer {
    spawn_workflow_server_with_env(
        label,
        extra_config,
        &[(
            "KARVEX_WORKFLOW_SUMMARY_COMMAND",
            &summary_command_argv(mode),
        )],
    )
}

/// [`subscribe`] plus the Phase 3 event kind. Separate from `subscribe` so the
/// Phase 1 scenarios keep the exact stream they were written against.
fn subscribe_phase3(socket: &Path) -> (JsonLineReader, Vec<Value>) {
    let mut reader = open_subscription(
        socket,
        &request(
            "sub_phase3",
            "events.subscribe",
            json!({
                "subscriptions": [
                    { "type": "workflow.run.started" },
                    { "type": "workflow.run.updated" },
                    { "type": "workflow.run.finished" },
                    { "type": "workflow.run.summarized" },
                    { "type": "workflow.node.created" },
                    { "type": "workflow.node.updated" },
                    { "type": "pane.created" },
                ]
            }),
        ),
    );
    let ack = reader.read_json_line(Duration::from_secs(5));
    assert_eq!(ack["id"], "sub_phase3", "unexpected subscribe ack: {ack}");
    assert_eq!(
        ack["result"]["type"], "subscription_started",
        "subscribe was rejected: {ack}"
    );
    (reader, Vec::new())
}

/// Blocks until `run_id` reports finished, and returns the event.
fn wait_for_run_finished(
    reader: &mut JsonLineReader,
    seen: &mut Vec<Value>,
    run_id: &str,
) -> Value {
    wait_for_event_matching(reader, seen, "workflow_run_finished", SETTLE, |event| {
        event["data"]["run"]["run_id"] == run_id
    })
}

/// Blocks until `run_id`'s summary lands. Event-driven rather than polled: the
/// epilogue runs after the run's terminal status, so there is no other moment a
/// caller can name.
fn wait_for_run_summarized(
    reader: &mut JsonLineReader,
    seen: &mut Vec<Value>,
    run_id: &str,
) -> Value {
    wait_for_event_matching(reader, seen, "workflow_run_summarized", SETTLE, |event| {
        event["data"]["run_id"] == run_id
    })
}

/// Stream position of the first event of `kind` that is about `run_id`.
fn position_of_run_event(events: &[Value], kind: &str, run_id: &str) -> usize {
    events
        .iter()
        .position(|event| {
            event["event"] == kind
                && (event["data"]["run"]["run_id"] == run_id || event["data"]["run_id"] == run_id)
        })
        .unwrap_or_else(|| panic!("missing {kind} for {run_id}; saw {:?}", event_kinds(events)))
}

/// `workflow.summary.get`'s answer. `Value::Null` is the normal "no summary
/// was written" answer, never an error (§4 D1).
fn summary_get(socket: &Path, run_id: &str) -> Value {
    let result = request_ok(
        socket,
        &request(
            "req_summary_get",
            "workflow.summary.get",
            json!({ "run_id": run_id }),
        ),
    );
    result["summary"].clone()
}

fn summary_list(socket: &Path, workflow_id: &str) -> Vec<Value> {
    let result = request_ok(
        socket,
        &request(
            "req_summary_list",
            "workflow.summary.list",
            json!({ "workflow_id": workflow_id }),
        ),
    );
    result["summaries"]
        .as_array()
        .unwrap_or_else(|| panic!("workflow.summary.list returned no summaries: {result}"))
        .clone()
}

/// `workflow.run` with explicit params, returning the whole result so a caller
/// can read the restore report off it.
fn start_run_with(socket: &Path, params: Value) -> Value {
    let result = request_ok(socket, &request("req_run_params", "workflow.run", params));
    assert_eq!(
        result["type"], "workflow_run_started",
        "unexpected: {result}"
    );
    result
}

fn run_id_of(started: &Value) -> String {
    started["run"]["run_id"]
        .as_str()
        .unwrap_or_else(|| panic!("workflow.run returned no run_id: {started}"))
        .to_string()
}

/// The restore fixture with its stub path and `plan`'s prompt substituted.
/// The prompt is a parameter because §4 D11's compatibility gate is
/// `sha256(prompt_template)`: changing it is what makes a v2 incompatible.
fn restore_definition_text(plan_prompt: &str) -> String {
    let stub = fixture_dir().join("restore_stub.sh");
    assert!(stub.exists(), "missing stub script at {}", stub.display());
    let path = fixture_dir().join("restore.toml");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let replaced = text
        .replace("@RESTORE_STUB@", &stub.to_string_lossy())
        .replace("@PLAN_PROMPT@", plan_prompt);
    for placeholder in ["@RESTORE_STUB@", "@PLAN_PROMPT@"] {
        assert!(
            !replaced.contains(placeholder),
            "unresolved {placeholder} in restore.toml"
        );
    }
    replaced
}

/// How many panes exist right now. Restore's headline claim is that a restored
/// node never spawns one, and a count is how that is proven against a runtime
/// where every other node does.
fn pane_count(socket: &Path) -> usize {
    let result = request_ok(socket, &request("req_pane_count", "pane.list", json!({})));
    result["panes"]
        .as_array()
        .unwrap_or_else(|| panic!("pane.list returned no panes: {result}"))
        .len()
}

/// The absolute `context/prior-runs.md` path a rendered `task.md` points at
/// (§4 D21: the section is two lines — the path in backticks, and permission to
/// ignore it).
fn prior_runs_pointer(task_md: &str) -> Option<String> {
    let section = task_md.split("## Prior runs\n").nth(1)?;
    let line = section.lines().find(|line| line.starts_with("- `"))?;
    line.trim_start_matches("- `")
        .strip_suffix('`')
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// 11. The summary lifecycle: finished, then summarised, then read by the next
//     run
// ---------------------------------------------------------------------------

/// `07-phase3-plan.md` §WS-J scenario 1 — the phase's headline promise as one
/// end-to-end story: **a run leaves something behind, and the next run reads
/// it.**
///
/// Four claims, in the order a user meets them:
///
///   * `workflow.run.finished` arrives **before** `workflow.run.summarized`.
///     The 0.10.2 ordering fix made one connection's stream globally causal, so
///     stream position is the assertion — and this ordering is the client
///     contract §4 D1's M8 states: `run.finished` is the durable terminal
///     signal, `run.summarized` is the follow-up for the summary's content.
///   * `workflow.summary.get` answers with what the summariser wrote, field for
///     field, and names the epilogue node that generated it.
///   * a second run of the same workflow records run 1 in `context_runs` and
///     every node's `task.md` points at `context/prior-runs.md` (§4 D21), which
///     really carries run 1's summary.
///   * a third run started with `include_prior_summaries: false` has neither —
///     the opt-out is a property of the run, not of the workflow.
#[test]
fn a_finished_run_is_summarised_afterwards_and_the_next_run_reads_its_summary() {
    let server = spawn_summary_server("summary-lifecycle", "ok", "");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe_phase3(&socket);

    let workflow_id = create_workflow(&socket, "two_node_command.toml");

    // ── run 1: finished, then summarised ────────────────────────────────────
    let run1 = start_run(&socket, &workflow_id, "add dark mode");
    let finished = wait_for_run_finished(&mut reader, &mut seen, &run1);
    assert_eq!(
        finished["data"]["run"]["status"], "succeeded",
        "the fixture must succeed before its summary is asserted: {finished}"
    );
    let summarized = wait_for_run_summarized(&mut reader, &mut seen, &run1);

    assert!(
        position_of_run_event(&seen, "workflow_run_finished", &run1)
            < position_of_run_event(&seen, "workflow_run_summarized", &run1),
        "the summary is an epilogue: workflow.run.finished is the terminal \
         signal and workflow.run.summarized follows it (§4 D1/M8); saw {:?}",
        event_kinds(&seen)
    );

    // The run's own status is untouched by the epilogue.
    assert_eq!(run_status(&socket, &run1), "succeeded");

    // ── what the summariser wrote is what the reader gets ───────────────────
    let expected_outcome = format!("stub summary of {run1}");
    let summary = summary_get(&socket, &run1);
    assert_eq!(summary["run_id"], run1.as_str(), "{summary}");
    assert_eq!(summary["workflow_id"], workflow_id.as_str(), "{summary}");
    assert_eq!(
        summary["text"], "the stub summariser saw this run reach its terminal status",
        "{summary}"
    );
    assert_eq!(summary["outcome"], expected_outcome.as_str(), "{summary}");
    assert_eq!(
        summary["highlights"],
        json!(["the stub summariser ran"]),
        "{summary}"
    );
    assert_eq!(summary["open_gaps"], json!([]), "{summary}");
    assert_eq!(summary["per_node"], json!([]), "{summary}");
    assert_eq!(summary["run_pruned"], false, "{summary}");
    assert_eq!(
        summary["generated_by_path"], ".summary",
        "the summary names the epilogue node that produced it (§4 D5): {summary}"
    );
    // The event and the read method describe one summary, not two.
    assert_eq!(
        summarized["data"]["summary"], summary,
        "workflow.run.summarized must carry exactly what workflow.summary.get \
         answers: {summarized}"
    );

    // The epilogue node is visible in the graph and excluded from the counts
    // (§4 D5) — the two halves of "excluded from counts, never from sight".
    let run1_get = run_get(&socket, &run1);
    assert_eq!(run1_get["run"]["nodes_total"], 2, "{run1_get}");
    assert_eq!(run1_get["run"]["nodes_done"], 2, "{run1_get}");
    let epilogue = node_get(&socket, &run1, ".summary");
    assert_eq!(epilogue["status"], "succeeded", "{epilogue}");
    assert_eq!(epilogue["evidence"], "self_report", "{epilogue}");

    // ── run 2 reads run 1's summary ─────────────────────────────────────────
    let run2 = start_run(&socket, &workflow_id, "add light mode");
    wait_for_run_finished(&mut reader, &mut seen, &run2);

    let run2_get = run_get(&socket, &run2);
    assert_eq!(
        run2_get["run"]["context_runs"],
        json!([run1]),
        "run 2 must record exactly the runs whose summaries it was offered: \
         {run2_get}"
    );

    let task = task_markdown(&socket, &run2, "plan");
    let pointer = prior_runs_pointer(&task)
        .unwrap_or_else(|| panic!("run 2's task.md carries no `## Prior runs` pointer:\n{task}"));
    let prior_runs = fs::read_to_string(&pointer)
        .unwrap_or_else(|err| panic!("the pointer names {pointer}, which is unreadable: {err}"));
    assert!(
        prior_runs.contains(&expected_outcome),
        "context/prior-runs.md must carry run 1's summary:\n{prior_runs}"
    );

    wait_for_run_summarized(&mut reader, &mut seen, &run2);

    // ── run 3 opts out, and gets neither half ───────────────────────────────
    let run3 = run_id_of(&start_run_with(
        &socket,
        json!({
            "workflow_id": workflow_id,
            "args": { "goal": "add high contrast mode" },
            "include_prior_summaries": false,
        }),
    ));
    wait_for_run_finished(&mut reader, &mut seen, &run3);

    let run3_get = run_get(&socket, &run3);
    assert_eq!(
        run3_get["run"]["context_runs"],
        json!([]),
        "--no-prior-summaries records no context runs: {run3_get}"
    );
    let run3_task = task_markdown(&socket, &run3, "plan");
    assert!(
        !run3_task.contains("## Prior runs"),
        "the opted-out run's task.md must render byte-identically to a \
         history-free run (§7 R-7):\n{run3_task}"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 12. An over-budget summary gives up, and the run's outcome is untouched
// ---------------------------------------------------------------------------

/// `07-phase3-plan.md` §WS-J scenario 1's second half, and the guarantee §4 D1
/// is built to make: **the summariser can never wedge or flip a run.**
///
/// The stub writes a `text` past `SUMMARY_TEXT_BUDGET`, so the built-in schema's
/// `maxLength` rejects it — the check WS-A added to `complete::check`, without
/// which the budget would be a hope rather than a schema property. The one
/// corrective re-prompt is consumed, the epilogue lands in `GaveUp`, and:
///
///   * the run still reports `succeeded`;
///   * `workflow.summary.get` answers `null`, which is a normal answer and not
///     an error;
///   * the `.summary` node is `failed` with a recorded succession, so nothing
///     that walks the graph later finds a succession gap.
///
/// The `{"reason": "summary_failed"}` journal entry itself is asserted where a
/// test can link the store (`src/workflow/engine/mod.rs`, `store/tests.rs`);
/// this file asserts only API-observable facts, and the API-observable face of
/// that journal entry is the give-up state above.
#[test]
fn an_over_budget_summary_gives_up_and_leaves_the_runs_outcome_alone() {
    let server = spawn_summary_server("summary-over-budget", "over", "");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe_phase3(&socket);

    let workflow_id = create_workflow(&socket, "two_node_command.toml");
    let run_id = start_run(&socket, &workflow_id, "add dark mode");

    let finished = wait_for_run_finished(&mut reader, &mut seen, &run_id);
    assert_eq!(finished["data"]["run"]["status"], "succeeded", "{finished}");

    // The give-up is announced on the same node-event stream every other node
    // uses, so waiting for it is event-driven rather than timed.
    let gave_up = wait_for_event_matching(
        &mut reader,
        &mut seen,
        "workflow_node_updated",
        SETTLE,
        |event| {
            event_node_path(event).as_deref() == Some(".summary")
                && event["data"]["node"]["status"] == "failed"
        },
    );
    assert_eq!(
        gave_up["data"]["node"]["succession"]["type"], "no_followup",
        "a given-up epilogue records its succession rather than leaving a gap: \
         {gave_up}"
    );
    // The recorded evidence names the schema rule that rejected the summary,
    // which is the e2e proof that `maxLength` is *enforced* rather than
    // advisory — the budget is a schema property, not a hope (WS-A's addition
    // to `complete::check`).
    let evidence = gave_up["data"]["node"]["succession"]["evidence"]
        .as_str()
        .unwrap_or_default();
    assert!(
        evidence.contains("maxLength of 4000") && evidence.contains("text"),
        "the give-up must record which rule rejected the summary: {gave_up}"
    );

    assert_eq!(
        summary_get(&socket, &run_id),
        Value::Null,
        "an epilogue that gave up leaves no summary, and `None` is the normal \
         answer (§4 D1)"
    );
    assert_eq!(
        run_status(&socket, &run_id),
        "succeeded",
        "the epilogue must never change the run's outcome"
    );
    let run = run_get(&socket, &run_id);
    assert_eq!(run["run"]["nodes_total"], 2, "{run}");
    assert_eq!(run["run"]["nodes_done"], 2, "{run}");
    assert_eq!(
        summary_list(&socket, &workflow_id),
        Vec::<Value>::new(),
        "no summary was written, so none is listed"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 13. Restore: a seeded node is pane-less, feeds its downstream edge, and a
//     changed definition is skipped until the caller forces it
// ---------------------------------------------------------------------------

/// `07-phase3-plan.md` §WS-J scenario 2, and the e2e face of §4 D3/D4/D11/D18.
///
/// Run 1 executes both nodes, and its `plan` payload names run 1 — so when run
/// 2 restores `plan` and never executes it, the marker in the *downstream*
/// node's rendered inputs can only have come from run 1's checkpoint. That is
/// the claim restore makes and the one a topology-only assertion would miss.
///
/// Then the cross-version half, both polarities (05 §5.3's requirement): a v2
/// whose `plan` prompt differs fails §4 D11's `sha256(prompt_template)` gate and
/// is reported as a `definition_changed` skip — a *successful* run start
/// carrying the report, never an error — and `allow_changed` restores it anyway.
///
/// Summaries are off: this scenario starts four runs of one workflow back to
/// back, and each start would otherwise land inside the previous run's epilogue
/// window and be refused with `workflow_run_in_flight` (§4 D1/M7). The epilogue
/// itself is asserted in scenarios 11, 12, and 15.
#[test]
fn restoring_a_past_run_seeds_the_node_pane_less_and_skips_a_changed_definition() {
    let server =
        spawn_workflow_server_with_config("restore", "[workflow]\nsummary_enabled = false\n");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe_phase3(&socket);

    let workflow_id =
        create_workflow_from_text(&socket, &restore_definition_text("Plan v1: {{goal}}"));

    // ── run 1: both nodes execute ───────────────────────────────────────────
    let run1 = start_run(&socket, &workflow_id, "add dark mode");
    let finished = wait_for_run_finished(&mut reader, &mut seen, &run1);
    assert_eq!(finished["data"]["run"]["status"], "succeeded", "{finished}");
    let run1_marker = format!("plan payload from {run1}");

    // ── run 2: `plan` restored, `implement` executed ────────────────────────
    let panes_before = pane_count(&socket);
    let started = start_run_with(
        &socket,
        json!({
            "workflow_id": workflow_id,
            "args": { "goal": "add dark mode" },
            "restore_from": { "run_id": run1, "nodes": ["plan"] },
        }),
    );
    assert_eq!(
        started["restore"]["restored"],
        json!(["plan"]),
        "an unchanged definition restores the selected node: {started}"
    );
    assert_eq!(
        started["restore"]["skipped"],
        json!([]),
        "nothing is skipped when the definition is unchanged: {started}"
    );
    let run2 = run_id_of(&started);

    let restored_node = node_get(&socket, &run2, "plan");
    assert_eq!(restored_node["status"], "restored", "{restored_node}");
    assert_eq!(restored_node["evidence"], "restored", "{restored_node}");
    assert_eq!(
        restored_node["succession"]["type"], "satisfied",
        "{restored_node}"
    );
    assert!(
        restored_node["pane_id"].is_null(),
        "a restored node is pane-less (§4 D3): {restored_node}"
    );
    assert_eq!(
        restored_node["restored_from"]["run_id"],
        run1.as_str(),
        "{restored_node}"
    );
    assert_eq!(
        restored_node["restored_from"]["node_key"], "plan",
        "{restored_node}"
    );
    assert!(
        restored_node["restored_from"]["checkpoint_seq"].is_u64(),
        "the provenance names the source checkpoint (§4 D4): {restored_node}"
    );

    let run2_finished = wait_for_run_finished(&mut reader, &mut seen, &run2);
    assert_eq!(
        run2_finished["data"]["run"]["status"], "succeeded",
        "a restored upstream must fire its outbound edge like a succeeded one: \
         {run2_finished}"
    );

    // Exactly one pane was created for run 2 — `implement`'s. The restored node
    // did not get one, and nothing spawned a second time to compensate.
    assert_eq!(
        pane_count(&socket),
        panes_before + 1,
        "run 2 has two nodes and one of them was restored, so exactly one pane \
         may appear"
    );
    assert!(
        node_get(&socket, &run2, "implement")["pane_id"].is_string(),
        "the executed node still gets its own pane"
    );

    // The downstream node was fed run 1's payload, not a fresh one.
    let downstream_task = task_markdown(&socket, &run2, "implement");
    assert!(
        downstream_task.contains(&run1_marker),
        "the restored node's edge must carry run 1's checkpoint into the \
         downstream node's inputs, but its task.md names no such payload \
         (expected {run1_marker:?}):\n{downstream_task}"
    );
    let run2_get = run_get(&socket, &run2);
    assert_eq!(
        run2_get["run"]["restore_from_run"],
        run1.as_str(),
        "the run records where it restored from: {run2_get}"
    );

    // ── v2 changes `plan`'s prompt: the digest gate skips it ────────────────
    let versioned = request_ok(
        &socket,
        &request(
            "req_version_create",
            "workflow.version.create",
            json!({
                "workflow_id": workflow_id,
                "definition": {
                    "format": "toml",
                    "text": restore_definition_text("Plan v2, worded differently: {{goal}}"),
                },
                "change_summary": "reword the plan prompt",
            }),
        ),
    );
    assert_eq!(
        versioned["type"], "workflow_version_created",
        "unexpected: {versioned}"
    );

    let skipped = start_run_with(
        &socket,
        json!({
            "workflow_id": workflow_id,
            "args": { "goal": "add dark mode" },
            "restore_from": { "run_id": run1, "nodes": ["plan"] },
        }),
    );
    assert_eq!(
        skipped["restore"]["restored"],
        json!([]),
        "a node whose prompt changed is not restorable by default (§4 D11): \
         {skipped}"
    );
    let skips = skipped["restore"]["skipped"]
        .as_array()
        .unwrap_or_else(|| panic!("no skip list: {skipped}"));
    assert_eq!(skips.len(), 1, "{skipped}");
    assert_eq!(skips[0]["selector"], "plan", "{skipped}");
    assert_eq!(skips[0]["reason"], "definition_changed", "{skipped}");
    let run3 = run_id_of(&skipped);
    wait_for_run_finished(&mut reader, &mut seen, &run3);
    // A skipped node runs instead of being restored, which is what "defaults to
    // re-run" means: it executed (self-reported, in its own pane) and carries no
    // provenance.
    let reran = node_get(&socket, &run3, "plan");
    assert_eq!(reran["status"], "succeeded", "{reran}");
    assert_eq!(reran["evidence"], "self_report", "{reran}");
    assert!(reran["pane_id"].is_string(), "{reran}");
    assert!(reran["restored_from"].is_null(), "{reran}");

    // ── the same restore with `allow_changed` goes through ──────────────────
    let forced = start_run_with(
        &socket,
        json!({
            "workflow_id": workflow_id,
            "args": { "goal": "add dark mode" },
            "restore_from": { "run_id": run1, "nodes": ["plan"], "allow_changed": true },
        }),
    );
    assert_eq!(
        forced["restore"]["restored"],
        json!(["plan"]),
        "allow_changed is the documented override for definition_changed: \
         {forced}"
    );
    assert_eq!(forced["restore"]["skipped"], json!([]), "{forced}");
    let run4 = run_id_of(&forced);
    assert_eq!(node_get(&socket, &run4, "plan")["status"], "restored");
    let run4_finished = wait_for_run_finished(&mut reader, &mut seen, &run4);
    assert_eq!(
        run4_finished["data"]["run"]["status"], "succeeded",
        "{run4_finished}"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 14. A pruned run survives as a summary, and says so instead of pretending
// ---------------------------------------------------------------------------

/// `07-phase3-plan.md` §WS-J scenario 4 / §4 D9, D12, and 03 §9's "summary-only
/// with restore disabled and a reason".
///
/// With `retention_runs = 1`, finishing a second run prunes the first: its
/// `workflow_run` row is deleted and only its `run_summary` survives. The three
/// surfaces that must then agree:
///
///   * `workflow.summary.list` still returns run 1's summary, flagged
///     `run_pruned: true` — the M9 pin, because filtering summaries through the
///     `run` reference would silently drop exactly these rows;
///   * `workflow.run.list` no longer carries the run at all;
///   * restore and `workflow.run.get` both answer `workflow_run_pruned` rather
///     than a bare not-found, so the surviving surface is named.
///
/// Pruning fires on the workflow tick after a run settles and has no event of
/// its own, so the wait is a poll on the API surface itself rather than on the
/// clock: the state converges and stays converged.
#[test]
fn a_pruned_run_survives_as_a_summary_and_refuses_restore_and_run_get() {
    let server = spawn_summary_server("summary-pruned", "ok", "[workflow]\nretention_runs = 1\n");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe_phase3(&socket);

    let workflow_id = create_workflow(&socket, "two_node_command.toml");

    let run1 = start_run(&socket, &workflow_id, "add dark mode");
    wait_for_run_finished(&mut reader, &mut seen, &run1);
    wait_for_run_summarized(&mut reader, &mut seen, &run1);

    let run2 = start_run(&socket, &workflow_id, "add light mode");
    wait_for_run_finished(&mut reader, &mut seen, &run2);
    wait_for_run_summarized(&mut reader, &mut seen, &run2);

    let pruned_summary = poll_until(
        "run 1's summary to be flagged pruned",
        SETTLE,
        Duration::from_millis(200),
        || {
            summary_list(&socket, &workflow_id)
                .into_iter()
                .find(|summary| {
                    summary["run_id"] == run1.as_str() && summary["run_pruned"] == json!(true)
                })
        },
    );
    assert_eq!(
        pruned_summary["outcome"],
        format!("stub summary of {run1}").as_str(),
        "a pruned run's summary keeps its contents; only the run row goes: \
         {pruned_summary}"
    );

    let listed_summary_runs: BTreeSet<String> = summary_list(&socket, &workflow_id)
        .iter()
        .filter_map(|summary| summary["run_id"].as_str())
        .map(str::to_string)
        .collect();
    assert_eq!(
        listed_summary_runs,
        BTreeSet::from([run1.clone(), run2.clone()]),
        "the summary of a pruned run is exactly what survives pruning"
    );

    let listed_runs: BTreeSet<String> = request_ok(
        &socket,
        &request(
            "req_run_list_pruned",
            "workflow.run.list",
            json!({ "workflow_id": workflow_id }),
        ),
    )["runs"]
        .as_array()
        .unwrap_or_else(|| panic!("workflow.run.list returned no runs"))
        .iter()
        .filter_map(|run| run["run_id"].as_str())
        .map(str::to_string)
        .collect();
    assert_eq!(
        listed_runs,
        BTreeSet::from([run2.clone()]),
        "retention_runs = 1 keeps exactly one run row"
    );

    let got = send_request(
        &socket,
        &request(
            "req_run_get_pruned",
            "workflow.run.get",
            json!({ "run_id": run1 }),
        ),
    );
    assert_eq!(
        error_code(&got),
        "workflow_run_pruned",
        "a pruned run is refused by name, not as a bare not-found: {got}"
    );

    let restore_attempt = send_request(
        &socket,
        &request(
            "req_run_restore_pruned",
            "workflow.run",
            json!({
                "workflow_id": workflow_id,
                "args": { "goal": "add dark mode" },
                "restore_from": { "run_id": run1 },
            }),
        ),
    );
    assert_eq!(
        error_code(&restore_attempt),
        "workflow_run_pruned",
        "restore from a pruned run is refused with the same code: \
         {restore_attempt}"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 15. Every Phase 3 durable field survives a restart, field for field
// ---------------------------------------------------------------------------

/// The comparable shape of one node, for the epilogue node — which
/// [`run_shape`] deliberately excludes because it is still live at the instant a
/// run reports finished. Here it is compared only after
/// `workflow.run.summarized`, when it is as terminal as any other node.
///
/// **The tier fields are the point, not padding.** `demand`, `model`, `effort`,
/// and `assignment_reason` are exactly what the epilogue cannot derive from a
/// definition — it has no kvdag node (§4 D5) — so they are the fields a reader
/// that falls back to a definition-shaped default gets wrong, which is defect
/// E-13 (`workflow_node_info`'s `Demand::Standard` fallback reporting `standard`
/// for a row the engine wrote as `light`). `run_shape` compares all four for
/// every ordinary node and none of them for this one, so without them here the
/// live-vs-durable rule §4 D16 states has no pin on the only node that needs it
/// most. `label` is here for the same reason: the epilogue's name comes from
/// `EPILOGUE_LABEL`, not from an authored kvdag label.
fn epilogue_node_shape(response: &Value) -> Value {
    let node = response["graph"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .find(|node| node["path"] == ".summary")
        .unwrap_or_else(|| panic!("the run graph carries no .summary node: {response}"));
    json!({
        "path": node["path"],
        "label": node["label"],
        "status": node["status"],
        "demand": node["demand"],
        "model": node["model"],
        "effort": node["effort"],
        "assignment_reason": node["assignment_reason"],
        "evidence": node["evidence"],
        "succession": node["succession"],
        "cwd": node["cwd"],
        "node_dir": node["node_dir"],
        "started_at_unix_ms": node["started_at_unix_ms"],
        "ended_at_unix_ms": node["ended_at_unix_ms"],
        "attempt": node["attempt"],
    })
}

/// `07-phase3-plan.md` §WS-J scenario 5 — the e2e face of §4 D16, and the guard
/// against the 0.10.2 P1 defect class the whole phase is designed against.
///
/// [`a_finished_run_reads_back_field_equal_to_its_live_projection`] proved the
/// *shape* survives a restart; it could not prove Phase 3's fields do, because a
/// plain two-node run has none of them: `context_runs` is empty,
/// `restore_from_run` is null, no node is restored, and no summary exists. This
/// run has all four at once — it restores `plan` from a past run, is given that
/// run's summary as context, and is summarised itself — so every new durable
/// field is non-trivially populated on the live side before the comparison
/// means anything.
///
/// What is asserted across the restart:
///
///   * the widened [`run_shape`] — including the timestamp, `growth_limited`,
///     and `label` classes the original omitted, plus `transcript_path`,
///     `restored_from`, `context_runs`, `restore_from_run`, and
///     `workflow_name`;
///   * the epilogue node's own projection ([`epilogue_node_shape`]);
///   * `workflow.summary.get`, **field by field and by name**, so a decoder
///     that drops one fails with that field's name in the message rather than
///     with an opaque struct diff.
///
/// It also pins the one interrogation path CI can reach through the *durable*
/// projection: after the restart nothing about this run is in memory, and
/// `workflow.node.interrogate` on a `runner = "command"` node must still refuse
/// with `workflow_transcript_unavailable` and create no pane (03 §4.4's "never
/// a silently failing pane"). The live-run half of that refusal is
/// `tests/cli/workflow.rs`'s
/// `node_interrogate_on_command_runner_node_refuses_without_a_pane`.
#[test]
fn a_restored_and_summarised_run_reads_back_field_equal_to_its_live_projection() {
    let server = spawn_summary_server("phase3-restart-fidelity", "ok", "");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe_phase3(&socket);

    let workflow_id =
        create_workflow_from_text(&socket, &restore_definition_text("Plan v1: {{goal}}"));

    // ── run 1: the source of both the checkpoint and the context ────────────
    let run1 = start_run(&socket, &workflow_id, "add dark mode");
    wait_for_run_finished(&mut reader, &mut seen, &run1);
    wait_for_run_summarized(&mut reader, &mut seen, &run1);

    // ── run 2: restores from run 1, reads run 1's summary, summarises ───────
    let started = start_run_with(
        &socket,
        json!({
            "workflow_id": workflow_id,
            "args": { "goal": "add dark mode" },
            "restore_from": { "run_id": run1, "nodes": ["plan"] },
        }),
    );
    assert_eq!(started["restore"]["restored"], json!(["plan"]), "{started}");
    let run2 = run_id_of(&started);
    wait_for_run_finished(&mut reader, &mut seen, &run2);
    wait_for_run_summarized(&mut reader, &mut seen, &run2);

    let live = run_get(&socket, &run2);
    let live_shape = run_shape(&live);
    let live_epilogue = epilogue_node_shape(&live);
    let live_summary = summary_get(&socket, &run2);

    // The live projection is the reference, so it has to carry the Phase 3
    // facts the restored one is being checked for — otherwise "equal" could
    // mean "both empty".
    assert_eq!(
        live["run"]["context_runs"],
        json!([run1]),
        "live run: {live}"
    );
    assert_eq!(
        live["run"]["restore_from_run"],
        run1.as_str(),
        "live run: {live}"
    );
    assert!(
        !live["run"]["workflow_name"]
            .as_str()
            .unwrap_or_default()
            .is_empty(),
        "live run: {live}"
    );
    let live_plan = live["graph"]["nodes"]
        .as_array()
        .and_then(|nodes| nodes.iter().find(|node| node["path"] == "plan"))
        .unwrap_or_else(|| panic!("live run has no plan node: {live}"));
    assert_eq!(live_plan["status"], "restored", "live plan: {live_plan}");
    assert_eq!(
        live_plan["restored_from"]["run_id"],
        run1.as_str(),
        "live plan: {live_plan}"
    );
    assert!(
        !live_summary.is_null(),
        "the run must have a summary before the summary's persistence is \
         compared"
    );

    // Durable writes are queued onto the store thread, which serves its jobs in
    // order; a completed `workflow.run.list` is therefore a barrier proving
    // every write submitted before it has already been applied.
    let listed = request_ok(
        &socket,
        &request(
            "req_phase3_barrier",
            "workflow.run.list",
            json!({ "workflow_id": workflow_id }),
        ),
    );
    assert!(
        listed["runs"]
            .as_array()
            .is_some_and(|runs| runs.iter().any(|run| run["run_id"] == run2.as_str())),
        "the run must be in the store before the restart: {listed}"
    );

    // ── the same run, from a server that never executed it ──────────────────
    let server = server.restart();
    let socket = server.socket().to_path_buf();
    require_workflow_api(&socket);

    let restored = run_get(&socket, &run2);
    assert_eq!(
        run_shape(&restored),
        live_shape,
        "a restored-and-summarised run read back from the store must describe \
         the same run the engine did.\nlive:     {live}\nrestored: {restored}"
    );
    assert_eq!(
        epilogue_node_shape(&restored),
        live_epilogue,
        "the epilogue node is a run node like any other and reads back like \
         one.\nlive:     {live}\nrestored: {restored}"
    );

    // Field by field and by name (§4 D16): a decoder that drops one field fails
    // naming it, rather than failing on an opaque whole-struct comparison.
    let restored_summary = summary_get(&socket, &run2);
    for field in [
        "run_id",
        "workflow_id",
        "workflow_name",
        "version_id",
        "text",
        "outcome",
        "highlights",
        "open_gaps",
        "per_node",
        "token_estimate",
        "generated_by_path",
        "created_at_unix_ms",
        "run_pruned",
    ] {
        assert_eq!(
            restored_summary[field], live_summary[field],
            "summary field `{field}` did not survive the restart.\nlive:     \
             {live_summary}\nrestored: {restored_summary}"
        );
    }

    // ── the durable projection still refuses to interrogate a command node ──
    let panes_before = pane_count(&socket);
    let refused = send_request(
        &socket,
        &request(
            "req_interrogate_historical",
            "workflow.node.interrogate",
            json!({ "run_id": run2, "path": "implement" }),
        ),
    );
    assert_eq!(
        error_code(&refused),
        "workflow_transcript_unavailable",
        "a `runner = \"command\"` node has no session to fork, and the answer \
         must say so even when the run is only known from the store: {refused}"
    );
    assert_eq!(
        pane_count(&socket),
        panes_before,
        "a refused interrogation must never create a pane (03 §4.4)"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 16. Interrogating the epilogue is answered by how the summariser was bound
// ---------------------------------------------------------------------------

/// `workflow.node.interrogate .summary` under a command-bound summariser must
/// answer **"it ran as a command"**, not "its transcript is unavailable"
/// (defect M-1).
///
/// The epilogue is the one node with no kvdag node behind it (§4 D5), so the
/// handler's runner gate cannot derive its binding the way it derives every
/// other node's: a definition lookup misses, and a miss is documented to fall
/// through to the stat, which then refuses on a transcript that a command node
/// was never going to have. `EpilogueState::runner` is the recorded authority
/// (§4 D2 / defect D-1), and this is the assertion that it is the one being
/// read — the same defect and the same fix as D-C's `runner_for_pane`, reached
/// by path instead of by pane.
///
/// The message is the assertion, because both answers carry
/// `workflow_transcript_unavailable`: a caller told the transcript is missing
/// goes looking for a file, and for this node there is no file to find.
/// `KARVEX_WORKFLOW_SUMMARY_COMMAND` is what [`spawn_summary_server`] sets, so
/// every summariser in these scenarios is `runner = "command"` already.
#[test]
fn interrogating_a_command_bound_epilogue_is_refused_on_its_runner() {
    let server = spawn_summary_server("epilogue-interrogate", "ok", "");
    let socket = server.socket().to_path_buf();
    if !require_workflow_api(&socket) {
        server.shutdown();
        return;
    }

    create_workspace(&socket, &server.base);
    let (mut reader, mut seen) = subscribe_phase3(&socket);

    let workflow_id = create_workflow(&socket, "two_node_command.toml");
    let run = start_run(&socket, &workflow_id, "add dark mode");
    wait_for_run_finished(&mut reader, &mut seen, &run);
    wait_for_run_summarized(&mut reader, &mut seen, &run);

    // The run is still this server's — the live epilogue state is exactly what
    // the handler has to consult, and the summariser has finished, so nothing
    // here is racing the epilogue.
    let panes_before = pane_count(&socket);
    let refused = send_request(
        &socket,
        &request(
            "req_interrogate_epilogue",
            "workflow.node.interrogate",
            json!({ "run_id": run, "path": ".summary" }),
        ),
    );
    assert_eq!(
        error_code(&refused),
        "workflow_transcript_unavailable",
        "the epilogue has no session to fork, so the interrogation is refused: \
         {refused}"
    );
    let message = refused["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("ran as a command, not an agent"),
        "the refusal must name the summariser's *runner*; a transcript-shaped \
         answer sends the caller looking for a file that never existed: \
         {refused}"
    );
    assert_eq!(
        pane_count(&socket),
        panes_before,
        "a refused interrogation must never create a pane (03 §4.4)"
    );

    server.shutdown();
}
