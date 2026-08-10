//! The live workflow DAG overlay
//! (`docs/design/workflow-builder/05-phase-plan.md` W6,
//! `docs/design/workflow-builder/04-kvdag-and-execution.md` §8).
//!
//! Two halves, deliberately split:
//!
//! - [`compute_workflow_dag_view`] runs in the view-computation pass, the one
//!   place allowed to mutate `AppState`. It runs the pure layered layout once
//!   and stores the resulting rectangles, edge cells, and the node projection
//!   into `ViewState`.
//! - [`render_workflow_dag`] takes `&AppState` and only draws, reading exactly
//!   the geometry that was stored. The mouse hit-test reads the same geometry,
//!   so what is clickable can never disagree with what was drawn.
//!
//! Node boxes are ratatui `Block`s and edges are direction-bit cells resolved
//! through [`line_cell_symbol`]; `ratatui::widgets::canvas::Canvas` is
//! deliberately not used (`00-overview.md` D9).

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use super::line_cells::{line_cell_symbol, LineCell};
use super::text::{display_width, truncate_end};
use super::widgets::panel_contrast_fg;
use crate::app::state::{
    AppState, DagInterrogationView, DagNodeView, DagRunCounts, DagViewState,
    HistoricalInterrogation, Mode, Palette, ProjectedNodeFacts, WorkflowRunPresentation,
};
use crate::workflow::layout::{
    detached_lane, detached_lane_height, layout, DagLayout, EdgeBits, LayoutRect,
};
use crate::workflow::model::{
    EpiloguePhase, NodeStatus, RestoredRef, RunGraph, RunNode, RunNodeIdx, RunStatus, Succession,
    RESERVED_PATH_PREFIX,
};

const HEADER_HEIGHT: u16 = 1;
/// Blocker line, status line, model/usage line, summary line.
const DETAIL_HEIGHT: u16 = 4;
const FOOTER_HEIGHT: u16 = 1;
/// The run banner: one line, the run's last growth limit. Allocated only when
/// a banner is present (`06-phase2-plan.md` §1 WS-G, §3 frozen interface 10).
const BANNER_HEIGHT: u16 = 1;

/// Which way a graph-aware navigation key moves the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DagNavDirection {
    Up,
    Down,
    Left,
    Right,
}

// ── view computation (mutation pass) ────────────────────────────────────────

/// Lays the run graph out once and projects it into [`DagViewState`].
///
/// Returns an empty state when the overlay is closed, so no other mode ever
/// hit-tests against stale DAG geometry.
pub(super) fn compute_workflow_dag_view(app: &AppState, area: Rect) -> DagViewState {
    if app.mode != Mode::WorkflowDag {
        return DagViewState::default();
    }

    let previous = &app.view.dag;
    // Whichever run this frame is projecting. A historical snapshot wins when
    // one is open: the DAG view has exactly one graph on screen at a time, and
    // "the past run the user just opened" is the one they asked for
    // (`07-phase3-plan.md` §1 WS-H).
    let historical = app.historical_run();
    // The run's last growth limit, formatted for the banner band. `None` costs
    // zero rows (§3 frozen interface 10). Mirrored onto
    // `WorkflowRunPresentation` alongside the run graph, the same way a refused
    // delivery is — and gated on there being a graph to banner, so a limit left
    // over from a run that has since been cleared cannot head an empty overlay.
    //
    // A historical run never banners: the mirrored limit belongs to the
    // *active* run, and hanging it over a past run's graph would attribute a
    // guardrail breach to the wrong run.
    let banner: Option<String> = if historical.is_some() {
        None
    } else {
        app.workflow_run_graph()
            .and(app.workflow_run_presentation().growth_banner.clone())
    };
    let (header_rect, banner_rect, graph_rect, detail_rect, footer_rect) =
        overlay_areas(area, banner.as_deref());
    let mut view = DagViewState {
        header_rect,
        banner_rect,
        graph_rect,
        detail_rect,
        footer_rect,
        banner,
        historical: historical.is_some(),
        // A Claude Code team lead executed this run rather than karvex's own
        // engine (§3.1). Read off the snapshot rather than guessed from the
        // graph: the graph looks the same either way, and which verbs the
        // overlay may offer depends entirely on which engine ran it.
        lead_run: historical.is_some_and(|snapshot| snapshot.is_lead_run()),
        ..DagViewState::default()
    };

    let Some(graph) = historical
        .map(|snapshot| snapshot.graph.as_ref())
        .or_else(|| app.workflow_run_graph())
    else {
        return view;
    };
    // A past run carries its own name but none of the live mirror's
    // presentation: node labels, refused deliveries, and growth notices are all
    // facts about a run that is currently executing. Reusing the live
    // presentation here would label a past run's nodes with the active run's
    // labels, which is worse than showing the keys.
    let historical_presentation;
    let presentation: &WorkflowRunPresentation = match historical {
        Some(snapshot) => {
            historical_presentation = WorkflowRunPresentation {
                workflow_name: snapshot.workflow_name.clone(),
                ..WorkflowRunPresentation::default()
            };
            &historical_presentation
        }
        None => app.workflow_run_presentation(),
    };
    // Interrogation panes live on `HistoricalRunSnapshot` only: the live run's
    // interrogations are tracked on `WorkflowRuntimeState` and are not mirrored
    // into `AppState` (the shape landed in step 1b says so). An empty slice
    // costs the graph band zero rows.
    let interrogations: &[HistoricalInterrogation] = historical
        .map(|snapshot| snapshot.interrogations.as_slice())
        .unwrap_or(&[]);
    let lane_height = lane_height_for(graph_rect.height, interrogations.len());
    let graph_band = Rect::new(
        graph_rect.x,
        graph_rect.y,
        graph_rect.width,
        graph_rect.height.saturating_sub(lane_height),
    );
    let lane_rect = Rect::new(
        graph_rect.x,
        graph_band.bottom(),
        graph_rect.width,
        lane_height,
    );

    view.run_id = graph.run_id.as_str().to_string();
    view.workflow_name = presentation.workflow_name.clone();
    view.run_status = Some(graph.status);
    // Counted over the whole graph, before clipping: a resize must never
    // change the answer the header gives about the run.
    view.counts = run_counts(graph);
    view.layout = clipped_layout(graph, graph_band);
    view.interrogation_nodes = project_interrogations(interrogations, lane_rect);
    let now_unix_ms = current_unix_ms();
    view.nodes = graph
        .nodes
        .iter()
        .filter(|node| view.layout.rect_of(node.idx).is_some())
        .map(|node| project_node(graph, node, presentation, now_unix_ms))
        .collect();
    // Only a lead run has a projection to merge, and skipping the walk for an
    // engine-era run is not just an optimisation: it is what guarantees a run
    // karvex's own engine executed keeps rendering exactly as it did before the
    // rework, down to the fields the merge would otherwise fill in.
    if view.lead_run {
        if let Some(snapshot) = historical {
            merge_projection(app, &mut view.nodes, &snapshot.projected, &snapshot.members);
        }
    }
    view.selected = carried_selection(previous, &view);
    // The steer line only survives while it still has a node to steer.
    view.steer = if view.selected.is_some() {
        previous.steer.clone()
    } else {
        None
    };
    view.last_click = previous.last_click;
    view
}

/// Whole-graph status tallies. Deliberately taken from `graph`, never from the
/// clipped projection.
fn run_counts(graph: &RunGraph) -> DagRunCounts {
    let mut counts = DagRunCounts {
        total: graph.nodes.len(),
        ..DagRunCounts::default()
    };
    for node in &graph.nodes {
        match node.status {
            NodeStatus::Running => counts.running += 1,
            NodeStatus::Failed => counts.failed += 1,
            NodeStatus::NeedsAttention => counts.needs_attention += 1,
            _ => {}
        }
    }
    counts
}

fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Splits the full-bleed overlay into its five bands, in priority order
/// `footer → banner → graph → detail`: a short terminal loses the detail
/// strip before the graph, the graph before the banner, and the banner before
/// the footer — the escape hatch is the last thing to go.
///
/// The banner band is allocated only when `banner.is_some()`, so a `None`
/// banner returns rects byte-identical to the pre-Phase-2 four-band layout —
/// every existing pinned geometry number stays exact
/// (`06-phase2-plan.md` §3 frozen interface 10). Only the banner's presence
/// affects geometry; its text does not, so the parameter is `Option<&str>`
/// rather than a bare `bool` to keep the call site self-explanatory.
fn overlay_areas(area: Rect, banner: Option<&str>) -> (Rect, Rect, Rect, Rect, Rect) {
    let header = Rect::new(area.x, area.y, area.width, HEADER_HEIGHT.min(area.height));
    let mut remaining = area.height.saturating_sub(header.height);

    let footer_height = FOOTER_HEIGHT.min(remaining);
    remaining = remaining.saturating_sub(footer_height);

    let banner_height = if banner.is_some() {
        BANNER_HEIGHT.min(remaining)
    } else {
        0
    };
    remaining = remaining.saturating_sub(banner_height);

    // The graph keeps at least one row; the detail strip yields first.
    let detail_height = DETAIL_HEIGHT.min(remaining.saturating_sub(1));
    remaining = remaining.saturating_sub(detail_height);

    let banner_rect = Rect::new(area.x, header.bottom(), area.width, banner_height);
    let graph = Rect::new(area.x, banner_rect.bottom(), area.width, remaining);
    let detail = Rect::new(area.x, graph.bottom(), area.width, detail_height);
    let footer = Rect::new(area.x, detail.bottom(), area.width, footer_height);
    (header, banner_rect, graph, detail, footer)
}

/// Layout, then drop everything the graph band cannot show.
///
/// Phase 1 has no panning or scrolling, so a node whose box does not fit is
/// simply not part of this frame — and dropping it from the stored geometry is
/// what keeps the hit-test honest: nothing invisible stays clickable.
fn clipped_layout(graph: &RunGraph, graph_rect: Rect) -> DagLayout {
    let bounds = to_layout_rect(graph_rect);
    let mut dag = layout(graph, bounds);
    dag.nodes.retain(|(_, rect)| contains_rect(bounds, *rect));
    // Endpoint survival, not just bounds: an edge whose box was clipped away
    // would otherwise leave a stub pointing at nothing.
    dag.retain_visible_edges(bounds);
    dag
}

/// Rows the interrogation lane may take out of the bottom of the graph band.
///
/// Zero when there is nothing to draw, which is what keeps every pinned graph
/// geometry number byte-identical for the runs that have no interrogations
/// (`07-phase3-plan.md` §1 WS-H, the banner's precedent). And zero again when
/// the band is too short to afford the lane *and* leave the graph its one
/// guaranteed row — the same yields-before-the-graph rule
/// [`overlay_areas`] applies to the detail strip. Carving a lane the boxes
/// cannot fit into would cost the graph rows and draw nothing with them.
fn lane_height_for(graph_height: u16, count: usize) -> u16 {
    let wanted = detached_lane_height(count);
    if wanted > 0 && graph_height > wanted {
        wanted
    } else {
        0
    }
}

/// Interrogation rows → the boxes the lane will draw, in row order.
///
/// The rects come from [`detached_lane`], which drops any box that does not
/// fit; zipping consumes the shorter of the two, so a dropped box is simply
/// absent from the projection — and therefore from the hit-test — rather than
/// clipped into something invisible that still answers clicks.
fn project_interrogations(
    rows: &[HistoricalInterrogation],
    lane: Rect,
) -> Vec<DagInterrogationView> {
    if rows.is_empty() || lane.height == 0 || lane.width == 0 {
        return Vec::new();
    }
    rows.iter()
        .zip(detached_lane(to_layout_rect(lane), rows.len()))
        .map(|(row, rect)| DagInterrogationView {
            id: row.id.clone(),
            path: row.path.clone(),
            label: interrogation_label(row),
            pane_id: row.pane_id.clone(),
            rect: to_rect(rect),
            ended: row.ended,
        })
        .collect()
}

/// What the box calls itself. A reconstructed session says so in its own title
/// — 00 Feature 3's "never presented as the original", made mechanical
/// (`07-phase3-plan.md` §4 D7).
fn interrogation_label(row: &HistoricalInterrogation) -> String {
    let verb = if row.reconstructed {
        "reconstructed"
    } else {
        "interrogate"
    };
    format!("{verb} · {}", row.path)
}

/// The kind word [`interrogation_label`] put at the front of the label.
///
/// The `DagInterrogationView` shape landed in step 1b carries no
/// `reconstructed` flag, so the label is where the distinction lives and the
/// only place the box can read it back. The separator never appears in the
/// kind, so the first split is always the right one.
fn interrogation_kind(item: &DagInterrogationView) -> &str {
    item.label
        .split_once(" · ")
        .map(|(kind, _)| kind)
        .unwrap_or(item.label.as_str())
}

/// Hit-test the interrogation lane.
///
/// Deliberately a separate lookup from [`DagViewState::node_at`] over a
/// separate vec: an interrogation is not a `RunGraph` node (§4 D8), so a click
/// on one must never resolve to a `RunNodeIdx` that some other caller would
/// then steer, restart, or select.
pub(crate) fn workflow_dag_interrogation_at(
    view: &DagViewState,
    col: u16,
    row: u16,
) -> Option<&DagInterrogationView> {
    view.interrogation_nodes
        .iter()
        .find(|item| rect_contains(item.rect, col, row))
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && col >= rect.x
        && col < rect.right()
        && row >= rect.y
        && row < rect.bottom()
}

fn contains_rect(bounds: LayoutRect, rect: LayoutRect) -> bool {
    !rect.is_empty()
        && rect.x >= bounds.x
        && rect.y >= bounds.y
        && rect.right() <= bounds.right()
        && rect.bottom() <= bounds.bottom()
}

fn project_node(
    graph: &RunGraph,
    node: &RunNode,
    presentation: &WorkflowRunPresentation,
    now_unix_ms: u64,
) -> DagNodeView {
    let node_labels = &presentation.node_labels;
    let successors = graph
        .outbound(node.idx)
        .filter_map(|edge_index| graph.edges.get(edge_index))
        .map(|edge| edge.to)
        .collect();
    let predecessors = graph
        .inbound(node.idx)
        .filter_map(|edge_index| graph.edges.get(edge_index))
        .map(|edge| edge.from)
        .collect();

    DagNodeView {
        idx: node.idx,
        path: node.path.as_str().to_string(),
        // Per instance first, per key second. A generation cut from one
        // template shares that template's key, so reading the map by key drew
        // N identical boxes for N children the proposing node had named apart;
        // `mirror_workflow_run_graph` keys every run node by its instance path
        // for exactly that reason. The per-key entry stays as the fallback for
        // a node the run graph does not carry — and a static node's path *is*
        // its key, so the two can never disagree.
        //
        // `RunNode::label` comes last before the bare key, and it is what names
        // a **historical** run: a past run has no live presentation to mirror,
        // but its labels were durable all along — on the node itself. Without
        // this rung every box in the history view falls through to its key, and
        // a whole fan-out is drawn under N identical names again.
        label: [node.path.as_str(), node.key.as_str()]
            .into_iter()
            .filter_map(|lookup| node_labels.get(lookup))
            .map(|label| label.trim())
            .chain(std::iter::once(node.label.trim()))
            .find(|label| !label.is_empty())
            .unwrap_or(node.key.as_str())
            .to_string(),
        status: node.status,
        model: node.assignment.model.as_str().to_string(),
        effort: node.assignment.effort.as_str().to_string(),
        attempt: node.attempt,
        usage: node.usage,
        duration_ms: display_duration_ms(node, now_unix_ms),
        summary: node
            .result
            .as_ref()
            .map(|result| result.summary.clone())
            .filter(|summary| !summary.trim().is_empty()),
        delivery_failure: presentation
            .delivery_failures
            .get(node.path.as_str())
            .cloned(),
        // The last guardrail this node ran into as a *proposer*, mirrored onto
        // `WorkflowRunPresentation` beside the delivery failures. Rendered by
        // `render_nodes` below whenever it is `Some`.
        growth_notice: presentation.growth_notices.get(node.path.as_str()).cloned(),
        depth: node.depth,
        parent: node.parent,
        blocker: match &node.succession {
            Some(Succession::Blocked {
                reason,
                resume_when,
            }) => Some(format!("{reason} — resume when {resume_when}")),
            _ => None,
        },
        pane_id: node
            .binding
            .as_ref()
            .map(|binding| binding.pane_id.as_str().to_string()),
        // The observed half is deliberately empty here. `project_node` reads a
        // `RunNode`, which is the engine's model of a node and carries none of
        // what a Claude Code team recorded; [`merge_projection`] fills these in
        // afterwards, and only for a lead run (§3.4).
        owner: String::new(),
        subject: String::new(),
        emergent: false,
        owner_pane_id: None,
        agent_state: None,
        successors,
        predecessors,
    }
}

