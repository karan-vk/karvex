//! Typed rows stored in redb, plus the record-id plumbing shared with the read
//! surface.
//!
//! Every row is plain `serde` and is written as JSON by `db::encode`. Optional
//! and collection fields carry `#[serde(default)]` so a row written by an older
//! karvex still decodes after a field is added — the store's forward
//! compatibility story, in place of a schema migration per field.
//!
//! Ids inside a row are always the *key* (`w000000000001-v000001`), never the
//! `table:key` form. The `table:` prefix is added only at the boundary where an
//! id leaves the store, by [`record_id_to_string`].

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

// ── identity plumbing ───────────────────────────────────────────────────────

/// Every id newtype in `workflow::model` (`WorkflowId`, `RunId`, ...) holds the
/// full `table:key` string, matching how the fixtures in `model.rs`'s own tests
/// construct them (e.g. `WorkflowId::new("workflow:1")`). These two helpers
/// convert between that public string form and the bare record key the redb
/// tables are actually indexed by.
pub fn record_id_to_string(table: &str, key: &str) -> String {
    format!("{table}:{key}")
}

/// Parses a `table:key` string minted by [`record_id_to_string`] back into the
/// bare key. Returns `None` if `full_id` does not belong to `table` or carries
/// no key at all.
pub fn parse_record_id(table: &str, full_id: &str) -> Option<String> {
    let (prefix, key) = full_id.split_once(':')?;
    if prefix != table || key.is_empty() {
        return None;
    }
    Some(key.to_string())
}

// ── definitions ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRow {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub head_version: Option<String>,
    pub default_tier: String,
    #[serde(default)]
    pub archived: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KvdagVersionRow {
    pub id: String,
    pub workflow: String,
    pub version: i64,
    #[serde(default)]
    pub parent: Option<String>,
    pub origin: String,
    #[serde(default)]
    pub change_summary: String,
    #[serde(default)]
    pub contract: String,
    #[serde(default)]
    pub args: Json,
    pub max_depth: i64,
    pub max_nodes: i64,
    pub spec_digest: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KvdagNodeRow {
    pub id: String,
    pub version: String,
    pub node_key: String,
    pub label: String,
    #[serde(default)]
    pub role: String,
    pub kind: String,
    pub runner: String,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    pub demand: String,
    pub prompt_template: String,
    #[serde(default)]
    pub system_contract: Option<String>,
    pub output_schema: Json,
    pub max_attempts: i64,
    #[serde(default)]
    pub timeout_ms: Option<i64>,
    pub isolation: String,
    #[serde(default)]
    pub is_template: bool,
    #[serde(default)]
    pub expand_allow: Vec<String>,
    #[serde(default)]
    pub expand_max: i64,
    /// Author-supplied canvas coordinates. Carried, never interpreted here.
    #[serde(default)]
    pub position: Option<Json>,
}

/// `r#in`/`out` keep the endpoint field names the graph has always used, so a
/// row still reads as an edge rather than as a pair of anonymous strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KvdagEdgeRow {
    pub id: String,
    pub r#in: String,
    pub out: String,
    pub kind: String,
    #[serde(default)]
    pub condition: Option<Json>,
    pub payload: String,
    #[serde(default)]
    pub port: Option<String>,
}

