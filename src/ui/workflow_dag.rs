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
use super::text::truncate_end;
use super::widgets::panel_contrast_fg;
use crate::app::state::{AppState, DagNodeView, DagViewState, Mode, Palette};
use crate::workflow::layout::{layout, DagLayout, EdgeBits, LayoutRect};
use crate::workflow::model::{NodeStatus, RunGraph, RunNode, RunNodeIdx, RunStatus, Succession};

const HEADER_HEIGHT: u16 = 1;
/// Status line, model/usage line, summary line, blocker line.
const DETAIL_HEIGHT: u16 = 4;
const FOOTER_HEIGHT: u16 = 1;

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

    let (header_rect, graph_rect, detail_rect, footer_rect) = overlay_areas(area);
    let previous = &app.view.dag;
    let mut view = DagViewState {
        header_rect,
        graph_rect,
        detail_rect,
        footer_rect,
        ..DagViewState::default()
    };

    let Some(graph) = app.workflow_run_graph() else {
        return view;
    };

    view.run_id = graph.run_id.as_str().to_string();
    view.run_status = Some(graph.status);
    view.layout = clipped_layout(graph, graph_rect);
    view.nodes = graph
        .nodes
        .iter()
        .filter(|node| view.layout.rect_of(node.idx).is_some())
        .map(|node| project_node(graph, node))
        .collect();
    view.selected = carried_selection(previous, &view);
    // The steer line only survives while it still has a node to steer.
    view.steer = if view.selected.is_some() {
        previous.steer.clone()
    } else {
        None
    };
    view
}

/// Splits the full-bleed overlay into its four bands. A short terminal loses
/// the detail strip before the graph, and the graph before the footer.
fn overlay_areas(area: Rect) -> (Rect, Rect, Rect, Rect) {
    let header = Rect::new(area.x, area.y, area.width, HEADER_HEIGHT.min(area.height));
    let mut remaining = area.height.saturating_sub(header.height);

    let footer_height = FOOTER_HEIGHT.min(remaining);
    remaining = remaining.saturating_sub(footer_height);

    // The graph keeps at least one row; the detail strip yields first.
    let detail_height = DETAIL_HEIGHT.min(remaining.saturating_sub(1));
    remaining = remaining.saturating_sub(detail_height);

    let graph = Rect::new(area.x, header.bottom(), area.width, remaining);
    let detail = Rect::new(area.x, graph.bottom(), area.width, detail_height);
    let footer = Rect::new(area.x, detail.bottom(), area.width, footer_height);
    (header, graph, detail, footer)
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
    dag.edge_cells.retain(|(x, y), _| bounds.contains(*x, *y));
    dag
}

fn contains_rect(bounds: LayoutRect, rect: LayoutRect) -> bool {
    !rect.is_empty()
        && rect.x >= bounds.x
        && rect.y >= bounds.y
        && rect.right() <= bounds.right()
        && rect.bottom() <= bounds.bottom()
}

fn project_node(graph: &RunGraph, node: &RunNode) -> DagNodeView {
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
        label: node.key.as_str().to_string(),
        status: node.status,
        model: node.assignment.model.as_str().to_string(),
        effort: node.assignment.effort.as_str().to_string(),
        attempt: node.attempt,
        usage: node.usage,
        summary: node
            .result
            .as_ref()
            .map(|result| result.summary.clone())
            .filter(|summary| !summary.trim().is_empty()),
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
        successors,
        predecessors,
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
        DagNavDirection::Down => nearest_by_x(view, &node.successors, origin)
            .or_else(|| nearest_in_band(view, rect, origin, true)),
        DagNavDirection::Up => nearest_by_x(view, &node.predecessors, origin)
            .or_else(|| nearest_in_band(view, rect, origin, false)),
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

fn nearest_by_x(view: &DagViewState, candidates: &[RunNodeIdx], origin: u16) -> Option<RunNodeIdx> {
    candidates
        .iter()
        .filter_map(|idx| view.rect_of(*idx).map(|rect| (*idx, rect)))
        .min_by_key(|(idx, rect)| (centre_x(*rect).abs_diff(origin), rect.x, idx.0))
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

pub(super) fn render_workflow_dag(app: &AppState, frame: &mut Frame, area: Rect) {
    let dag = &app.view.dag;
    let p = &app.palette;

    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(p.panel_bg)),
        area,
    );

    render_header(dag, p, frame);
    if dag.is_empty() {
        render_empty_state(dag, p, frame);
    } else {
        render_edges(dag, p, frame);
        render_nodes(dag, p, frame);
        render_detail(dag, p, frame);
    }
    render_footer(dag, p, frame);
}