/// Merges the run projection's observations onto the projected nodes (§3.4).
///
/// Two layers land here, from two different authorities on purpose: the task
/// facts are what the *team* recorded, and `agent_state` is what karvex's own
/// per-pane detector sees in that pane right now. A node whose task still says
/// `in_progress` while its pane says it is waiting on input is exactly the case
/// a single status would hide.
fn merge_projection(
    app: &AppState,
    nodes: &mut [DagNodeView],
    projected: &std::collections::BTreeMap<String, ProjectedNodeFacts>,
    members: &[crate::api::schema::WorkflowRunMemberInfo],
) {
    for node in nodes {
        if let Some(facts) = projected.get(&node.path) {
            node.subject = facts.subject.clone();
            node.owner = facts.owner.clone();
            node.emergent = facts.emergent;
        }
        // An unclaimed task has an empty owner, which matches no member — so
        // the lookup answers `None` rather than picking an arbitrary teammate,
        // which is the honest answer for "nobody has taken this yet".
        node.owner_pane_id = members
            .iter()
            .find(|member| member.name == node.owner && !node.owner.is_empty())
            .and_then(|member| member.pane_id.clone());
        // The owner's pane leads and the node's own binding is the fallback:
        // for a lead run the work happens in the teammate's pane, and the
        // binding is only ever set for a run the engine bound itself.
        node.agent_state = node
            .owner_pane_id
            .as_ref()
            .or(node.pane_id.as_ref())
            .and_then(|pane| pane_agent_state(app, pane));
    }
}

/// What karvex's own per-pane detection says `pane` is doing.
///
/// Resolves a **public** pane id — the id Claude Code's team config hands back
/// through `tmuxPaneId` — against the same public numbering
/// `public_pane_id_for_number` mints, then reads the state off the pane's
/// attached terminal, which is where every other surface (the sidebar, the
/// navigator, the agent panel) reads it. No workflow-specific detection is
/// involved and none should be: §3.4 says this layer is per-pane and needs no
/// workflow code. `None` means no pane of this server answers to that id,
/// which is the normal answer for a run whose panes are gone.
fn pane_agent_state(app: &AppState, pane: &str) -> Option<crate::detect::AgentState> {
    app.workspaces.iter().find_map(|workspace| {
        workspace
            .tabs
            .iter()
            .flat_map(|tab| tab.panes.iter())
            .find(|(pane_id, _)| {
                workspace
                    .public_pane_number(**pane_id)
                    .is_some_and(|number| {
                        crate::workspace::public_pane_id_for_number(&workspace.id, number) == pane
                    })
            })
            .and_then(|(_, state)| app.terminals.get(&state.attached_terminal_id))
            .map(|terminal| terminal.state)
    })
}

/// How long the node has been at work, as the detail strip should show it.
///
/// `usage.duration_ms` is only written when a node reaches a terminal status,
/// so a live node would otherwise read `0s` forever no matter how long it has
/// been stuck. A node that has started but not finished counts from
/// `started_at_unix_ms`; a clock that moved backwards falls back to the
/// recorded duration rather than underflowing.
fn display_duration_ms(node: &RunNode, now_unix_ms: u64) -> u64 {
    if node.status != NodeStatus::Running || node.ended_at_unix_ms.is_some() {
        return node.usage.duration_ms;
    }
    match node.started_at_unix_ms {
        Some(started) => now_unix_ms
            .checked_sub(started)
            .unwrap_or(node.usage.duration_ms),
        None => node.usage.duration_ms,
    }
}

/// Keeps the selection on the same node across frames.
///
/// Matching is by instance path, not by index: a node keeps its path when the
/// graph grows, which is exactly the stability the layout's ordering tiebreak
/// gives the geometry.
fn carried_selection(previous: &DagViewState, next: &DagViewState) -> Option<RunNodeIdx> {
    let previous_path = previous
        .selected
        .and_then(|idx| previous.node(idx))
        .map(|node| node.path.as_str());
    previous_path
        .and_then(|path| next.nodes.iter().find(|node| node.path == path))
        .or_else(|| next.nodes.first())
        .map(|node| node.idx)
}

fn to_layout_rect(rect: Rect) -> LayoutRect {
    LayoutRect::new(rect.x, rect.y, rect.width, rect.height)
}

fn to_rect(rect: LayoutRect) -> Rect {
    Rect::new(rect.x, rect.y, rect.width, rect.height)
}

// ── graph-aware navigation (pure, over the stored geometry) ─────────────────

/// The node a navigation key moves to, or `None` when there is nowhere to go.
///
/// `Down`/`Up` follow the graph — successors and predecessors first — and fall
/// back to the nearest box in the next/previous band so a disconnected node is
/// still reachable. `Left`/`Right` move within a band.
///
/// A node's graph successors/predecessors are not all one hop away in the
/// *rendered* layout: `layout()` layers by longest path
/// (`src/workflow/layout.rs::assign_layers`), so a fan-in node such as
/// `collect` sits two rows below `fanout` (one past the freshly appended
/// `worker` row) even though the authored edge from `fanout` to `collect` is
/// direct. Picking the graph successor nearest in `x` without first asking
/// which one is in the *nearest row* let that far edge win over the adjacent
/// one, stranding every node an expansion appends — `Enter`/`s` become
/// keyboard-unreachable for exactly the nodes dynamic growth creates. Ranking
/// by row distance first, `x` distance second, restores "the appended row is
/// adjacent from the parent" for `hjkl`/arrow navigation.
pub(crate) fn workflow_dag_neighbour(
    view: &DagViewState,
    direction: DagNavDirection,
) -> Option<RunNodeIdx> {
    let Some(current) = view.selected else {
        return view.nodes.first().map(|node| node.idx);
    };
    let (Some(node), Some(rect)) = (view.node(current), view.rect_of(current)) else {
        return view.nodes.first().map(|node| node.idx);
    };
    let origin = centre_x(rect);

    match direction {
        DagNavDirection::Down => {
            nearest_graph_neighbour(view, &node.successors, rect, origin, true)
                .or_else(|| nearest_in_band(view, rect, origin, true))
        }
        DagNavDirection::Up => {
            nearest_graph_neighbour(view, &node.predecessors, rect, origin, false)
                .or_else(|| nearest_in_band(view, rect, origin, false))
        }
        DagNavDirection::Left => same_band(view, rect)
            .filter(|(_, other)| other.x < rect.x)
            .max_by_key(|(_, other)| other.x)
            .map(|(idx, _)| idx),
        DagNavDirection::Right => same_band(view, rect)
            .filter(|(_, other)| other.x > rect.x)
            .min_by_key(|(_, other)| other.x)
            .map(|(idx, _)| idx),
    }
}

fn centre_x(rect: LayoutRect) -> u16 {
    rect.x.saturating_add(rect.width / 2)
}

/// The graph successor/predecessor nearest to `rect` in row order first, `x`
/// distance second.
///
/// `down` selects the direction: successors are asked for the nearest row
/// *below* `rect` (`Down`), predecessors for the nearest row *above*
/// (`Up`). A candidate that (defensively — layering should never produce
/// this for an acyclic graph) sits on the wrong side or shares the row is
/// skipped rather than treated as an equally good neighbour: it is not
/// where a `Down`/`Up` press should land.
fn nearest_graph_neighbour(
    view: &DagViewState,
    candidates: &[RunNodeIdx],
    rect: LayoutRect,
    origin: u16,
    down: bool,
) -> Option<RunNodeIdx> {
    candidates
        .iter()
        .filter_map(|idx| view.rect_of(*idx).map(|other| (*idx, other)))
        .filter(|(_, other)| {
            if down {
                other.y > rect.y
            } else {
                other.y < rect.y
            }
        })
        .min_by_key(|(idx, other)| {
            let band_distance = if down {
                other.y.saturating_sub(rect.y)
            } else {
                rect.y.saturating_sub(other.y)
            };
            (
                band_distance,
                centre_x(*other).abs_diff(origin),
                other.x,
                idx.0,
            )
        })
        .map(|(idx, _)| idx)
}

/// The nearest box in the next band below (`down`) or above the current one.
fn nearest_in_band(
    view: &DagViewState,
    rect: LayoutRect,
    origin: u16,
    down: bool,
) -> Option<RunNodeIdx> {
    view.layout
        .nodes
        .iter()
        .filter(|(_, other)| {
            if down {
                other.y > rect.y
            } else {
                other.y < rect.y
            }
        })
        .min_by_key(|(idx, other)| {
            let band_distance = if down {
                other.y.saturating_sub(rect.y)
            } else {
                rect.y.saturating_sub(other.y)
            };
            (band_distance, centre_x(*other).abs_diff(origin), idx.0)
        })
        .map(|(idx, _)| *idx)
}

fn same_band<'a>(
    view: &'a DagViewState,
    rect: LayoutRect,
) -> impl Iterator<Item = (RunNodeIdx, LayoutRect)> + 'a {
    view.layout
        .nodes
        .iter()
        .filter(move |(_, other)| other.y == rect.y && other.x != rect.x)
        .map(|(idx, other)| (*idx, *other))
}

// ── render (pure draw) ──────────────────────────────────────────────────────

/// The two facts the overlay draws that [`DagViewState`] has no field to carry.
///
/// Its shape was frozen in step 1b (`07-phase3-plan.md` §1 WS-G) and neither
/// the epilogue phase nor a node's restore provenance is in it. Both are pure
/// reads off the projected `RunGraph`, and neither influences a single
/// rectangle — so they are derived here, at draw time, rather than smuggled
/// into the view state or recomputed inside two render functions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DagExtras {
    /// The run's summariser epilogue, when the engine appended one.
    epilogue: Option<EpiloguePhase>,
    /// The selected node's `restored from …` provenance line (§4 D4).
    restored_from: Option<String>,
}

fn dag_extras(app: &AppState) -> DagExtras {
    let Some(graph) = app
        .historical_run()
        .map(|snapshot| snapshot.graph.as_ref())
        .or_else(|| app.workflow_run_graph())
    else {
        return DagExtras::default();
    };
    // By instance path, not by index: the projection and the graph are two
    // different orderings of the same run, and the path is the identity that
    // survives both (the same reason `carried_selection` matches on it).
    let restored_from = app
        .view
        .dag
        .selected_node()
        .map(|node| node.path.as_str())
        .and_then(|path| graph.nodes.iter().find(|node| node.path.as_str() == path))
        .and_then(|node| node.restored_from.as_ref())
        .map(restored_from_line);
    DagExtras {
        epilogue: graph.epilogue.as_ref().map(|epilogue| epilogue.phase),
        restored_from,
    }
}

/// Where a restored node's result actually came from. The run, the node, and
/// the checkpoint — all three, because a restore is only auditable if it names
/// the exact row it copied (§4 D4).
fn restored_from_line(source: &RestoredRef) -> String {
    format!(
        "restored from {} · {} · checkpoint {}",
        source.run.as_str(),
        source.node_key.as_str(),
        source.checkpoint_seq
    )
}

pub(super) fn render_workflow_dag(app: &AppState, frame: &mut Frame, area: Rect) {
    let dag = &app.view.dag;
    let p = &app.palette;

    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(p.panel_bg)),
        area,
    );

    // Nothing was drawn: say so once, on the whole overlay, instead of a header
    // and a navigation footer arguing with an empty body.
    if dag.is_empty() {
        render_single_message(dag, p, frame, area);
        return;
    }

    let extras = dag_extras(app);
    render_header(dag, &extras, p, frame);
    render_banner(dag, p, frame);
    render_edges(dag, p, frame);
    render_nodes(dag, p, frame);
    render_interrogations(dag, p, frame);
    render_detail(dag, &extras, p, frame);
    render_footer(dag, p, frame);
}

/// The run's last growth limit, in the palette's warning slot. Zero rows —
/// and nothing drawn — while [`DagViewState::banner`] is `None`, which is the
/// zero-height-when-absent rule `overlay_areas` already enforces on the rect
/// (`06-phase2-plan.md` §3 frozen interface 10).
fn render_banner(dag: &DagViewState, p: &Palette, frame: &mut Frame) {
    if dag.banner_rect.height == 0 {
        return;
    }
    let Some(banner) = &dag.banner else {
        return;
    };
    let width = (dag.banner_rect.width as usize).saturating_sub(1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {}", truncate_end(banner, width)),
            Style::default().fg(p.peach).add_modifier(Modifier::BOLD),
        ))),
        dag.banner_rect,
    );
}

/// What to call this run on screen: the authored workflow name when the
/// runtime mirrored one, and only otherwise the raw record id.
fn run_title(dag: &DagViewState) -> String {
    let name = dag.workflow_name.trim();
    if name.is_empty() {
        dag.run_id.clone()
    } else {
        name.to_string()
    }
}

/// Truncates a styled line to `width` cells, cutting inside the span that
/// crosses the boundary and marking the cut with an ellipsis. The renderer,
/// not the terminal, decides where a line ends — a hard clip turns a run id
/// into a different, plausible, wrong run id.
fn truncate_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let total: usize = spans.iter().map(|span| display_width(&span.content)).sum();
    if total <= width || width == 0 {
        return if width == 0 { Vec::new() } else { spans };
    }
    let mut used = 0usize;
    let mut kept: Vec<Span<'static>> = Vec::new();
    for span in spans {
        let span_width = display_width(&span.content);
        // Every kept span must leave room for the ellipsis that follows it.
        if used + span_width < width {
            used += span_width;
            kept.push(span);
            continue;
        }
        // One cell is reserved for the ellipsis, which is always emitted:
        // whatever follows this span was dropped, so the line has to say so
        // even when this span itself happened to fit.
        let remaining = width.saturating_sub(used).saturating_sub(1);
        let cut = truncate_end(&span.content, remaining);
        let cut = cut.trim_end_matches('…');
        let style = span.style;
        if !cut.is_empty() {
            kept.push(Span::styled(cut.to_string(), style));
        }
        kept.push(Span::styled("…", style));
        return kept;
    }
    kept
}

