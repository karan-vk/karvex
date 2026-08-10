//! Interrogation binding: reviving a finished node's Claude session in a pane
//! (`07-phase3-plan.md` §4 D7, `03-storage-schema.md` §4.4).
//!
//! The third binding half, beside `spawn` (nodes → panes) and `observe`
//! (runtime facts → engine inputs). It is deliberately **not** part of either:
//! an interrogation is not a run node (§4 D8). It has no node token, no
//! `RunGraph` entry, no place in layout's layered graph, and no counter — so a
//! spawn shape that reused [`crate::workflow::model::SpawnSpec`] would carry
//! exactly the fields an interrogation must not have.
//!
//! Two spawn shapes:
//!
//! - **Resumed** — `claude --session-id <fork> --resume <source> --fork-session`
//!   in the node's recorded cwd. `--fork-session` is what makes this
//!   non-mutating: Claude writes the forked transcript under the *new* session
//!   id and never touches the source `<sid>.jsonl`.
//! - **Reconstructed** — a fresh Claude seeded from a karvex-authored task file
//!   built out of the node's stored checkpoint. Its first line states that it
//!   is a reconstruction, because `00-overview.md` Feature 3 requires the
//!   degraded path never to be presented as the original teammate.
//!
//! Everything here is pure except [`materialise_interrogation_dir`] and
//! [`spawn_interrogation_pane`], so the argv, the pane title, and the seed
//! document are all testable without a PTY.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::detect::Agent;
use crate::workflow::binding::spawn::{
    self, format_uuid, path_segment, SpawnError, PANE_TITLE_BUDGET,
};
use crate::workflow::model::{InstancePath, InterrogationId};
use crate::workspace::{NewPane, Workspace};

/// Where a run's interrogation seeds live: a reserved sibling of the node
/// directories under the same run dir.
///
/// `.`-prefixed for the same reason the epilogue's instance path is
/// (`07-phase3-plan.md` §3 rule 3): node keys can no longer start with `.`, so
/// this directory can never collide with a node directory, however the author
/// names their nodes.
pub const INTERROGATIONS_DIR: &str = ".interrogations";

/// The seed task file a reconstructed interrogation reads.
pub const SEED_FILE: &str = "task.md";

/// `<run dir>/.interrogations/<id>/`. The id is sanitised the same way an
/// instance-path segment is, so the `interrogation:` record-id colon never
/// reaches the filesystem.
pub fn interrogation_dir(run_dir: &Path, id: &InterrogationId) -> PathBuf {
    run_dir
        .join(INTERROGATIONS_DIR)
        .join(path_segment(id.as_str()))
}

/// Creates the interrogation directory and writes its seed task file, returning
/// the seed's absolute path.
pub fn materialise_interrogation_dir(dir: &Path, seed_markdown: &str) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let file = dir.join(SEED_FILE);
    std::fs::write(&file, seed_markdown)?;
    Ok(file)
}

// ── identity ────────────────────────────────────────────────────────────────

static NEXT_INTERROGATION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// Allocates the record id the `interrogation` row is created under.
///
/// Minted by the app rather than the database so `StoreWrite::InterrogationStarted`
/// and the later `StoreWrite::InterrogationUpdate` address the same row with no
/// read-back between them (`07-phase3-plan.md` §3 rule 4). Spelled
/// `interrogation:<hex>` because the store's `parse_record_id` requires the
/// table prefix.
pub fn mint_interrogation_id() -> InterrogationId {
    InterrogationId::new(format!("interrogation:{}", mint_hex("id")))
}

/// The session id the fork runs under, passed as `claude --session-id`.
///
/// Minted rather than derived from `(run, path)` the way a node's session id is
/// ([`spawn::derive_agent_session_id`]): a node has one session per attempt, but
/// a node can be interrogated any number of times sequentially, and two forks
/// sharing a session id would have Claude write both transcripts to one file.
/// The v4 nibble is tagged for the same reason `derive_agent_session_id` tags
/// it — `claude --session-id` validates against the common uuid shape.
pub fn mint_forked_session_id() -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"karvex/workflow/interrogation-session\0");
    hasher.update(mint_hex("session").as_bytes());
    format_uuid(hasher.finalize().as_slice())
}

