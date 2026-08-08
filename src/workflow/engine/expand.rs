//! Expansion proposals, guardrails, and commit/reject.
//!
//! Phase 2 (`docs/design/workflow-builder/04-kvdag-and-execution.md` §3.4). The
//! types land now because the journal, the events, and the growth-limit badge
//! are already part of the Phase 1 vocabulary. A node cannot create nodes; it
//! proposes, and a rejection is always surfaced, never silently truncated.

use std::collections::BTreeMap;

use serde_json::json;
use tracing::debug;

use crate::workflow::engine::graph::FIRST_ATTEMPT;
use crate::workflow::engine::journal;
use crate::workflow::model::{
    Condition, Demand, EdgeKind, EdgePayload, GrowthLimits, InstancePath, Kvdag, NodeAssignment,
    NodeKey, NodeStatus, NodeUsage, ProgressTracker, RunEdge, RunEffect, RunEventKind, RunGraph,
    RunNode, RunNodeIdx, StoreWrite, WorkflowEvent,
};
use crate::workflow::tier;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandProposal {
    pub template: NodeKey,
    pub label: String,
    pub inputs: BTreeMap<String, String>,
    pub count: Option<u16>,
}

impl ExpandProposal {
    /// How many children this proposal asks for.
    ///
    /// `count` defaults to 1 (`06-phase2-plan.md` §4 D2). An explicit `0` is
    /// read as 1 rather than as "create nothing": a proposal is a request for
    /// at least one child, and a silently empty outcome is precisely the
    /// unreported truncation §3.4 forbids.
    pub fn requested(&self) -> u16 {
        self.count.unwrap_or(1).max(1)
    }
}

/// Which guardrail a proposal ran into. The wire spelling is
/// `WorkflowGrowthLimitKind`; the two vocabularies are declared separately on
/// purpose (`src/api/schema/workflows.rs`'s module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandLimit {
    /// The proposing node's own `expand_max`, counted cumulatively across every
    /// proposal it has made in this run.
    ExpandMax,
    /// `growth.max_depth`, guarded as `parent.depth + 1 <= max_depth` with
    /// static nodes at depth 0 (`06-phase2-plan.md` §4 D13).
    MaxDepth,
    /// `growth.max_nodes`, counted over every materialised `RunNode` regardless
    /// of status — a failed child does not refund budget (§4 D12).
    MaxNodes,
}

impl ExpandLimit {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExpandMax => "expand_max",
            Self::MaxDepth => "max_depth",
            Self::MaxNodes => "max_nodes",
        }
    }

    /// This limit's ceiling for one proposing node: the run's narrowed growth
    /// limits for the two run-level guardrails, the node's own `expand_max` for
    /// the node-level one.
    ///
    /// One authority for "what number was hit", so the journal payload, the
    /// wire's `WorkflowGrowthLimit.limit_value`, the DAG notice, and the CLI
    /// cannot disagree.
    pub fn value_in(self, growth: GrowthLimits, expand_max: u16) -> u16 {
        match self {
            Self::ExpandMax => expand_max,
            Self::MaxDepth => growth.max_depth,
            Self::MaxNodes => growth.max_nodes,
        }
    }
}

/// Why a proposal was refused. Each variant carries the exact limit hit so the
/// DAG view can render it on the proposing node.
///
/// The six original variants keep their exact Phase 1 shapes;
/// [`ExpandRejection::Truncated`] (`06-phase2-plan.md` §4 D2) and
/// [`ExpandRejection::UnknownInput`] (§4 D3) are the two additive ones, for
/// eight in total (§3 frozen interface 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpandRejection {
    NotAllowed {
        template: NodeKey,
    },
    UnknownTemplate {
        template: NodeKey,
    },
    NotATemplate {
        template: NodeKey,
    },
    ExpandMaxReached {
        limit: u16,
    },
    MaxDepthReached {
        limit: u16,
    },
    MaxNodesReached {
        limit: u16,
    },
    /// Partial acceptance: `requested` children were asked for, `accepted` fit
    /// inside `limit`, and the shortfall is reported rather than silently
    /// dropped. Accept-all would violate the ceiling and reject-all would waste
    /// budget; reporting the truncation is what makes partial acceptance
    /// legitimate (§4 D2).
    Truncated {
        template: NodeKey,
        requested: u16,
        accepted: u16,
        limit: ExpandLimit,
    },
    /// An `inputs` key that names no `{{slot}}` in the template's
    /// `prompt_template`. `inputs` is an override channel over the one
    /// validated renderer, never a second unvalidated one (§4 D3).
    UnknownInput {
        template: NodeKey,
        name: String,
    },
}

impl ExpandRejection {
    /// The wire's `WorkflowExpandRejectionReason` spelling. The two
    /// vocabularies are declared separately (`src/api/schema/workflows.rs`'s
    /// module doc); this is the one place that maps between them, so a new
    /// variant cannot reach the wire under an invented name.
    pub fn reason_code(&self) -> &'static str {
        match self {
            Self::NotAllowed { .. } => "not_allowed",
            Self::UnknownTemplate { .. } => "unknown_template",
            Self::NotATemplate { .. } => "not_a_template",
            Self::ExpandMaxReached { .. } => "expand_max_reached",
            Self::MaxDepthReached { .. } => "max_depth_reached",
            Self::MaxNodesReached { .. } => "max_nodes_reached",
            Self::Truncated { .. } => "truncated",
            Self::UnknownInput { .. } => "unknown_input",
        }
    }

    /// The template the rejection names, when its variant carries one. The
    /// three bare limit variants do not — their frozen Phase 1 shapes carry
    /// only the ceiling — so a caller rendering the wire's mandatory
    /// `template` field falls back to the proposal it holds.
    pub fn template(&self) -> Option<&NodeKey> {
        match self {
            Self::NotAllowed { template }
            | Self::UnknownTemplate { template }
            | Self::NotATemplate { template }
            | Self::Truncated { template, .. }
            | Self::UnknownInput { template, .. } => Some(template),
            Self::ExpandMaxReached { .. }
            | Self::MaxDepthReached { .. }
            | Self::MaxNodesReached { .. } => None,
        }
    }

    /// Which guardrail this rejection is, or `None` for the three validation
    /// failures. `Some(..)` is exactly the set that also produces a
    /// `growth_limited` journal entry and a `workflow.growth.limited` event.
    pub fn limit(&self) -> Option<ExpandLimit> {
        match self {
            Self::ExpandMaxReached { .. } => Some(ExpandLimit::ExpandMax),
            Self::MaxDepthReached { .. } => Some(ExpandLimit::MaxDepth),
            Self::MaxNodesReached { .. } => Some(ExpandLimit::MaxNodes),
            Self::Truncated { limit, .. } => Some(*limit),
            Self::NotAllowed { .. }
            | Self::UnknownTemplate { .. }
            | Self::NotATemplate { .. }
            | Self::UnknownInput { .. } => None,
        }
    }

    /// The ceiling's value when the variant carries it.
    ///
    /// `None` for [`Self::Truncated`], whose frozen shape names the limit but
    /// not its value — a caller that needs the number reads it from the run's
    /// [`GrowthLimits`] and the proposing node's `expand_max` through
    /// [`ExpandLimit::value_in`].
    pub fn limit_value(&self) -> Option<u16> {
        match self {
            Self::ExpandMaxReached { limit }
            | Self::MaxDepthReached { limit }
            | Self::MaxNodesReached { limit } => Some(*limit),
            Self::Truncated { .. }
            | Self::NotAllowed { .. }
            | Self::UnknownTemplate { .. }
            | Self::NotATemplate { .. }
            | Self::UnknownInput { .. } => None,
        }
    }

    /// `(requested, accepted)` for the one variant that reports a partial
    /// outcome. Every other rejection created nothing, and the count it was
    /// refused is the proposal's, which the caller holds.
    pub fn counts(&self) -> Option<(u16, u16)> {
        match self {
            Self::Truncated {
                requested,
                accepted,
                ..
            } => Some((*requested, *accepted)),
            _ => None,
        }
    }

    /// The user-facing line, used verbatim by the journal payload and as the
    /// default for the wire's `message`.
    ///
    /// `limit_value` is the ceiling when the caller knows it — mandatory in
    /// practice only for [`Self::Truncated`], which does not carry its own; the
    /// number is omitted rather than guessed when it is unknown.
    pub fn message(&self, limit_value: Option<u16>) -> String {
        match self {
            Self::NotAllowed { template } => {
                format!("template \"{template}\" is not in this node's expand_allow")
            }
            Self::UnknownTemplate { template } => {
                format!("no node named \"{template}\" exists in this workflow version")
            }
            Self::NotATemplate { template } => {
                format!("node \"{template}\" is not declared is_template")
            }
            Self::UnknownInput { template, name } => format!(
                "input \"{name}\" names no {{{{{name}}}}} slot in template \"{template}\"'s prompt"
            ),
            Self::ExpandMaxReached { limit } => {
                format!("expand_max {limit} reached; no nodes created")
            }
            Self::MaxDepthReached { limit } => {
                format!("max_depth {limit} reached; no nodes created")
            }
            Self::MaxNodesReached { limit } => {
                format!("max_nodes {limit} reached; no nodes created")
            }
            Self::Truncated {
                requested,
                accepted,
                limit,
                ..
            } => match limit_value {
                Some(value) => format!(
                    "{} {value} reached; {accepted} of {requested} requested nodes created",
                    limit.as_str()
                ),
                None => format!(
                    "{} reached; {accepted} of {requested} requested nodes created",
                    limit.as_str()
                ),
            },
        }
    }
}