/// The header names the run the way the author does, and reports the graph as
/// it is — total nodes always, plus what is offscreen and what is wrong.
fn render_header(dag: &DagViewState, extras: &DagExtras, p: &Palette, frame: &mut Frame) {
    if dag.header_rect.height == 0 {
        return;
    }
    let dim = Style::default().fg(p.overlay0);
    let mut spans = Vec::new();
    let title = match run_title(dag) {
        title if title.is_empty() => "workflow run".to_string(),
        title => title,
    };
    spans.push(Span::styled(
        format!(" {title}"),
        Style::default().fg(p.text).add_modifier(Modifier::BOLD),
    ));
    if let Some(status) = dag.run_status {
        spans.push(Span::styled(" · ", dim));
        spans.push(Span::styled(
            run_status_label(status),
            Style::default().fg(run_status_color(status, p)),
        ));
    }
    // A past run says so on the line that names it. Everything else about the
    // overlay looks identical to a live one, and a user acting on a stale graph
    // believing it is live is the failure this word prevents.
    if dag.historical {
        spans.push(Span::styled(" · ", dim));
        spans.push(Span::styled("past run", Style::default().fg(p.teal)));
    }
    // The epilogue has no band of its own (§1 WS-H): the summariser is visible
    // as a normal node in the graph, and its *state* rides the header, so a
    // succeeded run with a still-working summariser reads truthfully without
    // costing a row (§4 D1's post-`RunFinished` contract).
    match extras.epilogue {
        Some(EpiloguePhase::Pending) | Some(EpiloguePhase::Running) => {
            spans.push(Span::styled(" · ", dim));
            spans.push(Span::styled(
                "summarising…",
                Style::default().fg(p.subtext0),
            ));
        }
        Some(EpiloguePhase::GaveUp) => {
            spans.push(Span::styled(" · ", dim));
            spans.push(Span::styled("summary failed", Style::default().fg(p.peach)));
        }
        Some(EpiloguePhase::Done) | None => {}
    }
    spans.push(Span::styled(" · ", dim));
    spans.push(Span::styled(
        format!("{} nodes", dag.counts.total),
        Style::default().fg(p.subtext0),
    ));
    let offscreen = dag.offscreen_nodes();
    if offscreen > 0 {
        spans.push(Span::styled(
            format!(" ({offscreen} offscreen)"),
            Style::default().fg(p.peach),
        ));
    }
    if dag.counts.running > 0 {
        spans.push(Span::styled(" · ", dim));
        spans.push(Span::styled(
            format!("{} running", dag.counts.running),
            Style::default().fg(p.yellow),
        ));
    }
    if dag.counts.failed > 0 {
        spans.push(Span::styled(" · ", dim));
        spans.push(Span::styled(
            format!("{} failed", dag.counts.failed),
            Style::default().fg(p.red),
        ));
    }
    if dag.counts.needs_attention > 0 {
        spans.push(Span::styled(" · ", dim));
        spans.push(Span::styled(
            format!("{} needs attention", dag.counts.needs_attention),
            Style::default().fg(p.peach),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(truncate_spans(
            spans,
            dag.header_rect.width as usize,
        ))),
        dag.header_rect,
    );
}

/// The one screen the overlay shows when it drew no nodes at all.
///
/// Two distinct situations reach it and they get different copy: there is no
/// run to show, or there is a run and the terminal is too small to draw it.
/// Nothing else is rendered — no run header over an empty body, no navigation
/// hints for a graph that is not there.
fn render_single_message(dag: &DagViewState, p: &Palette, frame: &mut Frame, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let width = area.width.saturating_sub(1) as usize;
    let mut lines = Vec::new();
    if dag.too_small_to_draw() {
        lines.push(Line::from(Span::styled(
            format!(
                " {}",
                truncate_end(
                    &format!(
                        "terminal too small to draw {} nodes — resize the window",
                        dag.counts.total
                    ),
                    width,
                )
            ),
            Style::default().fg(p.peach).add_modifier(Modifier::BOLD),
        )));
        let target = run_title(dag);
        lines.push(Line::from(Span::styled(
            format!(
                " {}",
                truncate_end(&format!("or run: kvx workflow run show {target}"), width)
            ),
            Style::default().fg(p.subtext0),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            format!(" {}", truncate_end("no workflow run to show", width)),
            Style::default().fg(p.overlay0),
        )));
    }
    lines.push(Line::from(vec![
        Span::styled(
            " esc",
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" close", Style::default().fg(p.overlay0)),
    ]));
    frame.render_widget(Paragraph::new(lines), area);
}

/// Direction bits → box-drawing glyphs, with a `▾` arrowhead in the cell that
/// sits directly above a node box.
fn render_edges(dag: &DagViewState, p: &Palette, frame: &mut Frame) {
    if dag.graph_rect.height == 0 || dag.graph_rect.width == 0 {
        return;
    }
    let arrowheads: std::collections::HashSet<(u16, u16)> = dag
        .layout
        .nodes
        .iter()
        .filter_map(|(_, rect)| rect.y.checked_sub(1).map(|y| (centre_x(*rect), y)))
        .collect();

    let edge_cells = dag.layout.edge_cells();
    let buf = frame.buffer_mut();
    let bounds = buf.area;
    for (&(x, y), &bits) in &edge_cells {
        if x < bounds.x
            || x >= bounds.x.saturating_add(bounds.width)
            || y < bounds.y
            || y >= bounds.y.saturating_add(bounds.height)
        {
            continue;
        }
        let symbol = if arrowheads.contains(&(x, y)) {
            "▾"
        } else {
            line_cell_symbol(to_line_cell(bits))
        };
        if symbol.is_empty() {
            continue;
        }
        let cell = &mut buf[(x, y)];
        cell.set_symbol(symbol);
        cell.set_style(Style::default().fg(p.overlay0));
    }
}

fn to_line_cell(bits: EdgeBits) -> LineCell {
    LineCell {
        up: bits.up,
        down: bits.down,
        left: bits.left,
        right: bits.right,
    }
}

fn render_nodes(dag: &DagViewState, p: &Palette, frame: &mut Frame) {
    for (idx, rect) in &dag.layout.nodes {
        let Some(node) = dag.node(*idx) else {
            continue;
        };
        let rect = to_rect(*rect);
        if rect.width < 4 || rect.height < 3 {
            continue;
        }
        let selected = dag.selected == Some(*idx);
        let blocking = dag.is_blocking(*idx);
        let status_color = node_status_color(node.status, p);
        // A node the run is stuck behind is never quieter than the cursor:
        // both get an emphasised border, the blocking one in its status color.
        let border_style = if selected {
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
        } else if blocking {
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.surface1)
        };

        let title_style = if selected {
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else if blocking {
            Style::default()
                .fg(status_color)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text)
        };
        let label_width = rect.width.saturating_sub(4) as usize;
        // A node the team created without the definition planning it wears the
        // reserved-namespace prefix, because that is already this graph's word
        // for "karvex/team-owned, not authored": the `.summary` epilogue is
        // `.summary`, and an emergent task's instance path is `.task/7`
        // (`workflow/model.rs`). The box would otherwise hide that marker — it
        // titles itself with the observed subject, not with the path — so an
        // off-plan box reads `.add a regression test` beside a planned
        // `verify` (§3.4). An engine-era node is never emergent, so its title
        // is byte-identical to what it always was.
        let title = if node.emergent {
            format!("{RESERVED_PATH_PREFIX}{}", node.label)
        } else {
            node.label.clone()
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(Span::styled(
                format!(" {} ", truncate_end(&title, label_width)),
                title_style,
            ))
            .style(Style::default().bg(p.panel_bg));
        let inner = block.inner(rect);
        // Edges are routed without regard for the boxes they cross, so the box
        // interior is wiped before it is drawn: a `│` left inside a node body
        // reads as a table rule and corrupts the one fact the box exists to
        // carry. `Block` only sets style, never symbols, so it cannot do this.
        frame.render_widget(Clear, rect);
        frame.render_widget(block, rect);

        if inner.height == 0 {
            continue;
        }
        let mut spans = vec![
            Span::styled(
                node_status_glyph(node.status),
                Style::default().fg(status_color),
            ),
            Span::styled(
                format!(" {}", node_status_label(node.status)),
                Style::default().fg(status_color),
            ),
        ];
        if node.attempt > 1 {
            spans.push(Span::styled(
                format!(" ·{}", node.attempt),
                Style::default().fg(p.overlay0),
            ));
        }
        // The last growth limit this node ran into as a proposer, in the
        // palette's warning slot — one of the three non-optional surfaces the
        // "a rejection is always surfaced" guarantee rests on
        // (`06-phase2-plan.md` §4 D11). `NODE_HEIGHT` (`workflow/layout.rs`)
        // gives every box exactly one interior row, so the notice shares the
        // status row rather than a second one that does not exist — spans,
        // not a second `Line`, and truncated the same way the header and
        // detail rows already truncate a span run that overflows its band.
        if let Some(notice) = &node.growth_notice {
            spans.push(Span::styled(" · ", Style::default().fg(p.overlay0)));
            spans.push(Span::styled(
                notice.clone(),
                Style::default().fg(p.peach).add_modifier(Modifier::BOLD),
            ));
        }
        let spans = truncate_spans(spans, inner.width as usize);
        frame.render_widget(Paragraph::new(Line::from(spans)), inner);
    }
}

/// The detached interrogation lane, below the deepest graph layer.
///
/// Drawn after the nodes and never routed to by an edge: an interrogation is
/// not a `RunGraph` node (§4 D8), so it has no inbound or outbound rail and
/// must not read as a root. Teal while its forked session's pane is live,
/// `overlay0` once it has ended — the box stays on screen either way, because
/// "which past nodes have been interrogated" is history worth keeping.
fn render_interrogations(dag: &DagViewState, p: &Palette, frame: &mut Frame) {
    for item in &dag.interrogation_nodes {
        let rect = item.rect;
        if rect.width < 4 || rect.height < 3 {
            continue;
        }
        let color = if item.ended { p.overlay0 } else { p.teal };
        // The **source path** leads, not the word "interrogate". A box is
        // 22 cells wide (`NODE_WIDTH`), so a title of
        // `interrogate · <path>` spends fourteen of its eighteen usable cells
        // saying what the lane already says and truncates away the only part
        // that identifies which node this is. The kind moves inside, where it
        // has a row to itself. `DagInterrogationView::label` keeps the full
        // `<kind> · <path>` name for consumers that are not 22 cells wide.
        let title_width = rect.width.saturating_sub(5) as usize;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(color))
            .title(Span::styled(
                format!(" ⌕ {} ", truncate_end(&item.path, title_width)),
                Style::default().fg(color),
            ))
            .style(Style::default().bg(p.panel_bg));
        let inner = block.inner(rect);
        frame.render_widget(Clear, rect);
        frame.render_widget(block, rect);

        if inner.height == 0 {
            continue;
        }
        // A reconstructed session names itself on every surface it has (§4 D7).
        // "ended" is stated as well as coloured: a dimmed border is a hint, and
        // "is this session still answering?" deserves a word.
        let spans = truncate_spans(
            vec![
                Span::styled(
                    interrogation_kind(item).to_string(),
                    Style::default().fg(color),
                ),
                Span::styled(
                    if item.ended {
                        " · ended".to_string()
                    } else {
                        String::new()
                    },
                    Style::default().fg(p.overlay0),
                ),
            ],
            inner.width as usize,
        );
        frame.render_widget(Paragraph::new(Line::from(spans)), inner);
    }
}

