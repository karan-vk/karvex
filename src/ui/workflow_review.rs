//! The workflow review overlay — findings over a terminal run, and the
//! human's per-finding accept/decline
//! (`.local/prd/phase4-retarget-plan.md` §3.5, packets P2 and P13).
//!
//! P2 landed this as an inert stub: no `workflow.review.*` wire method
//! existed yet, so `ui.rs` was wired to dispatch into
//! [`compute_workflow_review_view`]/[`render_workflow_review`] once, ahead of
//! the packets that would give it real content, so those packets never have
//! to fight `Mode`-match churn across the tree. P3/P10/P11 landed the wire
//! surface, the orchestration, and apply; this is P13's real content.
//!
//! Split the same way the DAG/launcher/run-browser overlays are: the compute
//! pass (mutates [`AppState`]) stores every rectangle the renderer will
//! draw; the renderer only reads them back, so the hit-test can never
//! disagree with what was drawn (`AGENTS.md`'s render-purity rule).
//! Shaped like [`super::workflow_runs`] on purpose — list, detail, footer,
//! a two-step confirm — because karvex is a mouse-first TUI and consistency
//! across overlays is a stated rule, not a per-packet choice.

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use super::text::truncate_end;
use super::widgets::{centered_popup_rect, modal_stack_areas, render_panel_shell};
use crate::api::schema::{WorkflowReviewInterviewMode, WorkflowReviewStatus};
use crate::app::state::{
    AppState, Mode, Palette, WorkflowReviewConfirm, WorkflowReviewFindingRow, WorkflowReviewState,
};

pub(crate) const REVIEW_MODAL_WIDTH: u16 = 96;
pub(crate) const REVIEW_MODAL_HEIGHT: u16 = 26;

const HEADER_HEIGHT: u16 = 1;
const FOOTER_HEIGHT: u16 = 1;
const STACK_GAP: u16 = 1;
/// Detail strip rows: rationale, attribution, evidence, proposed change —
/// one fact per row, the same "no per-row content changes the budget" rule
/// the DAG detail strip follows.
const DETAIL_HEIGHT: u16 = 5;

// ── view computation (mutation pass) ────────────────────────────────────────

/// Refreshes the review overlay's geometry for this frame. `carried` is the
/// previous frame's state (the loaded cycle, selection, scroll, the open
/// confirm); `ViewState` is rebuilt wholesale every pass, so it is carried
/// rather than recomputed — same rule as
/// [`super::workflow_runs::compute_workflow_runs_view`].
///
/// Drops stale geometry the moment the mode goes inactive, exactly like
/// every sibling overlay: a closed modal must never leave a clickable rect
/// behind for whatever mode comes next.
pub(super) fn compute_workflow_review_view(
    app: &AppState,
    area: Rect,
    carried: WorkflowReviewState,
) -> WorkflowReviewState {
    if app.mode != Mode::WorkflowReview {
        return WorkflowReviewState::default();
    }
    let mut view = carried;
    apply_review_geometry(&mut view, area);
    view
}

/// Stores the rects the renderer draws and the hit-test reads. Pure: the
/// only inputs are the area and the state already on the view.
pub(crate) fn apply_review_geometry(view: &mut WorkflowReviewState, area: Rect) {
    view.modal_rect = Rect::default();
    view.list_rect = Rect::default();
    view.detail_rect = Rect::default();
    view.footer_rect = Rect::default();
    view.row_rects = vec![Rect::default(); view.findings.len()];

    let Some(popup) = centered_popup_rect(area, REVIEW_MODAL_WIDTH, REVIEW_MODAL_HEIGHT) else {
        return;
    };
    view.modal_rect = popup;
    let Some(sections) = review_sections(popup, view.findings.len(), view.selected) else {
        return;
    };
    view.list_rect = sections.list;
    view.detail_rect = sections.detail;
    view.footer_rect = sections.footer;
    view.row_rects = sections.rows;
}

struct ReviewSections {
    header: Rect,
    list: Rect,
    rows: Vec<Rect>,
    detail: Rect,
    footer: Rect,
}

