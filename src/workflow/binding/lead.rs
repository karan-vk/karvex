//! Launching the run's Claude Code team lead, and binding the run to the team
//! it creates.
//!
//! `09-agent-teams-rework.md` §3.1. A run is now one interactive `claude`
//! session in a karvex pane, orchestrating a Claude Code agent team whose
//! teammates are themselves karvex panes. This module owns the three things
//! that has to get right — the preflight, the argv/env, and the binding — and
//! keeps them pure so each is testable without a PTY.
//!
//! Everything here is a pure function over values. `spawn::spawn_node_pane`'s
//! sibling for the lead lives in the `App` glue, because only that layer knows
//! the geometry.
//!
//! ## Why the teammate mode is forced
//!
//! Claude Code's default teammate mode is `in-process`, not `auto`, and the
//! spawn path short-circuits to in-process *before* backend detection ever
//! runs — so being inside karvex's tmux shim is not enough on its own. Both
//! `--teammate-mode tmux` and `teammateMode: "tmux"` in `--settings` were
//! verified live against 2.1.226 to produce a teammate with
//! `backendType: "tmux"` and a karvex pane id. Both are passed: the flag is
//! experimental and hidden, the settings key is the stable spelling, and
//! neither is load-bearing alone.
//!
//! ## Why the binding is an assertion rather than a search
//!
//! Verified live against 2.1.226: `claude --session-id <uuid>` does **not**
//! determine the lead session id the team is named after, so karvex cannot know
//! the team name up front. It used to *guess* it, from a `createdAt` inside a
//! slack window plus a matching cwd.
//!
//! It no longer has to. Claude Code exports each session's identity and inbox
//! to its hooks before any hook runs, so the run's own `--settings` carries a
//! `SessionStart` hook that reports the session id back — and the team name
//! follows from it by a documented derivation. That decision lives in
//! [`super::identity`], which also keeps the old search as a documented
//! fallback for a lead whose hook never fired.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::workflow::model::RunId;
use crate::workflow::tier::Assignment;

/// The first Claude Code release with both agent teams and cross-session
/// messaging (`09-agent-teams-rework.md` §1). `workflow.run` refuses to launch
/// a lead below this, because the failure mode otherwise is a lead that starts
/// fine and silently never spawns a teammate.
pub const MIN_CLAUDE_VERSION: (u32, u32, u32) = (2, 1, 224);

/// Enables agent teams. Also the gate on `--teammate-mode` being accepted at
/// all, so the two always travel together.
pub const TEAMS_ENV_VAR: &str = "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS";

/// The file `render_lead_prompt`'s output is written to inside the run
/// directory, and the file the lead's initial input points at.
pub const LEAD_PROMPT_FILE: &str = "lead-prompt.md";

/// Where the lead is told to write its run summary before calling
/// `kvx workflow run finish`.
pub const LEAD_SUMMARY_FILE: &str = "summary.md";

/// The name the run's lead is addressed by in karvex's own messaging surface.
///
/// Deliberately the role rather than the Claude Code session name: a client
/// steering "the lead" should not have to know what karvex called the session,
/// and the team roster already uses `team-lead` for the same member.
pub const LEAD_TARGET_NAME: &str = "team-lead";

/// How far before karvex's own spawn instant a team's `createdAt` may sit and
/// still be believed to be this run's. Absorbs clock granularity and the gap
/// between karvex stamping the spawn and the pane's `claude` actually starting;
/// deliberately small, because the whole point is to not adopt a team that was
/// already there.
///
/// Only the *fallback* binding rule uses this. The primary rule is the lead's
/// own `SessionStart` assertion, in [`super::identity`], which needs no window
/// at all. The matching ceiling lives there too, beside the bind deadline it is
/// derived from.
pub const TEAM_MATCH_SLACK_MS: u64 = 15_000;

/// The run-scoped Claude Code settings payload, written into the run directory
/// and passed as `--settings`.
///
/// A file rather than the inline JSON this used to be, because it now carries
/// the run's `SessionStart` identity hook as well as the teammate mode, and
/// because Claude Code forwards the *value* of `--settings` to the teammates it
/// spawns — verified in the 2.1.232 bundle's teammate argv builder, which
/// re-emits `--settings <value>`. A path forwards cleanly; a multi-line JSON
/// blob through a shell command string does not.
pub const LEAD_SETTINGS_FILE: &str = "claude-settings.json";

