//! Input handling — translates crossterm key/mouse events into state mutations.

use bytes::Bytes;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tracing::warn;

use crate::app::PaneClickState;
use crate::input::TerminalKey;
#[cfg(test)]
use ratatui::layout::Direction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollbarClickTarget {
    Thumb { grab_row_offset: u16 },
    Track { offset_from_bottom: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
enum WheelRouting {
    HostScroll,
    MouseReport,
    AlternateScroll,
}

const WORKSPACE_DRAG_THRESHOLD: u16 = 1;
const TAB_DRAG_THRESHOLD: u16 = 1;

fn modified_url_click_modifier() -> KeyModifiers {
    KeyModifiers::CONTROL
}

#[cfg(test)]
#[test]
fn modified_url_click_modifier_matches_terminal_mouse_reporting() {
    assert_eq!(modified_url_click_modifier(), KeyModifiers::CONTROL);
}

mod clipboard;
mod copy_mode;
mod lease;
mod modal;
mod mouse;
mod navigate;
mod overlays;
mod selection;
mod settings;
mod sidebar;
mod terminal;
mod workflow_launch;
mod workflow_review;
mod workflow_runs;

pub(crate) use self::{
    lease::{ConsumedInputLease, ForwardedInputLease, InputLeaseKey, InputLeaseTable, RepeatPlan},
    modal::{
        handle_global_menu_key, handle_keybind_help_key, handle_navigator_key,
        insert_keybind_help_query_text, insert_navigator_search_text, insert_rename_input_text,
        open_new_workspace_dialog,
    },
    navigate::{
        terminal_direct_indexed_navigation_action, terminal_direct_non_indexed_navigation_action,
    },
    settings::open_settings_at,
};
use self::{
    modal::{
        leave_modal, modal_action_from_key, ModalAction, ONBOARDING_WELCOME_ACTIONS,
        RELEASE_NOTES_ACTIONS, WORKFLOW_DAG_ACTIONS,
    },
    mouse::MouseAction,
    settings::SettingsAction,
};
use super::state::{AppState, Mode};
use super::App;
use crate::api::schema::WorkflowInterrogationMode;
use crate::ui::DagNavDirection;

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

impl App {
    pub(super) async fn handle_key(
        &mut self,
        key: TerminalKey,
    ) -> Option<super::TerminalInputTarget> {
        if self.state.popup_pane.is_some() {
            return self.handle_terminal_key(key).await;
        }
        let key_event = key.as_key_event();
        if modal_paste_target_active(&self.state) && is_modal_paste_shortcut(&key_event) {
            if let Some(text) = crate::platform::read_clipboard_text() {
                self.paste_into_active_text_input(&text);
            }
            return None;
        }

        match self.state.mode {
            Mode::Terminal => return self.handle_terminal_key(key).await,
            Mode::Prefix => self.handle_prefix_key(key),
            Mode::Navigate => self.handle_navigate_key(key),
            Mode::Copy => self.handle_copy_mode_key(key),
            _ => match self.state.mode {
                Mode::Onboarding => self.handle_onboarding_key(key_event),
                Mode::ReleaseNotes => self.handle_release_notes_key(key_event),
                Mode::ProductAnnouncement => self.handle_product_announcement_key(key_event),
                Mode::Prefix | Mode::Navigate | Mode::Copy => unreachable!(),
                Mode::RenameWorkspace | Mode::RenameTab | Mode::RenamePane => {
                    self.handle_rename_key_via_api(key_event)
                }
                Mode::NewLinkedWorktree => self.handle_worktree_create_key(key_event),
                Mode::OpenExistingWorktree => self.handle_worktree_open_key(key_event),
                Mode::ConfirmRemoveWorktree => self.handle_worktree_remove_key(key_event),
                Mode::Resize => self.handle_resize_key_via_api(key),
                Mode::ConfirmClose => self.handle_confirm_close_key_via_api(key_event),
                Mode::ContextMenu => {
                    self.handle_context_menu_key_via_api(key_event);
                }
                Mode::Settings => self.handle_settings_key(key_event),
                Mode::GlobalMenu => handle_global_menu_key(&mut self.state, key_event),
                Mode::KeybindHelp => handle_keybind_help_key(&mut self.state, key),
                Mode::Navigator => {
                    handle_navigator_key(&mut self.state, &self.terminal_runtimes, key_event)
                }
                Mode::WorkflowDag => self.handle_workflow_dag_key(key_event),
                Mode::WorkflowLaunch => self.handle_workflow_launch_key(key_event),
                Mode::WorkflowRuns => self.handle_workflow_runs_key(key_event),
                Mode::WorkflowReview => self.handle_workflow_review_key(key_event),
                Mode::Terminal => unreachable!(),
            },
        }
        None
    }

    pub(crate) fn handle_text_commit_headless(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.state.popup_pane.is_some() {
            if let Some(runtime) = self.popup_runtime() {
                let _ = runtime.try_send_bytes(Bytes::copy_from_slice(text.as_bytes()));
            } else {
                self.close_popup_pane();
            }
            return;
        }
        if self.state.mode != Mode::Terminal {
            self.paste_into_active_text_input(text);
            return;
        }

        self.state.clear_selection();
        self.selection_autoscroll_deadline = None;
        self.state.update_dismissed = true;
        if let Some(ws_idx) = self.state.active {
            if let Some(runtime) = self
                .state
                .focused_runtime_in_workspace(&self.terminal_runtimes, ws_idx)
            {
                let _ = runtime.try_send_bytes(Bytes::copy_from_slice(text.as_bytes()));
            }
        }
    }

    pub(super) async fn handle_text_commit(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if self.state.popup_pane.is_some() {
            if let Some(runtime) = self.popup_runtime() {
                let _ = runtime.send_bytes(Bytes::from(text)).await;
            } else {
                self.close_popup_pane();
            }
            return;
        }
        if self.state.mode != Mode::Terminal {
            self.paste_into_active_text_input(&text);
            return;
        }

        self.state.clear_selection();
        self.selection_autoscroll_deadline = None;
        self.state.update_dismissed = true;
        if let Some(ws_idx) = self.state.active {
            if let Some(runtime) = self
                .state
                .focused_runtime_in_workspace(&self.terminal_runtimes, ws_idx)
            {
                let _ = runtime.send_bytes(Bytes::from(text)).await;
            }
        }
    }

    pub(super) async fn handle_paste(&mut self, text: String) {
        if self.state.popup_pane.is_some() {
            if let Some(runtime) = self.popup_runtime() {
                let _ = runtime.send_paste(text).await;
            } else {
                self.close_popup_pane();
            }
            return;
        }
        if self.state.mode != Mode::Terminal {
            self.paste_into_active_text_input(&text);
            return;
        }

        if let Some(ws_idx) = self.state.active {
            if let Some(rt) = self
                .state
                .focused_runtime_in_workspace(&self.terminal_runtimes, ws_idx)
            {
                let _ = rt.send_paste(text).await;
            }
        }
    }

    pub(crate) fn paste_into_active_text_input(&mut self, text: &str) -> bool {
        match self.state.mode {
            Mode::RenameWorkspace | Mode::RenameTab | Mode::RenamePane => {
                insert_rename_input_text(&mut self.state, text);
                true
            }
            Mode::NewLinkedWorktree => {
                self.insert_worktree_create_text(text);
                true
            }
            Mode::OpenExistingWorktree => {
                if !self
                    .state
                    .worktree_open
                    .as_ref()
                    .is_some_and(|open| open.search_focused)
                {
                    return false;
                }
                self.insert_worktree_open_search_text(text);
                true
            }
            Mode::Navigator => {
                if !self.state.navigator.search_focused {
                    return false;
                }
                insert_navigator_search_text(&mut self.state, &self.terminal_runtimes, text);
                true
            }
            Mode::KeybindHelp => {
                if !self.state.keybind_help.search_focused {
                    return false;
                }
                insert_keybind_help_query_text(&mut self.state, text);
                true
            }
            Mode::WorkflowDag => {
                if self.state.view.dag.steer.is_none() {
                    return false;
                }
                self.insert_workflow_dag_steer_text(text);
                true
            }
            Mode::WorkflowLaunch => {
                workflow_launch::insert_workflow_launch_text(&mut self.state, text)
            }
            Mode::WorkflowRuns => workflow_runs::insert_workflow_runs_text(&mut self.state, text),
            Mode::WorkflowReview => {
                workflow_review::insert_workflow_review_text(&mut self.state, text)
            }
            Mode::Copy => {
                let Some(prompt) = self
                    .state
                    .copy_mode
                    .as_mut()
                    .and_then(|copy_mode| copy_mode.search.prompt.as_mut())
                else {
                    return false;
                };
                prompt
                    .query
                    .extend(text.chars().filter(|ch| !ch.is_control()));
                true
            }
            _ => false,
        }
    }

    /// Key handling for the live workflow DAG overlay
    /// (`docs/design/workflow-builder/05-phase-plan.md` W6). Navigation is
    /// graph-aware and runs off the geometry stored by the view-computation
    /// pass, so it can never select a node the frame did not draw.
    pub(crate) fn handle_workflow_dag_key(&mut self, key: KeyEvent) {
        if self.state.view.dag.steer.is_some() {
            self.handle_workflow_dag_steer_key(key);
            return;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_workflow_dag_selection(DagNavDirection::Down)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_workflow_dag_selection(DagNavDirection::Up)
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.move_workflow_dag_selection(DagNavDirection::Left)
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.move_workflow_dag_selection(DagNavDirection::Right)
            }
            KeyCode::Enter => self.focus_workflow_dag_node(),
            KeyCode::Char('s') => self.open_workflow_dag_steer(),
            // §3.5's deferred `m message` verb, un-deferred by §3.1a: the run's
            // sessions now identify themselves, so karvex can address the one
            // that owns this node instead of guessing at a channel.
            KeyCode::Char('m') => self.open_workflow_dag_message(),
            // `i` resumes the source session; `Shift+I` reconstructs one from
            // the stored checkpoint. Two keys, not one key twice: escalation
            // into a session that is *not* the original teammate is always an
            // explicit choice (`00-overview.md` Feature 3).
            KeyCode::Char('i') => {
                self.interrogate_workflow_dag_node(WorkflowInterrogationMode::Resumed)
            }
            KeyCode::Char('I') => {
                self.interrogate_workflow_dag_node(WorkflowInterrogationMode::Reconstructed)
            }
            _ => {
                if let Some(ModalAction::Close) = modal_action_from_key(&key, WORKFLOW_DAG_ACTIONS)
                {
                    self.leave_workflow_dag();
                }
            }
        }
    }

    fn handle_workflow_dag_steer_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.state.view.dag.steer = None,
            KeyCode::Enter => self.submit_workflow_dag_steer(),
            KeyCode::Backspace => {
                if let Some(text) = self.state.view.dag.steer.as_mut() {
                    text.pop();
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(text) = self.state.view.dag.steer.as_mut() {
                    text.clear();
                }
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_workflow_dag_steer_text(&character.to_string());
            }
            _ => {}
        }
    }

    pub(crate) fn insert_workflow_dag_steer_text(&mut self, text: &str) {
        if let Some(steer) = self.state.view.dag.steer.as_mut() {
            steer.extend(text.chars().filter(|character| !character.is_control()));
        }
    }

    fn move_workflow_dag_selection(&mut self, direction: DagNavDirection) {
        if let Some(idx) = crate::ui::workflow_dag_neighbour(&self.state.view.dag, direction) {
            self.state.view.dag.selected = Some(idx);
        }
    }

    fn open_workflow_dag_steer(&mut self) {
        if self.state.view.dag.run_id.is_empty() {
            return;
        }
        if self.refuse_engine_verb_on_a_lead_run("steer") {
            return;
        }
        self.state.view.dag.input_kind = crate::app::state::DagInputKind::NodeSteer;
        // A past run is not the active run, so the server would answer the
        // not-the-active-run guard (`workflow_run_not_active`) — *not*
        // `workflow_run_closed`, which covers only the just-finished run that
        // is still active (`07-phase3-plan.md` §1 WS-H). Saying so here spends
        // no round trip and gets the reason right; opening a line that can
        // only ever be refused is the same swallowed-text failure the
        // no-pane check below exists to prevent.
        if self.state.view.dag.historical {
            self.show_workflow_notice(crate::workflow::model::UserNotice {
                level: crate::workflow::model::NoticeLevel::Warning,
                run: None,
                path: None,
                message: crate::app::workflow_history::historical_steer_refusal(),
            });
            return;
        }
        let Some(node) = self.state.view.dag.selected_node() else {
            return;
        };
        // A node with no pane has nothing to steer. Letting the line open, take
        // the text, and swallow it is the failure mode 2.15 describes; saying so
        // up front costs one line.
        if node.pane_id.is_none() {
            let path = node.path.clone();
            self.show_workflow_notice(crate::workflow::model::UserNotice {
                level: crate::workflow::model::NoticeLevel::Warning,
                run: None,
                path: Some(crate::workflow::model::InstancePath::new(path)),
                message: "the node has no pane to steer".to_string(),
            });
            return;
        }
        self.state.view.dag.steer = Some(String::new());
    }

    /// Opens the composer for `m`: a message to the Claude Code session that
    /// owns the selected node (`09-agent-teams-rework.md` §3.5a).
    ///
    /// Only for a lead run. karvex's own engine no longer executes anything, so
    /// on an engine-era run there is no Claude Code session to address and the
    /// key does nothing rather than opening a line that could only be refused.
    fn open_workflow_dag_message(&mut self) {
        if !self.state.view.dag.lead_run || self.state.view.dag.run_id.is_empty() {
            return;
        }
        if self.state.view.dag.historical {
            self.show_workflow_notice(crate::workflow::model::UserNotice {
                level: crate::workflow::model::NoticeLevel::Warning,
                run: None,
                path: None,
                message: "a past run's Claude Code sessions have exited, so there is nothing \
                          left to message"
                    .to_string(),
            });
            return;
        }
        if self.state.view.dag.selected_node().is_none() {
            return;
        }
        self.state.view.dag.input_kind = crate::app::state::DagInputKind::RunMessage;
        self.state.view.dag.steer = Some(String::new());
    }

    /// Sends the composed text to the session that owns the selected node.
    ///
    /// An unclaimed node has no owner, so the message goes to the lead — which
    /// is the right answer rather than a fallback: the lead is who would assign
    /// that task anyway.
    fn submit_workflow_dag_message(&mut self) {
        let Some(text) = self.state.view.dag.steer.take() else {
            return;
        };
        let text = text.trim().to_string();
        let run_id = self.state.view.dag.run_id.clone();
        let Some(node) = self.state.view.dag.selected_node() else {
            return;
        };
        let path = node.path.clone();
        let target = if node.owner.trim().is_empty() {
            crate::workflow::binding::lead::LEAD_TARGET_NAME.to_string()
        } else {
            node.owner.clone()
        };
        if text.is_empty() || run_id.is_empty() {
            return;
        }
        let response = self.dispatch_runtime_mutation(
            "tui.workflow.run.message",
            crate::api::schema::Method::WorkflowRunMessage(
                crate::api::schema::WorkflowRunMessageParams {
                    run_id,
                    target: target.clone(),
                    text,
                    priority: None,
                },
            ),
        );
        // The same discipline the steer path uses, for the same reason: a write
        // action that can fail must never fail in silence.
        let notice = match serde_json::from_str::<crate::api::schema::ErrorResponse>(&response) {
            Ok(error) => Some((
                crate::workflow::model::NoticeLevel::Error,
                format!("message not sent: {}", error.error.message),
            )),
            Err(_) => Some((
                crate::workflow::model::NoticeLevel::Info,
                format!("message handed to {target}"),
            )),
        };
        if let Some((level, message)) = notice {
            self.show_workflow_notice(crate::workflow::model::UserNotice {
                level,
                run: None,
                path: Some(crate::workflow::model::InstancePath::new(path)),
                message,
            });
        }
    }

    fn submit_workflow_dag_steer(&mut self) {
        if matches!(
            self.state.view.dag.input_kind,
            crate::app::state::DagInputKind::RunMessage
        ) {
            self.submit_workflow_dag_message();
            return;
        }
        let Some(text) = self.state.view.dag.steer.take() else {
            return;
        };
        let text = text.trim().to_string();
        let run_id = self.state.view.dag.run_id.clone();
        let Some(path) = self
            .state
            .view
            .dag
            .selected_node()
            .map(|node| node.path.clone())
        else {
            return;
        };
        if text.is_empty() || run_id.is_empty() {
            return;
        }
        let response = self.dispatch_runtime_mutation(
            "tui.workflow.node.steer",
            crate::api::schema::Method::WorkflowNodeSteer(
                crate::api::schema::WorkflowNodeSteerParams {
                    run_id: run_id.clone(),
                    path: path.clone(),
                    text,
                },
            ),
        );
        // The API answers with the same envelope a CLI caller gets, and it
        // already refuses a steer whose keystrokes never reached the process.
        // Dropping that envelope is what made the overlay's steer the one write
        // action in karvex that could fail in silence.
        if let Some(message) = crate::app::workflow::steer_failure_message(&response) {
            self.show_workflow_notice(crate::workflow::model::UserNotice {
                level: crate::workflow::model::NoticeLevel::Error,
                run: None,
                path: Some(crate::workflow::model::InstancePath::new(path)),
                message,
            });
        }
    }

    /// Interrogates the selected node: `workflow.node.interrogate` in-process,
    /// in the mode the pressed key chose (`07-phase3-plan.md` §1 WS-H, as
    /// amended: `i` resumes, `Shift+I` reconstructs).
    ///
    /// The decision and the answer-classification are pure functions in
    /// `app/workflow_history.rs`; this is only the dispatch and the notice.
    fn interrogate_workflow_dag_node(&mut self, mode: WorkflowInterrogationMode) {
        use crate::app::workflow_history::{
            interrogate_intent, interrogate_outcome, InterrogateIntent, InterrogateOutcome,
        };

        // Both `i` and `Shift+I` are engine inputs: they fork or reconstruct a
        // session karvex's own engine owned. A lead run has neither, so the
        // refusal comes before the intent is even computed.
        let verb = match mode {
            WorkflowInterrogationMode::Resumed => "interrogate",
            WorkflowInterrogationMode::Reconstructed => "reconstruct",
        };
        if self.refuse_engine_verb_on_a_lead_run(verb) {
            return;
        }
        let intent = interrogate_intent(&self.state.view.dag, mode);
        let InterrogateIntent::Send { run_id, path, mode } = intent else {
            return;
        };
        let response = self.dispatch_runtime_mutation(
            "tui.workflow.node.interrogate",
            crate::api::schema::Method::WorkflowNodeInterrogate(
                crate::api::schema::WorkflowNodeInterrogateParams {
                    run_id,
                    path: path.clone(),
                    mode,
                    note: None,
                },
            ),
        );
        // The pane is its own feedback on success; the two refusal shapes
        // differ only in whether there is a next step to name.
        let (level, message) = match interrogate_outcome(&path, mode, &response) {
            InterrogateOutcome::Opened => return,
            InterrogateOutcome::OfferReconstructed(message) => {
                (crate::workflow::model::NoticeLevel::Warning, message)
            }
            InterrogateOutcome::Refused(message) => {
                (crate::workflow::model::NoticeLevel::Error, message)
            }
        };
        self.show_workflow_notice(crate::workflow::model::UserNotice {
            level,
            run: None,
            path: Some(crate::workflow::model::InstancePath::new(path)),
            message,
        });
    }

    /// The DAG overlay's single exit.
    ///
    /// The historical snapshot dies with it, and is cleared **here** rather
    /// than inside the generic `leave_modal`: the overlay prefers a `Some`
    /// snapshot over the live run, so one left behind hijacks the next DAG
    /// open and shows a past run to a user who asked for the live one — and
    /// pushing DAG semantics into the shared leave-modal path would leak them
    /// into every other mode.
    fn leave_workflow_dag(&mut self) {
        self.close_historical_run();
        leave_modal(&mut self.state);
    }

    /// Opens an interrogation box's forked-session pane. The overlay is
    /// full-bleed, so this leaves it.
    pub(super) fn focus_workflow_dag_interrogation(&mut self, pane: &str) {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(pane) else {
            return;
        };
        self.focus_pane_internal_via_api(ws_idx, pane_id);
        self.leave_workflow_dag();
    }

    /// Refuses one of the engine's verbs on a run a Claude Code team lead
    /// executed, and says what to do instead (§3.5). Returns whether the key
    /// was refused, so each caller can return early.
    ///
    /// These are not merely unavailable, they are meaningless: `steer`
    /// delivers to a node the engine bound, and `interrogate`/`reconstruct`
    /// fork a session or rebuild one from a checkpoint the engine wrote. A
    /// lead run has no such bindings and no such checkpoints, so the honest
    /// answer names the affordance that replaced all three — the node's pane,
    /// where the teammate is already listening.
    fn refuse_engine_verb_on_a_lead_run(&mut self, verb: &str) -> bool {
        if !self.state.view.dag.lead_run {
            return false;
        }
        let path = self
            .state
            .view
            .dag
            .selected_node()
            .map(|node| crate::workflow::model::InstancePath::new(node.path.clone()));
        self.show_workflow_notice(crate::workflow::model::UserNotice {
            level: crate::workflow::model::NoticeLevel::Warning,
            run: None,
            path,
            message: format!(
                "{verb} does not apply to a team run — press enter to focus the node's pane and \
                 type there, or m to message the session that owns it"
            ),
        });
        true
    }

    /// The pane `Enter` should open for the selected node.
    ///
    /// For a lead run that is the **owner's** pane: the work happens in the
    /// teammate's session, and the node's own binding is only ever set by
    /// karvex's engine, which did not run this. The binding stays as the
    /// fallback so an engine-era run — and a lead run whose node somehow
    /// carries one — resolves exactly as it always did.
    pub(super) fn workflow_dag_focus_target(&self) -> Option<String> {
        let node = self.state.view.dag.selected_node()?;
        if self.state.view.dag.lead_run {
            return node.owner_pane_id.clone().or_else(|| node.pane_id.clone());
        }
        node.pane_id.clone()
    }

    /// Opens the selected node's teammate. The overlay is full-bleed, so
    /// focusing a pane means leaving the overlay behind it.
    pub(super) fn focus_workflow_dag_node(&mut self) {
        let Some(pane) = self.workflow_dag_focus_target() else {
            // A past run's panes are gone, so this is the common case there
            // rather than a glitch — and a key that does nothing at all is the
            // one thing a read-only view must not do. The footer already drops
            // the `focus` hint for a historical run; this covers the press that
            // happens anyway.
            let path = self
                .state
                .view
                .dag
                .selected_node()
                .map(|node| crate::workflow::model::InstancePath::new(node.path.clone()));
            // A lead run gets its own wording: `i` is refused there, so
            // pointing at it would be advice that cannot be followed. An
            // unclaimed task simply has no pane yet, which is a stage of the
            // run rather than a fault.
            if self.state.view.dag.lead_run {
                self.show_workflow_notice(crate::workflow::model::UserNotice {
                    level: crate::workflow::model::NoticeLevel::Info,
                    run: None,
                    path,
                    message: "this node has no pane yet — no teammate has claimed it".to_string(),
                });
                return;
            }
            if self.state.view.dag.historical {
                self.show_workflow_notice(crate::workflow::model::UserNotice {
                    level: crate::workflow::model::NoticeLevel::Info,
                    run: None,
                    path,
                    message: "this node's pane is gone — press i to interrogate it instead"
                        .to_string(),
                });
            }
            return;
        };
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&pane) else {
            return;
        };
        self.focus_pane_internal_via_api(ws_idx, pane_id);
        self.leave_workflow_dag();
    }

    pub(crate) fn handle_onboarding_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Right | KeyCode::Char('l') => self.open_settings_from_onboarding(),
            _ => {
                if let Some(ModalAction::Continue) =
                    modal_action_from_key(&key, ONBOARDING_WELCOME_ACTIONS)
                {
                    self.open_settings_from_onboarding();
                }
            }
        }
    }

    pub(crate) fn handle_release_notes_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll_release_notes(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_release_notes(1),
            KeyCode::PageUp => self.scroll_release_notes(-8),
            KeyCode::PageDown => self.scroll_release_notes(8),
            KeyCode::Home => {
                if let Some(notes) = &mut self.state.release_notes {
                    notes.scroll = 0;
                }
            }
            KeyCode::End => {
                let max_scroll = self.state.release_notes_max_scroll();
                if let Some(notes) = &mut self.state.release_notes {
                    notes.scroll = max_scroll;
                }
            }
            _ => {
                if let Some(ModalAction::Close) = modal_action_from_key(&key, RELEASE_NOTES_ACTIONS)
                {
                    self.dismiss_release_notes();
                }
            }
        }
    }

    pub(crate) fn handle_product_announcement_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll_product_announcement(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_product_announcement(1),
            KeyCode::PageUp => self.scroll_product_announcement(-8),
            KeyCode::PageDown => self.scroll_product_announcement(8),
            KeyCode::Home => {
                if let Some(announcement) = &mut self.state.product_announcement {
                    announcement.scroll = 0;
                }
            }
            KeyCode::End => {
                let max_scroll = self.state.product_announcement_max_scroll();
                if let Some(announcement) = &mut self.state.product_announcement {
                    announcement.scroll = max_scroll;
                }
            }
            _ => {
                if let Some(ModalAction::Close) = modal_action_from_key(&key, RELEASE_NOTES_ACTIONS)
                {
                    self.dismiss_product_announcement();
                }
            }
        }
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        self.handle_mouse_from_input_source(super::LOCAL_INPUT_SOURCE, mouse);
    }

    pub(super) fn handle_mouse_from_input_source(
        &mut self,
        source_id: super::InputSourceId,
        mouse: MouseEvent,
    ) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.pending_url_click_sources.remove(&source_id);
            }
            MouseEventKind::Drag(MouseButton::Left)
                if self.pending_url_click_sources.contains(&source_id) =>
            {
                return;
            }
            MouseEventKind::Up(MouseButton::Left)
                if self.pending_url_click_sources.remove(&source_id) =>
            {
                return;
            }
            _ => {}
        }

        if self.state.popup_pane.is_some() {
            self.handle_popup_mouse(mouse);
            return;
        }
        if self.handle_overlay_mouse(mouse) {
            return;
        }

        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self.state.on_sidebar_divider(mouse.column, mouse.row)
        {
            let now = std::time::Instant::now();
            let is_double_click = self
                .last_sidebar_divider_click
                .is_some_and(|last| now.duration_since(last) <= super::SIDEBAR_DOUBLE_CLICK_WINDOW);
            self.last_sidebar_divider_click = Some(now);

            if is_double_click {
                self.state.sidebar_width = self.state.default_sidebar_width;
                self.state.sidebar_width_source =
                    crate::app::state::SidebarWidthSource::ConfigDefault;
                self.state.sidebar_width_auto = false;
                self.state.mark_session_dirty();
                self.state.drag = None;
                return;
            }
        }

        if self.handle_modified_url_click(source_id, mouse) {
            return;
        }

        let handled_pane_double_click = self.handle_pane_double_click(mouse);
        if !handled_pane_double_click {
            self.focus_pane_before_mouse_press(mouse);
        }

        let previous_agent_panel_sort = self.state.agent_panel_sort;
        let previous_settings_section = self.state.settings.section;
        if !handled_pane_double_click {
            if let Some(action) = self.state.handle_mouse(&mut self.terminal_runtimes, mouse) {
                match action {
                    MouseAction::NewWorkspace => {
                        self.begin_tui_workspace_create("tui.mouse.workspace.create")
                    }
                    MouseAction::Settings(action) => match action {
                        SettingsAction::SaveTheme(name) => self.save_theme(&name),
                        SettingsAction::SaveStatusIndicators(style) => {
                            self.save_status_indicators(style)
                        }
                        SettingsAction::SaveSound(enabled) => self.save_sound(enabled),
                        SettingsAction::SaveToastDelivery(delivery) => {
                            self.save_toast_delivery(delivery)
                        }
                        SettingsAction::SaveAgentBorderLabels(enabled) => {
                            self.save_agent_border_labels(enabled)
                        }
                        SettingsAction::InstallRecommendedIntegrations => {
                            self.install_recommended_integrations()
                        }
                    },
                    MouseAction::FocusWorkspace { ws_idx } => {
                        self.focus_workspace_idx_via_api(ws_idx)
                    }
                    MouseAction::FocusTab { tab_idx } => self.focus_tab_idx_via_api(tab_idx),
                    MouseAction::FocusPane { ws_idx, pane_id } => {
                        self.focus_pane_internal_via_api(ws_idx, pane_id)
                    }
                    MouseAction::FocusToastTarget => self.focus_toast_target_via_api(),
                    MouseAction::MoveWorkspace {
                        source_ws_idx,
                        insert_idx,
                    } => self.move_workspace_via_api(source_ws_idx, insert_idx),
                    MouseAction::MoveWorkspaceBlock { params } => {
                        self.move_workspace_block_via_api(params)
                    }
                    MouseAction::MoveTab {
                        ws_idx,
                        source_tab_idx,
                        insert_idx,
                    } => self.move_tab_via_api(ws_idx, source_tab_idx, insert_idx),
                    MouseAction::SetSplitRatio { path, ratio } => {
                        self.set_split_ratio_via_api(path, ratio)
                    }
                    MouseAction::RenameModal(action) => {
                        self.apply_rename_mouse_action_via_api(action)
                    }
                    MouseAction::ConfirmCloseAccept => self.confirm_close_accept_via_api(),
                    MouseAction::ContextMenu { menu, idx } => {
                        self.apply_context_menu_action_via_api(menu, idx)
                    }
                }
            }
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
                && self
                    .state
                    .selection
                    .as_ref()
                    .is_none_or(crate::selection::Selection::is_in_progress)
            {
                self.selection_highlight_clear_deadline = None;
            }
        }
        if previous_settings_section != crate::app::state::SettingsSection::Integrations
            && self.state.settings.section == crate::app::state::SettingsSection::Integrations
        {
            self.refresh_integration_recommendations();
        }
        if self.state.agent_panel_sort != previous_agent_panel_sort {
            self.save_agent_panel_sort(self.state.agent_panel_sort);
        }

        self.dispatch_pending_clipboard_write();

        // Sync autoscroll deadline with state (mouse handler may have
        // set or cleared selection_autoscroll during handle_mouse).
        if self.state.selection_autoscroll.is_none() {
            self.selection_autoscroll_deadline = None;
        } else if self.selection_autoscroll_deadline.is_none() {
            self.selection_autoscroll_deadline =
                Some(std::time::Instant::now() + super::SELECTION_AUTOSCROLL_INTERVAL);
        }
    }

    fn handle_popup_mouse(&mut self, mouse: MouseEvent) {
        let Some((_outer, inner)) =
            crate::ui::popup_pane_rects(&self.state, self.state.view.terminal_area)
        else {
            return;
        };
        if mouse.column < inner.x
            || mouse.column >= inner.x.saturating_add(inner.width)
            || mouse.row < inner.y
            || mouse.row >= inner.y.saturating_add(inner.height)
        {
            return;
        }
        let Some(rt) = self.popup_runtime() else {
            self.close_popup_pane();
            return;
        };
        let column = mouse.column.saturating_sub(inner.x);
        let row = mouse.row.saturating_sub(inner.y);
        let bytes = match mouse.kind {
            MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => match rt.wheel_routing() {
                Some(crate::pane::WheelRouting::MouseReport) => {
                    rt.encode_mouse_wheel(mouse.kind, column, row, mouse.modifiers)
                }
                Some(crate::pane::WheelRouting::AlternateScroll) => {
                    rt.encode_alternate_scroll(mouse.kind)
                }
                Some(crate::pane::WheelRouting::HostScroll) | None => {
                    let lines_per_notch = self.state.mouse_scroll_lines;
                    match mouse.kind {
                        MouseEventKind::ScrollUp => rt.scroll_up(lines_per_notch),
                        MouseEventKind::ScrollDown => rt.scroll_down(lines_per_notch),
                        _ => {}
                    }
                    return;
                }
            },
            MouseEventKind::Down(_) | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
                rt.encode_mouse_button(mouse.kind, column, row, mouse.modifiers)
            }
            MouseEventKind::Moved => {
                rt.encode_mouse_motion(mouse.kind, column, row, mouse.modifiers)
            }
        };
        let Some(bytes) = bytes else {
            return;
        };
        if !matches!(mouse.kind, MouseEventKind::Moved) {
            rt.scroll_reset();
        }
        if let Err(err) = rt.try_send_bytes(Bytes::from(bytes)) {
            warn!(err = %err, kind = ?mouse.kind, "failed to forward popup mouse event");
        }
    }

    fn focus_pane_before_mouse_press(&mut self, mouse: MouseEvent) {
        if !matches!(self.state.mode, Mode::Terminal | Mode::Resize)
            || !matches!(
                mouse.kind,
                MouseEventKind::Down(MouseButton::Left | MouseButton::Middle)
            )
        {
            return;
        }

        let Some(pane_id) = self
            .state
            .pane_at(mouse.column, mouse.row)
            .map(|info| info.id)
        else {
            return;
        };
        let Some(ws_idx) = self.state.active else {
            return;
        };

        // Focus through the runtime API before an application can consume its press.
        self.focus_pane_internal_via_api(ws_idx, pane_id);
    }

    fn handle_modified_url_click(
        &mut self,
        source_id: super::InputSourceId,
        mouse: MouseEvent,
    ) -> bool {
        if self.state.mode != Mode::Terminal
            || !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            || !mouse.modifiers.contains(modified_url_click_modifier())
        {
            return false;
        }

        let Some(info) = self.state.pane_at(mouse.column, mouse.row).cloned() else {
            return false;
        };
        let viewport_row = mouse.row.saturating_sub(info.inner_rect.y);
        let col = mouse.column.saturating_sub(info.inner_rect.x);
        let Some(url) =
            self.state
                .url_at_pane_cell(&self.terminal_runtimes, info.id, viewport_row, col)
        else {
            return false;
        };

        self.last_pane_click = None;
        self.pending_url_click_sources.insert(source_id);
        match self.invoke_plugin_link_handler_for_url(&url, info.id) {
            Ok(true) => return true,
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(err = %err, url = %url, "failed to invoke plugin link handler");
            }
        }
        if let Err(err) = crate::platform::open_url(&url) {
            tracing::warn!(err = %err, url = %url, "failed to open pane URL");
        }
        true
    }

    fn handle_pane_double_click(&mut self, mouse: MouseEvent) -> bool {
        // A pane press stops being a double-click candidate once it becomes
        // a drag or completes as a real text selection.
        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                self.last_pane_click = None;
                return false;
            }
            MouseEventKind::Up(MouseButton::Left)
                if self
                    .state
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.is_visible()) =>
            {
                self.last_pane_click = None;
                return false;
            }
            _ => {}
        }

        // Only terminal-pane left-clicks can start this gesture; other clicks
        // should keep their existing mouse behavior and clear stale candidates.
        let Some(click) = self.pane_click_candidate(mouse) else {
            return false;
        };

        // Require the second click to land near the first click in the same pane
        // and within the double-click window so adjacent interactions do not select a word.
        if !self.take_pane_double_click(click) {
            return false;
        }

        self.select_double_clicked_word(click)
    }

    fn pane_click_candidate(&mut self, mouse: MouseEvent) -> Option<PaneClickState> {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return None;
        }

        if !mouse.modifiers.is_empty() {
            self.last_pane_click = None;
            return None;
        }

        if self.state.mode != Mode::Terminal {
            self.last_pane_click = None;
            return None;
        }

        let Some(info) = self.state.pane_at(mouse.column, mouse.row).cloned() else {
            self.last_pane_click = None;
            return None;
        };

        Some(PaneClickState {
            pane_id: info.id,
            viewport_row: mouse.row - info.inner_rect.y,
            col: mouse.column - info.inner_rect.x,
            at: std::time::Instant::now(),
        })
    }

    fn take_pane_double_click(&mut self, click: PaneClickState) -> bool {
        if !self
            .last_pane_click
            .is_some_and(|last| last.is_double_click_for(click))
        {
            self.last_pane_click = Some(click);
            return false;
        }

        self.last_pane_click = None;
        true
    }

    fn select_double_clicked_word(&mut self, click: PaneClickState) -> bool {
        let selected = self.state.select_word_at_pane_cell(
            &self.terminal_runtimes,
            click.pane_id,
            click.viewport_row,
            click.col,
        );
        if selected {
            self.selection_highlight_clear_deadline = self
                .state
                .copy_on_select
                .then(|| std::time::Instant::now() + super::PANE_COPY_HIGHLIGHT_DURATION);
        }
        selected
    }
}

