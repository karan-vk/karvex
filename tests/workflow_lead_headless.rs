//! Headless end-to-end tests for the **agent-teams lead run**
//! (`docs/design/workflow-builder/09-agent-teams-rework.md` §3.1, §3.3, §3.4).
//!
//! Karvex no longer executes a workflow. `workflow.run` renders a lead prompt,
//! spawns one interactive `claude` team lead into a pane, and then *observes*:
//! it recognises the Claude Code team that lead creates, projects that team's
//! shared task list and member roster into the run's own records, and closes
//! the run when the lead says so — or when the lead's pane goes away. Every
//! one of those steps is a conversation between a karvex server process and
//! files a foreign process owns on disk, driven by a real PTY, so none of it
//! is reachable from a unit test. That is why this file exists and why each
//! scenario below is an e2e.
//!
//! These six scenarios are the load-bearing coverage for the new main path.
//! The engine-era suite in `tests/workflow_headless.rs` is retired with the
//! engine; what is here has to stand in its place, so each scenario is written
//! against a *failure it would let through* rather than against a happy path.
//!
//!  1. **Launch** — the run comes up `running`, the rendered `lead-prompt.md`
//!     names every planned node's task subject, and the process karvex execed
//!     was given `--teammate-mode tmux`, the `teammateMode` settings snapshot,
//!     the agent-teams env flag, the run id — and **no positional prompt**.
//!     That last one is an absence assertion on purpose: a positional prompt
//!     is the obvious spelling, it is silently swallowed by an interactive
//!     `claude` in a fresh pane, and the symptom is a lead that looks healthy
//!     and never does anything. The plan arrives afterwards through
//!     `agent.prompt`, which the stub records off its own stdin, so the two
//!     halves of that contract are asserted together.
//!  2. **Preflight refusal** — a `claude` too old for agent teams refuses the
//!     run with `workflow_lead_unavailable` **and creates no run row**. The
//!     preflight runs before `create_run` precisely so a refusal cannot leave
//!     an orphan run nothing will ever advance, and the only way to observe
//!     that ordering from outside is that `workflow.run.list` stays empty.
//!  3. **Binding and projection** — the run recognises its team, maps the
//!     prefix-matched tasks onto their definition nodes with the right status
//!     and owner, first-classes the unplanned task as an emergent node under
//!     the reserved `.task/` namespace, and snapshots the tmux-backed teammate
//!     with its model and pane id. Then the on-disk state *changes* and the
//!     projection has to follow it. A projection that only ever produced a
//!     correct first snapshot would pass a static test and be useless: diffing
//!     is its entire job.
//!  4. **Finish** — `kvx workflow run finish` invoked as the real CLI binary
//!     with only `KARVEX_WORKFLOW_RUN_ID` in its environment, which is the
//!     self-identification contract §3.3 promises the lead. It closes the run
//!     `succeeded`, the summary reads back through `workflow.summary.get`, and
//!     `workflow.run.summarized` is emitted. Finishing with no summary at all
//!     is refused — one CLI call replaced the entire summariser subsystem, so
//!     an empty finish would silently lose the only record of what happened.
//!  5. **Cancel** — `workflow.run.cancel` reports `cancelled` and closes the
//!     lead's pane. Teammates belong to the lead; the pane going away is the
//!     whole cancellation, and a cancel that marked the row without killing
//!     the process would leave a live orchestrator behind a closed run.
//!  6. **Lead exit without finishing** — driven through the public
//!     `pane.close` API rather than by killing the process, because that is
//!     the route a user actually takes. The run must reach a terminal status,
//!     and the task and member snapshot recorded before the exit must still be
//!     readable: Claude Code deletes the team config when the lead session
//!     ends, so that retained snapshot is the only durable record of what the
//!     team did and the only thing a later resume (§3.7) can be built on.
//!
//! **Isolation.** Every server here is given its own `CLAUDE_CONFIG_DIR`, so
//! `crate::integration::claude_dir()` resolves into the test's own temp base
//! and karvex reads teams and tasks from there instead of from the developer's
//! real `~/.claude`. `tests/fixtures/workflow/lead_stub.sh` refuses to run at
//! all when that variable is unset, so a harness regression fails the test
//! rather than quietly editing a real agent team. The same redirection makes
//! the folder-trust preflight look for a `.claude.json` that does not exist,
//! and an unreadable config answers "trusted" by design, so the preflight
//! passes without a fixture pretending to be a user's config.
//!
//! **Why the test drives the on-disk change in scenario 3, not the stub.** The
//! projection's contract is with the *filesystem*: it re-reads directories a
//! foreign process owns and records what moved. Writing the change from the
//! test makes the transition exact and observable at a known instant, where a
//! stub staging itself would only add a second clock to synchronise against.
//! The stub still writes the *initial* state, because that has to happen after
//! the lead pane exists and carry a `createdAt` fresh against the spawn — both
//! facts the binding rule depends on.
//!
//! **What is asserted.** Only what the public surface answers: the JSON API
//! socket and the real `kvx` binary. karvex has no library target, so an
//! integration test cannot link the store — and it should not want to, since
//! the point of these scenarios is that a *client* can see what happened.

// Pane-driven tests are gated off the macOS CI leg, matching
// `tests/workflow_headless.rs`, `tests/api_ping.rs`, and `tests/cli.rs`.
#![cfg(not(target_os = "macos"))]

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};

use support::jsonrpc::{
    error_code, open_subscription, poll_until, request_ok, send_request, wait_for_event_matching,
    JsonLineReader,
};
use support::{
    app_dir_name, cleanup_test_base, register_runtime_dir, register_spawned_karvex_pid,
    unregister_spawned_karvex_pid,
};

/// Spawning a pane, execing the stub, letting agent detection settle, and
/// letting the projection's 2 s poll come round twice is the longest chain any
/// of these waits covers. Generous on purpose: every wait here is a
/// poll-until-condition, so a high ceiling costs nothing on a passing run and
/// buys a meaningful failure message on a loaded machine.
const SETTLE: Duration = Duration::from_secs(60);

/// The lead session id `lead_stub.sh` records, and the team name Claude Code's
/// own rule derives from it (`session-` plus the first eight characters).
/// Duplicated from the fixture rather than read out of it: these are the
/// identifiers the *binding* is asserted against, so a test that recomputed
/// them from whatever the stub happened to write could not fail.
const LEAD_SESSION_ID: &str = "7f3c9a12-0b64-4d8e-9a11-2c5f6d7e8a90";
const TEAM_NAME: &str = "session-7f3c9a12";