fn render_detail(dag: &DagViewState, extras: &DagExtras, p: &Palette, frame: &mut Frame) {
    if dag.detail_rect.height == 0 {
        return;
    }
    let Some(node) = dag.selected_node() else {
        return;
    };
    let width = dag.detail_rect.width as usize;
    let dim = Style::default().fg(p.overlay0);
    let mut lines: Vec<Line<'static>> = Vec::new();

    // A delivery the runtime refused leads even the blocker: it is the one
    // fact that contradicts what the user just did. Without it a steer that
    // never reached the process looks exactly like one that landed.
    if let Some(failure) = &node.delivery_failure {
        lines.push(Line::from(truncate_spans(
            vec![
                Span::styled(
                    " not delivered: ",
                    Style::default().fg(p.red).add_modifier(Modifier::BOLD),
                ),
                Span::styled(failure.clone(), Style::default().fg(p.red)),
            ],
            width,
        )));
    }
    // The blocker is the only line that asks the user to do something, so it
    // leads — labelled, not left to be mistaken for a summary.
    if let Some(blocker) = &node.blocker {
        lines.push(Line::from(truncate_spans(
            vec![
                Span::styled(
                    " blocked: ",
                    Style::default()
                        .fg(node_status_color(node.status, p))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(blocker.clone(), Style::default().fg(p.red)),
            ],
            width,
        )));
    }
    // The node box elides this to whatever fits its fixed ~20-column title
    // row (`render_nodes` above); the detail strip is the one place selecting
    // that node is guaranteed to show the *whole* limit, because it is the
    // strip's own width, not a box's, that bounds `truncate_spans` here. A
    // growth notice sharing this line rather than claiming a row of its own
    // keeps `DETAIL_HEIGHT` a true constant — no per-node content changes how
    // much of the overlay's fixed budget the strip needs.
    let mut path_status = vec![
        Span::styled(" ", dim),
        Span::styled(
            node.path.clone(),
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", dim),
        Span::styled(
            node_status_label(node.status),
            Style::default().fg(node_status_color(node.status, p)),
        ),
    ];
    if let Some(notice) = &node.growth_notice {
        path_status.push(Span::styled("  ", dim));
        path_status.push(Span::styled(
            notice.clone(),
            Style::default().fg(p.peach).add_modifier(Modifier::BOLD),
        ));
    }
    // Restore provenance shares the status row rather than claiming one of its
    // own, for the same reason the growth notice does: `DETAIL_HEIGHT` is a
    // constant, and no per-node fact may change how much of the overlay's fixed
    // budget the strip needs. It sits next to the status because `↺ restored`
    // without "restored from what" is the half of the fact that cannot be
    // acted on (§4 D4).
    if let Some(source) = &extras.restored_from {
        path_status.push(Span::styled("  ", dim));
        path_status.push(Span::styled(source.clone(), Style::default().fg(p.teal)));
    }
    // The path already carries the reserved `.` the box title borrows, but the
    // prefix is a marker and this is the strip that explains markers — and it
    // is the one place the word "emergent" can be spelled out without costing
    // a row. Never reached by an engine-era run: its nodes are never emergent.
    if dag.lead_run && node.emergent {
        path_status.push(Span::styled("  ", dim));
        path_status.push(Span::styled("emergent", Style::default().fg(p.teal)));
    }
    lines.push(Line::from(truncate_spans(path_status, width)));
    let mut assignment = vec![
        Span::styled(" ", dim),
        Span::styled(
            format!("{} · {}", node.model, node.effort),
            Style::default().fg(p.mauve),
        ),
        Span::styled(
            format!(
                "  {} tokens · {} tools · {}s",
                node.usage.total_tokens,
                node.usage.tool_uses,
                node.duration_ms / 1000
            ),
            dim,
        ),
        Span::styled(
            node.pane_id
                .as_ref()
                .map(|pane| format!("  pane {pane}"))
                .unwrap_or_default(),
            dim,
        ),
    ];
    // Who holds this node and what their pane is doing right now — §3.4's two
    // layers, side by side, on the row that already answers "where is this
    // running". It shares that row rather than claiming a new one because
    // `DETAIL_HEIGHT` is a constant and no per-node fact may change how much of
    // the overlay's fixed budget the strip needs.
    if dag.lead_run {
        assignment.push(Span::styled(
            format!("  owner {}", owner_label(node)),
            Style::default().fg(p.teal),
        ));
        if let Some(state) = node.agent_state {
            assignment.push(Span::styled(
                format!(" · {}", agent_state_label(state)),
                dim,
            ));
        }
    }
    lines.push(Line::from(truncate_spans(assignment, width)));
    // §3.4's loose contract made visible: the lead may reword, split, or merge
    // a task, so what the team called the work can differ from what the
    // definition called the node. Only the difference is worth a row — a
    // subject that merely repeats the label says nothing — and it takes the
    // summary's row because a lead run has no checkpoints to summarise.
    let observed_subject = dag
        .lead_run
        .then(|| node.subject.trim())
        .filter(|subject| !subject.is_empty() && *subject != node.label.trim());
    lines.push(match (observed_subject, &node.summary) {
        (Some(subject), _) => Line::from(Span::styled(
            format!(" task: {}", truncate_end(subject, width.saturating_sub(7))),
            Style::default().fg(p.subtext0),
        )),
        (None, Some(summary)) => Line::from(Span::styled(
            format!(" {}", truncate_end(summary, width.saturating_sub(1))),
            Style::default().fg(p.subtext0),
        )),
        (None, None) => Line::from(Span::styled(" no checkpoint yet", dim)),
    });
    frame.render_widget(Paragraph::new(lines), dag.detail_rect);
}

/// Who holds this node, as the detail strip names them.
///
/// `unclaimed` is a real state in the source data rather than a missing value:
/// Claude Code omits `owner` until a teammate claims the task, so leaving the
/// slot blank would read as a rendering gap instead of as "nobody yet".
fn owner_label(node: &DagNodeView) -> &str {
    let owner = node.owner.trim();
    if owner.is_empty() {
        "unclaimed"
    } else {
        owner
    }
}

/// karvex's per-pane detection states, in the overlay's vocabulary.
///
/// `Blocked` is spelled "needs input" here rather than "blocked": the DAG
/// already uses `blocked` for a node whose *succession* is gated, and the two
/// mean opposite things — one is waiting on the user, the other on the graph.
fn agent_state_label(state: crate::detect::AgentState) -> &'static str {
    match state {
        crate::detect::AgentState::Working => "working",
        crate::detect::AgentState::Idle => "idle",
        crate::detect::AgentState::Blocked => "needs input",
        crate::detect::AgentState::Unknown => "unknown",
    }
}

/// Every hint an **engine-era** run's overlay offers, in display order.
const FOOTER_HINTS: [(&str, &str); 6] = [
    ("enter", " focus"),
    ("hjkl/↑↓←→", " move"),
    ("s", " steer"),
    ("i", " interrogate"),
    // The degraded path gets its own key rather than a second meaning for
    // `i`: reconstructing a session that is not the original teammate is
    // always an explicit choice (`00-overview.md` Feature 3).
    ("I", " reconstruct"),
    ("esc", " close"),
];

/// Index of the `enter focus` hint in [`FOOTER_HINTS`].
const FOCUS_HINT: usize = 0;
/// Index of the `s steer` hint in [`FOOTER_HINTS`].
const STEER_HINT: usize = 2;

/// What a **lead run** offers instead (§3.5).
///
/// The engine verbs collapse because the operations behind them are gone: no
/// node is steered through karvex, no teammate session karvex forked can be
/// resumed by it, and there is no checkpoint to reconstruct from because the
/// engine never wrote one. Steering a lead run is opening the pane that owns
/// the node and typing in it, which is what `enter` now does.
const LEAD_FOOTER_HINTS: [(&str, &str); 3] = [
    ("enter", " focus pane"),
    ("hjkl/↑↓←→", " move"),
    ("esc", " close"),
];

/// Index of the `enter focus pane` hint in [`LEAD_FOOTER_HINTS`].
const LEAD_FOCUS_HINT: usize = 0;

/// Which hints fit in `width`, in display order.
///
/// A hint that does not fit is dropped whole rather than sliced mid-word, and
/// they go least-useful first: `esc close` is the last thing to leave, because
/// it is the only way out of a full-bleed overlay.
///
/// `steerable`/`focusable` come before the width budget: a key that would only
/// produce a refusal is not a hint, it is a lie the user pays a keystroke to
/// discover. A past run is not the active run, so the server answers
/// `workflow_run_not_active` for a steer, and a node whose pane is long gone
/// has nothing to focus (`07-phase3-plan.md` §1 WS-H).
fn footer_hints(
    width: usize,
    lead_run: bool,
    steerable: bool,
    focusable: bool,
) -> Vec<(&'static str, &'static str)> {
    if lead_run {
        /// Indices into [`LEAD_FOOTER_HINTS`], least useful first. `esc close`
        /// is last for the same reason it is last in the engine-era order: it
        /// is the only way out of a full-bleed overlay.
        const DROP_ORDER: [usize; 3] = [1, 0, 2];

        let mut keep = [true; LEAD_FOOTER_HINTS.len()];
        keep[LEAD_FOCUS_HINT] = focusable;
        return fit_hints(&LEAD_FOOTER_HINTS, &DROP_ORDER, &mut keep, width);
    }

    /// Indices into [`FOOTER_HINTS`], least useful first. `reconstruct` is the
    /// rarest action — it is only reachable after `interrogate` has already
    /// refused — so it is the first hint a narrow terminal loses.
    const DROP_ORDER: [usize; 6] = [4, 3, 2, 1, 0, 5];

    let mut keep = [true; FOOTER_HINTS.len()];
    keep[STEER_HINT] = steerable;
    keep[FOCUS_HINT] = focusable;
    fit_hints(&FOOTER_HINTS, &DROP_ORDER, &mut keep, width)
}

/// Drops hints in `drop_order` until what is left fits `width`.
///
/// Shared by both hint sets rather than duplicated per set: which verbs a run
/// offers depends on how it was executed, but "a hint that does not fit is
/// dropped whole, never sliced mid-word" is a property of the footer band.
fn fit_hints(
    hints: &'static [(&'static str, &'static str)],
    drop_order: &[usize],
    keep: &mut [bool],
    width: usize,
) -> Vec<(&'static str, &'static str)> {
    for index in drop_order {
        if hints_width(hints, keep) <= width {
            break;
        }
        if let Some(kept) = keep.get_mut(*index) {
            *kept = false;
        }
    }
    // Even the last hint can be wider than the band; a partial word is worse
    // than nothing, so the row goes empty instead.
    if hints_width(hints, keep) > width {
        return Vec::new();
    }
    hints
        .iter()
        .zip(keep.iter())
        .filter(|(_, kept)| **kept)
        .map(|(hint, _)| *hint)
        .collect()
}

fn hints_width(hints: &[(&str, &str)], keep: &[bool]) -> usize {
    let mut used = 0usize;
    let mut first = true;
    for (hint, kept) in hints.iter().zip(keep) {
        if !kept {
            continue;
        }
        used += if first { 1 } else { 2 };
        first = false;
        used += display_width(hint.0) + display_width(hint.1);
    }
    used
}

fn render_footer(dag: &DagViewState, p: &Palette, frame: &mut Frame) {
    if dag.footer_rect.height == 0 {
        return;
    }
    let key = Style::default().fg(p.accent).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(p.overlay0);
    let width = dag.footer_rect.width as usize;

    let line = if let Some(text) = &dag.steer {
        // The caret is what tells the user the line is live, so the text
        // yields to it rather than the other way round.
        let prefix = " steer › ";
        let budget = width
            .saturating_sub(display_width(prefix))
            .saturating_sub(1);
        Line::from(vec![
            Span::styled(" steer", key),
            Span::styled(" › ", dim),
            Span::styled(truncate_end(text, budget), Style::default().fg(p.text)),
            Span::styled("▏", Style::default().fg(p.accent)),
        ])
    } else {
        // A historical run answers `workflow_run_not_active` for a steer, and
        // its nodes' panes are gone — except when one somehow outlived the run,
        // which `Enter` still honours and the hint therefore still offers.
        let steerable = !dag.historical;
        let focusable = if dag.lead_run {
            // A lead run's `enter` opens the pane that *owns* the node, and a
            // past run's own node binding is never rehydrated — so the member
            // pane is what makes the hint honest, with the node's binding kept
            // only as the fallback the key itself also uses.
            dag.selected_node()
                .is_some_and(|node| node.owner_pane_id.is_some() || node.pane_id.is_some())
        } else {
            !dag.historical
                || dag
                    .selected_node()
                    .is_some_and(|node| node.pane_id.is_some())
        };
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (chord, label) in footer_hints(width, dag.lead_run, steerable, focusable) {
            let chord = if spans.is_empty() {
                format!(" {chord}")
            } else {
                format!("  {chord}")
            };
            spans.push(Span::styled(chord, key));
            spans.push(Span::styled(label.to_string(), dim));
        }
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(line), dag.footer_rect);
}

// ── status vocabulary (semantic palette slots only) ─────────────────────────

/// `04-kvdag-and-execution.md` §8 fixes the mapping for every status it lists;
/// `Failed` follows `NeedsAttention` and `Cancelled` follows `Skipped`, which
/// are the only readings consistent with the rest of the table.
fn node_status_color(status: NodeStatus, p: &Palette) -> Color {
    match status {
        NodeStatus::Running => p.yellow,
        // `needs_attention` is recoverable and is asking for the user;
        // `failed` is dead. They provoke opposite responses, so they must not
        // share a color. Amber here is the same slot the header already uses
        // for a paused run, which is exactly what a needs-attention node
        // causes.
        NodeStatus::NeedsAttention => p.peach,
        NodeStatus::Blocked | NodeStatus::Failed => p.red,
        NodeStatus::Succeeded => p.green,
        NodeStatus::Pending | NodeStatus::Ready => p.subtext0,
        NodeStatus::Skipped | NodeStatus::Cancelled => p.overlay0,
        NodeStatus::Restored => p.teal,
    }
}

fn node_status_glyph(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Pending => "·",
        NodeStatus::Ready => "▸",
        NodeStatus::Running => "●",
        NodeStatus::NeedsAttention => "!",
        NodeStatus::Blocked => "◼",
        NodeStatus::Succeeded => "✓",
        NodeStatus::Failed => "✗",
        NodeStatus::Skipped => "–",
        NodeStatus::Restored => "↺",
        NodeStatus::Cancelled => "×",
    }
}

fn node_status_label(status: NodeStatus) -> &'static str {
    match status {
        NodeStatus::Pending => "pending",
        NodeStatus::Ready => "ready",
        NodeStatus::Running => "running",
        NodeStatus::NeedsAttention => "needs attention",
        NodeStatus::Blocked => "blocked",
        NodeStatus::Succeeded => "succeeded",
        NodeStatus::Failed => "failed",
        NodeStatus::Skipped => "skipped",
        NodeStatus::Restored => "restored",
        NodeStatus::Cancelled => "cancelled",
    }
}

fn run_status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Pending => "pending",
        RunStatus::Running => "running",
        RunStatus::Paused => "paused",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

