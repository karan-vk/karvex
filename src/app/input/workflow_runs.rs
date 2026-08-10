//! The run browser overlay's input handling
//! (`docs/design/workflow-builder/07-phase3-plan.md` §WS-F).
//!
//! Split the way every other overlay's input is: the runtime half (`impl
//! App`) speaks the same in-process API path the CLI and the launcher use —
//! `workflow.run.list`, `workflow.summary.list`, `workflow.run` — so the
//! browser can never show or start anything the CLI could not. Hit-testing
//! duplicates the tiny rect-contains check the `ui` side also has rather than
//! importing it: `ui.rs` is frozen (never touched again past step 1b), and
//! nothing there re-exports the hit-test helpers this file needs, so a
//! handful of duplicated lines is cheaper than widening that surface.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};

use super::modal::{leave_modal, modal_action_from_key, ModalAction, WORKFLOW_RUNS_ACTIONS};
use crate::api::schema::{
    ErrorResponse, Method, ResponseResult, SuccessResponse, WorkflowRestoreRequest,
    WorkflowRunInfo, WorkflowRunListParams, WorkflowRunParams, WorkflowRunStatus,
    WorkflowRunSummaryInfo, WorkflowSummaryListParams, WorkflowTier,
};
use crate::app::state::{
    AppState, Mode, WorkflowRunsConfirmRestore, WorkflowRunsEntry, WorkflowRunsPrunedEntry,
    WorkflowRunsRunEntry, WorkflowRunsState,
};
use crate::app::App;
use crate::workflow::model::{NoticeLevel, RunId, RunStatus, UserNotice, WorkflowEvent};
use crate::workflow::tier::Tier;

/// Inserts typed or pasted text into whatever text field the run browser
/// ends up needing. No-op until WS-F lands a focused text field
/// (`07-phase3-plan.md` §WS-F, step 2e) — the browser's own delivers list
/// (list, detail, key-only actions) names none yet.
pub(crate) fn insert_workflow_runs_text(_state: &mut AppState, _text: &str) -> bool {
    false
}

// ── wire → state mapping ────────────────────────────────────────────────────

fn engine_tier(tier: WorkflowTier) -> Tier {
    match tier {
        WorkflowTier::Auto => Tier::Auto,
        WorkflowTier::Max => Tier::Max,
        WorkflowTier::High => Tier::High,
        WorkflowTier::Medium => Tier::Medium,
        WorkflowTier::Low => Tier::Low,
    }
}

fn engine_run_status(status: WorkflowRunStatus) -> RunStatus {
    match status {
        WorkflowRunStatus::Pending => RunStatus::Pending,
        WorkflowRunStatus::Running => RunStatus::Running,
        WorkflowRunStatus::Paused => RunStatus::Paused,
        WorkflowRunStatus::Succeeded => RunStatus::Succeeded,
        WorkflowRunStatus::Failed => RunStatus::Failed,
        WorkflowRunStatus::Cancelled => RunStatus::Cancelled,
    }
}

