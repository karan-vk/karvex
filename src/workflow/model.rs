//! kvdag data model: definitions, run graphs, and the engine's input/effect
//! vocabulary.
//!
//! This module is pure data plus construction-time validation. It must not
//! reference `App`, `TerminalRuntime`, SurrealDB, or ratatui — see
//! `docs/design/workflow-builder/04-kvdag-and-execution.md` §1 and §2.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::terminal::TerminalId;
use crate::workflow::tier::{Assignment, Effort, ModelAlias, Tier};

// ── identities ──────────────────────────────────────────────────────────────

/// Stable identity of a workflow (the family of kvdag versions).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkflowId(pub String);

/// Identity of one immutable revision of a graph.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KvdagVersionId(pub String);

/// Identity of one execution of one [`KvdagVersionId`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub String);

/// Author-chosen node identity. Stable across versions, which is what makes a
/// semantic diff and cross-version checkpoint restore possible.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeKey(pub String);

/// Topological address of a node instance inside one run: `research/2/verify`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InstancePath(pub String);

/// Index into [`RunGraph::nodes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunNodeIdx(pub usize);

/// Per-node capability token handed to the node as `KARVEX_WORKFLOW_NODE_TOKEN`
/// and required by `workflow.node.report`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeToken(pub String);

/// SHA-256 over the canonical serialisation of a version's nodes and edges.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SpecDigest(pub String);

/// The public API pane id (`pane.split`'s `pane_id`), never an internal index.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PublicPaneId(pub String);

/// Identity of one interrogation of a finished node's agent session.
///
/// An interrogation is **not** a run node (`07-phase3-plan.md` §4 D8): it has
/// its own table, its own lifecycle, and no node token. This is the id the
/// store row is created under, allocated by the app at spawn time so the
/// `InterrogationStarted` write and the later `InterrogationUpdate` address the
/// same record without a read-back.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InterrogationId(pub String);

/// Identity of one self-improvement review cycle over a finished run.
///
/// Allocated by the app at cycle start, the same way [`InterrogationId`] is,
/// so the [`StoreWrite::ReviewCycleStarted`] write and every later
/// [`StoreWrite::ReviewCycleUpdate`]/[`StoreWrite::ReviewFindings`] address the
/// same record without a read-back.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReviewCycleId(pub String);

macro_rules! impl_string_id {
    ($($ty:ident),* $(,)?) => {
        $(
            impl $ty {
                pub fn new(value: impl Into<String>) -> Self {
                    Self(value.into())
                }

                pub fn as_str(&self) -> &str {
                    &self.0
                }
            }

            impl fmt::Display for $ty {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str(&self.0)
                }
            }
        )*
    };
}

impl_string_id!(
    WorkflowId,
    KvdagVersionId,
    RunId,
    NodeKey,
    InstancePath,
    NodeToken,
    SpecDigest,
    PublicPaneId,
    InterrogationId,
    ReviewCycleId,
);

// ── reserved namespace ──────────────────────────────────────────────────────

/// Instance paths beginning with this are karvex-owned, never author-owned
/// (`07-phase3-plan.md` §3 rule 3). [`Kvdag::try_new`] rejects an authored node
/// key that starts with it, which is what guarantees the engine's epilogue node
/// can never collide with a user node — in a selector namespace, in a run
/// counter, or on disk.
pub const RESERVED_PATH_PREFIX: &str = ".";

/// The engine-owned end-of-run summariser's instance path (`07-phase3-plan.md`
/// §4 D5). It is a real `run_node` row so the summary's `generated_by` resolves
/// and the DAG view shows it, but it is excluded from `nodes_total`/`nodes_done`
/// and from growth accounting, and it has no kvdag node behind it.
pub const SUMMARY_INSTANCE_PATH: &str = ".summary";

/// Whether an instance path names an engine-owned node rather than a node the
/// author declared. The one predicate every counter, selector, and sweep filters
/// on, so the rule lives in exactly one place.
pub fn is_reserved_path(path: &str) -> bool {
    path.starts_with(RESERVED_PATH_PREFIX)
}

// ── definition (immutable, one per kvdag_version) ───────────────────────────

/// A run argument supplied at `workflow.run` time and materialised into
/// `workflow_run.args`. Declaring the namespace is what makes `{{goal}}` in a
/// root node's prompt template resolvable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArgSpec {
    pub name: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub description: String,
}

/// Authoritative growth guardrails. A run may narrow these but never widen
/// them; raising a ceiling is an authoring edit and creates a new version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrowthLimits {
    pub max_depth: u16,
    pub max_nodes: u16,
}

impl Default for GrowthLimits {
    fn default() -> Self {
        Self {
            max_depth: 3,
            max_nodes: 24,
        }
    }
}

impl GrowthLimits {
    /// How many nodes a run has already spent against [`Self::max_nodes`].
    ///
    /// The budget is **monotone** (`06-phase2-plan.md` §4 D12): every
    /// materialised [`RunNode`] counts, whatever its status, so a failed,
    /// skipped, or cancelled child does not refund budget. Otherwise a node
    /// could fan out indefinitely by failing and the ceiling would stop being a
    /// ceiling. Saturates rather than wrapping, so an absurd graph reports
    /// "full" instead of "empty".
    ///
    /// **Engine-owned nodes do not count** (`07-phase3-plan.md` §4 D5).
    /// `max_nodes` governs *expansion proposals* — what the run's own graph is
    /// allowed to grow into — and the `.summary` epilogue is karvex's, not the
    /// run's. Counting it would let the run's budget be spent by a node the
    /// author never declared and cannot remove, and would make the wire's
    /// `nodes_live` report one more node than the run has.
    pub fn live_node_count(nodes: &[RunNode]) -> u16 {
        let counted = nodes
            .iter()
            .filter(|node| !is_reserved_path(node.path.as_str()))
            .count();
        u16::try_from(counted).unwrap_or(u16::MAX)
    }

    /// Whether one more node still fits under `max_nodes`.
    pub fn has_node_budget(&self, nodes: &[RunNode]) -> bool {
        Self::live_node_count(nodes) < self.max_nodes
    }
}

/// What the node *is*. Orthogonal to [`Runner`], which selects how it is bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// A `claude` teammate in a pane — the normal case.
    #[default]
    Agent,
    /// karvex-owned utility (summariser, interviewer); still a visible node.
    Internal,
    /// No agent; evaluates conditions or waits for a human decision.
    Gate,
    /// Polls a condition; "checked, unchanged" is a legal no-op result.
    Monitor,
}

/// Selects the *binding* the spawner uses. Never inferred from "are we in a
/// test": a definition states it explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Runner {
    /// `claude` in a pane. Confirmed with `begin_managed_agent`, steered with
    /// `agent.prompt`.
    #[default]
    Agent,
    /// A plain process in a pane, launched from `command`. No managed-agent
    /// confirmation, no agent detection, steered with `pane.send_text`.
    Command,
}

/// How demanding the node's work is; drives the tier → (model, effort) mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Demand {
    Peak,
    Critical,
    #[default]
    Standard,
    Light,
}

/// Working-directory isolation for a node's pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    #[default]
    None,
    Worktree,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KvdagNode {
    pub key: NodeKey,
    pub label: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub kind: NodeKind,
    #[serde(default)]
    pub demand: Demand,
    #[serde(default)]
    pub runner: Runner,
    /// argv, never a shell string. Required iff `runner == Runner::Command`.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// `{{name}}` slots are filled from inbound edge ports or run args.
    pub prompt_template: String,
    #[serde(default)]
    pub system_contract: Option<String>,
    /// JSON Schema; the node's result is validated against it before the node
    /// may succeed.
    pub output_schema: OutputSchema,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u8,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub isolation: Isolation,
    /// Not scheduled directly; only instantiated via an accepted expand
    /// proposal.
    #[serde(default)]
    pub is_template: bool,
    #[serde(default)]
    pub expand_allow: Vec<NodeKey>,
    /// 0 = this node may not expand (the default).
    #[serde(default)]
    pub expand_max: u16,
}

fn default_max_attempts() -> u8 {
    2
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Sequence,
    Data,
    Conditional,
}

/// How much of the source's checkpoint is handed to the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EdgePayload {
    None,
    #[default]
    Summary,
    Full,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KvdagEdge {
    pub from: NodeKey,
    pub to: NodeKey,
    pub kind: EdgeKind,
    #[serde(default)]
    pub condition: Option<Condition>,
    #[serde(default)]
    pub payload: EdgePayload,
    /// Template slot in the target's prompt.
    #[serde(default)]
    pub port: Option<String>,
}

/// Dotted path into a node's validated output: `review.verdict`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FieldPath(pub String);

impl FieldPath {
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('.').filter(|segment| !segment.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonScalar {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Lt,
    Le,
    Gt,
    Ge,
}

/// Total, loop-free, side-effect-free predicate over a node's validated output.
/// Deliberately not Turing-complete.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Condition {
    Always,
    Exists {
        path: FieldPath,
    },
    Eq {
        path: FieldPath,
        value: JsonScalar,
    },
    Cmp {
        path: FieldPath,
        op: CmpOp,
        value: JsonScalar,
    },
    OneOf {
        path: FieldPath,
        values: Vec<JsonScalar>,
    },
    Not(Box<Condition>),
    All(Vec<Condition>),
    Any(Vec<Condition>),
}

/// The JSON Schema a node's `result.json` must satisfy before the node may
/// succeed.
///
/// Phase 1 validates the schema's *shape* at construction time; evaluating a
/// result against it is `engine::complete`'s job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OutputSchema(serde_json::Value);

impl OutputSchema {
    /// The schema document itself, for callers that hand it back out (the
    /// node's `output_schema.json`, the `workflow.version.get` projection).
    pub fn as_json(&self) -> &serde_json::Value {
        &self.0
    }

    /// Accepts a schema document after checking the subset of JSON Schema
    /// karvex evaluates.
    pub fn parse(value: serde_json::Value) -> Result<Self, SchemaError> {
        let schema = Self(value);
        schema.validate()?;
        Ok(schema)
    }

    pub fn as_value(&self) -> &serde_json::Value {
        &self.0
    }

    /// Field names the schema marks required, used both by the completion gate
    /// and by the corrective re-prompt that quotes the unfilled fields.
    pub fn required_fields(&self) -> Vec<String> {
        self.0
            .get("required")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn validate(&self) -> Result<(), SchemaError> {
        validate_schema_object(&self.0, "")
    }
}

const JSON_SCHEMA_TYPES: [&str; 7] = [
    "object", "array", "string", "number", "integer", "boolean", "null",
];

fn validate_schema_object(value: &serde_json::Value, at: &str) -> Result<(), SchemaError> {
    let object = value.as_object().ok_or_else(|| SchemaError {
        at: at.to_string(),
        message: "schema must be a JSON object".to_string(),
    })?;

    if let Some(type_value) = object.get("type") {
        validate_schema_type(type_value, at)?;
    }

    if let Some(required) = object.get("required") {
        let entries = required.as_array().ok_or_else(|| SchemaError {
            at: at.to_string(),
            message: "\"required\" must be an array of field names".to_string(),
        })?;
        for entry in entries {
            if !entry.is_string() {
                return Err(SchemaError {
                    at: at.to_string(),
                    message: "\"required\" must contain only field names".to_string(),
                });
            }
        }
    }

    if let Some(properties) = object.get("properties") {
        let entries = properties.as_object().ok_or_else(|| SchemaError {
            at: at.to_string(),
            message: "\"properties\" must be an object".to_string(),
        })?;
        for (name, property) in entries {
            validate_schema_object(property, &join_schema_path(at, name))?;
        }
    }

    if let Some(items) = object.get("items") {
        match items {
            serde_json::Value::Array(entries) => {
                for (index, entry) in entries.iter().enumerate() {
                    validate_schema_object(entry, &join_schema_path(at, &index.to_string()))?;
                }
            }
            other => validate_schema_object(other, &join_schema_path(at, "items"))?,
        }
    }

    Ok(())
}

fn validate_schema_type(value: &serde_json::Value, at: &str) -> Result<(), SchemaError> {
    let check = |name: &serde_json::Value| -> Result<(), SchemaError> {
        let name = name.as_str().ok_or_else(|| SchemaError {
            at: at.to_string(),
            message: "\"type\" must be a JSON Schema type name".to_string(),
        })?;
        if !JSON_SCHEMA_TYPES.contains(&name) {
            return Err(SchemaError {
                at: at.to_string(),
                message: format!("unknown JSON Schema type \"{name}\""),
            });
        }
        Ok(())
    };

    match value {
        serde_json::Value::Array(entries) => {
            if entries.is_empty() {
                return Err(SchemaError {
                    at: at.to_string(),
                    message: "\"type\" must name at least one JSON Schema type".to_string(),
                });
            }
            for entry in entries {
                check(entry)?;
            }
            Ok(())
        }
        other => check(other),
    }
}

fn join_schema_path(at: &str, segment: &str) -> String {
    if at.is_empty() {
        segment.to_string()
    } else {
        format!("{at}.{segment}")
    }
}

/// Where in a schema document the shape check failed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaError {
    pub at: String,
    pub message: String,
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.at.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "at \"{}\": {}", self.at, self.message)
        }
    }
}

