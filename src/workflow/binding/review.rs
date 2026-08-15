//! Runtime binding for self-improvement review interviews: forked-session
//! argv/env, same layering as [`super::lead`].
//!
//! `phase4-retarget-plan.md` P6, informed end to end by spike S2
//! (`.local/prd/spike-S2-findings.md`, verified against Claude Code 2.1.233).
//! `workflow::review` (P5) decides *who* gets interviewed, in what mode, and
//! ranks them; this module owns the *how*: the argv that revives a member's
//! session (or, evidence-only, starts a fresh one), the env that lets the
//! pane self-report, and the cycle directory layout every path in the P5
//! prompts points at. The adapter (wave 2b, out of this packet's scope)
//! spawns the pane, writes the rendered prompt file, and polls for the
//! answer.
//!
//! Everything here is a pure function over values, [`super::lead`]'s
//! contract exactly: no filesystem, no store, no panes. The one file write a
//! spawn needs (the rendered `interview-<member>.md`) is the same
//! directory-create-plus-write primitive [`super::spawn::write_run_context`]
//! already is, reused by the adapter rather than duplicated here.
//!
//! ## S2's three binding amendments, and how this file answers each
//!
//! 1. **A permission arrangement is mandatory, not optional.** With the
//!    argv `phase4-retarget-plan.md` originally specified — `--resume …
//!    --fork-session --add-dir … --model … --effort …` and nothing else —
//!    the live probe stalled at `Do you want to create answers/<member>.json?`
//!    and then at a Bash approval, and karvex observed `agent_status:
//!    blocked` (P5's [`crate::workflow::review::EvidenceOnlyReason::
//!    InterviewBlocked`] exists for exactly this pane state). Verified
//!    working and frozen here as [`INTERVIEW_PERMISSION_MODE`] /
//!    [`INTERVIEW_ALLOWED_TOOL`]: `--permission-mode acceptEdits` (clears the
//!    `Write` dialog for the answer file) plus a scoped `--allowedTools`
//!    entry (clears the `Bash` dialog for the one command the P5 prompt
//!    tells the teammate to run, `kvx workflow review answer --file …` —
//!    `review_prompt.rs`'s `render_answer_contract`). The probe's own
//!    example was the broader `Bash(kvx …:*)`; this file scopes tighter, to
//!    the exact subcommand the prompt asks for, because an interview pane's
//!    only sanctioned `Bash` use *is* that self-report and a narrower
//!    allow-list is strictly safer for the same verified mechanism.
//! 2. **The interview pane's cwd is the cycle directory; the member's
//!    project directory is only `--add-dir`'d.** This is a correction to the
//!    plan's own quoted argv shape (`--add-dir <cycle dir>`), not an
//!    addition: S2 fact 3 found that every forked session mints its own
//!    Claude Code team (`~/.claude/teams/session-<forksid8>/config.json`,
//!    `cwd` = the pane's cwd), and a team whose leader cwd equals a run's
//!    lead cwd is exactly what [`super::identity`]'s weakest fallback rule
//!    (a team created inside the spawn window whose leader cwd matches) is
//!    looking for. Giving the interview pane the cycle directory as its cwd
//!    — never the member's project directory — makes that collision
//!    structurally impossible rather than merely unlikely, and `--resume` is
//!    proven cwd-independent (S2 probe 4), so it costs nothing. The `--add-dir`
//!    target is therefore the member's project directory
//!    (`InterviewSpawnSpec::member_project_dir`), and the caller is
//!    responsible for starting the pane *in* `InterviewSpawnSpec::cycle_dir`
//!    — the same split [`super::lead::LeadSpawnSpec::cwd`] and its own
//!    `--add-dir spec.run_dir` already keep, mirrored.
//! 3. **The run's `--settings` payload must not reach an interview.** An
//!    interview pane is not a member of the run's team and must never claim
//!    to be one, so neither `--settings` nor `--teammate-mode` appears in
//!    either argv here — there is nothing to "drop", because neither
//!    function ever emits them. This does not blind karvex to the fork's
//!    identity: the `SessionStart` hook that reports a session id back lives
//!    in the *user's own* `~/.claude/settings.json`, not in a
//!    `--settings` payload (S2 fact 4), so it fires in every pane karvex
//!    starts regardless.
//!
//! ## On the plan's "no positional prompt, ever" premise
//!
//! S2 also found that a positional prompt *does* survive `--resume …
//! --fork-session` — it is only the fresh-launch path that drops it. Checked
//! against [`super::lead::lead_argv`]'s own doc comment: it already scopes
//! the claim to "an interactive `claude` launched into a fresh pane", which
//! is accurate on both paths (the lead is always a fresh launch) and was not
//! stated as a universal law, so no correction was needed there. Neither
//! function below carries a positional prompt regardless of which path it
//! uses, for the reason S2 itself keeps: seeding through `agent.prompt`
//! ([`interview_seed_prompt`]) is how karvex steers every pane and is
//! observable when it fails, independent of which launch path would have
//! carried a prompt anyway.