/// Everything the lead's pane needs, resolved. Pure data, so the argv and env
/// are testable without a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadSpawnSpec {
    pub run_id: RunId,
    /// The workflow's display name, used only for the pane title.
    pub workflow_name: String,
    /// The run directory, `KARVEX_WORKFLOW_RUNS_DIR/<run id>/`.
    pub run_dir: PathBuf,
    /// Where the lead's pane starts, which is also what [`match_team`]
    /// recognises the team by.
    pub cwd: PathBuf,
    /// The lead's own model/effort, resolved from the run's tier.
    pub assignment: Assignment,
}

impl LeadSpawnSpec {
    pub fn prompt_path(&self) -> PathBuf {
        self.run_dir.join(LEAD_PROMPT_FILE)
    }

    pub fn summary_path(&self) -> PathBuf {
        self.run_dir.join(LEAD_SUMMARY_FILE)
    }

    pub fn settings_path(&self) -> PathBuf {
        self.run_dir.join(LEAD_SETTINGS_FILE)
    }

    /// The name this run's lead answers to, for `/list-agents` and for a human
    /// looking at a pane title.
    pub fn session_name(&self) -> String {
        super::identity::lead_session_name(&self.run_id)
    }

    /// The pane's manual label. The sidebar renders this, and teammates get
    /// their own names from the team config's `tmuxPaneId` entries.
    pub fn pane_title(&self) -> String {
        format!("lead: {}", self.workflow_name)
    }
}

/// Why a lead could not be launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeadSpawnError {
    /// `claude --version` could not be run at all.
    ClaudeUnavailable(String),
    /// `claude --version` printed something with no version in it.
    ClaudeVersionUnreadable(String),
    /// Installed, but older than agent teams.
    ClaudeTooOld {
        found: String,
        required: String,
    },
    /// The run directory or the rendered prompt could not be written.
    RunDirUnwritable(String),
    /// Claude Code has not been told this directory is trusted, so the lead
    /// would open on the folder-trust dialog instead of on its plan.
    CwdNotTrusted(String),
    /// The run's workspace has no pane to split the lead off.
    NoTargetPane,
    PaneLaunchFailed(String),
}

impl fmt::Display for LeadSpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClaudeUnavailable(message) => write!(
                f,
                "a workflow run needs the `claude` CLI on PATH and it could not be run: {message}"
            ),
            Self::ClaudeVersionUnreadable(output) => write!(
                f,
                "`claude --version` printed no recognisable version: {output:?}"
            ),
            Self::ClaudeTooOld { found, required } => write!(
                f,
                "a workflow run needs Claude Code {required} or newer for agent teams; \
                 this machine has {found}. Run `claude update` and try again."
            ),
            Self::RunDirUnwritable(message) => {
                write!(f, "the run directory could not be prepared: {message}")
            }
            Self::CwdNotTrusted(cwd) => write!(
                f,
                "Claude Code has not been told {cwd} is trusted, so a run started here would \
                 open on its folder-trust prompt and never see its plan. Run `claude` in that \
                 directory once, answer \"Yes, I trust this folder\", then start the run."
            ),
            Self::NoTargetPane => {
                f.write_str("the run's workspace has no pane to split the lead off")
            }
            Self::PaneLaunchFailed(message) => {
                write!(f, "the lead's pane failed to launch: {message}")
            }
        }
    }
}

impl std::error::Error for LeadSpawnError {}

/// This machine's `claude` cannot run a workflow's team lead: absent,
/// unreadable, or older than agent teams.
pub const LEAD_UNAVAILABLE_CODE: &str = "workflow_lead_unavailable";
/// `claude` is fine; the lead's pane or run directory could not be made.
pub const LEAD_SPAWN_FAILED_CODE: &str = "workflow_lead_spawn_failed";

impl LeadSpawnError {
    /// The wire error code. Reuses the workflow subsystem's existing vocabulary
    /// rather than minting per-variant codes a client would have to learn.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ClaudeUnavailable(_)
            | Self::ClaudeVersionUnreadable(_)
            | Self::ClaudeTooOld { .. }
            | Self::CwdNotTrusted(_) => LEAD_UNAVAILABLE_CODE,
            Self::RunDirUnwritable(_) | Self::NoTargetPane | Self::PaneLaunchFailed(_) => {
                LEAD_SPAWN_FAILED_CODE
            }
        }
    }
}

// ── preflight ──────────────────────────────────────────────────────────────

