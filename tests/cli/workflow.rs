//! End-to-end coverage for `kvx workflow node expand` — the real binary
//! against a real server, per WS-I's "Tested" section
//! (`docs/design/workflow-builder/06-phase2-plan.md`). The `#![cfg(not(target_os
//! = "macos"))]` that exempts pane-driven workflow suites on macOS is declared
//! once on the crate root (`tests/cli.rs:1`) and inherited here — see that
//! file's doc comment before adding another.
//!
//! Everything below is `runner = "command"`, the same offline, deterministic
//! binding `tests/workflow_headless.rs` uses and for the same reason: the node
//! scripts are plain processes that write `result.json` and call `kvx
//! workflow node complete`, so no network, no `claude`, and no API cost. The
//! one addition here is that the proposing nodes' own scripts call `kvx
//! workflow node expand` on themselves, mid-run, exactly as
//! `04-kvdag-and-execution.md` §3.4 describes — reading their own
//! `KARVEX_WORKFLOW_NODE_TOKEN`, the same credential `node complete` reads.
//!
//! This suite spawns its own headless server (no PTY needed: `runner =
//! "command"` nodes are plain child processes, and the server itself does not
//! need a controlling terminal) rather than reusing `super::harness`'s
//! `spawn_karvex*` helpers, because those do not expose the workflow-specific
//! environment (`KARVEX_WORKFLOW_DB_PATH`/`KARVEX_WORKFLOW_RUNS_DIR`) or a
//! `PATH` carrying the `kvx` under test — both of which a node's own `kvx
//! workflow node complete`/`node expand` call needs to resolve correctly.
//!
//! **What this does *not* assert.** `WorkflowRunInfo.growth_limited` and
//! `WorkflowRunNodeInfo.growth_limited` are still hard-coded to `None` in
//! `src/app/workflow.rs`'s `workflow_run_info`/`workflow_node_info` as of this
//! writing ("nothing records a growth limit yet" / "WS-E projects the
//! recorded one at step 3a") — the live projection is cross-workstream, owned
//! by whichever step wires `GrowthLimited` emission into those two functions.
//! So `run show`/`node show`'s `growth:`/`growth_limited:` lines
//! (`format_run_growth_line`/`format_node_growth_limited_line` in
//! `src/cli/workflow.rs`) are proven correct against synthetic JSON in that
//! module's own `#[cfg(test)]` tests, and this suite only smoke-tests that
//! `run show`/`node show` still render cleanly once real growth events have
//! happened — not that a `growth:` line appears yet. What *is* fully live
//! here is the `node expand` verb's own response: `accepted`/`rejected`,
//! partial acceptance, and the guardrail each rejection names, all of which
//! the handler builds directly rather than through that stub.

use super::harness::*;
use std::os::unix::fs::PermissionsExt;

// ── a headless, workflow-capable server with `kvx` on its own PATH ─────────

struct WorkflowServer {
    base: PathBuf,
    socket: PathBuf,
    child: std::process::Child,
}

impl Drop for WorkflowServer {
    fn drop(&mut self) {
        let pid = self.child.id();
        let _ = self.child.kill();
        let _ = self.child.wait();
        unregister_spawned_karvex_pid(Some(pid));
    }
}

impl WorkflowServer {
    fn socket(&self) -> &Path {
        &self.socket
    }

    fn shutdown(self) {
        let base = self.base.clone();
        drop(self);
        cleanup_test_base(&base);
    }
}

fn spawn_workflow_server() -> WorkflowServer {
    let base = unique_test_dir();
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

    // A node's own `kvx workflow node complete`/`node expand` call resolves
    // `kvx` through `PATH` (`04-kvdag-and-execution.md` §4.3), so this has to
    // be the binary under test, not whatever the developer has installed.
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_kvx"), bin_dir.join("kvx")).unwrap();
    let path_override = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut command = Command::new(env!("CARGO_BIN_EXE_kvx"));
    command
        .arg("server")
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("XDG_STATE_HOME", &state_home)
        .env("KARVEX_SOCKET_PATH", &socket)
        .env("KARVEX_WORKFLOW_DB_PATH", base.join("workflow-db"))
        .env("KARVEX_WORKFLOW_RUNS_DIR", base.join("workflow-runs"))
        .env("PATH", &path_override)
        .env("SHELL", "/bin/sh")
        .env_remove("KARVEX_CLIENT_SOCKET_PATH")
        .env_remove("KARVEX_ENV")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child = command.spawn().unwrap();
    register_spawned_karvex_pid(Some(child.id()));

    let server = WorkflowServer {
        base,
        socket,
        child,
    };
    wait_for_socket(server.socket(), Duration::from_secs(20));
    server
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut perms = fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).unwrap();
}