use std::path::{Path, PathBuf};

use crate::workflow::model::{InterrogationId, ReviewCycleId};
use crate::workflow::tier::Assignment;

// ── cycle directory layout ──────────────────────────────────────────────────
//
// `<run dir>/review/<cycle>/{interview-<member>.md, answers/<member>.json,
// synthesis.md, findings.json}` — the layout every path in `review_prompt.rs`
// (`InterviewPromptInput::{cycle_dir,answer_path}`,
// `SynthesisPromptInput::{cycle_dir,findings_path}`) is rendered against, so a
// caller that builds paths any other way produces a prompt whose own promises
// about where things live are wrong.

/// The run directory's review subtree, sibling of `context/`
/// ([`super::spawn::CONTEXT_DIR`]).
pub const REVIEW_DIR: &str = "review";
/// Where an interviewed member's answer JSON lands, inside the cycle
/// directory.
pub const ANSWERS_DIR: &str = "answers";
/// The synthesis pane's rendered prompt file, at the cycle directory's root.
pub const SYNTHESIS_PROMPT_FILE: &str = "synthesis.md";
/// Where the synthesis pane's findings report lands, at the cycle directory's
/// root.
pub const FINDINGS_FILE: &str = "findings.json";

/// `<run dir>/review/<cycle>` — becomes the interview (and synthesis) pane's
/// cwd. `cycle`'s id is sanitised the same way [`super::spawn::run_dir`]
/// sanitises a run id: a store id carries a `review_cycle:` prefix and the
/// colon must never reach the filesystem.
pub fn cycle_dir(run_dir: &Path, cycle: &ReviewCycleId) -> PathBuf {
    run_dir
        .join(REVIEW_DIR)
        .join(super::spawn::path_segment(cycle.as_str()))
}

/// `<cycle dir>/interview-<member>.md` — the rendered
/// [`crate::workflow::review_prompt::render_interview_prompt`] output, and
/// what [`interview_seed_prompt`] points the pane at.
pub fn interview_prompt_path(cycle_dir: &Path, member: &str) -> PathBuf {
    cycle_dir.join(format!(
        "interview-{}.md",
        super::spawn::path_segment(member)
    ))
}

/// `<cycle dir>/answers/<member>.json` — where the P5 prompt's answer
/// contract (`review_prompt.rs::render_answer_contract`) tells the teammate
/// to write, and what `kvx workflow review answer --file …` is pointed at.
pub fn answer_path(cycle_dir: &Path, member: &str) -> PathBuf {
    cycle_dir
        .join(ANSWERS_DIR)
        .join(format!("{}.json", super::spawn::path_segment(member)))
}

/// `<cycle dir>/synthesis.md`.
pub fn synthesis_prompt_path(cycle_dir: &Path) -> PathBuf {
    cycle_dir.join(SYNTHESIS_PROMPT_FILE)
}