/// One child an accepted proposal instantiates. The instance path is decided
/// during [`evaluate`](self) — it is a pure function of the graph — so a caller
/// can report exactly which nodes an acceptance created before `commit` runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedExpansion {
    /// The kvdag template key this instance is cut from.
    pub template: NodeKey,
    /// `<parent>/<template>/<n>`, `n` 1-based per `(parent, template)`
    /// (§3 frozen interface 8).
    pub path: InstancePath,
    pub label: String,
    /// Slot overrides, already checked against the template's declared slots.
    pub inputs: BTreeMap<String, String>,
    /// The template's declared demand, carried so [`commit`] can fill
    /// `StoreWrite::RunNodeCreated.demand` and fall back to a tier resolution
    /// without a second `Kvdag` lookup. `RunNode` has no demand field, and
    /// [`commit`] takes only the run graph.
    pub demand: Demand,
}

/// The whole verdict on one proposal: what would be created, and every reason
/// something was not. Both halves can be non-empty at once — that is exactly
/// the truncation case (§4 D2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpandOutcome {
    pub accepted: Vec<AcceptedExpansion>,
    pub rejected: Vec<ExpandRejection>,
}

impl ExpandOutcome {
    pub fn is_empty(&self) -> bool {
        self.accepted.is_empty() && self.rejected.is_empty()
    }

    /// One rejection and nothing created — the whole proposal was refused.
    pub fn is_refusal(&self) -> bool {
        self.accepted.is_empty() && !self.rejected.is_empty()
    }
}

/// Decides one proposal against the §3.4 guardrails. **Pure**: it reads the run
/// graph and the definition, mutates nothing, and performs no effects, so a
/// caller can report exactly which nodes an acceptance would create before
/// [`commit`] runs (`06-phase2-plan.md` §3 frozen interface 2).
///
/// The guardrails run in the §3.4 step-2 order — allow-list, template exists,
/// template is a template, `inputs` name real slots, `expand_max`, `max_depth`,
/// `max_nodes` — and the first failing *validation* gate refuses the whole
/// proposal. The three budget gates are different: they yield a capacity, and a
/// proposal that asks for more than fits is **partially accepted** with the
/// shortfall reported as [`ExpandRejection::Truncated`] (§4 D2). Accept-all
/// would violate the ceiling; reject-all would waste budget and truncate
/// silently.
///
/// **Cumulative counting.** `expand_max` is counted per proposing node across
/// every proposal it has made in this run, by counting the children already
/// materialised under it, and `max_nodes` counts every materialised node
/// whatever its status (§4 D12). Both therefore require the caller to
/// `evaluate` and `commit` one proposal at a time, in order: evaluating a whole
/// batch against the pre-batch graph would let a node spend the same budget
/// twice.
pub fn evaluate(
    graph: &RunGraph,
    kvdag: &Kvdag,
    proposer: RunNodeIdx,
    proposal: &ExpandProposal,
) -> ExpandOutcome {
    let mut outcome = ExpandOutcome::default();
    let Some(parent) = graph.node(proposer) else {
        debug!(
            ?proposer,
            "expand proposal from a node that is not in the run graph"
        );
        return outcome;
    };
    let template = proposal.template.clone();

    // §3.4 step 2, gate 1: the proposing node's own allow-list. Checked before
    // the template is looked up, so a proposal cannot probe for the existence
    // of nodes it was never allowed to instantiate.
    let parent_spec = kvdag.node(&parent.key);
    let allowed = parent_spec.is_some_and(|spec| spec.expand_allow.contains(&template));
    if !allowed {
        outcome
            .rejected
            .push(ExpandRejection::NotAllowed { template });
        return outcome;
    }

    // Gates 2 and 3: the template exists and is one.
    let Some(spec) = kvdag.node(&template) else {
        outcome
            .rejected
            .push(ExpandRejection::UnknownTemplate { template });
        return outcome;
    };
    if !spec.is_template {
        outcome
            .rejected
            .push(ExpandRejection::NotATemplate { template });
        return outcome;
    }

    // §4 D3: `inputs` is an override channel over the one validated renderer,
    // never a second unvalidated one. A key naming no slot would render as
    // nothing at all, so the proposal is refused rather than quietly ignored.
    for name in proposal.inputs.keys() {
        if !template_declares_slot(&spec.prompt_template, name) {
            outcome.rejected.push(ExpandRejection::UnknownInput {
                template: template.clone(),
                name: name.clone(),
            });
        }
    }
    if !outcome.rejected.is_empty() {
        return outcome;
    }

    // Gate 4: the node's own cumulative expansion budget.
    let expand_max = parent_spec.map_or(0, |spec| spec.expand_max);
    let spawned = children_of(graph, proposer);
    let expand_budget = expand_max.saturating_sub(spawned);
    if expand_budget == 0 {
        outcome
            .rejected
            .push(ExpandRejection::ExpandMaxReached { limit: expand_max });
        return outcome;
    }

    // Gate 5: depth. Not a budget — a child either fits under the ceiling or no
    // child of this parent ever will, so there is nothing to truncate. Static
    // nodes are depth 0, so `max_depth = 3` permits three generations (§4 D13).
    if parent.depth.saturating_add(1) > graph.growth.max_depth {
        outcome.rejected.push(ExpandRejection::MaxDepthReached {
            limit: graph.growth.max_depth,
        });
        return outcome;
    }

    // Gate 6: the run's monotone node budget.
    let node_budget = graph
        .growth
        .max_nodes
        .saturating_sub(GrowthLimits::live_node_count(&graph.nodes));
    if node_budget == 0 {
        outcome.rejected.push(ExpandRejection::MaxNodesReached {
            limit: graph.growth.max_nodes,
        });
        return outcome;
    }

    let requested = proposal.requested();
    let capacity = expand_budget.min(node_budget);
    let accepted = requested.min(capacity);

    let first = next_instance_index(graph, &parent.path, &template);
    for offset in 0..accepted {
        outcome.accepted.push(AcceptedExpansion {
            template: template.clone(),
            path: instance_path(&parent.path, &template, first.saturating_add(offset)),
            // The label is the author's, verbatim for every child; the instance
            // path is what distinguishes siblings.
            label: if proposal.label.is_empty() {
                template.as_str().to_string()
            } else {
                proposal.label.clone()
            },
            inputs: proposal.inputs.clone(),
            demand: spec.demand,
        });
    }

    if accepted < requested {
        // Tie broken toward the node-level ceiling: it is the more specific and
        // more actionable of the two, and raising it is an authoring edit on
        // the proposing node rather than on the whole version.
        let limit = if expand_budget <= node_budget {
            ExpandLimit::ExpandMax
        } else {
            ExpandLimit::MaxNodes
        };
        outcome.rejected.push(ExpandRejection::Truncated {
            template,
            requested,
            accepted,
            limit,
        });
    }

    outcome
}

