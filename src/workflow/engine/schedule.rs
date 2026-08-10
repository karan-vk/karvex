//! Ready-set computation, admission, and the run's terminal-state conjunction.
//!
//! Implements `docs/design/workflow-builder/04-kvdag-and-execution.md` §3.1 and
//! §3.2. Nothing here reads the clock, the disk, or a pane: an edge is settled
//! purely from the status and validated result of its source node, which is
//! what makes the whole scheduler replayable.

use std::cmp::Ordering;
use std::fmt;

use crate::workflow::model::{
    CmpOp, Condition, EdgeKind, FieldPath, InstancePath, JsonScalar, NodeStatus, RunGraph, RunNode,
    RunNodeIdx,
};

/// The specific unmet conjunct that stops a run from reporting success. A run
/// that stalls surfaces one of these instead of reporting success — the classic
/// false-completion bug is exactly "no node is runnable" being treated as done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalBlocker {
    /// At least one node is still `Pending`, `Ready`, `Running`, or
    /// `NeedsAttention`.
    NodesOutstanding(InstancePath),
    /// A non-skipped node reached a terminal status without recording a
    /// succession.
    SuccessionGap(InstancePath),
    /// An expansion proposal is still unaccepted.
    ExpansionOutstanding,
    /// A monitor node's resume condition is still unsatisfied.
    MonitorWaiting(InstancePath),
}

impl fmt::Display for TerminalBlocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NodesOutstanding(path) => {
                write!(f, "node \"{path}\" has not reached a terminal status")
            }
            Self::SuccessionGap(path) => {
                write!(f, "node \"{path}\" closed without recording a succession")
            }
            Self::ExpansionOutstanding => f.write_str("an expansion proposal is still unaccepted"),
            Self::MonitorWaiting(path) => {
                write!(f, "monitor \"{path}\" is still waiting on its condition")
            }
        }
    }
}

/// The two §3.2 conjuncts that are not derivable from the run graph itself.
/// Expansion proposals arrive in Phase 2 and monitor waits in Phase 4, so both
/// are supplied by the caller rather than silently dropped from the
/// conjunction — an unevaluated conjunct that reads as "satisfied" is exactly
/// the false-completion bug §3.2 exists to prevent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalContext {
    pub pending_expansions: usize,
    pub waiting_monitors: Vec<InstancePath>,
}

/// How one edge stands right now (§3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeResolution {
    /// The source has not reached a state that settles this edge.
    Unresolved,
    /// The edge fired: its payload flows to the target.
    Fired,
    /// The edge will never fire. A node whose every inbound edge is dead
    /// becomes `Skipped`, not `Failed`.
    Dead,
}

/// Settles one edge against its source node.
///
/// A `Failed` or `Cancelled` source leaves a `Sequence`/`Data` edge
/// `Unresolved` on purpose: the target stays `Pending` and
/// [`run_terminal_ready`] names it, instead of the branch quietly evaporating
/// into `Skipped` while the run reports success.
pub fn resolve_edge(graph: &RunGraph, edge_index: usize) -> EdgeResolution {
    let Some(edge) = graph.edges.get(edge_index) else {
        return EdgeResolution::Unresolved;
    };
    let Some(source) = graph.node(edge.from) else {
        return EdgeResolution::Unresolved;
    };

    match edge.kind {
        EdgeKind::Sequence => match source.status {
            NodeStatus::Succeeded | NodeStatus::Restored => EdgeResolution::Fired,
            NodeStatus::Skipped => EdgeResolution::Dead,
            _ => EdgeResolution::Unresolved,
        },
        EdgeKind::Data => match source.status {
            NodeStatus::Succeeded | NodeStatus::Restored if source.result.is_some() => {
                EdgeResolution::Fired
            }
            NodeStatus::Skipped => EdgeResolution::Dead,
            _ => EdgeResolution::Unresolved,
        },
        EdgeKind::Conditional => {
            if source.status == NodeStatus::Skipped {
                return EdgeResolution::Dead;
            }
            if !source.status.is_terminal() {
                return EdgeResolution::Unresolved;
            }
            let null = serde_json::Value::Null;
            let output = source
                .result
                .as_ref()
                .map_or(&null, |result| &result.payload);
            let condition = edge.condition.as_ref().unwrap_or(&Condition::Always);
            if evaluate(condition, output) {
                EdgeResolution::Fired
            } else {
                EdgeResolution::Dead
            }
        }
    }
}