/// `<cycle dir>/findings.json`.
pub fn findings_path(cycle_dir: &Path) -> PathBuf {
    cycle_dir.join(FINDINGS_FILE)
}

// ── the spawn spec ───────────────────────────────────────────────────────────

/// Everything one interview pane's argv/env/paths need, resolved. Pure data,
/// the [`super::lead::LeadSpawnSpec`] precedent: the adapter builds one of
/// these from `workflow::review`'s [`crate::workflow::review::
/// InterviewAssignment`] plus the run's directory, and every function below
/// is testable against it without a pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterviewSpawnSpec {
    /// The team-roster name this interview is conducted with — the same
    /// `InterviewAssignment::member`.
    pub member: String,
    /// `<run dir>/review/<cycle>`. The caller starts the pane *in* this
    /// directory (S2 amendment 2, module doc); nothing here passes it as
    /// `--add-dir` or any other argv flag.
    pub cycle_dir: PathBuf,
    /// The member's own working directory, granted only via `--add-dir`.
    /// `None` when karvex never captured one (`InterviewAssignment::
    /// member_cwd`), in which case neither argv function emits the flag at
    /// all — an interview with no known project directory still runs, just
    /// without that grant.
    pub member_project_dir: Option<PathBuf>,
    /// The interview pane's own model/effort, resolved the same way the
    /// lead's is.
    pub assignment: Assignment,
}

impl InterviewSpawnSpec {
    pub fn prompt_path(&self) -> PathBuf {
        interview_prompt_path(&self.cycle_dir, &self.member)
    }

    pub fn answer_path(&self) -> PathBuf {
        answer_path(&self.cycle_dir, &self.member)
    }

    /// The pane's manual label, the sidebar's row.
    pub fn pane_title(&self) -> String {
        format!("review: {}", self.member)
    }
}

// ── permission arrangement (S2 amendment 1) ─────────────────────────────────

/// Clears the `Write` approval dialog for the answer file. Without it the
/// fork stops unattended at "Do you want to create answers/<member>.json?"
/// (verified live, S2 probe 2/3).
pub const INTERVIEW_PERMISSION_MODE: &str = "acceptEdits";

/// Clears the `Bash` approval dialog for exactly the one command the P5
/// prompt tells the teammate to run
/// (`review_prompt.rs::render_answer_contract`: `kvx workflow review answer
/// --file <path>`) and nothing broader. Verified live in shape (S2 probe 3
/// proved `--allowedTools 'Bash(kvx pane:*)'` silences the dialog for a
/// scoped `kvx` subcommand); this file narrows the scope to the interview's
/// own self-report rather than reusing the probe's diagnostic wildcard.
pub const INTERVIEW_ALLOWED_TOOL: &str = "Bash(kvx workflow review answer:*)";

fn permission_tail() -> Vec<String> {
    vec![
        "--permission-mode".to_string(),
        INTERVIEW_PERMISSION_MODE.to_string(),
        "--allowedTools".to_string(),
        INTERVIEW_ALLOWED_TOOL.to_string(),
    ]
}

fn claude_executable() -> &'static str {
    crate::detect::interactive_agent_executable(crate::detect::Agent::Claude)
}

/// The tail both argv shapes share: `--add-dir` (only when a member project
/// directory is known), `--model`/`--effort`, then the permission
/// arrangement. Deliberately carries no `--settings`/`--teammate-mode` (S2
/// amendment 3, module doc) and no positional prompt.
fn shared_tail(spec: &InterviewSpawnSpec) -> Vec<String> {
    let mut argv = Vec::new();
    if let Some(dir) = &spec.member_project_dir {
        argv.push("--add-dir".to_string());
        argv.push(dir.to_string_lossy().into_owned());
    }
    argv.push("--model".to_string());
    argv.push(spec.assignment.model.as_str().to_string());
    argv.push("--effort".to_string());
    argv.push(spec.assignment.effort.as_str().to_string());
    argv.extend(permission_tail());
    argv
}

