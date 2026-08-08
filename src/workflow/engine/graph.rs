//! Run-graph materialisation and edge resolution.
//!
//! The run-graph *data* (`RunGraph`, `RunNode`, `RunEdge`) lives in
//! `crate::workflow::model` beside the definition types it mirrors; the logic
//! that builds and walks it lives here
//! (`docs/design/workflow-builder/04-kvdag-and-execution.md` §3.1).

use std::collections::{BTreeMap, HashMap};

use crate::workflow::engine::schedule;
use crate::workflow::model::{
    GrowthLimits, InstancePath, Kvdag, NodeAssignment, NodeKey, NodeStatus, NodeUsage,
    ProgressTracker, RunEdge, RunGraph, RunId, RunNode, RunNodeIdx, RunStatus,
};
use crate::workflow::tier::{self, HistoryIndex, Tier};

/// The first attempt is numbered 1, matching `run_node.attempt`'s default in
/// `03-storage-schema.md` §4.2.
///
/// `pub(crate)` so an expansion child created mid-run by
/// [`crate::workflow::engine::expand::commit`] starts on the same attempt
/// number a statically materialised node does.
pub(crate) const FIRST_ATTEMPT: u8 = 1;

/// Every kvdag node's `(model, effort, reason)`, resolved once at run start.
///
/// The **single** tier resolver for the whole subsystem (`06-phase2-plan.md`
/// §4 D9). Before Phase 2 the pair was resolved in two places that agreed only
/// because both passed `None` for history; with `auto` reading a node's
/// measured record that coincidence would end, and a node's persisted row would
/// start disagreeing with the DAG about which model it ran on.
///
/// **Templates are included on purpose.** An accepted expand proposal
/// instantiates a template mid-run, and a mid-run history query would resolve
/// against a different `HistoryIndex` than the run started with — the same run
/// would then contain two nodes cut from one template with different
/// assignments and no way to explain the difference. Resolving every node up
/// front, templates included, makes the table a closed, replayable record.
///
/// An absent history entry behaves like an all-zero record, which is what
/// [`tier::resolve`] already documents.
pub fn resolve_assignments(
    kvdag: &Kvdag,
    tier: Tier,
    history: &HistoryIndex,
) -> BTreeMap<NodeKey, NodeAssignment> {
    kvdag
        .nodes
        .iter()
        .map(|node| {
            let measured = history.get(&node.key);
            let assignment = tier::resolve(tier, node.demand, measured);
            // A fixed tier's row *is* the explanation, so it carries no reason
            // string (`NodeAssignment::reason`'s doc comment).
            let reason = match tier {
                Tier::Auto => tier::auto_reason(node.demand, measured).to_string(),
                Tier::Max | Tier::High | Tier::Medium | Tier::Low => String::new(),
            };
            (
                node.key.clone(),
                NodeAssignment::from_assignment(assignment, reason),
            )
        })
        .collect()
}

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
        let assignments = resolve_assignments(kvdag, tier, &HistoryIndex::new());
        Self::materialise_with(kvdag, run_id, tier, &assignments)
    }

    /// [`RunGraph::materialise`] against an assignment table the caller already
    /// resolved — the entry point a run start uses, because only the caller can
    /// reach the store for the workflow's [`HistoryIndex`].
    ///
    /// The table is carried verbatim onto [`RunGraph::assignments`], so
    /// `materialise_run_nodes` persists what the run actually decided instead of
    /// re-deriving it (§4 D9). A node whose key is missing from the table falls
    /// back to a history-free resolution rather than panicking: the table comes
    /// from a query, and a run that lost one row should still start.
    pub fn materialise_with(
        kvdag: &Kvdag,
        run_id: RunId,
        tier: Tier,
        assignments: &BTreeMap<NodeKey, NodeAssignment>,
    ) -> Self {
        let mut nodes: Vec<RunNode> = Vec::with_capacity(kvdag.nodes.len());
        let mut index_by_key: HashMap<&str, RunNodeIdx> = HashMap::with_capacity(kvdag.nodes.len());

        // Template nodes are never scheduled directly; they only enter a run
        // through an accepted expand proposal (§3.4), so they are not
        // materialised at run start.
        for node in kvdag.nodes.iter().filter(|node| !node.is_template) {
            let idx = RunNodeIdx(nodes.len());
            index_by_key.insert(node.key.as_str(), idx);
            let resolved = assignments.get(&node.key).cloned().unwrap_or_else(|| {
                NodeAssignment::from_assignment(
                    tier::resolve(tier, node.demand, None),
                    String::new(),
                )
            });
            nodes.push(RunNode {
                idx,
                key: node.key.clone(),
                path: InstancePath::new(node.key.as_str()),
                // The authored label, carried onto the instance so every
                // renderer reads one field whether the node is static or was
                // proposed mid-run. Empty stays empty: the fallback to the key
                // belongs to the renderer, not to materialisation.
                label: node.label.clone(),
                // A static node is not the product of a proposal, so it has no
                // slot overrides.
                inputs: BTreeMap::new(),
                parent: None,
                depth: 0,
                status: NodeStatus::Pending,
                assignment: resolved.assignment(),
                assignment_reason: resolved.reason,
                attempt: FIRST_ATTEMPT,
                binding: None,
                result: None,
                usage: NodeUsage::default(),
                started_at_unix_ms: None,
                ended_at_unix_ms: None,
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
            assignments: assignments.clone(),
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
///
/// **Idempotent**: `narrow_growth(narrow_growth(g, t), t) == narrow_growth(g, t)`,
/// because every rule is a `min` against a constant. That is what lets the run
/// start narrow once and every later reader re-narrow freely without a run's
/// banner contradicting its own persisted row (`06-phase2-plan.md` §5 R-3).
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
    use crate::workflow::tier::{Effort, ModelAlias, NodeHistory};

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

        // `fanout -> collect` is not redundant with `worker -> collect`: it is
        // the §3.4 fan-in point, drawn from the node that expands the template
        // because an instantiated child inherits *its parent's* outbound edges.
        // Without it `collect` would be reachable only through the template and
        // `Kvdag::try_new` would reject the graph.
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
        assert_eq!(
            node(&graph, "collect").status,
            NodeStatus::Pending,
            "a node that depends on the expanding parent is not a root"
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
    fn narrowing_growth_twice_changes_nothing_the_second_time() {
        let version = GrowthLimits {
            max_depth: 3,
            max_nodes: 40,
        };
        for tier in [Tier::Auto, Tier::Max, Tier::High, Tier::Medium, Tier::Low] {
            let once = narrow_growth(version, tier);
            assert_eq!(
                narrow_growth(once, tier),
                once,
                "{tier} must be idempotent: the run narrows once and every later \
                 reader re-narrows freely"
            );
        }
    }

    #[test]
    fn resolve_assignments_covers_every_node_including_templates() {
        let mut template = spec_node(&TestNode::new("worker"));
        template.is_template = true;
        template.demand = Demand::Light;
        let mut fanout = spec_node(&TestNode::new("fanout"));
        fanout.expand_allow = vec![NodeKey::new("worker")];
        fanout.expand_max = 2;
        fanout.demand = Demand::Peak;

        let definition = kvdag_of(
            vec![fanout, template, spec_node(&TestNode::new("collect"))],
            vec![
                spec_edge("fanout", "worker", EdgeKind::Sequence),
                spec_edge("fanout", "collect", EdgeKind::Sequence),
            ],
        );
        let table = resolve_assignments(&definition, Tier::High, &HistoryIndex::new());

        assert_eq!(
            table.len(),
            3,
            "an expansion child must never need a mid-run lookup"
        );
        let worker = table
            .get(&NodeKey::new("worker"))
            .expect("the template is resolved at run start");
        assert_eq!(worker.model, ModelAlias::Sonnet);
        assert_eq!(worker.effort, Effort::Medium);
        assert_eq!(
            worker.reason,
            String::new(),
            "a fixed tier's table row is its own explanation"
        );
    }

    #[test]
    fn resolve_assignments_matches_the_tier_tables_for_every_tier_and_demand() {
        let demands = [
            Demand::Peak,
            Demand::Critical,
            Demand::Standard,
            Demand::Light,
        ];
        let nodes: Vec<crate::workflow::model::KvdagNode> = demands
            .iter()
            .map(|demand| {
                let mut spec = spec_node(&TestNode::new(match demand {
                    Demand::Peak => "peak",
                    Demand::Critical => "critical",
                    Demand::Standard => "standard",
                    Demand::Light => "light",
                }));
                spec.demand = *demand;
                spec
            })
            .collect();
        let definition = kvdag_of(nodes, Vec::new());

        for tier in [Tier::Auto, Tier::Max, Tier::High, Tier::Medium, Tier::Low] {
            let table = resolve_assignments(&definition, tier, &HistoryIndex::new());
            for node in &definition.nodes {
                let resolved = table
                    .get(&node.key)
                    .unwrap_or_else(|| panic!("{tier} resolves {}", node.key));
                assert_eq!(
                    resolved.assignment(),
                    tier::resolve(tier, node.demand, None),
                    "{tier}/{:?} must not drift from the §7.1/§7.2 tables",
                    node.demand
                );
            }
        }
    }

    #[test]
    fn auto_records_which_policy_step_explained_the_assignment() {
        let mut standard = spec_node(&TestNode::new("standard"));
        standard.demand = Demand::Standard;
        let definition = kvdag_of(vec![standard], Vec::new());
        let key = NodeKey::new("standard");

        let unmeasured = resolve_assignments(&definition, Tier::Auto, &HistoryIndex::new());
        assert_eq!(
            unmeasured.get(&key).map(|node| node.reason.as_str()),
            Some("auto/high-row"),
            "no history behaves like an all-zero record"
        );

        let mut history = HistoryIndex::new();
        history.insert(
            key.clone(),
            NodeHistory {
                runs: 4,
                first_pass_successes: 4,
                ..NodeHistory::default()
            },
        );
        let measured = resolve_assignments(&definition, Tier::Auto, &history);
        let resolved = measured.get(&key).expect("the node is resolved");
        assert_eq!(resolved.reason, "auto/downgrade-standard");
        assert_eq!(resolved.model, ModelAlias::Sonnet);
        assert_eq!(resolved.effort, Effort::High);

        let mut escalating = HistoryIndex::new();
        escalating.insert(
            key.clone(),
            NodeHistory {
                runs: 4,
                first_pass_successes: 4,
                recent_first_pass_failures: 2,
                ..NodeHistory::default()
            },
        );
        let both = resolve_assignments(&definition, Tier::Auto, &escalating);
        assert_eq!(
            both.get(&key).map(|node| node.reason.as_str()),
            Some("auto/downgrade-standard+escalate"),
            "the reason cannot describe a step the assignment did not take"
        );

        let mut failing = HistoryIndex::new();
        failing.insert(
            key.clone(),
            NodeHistory {
                runs: 2,
                recent_first_pass_failures: 2,
                ..NodeHistory::default()
            },
        );
        let escalated = resolve_assignments(&definition, Tier::Auto, &failing);
        assert_eq!(
            escalated.get(&key).map(|node| node.reason.as_str()),
            Some("auto/escalate")
        );
    }

    #[test]
    fn materialise_with_carries_the_resolved_table_onto_the_run() {
        let mut plan = spec_node(&TestNode::new("plan"));
        plan.demand = Demand::Standard;
        let definition = kvdag_of(vec![plan], Vec::new());
        let mut history = HistoryIndex::new();
        history.insert(
            NodeKey::new("plan"),
            NodeHistory {
                runs: 5,
                first_pass_successes: 5,
                ..NodeHistory::default()
            },
        );
        let table = resolve_assignments(&definition, Tier::Auto, &history);

        let graph = RunGraph::materialise_with(
            &definition,
            RunId::new("workflow_run:1"),
            Tier::Auto,
            &table,
        );

        assert_eq!(graph.assignments, table, "the store writes this verbatim");
        assert_eq!(node(&graph, "plan").assignment.model, ModelAlias::Sonnet);
        assert_eq!(
            node(&graph, "plan").assignment_reason,
            "auto/downgrade-standard",
            "a finished run can still be explained"
        );
    }

    #[test]
    fn materialise_resolves_a_history_free_table_of_its_own() {
        let mut plan = spec_node(&TestNode::new("plan"));
        plan.demand = Demand::Standard;
        let definition = kvdag_of(vec![plan], Vec::new());
        let graph = RunGraph::materialise(&definition, RunId::new("workflow_run:1"), Tier::Auto);

        assert_eq!(
            graph.assignments.len(),
            1,
            "the compatibility wrapper still fills the table"
        );
        assert_eq!(node(&graph, "plan").assignment_reason, "auto/high-row");
    }

    #[test]
    fn a_node_missing_from_the_table_still_resolves_from_its_tier() {
        let definition = kvdag_of(vec![spec_node(&TestNode::new("plan"))], Vec::new());
        let graph = RunGraph::materialise_with(
            &definition,
            RunId::new("workflow_run:1"),
            Tier::Low,
            &BTreeMap::new(),
        );

        assert_eq!(
            node(&graph, "plan").assignment,
            tier::resolve(Tier::Low, Demand::Standard, None),
            "a run that lost one row still starts"
        );
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
