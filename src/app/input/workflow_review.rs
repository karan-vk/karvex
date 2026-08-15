//! Input handling for the workflow review overlay
//! (`.local/prd/phase4-retarget-plan.md` §3.5, packets P2 and P13).
//!
//! P2 landed this as an inert stub — no `workflow.review.*` wire method
//! existed yet, so the only live behaviour was closing, through the same
//! `leave_modal` path every overlay in this family uses. P3/P10/P11 landed
//! the wire surface, the orchestration, and apply; this is P13's real
//! content.
//!
//! Split the way [`super::workflow_runs`] is: the **wire → state** half turns
//! a `workflow.review.get` answer into [`crate::app::state::WorkflowReviewState`]
//! — one pure mapping, so a test can assert what the overlay would show
//! without spinning up an `App` — and the **runtime** half (`impl App`)
//! dispatches the in-process wire calls and owns the two-step
//! toggle-then-confirm interaction, the same shape
//! [`super::workflow_runs::App::begin_workflow_runs_restore`] /
//! `submit_workflow_runs_restore` already established.
//!
//! **Attribution is never flattened.** A finding's `interview_mode` decides
//! whether [`attribution_text`] reads "an interview's own account" or
//! "evidence only" — the one rule every surface built on top of `review::
//! Attribution` must keep separate (`.local/prd/phase4-retarget-plan.md`:
//! "a finding derived without a live interview must never be presented as
//! the teammate's own words").
//!
//! **Acceptance is per node key, not per row.** `workflow.review.apply`'s
//! `accept` vocabulary is `node_key` (P11's `accepted_findings`): two
//! findings that happen to share a `node_key` toggle together, because that
//! is the actual granularity the compiler applies at.

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};

use super::modal::{leave_modal, modal_action_from_key, ModalAction, WORKFLOW_REVIEW_ACTIONS};
use crate::api::schema::{
    ErrorResponse, Method, ResponseResult, SuccessResponse, WorkflowReviewApplyParams,
    WorkflowReviewFindingInfo, WorkflowReviewInfo, WorkflowReviewInterviewMode,
    WorkflowReviewStatus, WorkflowRunTarget,
};
use crate::app::state::{
    AppState, Mode, WorkflowReviewConfirm, WorkflowReviewFindingRow, WorkflowReviewState,
};
use crate::app::App;
use crate::workflow::model::{NoticeLevel, RunId, UserNotice};

/// Inserts typed or pasted text into whatever text field the review overlay
/// ends up needing. No-op: accept/decline is a per-row toggle (`Space`), not
/// text entry, and the overlay has no other field to type into.
pub(crate) fn insert_workflow_review_text(_state: &mut AppState, _text: &str) -> bool {
    false
}