/// Waits for `workflow.run.get` to report a terminal `status`, polling over
/// the CLI exactly as an operator would with `kvx workflow run show`.
fn wait_for_run_terminal(socket: &Path, run_id: &str, timeout: Duration) -> serde_json::Value {
    let mut last = serde_json::Value::Null;
    let reached = wait_until(timeout, Duration::from_millis(100), || {
        let response = run_cli_json(socket, &["workflow", "run", "show", run_id, "--json"]);
        let status = response["result"]["run"]["status"]
            .as_str()
            .unwrap_or("")
            .to_string();
        last = response;
        matches!(status.as_str(), "succeeded" | "failed" | "cancelled")
    });
    assert!(
        reached,
        "run {run_id} never reached a terminal status: {last}"
    );
    last
}

/// Reads back a node's `expand_out.txt` — the captured stdout of that node's
/// own `kvx workflow node expand` invocation — via the node's `node_dir` as
/// reported by `workflow.node.get`, never by guessing the run-directory
/// layout: `node_dir` is the one API-observable path to it.
fn read_expand_output(socket: &Path, run_id: &str, path: &str) -> String {
    let node = run_cli_json(
        socket,
        &["workflow", "node", "show", run_id, path, "--json"],
    );
    let node_dir = node["result"]["node"]["node_dir"]
        .as_str()
        .unwrap_or_else(|| panic!("{path} has no node_dir: {node}"));
    fs::read_to_string(Path::new(node_dir).join("expand_out.txt"))
        .unwrap_or_else(|err| panic!("failed to read expand_out.txt for {path}: {err}"))
}

const DEFINITION_TEMPLATE: &str = r#"
name = "cli-node-expand-e2e"
description = "WS-I kvx workflow node expand end-to-end fixture"
max_depth = 3
max_nodes = 12

[[arg]]
name = "goal"
default = "cli-e2e"

[[node]]
key = "root_ok"
label = "RootOk"
runner = "command"
command = ["/bin/sh", "@ROOT_OK@"]
prompt_template = "root_ok"
output_schema = { type = "object" }
expand_allow = ["worker"]
expand_max = 2

[[node]]
key = "root_bad"
label = "RootBad"
runner = "command"
command = ["/bin/sh", "@ROOT_BAD@"]
prompt_template = "root_bad"
output_schema = { type = "object" }

[[node]]
key = "worker"
label = "Worker"
runner = "command"
command = ["/bin/sh", "@WORKER@"]
prompt_template = "worker"
output_schema = { type = "object" }
is_template = true
"#;

/// `root_ok`'s script: proposes 4 children of `worker` against its own
/// `expand_max = 2`, so this exercises §4 D2's partial-acceptance case
/// end to end — 2 created, 2 short, reported rather than silently truncated.
/// The default (human) rendering is captured, so this half of the pair also
/// covers the human `print_workflow_node_expand_response` path.
const ROOT_OK_SCRIPT: &str = r#"#!/bin/sh
set -u
printf '{}' > "$KARVEX_WORKFLOW_NODE_DIR/result.json"
kvx workflow node expand "$KARVEX_WORKFLOW_RUN_ID" "$KARVEX_WORKFLOW_NODE_PATH" \
  --template worker --label Worker --count 4 \
  > "$KARVEX_WORKFLOW_NODE_DIR/expand_out.txt" 2>&1
kvx workflow node complete
while :; do sleep 1; done
"#;

/// `root_bad`'s script: no `expand_allow`, so every proposal is refused
/// before it ever reaches a guardrail (`ExpandRejection::NotAllowed`) — zero
/// nodes created, not even a partial one. `--json` is captured here, so the
/// pair covers both rendering modes of `node expand`'s own response.
const ROOT_BAD_SCRIPT: &str = r#"#!/bin/sh
set -u
printf '{}' > "$KARVEX_WORKFLOW_NODE_DIR/result.json"
kvx workflow node expand "$KARVEX_WORKFLOW_RUN_ID" "$KARVEX_WORKFLOW_NODE_PATH" \
  --template worker --label Worker --json \
  > "$KARVEX_WORKFLOW_NODE_DIR/expand_out.txt" 2>&1
kvx workflow node complete
while :; do sleep 1; done
"#;

const WORKER_SCRIPT: &str = r#"#!/bin/sh
set -u
printf '{}' > "$KARVEX_WORKFLOW_NODE_DIR/result.json"
kvx workflow node complete
while :; do sleep 1; done
"#;