impl std::error::Error for SchemaError {}

/// The unvalidated inputs of a kvdag, before construction invariants run.
#[derive(Debug, Clone, PartialEq)]
pub struct KvdagSpec {
    pub version_id: KvdagVersionId,
    pub workflow_id: WorkflowId,
    pub version: u32,
    pub parent: Option<KvdagVersionId>,
    /// Prepended to every node's system prompt.
    pub contract: String,
    pub growth: GrowthLimits,
    pub args: Vec<ArgSpec>,
    pub nodes: Vec<KvdagNode>,
    pub edges: Vec<KvdagEdge>,
}

/// An immutable, validated graph definition.
#[derive(Debug, Clone, PartialEq)]
pub struct Kvdag {
    pub version_id: KvdagVersionId,
    pub workflow_id: WorkflowId,
    pub version: u32,
    pub parent: Option<KvdagVersionId>,
    pub contract: String,
    pub growth: GrowthLimits,
    pub args: Vec<ArgSpec>,
    /// Topologically sorted at construction.
    pub nodes: Vec<KvdagNode>,
    pub edges: Vec<KvdagEdge>,
    pub spec_digest: SpecDigest,
}

impl Kvdag {
    /// Checks every construction invariant, topologically sorts the nodes, and
    /// computes the spec digest.
    pub fn try_new(spec: KvdagSpec) -> Result<Self, KvdagError> {
        let KvdagSpec {
            version_id,
            workflow_id,
            version,
            parent,
            contract,
            growth,
            args,
            nodes,
            edges,
        } = spec;

        if nodes.is_empty() {
            return Err(KvdagError::EmptyGraph);
        }

        let mut by_key: HashMap<&str, &KvdagNode> = HashMap::with_capacity(nodes.len());
        for node in &nodes {
            // The reserved namespace is karvex's, from Phase 3 on: engine-owned
            // nodes live under `.`-prefixed instance paths, and an authored key
            // that could produce one would collide with them
            // (`07-phase3-plan.md` §3 rule 3, §6 A3). Only new definitions are
            // checked — stored versions are read back, never re-validated.
            if is_reserved_path(node.key.as_str()) {
                return Err(KvdagError::ReservedNodeKey(node.key.clone()));
            }
            if by_key.insert(node.key.as_str(), node).is_some() {
                return Err(KvdagError::DuplicateNodeKey(node.key.clone()));
            }
        }

        let mut arg_names: HashSet<&str> = HashSet::with_capacity(args.len());
        for arg in &args {
            if !arg_names.insert(arg.name.as_str()) {
                return Err(KvdagError::DuplicateArg(arg.name.clone()));
            }
        }

        for edge in &edges {
            if !by_key.contains_key(edge.from.as_str()) {
                return Err(KvdagError::UnknownEdgeEndpoint {
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                    missing: edge.from.clone(),
                });
            }
            if !by_key.contains_key(edge.to.as_str()) {
                return Err(KvdagError::UnknownEdgeEndpoint {
                    from: edge.from.clone(),
                    to: edge.to.clone(),
                    missing: edge.to.clone(),
                });
            }
            if edge.from == edge.to {
                return Err(KvdagError::SelfEdge(edge.from.clone()));
            }
        }

        for node in &nodes {
            match (node.runner, node.command.as_ref()) {
                (Runner::Command, None) => {
                    return Err(KvdagError::MissingCommand(node.key.clone()));
                }
                (Runner::Command, Some(command)) if command.is_empty() => {
                    return Err(KvdagError::MissingCommand(node.key.clone()));
                }
                (Runner::Agent, Some(_)) => {
                    return Err(KvdagError::UnexpectedCommand(node.key.clone()));
                }
                _ => {}
            }

            if let Err(error) = node.output_schema.validate() {
                return Err(KvdagError::InvalidOutputSchema {
                    node: node.key.clone(),
                    error,
                });
            }

            for template in &node.expand_allow {
                match by_key.get(template.as_str()) {
                    None => {
                        return Err(KvdagError::UnknownExpandTemplate {
                            node: node.key.clone(),
                            template: template.clone(),
                        });
                    }
                    Some(target) if !target.is_template => {
                        return Err(KvdagError::ExpandTargetNotTemplate {
                            node: node.key.clone(),
                            template: template.clone(),
                        });
                    }
                    Some(_) => {}
                }
            }
        }

        let ports = inbound_ports(&edges)?;
        for node in &nodes {
            let inbound = ports.get(node.key.as_str());
            for placeholder in scan_placeholders(&node.prompt_template).map_err(|error| {
                KvdagError::MalformedPlaceholder {
                    node: node.key.clone(),
                    error,
                }
            })? {
                let resolved = arg_names.contains(placeholder.as_str())
                    || inbound.is_some_and(|names| names.contains(placeholder.as_str()));
                if !resolved {
                    return Err(KvdagError::UnresolvedPlaceholder {
                        node: node.key.clone(),
                        name: placeholder,
                    });
                }
            }
        }

        let order = topological_order(&nodes, &edges)?;
        let roots: Vec<&KvdagNode> = nodes
            .iter()
            .filter(|node| !node.is_template && !has_inbound(&edges, &node.key))
            .collect();
        if roots.is_empty() {
            return Err(KvdagError::NoRoot);
        }
        assert_reachable(&nodes, &edges, &roots)?;

        let sorted: Vec<KvdagNode> = order
            .iter()
            .filter_map(|key| by_key.get(key.as_str()).map(|node| (*node).clone()))
            .collect();
        let spec_digest = spec_digest(&sorted, &edges)?;

        Ok(Self {
            version_id,
            workflow_id,
            version,
            parent,
            contract,
            growth,
            args,
            nodes: sorted,
            edges,
            spec_digest,
        })
    }

    pub fn node(&self, key: &NodeKey) -> Option<&KvdagNode> {
        self.nodes.iter().find(|node| &node.key == key)
    }

    pub fn inbound_edges<'a>(&'a self, key: &'a NodeKey) -> impl Iterator<Item = &'a KvdagEdge> {
        self.edges.iter().filter(move |edge| &edge.to == key)
    }

    pub fn outbound_edges<'a>(&'a self, key: &'a NodeKey) -> impl Iterator<Item = &'a KvdagEdge> {
        self.edges.iter().filter(move |edge| &edge.from == key)
    }

    /// Non-template nodes with no inbound edge; these start `Ready` at run
    /// start.
    pub fn roots(&self) -> impl Iterator<Item = &KvdagNode> {
        self.nodes
            .iter()
            .filter(|node| !node.is_template && !has_inbound(&self.edges, &node.key))
    }
}

fn has_inbound(edges: &[KvdagEdge], key: &NodeKey) -> bool {
    edges.iter().any(|edge| &edge.to == key)
}

fn inbound_ports(edges: &[KvdagEdge]) -> Result<HashMap<&str, HashSet<&str>>, KvdagError> {
    let mut ports: HashMap<&str, HashSet<&str>> = HashMap::new();
    for edge in edges {
        let Some(port) = edge.port.as_deref() else {
            continue;
        };
        if !ports.entry(edge.to.as_str()).or_default().insert(port) {
            return Err(KvdagError::DuplicatePort {
                node: edge.to.clone(),
                port: port.to_string(),
            });
        }
    }
    Ok(ports)
}

/// Kahn's algorithm with a `node_key` tiebreak so the order is deterministic
/// for a given graph.
fn topological_order(nodes: &[KvdagNode], edges: &[KvdagEdge]) -> Result<Vec<NodeKey>, KvdagError> {
    let mut indegree: BTreeMap<&str, usize> = nodes
        .iter()
        .map(|node| (node.key.as_str(), 0usize))
        .collect();
    let mut outgoing: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for edge in edges {
        if outgoing
            .entry(edge.from.as_str())
            .or_default()
            .insert(edge.to.as_str())
        {
            *indegree.entry(edge.to.as_str()).or_insert(0) += 1;
        }
    }

    let mut ready: BTreeSet<&str> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(key, _)| *key)
        .collect();
    let mut order: Vec<NodeKey> = Vec::with_capacity(nodes.len());
    while let Some(key) = ready.iter().next().copied() {
        ready.remove(key);
        order.push(NodeKey::new(key));
        for target in outgoing.get(key).into_iter().flatten().copied() {
            let degree = indegree.entry(target).or_insert(0);
            *degree = degree.saturating_sub(1);
            if *degree == 0 {
                ready.insert(target);
            }
        }
    }

    if order.len() != nodes.len() {
        let ordered: HashSet<&str> = order.iter().map(NodeKey::as_str).collect();
        let cycle: Vec<NodeKey> = nodes
            .iter()
            .filter(|node| !ordered.contains(node.key.as_str()))
            .map(|node| node.key.clone())
            .collect();
        return Err(KvdagError::Cycle(cycle));
    }
    Ok(order)
}

fn assert_reachable(
    nodes: &[KvdagNode],
    edges: &[KvdagEdge],
    roots: &[&KvdagNode],
) -> Result<(), KvdagError> {
    let templates: HashSet<&str> = nodes
        .iter()
        .filter(|node| node.is_template)
        .map(|node| node.key.as_str())
        .collect();
    let mut seen: HashSet<&str> = roots.iter().map(|node| node.key.as_str()).collect();
    let mut queue: VecDeque<&str> = seen.iter().copied().collect();
    while let Some(key) = queue.pop_front() {
        // The walk stops at a template. A template is never scheduled directly
        // (§3.1) and its edges are dropped when the run graph is materialised,
        // so a node whose only path from a root runs through one would start
        // the run with no inbound edge at all — that is, `Ready` at t=0,
        // executing before the work it is meant to consume. Reporting it as
        // unreachable is what keeps §3.1's root rule ("nodes with no inbound
        // edges start Ready") true of the materialised graph. §3.4's fan-in
        // point is the *proposing parent's* outbound edge, which is exactly the
        // edge this asks the author to draw.
        if templates.contains(key) {
            continue;
        }
        for edge in edges.iter().filter(|edge| edge.from.as_str() == key) {
            if seen.insert(edge.to.as_str()) {
                queue.push_back(edge.to.as_str());
            }
        }
    }

    for node in nodes {
        if !node.is_template && !seen.contains(node.key.as_str()) {
            return Err(KvdagError::UnreachableNode(node.key.clone()));
        }
    }
    Ok(())
}

/// Scans `{{name}}` slots. Whitespace inside the braces is allowed; anything
/// else that opens `{{` without a well-formed name and a closing `}}` is a
/// template typo and is rejected at authoring time rather than at render time.
fn scan_placeholders(template: &str) -> Result<Vec<String>, PlaceholderError> {
    let bytes = template.as_bytes();
    let mut names = Vec::new();
    let mut index = 0usize;
    while index + 1 < bytes.len() {
        if bytes[index] != b'{' || bytes[index + 1] != b'{' {
            index += 1;
            continue;
        }
        let body_start = index + 2;
        let Some(offset) = template[body_start..].find("}}") else {
            return Err(PlaceholderError::Unclosed { at: index });
        };
        let body = &template[body_start..body_start + offset];
        let name = body.trim();
        if name.is_empty() || !name.chars().all(is_placeholder_char) {
            return Err(PlaceholderError::InvalidName {
                at: index,
                text: body.to_string(),
            });
        }
        names.push(name.to_string());
        index = body_start + offset + 2;
    }
    Ok(names)
}

