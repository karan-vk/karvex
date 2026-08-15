//! `workflow.review.{start,get,answer,report}` handlers
//! (`.local/prd/phase4-retarget-plan.md` §5 packets P3 and **P10**).
//!
//! `workflow.review.apply` lives in `workflow_review_apply.rs` — the same
//! split wave 2b's real implementation uses (§3.6 module map), so filling
//! either method in later work never touches the other's file.
//!
//! P3 landed this file as four documented refusals; P10 made them real. The
//! orchestration itself — the panes, the polling, the store writes — is
//! [`crate::app::workflow_review`]; everything here is parameter validation,
//! the store read behind `review.get`, and the wire projection both this file
//! and `workflow_review_apply.rs` answer with.
//!
//! Boundary note (`AGENTS.md`): every fact on this surface is a shared runtime
//! fact read back from the store, so a headless server answers exactly what a
//! server with a TUI attached does.

use crate::api::schema::{
    WorkflowReviewAnswerParams, WorkflowReviewReportParams, WorkflowRunTarget,
};
use crate::app::App;

use super::responses::encode_error;

/// No review cycle answers this request: either this run has never had one
/// (`review.get`), or the run named by `review.start`/`review.answer`/
/// `review.report` does not exist. Its own code rather than a bare
/// `not_found`, matching [`super::workflows`]'s `workflow_node_verb_retired`
/// precedent: a client can tell "no review cycle" from "no such run" or "no
/// such method".
#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
pub(crate) const WORKFLOW_REVIEW_NOT_FOUND_CODE: &str = "workflow_review_not_found";
/// A review cycle is already `running` or `awaiting_user` for this run
/// (`.local/prd/phase4-retarget-plan.md` §3.5, §6 D-4: a review does **not**
/// block a new run, but a run has at most one live review cycle at a time).
#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
pub(crate) const WORKFLOW_REVIEW_IN_FLIGHT_CODE: &str = "workflow_review_in_flight";
/// `workflow.review.apply` addressed a cycle that is not `awaiting_user`
/// (`workflow_review_apply.rs`), or `workflow.review.report` addressed a cycle
/// that has already reported.
#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
pub(crate) const WORKFLOW_REVIEW_NOT_AWAITING_CODE: &str = "workflow_review_not_awaiting";
/// `workflow.review.start` on a run with zero interviewable members: karvex
/// never recorded a team for it and there is nobody to rank or interview
/// (`.local/prd/phase4-retarget-plan.md` §3.5). Its own code rather than
/// folding into `workflow_review_not_found`, because the run and its history
/// both exist — there is simply nobody left to ask.
#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
pub(crate) const WORKFLOW_REVIEW_NO_INTERVIEWABLE_MEMBERS_CODE: &str =
    "workflow_review_no_interviewable_members";
/// `workflow.review.start` on a run that has not finished (§3.5: the trigger is
/// "a run reaching a terminal status"). Its own code because it is the one
/// refusal that becomes stale on its own — the same request succeeds once the
/// lead calls `run finish`.
#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
pub(crate) const WORKFLOW_REVIEW_RUN_NOT_TERMINAL_CODE: &str = "workflow_review_run_not_terminal";
/// An interview answer karvex could not parse. Its own code because the message
/// is a *correction* printed back into the interviewing agent's pane, not a
/// diagnostic for a human: the interview stays open and the agent can fix the
/// document and retry (§3.5).
#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
pub(crate) const WORKFLOW_REVIEW_ANSWER_REFUSED_CODE: &str = "workflow_review_answer_refused";
/// A findings document karvex could not parse, refused the same way and for the
/// same reason. `verdict: replace` with no `replacement` is refused here,
/// before it can reach the store's own `review_finding_replace_requires_
/// replacement` event.
#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
pub(crate) const WORKFLOW_REVIEW_REPORT_REFUSED_CODE: &str = "workflow_review_report_refused";
/// An answer arrived for an interview that is already answered, or that karvex
/// closed before it spoke (blocked, timed out, or its pane went away). Distinct
/// from a parse refusal because retrying will not help: that member's findings
/// are `evidence_only` from here on and saying so is the honest answer.
#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
pub(crate) const WORKFLOW_REVIEW_INTERVIEW_CLOSED_CODE: &str = "workflow_review_interview_closed";

