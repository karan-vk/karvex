//! argv/env construction, node directory layout, and pane creation.
//!
//! Step 3b fills this in against
//! `docs/design/workflow-builder/04-kvdag-and-execution.md` §4.1 and §4.2: the
//! `claude` argv for `Runner::Agent`, the node's own argv for
//! `Runner::Command`, the shared `KARVEX_WORKFLOW_*` environment, and
//! `Workspace::split_pane_argv_command` as the in-process spawn path.
//!
//! Everything above the pane call is pure: argv, env, the node directory
//! layout, the rendered `task.md`, the derived session id. Only
//! [`spawn_node_pane`], [`confirm_managed_agent`], and [`close_node_pane`]
//! touch the runtime, and each takes the runtime object it needs rather than
//! `App`, so the rest of the module stays unit-testable without PTYs.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ratatui::layout::Direction;
use sha2::{Digest, Sha256};
use tracing::debug;

use crate::detect::Agent;
use crate::layout::PaneId;
use crate::terminal::TerminalState;
use crate::workflow::model::{InstancePath, NodeToken, OutputSchema, RunId, Runner, SpawnSpec};
use crate::workspace::{NewPane, Workspace};

// ── environment (§4.2) ──────────────────────────────────────────────────────

pub const RUN_ID_ENV_VAR: &str = "KARVEX_WORKFLOW_RUN_ID";
pub const NODE_PATH_ENV_VAR: &str = "KARVEX_WORKFLOW_NODE_PATH";
pub const NODE_DIR_ENV_VAR: &str = "KARVEX_WORKFLOW_NODE_DIR";
pub const NODE_TOKEN_ENV_VAR: &str = "KARVEX_WORKFLOW_NODE_TOKEN";

/// Overrides where run directories are created, mirroring the store's
/// `KARVEX_WORKFLOW_DB_PATH`. Run directories are kept out of the store's own
/// path so neither can disturb the other.
pub const RUNS_DIR_ENV_VAR: &str = "KARVEX_WORKFLOW_RUNS_DIR";

// ── node directory (§4.1) ───────────────────────────────────────────────────

pub const TASK_FILE: &str = "task.md";
pub const OUTPUT_SCHEMA_FILE: &str = "output_schema.json";
pub const RESULT_FILE: &str = "result.json";
pub const INPUTS_DIR: &str = "inputs";
pub const ARTIFACTS_DIR: &str = "artifacts";

/// The `claude` argv's trailing positional. `SpawnSpec::seed_prompt` overrides
/// it; this is the fallback so an empty spec still produces a working spawn.
pub const SEED_PROMPT: &str = "Read ./task.md and follow it.";

/// §5: interrupt is `agent.send_keys [Escape]`. Spelled exactly as the engine
/// emits it in `RunEffect::SendKeys`, so the two cannot drift.
pub const INTERRUPT_KEYS: [&str; 1] = ["Escape"];

/// Mirrors the settle delay and launch window `agent.start` uses. Those
/// constants are private to `src/app/agents.rs`; a node spawn is the same shape
/// of launch, and the manifest detector's confirmation only behaves like
/// `agent.start` if the window matches.
pub const NODE_AGENT_SETTLE_DELAY: Duration = Duration::from_secs(3);
pub const NODE_AGENT_LAUNCH_WINDOW: Duration = Duration::from_secs(30);

/// `split_pane_argv_command` is given a geometry estimate; the existing
/// in-process callers clamp it the same way before splitting.
const MIN_PANE_ROWS: u16 = 4;
const MIN_PANE_COLS: u16 = 10;

// ── errors ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    /// `Runner::Command` with no argv. `Kvdag::try_new` rejects this at
    /// authoring time, so reaching it means a spec was built by hand.
    MissingCommand(InstancePath),
    /// An argv element carries a NUL. The check is deliberately narrower than
    /// `agent.start`'s "no control characters": that path builds a shell
    /// string, this one execs argv directly, and `--append-system-prompt`
    /// legitimately carries newlines.
    InvalidArgument(String),
    /// The pane the node would have been split from is gone.
    TargetPaneNotFound,
    PaneLaunchFailed(String),
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCommand(path) => {
                write!(f, "node \"{path}\" uses runner \"command\" but has no argv")
            }
            Self::InvalidArgument(argument) => {
                write!(f, "argv element {argument:?} cannot be executed")
            }
            Self::TargetPaneNotFound => f.write_str("the target pane no longer exists"),
            Self::PaneLaunchFailed(message) => write!(f, "pane launch failed: {message}"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// Stable error codes for the API layer, so a spawn failure surfaces as a code
/// rather than a formatted string.
impl SpawnError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingCommand(_) => "workflow_node_missing_command",
            Self::InvalidArgument(_) => "workflow_node_invalid_argument",
            Self::TargetPaneNotFound => "workflow_node_target_pane_not_found",
            Self::PaneLaunchFailed(_) => "workflow_node_spawn_failed",
        }
    }
}

