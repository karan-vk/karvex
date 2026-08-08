//! Typed rows exchanged with SurrealDB.
//!
//! All DB-facing structs here derive `surrealdb_types::SurrealValue`, never
//! serde: against surrealdb 3.2.4, `Create::content()`/`Select` are bound on
//! `SurrealValue` and the serde-based examples in SurrealDB's own docs do not
//! compile (`03-storage-schema.md` §1).
//!
//! Each row mirrors one table's columns exactly, in the order
//! `migrations/0001_init.surql` defines them, so a schema diff is a visual
//! diff of this file against that one.

use serde_json::Value as Json;
use surrealdb_types::{Datetime, RecordId, SurrealValue};

// ── identity plumbing ───────────────────────────────────────────────────────

/// Every id newtype in `workflow::model` (`WorkflowId`, `RunId`, ...) holds the
/// full `table:key` string, matching how the fixtures in `model.rs`'s own
/// tests construct them (e.g. `WorkflowId::new("workflow:1")`). These helpers
/// convert between that string form and a [`RecordId`] for binding into
/// queries; the key is always [`surrealdb_types::RecordIdKey::String`], since
/// every id here is either store-generated (`RecordIdKey::rand()`) or parsed
/// back from a string we generated ourselves.
pub fn record_id_to_string(id: &RecordId) -> String {
    format!("{}:{}", id.table.as_str(), record_id_key_to_string(id))
}

fn record_id_key_to_string(id: &RecordId) -> String {
    match &id.key {
        surrealdb_types::RecordIdKey::String(value) => value.clone(),
        surrealdb_types::RecordIdKey::Number(value) => value.to_string(),
        other => {
            // Every id this store mints uses `RecordIdKey::rand()` (a string
            // key); this arm only exists so the match is exhaustive over a
            // type this crate doesn't otherwise construct.
            format!("{other:?}")
        }
    }
}

/// Parses a `table:key` string minted by [`record_id_to_string`] back into a
/// [`RecordId`] for binding into a query. Returns `None` if `full_id` does not
/// belong to `table`.
pub fn parse_record_id(table: &str, full_id: &str) -> Option<RecordId> {
    let (prefix, key) = full_id.split_once(':')?;
    if prefix != table {
        return None;
    }
    Some(RecordId::new(table, key))
}

// ── definitions ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, SurrealValue)]
pub struct WorkflowRow {
    pub id: RecordId,
    pub name: String,
    pub description: String,
    pub head_version: Option<RecordId>,
    pub default_tier: String,
    pub archived: bool,
    pub created_at: Datetime,
    pub updated_at: Datetime,
}

#[derive(Debug, Clone, PartialEq, SurrealValue)]
pub struct KvdagVersionRow {
    pub id: RecordId,
    pub workflow: RecordId,
    pub version: i64,
    pub parent: Option<RecordId>,
    pub origin: String,
    pub change_summary: String,
    pub contract: String,
    pub args: Json,
    pub max_depth: i64,
    pub max_nodes: i64,
    pub spec_digest: String,
    pub created_at: Datetime,
}

#[derive(Debug, Clone, PartialEq, SurrealValue)]
pub struct KvdagNodeRow {
    pub id: RecordId,
    pub version: RecordId,
    pub node_key: String,
    pub label: String,
    pub role: String,
    pub kind: String,
    pub runner: String,
    pub command: Option<Vec<String>>,
    pub demand: String,
    pub prompt_template: String,
    pub system_contract: Option<String>,
    pub output_schema: Json,
    pub max_attempts: i64,
    pub timeout_ms: Option<i64>,
    pub isolation: String,
    pub is_template: bool,
    pub expand_allow: Vec<String>,
    pub expand_max: i64,
    pub position: Option<Json>,
}

/// `kvdag_edge` is `TYPE RELATION FROM kvdag_node TO kvdag_node`; `r#in`/`out`
/// are SurrealDB's own relation endpoint fields.
#[derive(Debug, Clone, PartialEq, SurrealValue)]
pub struct KvdagEdgeRow {
    pub id: RecordId,
    pub r#in: RecordId,
    pub out: RecordId,
    pub kind: String,
    pub condition: Option<Json>,
    pub payload: String,
    pub port: Option<String>,
}

// ── runs ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, SurrealValue)]
pub struct RunRow {
    pub id: RecordId,
    pub workflow: RecordId,
    pub kvdag_version: RecordId,
    pub tier: String,
    pub status: String,
    pub args: Json,
    pub context_runs: Vec<RecordId>,
    pub restore_from: Option<Json>,
    pub max_depth: i64,
    pub max_nodes: i64,
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub started_at: Datetime,
    pub ended_at: Option<Datetime>,
    pub total_tokens: i64,
    pub total_tool_uses: i64,
    pub nodes_total: i64,
    pub nodes_done: i64,
    pub failure: Option<Json>,
}

