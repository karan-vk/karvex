//! JSON API wire types for the workflow subsystem: kvdag definitions,
//! versions, runs, and run nodes.
//!
//! Self-contained by design
//! (`docs/design/workflow-builder/05-phase-plan.md` W3): this module
//! declares its own vocabulary for node status, run status, demand, tier,
//! succession, and evidence rather than importing `crate::workflow::model`.
//! The wire contract in `docs/next/api/herdr-api.schema.json` is a stable,
//! versioned artifact; the engine's internal types are free to change shape
//! without touching it. Every conversion between the two lives in
//! `src/app/api/workflows.rs` behind `#[cfg(feature = "workflow")]`, so this
//! file has zero `use crate::workflow::*` and compiles unconditionally.
//!
//! A kvdag definition document (`workflow.create` / `workflow.version.create`
//! input) is carried as opaque TOML/JSON text
//! (`WorkflowDefinitionDocument`) rather than a second, fully typed copy of
//! the node/edge/condition tree: `docs/design/workflow-builder/05-phase-plan.md`
//! §4 already promises the document is "the same shape as the kvdag types",
//! and those types derive `Deserialize` directly. Parsing the text happens
//! server-side, behind the same feature gate.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ── shared vocabulary (deliberately duplicated — see the module doc) ───────

/// The run's cost/quality tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTier {
    Auto,
    Max,
    High,
    Medium,
    Low,
}

/// How demanding a node's work is; drives the tier's model/effort mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowDemand {
    Peak,
    Critical,
    Standard,
    Light,
}

/// What a node *is*. Orthogonal to [`WorkflowRunner`], which selects how it
/// is bound to a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowNodeKind {
    Agent,
    Internal,
    Gate,
    Monitor,
}

/// Selects the binding the spawner uses for a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunner {
    Agent,
    Command,
}

/// Working-directory isolation for a node's pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowIsolation {
    None,
    Worktree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowEdgeKind {
    Sequence,
    Data,
    Conditional,
}

/// How much of the source node's checkpoint is handed to the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowEdgePayload {
    None,
    Summary,
    Full,
}

/// How a kvdag version came to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowVersionOrigin {
    Authored,
    Imported,
    SelfImprovement,
    RestoreRewrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    Pending,
    Running,
    Paused,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowNodeStatus {
    Pending,
    Ready,
    Running,
    NeedsAttention,
    Blocked,
    Succeeded,
    Failed,
    Skipped,
    Restored,
    Cancelled,
}

/// Which completion signal was accepted for a node
/// (`docs/design/workflow-builder/04-kvdag-and-execution.md` §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowEvidence {
    SelfReport,
    Hook,
    Detection,
    Restored,
}

/// Every closing node records exactly one of these
/// (`docs/design/workflow-builder/04-kvdag-and-execution.md` §3.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowSuccession {
    Satisfied,
    Blocked { reason: String, resume_when: String },
    NoFollowup { evidence: String },
}

/// Wire format of a [`WorkflowDefinitionDocument`]'s text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowDefinitionFormat {
    Toml,
    Json,
}

/// A kvdag definition document: TOML or JSON, in the shape
/// `docs/design/workflow-builder/05-phase-plan.md` §4 documents. See the
/// module doc for why this is opaque text rather than a typed node/edge tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowDefinitionDocument {
    pub format: WorkflowDefinitionFormat,
    pub text: String,
}

// ── targets ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowTarget {
    pub workflow_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowVersionTarget {
    pub version_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowRunTarget {
    pub run_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowNodeTarget {
    pub run_id: String,
    pub path: String,
}

// ── workflow.create / workflow.version.create ──────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowCreateParams {
    pub definition: WorkflowDefinitionDocument,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowVersionCreateParams {
    pub workflow_id: String,
    pub definition: WorkflowDefinitionDocument,
    #[serde(default)]
    pub change_summary: String,
}

// ── workflow.run / workflow.run.list ────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowRunParams {
    pub workflow_id: String,
    /// Defaults to the workflow's head version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// Defaults to the workflow's `default_tier`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<WorkflowTier>,
    /// Materialised into `workflow_run.args`; fills the declared `ArgSpec`
    /// namespace (`kvx workflow run start … --arg k=v`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub args: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowRunListParams {
    pub workflow_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

// ── workflow.node.steer / workflow.node.report ──────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowNodeSteerParams {
    pub run_id: String,
    pub path: String,
    pub text: String,
}

/// The node self-report contract
/// (`docs/design/workflow-builder/04-kvdag-and-execution.md` §4.3): token-
/// authenticated with `KARVEX_WORKFLOW_NODE_TOKEN`, precedence 1 of the three
/// completion signals.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowNodeReportParams {
    pub run_id: String,
    pub path: String,
    pub token: String,
    pub result: serde_json::Value,
}

// ── workflow.node.expand ───────────────────────────────────────────────────

