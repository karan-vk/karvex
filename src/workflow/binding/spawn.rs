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
/// `KARVEX_WORKFLOW_DB_PATH`. Run directories cannot live *inside*
/// `state_dir()/workflow`: that path is the SurrealKV database directory.
pub const RUNS_DIR_ENV_VAR: &str = "KARVEX_WORKFLOW_RUNS_DIR";

// ── node directory (§4.1) ───────────────────────────────────────────────────

pub const TASK_FILE: &str = "task.md";
pub const OUTPUT_SCHEMA_FILE: &str = "output_schema.json";
pub const RESULT_FILE: &str = "result.json";
pub const INPUTS_DIR: &str = "inputs";
pub const ARTIFACTS_DIR: &str = "artifacts";

/// Run-level context, a sibling of the node directories: `<run dir>/context/`
/// (`07-phase3-plan.md` §4 D21).
pub const CONTEXT_DIR: &str = "context";
/// The prior-runs digest every node's `task.md` points at.
pub const PRIOR_RUNS_FILE: &str = "prior-runs.md";

/// The `claude` argv's trailing positional, and the same text when it has to be
/// re-delivered through `agent.prompt`. `SpawnSpec::seed_prompt` overrides it;
/// this is the fallback so an empty spec still produces a working spawn.
///
/// The path is **absolute**. A node's cwd is the workspace directory (§4.2), not
/// its node directory, so a relative `./task.md` names a file that does not
/// exist and the agent has nothing to read; `--add-dir <node_dir>` is what makes
/// the absolute path readable.
pub fn seed_prompt_for(node_dir: &Path) -> String {
    format!("Read {} and follow it.", node_dir.join(TASK_FILE).display())
}

/// §5: an agent node's interrupt is `agent.send_keys [Escape]` — what a
/// `claude` TUI reads as "stop this turn". A `Runner::Command` node is a plain
/// process with no such convention, so its interrupt is the terminal's own
/// `ctrl+c`, which the line discipline turns into SIGINT. Spelled exactly as
/// the engine emits them in `RunEffect::SendKeys`, so the two cannot drift.
pub const AGENT_INTERRUPT_KEYS: [&str; 1] = ["Escape"];
pub const COMMAND_INTERRUPT_KEYS: [&str; 1] = ["ctrl+c"];

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

/// Root of every run directory. Deliberately a sibling of the store directory,
/// never a child: `state_dir()/workflow` is opened by SurrealKV, which owns
/// every file under it.
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

/// One filesystem-safe path segment from arbitrary caller text.
///
/// Shared with `binding::interrogate`, which sanitises a record id the same way
/// — the `interrogation:` colon must never reach the filesystem — so the
/// traversal rules below are written once rather than approximated twice.
pub fn path_segment(raw: &str) -> String {
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

    /// `inputs/<port>/` — where a port that several upstreams fanned into keeps
    /// one file per contributor (§4.1, 2026-08-08 amendment).
    ///
    /// Never collides with [`Self::input_file`]: that path always ends in
    /// `.json` and this one never does.
    pub fn port_dir(&self, port: &str) -> PathBuf {
        self.inputs.join(path_segment(port))
    }

    /// `inputs/<port>/<stem>.json`, the file one contributor's payload is
    /// written to. `stem` comes from [`input_source_stems`], which is what keeps
    /// two contributors from resolving to the same name.
    pub fn input_source_file(&self, port: &str, stem: &str) -> PathBuf {
        self.port_dir(port).join(format!("{stem}.json"))
    }
}

/// `inputs/<port>`, relative to the node directory, in the same sanitised
/// spelling [`NodeDirLayout::port_dir`] writes to — so `task.md` and the
/// `inputs/<port>.json` index can only ever name files that exist.
pub fn port_dir_relative(port: &str) -> String {
    format!("{INPUTS_DIR}/{}", path_segment(port))
}