fn rect_contains(rect: ratatui::layout::Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

// ── wire → state mapping ────────────────────────────────────────────────────

/// `workflow.review.get`'s answer → the overlay's own vocabulary. The one
/// place a wire answer becomes [`WorkflowReviewState`], so every opener
/// (the DAG's `V`, the global `keys.open_workflow_review` keybind, a
/// `workflow.review.ready` notice) shows exactly the same thing for the same
/// cycle.
///
/// Findings are only ever projected while the cycle is `awaiting_user` — the
/// one status a human can act on. Every other status (no cycle yet, still
/// running, or already decided) carries an honest [`WorkflowReviewState::message`]
/// instead, the same spirit P2's stub placeholder had.
pub(crate) fn workflow_review_state_from(
    run_id: &str,
    review: Option<WorkflowReviewInfo>,
    findings: Vec<WorkflowReviewFindingInfo>,
) -> WorkflowReviewState {
    let Some(review) = review else {
        return WorkflowReviewState {
            run_id: run_id.to_string(),
            message: Some(
                "no review has ever run for this run — press V in the DAG view to start one"
                    .to_string(),
            ),
            ..WorkflowReviewState::default()
        };
    };

    let message = match review.status {
        WorkflowReviewStatus::AwaitingUser => None,
        WorkflowReviewStatus::Running => {
            Some("review is running — interviews are still in progress".to_string())
        }
        WorkflowReviewStatus::Applied => Some(
            "review applied — its accepted findings are already folded into a new version"
                .to_string(),
        ),
        WorkflowReviewStatus::Declined => Some("review declined — nothing was changed".to_string()),
        WorkflowReviewStatus::Failed => Some("review failed before it could finish".to_string()),
    };

    let rows = if review.status == WorkflowReviewStatus::AwaitingUser {
        findings.into_iter().map(finding_row).collect()
    } else {
        Vec::new()
    };

    WorkflowReviewState {
        run_id: run_id.to_string(),
        status: Some(review.status),
        evidence_only_count: review.evidence_only_count,
        findings: rows,
        message,
        ..WorkflowReviewState::default()
    }
}

/// One finding, as the wire answered it → one list/detail row.
///
/// `accept` always starts `false`: nothing is applied unless the human
/// explicitly says so, row by row (`Space`) — the wire's own `accepted`
/// field means "already folded into a compiled version", which cannot be
/// true for a cycle that is still `awaiting_user`.
fn finding_row(finding: WorkflowReviewFindingInfo) -> WorkflowReviewFindingRow {
    WorkflowReviewFindingRow {
        node_key: finding.node_key.clone(),
        run_path: finding.run_path.clone(),
        interview_mode: finding.interview_mode,
        level: finding.level,
        verdict: finding.verdict,
        rationale: finding.rationale.clone(),
        attribution: attribution_text(&finding),
        evidence_summary: evidence_summary_text(&finding.evidence),
        proposed_change_summary: proposed_change_summary_text(&finding),
        accept: false,
    }
}

/// Who this finding is attributed to and how — the one sentence every
/// surface built on it must show verbatim rather than re-deriving, so an
/// `evidence_only` finding can never drift into reading like a resumed
/// interview's own words.
///
/// `finding.interview_mode` is the store's own typed column and is the
/// authority; `evidence.attribution.{member,reason}` (P5's `finding_seed`
/// shape) supplies the human-readable extras when they are present and
/// degrades gracefully — never silently — when they are not.
fn attribution_text(finding: &WorkflowReviewFindingInfo) -> String {
    let attribution = finding.evidence.get("attribution");
    let member = attribution
        .and_then(|value| value.get("member"))
        .and_then(|value| value.as_str());
    match finding.interview_mode {
        WorkflowReviewInterviewMode::Resumed => match member {
            Some(name) => format!("{name}'s own account, from a resumed interview"),
            None => "a resumed interview's own account".to_string(),
        },
        WorkflowReviewInterviewMode::EvidenceOnly => {
            let reason = attribution
                .and_then(|value| value.get("reason"))
                .and_then(|value| value.as_str());
            match reason {
                Some(reason) => format!(
                    "evidence only, never the teammate's own words — {}",
                    evidence_only_reason_sentence(reason)
                ),
                None => {
                    "evidence only, never the teammate's own words — no interview was conducted"
                        .to_string()
                }
            }
        }
    }
}

/// [`crate::workflow::review::EvidenceOnlyReason`]'s wire strings, read back
/// into a sentence. A deliberate small copy rather than a dependency on that
/// pure module's own `sentence()` — this file is the client vocabulary for a
/// wire string, not a second definition of the reason itself, and an
/// unrecognised string still degrades to something true rather than being
/// hidden.
fn evidence_only_reason_sentence(reason: &str) -> &'static str {
    match reason {
        "no_session_id" => "karvex never captured a claude session id for this member",
        "transcript_unreadable" => "the member's claude transcript is no longer readable",
        "interview_blocked" => {
            "the interview pane stopped on a permission prompt and never got past it"
        }
        "interview_timed_out" => "the interview pane did not produce a usable answer in time",
        "interview_pane_gone" => "the interview pane exited before it answered",
        _ => "no live interview was available",
    }
}