/// The resumed interview's argv: `claude --resume <source session id>
/// --fork-session` plus [`shared_tail`]. `--fork-session` is mandatory —
/// proven non-destructive by S2 (source transcript sha256-identical after two
/// concurrent forks and a full tool-using turn) and it is what gives the
/// interview its own session id and its own transcript file, so nothing here
/// ever mutates the member's original record.
pub fn interview_argv(spec: &InterviewSpawnSpec, source_session_id: &str) -> Vec<String> {
    let mut argv = vec![
        claude_executable().to_string(),
        "--resume".to_string(),
        source_session_id.to_string(),
        "--fork-session".to_string(),
    ];
    argv.extend(shared_tail(spec));
    argv
}

/// The evidence-only interview's argv: an ordinary fresh `claude` launch
/// (no `--resume`, no `--fork-session` — there is no session to fork) plus
/// [`shared_tail`]. Used when `workflow::review::decide_interview_mode`
/// answers [`crate::workflow::model::InterviewMode::EvidenceOnly`]: no
/// session id was ever captured, or the transcript is no longer readable.
pub fn evidence_only_argv(spec: &InterviewSpawnSpec) -> Vec<String> {
    let mut argv = vec![claude_executable().to_string()];
    argv.extend(shared_tail(spec));
    argv
}

/// The interview pane's opening instruction, delivered once its session is
/// up — the [`super::lead::lead_seed_prompt`] precedent, and for the same
/// reason: no positional prompt, ever, in either argv above.
pub fn interview_seed_prompt(prompt_path: &Path) -> String {
    format!(
        "Read {} and follow it. It is your review interview for this workflow run.",
        prompt_path.display(),
    )
}

// ── env ──────────────────────────────────────────────────────────────────────

/// Exported into the interview pane so `kvx workflow review answer`/`report`
/// self-identify, the same shape [`super::spawn::RUN_ID_ENV_VAR`] gives the
/// lead. Deliberately the *only* karvex-owned var this pane carries: no
/// `KARVEX_WORKFLOW_RUN_ID`, no run-member mapping of any kind (the interview
/// answer handler resolves the run and the bound member from the
/// interrogation row this value addresses, not from a client-asserted
/// field) — S2 fact 4's corollary is that the fork's session id is a
/// *different* id from the member's, so nothing here may let a "pane reported
/// session X" path fold an interview pane back into a `run_member` the way a
/// teammate pane is.
pub const REVIEW_INTERVIEW_ENV_VAR: &str = "KARVEX_WORKFLOW_REVIEW_INTERVIEW";

/// The value [`REVIEW_INTERVIEW_ENV_VAR`] carries: cycle and interview,
/// joined so a value never on its own looks like a bare store id.
pub fn review_interview_id(cycle: &ReviewCycleId, interview: &InterrogationId) -> String {
    format!("{}:{}", cycle.as_str(), interview.as_str())
}

/// The interview pane's whole karvex-owned environment.
pub fn interview_env(cycle: &ReviewCycleId, interview: &InterrogationId) -> Vec<(String, String)> {
    vec![(
        REVIEW_INTERVIEW_ENV_VAR.to_string(),
        review_interview_id(cycle, interview),
    )]
}

// ── review command override ───────────────────────────────────────────────────

/// CI's escape hatch from `claude`, mirroring `KARVEX_WORKFLOW_SUMMARY_COMMAND`
/// exactly: read once by the adapter, argv as a JSON array, and a malformed
/// value disables reviews with one notice rather than silently falling back
/// to launching `claude` anyway (`07-phase3-plan.md` E-11's rules, verbatim).
pub const REVIEW_COMMAND_ENV: &str = "KARVEX_WORKFLOW_REVIEW_COMMAND";