/// The feature-off answer (`--no-default-features`), matching
/// [`super::workflows`]'s `workflow_unavailable` precedent. Kept as its own
/// copy rather than a shared helper: `workflows.rs`'s copy is private to that
/// module, and duplicating two string constants is cheaper than widening its
/// visibility.
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

/// The one place `answer`/`findings` are turned into the text the parser reads.
/// Exactly one form is required, the same split `WorkflowRunFinishParams` uses
/// and for the same reason: the reporting agent writes a JSON file and should
/// not have to inline it through argv.
#[cfg(feature = "workflow")]
fn one_document(
    id: &str,
    inline: Option<&serde_json::Value>,
    file: Option<&str>,
    what: &str,
) -> Result<String, String> {
    match (inline, file) {
        (Some(value), None) => Ok(value.to_string()),
        (None, Some(path)) => std::fs::read_to_string(path).map_err(|error| {
            encode_error(
                id.to_string(),
                "invalid_params",
                format!("the {what} file {path} could not be read: {error}"),
            )
        }),
        (Some(_), Some(_)) => Err(encode_error(
            id.to_string(),
            "invalid_params",
            format!("pass either the {what} or the {what} file, not both"),
        )),
        (None, None) => Err(encode_error(
            id.to_string(),
            "invalid_params",
            format!("exactly one of {what} or {what}_file is required"),
        )),
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
    /// `workflow.review.start` (§3.5): plan, spawn one pane per interview, and
    /// answer with the cycle that is now running.
    ///
    /// Never automatic. The trigger is this call, and the surfaces that offer
    /// it (`V`, `kvx workflow review start`) are asks, not modals.
    pub(super) fn handle_workflow_review_start(
        &mut self,
        id: String,
        target: WorkflowRunTarget,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &target.run_id) {
            return error;
        }
        let run_id = crate::workflow::model::RunId::new(target.run_id.trim().to_string());
        if let Err(refusal) = self.start_review_cycle(&run_id) {
            return encode_error(id, refusal.code, refusal.message);
        }
        match self.stored_review_info(&run_id) {
            Some(review) => encode_success(
                id,
                crate::api::schema::ResponseResult::WorkflowReviewStarted { review },
            ),
            None => encode_error(
                id,
                WORKFLOW_REVIEW_NOT_FOUND_CODE,
                "the review cycle was started but cannot be read back",
            ),
        }
    }

    /// `workflow.review.get`. A run that has never been reviewed answers
    /// `review: None` — a normal answer, not an error, matching
    /// `WorkflowSummaryGet`'s precedent.
    pub(super) fn handle_workflow_review_get(
        &mut self,
        id: String,
        target: WorkflowRunTarget,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &target.run_id) {
            return error;
        }
        let run_id = crate::workflow::model::RunId::new(target.run_id.trim().to_string());
        let review = self.stored_review_info(&run_id);
        let findings = match &review {
            Some(review) => self.stored_review_findings(
                &crate::workflow::model::ReviewCycleId::new(review.id.clone()),
            ),
            None => Vec::new(),
        };
        encode_success(
            id,
            crate::api::schema::ResponseResult::WorkflowReviewGet { review, findings },
        )
    }

    /// `workflow.review.answer`: one interview pane reporting its own answers,
    /// authorised by possession of the run id and the member name karvex itself
    /// exported into that pane.
    ///
    /// A parse refusal is printed back verbatim and the interview stays open —
    /// that refusal *is* the corrective re-prompt (§3.5).
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
        let raw = match one_document(
            &id,
            params.answer.as_ref(),
            params.answer_file.as_deref(),
            "answer",
        ) {
            Ok(raw) => raw,
            Err(error) => return error,
        };
        let run_id = crate::workflow::model::RunId::new(params.run_id.trim().to_string());
        if let Err(refusal) = self.record_review_answer(&run_id, params.member.trim(), &raw) {
            return encode_error(id, refusal.code, refusal.message);
        }
        match self.stored_review_info(&run_id) {
            Some(review) => encode_success(
                id,
                crate::api::schema::ResponseResult::WorkflowReviewAnswered { review },
            ),
            None => encode_error(
                id,
                WORKFLOW_REVIEW_NOT_FOUND_CODE,
                "the answer was accepted but the cycle cannot be read back",
            ),
        }
    }

    /// `workflow.review.report`: the synthesis pane's findings, recorded
    /// wholesale and applied selectively later (`08` D12).
    pub(super) fn handle_workflow_review_report(
        &mut self,
        id: String,
        params: WorkflowReviewReportParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &params.run_id) {
            return error;
        }
        let raw = match one_document(
            &id,
            params.findings.as_ref(),
            params.findings_file.as_deref(),
            "findings",
        ) {
            Ok(raw) => raw,
            Err(error) => return error,
        };
        let run_id = crate::workflow::model::RunId::new(params.run_id.trim().to_string());
        if let Err(refusal) = self.record_review_findings(&run_id, &raw) {
            return encode_error(id, refusal.code, refusal.message);
        }
        match self.stored_review_info(&run_id) {
            Some(review) => encode_success(
                id,
                crate::api::schema::ResponseResult::WorkflowReviewReported { review },
            ),
            None => encode_error(
                id,
                WORKFLOW_REVIEW_NOT_FOUND_CODE,
                "the findings were recorded but the cycle cannot be read back",
            ),
        }
    }

    /// The run's most recent review cycle, as the wire sees it.
    ///
    /// Read from the store rather than from [`crate::app::App::workflow_reviews`]
    /// on purpose: a cycle outlives the server that started it, and
    /// `review.get` — and `review.apply` after a restart — must answer the same
    /// thing whether or not this process happens to still be driving it.
    pub(crate) fn stored_review_info(
        &mut self,
        run_id: &crate::workflow::model::RunId,
    ) -> Option<crate::api::schema::WorkflowReviewInfo> {
        let wanted = run_id.clone();
        let loaded = self.workflow_store.call(move |cx| {
            let store = cx.store();
            let Some(cycle) = cx.block_on(store.get_review_cycle(&wanted))? else {
                return Ok::<_, crate::workflow::store::StoreError>(None);
            };
            let run = cx.block_on(store.get_run(&wanted))?;
            let findings = cx.block_on(store.list_review_findings(&cycle.id))?;
            let interrogations = cx.block_on(store.list_interrogations(&wanted))?;
            Ok(Some((cycle, run, findings, interrogations)))
        });
        let (cycle, run, findings, interrogations) = match loaded {
            Ok(Ok(Some(loaded))) => loaded,
            Ok(Ok(None)) => return None,
            Ok(Err(error)) => {
                tracing::warn!(%error, "a review cycle could not be read back");
                return None;
            }
            Err(unavailable) => {
                tracing::warn!(?unavailable, "the workflow store is unavailable");
                return None;
            }
        };
        let interview_paths = cycle
            .interviews
            .iter()
            .filter_map(|interview| {
                interrogations
                    .iter()
                    .find(|row| row.id == *interview)
                    .and_then(|row| row.transcript_path.clone())
            })
            .collect();
        // While the cycle is still this server's, the live plan knows exactly
        // how many interviews are degraded. Once it is not — after a restart,
        // or for a cycle another server ran — the honest durable answer is how
        // many members the findings themselves could not be attributed to.
        let evidence_only_count = self
            .live_review_evidence_only_count(run_id)
            .unwrap_or_else(|| evidence_only_members(&findings));
        Some(crate::api::schema::WorkflowReviewInfo {
            id: cycle.id.to_string(),
            run_id: cycle.run.to_string(),
            workflow_id: run
                .as_ref()
                .map(|run| run.workflow.to_string())
                .unwrap_or_default(),
            version_id: cycle.kvdag_version.to_string(),
            status: wire_review_status(cycle.status),
            started_at_unix_ms: cycle.started_at_unix_ms,
            ended_at_unix_ms: cycle.ended_at_unix_ms,
            resulting_version_id: cycle.resulting_version.map(|version| version.to_string()),
            interview_paths,
            evidence_only_count,
        })
    }

    /// One cycle's findings, as the wire sees them.
    pub(crate) fn stored_review_findings(
        &mut self,
        cycle: &crate::workflow::model::ReviewCycleId,
    ) -> Vec<crate::api::schema::WorkflowReviewFindingInfo> {
        let wanted = cycle.clone();
        let loaded = self
            .workflow_store
            .call(move |cx| cx.block_on(cx.store().list_review_findings(&wanted)));
        match loaded {
            Ok(Ok(findings)) => findings.into_iter().map(wire_review_finding).collect(),
            Ok(Err(error)) => {
                tracing::warn!(%error, "a review cycle's findings could not be read back");
                Vec::new()
            }
            Err(unavailable) => {
                tracing::warn!(?unavailable, "the workflow store is unavailable");
                Vec::new()
            }
        }
    }
}