/// `evidence.reported` (P5's `finding_seed` shape) → a compact one-line read,
/// never the raw JSON. The measured facts put to the teammate (or reasoned
/// from durable evidence alone), key by key.
fn evidence_summary_text(evidence: &serde_json::Value) -> String {
    let reported = evidence.get("reported").unwrap_or(evidence);
    match reported.as_object() {
        Some(map) if !map.is_empty() => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            entries
                .into_iter()
                .map(|(key, value)| format!("{key}: {}", compact_json_value(value)))
                .collect::<Vec<_>>()
                .join(" · ")
        }
        _ => "(no evidence reported)".to_string(),
    }
}

/// The concrete change a finding proposes, compacted the same way
/// [`evidence_summary_text`] is. `replace` names its own replacement
/// definition rather than dumping it: the full document belongs in
/// `workflow.review.get`'s JSON, not in a one-line summary.
fn proposed_change_summary_text(finding: &WorkflowReviewFindingInfo) -> String {
    use crate::api::schema::WorkflowReviewVerdict;
    if finding.verdict == WorkflowReviewVerdict::Keep {
        return "keep as-is — no change proposed".to_string();
    }
    if finding.replacement.is_some() {
        return "replace — a full replacement role definition is attached".to_string();
    }
    match finding.proposed_change.as_object() {
        Some(map) if !map.is_empty() => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            entries
                .into_iter()
                .map(|(key, value)| format!("{key}: {}", compact_json_value(value)))
                .collect::<Vec<_>>()
                .join(" · ")
        }
        _ => "(no change detail provided)".to_string(),
    }
}

fn compact_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
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

// ── runtime half (`impl App`) ───────────────────────────────────────────────

impl App {
    /// Opens the review overlay with real content
    /// (`.local/prd/phase4-retarget-plan.md` §3.5, packet P13).
    ///
    /// Always opens — like the P2 stub, a bound key that sometimes dead-ends
    /// is indistinguishable from a broken one — but the run it checks is now
    /// resolved rather than guessed: the DAG's currently open run when one
    /// is open, else the live lead run, else there is nothing to check and
    /// the overlay says so honestly.
    pub(crate) fn open_workflow_review(&mut self) {
        let run_id = self
            .state
            .historical_run()
            .map(|snapshot| snapshot.graph.run_id.to_string())
            .or_else(|| self.live_lead_run_id());

        self.state.view.workflow_review = match run_id {
            Some(run_id) => self.load_workflow_review(&run_id),
            None => WorkflowReviewState {
                message: Some("no workflow run on this server to review".to_string()),
                ..WorkflowReviewState::default()
            },
        };
        self.state.mode = Mode::WorkflowReview;
    }

    /// `workflow.review.get` for `run_id`, projected through
    /// [`workflow_review_state_from`] — the one place a wire answer becomes
    /// the overlay's state, shared by every opener.
    fn load_workflow_review(&mut self, run_id: &str) -> WorkflowReviewState {
        let response = self.dispatch_api_request(
            "tui.workflow.review.get",
            Method::WorkflowReviewGet(WorkflowRunTarget {
                run_id: run_id.to_string(),
            }),
        );
        let (review, findings) = match success_result(&response) {
            Some(ResponseResult::WorkflowReviewGet { review, findings }) => (review, findings),
            _ => (None, Vec::new()),
        };
        workflow_review_state_from(run_id, review, findings)
    }

