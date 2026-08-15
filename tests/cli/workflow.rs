//! Headless CLI e2e for `kvx workflow review` (`.local/prd/phase4-retarget-plan.md`
//! §5 packet **P12**), against a real running server and the real `kvx`
//! binary — no PTY, no `claude`.
//!
//! Driving a full interview/synthesis pane end to end needs a live
//! `claude` (or, at minimum, `KARVEX_WORKFLOW_REVIEW_COMMAND`'s stub-command
//! escape hatch and a real workflow run, which needs a PTY and a lead stub
//! the way `tests/workflow_lead_headless.rs` sets one up). Neither is
//! needed to prove what this packet actually owns: that the five verbs
//! parse, reach the right `workflow.review.*` method over the real socket,
//! and read `KARVEX_WORKFLOW_REVIEW_RUN_ID`/`MEMBER` rather than any
//! CLI flag. Every scenario below is deliberately a **refusal path** —
//! `workflow.review.start`/`.answer`/`.report` all validate their run
//! before touching a pane, so "no such run" and "no live cycle" are both
//! reachable, real, honest server answers with no run ever created.
//!
//! `review show` is the one verb with a real *success* path reachable with
//! no run at all: a run that was never reviewed answers `review: null`, a
//! normal answer rather than an error (`WorkflowReviewGet`'s own doc).

use super::harness::*;

/// Every scenario spins up its own named server with its own
/// `KARVEX_WORKFLOW_DB_PATH` (`workflow/store/mod.rs`'s `DB_PATH_ENV`),
/// isolated under `base` — the workflow store otherwise falls back to the
/// real developer machine's shared `state_dir()`/`workflow`, which any two
/// workflow-touching tests running concurrently (in this suite or another
/// one) would then contend over as `store_locked`
/// (`tests/workflow_lead_headless.rs`'s own precedent for why every server
/// that touches the store redirects it).
fn spawn_solo(base: &Path, config_home: &Path, runtime_dir: &Path) -> SpawnedServerProcess {
    spawn_named_server_with_env(
        config_home,
        runtime_dir,
        "solo",
        &[("KARVEX_WORKFLOW_DB_PATH", &base.join("workflow-db"))],
    )
}

/// `review show` on a run karvex has never heard of still answers
/// successfully with `review: null` — `workflow.review.get` never checks
/// that the run itself exists, only that it has a cycle, matching
/// `workflow.summary.get`'s own "no summary" precedent.
#[test]
fn review_show_on_an_unreviewed_run_answers_no_cycle() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let server = spawn_solo(&base, &config_home, &runtime_dir);
    wait_for_socket(
        &named_session_socket(&config_home, "solo"),
        Duration::from_secs(5),
    );

    let output = run_named_cli(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "solo",
            "workflow",
            "review",
            "show",
            "workflow_run:doesnotexist000000000",
        ],
    );
    assert!(
        output.status.success(),
        "review show on an unreviewed run must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("no review cycle for this run yet"),
        "stdout: {stdout}"
    );

    let json = run_named_cli(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "solo",
            "workflow",
            "review",
            "show",
            "workflow_run:doesnotexist000000000",
            "--json",
        ],
    );
    assert!(json.status.success());
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert!(value["result"]["review"].is_null(), "review: {value}");
    assert_eq!(
        value["result"]["findings"].as_array().map(Vec::len),
        Some(0)
    );

    drop(server);
    cleanup_test_base(&base);
}

/// `review start` on a run karvex never created is refused with
/// `workflow_review_not_found`, naming the run — never a generic error, and
/// never a panic reaching for a live run's team.
#[test]
fn review_start_on_an_unknown_run_is_refused() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let server = spawn_solo(&base, &config_home, &runtime_dir);
    wait_for_socket(
        &named_session_socket(&config_home, "solo"),
        Duration::from_secs(5),
    );

    let output = run_named_cli(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "solo",
            "workflow",
            "review",
            "start",
            "workflow_run:doesnotexist000000000",
            "--json",
        ],
    );
    assert!(
        !output.status.success(),
        "starting a review for an unknown run must not succeed"
    );
    // CLI convention: a server-side error is JSON on stderr with exit 1
    // (`skills/karvex/SKILL.md`'s own rule); stdout carries a success
    // envelope only.
    let value: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(
        value["error"]["code"].as_str(),
        Some("workflow_review_not_found"),
        "response: {value}"
    );
    let message = value["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("workflow_run:doesnotexist000000000"),
        "refusal must name the run: {message}"
    );

    drop(server);
    cleanup_test_base(&base);
}

/// `review apply` on a run karvex never created answers `workflow_not_found`
/// — the run itself is checked before the cycle, distinct from
/// `workflow_review_not_found` (`workflow_review_apply.rs`'s own
/// `ApplyOutcome::NoRun` vs `NoCycle` split).
#[test]
fn review_apply_on_an_unknown_run_is_refused() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let server = spawn_solo(&base, &config_home, &runtime_dir);
    wait_for_socket(
        &named_session_socket(&config_home, "solo"),
        Duration::from_secs(5),
    );

    let output = run_named_cli(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "solo",
            "workflow",
            "review",
            "apply",
            "workflow_run:doesnotexist000000000",
            "--decline-all",
            "--json",
        ],
    );
    assert!(!output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["error"]["code"].as_str(), Some("workflow_not_found"));

    drop(server);
    cleanup_test_base(&base);
}