/// A node proposing new nodes
/// (`docs/design/workflow-builder/04-kvdag-and-execution.md` §3.4).
///
/// Token-authenticated exactly like [`WorkflowNodeReportParams`]: an expand
/// proposal is a node speaking, not an operator. A *rejected* proposal is a
/// successful response carrying the rejection — the run continues; only a bad
/// run/path/token or a closed run is an error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowNodeExpandParams {
    pub run_id: String,
    /// The proposing node's instance path.
    pub path: String,
    pub token: String,
    /// The kvdag key of the template to instantiate.
    pub template: String,
    #[serde(default)]
    pub label: String,
    /// Slot overrides for the template's `prompt_template`. A key naming no
    /// declared slot is rejected rather than ignored: this is an override
    /// channel over the one validated renderer, never a second unvalidated one.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub inputs: HashMap<String, String>,
    /// How many children to instantiate; defaults to 1. `u32` on the wire and
    /// `u16` internally, so the handler narrows with `u16::try_from` and
    /// rejects an out-of-range value rather than truncating it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
}

/// Which growth guardrail was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowGrowthLimitKind {
    /// The proposing node's own `expand_max`, counted cumulatively across every
    /// proposal it has made in this run.
    ExpandMax,
    /// The run's `max_depth`, guarded as `parent.depth + 1 <= max_depth`.
    MaxDepth,
    /// The run's `max_nodes`, counted over every materialised node regardless
    /// of status — a failed child does not refund budget.
    MaxNodes,
}

/// One growth guardrail being reached, in the one shape reused by
/// [`WorkflowRunInfo`], [`WorkflowRunNodeInfo`], the `workflow.growth.limited`
/// event, and the CLI renderers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowGrowthLimit {
    pub kind: WorkflowGrowthLimitKind,
    /// The ceiling's value, so a reader does not have to look it up.
    pub limit_value: u32,
    pub requested: u32,
    /// How many of `requested` were created anyway. Non-zero means partial
    /// acceptance, which is the interesting case: the shortfall is reported
    /// rather than silently truncated.
    pub accepted: u32,
    pub at_unix_ms: u64,
    pub message: String,
}

/// Why one proposal was refused, in whole or in part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowExpandRejectionReason {
    /// The template is not in the proposing node's `expand_allow`.
    NotAllowed,
    /// No kvdag node carries that key.
    UnknownTemplate,
    /// The key names a node that is not declared `is_template`.
    NotATemplate,
    ExpandMaxReached,
    MaxDepthReached,
    MaxNodesReached,
    /// Partial acceptance: some children were created and the rest did not fit.
    Truncated,
    /// An `inputs` key naming no declared slot in the template.
    UnknownInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowExpandRejection {
    pub template: String,
    pub reason: WorkflowExpandRejectionReason,
    /// The guardrail hit, when the rejection is a limit rather than a
    /// validation failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<WorkflowGrowthLimit>,
    #[serde(default)]
    pub requested: u32,
    #[serde(default)]
    pub accepted: u32,
    pub message: String,
}