// ── run and node directories ────────────────────────────────────────────────

/// Root of every run directory. Deliberately a sibling of the store, never a
/// child: the store is the single file `state_dir()/workflow.redb`, and keeping
/// run directories out of its path leaves the store free to change layout.
pub fn runs_root() -> PathBuf {
    match std::env::var_os(RUNS_DIR_ENV_VAR) {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => crate::config::state_dir().join("workflow-runs"),
    }
}

pub fn run_dir(runs_root: &Path, run: &RunId) -> PathBuf {
    runs_root.join(path_segment(&run.0))
}

/// `<run dir>/<instance path>/` (§4.1). Every instance-path segment is
/// sanitised: node keys are author-supplied and `Kvdag::try_new` does not
/// constrain their character set, so `..` or an absolute segment would
/// otherwise escape the run directory.
pub fn node_dir(run_dir: &Path, path: &InstancePath) -> PathBuf {
    let mut dir = run_dir.to_path_buf();
    for segment in path.0.split('/').filter(|segment| !segment.is_empty()) {
        dir.push(path_segment(segment));
    }
    dir
}

fn path_segment(raw: &str) -> String {
    let mut segment = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            segment.push(ch);
        } else {
            segment.push('-');
        }
    }
    // A segment of only dots is `.` or `..` after sanitising and would still
    // traverse; anything empty would collapse the path.
    if segment.is_empty() || segment.chars().all(|ch| ch == '.') {
        return format!("_{}", short_digest(raw));
    }
    segment
}

/// The files of §4.1, as paths. Pure: nothing here touches the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeDirLayout {
    pub root: PathBuf,
    pub task: PathBuf,
    pub output_schema: PathBuf,
    pub result: PathBuf,
    pub inputs: PathBuf,
    pub artifacts: PathBuf,
}

impl NodeDirLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            task: root.join(TASK_FILE),
            output_schema: root.join(OUTPUT_SCHEMA_FILE),
            result: root.join(RESULT_FILE),
            inputs: root.join(INPUTS_DIR),
            artifacts: root.join(ARTIFACTS_DIR),
            root,
        }
    }

    pub fn for_node(run_dir: &Path, path: &InstancePath) -> Self {
        Self::new(node_dir(run_dir, path))
    }

    /// `inputs/<port>.json` (§4.1). The port name is sanitised for the same
    /// reason instance-path segments are.
    pub fn input_file(&self, port: &str) -> PathBuf {
        self.inputs.join(format!("{}.json", path_segment(port)))
    }
}

/// Everything karvex writes into a node directory before the process starts.
#[derive(Debug, Clone)]
pub struct NodeDirPlan<'a> {
    /// The rendered `task.md` body; see [`TaskDocument`].
    pub task_markdown: &'a str,
    pub output_schema: &'a OutputSchema,
    /// `port -> upstream checkpoint payload`, written to `inputs/<port>.json`.
    pub inputs: &'a BTreeMap<String, serde_json::Value>,
}

/// Creates the node directory and writes the karvex-owned files of §4.1.
///
/// A stale `result.json` is removed first. A restart reuses the node directory
/// (`05` W4: attempt += 1, respawn from `task.md`), and leaving the previous
/// attempt's artifact in place would let the completion gate accept work the
/// new attempt never did.
pub fn materialise_node_dir(layout: &NodeDirLayout, plan: &NodeDirPlan<'_>) -> io::Result<()> {
    std::fs::create_dir_all(&layout.root)?;
    std::fs::create_dir_all(&layout.inputs)?;
    std::fs::create_dir_all(&layout.artifacts)?;

    match std::fs::remove_file(&layout.result) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }

    std::fs::write(&layout.task, plan.task_markdown)?;
    let schema = serde_json::to_string_pretty(plan.output_schema.as_value())
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    std::fs::write(&layout.output_schema, format!("{schema}\n"))?;

    for (port, payload) in plan.inputs {
        let body = serde_json::to_string_pretty(payload)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        std::fs::write(layout.input_file(port), format!("{body}\n"))?;
    }
    Ok(())
}

// ── task.md ─────────────────────────────────────────────────────────────────

/// The node prompt contract (`05-phase-plan.md` §3 item 8). Changing the
/// rendered shape invalidates every node prompt template, so it is a frozen
/// contract rather than presentation.
#[derive(Debug, Clone, Default)]
pub struct TaskDocument<'a> {
    pub label: &'a str,
    pub role: &'a str,
    /// The workflow contract plus anything node-specific; the same text the
    /// `Agent` argv passes as `--append-system-prompt`.
    pub contract: &'a str,
    /// The node's `prompt_template` with every `{{slot}}` already filled.
    pub prompt: &'a str,
    /// Inbound edge ports that have an `inputs/<port>.json` file.
    pub input_ports: &'a [String],
}