/// Splits the modal into its sections — a private copy of
/// [`super::workflow_runs::runs_sections`]'s shape rather than a shared
/// dependency: the two overlays' rows describe different domains, and
/// duplicating ~30 lines of layout arithmetic is cheaper than coupling them
/// through a shared helper neither owns.
///
/// A short modal yields in this order: the detail strip shrinks first (down
/// to nothing), then the list — a list you cannot read is worse than a list
/// with no detail under it.
fn review_sections(popup: Rect, entry_count: usize, selected: usize) -> Option<ReviewSections> {
    if popup.width < 4 || popup.height < 6 {
        return None;
    }
    let inner = Rect::new(
        popup.x.saturating_add(1),
        popup.y.saturating_add(1),
        popup.width.saturating_sub(2),
        popup.height.saturating_sub(2),
    );
    let stack = modal_stack_areas(inner, HEADER_HEIGHT, FOOTER_HEIGHT, 0, STACK_GAP);
    let content = stack.content;
    if content.height == 0 {
        return None;
    }

    let detail_height = DETAIL_HEIGHT.min(content.height.saturating_sub(1));
    let list = Rect::new(
        content.x,
        content.y,
        content.width,
        content.height.saturating_sub(detail_height),
    );
    let detail = Rect::new(content.x, list.bottom(), content.width, detail_height);

    let rows = windowed_rows(
        list,
        entry_count,
        list_window_offset(entry_count, selected, list.height),
    );

    Some(ReviewSections {
        header: stack.header,
        list,
        rows,
        detail,
        footer: stack.footer.unwrap_or_default(),
    })
}

fn list_window_offset(count: usize, selected: usize, height: u16) -> usize {
    let height = height as usize;
    if height == 0 || count == 0 {
        return 0;
    }
    let selected = selected.min(count.saturating_sub(1));
    if selected >= height {
        selected + 1 - height
    } else {
        0
    }
}

fn windowed_rows(area: Rect, count: usize, offset: usize) -> Vec<Rect> {
    let mut rects = vec![Rect::default(); count];
    if area.height == 0 {
        return rects;
    }
    for row in 0..area.height {
        let Some(index) = offset.checked_add(row as usize) else {
            break;
        };
        let Some(slot) = rects.get_mut(index) else {
            break;
        };
        *slot = Rect::new(area.x, area.y.saturating_add(row), area.width, 1);
    }
    rects
}

// ── vocabulary ───────────────────────────────────────────────────────────

fn status_label(status: WorkflowReviewStatus) -> &'static str {
    match status {
        WorkflowReviewStatus::Running => "running",
        WorkflowReviewStatus::AwaitingUser => "awaiting your decision",
        WorkflowReviewStatus::Applied => "applied",
        WorkflowReviewStatus::Declined => "declined",
        WorkflowReviewStatus::Failed => "failed",
    }
}

/// The short list-row tag for a finding's attribution — visible without
/// opening the detail strip, so the resumed/evidence-only distinction is
/// never one click away from being missed
/// (`.local/prd/phase4-retarget-plan.md`: attribution must be visible).
fn interview_mode_tag(mode: WorkflowReviewInterviewMode) -> &'static str {
    match mode {
        WorkflowReviewInterviewMode::Resumed => "resumed",
        WorkflowReviewInterviewMode::EvidenceOnly => "evidence-only",
    }
}

fn level_label(level: crate::api::schema::WorkflowReviewFindingLevel) -> &'static str {
    match level {
        crate::api::schema::WorkflowReviewFindingLevel::Prompt => "prompt",
        crate::api::schema::WorkflowReviewFindingLevel::Structural => "structural",
    }
}

fn verdict_label(verdict: crate::api::schema::WorkflowReviewVerdict) -> &'static str {
    match verdict {
        crate::api::schema::WorkflowReviewVerdict::Keep => "keep",
        crate::api::schema::WorkflowReviewVerdict::Improve => "improve",
        crate::api::schema::WorkflowReviewVerdict::Replace => "replace",
    }
}

fn interview_mode_color(mode: WorkflowReviewInterviewMode, p: &Palette) -> Color {
    match mode {
        WorkflowReviewInterviewMode::Resumed => p.teal,
        WorkflowReviewInterviewMode::EvidenceOnly => p.peach,
    }
}

// ── rendering (draw only) ───────────────────────────────────────────────────

pub(super) fn render_workflow_review(app: &AppState, frame: &mut Frame, area: Rect) {
    let review = &app.view.workflow_review;
    let popup = review.modal_rect;
    if popup.width == 0 || popup.height == 0 {
        return;
    }
    let Some(sections) = review_sections(popup, review.findings.len(), review.selected) else {
        return;
    };

    let p = &app.palette;
    super::dim_background(frame, area);
    if render_panel_shell(frame, popup, p.accent, p.panel_bg).is_none() {
        return;
    }

    render_header(review, frame, sections.header, p);

    if review.findings.is_empty() {
        render_message(review, frame, sections.list, p);
    } else {
        render_rows(review, frame, &sections, p);
        render_detail(review, frame, sections.detail, p);
    }
    render_footer(review, frame, sections.footer, p);

    if let Some(confirm) = review.confirm {
        render_confirm(review, confirm, frame, popup, p);
    }
}