pub(crate) fn is_modal_paste_shortcut(key: &KeyEvent) -> bool {
    if !matches!(key.code, KeyCode::Char('v' | 'V')) {
        return false;
    }

    #[cfg(target_os = "macos")]
    {
        key.modifiers.contains(KeyModifiers::SUPER) || key.modifiers.contains(KeyModifiers::CONTROL)
    }

    #[cfg(not(target_os = "macos"))]
    {
        key.modifiers.contains(KeyModifiers::CONTROL)
    }
}

pub(crate) fn modal_paste_target_active(state: &AppState) -> bool {
    match state.mode {
        Mode::RenameWorkspace | Mode::RenameTab | Mode::RenamePane | Mode::NewLinkedWorktree => {
            true
        }
        Mode::OpenExistingWorktree => state
            .worktree_open
            .as_ref()
            .is_some_and(|open| open.search_focused),
        Mode::Navigator => state.navigator.search_focused,
        Mode::KeybindHelp => state.keybind_help.search_focused,
        Mode::WorkflowDag => state.view.dag.steer.is_some(),
        Mode::WorkflowLaunch => matches!(
            state.view.workflow_launch.focus,
            crate::app::state::WorkflowLaunchFocus::Arg(_)
        ),
        // See the matching arm in `paste_into_active_text_input`.
        Mode::WorkflowRuns => false,
        // Same reason: the stub has no focused text field yet.
        Mode::WorkflowReview => false,
        Mode::Copy => state
            .copy_mode
            .as_ref()
            .is_some_and(|copy_mode| copy_mode.search.prompt.is_some()),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Mouse handling
// ---------------------------------------------------------------------------

// Note: split_pane needs runtime (event_tx for PTY spawn), so it lives on App
impl AppState {
    #[cfg(test)]
    pub(crate) fn split_pane(
        &mut self,
        terminal_runtimes: &mut crate::terminal::TerminalRuntimeRegistry,
        direction: Direction,
    ) {
        // Actual PTY spawning happens in Workspace::split_focused
        // which needs events channel — this is called from navigate_key
        // where we don't have async context, so the workspace handles it
        let (rows, cols) = self.estimate_pane_size();
        let new_rows = (rows / 2).max(4);
        let new_cols = (cols / 2).max(10);

        let follow_cwd = self
            .active
            .and_then(|i| self.workspaces.get(i))
            .and_then(|ws| {
                let tab = ws.active_tab()?;
                let terminal_id = tab.terminal_id(tab.layout.focused())?;
                super::creation::launch_cwd_for_terminal(
                    terminal_id,
                    &self.terminals,
                    terminal_runtimes,
                )
            });
        let cwd = Some(super::creation::resolve_new_terminal_cwd(
            &self.new_terminal_cwd,
            follow_cwd,
        ));

        let previous_focus = self.current_pane_focus_target();
        if let Some(ws_idx) = self.active {
            let Some(ws) = self.workspaces.get_mut(ws_idx) else {
                return;
            };
            if let Ok(new_pane) = ws.split_focused(
                direction,
                new_rows,
                new_cols,
                cwd,
                self.pane_scrollback_limit_bytes,
                self.host_terminal_theme,
                self.host_terminal_appearance,
                crate::pane::PaneShellConfig::new(&self.default_shell, self.shell_mode),
                Vec::new(),
            ) {
                let new_id = new_pane.pane_id;
                terminal_runtimes.insert(new_pane.terminal.id.clone(), new_pane.runtime);
                self.remove_alias_shadowed_by_new_pane(new_id);
                self.terminals
                    .insert(new_pane.terminal.id.clone(), new_pane.terminal);
                self.record_pane_focus_change(previous_focus, ws_idx, new_id);
                self.mark_session_dirty();
                self.mode = Mode::Terminal;
            }
        }
    }
}

#[cfg(test)]
fn state_with_workspaces(names: &[&str]) -> AppState {
    let mut state = AppState::test_new();
    state.workspaces = names
        .iter()
        .map(|name| crate::workspace::Workspace::test_new(name))
        .collect();
    if !state.workspaces.is_empty() {
        state.active = Some(0);
        state.selected = 0;
        state.mode = Mode::Navigate;
    }
    state
}

#[cfg(test)]
fn app_for_mouse_test() -> App {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &crate::config::Config::default(),
        true,
        None,
        api_rx,
        crate::api::EventHub::default(),
    );
    app.state.mode = Mode::Terminal;
    app.state.update_available = None;
    app.state.latest_release_notes_available = false;
    app.state.view.sidebar_rect = ratatui::layout::Rect::new(0, 0, 26, 20);
    app.state.view.terminal_area = ratatui::layout::Rect::new(26, 0, 80, 20);
    app
}

#[cfg(test)]
fn mouse(
    kind: crossterm::event::MouseEventKind,
    col: u16,
    row: u16,
) -> crossterm::event::MouseEvent {
    crossterm::event::MouseEvent {
        kind,
        column: col,
        row,
        modifiers: crossterm::event::KeyModifiers::empty(),
    }
}

#[cfg(test)]
fn numbered_lines_bytes(count: usize) -> Vec<u8> {
    (0..count)
        .map(|i| format!("{i:06}\r\n"))
        .collect::<String>()
        .into_bytes()
}

#[cfg(test)]
fn capture_snapshot(state: &AppState) -> crate::persist::SessionSnapshot {
    let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
    crate::persist::capture(
        &state.workspaces,
        &state.terminals,
        &terminal_runtimes,
        state.active,
        state.selected,
        state.sidebar_width,
        state.sidebar_section_split,
        state.collapsed_space_keys.clone(),
    )
}

#[cfg(test)]
fn root_layout_ratio(snapshot: &crate::persist::SessionSnapshot) -> Option<f32> {
    match &snapshot.workspaces.first()?.tabs.first()?.layout {
        crate::persist::LayoutSnapshot::Split { ratio, .. } => Some(*ratio),
        crate::persist::LayoutSnapshot::Pane(_) => None,
    }
}

#[cfg(test)]
fn unique_temp_path(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("karvex-{name}-{}-{nanos}", std::process::id()))
}