/// Parses the `2.1.226 (Claude Code)` shape `claude --version` prints.
///
/// Deliberately lenient about everything but the leading `major.minor.patch`:
/// the suffix is decoration and has changed before, and refusing to launch a
/// run over a reworded suffix would be a worse failure than the one the check
/// exists to prevent.
pub fn parse_claude_version(output: &str) -> Option<(u32, u32, u32)> {
    let token = output.split_whitespace().find(|token| {
        token
            .split('.')
            .next()
            .is_some_and(|head| !head.is_empty() && head.chars().all(|ch| ch.is_ascii_digit()))
    })?;
    let mut parts = token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    // A trailing pre-release marker (`226-rc1`) still yields a usable patch.
    let patch_token = parts.next()?;
    let patch_digits: String = patch_token
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let patch = patch_digits.parse().ok()?;
    Some((major, minor, patch))
}

/// Whether a parsed version clears [`MIN_CLAUDE_VERSION`].
pub fn version_is_supported(found: (u32, u32, u32)) -> bool {
    found >= MIN_CLAUDE_VERSION
}

pub fn version_string(version: (u32, u32, u32)) -> String {
    format!("{}.{}.{}", version.0, version.1, version.2)
}

/// The whole preflight, as a pure function over `claude --version`'s output so
/// the policy is tested without running a subprocess. The `App` glue runs the
/// command and hands the result here.
pub fn check_claude_version(output: &str) -> Result<(u32, u32, u32), LeadSpawnError> {
    let Some(found) = parse_claude_version(output) else {
        return Err(LeadSpawnError::ClaudeVersionUnreadable(
            output.trim().to_string(),
        ));
    };
    if !version_is_supported(found) {
        return Err(LeadSpawnError::ClaudeTooOld {
            found: version_string(found),
            required: version_string(MIN_CLAUDE_VERSION),
        });
    }
    Ok(found)
}