impl TaskDocument<'_> {
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("# ");
        out.push_str(self.label);
        out.push_str("\n\n");

        if !self.role.trim().is_empty() {
            out.push_str("## Role\n\n");
            out.push_str(self.role.trim());
            out.push_str("\n\n");
        }

        out.push_str("## Task\n\n");
        out.push_str(self.prompt.trim());
        out.push_str("\n\n");

        if !self.input_ports.is_empty() {
            out.push_str("## Inputs\n\n");
            for port in self.input_ports {
                out.push_str(&format!(
                    "- `{port}`: `./{INPUTS_DIR}/{}.json`\n",
                    path_segment(port)
                ));
            }
            out.push('\n');
        }

        if !self.contract.trim().is_empty() {
            out.push_str("## Contract\n\n");
            out.push_str(self.contract.trim());
            out.push_str("\n\n");
        }

        // `summary` and `artifacts` are the keys the engine's checkpoint
        // summariser reads; without them a checkpoint degrades to raw JSON.
        out.push_str("## Reporting\n\n");
        out.push_str(&format!(
            "Write your result to `./{RESULT_FILE}`. It must validate against \
`./{OUTPUT_SCHEMA_FILE}`.\nInclude a `summary` string for downstream nodes and \
an `artifacts` array of paths under `./{ARTIFACTS_DIR}/` for anything large.\n\
Then report completion:\n\n```\nkvx workflow node complete\n```\n\nThat is the \
only way this node finishes. Ending your turn without a valid `{RESULT_FILE}` \
leaves the node waiting for attention.\n"
        ));
        out
    }
}

/// Fills `{{slot}}` placeholders in a node's `prompt_template`.
///
/// `Kvdag::try_new` has already proved every placeholder resolves to an inbound
/// edge port or a declared run argument, so an unknown name here is a caller
/// bug and is left in place verbatim rather than silently blanked — a blank
/// would look like a legitimately empty upstream result.
pub fn fill_slots(template: &str, slots: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            out.push_str(&rest[open..]);
            return out;
        };
        let name = after[..close].trim();
        match slots.get(name) {
            Some(value) => out.push_str(value),
            None => {
                debug!(slot = name, "workflow prompt slot has no value");
                out.push_str(&rest[open..open + 2 + close + 2]);
            }
        }
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    out
}

// ── argv and env (§4.2) ─────────────────────────────────────────────────────

/// `begin_managed_agent` is called only when the resolved argv maps to a known
/// `crate::detect::Agent`. A `Command` node is by construction not a detected
/// agent, so it never gets managed-agent confirmation and `agent.prompt` is
/// never used on it.
pub fn managed_agent_kind(runner: Runner) -> Option<Agent> {
    match runner {
        Runner::Agent => Some(Agent::Claude),
        Runner::Command => None,
    }
}

/// The `claude` argv of §4.2. `--append-system-prompt` is omitted when the
/// node has no contract: the flag takes the contract text, and there is no
/// text to pass.
pub fn agent_argv(spec: &SpawnSpec) -> Vec<String> {
    let mut argv = vec![
        crate::detect::interactive_agent_executable(Agent::Claude).to_string(),
        "--session-id".to_string(),
        spec.agent_session_id.clone(),
        "--model".to_string(),
        spec.assignment.model.as_str().to_string(),
        "--effort".to_string(),
        spec.assignment.effort.as_str().to_string(),
    ];
    if !spec.label.trim().is_empty() {
        argv.push("--name".to_string());
        argv.push(spec.label.clone());
    }
    if !spec.contract.trim().is_empty() {
        argv.push("--append-system-prompt".to_string());
        argv.push(spec.contract.clone());
    }
    argv.push("--add-dir".to_string());
    argv.push(spec.node_dir.to_string_lossy().into_owned());
    argv.push(seed_prompt(spec).to_string());
    argv
}

/// The node's own argv, verbatim (§4.2).
pub fn command_argv(spec: &SpawnSpec) -> Result<Vec<String>, SpawnError> {
    let argv = spec
        .command
        .as_ref()
        .filter(|argv| !argv.is_empty())
        .ok_or_else(|| SpawnError::MissingCommand(spec.path.clone()))?;
    Ok(argv.clone())
}

/// Runner-selected argv. Never selected by test-vs-production (§4.2).
pub fn argv_for(spec: &SpawnSpec) -> Result<Vec<String>, SpawnError> {
    let argv = match spec.runner {
        Runner::Agent => agent_argv(spec),
        Runner::Command => command_argv(spec)?,
    };
    for argument in &argv {
        if argument.contains('\0') {
            return Err(SpawnError::InvalidArgument(argument.clone()));
        }
    }
    Ok(argv)
}

fn seed_prompt(spec: &SpawnSpec) -> &str {
    if spec.seed_prompt.trim().is_empty() {
        SEED_PROMPT
    } else {
        spec.seed_prompt.as_str()
    }
}