fn render_header(review: &WorkflowReviewState, frame: &mut Frame, area: Rect, p: &Palette) {
    let mut spans = vec![Span::styled(
        " workflow review",
        Style::default().fg(p.text).add_modifier(Modifier::BOLD),
    )];
    if let Some(status) = review.status {
        spans.push(Span::styled(" · ", Style::default().fg(p.overlay0)));
        spans.push(Span::styled(
            status_label(status),
            Style::default().fg(p.subtext0),
        ));
    }
    if review.evidence_only_count > 0 {
        spans.push(Span::styled(" · ", Style::default().fg(p.overlay0)));
        spans.push(Span::styled(
            format!("{} evidence-only", review.evidence_only_count),
            Style::default().fg(p.peach),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The honest placeholder for every status but `awaiting_user`: no cycle
/// ever ran, one is still running, or one already closed. P2's stub
/// established this spirit ("no review available yet"); P13 keeps it, now
/// describing the *actual* state instead of a permanent unknown.
fn render_message(review: &WorkflowReviewState, frame: &mut Frame, area: Rect, p: &Palette) {
    if area.height == 0 {
        return;
    }
    let text = review
        .message
        .as_deref()
        .unwrap_or("no review available yet");
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {text}"),
            Style::default().fg(p.overlay1),
        )))
        .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_rows(
    review: &WorkflowReviewState,
    frame: &mut Frame,
    sections: &ReviewSections,
    p: &Palette,
) {
    for (index, (finding, rect)) in review.findings.iter().zip(sections.rows.iter()).enumerate() {
        if rect.height == 0 {
            continue;
        }
        let selected = index == review.selected;
        frame.render_widget(
            Paragraph::new(row_line(finding, rect.width, selected, p)),
            *rect,
        );
    }
}

fn row_line(
    finding: &WorkflowReviewFindingRow,
    width: u16,
    selected: bool,
    p: &Palette,
) -> Line<'static> {
    let marker = if selected { "▶ " } else { "  " };
    let checkbox = if finding.accept { "[x]" } else { "[ ]" };
    let text = format!(
        "{marker}{checkbox} {} · {} · {} · {}",
        finding.node_key,
        level_label(finding.level),
        verdict_label(finding.verdict),
        interview_mode_tag(finding.interview_mode),
    );
    let text = truncate_end(&text, width as usize);
    let fg = if selected {
        p.text
    } else if finding.accept {
        p.green
    } else {
        interview_mode_color(finding.interview_mode, p)
    };
    let mut style = Style::default().fg(fg);
    if selected {
        style = style.bg(p.surface0);
    }
    Line::from(vec![Span::styled(text, style)])
}

fn render_detail(review: &WorkflowReviewState, frame: &mut Frame, area: Rect, p: &Palette) {
    if area.height == 0 {
        return;
    }
    let Some(finding) = review.findings.get(review.selected) else {
        return;
    };
    let dim = Style::default().fg(p.overlay1);
    let text = Style::default().fg(p.text);
    let lines: Vec<Line<'static>> = vec![
        Line::from(vec![
            Span::styled(" rationale: ", dim),
            Span::styled(finding.rationale.clone(), text),
        ]),
        Line::from(vec![
            Span::styled(" attribution: ", dim),
            Span::styled(
                finding.attribution.clone(),
                Style::default().fg(interview_mode_color(finding.interview_mode, p)),
            ),
        ]),
        Line::from(vec![
            Span::styled(" evidence: ", dim),
            Span::styled(finding.evidence_summary.clone(), text),
        ]),
        Line::from(vec![
            Span::styled(" change: ", dim),
            Span::styled(finding.proposed_change_summary.clone(), text),
        ]),
    ];
    for (index, line) in lines.into_iter().take(area.height as usize).enumerate() {
        frame.render_widget(
            Paragraph::new(line).wrap(Wrap { trim: false }),
            Rect::new(area.x, area.y.saturating_add(index as u16), area.width, 1),
        );
    }
}