/// What [`REVIEW_COMMAND_ENV`] resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewCommandOverride {
    /// Not set, or set to an empty string — reviews spawn `claude` as normal.
    Unset,
    /// A validated argv. The adapter substitutes this for both
    /// [`interview_argv`]'s and [`evidence_only_argv`]'s output wholesale;
    /// this override exists precisely so CI never has to run `claude`.
    Command(Vec<String>),
}

/// [`REVIEW_COMMAND_ENV`] was set to something that is not a JSON array of
/// strings, or to `[]`. `notice` is the one-line explanation the adapter
/// surfaces once (a [`crate::workflow::model::UserNotice`] in the app layer)
/// — this type carries the text, not the notice plumbing, so it stays
/// testable without `App`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewCommandOverrideError {
    pub notice: String,
}

/// Parses [`REVIEW_COMMAND_ENV`]'s raw value. Pure over the string so the
/// adapter's one env read stays the single source of truth (the E-11
/// "read once" rule) and this parser needs no env access of its own to be
/// tested.
///
/// Malformed is narrow and deliberate: anything that is not valid JSON, is
/// valid JSON but not an array, contains a non-string element, or is the
/// empty array `[]` (nothing to spawn). Never silently treated as "use
/// `claude`" — that would defeat the one thing this override exists for,
/// keeping a misconfigured CI from ever launching a real agent.
pub fn parse_review_command_override(
    raw: Option<&str>,
) -> Result<ReviewCommandOverride, ReviewCommandOverrideError> {
    let Some(raw) = raw else {
        return Ok(ReviewCommandOverride::Unset);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(ReviewCommandOverride::Unset);
    }
    match serde_json::from_str::<Vec<String>>(trimmed) {
        Ok(argv) if !argv.is_empty() => Ok(ReviewCommandOverride::Command(argv)),
        _ => Err(ReviewCommandOverrideError {
            notice: format!(
                "{REVIEW_COMMAND_ENV} is set but is not a JSON array of one or more strings                  ({trimmed:?}); self-improvement reviews are disabled until it is fixed or                  unset. karvex will not fall back to launching claude.",
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::{InterrogationId, ReviewCycleId};
    use crate::workflow::tier::{Effort, ModelAlias};

    fn spec() -> InterviewSpawnSpec {
        InterviewSpawnSpec {
            member: "scout".to_string(),
            cycle_dir: PathBuf::from("/runs/abc/review/1"),
            member_project_dir: Some(PathBuf::from("/home/dev/project")),
            assignment: Assignment {
                model: ModelAlias::Opus,
                effort: Effort::High,
            },
        }
    }

    fn spec_with_no_project_dir() -> InterviewSpawnSpec {
        InterviewSpawnSpec {
            member_project_dir: None,
            ..spec()
        }
    }

    // ── cycle directory layout ────────────────────────────────────────────

    #[test]
    fn the_cycle_dir_lives_under_the_run_dirs_review_subtree() {
        let cycle = ReviewCycleId::new("review_cycle:1");
        assert_eq!(
            cycle_dir(Path::new("/runs/abc"), &cycle),
            PathBuf::from("/runs/abc/review/review_cycle-1"),
            "the store id's colon must never reach the filesystem",
        );
    }

    #[test]
    fn the_layout_matches_what_the_p5_prompts_promise() {
        let dir = Path::new("/runs/abc/review/1");
        assert_eq!(
            interview_prompt_path(dir, "scout"),
            PathBuf::from("/runs/abc/review/1/interview-scout.md")
        );
        assert_eq!(
            answer_path(dir, "scout"),
            PathBuf::from("/runs/abc/review/1/answers/scout.json")
        );
        assert_eq!(
            synthesis_prompt_path(dir),
            PathBuf::from("/runs/abc/review/1/synthesis.md")
        );
        assert_eq!(
            findings_path(dir),
            PathBuf::from("/runs/abc/review/1/findings.json")
        );
    }

    #[test]
    fn the_spec_paths_agree_with_the_free_functions() {
        let spec = spec();
        assert_eq!(
            spec.prompt_path(),
            interview_prompt_path(&spec.cycle_dir, &spec.member)
        );
        assert_eq!(
            spec.answer_path(),
            answer_path(&spec.cycle_dir, &spec.member)
        );
    }

    // ── argv: the frozen shape (S2) ───────────────────────────────────────

    #[test]
    fn the_resumed_argv_carries_resume_and_fork_session() {
        let argv = interview_argv(&spec(), "7694e312-source-sid");
        assert_eq!(argv[0], claude_executable());
        let resume = argv
            .iter()
            .position(|arg| arg == "--resume")
            .expect("--resume");
        assert_eq!(argv[resume + 1], "7694e312-source-sid");
        assert!(
            argv.iter().any(|arg| arg == "--fork-session"),
            "the fork is mandatory (S2): it is what keeps the source transcript untouched"
        );
    }

    #[test]
    fn the_evidence_only_argv_carries_neither_resume_nor_fork_session() {
        let argv = evidence_only_argv(&spec());
        assert_eq!(argv[0], claude_executable());
        assert!(!argv.iter().any(|arg| arg == "--resume"));
        assert!(!argv.iter().any(|arg| arg == "--fork-session"));
    }

    #[test]
    fn neither_argv_carries_a_positional_prompt() {
        for argv in [interview_argv(&spec(), "sid"), evidence_only_argv(&spec())] {
            assert!(
                !argv
                    .iter()
                    .any(|arg| arg.contains("interview-") && arg.ends_with(".md")),
                "the plan must not ride in argv: {argv:?}"
            );
            // Nor any bare positional trailing the recognised flags at all —
            // every element after argv[0] belongs to a `--flag` or is that
            // flag's value.
            assert!(!argv.iter().any(|arg| arg == "-p" || arg == "--print"));
        }
    }

    #[test]
    fn neither_argv_carries_the_runs_settings_or_teammate_mode() {
        for argv in [interview_argv(&spec(), "sid"), evidence_only_argv(&spec())] {
            assert!(
                !argv.iter().any(|arg| arg == "--settings"),
                "an interview pane is not a member of the run's team: {argv:?}"
            );
            assert!(!argv.iter().any(|arg| arg == "--teammate-mode"));
        }
    }

    #[test]
    fn both_argv_shapes_carry_the_permission_arrangement_or_the_interview_stalls() {
        for argv in [interview_argv(&spec(), "sid"), evidence_only_argv(&spec())] {
            let mode = argv
                .iter()
                .position(|arg| arg == "--permission-mode")
                .expect("--permission-mode is present (S2 amendment 1)");
            assert_eq!(argv[mode + 1], "acceptEdits");
            let tools = argv
                .iter()
                .position(|arg| arg == "--allowedTools")
                .expect("--allowedTools is present (S2 amendment 1)");
            assert_eq!(argv[tools + 1], "Bash(kvx workflow review answer:*)");
        }
    }

    #[test]
    fn add_dir_targets_the_members_project_directory_not_the_cycle_dir() {
        // S2 amendment 3 corrects the plan's own quoted shape
        // (`--add-dir <cycle dir>`): the pane's cwd is the cycle dir, and
        // `--add-dir` grants the member's project directory instead.
        for argv in [interview_argv(&spec(), "sid"), evidence_only_argv(&spec())] {
            let add_dir = argv
                .iter()
                .position(|arg| arg == "--add-dir")
                .expect("--add-dir is present when a member project dir is known");
            assert_eq!(argv[add_dir + 1], "/home/dev/project");
            assert!(
                !argv.contains(&"/runs/abc/review/1".to_string()),
                "the cycle dir must never appear in argv — it is the pane's cwd, set by the                  caller, not a flag: {argv:?}"
            );
        }
    }

    #[test]
    fn a_member_with_no_known_project_dir_still_produces_a_launchable_argv() {
        for argv in [
            interview_argv(&spec_with_no_project_dir(), "sid"),
            evidence_only_argv(&spec_with_no_project_dir()),
        ] {
            assert!(!argv.iter().any(|arg| arg == "--add-dir"));
        }
    }

    #[test]
    fn the_argv_carries_the_interview_assignment() {
        let argv = interview_argv(&spec(), "sid");
        assert!(argv.windows(2).any(|w| w[0] == "--model" && w[1] == "opus"));
        assert!(argv
            .windows(2)
            .any(|w| w[0] == "--effort" && w[1] == "high"));
    }

    // ── seed prompt ──────────────────────────────────────────────────────

    #[test]
    fn the_seed_prompt_names_an_absolute_path() {
        let text = interview_seed_prompt(&spec().prompt_path());
        assert!(text.contains("/runs/abc/review/1/interview-scout.md"));
        assert!(text.contains("follow it"));
    }

    // ── env: no run-member identity ─────────────────────────────────────

    #[test]
    fn the_interview_env_carries_no_run_member_identity() {
        let cycle = ReviewCycleId::new("review_cycle:1");
        let interview = InterrogationId::new("interrogation:5");
        let env = interview_env(&cycle, &interview);
        assert_eq!(
            env.len(),
            1,
            "exactly the one review-scoped var, nothing that could be read as a run/member              binding: {env:?}"
        );
        let (name, value) = &env[0];
        assert_eq!(name, REVIEW_INTERVIEW_ENV_VAR);
        assert_eq!(value, "review_cycle:1:interrogation:5");
        // Not the lead's own run-identity var: a teammate/lead pane
        // self-identifies through `KARVEX_WORKFLOW_RUN_ID`
        // (`super::spawn::RUN_ID_ENV_VAR`) and an interview pane must never
        // be foldable into that same mapping (S2 fact 4's corollary).
        assert!(!env
            .iter()
            .any(|(name, _)| name == crate::workflow::binding::spawn::RUN_ID_ENV_VAR));
    }

    // ── review command override ──────────────────────────────────────────

    #[test]
    fn an_unset_or_empty_override_is_unset() {
        assert_eq!(
            parse_review_command_override(None),
            Ok(ReviewCommandOverride::Unset)
        );
        assert_eq!(
            parse_review_command_override(Some("")),
            Ok(ReviewCommandOverride::Unset)
        );
        assert_eq!(
            parse_review_command_override(Some("   ")),
            Ok(ReviewCommandOverride::Unset)
        );
    }

    #[test]
    fn a_valid_json_array_of_strings_is_the_override_argv() {
        let result = parse_review_command_override(Some(r#"["/bin/review-stub", "--ok"]"#));
        assert_eq!(
            result,
            Ok(ReviewCommandOverride::Command(vec![
                "/bin/review-stub".to_string(),
                "--ok".to_string(),
            ]))
        );
    }

    #[test]
    fn a_malformed_override_disables_reviews_and_never_falls_back_to_claude() {
        for raw in ["not json", "{}", r#"["ok", 5]"#, "[]", "42"] {
            let error = parse_review_command_override(Some(raw))
                .expect_err(&format!("{raw:?} must be refused"));
            assert!(error.notice.contains(REVIEW_COMMAND_ENV));
            assert!(
                !error.notice.to_lowercase().contains("falling back"),
                "the notice must say reviews are disabled, not that claude will run anyway"
            );
            assert!(error.notice.contains("disabled"));
        }
    }

    #[test]
    fn the_malformed_notice_never_says_claude_will_run() {
        let error = parse_review_command_override(Some("not json")).expect_err("malformed");
        assert!(error.notice.contains("will not fall back"));
    }

    #[test]
    fn the_pane_title_names_the_member() {
        assert_eq!(spec().pane_title(), "review: scout");
    }
}