fn is_placeholder_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaceholderError {
    Unclosed { at: usize },
    InvalidName { at: usize, text: String },
}

impl fmt::Display for PlaceholderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unclosed { at } => write!(f, "unclosed {{{{ at byte {at}"),
            Self::InvalidName { at, text } => {
                write!(f, "invalid placeholder name \"{text}\" at byte {at}")
            }
        }
    }
}

/// A JSON value with every object's keys in sorted order, recursively.
///
/// The one canonicaliser in the tree: node-result digests
/// (`engine::complete`) and the cross-version restore compatibility digests
/// (`07-phase3-plan.md` §3 rule 5) must agree byte for byte, so both read this
/// function rather than each other's. It lives here because the store computes
/// the restore digests and must not depend on `engine/` internals; `complete.rs`
/// keeps a re-export so its own call sites read unchanged.
pub fn canonical(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            let mut sorted = serde_json::Map::with_capacity(keys.len());
            for key in keys {
                if let Some(entry) = map.get(key) {
                    sorted.insert(key.clone(), canonical(entry));
                }
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonical).collect())
        }
        other => other.clone(),
    }
}

/// SHA-256 over a canonical rendering of the nodes and edges: both are sorted
/// and every JSON object key is emitted in sorted order, so the digest depends
/// on the graph and not on authoring order or on which serde features other
/// crates in the build happen to enable.
fn spec_digest(nodes: &[KvdagNode], edges: &[KvdagEdge]) -> Result<SpecDigest, KvdagError> {
    let mut sorted_nodes: Vec<&KvdagNode> = nodes.iter().collect();
    sorted_nodes.sort_by(|left, right| left.key.cmp(&right.key));
    let mut sorted_edges: Vec<&KvdagEdge> = edges.iter().collect();
    sorted_edges.sort_by(|left, right| {
        (&left.from, &left.to, &left.port).cmp(&(&right.from, &right.to, &right.port))
    });

    let mut canonical = String::new();
    canonical.push_str("nodes\n");
    for node in sorted_nodes {
        canonical_value(&to_json(node)?, &mut canonical);
        canonical.push('\n');
    }
    canonical.push_str("edges\n");
    for edge in sorted_edges {
        canonical_value(&to_json(edge)?, &mut canonical);
        canonical.push('\n');
    }

    Ok(SpecDigest(format!(
        "{:x}",
        Sha256::digest(canonical.as_bytes())
    )))
}

fn to_json<T: Serialize>(value: &T) -> Result<serde_json::Value, KvdagError> {
    serde_json::to_value(value).map_err(|error| KvdagError::Digest(error.to_string()))
}

fn canonical_value(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Object(entries) => {
            let mut keys: Vec<&String> = entries.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::Value::String(key.clone()).to_string());
                out.push(':');
                if let Some(entry) = entries.get(key) {
                    canonical_value(entry, out);
                }
            }
            out.push('}');
        }
        serde_json::Value::Array(entries) => {
            out.push('[');
            for (index, entry) in entries.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                canonical_value(entry, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// Every way a kvdag definition can fail its construction invariants.
#[derive(Debug, Clone, PartialEq)]
pub enum KvdagError {
    EmptyGraph,
    DuplicateNodeKey(NodeKey),
    /// The key starts with [`RESERVED_PATH_PREFIX`], which names the engine's
    /// own namespace.
    ReservedNodeKey(NodeKey),
    DuplicateArg(String),
    UnknownEdgeEndpoint {
        from: NodeKey,
        to: NodeKey,
        missing: NodeKey,
    },
    SelfEdge(NodeKey),
    Cycle(Vec<NodeKey>),
    DuplicatePort {
        node: NodeKey,
        port: String,
    },
    UnresolvedPlaceholder {
        node: NodeKey,
        name: String,
    },
    MalformedPlaceholder {
        node: NodeKey,
        error: PlaceholderError,
    },
    UnknownExpandTemplate {
        node: NodeKey,
        template: NodeKey,
    },
    ExpandTargetNotTemplate {
        node: NodeKey,
        template: NodeKey,
    },
    NoRoot,
    UnreachableNode(NodeKey),
    InvalidOutputSchema {
        node: NodeKey,
        error: SchemaError,
    },
    MissingCommand(NodeKey),
    UnexpectedCommand(NodeKey),
    Digest(String),
}

impl fmt::Display for KvdagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyGraph => f.write_str("a kvdag needs at least one node"),
            Self::DuplicateNodeKey(key) => write!(f, "duplicate node key \"{key}\""),
            Self::ReservedNodeKey(key) => write!(
                f,
                "node key \"{key}\" starts with \"{RESERVED_PATH_PREFIX}\", which is reserved for karvex-owned nodes"
            ),
            Self::DuplicateArg(name) => write!(f, "duplicate run argument \"{name}\""),
            Self::UnknownEdgeEndpoint { from, to, missing } => write!(
                f,
                "edge {from} -> {to} references unknown node \"{missing}\""
            ),
            Self::SelfEdge(key) => write!(f, "node \"{key}\" has an edge to itself"),
            Self::Cycle(keys) => {
                let keys: Vec<&str> = keys.iter().map(NodeKey::as_str).collect();
                write!(f, "graph is cyclic through: {}", keys.join(", "))
            }
            Self::DuplicatePort { node, port } => write!(
                f,
                "node \"{node}\" has more than one inbound edge on port \"{port}\""
            ),
            Self::UnresolvedPlaceholder { node, name } => write!(
                f,
                "node \"{node}\" references {{{{{name}}}}}, which is neither an inbound edge port nor a declared argument"
            ),
            Self::MalformedPlaceholder { node, error } => {
                write!(f, "node \"{node}\" has a malformed template: {error}")
            }
            Self::UnknownExpandTemplate { node, template } => write!(
                f,
                "node \"{node}\" may expand into unknown node \"{template}\""
            ),
            Self::ExpandTargetNotTemplate { node, template } => write!(
                f,
                "node \"{node}\" may expand into \"{template}\", which is not a template node"
            ),
            Self::NoRoot => f.write_str("a kvdag needs at least one non-template node with no inbound edge"),
            Self::UnreachableNode(key) => {
                write!(f, "node \"{key}\" is not reachable from any root")
            }
            Self::InvalidOutputSchema { node, error } => {
                write!(f, "node \"{node}\" has an invalid output schema: {error}")
            }
            Self::MissingCommand(key) => write!(
                f,
                "node \"{key}\" declares runner \"command\" but no command argv"
            ),
            Self::UnexpectedCommand(key) => write!(
                f,
                "node \"{key}\" declares a command but runner \"agent\""
            ),
            Self::Digest(message) => write!(f, "could not compute the spec digest: {message}"),
        }
    }
}

impl std::error::Error for KvdagError {}

// ── run graph (mutable during a run, pure data) ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Running,
    Paused,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
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

impl NodeStatus {
    /// Terminal for scheduling purposes: no further transition happens without
    /// an explicit restart.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Skipped | Self::Restored | Self::Cancelled
        )
    }
}

/// Karvex's own opinion about a node needing intervention — never the
/// projected [`NodeStatus`] itself (`phase4-retarget-plan.md` D-10: "the
/// difference between a projection and an opinion"). `NodeStatus` is Claude
/// Code's fact, projected verbatim from its task/team state; `Attention` is
/// what the watchdog concludes from watching that fact over time, and it must
/// never overwrite the status column — it lives in its own `run_node.attention`
/// column instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    /// No material progress for `stuck_threshold` consecutive watchdog
    /// samples (§3.7).
    Stuck,
    /// The node's usage has crossed the run's configured budget.
    BudgetExceeded,
    /// The node's owner is waiting on a human or another agent to answer.
    NeedsInput,
    /// The team lead itself is blocked; every member downstream inherits it
    /// rather than each independently discovering the same fact.
    LeadBlocked,
    /// The watchdog has samples for this node but has not yet learned a
    /// claude session to attribute them to.
    Unbound,
}

impl Attention {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stuck => "stuck",
            Self::BudgetExceeded => "budget_exceeded",
            Self::NeedsInput => "needs_input",
            Self::LeadBlocked => "lead_blocked",
            Self::Unbound => "unbound",
        }
    }
}

/// Which completion signal was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    SelfReport,
    Hook,
    Detection,
    Restored,
}

/// Every closing node records exactly one of these; a terminal node without one
/// is a `SuccessionGap`, which is what stops a branch from quietly evaporating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Succession {
    /// Validated result present, outbound edges resolved from it.
    Satisfied,
    /// Structured blocker with an explicit resume condition.
    Blocked { reason: String, resume_when: String },
    /// Explicit terminal evidence that nothing follows.
    NoFollowup { evidence: String },
}

/// Where a node's pane and `claude` session live once it has been spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeBinding {
    pub pane_id: PublicPaneId,
    pub terminal_id: TerminalId,
    /// karvex-assigned, passed as `claude --session-id`, which is what makes
    /// `transcript_path` derivable before the process starts.
    pub agent_session_id: String,
    pub transcript_path: PathBuf,
    pub node_dir: PathBuf,
    pub cwd: PathBuf,
}

/// A node's schema-validated output.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeResult {
    pub payload: serde_json::Value,
    /// Token-lean handoff text; this, not `payload`, is what `payload: summary`
    /// edges pass downstream.
    pub summary: String,
    pub artifact_paths: Vec<String>,
    pub digest: String,
    pub evidence: Evidence,
}

/// Where a restored node's result came from: the source run, the node key it
/// ran under there, and which of that node's checkpoints was taken.
///
/// The pure-layer mirror of the `run_node.restored_from` column. The store maps
/// it to the source checkpoint's record id at write time and back to this shape
/// at read time; the engine never sees a database id
/// (`07-phase3-plan.md` §1 WS-A, §4 D4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredRef {
    pub run: RunId,
    pub node_key: NodeKey,
    /// Per-node checkpoint counter in the **source** run, matching
    /// [`RunNode::checkpoint_seq`]'s semantics there.
    pub checkpoint_seq: u64,
}

/// One node's restored result, as handed to
/// [`RunGraph::materialise_with_restored`].
///
/// Restore is materialisation, never an engine input (`07-phase3-plan.md`
/// §4 D3): a restored node is a fact about how the run *begins*, so there is no
/// window in which it is `Pending` and no second transition path into
/// [`NodeStatus::Restored`] for the invariants to cover.
#[derive(Debug, Clone, PartialEq)]
pub struct RestoredSeed {
    /// The **target** version's node key this seed satisfies. Compatibility
    /// with the source is decided by the caller (§4 D11) before the seed exists.
    pub node_key: NodeKey,
    pub payload: serde_json::Value,
    pub summary: String,
    pub artifact_paths: Vec<String>,
    pub digest: String,
    pub source: RestoredRef,
}

/// How far the engine-owned end-of-run summariser has got.
///
/// `GaveUp` is terminal and deliberately unremarkable: every failure mode of the
/// epilogue converges on it — schema-invalid twice, spawn failure, pane death,
/// cancel — and none of them touches the run's status (`07-phase3-plan.md`
/// §4 D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpiloguePhase {
    Pending,
    Running,
    Done,
    GaveUp,
}

impl EpiloguePhase {
    /// Whether the engine still needs ticks to drive the summariser. The app's
    /// liveness disjunct reads this through `Engine::epilogue_pending`.
    pub fn is_pending(self) -> bool {
        matches!(self, Self::Pending | Self::Running)
    }
}

