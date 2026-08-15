//! `workflow.review.{start,get,answer,report}` handlers
//! (`.local/prd/phase4-retarget-plan.md` §5 packet P3).
//!
//! `workflow.review.apply` lives in `workflow_review_apply.rs` — the same
//! split wave 2b's real implementation uses (§3.6 module map), so filling
//! either method in later work never touches the other's file.
//!
//! **This packet lands the wire surface only.** Every handler here is a
//! stub: it validates its params exactly as the real handler will, then
//! answers [`WORKFLOW_REVIEW_NOT_FOUND_CODE`] — never the dispatcher's
//! `not_implemented` catch-all, so a client that calls one of these methods
//! today learns "no review cycle exists" rather than "this method does not
//! exist" or "karvex is broken". The orchestration itself (`app/
//! workflow_review.rs`, the interview/synthesis panes, the store rows) is
//! wave 2b's (`.local/prd/phase4-retarget-plan.md` §5 Wave 2b).

use crate::api::schema::{
    WorkflowReviewAnswerParams, WorkflowReviewReportParams, WorkflowRunTarget,
};
use crate::app::App;

use super::responses::encode_error;

/// No review cycle answers this request: either this run has never had one
/// (`review.get`), or there is nothing for `review.start`/`review.answer`/
/// `review.report` to act on yet because the orchestration that creates and
/// advances cycles (wave 2b) has not landed. Its own code rather than a bare
/// `not_found`, matching [`super::workflows`]'s `workflow_node_verb_retired`
/// precedent: a client can tell "no review cycle" from "no such run" or "no
/// such method".
///
/// Only the feature-on stub produces this — the feature-off build answers
/// `workflow_unavailable` instead (`every_workflow_method_reports_
/// workflow_unavailable_with_the_feature_off`), so a `--no-default-features`
/// build never references it.
#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
pub(crate) const WORKFLOW_REVIEW_NOT_FOUND_CODE: &str = "workflow_review_not_found";
/// A review cycle is already `running` or `awaiting_user` for this run
/// (`.local/prd/phase4-retarget-plan.md` §3.5, §6 D-4: a review does **not**
/// block a new run, but a run has at most one live review cycle at a time).
///
/// Frozen here as part of P3's wire surface, ahead of the precondition check
/// that produces it: `handle_workflow_review_start` is a stub until wave 2b
/// (module doc), so this code is not reachable from any request yet. Not a
/// dead knob in the WI-R2 sense — its writer is roadmapped in the same phase,
/// not abandoned — but genuinely unused Rust until then.
#[allow(dead_code)] // wave-0 wire shape; wave 2b's precondition check produces this
pub(crate) const WORKFLOW_REVIEW_IN_FLIGHT_CODE: &str = "workflow_review_in_flight";
/// `workflow.review.apply` addressed a cycle that is not `awaiting_user`
/// (`workflow_review_apply.rs`). Same wave-0/wave-2b split as
/// [`WORKFLOW_REVIEW_IN_FLIGHT_CODE`].
#[allow(dead_code)] // wave-0 wire shape; wave 2b's precondition check produces this
pub(crate) const WORKFLOW_REVIEW_NOT_AWAITING_CODE: &str = "workflow_review_not_awaiting";
/// `workflow.review.start` on a run with zero interviewable members: no
/// member (or the lead) has a captured session id and a readable transcript,
/// so there is nothing to rank or interview
/// (`.local/prd/phase4-retarget-plan.md` §3.5). Its own code rather than
/// folding into `workflow_review_not_found`, because the run and its history
/// both exist — there is simply nobody left to ask. Same wave-0/wave-2b split
/// as [`WORKFLOW_REVIEW_IN_FLIGHT_CODE`].
#[allow(dead_code)] // wave-0 wire shape; wave 2b's precondition check produces this
pub(crate) const WORKFLOW_REVIEW_NO_INTERVIEWABLE_MEMBERS_CODE: &str =
    "workflow_review_no_interviewable_members";

