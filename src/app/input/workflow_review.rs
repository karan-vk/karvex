//! Input handling for the workflow review overlay
//! (`.local/prd/phase4-retarget-plan.md` §3.5, packet P2).
//!
//! Landed as an inert stub alongside the render half in `src/ui/workflow_review.rs`:
//! this packet's whole job is to touch every exhaustive `Mode`-match file
//! exactly once, so the packets that give review its behaviour (P10 wires
//! `workflow.review.*`, P13 builds the findings list and accept flow) never
//! have to fight match-arm churn across the tree again. The only live
//! behaviour here is closing — `Esc`, or a click outside the modal — through
//! the same `leave_modal` path every other overlay in this family uses, so
//! the stub can never become an input trap.

use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};

use super::modal::{leave_modal, modal_action_from_key, ModalAction, WORKFLOW_REVIEW_ACTIONS};
use crate::app::state::AppState;
use crate::app::App;

/// Inserts typed or pasted text into whatever text field the review overlay
/// ends up needing. No-op: the stub has no focused text field, and neither
/// does the plan's eventual findings list (accept/decline is per-row, not
/// text entry) — mirrors
/// [`super::workflow_runs::insert_workflow_runs_text`].
pub(crate) fn insert_workflow_review_text(_state: &mut AppState, _text: &str) -> bool {
    false
}

fn rect_contains(rect: ratatui::layout::Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

impl App {
    pub(crate) fn handle_workflow_review_key(&mut self, key: KeyEvent) {
        if let Some(ModalAction::Close) = modal_action_from_key(&key, WORKFLOW_REVIEW_ACTIONS) {
            leave_modal(&mut self.state);
        }
    }

    /// Mouse handling for the review overlay: a click outside the modal
    /// closes it, exactly like the run browser and every other stacked
    /// overlay — the hit-test reads the rect the view-computation pass
    /// stored, so there is no second geometry to keep in sync.
    pub(super) fn handle_workflow_review_mouse(&mut self, mouse: MouseEvent) -> bool {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            let modal_rect = self.state.view.workflow_review.modal_rect;
            if !rect_contains(modal_rect, mouse.column, mouse.row) {
                leave_modal(&mut self.state);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;
    use crate::app::state::Mode;
    use crate::app::App;

    fn test_app() -> App {
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        )
    }

    fn app_with_review_open() -> App {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowReview;
        app
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn esc_leaves_the_stub_through_the_shared_modal_exit() {
        let mut app = app_with_review_open();
        app.handle_workflow_review_key(key(KeyCode::Esc));
        assert_ne!(
            app.state.mode,
            Mode::WorkflowReview,
            "esc must never trap input in the stub"
        );
    }

    #[test]
    fn other_keys_do_not_close_the_stub() {
        let mut app = app_with_review_open();
        app.handle_workflow_review_key(key(KeyCode::Char('x')));
        assert_eq!(app.state.mode, Mode::WorkflowReview);
    }

    #[test]
    fn a_click_inside_the_modal_stays_open() {
        let mut app = app_with_review_open();
        app.state.view.workflow_review.modal_rect = ratatui::layout::Rect::new(10, 10, 20, 10);
        let handled = app.handle_workflow_review_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 15,
            row: 15,
            modifiers: KeyModifiers::NONE,
        });
        assert!(handled);
        assert_eq!(app.state.mode, Mode::WorkflowReview);
    }

    #[test]
    fn a_click_outside_the_modal_closes_it() {
        let mut app = app_with_review_open();
        app.state.view.workflow_review.modal_rect = ratatui::layout::Rect::new(10, 10, 20, 10);
        let handled = app.handle_workflow_review_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert!(handled);
        assert_ne!(app.state.mode, Mode::WorkflowReview);
    }

    #[test]
    fn paste_is_a_no_op() {
        let mut state = AppState::test_new();
        assert!(!insert_workflow_review_text(&mut state, "hello"));
    }
}