// A poll interval that takes far longer than requested is local evidence of
// scheduler contention, so stretch the remaining budget by that drift rather
// than assuming a constant is always enough, clamped to a hard ceiling so a
// command that never writes the file (a real failure or hang) still fails in
// bounded time. When sleeps land on time (no contention), this is a no-op.
//
// This is a supplementary signal, not the primary defense: a sleeping thread
// is off the run queue and gets woken promptly by the OS timer even under
// heavy load, so its wake-up drift understates the slowdown a CPU-bound
// producer process actually experiences (it needs many scheduling turns to
// finish real work, so its completion time scales closer to the
// oversubscription ratio itself). `wait_for_file`'s base budget carries the
// primary defense; this only adds headroom for contention that worsens
// mid-wait.
#[cfg(test)]
#[cfg(unix)]
fn extend_wait_budget(
    budget: std::time::Duration,
    poll_interval: std::time::Duration,
    slept: std::time::Duration,
    max_budget: std::time::Duration,
) -> std::time::Duration {
    match slept.checked_sub(poll_interval) {
        Some(drift) => (budget + drift).min(max_budget),
        None => budget,
    }
}

#[cfg(test)]
#[cfg(unix)]
fn wait_for_file(path: &std::path::Path) -> String {
    // BASE_BUDGET is the original 2s scaled by a 3x factor: a reproduction at
    // 48 test threads on a 16-core box (a measured 3x oversubscription ratio)
    // showed that 2s plus drift-only extension still misses occasionally
    // (observed panic at ~2.19s elapsed, i.e. sleeps mostly landed on time
    // while the spawned shell was still starved), because sleep-wake drift
    // undercounts CPU-bound producer slowdown (see `extend_wait_budget`).
    // Scaling the budget by the oversubscription ratio directly, instead of
    // only reacting to the weaker drift proxy, matches the mechanism causing
    // the delay: drift-extension stays supplementary headroom for contention
    // that worsens mid-wait, not the primary defense. MAX_BUDGET is scaled
    // the same way so a genuine hang still fails in bounded time. The larger
    // base only costs wall-clock time on the failure/timeout path — a
    // passing wait still returns the moment the file appears, so this does
    // not slow down normal (uncontended) test runs.
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);
    const BASE_BUDGET: std::time::Duration = std::time::Duration::from_secs(6);
    const MAX_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

    let start = std::time::Instant::now();
    let mut budget = BASE_BUDGET;
    loop {
        if let Ok(content) = std::fs::read_to_string(path) {
            if !content.is_empty() {
                return content;
            }
        }
        if start.elapsed() >= budget {
            break;
        }
        let before_sleep = std::time::Instant::now();
        std::thread::sleep(POLL_INTERVAL);
        let slept = before_sleep.elapsed();
        budget = extend_wait_budget(budget, POLL_INTERVAL, slept, MAX_BUDGET);
    }
    panic!(
        "timed out waiting for {} after {:?}",
        path.display(),
        start.elapsed()
    );
}

