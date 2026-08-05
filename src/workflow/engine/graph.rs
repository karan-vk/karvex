//! Run-graph materialisation and edge resolution.
//!
//! The run-graph *data* (`RunGraph`, `RunNode`, `RunEdge`) lives in
//! `crate::workflow::model` beside the definition types it mirrors; the logic
//! that builds and walks it lives here
//! (`docs/design/workflow-builder/04-kvdag-and-execution.md` §3.1).

use std::collections::HashMap;

use crate::workflow::engine::schedule;
use crate::workflow::model::{
    GrowthLimits, InstancePath, Kvdag, NodeStatus, NodeUsage, ProgressTracker, RunEdge, RunGraph,
    RunId, RunNode, RunNodeIdx, RunStatus,
};
use crate::workflow::tier::{self, Tier};

/// The first attempt is numbered 1, matching `run_node.attempt`'s default in
/// `03-storage-schema.md` §4.2.
const FIRST_ATTEMPT: u8 = 1;

impl RunGraph {
    /// Materialises a run graph from a validated definition: one `RunNode` per
    /// non-template node, one `RunEdge` per edge between two materialised
    /// nodes, each node's assignment resolved from the run's tier and the
    /// node's demand, and the run's growth limits narrowed (never widened) by
    /// the tier.
    ///
    /// Roots are left `Ready` and everything else `Pending`, because
    /// [`schedule::propagate`] runs before the graph is handed back — a run
    /// graph is never observed in a state where its roots have not been
    /// admitted.
    pub fn materialise(kvdag: &Kvdag, run_id: RunId, tier: Tier) -> Self {
        let mut nodes: Vec<RunNode> = Vec::with_capacity(kvdag.nodes.len());
        let mut index_by_key: HashMap<&str, RunNodeIdx> = HashMap::with_capacity(kvdag.nodes.len());

        // Template nodes are never scheduled directly; they only enter a run
        // through an accepted expand proposal (§3.4), so they are not
        // materialised at run start.
        for node in kvdag.nodes.iter().filter(|node| !node.is_template) {
            let idx = RunNodeIdx(nodes.len());
            index_by_key.insert(node.key.as_str(), idx);
            nodes.push(RunNode {
                idx,
                key: node.key.clone(),
                path: InstancePath::new(node.key.as_str()),
                parent: None,
                depth: 0,
                status: NodeStatus::Pending,
                assignment: tier::resolve(tier, node.demand, None),
                attempt: FIRST_ATTEMPT,
                binding: None,
                result: None,
                usage: NodeUsage::default(),
                progress: ProgressTracker::default(),
                succession: None,
                checkpoint_seq: 0,
            });
        }

        let mut edges: Vec<RunEdge> = Vec::with_capacity(kvdag.edges.len());
        for edge in &kvdag.edges {
            let (Some(from), Some(to)) = (
                index_by_key.get(edge.from.as_str()).copied(),
                index_by_key.get(edge.to.as_str()).copied(),
            ) else {
                continue;
            };
            edges.push(RunEdge {
                from,
                to,
                kind: edge.kind,
                condition: edge.condition.clone(),
                payload: edge.payload,
                port: edge.port.clone(),
                condition_result: None,
                fired: false,
            });
        }

        let mut graph = Self {
            run_id,
            version_id: kvdag.version_id.clone(),
            tier,
            growth: narrow_growth(kvdag.growth, tier),
            nodes,
            edges,
            status: RunStatus::Pending,
            seq: 0,
        };
        schedule::propagate(&mut graph);
        graph
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

/// The tier's growth influence is purely a narrowing one (§7.4), which is what
/// keeps `workflow_run.max_nodes <= kvdag_version.max_nodes` a true invariant.
/// `auto` starts from the `high` row (§7.3), so it narrows nothing.
pub fn narrow_growth(growth: GrowthLimits, tier: Tier) -> GrowthLimits {
    let ceiling = match tier {
        Tier::Auto | Tier::Max | Tier::High => None,
        Tier::Medium => Some(24),
        Tier::Low => Some(12),
    };
    GrowthLimits {
        max_depth: growth.max_depth,
        max_nodes: ceiling.map_or(growth.max_nodes, |ceiling| growth.max_nodes.min(ceiling)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::engine::tests_support::{kvdag_of, spec_edge, spec_node, TestNode};
    use crate::workflow::model::{Demand, EdgeKind, NodeKey};
    use crate::workflow::tier::{Effort, ModelAlias};

    fn node<'a>(graph: &'a RunGraph, key: &str) -> &'a crate::workflow::model::RunNode {
        graph
            .node_by_path(&InstancePath::new(key))
            .unwrap_or_else(|| panic!("the graph has a node named {key}"))
    }

    #[test]
    fn materialise_builds_one_node_per_non_template_node_with_roots_ready() {
        let definition = kvdag_of(
            vec![
                spec_node(&TestNode::new("plan")),
                spec_node(&TestNode::new("implement")),
            ],
            vec![spec_edge("plan", "implement", EdgeKind::Sequence)],
        );
        let graph = RunGraph::materialise(&definition, RunId::new("workflow_run:1"), Tier::High);

        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.run_id, RunId::new("workflow_run:1"));
        assert_eq!(graph.version_id, definition.version_id);
        assert_eq!(graph.status, RunStatus::Pending);
        assert_eq!(graph.seq, 0);
        assert_eq!(graph.nodes[0].path, InstancePath::new("plan"));
        assert_eq!(graph.nodes[0].idx, RunNodeIdx(0));
        assert_eq!(graph.nodes[0].attempt, FIRST_ATTEMPT);
        assert_eq!(graph.nodes[0].depth, 0);
        assert!(graph.nodes[0].parent.is_none());
        assert_eq!(graph.nodes[0].status, NodeStatus::Ready);
        assert_eq!(graph.nodes[1].status, NodeStatus::Pending);
    }

    #[test]
    fn materialise_skips_templates_and_the_edges_that_touch_them() {
        let mut template = spec_node(&TestNode::new("worker"));
        template.is_template = true;
        let mut fanout = spec_node(&TestNode::new("fanout"));
        fanout.expand_allow = vec![NodeKey::new("worker")];
        fanout.expand_max = 4;

        let definition = kvdag_of(
            vec![fanout, template, spec_node(&TestNode::new("collect"))],
            vec![
                spec_edge("fanout", "worker", EdgeKind::Sequence),
                spec_edge("worker", "collect", EdgeKind::Sequence),
                spec_edge("fanout", "collect", EdgeKind::Sequence),
            ],
        );
        let graph = RunGraph::materialise(&definition, RunId::new("workflow_run:1"), Tier::High);

        assert_eq!(graph.nodes.len(), 2);
        assert!(graph
            .nodes
            .iter()
            .all(|node| node.key != NodeKey::new("worker")));
        assert_eq!(
            graph.edges.len(),
            1,
            "only the fanout → collect edge has two materialised endpoints"
        );
    }

    #[test]
    fn materialise_resolves_the_assignment_from_tier_and_demand() {
        let mut peak = spec_node(&TestNode::new("peak"));
        peak.demand = Demand::Peak;
        let mut light = spec_node(&TestNode::new("light"));
        light.demand = Demand::Light;

        let definition = kvdag_of(vec![peak, light], Vec::new());
        let graph = RunGraph::materialise(&definition, RunId::new("workflow_run:1"), Tier::High);

        assert_eq!(node(&graph, "peak").assignment.model, ModelAlias::Opus);
        assert_eq!(node(&graph, "peak").assignment.effort, Effort::Xhigh);
        assert_eq!(node(&graph, "light").assignment.model, ModelAlias::Sonnet);
        assert_eq!(node(&graph, "light").assignment.effort, Effort::Medium);
    }

    #[test]
    fn a_tier_narrows_growth_limits_but_never_widens_them() {
        let version = GrowthLimits {
            max_depth: 3,
            max_nodes: 40,
        };
        assert_eq!(narrow_growth(version, Tier::Max).max_nodes, 40);
        assert_eq!(narrow_growth(version, Tier::High).max_nodes, 40);
        assert_eq!(narrow_growth(version, Tier::Auto).max_nodes, 40);
        assert_eq!(narrow_growth(version, Tier::Medium).max_nodes, 24);
        assert_eq!(narrow_growth(version, Tier::Low).max_nodes, 12);

        let narrow = GrowthLimits {
            max_depth: 2,
            max_nodes: 6,
        };
        assert_eq!(
            narrow_growth(narrow, Tier::Low).max_nodes,
            6,
            "a run never widens the version's ceiling"
        );
        assert_eq!(narrow_growth(narrow, Tier::Low).max_depth, 2);
    }

    #[test]
    fn inbound_and_outbound_address_the_right_edges() {
        let definition = kvdag_of(
            vec![
                spec_node(&TestNode::new("a")),
                spec_node(&TestNode::new("b")),
                spec_node(&TestNode::new("c")),
            ],
            vec![
                spec_edge("a", "c", EdgeKind::Sequence),
                spec_edge("b", "c", EdgeKind::Sequence),
            ],
        );
        let graph = RunGraph::materialise(&definition, RunId::new("workflow_run:1"), Tier::High);
        let c = graph
            .index_of(&InstancePath::new("c"))
            .expect("c is materialised");

        assert_eq!(graph.inbound(c).count(), 2);
        assert_eq!(graph.outbound(c).count(), 0);
        assert_eq!(graph.outbound(RunNodeIdx(0)).count(), 1);
    }
}