/// What one [`propagate`] pass settled, in index order.
///
/// Edges are reported alongside nodes because an edge's firing state is a
/// durable run fact, not a derived one: it is the only record of *why* a
/// branch was taken, and a caller that persists node statuses without it reads
/// every restored edge back unfired.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Propagation {
    /// Nodes whose `status` changed.
    pub nodes: Vec<RunNodeIdx>,
    /// Indices into `RunGraph::edges` whose `fired`/`condition_result` changed.
    pub edges: Vec<usize>,
}

/// Settles every edge against its source's *current* state, then moves every
/// node between `Pending` and `Ready`/`Skipped` accordingly. Returns what
/// changed.
///
/// `Skipped` propagates: a `Sequence`/`Data`/`Conditional` edge out of a
/// `Skipped` source is dead, so a whole conditional branch collapses in one
/// call.
///
/// Resolution is not one-way. Restarting a node clears its validated result, so
/// its outbound edges go back to `Unresolved` and any target admitted on their
/// strength waits again — §3.1 resolves a `Data` edge only while its source is
/// `Succeeded`/`Restored` *with* a validated result, and a stale `fired` would
/// run a fan-in node with the restarted branch's port missing.
pub fn propagate(graph: &mut RunGraph) -> Propagation {
    let mut changed: Vec<RunNodeIdx> = Vec::new();
    let mut changed_edges: Vec<usize> = Vec::new();
    // An edge resolves purely from its source's status, and the only status
    // moves this function makes are Pending → Ready/Skipped and Ready →
    // Pending — neither of which turns a settled edge back into an unresolved
    // one. So within a single call every edge only settles further, no node can
    // oscillate, and the fixpoint is reached in at most one pass per node plus
    // one; the bound is belt-and-braces against a graph whose nodes are not in
    // topological order.
    let rounds = graph.nodes.len() + graph.edges.len() + 1;

    for _ in 0..rounds {
        let mut progressed = false;

        for index in 0..graph.edges.len() {
            let (fired, condition_result) = match resolve_edge(graph, index) {
                EdgeResolution::Unresolved => (false, None),
                EdgeResolution::Fired => (true, Some(true)),
                EdgeResolution::Dead => (false, Some(false)),
            };
            let Some(edge) = graph.edges.get_mut(index) else {
                continue;
            };
            if edge.fired != fired || edge.condition_result != condition_result {
                edge.fired = fired;
                edge.condition_result = condition_result;
                changed_edges.push(index);
                progressed = true;
            }
        }

        for index in 0..graph.nodes.len() {
            let idx = RunNodeIdx(index);
            let status = graph.node(idx).map(|node| node.status);
            // Only the two admission statuses move here. A node that already
            // holds a pane, or that has closed, is the run's business, not the
            // scheduler's.
            if !matches!(status, Some(NodeStatus::Pending | NodeStatus::Ready)) {
                continue;
            }
            let inbound: Vec<usize> = graph.inbound(idx).collect();
            let settled = inbound.iter().all(|edge_index| {
                graph
                    .edges
                    .get(*edge_index)
                    .is_some_and(|edge| edge.condition_result.is_some())
            });
            let all_dead = !inbound.is_empty()
                && inbound
                    .iter()
                    .all(|edge_index| graph.edges.get(*edge_index).is_some_and(|edge| !edge.fired));
            let next = if !settled {
                // An admitted node whose upstream went back to `Unresolved`
                // waits again rather than starting without that branch's data.
                NodeStatus::Pending
            } else if all_dead {
                NodeStatus::Skipped
            } else {
                NodeStatus::Ready
            };
            if status == Some(next) {
                continue;
            }
            let Some(node) = graph.node_mut(idx) else {
                continue;
            };
            node.status = next;
            changed.push(idx);
            progressed = true;
        }

        if !progressed {
            break;
        }
    }

    changed.sort_unstable();
    changed.dedup();
    changed_edges.sort_unstable();
    changed_edges.dedup();
    Propagation {
        nodes: changed,
        edges: changed_edges,
    }
}