/// The tmux-backed teammate `lead_stub.sh` puts in the team config.
const TEAMMATE: &str = "build-hand";
const TEAMMATE_MODEL: &str = "sonnet";

/// The task subjects the stub writes, which are exactly what
/// `lead_prompt::task_subject` renders for `lead_run.toml`'s two nodes. The
/// prefix before `:` is the entire matching contract (§3.2).
const PLAN_SUBJECT: &str = "plan: Draft the approach";
const BUILD_SUBJECT: &str = "build: Carry out the approach";
/// A task the definition never planned, so the projection must record it as an
/// emergent node instead of forcing it onto a definition key.
const EMERGENT_SUBJECT: &str = "chase down the flaky fixture nobody planned for";
/// Where an emergent task lands: the reserved namespace karvex owns.
const EMERGENT_PATH: &str = ".task/3";

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct LeadServer {
    base: PathBuf,
    socket: PathBuf,
    /// This server's private `CLAUDE_CONFIG_DIR`. Everything the projection
    /// reads lives under here, and nothing outside the test's base directory
    /// is ever touched.
    claude_home: PathBuf,
    _master: Option<Box<dyn MasterPty + Send>>,
    child: Box<dyn Child + Send + Sync>,
}

impl LeadServer {
    fn socket(&self) -> &Path {
        &self.socket
    }

    fn shutdown(self) {
        drop(self);
    }

    fn teams_dir(&self) -> PathBuf {
        self.claude_home.join("teams").join(TEAM_NAME)
    }

    fn tasks_dir(&self) -> PathBuf {
        self.claude_home.join("tasks").join(TEAM_NAME)
    }

    fn stub_log(&self) -> PathBuf {
        self.base.join("lead-stub.log")
    }
}

/// All teardown lives here rather than in `shutdown`, so a panicking assertion
/// still reaps the server, the lead's pane, and the base directory.
impl Drop for LeadServer {
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
        "/tmp/karvex-lead-e2e-{label}-{}-{nanos}",
        std::process::id()
    ))
}

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("workflow")
}

fn spawn_lead_server(label: &str) -> LeadServer {
    spawn_lead_server_with_env(label, &[])
}