/// The summariser node the engine appends *after* the user graph's terminal
/// status is decided.
///
/// It is outside `run_terminal_ready`'s conjunction by construction: a
/// summariser inside it wedges failed runs, because a `Failed` leaf never
/// resolves its outbound edge (`07-phase3-plan.md` §0.7, §4 D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpilogueState {
    pub node: RunNodeIdx,
    pub phase: EpiloguePhase,
    /// How the summariser is actually bound, decided once when the epilogue is
    /// appended (`07-phase3-plan.md` §4 D2, as amended by defect D-1).
    ///
    /// The epilogue has no kvdag node, so the engine cannot derive this the way
    /// it derives every other node's runner — and the default it *would* fall
    /// back to is `Agent`, which lies whenever
    /// `KARVEX_WORKFLOW_SUMMARY_COMMAND` binds the summariser to a script. That
    /// lie is not cosmetic: it decides whether sustained-idle detection is an
    /// admissible completion signal, and therefore whether karvex would type a
    /// seed prompt into what is really a shell pane. Recording the truth here
    /// is what keeps `Engine::runner_of` honest for the reserved path.
    pub runner: Runner,
}

/// `total_tokens` and `tool_uses` only ever advance through
/// `EngineInput::ProgressObserved`, which is documented (`04-kvdag-and-execution.md`
/// §6.1) to be fed by a tail of the node's transcript JSONL. That transcript-tail
/// producer is not implemented yet — no caller in this build ever constructs a
/// `ProgressDelta` with a nonzero `tokens`/`tool_calls` — so both fields read `0`
/// for every run today. This is a known gap, not silent data loss: wiring a real
/// source would be a new subsystem (transcript tailing/parsing), out of scope for
/// the timestamp/duration fix that populates `duration_ms`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NodeUsage {
    pub total_tokens: u64,
    pub tool_uses: u32,
    pub duration_ms: u64,
}

/// Watchdog bookkeeping. Phase 1 records the evidence; Phase 4 acts on it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProgressTracker {
    pub no_progress_streak: u16,
    pub drift_streak: u16,
    pub last_progress_at: Option<Instant>,
    pub last_screen_digest: Option<String>,
    pub tool_calls: u32,
    pub tokens: u64,
    pub artifact_changes: u32,
    pub interventions: u16,
}

/// A resolved `(model, effort)` **with the reason it was chosen**.
///
/// [`crate::workflow::tier::Assignment`] carries the pair alone and stays as it
/// is; this is the type the run persists, because `auto` resolves from a node's
/// measured history and an assignment nobody can explain after the fact is not
/// auditable (`06-phase2-plan.md` §4 D9). One table, computed once at run start
/// by `graph::resolve_assignments` for **every** kvdag node including templates,
/// so an expansion child never needs a mid-run history lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeAssignment {
    pub model: ModelAlias,
    pub effort: Effort,
    /// The §7.3 reason string, persisted to `run_node.assignment_reason`.
    /// Empty for a fixed tier, whose row *is* the explanation.
    pub reason: String,
}

impl NodeAssignment {
    /// The `(model, effort)` half, for the call sites that bind a spawn.
    pub fn assignment(&self) -> Assignment {
        Assignment {
            model: self.model,
            effort: self.effort,
        }
    }

    pub fn from_assignment(assignment: Assignment, reason: impl Into<String>) -> Self {
        Self {
            model: assignment.model,
            effort: assignment.effort,
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunNode {
    pub idx: RunNodeIdx,
    pub key: NodeKey,
    pub path: InstancePath,
    /// What this **instance** is called: the kvdag node's authored `label` for a
    /// static node, and the proposing node's `--label` for an expansion child
    /// (`04-kvdag-and-execution.md` §3.4 step 1).
    ///
    /// Per instance, never per key. A generation cut from one template shares a
    /// key, so a label resolved from the definition names every sibling
    /// identically — which is the one thing a fan-out must not do. Empty means
    /// "the author named nothing"; every renderer falls back to the key or the
    /// instance path, and none of them invents a name here.
    pub label: String,
    /// The `{{slot}}` overrides this instance was created with — the `--input
    /// k=v` half of an expand proposal (§3.4 step 1, `06-phase2-plan.md` §4 D3),
    /// empty for a static node.
    ///
    /// Kept on the node rather than only in the `expand_accepted` journal entry
    /// because the prompt is rendered at **spawn** time, which is after the
    /// parent settles: without this the accepted override is validated, audited,
    /// and then discarded before the child's `task.md` is written.
    pub inputs: BTreeMap<String, String>,
    pub parent: Option<RunNodeIdx>,
    pub depth: u16,
    pub status: NodeStatus,
    /// Model and effort, resolved from the run's tier and the node's demand.
    pub assignment: Assignment,
    /// Why [`Self::assignment`] reads what it reads — the §7.3 reason string
    /// for `auto`, empty for a fixed tier. Persisted to
    /// `run_node.assignment_reason` so a finished run can still be explained
    /// (`06-phase2-plan.md` §4 D9).
    pub assignment_reason: String,
    pub attempt: u8,
    pub binding: Option<NodeBinding>,
    pub result: Option<NodeResult>,
    pub usage: NodeUsage,
    /// Stamped the first time the node reaches `Running` (`engine::record_status`).
    /// Cleared on restart, so a new attempt gets its own `started_at`.
    pub started_at_unix_ms: Option<u64>,
    /// Stamped the first time the node reaches a terminal status
    /// (`NodeStatus::is_terminal`). Cleared on restart alongside `started_at_unix_ms`.
    pub ended_at_unix_ms: Option<u64>,
    pub progress: ProgressTracker,
    pub succession: Option<Succession>,
    /// Per-node checkpoint counter, starting at 1 for the node's first
    /// checkpoint. `node_checkpoint`'s unique index is `(run_node, seq)`
    /// (`03-storage-schema.md` §4.3), so this is deliberately *not* the run's
    /// journal cursor — two nodes' first checkpoints are both `seq = 1`.
    pub checkpoint_seq: u64,
    /// Set only on a node seeded by [`RunGraph::materialise_with_restored`];
    /// `None` for every node this run actually executed.
    ///
    /// A restored node's own timestamps are the **restore instant**, not the
    /// source run's (`07-phase3-plan.md` §4 D4) — copying them would make the
    /// new run's timeline claim a node finished before its run started — so
    /// this is the only place its provenance lives.
    pub restored_from: Option<RestoredRef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunEdge {
    pub from: RunNodeIdx,
    pub to: RunNodeIdx,
    pub kind: EdgeKind,
    pub condition: Option<Condition>,
    pub payload: EdgePayload,
    pub port: Option<String>,
    /// `Some(false)` marks a dead edge; a node whose every inbound edge is dead
    /// becomes `Skipped`.
    pub condition_result: Option<bool>,
    pub fired: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunGraph {
    pub run_id: RunId,
    pub version_id: KvdagVersionId,
    pub tier: Tier,
    pub growth: GrowthLimits,
    /// Every kvdag node's resolved `(model, effort, reason)`, **including
    /// templates**, computed once at run start by
    /// `crate::workflow::engine::graph::resolve_assignments`.
    ///
    /// Carrying the whole table is what lets an accepted expand proposal
    /// instantiate a template without a mid-run history query, and what makes
    /// the store's `materialise_run_nodes` a verbatim writer rather than a
    /// second tier resolver (`06-phase2-plan.md` §4 D9).
    pub assignments: BTreeMap<NodeKey, NodeAssignment>,
    /// Index equals [`RunNodeIdx`].
    pub nodes: Vec<RunNode>,
    pub edges: Vec<RunEdge>,
    pub status: RunStatus,
    /// Journal cursor.
    pub seq: u64,
    /// The engine-owned summariser, appended by `finish` once the user graph's
    /// terminal status is decided and `None` until then (and forever, for a
    /// cancelled run or a run with `summary_enabled: false`).
    ///
    /// Deliberately not a node like any other: `RunGraph.nodes` holds its
    /// `RunNode` so the DAG view and `run_summary.generated_by` can reach it,
    /// but the run's status is already final when it appears, so `finish` is
    /// never re-entered on its account and the counters exclude it
    /// (`07-phase3-plan.md` §4 D1, D5).
    pub epilogue: Option<EpilogueState>,
}

impl RunGraph {
    pub fn node(&self, idx: RunNodeIdx) -> Option<&RunNode> {
        self.nodes.get(idx.0)
    }

    pub fn node_mut(&mut self, idx: RunNodeIdx) -> Option<&mut RunNode> {
        self.nodes.get_mut(idx.0)
    }

    pub fn index_of(&self, path: &InstancePath) -> Option<RunNodeIdx> {
        self.nodes
            .iter()
            .find(|node| &node.path == path)
            .map(|node| node.idx)
    }

    pub fn node_by_path(&self, path: &InstancePath) -> Option<&RunNode> {
        self.nodes.iter().find(|node| &node.path == path)
    }

    pub fn node_by_pane(&self, pane: &PublicPaneId) -> Option<&RunNode> {
        self.nodes.iter().find(|node| {
            node.binding
                .as_ref()
                .is_some_and(|binding| &binding.pane_id == pane)
        })
    }

    /// Indices of the edges that terminate at `idx`.
    pub fn inbound(&self, idx: RunNodeIdx) -> impl Iterator<Item = usize> + '_ {
        self.edges
            .iter()
            .enumerate()
            .filter(move |(_, edge)| edge.to == idx)
            .map(|(index, _)| index)
    }

    /// Indices of the edges that originate at `idx`.
    pub fn outbound(&self, idx: RunNodeIdx) -> impl Iterator<Item = usize> + '_ {
        self.edges
            .iter()
            .enumerate()
            .filter(move |(_, edge)| edge.from == idx)
            .map(|(index, _)| index)
    }
}

// ── journal vocabulary ──────────────────────────────────────────────────────

/// `run_event.kind`. The DAG view is a projection of this journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEventKind {
    RunStarted,
    RunFinished,
    NodeCreated,
    NodeStarted,
    NodeStatus,
    NodeOutput,
    ToolActivity,
    Plan,
    Usage,
    MessageDelivered,
    Steer,
    Interrupt,
    ExpandProposed,
    ExpandAccepted,
    ExpandRejected,
    GrowthLimited,
    Watchdog,
    Checkpoint,
    Succession,
    Error,
    /// The end-of-run summary landed. Exists for replay and audit; the read path
    /// for summaries is the `run_summary` row, never this payload
    /// (`07-phase3-plan.md` §4 D10).
    Summary,
    /// The projection saw a change in the run's team config: a teammate
    /// appeared, went active, or moved pane (`09-agent-teams-rework.md` §3.4).
    /// Journalled because Claude Code deletes the team config when the lead
    /// session ends, so the `run_member` snapshot is a *current* picture and
    /// this is the only record of how it got there.
    Member,
    /// The projection saw a change in the run's Claude Code task list: a task
    /// was created, claimed, or closed (§3.4). Karvex observes these, it does
    /// not decide them.
    Task,
}

impl RunEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RunStarted => "run_started",
            Self::RunFinished => "run_finished",
            Self::NodeCreated => "node_created",
            Self::NodeStarted => "node_started",
            Self::NodeStatus => "node_status",
            Self::NodeOutput => "node_output",
            Self::ToolActivity => "tool_activity",
            Self::Plan => "plan",
            Self::Usage => "usage",
            Self::MessageDelivered => "message_delivered",
            Self::Steer => "steer",
            Self::Interrupt => "interrupt",
            Self::ExpandProposed => "expand_proposed",
            Self::ExpandAccepted => "expand_accepted",
            Self::ExpandRejected => "expand_rejected",
            Self::GrowthLimited => "growth_limited",
            Self::Watchdog => "watchdog",
            Self::Checkpoint => "checkpoint",
            Self::Succession => "succession",
            Self::Error => "error",
            Self::Summary => "summary",
            Self::Member => "member",
            Self::Task => "task",
        }
    }
}

/// `node_checkpoint.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    Result,
    Partial,
    ArtifactIndex,
}

