//! End-to-end tests driving the *real* `kvx` binary through a *real* `tmux`
//! symlink, exercising W2's dispatch/translation table
//! (`src/cli/tmux_compat.rs`) and W1's shim-install guard
//! (`src/platform/tmux_shim.rs`) across an actual process boundary, against a
//! scratch Karvex server. See `docs/design/claude-teammates/01-port-plan.md`
//! §5.10 W7 for the ownership and scope of this file.
//!
//! Every test builds its own `tmux`-named symlink pointing at
//! `env!("CARGO_BIN_EXE_kvx")` and invokes it exactly as Claude Code's
//! `TmuxBackend` would: `TMUX=<socket>,<pid>,0` / `TMUX_PANE=<pane id>` in the
//! environment, `-S <socket>` on argv. This deliberately does **not** go
//! through the product's own shim-install machinery
//! (`platform::ensure_tmux_shim_dir`, which is `pub(crate)` and unreachable
//! from this external test crate) for the dispatch/translation tests —
//! that machinery is W1's own in-file unit-tested surface. What *is* tested
//! here end-to-end is the install machinery's real, non-injected behaviour as
//! exercised by a genuine Managed pane spawn inside a real server process
//! (`shim_dir_contains_only_the_tmux_entry`,
//! `nextest_binary_never_installs_a_tmux_shim`).
//!
//! Per R7 (docs/design/claude-teammates/01-port-plan.md §4), every shim
//! invocation here starts from [`shim_command`], which scrubs the ambient
//! Karvex/tmux env vars first so a developer's own live Karvex/tmux session
//! (if these tests happen to run from inside one) cannot leak into the
//! subprocess under test. `tests/cli/harness.rs` is W7's shared file per
//! §5.10 but is intentionally left untouched by this workstream (see the
//! build report); the equivalent scrub for the *server* spawn side is not
//! applied here for that reason — it is not needed for correctness (every
//! Managed-pane TMUX export is computed fresh and unconditionally overwrites
//! any inherited value, `src/pane.rs` `apply_tmux_compat_env_with_shim_dir`)
//! except for `KARVEX_NO_TMUX_COMPAT`, which this file removes from its own
//! process env defensively before spawning a server — safe because
//! `cargo nextest run` gives every test its own process.

use super::harness::*;

/// Ambient env vars scrubbed from every shim invocation before a test sets
/// exactly what its scenario needs (R7).
const AMBIENT_ENV_VARS_TO_SCRUB: &[&str] = &[
    "KARVEX_SOCKET_PATH",
    "KARVEX_CLIENT_SOCKET_PATH",
    "KARVEX_ENV",
    "KARVEX_STARTUP_CWD",
    "KARVEX_PANE_ID",
    "KARVEX_WORKSPACE_ID",
    "KARVEX_TAB_ID",
    "KARVEX_NO_TMUX_COMPAT",
    "TMUX",
    "TMUX_PANE",
];

/// Creates `<dir>/tmux` as a symlink to the real built `kvx` binary, mirroring
/// what `platform::ensure_tmux_shim_dir` installs into
/// `<data_dir>/shims/tmux` for a live pane.
fn install_tmux_symlink(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir).expect("create shim dir");
    let link = dir.join("tmux");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_kvx"), &link).expect("create tmux symlink");
    link
}

/// A `Command` for the shim binary at `tmux`, with every ambient
/// Karvex/tmux env var scrubbed first (R7) so the caller starts from a clean
/// slate and sets exactly what the scenario needs.
fn shim_command(tmux: &Path) -> Command {
    let mut command = Command::new(tmux);
    for var in AMBIENT_ENV_VARS_TO_SCRUB {
        command.env_remove(var);
    }
    command
}