// ── read-model types shared by results and events ──────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowSummary {
    pub workflow_id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub default_tier: WorkflowTier,
    #[serde(default)]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_version_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_version: Option<u32>,
    pub created_at_unix_ms: u64,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct KvdagVersionSummary {
    pub version_id: String,
    pub workflow_id: String,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_version_id: Option<String>,
    pub origin: WorkflowVersionOrigin,
    #[serde(default)]
    pub change_summary: String,
    pub spec_digest: String,
    pub max_depth: u32,
    pub max_nodes: u32,
    pub created_at_unix_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowArgSpec {
    pub name: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct KvdagNodeInfo {
    pub node_key: String,
    pub label: String,
    #[serde(default)]
    pub role: String,
    pub kind: WorkflowNodeKind,
    pub runner: WorkflowRunner,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    pub demand: WorkflowDemand,
    pub prompt_template: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_contract: Option<String>,
    pub output_schema: serde_json::Value,
    pub max_attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    pub isolation: WorkflowIsolation,
    #[serde(default)]
    pub is_template: bool,
    #[serde(default)]
    pub expand_allow: Vec<String>,
    #[serde(default)]
    pub expand_max: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct KvdagEdgeInfo {
    pub from: String,
    pub to: String,
    pub kind: WorkflowEdgeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<serde_json::Value>,
    pub payload: WorkflowEdgePayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct KvdagVersionDetail {
    pub version_id: String,
    pub workflow_id: String,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_version_id: Option<String>,
    pub origin: WorkflowVersionOrigin,
    #[serde(default)]
    pub change_summary: String,
    #[serde(default)]
    pub contract: String,
    #[serde(default)]
    pub args: Vec<WorkflowArgSpec>,
    pub max_depth: u32,
    pub max_nodes: u32,
    pub spec_digest: String,
    pub created_at_unix_ms: u64,
    pub nodes: Vec<KvdagNodeInfo>,
    pub edges: Vec<KvdagEdgeInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowRunInfo {
    pub run_id: String,
    pub workflow_id: String,
    pub version_id: String,
    pub tier: WorkflowTier,
    pub status: WorkflowRunStatus,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub args: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    pub started_at_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub total_tool_uses: u64,
    #[serde(default)]
    pub nodes_total: u32,
    #[serde(default)]
    pub nodes_done: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<serde_json::Value>,
    /// The run's **effective** growth ceilings — the version's, narrowed by the
    /// tier. One authority: what the run graph enforces is what the run row
    /// persists and what this reports.
    #[serde(default)]
    pub max_depth: u32,
    #[serde(default)]
    pub max_nodes: u32,
    /// Nodes materialised so far, counted against `max_nodes` regardless of
    /// status.
    #[serde(default)]
    pub nodes_live: u32,
    /// The run's most recent growth limit, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub growth_limited: Option<WorkflowGrowthLimit>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowRunNodeInfo {
    pub path: String,
    pub node_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_path: Option<String>,
    #[serde(default)]
    pub depth: u32,
    pub status: WorkflowNodeStatus,
    pub demand: WorkflowDemand,
    pub model: String,
    pub effort: String,
    #[serde(default = "default_attempt")]
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub tool_uses: u32,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<WorkflowEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub succession: Option<WorkflowSuccession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<serde_json::Value>,
    #[serde(default)]
    pub watchdog_interventions: u32,
    /// Why `model`/`effort` read what they read — the `auto` tier's reason
    /// string, empty for a fixed tier whose table row is the explanation.
    #[serde(default)]
    pub assignment_reason: String,
    /// The last pane delivery the runtime refused for this node, e.g. a steer
    /// whose keystrokes never reached the process. A shared runtime fact, so it
    /// belongs on the API and not only in TUI state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_failure: Option<String>,
    /// The last growth limit this node ran into as a *proposer*.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub growth_limited: Option<WorkflowGrowthLimit>,
}

fn default_attempt() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowRunEdgeInfo {
    pub from: String,
    pub to: String,
    pub kind: WorkflowEdgeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition_result: Option<bool>,
    #[serde(default)]
    pub fired: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowRunGraph {
    pub nodes: Vec<WorkflowRunNodeInfo>,
    pub edges: Vec<WorkflowRunEdgeInfo>,
}

/// One projection of a workflow and its head document, read by **both**
/// `workflow.get`'s human renderer and its `--json` path so the two cannot
/// describe different field sets.
///
/// It arrives as a new sibling field on `ResponseResult::WorkflowGet` rather
/// than replacing that response's existing `workflow: WorkflowSummary`:
/// re-typing a published response field is the one change in this phase that
/// would be wire-incompatible, and every other addition is additive
/// self-describing JSON, which is what keeps `PROTOCOL_VERSION` where it is.
///
/// `description` comes from the `workflow` row, which `create_version` keeps
/// equal to the head document — `kvdag_version` has no `description` column, so
/// "read it from the head version" is not implementable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WorkflowDetail {
    pub workflow: WorkflowSummary,
    #[serde(default)]
    pub nodes: Vec<KvdagNodeInfo>,
    #[serde(default)]
    pub edges: Vec<KvdagEdgeInfo>,
    #[serde(default)]
    pub args: Vec<WorkflowArgSpec>,
    #[serde(default)]
    pub versions: Vec<KvdagVersionSummary>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{Method, Request, ResponseResult, SuccessResponse};

    fn growth_limit() -> WorkflowGrowthLimit {
        WorkflowGrowthLimit {
            kind: WorkflowGrowthLimitKind::MaxNodes,
            limit_value: 12,
            requested: 4,
            accepted: 2,
            at_unix_ms: 1_700_000_000_000,
            message: "max_nodes 12 reached; 2 of 4 requested nodes created".into(),
        }
    }

    fn definition_document() -> WorkflowDefinitionDocument {
        WorkflowDefinitionDocument {
            format: WorkflowDefinitionFormat::Toml,
            text: "name = \"ship-feature\"\n".to_string(),
        }
    }

    /// Every `workflow.*` `Method` variant round-trips through JSON and uses
    /// the documented `domain.verb` dot name
    /// (`docs/design/workflow-builder/05-phase-plan.md` W3).
    #[test]
    fn workflow_method_requests_round_trip_with_dot_names() {
        let cases: Vec<(Method, &str)> = vec![
            (
                Method::WorkflowList(crate::api::schema::EmptyParams::default()),
                "workflow.list",
            ),
            (
                Method::WorkflowGet(WorkflowTarget {
                    workflow_id: "workflow:1".into(),
                }),
                "workflow.get",
            ),
            (
                Method::WorkflowCreate(WorkflowCreateParams {
                    definition: definition_document(),
                }),
                "workflow.create",
            ),
            (
                Method::WorkflowVersionCreate(WorkflowVersionCreateParams {
                    workflow_id: "workflow:1".into(),
                    definition: definition_document(),
                    change_summary: "widen the review step".into(),
                }),
                "workflow.version.create",
            ),
            (
                Method::WorkflowVersionGet(WorkflowVersionTarget {
                    version_id: "kvdag_version:1".into(),
                }),
                "workflow.version.get",
            ),
            (
                Method::WorkflowRun(WorkflowRunParams {
                    workflow_id: "workflow:1".into(),
                    version: Some(1),
                    tier: Some(WorkflowTier::Auto),
                    args: HashMap::from([("goal".to_string(), "add dark mode".to_string())]),
                }),
                "workflow.run",
            ),
            (
                Method::WorkflowRunGet(WorkflowRunTarget {
                    run_id: "workflow_run:1".into(),
                }),
                "workflow.run.get",
            ),
            (
                Method::WorkflowRunList(WorkflowRunListParams {
                    workflow_id: "workflow:1".into(),
                    limit: Some(10),
                }),
                "workflow.run.list",
            ),
            (
                Method::WorkflowRunCancel(WorkflowRunTarget {
                    run_id: "workflow_run:1".into(),
                }),
                "workflow.run.cancel",
            ),
            (
                Method::WorkflowNodeGet(WorkflowNodeTarget {
                    run_id: "workflow_run:1".into(),
                    path: "plan".into(),
                }),
                "workflow.node.get",
            ),
            (
                Method::WorkflowNodeSteer(WorkflowNodeSteerParams {
                    run_id: "workflow_run:1".into(),
                    path: "plan".into(),
                    text: "focus on the auth flow first".into(),
                }),
                "workflow.node.steer",
            ),
            (
                Method::WorkflowNodeInterrupt(WorkflowNodeTarget {
                    run_id: "workflow_run:1".into(),
                    path: "plan".into(),
                }),
                "workflow.node.interrupt",
            ),
            (
                Method::WorkflowNodeReport(WorkflowNodeReportParams {
                    run_id: "workflow_run:1".into(),
                    path: "plan".into(),
                    token: "node-token".into(),
                    result: serde_json::json!({"plan": "..."}),
                }),
                "workflow.node.report",
            ),
            (
                Method::WorkflowNodeRestart(WorkflowNodeTarget {
                    run_id: "workflow_run:1".into(),
                    path: "plan".into(),
                }),
                "workflow.node.restart",
            ),
            (
                Method::WorkflowNodeExpand(WorkflowNodeExpandParams {
                    run_id: "workflow_run:1".into(),
                    path: "fanout".into(),
                    token: "node-token".into(),
                    template: "worker".into(),
                    label: "worker".into(),
                    inputs: HashMap::from([("slice".to_string(), "auth".to_string())]),
                    count: Some(4),
                }),
                "workflow.node.expand",
            ),
        ];

        for (method, dot_name) in cases {
            let request = Request {
                id: "req_1".into(),
                method,
            };
            let json = serde_json::to_value(&request).unwrap();
            assert_eq!(json["method"], dot_name, "unexpected dot name");
            let restored: Request = serde_json::from_value(json).unwrap();
            assert_eq!(restored, request);
        }
    }

    fn sample_node_info() -> KvdagNodeInfo {
        KvdagNodeInfo {
            node_key: "plan".into(),
            label: "Plan".into(),
            role: "planner".into(),
            kind: WorkflowNodeKind::Agent,
            runner: WorkflowRunner::Agent,
            command: None,
            demand: WorkflowDemand::Critical,
            prompt_template: "Produce an implementation plan for: {{goal}}".into(),
            system_contract: Some("Reply only through result.json".into()),
            output_schema: serde_json::json!({"type": "object"}),
            max_attempts: 2,
            timeout_ms: Some(600_000),
            isolation: WorkflowIsolation::None,
            is_template: false,
            expand_allow: Vec::new(),
            expand_max: 0,
        }
    }

    fn sample_edge_info() -> KvdagEdgeInfo {
        KvdagEdgeInfo {
            from: "plan".into(),
            to: "implement".into(),
            kind: WorkflowEdgeKind::Data,
            condition: None,
            payload: WorkflowEdgePayload::Summary,
            port: Some("plan".into()),
        }
    }

    /// `workflow.version.get`'s result exercises every node/edge/isolation/
    /// runner/kind vocabulary value at least once.
    #[test]
    fn kvdag_version_detail_round_trips() {
        let mut command_node = sample_node_info();
        command_node.node_key = "verify".into();
        command_node.runner = WorkflowRunner::Command;
        command_node.command = Some(vec!["bash".into(), "run.sh".into()]);
        command_node.kind = WorkflowNodeKind::Internal;
        command_node.isolation = WorkflowIsolation::Worktree;
        command_node.is_template = true;

        let detail = KvdagVersionDetail {
            version_id: "kvdag_version:1".into(),
            workflow_id: "workflow:1".into(),
            version: 1,
            parent_version_id: None,
            origin: WorkflowVersionOrigin::Authored,
            change_summary: String::new(),
            contract: "Reply only through result.json".into(),
            args: vec![WorkflowArgSpec {
                name: "goal".into(),
                required: true,
                default: None,
                description: "what to build".into(),
            }],
            max_depth: 3,
            max_nodes: 24,
            spec_digest: "a".repeat(64),
            created_at_unix_ms: 1,
            nodes: vec![sample_node_info(), command_node],
            edges: vec![sample_edge_info()],
        };

        let response = SuccessResponse {
            id: "req_version_get".into(),
            result: ResponseResult::WorkflowVersionGet {
                version: detail.clone(),
            },
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"type\":\"workflow_version_get\""));
        let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, response);
    }

    fn sample_run_node_info(
        path: &str,
        succession: Option<WorkflowSuccession>,
    ) -> WorkflowRunNodeInfo {
        WorkflowRunNodeInfo {
            path: path.into(),
            node_key: "plan".into(),
            parent_path: None,
            depth: 0,
            status: WorkflowNodeStatus::Succeeded,
            demand: WorkflowDemand::Standard,
            model: "sonnet".into(),
            effort: "low".into(),
            attempt: 1,
            pane_id: Some("w_1-2".into()),
            terminal_id: Some("term_1".into()),
            agent_session_id: Some("11111111-1111-1111-1111-111111111111".into()),
            cwd: Some("/repo".into()),
            node_dir: Some("/repo/.karvex/runs/1/plan".into()),
            started_at_unix_ms: Some(1),
            ended_at_unix_ms: Some(2),
            total_tokens: 100,
            tool_uses: 3,
            duration_ms: 1_000,
            evidence: Some(WorkflowEvidence::SelfReport),
            succession,
            blocker: None,
            watchdog_interventions: 0,
            assignment_reason: "auto: 4 prior runs at 100% first pass".into(),
            delivery_failure: None,
            growth_limited: None,
        }
    }

    /// Exercises all three [`WorkflowSuccession`] shapes and the run-graph
    /// projection returned by `workflow.run.get`.
    #[test]
    fn workflow_run_graph_round_trips_every_succession_shape() {
        let nodes = vec![
            sample_run_node_info("plan", Some(WorkflowSuccession::Satisfied)),
            sample_run_node_info(
                "review",
                Some(WorkflowSuccession::Blocked {
                    reason: "needs a human decision".into(),
                    resume_when: "gate answered".into(),
                }),
            ),
            sample_run_node_info(
                "notify",
                Some(WorkflowSuccession::NoFollowup {
                    evidence: "conditional branch produced nothing".into(),
                }),
            ),
        ];
        let graph = WorkflowRunGraph {
            nodes,
            edges: vec![WorkflowRunEdgeInfo {
                from: "plan".into(),
                to: "review".into(),
                kind: WorkflowEdgeKind::Sequence,
                condition_result: None,
                fired: true,
            }],
        };

        let response = SuccessResponse {
            id: "req_run_get".into(),
            result: ResponseResult::WorkflowRunGet {
                run: WorkflowRunInfo {
                    run_id: "workflow_run:1".into(),
                    workflow_id: "workflow:1".into(),
                    version_id: "kvdag_version:1".into(),
                    tier: WorkflowTier::Max,
                    status: WorkflowRunStatus::Running,
                    args: HashMap::from([("goal".to_string(), "add dark mode".to_string())]),
                    workspace_id: Some("w_1".into()),
                    tab_id: Some("w_1:1".into()),
                    started_at_unix_ms: 1,
                    ended_at_unix_ms: None,
                    total_tokens: 500,
                    total_tool_uses: 12,
                    nodes_total: 3,
                    nodes_done: 1,
                    failure: None,
                    max_depth: 3,
                    max_nodes: 24,
                    nodes_live: 3,
                    growth_limited: None,
                },
                graph,
            },
        };
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"type\":\"workflow_run_get\""));
        let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, response);
    }

    #[test]
    fn workflow_list_and_get_results_round_trip() {
        let workflow = WorkflowSummary {
            workflow_id: "workflow:1".into(),
            name: "ship-feature".into(),
            description: "plan -> implement -> review".into(),
            default_tier: WorkflowTier::Auto,
            archived: false,
            head_version_id: Some("kvdag_version:2".into()),
            head_version: Some(2),
            created_at_unix_ms: 1,
            updated_at_unix_ms: 2,
        };
        let version_summary = KvdagVersionSummary {
            version_id: "kvdag_version:2".into(),
            workflow_id: "workflow:1".into(),
            version: 2,
            parent_version_id: Some("kvdag_version:1".into()),
            origin: WorkflowVersionOrigin::SelfImprovement,
            change_summary: "tightened the review prompt".into(),
            spec_digest: "b".repeat(64),
            max_depth: 3,
            max_nodes: 24,
            created_at_unix_ms: 2,
        };

        for response in [
            SuccessResponse {
                id: "req_list".into(),
                result: ResponseResult::WorkflowList {
                    workflows: vec![workflow.clone()],
                },
            },
            SuccessResponse {
                id: "req_get".into(),
                result: ResponseResult::WorkflowGet {
                    workflow: workflow.clone(),
                    versions: vec![version_summary.clone()],
                    detail: Some(WorkflowDetail {
                        workflow: workflow.clone(),
                        nodes: vec![sample_node_info()],
                        edges: vec![sample_edge_info()],
                        args: vec![WorkflowArgSpec {
                            name: "goal".into(),
                            required: true,
                            default: None,
                            description: "what to build".into(),
                        }],
                        versions: vec![version_summary.clone()],
                    }),
                },
            },
            SuccessResponse {
                id: "req_create".into(),
                result: ResponseResult::WorkflowCreated {
                    workflow: workflow.clone(),
                    version: version_summary.clone(),
                },
            },
            SuccessResponse {
                id: "req_version_create".into(),
                result: ResponseResult::WorkflowVersionCreated {
                    workflow: workflow.clone(),
                    version: version_summary.clone(),
                },
            },
        ] {
            let json = serde_json::to_string(&response).unwrap();
            let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, response);
        }
    }

    #[test]
    fn workflow_run_and_node_action_results_round_trip() {
        let run = WorkflowRunInfo {
            run_id: "workflow_run:1".into(),
            workflow_id: "workflow:1".into(),
            version_id: "kvdag_version:1".into(),
            tier: WorkflowTier::Low,
            status: WorkflowRunStatus::Cancelled,
            args: HashMap::new(),
            workspace_id: None,
            tab_id: None,
            started_at_unix_ms: 1,
            ended_at_unix_ms: Some(5),
            total_tokens: 0,
            total_tool_uses: 0,
            nodes_total: 1,
            nodes_done: 0,
            failure: Some(serde_json::json!({"reason": "cancelled by user"})),
            max_depth: 3,
            max_nodes: 12,
            nodes_live: 1,
            growth_limited: Some(growth_limit()),
        };
        let node = sample_run_node_info("plan", None);

        for response in [
            SuccessResponse {
                id: "req_run_started".into(),
                result: ResponseResult::WorkflowRunStarted { run: run.clone() },
            },
            SuccessResponse {
                id: "req_run_list".into(),
                result: ResponseResult::WorkflowRunList {
                    runs: vec![run.clone()],
                },
            },
            SuccessResponse {
                id: "req_run_cancelled".into(),
                result: ResponseResult::WorkflowRunCancelled { run: run.clone() },
            },
            SuccessResponse {
                id: "req_node_get".into(),
                result: ResponseResult::WorkflowNodeGet { node: node.clone() },
            },
            SuccessResponse {
                id: "req_node_steered".into(),
                result: ResponseResult::WorkflowNodeSteered { node: node.clone() },
            },
            SuccessResponse {
                id: "req_node_interrupted".into(),
                result: ResponseResult::WorkflowNodeInterrupted { node: node.clone() },
            },
            SuccessResponse {
                id: "req_node_reported".into(),
                result: ResponseResult::WorkflowNodeReported { node: node.clone() },
            },
            SuccessResponse {
                id: "req_node_restarted".into(),
                result: ResponseResult::WorkflowNodeRestarted { node: node.clone() },
            },
        ] {
            let json = serde_json::to_string(&response).unwrap();
            let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(restored, response);
        }
    }

    /// Every Phase 2 addition round-trips, including the partial-acceptance
    /// case: `accepted` and `rejected` both non-empty, which is the shape a
    /// truncated proposal produces and the one a client must not treat as an
    /// error.
    #[test]
    fn workflow_node_expanded_result_round_trips_with_partial_acceptance() {
        let response = SuccessResponse {
            id: "req_node_expand".into(),
            result: ResponseResult::WorkflowNodeExpanded {
                accepted: vec!["fanout/worker/1".into(), "fanout/worker/2".into()],
                rejected: vec![
                    WorkflowExpandRejection {
                        template: "worker".into(),
                        reason: WorkflowExpandRejectionReason::Truncated,
                        limit: Some(growth_limit()),
                        requested: 4,
                        accepted: 2,
                        message: "max_nodes 12 reached; 2 of 4 requested nodes created".into(),
                    },
                    WorkflowExpandRejection {
                        template: "auditor".into(),
                        reason: WorkflowExpandRejectionReason::NotAllowed,
                        limit: None,
                        requested: 1,
                        accepted: 0,
                        message: "auditor is not in fanout's expand_allow".into(),
                    },
                ],
            },
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"type\":\"workflow_node_expanded\""));
        let restored: SuccessResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, response);
    }

    /// Every rejection reason and every limit kind has a wire spelling, and
    /// each one survives a round trip.
    #[test]
    fn every_expand_rejection_reason_and_growth_limit_kind_round_trips() {
        let reasons = [
            (WorkflowExpandRejectionReason::NotAllowed, "not_allowed"),
            (
                WorkflowExpandRejectionReason::UnknownTemplate,
                "unknown_template",
            ),
            (
                WorkflowExpandRejectionReason::NotATemplate,
                "not_a_template",
            ),
            (
                WorkflowExpandRejectionReason::ExpandMaxReached,
                "expand_max_reached",
            ),
            (
                WorkflowExpandRejectionReason::MaxDepthReached,
                "max_depth_reached",
            ),
            (
                WorkflowExpandRejectionReason::MaxNodesReached,
                "max_nodes_reached",
            ),
            (WorkflowExpandRejectionReason::Truncated, "truncated"),
            (WorkflowExpandRejectionReason::UnknownInput, "unknown_input"),
        ];
        for (reason, wire) in reasons {
            let json = serde_json::to_value(reason).unwrap();
            assert_eq!(json, serde_json::Value::String(wire.to_string()));
            let restored: WorkflowExpandRejectionReason = serde_json::from_value(json).unwrap();
            assert_eq!(restored, reason);
        }

        let kinds = [
            (WorkflowGrowthLimitKind::ExpandMax, "expand_max"),
            (WorkflowGrowthLimitKind::MaxDepth, "max_depth"),
            (WorkflowGrowthLimitKind::MaxNodes, "max_nodes"),
        ];
        for (kind, wire) in kinds {
            let json = serde_json::to_value(kind).unwrap();
            assert_eq!(json, serde_json::Value::String(wire.to_string()));
            let restored: WorkflowGrowthLimitKind = serde_json::from_value(json).unwrap();
            assert_eq!(restored, kind);
        }
    }

    /// `WorkflowGet.detail` is additive: a payload written before Phase 2 —
    /// one with no `detail` key at all — still deserialises, which is the
    /// property that lets `PROTOCOL_VERSION` stay where it is.
    #[test]
    fn workflow_get_detail_is_additive() {
        let json = serde_json::json!({
            "id": "req_get",
            "result": {
                "type": "workflow_get",
                "workflow": {
                    "workflow_id": "workflow:1",
                    "name": "ship-feature",
                    "description": "",
                    "default_tier": "high",
                    "archived": false,
                    "created_at_unix_ms": 1,
                    "updated_at_unix_ms": 2
                },
                "versions": []
            }
        });
        let restored: SuccessResponse = serde_json::from_value(json).unwrap();
        match restored.result {
            ResponseResult::WorkflowGet { detail, .. } => {
                assert!(detail.is_none(), "a pre-Phase-2 payload carries no detail");
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    /// Every identifier this task adds to the JSON API (dot-method names,
    /// Rust type/variant/field names) — checked against
    /// `docs/design/workflow-builder/05-phase-plan.md` W3's ban on
    /// UI-surface vocabulary. Matched by whole word (split on `.`/`_`/case
    /// boundaries), not raw substring, so legitimate words like `growth` or
    /// `narrow` never false-positive on `row`.
    #[test]
    fn no_new_workflow_api_identifier_uses_banned_ui_surface_words() {
        // `badge`, `banner`, `toast`, and `modal` are not UI-surface words the
        // Phase 1 list happened to name, but they are UI-surface words by the
        // same rule — hence `growth_limited`, not `growth_banner`
        // (`06-phase2-plan.md` WS-D "Naming guard").
        const BANNED: &[&str] = &[
            "sidebar", "card", "widget", "row", "panel", "badge", "banner", "toast", "modal",
        ];

        fn words(identifier: &str) -> Vec<String> {
            let mut words = Vec::new();
            let mut current = String::new();
            for ch in identifier.chars() {
                if ch == '_' || ch == '.' || ch == '-' {
                    if !current.is_empty() {
                        words.push(std::mem::take(&mut current));
                    }
                    continue;
                }
                if ch.is_uppercase() && !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
                current.push(ch.to_ascii_lowercase());
            }
            if !current.is_empty() {
                words.push(current);
            }
            words
        }

        let identifiers: &[&str] = &[
            // dot method names
            "workflow.list",
            "workflow.get",
            "workflow.create",
            "workflow.version.create",
            "workflow.version.get",
            "workflow.run",
            "workflow.run.get",
            "workflow.run.list",
            "workflow.run.cancel",
            "workflow.node.get",
            "workflow.node.steer",
            "workflow.node.interrupt",
            "workflow.node.report",
            "workflow.node.restart",
            "workflow.node.expand",
            // event dot names
            "workflow.run.started",
            "workflow.run.updated",
            "workflow.run.finished",
            "workflow.node.created",
            "workflow.node.updated",
            "workflow.node.output_checkpoint",
            "workflow.growth.limited",
            // vocab enums + variants
            "WorkflowTier",
            "auto",
            "max",
            "high",
            "medium",
            "low",
            "WorkflowDemand",
            "peak",
            "critical",
            "standard",
            "light",
            "WorkflowNodeKind",
            "agent",
            "internal",
            "gate",
            "monitor",
            "WorkflowRunner",
            "command",
            "WorkflowIsolation",
            "none",
            "worktree",
            "WorkflowEdgeKind",
            "sequence",
            "data",
            "conditional",
            "WorkflowEdgePayload",
            "summary",
            "full",
            "WorkflowVersionOrigin",
            "authored",
            "imported",
            "self_improvement",
            "restore_rewrite",
            "WorkflowRunStatus",
            "pending",
            "running",
            "paused",
            "succeeded",
            "failed",
            "cancelled",
            "WorkflowNodeStatus",
            "ready",
            "needs_attention",
            "blocked",
            "skipped",
            "restored",
            "WorkflowEvidence",
            "self_report",
            "hook",
            "detection",
            "WorkflowSuccession",
            "satisfied",
            "no_followup",
            "WorkflowDefinitionFormat",
            "toml",
            "json",
            // struct/field names
            "WorkflowDefinitionDocument",
            "format",
            "text",
            "WorkflowTarget",
            "workflow_id",
            "WorkflowVersionTarget",
            "version_id",
            "WorkflowRunTarget",
            "run_id",
            "WorkflowNodeTarget",
            "path",
            "WorkflowCreateParams",
            "definition",
            "WorkflowVersionCreateParams",
            "change_summary",
            "WorkflowRunParams",
            "version",
            "tier",
            "args",
            "WorkflowRunListParams",
            "limit",
            "WorkflowNodeSteerParams",
            "WorkflowNodeReportParams",
            "token",
            "result",
            "WorkflowSummary",
            "name",
            "description",
            "default_tier",
            "archived",
            "head_version_id",
            "head_version",
            "created_at_unix_ms",
            "updated_at_unix_ms",
            "KvdagVersionSummary",
            "parent_version_id",
            "origin",
            "spec_digest",
            "max_depth",
            "max_nodes",
            "WorkflowArgSpec",
            "required",
            "default",
            "KvdagNodeInfo",
            "node_key",
            "label",
            "role",
            "kind",
            "runner",
            "prompt_template",
            "system_contract",
            "output_schema",
            "max_attempts",
            "timeout_ms",
            "isolation",
            "is_template",
            "expand_allow",
            "expand_max",
            "KvdagEdgeInfo",
            "from",
            "to",
            "condition",
            "payload",
            "port",
            "KvdagVersionDetail",
            "contract",
            "nodes",
            "edges",
            "WorkflowRunInfo",
            "status",
            "workspace_id",
            "tab_id",
            "started_at_unix_ms",
            "ended_at_unix_ms",
            "total_tokens",
            "total_tool_uses",
            "nodes_total",
            "nodes_done",
            "failure",
            "WorkflowRunNodeInfo",
            "parent_path",
            "depth",
            "demand",
            "model",
            "effort",
            "attempt",
            "pane_id",
            "terminal_id",
            "agent_session_id",
            "cwd",
            "node_dir",
            "tool_uses",
            "duration_ms",
            "evidence",
            "succession",
            "blocker",
            "watchdog_interventions",
            "WorkflowRunEdgeInfo",
            "condition_result",
            "fired",
            "WorkflowRunGraph",
            // Phase 2 additions
            "WorkflowNodeExpandParams",
            "template",
            "inputs",
            "count",
            "WorkflowGrowthLimitKind",
            "expand_max",
            "max_depth",
            "max_nodes",
            "WorkflowGrowthLimit",
            "limit",
            "limit_value",
            "requested",
            "accepted",
            "at_unix_ms",
            "message",
            "WorkflowExpandRejectionReason",
            "not_allowed",
            "unknown_template",
            "not_a_template",
            "expand_max_reached",
            "max_depth_reached",
            "max_nodes_reached",
            "truncated",
            "unknown_input",
            "WorkflowExpandRejection",
            "reason",
            "WorkflowDetail",
            "detail",
            "versions",
            "workflow",
            "nodes_live",
            "growth_limited",
            "assignment_reason",
            "delivery_failure",
            "WorkflowNodeExpanded",
            "rejected",
        ];

        let offenders: Vec<&str> = identifiers
            .iter()
            .copied()
            .filter(|identifier| {
                words(identifier)
                    .iter()
                    .any(|word| BANNED.contains(&word.as_str()))
            })
            .collect();
        assert!(
            offenders.is_empty(),
            "banned UI-surface identifiers found: {offenders:?}"
        );
    }
}