/// Whether Claude Code will open in `cwd` without its folder-trust dialog.
///
/// This is a preflight rather than a nicety, because of how the dialog fails:
/// verified live, a lead spawned into an untrusted directory shows the trust
/// prompt *and discards its initial prompt entirely*. Answering the dialog
/// leaves a perfectly healthy `claude` sitting at an empty prompt with no plan
/// and no error — a run that looks started and will never do anything. Failing
/// the launch with an actionable message is strictly better.
///
/// Reads `~/.claude.json`'s `projects.<cwd>.hasTrustDialogAccepted`. A file
/// that cannot be read or parsed answers `true`: refusing every run because a
/// foreign config moved would be a worse failure than the one this prevents.
pub fn cwd_is_trusted(claude_json: Option<&str>, cwd: &Path) -> bool {
    let Some(text) = claude_json else {
        return true;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return true;
    };
    let Some(projects) = value.get("projects").and_then(|p| p.as_object()) else {
        return true;
    };
    let Some(entry) = projects.get(&cwd.to_string_lossy().into_owned()) else {
        return false;
    };
    entry
        .get("hasTrustDialogAccepted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

// ── argv and env ───────────────────────────────────────────────────────────

/// The subcommand the run's `SessionStart` hook calls back with.
///
/// Deliberately a `kvx` verb rather than a shipped shell asset like
/// `assets/claude/karvex-agent-state.sh`. The asset pattern exists because that
/// hook is *installed once* into the user's settings and has to keep working
/// across karvex upgrades, which is what the `KARVEX_INTEGRATION_VERSION`
/// migration rule governs. This hook is written fresh into the run directory on
/// every launch, so there is no installed copy to migrate; and routing it
/// through `kvx` — the same binary that is already on the lead's `PATH` for
/// `kvx workflow run finish` — means the payload parsing lives in Rust, is unit
/// tested, and behaves identically on Windows without a second PowerShell
/// implementation to keep in step.
pub const IDENTITY_HOOK_VERB: &str = "workflow run report-session";

/// The hook command string Claude Code will run through a shell.
///
/// The run id is baked into the command rather than read from the environment
/// on purpose: Claude Code forwards `--settings` to the teammates it spawns,
/// but a teammate's pane is created by karvex's tmux shim and carries karvex's
/// own base environment, *not* the lead's — so `KARVEX_WORKFLOW_RUN_ID` does
/// not reach a teammate. Baking the run id into the settings file is what makes
/// a teammate's self-report land on the right run.
pub fn identity_hook_command(kvx_executable: &Path, run_id: &RunId) -> String {
    format!(
        "{} {IDENTITY_HOOK_VERB} --run {}",
        quote_for_hook_shell(&kvx_executable.display().to_string()),
        quote_for_hook_shell(&run_id.0),
    )
}

#[cfg(not(windows))]
fn quote_for_hook_shell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(windows)]
fn quote_for_hook_shell(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
}

/// The run-scoped `--settings` document.
///
/// Three keys, each load-bearing and each verified live against 2.1.232:
///
/// * `teammateMode: "tmux"` — the default is `in-process` even inside tmux, and
///   in-process teammates do not survive `/resume`. Passed here *and* as
///   `--teammate-mode`, because the flag is experimental and hidden while the
///   settings key is the stable spelling, and neither is load-bearing alone.
/// * `hooks.SessionStart` — the identity assertion. Hook entries from a
///   `--settings` payload are *added* to the user's own hooks rather than
///   replacing them: the probe's hook ran as `sessionstart-hook-3.sh`, third of
///   the three registered, so karvex's own agent-state hook keeps working in
///   the lead's pane.
/// * `crossSessionInbound: "accept"` — karvex is the lead's parent, not its
///   child, so a karvex message is an ordinary peer message and its delivery
///   would otherwise depend on the lead's permission mode. This is upstream's
///   documented knob for exactly this case.
pub fn lead_settings_document(hook_command: &str) -> serde_json::Value {
    serde_json::json!({
        "teammateMode": "tmux",
        "crossSessionInbound": "accept",
        "hooks": {
            "SessionStart": [
                {
                    "matcher": "*",
                    "hooks": [
                        {
                            "type": "command",
                            "command": hook_command,
                            "timeout": IDENTITY_HOOK_TIMEOUT_SECONDS,
                        }
                    ]
                }
            ]
        }
    })
}

/// How long Claude Code will wait for the identity hook.
///
/// The hook writes one line to a Unix socket karvex is already listening on;
/// the live probe's whole `SessionStart` hook chain took under 10 ms. Ten
/// seconds is the same budget the installed agent-state hook uses and leaves
/// room for a loaded machine without holding a session's startup open.
pub const IDENTITY_HOOK_TIMEOUT_SECONDS: u32 = 10;

/// The lead's argv.
///
/// Deliberately carries **no** initial prompt. A positional prompt was the
/// obvious spelling and it does not survive: verified live, an interactive
/// `claude` launched into a fresh pane with a positional prompt comes up at an
/// empty input box, and the plan is simply gone — a run that looks started and
/// never does anything. karvex already knows this failure mode for node panes
/// (`an_agent_that_never_saw_its_seed_prompt_is_reseeded_with_an_absolute_path`).
/// The lead is therefore seeded once through `agent.prompt` after its session
/// is up, which is how karvex steers every other agent and is observable when
/// it fails.
///
/// `--name` is what makes the run's lead addressable: it is the name
/// `/list-agents` lists and `SendMessage` routes by, and without it Claude Code
/// derives one from the cwd's folder name — identical for every run in the same
/// repository.
pub fn lead_argv(spec: &LeadSpawnSpec) -> Vec<String> {
    vec![
        crate::detect::interactive_agent_executable(crate::detect::Agent::Claude).to_string(),
        "--name".to_string(),
        spec.session_name(),
        "--teammate-mode".to_string(),
        "tmux".to_string(),
        "--settings".to_string(),
        spec.settings_path().to_string_lossy().into_owned(),
        "--model".to_string(),
        spec.assignment.model.as_str().to_string(),
        "--effort".to_string(),
        spec.assignment.effort.as_str().to_string(),
        "--add-dir".to_string(),
        spec.run_dir.to_string_lossy().into_owned(),
    ]
}

/// The lead's opening instruction, delivered into its pane once its session is
/// up. Points at the rendered file rather than inlining a multi-kilobyte plan.
pub fn lead_seed_prompt(prompt_path: &Path) -> String {
    format!(
        "Read {} and follow it. It is your plan for this workflow run.",
        prompt_path.display()
    )
}

/// The lead pane's environment.
///
/// `KARVEX_WORKFLOW_RUN_ID` is what makes `kvx workflow run finish` need no
/// argument: the lead's own pane self-identifies. `KARVEX_ENV`,
/// `KARVEX_PANE_ID`, and the socket path are injected by the pane launch path
/// already, which is also what puts karvex's `tmux` shim on the lead's PATH.
pub fn lead_env(spec: &LeadSpawnSpec) -> Vec<(String, String)> {
    vec![
        (TEAMS_ENV_VAR.to_string(), "1".to_string()),
        (
            super::spawn::RUN_ID_ENV_VAR.to_string(),
            spec.run_id.0.clone(),
        ),
    ]
}

// ── binding ────────────────────────────────────────────────────────────────

/// A team karvex has recognised as this run's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeadBinding {
    pub team_name: String,
    pub lead_session_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::tier::{Effort, ModelAlias};

    fn spec() -> LeadSpawnSpec {
        LeadSpawnSpec {
            run_id: RunId::new("workflow_run:abc"),
            workflow_name: "parser-work".to_string(),
            run_dir: PathBuf::from("/runs/abc"),
            cwd: PathBuf::from("/home/dev/project"),
            assignment: Assignment {
                model: ModelAlias::Opus,
                effort: Effort::High,
            },
        }
    }

    #[test]
    fn the_installed_version_string_parses() {
        assert_eq!(
            parse_claude_version("2.1.226 (Claude Code)"),
            Some((2, 1, 226))
        );
    }

    #[test]
    fn a_prerelease_patch_still_parses() {
        assert_eq!(
            parse_claude_version("2.1.230-rc1 (Claude Code)"),
            Some((2, 1, 230))
        );
    }

    #[test]
    fn output_with_no_version_is_unreadable_rather_than_old() {
        let error = check_claude_version("command not found").expect_err("no version");
        assert!(matches!(error, LeadSpawnError::ClaudeVersionUnreadable(_)));
    }

    #[test]
    fn the_preflight_floor_is_the_agent_teams_release() {
        assert!(version_is_supported(MIN_CLAUDE_VERSION));
        assert!(version_is_supported((2, 1, 226)));
        assert!(version_is_supported((2, 2, 0)));
        assert!(!version_is_supported((2, 1, 223)));
        assert!(!version_is_supported((2, 0, 999)));
    }

    #[test]
    fn a_too_old_claude_is_refused_with_both_versions_named() {
        let error = check_claude_version("2.1.221 (Claude Code)").expect_err("too old");
        match error {
            LeadSpawnError::ClaudeTooOld { found, required } => {
                assert_eq!(found, "2.1.221");
                assert_eq!(required, "2.1.224");
            }
            other => panic!("expected ClaudeTooOld, got {other:?}"),
        }
        // The message names the fix, not just the problem.
        let message = LeadSpawnError::ClaudeTooOld {
            found: "2.1.221".to_string(),
            required: "2.1.224".to_string(),
        }
        .to_string();
        assert!(message.contains("claude update"));
    }

    #[test]
    fn a_directory_claude_has_been_trusted_in_passes_the_preflight() {
        let config = r#"{"projects":{"/home/dev/project":{"hasTrustDialogAccepted":true}}}"#;
        assert!(cwd_is_trusted(Some(config), Path::new("/home/dev/project")));
    }

    #[test]
    fn an_untrusted_or_unseen_directory_fails_the_preflight() {
        let config = r#"{"projects":{"/home/dev/project":{"hasTrustDialogAccepted":false}}}"#;
        assert!(!cwd_is_trusted(
            Some(config),
            Path::new("/home/dev/project")
        ));
        assert!(!cwd_is_trusted(Some(config), Path::new("/home/dev/other")));
        // The flag missing entirely is not consent either.
        let bare = r#"{"projects":{"/home/dev/project":{}}}"#;
        assert!(!cwd_is_trusted(Some(bare), Path::new("/home/dev/project")));
    }

    #[test]
    fn an_unreadable_claude_config_does_not_block_every_run() {
        assert!(cwd_is_trusted(None, Path::new("/home/dev/project")));
        assert!(cwd_is_trusted(
            Some("not json"),
            Path::new("/home/dev/project")
        ));
        assert!(cwd_is_trusted(Some("{}"), Path::new("/home/dev/project")));
    }

    #[test]
    fn the_untrusted_message_names_the_directory_and_the_fix() {
        let message = LeadSpawnError::CwdNotTrusted("/home/dev/project".to_string()).to_string();
        assert!(message.contains("/home/dev/project"));
        assert!(message.contains("Yes, I trust this folder"));
    }

    #[test]
    fn the_argv_forces_split_pane_teammates_both_ways() {
        let argv = lead_argv(&spec());
        let flag = argv
            .iter()
            .position(|arg| arg == "--teammate-mode")
            .expect("the flag is passed");
        assert_eq!(argv[flag + 1], "tmux");
        let settings = argv
            .iter()
            .position(|arg| arg == "--settings")
            .expect("the settings key is passed");
        assert_eq!(argv[settings + 1], "/runs/abc/claude-settings.json");
        assert_eq!(
            lead_settings_document("kvx")["teammateMode"],
            serde_json::json!("tmux")
        );
    }

    #[test]
    fn the_argv_names_the_run_so_the_lead_is_addressable() {
        let argv = lead_argv(&spec());
        let name = argv
            .iter()
            .position(|arg| arg == "--name")
            .expect("the lead is named");
        assert_eq!(argv[name + 1], "karvex-run-abc");
        assert_eq!(argv[name + 1], spec().session_name());
    }

    #[test]
    fn the_settings_document_carries_the_identity_hook_and_the_inbound_policy() {
        let command = identity_hook_command(
            Path::new("/usr/local/bin/kvx"),
            &RunId::new("workflow_run:abc"),
        );
        let document = lead_settings_document(&command);
        // Upstream's own knob for a session that should take messages without
        // its permission mode deciding: karvex is a peer, not a child.
        assert_eq!(document["crossSessionInbound"], serde_json::json!("accept"));
        let hook = &document["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(hook["type"], serde_json::json!("command"));
        assert_eq!(
            hook["timeout"],
            serde_json::json!(IDENTITY_HOOK_TIMEOUT_SECONDS)
        );
        let command = hook["command"].as_str().expect("a command string");
        assert!(command.contains("workflow run report-session"));
        // The run id rides in the command, not the environment: teammate panes
        // are made by karvex's shim and never see the lead's environment.
        assert!(command.contains("workflow_run:abc"));
        assert_eq!(document["hooks"]["SessionStart"][0]["matcher"], "*");
    }

    #[cfg(not(windows))]
    #[test]
    fn a_hook_command_quotes_a_path_a_shell_would_otherwise_split() {
        let command = identity_hook_command(
            Path::new("/home/some one/bin/kvx"),
            &RunId::new("workflow_run:abc"),
        );
        assert!(command.starts_with("'/home/some one/bin/kvx'"));
        // A quote inside the path cannot end the quoting.
        let nasty = identity_hook_command(
            Path::new("/home/o'brien/kvx"),
            &RunId::new("workflow_run:abc"),
        );
        assert!(nasty.starts_with(r#"'/home/o'"'"'brien/kvx'"#), "{nasty}");
    }

    #[test]
    fn the_settings_file_lives_in_the_run_directory() {
        assert_eq!(
            spec().settings_path(),
            PathBuf::from("/runs/abc/claude-settings.json")
        );
    }

    #[test]
    fn the_argv_carries_the_lead_assignment_and_no_positional_prompt() {
        let argv = lead_argv(&spec());
        assert!(argv.windows(2).any(|w| w[0] == "--model" && w[1] == "opus"));
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--effort" && w[1] == "high"));
        // Not `-p`: the lead is interactive on purpose (§3.1).
        assert!(!argv.iter().any(|arg| arg == "-p" || arg == "--print"));
        // And no positional prompt. A launched-with prompt is dropped on the
        // floor by an interactive `claude` in a fresh pane (verified live), so
        // the plan is delivered afterwards through `agent.prompt` instead.
        assert!(
            !argv.iter().any(|arg| arg.contains("lead-prompt.md")),
            "the plan must not ride in argv: {argv:?}"
        );
        assert_eq!(argv.last().map(String::as_str), Some("/runs/abc"));
    }

    #[test]
    fn the_seed_prompt_names_the_rendered_plan() {
        let text = lead_seed_prompt(&spec().prompt_path());
        assert!(text.contains("/runs/abc/lead-prompt.md"));
        assert!(text.contains("follow it"));
    }

    #[test]
    fn the_env_enables_teams_and_self_identifies_the_run() {
        let env = lead_env(&spec());
        assert!(env.contains(&(TEAMS_ENV_VAR.to_string(), "1".to_string())));
        assert!(env.contains(&(
            "KARVEX_WORKFLOW_RUN_ID".to_string(),
            "workflow_run:abc".to_string()
        )));
    }

    #[test]
    fn the_run_dir_layout_is_the_one_the_prompt_promises() {
        let spec = spec();
        assert_eq!(
            spec.prompt_path(),
            PathBuf::from("/runs/abc/lead-prompt.md")
        );
        assert_eq!(spec.summary_path(), PathBuf::from("/runs/abc/summary.md"));
    }
}