fn root_pane_id(created: &serde_json::Value) -> String {
    created["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("workspace create should return a root pane id")
        .to_string()
}

fn root_tab_id(created: &serde_json::Value) -> String {
    created["result"]["root_pane"]["tab_id"]
        .as_str()
        .expect("workspace create should return a root pane tab id")
        .to_string()
}

fn workspace_id(created: &serde_json::Value) -> String {
    created["result"]["workspace"]["workspace_id"]
        .as_str()
        .expect("workspace create should return a workspace id")
        .to_string()
}

fn pane_count(socket_path: &Path) -> usize {
    let panes = run_cli_json(socket_path, &["pane", "list"]);
    panes["result"]["panes"]
        .as_array()
        .expect("pane.list should return an array")
        .len()
}

// ---------------------------------------------------------------------------
// Flagship: a full Claude Agent Teams-style lifecycle through the real shim
// ---------------------------------------------------------------------------

#[test]
fn tmux_shim_fakes_a_claude_teammate_lifecycle() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("karvex.sock");
    let shim_dir = base.join("shim");

    let karvex = spawn_karvex(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let leader_pane = root_pane_id(&created);
    let tab_id = root_tab_id(&created);
    let workspace = workspace_id(&created);

    let tmux = install_tmux_symlink(&shim_dir);
    let tmux_env = format!("{},{},0", socket_path.display(), std::process::id());
    let socket_str = socket_path.to_str().unwrap();

    // `-V` never touches the API; the availability probe must always succeed.
    let version = shim_command(&tmux).arg("-V").output().unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        "tmux 3.5a (karvex-compat)"
    );

    // `display-message -p '#{pane_id}'` with no `-t` resolves through
    // `TMUX_PANE`, exactly like Claude's own probes.
    let display = shim_command(&tmux)
        .env("TMUX", &tmux_env)
        .env("TMUX_PANE", &leader_pane)
        .args(["-S", socket_str, "display-message", "-p", "#{pane_id}"])
        .output()
        .unwrap();
    assert!(
        display.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&display.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&display.stdout).trim(),
        leader_pane,
        "Claude parses this stdout literally"
    );

    // `list-panes -t <tab> -F '#{pane_id}'` -> exactly the leader, one line.
    let listed = shim_command(&tmux)
        .env("TMUX", &tmux_env)
        .env("TMUX_PANE", &leader_pane)
        .args([
            "-S",
            socket_str,
            "list-panes",
            "-t",
            &tab_id,
            "-F",
            "#{pane_id}",
        ])
        .output()
        .unwrap();
    assert!(
        listed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&listed.stdout),
        format!("{leader_pane}\n")
    );

    // `split-window -d -t <leader> -h -l 30% -P -F '#{pane_id}'` -> prints the
    // new pane id and the leader stays element 0 of `pane.list`.
    let split = shim_command(&tmux)
        .env("TMUX", &tmux_env)
        .env("TMUX_PANE", &leader_pane)
        .args([
            "-S",
            socket_str,
            "split-window",
            "-d",
            "-t",
            &leader_pane,
            "-h",
            "-l",
            "30%",
            "-P",
            "-F",
            "#{pane_id}",
        ])
        .output()
        .unwrap();
    assert!(
        split.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&split.stderr)
    );
    let new_pane = String::from_utf8_lossy(&split.stdout).trim().to_string();
    assert!(!new_pane.is_empty(), "split-window -P must print a pane id");
    assert_ne!(new_pane, leader_pane);

    let panes = run_cli_json(&socket_path, &["pane", "list", "--workspace", &workspace]);
    let ids: Vec<String> = panes["result"]["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pane| pane["pane_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 2, "leader + one teammate pane");
    assert_eq!(ids[0], leader_pane, "the leader must stay element 0");
    assert_eq!(ids[1], new_pane);

    // `select-pane -t <new> -T teammate-a` -> the pane's label changes.
    let select = shim_command(&tmux)
        .env("TMUX", &tmux_env)
        .env("TMUX_PANE", &leader_pane)
        .args([
            "-S",
            socket_str,
            "select-pane",
            "-t",
            &new_pane,
            "-T",
            "teammate-a",
        ])
        .output()
        .unwrap();
    assert!(
        select.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&select.stderr)
    );
    let got = run_cli_json(&socket_path, &["pane", "get", &new_pane]);
    assert_eq!(got["result"]["pane"]["label"].as_str(), Some("teammate-a"));

    // `send-keys -t <new> echo hi Enter`.
    let send = shim_command(&tmux)
        .env("TMUX", &tmux_env)
        .env("TMUX_PANE", &leader_pane)
        .args([
            "-S",
            socket_str,
            "send-keys",
            "-t",
            &new_pane,
            "echo",
            "hi-from-send-keys",
            "Enter",
        ])
        .output()
        .unwrap();
    assert!(
        send.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&send.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(3), Duration::from_millis(50), || {
            pane_read_recent_contains(&socket_path, &new_pane, "hi-from-send-keys")
        }),
        "send-keys must run inside the pane's shell"
    );

    // `respawn-pane -k -t <new> -- <command>`.
    let respawn = shim_command(&tmux)
        .env("TMUX", &tmux_env)
        .env("TMUX_PANE", &leader_pane)
        .args([
            "-S",
            socket_str,
            "respawn-pane",
            "-k",
            "-t",
            &new_pane,
            "--",
            "echo teammate-ready",
        ])
        .output()
        .unwrap();
    assert!(
        respawn.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&respawn.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(3), Duration::from_millis(50), || {
            pane_read_recent_contains(&socket_path, &new_pane, "teammate-ready")
        }),
        "respawn-pane must submit the command into the pane's shell"
    );

    // `kill-pane -t <new>` -> back to one pane.
    let kill = shim_command(&tmux)
        .env("TMUX", &tmux_env)
        .env("TMUX_PANE", &leader_pane)
        .args(["-S", socket_str, "kill-pane", "-t", &new_pane])
        .output()
        .unwrap();
    assert!(
        kill.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&kill.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(3), Duration::from_millis(50), || {
            pane_count(&socket_path) == 1
        }),
        "kill-pane must close the teammate pane"
    );

    cleanup_spawned_karvex(karvex, base);
}

