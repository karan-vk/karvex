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
    ErrorResponse, EventKind, Method, ResponseResult, SuccessResponse, WorkflowRestoreRequest,
    WorkflowRunInfo, WorkflowRunListParams, WorkflowRunParams, WorkflowRunStatus,
    WorkflowRunSummaryInfo, WorkflowSummaryListParams, WorkflowTier,
};
use crate::app::state::{
    AppState, Mode, WorkflowRunsConfirmRestore, WorkflowRunsEntry, WorkflowRunsPrunedEntry,
    WorkflowRunsRunEntry, WorkflowRunsState,
};
use crate::app::App;
use crate::workflow::model::{NoticeLevel, RunId, RunStatus, UserNotice};
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

/// One team member, as the run browser's detail strip names them:
/// `name · model · pane`, and the member's **backend** where it has no pane —
/// which is how the in-process lead reads, since it is a session rather than a
/// pane (`09-agent-teams-rework.md` §3.4). Empty fields drop out rather than
/// leaving `· ·` behind.
fn member_label(member: &crate::api::schema::WorkflowRunMemberInfo) -> String {
    let where_it_runs = member
        .pane_id
        .clone()
        .unwrap_or_else(|| member.backend_type.clone());
    [
        member.name.as_str(),
        member.model.as_str(),
        where_it_runs.as_str(),
    ]
    .into_iter()
    .map(str::trim)
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" · ")
}