/// Nodes admitted to run now, in `(depth, path)` order so breadth stays
/// predictable and the DAG view does not reshuffle, capped so at most
/// `max_parallel_nodes` are `Running` at once.
///
/// Reads statuses only: call [`propagate`] first.
pub fn ready_set(graph: &RunGraph, max_parallel_nodes: usize) -> Vec<RunNodeIdx> {
    let running = graph
        .nodes
        .iter()
        .filter(|node| node.status == NodeStatus::Running)
        .count();
    let capacity = max_parallel_nodes.saturating_sub(running);
    if capacity == 0 {
        return Vec::new();
    }

    let mut candidates: Vec<&RunNode> = graph
        .nodes
        .iter()
        .filter(|node| node.status == NodeStatus::Ready)
        .collect();
    candidates.sort_by(|left, right| {
        left.depth
            .cmp(&right.depth)
            .then_with(|| left.path.cmp(&right.path))
    });
    candidates
        .into_iter()
        .take(capacity)
        .map(|node| node.idx)
        .collect()
}

/// The conjunction of §3.2 with the Phase 2/4 conjuncts defaulted to "nothing
/// outstanding". `Ok(())` means the run may finish; `Err` names the conjunct
/// that is not satisfied.
pub fn run_terminal_ready(graph: &RunGraph) -> Result<(), TerminalBlocker> {
    run_terminal_ready_with(graph, &TerminalContext::default())
}

/// The full conjunction. Never "no node is runnable": a stalled graph produces
/// the specific unmet conjunct, which the caller surfaces as `Paused`.
///
/// Evaluated over the **user graph only**: engine-owned nodes (reserved
/// `.`-prefixed instance paths — today just the `.summary` epilogue) are outside
/// the conjunction by construction. That is what makes
/// `07-phase3-plan.md` §4 D1's guarantee structural rather than incidental: a
/// summariser that is still running, or that gave up, cannot hold a finished run
/// open or turn its outcome into a failure, because this function never sees it.
pub fn run_terminal_ready_with(
    graph: &RunGraph,
    context: &TerminalContext,
) -> Result<(), TerminalBlocker> {
    let user_nodes = || {
        graph
            .nodes
            .iter()
            .filter(|node| !crate::workflow::model::is_reserved_path(node.path.as_str()))
    };

    for node in user_nodes() {
        if matches!(
            node.status,
            NodeStatus::Pending
                | NodeStatus::Ready
                | NodeStatus::Running
                | NodeStatus::NeedsAttention
        ) {
            return Err(TerminalBlocker::NodesOutstanding(node.path.clone()));
        }
    }

    for node in user_nodes() {
        if node.status != NodeStatus::Skipped && node.succession.is_none() {
            return Err(TerminalBlocker::SuccessionGap(node.path.clone()));
        }
    }

    if context.pending_expansions > 0 {
        return Err(TerminalBlocker::ExpansionOutstanding);
    }

    if let Some(path) = context.waiting_monitors.first() {
        return Err(TerminalBlocker::MonitorWaiting(path.clone()));
    }

    Ok(())
}

/// Evaluates a conditional edge's predicate against a node's validated output.
/// Total and loop-free: an unresolvable path is `false`, never an error.
pub fn evaluate(condition: &Condition, output: &serde_json::Value) -> bool {
    match condition {
        Condition::Always => true,
        Condition::Exists { path } => lookup(output, path).is_some_and(|value| !value.is_null()),
        Condition::Eq { path, value } => {
            lookup(output, path).is_some_and(|found| scalar_eq(value, found))
        }
        Condition::Cmp { path, op, value } => {
            lookup(output, path).is_some_and(|found| scalar_cmp(found, *op, value))
        }
        Condition::OneOf { path, values } => lookup(output, path)
            .is_some_and(|found| values.iter().any(|value| scalar_eq(value, found))),
        Condition::Not(inner) => !evaluate(inner, output),
        Condition::All(inner) => inner.iter().all(|child| evaluate(child, output)),
        Condition::Any(inner) => inner.iter().any(|child| evaluate(child, output)),
    }
}