    pub(crate) fn handle_workflow_review_key(&mut self, key: KeyEvent) {
        if self.state.view.workflow_review.confirm.is_some() {
            match key.code {
                KeyCode::Enter => self.submit_workflow_review_confirm(),
                KeyCode::Esc => self.state.view.workflow_review.confirm = None,
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.move_workflow_review_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_workflow_review_selection(-1),
            KeyCode::Char(' ') => self.toggle_workflow_review_accept(),
            KeyCode::Enter => self.begin_workflow_review_apply(),
            KeyCode::Char('d') | KeyCode::Char('D') => self.begin_workflow_review_decline_all(),
            _ => {
                if let Some(ModalAction::Close) =
                    modal_action_from_key(&key, WORKFLOW_REVIEW_ACTIONS)
                {
                    leave_modal(&mut self.state);
                }
            }
        }
    }

    /// Mouse handling for the review overlay: a click on a row selects it, a
    /// click outside the modal closes it — the confirm sub-state answers
    /// only to keys, the same rule [`super::workflow_runs`]'s restore confirm
    /// follows.
    pub(super) fn handle_workflow_review_mouse(&mut self, mouse: MouseEvent) -> bool {
        if self.state.view.workflow_review.confirm.is_some() {
            return true;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.move_workflow_review_selection(-1),
            MouseEventKind::ScrollDown => self.move_workflow_review_selection(1),
            MouseEventKind::Down(MouseButton::Left) => {
                self.click_workflow_review(mouse.column, mouse.row)
            }
            _ => {}
        }
        true
    }

    fn click_workflow_review(&mut self, column: u16, row: u16) {
        let review = &self.state.view.workflow_review;
        let Some(index) = review
            .row_rects
            .iter()
            .position(|rect| rect_contains(*rect, column, row))
        else {
            if !rect_contains(review.modal_rect, column, row) {
                leave_modal(&mut self.state);
            }
            return;
        };
        self.state.view.workflow_review.selected = index;
    }

    fn move_workflow_review_selection(&mut self, delta: isize) {
        let review = &mut self.state.view.workflow_review;
        if review.findings.is_empty() {
            return;
        }
        let len = review.findings.len() as isize;
        let current = review.selected.min(review.findings.len() - 1) as isize;
        let next = (current + delta).rem_euclid(len) as usize;
        review.selected = next;
    }

    /// `Space`: toggles the selected finding's pending accept state.
    ///
    /// Every other finding sharing its `node_key` toggles with it —
    /// `workflow.review.apply`'s `accept` vocabulary is the node key, not a
    /// per-row id (P11's `accepted_findings`), so two findings for one key
    /// can never be half-accepted on the wire and this keeps the overlay
    /// from promising a granularity the apply call does not have.
    fn toggle_workflow_review_accept(&mut self) {
        let review = &mut self.state.view.workflow_review;
        let Some(selected) = review.findings.get(review.selected) else {
            return;
        };
        let node_key = selected.node_key.clone();
        let next = !selected.accept;
        for finding in &mut review.findings {
            if finding.node_key == node_key {
                finding.accept = next;
            }
        }
    }

    /// `Enter`: proposes applying whatever is currently toggled on. A no-op
    /// when the cycle is not `awaiting_user` — nothing loaded means nothing
    /// to decide, and the confirm step must never open onto an empty apply
    /// target.
    fn begin_workflow_review_apply(&mut self) {
        let review = &mut self.state.view.workflow_review;
        if review.status != Some(WorkflowReviewStatus::AwaitingUser) {
            return;
        }
        review.confirm = Some(WorkflowReviewConfirm::Apply);
    }

    /// `d`: proposes declining the whole cycle, regardless of any row's
    /// toggle. Same "awaiting_user only" guard as `Enter`.
    fn begin_workflow_review_decline_all(&mut self) {
        let review = &mut self.state.view.workflow_review;
        if review.status != Some(WorkflowReviewStatus::AwaitingUser) {
            return;
        }
        review.confirm = Some(WorkflowReviewConfirm::DeclineAll);
    }

    /// `Enter` on an open confirm: calls `workflow.review.apply`. Every
    /// outcome closes the confirm; success also closes the whole overlay —
    /// applying (or declining) a cycle is a decision, not a state to keep
    /// browsing — and refreshes the DAG's own mirror of the review status so
    /// a stale "review ready" header segment cannot survive its own
    /// decision. A refusal leaves the overlay open with the findings
    /// reloaded, so a concurrent change (e.g. the cycle was already decided
    /// elsewhere) is visible rather than silently retried.
    fn submit_workflow_review_confirm(&mut self) {
        let Some(confirm) = self.state.view.workflow_review.confirm.take() else {
            return;
        };
        let run_id = self.state.view.workflow_review.run_id.clone();
        let accept: Vec<String> = match confirm {
            WorkflowReviewConfirm::Apply => self
                .state
                .view
                .workflow_review
                .findings
                .iter()
                .filter(|finding| finding.accept)
                .map(|finding| finding.node_key.clone())
                .collect(),
            WorkflowReviewConfirm::DeclineAll => Vec::new(),
        };
        let accepted_count = accept.len();

        let response = self.dispatch_api_request(
            "tui.workflow.review.apply",
            Method::WorkflowReviewApply(WorkflowReviewApplyParams {
                run_id: run_id.clone(),
                accept,
            }),
        );

        match success_result(&response) {
            Some(ResponseResult::WorkflowReviewApplied { version_id, .. }) => {
                let message = match version_id {
                    Some(version_id) => format!(
                        "review applied: {accepted_count} finding(s) folded into {version_id}"
                    ),
                    None => "review declined: nothing was changed".to_string(),
                };
                self.refresh_open_dag_review(&run_id);
                leave_modal(&mut self.state);
                self.show_workflow_notice(UserNotice {
                    level: NoticeLevel::Info,
                    run: Some(RunId::new(run_id)),
                    path: None,
                    message,
                });
            }
            _ => {
                let message = error_message(&response)
                    .unwrap_or_else(|| "could not apply this review cycle".to_string());
                self.show_workflow_notice(UserNotice {
                    level: NoticeLevel::Warning,
                    run: Some(RunId::new(run_id.clone())),
                    path: None,
                    message,
                });
                // The refusal may mean the cycle moved under us (already
                // decided elsewhere, or no longer exists) — reload rather
                // than leave a stale findings list on screen with a confirm
                // that can no longer be trusted.
                self.state.view.workflow_review = self.load_workflow_review(&run_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyModifiers};

    use super::*;
    use crate::api::schema::{
        WorkflowReviewFindingLevel, WorkflowReviewInterviewMode, WorkflowReviewVerdict,
    };
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

    fn app_with_findings(findings: Vec<WorkflowReviewFindingRow>) -> App {
        let mut app = app_with_review_open();
        app.state.view.workflow_review = WorkflowReviewState {
            run_id: "workflow_run:1".to_string(),
            status: Some(WorkflowReviewStatus::AwaitingUser),
            findings,
            ..WorkflowReviewState::default()
        };
        app
    }

    // ── mode round-trip ──────────────────────────────────────────────────

    #[test]
    fn esc_leaves_the_overlay_through_the_shared_modal_exit() {
        let mut app = app_with_review_open();
        app.handle_workflow_review_key(key(KeyCode::Esc));
        assert_ne!(
            app.state.mode,
            Mode::WorkflowReview,
            "esc must never trap input in the overlay"
        );
    }

    #[test]
    fn esc_with_no_decision_leaves_the_cycle_awaiting_user() {
        // The whole point of Esc closing without a confirm: it never calls
        // `workflow.review.apply`, so a real server would leave the cycle
        // exactly where it was. Here that is `status`, unchanged.
        let mut app = app_with_findings(vec![finding("plan")]);
        app.handle_workflow_review_key(key(KeyCode::Esc));
        assert_ne!(app.state.mode, Mode::WorkflowReview);
    }

    #[test]
    fn other_keys_do_not_close_the_overlay() {
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

    // ── the toggle state machine (pure key-handler tests) ──────────────────

    #[test]
    fn space_toggles_the_selected_finding_on_and_off() {
        let mut app = app_with_findings(vec![finding("plan"), finding("build")]);
        app.handle_workflow_review_key(key(KeyCode::Char(' ')));
        assert!(app.state.view.workflow_review.findings[0].accept);
        assert!(!app.state.view.workflow_review.findings[1].accept);

        app.handle_workflow_review_key(key(KeyCode::Char(' ')));
        assert!(!app.state.view.workflow_review.findings[0].accept);
    }

    #[test]
    fn toggling_one_row_toggles_every_row_sharing_its_node_key() {
        // Two findings for the same node key can only ever be accepted or
        // declined together — `workflow.review.apply`'s vocabulary is the
        // node key, not a per-row id.
        let mut app = app_with_findings(vec![finding("plan"), finding("plan")]);
        app.handle_workflow_review_key(key(KeyCode::Char(' ')));
        assert!(app.state.view.workflow_review.findings[0].accept);
        assert!(app.state.view.workflow_review.findings[1].accept);
    }

    #[test]
    fn j_and_k_move_the_selection_and_wrap() {
        let mut app = app_with_findings(vec![finding("plan"), finding("build")]);
        assert_eq!(app.state.view.workflow_review.selected, 0);
        app.handle_workflow_review_key(key(KeyCode::Char('j')));
        assert_eq!(app.state.view.workflow_review.selected, 1);
        app.handle_workflow_review_key(key(KeyCode::Char('j')));
        assert_eq!(app.state.view.workflow_review.selected, 0, "wraps around");
        app.handle_workflow_review_key(key(KeyCode::Char('k')));
        assert_eq!(
            app.state.view.workflow_review.selected, 1,
            "wraps backward too"
        );
    }

    #[test]
    fn enter_opens_an_apply_confirm_with_the_awaiting_findings() {
        let mut app = app_with_findings(vec![finding("plan")]);
        app.handle_workflow_review_key(key(KeyCode::Enter));
        assert_eq!(
            app.state.view.workflow_review.confirm,
            Some(WorkflowReviewConfirm::Apply)
        );
    }

    #[test]
    fn d_opens_a_decline_all_confirm() {
        let mut app = app_with_findings(vec![finding("plan")]);
        app.handle_workflow_review_key(key(KeyCode::Char('d')));
        assert_eq!(
            app.state.view.workflow_review.confirm,
            Some(WorkflowReviewConfirm::DeclineAll)
        );
    }

    #[test]
    fn esc_on_an_open_confirm_cancels_it_without_leaving_the_overlay() {
        let mut app = app_with_findings(vec![finding("plan")]);
        app.handle_workflow_review_key(key(KeyCode::Enter));
        assert!(app.state.view.workflow_review.confirm.is_some());
        app.handle_workflow_review_key(key(KeyCode::Esc));
        assert!(app.state.view.workflow_review.confirm.is_none());
        assert_eq!(app.state.mode, Mode::WorkflowReview);
    }

    // ── a closed cycle maps to no apply target ──────────────────────────────

    #[test]
    fn a_cycle_that_is_not_awaiting_user_offers_no_confirm() {
        for status in [
            WorkflowReviewStatus::Running,
            WorkflowReviewStatus::Applied,
            WorkflowReviewStatus::Declined,
            WorkflowReviewStatus::Failed,
        ] {
            let mut app = app_with_review_open();
            app.state.view.workflow_review = WorkflowReviewState {
                run_id: "workflow_run:1".to_string(),
                status: Some(status),
                message: Some("nothing to decide".to_string()),
                ..WorkflowReviewState::default()
            };
            app.handle_workflow_review_key(key(KeyCode::Enter));
            assert_eq!(
                app.state.view.workflow_review.confirm, None,
                "{status:?} must never open an apply confirm"
            );
            app.handle_workflow_review_key(key(KeyCode::Char('d')));
            assert_eq!(
                app.state.view.workflow_review.confirm, None,
                "{status:?} must never open a decline confirm either"
            );
        }
    }

    #[test]
    fn no_cycle_at_all_offers_no_confirm() {
        let mut app = app_with_review_open();
        app.state.view.workflow_review = WorkflowReviewState {
            run_id: "workflow_run:1".to_string(),
            status: None,
            message: Some("no review has ever run for this run".to_string()),
            ..WorkflowReviewState::default()
        };
        app.handle_workflow_review_key(key(KeyCode::Enter));
        assert!(app.state.view.workflow_review.confirm.is_none());
    }

    // ── wire → state mapping ─────────────────────────────────────────────

    fn wire_finding(node_key: &str) -> WorkflowReviewFindingInfo {
        WorkflowReviewFindingInfo {
            node_key: node_key.to_string(),
            run_path: Some(format!(".{node_key}")),
            interrogation_id: Some("interrogation:1".to_string()),
            interview_mode: WorkflowReviewInterviewMode::Resumed,
            level: WorkflowReviewFindingLevel::Prompt,
            verdict: WorkflowReviewVerdict::Improve,
            rationale: "drifted from the role".to_string(),
            evidence: serde_json::json!({
                "reported": {"idle_while_in_progress_ms": 900_000},
                "attribution": {"member": "verify", "interview_mode": "resumed"},
            }),
            proposed_change: serde_json::json!({"prompt": "tightened the scope"}),
            replacement: None,
            accepted: false,
            applied_in_version: None,
        }
    }

    #[test]
    fn no_review_ever_run_is_an_honest_message_not_a_blank_screen() {
        let state = workflow_review_state_from("workflow_run:1", None, Vec::new());
        assert!(state.message.is_some());
        assert!(state.findings.is_empty());
        assert_eq!(state.status, None);
    }

    #[test]
    fn awaiting_user_projects_findings_with_visible_attribution() {
        let review = WorkflowReviewInfo {
            id: "review_cycle:1".to_string(),
            run_id: "workflow_run:1".to_string(),
            workflow_id: "workflow:1".to_string(),
            version_id: "kvdag_version:1".to_string(),
            status: WorkflowReviewStatus::AwaitingUser,
            started_at_unix_ms: 1,
            ended_at_unix_ms: None,
            resulting_version_id: None,
            interview_paths: Vec::new(),
            evidence_only_count: 1,
        };
        let state =
            workflow_review_state_from("workflow_run:1", Some(review), vec![wire_finding("plan")]);
        assert_eq!(state.message, None);
        assert_eq!(state.evidence_only_count, 1);
        assert_eq!(state.findings.len(), 1);
        assert!(!state.findings[0].accept, "nothing is pre-accepted");
        assert!(
            state.findings[0].attribution.contains("verify"),
            "the member is visible: {}",
            state.findings[0].attribution
        );
    }

    #[test]
    fn a_running_cycle_carries_no_findings_to_decide() {
        let review = WorkflowReviewInfo {
            id: "review_cycle:1".to_string(),
            run_id: "workflow_run:1".to_string(),
            workflow_id: "workflow:1".to_string(),
            version_id: "kvdag_version:1".to_string(),
            status: WorkflowReviewStatus::Running,
            started_at_unix_ms: 1,
            ended_at_unix_ms: None,
            resulting_version_id: None,
            interview_paths: Vec::new(),
            evidence_only_count: 0,
        };
        // A running cycle should not even hand the overlay findings — there
        // is nothing to decide until synthesis reports.
        let state =
            workflow_review_state_from("workflow_run:1", Some(review), vec![wire_finding("plan")]);
        assert!(state.findings.is_empty());
        assert!(state.message.is_some());
    }

    #[test]
    fn evidence_only_never_reads_as_the_teammates_own_words() {
        let mut wire = wire_finding("plan");
        wire.interview_mode = WorkflowReviewInterviewMode::EvidenceOnly;
        wire.evidence = serde_json::json!({
            "reported": {"idle_while_in_progress_ms": 900_000},
            "attribution": {"member": null, "interview_mode": "evidence_only", "reason": "no_session_id"},
        });
        let row = finding_row(wire);
        assert!(
            row.attribution.starts_with("evidence only"),
            "must lead with the honesty label: {}",
            row.attribution
        );
        assert!(!row.attribution.contains("own account"));
    }
}