#[derive(Debug, Clone, PartialEq, SurrealValue)]
pub struct RunNodeRow {
    pub id: RecordId,
    pub run: RecordId,
    pub kvdag_node: RecordId,
    pub node_key: String,
    pub instance_path: String,
    pub parent: Option<RecordId>,
    pub depth: i64,
    pub status: String,
    pub model: String,
    pub effort: String,
    pub demand: String,
    pub attempt: i64,
    pub pane_id: Option<String>,
    pub terminal_id: Option<String>,
    pub agent_session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub cwd: Option<String>,
    pub node_dir: Option<String>,
    pub started_at: Option<Datetime>,
    pub ended_at: Option<Datetime>,
    pub total_tokens: i64,
    pub tool_uses: i64,
    pub duration_ms: i64,
    pub evidence: Option<String>,
    pub succession: Option<String>,
    pub blocker: Option<Json>,
    pub restored_from: Option<RecordId>,
    pub watchdog_interventions: i64,
    // ── added by migrations/0002_growth_and_history.surql ──
    pub assignment_reason: String,
    pub first_pass_succeeded: bool,
    pub schema_failures: i64,
    // ── added by migrations/0003_node_identity.surql ──
    /// What this instance is called — the authored kvdag label for a static
    /// node, the proposing node's `--label` for an expansion child.
    pub label: String,
    /// The accepted `--input k=v` slot overrides this instance was created
    /// with, as a flat `string -> string` object. Empty for a static node.
    pub inputs: Json,
}

/// `run_edge` is `TYPE RELATION FROM run_node TO run_node`.
#[derive(Debug, Clone, PartialEq, SurrealValue)]
pub struct RunEdgeRow {
    pub id: RecordId,
    pub r#in: RecordId,
    pub out: RecordId,
    pub run: RecordId,
    pub kind: String,
    pub kvdag_edge: Option<RecordId>,
    pub condition_result: Option<bool>,
    pub fired_at: Option<Datetime>,
}

// ── journal, checkpoints, summaries ────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, SurrealValue)]
pub struct RunEventRow {
    pub id: RecordId,
    pub run: RecordId,
    pub seq: i64,
    pub at: Datetime,
    pub kind: String,
    pub run_node: Option<RecordId>,
    pub payload: Json,
}

#[derive(Debug, Clone, PartialEq, SurrealValue)]
pub struct CheckpointRow {
    pub id: RecordId,
    pub run: RecordId,
    pub run_node: RecordId,
    pub node_key: String,
    pub instance_path: String,
    pub kvdag_version: RecordId,
    pub seq: i64,
    pub kind: String,
    pub schema_valid: bool,
    pub payload: Json,
    pub summary: String,
    pub artifact_paths: Vec<String>,
    pub digest: String,
    pub created_at: Datetime,
}

#[derive(Debug, Clone, PartialEq, SurrealValue)]
pub struct RunSummaryRow {
    pub id: RecordId,
    pub run: RecordId,
    pub kvdag_version: RecordId,
    pub text: String,
    pub outcome: String,
    pub highlights: Vec<String>,
    pub open_gaps: Vec<String>,
    pub per_node: Vec<Json>,
    pub token_estimate: i64,
    pub generated_by: Option<RecordId>,
    pub created_at: Datetime,
}

// ── interrogation / review (schema present; Phase 1 has no writer) ─────────

#[derive(Debug, Clone, PartialEq, SurrealValue)]
pub struct InterrogationRow {
    pub id: RecordId,
    pub run_node: RecordId,
    pub source_session_id: String,
    pub forked_session_id: String,
    pub transcript_path: Option<String>,
    pub cwd: String,
    pub pane_id: Option<String>,
    pub started_at: Datetime,
    pub ended_at: Option<Datetime>,
    pub note: String,
    pub reconstructed: bool,
    pub seeded_from: Option<RecordId>,
}

// ── schema_meta ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, SurrealValue)]
pub struct SchemaMetaRow {
    pub id: RecordId,
    pub version: String,
    pub applied_at: Datetime,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_id_string_round_trips() {
        let id = RecordId::new("workflow", "abc123");
        let full = record_id_to_string(&id);
        assert_eq!(full, "workflow:abc123");
        let parsed = parse_record_id("workflow", &full).expect("parses back");
        assert_eq!(parsed, id);
    }

    #[test]
    fn parse_record_id_rejects_the_wrong_table() {
        assert!(parse_record_id("workflow", "kvdag_version:abc123").is_none());
    }

    #[test]
    fn parse_record_id_rejects_a_malformed_id() {
        assert!(parse_record_id("workflow", "no-colon-here").is_none());
    }
}