// ── runs ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRow {
    pub id: String,
    pub workflow: String,
    pub kvdag_version: String,
    pub tier: String,
    pub status: String,
    #[serde(default)]
    pub args: Json,
    #[serde(default)]
    pub context_runs: Vec<String>,
    #[serde(default)]
    pub restore_from: Option<Json>,
    pub max_depth: i64,
    pub max_nodes: i64,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub tab_id: Option<String>,
    pub started_at: i64,
    #[serde(default)]
    pub ended_at: Option<i64>,
    #[serde(default)]
    pub total_tokens: i64,
    #[serde(default)]
    pub total_tool_uses: i64,
    #[serde(default)]
    pub nodes_total: i64,
    #[serde(default)]
    pub nodes_done: i64,
    #[serde(default)]
    pub failure: Option<Json>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunNodeRow {
    pub id: String,
    pub run: String,
    pub kvdag_node: String,
    pub node_key: String,
    pub instance_path: String,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub depth: i64,
    pub status: String,
    pub model: String,
    pub effort: String,
    pub demand: String,
    pub attempt: i64,
    #[serde(default)]
    pub pane_id: Option<String>,
    #[serde(default)]
    pub terminal_id: Option<String>,
    #[serde(default)]
    pub agent_session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub node_dir: Option<String>,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub ended_at: Option<i64>,
    #[serde(default)]
    pub total_tokens: i64,
    #[serde(default)]
    pub tool_uses: i64,
    #[serde(default)]
    pub duration_ms: i64,
    #[serde(default)]
    pub evidence: Option<String>,
    #[serde(default)]
    pub succession: Option<String>,
    #[serde(default)]
    pub blocker: Option<Json>,
    #[serde(default)]
    pub restored_from: Option<String>,
    #[serde(default)]
    pub watchdog_interventions: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEdgeRow {
    pub id: String,
    pub r#in: String,
    pub out: String,
    pub run: String,
    pub kind: String,
    #[serde(default)]
    pub kvdag_edge: Option<String>,
    #[serde(default)]
    pub condition_result: Option<bool>,
    #[serde(default)]
    pub fired_at: Option<i64>,
}

// ── journal, checkpoints, summaries ────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEventRow {
    pub id: String,
    pub run: String,
    pub seq: i64,
    pub at: i64,
    pub kind: String,
    #[serde(default)]
    pub run_node: Option<String>,
    #[serde(default)]
    pub payload: Json,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointRow {
    pub id: String,
    pub run: String,
    pub run_node: String,
    pub node_key: String,
    pub instance_path: String,
    pub kvdag_version: String,
    pub seq: i64,
    pub kind: String,
    #[serde(default)]
    pub schema_valid: bool,
    #[serde(default)]
    pub payload: Json,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub artifact_paths: Vec<String>,
    pub digest: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummaryRow {
    pub id: String,
    pub run: String,
    pub kvdag_version: String,
    pub text: String,
    pub outcome: String,
    #[serde(default)]
    pub highlights: Vec<String>,
    #[serde(default)]
    pub open_gaps: Vec<String>,
    #[serde(default)]
    pub per_node: Vec<Json>,
    #[serde(default)]
    pub token_estimate: i64,
    /// The `run_node` that produced the summary. Nulled when that run is
    /// pruned: the summary outlives its run, the node identity does not.
    #[serde(default)]
    pub generated_by: Option<String>,
    pub created_at: i64,
}

// ── interrogation / review ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterrogationRow {
    pub id: String,
    pub run_node: String,
    pub source_session_id: String,
    pub forked_session_id: String,
    #[serde(default)]
    pub transcript_path: Option<String>,
    pub cwd: String,
    #[serde(default)]
    pub pane_id: Option<String>,
    pub started_at: i64,
    #[serde(default)]
    pub ended_at: Option<i64>,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub reconstructed: bool,
    #[serde(default)]
    pub seeded_from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewCycleRow {
    pub id: String,
    pub run: String,
    pub kvdag_version: String,
    pub status: String,
    #[serde(default)]
    pub interviews: Vec<String>,
    pub started_at: i64,
    #[serde(default)]
    pub ended_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewFindingRow {
    pub id: String,
    pub cycle: String,
    pub node_key: String,
    /// The interrogation the finding was drawn from. Nulled when that
    /// interrogation is pruned with its run.
    #[serde(default)]
    pub interview: Option<String>,
    pub interview_mode: String,
    pub level: String,
    pub verdict: String,
    pub rationale: String,
    #[serde(default)]
    pub replacement: Option<Json>,
    #[serde(default)]
    pub evidence: Json,
    #[serde(default)]
    pub proposed_change: Json,
    #[serde(default)]
    pub accepted: bool,
}

// ── schema_meta ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_id_string_round_trips() {
        let full = record_id_to_string("workflow", "abc123");
        assert_eq!(full, "workflow:abc123");
        let parsed = parse_record_id("workflow", &full).expect("parses back");
        assert_eq!(parsed, "abc123");
    }

    #[test]
    fn parse_record_id_rejects_the_wrong_table() {
        assert!(parse_record_id("workflow", "kvdag_version:abc123").is_none());
    }

    #[test]
    fn parse_record_id_rejects_a_malformed_id() {
        assert!(parse_record_id("workflow", "no-colon-here").is_none());
        assert!(parse_record_id("workflow", "workflow:").is_none());
    }

    #[test]
    fn an_older_row_still_decodes_after_a_field_is_added() {
        // `position` is the field `kvdag_node` gained last; a row written
        // without it must not fail to decode.
        let older = serde_json::json!({
            "id": "v1\u{1f}solo",
            "version": "v1",
            "node_key": "solo",
            "label": "Solo",
            "kind": "agent",
            "runner": "agent",
            "demand": "standard",
            "prompt_template": "do it",
            "output_schema": {"type": "object"},
            "max_attempts": 2,
            "isolation": "none",
        });
        let row: KvdagNodeRow = serde_json::from_value(older).expect("decodes");
        assert_eq!(row.node_key, "solo");
        assert!(row.position.is_none());
        assert!(row.expand_allow.is_empty());
    }
}
