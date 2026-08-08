//! Shared fixtures for the engine's unit tests.
//!
//! Pure data builders: every fixture is a plain `RunGraph`/`Kvdag`, so engine
//! behaviour is exercised without a PTY, a store, or an async runtime.

use crate::workflow::model::{
    ArgSpec, Condition, Demand, EdgeKind, EdgePayload, Evidence, GrowthLimits, InstancePath, Kvdag,
    KvdagEdge, KvdagNode, KvdagSpec, KvdagVersionId, NodeKey, NodeResult, NodeStatus, NodeUsage,
    OutputSchema, ProgressTracker, RunEdge, RunGraph, RunId, RunNode, RunNodeIdx, RunStatus,
    Runner, WorkflowId,
};
use crate::workflow::tier::{self, Tier};

pub struct TestNode {
    pub key: String,
    pub demand: Demand,
    pub runner: Runner,
    pub required: Vec<String>,
}

impl TestNode {
    pub fn new(key: &str) -> Self {
        Self {
            key: key.to_string(),
            demand: Demand::Standard,
            runner: Runner::Agent,
            required: Vec::new(),
        }
    }

    pub fn requiring(key: &str, required: &[&str]) -> Self {
        Self {
            required: required.iter().map(|name| (*name).to_string()).collect(),
            ..Self::new(key)
        }
    }
}

pub struct TestEdge {
    pub from: usize,
    pub to: usize,
    pub kind: EdgeKind,
    pub condition: Option<Condition>,
    pub payload: EdgePayload,
    pub port: Option<String>,
}

pub fn edge(from: usize, to: usize, kind: EdgeKind) -> TestEdge {
    TestEdge {
        from,
        to,
        kind,
        condition: None,
        payload: EdgePayload::Summary,
        port: None,
    }
}

impl TestEdge {
    pub fn with_condition(mut self, condition: Condition) -> Self {
        self.condition = Some(condition);
        self
    }

    pub fn with_payload(mut self, payload: EdgePayload) -> Self {
        self.payload = payload;
        self
    }

    pub fn with_port(mut self, port: &str) -> Self {
        self.port = Some(port.to_string());
        self
    }
}

pub fn graph_of(nodes: &[TestNode], edges: &[TestEdge]) -> RunGraph {
    RunGraph {
        run_id: RunId::new("workflow_run:test"),
        version_id: KvdagVersionId::new("kvdag_version:test"),
        tier: Tier::High,
        growth: GrowthLimits::default(),
        assignments: std::collections::BTreeMap::new(),
        nodes: nodes
            .iter()
            .enumerate()
            .map(|(index, node)| RunNode {
                idx: RunNodeIdx(index),
                key: NodeKey::new(node.key.as_str()),
                path: InstancePath::new(node.key.as_str()),
                parent: None,
                depth: 0,
                status: NodeStatus::Pending,
                assignment: tier::resolve(Tier::High, node.demand, None),
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
            })
            .collect(),
        edges: edges
            .iter()
            .map(|edge| RunEdge {
                from: RunNodeIdx(edge.from),
                to: RunNodeIdx(edge.to),
                kind: edge.kind,
                condition: edge.condition.clone(),
                payload: edge.payload,
                port: edge.port.clone(),
                condition_result: None,
                fired: false,
            })
            .collect(),
        status: RunStatus::Pending,
        seq: 0,
    }
}

/// A chain of `Sequence` edges, one per adjacent pair.
pub fn linear(keys: &[&str]) -> RunGraph {
    let nodes: Vec<TestNode> = keys.iter().map(|key| TestNode::new(key)).collect();
    let edges: Vec<TestEdge> = (1..keys.len())
        .map(|index| edge(index - 1, index, EdgeKind::Sequence))
        .collect();
    graph_of(&nodes, &edges)
}

/// `plan → {left, right} → join`, all `Data` edges.
pub fn diamond() -> RunGraph {
    graph_of(
        &[
            TestNode::new("plan"),
            TestNode::new("left"),
            TestNode::new("right"),
            TestNode::new("join"),
        ],
        &[
            edge(0, 1, EdgeKind::Data),
            edge(0, 2, EdgeKind::Data),
            edge(1, 3, EdgeKind::Data),
            edge(2, 3, EdgeKind::Data),
        ],
    )
}

pub fn node_at<'a>(graph: &'a RunGraph, key: &str) -> &'a RunNode {
    graph
        .node_by_path(&InstancePath::new(key))
        .unwrap_or_else(|| panic!("fixture graph has a node named {key}"))
}

/// Marks a node `Succeeded` with a validated result, the way the completion
/// gate would.
pub fn set_result(graph: &mut RunGraph, key: &str, payload: serde_json::Value) {
    let idx = node_at(graph, key).idx;
    let Some(node) = graph.node_mut(idx) else {
        return;
    };
    node.status = NodeStatus::Succeeded;
    node.result = Some(NodeResult {
        summary: String::new(),
        artifact_paths: Vec::new(),
        digest: String::new(),
        evidence: Evidence::SelfReport,
        payload,
    });
}

pub fn spec_node(node: &TestNode) -> KvdagNode {
    let required: Vec<serde_json::Value> = node
        .required
        .iter()
        .map(|name| serde_json::Value::String(name.clone()))
        .collect();
    KvdagNode {
        key: NodeKey::new(node.key.as_str()),
        label: node.key.clone(),
        role: String::new(),
        kind: crate::workflow::model::NodeKind::Agent,
        demand: node.demand,
        runner: node.runner,
        command: match node.runner {
            Runner::Agent => None,
            Runner::Command => Some(vec!["true".to_string()]),
        },
        prompt_template: format!("do {}", node.key),
        system_contract: None,
        output_schema: OutputSchema::parse(serde_json::json!({
            "type": "object",
            "required": required,
        }))
        .unwrap_or_else(|error| panic!("fixture schema parses: {error}")),
        max_attempts: 2,
        timeout_ms: None,
        isolation: crate::workflow::model::Isolation::None,
        is_template: false,
        expand_allow: Vec::new(),
        expand_max: 0,
    }
}

pub fn kvdag_spec(nodes: Vec<KvdagNode>, edges: Vec<KvdagEdge>) -> KvdagSpec {
    KvdagSpec {
        version_id: KvdagVersionId::new("kvdag_version:test"),
        workflow_id: WorkflowId::new("workflow:test"),
        version: 1,
        parent: None,
        contract: "Reply only through result.json.".to_string(),
        growth: GrowthLimits::default(),
        args: vec![ArgSpec {
            name: "goal".to_string(),
            required: false,
            default: None,
            description: String::new(),
        }],
        nodes,
        edges,
    }
}

pub fn kvdag_of(nodes: Vec<KvdagNode>, edges: Vec<KvdagEdge>) -> Kvdag {
    Kvdag::try_new(kvdag_spec(nodes, edges))
        .unwrap_or_else(|error| panic!("fixture kvdag: {error}"))
}

pub fn spec_edge(from: &str, to: &str, kind: EdgeKind) -> KvdagEdge {
    KvdagEdge {
        from: NodeKey::new(from),
        to: NodeKey::new(to),
        kind,
        condition: None,
        payload: EdgePayload::Summary,
        port: None,
    }
}