// ---------------------------------------------------------------------------
// send-keys key-spec translation: a real Ctrl-C interrupt, not literal text
// ---------------------------------------------------------------------------

/// Regression coverage for the defect this shim's `send-keys` handler was
/// fixed for: tmux key specs like `C-c` were being typed into the pane as
/// the literal three characters "C-c" instead of an actual Ctrl-C keystroke.
/// A foreground `sleep 30` blocks the shell from acting on any further input
/// until it exits or is interrupted, so if `C-c` reaches the pane as a real
/// interrupt, `echo interrupted-ok` runs almost immediately; if it were still
/// typed as literal text, the shell would not see it until the real 30s
/// sleep finished, well past this test's wait window.
#[test]
fn tmux_shim_send_keys_translates_ctrl_c_to_a_real_interrupt() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("karvex.sock");
    let shim_dir = base.join("shim");

    let karvex = spawn_karvex(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let leader_pane = root_pane_id(&created);

    let tmux = install_tmux_symlink(&shim_dir);
    let tmux_env = format!("{},{},0", socket_path.display(), std::process::id());
    let socket_str = socket_path.to_str().unwrap();

    let start = shim_command(&tmux)
        .env("TMUX", &tmux_env)
        .env("TMUX_PANE", &leader_pane)
        .args([
            "-S",
            socket_str,
            "send-keys",
            "-t",
            &leader_pane,
            "sh -c 'echo READY-FOR-CTRL-C && sleep 30'",
            "Enter",
        ])
        .output()
        .unwrap();
    assert!(
        start.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(3), Duration::from_millis(50), || {
            pane_read_recent_contains(&socket_path, &leader_pane, "READY-FOR-CTRL-C")
        }),
        "the foreground sleep must have started before the interrupt is sent"
    );

    let interrupt = shim_command(&tmux)
        .env("TMUX", &tmux_env)
        .env("TMUX_PANE", &leader_pane)
        .args(["-S", socket_str, "send-keys", "-t", &leader_pane, "C-c"])
        .output()
        .unwrap();
    assert!(
        interrupt.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&interrupt.stderr)
    );

    let confirm = shim_command(&tmux)
        .env("TMUX", &tmux_env)
        .env("TMUX_PANE", &leader_pane)
        .args([
            "-S",
            socket_str,
            "send-keys",
            "-t",
            &leader_pane,
            "echo",
            "interrupted-ok",
            "Enter",
        ])
        .output()
        .unwrap();
    assert!(
        confirm.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&confirm.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(50), || {
            pane_read_recent_contains(&socket_path, &leader_pane, "interrupted-ok")
        }),
        "C-c must reach the pane as a real interrupt, not literal text"
    );

    cleanup_spawned_karvex(karvex, base);
}