/// Process-local uniqueness for the two mints above: wall clock, process id,
/// and a counter, hashed so none of them is recoverable. karvex has no CSPRNG
/// dependency, and neither of these is a secret — they are anti-collision ids.
fn mint_hex(domain: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let sequence = NEXT_INTERROGATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(b"karvex/workflow/interrogation\0");
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(sequence.to_le_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for byte in &digest[..16] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// ── argv ────────────────────────────────────────────────────────────────────

/// The resumed fork's argv (`07-phase3-plan.md` §3 rule 7, as amended by the
/// step-0 spike): the `--session-id` pre-assignment verified against the
/// installed `claude`, so the forked id is known at record-creation time
/// instead of being learned asynchronously (§4 D7's preferred path).
///
/// Deliberately nothing else. No `--name`: the label an interrogation shows in
/// the sidebar is set through `begin_managed_agent`, and passing it to `claude`
/// too would let a fork collide with a live agent's name. No trailing seed
/// prompt: a resumed fork opens on the source transcript and the human types
/// the question — the `note` is recorded on the row, not injected as a turn.
/// No `--add-dir`: the fork inherits the session's own directory grants.
pub fn resumed_argv(source_session_id: &str, forked_session_id: &str) -> Vec<String> {
    vec![
        crate::detect::interactive_agent_executable(Agent::Claude).to_string(),
        "--session-id".to_string(),
        forked_session_id.to_string(),
        "--resume".to_string(),
        source_session_id.to_string(),
        "--fork-session".to_string(),
    ]
}

/// The reconstructed stand-in's argv: a fresh session, granted the seed
/// directory, opened on the seed document.
///
/// `--resume` is absent by construction — there is no transcript to resume,
/// which is the only reason this path exists.
pub fn reconstructed_argv(
    forked_session_id: &str,
    seed_dir: &Path,
    seed_file: &Path,
) -> Vec<String> {
    vec![
        crate::detect::interactive_agent_executable(Agent::Claude).to_string(),
        "--session-id".to_string(),
        forked_session_id.to_string(),
        "--add-dir".to_string(),
        seed_dir.to_string_lossy().into_owned(),
        format!("Read {} and follow it.", seed_file.to_string_lossy()),
    ]
}

// ── presentation ────────────────────────────────────────────────────────────

/// `interrogate · <workflow> · <path>`, or `reconstructed · <workflow> · <path>`
/// for the degraded path (§4 D7).
///
/// The mode is the *first* segment rather than a suffix so it survives the
/// pane-title budget: a reconstruction that truncated to look like a real fork
/// is exactly what "never presented as the original" forbids.
pub fn interrogation_pane_title(workflow_name: &str, path: &str, reconstructed: bool) -> String {
    let mode = if reconstructed {
        "reconstructed"
    } else {
        "interrogate"
    };
    let mut title = mode.to_string();
    let workflow = workflow_name.trim();
    if !workflow.is_empty() {
        title.push_str(" · ");
        title.push_str(workflow);
    }
    let path = path.trim();
    if !path.is_empty() {
        title.push_str(" · ");
        title.push_str(path);
    }
    if title.chars().count() <= PANE_TITLE_BUDGET {
        return title;
    }
    title
        .chars()
        .take(PANE_TITLE_BUDGET - 1)
        .collect::<String>()
        + "…"
}

/// The karvex agent name the pane's managed-agent confirmation registers.
///
/// Keyed on the interrogation's own id, not the node's label: a node can be
/// interrogated several times in sequence and agent names have to stay unique
/// among live agents, exactly as `agent.start` requires.
pub fn interrogation_agent_name(path: &InstancePath, id: &InterrogationId) -> String {
    let suffix = id
        .as_str()
        .rsplit(':')
        .next()
        .unwrap_or_default()
        .chars()
        .take(6)
        .collect::<String>();
    if suffix.is_empty() {
        format!("interrogate-{}", path_segment(path.as_str()))
    } else {
        format!("interrogate-{}-{suffix}", path_segment(path.as_str()))
    }
}

// ── the reconstructed seed document ─────────────────────────────────────────

/// What a reconstructed interrogation is told, from the stored outputs of the
/// node it stands in for.
#[derive(Debug, Clone, Default)]
pub struct ReconstructedSeed<'a> {
    pub workflow_name: &'a str,
    pub run: &'a str,
    pub path: &'a str,
    pub label: &'a str,
    /// The checkpoint the seed was built from.
    pub checkpoint_seq: u64,
    pub summary: &'a str,
    pub payload: &'a str,
    /// The node's original `task.md`, when its node directory survived.
    pub original_task: Option<&'a str>,
    /// The caller's question, when `workflow.node.interrogate` carried one.
    pub note: &'a str,
}

impl ReconstructedSeed<'_> {
    /// The seed's **first line** says what this session is, before anything
    /// else it could be mistaken for (`00-overview.md` Feature 3, made
    /// mechanical): a reconstruction from stored outputs, not the original
    /// session.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "You are a **reconstruction**, not the original teammate: that session's transcript \
             is gone, and everything below was rebuilt from karvex's stored record of what the \
             node produced. Say so if you are asked what you remember — you remember nothing; \
             you are reading a file.\n\n",
        );
        out.push_str(&format!("# reconstructed · {}\n\n", self.label.trim()));
        out.push_str("## What this node was\n\n");
        out.push_str(&format!("- workflow: `{}`\n", self.workflow_name.trim()));
        out.push_str(&format!("- run: `{}`\n", self.run.trim()));
        out.push_str(&format!("- node: `{}`\n", self.path.trim()));
        out.push_str(&format!("- checkpoint: seq {}\n\n", self.checkpoint_seq));

        if !self.summary.trim().is_empty() {
            out.push_str("## What it reported\n\n");
            out.push_str(self.summary.trim());
            out.push_str("\n\n");
        }
        if !self.payload.trim().is_empty() {
            out.push_str("## Its full result\n\n```json\n");
            out.push_str(self.payload.trim());
            out.push_str("\n```\n\n");
        }
        if let Some(task) = self.original_task.map(str::trim).filter(|t| !t.is_empty()) {
            out.push_str("## The task it was given\n\n");
            out.push_str(task);
            out.push_str("\n\n");
        }
        if !self.note.trim().is_empty() {
            out.push_str("## What you are being asked\n\n");
            out.push_str(self.note.trim());
            out.push('\n');
        } else {
            out.push_str(
                "Wait for the human's question and answer it from the record above, saying \
                 plainly when the record does not contain the answer.\n",
            );
        }
        out
    }
}