/// One upstream's contribution to a single inbound port.
///
/// A port used to be a `port -> payload` entry, which silently held only the
/// last writer once §3.4's inherited fan-in gave one port several sources: a
/// whole generation of children collapsed to one file. The contributing node is
/// therefore part of the value, not a key that can be overwritten.
#[derive(Debug, Clone, PartialEq)]
pub struct PortContribution {
    /// The contributing node's instance path — unique in a run, which is what
    /// makes it the identity a per-contributor file is named after.
    pub from: String,
    /// The contributing node's label, for the human reading `task.md`. May be
    /// empty; nothing is invented in its place.
    pub label: String,
    pub payload: serde_json::Value,
}

/// File-name stems for one port's contributors, positionally aligned with
/// `sources`.
///
/// The instance path sanitised the same way a directory segment is, which maps
/// `fanout/worker/1` to `fanout-worker-1`. Sanitising is lossy — `a/b` and `a-b`
/// both become `a-b` — so a stem that is not unique among *this port's*
/// contributors is disambiguated with a digest of the path it came from. Two
/// contributions must never resolve to one file: that is the exact silent loss
/// this shape exists to end.
pub fn input_source_stems(sources: &[&str]) -> Vec<String> {
    let plain: Vec<String> = sources.iter().map(|source| path_segment(source)).collect();
    plain
        .iter()
        .enumerate()
        .map(|(index, stem)| {
            let unique = plain
                .iter()
                .enumerate()
                .all(|(other, candidate)| other == index || candidate != stem);
            if unique {
                stem.clone()
            } else {
                format!("{stem}-{}", short_digest(sources[index]))
            }
        })
        .collect()
}

/// The text one payload fills a `{{slot}}` with. A summary edge carries a
/// string already; a full payload renders as the JSON the node also finds under
/// `inputs/`.
pub fn payload_text(payload: &serde_json::Value) -> String {
    match payload {
        serde_json::Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// What a `{{port}}` slot renders to, for however many upstreams fed the port
/// (§4.1, 2026-08-08 amendment).
///
/// One contributor renders exactly as it always did, so a static graph's prompt
/// is byte-identical to before. Several render as one block each, attributed
/// with the same `[karvex · from …]`-shaped frame cross-node messages use, in
/// the caller's order — node-creation order, so a generation reads the same way
/// twice. Concatenating them unattributed would leave a fan-in node unable to
/// tell whose report is whose, and taking only one would be the data loss with
/// extra steps.
pub fn port_slot_text(contributions: &[PortContribution]) -> String {
    match contributions {
        [] => String::new(),
        [only] => payload_text(&only.payload),
        many => many
            .iter()
            .map(|contribution| {
                let attribution = if contribution.label.trim().is_empty() {
                    format!("[from {}]", contribution.from)
                } else {
                    format!(
                        "[from {} · {}]",
                        contribution.from,
                        contribution.label.trim()
                    )
                };
                format!("{attribution}\n{}", payload_text(&contribution.payload))
            })
            .collect::<Vec<String>>()
            .join("\n\n"),
    }
}

/// Everything karvex writes into a node directory before the process starts.
#[derive(Debug, Clone)]
pub struct NodeDirPlan<'a> {
    /// The rendered `task.md` body; see [`TaskDocument`].
    pub task_markdown: &'a str,
    pub output_schema: &'a OutputSchema,
    /// `port -> every upstream that fired into it`, written to
    /// `inputs/<port>.json` and, when a port has more than one contributor, to
    /// one `inputs/<port>/<source>.json` per contributor.
    pub inputs: &'a BTreeMap<String, Vec<PortContribution>>,
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

    for (port, contributions) in plan.inputs {
        write_port_inputs(layout, port, contributions)?;
    }
    Ok(())
}

/// Writes one inbound port's files (§4.1, 2026-08-08 amendment).
///
/// A port with a single contributor keeps the original shape exactly:
/// `inputs/<port>.json` **is** that payload. A port several upstreams fanned
/// into gets one file per contributor under `inputs/<port>/`, and
/// `inputs/<port>.json` becomes the ordered index of them — the port still has
/// one file, and no contribution can overwrite another.
fn write_port_inputs(
    layout: &NodeDirLayout,
    port: &str,
    contributions: &[PortContribution],
) -> io::Result<()> {
    let Some((first, rest)) = contributions.split_first() else {
        return Ok(());
    };
    // A restart reuses the node directory, and this attempt's contributor set
    // may be smaller than the last one's. A leftover file would read as a
    // contribution nobody made, so the directory is rebuilt rather than merged.
    match std::fs::remove_dir_all(layout.port_dir(port)) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    if rest.is_empty() {
        return write_json(&layout.input_file(port), &first.payload);
    }

    std::fs::create_dir_all(layout.port_dir(port))?;
    let sources: Vec<&str> = contributions
        .iter()
        .map(|contribution| contribution.from.as_str())
        .collect();
    let stems = input_source_stems(&sources);
    let mut index = Vec::with_capacity(contributions.len());
    for (contribution, stem) in contributions.iter().zip(stems.iter()) {
        let file = layout.input_source_file(port, stem);
        write_json(&file, &contribution.payload)?;
        index.push(serde_json::json!({
            "from": contribution.from,
            "label": contribution.label,
            "file": format!("{}/{stem}.json", port_dir_relative(port)),
            "payload": contribution.payload,
        }));
    }
    write_json(&layout.input_file(port), &serde_json::Value::Array(index))
}

// ── run context (§4 D21) ────────────────────────────────────────────────────

/// One past run's summary, as `context/prior-runs.md` renders it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PriorRunSection<'a> {
    /// The source run's id. Rendered short — the section heading is a label,
    /// and the full record id is noise in a document an agent reads.
    pub run: &'a str,
    pub outcome: &'a str,
    pub text: &'a str,
    pub highlights: &'a [String],
    pub open_gaps: &'a [String],
}