/// `review_cycle.status`, mirroring the store's ASSERT
/// (`store/migrations/0001_init.surql:261-262`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewCycleStatus {
    Running,
    AwaitingUser,
    Applied,
    Declined,
    Failed,
}

impl ReviewCycleStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::AwaitingUser => "awaiting_user",
            Self::Applied => "applied",
            Self::Declined => "declined",
            Self::Failed => "failed",
        }
    }
}

/// `review_finding.interview_mode`, mirroring the store's ASSERT
/// (`store/migrations/0001_init.surql:278-279`): `"resumed"` is a teammate's
/// own account via `claude --resume … --fork-session`; `"evidence_only"` is an
/// inference over the journal/checkpoints/usage when the source session could
/// not be resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterviewMode {
    Resumed,
    EvidenceOnly,
}

impl InterviewMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resumed => "resumed",
            Self::EvidenceOnly => "evidence_only",
        }
    }
}

// ── engine interface: pure state machine ────────────────────────────────────

/// A durable write the engine wants made. Issued off the critical path: the
/// in-memory [`RunGraph`] stays authoritative during a run.
#[derive(Debug, Clone, PartialEq)]
pub enum StoreWrite {
    RunEvent {
        run: RunId,
        seq: u64,
        kind: RunEventKind,
        path: Option<InstancePath>,
        payload: serde_json::Value,
        /// When the event happened, stamped by the producer.
        ///
        /// Not by the store: `run_event.at` used to be minted by
        /// `DEFAULT time::now()` when the queued write was finally applied, so
        /// every journal-derived timestamp drifted from the live one by the
        /// write-queue latency — unboundedly under backlog. The same
        /// second-clock defect migration `0002` killed for
        /// `workflow_run.started_at`, applied to the journal
        /// (`07-phase3-plan.md` §0.6b, §4 D14).
        at_unix_ms: u64,
    },
    RunStatus {
        run: RunId,
        status: RunStatus,
        /// When the run closed, stamped once by the engine as it closes the run
        /// (`None` for every non-terminal status). The engine is the single
        /// authority: without it the store stamped its own `time::now()` when
        /// the queued write was finally applied, so a restored run's end time
        /// disagreed with the live one by however far the store thread was
        /// behind.
        ended_at_unix_ms: Option<u64>,
    },
    RunNode {
        run: RunId,
        path: InstancePath,
        status: NodeStatus,
        attempt: u8,
        binding: Option<NodeBinding>,
        usage: NodeUsage,
        evidence: Option<Evidence>,
        succession: Option<Succession>,
        started_at_unix_ms: Option<u64>,
        ended_at_unix_ms: Option<u64>,
        /// [`RunNode::restored_from`]. The store resolves it to the source
        /// checkpoint's record id; a checkpoint pruned away between restore and
        /// write decodes back as `None` rather than failing the write.
        restored_from: Option<RestoredRef>,
    },
    /// A node that did not exist when the run started: an expansion child
    /// (`06-phase2-plan.md` §4 D7).
    ///
    /// [`StoreWrite::RunNode`] is find-then-`UPDATE` and errors on a missing
    /// row, so before Phase 2 the only create path was `create_run`. The store
    /// side writes the `run_node` row **and** the `spawned` relation from the
    /// proposing parent in one batch, which is the first writer that table has
    /// ever had.
    ///
    /// Two ordering rules ride on this variant. `commit` emits it before any
    /// [`StoreWrite::RunNode`] update for the same path in the same
    /// `Vec<RunEffect>`, and the app's bounded `pending_writes` queue must
    /// never evict a create — dropping the create while keeping the update
    /// would leave a permanent decode error for that node.
    RunNodeCreated {
        run: RunId,
        /// The kvdag key this instance is cut from; also `spawned.template_key`.
        key: NodeKey,
        path: InstancePath,
        /// [`RunNode::label`] — the proposing node's `--label`, persisted so a
        /// restarted server reads back the name the child actually ran under
        /// rather than the template's.
        label: String,
        /// [`RunNode::inputs`] — the accepted `--input k=v` overrides, persisted
        /// for the same reason.
        inputs: BTreeMap<String, String>,
        /// The proposing node. `None` is not expected for an expansion child
        /// and exists only so the variant can also express a create with no
        /// provenance.
        parent: Option<InstancePath>,
        /// Expansion depth, not topological depth: first-generation children
        /// are 1 and static nodes stay 0 (§4 D13).
        depth: u16,
        status: NodeStatus,
        demand: Demand,
        assignment: Assignment,
        assignment_reason: String,
        attempt: u8,
        /// `spawned.proposal_id` — the audit link back to the `expand_proposed`
        /// journal entry that produced this child.
        proposal_id: String,
    },
    /// An edge that did not exist when the run started: the parent→child
    /// `sequence` edge an accepted proposal adds, or the child's inherited copy
    /// of one of its parent's outbound edges (§4 D4).
    ///
    /// The create-shaped sibling of [`StoreWrite::RunEdge`], which is
    /// find-then-`UPDATE` and errors on a missing row.
    RunEdgeCreated {
        run: RunId,
        from: InstancePath,
        to: InstancePath,
        kind: EdgeKind,
        /// The authored edge this instance copies, addressed by kvdag node keys
        /// so the store can resolve `run_edge.kvdag_edge`. `None` for the
        /// synthetic parent→child `sequence` edge, which has no authored
        /// counterpart — `run_edge.kvdag_edge` is `option<record<kvdag_edge>>`
        /// for exactly this case.
        kvdag_edge: Option<(NodeKey, NodeKey)>,
        condition_result: Option<bool>,
        fired: bool,
    },
    /// One edge's settled firing state. Addressed by its endpoints and kind,
    /// which is the identity the read path (`store::list_run_edges`) reports it
    /// back under.
    RunEdge {
        run: RunId,
        from: InstancePath,
        to: InstancePath,
        kind: EdgeKind,
        condition_result: Option<bool>,
        fired: bool,
    },
    Checkpoint {
        run: RunId,
        path: InstancePath,
        seq: u64,
        kind: CheckpointKind,
        schema_valid: bool,
        payload: serde_json::Value,
        summary: String,
        artifact_paths: Vec<String>,
        digest: String,
    },
    /// The end-of-run summary the epilogue produced, validated against
    /// [`crate::workflow::engine::summary_output_schema`] before it is enqueued.
    ///
    /// `run_summary` is the one never-pruned table: a pruned run leaves its
    /// summary behind, which is exactly the row the run browser most needs
    /// (`03-storage-schema.md` §9, `07-phase3-plan.md` §4 D9).
    RunSummary {
        run: RunId,
        /// The version the run executed. Carried explicitly because the
        /// summary outlives its run row, and `kvdag_version` is what a
        /// per-workflow listing can still filter on afterwards.
        kvdag_version: KvdagVersionId,
        text: String,
        outcome: String,
        highlights: Vec<String>,
        open_gaps: Vec<String>,
        per_node: Vec<SummaryNodeLine>,
        token_estimate: u32,
        /// The `.summary` node that wrote it, resolved by the store to a
        /// `run_node` id. `None` is tolerated — a summary without a resolvable
        /// producer is still a summary.
        generated_by_path: Option<InstancePath>,
    },
    /// A past node's session was revived in a pane
    /// (`--resume --fork-session`, or a reconstructed stand-in).
    ///
    /// A create, so the app's bounded `pending_writes` queue must never evict
    /// it — the later [`StoreWrite::InterrogationUpdate`] addresses this row by
    /// id and would have nothing to update (`07-phase3-plan.md` §3 rule 4).
    InterrogationStarted {
        id: InterrogationId,
        run: RunId,
        /// The **source** node being interrogated. An interrogation is not a
        /// run node itself (§4 D8).
        path: InstancePath,
        source_session_id: String,
        /// `None` until the fork's id is known: it is pre-assignable only if
        /// `--session-id` combines with `--resume --fork-session`, and
        /// otherwise arrives later through the pane's session report (§4 D7).
        forked_session_id: Option<String>,
        transcript_path: Option<String>,
        cwd: String,
        pane_id: PublicPaneId,
        /// `true` for the degraded path: a fresh agent seeded from stored
        /// outputs, which must never be presented as the original session.
        reconstructed: bool,
        /// Which checkpoint the reconstructed seed was built from; `None` for a
        /// real fork.
        seeded_from_seq: Option<u64>,
        note: String,
        started_at_unix_ms: u64,
    },
    /// The two things learned about an interrogation after its record exists:
    /// the forked session id, and the moment its pane went away. One shape for
    /// both, because either can be the only thing that ever changes (§4 D7).
    InterrogationUpdate {
        id: InterrogationId,
        forked_session_id: Option<String>,
        ended_at_unix_ms: Option<u64>,
    },
    /// Closes a run as `failed` with a machine-readable reason on the run row.
    ///
    /// `09-agent-teams-rework.md` §3.3 asks for this shape rather than a new
    /// wire status: the protocol's `WorkflowRunStatus` cannot gain a variant
    /// before the next bump, and a `failure` payload says more than a status
    /// word would anyway. Two kinds use it today — `lead_exited`, when the
    /// lead's pane went away without a `finish`, and `lead_unbound`, when the
    /// bind deadline passed with no team recognised.
    RunFailed {
        run: RunId,
        ended_at_unix_ms: u64,
        failure: serde_json::Value,
    },
    /// The pane karvex launched a run's team lead into, written the moment that
    /// pane exists (`09-agent-teams-rework.md` §3.1a).
    ///
    /// Separate from, and always earlier than, [`StoreWrite::RunLeadBinding`],
    /// because the two facts are known at different times and one of them is
    /// not an inference at all: karvex *launched* this lead, so "a Claude Code
    /// team lead is executing this run" is true from the first instant, while
    /// the team name has to wait for the lead to say what its session id is.
    /// Without this write the run row says nothing about its execution model
    /// until binding, and every surface that asks — the DAG's verb set most
    /// visibly — treats the unbound window as an engine-era run and offers
    /// verbs the server then refuses.
    RunLeadPane {
        run: RunId,
        lead_pane_id: String,
        lead_terminal_id: String,
        /// Which revision of the render contract (§3.2) produced the prompt
        /// this lead was launched with. Written here rather than only at
        /// binding so a run that never binds still records what it was given.
        lead_prompt_version: u32,
    },
    /// The run's lead binding, learned once the spawned `claude` pane registers
    /// its session (`09-agent-teams-rework.md` §3.1 step 4).
    ///
    /// Not part of `create_run`: the run row exists before the pane does, and
    /// the session id only appears in `~/.claude/sessions/` after the lead has
    /// started. An `UPDATE`, and idempotent — the projection may re-learn the
    /// same binding after a server restart.
    RunLeadBinding {
        run: RunId,
        lead_session_id: String,
        team_name: String,
        lead_pane_id: Option<String>,
        lead_terminal_id: Option<String>,
        /// Which revision of the render contract (§3.2) produced the prompt
        /// this lead was launched with, so a resume renders the same contract
        /// rather than whatever karvex ships by then.
        lead_prompt_version: u32,
    },
    /// One projected Claude Code task (§3.4). Upsert-shaped on
    /// `(run, instance_path)`: the projection re-observes the same task every
    /// poll and must not accumulate rows.
    ///
    /// A *planned* task lands on a `run_node` row `create_run` already
    /// materialised, so it is always an `UPDATE`. Only an *emergent* task
    /// creates, and only into the reserved namespace — the store asserts that,
    /// the same way [`StoreWrite::RunNodeCreated`]'s epilogue branch does,
    /// because a create here binds `kvdag_node = NONE`.
    RunTaskProjected {
        run: RunId,
        /// `<node key>` for a planned task, or the reserved `.task/<task id>`
        /// namespace for an emergent one.
        path: InstancePath,
        node_key: NodeKey,
        /// The id of the file under `~/.claude/tasks/<team>/`, e.g. `"7"`.
        task_id: String,
        /// The observed subject, verbatim. The lead may reword it; the
        /// definition's own name stays in `label`.
        subject: String,
        /// The claiming teammate's name, or empty for an unclaimed task —
        /// which is a real state in the source data, not a missing value.
        owner: String,
        status: NodeStatus,
        /// A task the definition never planned.
        emergent: bool,
        /// `blockedBy`, as the instance paths of the blocking tasks.
        blocked_by: Vec<InstancePath>,
        /// When the projection read this, and the only clock it has for a
        /// projected task: the source files carry no timestamps.
        observed_at_unix_ms: u64,
    },
    /// One member of the run's team, snapshotted on every observed change
    /// because Claude Code deletes the team config when the lead session ends
    /// (§3.4, and the §4 risk row).
    ///
    /// Upsert-shaped on `(run, name)`, like [`StoreWrite::RunTaskProjected`]
    /// and for the same reason: this arrives once per poll for every member.
    RunMemberSnapshot {
        run: RunId,
        name: String,
        agent_type: String,
        model: String,
        /// The team config's `tmuxPaneId`, which is a karvex public pane id —
        /// karvex's own identifier handed back to it through Claude Code's
        /// team state (§1). `None` for the in-process lead, which has no pane
        /// of its own here.
        pane_id: Option<String>,
        backend_type: String,
        is_active: bool,
        cwd: Option<String>,
        /// The member's own claude session id, learned the same way karvex
        /// already learns it for every tmux teammate today: the bundled
        /// `SessionStart` hook's report, joined on `tmuxPaneId`
        /// (`phase4-retarget-plan.md` S1). `None` until that report lands.
        session_id: Option<String>,
        /// Derived alongside `session_id`, never read from the dead
        /// `~/.claude/sessions/<pid>.json` registry fallback for a teammate
        /// (S1: that registry is lead-only). `None` until the first turn
        /// writes the transcript file.
        transcript_path: Option<String>,
        /// The last observed pane agent state, so a finished run can still say
        /// how long a teammate sat idle while its task stayed `in_progress`
        /// (§3.7, `run_member.last_state`). Free text, not a closed
        /// vocabulary: it mirrors whatever the pane's own detection reported.
        last_state: Option<String>,
        /// When `last_state` was last observed to change.
        last_state_at_unix_ms: Option<u64>,
        observed_at_unix_ms: u64,
    },
    /// The watchdog's current opinion about one node, written independently of
    /// [`StoreWrite::RunNode`] because [`Attention`] is karvex's own column,
    /// never a value the projected `run_node.status` takes on
    /// (`phase4-retarget-plan.md` D-10). `None` clears a prior attention once
    /// the watchdog sees the node moving again — this is a re-evaluation on
    /// every tick, not a one-way escalation.
    RunNodeAttention {
        run: RunId,
        path: InstancePath,
        attention: Option<Attention>,
        observed_at_unix_ms: u64,
    },
    /// A past run's self-improvement review cycle began.
    ///
    /// A create, so the app's bounded `pending_writes` queue must never evict
    /// it — [`StoreWrite::ReviewCycleUpdate`] and [`StoreWrite::ReviewFindings`]
    /// address this row by id and would have nothing to update
    /// (`07-phase3-plan.md` §3 rule 4, the same rule
    /// [`StoreWrite::InterrogationStarted`] follows).
    ReviewCycleStarted {
        id: ReviewCycleId,
        run: RunId,
        kvdag_version: KvdagVersionId,
        started_at_unix_ms: u64,
    },
    /// What changes on a review cycle after it starts: its status, when it
    /// ended, and the kvdag version an accepted change produced. One shape for
    /// all three, because any of them can be the only thing that ever changes
    /// (the same reasoning [`StoreWrite::InterrogationUpdate`] uses) — `None`
    /// leaves the corresponding column untouched rather than clearing it.
    ReviewCycleUpdate {
        id: ReviewCycleId,
        status: Option<ReviewCycleStatus>,
        ended_at_unix_ms: Option<u64>,
        resulting_version: Option<KvdagVersionId>,
    },
    /// One review cycle's findings, written together once its interviews (or
    /// their `evidence_only` fallbacks) are in.
    ReviewFindings {
        cycle: ReviewCycleId,
        findings: Vec<ReviewFindingSeed>,
    },
}