#[cfg(feature = "workflow")]
use super::responses::encode_success;

/// How many distinct members a cycle's findings could not be attributed to.
///
/// The durable half of `evidence_only_count`: `finding_seed` records karvex's
/// own attribution under `evidence.attribution`, so this counts members, not
/// findings — five evidence-only findings about one teammate are one teammate
/// karvex could not hear from.
#[cfg(feature = "workflow")]
fn evidence_only_members(findings: &[crate::workflow::store::ReviewFindingRecord]) -> u32 {
    let members: std::collections::BTreeSet<&str> = findings
        .iter()
        .filter(|finding| {
            matches!(
                finding.interview_mode,
                crate::workflow::model::InterviewMode::EvidenceOnly
            )
        })
        .filter_map(|finding| {
            finding
                .evidence
                .get("attribution")
                .and_then(|attribution| attribution.get("member"))
                .and_then(serde_json::Value::as_str)
        })
        .collect();
    members.len() as u32
}

#[cfg(feature = "workflow")]
fn wire_review_status(
    status: crate::workflow::model::ReviewCycleStatus,
) -> crate::api::schema::WorkflowReviewStatus {
    use crate::api::schema::WorkflowReviewStatus as Wire;
    use crate::workflow::model::ReviewCycleStatus as Stored;
    match status {
        Stored::Running => Wire::Running,
        Stored::AwaitingUser => Wire::AwaitingUser,
        Stored::Applied => Wire::Applied,
        Stored::Declined => Wire::Declined,
        Stored::Failed => Wire::Failed,
    }
}