fn render_header(dag: &DagViewState, p: &Palette, frame: &mut Frame) {
    if dag.header_rect.height == 0 {
        return;
    }
    let mut spans = vec![Span::styled(
        " workflow run",
        Style::default().fg(p.text).add_modifier(Modifier::BOLD),
    )];
    if !dag.run_id.is_empty() {
        spans.push(Span::styled(
            format!(" {}", dag.run_id),
            Style::default().fg(p.subtext0),
        ));
    }
    if let Some(status) = dag.run_status {
        spans.push(Span::styled(" · ", Style::default().fg(p.overlay0)));
        spans.push(Span::styled(
            run_status_label(status),
            Style::default().fg(run_status_color(status, p)),
        ));
    }
    if !dag.is_empty() {
        spans.push(Span::styled(
            format!(
                " · {} nodes · {} running",
                dag.nodes.len(),
                dag.nodes
                    .iter()
                    .filter(|node| node.status == NodeStatus::Running)
                    .count()
            ),
            Style::default().fg(p.overlay0),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), dag.header_rect);
}

fn render_empty_state(dag: &DagViewState, p: &Palette, frame: &mut Frame) {
    if dag.graph_rect.height == 0 {
        return;
    }
    let line = Line::from(Span::styled(
        " no workflow run to show",
        Style::default().fg(p.overlay0),
    ));
    frame.render_widget(Paragraph::new(line), dag.graph_rect);
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

    let buf = frame.buffer_mut();
    let bounds = buf.area;
    for (&(x, y), &bits) in &dag.layout.edge_cells {
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
        let status_color = node_status_color(node.status, p);
        let border_color = if selected { p.accent } else { p.surface1 };

        let title_style = if selected {
            Style::default()
                .fg(panel_contrast_fg(p))
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text)
        };
        let label_width = rect.width.saturating_sub(4) as usize;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(Span::styled(
                format!(" {} ", truncate_end(&node.label, label_width)),
                title_style,
            ))
            .style(Style::default().bg(p.panel_bg));
        let inner = block.inner(rect);
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
        frame.render_widget(Paragraph::new(Line::from(spans)), inner);
    }
}

fn render_detail(dag: &DagViewState, p: &Palette, frame: &mut Frame) {
    if dag.detail_rect.height == 0 {
        return;
    }
    let Some(node) = dag.selected_node() else {
        return;
    };
    let width = dag.detail_rect.width.saturating_sub(1) as usize;
    let dim = Style::default().fg(p.overlay0);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(" ", dim),
            Span::styled(
                truncate_end(&node.path, width),
                Style::default().fg(p.text).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", dim),
            Span::styled(
                node_status_label(node.status),
                Style::default().fg(node_status_color(node.status, p)),
            ),
        ]),
        Line::from(vec![
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
                    node.usage.duration_ms / 1000
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
        ]),
    ];
    lines.push(match &node.summary {
        Some(summary) => Line::from(Span::styled(
            format!(" {}", truncate_end(summary, width)),
            Style::default().fg(p.subtext0),
        )),
        None => Line::from(Span::styled(" no checkpoint yet", dim)),
    });
    if let Some(blocker) = &node.blocker {
        lines.push(Line::from(Span::styled(
            format!(" {}", truncate_end(blocker, width)),
            Style::default().fg(p.red),
        )));
    }
    frame.render_widget(Paragraph::new(lines), dag.detail_rect);
}