/// One finding as it is written to the store: the pure-layer shape of a
/// `review_finding` row, minus the ids the store resolves at write time
/// (`review_finding.cycle` from [`StoreWrite::ReviewFindings::cycle`], its own
/// row id from the `CREATE`).
///
/// `level` and `verdict` mirror `review_finding`'s own ASSERTed vocabulary
/// (`"prompt"`/`"structural"` and `"keep"`/`"improve"`/`"replace"`,
/// `store/migrations/0001_init.surql:280-283`) but are left untyped here: the
/// pure review core that actually produces these values, and therefore owns
/// naming them, is `workflow::review` (P5), not this packet.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewFindingSeed {
    pub node_key: NodeKey,
    /// The run node this finding is about, when it resolves to one. `None` is
    /// tolerated the way `review_finding.run_node` allows it.
    pub run_node: Option<InstancePath>,
    /// The interview this finding came out of. `None` only when
    /// `interview_mode` is [`InterviewMode::EvidenceOnly`].
    pub interview: Option<InterrogationId>,
    pub interview_mode: InterviewMode,
    pub level: String,
    pub verdict: String,
    pub rationale: String,
    /// Measured, not asserted: attempts, watchdog interventions, tokens, tool
    /// uses, duration, downstream rework, schema failures — the free-form
    /// object `review_finding.evidence` holds.
    pub evidence: serde_json::Value,
    /// The concrete change: prompt rewrite, or a node/edge delta.
    pub proposed_change: serde_json::Value,
    /// Mandatory when `verdict = "replace"`: a full replacement role
    /// definition. The store enforces the pairing
    /// (`store/migrations/0001_init.surql:306-308`); this layer only carries
    /// what the review core produced.
    pub replacement: Option<serde_json::Value>,
}

/// One node's line in a run summary: what the summariser concluded about it.
///
/// `node_key` is a `String`, not a [`NodeKey`], on purpose: it is free text an
/// agent wrote, and typing it as a key would imply a validation that never
/// happens. Renderers match it against real nodes best-effort and fall back to
/// printing it verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryNodeLine {
    pub node_key: String,
    pub verdict: String,
    pub one_liner: String,
}

/// The engine-side event; `src/app/api/workflows.rs` converts it to the wire
/// event, which is declared separately and independently in `src/api/schema`.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowEvent {
    RunStarted {
        run: RunId,
    },
    RunUpdated {
        run: RunId,
        status: RunStatus,
    },
    RunFinished {
        run: RunId,
        status: RunStatus,
    },
    NodeCreated {
        run: RunId,
        path: InstancePath,
    },
    NodeUpdated {
        run: RunId,
        path: InstancePath,
        status: NodeStatus,
    },
    NodeOutputCheckpoint {
        run: RunId,
        path: InstancePath,
        seq: u64,
        summary: String,
    },
    /// The epilogue's summary was accepted and enqueued.
    ///
    /// Carries the run and nothing else, deliberately: the app-side emitter
    /// re-reads the stored summary and publishes the full
    /// `workflow.run.summarized` from that one row, exactly as
    /// [`WorkflowEvent::NodeUpdated`] works today. A `text_len`/`outcome` pair
    /// rode here at first and was never read — the emitter always preferred the
    /// durable row — and two descriptions of one summary is how the event and
    /// the row drift apart. What the summariser actually wrote is journalled
    /// under [`RunEventKind::Summary`] for replay and audit. There is no second
    /// `RunFinished` and no `RunUpdated`: the run's status was final before the
    /// summariser started (`07-phase3-plan.md` §4 D1).
    RunSummarized {
        run: RunId,
    },
    /// The watchdog's opinion about one node changed (§3.7,
    /// `workflow.node.watchdog`). Carries the new value rather than nothing,
    /// unlike [`WorkflowEvent::RunSummarized`]'s re-read pattern, because
    /// [`Attention`] has no durable row of its own for the emitter to re-read
    /// — it lives on `run_node.attention` alongside the fields
    /// [`WorkflowEvent::NodeUpdated`] already re-reads from.
    NodeWatchdog {
        run: RunId,
        path: InstancePath,
        attention: Option<Attention>,
    },
    /// A self-improvement review cycle began (`workflow.review.started`).
    ReviewStarted {
        run: RunId,
        cycle: ReviewCycleId,
    },
    /// A review cycle's interviews (or their `evidence_only` fallbacks) are in
    /// and its findings are ready for the user (`workflow.review.ready`).
    ReviewReady {
        run: RunId,
        cycle: ReviewCycleId,
    },
    /// A review cycle reached a terminal status: `applied`, `declined`, or
    /// `failed` (`workflow.review.closed`).
    ReviewClosed {
        run: RunId,
        cycle: ReviewCycleId,
        status: ReviewCycleStatus,
    },
    // No interrogation variants. Step 1a landed `InterrogationStarted`/
    // `InterrogationEnded` here because §1 WS-A read as though the app-side
    // emitter would re-read an interrogation projection the way it re-reads a
    // node's — but that premise is false against the tree, so both were removed
    // as unconstructable vocabulary.
    //
    // An interrogation is not a run node anywhere (§4 D8): it has no `RunGraph`
    // entry and no engine state at all, so the engine can never produce such an
    // effect and `emit_workflow_event` would have nothing to re-read. Nor could
    // the variants carry the projection themselves — `WorkflowInterrogationInfo`
    // needs the session ids, pane, cwd, transcript path and both stamps, and
    // widening these to hold it would make this module depend on
    // `api::schema`, which the pure-layer rule forbids. The app therefore emits
    // both envelopes directly at its two call sites through one shared mapper.
    //
    // Leaving them would have been an invitation: the natural reading of a dead
    // variant is "wire this up", and the two ways to do that are duplicate
    // emission or a dependency this layer must not have.
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

/// Something the user has to see: a toast or the run banner in the DAG view.
#[derive(Debug, Clone, PartialEq)]
pub struct UserNotice {
    pub level: NoticeLevel,
    pub run: Option<RunId>,
    pub path: Option<InstancePath>,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(required: &[&str]) -> OutputSchema {
        let required: Vec<serde_json::Value> = required
            .iter()
            .map(|name| serde_json::Value::String((*name).to_string()))
            .collect();
        OutputSchema(serde_json::json!({
            "type": "object",
            "required": required,
        }))
    }

    fn node(key: &str, prompt: &str) -> KvdagNode {
        KvdagNode {
            key: NodeKey::new(key),
            label: key.to_string(),
            role: String::new(),
            kind: NodeKind::Agent,
            demand: Demand::Standard,
            runner: Runner::Agent,
            command: None,
            prompt_template: prompt.to_string(),
            system_contract: None,
            output_schema: schema(&["report"]),
            max_attempts: 2,
            timeout_ms: None,
            isolation: Isolation::None,
            is_template: false,
            expand_allow: Vec::new(),
            expand_max: 0,
        }
    }

    fn edge(from: &str, to: &str, port: Option<&str>) -> KvdagEdge {
        KvdagEdge {
            from: NodeKey::new(from),
            to: NodeKey::new(to),
            kind: if port.is_some() {
                EdgeKind::Data
            } else {
                EdgeKind::Sequence
            },
            condition: None,
            payload: EdgePayload::Summary,
            port: port.map(str::to_string),
        }
    }

    fn spec(nodes: Vec<KvdagNode>, edges: Vec<KvdagEdge>) -> KvdagSpec {
        KvdagSpec {
            version_id: KvdagVersionId::new("kvdag_version:1"),
            workflow_id: WorkflowId::new("workflow:1"),
            version: 1,
            parent: None,
            contract: "reply only through result.json".to_string(),
            growth: GrowthLimits::default(),
            args: vec![ArgSpec {
                name: "goal".to_string(),
                required: true,
                default: None,
                description: "what to build".to_string(),
            }],
            nodes,
            edges,
        }
    }