#[test]
fn node_expand_grows_and_rejects_across_run_and_node_show() {
    let server = spawn_workflow_server();
    let socket = server.socket().to_path_buf();

    let scripts_dir = server.base.join("scripts");
    fs::create_dir_all(&scripts_dir).unwrap();
    let root_ok_path = scripts_dir.join("root_ok.sh");
    let root_bad_path = scripts_dir.join("root_bad.sh");
    let worker_path = scripts_dir.join("worker.sh");
    write_script(&root_ok_path, ROOT_OK_SCRIPT);
    write_script(&root_bad_path, ROOT_BAD_SCRIPT);
    write_script(&worker_path, WORKER_SCRIPT);

    let definition = DEFINITION_TEMPLATE
        .replace("@ROOT_OK@", &root_ok_path.to_string_lossy())
        .replace("@ROOT_BAD@", &root_bad_path.to_string_lossy())
        .replace("@WORKER@", &worker_path.to_string_lossy());
    let definition_path = server.base.join("definition.toml");
    fs::write(&definition_path, &definition).unwrap();

    // Every run needs somewhere to host its panes.
    let workspace_created = run_cli_json(
        &socket,
        &[
            "workspace",
            "create",
            "--cwd",
            server.base.to_str().unwrap(),
            "--focus",
        ],
    );
    assert!(
        workspace_created["result"]["workspace"]["workspace_id"]
            .as_str()
            .is_some(),
        "workspace.create failed: {workspace_created}"
    );

    let created = run_cli_json(
        &socket,
        &[
            "workflow",
            "create",
            "--file",
            definition_path.to_str().unwrap(),
            "--json",
        ],
    );
    let workflow_id = created["result"]["workflow"]["workflow_id"]
        .as_str()
        .unwrap_or_else(|| panic!("workflow.create returned no workflow_id: {created}"))
        .to_string();

    let started = run_cli_json(
        &socket,
        &["workflow", "run", "start", &workflow_id, "--json"],
    );
    let run_id = started["result"]["run"]["run_id"]
        .as_str()
        .unwrap_or_else(|| panic!("workflow.run returned no run_id: {started}"))
        .to_string();

    let finished = wait_for_run_terminal(&socket, &run_id, Duration::from_secs(45));
    assert_eq!(
        finished["result"]["run"]["status"], "succeeded",
        "unexpected terminal status: {finished}"
    );

    // ── root_ok: partial acceptance, human rendering ────────────────────
    let root_ok_output = read_expand_output(&socket, &run_id, "root_ok");
    assert!(
        root_ok_output.contains("accepted: 2"),
        "root_ok expand output: {root_ok_output}"
    );
    assert!(
        root_ok_output.contains("root_ok/worker/1") && root_ok_output.contains("root_ok/worker/2"),
        "root_ok expand output should name both accepted children: {root_ok_output}"
    );
    assert!(
        root_ok_output.contains("rejected: 1"),
        "root_ok expand output: {root_ok_output}"
    );
    assert!(
        root_ok_output.contains("template=worker reason=truncated"),
        "root_ok expand output: {root_ok_output}"
    );
    assert!(
        root_ok_output.contains("2 of 4"),
        "the shortfall message should be printed, not just the reason: {root_ok_output}"
    );

    // The children the response claimed really exist and really ran.
    for child in ["root_ok/worker/1", "root_ok/worker/2"] {
        let node = run_cli_json(
            &socket,
            &["workflow", "node", "show", &run_id, child, "--json"],
        );
        assert_eq!(
            node["result"]["node"]["status"], "succeeded",
            "expansion child {child} did not run: {node}"
        );
    }

    // ── root_bad: whole-proposal rejection, --json rendering ────────────
    let root_bad_output = read_expand_output(&socket, &run_id, "root_bad");
    let root_bad_json: serde_json::Value = serde_json::from_str(root_bad_output.trim())
        .unwrap_or_else(|err| {
            panic!("root_bad expand output was not JSON: {err}\n{root_bad_output}")
        });
    assert_eq!(
        root_bad_json["result"]["type"], "workflow_node_expanded",
        "a rejection is a success response, not an error: {root_bad_json}"
    );
    assert!(
        root_bad_json["result"]["accepted"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "a disallowed template creates nothing: {root_bad_json}"
    );
    let rejected = root_bad_json["result"]["rejected"].as_array().unwrap();
    assert_eq!(rejected.len(), 1, "{root_bad_json}");
    assert_eq!(rejected[0]["reason"], "not_allowed");
    assert_eq!(rejected[0]["template"], "worker");

    // Zero side effects: no `root_bad/worker/*` node was ever created.
    // `print_response` (`src/cli.rs`) writes an error envelope to stderr with
    // a non-zero exit even under `--json` — `--json` only changes how a
    // *success* renders, never where an error goes.
    let missing = run_cli(
        &socket,
        &[
            "workflow",
            "node",
            "show",
            &run_id,
            "root_bad/worker/1",
            "--json",
        ],
    );
    assert!(
        !missing.status.success(),
        "a disallowed proposal must not create root_bad/worker/1: {missing:?}"
    );
    let missing_stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(
        missing_stderr.contains("workflow_not_found"),
        "unexpected refusal for root_bad/worker/1: {missing_stderr}"
    );

    // `run show`/`node show` render cleanly (human and `--json`) once real
    // growth/rejection events have happened, whether or not the live
    // `growth_limited` projection is wired yet (see the module doc comment).
    let run_show_human = run_cli(&socket, &["workflow", "run", "show", &run_id]);
    assert!(run_show_human.status.success(), "{run_show_human:?}");
    let node_show_human = run_cli(&socket, &["workflow", "node", "show", &run_id, "root_ok"]);
    assert!(node_show_human.status.success(), "{node_show_human:?}");

    server.shutdown();
}
