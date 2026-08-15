//! Run-directory layout and the pane-geometry constants a workflow spawn needs.
//!
//! What is left here after the engine's removal is the part that outlives it:
//! where a run's files go on disk, how arbitrary author text is made safe as a
//! path segment, and the `context/prior-runs.md` digest a run is given when it
//! is launched with history. The team lead reads that digest; nothing renders
//! per-node contracts any more.
//!
//! Everything in this module is pure except [`write_run_context`], which is a
//! single directory-create plus write, so the module stays unit-testable
//! without PTYs.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::workflow::model::RunId;

// ── environment (§4.2) ──────────────────────────────────────────────────────

/// Exported into the lead's pane so its `kvx` calls self-identify.
pub const RUN_ID_ENV_VAR: &str = "KARVEX_WORKFLOW_RUN_ID";

/// Overrides where run directories are created, mirroring the store's
/// `KARVEX_WORKFLOW_DB_PATH`. Run directories cannot live *inside*
/// `state_dir()/workflow`: that path is the SurrealKV database directory.
pub const RUNS_DIR_ENV_VAR: &str = "KARVEX_WORKFLOW_RUNS_DIR";

// ── run directory (§4.1) ────────────────────────────────────────────────────

/// Run-level context, a sibling of any per-node directories: `<run dir>/context/`
/// (`07-phase3-plan.md` §4 D21).
pub const CONTEXT_DIR: &str = "context";
/// The prior-runs digest the lead prompt points at.
pub const PRIOR_RUNS_FILE: &str = "prior-runs.md";

/// Mirrors the settle delay and launch window `agent.start` uses. Those
/// constants are private to `src/app/agents.rs`; a lead spawn is the same shape
/// of launch, and the manifest detector's confirmation only behaves like
/// `agent.start` if the window matches.
pub const NODE_AGENT_SETTLE_DELAY: Duration = Duration::from_secs(3);
pub const NODE_AGENT_LAUNCH_WINDOW: Duration = Duration::from_secs(30);

/// `split_pane_argv_command` is given a geometry estimate; the existing
/// in-process callers clamp it the same way before splitting.
pub(crate) const MIN_PANE_ROWS: u16 = 4;
pub(crate) const MIN_PANE_COLS: u16 = 10;

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

/// One filesystem-safe path segment from arbitrary caller text.
///
/// A run id carries a `workflow_run:` prefix and the colon must never reach the
/// filesystem, so the traversal rules live here rather than being approximated
/// at each call site.
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

fn short_digest(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    hex(&digest[..4])
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
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
/// summary into every prompt would be a token tax on exactly the runs history
/// is supposed to make cheaper; the lead prompt gets a pointer at this instead,
/// and the team reads it when the work warrants.
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
/// path so the caller can point the lead prompt at it.
pub fn write_run_context(run_dir: &Path, body: &str) -> io::Result<PathBuf> {
    let file = run_context_file(run_dir);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&file, body)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_directory_is_one_sanitised_segment_under_the_root() {
        let run = run_dir(
            Path::new("/state/workflow-runs"),
            &RunId("run-1".to_string()),
        );
        assert_eq!(run, PathBuf::from("/state/workflow-runs/run-1"));

        // The record-id colon and any traversal attempt collapse into a single
        // safe segment rather than reaching the filesystem as structure.
        let record = run_dir(
            Path::new("/state/workflow-runs"),
            &RunId("workflow_run:abc".to_string()),
        );
        assert_eq!(
            record,
            PathBuf::from("/state/workflow-runs/workflow_run-abc")
        );
        let sneaky = run_dir(
            Path::new("/state/workflow-runs"),
            &RunId("../../etc/passwd".to_string()),
        );
        assert!(sneaky.starts_with("/state/workflow-runs"));
        assert!(!sneaky.to_string_lossy().contains(".."));
        assert_eq!(sneaky.components().count(), 4);
    }

    #[test]
    fn a_dot_only_segment_is_renamed_rather_than_dropped() {
        // Dropping it would let two distinct ids resolve to the same directory.
        assert_ne!(path_segment("."), path_segment(".."));
        assert!(!path_segment("..").contains('.'));
        assert!(!path_segment("").is_empty());
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
    fn the_run_context_file_is_the_path_the_writer_returns() {
        let dir = std::env::temp_dir().join(format!(
            "karvex-run-context-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let written = write_run_context(&dir, "# Prior runs\n").expect("write run context");
        assert_eq!(written, run_context_file(&dir));
        assert_eq!(
            std::fs::read_to_string(&written).expect("read back"),
            "# Prior runs\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
