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
    AppState, DagNodeView, DagRunCounts, DagViewState, Mode, Palette, WorkflowRunPresentation,
};
use crate::workflow::layout::{layout, DagLayout, EdgeBits, LayoutRect};
use crate::workflow::model::{NodeStatus, RunGraph, RunNode, RunNodeIdx, RunStatus, Succession};

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
    // The run's last growth limit, formatted for the banner band. `None` costs
    // zero rows (§3 frozen interface 10). Mirrored onto
    // `WorkflowRunPresentation` alongside the run graph, the same way a refused
    // delivery is — and gated on there being a graph to banner, so a limit left
    // over from a run that has since been cleared cannot head an empty overlay.
    let banner: Option<String> = app
        .workflow_run_graph()
        .and(app.workflow_run_presentation().growth_banner.clone());
    let (header_rect, banner_rect, graph_rect, detail_rect, footer_rect) =
        overlay_areas(area, banner.as_deref());
    let mut view = DagViewState {
        header_rect,
        banner_rect,
        graph_rect,
        detail_rect,
        footer_rect,
        banner,
        ..DagViewState::default()
    };

    let Some(graph) = app.workflow_run_graph() else {
        return view;
    };
    let presentation = app.workflow_run_presentation();

    view.run_id = graph.run_id.as_str().to_string();
    view.workflow_name = presentation.workflow_name.clone();
    view.run_status = Some(graph.status);
    // Counted over the whole graph, before clipping: a resize must never
    // change the answer the header gives about the run.
    view.counts = run_counts(graph);
    view.layout = clipped_layout(graph, graph_rect);
    let now_unix_ms = current_unix_ms();
    view.nodes = graph
        .nodes
        .iter()
        .filter(|node| view.layout.rect_of(node.idx).is_some())
        .map(|node| project_node(graph, node, presentation, now_unix_ms))
        .collect();
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
        label: node_labels
            .get(node.key.as_str())
            .map(|label| label.trim())
            .filter(|label| !label.is_empty())
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
        successors,
        predecessors,
    }
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

    // Nothing was drawn: say so once, on the whole overlay, instead of a header
    // and a navigation footer arguing with an empty body.
    if dag.is_empty() {
        render_single_message(dag, p, frame, area);
        return;
    }

    render_header(dag, p, frame);
    render_banner(dag, p, frame);
    render_edges(dag, p, frame);
    render_nodes(dag, p, frame);
    render_detail(dag, p, frame);
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
fn render_header(dag: &DagViewState, p: &Palette, frame: &mut Frame) {
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
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(Span::styled(
                format!(" {} ", truncate_end(&node.label, label_width)),
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

fn render_detail(dag: &DagViewState, p: &Palette, frame: &mut Frame) {
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
    lines.push(Line::from(truncate_spans(
        vec![
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
        ],
        width,
    )));
    lines.push(Line::from(truncate_spans(
        vec![
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
        ],
        width,
    )));
    lines.push(match &node.summary {
        Some(summary) => Line::from(Span::styled(
            format!(" {}", truncate_end(summary, width.saturating_sub(1))),
            Style::default().fg(p.subtext0),
        )),
        None => Line::from(Span::styled(" no checkpoint yet", dim)),
    });
    frame.render_widget(Paragraph::new(lines), dag.detail_rect);
}

/// Every hint the overlay offers, in display order.
const FOOTER_HINTS: [(&str, &str); 4] = [
    ("enter", " focus"),
    ("hjkl/↑↓←→", " move"),
    ("s", " steer"),
    ("esc", " close"),
];

/// Which hints fit in `width`, in display order.
///
/// A hint that does not fit is dropped whole rather than sliced mid-word, and
/// they go least-useful first: `esc close` is the last thing to leave, because
/// it is the only way out of a full-bleed overlay.
fn footer_hints(width: usize) -> Vec<(&'static str, &'static str)> {
    /// Indices into [`FOOTER_HINTS`], least useful first.
    const DROP_ORDER: [usize; 4] = [2, 1, 0, 3];

    let mut keep = [true; FOOTER_HINTS.len()];
    for index in DROP_ORDER {
        if footer_hints_width(&keep) <= width {
            break;
        }
        keep[index] = false;
    }
    // Even the last hint can be wider than the band; a partial word is worse
    // than nothing, so the row goes empty instead.
    if footer_hints_width(&keep) > width {
        return Vec::new();
    }
    FOOTER_HINTS
        .iter()
        .zip(keep)
        .filter(|(_, kept)| *kept)
        .map(|(hint, _)| *hint)
        .collect()
}

fn footer_hints_width(keep: &[bool; FOOTER_HINTS.len()]) -> usize {
    let mut used = 0usize;
    let mut first = true;
    for (hint, kept) in FOOTER_HINTS.iter().zip(keep) {
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
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (chord, label) in footer_hints(width) {
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
        let full = footer_hints(200);
        assert_eq!(full.len(), FOOTER_HINTS.len());

        let mut previous = full.len();
        for width in (0..=60).rev() {
            let hints = footer_hints(width);
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