fn render_footer(review: &WorkflowReviewState, frame: &mut Frame, area: Rect, p: &Palette) {
    if area.height == 0 {
        return;
    }
    let text = if review.findings.is_empty() {
        " esc close"
    } else {
        " space toggle · ↵ apply · d decline all · esc close"
    };
    frame.render_widget(
        Paragraph::new(Span::styled(text, Style::default().fg(p.overlay1))),
        area,
    );
}

fn render_confirm(
    review: &WorkflowReviewState,
    confirm: WorkflowReviewConfirm,
    frame: &mut Frame,
    parent: Rect,
    p: &Palette,
) {
    let Some(popup) = centered_popup_rect(parent, 60.min(parent.width), 6.min(parent.height))
    else {
        return;
    };
    let Some(inner) = render_panel_shell(frame, popup, p.peach, p.panel_bg) else {
        return;
    };
    let accepted_count = review
        .findings
        .iter()
        .filter(|finding| finding.accept)
        .count();
    let (title, detail) = match confirm {
        WorkflowReviewConfirm::Apply if accepted_count == 0 => (
            " apply — nothing accepted".to_string(),
            " every finding stays out; the cycle closes as declined".to_string(),
        ),
        WorkflowReviewConfirm::Apply => (
            format!(" apply {accepted_count} accepted finding(s)?"),
            " everything not accepted is declined; this closes the cycle".to_string(),
        ),
        WorkflowReviewConfirm::DeclineAll => (
            " decline the whole cycle?".to_string(),
            " no finding is applied, regardless of any toggle".to_string(),
        ),
    };
    let lines = vec![
        Line::from(Span::styled(
            title,
            Style::default().fg(p.text).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(detail, Style::default().fg(p.overlay1))),
        Line::from(Span::styled(
            " ↵ confirm · esc cancel",
            Style::default().fg(p.overlay1),
        )),
    ];
    for (index, line) in lines.into_iter().enumerate() {
        if (index as u16) >= inner.height {
            break;
        }
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(
                inner.x,
                inner.y.saturating_add(index as u16),
                inner.width,
                1,
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{WorkflowReviewFindingLevel, WorkflowReviewVerdict};
    use crate::app::AppState;

    fn app_with_review_open() -> AppState {
        let mut app = AppState::test_new();
        app.mode = Mode::WorkflowReview;
        app
    }

    fn finding(node_key: &str) -> WorkflowReviewFindingRow {
        WorkflowReviewFindingRow {
            node_key: node_key.to_string(),
            run_path: None,
            interview_mode: WorkflowReviewInterviewMode::Resumed,
            level: WorkflowReviewFindingLevel::Prompt,
            verdict: WorkflowReviewVerdict::Improve,
            rationale: "the prompt drifted from the role".to_string(),
            attribution: "verify's own account, from a resumed interview".to_string(),
            evidence_summary: "idle_while_in_progress_ms: 900000".to_string(),
            proposed_change_summary: "prompt: tightened the scope".to_string(),
            accept: false,
        }
    }

    // ── mode round-trip ──────────────────────────────────────────────────

    #[test]
    fn compute_view_is_inert_outside_the_mode() {
        let app = AppState::test_new();
        assert_ne!(app.mode, Mode::WorkflowReview);
        let view = compute_workflow_review_view(
            &app,
            Rect::new(0, 0, 120, 40),
            WorkflowReviewState {
                modal_rect: Rect::new(1, 1, 1, 1),
                findings: vec![finding("plan")],
                ..WorkflowReviewState::default()
            },
        );
        assert_eq!(
            view,
            WorkflowReviewState::default(),
            "stale geometry and a stale cycle from a previous frame must never survive a mode \
             change"
        );
    }

    #[test]
    fn compute_view_centers_the_modal_while_open() {
        let app = app_with_review_open();
        let view = compute_workflow_review_view(
            &app,
            Rect::new(0, 0, 120, 40),
            WorkflowReviewState::default(),
        );
        assert_eq!(view.modal_rect.width, REVIEW_MODAL_WIDTH);
        assert_eq!(view.modal_rect.height, REVIEW_MODAL_HEIGHT);
        assert!(view.modal_rect.x > 0 && view.modal_rect.y > 0);
    }

    #[test]
    fn compute_view_degrades_gracefully_on_a_tiny_terminal() {
        let app = app_with_review_open();
        let view = compute_workflow_review_view(
            &app,
            Rect::new(0, 0, 3, 3),
            WorkflowReviewState::default(),
        );
        assert_eq!(
            view.modal_rect,
            Rect::default(),
            "too small to fit a modal is an empty rect, never a panic or a clipped draw"
        );
    }

    // ── geometry partition/degradation ──────────────────────────────────────

    #[test]
    fn the_detail_strip_shrinks_before_the_list_does() {
        let popup = Rect::new(0, 0, REVIEW_MODAL_WIDTH, 9);
        let sections = review_sections(popup, 3, 0).expect("tall enough for a list");
        assert!(
            sections.list.height > 0,
            "the list is never sacrificed first"
        );
    }

    #[test]
    fn too_short_for_any_content_answers_none() {
        assert!(review_sections(Rect::new(0, 0, REVIEW_MODAL_WIDTH, 5), 3, 0).is_none());
    }

    #[test]
    fn row_rects_line_up_one_to_one_with_findings() {
        let mut view = WorkflowReviewState {
            findings: vec![finding("plan"), finding("build"), finding("verify")],
            ..WorkflowReviewState::default()
        };
        apply_review_geometry(&mut view, Rect::new(0, 0, 120, 40));
        assert_eq!(view.row_rects.len(), 3);
        assert!(view.row_rects.iter().all(|rect| rect.height == 1));
    }

    #[test]
    fn stale_geometry_is_dropped_the_moment_the_mode_goes_inactive() {
        let mut app = app_with_review_open();
        app.view.workflow_review = compute_workflow_review_view(
            &app,
            Rect::new(0, 0, 120, 40),
            WorkflowReviewState {
                findings: vec![finding("plan")],
                ..WorkflowReviewState::default()
            },
        );
        assert_ne!(app.view.workflow_review.row_rects.len(), 0);

        app.mode = Mode::Terminal;
        let view = compute_workflow_review_view(
            &app,
            Rect::new(0, 0, 120, 40),
            app.view.workflow_review.clone(),
        );
        assert_eq!(view, WorkflowReviewState::default());
    }

    // ── render ───────────────────────────────────────────────────────────

    #[test]
    fn render_does_not_panic_with_no_cycle_or_a_real_one() {
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        let mut app = app_with_review_open();
        app.view.workflow_review = compute_workflow_review_view(
            &app,
            Rect::new(0, 0, 120, 40),
            WorkflowReviewState {
                message: Some("no review has ever run for this run".to_string()),
                ..WorkflowReviewState::default()
            },
        );
        terminal
            .draw(|frame| render_workflow_review(&app, frame, Rect::new(0, 0, 120, 40)))
            .unwrap();

        app.view.workflow_review = compute_workflow_review_view(
            &app,
            Rect::new(0, 0, 120, 40),
            WorkflowReviewState {
                status: Some(WorkflowReviewStatus::AwaitingUser),
                findings: vec![finding("plan"), finding("build")],
                selected: 1,
                ..WorkflowReviewState::default()
            },
        );
        terminal
            .draw(|frame| render_workflow_review(&app, frame, Rect::new(0, 0, 120, 40)))
            .unwrap();

        app.mode = Mode::Terminal;
        app.view.workflow_review = WorkflowReviewState::default();
        terminal
            .draw(|frame| render_workflow_review(&app, frame, Rect::new(0, 0, 120, 40)))
            .unwrap();
    }

    #[test]
    fn render_does_not_panic_with_an_open_confirm() {
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = app_with_review_open();
        app.view.workflow_review = compute_workflow_review_view(
            &app,
            Rect::new(0, 0, 120, 40),
            WorkflowReviewState {
                status: Some(WorkflowReviewStatus::AwaitingUser),
                findings: vec![finding("plan")],
                confirm: Some(WorkflowReviewConfirm::Apply),
                ..WorkflowReviewState::default()
            },
        );
        terminal
            .draw(|frame| render_workflow_review(&app, frame, Rect::new(0, 0, 120, 40)))
            .unwrap();
    }

    // ── attribution is visible without opening the detail strip ────────────

    #[test]
    fn the_list_row_carries_the_interview_mode_tag() {
        let palette = crate::app::state::Palette::catppuccin();
        let mut resumed = finding("plan");
        resumed.interview_mode = WorkflowReviewInterviewMode::Resumed;
        let line = row_line(&resumed, 80, false, &palette);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("resumed"), "{text}");

        let mut evidence_only = finding("plan");
        evidence_only.interview_mode = WorkflowReviewInterviewMode::EvidenceOnly;
        let line = row_line(&evidence_only, 80, false, &palette);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("evidence-only"), "{text}");
    }
}