/// Walks a dotted [`FieldPath`]. A numeric segment indexes an array, so
/// `changed_files.0` addresses the first entry.
fn lookup<'a>(output: &'a serde_json::Value, path: &FieldPath) -> Option<&'a serde_json::Value> {
    let mut cursor = output;
    for segment in path.segments() {
        cursor = match cursor {
            serde_json::Value::Object(map) => map.get(segment)?,
            serde_json::Value::Array(items) => items.get(segment.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cursor)
}

fn scalar_eq(scalar: &JsonScalar, value: &serde_json::Value) -> bool {
    match scalar {
        JsonScalar::Bool(expected) => value.as_bool() == Some(*expected),
        JsonScalar::String(expected) => value.as_str() == Some(expected.as_str()),
        JsonScalar::Null => value.is_null(),
        JsonScalar::Int(expected) => value.as_f64() == Some(*expected as f64),
        JsonScalar::Float(expected) => value.as_f64() == Some(*expected),
    }
}

fn scalar_cmp(value: &serde_json::Value, op: CmpOp, scalar: &JsonScalar) -> bool {
    let order: Option<Ordering> = match scalar {
        JsonScalar::Int(expected) => value
            .as_f64()
            .and_then(|found| found.partial_cmp(&(*expected as f64))),
        JsonScalar::Float(expected) => value.as_f64().and_then(|found| found.partial_cmp(expected)),
        JsonScalar::String(expected) => value.as_str().map(|found| found.cmp(expected.as_str())),
        JsonScalar::Bool(_) | JsonScalar::Null => None,
    };
    let Some(order) = order else {
        return false;
    };
    match op {
        CmpOp::Lt => order.is_lt(),
        CmpOp::Le => order.is_le(),
        CmpOp::Gt => order.is_gt(),
        CmpOp::Ge => order.is_ge(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::engine::tests_support::{
        diamond, edge, graph_of, linear, node_at, set_result, TestNode,
    };
    use crate::workflow::model::{EdgePayload, Succession};

    fn json(raw: &str) -> serde_json::Value {
        serde_json::from_str(raw).expect("test json parses")
    }

    fn field(path: &str) -> FieldPath {
        FieldPath(path.to_string())
    }

    #[test]
    fn roots_become_ready_and_dependants_stay_pending() {
        let mut graph = linear(&["plan", "implement"]);
        let changed = propagate(&mut graph);

        assert_eq!(changed.nodes, vec![RunNodeIdx(0)]);
        assert_eq!(node_at(&graph, "plan").status, NodeStatus::Ready);
        assert_eq!(node_at(&graph, "implement").status, NodeStatus::Pending);
    }

    #[test]
    fn diamond_ready_set_is_breadth_ordered_and_capped() {
        let mut graph = diamond();
        propagate(&mut graph);
        assert_eq!(ready_set(&graph, 4), vec![RunNodeIdx(0)]);

        set_result(&mut graph, "plan", json(r#"{"plan":"do it"}"#));
        let changed = propagate(&mut graph);
        assert_eq!(changed.nodes, vec![RunNodeIdx(1), RunNodeIdx(2)]);

        // "left" and "right" are both Ready; (depth, path) puts left first.
        assert_eq!(
            ready_set(&graph, 4),
            vec![RunNodeIdx(1), RunNodeIdx(2)],
            "both branches admit at once when the cap allows it"
        );
        assert_eq!(
            ready_set(&graph, 1),
            vec![RunNodeIdx(1)],
            "the cap admits the lowest (depth, path) first"
        );
        assert!(node_at(&graph, "join").status == NodeStatus::Pending);
    }

    #[test]
    fn admission_counts_already_running_nodes_against_the_cap() {
        let mut graph = diamond();
        propagate(&mut graph);
        set_result(&mut graph, "plan", json(r#"{"plan":"do it"}"#));
        propagate(&mut graph);

        let idx = node_at(&graph, "left").idx;
        if let Some(node) = graph.node_mut(idx) {
            node.status = NodeStatus::Running;
        }
        assert!(ready_set(&graph, 1).is_empty());
        assert_eq!(ready_set(&graph, 2), vec![RunNodeIdx(2)]);
    }

    #[test]
    fn false_conditional_kills_the_edge_and_skips_the_branch() {
        let mut graph = graph_of(
            &[
                TestNode::new("gate"),
                TestNode::new("hotfix"),
                TestNode::new("ship"),
            ],
            &[
                edge(0, 1, EdgeKind::Conditional).with_condition(Condition::Eq {
                    path: field("verdict"),
                    value: JsonScalar::String("fail".to_string()),
                }),
                edge(1, 2, EdgeKind::Sequence),
            ],
        );
        propagate(&mut graph);
        set_result(&mut graph, "gate", json(r#"{"verdict":"pass"}"#));
        let changed = propagate(&mut graph);

        assert_eq!(changed.nodes, vec![RunNodeIdx(1), RunNodeIdx(2)]);
        assert_eq!(node_at(&graph, "hotfix").status, NodeStatus::Skipped);
        assert_eq!(
            node_at(&graph, "ship").status,
            NodeStatus::Skipped,
            "Skipped propagates through a Sequence edge"
        );
        assert_eq!(graph.edges[0].condition_result, Some(false));
        assert!(!graph.edges[0].fired);
    }

    #[test]
    fn true_conditional_fires_and_a_partly_dead_fan_in_still_readies() {
        let mut graph = graph_of(
            &[
                TestNode::new("gate"),
                TestNode::new("skipme"),
                TestNode::new("always"),
                TestNode::new("join"),
            ],
            &[
                edge(0, 1, EdgeKind::Conditional).with_condition(Condition::Eq {
                    path: field("verdict"),
                    value: JsonScalar::String("fail".to_string()),
                }),
                edge(0, 2, EdgeKind::Sequence),
                edge(1, 3, EdgeKind::Sequence),
                edge(2, 3, EdgeKind::Sequence),
            ],
        );
        propagate(&mut graph);
        set_result(&mut graph, "gate", json(r#"{"verdict":"pass"}"#));
        propagate(&mut graph);
        assert_eq!(node_at(&graph, "skipme").status, NodeStatus::Skipped);
        assert_eq!(node_at(&graph, "always").status, NodeStatus::Ready);

        set_result(&mut graph, "always", json("{}"));
        propagate(&mut graph);
        assert_eq!(
            node_at(&graph, "join").status,
            NodeStatus::Ready,
            "a dead inbound edge is resolved, so it does not block the join"
        );
    }

    #[test]
    fn a_data_edge_waits_for_a_validated_result() {
        let mut graph = graph_of(
            &[TestNode::new("plan"), TestNode::new("implement")],
            &[edge(0, 1, EdgeKind::Data)],
        );
        propagate(&mut graph);

        let idx = node_at(&graph, "plan").idx;
        if let Some(node) = graph.node_mut(idx) {
            node.status = NodeStatus::Succeeded;
        }
        propagate(&mut graph);
        assert_eq!(
            node_at(&graph, "implement").status,
            NodeStatus::Pending,
            "Succeeded without a result never satisfies a Data edge"
        );

        set_result(&mut graph, "plan", json(r#"{"plan":"x"}"#));
        propagate(&mut graph);
        assert_eq!(node_at(&graph, "implement").status, NodeStatus::Ready);
    }

    #[test]
    fn a_failed_source_stalls_the_branch_instead_of_skipping_it() {
        let mut graph = linear(&["plan", "implement"]);
        propagate(&mut graph);
        let idx = node_at(&graph, "plan").idx;
        if let Some(node) = graph.node_mut(idx) {
            node.status = NodeStatus::Failed;
            node.succession = Some(Succession::Blocked {
                reason: "pane exited".to_string(),
                resume_when: "restart the node".to_string(),
            });
        }
        propagate(&mut graph);

        assert_eq!(node_at(&graph, "implement").status, NodeStatus::Pending);
        assert_eq!(
            run_terminal_ready(&graph),
            Err(TerminalBlocker::NodesOutstanding(InstancePath::new(
                "implement"
            )))
        );
    }

    #[test]
    fn terminal_ready_refuses_while_a_node_needs_attention() {
        let mut graph = linear(&["only"]);
        let idx = node_at(&graph, "only").idx;
        if let Some(node) = graph.node_mut(idx) {
            node.status = NodeStatus::NeedsAttention;
            node.succession = Some(Succession::Satisfied);
        }
        assert_eq!(
            run_terminal_ready(&graph),
            Err(TerminalBlocker::NodesOutstanding(InstancePath::new("only")))
        );
    }

    /// The one way surfacing the blocker could go wrong: `Succession::Blocked`
    /// is a *recorded* succession, and the succession-gap conjunct is satisfied
    /// by any succession at all. The status conjunct runs first and holds the
    /// run open, so a stalled node can never be mistaken for a finished one.
    #[test]
    fn a_blocked_needs_attention_node_still_refuses_to_let_the_run_succeed() {
        let mut graph = linear(&["only"]);
        let idx = node_at(&graph, "only").idx;
        if let Some(node) = graph.node_mut(idx) {
            node.status = NodeStatus::NeedsAttention;
            node.succession = Some(Succession::Blocked {
                reason: "the node's pane could not be started".to_string(),
                resume_when: "a workspace exists and the node is restarted".to_string(),
            });
        }
        assert_eq!(
            run_terminal_ready(&graph),
            Err(TerminalBlocker::NodesOutstanding(InstancePath::new("only")))
        );
    }

    #[test]
    fn terminal_ready_reports_a_succession_gap() {
        let mut graph = linear(&["only"]);
        let idx = node_at(&graph, "only").idx;
        if let Some(node) = graph.node_mut(idx) {
            node.status = NodeStatus::Succeeded;
        }
        assert_eq!(
            run_terminal_ready(&graph),
            Err(TerminalBlocker::SuccessionGap(InstancePath::new("only")))
        );

        if let Some(node) = graph.node_mut(idx) {
            node.succession = Some(Succession::Satisfied);
        }
        assert_eq!(run_terminal_ready(&graph), Ok(()));
    }

    #[test]
    fn terminal_ready_exempts_skipped_nodes_from_the_succession_requirement() {
        let mut graph = linear(&["only"]);
        let idx = node_at(&graph, "only").idx;
        if let Some(node) = graph.node_mut(idx) {
            node.status = NodeStatus::Skipped;
        }
        assert_eq!(run_terminal_ready(&graph), Ok(()));
    }

    #[test]
    fn terminal_ready_honours_the_phase_two_and_four_conjuncts() {
        let mut graph = linear(&["only"]);
        let idx = node_at(&graph, "only").idx;
        if let Some(node) = graph.node_mut(idx) {
            node.status = NodeStatus::Succeeded;
            node.succession = Some(Succession::Satisfied);
        }

        assert_eq!(
            run_terminal_ready_with(
                &graph,
                &TerminalContext {
                    pending_expansions: 1,
                    waiting_monitors: Vec::new(),
                }
            ),
            Err(TerminalBlocker::ExpansionOutstanding)
        );
        assert_eq!(
            run_terminal_ready_with(
                &graph,
                &TerminalContext {
                    pending_expansions: 0,
                    waiting_monitors: vec![InstancePath::new("watch")],
                }
            ),
            Err(TerminalBlocker::MonitorWaiting(InstancePath::new("watch")))
        );
    }

    /// §3.1 already resolves every edge kind out of a `Restored` source; this
    /// pins that the Phase 3 producer actually exercises all three, so a later
    /// edit to `resolve_edge` cannot quietly drop one.
    #[test]
    fn a_restored_source_fires_sequence_data_and_conditional_edges() {
        let mut graph = graph_of(
            &[
                TestNode::new("seed"),
                TestNode::new("after"),
                TestNode::new("consumer"),
                TestNode::new("gated"),
            ],
            &[
                edge(0, 1, EdgeKind::Sequence),
                edge(0, 2, EdgeKind::Data),
                edge(0, 3, EdgeKind::Conditional).with_condition(Condition::Eq {
                    path: field("verdict"),
                    value: JsonScalar::String("pass".to_string()),
                }),
            ],
        );
        let idx = node_at(&graph, "seed").idx;
        if let Some(node) = graph.node_mut(idx) {
            node.status = NodeStatus::Restored;
            node.succession = Some(Succession::Satisfied);
            node.result = Some(crate::workflow::model::NodeResult {
                payload: json(r#"{"verdict":"pass"}"#),
                summary: String::new(),
                artifact_paths: Vec::new(),
                digest: String::new(),
                evidence: crate::workflow::model::Evidence::Restored,
            });
        }
        propagate(&mut graph);

        assert_eq!(node_at(&graph, "after").status, NodeStatus::Ready);
        assert_eq!(node_at(&graph, "consumer").status, NodeStatus::Ready);
        assert_eq!(node_at(&graph, "gated").status, NodeStatus::Ready);
        assert!(graph.edges.iter().all(|edge| edge.fired));
    }

    /// §4 D1's structural half: the conjunction is evaluated over the user graph
    /// only, so a summariser that is still running — or that gave up — can
    /// neither hold a finished run open nor turn its outcome into a failure.
    #[test]
    fn engine_owned_nodes_are_outside_the_terminal_conjunction() {
        let mut graph = linear(&["only"]);
        let idx = node_at(&graph, "only").idx;
        if let Some(node) = graph.node_mut(idx) {
            node.status = NodeStatus::Succeeded;
            node.succession = Some(Succession::Satisfied);
        }
        assert_eq!(run_terminal_ready(&graph), Ok(()));

        let epilogue = RunNodeIdx(graph.nodes.len());
        let mut summariser = graph.nodes[0].clone();
        summariser.idx = epilogue;
        summariser.key =
            crate::workflow::model::NodeKey::new(crate::workflow::model::SUMMARY_INSTANCE_PATH);
        summariser.path = InstancePath::new(crate::workflow::model::SUMMARY_INSTANCE_PATH);
        summariser.status = NodeStatus::Running;
        summariser.succession = None;
        graph.nodes.push(summariser);

        assert_eq!(
            run_terminal_ready(&graph),
            Ok(()),
            "a running summariser neither blocks on status nor reads as a succession gap"
        );

        // The same holds once it has given up.
        if let Some(node) = graph.node_mut(epilogue) {
            node.status = NodeStatus::Failed;
            node.succession = None;
        }
        assert_eq!(run_terminal_ready(&graph), Ok(()));

        // And an authored node with the same failure still blocks, so the
        // exemption is about the reserved namespace and nothing else.
        if let Some(node) = graph.node_mut(idx) {
            node.status = NodeStatus::Running;
        }
        assert_eq!(
            run_terminal_ready(&graph),
            Err(TerminalBlocker::NodesOutstanding(InstancePath::new("only")))
        );
    }

    #[test]
    fn edge_payload_and_port_survive_propagation() {
        let mut graph = graph_of(
            &[TestNode::new("plan"), TestNode::new("implement")],
            &[edge(0, 1, EdgeKind::Data)
                .with_port("plan")
                .with_payload(EdgePayload::Full)],
        );
        propagate(&mut graph);
        set_result(&mut graph, "plan", json(r#"{"plan":"x"}"#));
        propagate(&mut graph);

        assert_eq!(graph.edges[0].port.as_deref(), Some("plan"));
        assert_eq!(graph.edges[0].payload, EdgePayload::Full);
        assert!(graph.edges[0].fired);
        assert_eq!(graph.edges[0].condition_result, Some(true));
    }

    #[test]
    fn conditions_are_total_over_missing_and_mistyped_paths() {
        let output = json(r#"{"verdict":"pass","score":7,"files":["a","b"],"empty":null}"#);

        assert!(evaluate(&Condition::Always, &output));
        assert!(evaluate(
            &Condition::Exists {
                path: field("verdict")
            },
            &output
        ));
        assert!(
            !evaluate(
                &Condition::Exists {
                    path: field("empty")
                },
                &output
            ),
            "an explicit null is not evidence that a field was filled in"
        );
        assert!(!evaluate(
            &Condition::Exists {
                path: field("missing")
            },
            &output
        ));
        assert!(evaluate(
            &Condition::Eq {
                path: field("files.1"),
                value: JsonScalar::String("b".to_string())
            },
            &output
        ));
        assert!(evaluate(
            &Condition::Cmp {
                path: field("score"),
                op: CmpOp::Ge,
                value: JsonScalar::Int(7)
            },
            &output
        ));
        assert!(!evaluate(
            &Condition::Cmp {
                path: field("verdict"),
                op: CmpOp::Gt,
                value: JsonScalar::Int(7)
            },
            &output
        ));
        assert!(evaluate(
            &Condition::OneOf {
                path: field("verdict"),
                values: vec![
                    JsonScalar::String("pass".to_string()),
                    JsonScalar::String("warn".to_string())
                ]
            },
            &output
        ));
        assert!(evaluate(
            &Condition::Not(Box::new(Condition::Exists {
                path: field("missing")
            })),
            &output
        ));
        assert!(evaluate(&Condition::All(Vec::new()), &output));
        assert!(!evaluate(&Condition::Any(Vec::new()), &output));
        assert!(evaluate(
            &Condition::All(vec![
                Condition::Always,
                Condition::Eq {
                    path: field("score"),
                    value: JsonScalar::Float(7.0)
                }
            ]),
            &output
        ));
    }
}