/// Brings up a real headless server whose config, state, workflow database,
/// run directories **and Claude Code config directory** all live inside the
/// test's own base.
///
/// `CLAUDE_CONFIG_DIR` is the load-bearing one. It is read by
/// `crate::integration::claude_dir()`, which is the single place karvex
/// resolves `teams/` and `tasks/` from, and it is inherited by the lead pane so
/// the stub writes where karvex will look. It also moves the folder-trust
/// preflight's `.claude.json` lookup into the temp base, where no file exists —
/// and an unreadable config counts as trusted, so the preflight passes.
fn spawn_lead_server_with_env(label: &str, extra_env: &[(&str, &str)]) -> LeadServer {
    let base = unique_base(label);
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let state_home = base.join("state");
    let bin_dir = base.join("bin");
    let claude_home = base.join("claude");
    let socket = runtime_dir.join("karvex.sock");

    fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(&state_home).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(claude_home.join("teams")).unwrap();
    fs::create_dir_all(claude_home.join("tasks")).unwrap();
    register_runtime_dir(&runtime_dir);
    fs::write(
        config_home.join(app_dir_name()).join("config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    // The lead's own pane calls plain `kvx workflow run finish`
    // (§3.2's finish rule), so `kvx` on its PATH has to be the binary under
    // test rather than whatever the developer has installed.
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_kvx"), bin_dir.join("kvx")).unwrap();
    // `lead_argv` resolves the lead's executable through PATH
    // (`crate::detect::interactive_agent_executable`), and the version
    // preflight runs `claude --version` as a plain subprocess, so both resolve
    // to this stub.
    std::os::unix::fs::symlink(fixture_dir().join("lead_stub.sh"), bin_dir.join("claude")).unwrap();

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
    cmd.env("CLAUDE_CONFIG_DIR", &claude_home);
    cmd.env("LEAD_STUB_LOG", base.join("lead-stub.log"));
    cmd.env("LEAD_STUB_SESSION", LEAD_SESSION_ID);
    cmd.env("LEAD_STUB_TEAMMATE", TEAMMATE);
    cmd.env("LEAD_STUB_TEAMMATE_MODEL", TEAMMATE_MODEL);
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

    let server = LeadServer {
        base,
        socket,
        claude_home,
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

fn request(id: &str, method: &str, params: Value) -> String {
    json!({ "id": id, "method": method, "params": params }).to_string()
}

/// Fails loudly rather than skipping when the workflow API is not answering.
///
/// This binary only builds with the `workflow` feature on
/// (`[[test]] required-features`), so there is no configuration in which
/// having nothing to assert is the correct outcome — a skip here would
/// reproduce exactly the silent-pass this suite exists to prevent.
fn require_workflow_api(socket: &Path) {
    let response = send_request(socket, &request("probe", "workflow.list", json!({})));
    let code = error_code(&response);
    assert!(
        code.is_empty(),
        "the server answers workflow.* with `{code}`; this binary is only built with the \
         `workflow` feature on, so this is a real failure and not a feature-off build: \
         {response}"
    );
}

// ---------------------------------------------------------------------------
// API helpers
// ---------------------------------------------------------------------------

/// Creates the run's workspace, so the lead pane is split off a pane whose cwd
/// is the test's own base directory.
///
/// The path is canonicalised because `match_team`'s tier-1 rule compares the
/// leader member's recorded `cwd` against the lead pane's cwd as *paths*, and
/// the stub records `pwd -P`.
fn create_workspace(socket: &Path, cwd: &Path) -> String {
    let cwd = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
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

fn create_workflow(socket: &Path) -> String {
    let path = fixture_dir().join("lead_run.toml");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let result = request_ok(
        socket,
        &request(
            "req_create",
            "workflow.create",
            json!({ "definition": { "format": "toml", "text": text } }),
        ),
    );
    assert_eq!(result["type"], "workflow_created", "unexpected: {result}");
    result["workflow"]["workflow_id"]
        .as_str()
        .expect("workflow.create returned no workflow_id")
        .to_string()
}

fn run_request(workflow_id: &str, goal: &str) -> String {
    request(
        "req_run",
        "workflow.run",
        json!({ "workflow_id": workflow_id, "args": { "goal": goal } }),
    )
}

fn start_run(socket: &Path, workflow_id: &str, goal: &str) -> String {
    let result = request_ok(socket, &run_request(workflow_id, goal));
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

fn run_status(socket: &Path, run_id: &str) -> String {
    run_get(socket, run_id)["run"]["status"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn run_list(socket: &Path) -> Vec<Value> {
    let result = request_ok(
        socket,
        &request("req_run_list", "workflow.run.list", json!({})),
    );
    result["runs"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| panic!("workflow.run.list returned no runs array: {result}"))
}

/// Every node of the run's graph, keyed by its instance path.
fn run_nodes(socket: &Path, run_id: &str) -> BTreeMap<String, Value> {
    let response = run_get(socket, run_id);
    response["graph"]["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|node| Some((node["path"].as_str()?.to_string(), node)))
        .collect()
}

/// The run's member snapshot, keyed by teammate name.
fn run_members(socket: &Path, run_id: &str) -> BTreeMap<String, Value> {
    let response = run_get(socket, run_id);
    response["graph"]["members"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|member| Some((member["name"].as_str()?.to_string(), member)))
        .collect()
}

fn pane_ids(socket: &Path) -> Vec<String> {
    let result = request_ok(socket, &request("req_pane_list", "pane.list", json!({})));
    result["panes"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|pane| pane["pane_id"].as_str().map(str::to_string))
        .collect()
}

/// Subscribes to everything these scenarios assert on, in one connection.
fn subscribe(socket: &Path) -> JsonLineReader {
    let mut reader = open_subscription(
        socket,
        &request(
            "sub_lead",
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
    assert_eq!(ack["id"], "sub_lead", "unexpected subscribe ack: {ack}");
    assert_eq!(
        ack["result"]["type"], "subscription_started",
        "subscribe was rejected: {ack}"
    );
    reader
}

// ---------------------------------------------------------------------------
// Stub log helpers
// ---------------------------------------------------------------------------

fn read_stub_log(server: &LeadServer) -> String {
    fs::read_to_string(server.stub_log()).unwrap_or_default()
}

/// Every value the stub logged under `tag`, in order.
fn stub_lines(log: &str, tag: &str) -> Vec<String> {
    let prefix = format!("{tag}\t");
    log.lines()
        .filter_map(|line| line.strip_prefix(prefix.as_str()))
        .map(str::to_string)
        .collect()
}

/// Waits until the lead process has finished logging its argv, and returns the
/// whole log. `ARGC` is written after the last `ARG`, so its presence is what
/// makes the argv assertions read a complete list rather than a prefix.
fn wait_for_lead_argv(server: &LeadServer) -> String {
    poll_until(
        "the lead stub to record the argv karvex execed it with",
        SETTLE,
        Duration::from_millis(100),
        || {
            let log = read_stub_log(server);
            stub_lines(&log, "ARGC").first().map(|_| log)
        },
    )
}

/// The rendered lead prompt, found by searching the run-directory root rather
/// than by rebuilding the run-directory layout the test does not own.
fn find_lead_prompt(runs_root: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(runs_root).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join("lead-prompt.md");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Polling helpers
// ---------------------------------------------------------------------------

/// Polls `probe` until it answers `true`, panicking with `what` and the last
/// response seen. The projection ticks every 2 s, so everything downstream of
/// it is polled rather than slept on.
fn wait_for_run<F>(socket: &Path, run_id: &str, what: &str, mut probe: F) -> Value
where
    F: FnMut(&Value) -> bool,
{
    let deadline = Instant::now() + SETTLE;
    loop {
        let last = run_get(socket, run_id);
        if probe(&last) {
            return last;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for {what}; last workflow.run.get was:\n{last:#}");
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn wait_for_status(socket: &Path, run_id: &str, wanted: &str) -> Value {
    wait_for_run(
        socket,
        run_id,
        &format!("the run to report `{wanted}`"),
        |response| response["run"]["status"] == wanted,
    )
}

// ---------------------------------------------------------------------------
// On-disk drivers (§3.4's source of truth)
// ---------------------------------------------------------------------------

/// Rewrites one field of a task file the stub already wrote, atomically.
///
/// Read-modify-write rather than a fresh document: the point of the scenario is
/// that the projection notices *what changed*, so everything else about the
/// task must stay byte-identical.
fn edit_task(server: &LeadServer, id: &str, edit: impl FnOnce(&mut Value)) {
    let path = server.tasks_dir().join(format!("{id}.json"));
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("the stub never wrote {}: {err}", path.display()));
    let mut value: Value = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("{} is not JSON ({err}): {text}", path.display()));
    edit(&mut value);
    write_atomic(&path, &serde_json::to_string_pretty(&value).unwrap());
}

/// Rewrites the team config the stub wrote, keeping `createdAt` and the roster
/// shape intact.
fn edit_team_config(server: &LeadServer, edit: impl FnOnce(&mut Value)) {
    let path = server.teams_dir().join("config.json");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("the stub never wrote {}: {err}", path.display()));
    let mut value: Value = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("{} is not JSON ({err}): {text}", path.display()));
    edit(&mut value);
    write_atomic(&path, &serde_json::to_string_pretty(&value).unwrap());
}

/// A rename, so a poll that lands mid-write reads the old document rather than
/// half of the new one. karvex tolerates a torn read by design; producing one
/// on purpose would make this a test of the retry loop instead.
fn write_atomic(path: &Path, body: &str) {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, body).unwrap_or_else(|err| panic!("failed to write {}: {err}", tmp.display()));
    fs::rename(&tmp, path)
        .unwrap_or_else(|err| panic!("failed to rename onto {}: {err}", path.display()));
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Runs the real `kvx` binary against this server's socket, with `extra_env`
/// on top. Used instead of a socket request wherever the scenario is about the
/// CLI contract itself.
fn run_cli(socket: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kvx"));
    command.args(args);
    command.env("KARVEX_SOCKET_PATH", socket);
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().unwrap()
}

// ---------------------------------------------------------------------------
// Shared setup
// ---------------------------------------------------------------------------

/// The full launch: workspace, definition, run, and a wait until the lead
/// process is up and has logged its argv.
///
/// Every scenario past the first needs a live lead, and each of them would
/// otherwise repeat the same four steps with slightly different waits. Returns
/// the workflow id alongside the run id, so a scenario can launch against the
/// same definition twice.
fn launch_lead_run(server: &LeadServer) -> (String, String) {
    let socket = server.socket().to_path_buf();
    require_workflow_api(&socket);
    create_workspace(&socket, &server.base);
    let workflow_id = create_workflow(&socket);
    let run_id = start_run(&socket, &workflow_id, "add dark mode");
    wait_for_lead_argv(server);
    (workflow_id, run_id)
}

/// Waits until the run has recognised the team the stub created.
fn wait_for_team_binding(socket: &Path, run_id: &str) -> Value {
    wait_for_run(
        socket,
        run_id,
        "the run to bind to the team its lead created",
        |response| response["run"]["team_name"] == TEAM_NAME,
    )
}

// ---------------------------------------------------------------------------
// 1. Launch
// ---------------------------------------------------------------------------

/// §3.1 steps 2 and 3, end to end: what karvex writes, and what it execs.
///
/// The argv assertions are the reason this is an e2e. `lead_argv` is a pure
/// function with its own unit tests, but nothing in a unit test can show that
/// the vector it returns is what the *process* was started with — that it
/// survived the pane launch, the shell config, and the runtime's env handling
/// intact. The absence of a positional prompt is asserted the same way and for
/// a sharper reason: adding one is the natural way to "fix" a lead that has no
/// plan, it type-checks, it looks right in a review, and it produces a lead
/// that comes up healthy and never acts. The paired assertion — that the plan
/// arrives on the process's *stdin* afterwards — is what makes the absence
/// safe rather than merely true.
#[test]
fn a_lead_run_spawns_an_agent_teams_claude_with_no_positional_prompt_and_is_seeded_after() {
    let server = spawn_lead_server("launch");
    let socket = server.socket().to_path_buf();
    let mut events = subscribe(&socket);

    let (workflow_id, run_id) = launch_lead_run(&server);

    // The run row is `running` the moment its pane is: no engine will move it
    // off `pending` now, so a lead run stuck at `pending` is a dead run.
    assert_eq!(
        run_status(&socket, &run_id),
        "running",
        "a spawned lead must leave the run row on `running`: {:#}",
        run_get(&socket, &run_id)
    );

    // The positive control for the preflight-refusal scenario below: a launch
    // that *is* allowed puts exactly one row in `workflow.run.list`. Without
    // this, "the list is empty after a refusal" could be true because the list
    // never carries anything.
    let listed = run_list(&socket);
    assert_eq!(
        listed.len(),
        1,
        "a successful launch must leave exactly one run listed: {listed:#?}"
    );
    assert_eq!(listed[0]["run_id"], run_id.as_str(), "{listed:#?}");

    // The rendered plan (§3.2). Its whole job is to name the tasks the lead
    // must create, with the exact subjects the projection matches back.
    let runs_root = server.base.join("workflow-runs");
    let prompt_path = poll_until(
        "the rendered lead prompt to appear in the run directory",
        SETTLE,
        Duration::from_millis(100),
        || find_lead_prompt(&runs_root),
    );
    let prompt = fs::read_to_string(&prompt_path).unwrap();
    for subject in [PLAN_SUBJECT, BUILD_SUBJECT] {
        assert!(
            prompt.contains(subject),
            "the lead prompt must name every planned node's task subject verbatim; \
             {subject:?} is missing from {}:\n{prompt}",
            prompt_path.display()
        );
    }
    assert!(
        prompt.contains("add dark mode"),
        "the lead prompt must carry the run's interpolated args:\n{prompt}"
    );

    let log = read_stub_log(&server);
    let args = stub_lines(&log, "ARG");

    // Forcing tmux teammate mode is mandatory, not defensive: Claude Code's
    // default is in-process even inside tmux, and in-process teammates are not
    // panes and do not survive a resume.
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--teammate-mode" && pair[1] == "tmux"),
        "the lead must be launched with `--teammate-mode tmux`, got {args:?}"
    );
    // The settings payload is a file in the run directory now, because it also
    // carries the run's `SessionStart` identity hook — and because Claude Code
    // forwards the *value* of `--settings` to the teammates it spawns, so the
    // hook reaches them too.
    let settings_path = args
        .windows(2)
        .find(|pair| pair[0] == "--settings")
        .map(|pair| pair[1].clone())
        .expect("the lead must be launched with --settings");
    let settings: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&settings_path).expect("the settings file exists"),
    )
    .expect("the settings file is JSON");
    assert_eq!(
        settings["teammateMode"], "tmux",
        "the lead must also carry the `teammateMode` settings snapshot — the flag is \
         hidden and experimental, so neither spelling is load-bearing alone; got {settings}"
    );
    // karvex is the lead's parent, not its child, so a karvex message is an
    // ordinary peer message whose delivery would otherwise depend on the lead's
    // permission mode. This is upstream's documented knob for that case.
    assert_eq!(
        settings["crossSessionInbound"], "accept",
        "the run must accept messages from karvex; got {settings}"
    );
    let hook = &settings["hooks"]["SessionStart"][0]["hooks"][0]["command"];
    let hook = hook.as_str().unwrap_or_default();
    assert!(
        hook.contains("workflow run report-session"),
        "the run's settings must carry the SessionStart identity hook; got {settings}"
    );
    assert!(
        hook.contains("workflow_run"),
        "the identity hook must name the run it reports for, because a teammate's pane \
         never sees the lead's environment; got {hook}"
    );

    // `--name` is what makes the lead addressable: without it Claude Code
    // derives a name from the cwd's folder, identical for every run in a repo.
    let name = args
        .windows(2)
        .find(|pair| pair[0] == "--name")
        .map(|pair| pair[1].clone())
        .expect("the lead must be launched with --name");
    assert!(
        name.starts_with("karvex-run-"),
        "the lead's session name must be run-scoped, got {name:?}"
    );

    // The absence assertion. A flags-only argv is an even number of arguments
    // in `--flag value` pairs; a trailing positional prompt makes it odd and
    // puts a non-`--` token in a flag slot. Both halves are checked, so
    // appending a prompt *and* another flag pair still fails.
    assert_eq!(
        args.len() % 2,
        0,
        "the lead's argv must be `--flag value` pairs with no trailing positional \
         prompt — an interactive claude silently discards one, leaving a healthy lead \
         with no plan. Got {args:?}"
    );
    for (index, arg) in args.iter().enumerate().filter(|(index, _)| index % 2 == 0) {
        assert!(
            arg.starts_with("--"),
            "argument {index} ({arg:?}) sits in a flag slot but is not a flag; the argv \
             has drifted from `--flag value` pairs: {args:?}"
        );
    }
    assert!(
        !args.iter().any(|arg| arg.contains("lead-prompt.md")),
        "the plan must not be passed on the command line in any form; it is delivered \
         into the pane afterwards. Got {args:?}"
    );

    // `--add-dir` must name the run directory, which is what makes the lead
    // able to read the plan it is about to be pointed at.
    let run_dir = prompt_path.parent().unwrap().to_string_lossy().into_owned();
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "--add-dir" && pair[1] == run_dir),
        "the lead must be given its run directory with `--add-dir {run_dir}`, got {args:?}"
    );

    // The environment §3.1 step 3 promises: the experimental flag that turns
    // agent teams on at all, and the run id that makes `kvx workflow run
    // finish` need no argument.
    assert_eq!(
        stub_lines(&log, "TEAMS"),
        vec!["1".to_string()],
        "the lead pane must carry CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1:\n{log}"
    );
    assert_eq!(
        stub_lines(&log, "RUN"),
        vec![run_id.clone()],
        "the lead pane must carry KARVEX_WORKFLOW_RUN_ID so the lead self-identifies:\n{log}"
    );
    let pane_env = stub_lines(&log, "PANE");
    assert!(
        pane_env.first().is_some_and(|pane| !pane.is_empty()),
        "the lead pane must carry KARVEX_PANE_ID; without it karvex's own identifiers \
         cannot come back through Claude Code's team state:\n{log}"
    );

    // A pane really exists for it, and it is the pane the lead reports.
    let lead_pane = pane_env[0].clone();
    assert!(
        pane_ids(&socket).contains(&lead_pane),
        "the lead's own pane id must be a pane this server lists: {:?}",
        pane_ids(&socket)
    );
    let mut seen = Vec::new();
    wait_for_event_matching(&mut events, &mut seen, "pane_created", SETTLE, |event| {
        event["data"]["pane"]["pane_id"] == lead_pane.as_str()
    });

    // And the run itself is announced. The engine used to publish this from
    // its own event funnel; that funnel went with it, and for a while nothing
    // published `workflow.run.started` at all — a subscriber watching for runs
    // saw a server that never started one. Asserted here because the only way
    // to see it is from outside the process, on a real subscription.
    let started_event = wait_for_event_matching(
        &mut events,
        &mut seen,
        "workflow_run_started",
        SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );
    assert_eq!(
        started_event["data"]["run"]["status"], "running",
        "the started event carries the row as it reads back: {started_event}"
    );

    // The single-live-run guard. §3.1's binding is a *search* over team configs
    // filtered by a freshness window and the lead pane's cwd, and its one
    // documented race is two runs launched into the same cwd inside that
    // window — which this guard is what makes rare. A second launch that
    // slipped through would not fail loudly; it would bind to the wrong team.
    let second = send_request(&socket, &run_request(&workflow_id, "add light mode"));
    assert_eq!(
        error_code(&second),
        "workflow_run_in_flight",
        "a second run must be refused while a lead is live, or two runs race for the \
         same team config: {second}"
    );
    assert_eq!(
        run_list(&socket).len(),
        1,
        "and the refusal must not have created a row either: {:#?}",
        run_list(&socket)
    );

    // The other half of "no positional prompt": the plan is delivered into the
    // pane once the session is up, and the stub records it off its own stdin.
    let delivered = poll_until(
        "the lead's plan to be delivered into its pane",
        SETTLE,
        Duration::from_millis(250),
        || {
            let log = read_stub_log(&server);
            let lines = stub_lines(&log, "STDIN");
            (!lines.is_empty()).then_some(lines)
        },
    );
    assert!(
        delivered.iter().any(|line| line.contains("lead-prompt.md")),
        "the seed delivered into the lead's pane must point at the rendered plan by \
         absolute path, got {delivered:?}"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 2. Preflight refusal
// ---------------------------------------------------------------------------

/// §4's Claude-Code-version risk row, and the ordering that makes it safe.
///
/// A `claude` older than agent teams starts perfectly well and then simply
/// never spawns a teammate, so the failure this prevents is a run that looks
/// alive forever. The refusal itself is only half the guarantee: the preflight
/// deliberately runs *before* `create_run`, because a hard error after the row
/// exists leaves an orphan `workflow_run` nothing will ever advance. From
/// outside the process the only way to see that ordering is that no run exists
/// at all afterwards — which is why the emptiness of `workflow.run.list` is
/// asserted here and not treated as incidental.
#[test]
fn a_claude_too_old_for_agent_teams_refuses_the_run_and_creates_no_run_row() {
    let server = spawn_lead_server_with_env("preflight", &[("LEAD_STUB_VERSION", "2.1.100")]);
    let socket = server.socket().to_path_buf();
    require_workflow_api(&socket);

    create_workspace(&socket, &server.base);
    let workflow_id = create_workflow(&socket);

    let response = send_request(&socket, &run_request(&workflow_id, "add dark mode"));
    assert_eq!(
        error_code(&response),
        "workflow_lead_unavailable",
        "an unsupported claude must refuse the run with the lead-unavailable code: {response}"
    );
    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("2.1.100") && message.contains("2.1.224"),
        "the refusal must name both the version found and the version required so the \
         remedy is actionable, got {message:?}"
    );

    assert!(
        run_list(&socket).is_empty(),
        "a refused launch must leave no run behind; the preflight runs before \
         `create_run` precisely so a refusal cannot orphan a row: {:#?}",
        run_list(&socket)
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 3. Binding and projection
// ---------------------------------------------------------------------------

/// §3.1 step 4 and §3.4, which are one story: the binding writes the team name
/// the projection then reads.
///
/// Nothing here is reachable without a real server. The binding is a *search*
/// over team configs on disk, gated on a freshness window against the spawn
/// instant and on the leader member's cwd matching the pane karvex launched —
/// so it can only be exercised by a process that actually launched a pane and
/// then wrote a config. The projection then has to turn those files into run
/// records that a client can read back, and the mapping it performs (subject
/// prefix onto definition node, everything else into the reserved `.task/`
/// namespace) is the whole of §3.2's "loose" contract.
///
/// The second half drives a *change* on disk. A projection that produced one
/// correct snapshot and then went deaf would pass every static assertion above
/// and be worthless: diffing is its entire job, and a run whose nodes freeze at
/// their first observed status is indistinguishable from a healthy one until
/// someone looks at the pane.
#[test]
fn the_run_binds_its_team_and_projects_tasks_members_and_their_changes() {
    let server = spawn_lead_server("projection");
    let socket = server.socket().to_path_buf();
    let (_workflow_id, run_id) = launch_lead_run(&server);

    let bound = wait_for_team_binding(&socket, &run_id);
    assert_eq!(
        bound["run"]["lead_session_id"], LEAD_SESSION_ID,
        "the binding must record the lead session id from the team config, which is \
         what a later resume is addressed by: {bound:#}"
    );
    assert!(
        bound["run"]["lead_pane_id"]
            .as_str()
            .is_some_and(|pane| { pane_ids(&socket).iter().any(|known| known == pane) }),
        "the bound run must name the lead's own pane, which is where a user steers the \
         whole run: {bound:#}"
    );

    // The planned half: two tasks whose subjects carry a definition key.
    let nodes = poll_until(
        "the projection to record all three of the lead's tasks",
        SETTLE,
        Duration::from_millis(250),
        || {
            let nodes = run_nodes(&socket, &run_id);
            let complete = nodes.contains_key("plan")
                && nodes.contains_key("build")
                && nodes.contains_key(EMERGENT_PATH);
            complete.then_some(nodes)
        },
    );

    let plan = &nodes["plan"];
    assert_eq!(
        plan["task_id"], "1",
        "plan must carry Claude Code's task id: {plan:#}"
    );
    assert_eq!(
        plan["subject"], PLAN_SUBJECT,
        "plan must record the subject the team actually used, verbatim: {plan:#}"
    );
    assert_eq!(
        plan["status"], "running",
        "an `in_progress` task maps onto a running node: {plan:#}"
    );
    assert_eq!(
        plan["owner"], TEAMMATE,
        "the claiming teammate is what makes `select a node, focus its pane` \
         resolvable: {plan:#}"
    );
    assert_eq!(
        plan["emergent"], false,
        "a subject carrying a definition key is planned work, not emergent: {plan:#}"
    );

    let build = &nodes["build"];
    assert_eq!(
        build["task_id"], "2",
        "build must carry its task id: {build:#}"
    );
    assert_eq!(
        build["status"], "pending",
        "unclaimed work is pending: {build:#}"
    );
    assert_eq!(
        build["owner"], "",
        "an unclaimed task genuinely omits `owner`; karvex must not invent one: {build:#}"
    );
    assert_eq!(
        build["emergent"], false,
        "build is a planned node: {build:#}"
    );

    // The emergent half. The lead is allowed to invent work (§3.2's "loose"
    // paragraph); recording it under a reserved namespace instead of forcing
    // it onto a definition key is what makes that first-class rather than
    // silently lost.
    let emergent = &nodes[EMERGENT_PATH];
    assert_eq!(
        emergent["emergent"], true,
        "a task matching no definition key must be recorded as emergent: {emergent:#}"
    );
    assert_eq!(
        emergent["subject"], EMERGENT_SUBJECT,
        "the emergent node keeps the subject the team gave it: {emergent:#}"
    );
    assert_eq!(
        emergent["task_id"], "3",
        "the emergent node keeps its Claude Code task id: {emergent:#}"
    );

    // The member snapshot. It exists because Claude Code deletes the team
    // config when the lead session ends, so this is the durable record of who
    // worked on the run and in which pane.
    let members = poll_until(
        "the projection to snapshot the run's tmux-backed teammate",
        SETTLE,
        Duration::from_millis(250),
        || {
            let members = run_members(&socket, &run_id);
            members.contains_key(TEAMMATE).then_some(members)
        },
    );
    let teammate = &members[TEAMMATE];
    assert_eq!(
        teammate["model"], TEAMMATE_MODEL,
        "a teammate's model is part of what the run record has to preserve: {teammate:#}"
    );
    assert_eq!(
        teammate["backend_type"], "tmux",
        "karvex forces split-pane teammate mode and then has to check it took; an \
         in-process teammate here means the force silently failed: {teammate:#}"
    );
    assert_eq!(teammate["is_active"], true, "{teammate:#}");
    let pane = teammate["pane_id"]
        .as_str()
        .unwrap_or_else(|| panic!("a tmux-backed teammate must carry a pane id: {teammate:#}"));
    assert!(
        pane_ids(&socket).iter().any(|known| known == pane),
        "the teammate's `tmuxPaneId` is a karvex pane id and must name a pane this \
         server has: {:?}",
        pane_ids(&socket)
    );
    // The lead is in the roster too, but without a pane: it is a session, not a
    // pane, and its `tmuxPaneId` is the literal `"leader"` sentinel rather than
    // an id. Recording that sentinel as a pane would put an unresolvable pane
    // reference in the roster the DAG resolves node ownership from.
    let lead_member = members
        .get("team-lead")
        .unwrap_or_else(|| panic!("the run must record the lead itself as a member: {members:#?}"));
    assert_eq!(
        lead_member["backend_type"], "in-process",
        "the lead is always the in-process member: {lead_member:#}"
    );
    assert!(
        lead_member["pane_id"].is_null(),
        "the lead's `\"leader\"` sentinel must not be recorded as a pane id: {lead_member:#}"
    );

    // ── and now the diff ───────────────────────────────────────────────────
    // The team finishes the planning task and claims the next one, and a
    // teammate goes inactive. All three are observed only as file edits, which
    // is exactly how the real thing observes them.
    edit_task(&server, "1", |task| {
        task["status"] = json!("completed");
    });
    edit_task(&server, "2", |task| {
        task["status"] = json!("in_progress");
        task["owner"] = json!(TEAMMATE);
    });

    wait_for_run(
        &socket,
        &run_id,
        "the projection to follow the plan task to completion",
        |response| {
            let nodes = response["graph"]["nodes"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            nodes
                .iter()
                .any(|node| node["path"] == "plan" && node["status"] == "succeeded")
        },
    );
    let moved = run_nodes(&socket, &run_id);
    assert_eq!(
        moved["build"]["status"], "running",
        "the claimed task must move too; a projection that only followed one file \
         would pass the assertion above and still be broken: {:#}",
        moved["build"]
    );
    assert_eq!(
        moved["build"]["owner"], TEAMMATE,
        "an owner that appears after the fact must be recorded: {:#}",
        moved["build"]
    );
    assert_eq!(
        moved[EMERGENT_PATH]["status"], "pending",
        "the untouched task must not drift: {:#}",
        moved[EMERGENT_PATH]
    );

    edit_team_config(&server, |config| {
        if let Some(members) = config["members"].as_array_mut() {
            for member in members.iter_mut() {
                if member["name"] == TEAMMATE {
                    member["isActive"] = json!(false);
                }
            }
        }
    });
    wait_for_run(
        &socket,
        &run_id,
        "the projection to follow the teammate going inactive",
        |response| {
            let members = response["graph"]["members"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            members
                .iter()
                .any(|member| member["name"] == TEAMMATE && member["is_active"] == false)
        },
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 4. Finish
// ---------------------------------------------------------------------------

/// §3.3's normal finish, driven exactly as the lead is told to drive it.
///
/// This one call replaced the whole summariser subsystem — the epilogue node,
/// its schema, its retry ladder, its give-up states. So the contract it has to
/// honour is bigger than it looks: the run closes, the prose survives in the
/// same `run_summary` table the old summariser wrote to, and the event that
/// told clients a summary exists still fires.
///
/// It is invoked as the real `kvx` binary with nothing but
/// `KARVEX_WORKFLOW_RUN_ID` in its environment, because that *is* the contract:
/// karvex puts the run id in the lead's pane so the lead needs no argument, and
/// a `run finish` that quietly required `--run` would be a promise broken in
/// the one place nobody would look.
///
/// The refusal half matters as much. `finish` is the only thing that records
/// what a run did; accepting an empty one would close runs into silence.
#[test]
fn the_lead_finishes_its_own_run_through_the_cli_and_an_empty_finish_is_refused() {
    let server = spawn_lead_server("finish");
    let socket = server.socket().to_path_buf();
    let mut events = subscribe(&socket);
    let (_workflow_id, run_id) = launch_lead_run(&server);
    wait_for_team_binding(&socket, &run_id);

    // A finish with no summary at all, refused before anything is recorded.
    let refused = run_cli(
        &socket,
        &["workflow", "run", "finish"],
        &[("KARVEX_WORKFLOW_RUN_ID", run_id.as_str())],
    );
    assert!(
        !refused.status.success(),
        "finishing with no summary must fail: stdout={} stderr={}",
        String::from_utf8_lossy(&refused.stdout),
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("summary"),
        "the refusal must say a summary is what is missing, got {:?}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(
        run_status(&socket, &run_id),
        "running",
        "a refused finish must leave the run untouched"
    );

    // The real thing: a summary file, and no `--run` argument.
    let summary_path = server.base.join("run-summary.md");
    let summary_text = "# What this run did\n\nDrafted the approach, then carried it out.\n";
    fs::write(&summary_path, summary_text).unwrap();
    let finished = run_cli(
        &socket,
        &[
            "workflow",
            "run",
            "finish",
            "--summary-file",
            &summary_path.to_string_lossy(),
        ],
        &[("KARVEX_WORKFLOW_RUN_ID", run_id.as_str())],
    );
    assert!(
        finished.status.success(),
        "`kvx workflow run finish` must self-identify from KARVEX_WORKFLOW_RUN_ID: \
         stdout={} stderr={}",
        String::from_utf8_lossy(&finished.stdout),
        String::from_utf8_lossy(&finished.stderr)
    );

    let closed = wait_for_status(&socket, &run_id, "succeeded");
    assert!(
        closed["run"]["ended_at_unix_ms"]
            .as_u64()
            .is_some_and(|at| at > 0),
        "a finished run must be stamped with when it closed: {closed:#}"
    );

    let stored = request_ok(
        &socket,
        &request(
            "req_summary",
            "workflow.summary.get",
            json!({ "run_id": run_id }),
        ),
    );
    assert_eq!(
        stored["summary"]["text"], summary_text,
        "the lead's prose must be readable back verbatim; it is the only durable record \
         of what the run did: {stored:#}"
    );
    assert_eq!(
        stored["summary"]["outcome"], "succeeded",
        "an unqualified finish defaults to `succeeded`: {stored:#}"
    );
    assert_eq!(stored["summary"]["run_id"], run_id, "{stored:#}");

    let mut seen = Vec::new();
    let summarized = wait_for_event_matching(
        &mut events,
        &mut seen,
        "workflow_run_summarized",
        SETTLE,
        |event| event["data"]["run_id"] == run_id.as_str(),
    );
    assert_eq!(
        summarized["data"]["summary"]["text"], summary_text,
        "the event must carry the same summary the store answers with; an event that \
         disagreed with the row would be worse than no event: {summarized:#}"
    );
    // And the run announces itself finished, carrying the closed row. Both
    // events matter: `summarized` is what tells a client a summary exists,
    // `finished` is what tells it the run is over, and a lead self-report has
    // to produce the same pair the retired engine's close-out did.
    let finished_event = wait_for_event_matching(
        &mut events,
        &mut seen,
        "workflow_run_finished",
        SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );
    assert_eq!(
        finished_event["data"]["run"]["status"], "succeeded",
        "the finish event must carry the closed run, not the running one: \
         {finished_event:#}"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 5. Cancel
// ---------------------------------------------------------------------------

/// §3.3's cancel: no task-level kill choreography, because teammates belong to
/// the lead. Closing the lead's pane *is* the cancellation.
///
/// Both halves are asserted because either alone is a bug that reads as
/// success. A cancel that marks the row and leaves the pane running leaves a
/// live orchestrator spawning teammates behind a run the user believes is over;
/// a cancel that closes the pane without marking the row would then be
/// re-reported by the lead-exit path as a *failure*, which is a different and
/// wrong story about what happened.
#[test]
fn cancelling_a_lead_run_closes_the_lead_pane_and_reports_cancelled() {
    let server = spawn_lead_server("cancel");
    let socket = server.socket().to_path_buf();
    let mut events = subscribe(&socket);
    let (_workflow_id, run_id) = launch_lead_run(&server);
    let bound = wait_for_team_binding(&socket, &run_id);

    let lead_pane = bound["run"]["lead_pane_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the bound run named no lead pane: {bound:#}"))
        .to_string();
    assert!(
        pane_ids(&socket).contains(&lead_pane),
        "the lead's pane must exist before it is cancelled: {:?}",
        pane_ids(&socket)
    );

    let cancelled = request_ok(
        &socket,
        &request(
            "req_cancel",
            "workflow.run.cancel",
            json!({ "run_id": run_id }),
        ),
    );
    assert_eq!(
        cancelled["run"]["status"], "cancelled",
        "cancel must answer with the closed run: {cancelled:#}"
    );
    assert_eq!(
        run_status(&socket, &run_id),
        "cancelled",
        "and the stored row must agree with the response"
    );

    poll_until(
        "the lead's pane to be closed by the cancellation",
        SETTLE,
        Duration::from_millis(200),
        || (!pane_ids(&socket).contains(&lead_pane)).then_some(()),
    );

    // A cancel is terminal, so subscribers hear `run.finished` — there is no
    // cancelled event kind on the wire, and a client watching only for
    // finishes must not miss the run that ended because it was cancelled.
    let mut seen = Vec::new();
    let finished_event = wait_for_event_matching(
        &mut events,
        &mut seen,
        "workflow_run_finished",
        SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );
    assert_eq!(
        finished_event["data"]["run"]["status"], "cancelled",
        "the finish event carries the cancelled row: {finished_event:#}"
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// 6. Lead exit without finishing
// ---------------------------------------------------------------------------

/// §3.3's lead-exit case, driven through `pane.close` because that is the route
/// a user actually takes — they close the pane. Killing the process would
/// exercise the same reconciliation through a door no user opens.
///
/// The status half is the obvious guarantee: a run whose orchestrator vanished
/// must not stay `running` forever. The retention half is the one that matters
/// more and is easier to lose. Claude Code deletes `~/.claude/teams/<team>/`
/// when the lead session ends, so the snapshot karvex took while the run lived
/// is the *only* durable record of who the teammates were and what the task
/// list said — and it is the whole basis for §3.7's resume. A close-out path
/// that cleared the run's projected nodes and members on the way out would look
/// perfectly correct on a status assertion and would quietly make resume
/// impossible.
///
/// Known gap, deliberately not asserted: §3.3 specifies this terminal state
/// carries a structured `failure = {"kind": "lead_exited", "resumable": true}`
/// payload so a lead exit is distinguishable from a lead that reported failure.
/// The implementation writes the terminal status without that payload today, so
/// `run.failure` is absent here. What *is* asserted is the weaker fact the
/// current code does honour: this route lands on a different status than the
/// cancel route, so the two are at least not conflated.
#[test]
fn a_lead_pane_closed_without_finishing_closes_the_run_and_keeps_its_snapshot() {
    let server = spawn_lead_server("lead-exit");
    let socket = server.socket().to_path_buf();
    let mut events = subscribe(&socket);
    let (_workflow_id, run_id) = launch_lead_run(&server);
    let bound = wait_for_team_binding(&socket, &run_id);

    // Everything the run knew before the lead went away, captured so the
    // retention assertion compares against a real observation rather than
    // against an expectation the projection might never have met.
    let before_nodes = poll_until(
        "the projection to record the team's tasks before the lead exits",
        SETTLE,
        Duration::from_millis(250),
        || {
            let nodes = run_nodes(&socket, &run_id);
            (nodes.contains_key("plan") && nodes.contains_key(EMERGENT_PATH)).then_some(nodes)
        },
    );
    let before_members = poll_until(
        "the projection to snapshot the team's members before the lead exits",
        SETTLE,
        Duration::from_millis(250),
        || {
            let members = run_members(&socket, &run_id);
            members.contains_key(TEAMMATE).then_some(members)
        },
    );

    let lead_pane = bound["run"]["lead_pane_id"]
        .as_str()
        .unwrap_or_else(|| panic!("the bound run named no lead pane: {bound:#}"))
        .to_string();

    // The public route. Not a signal, not a store write — the same call the
    // TUI's close-pane action and any other client would make.
    request_ok(
        &socket,
        &request("req_close", "pane.close", json!({ "pane_id": lead_pane })),
    );

    let closed = wait_for_run(
        &socket,
        &run_id,
        "the run to close after its lead's pane went away",
        |response| {
            matches!(
                response["run"]["status"].as_str(),
                Some("failed") | Some("cancelled") | Some("succeeded")
            )
        },
    );
    assert_eq!(
        closed["run"]["status"], "failed",
        "a lead that vanished without reporting is recorded terminal and distinct from \
         a cancellation, so the two routes are not conflated: {closed:#}"
    );
    assert!(
        closed["run"]["ended_at_unix_ms"]
            .as_u64()
            .is_some_and(|at| at > 0),
        "the close-out must stamp when the run ended: {closed:#}"
    );

    // Nobody asked for this close-out, which is exactly why it has to be
    // announced: a subscriber that only ever polls would otherwise show the
    // run as live until something unrelated moved it.
    let mut seen = Vec::new();
    let finished_event = wait_for_event_matching(
        &mut events,
        &mut seen,
        "workflow_run_finished",
        SETTLE,
        |event| event["data"]["run"]["run_id"] == run_id.as_str(),
    );
    assert_eq!(
        finished_event["data"]["run"]["status"], "failed",
        "the unasked-for close-out is announced with the row it wrote: {finished_event:#}"
    );

    // The retention guarantee. Read back after the run is terminal, from the
    // same public call any client would use.
    let after_nodes = run_nodes(&socket, &run_id);
    for (path, before) in &before_nodes {
        let after = after_nodes.get(path).unwrap_or_else(|| {
            panic!(
                "the run dropped node {path} on close-out; the task snapshot is what makes \
                 a stopped run resumable at all. After: {after_nodes:#?}"
            )
        });
        assert_eq!(
            after["subject"], before["subject"],
            "node {path}'s observed subject must survive the lead's exit"
        );
        assert_eq!(
            after["task_id"], before["task_id"],
            "node {path}'s Claude Code task id must survive the lead's exit"
        );
        assert_eq!(
            after["emergent"], before["emergent"],
            "node {path} must not change kind on close-out"
        );
    }

    let after_members = run_members(&socket, &run_id);
    let teammate = after_members.get(TEAMMATE).unwrap_or_else(|| {
        panic!(
            "the run dropped its member snapshot on close-out; Claude Code deletes the \
             team config when the lead session ends, so this is the only record left of \
             who worked on the run. Before: {before_members:#?}"
        )
    });
    assert_eq!(
        teammate["model"], TEAMMATE_MODEL,
        "the retained member snapshot must keep the teammate's model: {teammate:#}"
    );
    assert!(
        teammate["pane_id"]
            .as_str()
            .is_some_and(|pane| !pane.is_empty()),
        "the retained member snapshot must keep the pane the teammate ran in, which is \
         what a later interrogation is addressed by: {teammate:#}"
    );

    // The team name and lead session id are the two keys a resume needs; a
    // close-out that cleared them would strand the run's own task directory.
    assert_eq!(closed["run"]["team_name"], TEAM_NAME, "{closed:#}");
    assert_eq!(
        closed["run"]["lead_session_id"], LEAD_SESSION_ID,
        "{closed:#}"
    );

    server.shutdown();
}
