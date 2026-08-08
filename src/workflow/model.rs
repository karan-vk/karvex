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

use crate::detect::AgentState;
use crate::terminal::TerminalId;
use crate::workflow::engine::expand::{ExpandLimit, ExpandProposal};
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
);

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
    pub fn live_node_count(nodes: &[RunNode]) -> u16 {
        u16::try_from(nodes.len()).unwrap_or(u16::MAX)
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

/// One observation of material progress. Text output, liveness, and a redrawn
/// screen are deliberately absent: they are not progress.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProgressDelta {
    pub tool_calls: u32,
    pub tokens: u64,
    pub artifact_changes: u32,
    pub screen_digest: Option<String>,
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

// ── engine interface: pure state machine ────────────────────────────────────

/// Parsed but not yet schema-validated node output.
#[derive(Debug, Clone, PartialEq)]
pub struct RawJson(pub serde_json::Value);

/// Everything the binder needs to put a node's process in a pane. Built by the
/// engine, executed by `workflow::binding::spawn`.
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnSpec {
    pub run_id: RunId,
    pub path: InstancePath,
    pub label: String,
    pub runner: Runner,
    /// argv for `Runner::Command`; the `claude` argv is built by the binder for
    /// `Runner::Agent`.
    pub command: Option<Vec<String>>,
    pub assignment: Assignment,
    pub agent_session_id: String,
    pub node_dir: PathBuf,
    pub cwd: PathBuf,
    pub isolation: Isolation,
    /// kvdag contract plus the node's role; passed as `--append-system-prompt`.
    pub contract: String,
    pub seed_prompt: String,
    pub token: NodeToken,
}

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
    /// A growth guardrail refused or truncated a proposal.
    ///
    /// The **only** new wire event in Phase 2 (`06-phase2-plan.md` §4 D5): an
    /// expansion child is already announced by
    /// [`WorkflowEvent::NodeCreated`], and `WorkflowRunNodeInfo` already
    /// carries `parent_path`/`depth`, so a client can derive everything about
    /// an *accepted* proposal. What no client can derive is the node that was
    /// asked for and never created. `src/app/api/workflows.rs` widens the
    /// counts to the wire's `u32`.
    GrowthLimited {
        run: RunId,
        /// The proposing node.
        path: InstancePath,
        template: NodeKey,
        limit: ExpandLimit,
        /// The ceiling's value, so a reader does not have to look it up.
        limit_value: u16,
        requested: u16,
        accepted: u16,
        message: String,
    },
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

#[derive(Debug, Clone, PartialEq)]
pub enum EngineInput {
    Start {
        graph: Box<RunGraph>,
    },
    NodeSelfReport {
        path: InstancePath,
        token: NodeToken,
        result: RawJson,
    },
    /// A node proposed new nodes. **A node cannot create nodes; it proposes,
    /// and karvex decides** (`04-kvdag-and-execution.md` §3.4).
    ///
    /// Token-authenticated exactly like [`EngineInput::NodeSelfReport`] — an
    /// expand proposal is a node speaking, not an operator — and reached by two
    /// routes that converge here: the `workflow.node.expand` API verb, and the
    /// top-level `expand` key lifted out of a node result *before* schema
    /// validation (§4 D6). A rejected proposal is not an error: the run
    /// continues and the rejection is reported.
    ExpandProposed {
        path: InstancePath,
        token: NodeToken,
        proposals: Vec<ExpandProposal>,
    },
    /// The Claude `stop` hook fired for this pane.
    TurnEnded {
        pane: PublicPaneId,
    },
    AgentStatus {
        pane: PublicPaneId,
        state: AgentState,
        at: Instant,
    },
    ProgressObserved {
        path: InstancePath,
        delta: ProgressDelta,
    },
    PaneExited {
        pane: PublicPaneId,
        code: Option<i32>,
    },
    /// The runtime gave up putting an admitted node into a pane. The node never
    /// acquired one, so there is no `PaneExited` to report; without this the
    /// node would sit `Ready` forever and the run would never pause or finish
    /// (`04` §9: every failure path has a node status).
    SpawnFailed {
        path: InstancePath,
        reason: String,
    },
    Steer {
        path: InstancePath,
        text: String,
    },
    Interrupt {
        path: InstancePath,
    },
    RestartNode {
        path: InstancePath,
    },
    CancelRun,
    Tick {
        now: Instant,
    },
}

/// `SpawnSpec` and `StoreWrite` are boxed: both dwarf every other variant, and
/// effects are moved around in `Vec<RunEffect>` on every engine step.
#[derive(Debug, Clone, PartialEq)]
pub enum RunEffect {
    SpawnNode {
        path: InstancePath,
        spec: Box<SpawnSpec>,
    },
    PromptNode {
        pane: PublicPaneId,
        text: String,
    },
    SendKeys {
        pane: PublicPaneId,
        keys: Vec<String>,
    },
    ClosePane {
        pane: PublicPaneId,
    },
    Persist(Box<StoreWrite>),
    Emit(WorkflowEvent),
    Notify(UserNotice),
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
}