/// One stored finding, projected onto the wire.
///
/// `level` and `verdict` are stored as free text because the store's ASSERT is
/// the vocabulary's authority; an unknown value reads back as the least
/// alarming member of each enum rather than failing the whole response — a
/// finding karvex cannot classify is still a finding a human should see.
#[cfg(feature = "workflow")]
pub(crate) fn wire_review_finding(
    finding: crate::workflow::store::ReviewFindingRecord,
) -> crate::api::schema::WorkflowReviewFindingInfo {
    use crate::api::schema::{
        WorkflowReviewFindingLevel, WorkflowReviewInterviewMode, WorkflowReviewVerdict,
    };
    use crate::workflow::review::{FindingLevel, FindingVerdict};
    crate::api::schema::WorkflowReviewFindingInfo {
        node_key: finding.node_key.to_string(),
        run_path: finding.run_node.map(|path| path.to_string()),
        interrogation_id: finding.interview.map(|id| id.to_string()),
        interview_mode: match finding.interview_mode {
            crate::workflow::model::InterviewMode::Resumed => WorkflowReviewInterviewMode::Resumed,
            crate::workflow::model::InterviewMode::EvidenceOnly => {
                WorkflowReviewInterviewMode::EvidenceOnly
            }
        },
        level: match FindingLevel::parse(&finding.level) {
            Some(FindingLevel::Structural) => WorkflowReviewFindingLevel::Structural,
            _ => WorkflowReviewFindingLevel::Prompt,
        },
        verdict: match FindingVerdict::parse(&finding.verdict) {
            Some(FindingVerdict::Improve) => WorkflowReviewVerdict::Improve,
            Some(FindingVerdict::Replace) => WorkflowReviewVerdict::Replace,
            _ => WorkflowReviewVerdict::Keep,
        },
        rationale: finding.rationale,
        evidence: finding.evidence,
        proposed_change: finding.proposed_change,
        replacement: finding.replacement,
        accepted: finding.accepted,
        applied_in_version: finding.applied_in.map(|version| version.to_string()),
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
    fn an_answer_with_both_forms_is_rejected() {
        let mut app = app();
        let response = app.handle_workflow_review_answer(
            "req".into(),
            WorkflowReviewAnswerParams {
                run_id: "workflow_run:1".into(),
                member: "research".into(),
                answer: Some(serde_json::json!({"account": "done"})),
                answer_file: Some("/tmp/answer.json".into()),
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

    /// A run that has never been reviewed is a **successful** `review: null`,
    /// not an error — `WorkflowSummaryGet`'s precedent, and the shape the
    /// overlay's "no review yet" state reads.
    #[cfg(feature = "workflow")]
    #[test]
    fn review_get_on_a_run_with_no_cycle_answers_successfully() {
        let mut app = app();
        let response = app.handle_workflow_review_get(
            "req".into(),
            WorkflowRunTarget {
                run_id: "workflow_run:1".into(),
            },
        );
        let value: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert!(value["error"].is_null(), "{response}");
        assert!(value["result"]["review"].is_null(), "{response}");
        assert_eq!(
            value["result"]["findings"].as_array().map(Vec::len),
            Some(0),
        );
    }

    /// Every mutating verb names a run, and a run that does not exist is said
    /// so by name rather than by the dispatcher's catch-all.
    #[cfg(feature = "workflow")]
    #[test]
    fn the_mutating_verbs_refuse_an_unknown_run_by_name() {
        let mut app = app();
        let start = app.handle_workflow_review_start(
            "req".into(),
            WorkflowRunTarget {
                run_id: "workflow_run:1".into(),
            },
        );
        assert_eq!(error_code(&start), WORKFLOW_REVIEW_NOT_FOUND_CODE);

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

    /// The eight codes this file owns are part of the wire surface: a client
    /// switches on them, so they are pinned here rather than left to whatever
    /// a refactor renames them to.
    #[cfg(feature = "workflow")]
    #[test]
    fn the_review_error_codes_are_frozen() {
        assert_eq!(WORKFLOW_REVIEW_NOT_FOUND_CODE, "workflow_review_not_found");
        assert_eq!(WORKFLOW_REVIEW_IN_FLIGHT_CODE, "workflow_review_in_flight");
        assert_eq!(
            WORKFLOW_REVIEW_NOT_AWAITING_CODE,
            "workflow_review_not_awaiting"
        );
        assert_eq!(
            WORKFLOW_REVIEW_NO_INTERVIEWABLE_MEMBERS_CODE,
            "workflow_review_no_interviewable_members"
        );
        assert_eq!(
            WORKFLOW_REVIEW_RUN_NOT_TERMINAL_CODE,
            "workflow_review_run_not_terminal"
        );
        assert_eq!(
            WORKFLOW_REVIEW_ANSWER_REFUSED_CODE,
            "workflow_review_answer_refused"
        );
        assert_eq!(
            WORKFLOW_REVIEW_REPORT_REFUSED_CODE,
            "workflow_review_report_refused"
        );
        assert_eq!(
            WORKFLOW_REVIEW_INTERVIEW_CLOSED_CODE,
            "workflow_review_interview_closed"
        );
    }
}