/// The `KARVEX_WORKFLOW_*` environment, identical for both runners (§4.2).
/// `KARVEX_ENV`, `KARVEX_PANE_ID`, and the socket path are already injected by
/// the pane launch path, so they are deliberately absent here.
pub fn node_env(spec: &SpawnSpec) -> Vec<(String, String)> {
    vec![
        (RUN_ID_ENV_VAR.to_string(), spec.run_id.0.clone()),
        (NODE_PATH_ENV_VAR.to_string(), spec.path.0.clone()),
        (
            NODE_DIR_ENV_VAR.to_string(),
            spec.node_dir.to_string_lossy().into_owned(),
        ),
        (NODE_TOKEN_ENV_VAR.to_string(), spec.token.0.clone()),
    ]
}

// ── identities ──────────────────────────────────────────────────────────────

/// The session id passed as `claude --session-id`, derived rather than random.
///
/// Deriving it from `(run, path, attempt)` keeps a run replayable and makes a
/// restart's session id predictable, which is what lets the transcript path be
/// known before the process starts. The version nibble is tagged 4 even though
/// the bytes are not random: `claude --session-id` validates against the common
/// v1–v5 uuid shape, and a v8 tag risks rejection.
pub fn derive_agent_session_id(run: &RunId, path: &InstancePath, attempt: u8) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"karvex/workflow/agent-session\0");
    hasher.update(run.0.as_bytes());
    hasher.update([0]);
    hasher.update(path.0.as_bytes());
    hasher.update([0]);
    hasher.update([attempt]);
    format_uuid(hasher.finalize().as_slice())
}

fn format_uuid(bytes: &[u8]) -> String {
    let mut octets = [0u8; 16];
    for (slot, byte) in octets.iter_mut().zip(bytes.iter()) {
        *slot = *byte;
    }
    octets[6] = (octets[6] & 0x0f) | 0x40;
    octets[8] = (octets[8] & 0x3f) | 0x80;

    let mut out = String::with_capacity(36);
    for (index, byte) in octets.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

static NEXT_TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Mints a node's capability token (`KARVEX_WORKFLOW_NODE_TOKEN`).
///
/// This authenticates `workflow.node.report` against a local socket that any
/// process running as the user can already reach, so it is an anti-confusion
/// capability — it stops one node reporting as another — not a secret against a
/// local attacker. karvex has no CSPRNG dependency; the seed is wall clock,
/// process id, and a process-local counter, hashed so none of them is
/// recoverable from the token.
pub fn mint_node_token() -> NodeToken {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let sequence = NEXT_TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(b"karvex/workflow/node-token\0");
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(sequence.to_le_bytes());
    NodeToken(hex(&hasher.finalize()[..16]))
}

fn short_digest(value: &str) -> String {
    hex(&Sha256::digest(value.as_bytes())[..4])
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Where `claude` will write this session's transcript. Derivable before the
/// process starts because karvex assigns the session id (§4.2).
///
/// The project-directory encoding is `claude`'s own and is not a documented
/// interface: every non-alphanumeric byte of the absolute cwd becomes `-`. The
/// `SessionStart` hook reports the real `transcript_path` shortly after launch,
/// so treat this as the pre-launch estimate and prefer the reported path once
/// it arrives.
pub fn transcript_path(cwd: &Path, agent_session_id: &str) -> io::Result<PathBuf> {
    Ok(transcript_path_in(&claude_dir()?, cwd, agent_session_id))
}

/// The env-free half of [`transcript_path`].
pub fn transcript_path_in(claude_dir: &Path, cwd: &Path, agent_session_id: &str) -> PathBuf {
    claude_dir
        .join("projects")
        .join(claude_project_slug(cwd))
        .join(format!("{agent_session_id}.jsonl"))
}

/// Duplicates `integration::env::claude_dir`, which is private to that module
/// and not re-exported. Both must agree: the hook installer writes into this
/// directory and the transcript is read out of it.
fn claude_dir() -> io::Result<PathBuf> {
    if let Some(configured) =
        std::env::var_os("CLAUDE_CONFIG_DIR").filter(|value| !value.is_empty())
    {
        return Ok(PathBuf::from(configured));
    }
    Ok(home_dir()?.join(".claude"))
}

fn home_dir() -> io::Result<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home));
    }

    #[cfg(windows)]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(profile));
        }
        if let (Some(drive), Some(path)) = (
            std::env::var_os("HOMEDRIVE").filter(|value| !value.is_empty()),
            std::env::var_os("HOMEPATH").filter(|value| !value.is_empty()),
        ) {
            let mut home = PathBuf::from(drive);
            home.push(path);
            return Ok(home);
        }
    }

    Err(io::Error::other(
        "home directory is not set; cannot locate the claude transcript directory",
    ))
}

fn claude_project_slug(cwd: &Path) -> String {
    let raw = cwd.to_string_lossy();
    let mut slug = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else {
            slug.push('-');
        }
    }
    slug
}