// ---------------------------------------------------------------------------
// respawn-pane positional form (no `--` separator)
// ---------------------------------------------------------------------------

/// Real tmux accepts `respawn-pane [-k] [-t target-pane] [shell-command]`
/// without a `--` separator too; Claude's own backend always sends the `--`
/// form (covered by the flagship lifecycle test above), but the positional
/// form must not silently report success without running anything.
#[test]
fn tmux_shim_respawn_pane_runs_positional_command_without_separator() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("karvex.sock");
    let shim_dir = base.join("shim");

    let karvex = spawn_karvex(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let leader_pane = root_pane_id(&created);

    let tmux = install_tmux_symlink(&shim_dir);
    let tmux_env = format!("{},{},0", socket_path.display(), std::process::id());
    let socket_str = socket_path.to_str().unwrap();

    let split = shim_command(&tmux)
        .env("TMUX", &tmux_env)
        .env("TMUX_PANE", &leader_pane)
        .args([
            "-S",
            socket_str,
            "split-window",
            "-d",
            "-t",
            &leader_pane,
            "-h",
            "-l",
            "30%",
            "-P",
            "-F",
            "#{pane_id}",
        ])
        .output()
        .unwrap();
    assert!(
        split.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&split.stderr)
    );
    let new_pane = String::from_utf8_lossy(&split.stdout).trim().to_string();

    // `respawn-pane -k -t <new> echo teammate-ready-positional` — no `--`.
    let respawn = shim_command(&tmux)
        .env("TMUX", &tmux_env)
        .env("TMUX_PANE", &leader_pane)
        .args([
            "-S",
            socket_str,
            "respawn-pane",
            "-k",
            "-t",
            &new_pane,
            "echo",
            "teammate-ready-positional",
        ])
        .output()
        .unwrap();
    assert!(
        respawn.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&respawn.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(3), Duration::from_millis(50), || {
            pane_read_recent_contains(&socket_path, &new_pane, "teammate-ready-positional")
        }),
        "respawn-pane's positional form (no `--`) must submit the command into the pane's \
         shell, not silently no-op"
    );

    cleanup_spawned_karvex(karvex, base);
}

// ---------------------------------------------------------------------------
// Version probe without a server
// ---------------------------------------------------------------------------

#[test]
fn tmux_shim_reports_version_without_a_server() {
    let base = unique_test_dir();
    let shim_dir = base.join("shim");
    let tmux = install_tmux_symlink(&shim_dir);

    let output = shim_command(&tmux).arg("-V").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "tmux 3.5a (karvex-compat)"
    );
    assert!(output.stderr.is_empty());

    cleanup_test_base(&base);
}

// ---------------------------------------------------------------------------
// Passthrough gates
// ---------------------------------------------------------------------------

#[test]
fn tmux_shim_passes_through_named_socket_invocation() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("karvex.sock");
    let shim_dir = base.join("shim");
    let empty_path_dir = base.join("empty-path");
    fs::create_dir_all(&empty_path_dir).unwrap();

    let karvex = spawn_karvex(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let leader_pane = root_pane_id(&created);

    let tmux = install_tmux_symlink(&shim_dir);
    let tmux_env = format!("{},{},0", socket_path.display(), std::process::id());

    // `-L <name>` is Claude's external-session mode; even a serviced-looking
    // verb must never reach the Karvex API under it. `PATH` is pinned to a
    // directory with no real `tmux` so the fallback is deterministic.
    let output = shim_command(&tmux)
        .env("TMUX", &tmux_env)
        .env("TMUX_PANE", &leader_pane)
        .env("PATH", &empty_path_dir)
        .args(["-L", "claude-external", "kill-pane", "-t", &leader_pane])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no server running"),
        "unexpected stderr: {stderr}"
    );

    // Proof the API was never touched: the leader pane still exists.
    assert_eq!(pane_count(&socket_path), 1);

    cleanup_spawned_karvex(karvex, base);
}