    /// plan → {left, right} → join
    fn diamond() -> KvdagSpec {
        spec(
            vec![
                node("plan", "Plan for: {{goal}}"),
                node("left", "Left half of {{plan}}"),
                node("right", "Right half of {{plan}}"),
                node("join", "Merge {{left_out}} and {{right_out}}"),
            ],
            vec![
                edge("plan", "left", Some("plan")),
                edge("plan", "right", Some("plan")),
                edge("left", "join", Some("left_out")),
                edge("right", "join", Some("right_out")),
            ],
        )
    }

    #[test]
    fn valid_diamond_is_accepted_and_topologically_sorted() {
        let kvdag = Kvdag::try_new(diamond()).expect("diamond is valid");
        let order: Vec<&str> = kvdag.nodes.iter().map(|node| node.key.as_str()).collect();
        assert_eq!(order.first(), Some(&"plan"));
        assert_eq!(order.last(), Some(&"join"));
        let left = order.iter().position(|key| *key == "left").expect("left");
        let join = order.iter().position(|key| *key == "join").expect("join");
        assert!(left < join);
        assert_eq!(kvdag.spec_digest.as_str().len(), 64);
        assert_eq!(kvdag.roots().count(), 1);
    }

    /// §3 rule 3 / §6 A3: the `.` namespace is karvex's from Phase 3 on, so the
    /// engine's `.summary` epilogue node can never collide with an authored one
    /// — in a restore selector, in a counter, or on disk. Only new definitions
    /// are checked; stored versions are read back unvalidated.
    #[test]
    fn a_node_key_in_the_reserved_namespace_is_rejected() {
        let mut spec = diamond();
        spec.nodes.push(node(".summary", "Summarise {{goal}}"));
        assert_eq!(
            Kvdag::try_new(spec),
            Err(KvdagError::ReservedNodeKey(NodeKey::new(".summary")))
        );

        // The rule is the prefix, not the one reserved path.
        let mut spec = diamond();
        spec.nodes
            .push(node(".anything", "Anything about {{goal}}"));
        assert_eq!(
            Kvdag::try_new(spec),
            Err(KvdagError::ReservedNodeKey(NodeKey::new(".anything")))
        );

        // A dot anywhere but the front is still an ordinary key.
        let mut spec = diamond();
        spec.nodes
            .push(node("plan.v2", "More planning for {{goal}}"));
        spec.edges.push(edge("plan", "plan.v2", None));
        assert!(Kvdag::try_new(spec).is_ok());
    }

    /// The engine's reserved path is inside the namespace the check defends, so
    /// the two constants can never drift apart.
    #[test]
    fn the_summary_path_is_reserved() {
        assert!(is_reserved_path(SUMMARY_INSTANCE_PATH));
        assert!(!is_reserved_path("plan"));
        assert!(!is_reserved_path("research/2/verify"));
    }

    /// §4 D5: `max_nodes` governs what the *run's* graph may grow into, and the
    /// epilogue is karvex's node, not the run's. Counting it would spend the
    /// author's budget on a node they never declared and cannot remove.
    #[test]
    fn the_growth_budget_does_not_count_engine_owned_nodes() {
        let authored = |path: &str| RunNode {
            idx: RunNodeIdx(0),
            key: NodeKey::new(path),
            path: InstancePath::new(path),
            label: String::new(),
            inputs: BTreeMap::new(),
            parent: None,
            depth: 0,
            status: NodeStatus::Succeeded,
            assignment: Assignment {
                model: ModelAlias::Sonnet,
                effort: Effort::Low,
            },
            assignment_reason: String::new(),
            attempt: 1,
            binding: None,
            result: None,
            usage: NodeUsage::default(),
            started_at_unix_ms: None,
            ended_at_unix_ms: None,
            progress: ProgressTracker::default(),
            succession: None,
            checkpoint_seq: 0,
            restored_from: None,
        };
        let nodes = vec![
            authored("plan"),
            authored("implement"),
            authored(SUMMARY_INSTANCE_PATH),
        ];

        assert_eq!(
            GrowthLimits::live_node_count(&nodes),
            2,
            "the summariser is not one of the run's declared nodes"
        );
        let limits = GrowthLimits {
            max_depth: 3,
            max_nodes: 3,
        };
        assert!(
            limits.has_node_budget(&nodes),
            "a run at its ceiling is not pushed over it by its own epilogue"
        );
    }

    /// `canonical` is the single canonicaliser both the result digest and the
    /// cross-version restore digests read (§3 rule 5), so key order in the input
    /// must never reach the output.
    #[test]
    fn canonical_sorts_object_keys_at_every_depth() {
        let left = serde_json::json!({
            "b": 1,
            "a": { "z": [ { "y": 2, "x": 3 } ], "w": 4 },
        });
        let right = serde_json::json!({
            "a": { "w": 4, "z": [ { "x": 3, "y": 2 } ] },
            "b": 1,
        });
        assert_eq!(canonical(&left).to_string(), canonical(&right).to_string());
        assert_eq!(
            canonical(&left).to_string(),
            r#"{"a":{"w":4,"z":[{"x":3,"y":2}]},"b":1}"#
        );
    }