// ── runtime ─────────────────────────────────────────────────────────────────

/// Where and how the node's pane is split off. Supplied by the `App` glue,
/// which is the only layer that knows the current geometry and theme.
#[derive(Debug, Clone, Copy)]
pub struct PaneSpawnContext {
    pub target_pane: PaneId,
    pub direction: Direction,
    pub rows: u16,
    pub cols: u16,
    pub scrollback_limit_bytes: usize,
    pub host_terminal_theme: crate::terminal_theme::TerminalTheme,
    pub host_terminal_appearance: Option<crate::terminal_theme::HostAppearance>,
    pub focus: bool,
}

/// Creates the node's pane in-process (§4.2). Third in-process caller of
/// `split_pane_argv_command`, after the plugin pane-open path and the
/// scrollback-editor launch; the public `pane.split` schema stays argv-free.
pub fn spawn_node_pane(
    workspace: &mut Workspace,
    context: PaneSpawnContext,
    spec: &SpawnSpec,
) -> Result<(usize, NewPane), SpawnError> {
    let argv = argv_for(spec)?;
    let env = node_env(spec);
    let result = workspace.split_pane_argv_command(
        context.target_pane,
        context.direction,
        context.rows.max(MIN_PANE_ROWS),
        context.cols.max(MIN_PANE_COLS),
        Some(spec.cwd.clone()),
        &argv,
        env,
        context.scrollback_limit_bytes,
        context.host_terminal_theme,
        context.host_terminal_appearance,
        context.focus,
    );
    match result {
        Some(Ok(spawned)) => Ok(spawned),
        Some(Err(err)) => Err(SpawnError::PaneLaunchFailed(err.to_string())),
        None => Err(SpawnError::TargetPaneNotFound),
    }
}

/// Spawn confirmation for `Runner::Agent` only (§4.2). Returns whether the
/// managed-agent launch window was opened; `false` means the node is a plain
/// process and confirmation degrades to "process started".
///
/// `SpawnSpec::label` becomes both `claude --name` and the karvex agent name,
/// so it has to be unique among live agents in the session — the spec builder
/// owns that, exactly as `agent.start` rejects duplicate names.
pub fn confirm_managed_agent(terminal: &mut TerminalState, spec: &SpawnSpec, now: Instant) -> bool {
    let Some(kind) = managed_agent_kind(spec.runner) else {
        return false;
    };
    terminal.begin_managed_agent(
        spec.label.clone(),
        kind,
        now,
        NODE_AGENT_SETTLE_DELAY,
        NODE_AGENT_LAUNCH_WINDOW,
    );
    true
}

/// Executes `RunEffect::ClosePane` and the pane half of a restart (§5).
pub fn close_node_pane(workspace: &mut Workspace, pane_id: PaneId) -> bool {
    workspace.close_pane(pane_id)
}

/// The keys `RunEffect::SendKeys` carries for an interrupt (§5).
pub fn interrupt_keys() -> Vec<String> {
    INTERRUPT_KEYS
        .iter()
        .map(|key| (*key).to_string())
        .collect()
}

// ── steering and delivery (§5, §5.1) ────────────────────────────────────────

/// Which injection primitive a `RunEffect::PromptNode` uses. The engine emits
/// one effect for both runners, so the runner-selected split of §4.2 and §5 is
/// resolved here rather than in the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// `agent.prompt`: the only injection primitive that verifies the live
    /// foreground process still matches the expected agent and handles the
    /// Enter-submit race.
    AgentPrompt,
    /// `pane.send_text`: a `Command` node is not a detected agent, so
    /// `agent.prompt` would answer `agent_not_ready` for it.
    RawText,
}

impl Delivery {
    /// The value journalled as `delivery` on `steer` and `message_delivered`.
    pub fn journal_label(self) -> &'static str {
        match self {
            Self::AgentPrompt => "agent_prompt",
            Self::RawText => "raw",
        }
    }
}

pub fn delivery_for(runner: Runner) -> Delivery {
    match runner {
        Runner::Agent => Delivery::AgentPrompt,
        Runner::Command => Delivery::RawText,
    }
}