#[test]
fn tmux_shim_ignores_mismatched_socket_argument() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("karvex.sock");
    let shim_dir = base.join("shim");
    let empty_path_dir = base.join("empty-path");
    fs::create_dir_all(&empty_path_dir).unwrap();

    let karvex = spawn_karvex(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let leader_pane = root_pane_id(&created);

    let tmux = install_tmux_symlink(&shim_dir);
    let tmux_env = format!("{},{},0", socket_path.display(), std::process::id());
    let other_socket = runtime_dir.join("not-karvex.sock");

    let output = shim_command(&tmux)
        .env("TMUX", &tmux_env)
        .env("TMUX_PANE", &leader_pane)
        .env("PATH", &empty_path_dir)
        .args([
            "-S",
            other_socket.to_str().unwrap(),
            "kill-pane",
            "-t",
            &leader_pane,
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no server running"),
        "unexpected stderr: {stderr}"
    );

    assert_eq!(pane_count(&socket_path), 1);

    cleanup_spawned_karvex(karvex, base);
}

// ---------------------------------------------------------------------------
// Dead server (D3)
// ---------------------------------------------------------------------------

#[test]
fn tmux_shim_version_succeeds_with_the_server_socket_dead() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("karvex.sock");
    let shim_dir = base.join("shim");

    let mut karvex = spawn_karvex(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let tmux = install_tmux_symlink(&shim_dir);

    let stopped = run_cli(&socket_path, &["server", "stop"]);
    assert!(stopped.status.success());
    let pid = karvex.child.process_id();
    karvex.child.wait().unwrap();
    unregister_spawned_karvex_pid(pid);

    let started = Instant::now();
    let output = shim_command(&tmux).arg("-V").output().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "must short-circuit before any socket work"
    );
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "tmux 3.5a (karvex-compat)"
    );

    drop(karvex);
    cleanup_test_base(&base);
}

#[test]
fn tmux_shim_serviced_verb_fails_fast_when_the_server_socket_is_dead() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("karvex.sock");
    let shim_dir = base.join("shim");

    let mut karvex = spawn_karvex(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let leader_pane = root_pane_id(&created);
    let tmux = install_tmux_symlink(&shim_dir);

    let stopped = run_cli(&socket_path, &["server", "stop"]);
    assert!(stopped.status.success());
    let pid = karvex.child.process_id();
    karvex.child.wait().unwrap();
    unregister_spawned_karvex_pid(pid);

    let started = Instant::now();
    let output = shim_command(&tmux)
        .env("KARVEX_SOCKET_PATH", &socket_path)
        .args(["kill-pane", "-t", &leader_pane])
        .output()
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "must fail fast rather than hang out to (or past) the shim's own 1500ms budget"
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "no JSON envelope on stdout");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.trim(), "no server running");
    assert!(
        !stderr.contains('{') && !stderr.contains('}'),
        "no JSON envelope on stderr: {stderr}"
    );

    drop(karvex);
    cleanup_test_base(&base);
}

// ---------------------------------------------------------------------------
// Socket resolution (D5)
// ---------------------------------------------------------------------------