/// The plan's own contract: bare `apply` (neither `--accept` nor
/// `--decline-all`) is refused **locally**, before any request is sent —
/// exit 2, not a wire round trip, and never a network call at all.
#[test]
fn review_apply_bare_is_refused_locally_with_no_network_call() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let server = spawn_solo(&base, &config_home, &runtime_dir);
    wait_for_socket(
        &named_session_socket(&config_home, "solo"),
        Duration::from_secs(5),
    );

    let output = run_named_cli(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "solo",
            "workflow",
            "review",
            "apply",
            "workflow_run:whatever00000000000000",
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--accept"), "stderr: {stderr}");
    assert!(stderr.contains("--decline-all"), "stderr: {stderr}");

    // `--accept` and `--decline-all` together are refused the same way.
    let both = run_named_cli(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "solo",
            "workflow",
            "review",
            "apply",
            "workflow_run:whatever00000000000000",
            "--accept",
            "plan",
            "--decline-all",
        ],
    );
    assert_eq!(both.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&both.stderr).contains("not both"));

    drop(server);
    cleanup_test_base(&base);
}

/// `review answer`/`review report` take no `--run`/`--member` flag at all —
/// only `KARVEX_WORKFLOW_REVIEW_RUN_ID`/`MEMBER`, which karvex itself
/// exports into a review pane
/// (`.local/prd/phase4-retarget-plan.md`, amendment log). Missing that
/// environment is refused locally, naming the variable, before any request
/// reaches the socket.
#[test]
fn review_answer_and_report_require_their_env_and_take_no_override_flag() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let server = spawn_solo(&base, &config_home, &runtime_dir);
    wait_for_socket(
        &named_session_socket(&config_home, "solo"),
        Duration::from_secs(5),
    );

    let answer_dir = base.join("answer");
    fs::create_dir_all(&answer_dir).unwrap();
    let answer_file = answer_dir.join("answer.json");
    fs::write(&answer_file, "{}").unwrap();
    let answer_file_str = answer_file.to_string_lossy().into_owned();

    // No KARVEX_WORKFLOW_REVIEW_RUN_ID / MEMBER at all: refused locally.
    let output = run_named_cli(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "solo",
            "workflow",
            "review",
            "answer",
            "--file",
            &answer_file_str,
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("KARVEX_WORKFLOW_REVIEW_RUN_ID"),
        "stderr: {stderr}"
    );

    // `--run`/`--member` are not real flags on this verb — an interview pane
    // must not be able to answer for a run or member other than its own.
    let unknown_flag = run_named_cli(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "solo",
            "workflow",
            "review",
            "answer",
            "--file",
            &answer_file_str,
            "--run",
            "workflow_run:elsewhere0000000000000",
        ],
    );
    assert_eq!(unknown_flag.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&unknown_flag.stderr).contains("unknown option"));

    // `review report` has the same env requirement, minus `member`.
    let report = run_named_cli(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "solo",
            "workflow",
            "review",
            "report",
            "--file",
            &answer_file_str,
        ],
    );
    assert_eq!(report.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&report.stderr).contains("KARVEX_WORKFLOW_REVIEW_RUN_ID"),);

    drop(server);
    cleanup_test_base(&base);
}

/// With the env set, `review answer`/`report` reach the real socket naming
/// the run the env named — proven by the wire refusal that comes back
/// (`workflow_review_not_found`: no live cycle on this server, since none
/// was ever started), not by a local parse error.
#[test]
fn review_answer_and_report_send_the_env_run_id_over_the_wire() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let server = spawn_solo(&base, &config_home, &runtime_dir);
    wait_for_socket(
        &named_session_socket(&config_home, "solo"),
        Duration::from_secs(5),
    );

    let doc_dir = base.join("doc");
    fs::create_dir_all(&doc_dir).unwrap();
    let file = doc_dir.join("doc.json");
    fs::write(&file, "{}").unwrap();
    let file_str = file.to_string_lossy().into_owned();

    let run_id_path = std::path::Path::new("workflow_run:fromenv0000000000000000");
    let member_path = std::path::Path::new("scout");

    let answer = run_named_cli_with_env(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "solo",
            "workflow",
            "review",
            "answer",
            "--file",
            &file_str,
            "--json",
        ],
        &[
            ("KARVEX_WORKFLOW_REVIEW_RUN_ID", run_id_path),
            ("KARVEX_WORKFLOW_REVIEW_MEMBER", member_path),
        ],
    );
    assert!(!answer.status.success());
    let value: serde_json::Value = serde_json::from_slice(&answer.stderr).unwrap();
    assert_eq!(
        value["error"]["code"].as_str(),
        Some("workflow_review_not_found"),
        "response: {value}"
    );
    let message = value["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("workflow_run:fromenv0000000000000000"),
        "the run id the CLI read from KARVEX_WORKFLOW_REVIEW_RUN_ID must reach the \
         server verbatim: {message}"
    );

    let report = run_named_cli_with_env(
        &config_home,
        &runtime_dir,
        &[
            "--session",
            "solo",
            "workflow",
            "review",
            "report",
            "--file",
            &file_str,
            "--json",
        ],
        &[("KARVEX_WORKFLOW_REVIEW_RUN_ID", run_id_path)],
    );
    assert!(!report.status.success());
    let value: serde_json::Value = serde_json::from_slice(&report.stderr).unwrap();
    assert_eq!(
        value["error"]["code"].as_str(),
        Some("workflow_review_not_found"),
        "response: {value}"
    );

    drop(server);
    cleanup_test_base(&base);
}
