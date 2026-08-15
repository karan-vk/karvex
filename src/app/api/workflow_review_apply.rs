//! `workflow.review.apply` handler
//! (`.local/prd/phase4-retarget-plan.md` §5 packet P3).
//!
//! Split from `workflow_review.rs` on purpose — matching wave 2b's real
//! implementation split (§3.6 module map): apply compiles accepted findings
//! into a new `kvdag_version` and flips the run's head, which is a
//! meaningfully different (and store-only, §6 D-13) concern from the four
//! methods that plan, spawn, and self-report a cycle.
//!
//! **This packet lands the wire surface only** — see `workflow_review.rs`'s
//! module doc for the stub contract this handler follows.

use crate::api::schema::WorkflowReviewApplyParams;
use crate::app::App;

use super::responses::encode_error;
#[cfg(feature = "workflow")]
use super::workflow_review::WORKFLOW_REVIEW_NOT_FOUND_CODE;

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
    pub(super) fn handle_workflow_review_apply(
        &mut self,
        id: String,
        params: WorkflowReviewApplyParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &params.run_id) {
            return error;
        }
        encode_error(id, WORKFLOW_UNAVAILABLE_CODE, WORKFLOW_UNAVAILABLE_MESSAGE)
    }
}

#[cfg(feature = "workflow")]
impl App {
    /// Wave-0 stub — see `workflow_review.rs`'s module doc. Wave 2b's real
    /// body: cycle must be `awaiting_user` (`workflow_review_not_awaiting`),
    /// compile accepted findings against the run's `kvdag`, mint the version,
    /// flip head, `finding_mark_applied`, `ReviewCycleUpdate{Applied}`.
    pub(super) fn handle_workflow_review_apply(
        &mut self,
        id: String,
        params: WorkflowReviewApplyParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &params.run_id) {
            return error;
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
    fn empty_run_id_is_rejected() {
        let mut app = app();
        let response = app.handle_workflow_review_apply(
            "req".into(),
            WorkflowReviewApplyParams {
                run_id: String::new(),
                accept: Vec::new(),
            },
        );
        assert_eq!(error_code(&response), "invalid_params");
    }

    /// An empty `accept` is a valid request (declines the whole cycle,
    /// `.local/prd/phase4-retarget-plan.md` §5 packet P3 delivers table) —
    /// the stub still answers `workflow_review_not_found` rather than
    /// `invalid_params` or `not_implemented`.
    #[cfg(feature = "workflow")]
    #[test]
    fn an_empty_accept_reaches_the_stub_answer_rather_than_being_rejected() {
        let mut app = app();
        let response = app.handle_workflow_review_apply(
            "req".into(),
            WorkflowReviewApplyParams {
                run_id: "workflow_run:1".into(),
                accept: Vec::new(),
            },
        );
        assert_eq!(error_code(&response), WORKFLOW_REVIEW_NOT_FOUND_CODE);
    }
}