#[cfg(test)]
#[cfg(unix)]
#[test]
fn extend_wait_budget_is_a_no_op_when_the_poll_lands_on_time() {
    let budget = std::time::Duration::from_secs(2);
    let extended = extend_wait_budget(
        budget,
        std::time::Duration::from_millis(20),
        std::time::Duration::from_millis(20),
        std::time::Duration::from_secs(20),
    );
    assert_eq!(extended, budget);
}

#[cfg(test)]
#[cfg(unix)]
#[test]
fn extend_wait_budget_is_a_no_op_when_the_poll_wakes_early() {
    let budget = std::time::Duration::from_secs(2);
    let extended = extend_wait_budget(
        budget,
        std::time::Duration::from_millis(20),
        std::time::Duration::from_millis(10),
        std::time::Duration::from_secs(20),
    );
    assert_eq!(extended, budget);
}

#[cfg(test)]
#[cfg(unix)]
#[test]
fn extend_wait_budget_grows_by_the_observed_scheduling_drift() {
    let budget = std::time::Duration::from_secs(2);
    let extended = extend_wait_budget(
        budget,
        std::time::Duration::from_millis(20),
        std::time::Duration::from_millis(60),
        std::time::Duration::from_secs(20),
    );
    assert_eq!(extended, budget + std::time::Duration::from_millis(40));
}

