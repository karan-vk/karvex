//! The workflow review overlay — forked-session interviews and findings
//! over a terminal run
//! (`.local/prd/phase4-retarget-plan.md` §3.5, packet P2).
//!
//! Landed as an inert stub: packet P2's whole job is to touch every
//! exhaustive `Mode`-match file exactly once — `ui.rs` above all — so the
//! parallel review packets (P10 builds the wire-backed cycle, P13 builds the
//! findings list and accept flow) never fight match-arm churn again.
//! `ui.rs` already dispatches into [`compute_workflow_review_view`] and
//! [`render_workflow_review`] and never needs to change for review again.
//!
//! Split the same way the DAG/launcher/run-browser overlays are: the
//! compute pass (mutates [`AppState`]) stores every rectangle the renderer
//! will draw; the renderer ([`render_workflow_review`]) only reads them
//! back, so the hit-test can never disagree with what was drawn
//! (`AGENTS.md`'s render-purity rule: `compute_view()` mutates, `render()`
//! only draws).
//!
//! There is no `workflow.review.*` wire method yet — that lands with P3/P10
//! — so this renders an honest "review isn't wired up yet" placeholder
//! rather than guessing at a domain shape nobody has designed. Closing is
//! the one behaviour every overlay in this family already has: `Esc`, or a
//! click outside the modal, leaves through the same `leave_modal` path as
//! the DAG/launcher/run-browser overlays.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};

use super::widgets::{
    centered_popup_rect, modal_stack_areas, render_modal_header, render_panel_shell,
};
use crate::app::state::{AppState, Mode, WorkflowReviewState};

pub(crate) const REVIEW_MODAL_WIDTH: u16 = 56;
pub(crate) const REVIEW_MODAL_HEIGHT: u16 = 9;

const HEADER_HEIGHT: u16 = 1;
const FOOTER_HEIGHT: u16 = 1;
const STACK_GAP: u16 = 1;

// ── view computation (mutation pass) ────────────────────────────────────────

/// Refreshes the review overlay's geometry for this frame. `carried` is the
/// previous frame's state; `ViewState` is rebuilt wholesale every pass, so
/// it is carried rather than recomputed — same rule as
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
    view.modal_rect =
        centered_popup_rect(area, REVIEW_MODAL_WIDTH, REVIEW_MODAL_HEIGHT).unwrap_or_default();
    view
}

// ── rendering (draw only) ───────────────────────────────────────────────────

pub(super) fn render_workflow_review(app: &AppState, frame: &mut Frame, area: Rect) {
    let popup = app.view.workflow_review.modal_rect;
    if popup.width == 0 || popup.height == 0 {
        return;
    }

    let p = &app.palette;
    super::dim_background(frame, area);
    let Some(inner) = render_panel_shell(frame, popup, p.accent, p.panel_bg) else {
        return;
    };
    let stack = modal_stack_areas(inner, HEADER_HEIGHT, FOOTER_HEIGHT, 0, STACK_GAP);

    render_modal_header(frame, stack.header, " workflow review", p);

    if stack.content.height > 0 {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " no review available yet",
                Style::default().fg(p.overlay1),
            )))
            .wrap(Wrap { trim: false }),
            stack.content,
        );
    }

    if let Some(footer) = stack.footer {
        frame.render_widget(
            Paragraph::new(Span::styled(" esc  close", Style::default().fg(p.overlay0))),
            footer,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppState;

    fn app_with_review_open() -> AppState {
        let mut app = AppState::test_new();
        app.mode = Mode::WorkflowReview;
        app
    }

    #[test]
    fn compute_view_is_inert_outside_the_mode() {
        let app = AppState::test_new();
        assert_ne!(app.mode, Mode::WorkflowReview);
        let view = compute_workflow_review_view(
            &app,
            Rect::new(0, 0, 120, 40),
            WorkflowReviewState {
                modal_rect: Rect::new(1, 1, 1, 1),
            },
        );
        assert_eq!(
            view,
            WorkflowReviewState::default(),
            "stale geometry from a previous frame must never survive a mode change"
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

    #[test]
    fn render_does_not_panic_when_open_or_closed() {
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        let mut app = app_with_review_open();
        app.view.workflow_review = compute_workflow_review_view(
            &app,
            Rect::new(0, 0, 120, 40),
            WorkflowReviewState::default(),
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
}