/// `<run dir>/context/prior-runs.md`.
pub fn run_context_file(run_dir: &Path) -> PathBuf {
    run_dir.join(CONTEXT_DIR).join(PRIOR_RUNS_FILE)
}

/// The digest of what past runs of this workflow left behind (§4 D21).
///
/// **One file per run, not N×4,000 characters per node.** Injecting every
/// summary into every node's prompt would be a token tax on exactly the runs
/// history is supposed to make cheaper; the node's `task.md` gets a two-line
/// pointer at this instead, and an agent reads it when the task warrants.
pub fn render_prior_runs(workflow_name: &str, sections: &[PriorRunSection<'_>]) -> String {
    let mut out = String::new();
    out.push_str("# Prior runs\n\n");
    let workflow = workflow_name.trim();
    if workflow.is_empty() {
        out.push_str("What earlier runs of this workflow left behind, newest first.\n\n");
    } else {
        out.push_str(&format!(
            "What earlier runs of `{workflow}` left behind, newest first.\n\n"
        ));
    }
    for section in sections {
        out.push_str(&format!(
            "## Run {} — {}\n\n",
            short_run_id(section.run),
            section.outcome.trim()
        ));
        if !section.text.trim().is_empty() {
            out.push_str(section.text.trim());
            out.push_str("\n\n");
        }
        if !section.highlights.is_empty() {
            out.push_str("### Highlights\n\n");
            for highlight in section.highlights {
                out.push_str(&format!("- {}\n", highlight.trim()));
            }
            out.push('\n');
        }
        if !section.open_gaps.is_empty() {
            out.push_str("### Open gaps\n\n");
            for gap in section.open_gaps {
                out.push_str(&format!("- {}\n", gap.trim()));
            }
            out.push('\n');
        }
    }
    out
}

/// The readable half of a `workflow_run:abc123…` record id.
fn short_run_id(run: &str) -> &str {
    let key = run.split_once(':').map_or(run, |(_, key)| key);
    let end = key
        .char_indices()
        .nth(8)
        .map_or(key.len(), |(offset, _)| offset);
    &key[..end]
}

/// Creates `<run dir>/context/` and writes the prior-runs digest, returning its
/// path so the caller can point every node's `task.md` at it.
pub fn write_run_context(run_dir: &Path, body: &str) -> io::Result<PathBuf> {
    let file = run_context_file(run_dir);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&file, body)?;
    Ok(file)
}