/// The feature-off answer (`--no-default-features`), matching
/// [`super::workflows`]'s `workflow_unavailable` precedent. Kept as its own
/// copy rather than a shared helper: `workflows.rs`'s copy is private to that
/// module, and duplicating two string constants is cheaper than widening its
/// visibility for a wave-0 stub.
#[cfg(not(feature = "workflow"))]
const WORKFLOW_UNAVAILABLE_CODE: &str = "workflow_unavailable";
#[cfg(not(feature = "workflow"))]
const WORKFLOW_UNAVAILABLE_MESSAGE: &str =
    "the workflow feature is not compiled into this server (built with --no-default-features);      rebuild with --features workflow";

fn require_non_empty(id: &str, field: &str, value: &str) -> Option<String> {
    if value.trim().is_empty() {
        Some(encode_error(
            id.to_string(),
            "invalid_params",
            format!("{field} must not be empty"),
        ))
    } else {
        None
    }
}

#[cfg(not(feature = "workflow"))]
impl App {
    pub(super) fn handle_workflow_review_start(
        &mut self,
        id: String,
        target: WorkflowRunTarget,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &target.run_id) {
            return error;
        }
        encode_error(id, WORKFLOW_UNAVAILABLE_CODE, WORKFLOW_UNAVAILABLE_MESSAGE)
    }

    pub(super) fn handle_workflow_review_get(
        &mut self,
        id: String,
        target: WorkflowRunTarget,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &target.run_id) {
            return error;
        }
        encode_error(id, WORKFLOW_UNAVAILABLE_CODE, WORKFLOW_UNAVAILABLE_MESSAGE)
    }

    pub(super) fn handle_workflow_review_answer(
        &mut self,
        id: String,
        params: WorkflowReviewAnswerParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &params.run_id) {
            return error;
        }
        if let Some(error) = require_non_empty(&id, "member", &params.member) {
            return error;
        }
        encode_error(id, WORKFLOW_UNAVAILABLE_CODE, WORKFLOW_UNAVAILABLE_MESSAGE)
    }

    pub(super) fn handle_workflow_review_report(
        &mut self,
        id: String,
        params: WorkflowReviewReportParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &params.run_id) {
            return error;
        }
        encode_error(id, WORKFLOW_UNAVAILABLE_CODE, WORKFLOW_UNAVAILABLE_MESSAGE)
    }
}