/// `reason` is the field every Phase 3 failure/error journal payload uses
/// (`07-phase3-plan.md` §4, e.g. `{"reason": "summary_failed"}`); falling
/// back to the raw JSON keeps an unrecognised shape visible instead of blank.
fn format_failure(value: &serde_json::Value) -> String {
    value
        .get("reason")
        .and_then(|reason| reason.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn run_entry(
    run: WorkflowRunInfo,
    summary: Option<WorkflowRunSummaryInfo>,
) -> WorkflowRunsRunEntry {
    let mut args: Vec<(String, String)> = run.args.into_iter().collect();
    args.sort_by(|a, b| a.0.cmp(&b.0));
    WorkflowRunsRunEntry {
        run_id: run.run_id,
        workflow_id: run.workflow_id,
        workflow_name: run.workflow_name,
        tier: Some(engine_tier(run.tier)),
        status: engine_run_status(run.status),
        started_at_unix_ms: run.started_at_unix_ms,
        nodes_done: run.nodes_done as usize,
        nodes_total: run.nodes_total as usize,
        args: args
            .into_iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect(),
        limits: format!(
            "max_depth {} · max_nodes {} ({} live)",
            run.max_depth, run.max_nodes, run.nodes_live
        ),
        blocker: run
            .failure
            .as_ref()
            .map(format_failure)
            .or_else(|| run.growth_limited.map(|limit| limit.message)),
        summary_outcome: summary.as_ref().map(|summary| summary.outcome.clone()),
        summary_first_highlight: summary.and_then(|summary| summary.highlights.into_iter().next()),
    }
}

fn pruned_entry(summary: WorkflowRunSummaryInfo) -> WorkflowRunsPrunedEntry {
    WorkflowRunsPrunedEntry {
        run_id: summary.run_id,
        workflow_id: summary.workflow_id,
        workflow_name: summary.workflow_name,
        summary_outcome: summary.outcome,
        summary_text: summary.text,
    }
}

/// Merges `workflow.run.list` and `workflow.summary.list` into one
/// newest-first row set: a live/closed run's summary (when one exists) folds
/// into its own row rather than appearing twice, and a pruned run — no
/// `workflow_run` row left, only its summary — becomes its own dimmed row
/// (`07-phase3-plan.md` §0.3, §WS-F).
fn build_entries(
    runs: Vec<WorkflowRunInfo>,
    summaries: Vec<WorkflowRunSummaryInfo>,
) -> Vec<WorkflowRunsEntry> {
    let mut by_run: HashMap<String, WorkflowRunSummaryInfo> = HashMap::new();
    let mut pruned: Vec<(u64, WorkflowRunsEntry)> = Vec::new();
    for summary in summaries {
        if summary.run_pruned {
            let at = summary.created_at_unix_ms;
            pruned.push((at, WorkflowRunsEntry::PrunedSummary(pruned_entry(summary))));
        } else {
            by_run.insert(summary.run_id.clone(), summary);
        }
    }

    let mut rows: Vec<(u64, WorkflowRunsEntry)> = runs
        .into_iter()
        .map(|run| {
            let at = run.started_at_unix_ms;
            let summary = by_run.remove(&run.run_id);
            (at, WorkflowRunsEntry::Run(run_entry(run, summary)))
        })
        .collect();
    rows.extend(pruned);
    rows.sort_by(|(a, _), (b, _)| b.cmp(a));
    rows.into_iter().map(|(_, entry)| entry).collect()
}

fn success_result(response: &str) -> Option<ResponseResult> {
    serde_json::from_str::<SuccessResponse>(response)
        .ok()
        .map(|success| success.result)
}

fn error_message(response: &str) -> Option<String> {
    serde_json::from_str::<ErrorResponse>(response)
        .ok()
        .map(|error| error.error.message)
}

fn entry_run_id(entry: &WorkflowRunsEntry) -> &str {
    match entry {
        WorkflowRunsEntry::Run(run) => &run.run_id,
        WorkflowRunsEntry::PrunedSummary(pruned) => &pruned.run_id,
    }
}

// ── hit-testing (duplicated from the `ui` side; see module docs) ───────────

fn rect_contains(rect: ratatui::layout::Rect, col: u16, row: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn row_at(state: &WorkflowRunsState, col: u16, row: u16) -> Option<usize> {
    state
        .row_rects
        .iter()
        .position(|rect| rect_contains(*rect, col, row))
}

impl App {
    /// Opens the browser, seeded from `workflow.run.list` (`workflow_id:
    /// None`) and `workflow.summary.list`.
    ///
    /// Unlike the launcher, an empty result still opens: browsing a server
    /// with no run history yet is a normal, informative state, not a
    /// dead end — the overlay says so instead of refusing to appear. Only a
    /// structural failure (no store, wrong build) refuses, so the caller can
    /// tell the user why nothing came up.
    pub(crate) fn open_workflow_runs(&mut self) -> bool {
        let response = self.dispatch_api_request(
            "tui.workflow.run.list",
            Method::WorkflowRunList(WorkflowRunListParams {
                workflow_id: None,
                limit: None,
            }),
        );
        let Some(ResponseResult::WorkflowRunList { runs }) = success_result(&response) else {
            if let Some(message) = error_message(&response) {
                tracing::debug!(%message, "the run browser could not list runs");
            }
            return false;
        };

        // Summaries are supplementary detail (outcome, highlights, pruned
        // rows): a failure here degrades the browser, it does not refuse to
        // open it — the run list alone is still a usable browser.
        let summaries_response = self.dispatch_api_request(
            "tui.workflow.summary.list",
            Method::WorkflowSummaryList(WorkflowSummaryListParams {
                workflow_id: None,
                limit: None,
            }),
        );
        let summaries = match success_result(&summaries_response) {
            Some(ResponseResult::WorkflowSummaryList { summaries }) => summaries,
            _ => Vec::new(),
        };

        self.state.view.workflow_runs = WorkflowRunsState {
            entries: build_entries(runs, summaries),
            ..WorkflowRunsState::default()
        };
        self.state.mode = Mode::WorkflowRuns;
        true
    }

    /// `keys.open_workflow_runs` was pressed and the browser could not open
    /// (no workflow store, or a slim build).
    pub(crate) fn notify_workflow_runs_unavailable(&mut self) {
        self.show_workflow_notice(UserNotice {
            level: NoticeLevel::Info,
            run: None,
            path: None,
            message: "workflow run history is unavailable on this server".to_string(),
        });
    }

    /// Re-loads the browser's row set after a run-level event arrives while
    /// it is open (`07-phase3-plan.md` §A7, C-4). Called from
    /// `app/workflow.rs`'s `emit_workflow_event` — the one place every
    /// workflow event already flows through — rather than from a dedicated
    /// subscription of the browser's own: `compute_workflow_runs_view` stays
    /// pure, and this is a runtime-half method exactly like the key/mouse
    /// handlers, just triggered by an event arrival instead of a keypress.
    ///
    /// No-ops outside `Mode::WorkflowRuns`, and for every event kind that
    /// cannot change a list row: node-level events, and interrogation events
    /// (the browser shows interrogations only through the historical DAG,
    /// WS-H — never through the list). Re-anchors the selection by run id
    /// rather than by index, since a reload can reorder or add rows out from
    /// under a fixed index.
    pub(crate) fn refresh_workflow_runs_overlay(&mut self, event: &WorkflowEvent) {
        if self.state.mode != Mode::WorkflowRuns {
            return;
        }
        let is_run_level = matches!(
            event,
            WorkflowEvent::RunStarted { .. }
                | WorkflowEvent::RunUpdated { .. }
                | WorkflowEvent::RunFinished { .. }
                | WorkflowEvent::RunSummarized { .. }
        );
        if !is_run_level {
            return;
        }

        let selected_run_id = self
            .state
            .view
            .workflow_runs
            .entries
            .get(self.state.view.workflow_runs.selected)
            .map(|entry| entry_run_id(entry).to_string());

        if !self.open_workflow_runs() {
            return;
        }
        let Some(run_id) = selected_run_id else {
            return;
        };
        if let Some(index) = self
            .state
            .view
            .workflow_runs
            .entries
            .iter()
            .position(|entry| entry_run_id(entry) == run_id)
        {
            self.state.view.workflow_runs.selected = index;
        }
    }

    pub(crate) fn handle_workflow_runs_key(&mut self, key: KeyEvent) {
        if self.state.view.workflow_runs.confirm_restore.is_some() {
            match key.code {
                KeyCode::Enter => self.submit_workflow_runs_restore(),
                KeyCode::Esc => self.state.view.workflow_runs.confirm_restore = None,
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.move_workflow_runs_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_workflow_runs_selection(-1),
            KeyCode::Enter => self.open_selected_workflow_run(),
            KeyCode::Char('r') | KeyCode::Char('R') => self.begin_workflow_runs_restore(),
            _ => {
                if let Some(ModalAction::Close) = modal_action_from_key(&key, WORKFLOW_RUNS_ACTIONS)
                {
                    leave_modal(&mut self.state);
                }
            }
        }
    }

    /// Mouse handling for the run browser overlay: the hit-test reads
    /// exactly the rects the view-computation pass stored, so there is no
    /// second geometry to keep in sync — same rule as the DAG/launcher
    /// overlays.
    pub(super) fn handle_workflow_runs_mouse(&mut self, mouse: MouseEvent) -> bool {
        if self.state.view.workflow_runs.confirm_restore.is_some() {
            // The confirm dialog only answers to keys, like `ConfirmClose`.
            return true;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.move_workflow_runs_selection(-1),
            MouseEventKind::ScrollDown => self.move_workflow_runs_selection(1),
            MouseEventKind::Down(MouseButton::Left) => {
                self.click_workflow_runs(mouse.column, mouse.row)
            }
            _ => {}
        }
        true
    }

    fn click_workflow_runs(&mut self, column: u16, row: u16) {
        let runs = &self.state.view.workflow_runs;
        let Some(index) = row_at(runs, column, row) else {
            // A click inside the modal but on no row changes nothing; a click
            // outside it closes the browser, the same gesture every other
            // overlay answers to.
            if !rect_contains(runs.modal_rect, column, row) {
                leave_modal(&mut self.state);
            }
            return;
        };
        if index == self.state.view.workflow_runs.selected {
            self.open_selected_workflow_run();
        } else {
            self.state.view.workflow_runs.selected = index;
        }
    }

    fn move_workflow_runs_selection(&mut self, delta: isize) {
        let runs = &mut self.state.view.workflow_runs;
        if runs.entries.is_empty() {
            return;
        }
        let len = runs.entries.len() as isize;
        let current = runs.selected.min(runs.entries.len() - 1) as isize;
        let next = (current + delta).rem_euclid(len) as usize;
        runs.selected = next;
    }

    /// `Enter` on the selected row: the live run opens through the existing
    /// live path, a closed run opens as a read-only historical projection
    /// (`07-phase3-plan.md` §WS-H's `load_historical_run`). A pruned row has
    /// no `workflow_run` left to open — the same call answers
    /// `workflow_run_pruned`, surfaced as a notice rather than special-cased
    /// here, since the server's answer is the one place that refusal is
    /// authoritative.
    fn open_selected_workflow_run(&mut self) {
        let Some(entry) = self
            .state
            .view
            .workflow_runs
            .entries
            .get(self.state.view.workflow_runs.selected)
        else {
            return;
        };
        let run_id = entry_run_id(entry).to_string();

        let is_live_run = self
            .state
            .workflow_run_graph()
            .is_some_and(|graph| graph.run_id.as_str() == run_id);
        if is_live_run {
            self.state.mode = Mode::WorkflowDag;
            return;
        }

        if let Err(message) = self.load_historical_run(&run_id) {
            self.show_workflow_notice(UserNotice {
                level: NoticeLevel::Error,
                run: Some(RunId::new(run_id)),
                path: None,
                message,
            });
        }
    }

    /// `r`/`R`: opens the restore-all confirmation. A pruned row's reason is
    /// already the fixed line permanently shown in its detail strip, so there
    /// is nothing more to say by pressing the key — it is simply inert there
    /// rather than opening a dialog with nowhere useful to go. A run already
    /// in flight is refused by the server's existing `workflow_run_in_flight`
    /// guard when the confirm is submitted; there is no client-side
    /// liveness check to duplicate that against.
    fn begin_workflow_runs_restore(&mut self) {
        let Some(WorkflowRunsEntry::Run(run)) = self
            .state
            .view
            .workflow_runs
            .entries
            .get(self.state.view.workflow_runs.selected)
        else {
            return;
        };
        self.state.view.workflow_runs.confirm_restore = Some(WorkflowRunsConfirmRestore {
            run_id: run.run_id.clone(),
            workflow_name: run.workflow_name.clone(),
        });
    }

    /// Starts a restore-all run through the same in-process `workflow.run`
    /// path the CLI's `--restore-from` uses, with an empty `nodes` selector
    /// meaning "every compatible succeeded node" (`07-phase3-plan.md` §4
    /// D18). Every outcome — success, refusal, a fully-skipped restore —
    /// closes the confirm dialog; a fully-skipped restore is still a
    /// successful run start (§4 D18) and is not distinguished from any other
    /// start here.
    fn submit_workflow_runs_restore(&mut self) {
        let Some(confirm) = self.state.view.workflow_runs.confirm_restore.take() else {
            return;
        };
        let Some(WorkflowRunsEntry::Run(run)) = self
            .state
            .view
            .workflow_runs
            .entries
            .iter()
            .find(|entry| entry_run_id(entry) == confirm.run_id)
        else {
            return;
        };
        let workflow_id = run.workflow_id.clone();

        let params = WorkflowRunParams {
            workflow_id,
            version: None,
            tier: None,
            args: HashMap::new(),
            restore_from: Some(WorkflowRestoreRequest {
                run_id: confirm.run_id.clone(),
                nodes: Vec::new(),
                allow_changed: false,
            }),
            include_prior_summaries: None,
        };
        let response =
            self.dispatch_runtime_mutation("tui.workflow.run", Method::WorkflowRun(params));
        match success_result(&response) {
            Some(ResponseResult::WorkflowRunStarted { .. }) => {
                if self.state.workflow_run_graph().is_some() {
                    self.state.mode = Mode::WorkflowDag;
                }
            }
            _ => {
                let message =
                    error_message(&response).unwrap_or_else(|| "restore failed".to_string());
                self.show_workflow_notice(UserNotice {
                    level: NoticeLevel::Error,
                    run: Some(RunId::new(confirm.run_id)),
                    path: None,
                    message,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::layout::Rect;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn wire_run(id: &str, status: WorkflowRunStatus, started_at: u64) -> WorkflowRunInfo {
        WorkflowRunInfo {
            run_id: id.to_string(),
            workflow_id: "workflow:1".to_string(),
            version_id: "kvdag_version:1".to_string(),
            tier: WorkflowTier::High,
            status,
            args: HashMap::new(),
            workspace_id: None,
            tab_id: None,
            started_at_unix_ms: started_at,
            ended_at_unix_ms: None,
            total_tokens: 0,
            total_tool_uses: 0,
            nodes_total: 3,
            nodes_done: 2,
            failure: None,
            max_depth: 3,
            max_nodes: 24,
            nodes_live: 2,
            growth_limited: None,
            workflow_name: "ship-it".to_string(),
            context_runs: Vec::new(),
            restore_from_run: None,
            lead_session_id: None,
            team_name: None,
            lead_pane_id: None,
            lead_prompt_version: None,
        }
    }

    fn wire_summary(run_id: &str, pruned: bool, created_at: u64) -> WorkflowRunSummaryInfo {
        WorkflowRunSummaryInfo {
            run_id: run_id.to_string(),
            workflow_id: "workflow:1".to_string(),
            workflow_name: "ship-it".to_string(),
            version_id: "kvdag_version:1".to_string(),
            text: "shipped the thing".to_string(),
            outcome: "succeeded".to_string(),
            highlights: vec!["added dark mode".to_string()],
            open_gaps: Vec::new(),
            per_node: Vec::new(),
            token_estimate: 100,
            generated_by_path: None,
            created_at_unix_ms: created_at,
            run_pruned: pruned,
        }
    }

    #[test]
    fn a_summary_folds_into_its_runs_row_rather_than_appearing_twice() {
        let entries = build_entries(
            vec![wire_run("run:1", WorkflowRunStatus::Succeeded, 1_000)],
            vec![wire_summary("run:1", false, 1_500)],
        );
        assert_eq!(entries.len(), 1);
        let WorkflowRunsEntry::Run(run) = &entries[0] else {
            panic!("expected a run row");
        };
        assert_eq!(run.summary_outcome.as_deref(), Some("succeeded"));
        assert_eq!(
            run.summary_first_highlight.as_deref(),
            Some("added dark mode")
        );
    }

    #[test]
    fn a_pruned_summary_becomes_its_own_dimmed_row() {
        let entries = build_entries(Vec::new(), vec![wire_summary("run:gone", true, 1_000)]);
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0], WorkflowRunsEntry::PrunedSummary(_)));
    }

    #[test]
    fn entries_sort_newest_first_across_runs_and_pruned_summaries() {
        let entries = build_entries(
            vec![
                wire_run("run:1", WorkflowRunStatus::Succeeded, 1_000),
                wire_run("run:3", WorkflowRunStatus::Succeeded, 3_000),
            ],
            vec![wire_summary("run:gone", true, 2_000)],
        );
        let ids: Vec<&str> = entries.iter().map(entry_run_id).collect();
        assert_eq!(ids, vec!["run:3", "run:gone", "run:1"]);
    }

    #[test]
    fn args_render_sorted_by_name_for_stable_output() {
        let mut run = wire_run("run:1", WorkflowRunStatus::Succeeded, 1_000);
        run.args
            .insert("scope".to_string(), "everything".to_string());
        run.args.insert("goal".to_string(), "ship it".to_string());
        let entry = run_entry(run, None);
        assert_eq!(entry.args, vec!["goal=ship it", "scope=everything"]);
    }

    #[test]
    fn a_failure_reason_becomes_the_blocker_line() {
        let mut run = wire_run("run:1", WorkflowRunStatus::Failed, 1_000);
        run.failure = Some(serde_json::json!({"reason": "node plan failed"}));
        let entry = run_entry(run, None);
        assert_eq!(entry.blocker.as_deref(), Some("node plan failed"));
    }

    fn state_with_geometry(entries: Vec<WorkflowRunsEntry>) -> WorkflowRunsState {
        let row_rects = (0..entries.len())
            .map(|index| Rect::new(1, 2 + index as u16, 40, 1))
            .collect();
        WorkflowRunsState {
            entries,
            modal_rect: Rect::new(0, 0, 60, 20),
            row_rects,
            ..WorkflowRunsState::default()
        }
    }

    fn run_row(id: &str) -> WorkflowRunsEntry {
        WorkflowRunsEntry::Run(WorkflowRunsRunEntry {
            run_id: id.to_string(),
            workflow_id: "workflow:1".to_string(),
            workflow_name: "ship-it".to_string(),
            tier: Some(Tier::High),
            status: RunStatus::Succeeded,
            started_at_unix_ms: 1_000,
            nodes_done: 2,
            nodes_total: 3,
            args: Vec::new(),
            limits: String::new(),
            blocker: None,
            summary_outcome: None,
            summary_first_highlight: None,
        })
    }

    fn pruned_row(id: &str) -> WorkflowRunsEntry {
        WorkflowRunsEntry::PrunedSummary(WorkflowRunsPrunedEntry {
            run_id: id.to_string(),
            workflow_id: "workflow:1".to_string(),
            workflow_name: "ship-it".to_string(),
            summary_outcome: "succeeded".to_string(),
            summary_text: "shipped".to_string(),
        })
    }

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        #[cfg_attr(not(feature = "workflow"), allow(unused_mut))]
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        #[cfg(feature = "workflow")]
        {
            app.workflow_store = crate::app::workflow_store::WorkflowStoreHandle::in_memory();
        }
        app
    }

    #[test]
    fn j_and_k_move_the_selection_and_wrap() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowRuns;
        app.state.view.workflow_runs =
            state_with_geometry(vec![run_row("run:1"), run_row("run:2")]);

        app.handle_workflow_runs_key(key(KeyCode::Char('j')));
        assert_eq!(app.state.view.workflow_runs.selected, 1);
        app.handle_workflow_runs_key(key(KeyCode::Char('j')));
        assert_eq!(app.state.view.workflow_runs.selected, 0, "wraps around");
        app.handle_workflow_runs_key(key(KeyCode::Char('k')));
        assert_eq!(
            app.state.view.workflow_runs.selected, 1,
            "wraps the other way"
        );
    }

    #[test]
    fn escape_closes_the_browser() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowRuns;
        app.state.view.workflow_runs = state_with_geometry(vec![run_row("run:1")]);

        app.handle_workflow_runs_key(key(KeyCode::Esc));
        assert_ne!(app.state.mode, Mode::WorkflowRuns);
    }

    #[test]
    fn r_on_a_pruned_row_opens_no_confirm_dialog() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowRuns;
        app.state.view.workflow_runs = state_with_geometry(vec![pruned_row("run:gone")]);

        app.handle_workflow_runs_key(key(KeyCode::Char('r')));
        assert!(app.state.view.workflow_runs.confirm_restore.is_none());
    }

    #[test]
    fn r_on_a_run_opens_the_confirm_dialog_and_escape_cancels_it_without_closing() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowRuns;
        app.state.view.workflow_runs = state_with_geometry(vec![run_row("run:1")]);

        app.handle_workflow_runs_key(key(KeyCode::Char('r')));
        let confirm = app
            .state
            .view
            .workflow_runs
            .confirm_restore
            .clone()
            .expect("confirm opened");
        assert_eq!(confirm.run_id, "run:1");
        assert_eq!(confirm.workflow_name, "ship-it");

        app.handle_workflow_runs_key(key(KeyCode::Esc));
        assert!(app.state.view.workflow_runs.confirm_restore.is_none());
        assert_eq!(
            app.state.mode,
            Mode::WorkflowRuns,
            "cancelling the confirm does not close the browser"
        );
    }

    #[test]
    fn escape_while_confirming_never_falls_through_to_closing_the_browser() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowRuns;
        app.state.view.workflow_runs = state_with_geometry(vec![run_row("run:1")]);
        app.state.view.workflow_runs.confirm_restore = Some(WorkflowRunsConfirmRestore {
            run_id: "run:1".to_string(),
            workflow_name: "ship-it".to_string(),
        });

        app.handle_workflow_runs_key(key(KeyCode::Esc));
        assert_eq!(app.state.mode, Mode::WorkflowRuns);
        assert!(app.state.view.workflow_runs.confirm_restore.is_none());
    }

    fn click(app: &mut App, column: u16, row: u16) -> bool {
        app.handle_workflow_runs_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn a_click_on_a_row_selects_it_a_second_click_opens_it() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowRuns;
        app.state.view.workflow_runs =
            state_with_geometry(vec![run_row("run:1"), run_row("run:2")]);

        assert!(click(&mut app, 5, 3), "the modal swallows the event");
        assert_eq!(app.state.view.workflow_runs.selected, 1);
        assert_eq!(
            app.state.mode,
            Mode::WorkflowRuns,
            "selecting does not open"
        );

        // A run with no live-graph match and no store fails to load
        // historically; the browser stays open and a notice is shown rather
        // than panicking or silently doing nothing.
        click(&mut app, 5, 3);
        assert_eq!(app.state.mode, Mode::WorkflowRuns);
    }

    #[test]
    fn a_click_outside_the_modal_closes_it() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowRuns;
        app.state.view.workflow_runs = state_with_geometry(vec![run_row("run:1")]);

        // `state_with_geometry`'s modal spans x:0-59, y:0-19; this lands
        // outside it.
        click(&mut app, 70, 25);
        assert_ne!(app.state.mode, Mode::WorkflowRuns);
    }

    // Needs a store: with the feature off, `workflow.run.list` answers
    // `workflow_unavailable`, so the browser correctly declines to open and
    // shows the unavailable notice instead. That is the slim build's intended
    // behaviour, not this test's subject — an *empty* history still opening.
    #[cfg(feature = "workflow")]
    #[test]
    fn a_server_with_no_run_history_still_opens_the_browser() {
        let mut app = test_app();
        assert!(app.open_workflow_runs());
        assert_eq!(app.state.mode, Mode::WorkflowRuns);
        assert!(app.state.view.workflow_runs.entries.is_empty());
    }

    #[cfg(feature = "workflow")]
    fn create_workflow(app: &mut App, name: &str) -> String {
        let response = app.dispatch_api_request(
            "test.workflow.create",
            Method::WorkflowCreate(crate::api::schema::WorkflowCreateParams {
                definition: crate::api::schema::WorkflowDefinitionDocument {
                    format: crate::api::schema::WorkflowDefinitionFormat::Toml,
                    text: format!(
                        r#"
name = "{name}"
description = "a test workflow"
default_tier = "low"

[[node]]
key = "plan"
label = "Plan"
runner = "command"
command = ["/bin/true"]
prompt_template = "plan"
output_schema = {{ type = "object" }}
"#
                    ),
                },
            }),
        );
        let Some(ResponseResult::WorkflowCreated { workflow, .. }) = success_result(&response)
        else {
            panic!("the workflow was created: {response}");
        };
        workflow.workflow_id
    }

    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn the_browser_lists_a_started_run() {
        let mut app = test_app();
        let workflow_id = create_workflow(&mut app, "ship-feature");
        let response = app.dispatch_runtime_mutation(
            "test.workflow.run",
            Method::WorkflowRun(WorkflowRunParams {
                workflow_id,
                version: None,
                tier: None,
                args: HashMap::new(),
                restore_from: None,
                include_prior_summaries: None,
            }),
        );
        assert!(
            matches!(
                success_result(&response),
                Some(ResponseResult::WorkflowRunStarted { .. })
            ),
            "{response}"
        );

        assert!(app.open_workflow_runs());
        assert_eq!(app.state.view.workflow_runs.entries.len(), 1);
        let WorkflowRunsEntry::Run(run) = &app.state.view.workflow_runs.entries[0] else {
            panic!("expected a run row");
        };
        assert_eq!(run.workflow_name, "ship-feature");
    }

    #[test]
    fn refresh_no_ops_outside_the_browser_mode() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowDag;
        let before = app.state.view.workflow_runs.clone();

        app.refresh_workflow_runs_overlay(&WorkflowEvent::RunStarted {
            run: RunId::new("workflow_run:1"),
        });
        assert_eq!(app.state.view.workflow_runs, before);
        assert_eq!(app.state.mode, Mode::WorkflowDag);
    }

    #[test]
    fn refresh_no_ops_for_a_node_level_event() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowRuns;
        app.state.view.workflow_runs = state_with_geometry(vec![run_row("run:1")]);
        let before = app.state.view.workflow_runs.clone();

        app.refresh_workflow_runs_overlay(&WorkflowEvent::NodeCreated {
            run: RunId::new("workflow_run:1"),
            path: crate::workflow::model::InstancePath::new("root"),
        });
        assert_eq!(
            app.state.view.workflow_runs, before,
            "a node-level event cannot change a run-list row"
        );
    }

    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "drives the retired engine launch path: `workflow.run` now spawns a Claude Code team lead (09-agent-teams-rework.md §3.1). Reshaped in phase D."]
    fn refresh_on_a_run_level_event_reloads_and_reanchors_selection_by_run_id() {
        let mut app = test_app();
        let first = create_workflow(&mut app, "first");
        let response = app.dispatch_runtime_mutation(
            "test.workflow.run",
            Method::WorkflowRun(WorkflowRunParams {
                workflow_id: first,
                version: None,
                tier: None,
                args: HashMap::new(),
                restore_from: None,
                include_prior_summaries: None,
            }),
        );
        let Some(ResponseResult::WorkflowRunStarted { run, .. }) = success_result(&response) else {
            panic!("the run started: {response}");
        };

        assert!(app.open_workflow_runs());
        assert_eq!(app.state.view.workflow_runs.selected, 0);

        app.refresh_workflow_runs_overlay(&WorkflowEvent::RunUpdated {
            run: RunId::new(run.run_id.clone()),
            status: RunStatus::Running,
        });
        assert_eq!(
            app.state.view.workflow_runs.entries.len(),
            1,
            "the reload picked the run back up"
        );
        assert_eq!(
            app.state.view.workflow_runs.selected, 0,
            "the only run stays selected across the reload"
        );
    }
}