#[test]
fn tmux_shim_uses_the_socket_from_tmux_env_not_the_default_session() {
    let base_a = unique_test_dir();
    let config_a = base_a.join("config");
    let runtime_a = base_a.join("runtime");
    let socket_a = runtime_a.join("karvex.sock");
    let karvex_a = spawn_karvex(&config_a, &runtime_a, &socket_a);
    wait_for_socket(&socket_a, Duration::from_secs(5));

    let base_b = unique_test_dir();
    let config_b = base_b.join("config");
    let runtime_b = base_b.join("runtime");
    let socket_b = runtime_b.join("karvex.sock");
    let karvex_b = spawn_karvex(&config_b, &runtime_b, &socket_b);
    wait_for_socket(&socket_b, Duration::from_secs(5));

    let created_b = run_cli_json(
        &socket_b,
        &["workspace", "create", "--cwd", base_b.to_str().unwrap()],
    );
    let leader_b = root_pane_id(&created_b);

    // The symlink itself can live anywhere; its location is unrelated to
    // which server it ends up talking to.
    let shim_dir = base_a.join("shim");
    let tmux = install_tmux_symlink(&shim_dir);
    let tmux_env = format!("{},{},0", socket_b.display(), std::process::id());

    // `$KARVEX_SOCKET_PATH` is deliberately left unset: only `$TMUX` names a
    // socket, and it names server B's.
    let output = shim_command(&tmux)
        .env("TMUX", &tmux_env)
        .env("TMUX_PANE", &leader_b)
        .args(["display-message", "-p", "#{pane_id}"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), leader_b);

    // Server A never got a workspace created on it, so it staying at zero
    // panes corroborates the request landed on B, not A / not the default
    // session.
    assert_eq!(pane_count(&socket_a), 0);

    cleanup_spawned_karvex(karvex_a, base_a);
    cleanup_spawned_karvex(karvex_b, base_b);
}

// ---------------------------------------------------------------------------
// Shim install (W1, exercised end to end through a real server process) — D6/R8
// ---------------------------------------------------------------------------

#[test]
fn shim_dir_contains_only_the_tmux_entry() {
    // Per R7: the opt-out now short-circuits *before* the shim install runs
    // (`src/pane.rs` `apply_tmux_compat_env_with_shim_dir`), so an ambient
    // `KARVEX_NO_TMUX_COMPAT` inherited from a developer's shell would make
    // this test's positive expectation unreachable. Safe without a lock:
    // `cargo nextest run` gives every test its own process.
    std::env::remove_var("KARVEX_NO_TMUX_COMPAT");

    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("karvex.sock");

    let karvex = spawn_karvex(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    // A Managed pane spawn (the workspace's root pane) is the only trigger
    // for `platform::ensure_tmux_shim_dir()`.
    run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );

    // The shim dir is per-server state, so it lives beside that server's API
    // socket rather than in the shared config directory.
    let shims_dir = server_state_dir(&socket_path).join("shims");
    let link = shims_dir.join("tmux");
    assert!(
        wait_until(Duration::from_secs(2), Duration::from_millis(25), || {
            link.exists()
        }),
        "the shim symlink should appear once a Managed pane has spawned"
    );

    let entries: Vec<_> = fs::read_dir(&shims_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        entries,
        vec![std::ffi::OsString::from("tmux")],
        "R8: the shims dir sits ahead of everything on every managed pane's PATH \
         and must never contain anything but the single `tmux` entry"
    );

    let metadata = fs::symlink_metadata(&link).unwrap();
    assert!(metadata.file_type().is_symlink());
    let target = fs::read_link(&link).unwrap();
    assert_eq!(
        target.canonicalize().unwrap(),
        PathBuf::from(env!("CARGO_BIN_EXE_kvx"))
            .canonicalize()
            .unwrap(),
        "the shim must point at the real kvx binary"
    );

    cleanup_spawned_karvex(karvex, base);
}