#[cfg(feature = "workflow")]
impl App {
    /// Wave-0 stub (see module doc). Wave 2b replaces this body with the real
    /// plan/spawn orchestration; the signature and error surface are frozen
    /// here so it does not change again underneath the CLI/TUI callers this
    /// packet's siblings (P2, P12, P13) build against.
    pub(super) fn handle_workflow_review_start(
        &mut self,
        id: String,
        target: WorkflowRunTarget,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &target.run_id) {
            return error;
        }
        encode_error(
            id,
            WORKFLOW_REVIEW_NOT_FOUND_CODE,
            "no review cycle exists for this run",
        )
    }

    pub(super) fn handle_workflow_review_get(
        &mut self,
        id: String,
        target: WorkflowRunTarget,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &target.run_id) {
            return error;
        }
        encode_error(
            id,
            WORKFLOW_REVIEW_NOT_FOUND_CODE,
            "no review cycle exists for this run",
        )
    }

    pub(super) fn handle_workflow_review_answer(
        &mut self,
        id: String,
        params: WorkflowReviewAnswerParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &params.run_id) {
            return error;
        }
        if let Some(error) = require_non_empty(&id, "member", &params.member) {
            return error;
        }
        if params.answer.is_none() && params.answer_file.is_none() {
            return encode_error(
                id,
                "invalid_params",
                "exactly one of answer or answer_file is required",
            );
        }
        encode_error(
            id,
            WORKFLOW_REVIEW_NOT_FOUND_CODE,
            "no review cycle exists for this run",
        )
    }

    pub(super) fn handle_workflow_review_report(
        &mut self,
        id: String,
        params: WorkflowReviewReportParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &params.run_id) {
            return error;
        }
        if params.findings.is_none() && params.findings_file.is_none() {
            return encode_error(
                id,
                "invalid_params",
                "exactly one of findings or findings_file is required",
            );
        }
        encode_error(
            id,
            WORKFLOW_REVIEW_NOT_FOUND_CODE,
            "no review cycle exists for this run",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        #[cfg_attr(not(feature = "workflow"), allow(unused_mut))]
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        // Never let a unit test open — or lock — the user's real workflow
        // database. With the feature off there is no store to redirect.
        #[cfg(feature = "workflow")]
        {
            app.workflow_store = crate::app::workflow_store::WorkflowStoreHandle::in_memory();
        }
        app
    }

    fn error_code(response: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(response).unwrap();
        value["error"]["code"].as_str().unwrap().to_string()
    }

    #[test]
    fn empty_run_id_is_rejected_by_review_start() {
        let mut app = app();
        let response = app.handle_workflow_review_start(
            "req".into(),
            WorkflowRunTarget {
                run_id: String::new(),
            },
        );
        assert_eq!(error_code(&response), "invalid_params");
    }

    #[test]
    fn empty_run_id_is_rejected_by_review_get() {
        let mut app = app();
        let response = app.handle_workflow_review_get(
            "req".into(),
            WorkflowRunTarget {
                run_id: String::new(),
            },
        );
        assert_eq!(error_code(&response), "invalid_params");
    }

    // The feature-off handlers only ever answer `workflow_unavailable`
    // (`handle_workflow_run_finish`'s precedent, `workflows.rs:1049`) — the
    // "exactly one form" business validation is feature-on only, so these two
    // tests are too.
    #[cfg(feature = "workflow")]
    #[test]
    fn an_answer_with_neither_form_is_rejected() {
        let mut app = app();
        let response = app.handle_workflow_review_answer(
            "req".into(),
            WorkflowReviewAnswerParams {
                run_id: "workflow_run:1".into(),
                member: "research".into(),
                answer: None,
                answer_file: None,
            },
        );
        assert_eq!(error_code(&response), "invalid_params");
    }

    #[cfg(feature = "workflow")]
    #[test]
    fn a_report_with_neither_form_is_rejected() {
        let mut app = app();
        let response = app.handle_workflow_review_report(
            "req".into(),
            WorkflowReviewReportParams {
                run_id: "workflow_run:1".into(),
                findings: None,
                findings_file: None,
            },
        );
        assert_eq!(error_code(&response), "invalid_params");
    }

    /// Every method this file owns is a documented stub, never the
    /// dispatcher's `not_implemented` catch-all (`.local/prd/
    /// phase4-retarget-plan.md` §5 packet P3 contract).
    #[cfg(feature = "workflow")]
    #[test]
    fn valid_requests_answer_review_not_found_rather_than_not_implemented() {
        let mut app = app();
        let start = app.handle_workflow_review_start(
            "req".into(),
            WorkflowRunTarget {
                run_id: "workflow_run:1".into(),
            },
        );
        assert_eq!(error_code(&start), WORKFLOW_REVIEW_NOT_FOUND_CODE);

        let get = app.handle_workflow_review_get(
            "req".into(),
            WorkflowRunTarget {
                run_id: "workflow_run:1".into(),
            },
        );
        assert_eq!(error_code(&get), WORKFLOW_REVIEW_NOT_FOUND_CODE);

        let answer = app.handle_workflow_review_answer(
            "req".into(),
            WorkflowReviewAnswerParams {
                run_id: "workflow_run:1".into(),
                member: "research".into(),
                answer: Some(serde_json::json!({"account": "done"})),
                answer_file: None,
            },
        );
        assert_eq!(error_code(&answer), WORKFLOW_REVIEW_NOT_FOUND_CODE);

        let report = app.handle_workflow_review_report(
            "req".into(),
            WorkflowReviewReportParams {
                run_id: "workflow_run:1".into(),
                findings: Some(serde_json::json!([])),
                findings_file: None,
            },
        );
        assert_eq!(error_code(&report), WORKFLOW_REVIEW_NOT_FOUND_CODE);
    }
}