#[cfg(test)]
#[cfg(unix)]
#[test]
fn extend_wait_budget_clamps_to_the_hard_ceiling() {
    let budget = std::time::Duration::from_secs(19);
    let extended = extend_wait_budget(
        budget,
        std::time::Duration::from_millis(20),
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(20),
    );
    assert_eq!(extended, std::time::Duration::from_secs(20));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        )
    }

    #[tokio::test]
    async fn paste_routes_to_rename_modal_input() {
        let mut app = test_app();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("test")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::RenameTab;
        app.state.name_input = "2".into();
        app.state.name_input_replace_on_type = true;

        app.handle_paste("feature/logs".into()).await;

        assert_eq!(app.state.name_input, "feature/logs");
        assert!(!app.state.name_input_replace_on_type);
    }

    #[tokio::test]
    async fn paste_routes_to_keybind_help_query_only_when_searching() {
        let mut app = test_app();
        app.state.mode = Mode::KeybindHelp;
        app.handle_paste("ignored".into()).await;
        assert!(app.state.keybind_help.query.is_empty());

        app.state.keybind_help.search_focused = true;
        app.state.keybind_help.scroll = 3;
        app.handle_paste("work\nspace".into()).await;

        assert_eq!(app.state.keybind_help.query, "workspace");
        assert_eq!(app.state.keybind_help.scroll, 0);
    }

    #[tokio::test]
    async fn paste_routes_to_new_linked_worktree_input() {
        let mut app = test_app();
        app.state.mode = Mode::NewLinkedWorktree;
        app.state.name_input = "generated-branch".into();
        app.state.name_input_replace_on_type = true;
        app.state.worktree_create = Some(crate::app::state::WorktreeCreateState {
            source_workspace_id: "source".into(),
            source_checkout_path: "/repo/karvex".into(),
            source_existing_membership: None,
            source_repo_root: "/repo/karvex".into(),
            repo_key: "repo-key".into(),
            repo_name: "karvex".into(),
            branch: "generated-branch".into(),
            checkout_path: "/repo/karvex-generated-branch".into(),
            error: None,
            creating: false,
        });

        app.handle_paste("feature/linear-302".into()).await;

        assert_eq!(app.state.name_input, "feature/linear-302");
        assert_eq!(
            app.state
                .worktree_create
                .as_ref()
                .map(|create| create.branch.as_str()),
            Some("feature/linear-302")
        );
    }

    #[test]
    fn modal_paste_shortcut_matches_platform_primary_v() {
        #[cfg(target_os = "macos")]
        let modifiers = KeyModifiers::SUPER;
        #[cfg(not(target_os = "macos"))]
        let modifiers = KeyModifiers::CONTROL;

        assert!(is_modal_paste_shortcut(&KeyEvent::new(
            KeyCode::Char('v'),
            modifiers
        )));
        assert!(is_modal_paste_shortcut(&KeyEvent::new(
            KeyCode::Char('V'),
            modifiers | KeyModifiers::SHIFT
        )));
        assert!(!is_modal_paste_shortcut(&KeyEvent::new(
            KeyCode::Char('v'),
            KeyModifiers::ALT
        )));
    }

    #[test]
    fn modal_paste_target_is_active_only_for_text_inputs() {
        let mut state = AppState::test_new();

        state.mode = Mode::RenameTab;
        assert!(modal_paste_target_active(&state));

        state.mode = Mode::Navigator;
        state.navigator.search_focused = false;
        assert!(!modal_paste_target_active(&state));
        state.navigator.search_focused = true;
        assert!(modal_paste_target_active(&state));

        state.mode = Mode::KeybindHelp;
        state.keybind_help.search_focused = false;
        assert!(!modal_paste_target_active(&state));
        state.keybind_help.search_focused = true;
        assert!(modal_paste_target_active(&state));

        state.mode = Mode::ConfirmClose;
        assert!(!modal_paste_target_active(&state));

        state.mode = Mode::WorkflowDag;
        assert!(!modal_paste_target_active(&state));
        state.view.dag.steer = Some(String::new());
        assert!(modal_paste_target_active(&state));
    }

    // ── workflow DAG overlay (`05-phase-plan.md` W6, step 4c) ───────────────

    fn dag_node(
        idx: usize,
        path: &str,
        successors: Vec<crate::workflow::model::RunNodeIdx>,
        predecessors: Vec<crate::workflow::model::RunNodeIdx>,
    ) -> crate::app::state::DagNodeView {
        crate::app::state::DagNodeView {
            idx: crate::workflow::model::RunNodeIdx(idx),
            path: path.to_string(),
            label: path.to_string(),
            status: crate::workflow::model::NodeStatus::Running,
            model: "sonnet".into(),
            effort: "low".into(),
            attempt: 1,
            usage: crate::workflow::model::NodeUsage::default(),
            duration_ms: 0,
            delivery_failure: None,
            growth_notice: None,
            depth: 0,
            parent: None,
            summary: None,
            blocker: None,
            // A `Running` node has a pane by construction; the steer affordance
            // reads exactly this field to know there is something to steer.
            pane_id: Some(format!("w1:p{idx}")),
            owner: String::new(),
            subject: String::new(),
            emergent: false,
            owner_pane_id: None,
            agent_state: None,
            successors,
            predecessors,
        }
    }

    /// Two stacked boxes, laid out the way the view-computation pass would
    /// store them.
    fn dag_view() -> crate::app::state::DagViewState {
        use crate::workflow::layout::{DagLayout, LayoutRect};
        use crate::workflow::model::RunNodeIdx;
        use ratatui::layout::Rect;

        crate::app::state::DagViewState {
            run_id: "workflow_run:1".into(),
            run_status: Some(crate::workflow::model::RunStatus::Running),
            counts: crate::app::state::DagRunCounts {
                total: 2,
                running: 2,
                ..crate::app::state::DagRunCounts::default()
            },
            header_rect: Rect::new(0, 0, 80, 1),
            graph_rect: Rect::new(0, 1, 80, 18),
            detail_rect: Rect::new(0, 19, 80, 4),
            footer_rect: Rect::new(0, 23, 80, 1),
            layout: DagLayout {
                nodes: vec![
                    (RunNodeIdx(0), LayoutRect::new(0, 1, 22, 3)),
                    (RunNodeIdx(1), LayoutRect::new(0, 6, 22, 3)),
                ],
                edges: Vec::new(),
            },
            nodes: vec![
                dag_node(0, "plan", vec![RunNodeIdx(1)], Vec::new()),
                dag_node(1, "build", Vec::new(), vec![RunNodeIdx(0)]),
            ],
            selected: Some(RunNodeIdx(0)),
            ..crate::app::state::DagViewState::default()
        }
    }

    /// A `DagViewState` projecting a past run, plus the snapshot behind it.
    fn historical_app() -> App {
        historical_app_with(None)
    }

    /// The same, for a run executed by a Claude Code team lead when `team` is
    /// `Some` (`09-agent-teams-rework.md` §3.1). The team lives on the
    /// snapshot and the flag on the view, exactly as the compute pass derives
    /// it, so the input path is exercised through the same state the renderer
    /// sees.
    fn historical_app_with(team: Option<&str>) -> App {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowDag;
        app.state.view.dag = crate::app::state::DagViewState {
            historical: true,
            lead_run: team.is_some(),
            ..dag_view()
        };
        app.state
            .set_historical_run(Some(crate::app::state::HistoricalRunSnapshot {
                graph: Box::new(crate::workflow::model::RunGraph {
                    run_id: crate::workflow::model::RunId::new("workflow_run:past"),
                    version_id: crate::workflow::model::KvdagVersionId::new("v1"),
                    tier: crate::workflow::tier::Tier::Auto,
                    growth: crate::workflow::model::GrowthLimits::default(),
                    assignments: std::collections::BTreeMap::new(),
                    nodes: Vec::new(),
                    edges: Vec::new(),
                    status: crate::workflow::model::RunStatus::Succeeded,
                    seq: 0,
                    epilogue: None,
                }),
                workflow_name: "past".to_string(),
                interrogations: Vec::new(),
                team_name: team.map(str::to_string),
                lead_pane_id: None,
                projected: std::collections::BTreeMap::new(),
                members: Vec::new(),
            }));
        app
    }

    /// A lead run whose nodes are owned by a teammate holding a pane, and
    /// whose own bindings are cleared — the engine binds nodes, and it did not
    /// run this — so nothing masks the owner resolution under test.
    fn lead_run_app() -> App {
        let mut app = historical_app_with(Some("session-213aa9bf"));
        // Notices reach `state.toast` only under karvex's own delivery; the
        // other deliveries fire an OS notification instead, which a unit test
        // cannot observe.
        app.state.toast_config.delivery = crate::config::ToastDelivery::Karvex;
        for node in &mut app.state.view.dag.nodes {
            node.pane_id = None;
            node.owner = "verify".to_string();
            node.owner_pane_id = Some("w1:p9".to_string());
        }
        app
    }

    /// §3.5: the engine's verbs are refused on a lead run rather than
    /// half-working. `s` must not open a line that could only be swallowed,
    /// and `i`/`I` must not dispatch an interrogation against a run whose
    /// sessions karvex never owned.
    #[test]
    fn the_engine_verbs_are_refused_on_a_lead_run() {
        for key in ['s', 'i', 'I'] {
            let mut app = lead_run_app();
            app.handle_workflow_dag_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));

            assert_eq!(
                app.state.view.dag.steer, None,
                "`{key}` must not open a steer line on a lead run"
            );
            assert_eq!(
                app.state.mode,
                Mode::WorkflowDag,
                "`{key}` must not close the overlay"
            );
            let toast = app
                .state
                .toast
                .as_ref()
                .unwrap_or_else(|| panic!("`{key}` must say why it did nothing"));
            assert!(
                toast.context.contains("focus the node's pane"),
                "`{key}` must name the affordance that replaced it: {}",
                toast.context
            );
        }
    }

    /// The same three keys on an engine-era run behave exactly as they did:
    /// this is the other half of the refusal, and the reason the refusal is
    /// gated on the run's execution model rather than on the overlay.
    #[test]
    fn the_engine_verbs_still_work_on_an_engine_era_run() {
        let mut live = test_app();
        live.state.mode = Mode::WorkflowDag;
        live.state.view.dag = dag_view();
        live.handle_workflow_dag_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(live.state.view.dag.steer.as_deref(), Some(""));
    }

    /// §3.5: `Enter` on a lead run opens the pane of the teammate that owns the
    /// node — the primary steer affordance now — and falls back to the node's
    /// own binding only when there is no owner pane to prefer.
    #[test]
    fn enter_on_a_lead_run_resolves_the_owners_pane() {
        let app = lead_run_app();
        assert_eq!(app.workflow_dag_focus_target().as_deref(), Some("w1:p9"));

        let mut fallback = lead_run_app();
        for node in &mut fallback.state.view.dag.nodes {
            node.owner_pane_id = None;
            node.pane_id = Some("w1:p3".to_string());
        }
        assert_eq!(
            fallback.workflow_dag_focus_target().as_deref(),
            Some("w1:p3")
        );

        let mut unclaimed = lead_run_app();
        for node in &mut unclaimed.state.view.dag.nodes {
            node.owner_pane_id = None;
        }
        assert_eq!(unclaimed.workflow_dag_focus_target(), None);
        unclaimed.handle_workflow_dag_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let toast = unclaimed
            .state
            .toast
            .as_ref()
            .expect("a node with no pane says so");
        assert!(toast.context.contains("no pane yet"), "{}", toast.context);

        // An engine-era run ignores the owner entirely and reads its binding.
        let mut engine = historical_app();
        for node in &mut engine.state.view.dag.nodes {
            node.owner_pane_id = Some("w1:p9".to_string());
            node.pane_id = Some("w1:p3".to_string());
        }
        assert_eq!(engine.workflow_dag_focus_target().as_deref(), Some("w1:p3"));
    }

    /// m7: a past run is not the active run, so a steer would answer the
    /// not-the-active-run guard. The line must not open at all — a steer line
    /// that takes text and then swallows it is exactly the 2.15 failure the
    /// no-pane check already guards against.
    #[test]
    fn steering_a_historical_run_is_refused_instead_of_opening_the_line() {
        let mut app = historical_app();
        app.handle_workflow_dag_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(
            app.state.view.dag.steer, None,
            "the steer line must not open"
        );

        // And the live projection of the same view still opens it.
        let mut live = test_app();
        live.state.mode = Mode::WorkflowDag;
        live.state.view.dag = dag_view();
        live.handle_workflow_dag_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(live.state.view.dag.steer.as_deref(), Some(""));
    }

    /// Closing the overlay closes the historical projection with it. A
    /// snapshot left behind hijacks the next DAG open, which would put a past
    /// run on screen for a user who asked for the live one.
    #[test]
    fn closing_the_dag_view_clears_the_historical_snapshot() {
        let mut app = historical_app();
        assert!(app.state.historical_run().is_some());

        app.handle_workflow_dag_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.state.mode, Mode::Navigate);
        assert!(
            app.state.historical_run().is_none(),
            "a past run must not survive the overlay that showed it"
        );
    }

    /// Both interrogate keys are handled by the DAG's own arm. Before they
    /// existed, `I` fell through to the modal-action lookup — and any binding
    /// that resolved to `Close` there would have torn the overlay down instead
    /// of interrogating. Neither key may close it, whatever the request answers.
    #[test]
    fn neither_interrogate_key_falls_through_to_close() {
        for key in ['i', 'I'] {
            let mut app = historical_app();
            app.handle_workflow_dag_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
            assert_eq!(
                app.state.mode,
                Mode::WorkflowDag,
                "{key} must not close the overlay"
            );
            assert!(app.state.historical_run().is_some(), "{key}");
            assert_eq!(app.state.view.dag.steer, None, "{key} is not a steer");
        }
        // Shift-reported as a modifier as well as a capital, which is what
        // crossterm sends for Shift+I on most terminals.
        let mut app = historical_app();
        app.handle_workflow_dag_key(KeyEvent::new(KeyCode::Char('I'), KeyModifiers::SHIFT));
        assert_eq!(app.state.mode, Mode::WorkflowDag);
    }

    /// `Enter` on a past node whose pane is gone says so rather than doing
    /// nothing, and does not leave the overlay.
    #[test]
    fn focusing_a_historical_node_with_no_pane_says_why() {
        let mut app = historical_app();
        app.handle_workflow_dag_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.state.mode, Mode::WorkflowDag, "the overlay stays open");
        assert!(app.state.historical_run().is_some());
    }

    #[test]
    fn workflow_dag_keys_navigate_the_graph_and_escape_closes_the_overlay() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowDag;
        app.state.view.dag = dag_view();

        app.handle_workflow_dag_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(
            app.state.view.dag.selected,
            Some(crate::workflow::model::RunNodeIdx(1))
        );
        app.handle_workflow_dag_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            app.state.view.dag.selected,
            Some(crate::workflow::model::RunNodeIdx(0))
        );

        app.handle_workflow_dag_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.state.mode, Mode::Navigate);
    }

    #[test]
    fn workflow_dag_steer_line_collects_text_and_escape_only_closes_the_line() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowDag;
        app.state.view.dag = dag_view();

        app.handle_workflow_dag_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.state.view.dag.steer.as_deref(), Some(""));

        for character in "hi".chars() {
            app.handle_workflow_dag_key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::NONE,
            ));
        }
        assert_eq!(app.state.view.dag.steer.as_deref(), Some("hi"));
        app.handle_workflow_dag_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.state.view.dag.steer.as_deref(), Some("h"));

        // Escape dismisses the steer line, not the overlay.
        app.handle_workflow_dag_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.state.view.dag.steer, None);
        assert_eq!(app.state.mode, Mode::WorkflowDag);
    }

    #[test]
    fn workflow_dag_steer_needs_a_run_and_a_selection() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowDag;
        app.state.view.dag = crate::app::state::DagViewState::default();

        app.handle_workflow_dag_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.state.view.dag.steer, None);
    }

    /// 2.15: the steer line used to open on any node, take the text, and drop
    /// it. A node with no pane has nothing to steer, and says so instead of
    /// accepting input it cannot deliver.
    #[test]
    fn workflow_dag_steer_refuses_a_node_with_no_pane_and_says_why() {
        let mut app = test_app();
        app.state.toast_config.delivery = crate::config::ToastDelivery::Karvex;
        app.state.mode = Mode::WorkflowDag;
        app.state.view.dag = dag_view();
        if let Some(node) = app.state.view.dag.nodes.first_mut() {
            node.pane_id = None;
        }

        app.handle_workflow_dag_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));

        assert_eq!(app.state.view.dag.steer, None);
        let toast = app.state.toast.as_ref().expect("the refusal is shown");
        assert!(toast.context.contains("no pane to steer"), "{toast:?}");
    }

    #[test]
    fn workflow_dag_click_selects_exactly_the_node_the_frame_drew() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowDag;
        app.state.view.dag = dag_view();

        // Every cell of every stored box selects that box, and the mouse event
        // never escapes the overlay.
        for (idx, rect) in app.state.view.dag.layout.nodes.clone() {
            for row in rect.y..rect.bottom() {
                for column in rect.x..rect.right() {
                    app.state.view.dag.selected = None;
                    let handled = app.handle_overlay_mouse(MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column,
                        row,
                        modifiers: KeyModifiers::NONE,
                    });
                    assert!(handled);
                    assert_eq!(app.state.view.dag.selected, Some(idx), "({column},{row})");
                }
            }
        }

        // A click in the gap between boxes hits nothing.
        app.state.view.dag.selected = None;
        assert!(app.handle_overlay_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 5,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.state.view.dag.selected, None);
    }

    #[test]
    fn workflow_dag_mode_round_trips_through_the_view_computation() {
        let mut app = AppState::test_new();
        app.workspaces = vec![crate::workspace::Workspace::test_new("one")];
        app.active = Some(0);
        app.selected = 0;
        app.mode = Mode::WorkflowDag;

        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        crate::ui::compute_view(&mut app, area);
        assert_eq!(app.view.dag.header_rect.width, area.width);
        assert_eq!(app.view.dag.footer_rect.bottom(), area.bottom());

        leave_modal(&mut app);
        assert_eq!(app.mode, Mode::Terminal);

        // Leaving the overlay leaves no geometry behind for a later hit-test.
        crate::ui::compute_view(&mut app, area);
        assert_eq!(app.view.dag, crate::app::state::DagViewState::default());
    }
}