    #[test]
    fn spec_digest_ignores_authoring_order() {
        let first = Kvdag::try_new(diamond()).expect("diamond is valid");
        let mut shuffled = diamond();
        shuffled.nodes.reverse();
        shuffled.edges.reverse();
        let second = Kvdag::try_new(shuffled).expect("diamond is valid");
        assert_eq!(first.spec_digest, second.spec_digest);
        assert_eq!(
            first
                .nodes
                .iter()
                .map(|n| n.key.clone())
                .collect::<Vec<_>>(),
            second
                .nodes
                .iter()
                .map(|n| n.key.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn spec_digest_changes_with_the_graph() {
        let first = Kvdag::try_new(diamond()).expect("diamond is valid");
        let mut changed = diamond();
        changed.nodes[1].prompt_template = "Other half of {{plan}}".to_string();
        let second = Kvdag::try_new(changed).expect("diamond is valid");
        assert_ne!(first.spec_digest, second.spec_digest);
    }

    #[test]
    fn cycle_is_rejected() {
        let mut spec = diamond();
        spec.edges.push(edge("join", "plan", None));
        match Kvdag::try_new(spec) {
            Err(KvdagError::Cycle(keys)) => {
                assert!(keys.contains(&NodeKey::new("plan")));
                assert!(keys.contains(&NodeKey::new("join")));
            }
            other => panic!("expected a cycle rejection, got {other:?}"),
        }
    }

    #[test]
    fn self_edge_is_rejected() {
        let mut spec = diamond();
        spec.edges.push(edge("left", "left", None));
        assert_eq!(
            Kvdag::try_new(spec),
            Err(KvdagError::SelfEdge(NodeKey::new("left")))
        );
    }

    #[test]
    fn unknown_edge_endpoint_is_rejected() {
        let mut spec = diamond();
        spec.edges.push(edge("plan", "ship", None));
        match Kvdag::try_new(spec) {
            Err(KvdagError::UnknownEdgeEndpoint { missing, .. }) => {
                assert_eq!(missing, NodeKey::new("ship"));
            }
            other => panic!("expected an unknown endpoint rejection, got {other:?}"),
        }
    }

    #[test]
    fn placeholder_without_a_matching_port_is_rejected() {
        let spec = spec(
            vec![
                node("plan", "Plan for: {{goal}}"),
                node("implement", "Implement {{plan}}"),
            ],
            vec![edge("plan", "implement", Some("blueprint"))],
        );
        match Kvdag::try_new(spec) {
            Err(KvdagError::UnresolvedPlaceholder { node, name }) => {
                assert_eq!(node, NodeKey::new("implement"));
                assert_eq!(name, "plan");
            }
            other => panic!("expected an unresolved placeholder, got {other:?}"),
        }
    }

    #[test]
    fn placeholder_resolving_to_a_declared_arg_is_accepted() {
        let spec = spec(
            vec![node("plan", "Plan for: {{ goal }} and {{goal}}")],
            Vec::new(),
        );
        assert!(Kvdag::try_new(spec).is_ok());
    }

    #[test]
    fn placeholder_without_a_declared_arg_is_rejected() {
        let mut spec = spec(vec![node("plan", "Plan for: {{mission}}")], Vec::new());
        spec.args.clear();
        match Kvdag::try_new(spec) {
            Err(KvdagError::UnresolvedPlaceholder { name, .. }) => assert_eq!(name, "mission"),
            other => panic!("expected an unresolved placeholder, got {other:?}"),
        }
    }

    #[test]
    fn malformed_placeholder_is_rejected() {
        let spec = spec(vec![node("plan", "Plan for: {{goal")], Vec::new());
        match Kvdag::try_new(spec) {
            Err(KvdagError::MalformedPlaceholder { error, .. }) => {
                assert_eq!(error, PlaceholderError::Unclosed { at: 10 });
            }
            other => panic!("expected a malformed placeholder, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_inbound_port_is_rejected() {
        let mut spec = diamond();
        spec.edges.push(edge("right", "join", Some("left_out")));
        match Kvdag::try_new(spec) {
            Err(KvdagError::DuplicatePort { node, port }) => {
                assert_eq!(node, NodeKey::new("join"));
                assert_eq!(port, "left_out");
            }
            other => panic!("expected a duplicate port rejection, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_node_key_is_rejected() {
        let mut spec = diamond();
        spec.nodes.push(node("plan", "Plan for: {{goal}}"));
        assert_eq!(
            Kvdag::try_new(spec),
            Err(KvdagError::DuplicateNodeKey(NodeKey::new("plan")))
        );
    }

    #[test]
    fn invalid_output_schema_is_rejected() {
        let mut spec = diamond();
        spec.nodes[0].output_schema = OutputSchema(serde_json::json!({"type": "objekt"}));
        match Kvdag::try_new(spec) {
            Err(KvdagError::InvalidOutputSchema { node, error }) => {
                assert_eq!(node, NodeKey::new("plan"));
                assert!(error.message.contains("objekt"), "{error}");
            }
            other => panic!("expected an invalid schema rejection, got {other:?}"),
        }
    }

    #[test]
    fn non_object_output_schema_is_rejected() {
        let mut spec = diamond();
        spec.nodes[0].output_schema = OutputSchema(serde_json::json!("object"));
        assert!(matches!(
            Kvdag::try_new(spec),
            Err(KvdagError::InvalidOutputSchema { .. })
        ));
    }

    #[test]
    fn nested_output_schema_is_validated() {
        let value = serde_json::json!({
            "type": "object",
            "properties": { "plan": { "type": "strong" } },
        });
        let error = OutputSchema::parse(value).expect_err("nested type is invalid");
        assert_eq!(error.at, "plan");
    }

    #[test]
    fn output_schema_required_fields_are_reported() {
        let schema = schema(&["plan", "report"]);
        assert_eq!(schema.required_fields(), vec!["plan", "report"]);
    }

    #[test]
    fn command_runner_requires_argv() {
        let mut spec = diamond();
        spec.nodes[1].runner = Runner::Command;
        assert_eq!(
            Kvdag::try_new(spec),
            Err(KvdagError::MissingCommand(NodeKey::new("left")))
        );
    }

    #[test]
    fn agent_runner_rejects_argv() {
        let mut spec = diamond();
        spec.nodes[1].command = Some(vec!["true".to_string()]);
        assert_eq!(
            Kvdag::try_new(spec),
            Err(KvdagError::UnexpectedCommand(NodeKey::new("left")))
        );
    }

    #[test]
    fn command_runner_with_argv_is_accepted() {
        let mut spec = diamond();
        spec.nodes[1].runner = Runner::Command;
        spec.nodes[1].command = Some(vec!["bash".to_string(), "run.sh".to_string()]);
        assert!(Kvdag::try_new(spec).is_ok());
    }

    #[test]
    fn expand_allow_must_reference_a_template() {
        let mut spec = diamond();
        spec.nodes[0].expand_allow = vec![NodeKey::new("left")];
        match Kvdag::try_new(spec) {
            Err(KvdagError::ExpandTargetNotTemplate { node, template }) => {
                assert_eq!(node, NodeKey::new("plan"));
                assert_eq!(template, NodeKey::new("left"));
            }
            other => panic!("expected a template rejection, got {other:?}"),
        }
    }

    #[test]
    fn expand_allow_must_reference_a_known_node() {
        let mut spec = diamond();
        spec.nodes[0].expand_allow = vec![NodeKey::new("verify")];
        assert!(matches!(
            Kvdag::try_new(spec),
            Err(KvdagError::UnknownExpandTemplate { .. })
        ));
    }

    #[test]
    fn template_nodes_are_not_roots_and_need_not_be_reachable() {
        let mut template = node("verify", "Verify {{goal}}");
        template.is_template = true;
        let mut spec = diamond();
        spec.nodes.push(template);
        spec.nodes[0].expand_allow = vec![NodeKey::new("verify")];
        spec.nodes[0].expand_max = 4;
        let kvdag = Kvdag::try_new(spec).expect("templates are allowed to float");
        assert_eq!(kvdag.roots().count(), 1);
    }

    #[test]
    fn unreachable_node_is_rejected() {
        let mut template = node("verify", "Verify {{goal}}");
        template.is_template = true;
        let mut spec = diamond();
        spec.nodes.push(template);
        spec.nodes.push(node("report", "Report {{verified}}"));
        spec.edges.push(edge("verify", "report", Some("verified")));
        spec.nodes[0].expand_allow = vec![NodeKey::new("verify")];
        assert_eq!(
            Kvdag::try_new(spec),
            Err(KvdagError::UnreachableNode(NodeKey::new("report")))
        );
    }

    /// A template is not materialised at run start and neither are its edges,
    /// so a node whose only path from a root runs through one would be a root
    /// of the *materialised* graph and execute at t=0 with nothing to consume.
    /// §3.4's fan-in point is the proposing parent's own outbound edge.
    #[test]
    fn a_node_reachable_only_through_a_template_is_rejected() {
        let mut template = node("worker", "Work on {{plan}}");
        template.is_template = true;
        let mut spec = spec(
            vec![
                node("fanout", "Plan for: {{goal}}"),
                template,
                node("collect", "Collect {{worker_out}}"),
            ],
            vec![
                edge("fanout", "worker", Some("plan")),
                edge("worker", "collect", Some("worker_out")),
            ],
        );
        spec.nodes[0].expand_allow = vec![NodeKey::new("worker")];
        spec.nodes[0].expand_max = 4;
        assert_eq!(
            Kvdag::try_new(spec.clone()),
            Err(KvdagError::UnreachableNode(NodeKey::new("collect")))
        );

        // Drawing the fan-in from the expanding parent makes it valid again.
        spec.edges.push(edge("fanout", "collect", None));
        assert!(Kvdag::try_new(spec).is_ok());
    }

    #[test]
    fn graph_without_a_root_is_rejected() {
        let mut spec = spec(
            vec![node("plan", "Plan {{goal}}"), node("ship", "Ship {{plan}}")],
            vec![
                edge("plan", "ship", Some("plan")),
                edge("ship", "plan", Some("goal")),
            ],
        );
        spec.nodes[0].prompt_template = "Plan {{goal}}".to_string();
        // The cycle is reported first; drop it and the graph still has no root.
        assert!(matches!(
            Kvdag::try_new(spec),
            Err(KvdagError::Cycle(_)) | Err(KvdagError::NoRoot)
        ));
    }

    #[test]
    fn empty_graph_is_rejected() {
        assert_eq!(
            Kvdag::try_new(spec(Vec::new(), Vec::new())),
            Err(KvdagError::EmptyGraph)
        );
    }

    #[test]
    fn duplicate_arg_is_rejected() {
        let mut spec = diamond();
        spec.args.push(ArgSpec {
            name: "goal".to_string(),
            required: false,
            default: Some("ship it".to_string()),
            description: String::new(),
        });
        assert_eq!(
            Kvdag::try_new(spec),
            Err(KvdagError::DuplicateArg("goal".to_string()))
        );
    }

    #[test]
    fn placeholders_are_scanned_in_order() {
        assert_eq!(
            scan_placeholders("a {{one}} b {{ two }} c"),
            Ok(vec!["one".to_string(), "two".to_string()])
        );
        assert_eq!(scan_placeholders("no slots"), Ok(Vec::new()));
        assert_eq!(
            scan_placeholders("{{}}"),
            Err(PlaceholderError::InvalidName {
                at: 0,
                text: String::new()
            })
        );
        assert!(matches!(
            scan_placeholders("{{a b}}"),
            Err(PlaceholderError::InvalidName { .. })
        ));
    }

    #[test]
    fn condition_round_trips_through_json() {
        let condition = Condition::All(vec![
            Condition::Exists {
                path: FieldPath("plan".to_string()),
            },
            Condition::Not(Box::new(Condition::Eq {
                path: FieldPath("verdict".to_string()),
                value: JsonScalar::String("reject".to_string()),
            })),
            Condition::Cmp {
                path: FieldPath("score".to_string()),
                op: CmpOp::Ge,
                value: JsonScalar::Int(3),
            },
            Condition::Any(vec![Condition::Always]),
        ]);
        let encoded = serde_json::to_string(&condition).expect("condition serialises");
        let decoded: Condition = serde_json::from_str(&encoded).expect("condition deserialises");
        assert_eq!(decoded, condition);
    }

    #[test]
    fn field_path_segments_skip_empty_parts() {
        let path = FieldPath("review..verdict".to_string());
        assert_eq!(
            path.segments().collect::<Vec<_>>(),
            vec!["review", "verdict"]
        );
    }

    #[test]
    fn node_status_terminality_matches_the_scheduler_contract() {
        assert!(!NodeStatus::Pending.is_terminal());
        assert!(!NodeStatus::NeedsAttention.is_terminal());
        assert!(!NodeStatus::Blocked.is_terminal());
        assert!(NodeStatus::Succeeded.is_terminal());
        assert!(NodeStatus::Skipped.is_terminal());
    }

    #[test]
    fn definition_document_node_defaults_match_the_schema() {
        let node: KvdagNode = serde_json::from_value(serde_json::json!({
            "key": "plan",
            "label": "Plan",
            "prompt_template": "Plan for: {{goal}}",
            "output_schema": { "type": "object" },
        }))
        .expect("a minimal node deserialises");
        assert_eq!(node.kind, NodeKind::Agent);
        assert_eq!(node.runner, Runner::Agent);
        assert_eq!(node.demand, Demand::Standard);
        assert_eq!(node.isolation, Isolation::None);
        assert_eq!(node.max_attempts, 2);
        assert_eq!(node.expand_max, 0);
        assert!(node.expand_allow.is_empty());
        assert!(!node.is_template);
    }

    #[test]
    fn definition_document_edge_defaults_match_the_schema() {
        let edge: KvdagEdge = serde_json::from_value(serde_json::json!({
            "from": "plan",
            "to": "implement",
            "kind": "data",
            "port": "plan",
        }))
        .expect("a minimal edge deserialises");
        assert_eq!(edge.payload, EdgePayload::Summary);
        assert_eq!(edge.condition, None);
    }

    // ── phase 4, packet P1: serde round-trips for the new enums ────────────

    #[test]
    fn attention_round_trips_through_json_and_matches_the_store_assert() {
        let variants = [
            (Attention::Stuck, "\"stuck\""),
            (Attention::BudgetExceeded, "\"budget_exceeded\""),
            (Attention::NeedsInput, "\"needs_input\""),
            (Attention::LeadBlocked, "\"lead_blocked\""),
            (Attention::Unbound, "\"unbound\""),
        ];
        for (value, wire) in variants {
            let encoded = serde_json::to_string(&value).expect("attention serialises");
            assert_eq!(encoded, wire);
            assert_eq!(value.as_str(), wire.trim_matches('"'));
            let decoded: Attention = serde_json::from_str(&encoded).expect("attention parses");
            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn review_cycle_status_round_trips_through_json_and_matches_the_store_assert() {
        let variants = [
            (ReviewCycleStatus::Running, "\"running\""),
            (ReviewCycleStatus::AwaitingUser, "\"awaiting_user\""),
            (ReviewCycleStatus::Applied, "\"applied\""),
            (ReviewCycleStatus::Declined, "\"declined\""),
            (ReviewCycleStatus::Failed, "\"failed\""),
        ];
        for (value, wire) in variants {
            let encoded = serde_json::to_string(&value).expect("review cycle status serialises");
            assert_eq!(encoded, wire);
            assert_eq!(value.as_str(), wire.trim_matches('"'));
            let decoded: ReviewCycleStatus =
                serde_json::from_str(&encoded).expect("review cycle status parses");
            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn interview_mode_round_trips_through_json_and_matches_the_store_assert() {
        let variants = [
            (InterviewMode::Resumed, "\"resumed\""),
            (InterviewMode::EvidenceOnly, "\"evidence_only\""),
        ];
        for (value, wire) in variants {
            let encoded = serde_json::to_string(&value).expect("interview mode serialises");
            assert_eq!(encoded, wire);
            assert_eq!(value.as_str(), wire.trim_matches('"'));
            let decoded: InterviewMode =
                serde_json::from_str(&encoded).expect("interview mode parses");
            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn review_cycle_id_and_attention_are_usable_in_the_new_store_writes() {
        // A construction smoke test, not a behaviour test (P1 ships shape
        // only): every new `StoreWrite` variant and `ReviewFindingSeed` build
        // from the types this packet defines, and `WorkflowEvent`'s new
        // variants carry them too.
        let run = RunId::new("workflow_run:abc");
        let cycle = ReviewCycleId::new("review_cycle:1");
        let path = InstancePath("plan".to_string());

        let attention_write = StoreWrite::RunNodeAttention {
            run: run.clone(),
            path: path.clone(),
            attention: Some(Attention::Stuck),
            observed_at_unix_ms: 1,
        };
        assert!(matches!(
            attention_write,
            StoreWrite::RunNodeAttention { .. }
        ));

        let cycle_started = StoreWrite::ReviewCycleStarted {
            id: cycle.clone(),
            run: run.clone(),
            kvdag_version: KvdagVersionId::new("kvdag_version:1"),
            started_at_unix_ms: 1,
        };
        assert!(matches!(
            cycle_started,
            StoreWrite::ReviewCycleStarted { .. }
        ));

        let cycle_update = StoreWrite::ReviewCycleUpdate {
            id: cycle.clone(),
            status: Some(ReviewCycleStatus::AwaitingUser),
            ended_at_unix_ms: None,
            resulting_version: None,
        };
        assert!(matches!(cycle_update, StoreWrite::ReviewCycleUpdate { .. }));

        let finding = ReviewFindingSeed {
            node_key: NodeKey::new("plan"),
            run_node: Some(path),
            interview: None,
            interview_mode: InterviewMode::EvidenceOnly,
            level: "prompt".to_string(),
            verdict: "keep".to_string(),
            rationale: "no drift observed".to_string(),
            evidence: serde_json::json!({}),
            proposed_change: serde_json::json!({}),
            replacement: None,
        };
        let findings_write = StoreWrite::ReviewFindings {
            cycle: cycle.clone(),
            findings: vec![finding],
        };
        assert!(matches!(findings_write, StoreWrite::ReviewFindings { .. }));

        let watchdog_event = WorkflowEvent::NodeWatchdog {
            run: run.clone(),
            path: InstancePath("plan".to_string()),
            attention: Some(Attention::NeedsInput),
        };
        assert!(matches!(watchdog_event, WorkflowEvent::NodeWatchdog { .. }));

        let review_started = WorkflowEvent::ReviewStarted {
            run: run.clone(),
            cycle: cycle.clone(),
        };
        assert!(matches!(
            review_started,
            WorkflowEvent::ReviewStarted { .. }
        ));

        let review_ready = WorkflowEvent::ReviewReady {
            run: run.clone(),
            cycle: cycle.clone(),
        };
        assert!(matches!(review_ready, WorkflowEvent::ReviewReady { .. }));

        let review_closed = WorkflowEvent::ReviewClosed {
            run,
            cycle,
            status: ReviewCycleStatus::Applied,
        };
        assert!(matches!(review_closed, WorkflowEvent::ReviewClosed { .. }));
    }
}