fn run_entry(
    run: WorkflowRunInfo,
    summary: Option<WorkflowRunSummaryInfo>,
    members: Vec<String>,
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
        team_name: run.team_name,
        members,
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
    members_by_run: &HashMap<String, Vec<String>>,
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
            let members = members_by_run.get(&run.run_id).cloned().unwrap_or_default();
            (at, WorkflowRunsEntry::Run(run_entry(run, summary, members)))
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
    /// The formatted member list for each lead run in `runs`.
    ///
    /// Engine-era runs are skipped outright: they have no team, so a
    /// `workflow.run.get` for them would be a round trip that could only ever
    /// answer "no members". A run whose graph will not load is simply absent
    /// from the map and renders without a member line — the browser degrades
    /// the way it already does for a missing summary rather than refusing to
    /// open.
    fn lead_run_members(&mut self, runs: &[WorkflowRunInfo]) -> HashMap<String, Vec<String>> {
        let lead_runs: Vec<String> = runs
            .iter()
            .filter(|run| run.team_name.is_some())
            .map(|run| run.run_id.clone())
            .collect();
        let mut members_by_run = HashMap::new();
        for run_id in lead_runs {
            let response = self.dispatch_api_request(
                "tui.workflow.run.get",
                Method::WorkflowRunGet(crate::api::schema::WorkflowRunTarget {
                    run_id: run_id.clone(),
                }),
            );
            let Some(ResponseResult::WorkflowRunGet { graph, .. }) = success_result(&response)
            else {
                continue;
            };
            members_by_run.insert(
                run_id,
                graph.members.iter().map(member_label).collect::<Vec<_>>(),
            );
        }
        members_by_run
    }

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

        // Members are on the run *graph*, not on `workflow.run.list`'s row, so
        // they cost one `workflow.run.get` each — and only for the runs that
        // could possibly have any. `team_name` is the exact predicate for that
        // (§3.1), which bounds the extra calls by the number of lead runs in a
        // page the server already caps.
        let members_by_run = self.lead_run_members(&runs);

        self.state.view.workflow_runs = WorkflowRunsState {
            entries: build_entries(runs, summaries, &members_by_run),
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
    /// `App::emit_workflow_run_event` — the one place every run-level workflow
    /// event flows through — rather than from a dedicated subscription of the
    /// browser's own: `compute_workflow_runs_view` stays pure, and this is a
    /// runtime-half method exactly like the key/mouse handlers, just triggered
    /// by an event arrival instead of a keypress.
    ///
    /// It takes the **wire** `EventKind` now. The trigger used to sit in the
    /// engine's `emit_workflow_event`, which fanned the engine-side
    /// `WorkflowEvent` out to both the wire and here; the engine went, and with
    /// it the only producer of that enum (`09-agent-teams-rework.md` §2). The
    /// lead path publishes wire envelopes directly, so this reads the same kind
    /// a subscribed client would.
    ///
    /// No-ops outside `Mode::WorkflowRuns`, and for every event kind that
    /// cannot change a list row: node-level events above all (several per node
    /// per run, and none of them moves a row). Re-anchors the selection by run
    /// id rather than by index, since a reload can reorder or add rows out from
    /// under a fixed index.
    pub(crate) fn refresh_workflow_runs_overlay(&mut self, event: EventKind) {
        if self.state.mode != Mode::WorkflowRuns {
            return;
        }
        let is_run_level = matches!(
            event,
            EventKind::WorkflowRunStarted
                | EventKind::WorkflowRunUpdated
                | EventKind::WorkflowRunFinished
                | EventKind::WorkflowRunSummarized
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
                if self.open_workflow_dag_on_the_live_run()
                    || self.state.workflow_run_graph().is_some()
                {
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
            &HashMap::new(),
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

    /// §3.4: the browser labels a lead run with its team and its members.
    /// Members come from the run *graph*, so the list loader fetches them per
    /// lead run and hands them in here — this asserts the mapping, and that an
    /// engine-era row is left exactly as it was.
    #[test]
    fn a_lead_runs_row_carries_its_team_and_formatted_members() {
        let mut run = wire_run("run:lead", WorkflowRunStatus::Running, 1_000);
        run.team_name = Some("session-213aa9bf".to_string());
        let members = HashMap::from([(
            "run:lead".to_string(),
            vec!["research · sonnet · w1:p3".to_string()],
        )]);

        let entries = build_entries(vec![run], Vec::new(), &members);
        let WorkflowRunsEntry::Run(row) = &entries[0] else {
            panic!("expected a run row");
        };
        assert_eq!(row.team_name.as_deref(), Some("session-213aa9bf"));
        assert_eq!(row.members, vec!["research · sonnet · w1:p3"]);

        let engine = build_entries(
            vec![wire_run("run:1", WorkflowRunStatus::Succeeded, 1_000)],
            Vec::new(),
            &members,
        );
        let WorkflowRunsEntry::Run(row) = &engine[0] else {
            panic!("expected a run row");
        };
        assert_eq!(row.team_name, None);
        assert!(row.members.is_empty());
    }

    /// A member with no pane is a session rather than a pane — the in-process
    /// lead — so it names its backend instead of leaving the slot blank.
    #[test]
    fn a_member_with_no_pane_names_its_backend_instead() {
        let mut member = crate::api::schema::WorkflowRunMemberInfo {
            name: "research".to_string(),
            agent_type: "Explore".to_string(),
            model: "sonnet".to_string(),
            pane_id: Some("w1:p3".to_string()),
            backend_type: "tmux".to_string(),
            is_active: true,
            cwd: None,
            first_seen_at_unix_ms: 1,
            last_seen_at_unix_ms: 2,
        };
        assert_eq!(member_label(&member), "research · sonnet · w1:p3");

        member.name = "team-lead".to_string();
        member.pane_id = None;
        member.backend_type = "in-process".to_string();
        member.model = String::new();
        assert_eq!(member_label(&member), "team-lead · in-process");
    }

    #[test]
    fn a_pruned_summary_becomes_its_own_dimmed_row() {
        let entries = build_entries(
            Vec::new(),
            vec![wire_summary("run:gone", true, 1_000)],
            &HashMap::new(),
        );
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
            &HashMap::new(),
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
        let entry = run_entry(run, None, Vec::new());
        assert_eq!(entry.args, vec!["goal=ship it", "scope=everything"]);
    }

    #[test]
    fn a_failure_reason_becomes_the_blocker_line() {
        let mut run = wire_run("run:1", WorkflowRunStatus::Failed, 1_000);
        run.failure = Some(serde_json::json!({"reason": "node plan failed"}));
        let entry = run_entry(run, None, Vec::new());
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
            team_name: None,
            members: Vec::new(),
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

    /// The browser lists a live lead run, not just closed ones. It reads
    /// `workflow.run.list`, which is store-backed, so the run row `workflow.run`
    /// writes is all it needs — there is no in-memory engine projection behind
    /// it any more.
    #[cfg(feature = "workflow")]
    #[test]
    fn the_browser_lists_a_live_lead_run() {
        let mut app = test_app();
        let workflow_id = create_workflow(&mut app, "ship-feature");
        app.test_bind_a_live_lead_run(&workflow_id, "ship-feature");

        assert!(app.open_workflow_runs());
        assert_eq!(app.state.view.workflow_runs.entries.len(), 1);
        let WorkflowRunsEntry::Run(run) = &app.state.view.workflow_runs.entries[0] else {
            panic!("expected a run row");
        };
        assert_eq!(run.workflow_name, "ship-feature");
        assert_eq!(run.status, RunStatus::Running);
    }

    #[test]
    fn refresh_no_ops_outside_the_browser_mode() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowDag;
        let before = app.state.view.workflow_runs.clone();

        app.refresh_workflow_runs_overlay(EventKind::WorkflowRunStarted);
        assert_eq!(app.state.view.workflow_runs, before);
        assert_eq!(app.state.mode, Mode::WorkflowDag);
    }

    #[test]
    fn refresh_no_ops_for_a_node_level_event() {
        let mut app = test_app();
        app.state.mode = Mode::WorkflowRuns;
        app.state.view.workflow_runs = state_with_geometry(vec![run_row("run:1")]);
        let before = app.state.view.workflow_runs.clone();

        app.refresh_workflow_runs_overlay(EventKind::WorkflowNodeCreated);
        assert_eq!(
            app.state.view.workflow_runs, before,
            "a node-level event cannot change a run-list row"
        );
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn refresh_on_a_run_level_event_reloads_and_reanchors_selection_by_run_id() {
        let mut app = test_app();
        let workflow_id = create_workflow(&mut app, "first");
        app.test_bind_a_live_lead_run(&workflow_id, "first");

        assert!(app.open_workflow_runs());
        assert_eq!(app.state.view.workflow_runs.selected, 0);

        app.refresh_workflow_runs_overlay(EventKind::WorkflowRunUpdated);
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

    /// The wiring phase C left undone: the DAG keybinding and the launcher's
    /// "watch what you just started" jump both asked the engine's mirrored run
    /// graph, which the lead path does not write, so both dead-ended while a
    /// team lead was live. They go through the store-backed snapshot now, the
    /// same one the run browser opens.
    #[cfg(feature = "workflow")]
    #[test]
    fn the_dag_opens_on_a_live_lead_run_through_the_stored_snapshot() {
        let mut app = test_app();
        let workflow_id = create_workflow(&mut app, "ship-feature");
        let run_id = app.test_bind_a_live_lead_run(&workflow_id, "ship-feature");

        assert!(
            app.state.workflow_run_graph().is_none(),
            "a lead run has no mirrored graph — that is what this covers"
        );
        assert!(app.open_workflow_dag_on_the_live_run());
        assert_eq!(app.state.mode, Mode::WorkflowDag);
        let snapshot = app
            .state
            .historical_run()
            .expect("the overlay is showing the live run");
        assert_eq!(snapshot.graph.run_id.as_str(), run_id.as_str());

        // With no lead live it answers `false` rather than opening an empty
        // overlay, which is what lets the keybinding fall through to the
        // launcher exactly as it did before.
        let mut empty = test_app();
        assert!(!empty.open_workflow_dag_on_the_live_run());
        assert_ne!(empty.state.mode, Mode::WorkflowDag);
    }

    /// **Known gap, not a passing test.** Between `workflow.run` spawning the
    /// lead's pane and the projection recognising its team, nothing on the run
    /// row says a team lead is executing it: `StoreWrite::RunLeadBinding` — the
    /// only writer of `team_name` and `lead_pane_id` — is issued from
    /// `bind_run_team`, which needs the team config to exist first
    /// (`09-agent-teams-rework.md` §3.1 step 4). `HistoricalRunSnapshot::
    /// is_lead_run` reads `team_name`, so for that window the overlay treats a
    /// lead run as an engine-era run and offers `s`/`i`/`Shift+I`, which the
    /// server then refuses as retired verbs.
    ///
    /// Closing it means recording the pane karvex itself launched at spawn
    /// time — karvex knows it launched a lead, it does not have to infer that —
    /// which is a second, smaller `StoreWrite` and a widened `is_lead_run`. It
    /// belongs with the bind-deadline work the rework audit files as WI-5,
    /// because both are about the same unbound window.
    #[cfg(feature = "workflow")]
    #[test]
    #[ignore = "gap: a lead run is only marked as one once its team binds, so the DAG offers retired verbs during the unbound window; needs the lead pane recorded at spawn (audit WI-5)"]
    fn a_lead_run_is_known_to_be_one_before_its_team_binds() {
        let mut app = test_app();
        let workflow_id = create_workflow(&mut app, "ship-feature");
        app.test_bind_a_live_lead_run(&workflow_id, "ship-feature");
        assert!(app.open_workflow_dag_on_the_live_run());

        let snapshot = app
            .state
            .historical_run()
            .expect("the overlay is showing the live run");
        assert!(
            snapshot.is_lead_run(),
            "karvex launched this lead itself; the record has to say so before \
             the team config shows up"
        );
    }

    /// The wiring the engine's removal broke: nothing published a run-level
    /// event any more, so the browser sat on whatever rows it had loaded for
    /// the whole life of a run. Cancelling is the cheapest run-level change to
    /// drive from a unit test, and it goes through the same
    /// `emit_workflow_run_event` funnel every other one does.
    #[cfg(feature = "workflow")]
    #[test]
    fn a_run_level_change_refreshes_the_open_browser_without_being_asked() {
        let mut app = test_app();
        let workflow_id = create_workflow(&mut app, "ship-feature");
        let run_id = app.test_bind_a_live_lead_run(&workflow_id, "ship-feature");

        assert!(app.open_workflow_runs());
        assert_eq!(
            entry_status(&app.state.view.workflow_runs.entries[0]),
            Some(RunStatus::Running)
        );

        app.dispatch_runtime_mutation(
            "test.workflow.run.cancel",
            Method::WorkflowRunCancel(crate::api::schema::WorkflowRunTarget {
                run_id: run_id.to_string(),
            }),
        );

        assert_eq!(
            entry_status(&app.state.view.workflow_runs.entries[0]),
            Some(RunStatus::Cancelled),
            "the open browser must show the cancel it was not told to re-read"
        );
    }

    #[cfg(feature = "workflow")]
    fn entry_status(entry: &WorkflowRunsEntry) -> Option<RunStatus> {
        match entry {
            WorkflowRunsEntry::Run(run) => Some(run.status),
            WorkflowRunsEntry::PrunedSummary(_) => None,
        }
    }
}