/// Applies a decided [`ExpandOutcome`] to the run graph and returns the effects
/// it produced.
///
/// Every accepted child gets, in this order: its `RunNode`, a `sequence`
/// run_edge from its parent so it cannot start before the parent settles, and a
/// copy of each of the parent's **outbound** edges with `from = child` — which
/// is what preserves the fan-in point §3.4 requires (§4 D4). Edges the parent
/// holds toward its *own* earlier children are not inherited: those are spawn
/// edges, and copying them would make each new child wait on its siblings.
///
/// **Write ordering.** `StoreWrite::RunNodeCreated` for a path precedes every
/// other write that names it, in the same effect vector: the store's edge and
/// status writes are find-then-`UPDATE` and error on a missing row (§4 D7).
///
/// **Not emitted here:** `WorkflowEvent::GrowthLimited` and `RunEffect::Notify`.
/// The wire event needs the proposal's `template` and `count`, which the three
/// bare limit rejections do not carry, so it is built by the caller that still
/// holds the [`ExpandProposal`]. `commit` writes the `growth_limited` *journal*
/// entry, which is the durable audit record.
///
/// Scheduling is also the caller's: `commit` leaves new nodes `Pending` and new
/// edges unsettled, and a following [`crate::workflow::engine::schedule::propagate`]
/// pass admits any child whose parent has already succeeded.
pub fn commit(
    graph: &mut RunGraph,
    proposer: RunNodeIdx,
    outcome: &ExpandOutcome,
) -> Vec<RunEffect> {
    let proposal_id = default_proposal_id(graph);
    commit_with_proposal_id(graph, proposer, outcome, &proposal_id)
}