fn write_json(file: &Path, value: &serde_json::Value) -> io::Result<()> {
    let body = serde_json::to_string_pretty(value)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    std::fs::write(file, format!("{body}\n"))
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
    /// Inbound edge ports that have an `inputs/<port>.json` file, with the
    /// per-contributor files of a fanned-in port beside them.
    pub input_ports: &'a [TaskInputPort],
    /// Absolute path of `context/prior-runs.md`, when this run was given prior
    /// summaries (`07-phase3-plan.md` §3 rule 9, §4 D21).
    ///
    /// **Absent when absent**: a run with no history — or one started with
    /// `include_prior_summaries: false` — renders byte-identically to every
    /// Phase 1–2 `task.md`, which is what keeps this frozen-contract change from
    /// invalidating existing prompt expectations (§7 R-7).
    pub prior_runs: Option<&'a str>,
}

/// One inbound port as `task.md` describes it (§4.1, 2026-08-08 amendment).
///
/// `sources` is empty for the ordinary one-edge port, whose file is exactly the
/// upstream payload. A fanned-in port lists every contributor, so a node told
/// "here is your input" can see that there are five of them and open any one of
/// them individually.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskInputPort {
    pub port: String,
    /// `(contributing instance path, path relative to the node dir)`.
    pub sources: Vec<(String, String)>,
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
            for input in self.input_ports {
                let port = &input.port;
                out.push_str(&format!(
                    "- `{port}`: `./{INPUTS_DIR}/{}.json`",
                    path_segment(port)
                ));
                if input.sources.len() > 1 {
                    // The count is stated rather than left to be counted: a
                    // fan-in node that reads one of five contributions and
                    // reports is the failure this section exists to prevent.
                    out.push_str(&format!(
                        " — {} contributions, indexed there and one file each:\n",
                        input.sources.len()
                    ));
                    for (from, file) in &input.sources {
                        out.push_str(&format!("  - `{from}`: `./{file}`\n"));
                    }
                } else {
                    out.push('\n');
                }
            }
            out.push('\n');
        }

        // Two lines, deliberately (§4 D21): the path and permission to ignore
        // it. The summaries themselves live in the file, so a node that does not
        // need history pays two lines of prompt rather than N×4,000 characters.
        if let Some(prior_runs) = self.prior_runs.map(str::trim).filter(|p| !p.is_empty()) {
            out.push_str("## Prior runs\n\n");
            out.push_str(&format!("- `{prior_runs}`\n"));
            out.push_str("- Read it if the task benefits from history.\n\n");
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

// ── pane presentation ───────────────────────────────────────────────────────

/// How wide a node pane's title is allowed to get. A run fans out over several
/// splits, so a title that does not fit the pane header is worse than a shorter
/// one that does.
pub const PANE_TITLE_BUDGET: usize = 40;

/// The pane title a node's split carries.
///
/// A `Runner::Command` node never emits an OSC title, so without this its pane
/// is a blank rectangle indistinguishable from any bare process — and `enter
/// focus` from the DAG view lands the user in exactly these panes. The title is
/// the two facts that identify a node to a human: the workflow it belongs to
/// and what the node is called.
///
/// `node_label` is the authored kvdag `label` when there is one; the caller
/// falls back to the node key, which is why this only ever trims and joins.
pub fn node_pane_title(workflow_name: &str, node_label: &str) -> String {
    let workflow = workflow_name.trim();
    let node = node_label.trim();
    let title = match (workflow.is_empty(), node.is_empty()) {
        (_, true) => workflow.to_string(),
        (true, false) => node.to_string(),
        (false, false) => format!("{workflow} · {node}"),
    };
    if title.chars().count() <= PANE_TITLE_BUDGET {
        return title;
    }
    title
        .chars()
        .take(PANE_TITLE_BUDGET - 1)
        .collect::<String>()
        + "…"
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
    argv.push(seed_prompt(spec));
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

fn seed_prompt(spec: &SpawnSpec) -> String {
    if spec.seed_prompt.trim().is_empty() {
        seed_prompt_for(&spec.node_dir)
    } else {
        spec.seed_prompt.clone()
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

/// The first 16 bytes of a digest, formatted as a v4-tagged uuid.
///
/// Shared with `binding::interrogate`'s forked-session mint for the reason the
/// caller above documents: `claude --session-id` validates against the common
/// v1–v5 shape, so both mints have to tag the same nibble.
pub fn format_uuid(bytes: &[u8]) -> String {
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

/// The keys `RunEffect::SendKeys` carries for an interrupt (§5), by runner.
pub fn interrupt_keys(runner: Runner) -> Vec<String> {
    let keys: &[&str] = match runner {
        Runner::Agent => &AGENT_INTERRUPT_KEYS,
        Runner::Command => &COMMAND_INTERRUPT_KEYS,
    };
    keys.iter().map(|key| (*key).to_string()).collect()
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

    /// 2.20: a command node's pane emits no OSC title, so without this it is a
    /// blank rectangle — and `enter focus` from the DAG view lands in exactly
    /// these panes.
    #[test]
    fn a_node_pane_is_titled_with_its_workflow_and_its_node() {
        assert_eq!(
            node_pane_title("ux-dag-probe", "Typecheck"),
            "ux-dag-probe · Typecheck"
        );
    }

    #[test]
    fn a_node_pane_title_falls_back_to_whichever_name_it_has() {
        assert_eq!(node_pane_title("", "typecheck"), "typecheck");
        assert_eq!(node_pane_title("  ", " typecheck "), "typecheck");
        assert_eq!(node_pane_title("ux-dag-probe", "   "), "ux-dag-probe");
        assert_eq!(node_pane_title("", ""), "");
    }

    /// A run fans out over several splits, so a title that does not fit the
    /// pane header is worse than a shorter one that does.
    #[test]
    fn a_long_node_pane_title_is_trimmed_to_the_budget() {
        let title = node_pane_title(&"w".repeat(40), &"n".repeat(40));
        assert_eq!(title.chars().count(), PANE_TITLE_BUDGET);
        assert!(title.ends_with('…'), "{title}");
    }

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
                seed_prompt_for(&PathBuf::from("/runs/run-1/plan")),
            ]
        );
    }

    /// The node's cwd is the workspace, not the node directory, so a relative
    /// `./task.md` in the seed prompt names a file the agent cannot open. The
    /// path has to be the absolute one `--add-dir` just made readable.
    #[test]
    fn the_default_seed_prompt_names_task_md_by_absolute_path() {
        let spec = agent_spec();
        let seed = agent_argv(&spec)
            .last()
            .cloned()
            .expect("the argv ends with the seed prompt");

        let task = PathBuf::from("/runs/run-1/plan").join(TASK_FILE);
        assert!(
            seed.contains(&task.display().to_string()),
            "the seed prompt must name {}: {seed}",
            task.display()
        );
        assert!(
            !seed.contains("./task.md"),
            "a cwd-relative task.md does not resolve from the node's cwd: {seed}"
        );
        assert!(
            std::path::Path::new(&task).is_absolute(),
            "the node dir the seed points at is absolute"
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

    /// A private directory per test, so the filesystem-touching cases here
    /// never share a node directory.
    fn temp_base() -> PathBuf {
        std::env::temp_dir().join(format!(
            "karvex-workflow-spawn-{}-{}",
            std::process::id(),
            NEXT_TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn materialise_writes_the_contract_and_clears_a_stale_result() {
        let base = temp_base();
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
        inputs.insert(
            "plan".to_string(),
            vec![contribution("plan", serde_json::json!({ "summary": "ok" }))],
        );

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

    /// One contributor on a port, which is what an ordinary single-edge port
    /// carries.
    fn contribution(from: &str, payload: serde_json::Value) -> PortContribution {
        PortContribution {
            from: from.to_string(),
            label: String::new(),
            payload,
        }
    }

    fn port(name: &str, sources: &[(&str, &str)]) -> TaskInputPort {
        TaskInputPort {
            port: name.to_string(),
            sources: sources
                .iter()
                .map(|(from, file)| ((*from).to_string(), (*file).to_string()))
                .collect(),
        }
    }

    #[test]
    fn task_document_names_the_result_contract() {
        let ports = vec![port("plan", &[])];
        let rendered = TaskDocument {
            label: "Implement",
            role: "You are the implementer.",
            contract: "Reply only through result.json.",
            prompt: "Implement this plan.",
            input_ports: &ports,
            prior_runs: None,
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

    /// The retest's third P0: §3.4's inherited fan-in gives one port a whole
    /// generation of contributors, and `inputs/<port>.json` used to be written
    /// once per port — last writer wins, five of six results gone with nothing
    /// said. Every contribution must land in its own file, and the port's own
    /// file must index all of them.
    #[test]
    fn a_fanned_in_port_keeps_one_file_per_contributor() {
        let base = temp_base();
        let layout = NodeDirLayout::new(base.join("collect"));
        let schema = OutputSchema::parse(serde_json::json!({ "type": "object" })).unwrap();
        let mut inputs = BTreeMap::new();
        inputs.insert(
            "shard".to_string(),
            vec![
                contribution("fanout/worker/1", serde_json::json!("report one")),
                contribution("fanout/worker/2", serde_json::json!("report two")),
                contribution("fanout", serde_json::json!("the plan")),
            ],
        );

        materialise_node_dir(
            &layout,
            &NodeDirPlan {
                task_markdown: "# Collect\n",
                output_schema: &schema,
                inputs: &inputs,
            },
        )
        .unwrap();

        let read = |path: PathBuf| -> serde_json::Value {
            serde_json::from_str(
                &std::fs::read_to_string(&path)
                    .unwrap_or_else(|err| panic!("{} is missing: {err}", path.display())),
            )
            .expect("valid json")
        };
        assert_eq!(
            read(layout.input_source_file("shard", "fanout-worker-1")),
            serde_json::json!("report one")
        );
        assert_eq!(
            read(layout.input_source_file("shard", "fanout-worker-2")),
            serde_json::json!("report two")
        );
        assert_eq!(
            read(layout.input_source_file("shard", "fanout")),
            serde_json::json!("the plan"),
            "the parent contributes to the same port its children inherited"
        );

        let index = read(layout.input_file("shard"));
        let entries = index.as_array().expect("the port file indexes its sources");
        assert_eq!(
            entries.len(),
            3,
            "no contribution may be overwritten by another: {index}"
        );
        assert_eq!(entries[0]["from"], "fanout/worker/1");
        assert_eq!(entries[0]["file"], "inputs/shard/fanout-worker-1.json");
        assert_eq!(entries[0]["payload"], serde_json::json!("report one"));

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A restart reuses the node directory. A contributor that is no longer
    /// upstream must not survive as a file the node reads as this attempt's
    /// input.
    #[test]
    fn a_second_materialisation_rebuilds_a_ports_directory() {
        let base = temp_base();
        let layout = NodeDirLayout::new(base.join("collect"));
        let schema = OutputSchema::parse(serde_json::json!({ "type": "object" })).unwrap();
        let plan_with = |inputs: &BTreeMap<String, Vec<PortContribution>>| {
            materialise_node_dir(
                &layout,
                &NodeDirPlan {
                    task_markdown: "# Collect\n",
                    output_schema: &schema,
                    inputs,
                },
            )
            .unwrap();
        };

        let mut first = BTreeMap::new();
        first.insert(
            "shard".to_string(),
            vec![
                contribution("worker/1", serde_json::json!("one")),
                contribution("worker/2", serde_json::json!("two")),
            ],
        );
        plan_with(&first);
        assert!(layout.input_source_file("shard", "worker-2").exists());

        let mut second = BTreeMap::new();
        second.insert(
            "shard".to_string(),
            vec![contribution("worker/1", serde_json::json!("one"))],
        );
        plan_with(&second);
        assert!(
            !layout.input_source_file("shard", "worker-2").exists(),
            "a stale contributor would read as an input nobody produced"
        );
        assert_eq!(
            std::fs::read_to_string(layout.input_file("shard"))
                .unwrap()
                .trim(),
            "\"one\"",
            "a port back down to one contributor keeps the original shape"
        );

        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Sanitising an instance path is lossy, and two contributions resolving to
    /// one file is the silent loss this shape exists to end.
    #[test]
    fn contributor_file_names_are_unique_even_when_sanitising_collides() {
        assert_eq!(
            input_source_stems(&["fanout/worker/1", "fanout/worker/2"]),
            vec!["fanout-worker-1".to_string(), "fanout-worker-2".to_string()]
        );
        let colliding = input_source_stems(&["a/b", "a-b"]);
        assert_eq!(colliding.len(), 2);
        assert_ne!(
            colliding[0], colliding[1],
            "two sources may never name one file: {colliding:?}"
        );
        assert!(colliding.iter().all(|stem| stem.starts_with("a-b")));
    }

    /// The `{{port}}` slot is the other half of the fan-in: a node whose port
    /// carries a generation must see the whole generation, attributed.
    #[test]
    fn a_fanned_in_slot_renders_every_contribution_attributed() {
        assert_eq!(
            port_slot_text(&[contribution("plan", serde_json::json!("one step"))]),
            "one step",
            "a single contributor renders exactly as it always did"
        );

        let mut first = contribution("fanout/worker/1", serde_json::json!("auth done"));
        first.label = "Shard: auth".to_string();
        let second = contribution("fanout/worker/2", serde_json::json!("ui done"));
        assert_eq!(
            port_slot_text(&[first, second]),
            "[from fanout/worker/1 · Shard: auth]\nauth done\n\n[from fanout/worker/2]\nui done"
        );
        assert_eq!(port_slot_text(&[]), "");
    }

    #[test]
    fn task_document_lists_every_contributor_of_a_fanned_in_port() {
        let ports = vec![port(
            "shard",
            &[
                ("fanout/worker/1", "inputs/shard/fanout-worker-1.json"),
                ("fanout/worker/2", "inputs/shard/fanout-worker-2.json"),
            ],
        )];
        let rendered = TaskDocument {
            label: "Collect",
            role: "",
            contract: "",
            prompt: "Collect them.",
            input_ports: &ports,
            prior_runs: None,
        }
        .render();

        assert!(rendered.contains("- `shard`: `./inputs/shard.json`"));
        assert!(
            rendered.contains("2 contributions"),
            "the count is stated so a node cannot read one of two and report: {rendered}"
        );
        assert!(rendered.contains("- `fanout/worker/1`: `./inputs/shard/fanout-worker-1.json`"));
        assert!(rendered.contains("- `fanout/worker/2`: `./inputs/shard/fanout-worker-2.json`"));
    }

    #[test]
    fn task_document_omits_empty_optional_sections() {
        let rendered = TaskDocument {
            label: "Plan",
            role: "  ",
            contract: "",
            prompt: "Do the thing.",
            input_ports: &[],
            prior_runs: None,
        }
        .render();
        assert!(!rendered.contains("## Role"));
        assert!(!rendered.contains("## Contract"));
        assert!(!rendered.contains("## Inputs"));
        assert!(rendered.contains("## Task"));
    }

    /// §7 R-7 / §3 rule 9: the `## Prior runs` section is the one
    /// frozen-contract change Phase 3 makes to `task.md`, and it is
    /// **absent-when-absent** — a run with no history renders byte-identically
    /// to every Phase 1–2 task document, so no existing prompt expectation is
    /// invalidated.
    #[test]
    fn the_prior_runs_section_is_absent_when_the_run_has_no_history() {
        let document = TaskDocument {
            label: "Plan",
            role: "You plan.",
            contract: "Reply only through result.json.",
            prompt: "Do the thing.",
            input_ports: &[],
            prior_runs: None,
        };
        let without = document.render();
        assert!(!without.contains("## Prior runs"), "{without}");

        let with = TaskDocument {
            prior_runs: Some("/runs/r1/context/prior-runs.md"),
            ..document
        }
        .render();
        assert!(with.contains("## Prior runs"), "{with}");
        assert!(
            with.contains("`/runs/r1/context/prior-runs.md`"),
            "the section is a pointer, so it has to carry the path: {with}"
        );
        assert_eq!(
            with.replace(
                "## Prior runs\n\n- `/runs/r1/context/prior-runs.md`\n- Read it if the task \
                 benefits from history.\n\n",
                ""
            ),
            without,
            "§4 D21: two lines and nothing else — the rest of the document is untouched"
        );
    }

    /// §4 D21: the digest is one file per run, and it says which run each
    /// section came from in a form a reader can scan.
    #[test]
    fn the_prior_runs_digest_renders_one_section_per_summary() {
        let highlights = vec!["shipped the parser".to_string()];
        let gaps = vec!["no windows coverage".to_string()];
        let rendered = render_prior_runs(
            "release",
            &[
                PriorRunSection {
                    run: "workflow_run:abcdef0123456789",
                    outcome: "succeeded",
                    text: "the parser landed",
                    highlights: &highlights,
                    open_gaps: &gaps,
                },
                PriorRunSection {
                    run: "workflow_run:0011223344",
                    outcome: "failed",
                    text: "the build broke",
                    highlights: &[],
                    open_gaps: &[],
                },
            ],
        );
        assert!(
            rendered.contains("## Run abcdef01 — succeeded"),
            "{rendered}"
        );
        assert!(rendered.contains("## Run 00112233 — failed"), "{rendered}");
        assert!(rendered.contains("- shipped the parser"), "{rendered}");
        assert!(rendered.contains("- no windows coverage"), "{rendered}");
        assert!(
            !rendered.contains("workflow_run:"),
            "the record-id prefix is noise in a document an agent reads: {rendered}"
        );
        assert!(
            !rendered.contains("### Highlights\n\n### Open gaps"),
            "an empty list renders no heading at all: {rendered}"
        );
    }

    #[test]
    fn a_short_run_id_survives_a_key_shorter_than_the_window() {
        assert_eq!(short_run_id("workflow_run:abc"), "abc");
        assert_eq!(short_run_id("bare"), "bare");
        assert_eq!(short_run_id("workflow_run:"), "");
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
        assert_eq!(
            interrupt_keys(Runner::Agent),
            vec!["Escape".to_string()],
            "an agent's turn is stopped with Escape"
        );
        let (code, modifiers) = crate::config::parse_key_combo(AGENT_INTERRUPT_KEYS[0]).unwrap();
        assert_eq!(code, crossterm::event::KeyCode::Esc);
        assert!(modifiers.is_empty());
    }

    /// Escape is a convention of the `claude` TUI. A `Runner::Command` node is
    /// a plain process that ignores it; the interrupt it does observe is
    /// `ctrl+c`, which the PTY line discipline turns into SIGINT.
    #[test]
    fn a_command_node_is_interrupted_with_a_signal_it_can_observe() {
        assert_eq!(interrupt_keys(Runner::Command), vec!["ctrl+c".to_string()]);
        let (code, modifiers) = crate::config::parse_key_combo(COMMAND_INTERRUPT_KEYS[0]).unwrap();
        assert_eq!(code, crossterm::event::KeyCode::Char('c'));
        assert!(modifiers.contains(crossterm::event::KeyModifiers::CONTROL));
    }
}