fn run_status_color(status: RunStatus, p: &Palette) -> Color {
    match status {
        RunStatus::Running => p.yellow,
        RunStatus::Succeeded => p.green,
        RunStatus::Failed => p.red,
        RunStatus::Paused => p.peach,
        RunStatus::Pending => p.subtext0,
        RunStatus::Cancelled => p.overlay0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::{
        EdgeKind, EdgePayload, GrowthLimits, InstancePath, KvdagVersionId, NodeKey, NodeUsage,
        ProgressTracker, RunEdge, RunId,
    };
    use crate::workflow::tier::{Assignment, Effort, ModelAlias, Tier};

    fn test_node(idx: usize, key: &str) -> RunNode {
        RunNode {
            idx: RunNodeIdx(idx),
            key: NodeKey::new(key),
            path: InstancePath::new(key),
            label: String::new(),
            inputs: std::collections::BTreeMap::new(),
            parent: None,
            depth: 0,
            status: NodeStatus::Pending,
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
        }
    }

    fn test_edge(from: usize, to: usize) -> RunEdge {
        RunEdge {
            from: RunNodeIdx(from),
            to: RunNodeIdx(to),
            kind: EdgeKind::Sequence,
            condition: None,
            payload: EdgePayload::default(),
            port: None,
            condition_result: None,
            fired: false,
        }
    }

    /// `start` → `{left, right}` → `end`.
    fn diamond() -> RunGraph {
        RunGraph {
            run_id: RunId::new("workflow_run:1"),
            version_id: KvdagVersionId::new("v1"),
            tier: Tier::Auto,
            growth: GrowthLimits::default(),
            assignments: std::collections::BTreeMap::new(),
            nodes: vec![
                test_node(0, "start"),
                test_node(1, "left"),
                test_node(2, "right"),
                test_node(3, "end"),
            ],
            edges: vec![
                test_edge(0, 1),
                test_edge(0, 2),
                test_edge(1, 3),
                test_edge(2, 3),
            ],
            status: RunStatus::Running,
            seq: 0,
            epilogue: None,
        }
    }

    /// The projection the overlay would hold for `graph` in `area`, without
    /// needing an `AppState` wired to a live run.
    fn view_of(graph: &RunGraph, area: Rect) -> DagViewState {
        view_of_named(graph, area, "", &std::collections::HashMap::new())
    }

    fn view_of_named(
        graph: &RunGraph,
        area: Rect,
        workflow_name: &str,
        labels: &std::collections::HashMap<String, String>,
    ) -> DagViewState {
        view_of_full(graph, area, workflow_name, labels, None)
    }

    /// Like [`view_of_named`], plus the run banner. The WS-G geometry and
    /// rendering tests construct the banner directly rather than driving a real
    /// growth limit through the engine: this file's subject is the geometry and
    /// the drawing, and `compute_workflow_dag_view` reads the banner off
    /// `WorkflowRunPresentation`, whose mirroring is covered in
    /// `src/app/workflow.rs`.
    fn view_of_full(
        graph: &RunGraph,
        area: Rect,
        workflow_name: &str,
        labels: &std::collections::HashMap<String, String>,
        banner: Option<&str>,
    ) -> DagViewState {
        let (header_rect, banner_rect, graph_rect, detail_rect, footer_rect) =
            overlay_areas(area, banner);
        let mut view = DagViewState {
            header_rect,
            banner_rect,
            graph_rect,
            detail_rect,
            footer_rect,
            banner: banner.map(str::to_string),
            run_id: graph.run_id.as_str().to_string(),
            workflow_name: workflow_name.to_string(),
            run_status: Some(graph.status),
            counts: run_counts(graph),
            ..DagViewState::default()
        };
        view.layout = clipped_layout(graph, graph_rect);
        view.nodes = graph
            .nodes
            .iter()
            .filter(|node| view.layout.rect_of(node.idx).is_some())
            .map(|node| {
                project_node(
                    graph,
                    node,
                    &WorkflowRunPresentation {
                        workflow_name: workflow_name.to_string(),
                        node_labels: labels.clone(),
                        delivery_failures: std::collections::HashMap::new(),
                        growth_banner: None,
                        growth_notices: std::collections::HashMap::new(),
                    },
                    0,
                )
            })
            .collect();
        view.selected = carried_selection(&DagViewState::default(), &view);
        view
    }

    fn screen_of(view: &DagViewState, area: Rect) -> String {
        use ratatui::{backend::TestBackend, Terminal};

        let mut app = AppState::test_new();
        app.mode = Mode::WorkflowDag;
        app.view.dag = view.clone();
        let app = &app;
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("term");
        terminal
            .draw(|frame| render_workflow_dag(app, frame, area))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn hit_test_agrees_with_every_stored_rect() {
        let view = view_of(&diamond(), Rect::new(0, 0, 120, 40));
        assert_eq!(view.nodes.len(), 4);

        for (idx, rect) in &view.layout.nodes {
            // Every cell of every stored box hit-tests back to that box.
            for row in rect.y..rect.bottom() {
                for col in rect.x..rect.right() {
                    assert_eq!(view.node_at(col, row), Some(*idx), "({col},{row})");
                }
            }
            // And the cell just outside the box does not.
            assert_ne!(view.node_at(rect.right(), rect.y), Some(*idx));
        }
    }

    #[test]
    fn stored_rects_stay_inside_the_graph_band() {
        let view = view_of(&diamond(), Rect::new(2, 1, 60, 20));
        for (_, rect) in &view.layout.nodes {
            assert!(
                contains_rect(to_layout_rect(view.graph_rect), *rect),
                "{rect:?} escapes {:?}",
                view.graph_rect
            );
        }
        for (x, y) in view.layout.edge_cells().keys() {
            assert!(to_layout_rect(view.graph_rect).contains(*x, *y));
        }
    }

    #[test]
    fn a_short_graph_band_drops_boxes_instead_of_drawing_off_screen() {
        // Only the first layer fits; the rest is clipped away rather than
        // becoming invisible-but-clickable geometry.
        let view = view_of(&diamond(), Rect::new(0, 0, 120, 9));
        assert!(!view.layout.nodes.is_empty());
        assert!(view.layout.nodes.len() < 4);
        assert_eq!(view.nodes.len(), view.layout.nodes.len());
        for (idx, _) in &view.layout.nodes {
            assert!(view.node(*idx).is_some());
        }
    }

    #[test]
    fn navigation_follows_edges_then_bands() {
        let mut view = view_of(&diamond(), Rect::new(0, 0, 120, 40));
        let start = RunNodeIdx(0);
        let left = RunNodeIdx(1);
        let right = RunNodeIdx(2);
        let end = RunNodeIdx(3);
        assert_eq!(view.selected, Some(start));

        // Down follows an outbound edge, up an inbound one.
        let down = workflow_dag_neighbour(&view, DagNavDirection::Down);
        assert!(down == Some(left) || down == Some(right), "{down:?}");
        view.selected = down;
        assert_eq!(
            workflow_dag_neighbour(&view, DagNavDirection::Up),
            Some(start)
        );
        assert_eq!(
            workflow_dag_neighbour(&view, DagNavDirection::Down),
            Some(end)
        );

        // Left/right move within the band and stop at its ends.
        view.selected = Some(left);
        assert_eq!(
            workflow_dag_neighbour(&view, DagNavDirection::Right),
            Some(right)
        );
        assert_eq!(workflow_dag_neighbour(&view, DagNavDirection::Left), None);
        view.selected = Some(right);
        assert_eq!(
            workflow_dag_neighbour(&view, DagNavDirection::Left),
            Some(left)
        );
        assert_eq!(workflow_dag_neighbour(&view, DagNavDirection::Right), None);
    }

    /// Regression for the retest P1: `fanout` proposes `worker`, and `collect`
    /// sits downstream of `fanout` on a direct authored data edge
    /// (`tests/fixtures/workflow/expand.toml`, mirroring
    /// `04-kvdag-and-execution.md` §3.4's fan-in inheritance — `collect` also
    /// inherits an edge from `worker`). `collect` is one authored hop from
    /// `fanout`, but two *rows* away once `worker` is materialised, because
    /// `layout()` layers by longest path
    /// (`src/workflow/layout.rs::assign_layers`). Before the fix, `Down` from
    /// `fanout` picked whichever successor was nearest in `x` regardless of
    /// row and landed on `collect`, skipping the entire appended `worker` row
    /// — exactly the frames the retest captured (`dag-02` → `dag-03`).
    #[test]
    fn navigation_reaches_an_expansion_child_before_its_downstream_fan_in() {
        let graph = RunGraph {
            run_id: RunId::new("workflow_run:1"),
            version_id: KvdagVersionId::new("v1"),
            tier: Tier::Auto,
            growth: GrowthLimits::default(),
            assignments: std::collections::BTreeMap::new(),
            nodes: vec![
                test_node(0, "fanout"),
                test_node(1, "collect"),
                test_node(2, "worker"),
            ],
            edges: vec![
                // Authored: fanout -> collect, direct.
                test_edge(0, 1),
                // Spawned by expansion: fanout -> worker.
                test_edge(0, 2),
                // Inherited fan-in: worker -> collect, so collect waits on
                // the whole generation (§3.4).
                test_edge(2, 1),
            ],
            status: RunStatus::Running,
            seq: 0,
            epilogue: None,
        };
        let fanout = RunNodeIdx(0);
        let collect = RunNodeIdx(1);
        let worker = RunNodeIdx(2);

        let mut view = view_of(&graph, Rect::new(0, 0, 120, 40));
        view.selected = Some(fanout);

        // collect is a genuine graph successor of fanout, but it renders two
        // rows down; worker renders one row down and must win.
        assert_eq!(
            workflow_dag_neighbour(&view, DagNavDirection::Down),
            Some(worker),
            "Down from fanout must reach the appended worker row, not skip to collect"
        );

        view.selected = Some(worker);
        assert_eq!(
            workflow_dag_neighbour(&view, DagNavDirection::Up),
            Some(fanout),
            "Up from worker must return to its proposing parent"
        );
        assert_eq!(
            workflow_dag_neighbour(&view, DagNavDirection::Down),
            Some(collect),
            "Down from worker still reaches collect, one row below it"
        );
    }

    /// The retest's `--label` P0, on the surface that names nodes: a whole
    /// generation cut from one template shares that template's `key`, so a
    /// label map read by key drew N identical boxes for N children the
    /// proposing node had deliberately named apart. `mirror_workflow_run_graph`
    /// keys per *instance path*; the projection has to read it the same way,
    /// falling back to the per-key definition entry for a node the run graph
    /// does not carry.
    #[test]
    fn siblings_cut_from_one_template_are_drawn_under_their_own_labels() {
        let mut graph = diamond();
        graph.nodes = vec![
            test_node(0, "fanout"),
            test_node(1, "worker"),
            test_node(2, "worker"),
        ];
        graph.nodes[1].path = InstancePath::new("fanout/worker/1");
        graph.nodes[2].path = InstancePath::new("fanout/worker/2");
        graph.edges = vec![test_edge(0, 1), test_edge(0, 2)];

        let presentation = WorkflowRunPresentation {
            workflow_name: String::new(),
            node_labels: std::collections::HashMap::from([
                // The definition's per-key entry — the fallback, and the only
                // thing the broken lookup ever saw.
                ("worker".to_string(), "Worker".to_string()),
                ("fanout".to_string(), "Fan out".to_string()),
                // What the proposals actually named these two children.
                ("fanout/worker/1".to_string(), "Shard: auth".to_string()),
                ("fanout/worker/2".to_string(), "Shard: ui".to_string()),
            ]),
            delivery_failures: std::collections::HashMap::new(),
            growth_banner: None,
            growth_notices: std::collections::HashMap::new(),
        };

        let drawn: Vec<String> = graph
            .nodes
            .iter()
            .map(|node| project_node(&graph, node, &presentation, 0).label)
            .collect();
        assert_eq!(
            drawn,
            vec![
                "Fan out".to_string(),
                "Shard: auth".to_string(),
                "Shard: ui".to_string(),
            ],
            "two children of one template must not be drawn as the same box"
        );
    }

    /// A static node's path *is* its key, so the per-key definition entry has
    /// to keep working — and a run graph that carries no per-path label at all
    /// (an older mirror, a node the definition alone describes) must still be
    /// named rather than falling through to the bare key.
    #[test]
    fn a_static_node_still_reads_its_definition_label() {
        let graph = diamond();
        let presentation = WorkflowRunPresentation {
            workflow_name: String::new(),
            node_labels: std::collections::HashMap::from([(
                graph.nodes[0].key.as_str().to_string(),
                "Plan".to_string(),
            )]),
            delivery_failures: std::collections::HashMap::new(),
            growth_banner: None,
            growth_notices: std::collections::HashMap::new(),
        };
        assert_eq!(
            project_node(&graph, &graph.nodes[0], &presentation, 0).label,
            "Plan"
        );
        assert_eq!(
            project_node(&graph, &graph.nodes[1], &presentation, 0).label,
            graph.nodes[1].key.as_str(),
            "no label anywhere still falls back to the key, not to an empty box"
        );
    }

    #[test]
    fn navigation_selects_the_first_node_when_nothing_is_selected() {
        let mut view = view_of(&diamond(), Rect::new(0, 0, 120, 40));
        view.selected = None;
        assert_eq!(
            workflow_dag_neighbour(&view, DagNavDirection::Down),
            Some(RunNodeIdx(0))
        );
    }

    #[test]
    fn selection_survives_a_relayout_by_path() {
        let graph = diamond();
        let previous = {
            let mut view = view_of(&graph, Rect::new(0, 0, 120, 40));
            view.selected = Some(RunNodeIdx(2));
            view
        };
        let next = view_of(&graph, Rect::new(0, 0, 90, 40));
        assert_eq!(
            carried_selection(&previous, &next),
            Some(RunNodeIdx(2)),
            "selection must follow the node, not the frame"
        );
    }

    #[test]
    fn an_empty_graph_projects_an_empty_overlay() {
        let graph = RunGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
            ..diamond()
        };
        let view = view_of(&graph, Rect::new(0, 0, 80, 24));
        assert!(view.is_empty());
        assert_eq!(view.selected, None);
        assert_eq!(view.node_at(1, 1), None);
    }

    #[test]
    fn edge_bits_become_the_shared_box_drawing_glyphs() {
        assert_eq!(
            line_cell_symbol(to_line_cell(EdgeBits {
                up: true,
                down: true,
                left: false,
                right: true,
            })),
            "├"
        );
    }

    /// Characterization pin, written *before* the agent-teams rework touched
    /// any shared rendering code (`09-agent-teams-rework.md` phase C).
    ///
    /// The rework re-points this overlay at the run projection, and the one
    /// thing it must not do is change how an engine-era run — every run in
    /// anyone's history — draws. This asserts the whole rendered screen
    /// verbatim rather than a handful of substrings, so any drift at all in the
    /// engine-era rendering fails here and has to be justified rather than
    /// noticed later. Update it only with a deliberate reason.
    #[test]
    fn an_engine_era_run_renders_exactly_as_it_did_before_the_rework() {
        let mut graph = diamond();
        graph.nodes[0].status = NodeStatus::Succeeded;
        graph.nodes[1].status = NodeStatus::Running;
        graph.nodes[2].status = NodeStatus::NeedsAttention;
        graph.nodes[3].status = NodeStatus::Pending;

        let area = Rect::new(0, 0, 80, 24);
        let view = view_of(&graph, area);
        let screen = screen_of(&view, area);

        insta_like_pin(
            &screen,
            // Structure the pin cares about, in the order it appears. Kept as
            // an ordered slice rather than one big literal so a width change
            // in an unrelated column does not turn into an unreadable diff.
            &[
                "workflow_run:1",
                "start",
                "left",
                "right",
                "end",
                "succeeded",
                "enter",
                " focus",
                "s",
                " steer",
                "i",
                " interrogate",
                "I",
                " reconstruct",
                "esc",
            ],
        );

        // The engine-era footer offers the full engine verb set. A new-path run
        // must not, and this is the line that says what "unchanged" means.
        assert!(
            screen.contains("steer"),
            "engine-era footer lost steer\n{screen}"
        );
        assert!(
            screen.contains("interrogate"),
            "engine-era footer lost interrogate\n{screen}"
        );
        assert!(
            screen.contains("reconstruct"),
            "engine-era footer lost reconstruct\n{screen}"
        );
        // And nothing from the projection leaks into a run that never had one.
        assert!(
            !screen.contains("emergent"),
            "projection vocabulary leaked into an engine-era run\n{screen}"
        );
        assert!(
            !screen.contains("owner"),
            "projection vocabulary leaked into an engine-era run\n{screen}"
        );
    }

    /// Asserts every fragment appears, in order, so the pin describes layout
    /// and not just presence.
    fn insta_like_pin(screen: &str, fragments: &[&str]) {
        let mut cursor = 0usize;
        for fragment in fragments {
            match screen[cursor..].find(fragment) {
                Some(offset) => cursor += offset + fragment.len(),
                None => panic!(
                    "expected {fragment:?} after byte {cursor} of the rendered screen\n{screen}"
                ),
            }
        }
    }

    #[test]
    fn render_draws_boxes_edges_and_the_selected_node_detail() {
        let mut graph = diamond();
        graph.nodes[0].status = NodeStatus::Running;
        graph.nodes[0].succession = Some(crate::workflow::model::Succession::Blocked {
            reason: "waiting on review".into(),
            resume_when: "the reviewer replies".into(),
        });

        let area = Rect::new(0, 0, 100, 30);
        let view = view_of(&graph, area);
        assert_eq!(view.selected, Some(RunNodeIdx(0)));
        let screen = screen_of(&view, area);

        // Header, every node box, the edge glyphs, the arrowheads, the detail
        // strip for the selected node, and the hint bar.
        assert!(screen.contains("workflow_run:1"), "{screen}");
        for label in ["start", "left", "right", "end"] {
            assert!(screen.contains(label), "missing {label}\n{screen}");
        }
        assert!(screen.contains('┌') && screen.contains('┘'), "{screen}");
        assert!(screen.contains('▾'), "no arrowhead\n{screen}");
        assert!(screen.contains("running"), "{screen}");
        assert!(screen.contains("sonnet · low"), "{screen}");
        assert!(screen.contains("waiting on review"), "{screen}");
        assert!(screen.contains("esc"), "{screen}");
    }

    /// 2.4: `render_edges` runs before `render_nodes`, and a `Block` only sets
    /// style — so without an explicit clear a skip-layer edge routed straight
    /// down through a box body survives inside it.
    #[test]
    fn no_edge_glyph_survives_inside_a_node_box() {
        // `a → b → c` plus the skip-layer `a → c`, which routes straight down
        // the shared centre column — directly through `b`'s box body.
        let graph = RunGraph {
            nodes: vec![test_node(0, "a"), test_node(1, "b"), test_node(2, "c")],
            edges: vec![test_edge(0, 1), test_edge(1, 2), test_edge(0, 2)],
            ..diamond()
        };

        let area = Rect::new(0, 0, 60, 20);
        let view = view_of(&graph, area);
        let mut app = AppState::test_new();
        app.mode = Mode::WorkflowDag;
        app.view.dag = view.clone();
        let app = &app;

        use ratatui::{backend::TestBackend, Terminal};
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("term");
        terminal
            .draw(|frame| render_workflow_dag(app, frame, area))
            .expect("draw");
        let buffer = terminal.backend().buffer();

        for (_, rect) in &view.layout.nodes {
            for row in (rect.y + 1)..(rect.bottom() - 1) {
                for column in (rect.x + 1)..(rect.right() - 1) {
                    let symbol = buffer[(column, row)].symbol();
                    assert!(
                        !"│─┌┐└┘├┤┬┴┼▾".contains(symbol),
                        "edge glyph {symbol:?} bled into a box at ({column},{row})"
                    );
                }
            }
        }
    }

    /// 2.7: clipping a box must take its edges with it, or the frame draws a
    /// rail and an arrowhead pointing at nothing.
    #[test]
    fn clipping_a_node_drops_the_edge_cells_that_pointed_at_it() {
        let graph = diamond();
        let view = view_of(&graph, Rect::new(0, 0, 120, 9));
        assert!(
            view.layout.nodes.len() < graph.nodes.len(),
            "nothing clipped"
        );

        let surviving: Vec<RunNodeIdx> = view.layout.nodes.iter().map(|(idx, _)| *idx).collect();
        for route in &view.layout.edges {
            assert!(surviving.contains(&route.from), "{route:?}");
            assert!(surviving.contains(&route.to), "{route:?}");
        }
        // Only the first layer survives here, so every edge went with its box
        // and no orphan stub is left under it.
        assert_eq!(surviving.len(), 1);
        assert!(
            view.layout.edge_cells().is_empty(),
            "{:?}",
            view.layout.edges
        );
    }

    /// 2.5: the header counts the run, not the frame.
    #[test]
    fn the_header_counts_every_node_and_names_what_is_offscreen() {
        let mut graph = diamond();
        graph.nodes[1].status = NodeStatus::Failed;
        graph.nodes[2].status = NodeStatus::NeedsAttention;

        let area = Rect::new(0, 0, 120, 40);
        let full = view_of(&graph, area);
        assert_eq!(full.counts.total, 4);
        assert_eq!(full.offscreen_nodes(), 0);
        let screen = screen_of(&full, area);
        assert!(screen.contains("4 nodes"), "{screen}");
        assert!(screen.contains("1 failed"), "{screen}");
        assert!(screen.contains("1 needs attention"), "{screen}");
        assert!(!screen.contains("offscreen"), "{screen}");

        // The same run in a band that only fits the first layer still reports
        // four nodes, and says how many it could not draw.
        let clipped_area = Rect::new(0, 0, 120, 9);
        let clipped = view_of(&graph, clipped_area);
        assert!(clipped.nodes.len() < 4);
        assert_eq!(clipped.counts.total, 4);
        let screen = screen_of(&clipped, clipped_area);
        assert!(screen.contains("4 nodes"), "{screen}");
        assert!(
            screen.contains(&format!("({} offscreen)", clipped.offscreen_nodes())),
            "{screen}"
        );
    }

    /// 2.25 / 2.30: authored names win over record ids and node keys.
    #[test]
    fn the_overlay_shows_authored_names_before_ids_and_keys() {
        let graph = diamond();
        let labels: std::collections::HashMap<String, String> =
            [("start".to_string(), "Kick Off".to_string())]
                .into_iter()
                .collect();
        let area = Rect::new(0, 0, 120, 40);
        let view = view_of_named(&graph, area, "ux-dag-probe", &labels);

        assert_eq!(
            view.node(RunNodeIdx(0)).map(|node| node.label.as_str()),
            Some("Kick Off")
        );
        // A node the definition did not label still shows its key.
        assert_eq!(
            view.node(RunNodeIdx(1)).map(|node| node.label.as_str()),
            Some("left")
        );

        let screen = screen_of(&view, area);
        assert!(screen.contains("ux-dag-probe"), "{screen}");
        assert!(screen.contains("Kick Off"), "{screen}");
        assert!(!screen.contains("workflow_run:1"), "{screen}");
    }

    /// 2.6: a run that does not fit must not be reported as a run that does not
    /// exist, and nothing navigable may be advertised.
    #[test]
    fn a_run_too_small_to_draw_says_so_once() {
        let graph = diamond();
        let area = Rect::new(0, 0, 24, 8);
        let view = view_of(&graph, area);
        assert!(view.is_empty());
        assert!(view.too_small_to_draw());

        let screen = screen_of(&view, area);
        assert!(screen.contains("too small"), "{screen}");
        assert!(!screen.contains("no workflow run to show"), "{screen}");
        // No navigation hints for a graph that is not on screen.
        assert!(!screen.contains("move"), "{screen}");
        assert!(!screen.contains("steer"), "{screen}");
        assert!(screen.contains("esc"), "{screen}");
    }

    #[test]
    fn no_run_at_all_still_reads_as_no_run() {
        let graph = RunGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
            ..diamond()
        };
        let area = Rect::new(0, 0, 80, 24);
        let view = view_of(&graph, area);
        assert!(!view.too_small_to_draw());

        let screen = screen_of(&view, area);
        assert!(screen.contains("no workflow run to show"), "{screen}");
        assert!(!screen.contains("too small"), "{screen}");
    }

    /// 2.27: the renderer decides where a line ends, so a narrow terminal never
    /// leaves a plausible-but-wrong run id on screen.
    #[test]
    fn narrow_bands_are_truncated_with_an_ellipsis_not_hard_clipped() {
        let mut graph = diamond();
        graph.nodes[0].status = NodeStatus::Running;

        let area = Rect::new(0, 0, 40, 20);
        let view = view_of(&graph, area);
        let screen = screen_of(&view, area);
        for line in screen.lines() {
            assert!(
                line.chars().count() <= area.width as usize,
                "line escapes the overlay: {line:?}"
            );
        }
        // The run id is longer than the header band, so it ends in an ellipsis
        // rather than in a different id.
        let header = screen.lines().next().unwrap_or_default();
        assert!(header.contains('…'), "{header:?}");
    }

    /// 2.28: `usage.duration_ms` is only written at completion.
    #[test]
    fn a_running_node_reports_elapsed_time_from_its_start() {
        let mut node = test_node(0, "start");
        node.status = NodeStatus::Running;
        node.started_at_unix_ms = Some(1_000);
        assert_eq!(display_duration_ms(&node, 8_000), 7_000);

        // A finished node keeps the recorded duration.
        node.status = NodeStatus::Succeeded;
        node.usage.duration_ms = 42;
        assert_eq!(display_duration_ms(&node, 8_000), 42);

        // A backwards clock never underflows into a nonsense duration.
        node.status = NodeStatus::Running;
        assert_eq!(display_duration_ms(&node, 0), 42);
    }

    /// 2.22: the blocker is the actionable line, so it leads and it is labelled.
    #[test]
    fn the_detail_strip_leads_with_the_blocker() {
        let mut graph = diamond();
        graph.nodes[0].status = NodeStatus::NeedsAttention;
        graph.nodes[0].succession = Some(crate::workflow::model::Succession::Blocked {
            reason: "the node's pane exited".into(),
            resume_when: "the node is restarted".into(),
        });

        let area = Rect::new(0, 0, 120, 40);
        let view = view_of(&graph, area);
        let screen = screen_of(&view, area);
        let detail: Vec<&str> = screen
            .lines()
            .skip(view.detail_rect.y as usize)
            .take(view.detail_rect.height as usize)
            .collect();
        assert!(
            detail.first().is_some_and(|line| line.contains("blocked:")),
            "{detail:?}"
        );
        assert!(
            detail
                .first()
                .is_some_and(|line| line.contains("pane exited")),
            "{detail:?}"
        );
    }

    /// 2.15: a steer the runtime refused used to leave no trace on any surface
    /// — the user was left believing it was delivered. The marker leads the
    /// strip, above even the blocker, because it contradicts what the user just
    /// did.
    #[test]
    fn a_refused_delivery_is_shown_on_the_node_it_was_meant_for() {
        let mut graph = diamond();
        graph.nodes[0].status = NodeStatus::Failed;
        let path = graph.nodes[0].path.to_string();

        let area = Rect::new(0, 0, 120, 40);
        let presentation = WorkflowRunPresentation {
            workflow_name: String::new(),
            node_labels: std::collections::HashMap::new(),
            delivery_failures: std::collections::HashMap::from([(
                path,
                "pane.send_text: pane_not_found: no such pane".to_string(),
            )]),
            growth_banner: None,
            growth_notices: std::collections::HashMap::new(),
        };
        let mut view = view_of(&graph, area);
        view.nodes = graph
            .nodes
            .iter()
            .filter(|node| view.layout.rect_of(node.idx).is_some())
            .map(|node| project_node(&graph, node, &presentation, 0))
            .collect();

        let screen = screen_of(&view, area);
        let detail: Vec<&str> = screen
            .lines()
            .skip(view.detail_rect.y as usize)
            .take(view.detail_rect.height as usize)
            .collect();
        assert!(
            detail
                .first()
                .is_some_and(|line| line.contains("not delivered:")),
            "{detail:?}"
        );
        assert!(
            detail
                .first()
                .is_some_and(|line| line.contains("pane_not_found")),
            "{detail:?}"
        );
    }

    /// 2.8: the two states with opposite remedies must not share a color, and
    /// the node the run is stuck behind is never quieter than the cursor.
    #[test]
    fn needs_attention_is_amber_and_a_blocking_node_is_emphasised() {
        let palette = Palette::catppuccin();
        assert_ne!(
            node_status_color(NodeStatus::NeedsAttention, &palette),
            node_status_color(NodeStatus::Failed, &palette)
        );
        assert_eq!(
            node_status_color(NodeStatus::NeedsAttention, &palette),
            palette.peach
        );

        let mut graph = diamond();
        graph.status = RunStatus::Paused;
        graph.nodes[1].status = NodeStatus::NeedsAttention;
        let view = view_of(&graph, Rect::new(0, 0, 120, 40));
        assert!(view.is_blocking(RunNodeIdx(1)));
        assert!(!view.is_blocking(RunNodeIdx(2)));

        // A running run has no blocking node, however a node is doing.
        let mut running = diamond();
        running.nodes[1].status = NodeStatus::NeedsAttention;
        let view = view_of(&running, Rect::new(0, 0, 120, 40));
        assert!(!view.is_blocking(RunNodeIdx(1)));
    }

    #[test]
    fn truncate_spans_never_exceeds_the_band_and_always_marks_the_cut() {
        let line = || {
            vec![
                Span::raw("workflow_run:ue3nqrnztwlifx4a2m3g".to_string()),
                Span::raw(" · ".to_string()),
                Span::raw("running".to_string()),
            ]
        };
        let full: usize = line().iter().map(|span| display_width(&span.content)).sum();

        // A band that fits keeps the line untouched.
        assert_eq!(truncate_spans(line(), full), line());

        for width in 0..=full {
            let cut = truncate_spans(line(), width);
            let rendered: String = cut.iter().map(|span| span.content.to_string()).collect();
            assert!(display_width(&rendered) <= width, "{width}: {rendered:?}");
            if width < full && width > 0 {
                assert!(rendered.ends_with('…'), "{width}: {rendered:?}");
                // Never a plausible-but-wrong id: the cut is always marked.
                assert!(!rendered.contains("ue3nqrnztwlifx4a2m3g ·") || rendered.ends_with('…'));
            }
        }
    }

    /// 2.27: the footer drops hints whole, and never the way out.
    #[test]
    fn footer_hints_degrade_without_ever_losing_escape() {
        let full = footer_hints(200, false, true, true);
        assert_eq!(full.len(), FOOTER_HINTS.len());

        let mut previous = full.len();
        for width in (0..=60).rev() {
            let hints = footer_hints(width, false, true, true);
            assert!(hints.len() <= previous, "width {width} gained a hint");
            previous = hints.len();
            let rendered: String = hints
                .iter()
                .enumerate()
                .map(|(index, (chord, label))| {
                    format!("{}{chord}{label}", if index == 0 { " " } else { "  " })
                })
                .collect();
            assert!(display_width(&rendered) <= width, "{width}: {rendered:?}");
            if !hints.is_empty() {
                assert_eq!(hints.last(), Some(&("esc", " close")), "width {width}");
            }
        }
    }

    /// 2.21: one click selects, two focus.
    #[test]
    fn a_second_click_on_the_same_box_is_a_double_click() {
        use std::time::{Duration, Instant};

        let mut view = view_of(&diamond(), Rect::new(0, 0, 120, 40));
        let now = Instant::now();
        assert!(!view.register_click(RunNodeIdx(0), 4, 1, now));
        assert!(view.register_click(RunNodeIdx(0), 4, 1, now + Duration::from_millis(120)));
        // The double click is consumed, so a third click starts over.
        assert!(!view.register_click(RunNodeIdx(0), 4, 1, now + Duration::from_millis(200)));

        // Too slow, a different box, or too far away is a fresh single click.
        assert!(!view.register_click(RunNodeIdx(0), 4, 1, now + Duration::from_secs(5)));
        assert!(!view.register_click(RunNodeIdx(1), 4, 1, now + Duration::from_millis(5100)));
    }

    /// Pinned: a `None` banner must return rects byte-identical to the
    /// pre-Phase-2 four-band layout, so every number here is exactly what it
    /// was before the fifth band existed (`06-phase2-plan.md` §3 frozen
    /// interface 10) — only the destructuring grew from four to five.
    #[test]
    fn overlay_bands_partition_the_area_without_overlap() {
        let area = Rect::new(0, 0, 80, 24);
        let (header, banner, graph, detail, footer) = overlay_areas(area, None);
        assert_eq!(header.y, area.y);
        assert_eq!(header.bottom(), banner.y);
        assert_eq!(banner.height, 0, "no banner costs zero rows");
        assert_eq!(banner.bottom(), graph.y);
        assert_eq!(graph.bottom(), detail.y);
        assert_eq!(detail.bottom(), footer.y);
        assert_eq!(footer.bottom(), area.bottom());
        assert_eq!(detail.height, DETAIL_HEIGHT);
        assert_eq!(footer.height, FOOTER_HEIGHT);
    }

    /// A banner takes its row from the detail strip before it ever touches
    /// the graph's one guaranteed row — priority `footer → banner → graph →
    /// detail` (`06-phase2-plan.md` §1 WS-G). `height = 7` is exactly
    /// header(1) + footer(1) + detail(4) + graph(1), so there is no slack:
    /// the banner's extra row has to come from somewhere, and it must not
    /// come from the graph.
    #[test]
    fn a_present_banner_shrinks_detail_before_the_graphs_one_guaranteed_row() {
        let area = Rect::new(0, 0, 80, 7);
        let (header, banner, graph, detail, footer) = overlay_areas(area, None);
        assert_eq!(banner.height, 0);
        assert_eq!(detail.height, DETAIL_HEIGHT);
        assert_eq!(graph.height, 1);
        assert_eq!(
            header.height + graph.height + detail.height + footer.height,
            7
        );

        let (header, banner, graph, detail, footer) = overlay_areas(area, Some("growth limited"));
        assert_eq!(banner.height, BANNER_HEIGHT);
        assert_eq!(header.bottom(), banner.y);
        assert_eq!(banner.bottom(), graph.y);
        assert_eq!(graph.bottom(), detail.y);
        assert_eq!(detail.bottom(), footer.y);
        assert_eq!(footer.bottom(), area.bottom());
        // The graph keeps its one row; the detail strip is what shrank.
        assert_eq!(graph.height, 1, "the graph keeps at least one row");
        assert_eq!(detail.height, DETAIL_HEIGHT - BANNER_HEIGHT);
        assert_eq!(
            header.height + banner.height + graph.height + detail.height + footer.height,
            7
        );
    }

    #[test]
    fn a_tiny_area_still_partitions_without_panicking() {
        for height in 0..=6u16 {
            let area = Rect::new(0, 0, 20, height);
            let (header, banner, graph, detail, footer) = overlay_areas(area, None);
            assert_eq!(
                header.height + banner.height + graph.height + detail.height + footer.height,
                height
            );
            let (header, banner, graph, detail, footer) =
                overlay_areas(area, Some("growth limited"));
            assert_eq!(
                header.height + banner.height + graph.height + detail.height + footer.height,
                height
            );
        }
    }

    /// Hit-testing reads `view.layout`, stored inside `view.graph_rect`, so a
    /// banner shifting the graph band down must not desync the two — this is
    /// the WS-G "hit-test still agrees with stored geometry when a banner is
    /// present" case.
    #[test]
    fn hit_test_still_agrees_with_stored_geometry_when_a_banner_is_present() {
        let view = view_of_full(
            &diamond(),
            Rect::new(0, 0, 120, 40),
            "",
            &std::collections::HashMap::new(),
            Some("growth limited · max_nodes 12 reached · 2 of 4 requested nodes created"),
        );
        assert_eq!(view.banner_rect.height, BANNER_HEIGHT);
        assert_eq!(view.nodes.len(), 4);

        for (idx, rect) in &view.layout.nodes {
            assert!(
                contains_rect(to_layout_rect(view.graph_rect), *rect),
                "{rect:?} escapes {:?}",
                view.graph_rect
            );
            for row in rect.y..rect.bottom() {
                for col in rect.x..rect.right() {
                    assert_eq!(view.node_at(col, row), Some(*idx), "({col},{row})");
                }
            }
        }
        // The banner band itself hit-tests to no node.
        for col in view.banner_rect.x..view.banner_rect.right() {
            assert_eq!(view.node_at(col, view.banner_rect.y), None);
        }
    }

    /// The banner renders in the palette's warning slot and costs the row it
    /// claims; the per-node growth notice renders inside the proposing node's
    /// box when the box has a spare interior row.
    #[test]
    fn render_draws_the_banner_and_a_per_node_growth_notice() {
        let mut view = view_of_full(
            &diamond(),
            Rect::new(0, 0, 100, 30),
            "",
            &std::collections::HashMap::new(),
            Some("growth limited · max_nodes 12 reached · 2 of 4 requested nodes created"),
        );
        // `NODE_HEIGHT` (`workflow/layout.rs`) gives a box exactly one
        // interior row shared with the status text, so this stays short
        // enough to survive `truncate_spans` unclipped — the truncation path
        // itself is covered by `narrow_bands_are_truncated_with_an_ellipsis_not_hard_clipped`.
        if let Some(node) = view.nodes.first_mut() {
            node.growth_notice = Some("cap 4".to_string());
        }
        let area = Rect::new(0, 0, 100, 30);
        let screen = screen_of(&view, area);
        assert!(
            screen.contains("growth limited · max_nodes 12 reached"),
            "{screen}"
        );
        assert!(screen.contains("cap 4"), "{screen}");
    }

    /// Regression for the retest P2: the node box has exactly one interior
    /// row shared with the status text (`NODE_HEIGHT`,
    /// `workflow/layout.rs`), so a growth notice longer than the box's title
    /// width is elided there — by design, and covered above. The retest found
    /// the detail strip did not pick up the slack: selecting the node that
    /// hit the limit showed path/status/model/usage/result and no growth
    /// line at all, so the limit "renders but conveys nothing"
    /// (`06-phase2-plan.md` §4 D11's guarantee needs *one* surface to carry
    /// the full text, not just the fact that a limit exists). The detail
    /// strip is the overlay's one band wide enough to carry it whole.
    #[test]
    fn detail_strip_carries_the_full_growth_notice_the_node_box_elides() {
        let mut view = view_of(&diamond(), Rect::new(0, 0, 100, 30));
        let long_notice =
            "growth limited: max_nodes 12 · 6 of 8 requested nodes created for this proposal";
        if let Some(node) = view.nodes.first_mut() {
            node.growth_notice = Some(long_notice.to_string());
        }
        assert_eq!(
            view.selected,
            Some(RunNodeIdx(0)),
            "start is selected by default"
        );

        let screen = screen_of(&view, Rect::new(0, 0, 100, 30));
        assert!(
            screen.contains(long_notice),
            "detail strip must render the whole growth notice, not the node box's elided form: {screen}"
        );
    }

    // ── WS-H: historical runs and the detached interrogation lane ───────────

    fn interrogation(id: &str, path: &str, pane: Option<&str>) -> HistoricalInterrogation {
        HistoricalInterrogation {
            id: id.to_string(),
            path: path.to_string(),
            pane_id: pane.map(str::to_string),
            reconstructed: false,
            ended: pane.is_none(),
        }
    }

    /// A finished run, as `load_historical_run` would hand it over: every node
    /// terminal, so nothing in the projection depends on the wall clock.
    fn finished_graph() -> RunGraph {
        let mut graph = diamond();
        graph.status = RunStatus::Succeeded;
        for node in &mut graph.nodes {
            node.status = NodeStatus::Succeeded;
        }
        graph
    }

    fn historical_app(graph: RunGraph, interrogations: Vec<HistoricalInterrogation>) -> AppState {
        let mut app = AppState::test_new();
        app.mode = Mode::WorkflowDag;
        app.set_historical_run(Some(crate::app::state::HistoricalRunSnapshot {
            graph: Box::new(graph),
            workflow_name: "ux-dag-probe".to_string(),
            interrogations,
            team_name: None,
            lead_pane_id: None,
            projected: std::collections::BTreeMap::new(),
            members: Vec::new(),
        }));
        app
    }

    fn live_app(graph: RunGraph) -> AppState {
        let mut app = AppState::test_new();
        app.mode = Mode::WorkflowDag;
        app.run_graph = Some(Box::new(graph));
        app
    }

    fn render_of(app: &AppState, area: Rect) -> String {
        use ratatui::{backend::TestBackend, Terminal};

        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("term");
        terminal
            .draw(|frame| render_workflow_dag(app, frame, area))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ── the agent-teams rework (`09-agent-teams-rework.md` §3.4, §3.5) ─────

    /// A lead run's graph: one node the definition planned, one the team added
    /// on its own. The emergent node lives in the reserved namespace and is
    /// named after its subject, which is exactly what the store writes for a
    /// task no definition planned (`workflow/store/mod.rs`).
    fn lead_graph() -> RunGraph {
        let mut plan = test_node(0, "plan");
        plan.label = "plan".to_string();
        plan.status = NodeStatus::Running;
        let mut emergent = test_node(1, ".task/7");
        emergent.label = "retest".to_string();
        emergent.status = NodeStatus::Running;
        RunGraph {
            run_id: RunId::new("workflow_run:lead"),
            version_id: KvdagVersionId::new("v1"),
            tier: Tier::Auto,
            growth: GrowthLimits::default(),
            assignments: std::collections::BTreeMap::new(),
            nodes: vec![plan, emergent],
            edges: vec![test_edge(0, 1)],
            status: RunStatus::Running,
            seq: 0,
            epilogue: None,
        }
    }

    fn member(name: &str, pane: Option<&str>) -> crate::api::schema::WorkflowRunMemberInfo {
        crate::api::schema::WorkflowRunMemberInfo {
            name: name.to_string(),
            agent_type: "Explore".to_string(),
            model: "sonnet".to_string(),
            pane_id: pane.map(str::to_string),
            backend_type: "tmux".to_string(),
            is_active: true,
            cwd: None,
            first_seen_at_unix_ms: 1,
            last_seen_at_unix_ms: 2,
        }
    }

    /// A lead run as the overlay holds it: the snapshot above, a team whose
    /// `verify` member owns the emergent node, and a **real** pane behind that
    /// member — the live-agent-state layer reads karvex's own per-pane
    /// detection, so there has to be a pane for it to have read.
    fn lead_app(agent_state: crate::detect::AgentState) -> AppState {
        let mut app = AppState::test_new();
        app.mode = Mode::WorkflowDag;
        app.workspaces = vec![crate::workspace::Workspace::test_new("teams")];
        app.ensure_test_terminals();
        let pane = app.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.workspaces[0].tabs[0].panes[&pane]
            .attached_terminal_id
            .clone();
        if let Some(terminal) = app.terminals.get_mut(&terminal_id) {
            terminal.state = agent_state;
        }
        let number = app.workspaces[0]
            .public_pane_number(pane)
            .expect("the test workspace numbers its root pane");
        let owner_pane = crate::workspace::public_pane_id_for_number(&app.workspaces[0].id, number);

        let projected = [
            (
                "plan".to_string(),
                ProjectedNodeFacts {
                    task_id: Some("1".to_string()),
                    subject: "plan".to_string(),
                    owner: "research".to_string(),
                    emergent: false,
                },
            ),
            (
                ".task/7".to_string(),
                ProjectedNodeFacts {
                    task_id: Some("7".to_string()),
                    subject: "retest the parser".to_string(),
                    owner: "verify".to_string(),
                    emergent: true,
                },
            ),
        ]
        .into_iter()
        .collect();

        app.set_historical_run(Some(crate::app::state::HistoricalRunSnapshot {
            graph: Box::new(lead_graph()),
            workflow_name: "ship-it".to_string(),
            interrogations: Vec::new(),
            team_name: Some("session-213aa9bf".to_string()),
            lead_pane_id: Some("w1:p1".to_string()),
            projected,
            members: vec![
                member("research", None),
                member("verify", Some(&owner_pane)),
            ],
        }));
        app
    }

    fn selected_path(app: &mut AppState, path: &str) {
        let idx = app
            .view
            .dag
            .nodes
            .iter()
            .find(|node| node.path == path)
            .map(|node| node.idx)
            .unwrap_or_else(|| panic!("no node at {path}"));
        app.view.dag.selected = Some(idx);
    }

    /// §3.4's two layers, on screen: the emergent box says it is not in the
    /// plan, and the detail strip answers both "who holds this" and "what is
    /// their pane doing right now".
    #[test]
    fn a_lead_run_marks_emergent_nodes_and_shows_owner_and_agent_state() {
        let area = Rect::new(0, 0, 120, 40);
        let mut app = lead_app(crate::detect::AgentState::Blocked);
        app.view.dag = compute_workflow_dag_view(&app, area);
        assert!(app.view.dag.lead_run, "the snapshot names a team");

        // The reserved prefix marks the box the definition never planned, and
        // only that box.
        let screen = render_of(&app, area);
        assert!(screen.contains(".retest"), "{screen}");
        assert!(!screen.contains(".plan"), "{screen}");

        selected_path(&mut app, ".task/7");
        let screen = render_of(&app, area);
        assert!(screen.contains("emergent"), "{screen}");
        assert!(screen.contains("owner verify"), "{screen}");
        assert!(screen.contains("needs input"), "{screen}");
        // The team reworded the task, and the difference is what earns the row.
        assert!(screen.contains("task: retest the parser"), "{screen}");

        // An unclaimed-looking node whose subject repeats its label says so
        // without repeating itself.
        selected_path(&mut app, "plan");
        let screen = render_of(&app, area);
        assert!(screen.contains("owner research"), "{screen}");
        assert!(!screen.contains("task: plan"), "{screen}");
    }

    /// §3.5: the verbs collapse. Steer, interrogate, and reconstruct are engine
    /// inputs and a lead run has no engine, so offering them would cost the
    /// user a keystroke to discover a refusal.
    #[test]
    fn a_lead_runs_footer_offers_focus_and_none_of_the_engine_verbs() {
        let area = Rect::new(0, 0, 120, 40);
        let mut app = lead_app(crate::detect::AgentState::Working);
        app.view.dag = compute_workflow_dag_view(&app, area);
        selected_path(&mut app, ".task/7");
        let screen = render_of(&app, area);

        assert!(screen.contains("focus pane"), "{screen}");
        assert!(!screen.contains("steer"), "{screen}");
        assert!(!screen.contains("interrogate"), "{screen}");
        assert!(!screen.contains("reconstruct"), "{screen}");
        assert!(screen.contains("esc"), "{screen}");
    }

    /// The narrow half of the engine-era pin: which hint set a run gets is
    /// decided by how the run was executed, and the engine-era set is untouched.
    #[test]
    fn an_engine_era_footer_keeps_every_verb_a_lead_run_drops() {
        let engine = footer_hints(200, false, true, true);
        let lead = footer_hints(200, true, false, true);
        for verb in [" steer", " interrogate", " reconstruct"] {
            assert!(
                engine.iter().any(|(_, label)| *label == verb),
                "engine-era footer lost {verb}: {engine:?}"
            );
            assert!(
                !lead.iter().any(|(_, label)| *label == verb),
                "a lead run must not offer {verb}: {lead:?}"
            );
        }
        assert!(lead
            .iter()
            .any(|(chord, label)| *chord == "enter" && *label == " focus pane"));
        assert!(lead.iter().any(|(chord, _)| *chord == "esc"));
    }

    /// The overlay never guesses which pane a node belongs to: the owner is
    /// resolved through the run's member list, and the member's pane is what
    /// the live-agent-state layer reads. A member with no pane resolves to
    /// nothing rather than to some other member's pane.
    #[test]
    fn a_nodes_owner_resolves_through_the_member_list_to_a_pane() {
        let area = Rect::new(0, 0, 120, 40);
        let app = lead_app(crate::detect::AgentState::Working);
        let view = compute_workflow_dag_view(&app, area);

        let emergent = view
            .nodes
            .iter()
            .find(|node| node.path == ".task/7")
            .expect("the emergent node is drawn");
        assert_eq!(emergent.owner, "verify");
        assert!(emergent.emergent);
        assert!(emergent.owner_pane_id.is_some());
        assert_eq!(
            emergent.agent_state,
            Some(crate::detect::AgentState::Working)
        );

        let planned = view
            .nodes
            .iter()
            .find(|node| node.path == "plan")
            .expect("the planned node is drawn");
        assert_eq!(planned.owner, "research");
        assert!(!planned.emergent);
        assert_eq!(
            planned.owner_pane_id, None,
            "a member with no pane resolves to no pane"
        );
        assert_eq!(planned.agent_state, None);
    }

    /// The zero-height-when-absent rule, at the level that matters: a
    /// historical run with no interrogations must produce **exactly** the
    /// geometry the same graph produces live. Every pinned number in this file
    /// is a number about that geometry (`07-phase3-plan.md` §1 WS-H).
    #[test]
    fn an_absent_interrogation_lane_leaves_the_graph_geometry_untouched() {
        let area = Rect::new(0, 0, 120, 40);
        let live = compute_workflow_dag_view(&live_app(finished_graph()), area);
        let past = compute_workflow_dag_view(&historical_app(finished_graph(), Vec::new()), area);

        assert!(!live.historical);
        assert!(past.historical);
        assert!(past.interrogation_nodes.is_empty());
        assert_eq!(past.graph_rect, live.graph_rect);
        assert_eq!(past.detail_rect, live.detail_rect);
        assert_eq!(past.footer_rect, live.footer_rect);
        assert_eq!(past.banner_rect, live.banner_rect);
        assert_eq!(
            past.layout, live.layout,
            "an empty lane must not move a single box"
        );
    }

    /// The lane takes its rows from the bottom of the graph band, never from
    /// the bands `overlay_areas` already partitioned, and never from a box.
    #[test]
    fn the_interrogation_lane_sits_below_the_graph_without_overlapping_it() {
        let area = Rect::new(0, 0, 120, 40);
        let bare = compute_workflow_dag_view(&historical_app(finished_graph(), Vec::new()), area);
        let app = historical_app(
            finished_graph(),
            vec![
                interrogation("i1", "start", Some("pane-1")),
                interrogation("i2", "left", None),
            ],
        );
        let view = compute_workflow_dag_view(&app, area);

        assert_eq!(view.interrogation_nodes.len(), 2);
        // The overlay's five bands are untouched: the lane is carved out of the
        // graph band's interior, not out of the header/detail/footer budget.
        assert_eq!(view.graph_rect, bare.graph_rect);
        assert_eq!(view.detail_rect, bare.detail_rect);
        assert_eq!(view.footer_rect, bare.footer_rect);

        let band = to_layout_rect(view.graph_rect);
        for item in &view.interrogation_nodes {
            let rect = to_layout_rect(item.rect);
            assert!(contains_rect(band, rect), "{rect:?} escapes {band:?}");
            for (_, node) in &view.layout.nodes {
                assert!(!node.intersects(&rect), "{node:?} overlaps {rect:?}");
                assert!(node.bottom() <= rect.y, "the lane must sit below every box");
            }
            for (x, y) in view.layout.edge_cells().keys() {
                assert!(!rect.contains(*x, *y), "an edge cell landed in the lane");
            }
        }
        for i in 0..view.interrogation_nodes.len() {
            for j in (i + 1)..view.interrogation_nodes.len() {
                let (a, b) = (
                    to_layout_rect(view.interrogation_nodes[i].rect),
                    to_layout_rect(view.interrogation_nodes[j].rect),
                );
                assert!(!a.intersects(&b));
            }
        }
    }

    /// §4 D8: an interrogation is not a run node anywhere. The two hit-tests
    /// are separate namespaces, and a click in one must never answer from the
    /// other — otherwise a click on an interrogation box selects a graph node
    /// that some later caller would steer or restart.
    #[test]
    fn the_interrogation_lane_hit_tests_in_its_own_namespace() {
        let area = Rect::new(0, 0, 120, 40);
        let app = historical_app(
            finished_graph(),
            vec![
                interrogation("i1", "start", Some("pane-1")),
                interrogation("i2", "left", None),
            ],
        );
        let view = compute_workflow_dag_view(&app, area);

        for item in &view.interrogation_nodes {
            for row in item.rect.y..item.rect.bottom() {
                for col in item.rect.x..item.rect.right() {
                    assert_eq!(
                        view.node_at(col, row),
                        None,
                        "({col},{row}) resolved to a graph node"
                    );
                    assert_eq!(
                        workflow_dag_interrogation_at(&view, col, row).map(|hit| hit.id.as_str()),
                        Some(item.id.as_str())
                    );
                }
            }
        }
        // And every cell of every drawn node box answers only the node lookup.
        for (idx, rect) in &view.layout.nodes {
            assert_eq!(view.node_at(rect.x, rect.y), Some(*idx));
            assert!(workflow_dag_interrogation_at(&view, rect.x, rect.y).is_none());
        }
    }

    /// A box that does not fit the lane is absent from the projection, so it
    /// is neither drawn nor clickable — the graph band's own clipping rule,
    /// applied to the lane.
    #[test]
    fn interrogation_boxes_that_do_not_fit_are_dropped_from_the_projection() {
        let area = Rect::new(0, 0, 50, 40);
        let app = historical_app(
            finished_graph(),
            (1..=6)
                .map(|n| interrogation(&format!("i{n}"), "start", None))
                .collect(),
        );
        let view = compute_workflow_dag_view(&app, area);
        assert!(!view.interrogation_nodes.is_empty());
        assert!(view.interrogation_nodes.len() < 6, "nothing was dropped");
        for item in &view.interrogation_nodes {
            assert!(item.rect.right() <= view.graph_rect.right());
        }
    }

    /// A graph band with no room to spare keeps its one guaranteed row rather
    /// than spending it on a lane the boxes could not fit into anyway.
    #[test]
    fn a_band_too_short_for_the_lane_keeps_the_graphs_rows() {
        for height in 0..=12u16 {
            let area = Rect::new(0, 0, 120, height);
            let app = historical_app(
                finished_graph(),
                vec![interrogation("i1", "start", Some("pane-1"))],
            );
            let view = compute_workflow_dag_view(&app, area);
            let lane = lane_height_for(view.graph_rect.height, 1);
            assert!(
                lane == 0 || view.graph_rect.height > lane,
                "height {height}: the lane took the graph's last row"
            );
            for item in &view.interrogation_nodes {
                assert!(contains_rect(
                    to_layout_rect(view.graph_rect),
                    to_layout_rect(item.rect)
                ));
            }
        }
    }

    /// The same stored run projects identically twice — the "renders byte-stably"
    /// requirement. Every node is terminal, so nothing here reads the clock.
    #[test]
    fn a_historical_snapshot_projects_byte_stably() {
        let area = Rect::new(0, 0, 120, 40);
        let app = historical_app(
            finished_graph(),
            vec![interrogation("i1", "start", Some("pane-1"))],
        );
        let first = compute_workflow_dag_view(&app, area);
        let second = compute_workflow_dag_view(&app, area);
        assert_eq!(first, second);
        assert_eq!(first.workflow_name, "ux-dag-probe");
    }

    /// A past run must be visibly a past run, and must not advertise a key
    /// whose only possible answer is a refusal (`workflow_run_not_active`).
    #[test]
    fn a_historical_run_says_so_and_offers_no_steer() {
        let area = Rect::new(0, 0, 120, 40);
        let app = historical_app(
            finished_graph(),
            vec![interrogation("i1", "start", Some("pane-1"))],
        );
        let mut app = app;
        app.view.dag = compute_workflow_dag_view(&app, area);
        let screen = render_of(&app, area);

        assert!(screen.contains("past run"), "{screen}");
        assert!(!screen.contains("steer"), "{screen}");
        assert!(screen.contains("interrogate"), "{screen}");
        assert!(screen.contains("esc"), "{screen}");
        // The live projection of the same graph keeps the steer hint.
        let mut live = live_app(finished_graph());
        live.view.dag = compute_workflow_dag_view(&live, area);
        let live_screen = render_of(&live, area);
        assert!(live_screen.contains("steer"), "{live_screen}");
        assert!(!live_screen.contains("past run"), "{live_screen}");
    }

    /// `Enter` on a past node has nothing to focus once its pane is gone, so
    /// the hint goes with it — and comes back for the rare pane that outlived
    /// its run.
    #[test]
    fn the_focus_hint_follows_whether_a_pane_is_still_there() {
        assert!(footer_hints(200, false, false, false)
            .iter()
            .all(|(chord, _)| *chord != "enter" && *chord != "s"));
        assert!(footer_hints(200, false, false, true)
            .iter()
            .any(|(chord, _)| *chord == "enter"));
        assert!(footer_hints(200, false, false, true)
            .iter()
            .all(|(chord, _)| *chord != "s"));
        // Whatever is disabled, the way out survives.
        for width in 0..=60 {
            let hints = footer_hints(width, false, false, false);
            if !hints.is_empty() {
                assert_eq!(hints.last(), Some(&("esc", " close")), "width {width}");
            }
        }
    }

    /// §4 D4: `↺ restored` without "restored from what" is the half of the
    /// fact nobody can act on, so the detail strip carries the provenance.
    #[test]
    fn a_restored_node_renders_its_status_and_its_provenance() {
        let palette = Palette::catppuccin();
        assert_eq!(
            node_status_color(NodeStatus::Restored, &palette),
            palette.teal
        );
        assert_eq!(node_status_glyph(NodeStatus::Restored), "↺");
        assert_eq!(node_status_label(NodeStatus::Restored), "restored");

        let mut graph = finished_graph();
        graph.nodes[0].status = NodeStatus::Restored;
        graph.nodes[0].restored_from = Some(RestoredRef {
            run: RunId::new("workflow_run:old"),
            node_key: NodeKey::new("start"),
            checkpoint_seq: 3,
        });

        let area = Rect::new(0, 0, 140, 40);
        let mut app = historical_app(graph, Vec::new());
        app.view.dag = compute_workflow_dag_view(&app, area);
        assert_eq!(app.view.dag.selected, Some(RunNodeIdx(0)));
        let screen = render_of(&app, area);

        assert!(screen.contains("restored"), "{screen}");
        assert!(
            screen.contains("restored from workflow_run:old · start · checkpoint 3"),
            "{screen}"
        );

        // Selecting a node that was not restored shows no provenance at all.
        app.view.dag.selected = Some(RunNodeIdx(1));
        let screen = render_of(&app, area);
        assert!(!screen.contains("checkpoint 3"), "{screen}");
    }

    /// §4 D1's post-`RunFinished` contract, on screen: a succeeded run with a
    /// still-working summariser reads truthfully, and it costs no band.
    #[test]
    fn the_header_reports_the_summarisers_state_without_a_band() {
        use crate::workflow::model::EpilogueState;

        let area = Rect::new(0, 0, 140, 40);
        let with_phase = |phase: EpiloguePhase| {
            let mut graph = finished_graph();
            graph.epilogue = Some(EpilogueState {
                node: RunNodeIdx(0),
                phase,
                runner: crate::workflow::model::Runner::Agent,
            });
            let mut app = live_app(graph);
            app.view.dag = compute_workflow_dag_view(&app, area);
            (app.view.dag.banner_rect.height, render_of(&app, area))
        };

        for phase in [EpiloguePhase::Pending, EpiloguePhase::Running] {
            let (banner_rows, screen) = with_phase(phase);
            assert_eq!(banner_rows, 0, "the epilogue must not claim a band");
            assert!(screen.contains("summarising…"), "{phase:?}\n{screen}");
            assert!(screen.contains("succeeded"), "{phase:?}\n{screen}");
        }

        let (_, screen) = with_phase(EpiloguePhase::GaveUp);
        assert!(screen.contains("summary failed"), "{screen}");

        let (_, screen) = with_phase(EpiloguePhase::Done);
        assert!(!screen.contains("summarising"), "{screen}");
        assert!(!screen.contains("summary failed"), "{screen}");
    }

    /// The lane draws its own vocabulary: `⌕`, the source path, and — for the
    /// degraded path — the word that stops it being mistaken for the original
    /// session (§4 D7).
    #[test]
    fn the_lane_draws_live_and_ended_interrogations_differently() {
        let area = Rect::new(0, 0, 140, 40);
        let mut reconstructed = interrogation("i2", "left", None);
        reconstructed.reconstructed = true;
        let mut app = historical_app(
            finished_graph(),
            vec![interrogation("i1", "start", Some("pane-7")), reconstructed],
        );
        app.view.dag = compute_workflow_dag_view(&app, area);

        assert_eq!(
            app.view.dag.interrogation_nodes[0].label,
            "interrogate · start"
        );
        assert_eq!(
            app.view.dag.interrogation_nodes[1].label,
            "reconstructed · left"
        );

        // The kind is readable off the label, which is the only place the
        // landed `DagInterrogationView` records it.
        assert_eq!(
            interrogation_kind(&app.view.dag.interrogation_nodes[0]),
            "interrogate"
        );
        assert_eq!(
            interrogation_kind(&app.view.dag.interrogation_nodes[1]),
            "reconstructed"
        );

        let screen = render_of(&app, area);
        assert!(screen.contains('⌕'), "{screen}");
        // The identifying path survives in the title; the kind gets its own
        // row, so neither is truncated away by the other.
        assert!(screen.contains("⌕ start"), "{screen}");
        assert!(screen.contains("⌕ left"), "{screen}");
        assert!(screen.contains("interrogate"), "{screen}");
        assert!(screen.contains("reconstructed"), "{screen}");
        assert!(screen.contains("ended"), "{screen}");
    }

    /// Closing the historical run must put the overlay back on the live one —
    /// the projection is chosen per frame, so a stale snapshot left behind
    /// would keep showing a past run over a live one.
    #[test]
    fn clearing_the_snapshot_returns_the_overlay_to_the_live_run() {
        let area = Rect::new(0, 0, 120, 40);
        let mut app = live_app(finished_graph());
        app.set_historical_run(Some(crate::app::state::HistoricalRunSnapshot {
            graph: Box::new(finished_graph()),
            workflow_name: "past".to_string(),
            interrogations: vec![interrogation("i1", "start", None)],
            team_name: None,
            lead_pane_id: None,
            projected: std::collections::BTreeMap::new(),
            members: Vec::new(),
        }));
        let view = compute_workflow_dag_view(&app, area);
        assert!(view.historical);
        assert_eq!(view.workflow_name, "past");
        assert_eq!(view.interrogation_nodes.len(), 1);

        app.set_historical_run(None);
        let view = compute_workflow_dag_view(&app, area);
        assert!(!view.historical);
        assert!(view.interrogation_nodes.is_empty());
    }

    /// Selection is carried by instance path (`carried_selection`), which is
    /// what keeps it stable when expansion appends new nodes to the graph
    /// rather than reordering the existing ones — the WS-G "selection
    /// survives appending five nodes to a graph" case.
    #[test]
    fn selection_survives_appending_expansion_nodes() {
        let graph = diamond();
        // Wide enough that six nodes sharing "right"'s next layer (the
        // original "end" plus five expansion children) never need clipping —
        // this test is about selection stability, not the graph band's width
        // budget, which `a_short_graph_band_drops_boxes_instead_of_drawing_off_screen`
        // already covers.
        let area = Rect::new(0, 0, 220, 40);
        let previous = {
            let mut view = view_of(&graph, area);
            view.selected = Some(RunNodeIdx(2)); // "right"
            view
        };

        let mut grown = graph.clone();
        for n in 1..=5u8 {
            let child_idx = 3 + n as usize;
            let mut child = test_node(child_idx, "right/worker");
            child.path = InstancePath::new(format!("right/worker/{n}"));
            child.parent = Some(RunNodeIdx(2));
            child.depth = 1;
            grown.nodes.push(child);
            // The `sequence` parent→child edge every accepted expansion child
            // gets (§4 D4), so the appended nodes stay part of the same
            // connected graph the layout places rather than becoming stray
            // disconnected roots.
            grown.edges.push(test_edge(2, child_idx));
        }
        let next = view_of(&grown, area);

        assert_eq!(next.nodes.len(), 9);
        assert_eq!(
            carried_selection(&previous, &next),
            Some(RunNodeIdx(2)),
            "the proposing node's selection must survive its own expansion"
        );
    }
}