// ── runtime ─────────────────────────────────────────────────────────────────

/// Creates the interrogation's pane.
///
/// The environment is **empty** (`07-phase3-plan.md` §3 rule 7, §4 D7): not
/// just `KARVEX_WORKFLOW_NODE_TOKEN` but every `KARVEX_WORKFLOW_*` variable is
/// withheld, because `kvx workflow node complete` resolves its target from
/// `KARVEX_WORKFLOW_NODE_DIR` and its authority from the token — an
/// interrogation must not be able to report on the source node's behalf, and
/// handing it three quarters of the contract is a worse failure than handing it
/// none.
pub fn spawn_interrogation_pane(
    workspace: &mut Workspace,
    context: spawn::PaneSpawnContext,
    argv: &[String],
    cwd: &Path,
) -> Result<(usize, NewPane), SpawnError> {
    for argument in argv {
        if argument.contains('\0') {
            return Err(SpawnError::InvalidArgument(argument.clone()));
        }
    }
    let result = workspace.split_pane_argv_command(
        context.target_pane,
        context.direction,
        context.rows.max(4),
        context.cols.max(10),
        Some(cwd.to_path_buf()),
        argv,
        Vec::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resumed_argv_is_the_frozen_six_tokens_in_order() {
        let argv = resumed_argv("source-sid", "forked-sid");
        assert_eq!(
            argv,
            vec![
                crate::detect::interactive_agent_executable(Agent::Claude).to_string(),
                "--session-id".to_string(),
                "forked-sid".to_string(),
                "--resume".to_string(),
                "source-sid".to_string(),
                "--fork-session".to_string(),
            ],
            "`07-phase3-plan.md` §3 rule 7: the fork's argv is frozen contract"
        );
    }

    #[test]
    fn the_reconstructed_argv_never_resumes_anything() {
        let argv = reconstructed_argv(
            "forked-sid",
            Path::new("/runs/r1/.interrogations/i1"),
            Path::new("/runs/r1/.interrogations/i1/task.md"),
        );
        assert!(
            !argv
                .iter()
                .any(|arg| arg == "--resume" || arg == "--fork-session"),
            "there is no transcript to resume — that is why this path exists: {argv:?}"
        );
        assert!(argv.iter().any(|arg| arg == "--add-dir"));
    }

    #[test]
    fn two_forked_session_ids_never_collide() {
        let first = mint_forked_session_id();
        let second = mint_forked_session_id();
        assert_ne!(
            first, second,
            "a node can be interrogated repeatedly; two forks sharing a session id \
             would write both transcripts to one file"
        );
        assert_eq!(first.len(), 36, "uuid shape: {first}");
    }

    #[test]
    fn a_minted_interrogation_id_carries_the_table_prefix_the_store_parses() {
        let id = mint_interrogation_id();
        assert!(
            id.as_str().starts_with("interrogation:"),
            "the store's `parse_record_id` requires it: {id}"
        );
        assert_ne!(id, mint_interrogation_id());
    }

    #[test]
    fn the_reconstructed_title_leads_with_its_mode() {
        let title = interrogation_pane_title("release", "plan", true);
        assert!(title.starts_with("reconstructed · "), "{title}");
        let title = interrogation_pane_title("release", "plan", false);
        assert_eq!(title, "interrogate · release · plan");
    }

    #[test]
    fn a_long_reconstructed_title_still_leads_with_its_mode() {
        let title = interrogation_pane_title(&"w".repeat(200), &"p".repeat(200), true);
        assert!(
            title.starts_with("reconstructed"),
            "truncation must never make a reconstruction look like a real fork: {title}"
        );
        assert!(title.chars().count() <= PANE_TITLE_BUDGET);
    }

    #[test]
    fn the_seed_says_it_is_a_reconstruction_in_its_first_line() {
        let seed = ReconstructedSeed {
            workflow_name: "release",
            run: "workflow_run:1",
            path: "plan",
            label: "plan",
            checkpoint_seq: 1,
            summary: "listed the release steps",
            payload: "{\"steps\":3}",
            original_task: Some("# plan\n\nPlan the release."),
            note: "why three attempts?",
        }
        .render();
        let first = seed.lines().next().unwrap_or_default();
        assert!(
            first.contains("reconstruction"),
            "00 Feature 3: never presented as the original teammate: {first}"
        );
        assert!(seed.contains("listed the release steps"));
        assert!(seed.contains("Plan the release."));
        assert!(seed.contains("why three attempts?"));
    }

    #[test]
    fn a_seed_with_no_note_still_tells_the_session_what_to_do() {
        let seed = ReconstructedSeed {
            label: "plan",
            summary: "did the thing",
            ..ReconstructedSeed::default()
        }
        .render();
        assert!(seed.contains("Wait for the human's question"), "{seed}");
    }

    #[test]
    fn the_interrogation_directory_is_inside_the_run_dir_and_carries_no_colon() {
        let dir = interrogation_dir(
            Path::new("/runs/workflow_run-1"),
            &InterrogationId::new("interrogation:abc123"),
        );
        assert!(dir.starts_with("/runs/workflow_run-1"));
        assert!(
            !dir.to_string_lossy().contains(':'),
            "a record-id colon must never reach the filesystem: {dir:?}"
        );
    }

    #[test]
    fn the_agent_name_distinguishes_two_interrogations_of_one_node() {
        let path = InstancePath::new("research/2/verify");
        let first =
            interrogation_agent_name(&path, &InterrogationId::new("interrogation:aaaaaa11"));
        let second =
            interrogation_agent_name(&path, &InterrogationId::new("interrogation:bbbbbb22"));
        assert_ne!(
            first, second,
            "agent names have to stay unique among live agents"
        );
        assert!(!first.contains('/'), "{first}");
    }
}