/// The fixed frame every cross-node delivery is wrapped in (§5.1), so a
/// teammate can always tell karvex's words from a human's. Content reaches a
/// node only when an inbound edge fires, a human steers it, or the watchdog
/// nudges it — never by broadcast.
pub fn message_frame(source_label: &str, summary: &str) -> String {
    format!(
        "[karvex · from {source_label}]\n{}\nContinue with ./{TASK_FILE}. Reply only through {RESULT_FILE}.",
        summary.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::workflow::model::{InstancePath, Isolation, NodeToken, RunId, SpawnSpec};
    use crate::workflow::tier::{Assignment, Effort, ModelAlias};

    fn agent_spec() -> SpawnSpec {
        SpawnSpec {
            run_id: RunId("run-1".to_string()),
            path: InstancePath("plan".to_string()),
            label: "Plan".to_string(),
            runner: Runner::Agent,
            command: None,
            assignment: Assignment {
                model: ModelAlias::Opus,
                effort: Effort::High,
            },
            agent_session_id: "11111111-1111-4111-8111-111111111111".to_string(),
            node_dir: PathBuf::from("/runs/run-1/plan"),
            cwd: PathBuf::from("/work/repo"),
            isolation: Isolation::None,
            contract: "Reply only through result.json.".to_string(),
            seed_prompt: String::new(),
            token: NodeToken("token-1".to_string()),
        }
    }

    fn command_spec() -> SpawnSpec {
        SpawnSpec {
            runner: Runner::Command,
            command: Some(vec![
                "sh".to_string(),
                "-c".to_string(),
                "printf '{}' > result.json".to_string(),
            ]),
            label: "Stub".to_string(),
            path: InstancePath("stub".to_string()),
            ..agent_spec()
        }
    }

    #[test]
    fn agent_argv_follows_the_documented_order() {
        let spec = agent_spec();
        assert_eq!(
            agent_argv(&spec),
            vec![
                "claude".to_string(),
                "--session-id".to_string(),
                "11111111-1111-4111-8111-111111111111".to_string(),
                "--model".to_string(),
                "opus".to_string(),
                "--effort".to_string(),
                "high".to_string(),
                "--name".to_string(),
                "Plan".to_string(),
                "--append-system-prompt".to_string(),
                "Reply only through result.json.".to_string(),
                "--add-dir".to_string(),
                "/runs/run-1/plan".to_string(),
                SEED_PROMPT.to_string(),
            ]
        );
    }

    #[test]
    fn agent_argv_carries_the_resolved_assignment() {
        let mut spec = agent_spec();
        spec.assignment = Assignment {
            model: ModelAlias::Fable,
            effort: Effort::Max,
        };
        let argv = agent_argv(&spec);
        let model = argv.iter().position(|arg| arg == "--model").unwrap();
        let effort = argv.iter().position(|arg| arg == "--effort").unwrap();
        assert_eq!(argv[model + 1], "fable");
        assert_eq!(argv[effort + 1], "max");
    }

    #[test]
    fn agent_argv_omits_an_empty_contract_and_label() {
        let mut spec = agent_spec();
        spec.contract = "   ".to_string();
        spec.label = String::new();
        let argv = agent_argv(&spec);
        assert!(!argv.iter().any(|arg| arg == "--append-system-prompt"));
        assert!(!argv.iter().any(|arg| arg == "--name"));
    }

    #[test]
    fn agent_argv_uses_the_spec_seed_prompt_when_present() {
        let mut spec = agent_spec();
        spec.seed_prompt = "Read ./task.md, then stop.".to_string();
        let argv = agent_argv(&spec);
        assert_eq!(
            argv.last().map(String::as_str),
            Some("Read ./task.md, then stop.")
        );
    }

    #[test]
    fn command_argv_is_verbatim_and_never_wraps_in_claude() {
        let spec = command_spec();
        let argv = argv_for(&spec).unwrap();
        assert_eq!(
            argv,
            vec![
                "sh".to_string(),
                "-c".to_string(),
                "printf '{}' > result.json".to_string(),
            ]
        );
        assert!(!argv.iter().any(|arg| arg == "claude"));
        assert!(!argv.iter().any(|arg| arg == "--session-id"));
    }

    #[test]
    fn command_runner_without_argv_is_a_spawn_error() {
        let mut spec = command_spec();
        spec.command = Some(Vec::new());
        assert_eq!(
            argv_for(&spec),
            Err(SpawnError::MissingCommand(InstancePath("stub".to_string())))
        );
        spec.command = None;
        assert!(matches!(
            argv_for(&spec),
            Err(SpawnError::MissingCommand(_))
        ));
    }

    #[test]
    fn argv_rejects_nul_but_keeps_multiline_contracts() {
        let mut spec = agent_spec();
        spec.contract = "line one\nline two".to_string();
        let argv = argv_for(&spec).unwrap();
        assert!(argv.iter().any(|arg| arg == "line one\nline two"));

        let mut broken = command_spec();
        broken.command = Some(vec!["sh".to_string(), "a\0b".to_string()]);
        assert_eq!(
            argv_for(&broken),
            Err(SpawnError::InvalidArgument("a\0b".to_string()))
        );
    }

    #[test]
    fn both_runners_share_one_env_block() {
        let agent = node_env(&agent_spec());
        let command = node_env(&command_spec());
        assert_eq!(
            agent,
            vec![
                ("KARVEX_WORKFLOW_RUN_ID".to_string(), "run-1".to_string()),
                ("KARVEX_WORKFLOW_NODE_PATH".to_string(), "plan".to_string()),
                (
                    "KARVEX_WORKFLOW_NODE_DIR".to_string(),
                    "/runs/run-1/plan".to_string()
                ),
                (
                    "KARVEX_WORKFLOW_NODE_TOKEN".to_string(),
                    "token-1".to_string()
                ),
            ]
        );
        let names: Vec<&String> = command.iter().map(|(name, _)| name).collect();
        assert_eq!(
            names,
            vec![
                "KARVEX_WORKFLOW_RUN_ID",
                "KARVEX_WORKFLOW_NODE_PATH",
                "KARVEX_WORKFLOW_NODE_DIR",
                "KARVEX_WORKFLOW_NODE_TOKEN",
            ]
        );
        // The socket path and pane identity come from the pane launch env, so
        // a node never receives a second, conflicting copy.
        assert!(!agent
            .iter()
            .any(|(name, _)| name == crate::api::SOCKET_PATH_ENV_VAR));
    }

    #[test]
    fn only_the_agent_runner_is_a_managed_agent() {
        assert_eq!(managed_agent_kind(Runner::Agent), Some(Agent::Claude));
        assert_eq!(managed_agent_kind(Runner::Command), None);
    }

    #[test]
    fn node_dir_nests_every_instance_path_segment() {
        let run = run_dir(
            Path::new("/state/workflow-runs"),
            &RunId("run-1".to_string()),
        );
        assert_eq!(run, PathBuf::from("/state/workflow-runs/run-1"));
        let dir = node_dir(&run, &InstancePath("research/2/verify".to_string()));
        assert_eq!(
            dir,
            PathBuf::from("/state/workflow-runs/run-1/research/2/verify")
        );
    }

    #[test]
    fn node_dir_cannot_escape_the_run_directory() {
        let run = PathBuf::from("/state/workflow-runs/run-1");
        let dir = node_dir(&run, &InstancePath("../../etc/passwd".to_string()));
        assert!(dir.starts_with(&run));
        assert!(!dir.to_string_lossy().contains(".."));

        let absolute = node_dir(&run, &InstancePath("/etc/shadow".to_string()));
        assert!(absolute.starts_with(&run));

        // A dot segment is renamed, not dropped: dropping it would let
        // "a/./b" and "a/b" resolve to the same directory.
        let sneaky = node_dir(&run, &InstancePath("a/./b".to_string()));
        assert!(sneaky.starts_with(&run));
        assert_eq!(sneaky.components().count(), run.components().count() + 3);
        assert_ne!(sneaky, node_dir(&run, &InstancePath("a/b".to_string())));
        for component in dir
            .components()
            .chain(absolute.components())
            .chain(sneaky.components())
        {
            assert!(!matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            ));
        }
    }

    #[test]
    fn node_dir_layout_names_every_documented_file() {
        let layout = NodeDirLayout::for_node(
            Path::new("/state/workflow-runs/run-1"),
            &InstancePath("plan".to_string()),
        );
        assert_eq!(
            layout.root,
            PathBuf::from("/state/workflow-runs/run-1/plan")
        );
        assert_eq!(layout.task, layout.root.join("task.md"));
        assert_eq!(layout.output_schema, layout.root.join("output_schema.json"));
        assert_eq!(layout.result, layout.root.join("result.json"));
        assert_eq!(layout.inputs, layout.root.join("inputs"));
        assert_eq!(layout.artifacts, layout.root.join("artifacts"));
        assert_eq!(
            layout.input_file("plan"),
            layout.root.join("inputs").join("plan.json")
        );
    }

    #[test]
    fn materialise_writes_the_contract_and_clears_a_stale_result() {
        let base = std::env::temp_dir().join(format!(
            "karvex-workflow-spawn-{}-{}",
            std::process::id(),
            NEXT_TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let layout = NodeDirLayout::new(base.join("plan"));
        std::fs::create_dir_all(&layout.root).unwrap();
        std::fs::write(&layout.result, "{\"stale\":true}").unwrap();

        let schema = OutputSchema::parse(serde_json::json!({
            "type": "object",
            "required": ["plan"],
            "properties": { "plan": { "type": "string" } }
        }))
        .unwrap();
        let mut inputs = BTreeMap::new();
        inputs.insert("plan".to_string(), serde_json::json!({ "summary": "ok" }));

        materialise_node_dir(
            &layout,
            &NodeDirPlan {
                task_markdown: "# Plan\n",
                output_schema: &schema,
                inputs: &inputs,
            },
        )
        .unwrap();

        assert!(!layout.result.exists());
        assert!(layout.artifacts.is_dir());
        assert_eq!(std::fs::read_to_string(&layout.task).unwrap(), "# Plan\n");
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&layout.output_schema).unwrap()).unwrap();
        assert_eq!(&written, schema.as_value());
        let input: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(layout.input_file("plan")).unwrap())
                .unwrap();
        assert_eq!(input, serde_json::json!({ "summary": "ok" }));

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn task_document_names_the_result_contract() {
        let ports = vec!["plan".to_string()];
        let rendered = TaskDocument {
            label: "Implement",
            role: "You are the implementer.",
            contract: "Reply only through result.json.",
            prompt: "Implement this plan.",
            input_ports: &ports,
        }
        .render();

        assert!(rendered.starts_with("# Implement\n"));
        assert!(rendered.contains("You are the implementer."));
        assert!(rendered.contains("Implement this plan."));
        assert!(rendered.contains("`./inputs/plan.json`"));
        assert!(rendered.contains("Reply only through result.json."));
        assert!(rendered.contains("kvx workflow node complete"));
        assert!(rendered.contains("output_schema.json"));
        // The engine's checkpoint summariser reads these two keys.
        assert!(rendered.contains("`summary`"));
        assert!(rendered.contains("`artifacts`"));
    }

    #[test]
    fn task_document_omits_empty_optional_sections() {
        let rendered = TaskDocument {
            label: "Plan",
            role: "  ",
            contract: "",
            prompt: "Do the thing.",
            input_ports: &[],
        }
        .render();
        assert!(!rendered.contains("## Role"));
        assert!(!rendered.contains("## Contract"));
        assert!(!rendered.contains("## Inputs"));
        assert!(rendered.contains("## Task"));
    }

    #[test]
    fn fill_slots_substitutes_ports_and_keeps_unknown_names() {
        let mut slots = BTreeMap::new();
        slots.insert("plan".to_string(), "step one".to_string());
        assert_eq!(
            fill_slots("Implement this plan:\n{{plan}}", &slots),
            "Implement this plan:\nstep one"
        );
        assert_eq!(fill_slots("{{ plan }}!", &slots), "step one!");
        assert_eq!(fill_slots("{{missing}}", &slots), "{{missing}}");
        assert_eq!(fill_slots("no slots", &slots), "no slots");
        assert_eq!(fill_slots("{{unclosed", &slots), "{{unclosed");
    }

    #[test]
    fn derived_session_id_is_a_stable_uuid_per_attempt() {
        let run = RunId("run-1".to_string());
        let path = InstancePath("research/2".to_string());
        let first = derive_agent_session_id(&run, &path, 1);
        assert_eq!(first, derive_agent_session_id(&run, &path, 1));
        assert_ne!(first, derive_agent_session_id(&run, &path, 2));
        assert_ne!(
            first,
            derive_agent_session_id(&run, &InstancePath("research/3".to_string()), 1)
        );
        assert_ne!(
            first,
            derive_agent_session_id(&RunId("run-2".to_string()), &path, 1)
        );

        assert_eq!(first.len(), 36);
        let groups: Vec<&str> = first.split('-').collect();
        assert_eq!(
            groups.iter().map(|group| group.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(first.chars().all(|ch| ch == '-' || ch.is_ascii_hexdigit()));
        assert!(groups[2].starts_with('4'));
        assert!(matches!(
            groups[3].chars().next(),
            Some('8') | Some('9') | Some('a') | Some('b')
        ));
    }

    #[test]
    fn minted_tokens_do_not_repeat() {
        let first = mint_node_token();
        let second = mint_node_token();
        assert_ne!(first, second);
        assert_eq!(first.0.len(), 32);
        assert!(first.0.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn transcript_path_is_derived_from_the_assigned_session_id() {
        let path = transcript_path_in(
            Path::new("/home/dev/.claude"),
            Path::new("/work/my_repo"),
            "abc-123",
        );
        assert_eq!(
            path,
            PathBuf::from("/home/dev/.claude/projects/-work-my-repo/abc-123.jsonl")
        );
        // Every non-alphanumeric byte becomes a dash, with no collapsing: that
        // is `claude`'s own project-directory encoding.
        assert_eq!(claude_project_slug(Path::new("/a.b/c-d")), "-a-b-c-d");
    }

    #[test]
    fn delivery_is_selected_by_the_runner_not_by_the_environment() {
        assert_eq!(delivery_for(Runner::Agent), Delivery::AgentPrompt);
        assert_eq!(delivery_for(Runner::Command), Delivery::RawText);
        assert_eq!(Delivery::AgentPrompt.journal_label(), "agent_prompt");
        assert_eq!(Delivery::RawText.journal_label(), "raw");
    }

    #[test]
    fn cross_node_messages_use_the_fixed_frame() {
        let framed = message_frame("Plan", "  three steps  ");
        assert_eq!(
            framed,
            "[karvex · from Plan]\nthree steps\nContinue with ./task.md. Reply only through result.json."
        );
    }

    #[test]
    fn interrupt_uses_a_key_the_api_key_parser_accepts() {
        assert_eq!(interrupt_keys(), vec!["Escape".to_string()]);
        let (code, modifiers) = crate::config::parse_key_combo(INTERRUPT_KEYS[0]).unwrap();
        assert_eq!(code, crossterm::event::KeyCode::Esc);
        assert!(modifiers.is_empty());
    }
}