/// Mirrors cargo's own naming convention for compiled test binaries under
/// `target/*/deps` (`kvx-<hash>` / `karvex-<hash>`) by running the real,
/// fully-functional `kvx` binary under a copied/hard-linked path whose file
/// stem is *not* exactly `kvx`. `platform::tmux_shim::binary_owns_shim`
/// requires an **exact** stem match (D6), so a Managed pane spawned by a
/// process running under this name must never install a shim — this is the
/// regression class the donor bakr needed commit `7d69200` to learn.
///
/// This is the real, non-injected `ensure_tmux_shim_dir` code path exercised
/// through an actual process boundary; W1's in-process unit test
/// (`ensure_tmux_shim_dir_platform_refuses_non_kvx_test_binary`) proves the
/// same guard but cannot reach across a process boundary the way this test
/// does, and this test crate has no `pub(crate)` visibility into
/// `platform::tmux_shim` to call it directly.
#[test]
fn nextest_binary_never_installs_a_tmux_shim() {
    // Defensive per R7: this test's own positive-path expectations would be
    // defeated by an ambient opt-out inherited from a developer's shell.
    // Safe without a lock: `cargo nextest run` gives every test its own
    // process.
    std::env::remove_var("KARVEX_NO_TMUX_COMPAT");

    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("karvex.sock");
    let impostor_dir = base.join("bin");
    fs::create_dir_all(&impostor_dir).unwrap();
    // A stem matching cargo's own hash-suffixed test-binary convention.
    let impostor = impostor_dir.join("kvx-9f2a1cdeadbe");
    let real = PathBuf::from(env!("CARGO_BIN_EXE_kvx"));
    // Prefer a hard link (no extra disk — the real binary is sizeable and
    // disk is tight); fall back to a copy when `/tmp` and the build output
    // are on different filesystems (the common case here).
    if std::fs::hard_link(&real, &impostor).is_err() {
        fs::copy(&real, &impostor).expect("copy the real kvx binary under an impostor name");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&impostor).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&impostor, perms).unwrap();
        }
    }

    fs::create_dir_all(config_home.join(app_dir_name())).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    register_runtime_dir(&runtime_dir);
    fs::write(
        config_home.join(app_dir_name()).join("config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let mut server = Command::new(&impostor)
        .arg("server")
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .env("KARVEX_SOCKET_PATH", &socket_path)
        .env("SHELL", "/bin/sh")
        .env_remove("KARVEX_CLIENT_SOCKET_PATH")
        .env_remove("KARVEX_ENV")
        .env_remove("KARVEX_NO_TMUX_COMPAT")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the impostor-named server");
    register_spawned_karvex_pid(Some(server.id()));
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    assert!(
        root_pane_id(&created).starts_with("w"),
        "the pane must still spawn normally even though the shim install is refused"
    );

    let shim_path = server_state_dir(&socket_path).join("shims").join("tmux");
    assert!(
        !shim_path.exists(),
        "a binary whose stem is not exactly `kvx` must never install the tmux shim"
    );

    let stopped = run_cli(&socket_path, &["server", "stop"]);
    assert!(stopped.status.success());
    let _ = server.wait();
    unregister_spawned_karvex_pid(Some(server.id()));
    cleanup_test_base(&base);
}

// ---------------------------------------------------------------------------
// PATH containment
// ---------------------------------------------------------------------------

/// The shim-dir prepend (`apply_tmux_compat_env_with_shim_dir`) only ever
/// mutates the `CommandBuilder` used to spawn a *pane's* child process; the
/// server's own process environment must never see it. `/proc/<pid>/environ`
/// gives an outside, black-box view of the running server's actual env, so
/// this is the one part of R8 that is genuinely only checkable from outside
/// the process.
#[cfg(target_os = "linux")]
#[test]
fn tmux_shim_does_not_leak_path_into_the_server_environment() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("karvex.sock");
    let controlled_path = Path::new("/usr/bin:/bin");

    let karvex = spawn_karvex_with_path(
        &config_home,
        &runtime_dir,
        &socket_path,
        Some(controlled_path),
    );
    wait_for_socket(&socket_path, Duration::from_secs(5));

    // Trigger a Managed pane spawn, the only code path that ever prepends
    // the shim dir onto a `PATH` — but only onto the *pane's* env.
    run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );

    let pid = karvex.child.process_id().expect("server pid");
    let environ =
        fs::read(format!("/proc/{pid}/environ")).expect("read the live server's own environ");
    let server_path = environ
        .split(|byte| *byte == 0)
        .find_map(|entry| {
            let entry = std::str::from_utf8(entry).ok()?;
            entry.strip_prefix("PATH=").map(str::to_string)
        })
        .expect("the server's environ carries a PATH entry");

    assert_eq!(
        server_path, "/usr/bin:/bin",
        "the shim dir must never be prepended onto the server's own PATH, \
         only onto a spawned pane's"
    );

    cleanup_spawned_karvex(karvex, base);
}