/// [`commit`] with an explicit `spawned.proposal_id`.
///
/// The id is the audit link from a child row back to the `expand_proposed`
/// journal entry that produced it. [`commit`]'s default is
/// `<run>#<journal cursor>`, which is that entry's `seq` exactly when the
/// caller journalled the proposal immediately before committing it — the
/// engine's `ExpandProposed` arm does. A caller that batches differently passes
/// the id it recorded instead of relying on the cursor.
pub fn commit_with_proposal_id(
    graph: &mut RunGraph,
    proposer: RunNodeIdx,
    outcome: &ExpandOutcome,
    proposal_id: &str,
) -> Vec<RunEffect> {
    let mut effects = Vec::new();
    let Some(parent) = graph.node(proposer) else {
        debug!(
            ?proposer,
            "expand commit for a node that is not in the run graph"
        );
        return effects;
    };
    let run = graph.run_id.clone();
    let parent_path = parent.path.clone();
    let parent_key = parent.key.clone();
    let depth = parent.depth.saturating_add(1);

    // Snapshotted before anything is pushed, so a child inherits the parent's
    // authored fan-out and never a sibling's spawn edge.
    let inherited: Vec<InheritedEdge> = graph
        .outbound(proposer)
        .filter_map(|index| graph.edges.get(index))
        .filter(|edge| {
            graph
                .node(edge.to)
                .is_some_and(|target| target.parent != Some(proposer))
        })
        .filter_map(|edge| {
            let target = graph.node(edge.to)?;
            Some(InheritedEdge {
                to: edge.to,
                to_path: target.path.clone(),
                to_key: target.key.clone(),
                kind: edge.kind,
                condition: edge.condition.clone(),
                payload: edge.payload,
                port: edge.port.clone(),
            })
        })
        .collect();

    for accepted in &outcome.accepted {
        // Re-committing the same outcome must not double-create. The path is a
        // pure function of the graph, so a replayed commit addresses the child
        // that already exists.
        if graph.index_of(&accepted.path).is_some() {
            debug!(path = %accepted.path, "expansion child already exists; commit skipped it");
            continue;
        }
        let idx = RunNodeIdx(graph.nodes.len());
        let resolved = graph
            .assignments
            .get(&accepted.template)
            .cloned()
            .unwrap_or_else(|| {
                NodeAssignment::from_assignment(
                    tier::resolve(graph.tier, accepted.demand, None),
                    String::new(),
                )
            });
        graph.nodes.push(RunNode {
            idx,
            key: accepted.template.clone(),
            path: accepted.path.clone(),
            parent: Some(proposer),
            depth,
            // Pending, not Ready: the parent→child sequence edge below is an
            // inbound edge, so admission is `propagate`'s decision.
            status: NodeStatus::Pending,
            assignment: resolved.assignment(),
            assignment_reason: resolved.reason.clone(),
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

        effects.push(RunEffect::Persist(Box::new(StoreWrite::RunNodeCreated {
            run: run.clone(),
            key: accepted.template.clone(),
            path: accepted.path.clone(),
            parent: Some(parent_path.clone()),
            depth,
            status: NodeStatus::Pending,
            demand: accepted.demand,
            assignment: resolved.assignment(),
            assignment_reason: resolved.reason,
            attempt: FIRST_ATTEMPT,
            proposal_id: proposal_id.to_string(),
        })));

        graph.edges.push(RunEdge {
            from: proposer,
            to: idx,
            kind: EdgeKind::Sequence,
            condition: None,
            payload: EdgePayload::Summary,
            port: None,
            condition_result: None,
            fired: false,
        });
        effects.push(RunEffect::Persist(Box::new(StoreWrite::RunEdgeCreated {
            run: run.clone(),
            from: parent_path.clone(),
            to: accepted.path.clone(),
            kind: EdgeKind::Sequence,
            // Synthetic: no authored edge to point at.
            kvdag_edge: None,
            condition_result: None,
            fired: false,
        })));

        for edge in &inherited {
            graph.edges.push(RunEdge {
                from: idx,
                to: edge.to,
                kind: edge.kind,
                condition: edge.condition.clone(),
                payload: edge.payload,
                port: edge.port.clone(),
                // A fresh edge has settled nothing, whatever the parent's copy
                // had already resolved to.
                condition_result: None,
                fired: false,
            });
            effects.push(RunEffect::Persist(Box::new(StoreWrite::RunEdgeCreated {
                run: run.clone(),
                from: accepted.path.clone(),
                to: edge.to_path.clone(),
                kind: edge.kind,
                kvdag_edge: Some((parent_key.clone(), edge.to_key.clone())),
                condition_result: None,
                fired: false,
            })));
        }

        let payload = json!({
            "template": accepted.template.as_str(),
            "parent": parent_path.as_str(),
            "depth": depth,
            "label": accepted.label,
            "proposal_id": proposal_id,
        });
        effects.push(journal(
            graph,
            RunEventKind::NodeCreated,
            Some(accepted.path.clone()),
            payload,
        ));
        let payload = json!({
            "template": accepted.template.as_str(),
            "path": accepted.path.as_str(),
            "label": accepted.label,
            "inputs": accepted.inputs,
            "proposal_id": proposal_id,
        });
        effects.push(journal(
            graph,
            RunEventKind::ExpandAccepted,
            Some(parent_path.clone()),
            payload,
        ));
        effects.push(RunEffect::Emit(WorkflowEvent::NodeCreated {
            run: run.clone(),
            path: accepted.path.clone(),
        }));
    }

    let growth = graph.growth;
    for rejection in &outcome.rejected {
        // `expand_max`'s value is on the rejection when it is the one that
        // fired; a truncation against it reports the ceiling the caller knows.
        let limit_value = rejection
            .limit_value()
            .or_else(|| match rejection.limit()? {
                ExpandLimit::ExpandMax => None,
                other => Some(other.value_in(growth, 0)),
            });
        let message = rejection.message(limit_value);
        let mut payload = json!({
            "reason": rejection.reason_code(),
            "message": message,
            "proposal_id": proposal_id,
        });
        if let Some(object) = payload.as_object_mut() {
            if let Some(template) = rejection.template() {
                object.insert("template".into(), json!(template.as_str()));
            }
            if let Some(limit) = rejection.limit() {
                object.insert("limit".into(), json!(limit.as_str()));
            }
            if let Some(value) = limit_value {
                object.insert("limit_value".into(), json!(value));
            }
            if let Some((requested, accepted)) = rejection.counts() {
                object.insert("requested".into(), json!(requested));
                object.insert("accepted".into(), json!(accepted));
            }
        }
        effects.push(journal(
            graph,
            RunEventKind::ExpandRejected,
            Some(parent_path.clone()),
            payload.clone(),
        ));
        if rejection.limit().is_some() {
            effects.push(journal(
                graph,
                RunEventKind::GrowthLimited,
                Some(parent_path.clone()),
                payload,
            ));
        }
    }

    effects
}

/// One of the parent's outbound edges, resolved to everything a child's copy
/// needs before the graph is mutated.
struct InheritedEdge {
    to: RunNodeIdx,
    to_path: InstancePath,
    to_key: NodeKey,
    kind: EdgeKind,
    condition: Option<Condition>,
    payload: EdgePayload,
    port: Option<String>,
}

/// `<run>#<journal cursor>`; see [`commit_with_proposal_id`].
fn default_proposal_id(graph: &RunGraph) -> String {
    format!("{}#{}", graph.run_id, graph.seq)
}

/// How many children this node has already spawned, across every proposal it
/// has made in this run — `expand_max` is cumulative, not per proposal.
fn children_of(graph: &RunGraph, proposer: RunNodeIdx) -> u16 {
    let count = graph
        .nodes
        .iter()
        .filter(|node| node.parent == Some(proposer))
        .count();
    u16::try_from(count).unwrap_or(u16::MAX)
}

/// `<parent>/<template>/<n>` (§3 frozen interface 8).
fn instance_path(parent: &InstancePath, template: &NodeKey, n: u16) -> InstancePath {
    InstancePath::new(format!("{parent}/{template}/{n}"))
}

/// The next 1-based instance number for `(parent, template)`.
///
/// Derived from the highest number already present rather than from a count, so
/// numbering is monotone within a run: re-proposing the same template after an
/// earlier child failed produces a new instance instead of colliding with the
/// old one, which is what the store's `run_node_instance` unique index and the
/// DAG's selection anchoring both need. Only direct children match — a
/// grandchild's path carries a further `/`, so it never parses as this
/// generation's number.
fn next_instance_index(graph: &RunGraph, parent: &InstancePath, template: &NodeKey) -> u16 {
    let prefix = format!("{parent}/{template}/");
    let highest = graph
        .nodes
        .iter()
        .filter_map(|node| node.path.as_str().strip_prefix(prefix.as_str()))
        .filter_map(|rest| rest.parse::<u16>().ok())
        .max()
        .unwrap_or(0);
    highest.saturating_add(1)
}

/// Whether `{{name}}` appears in `template`.
///
/// Deliberately duplicated from `workflow::model`'s private
/// `scan_placeholders`, exactly as `definition::template_declares_placeholder`
/// is: a yes/no answer is all the §4 D3 override check needs, and the template
/// has already been proved well-formed by `Kvdag::try_new`.
fn template_declares_slot(template: &str, name: &str) -> bool {
    let mut search_from = 0;
    while let Some(start) = template[search_from..].find("{{") {
        let body_start = search_from + start + 2;
        let Some(end_offset) = template[body_start..].find("}}") else {
            break;
        };
        if template[body_start..body_start + end_offset].trim() == name {
            return true;
        }
        search_from = body_start + end_offset + 2;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::engine::tests_support::{kvdag_spec, spec_node, TestNode};
    use crate::workflow::model::{Condition, FieldPath, KvdagEdge, NodeResult, RunId, RunStatus};
    use crate::workflow::tier::Tier;

    const PARENT: &str = "fanout";
    const TEMPLATE: &str = "worker";
    const SINK: &str = "collect";

    /// `fanout → worker(template)` plus the §3.4 fan-in edge `fanout → collect`,
    /// which is the edge an accepted child inherits. `worker` is allowed to
    /// expand itself so the depth guard has a second generation to refuse.
    fn workflow(expand_max: u16, growth: GrowthLimits) -> Kvdag {
        let mut fanout = spec_node(&TestNode::new(PARENT));
        fanout.expand_allow = vec![NodeKey::new(TEMPLATE)];
        fanout.expand_max = expand_max;

        let mut worker = spec_node(&TestNode::new(TEMPLATE));
        worker.prompt_template = "work on {{focus}} toward {{goal}}".to_string();
        worker.is_template = true;
        worker.expand_allow = vec![NodeKey::new(TEMPLATE)];
        worker.expand_max = expand_max;

        let mut collect = spec_node(&TestNode::new(SINK));
        collect.prompt_template = "combine {{findings}}".to_string();

        let mut spec = kvdag_spec(
            vec![fanout, worker, collect],
            vec![
                KvdagEdge {
                    from: NodeKey::new(PARENT),
                    to: NodeKey::new(TEMPLATE),
                    kind: EdgeKind::Data,
                    condition: None,
                    payload: EdgePayload::Summary,
                    port: Some("focus".to_string()),
                },
                KvdagEdge {
                    from: NodeKey::new(PARENT),
                    to: NodeKey::new(SINK),
                    kind: EdgeKind::Conditional,
                    condition: Some(Condition::Exists {
                        path: FieldPath("report".to_string()),
                    }),
                    payload: EdgePayload::Full,
                    port: Some("findings".to_string()),
                },
            ],
        );
        spec.growth = growth;
        Kvdag::try_new(spec).unwrap_or_else(|error| panic!("fixture kvdag: {error}"))
    }

    fn growth(max_depth: u16, max_nodes: u16) -> GrowthLimits {
        GrowthLimits {
            max_depth,
            max_nodes,
        }
    }

    fn run(kvdag: &Kvdag) -> RunGraph {
        RunGraph::materialise(kvdag, RunId::new("workflow_run:1"), Tier::High)
    }

    fn idx(graph: &RunGraph, path: &str) -> RunNodeIdx {
        graph
            .index_of(&InstancePath::new(path))
            .unwrap_or_else(|| panic!("the run graph has a node at {path}"))
    }

    fn proposal(count: Option<u16>) -> ExpandProposal {
        ExpandProposal {
            template: NodeKey::new(TEMPLATE),
            label: "worker child".to_string(),
            inputs: BTreeMap::new(),
            count,
        }
    }

    /// Marks a node `Succeeded` with a validated result, so the parent→child
    /// sequence edge can settle the way the completion gate would leave it.
    fn succeed(graph: &mut RunGraph, path: &str) {
        let at = idx(graph, path);
        let Some(node) = graph.node_mut(at) else {
            return;
        };
        node.status = NodeStatus::Succeeded;
        node.result = Some(NodeResult {
            summary: String::new(),
            artifact_paths: Vec::new(),
            digest: String::new(),
            evidence: crate::workflow::model::Evidence::SelfReport,
            payload: serde_json::json!({ "report": "done" }),
        });
    }

    fn accept(
        graph: &mut RunGraph,
        kvdag: &Kvdag,
        from: &str,
        count: Option<u16>,
    ) -> ExpandOutcome {
        let proposer = idx(graph, from);
        let outcome = evaluate(graph, kvdag, proposer, &proposal(count));
        commit(graph, proposer, &outcome);
        outcome
    }

    fn paths(outcome: &ExpandOutcome) -> Vec<String> {
        outcome
            .accepted
            .iter()
            .map(|accepted| accepted.path.as_str().to_string())
            .collect()
    }

    fn store_writes(effects: &[RunEffect]) -> Vec<&StoreWrite> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                RunEffect::Persist(write) => Some(write.as_ref()),
                _ => None,
            })
            .collect()
    }

    fn journal_kinds(effects: &[RunEffect]) -> Vec<RunEventKind> {
        store_writes(effects)
            .into_iter()
            .filter_map(|write| match write {
                StoreWrite::RunEvent { kind, .. } => Some(*kind),
                _ => None,
            })
            .collect()
    }

    // ── the eight rejections ────────────────────────────────────────────────

    #[test]
    fn a_template_outside_the_proposing_nodes_allow_list_is_not_allowed() {
        let kvdag = workflow(4, GrowthLimits::default());
        let graph = run(&kvdag);
        // `collect` declares no `expand_allow` at all.
        let outcome = evaluate(&graph, &kvdag, idx(&graph, SINK), &proposal(None));

        assert!(outcome.accepted.is_empty());
        assert_eq!(
            outcome.rejected,
            vec![ExpandRejection::NotAllowed {
                template: NodeKey::new(TEMPLATE)
            }]
        );
    }

    #[test]
    fn a_template_the_definition_no_longer_carries_is_unknown() {
        let mut kvdag = workflow(4, GrowthLimits::default());
        let graph = run(&kvdag);
        // `Kvdag::try_new` proves every `expand_allow` entry exists, so this
        // state can only arise from a definition that drifted away from a live
        // run graph — which is exactly what the guard is for.
        kvdag.nodes.retain(|node| node.key.as_str() != TEMPLATE);
        let outcome = evaluate(&graph, &kvdag, idx(&graph, PARENT), &proposal(None));

        assert_eq!(
            outcome.rejected,
            vec![ExpandRejection::UnknownTemplate {
                template: NodeKey::new(TEMPLATE)
            }]
        );
    }

    #[test]
    fn a_key_that_names_a_normal_node_is_not_a_template() {
        let mut kvdag = workflow(4, GrowthLimits::default());
        let graph = run(&kvdag);
        for node in &mut kvdag.nodes {
            if node.key.as_str() == TEMPLATE {
                node.is_template = false;
            }
        }
        let outcome = evaluate(&graph, &kvdag, idx(&graph, PARENT), &proposal(None));

        assert_eq!(
            outcome.rejected,
            vec![ExpandRejection::NotATemplate {
                template: NodeKey::new(TEMPLATE)
            }]
        );
    }

    #[test]
    fn an_input_naming_no_slot_in_the_template_is_refused_and_creates_nothing() {
        let kvdag = workflow(4, GrowthLimits::default());
        let graph = run(&kvdag);
        let mut proposal = proposal(Some(3));
        proposal
            .inputs
            .insert("focus".to_string(), "auth".to_string());
        proposal
            .inputs
            .insert("nonesuch".to_string(), "x".to_string());

        let outcome = evaluate(&graph, &kvdag, idx(&graph, PARENT), &proposal);

        assert!(
            outcome.accepted.is_empty(),
            "an override channel that names nothing renders nothing, so the whole proposal fails"
        );
        assert_eq!(
            outcome.rejected,
            vec![ExpandRejection::UnknownInput {
                template: NodeKey::new(TEMPLATE),
                name: "nonesuch".to_string(),
            }],
            "\"focus\" is a real inbound port on the template and must be accepted"
        );
    }

    #[test]
    fn a_node_that_may_not_expand_reports_its_own_zero_ceiling() {
        let kvdag = workflow(0, GrowthLimits::default());
        let graph = run(&kvdag);
        let outcome = evaluate(&graph, &kvdag, idx(&graph, PARENT), &proposal(None));

        assert_eq!(
            outcome.rejected,
            vec![ExpandRejection::ExpandMaxReached { limit: 0 }],
            "expand_max defaults to 0: expansion is opt-in per node"
        );
    }

    #[test]
    fn a_child_past_the_depth_ceiling_is_refused_rather_than_truncated() {
        let kvdag = workflow(4, growth(0, 24));
        let graph = run(&kvdag);
        let outcome = evaluate(&graph, &kvdag, idx(&graph, PARENT), &proposal(Some(4)));

        assert!(outcome.accepted.is_empty());
        assert_eq!(
            outcome.rejected,
            vec![ExpandRejection::MaxDepthReached { limit: 0 }],
            "depth is not a budget: no count of children fits under an exhausted ceiling"
        );
    }

    #[test]
    fn a_full_run_refuses_the_whole_proposal() {
        // Two static nodes materialise, so `max_nodes = 2` leaves no budget.
        let kvdag = workflow(4, growth(3, 2));
        let graph = run(&kvdag);
        let outcome = evaluate(&graph, &kvdag, idx(&graph, PARENT), &proposal(Some(2)));

        assert_eq!(
            outcome.rejected,
            vec![ExpandRejection::MaxNodesReached { limit: 2 }]
        );
    }

    #[test]
    fn a_proposal_that_does_not_fit_is_partially_accepted_and_reports_the_shortfall() {
        let kvdag = workflow(2, growth(3, 24));
        let mut graph = run(&kvdag);
        let outcome = accept(&mut graph, &kvdag, PARENT, Some(4));

        assert_eq!(
            paths(&outcome),
            vec!["fanout/worker/1".to_string(), "fanout/worker/2".to_string()],
            "never accept-all: the ceiling holds"
        );
        assert_eq!(
            outcome.rejected,
            vec![ExpandRejection::Truncated {
                template: NodeKey::new(TEMPLATE),
                requested: 4,
                accepted: 2,
                limit: ExpandLimit::ExpandMax,
            }],
            "never reject-all, and never silently: the shortfall is reported"
        );
        assert_eq!(graph.nodes.len(), 4);
    }

    #[test]
    fn a_truncation_names_whichever_ceiling_actually_bound_it() {
        // Node budget (3 - 2 static = 1) is tighter than the node's own
        // expand_max of 8.
        let kvdag = workflow(8, growth(3, 3));
        let graph = run(&kvdag);
        let outcome = evaluate(&graph, &kvdag, idx(&graph, PARENT), &proposal(Some(4)));

        assert_eq!(outcome.accepted.len(), 1);
        assert_eq!(
            outcome.rejected,
            vec![ExpandRejection::Truncated {
                template: NodeKey::new(TEMPLATE),
                requested: 4,
                accepted: 1,
                limit: ExpandLimit::MaxNodes,
            }]
        );
    }

    #[test]
    fn every_rejection_carries_a_reason_code_and_a_message() {
        let rejections = vec![
            ExpandRejection::NotAllowed {
                template: NodeKey::new(TEMPLATE),
            },
            ExpandRejection::UnknownTemplate {
                template: NodeKey::new(TEMPLATE),
            },
            ExpandRejection::NotATemplate {
                template: NodeKey::new(TEMPLATE),
            },
            ExpandRejection::ExpandMaxReached { limit: 4 },
            ExpandRejection::MaxDepthReached { limit: 3 },
            ExpandRejection::MaxNodesReached { limit: 24 },
            ExpandRejection::Truncated {
                template: NodeKey::new(TEMPLATE),
                requested: 4,
                accepted: 2,
                limit: ExpandLimit::MaxNodes,
            },
            ExpandRejection::UnknownInput {
                template: NodeKey::new(TEMPLATE),
                name: "nonesuch".to_string(),
            },
        ];
        let codes: Vec<&str> = rejections
            .iter()
            .map(ExpandRejection::reason_code)
            .collect();

        assert_eq!(
            codes,
            vec![
                "not_allowed",
                "unknown_template",
                "not_a_template",
                "expand_max_reached",
                "max_depth_reached",
                "max_nodes_reached",
                "truncated",
                "unknown_input",
            ],
            "the wire's WorkflowExpandRejectionReason spellings, mapped in one place"
        );
        for rejection in &rejections {
            assert!(!rejection.message(rejection.limit_value()).is_empty());
        }
        assert_eq!(
            rejections[6].message(Some(12)),
            "max_nodes 12 reached; 2 of 4 requested nodes created",
            "Truncated carries no ceiling of its own, so the caller supplies it"
        );
        let limits: Vec<Option<ExpandLimit>> =
            rejections.iter().map(ExpandRejection::limit).collect();
        assert_eq!(
            limits.iter().filter(|limit| limit.is_some()).count(),
            4,
            "the three hard limits plus a truncation are the growth-limited set"
        );
    }

    #[test]
    fn a_limits_value_is_read_from_the_run_and_the_proposing_node() {
        let limits = growth(3, 24);
        assert_eq!(ExpandLimit::ExpandMax.value_in(limits, 4), 4);
        assert_eq!(ExpandLimit::MaxDepth.value_in(limits, 4), 3);
        assert_eq!(ExpandLimit::MaxNodes.value_in(limits, 4), 24);
    }

    // ── budgets ─────────────────────────────────────────────────────────────

    #[test]
    fn expand_max_is_cumulative_across_a_nodes_proposals() {
        let kvdag = workflow(3, growth(3, 24));
        let mut graph = run(&kvdag);

        let first = accept(&mut graph, &kvdag, PARENT, Some(2));
        assert_eq!(first.accepted.len(), 2);
        assert!(first.rejected.is_empty());

        let second = accept(&mut graph, &kvdag, PARENT, Some(2));
        assert_eq!(
            second.accepted.len(),
            1,
            "one of the node's three slots is left"
        );
        assert_eq!(
            second.rejected,
            vec![ExpandRejection::Truncated {
                template: NodeKey::new(TEMPLATE),
                requested: 2,
                accepted: 1,
                limit: ExpandLimit::ExpandMax,
            }]
        );

        let third = evaluate(&graph, &kvdag, idx(&graph, PARENT), &proposal(Some(1)));
        assert_eq!(
            third.rejected,
            vec![ExpandRejection::ExpandMaxReached { limit: 3 }]
        );
    }

    #[test]
    fn expand_max_is_counted_per_proposing_node_not_per_run() {
        let kvdag = workflow(1, growth(3, 24));
        let mut graph = run(&kvdag);
        accept(&mut graph, &kvdag, PARENT, Some(1));

        // The child is a `worker` too, with its own untouched budget.
        let child = evaluate(
            &graph,
            &kvdag,
            idx(&graph, "fanout/worker/1"),
            &proposal(Some(1)),
        );
        assert_eq!(paths(&child), vec!["fanout/worker/1/worker/1".to_string()]);
    }

    #[test]
    fn the_node_budget_is_monotone_so_a_failed_child_refunds_nothing() {
        let kvdag = workflow(8, growth(3, 3));
        let mut graph = run(&kvdag);
        accept(&mut graph, &kvdag, PARENT, Some(1));
        assert_eq!(graph.nodes.len(), 3);

        let failed = idx(&graph, "fanout/worker/1");
        if let Some(node) = graph.node_mut(failed) {
            node.status = NodeStatus::Failed;
        }

        let outcome = evaluate(&graph, &kvdag, idx(&graph, PARENT), &proposal(Some(1)));
        assert_eq!(
            outcome.rejected,
            vec![ExpandRejection::MaxNodesReached { limit: 3 }],
            "a node could otherwise fan out indefinitely by failing"
        );
    }

    #[test]
    fn static_nodes_stay_at_depth_zero_and_generations_count_from_one() {
        let kvdag = workflow(4, growth(2, 24));
        let mut graph = run(&kvdag);
        assert!(graph.nodes.iter().all(|node| node.depth == 0));

        accept(&mut graph, &kvdag, PARENT, Some(1));
        assert_eq!(
            graph.node(idx(&graph, "fanout/worker/1")).map(|n| n.depth),
            Some(1)
        );

        accept(&mut graph, &kvdag, "fanout/worker/1", Some(1));
        assert_eq!(
            graph
                .node(idx(&graph, "fanout/worker/1/worker/1"))
                .map(|n| n.depth),
            Some(2),
            "max_depth = 2 permits two generations"
        );

        let third = evaluate(
            &graph,
            &kvdag,
            idx(&graph, "fanout/worker/1/worker/1"),
            &proposal(Some(1)),
        );
        assert_eq!(
            third.rejected,
            vec![ExpandRejection::MaxDepthReached { limit: 2 }]
        );
    }

    #[test]
    fn a_tier_narrowed_node_ceiling_is_the_one_the_guardrail_enforces() {
        let kvdag = workflow(40, growth(3, 40));
        let graph = RunGraph::materialise(&kvdag, RunId::new("workflow_run:1"), Tier::Low);
        assert_eq!(graph.growth.max_nodes, 12, "low narrows to 12");

        let outcome = evaluate(&graph, &kvdag, idx(&graph, PARENT), &proposal(Some(40)));
        assert_eq!(
            outcome.accepted.len(),
            10,
            "12 minus the two static nodes already materialised"
        );
    }

    // ── instance paths ──────────────────────────────────────────────────────

    #[test]
    fn instance_paths_are_one_based_per_parent_and_template() {
        let kvdag = workflow(8, growth(3, 24));
        let mut graph = run(&kvdag);

        let first = accept(&mut graph, &kvdag, PARENT, Some(2));
        assert_eq!(
            paths(&first),
            vec!["fanout/worker/1".to_string(), "fanout/worker/2".to_string()]
        );

        let second = accept(&mut graph, &kvdag, PARENT, Some(1));
        assert_eq!(
            paths(&second),
            vec!["fanout/worker/3".to_string()],
            "numbering is monotone within a run, never reused"
        );

        // A grandchild's path also starts with `fanout/worker/`, and must not
        // disturb the parent's numbering.
        accept(&mut graph, &kvdag, "fanout/worker/1", Some(1));
        let third = accept(&mut graph, &kvdag, PARENT, Some(1));
        assert_eq!(paths(&third), vec!["fanout/worker/4".to_string()]);
    }

    #[test]
    fn a_failed_child_does_not_hand_its_number_back() {
        let kvdag = workflow(8, growth(3, 24));
        let mut graph = run(&kvdag);
        accept(&mut graph, &kvdag, PARENT, Some(1));
        let child = idx(&graph, "fanout/worker/1");
        if let Some(node) = graph.node_mut(child) {
            node.status = NodeStatus::Failed;
        }

        let retry = accept(&mut graph, &kvdag, PARENT, Some(1));
        assert_eq!(
            paths(&retry),
            vec!["fanout/worker/2".to_string()],
            "the store's run_node_instance index is unique per (run, path)"
        );
    }

    // ── commit ──────────────────────────────────────────────────────────────

    #[test]
    fn a_child_gets_a_sequence_edge_from_its_parent_and_inherits_the_fan_in() {
        let kvdag = workflow(4, growth(3, 24));
        let mut graph = run(&kvdag);
        let parent = idx(&graph, PARENT);
        let outcome = evaluate(&graph, &kvdag, parent, &proposal(Some(1)));
        commit(&mut graph, parent, &outcome);

        let child = idx(&graph, "fanout/worker/1");
        let sink = idx(&graph, SINK);
        let inbound: Vec<&RunEdge> = graph
            .inbound(child)
            .filter_map(|index| graph.edges.get(index))
            .collect();
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0].from, parent);
        assert_eq!(
            inbound[0].kind,
            EdgeKind::Sequence,
            "a child cannot start before its parent settles"
        );

        let outbound: Vec<&RunEdge> = graph
            .outbound(child)
            .filter_map(|index| graph.edges.get(index))
            .collect();
        assert_eq!(outbound.len(), 1, "the parent's fan-in point is preserved");
        let authored = graph
            .edges
            .iter()
            .find(|edge| edge.from == parent && edge.to == sink)
            .expect("the authored fanout -> collect edge");
        assert_eq!(outbound[0].to, sink);
        assert_eq!(outbound[0].kind, authored.kind);
        assert_eq!(outbound[0].payload, authored.payload);
        assert_eq!(outbound[0].port, authored.port);
        assert_eq!(outbound[0].condition, authored.condition);
        assert_eq!(outbound[0].condition_result, None);
        assert!(!outbound[0].fired, "a fresh edge has settled nothing");
    }

    #[test]
    fn a_second_child_inherits_the_parents_edges_and_never_its_siblings() {
        let kvdag = workflow(4, growth(3, 24));
        let mut graph = run(&kvdag);
        accept(&mut graph, &kvdag, PARENT, Some(1));
        accept(&mut graph, &kvdag, PARENT, Some(1));

        let first = idx(&graph, "fanout/worker/1");
        let second = idx(&graph, "fanout/worker/2");
        let sink = idx(&graph, SINK);
        let targets: Vec<RunNodeIdx> = graph
            .outbound(second)
            .filter_map(|index| graph.edges.get(index))
            .map(|edge| edge.to)
            .collect();
        assert_eq!(targets, vec![sink]);
        assert!(
            !targets.contains(&first),
            "a spawn edge is not part of the parent's authored fan-out"
        );
        assert_eq!(
            graph.inbound(sink).count(),
            3,
            "both children fan in beside their parent"
        );
    }

    #[test]
    fn a_child_starts_pending_and_is_admitted_once_its_parent_settles() {
        let kvdag = workflow(4, growth(3, 24));
        let mut graph = run(&kvdag);
        accept(&mut graph, &kvdag, PARENT, Some(1));
        assert_eq!(
            graph.node(idx(&graph, "fanout/worker/1")).map(|n| n.status),
            Some(NodeStatus::Pending)
        );

        succeed(&mut graph, PARENT);
        crate::workflow::engine::schedule::propagate(&mut graph);
        assert_eq!(
            graph.node(idx(&graph, "fanout/worker/1")).map(|n| n.status),
            Some(NodeStatus::Ready),
            "scheduling stays the scheduler's job; commit only adds the edge"
        );
    }

    #[test]
    fn the_create_write_precedes_every_other_write_for_the_same_path() {
        let kvdag = workflow(4, growth(3, 24));
        let mut graph = run(&kvdag);
        let parent = idx(&graph, PARENT);
        let outcome = evaluate(&graph, &kvdag, parent, &proposal(Some(2)));
        let effects = commit(&mut graph, parent, &outcome);

        let mut seen: Vec<String> = Vec::new();
        for write in store_writes(&effects) {
            match write {
                StoreWrite::RunNodeCreated { path, .. } => seen.push(path.as_str().to_string()),
                StoreWrite::RunEdgeCreated { from, to, .. } => {
                    for endpoint in [from, to] {
                        if endpoint.as_str().contains('/') {
                            assert!(
                                seen.contains(&endpoint.as_str().to_string()),
                                "{endpoint} is written before its create"
                            );
                        }
                    }
                }
                _ => {}
            }
        }
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn a_committed_child_carries_its_provenance_and_the_run_assignment() {
        let kvdag = workflow(4, growth(3, 24));
        let mut graph = run(&kvdag);
        let parent = idx(&graph, PARENT);
        let outcome = evaluate(&graph, &kvdag, parent, &proposal(Some(1)));
        let effects = commit(&mut graph, parent, &outcome);

        let created = store_writes(&effects)
            .into_iter()
            .find_map(|write| match write {
                StoreWrite::RunNodeCreated { .. } => Some(write.clone()),
                _ => None,
            })
            .expect("the child's create write");
        let StoreWrite::RunNodeCreated {
            key,
            path,
            parent: parent_path,
            depth,
            status,
            assignment,
            attempt,
            proposal_id,
            ..
        } = created
        else {
            panic!("matched above");
        };
        assert_eq!(key, NodeKey::new(TEMPLATE));
        assert_eq!(path, InstancePath::new("fanout/worker/1"));
        assert_eq!(parent_path, Some(InstancePath::new(PARENT)));
        assert_eq!(depth, 1);
        assert_eq!(status, NodeStatus::Pending);
        assert_eq!(attempt, 1);
        assert!(
            proposal_id.starts_with("workflow_run:1#"),
            "the audit link back to the expand_proposed entry"
        );
        assert_eq!(
            Some(assignment),
            graph
                .assignments
                .get(&NodeKey::new(TEMPLATE))
                .map(NodeAssignment::assignment),
            "the child resolves from the run's table, never from a mid-run lookup"
        );
    }

    #[test]
    fn an_acceptance_journals_and_announces_every_child_it_created() {
        let kvdag = workflow(4, growth(3, 24));
        let mut graph = run(&kvdag);
        let parent = idx(&graph, PARENT);
        let outcome = evaluate(&graph, &kvdag, parent, &proposal(Some(2)));
        let effects = commit(&mut graph, parent, &outcome);

        assert_eq!(
            journal_kinds(&effects),
            vec![
                RunEventKind::NodeCreated,
                RunEventKind::ExpandAccepted,
                RunEventKind::NodeCreated,
                RunEventKind::ExpandAccepted,
            ]
        );
        let announced: Vec<InstancePath> = effects
            .iter()
            .filter_map(|effect| match effect {
                RunEffect::Emit(WorkflowEvent::NodeCreated { path, .. }) => Some(path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            announced,
            vec![
                InstancePath::new("fanout/worker/1"),
                InstancePath::new("fanout/worker/2"),
            ],
            "workflow.node.created already means a node entered the run graph"
        );
    }

    #[test]
    fn a_growth_rejection_is_journalled_twice_and_a_validation_failure_once() {
        let kvdag = workflow(1, growth(3, 24));
        let mut graph = run(&kvdag);
        let parent = idx(&graph, PARENT);
        let outcome = evaluate(&graph, &kvdag, parent, &proposal(Some(3)));
        let effects = commit(&mut graph, parent, &outcome);

        assert_eq!(
            journal_kinds(&effects),
            vec![
                RunEventKind::NodeCreated,
                RunEventKind::ExpandAccepted,
                RunEventKind::ExpandRejected,
                RunEventKind::GrowthLimited,
            ]
        );

        let refusal = ExpandOutcome {
            accepted: Vec::new(),
            rejected: vec![ExpandRejection::NotAllowed {
                template: NodeKey::new(TEMPLATE),
            }],
        };
        assert_eq!(
            journal_kinds(&commit(&mut graph, parent, &refusal)),
            vec![RunEventKind::ExpandRejected],
            "a validation failure is not a growth limit"
        );
    }

    #[test]
    fn a_growth_limited_payload_names_the_ceiling_that_bound_the_proposal() {
        let kvdag = workflow(8, growth(3, 3));
        let mut graph = run(&kvdag);
        let parent = idx(&graph, PARENT);
        let outcome = evaluate(&graph, &kvdag, parent, &proposal(Some(4)));
        let effects = commit(&mut graph, parent, &outcome);

        let payload = store_writes(&effects)
            .into_iter()
            .find_map(|write| match write {
                StoreWrite::RunEvent {
                    kind: RunEventKind::GrowthLimited,
                    payload,
                    ..
                } => Some(payload.clone()),
                _ => None,
            })
            .expect("a growth_limited journal entry");
        assert_eq!(payload["reason"], "truncated");
        assert_eq!(payload["limit"], "max_nodes");
        assert_eq!(payload["limit_value"], 3);
        assert_eq!(payload["requested"], 4);
        assert_eq!(payload["accepted"], 1);
        assert_eq!(
            payload["message"],
            "max_nodes 3 reached; 1 of 4 requested nodes created"
        );
    }

    #[test]
    fn re_committing_the_same_outcome_creates_nothing_twice() {
        let kvdag = workflow(4, growth(3, 24));
        let mut graph = run(&kvdag);
        let parent = idx(&graph, PARENT);
        let outcome = evaluate(&graph, &kvdag, parent, &proposal(Some(1)));
        commit(&mut graph, parent, &outcome);
        let nodes = graph.nodes.len();
        let edges = graph.edges.len();

        let replay = commit(&mut graph, parent, &outcome);
        assert_eq!(graph.nodes.len(), nodes);
        assert_eq!(graph.edges.len(), edges);
        assert!(store_writes(&replay)
            .into_iter()
            .all(|write| !matches!(write, StoreWrite::RunNodeCreated { .. })));
    }

    #[test]
    fn evaluate_mutates_nothing() {
        let kvdag = workflow(4, growth(3, 24));
        let graph = run(&kvdag);
        let before = graph.clone();
        let outcome = evaluate(&graph, &kvdag, idx(&graph, PARENT), &proposal(Some(2)));

        assert_eq!(outcome.accepted.len(), 2);
        assert_eq!(graph, before);
        assert_eq!(graph.status, RunStatus::Pending);
    }

    #[test]
    fn a_count_of_zero_still_asks_for_one_child() {
        assert_eq!(proposal(None).requested(), 1);
        assert_eq!(proposal(Some(0)).requested(), 1);
        assert_eq!(proposal(Some(7)).requested(), 7);
    }

    #[test]
    fn a_proposal_from_a_node_outside_the_run_graph_decides_nothing() {
        let kvdag = workflow(4, growth(3, 24));
        let mut graph = run(&kvdag);
        let stranger = RunNodeIdx(99);

        assert!(evaluate(&graph, &kvdag, stranger, &proposal(None)).is_empty());
        assert!(commit(&mut graph, stranger, &ExpandOutcome::default()).is_empty());
    }

    #[test]
    fn a_slot_is_recognised_with_or_without_inner_whitespace() {
        assert!(template_declares_slot("work on {{focus}}", "focus"));
        assert!(template_declares_slot("work on {{ focus }}", "focus"));
        assert!(!template_declares_slot("work on {{focus}}", "goal"));
        assert!(!template_declares_slot("work on {{focus", "focus"));
    }
}