fn render_footer(dag: &DagViewState, p: &Palette, frame: &mut Frame) {
    if dag.footer_rect.height == 0 {
        return;
    }
    let key = Style::default().fg(p.accent).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(p.overlay0);

    let line = if let Some(text) = &dag.steer {
        Line::from(vec![
            Span::styled(" steer", key),
            Span::styled(" › ", dim),
            Span::styled(
                truncate_end(text, dag.footer_rect.width.saturating_sub(12) as usize),
                Style::default().fg(p.text),
            ),
            Span::styled("▏", Style::default().fg(p.accent)),
        ])
    } else {
        Line::from(vec![
            Span::styled(" enter", key),
            Span::styled(" focus  ", dim),
            Span::styled("hjkl/↑↓←→", key),
            Span::styled(" move  ", dim),
            Span::styled("s", key),
            Span::styled(" steer  ", dim),
            Span::styled("esc", key),
            Span::styled(" close", dim),
        ])
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
        NodeStatus::NeedsAttention | NodeStatus::Blocked | NodeStatus::Failed => p.red,
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
            parent: None,
            depth: 0,
            status: NodeStatus::Pending,
            assignment: Assignment {
                model: ModelAlias::Sonnet,
                effort: Effort::Low,
            },
            attempt: 1,
            binding: None,
            result: None,
            usage: NodeUsage::default(),
            started_at_unix_ms: None,
            ended_at_unix_ms: None,
            progress: ProgressTracker::default(),
            succession: None,
            checkpoint_seq: 0,
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
        }
    }

    /// The projection the overlay would hold for `graph` in `area`, without
    /// needing an `AppState` wired to a live run.
    fn view_of(graph: &RunGraph, area: Rect) -> DagViewState {
        let (header_rect, graph_rect, detail_rect, footer_rect) = overlay_areas(area);
        let mut view = DagViewState {
            header_rect,
            graph_rect,
            detail_rect,
            footer_rect,
            run_id: graph.run_id.as_str().to_string(),
            run_status: Some(graph.status),
            ..DagViewState::default()
        };
        view.layout = clipped_layout(graph, graph_rect);
        view.nodes = graph
            .nodes
            .iter()
            .filter(|node| view.layout.rect_of(node.idx).is_some())
            .map(|node| project_node(graph, node))
            .collect();
        view.selected = carried_selection(&DagViewState::default(), &view);
        view
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
        for (x, y) in view.layout.edge_cells.keys() {
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

    #[test]
    fn render_draws_boxes_edges_and_the_selected_node_detail() {
        use ratatui::{backend::TestBackend, Terminal};

        let mut graph = diamond();
        graph.nodes[0].status = NodeStatus::Running;
        graph.nodes[0].succession = Some(crate::workflow::model::Succession::Blocked {
            reason: "waiting on review".into(),
            resume_when: "the reviewer replies".into(),
        });

        let area = Rect::new(0, 0, 100, 30);
        let mut app = AppState::test_new();
        app.mode = Mode::WorkflowDag;
        app.view.dag = view_of(&graph, area);
        assert_eq!(app.view.dag.selected, Some(RunNodeIdx(0)));

        // Shared borrow: `render_workflow_dag` cannot mutate what it draws.
        let app = &app;
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).expect("term");
        terminal
            .draw(|frame| render_workflow_dag(app, frame, area))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let screen = (0..area.height)
            .map(|row| {
                (0..area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Header, every node box, the edge glyphs, the arrowheads, the detail
        // strip for the selected node, and the hint bar.
        assert!(screen.contains("workflow run workflow_run:1"), "{screen}");
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

    #[test]
    fn overlay_bands_partition_the_area_without_overlap() {
        let area = Rect::new(0, 0, 80, 24);
        let (header, graph, detail, footer) = overlay_areas(area);
        assert_eq!(header.y, area.y);
        assert_eq!(header.bottom(), graph.y);
        assert_eq!(graph.bottom(), detail.y);
        assert_eq!(detail.bottom(), footer.y);
        assert_eq!(footer.bottom(), area.bottom());
        assert_eq!(detail.height, DETAIL_HEIGHT);
        assert_eq!(footer.height, FOOTER_HEIGHT);
    }

    #[test]
    fn a_tiny_area_still_partitions_without_panicking() {
        for height in 0..=6u16 {
            let area = Rect::new(0, 0, 20, height);
            let (header, graph, detail, footer) = overlay_areas(area);
            assert_eq!(
                header.height + graph.height + detail.height + footer.height,
                height
            );
        }
    }
}
