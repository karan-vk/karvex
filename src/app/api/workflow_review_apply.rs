//! `workflow.review.apply`: the human's per-finding accept becomes a new
//! immutable definition version (`.local/prd/phase4-retarget-plan.md` §3.5,
//! §5 packet P11).
//!
//! Split from `workflow_review.rs` on purpose — matching wave 2b's real
//! implementation split (§3.6 module map): apply compiles accepted findings
//! into a new `kvdag_version` and flips the workflow's head, which is a
//! meaningfully different concern from the four methods that plan, spawn, and
//! self-report a cycle.
//!
//! **Store-only** (§6 D-13, and unconditionally so now that node residency is
//! gone): this handler reads rows, calls a pure compiler, and writes rows. It
//! never asks whether a run is live, never touches `workflow_lead`, and never
//! looks at a pane — which is what makes "kill the server after
//! `review.ready`, restart, and apply still succeeds" a property of the design
//! rather than a scenario that happens to pass.
//!
//! The five rules this handler enforces, and where each one lives:
//!
//! 1. **Per-finding acceptance.** `accept` names `node_key`s; everything else
//!    the cycle produced is declined and leaves no trace, because only the
//!    named findings are ever built into an
//!    [`AcceptedFinding`](crate::workflow::compile_findings::AcceptedFinding).
//!    An empty `accept` declines the whole cycle and mints nothing.
//! 2. **Immutability.** The parent version is read, never written. The new
//!    version is minted by the *existing* `create_version_with_metadata`
//!    (`origin: self_improvement`, explicit parent), the one authoring already
//!    uses — there is no second versioning path in karvex and this packet did
//!    not add one.
//! 3. **The parent is the run's version, never the head.** A workflow whose
//!    head moved on since the run is still improved *from what the run
//!    actually executed*; `KvdagSpec::parent` states that explicitly, which is
//!    also what makes the store skip its no-op-revision collapse (a deliberate
//!    non-linear origin is never a no-op).
//! 4. **All or nothing.** A compile refusal fails the *apply* with the
//!    validation message and leaves the cycle `awaiting_user`, so the human
//!    can accept a smaller set. There is no half-applied version, because the
//!    compiler returns a document or an error and the first write happens
//!    after it returns.
//! 5. **Attribution survives.** The minted version's `change_summary` names
//!    each accepted finding and whether it is a teammate's own account or an
//!    evidence-only inference (`compile_findings::change_summary`).

use crate::api::schema::WorkflowReviewApplyParams;
use crate::app::App;

use super::responses::encode_error;
#[cfg(feature = "workflow")]
use super::responses::encode_success;
#[cfg(feature = "workflow")]
use super::workflow_review::{WORKFLOW_REVIEW_NOT_AWAITING_CODE, WORKFLOW_REVIEW_NOT_FOUND_CODE};

#[cfg(feature = "workflow")]
use crate::api::schema::{
    EventData, EventKind, ResponseResult, WorkflowReviewInfo, WorkflowReviewStatus,
};
#[cfg(feature = "workflow")]
use crate::app::workflow_store::StoreContext;
#[cfg(feature = "workflow")]
use crate::workflow::compile_findings::{
    apply_findings, change_summary, spec_of, AcceptedFinding, FindingAttribution,
};
#[cfg(feature = "workflow")]
use crate::workflow::model::{
    KvdagSpec, NodeKey, ReviewCycleId, ReviewCycleStatus, RunId, StoreWrite,
};
#[cfg(feature = "workflow")]
use crate::workflow::review::{FindingLevel, FindingVerdict};
#[cfg(feature = "workflow")]
use crate::workflow::store::{ReviewFindingRecord, RunRecord};
#[cfg(feature = "workflow")]
use crate::workflow::store::{StoreError, VersionOrigin};

#[cfg(not(feature = "workflow"))]
const WORKFLOW_UNAVAILABLE_CODE: &str = "workflow_unavailable";
#[cfg(not(feature = "workflow"))]
const WORKFLOW_UNAVAILABLE_MESSAGE: &str =
    "the workflow feature is not compiled into this server (built with --no-default-features);      rebuild with --features workflow";

/// The run this apply names does not exist. `workflows.rs`'s own
/// `workflow_not_found`, spelled again here for the same reason P3 duplicated
/// `require_non_empty`: that module's copy is private to it, and a client
/// distinguishing "no such run" from "no review cycle" needs the two codes to
/// be the ones it already knows.
#[cfg(feature = "workflow")]
const WORKFLOW_NOT_FOUND_CODE: &str = "workflow_not_found";
/// `accept` names a finding this cycle never produced.
#[cfg(feature = "workflow")]
const INVALID_ARGUMENT_CODE: &str = "workflow_invalid_argument";
/// The accepted findings do not compile into a definition karvex would author
/// (`compile_findings::CompileError`, or a stored finding whose `level`/
/// `verdict` is outside the vocabulary `workflow::review` owns).
///
/// Its own code rather than `workflow_invalid_definition`: nothing was
/// authored, nothing was written, and the cycle is still `awaiting_user` —
/// the client's next move is to accept a smaller set, which is a different
/// instruction from "fix your document".
#[cfg(feature = "workflow")]
pub(crate) const WORKFLOW_REVIEW_COMPILE_FAILED_CODE: &str = "workflow_review_compile_failed";

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

/// What one apply did, decided entirely on the store thread so the handler
/// body below is a translation into a response and nothing else.
#[cfg(feature = "workflow")]
enum ApplyOutcome {
    NoRun,
    NoCycle,
    /// The cycle exists but is `running`, `applied`, `declined`, or `failed`.
    NotAwaiting(ReviewCycleStatus),
    /// `accept` named findings this cycle never produced. Carries what was
    /// asked for and what is actually on offer, because a typo'd node key
    /// would otherwise silently decline everything.
    UnknownFindings {
        unknown: Vec<String>,
        available: Vec<String>,
    },
    /// A stored finding carries a `level`/`verdict` outside the closed
    /// vocabulary. Refused rather than skipped: the human accepted it.
    UnknownVocabulary(String),
    /// The accepted set does not compile. Nothing was written; the cycle is
    /// still `awaiting_user`.
    CompileRefused(String),
    Declined(WorkflowReviewInfo),
    Applied {
        review: WorkflowReviewInfo,
        version_id: String,
    },
}

#[cfg(feature = "workflow")]
impl App {
    /// Compiles the accepted findings of a run's review cycle into a new
    /// version, advances the workflow's head to it, and closes the cycle.
    ///
    /// Authorisation is possession of the run id, like every other run-scoped
    /// method: this one is reached from the TUI overlay and the CLI, both of
    /// which are already the user.
    pub(super) fn handle_workflow_review_apply(
        &mut self,
        id: String,
        params: WorkflowReviewApplyParams,
    ) -> String {
        if let Some(error) = require_non_empty(&id, "run_id", &params.run_id) {
            return error;
        }
        let run_id = RunId::new(params.run_id.trim().to_string());
        // Deduplicated, and in a stable order: `accept` is repeatable on the
        // CLI (`--accept plan --accept plan`) and the store marks findings by
        // key, so the same key twice is one acceptance, not two.
        let mut accept: Vec<String> = params
            .accept
            .iter()
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
            .collect();
        accept.sort();
        accept.dedup();

        // The clock is read here, on the app thread, and handed to the store —
        // never `time::now()` inside a query (§4 D13/D14, the rule migration
        // `0002` exists to hold).
        let now_unix_ms = unix_now_ms();
        let target = run_id.clone();
        let applied = self
            .workflow_store
            .call(move |cx| apply_review_cycle(cx, &target, &accept, now_unix_ms));

        let outcome = match applied {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => return encode_error(id, error.api_code(), error.to_string()),
            Err(unavailable) => {
                return encode_error(id, unavailable.code, unavailable.message.clone())
            }
        };

        match outcome {
            ApplyOutcome::NoRun => {
                encode_error(id, WORKFLOW_NOT_FOUND_CODE, format!("no run {run_id}"))
            }
            ApplyOutcome::NoCycle => encode_error(
                id,
                WORKFLOW_REVIEW_NOT_FOUND_CODE,
                format!("no review cycle exists for run {run_id}"),
            ),
            ApplyOutcome::NotAwaiting(status) => encode_error(
                id,
                WORKFLOW_REVIEW_NOT_AWAITING_CODE,
                match status {
                    ReviewCycleStatus::Running => format!(
                        "run {run_id}'s review cycle is still running; its findings are not \
                         ready to accept yet"
                    ),
                    other => format!(
                        "run {run_id}'s review cycle is already {}; a cycle is decided once",
                        other.as_str()
                    ),
                },
            ),
            ApplyOutcome::UnknownFindings { unknown, available } => encode_error(
                id,
                INVALID_ARGUMENT_CODE,
                format!(
                    "this review cycle produced no finding for {}; it has findings for: {}",
                    quoted(&unknown),
                    if available.is_empty() {
                        "nothing".to_string()
                    } else {
                        quoted(&available)
                    }
                ),
            ),
            ApplyOutcome::UnknownVocabulary(message) | ApplyOutcome::CompileRefused(message) => {
                encode_error(id, WORKFLOW_REVIEW_COMPILE_FAILED_CODE, message)
            }
            ApplyOutcome::Declined(review) => {
                self.emit_review_closed(&run_id, &review);
                encode_success(
                    id,
                    ResponseResult::WorkflowReviewApplied {
                        review,
                        version_id: None,
                    },
                )
            }
            ApplyOutcome::Applied { review, version_id } => {
                self.emit_review_closed(&run_id, &review);
                encode_success(
                    id,
                    ResponseResult::WorkflowReviewApplied {
                        review,
                        version_id: Some(version_id),
                    },
                )
            }
        }
    }

    /// `workflow.review.closed` — emitted for `applied` and `declined` alike,
    /// because both are the cycle reaching a terminal status and a client
    /// watching for "the review is over" must not have to guess which verb
    /// produced it.
    fn emit_review_closed(&mut self, run_id: &RunId, review: &WorkflowReviewInfo) {
        self.emit_workflow_run_event(
            EventKind::WorkflowReviewClosed,
            EventData::WorkflowReviewClosed {
                run_id: run_id.to_string(),
                review: review.clone(),
            },
        );
    }
}

/// The whole apply, on the store thread: read, decide, compile, write.
///
/// One store job rather than several, so no other request can interleave
/// between the precondition read and the writes it authorises.
#[cfg(feature = "workflow")]
fn apply_review_cycle(
    cx: &StoreContext<'_>,
    run_id: &RunId,
    accept: &[String],
    now_unix_ms: u64,
) -> Result<ApplyOutcome, StoreError> {
    let Some(run) = cx.block_on(cx.store().get_run(run_id))? else {
        return Ok(ApplyOutcome::NoRun);
    };
    let Some(cycle) = cx.block_on(cx.store().get_review_cycle(run_id))? else {
        return Ok(ApplyOutcome::NoCycle);
    };
    if cycle.status != ReviewCycleStatus::AwaitingUser {
        return Ok(ApplyOutcome::NotAwaiting(cycle.status));
    }

    let findings = cx.block_on(cx.store().list_review_findings(&cycle.id))?;
    let mut available: Vec<String> = findings
        .iter()
        .map(|finding| finding.node_key.to_string())
        .collect();
    available.dedup();
    let unknown: Vec<String> = accept
        .iter()
        .filter(|key| !available.iter().any(|known| known == *key))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        return Ok(ApplyOutcome::UnknownFindings { unknown, available });
    }

    // Nothing accepted: the cycle is declined, no version is minted, and no
    // finding is marked. "The human looked and said no" is a real outcome and
    // is recorded as one (§5 P11: an empty `accept` declines the cycle).
    if accept.is_empty() {
        cx.block_on(cx.store().write(StoreWrite::ReviewCycleUpdate {
            id: cycle.id.clone(),
            status: Some(ReviewCycleStatus::Declined),
            ended_at_unix_ms: Some(now_unix_ms),
            resulting_version: None,
        }))?;
        return Ok(ApplyOutcome::Declined(review_info(cx, &run, &cycle.id)?));
    }

    let accepted = match accepted_findings(&findings, accept) {
        Ok(accepted) => accepted,
        Err(message) => return Ok(ApplyOutcome::UnknownVocabulary(message)),
    };

    // The version the run *executed*, not the workflow's head: a head that
    // moved on since is somebody else's edit, and improving a document the
    // review never saw would attribute a stranger's nodes to this run's
    // teammates.
    let parent = cx.block_on(cx.store().load_version(&run.version))?;
    let spec = KvdagSpec {
        parent: Some(run.version.clone()),
        ..spec_of(&parent)
    };
    let compiled = match apply_findings(spec, &accepted) {
        Ok(compiled) => compiled,
        Err(refusal) => return Ok(ApplyOutcome::CompileRefused(refusal.to_string())),
    };

    // From here on the writes begin, and every one of them is idempotent-ish
    // by construction rather than transactional: the store is not one
    // transaction across calls, so the *order* is the guarantee. The cycle's
    // status flips **last**, because it is the precondition that stops a
    // second apply — a crash before it leaves a cycle that can be applied
    // again (visible, recoverable), while flipping it first would strand the
    // findings' `accepted` marks with no verb left that could set them.
    let created = cx.block_on(cx.store().create_version_with_metadata(
        &run.workflow,
        VersionOrigin::SelfImprovement,
        &change_summary(&accepted),
        compiled,
        None,
    ))?;
    cx.block_on(
        cx.store()
            .set_head_version(&run.workflow, &created.version_id),
    )?;
    let keys: Vec<NodeKey> = accept.iter().map(|key| NodeKey::new(key.clone())).collect();
    cx.block_on(
        cx.store()
            .finding_mark_applied(&cycle.id, &keys, &created.version_id),
    )?;
    cx.block_on(cx.store().write(StoreWrite::ReviewCycleUpdate {
        id: cycle.id.clone(),
        status: Some(ReviewCycleStatus::Applied),
        ended_at_unix_ms: Some(now_unix_ms),
        resulting_version: Some(created.version_id.clone()),
    }))?;

    Ok(ApplyOutcome::Applied {
        review: review_info(cx, &run, &cycle.id)?,
        version_id: created.version_id.to_string(),
    })
}

/// Turns the accepted subset of stored findings into the compiler's input.
///
/// Every finding for an accepted `node_key` is included: acceptance is per
/// node key, which is also the granularity the store marks
/// (`finding_mark_applied(cycle, keys, version)`) and the CLI offers
/// (`--accept <node_key>`).
///
/// `level`/`verdict` are stored as strings (the store's own ASSERTed
/// vocabulary), and parsing them is this adapter's job — the pure compiler is
/// handed the typed vocabulary `workflow::review` owns, so it is total over
/// its inputs.
#[cfg(feature = "workflow")]
fn accepted_findings(
    findings: &[ReviewFindingRecord],
    accept: &[String],
) -> Result<Vec<AcceptedFinding>, String> {
    let mut accepted = Vec::new();
    for finding in findings {
        if !accept.iter().any(|key| key == finding.node_key.as_str()) {
            continue;
        }
        let Some(level) = FindingLevel::parse(&finding.level) else {
            return Err(format!(
                "the finding for \"{}\" carries level \"{}\", which is not \"prompt\" or \
                 \"structural\"; karvex will not guess what it meant",
                finding.node_key, finding.level
            ));
        };
        let Some(verdict) = FindingVerdict::parse(&finding.verdict) else {
            return Err(format!(
                "the finding for \"{}\" carries verdict \"{}\", which is not \"keep\", \
                 \"improve\", or \"replace\"; karvex will not guess what it meant",
                finding.node_key, finding.verdict
            ));
        };
        accepted.push(AcceptedFinding {
            node_key: finding.node_key.clone(),
            level,
            verdict,
            proposed_change: finding.proposed_change.clone(),
            replacement: finding.replacement.clone(),
            // The interview mode comes from the store's own typed column; the
            // member and the evidence-only reason come from the `attribution`
            // object `review::finding_seed` wrote beside the synthesiser's
            // evidence. Neither is the synthesiser's to claim.
            attribution: FindingAttribution::from_seed_evidence(
                &finding.evidence,
                finding.interview_mode,
            ),
        });
    }
    Ok(accepted)
}

/// One review cycle, as the wire sees it, read back after the writes so the
/// response describes what is actually stored rather than what was intended.
///
/// `pub(super)` because `workflow.review.get`/`start` (P10) need exactly this
/// projection and two copies of it would drift; it lives here because this is
/// the file that landed first with a store-backed one.
#[cfg(feature = "workflow")]
pub(super) fn review_info(
    cx: &StoreContext<'_>,
    run: &RunRecord,
    cycle_id: &ReviewCycleId,
) -> Result<WorkflowReviewInfo, StoreError> {
    let cycle = cx
        .block_on(cx.store().get_review_cycle(&run.id))?
        .filter(|cycle| &cycle.id == cycle_id)
        .ok_or_else(|| {
            StoreError::Decode(format!("review cycle {cycle_id} could not be read back"))
        })?;
    let findings = cx.block_on(cx.store().list_review_findings(&cycle.id))?;
    // One path per interview this cycle conducted. Only the interrogations the
    // cycle actually cites, and only the ones with a transcript on disk: a
    // path karvex cannot show is not a path.
    let interview_paths = cx
        .block_on(cx.store().list_interrogations(&run.id))?
        .into_iter()
        .filter(|interrogation| cycle.interviews.contains(&interrogation.id))
        .filter_map(|interrogation| interrogation.transcript_path)
        .collect();
    Ok(WorkflowReviewInfo {
        id: cycle.id.to_string(),
        run_id: run.id.to_string(),
        workflow_id: run.workflow.to_string(),
        version_id: cycle.kvdag_version.to_string(),
        status: wire_status(cycle.status),
        started_at_unix_ms: cycle.started_at_unix_ms,
        ended_at_unix_ms: cycle.ended_at_unix_ms,
        resulting_version_id: cycle
            .resulting_version
            .as_ref()
            .map(|version| version.to_string()),
        interview_paths,
        evidence_only_count: evidence_only_members(&findings),
    })
}

/// How many members this cycle could only infer about.
///
/// Reconstructed from the findings, because the *plan* that decided it lives
/// in memory and does not survive the restart this handler is specified to
/// work across: it counts distinct members whose findings are stamped
/// `evidence_only`. A finding attributed to nobody is about the run rather
/// than about a teammate and is not a degraded interview, so it is not
/// counted.
#[cfg(feature = "workflow")]
fn evidence_only_members(findings: &[ReviewFindingRecord]) -> u32 {
    let mut members: Vec<String> = findings
        .iter()
        .filter(|finding| {
            finding.interview_mode == crate::workflow::model::InterviewMode::EvidenceOnly
        })
        .filter_map(|finding| {
            FindingAttribution::from_seed_evidence(&finding.evidence, finding.interview_mode).member
        })
        .collect();
    members.sort();
    members.dedup();
    u32::try_from(members.len()).unwrap_or(u32::MAX)
}

#[cfg(feature = "workflow")]
fn wire_status(status: ReviewCycleStatus) -> WorkflowReviewStatus {
    match status {
        ReviewCycleStatus::Running => WorkflowReviewStatus::Running,
        ReviewCycleStatus::AwaitingUser => WorkflowReviewStatus::AwaitingUser,
        ReviewCycleStatus::Applied => WorkflowReviewStatus::Applied,
        ReviewCycleStatus::Declined => WorkflowReviewStatus::Declined,
        ReviewCycleStatus::Failed => WorkflowReviewStatus::Failed,
    }
}

#[cfg(feature = "workflow")]
fn quoted(keys: &[String]) -> String {
    keys.iter()
        .map(|key| format!("\"{key}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(feature = "workflow")]
fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
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

    #[cfg(feature = "workflow")]
    mod store_backed {
        use super::*;
        use crate::workflow::model::{
            ArgSpec, Demand, EdgeKind, EdgePayload, GrowthLimits, InterviewMode, Kvdag, KvdagEdge,
            KvdagNode, KvdagVersionId, NodeKind, OutputSchema, ReviewFindingSeed, Runner,
            WorkflowId,
        };
        use crate::workflow::store::NewRun;
        use crate::workflow::tier::{resolve_assignments, HistoryIndex, Tier};

        /// The whole world one apply needs: a workflow, the version the run
        /// executed, the run, and a review cycle sitting in `awaiting_user`
        /// with findings on it.
        struct Seeded {
            workflow: WorkflowId,
            version: KvdagVersionId,
            run: RunId,
            cycle: ReviewCycleId,
        }

        fn node(key: &str, prompt: &str) -> KvdagNode {
            KvdagNode {
                key: NodeKey::new(key),
                label: key.to_string(),
                role: String::new(),
                kind: NodeKind::Agent,
                demand: Demand::Standard,
                runner: Runner::Agent,
                command: None,
                prompt_template: prompt.to_string(),
                system_contract: None,
                output_schema: OutputSchema::parse(serde_json::json!({"type": "object"}))
                    .expect("valid schema"),
                max_attempts: 2,
                timeout_ms: None,
                isolation: crate::workflow::model::Isolation::None,
                is_template: false,
                expand_allow: Vec::new(),
                expand_max: 0,
            }
        }

        fn spec() -> KvdagSpec {
            KvdagSpec {
                version_id: KvdagVersionId::new("kvdag_version:placeholder"),
                workflow_id: WorkflowId::new("workflow:placeholder"),
                version: 1,
                parent: None,
                contract: "reply only through result.json".to_string(),
                growth: GrowthLimits::default(),
                args: vec![ArgSpec {
                    name: "goal".to_string(),
                    required: true,
                    default: None,
                    description: "what to build".to_string(),
                }],
                nodes: vec![
                    node("plan", "Plan for: {{goal}}"),
                    node("implement", "Implement {{goal}}"),
                ],
                edges: vec![KvdagEdge {
                    from: NodeKey::new("plan"),
                    to: NodeKey::new("implement"),
                    kind: EdgeKind::Sequence,
                    condition: None,
                    payload: EdgePayload::Summary,
                    port: None,
                }],
            }
        }

        fn finding_seed(
            node_key: &str,
            level: &str,
            verdict: &str,
            proposed_change: serde_json::Value,
        ) -> ReviewFindingSeed {
            ReviewFindingSeed {
                node_key: NodeKey::new(node_key),
                run_node: None,
                interview: None,
                interview_mode: InterviewMode::EvidenceOnly,
                level: level.to_string(),
                verdict: verdict.to_string(),
                rationale: "measured".to_string(),
                // The shape `review::finding_seed` writes, so the attribution
                // this handler reads back is the real one.
                evidence: serde_json::json!({
                    "reported": {"idle_ms": 900000},
                    "attribution": {
                        "member": "builder",
                        "interview_mode": "evidence_only",
                        "reason": "no_session_id",
                    },
                }),
                proposed_change,
                replacement: None,
            }
        }

        fn seed(
            app: &mut App,
            findings: Vec<ReviewFindingSeed>,
            status: ReviewCycleStatus,
        ) -> Seeded {
            seed_with_spec(app, spec(), findings, status)
        }

        fn seed_with_spec(
            app: &mut App,
            spec: KvdagSpec,
            findings: Vec<ReviewFindingSeed>,
            status: ReviewCycleStatus,
        ) -> Seeded {
            app.workflow_store
                .call(move |cx| {
                    let workflow = cx.block_on(cx.store().create_workflow(
                        "ship-feature",
                        "plan then implement",
                        Tier::High,
                    ))?;
                    let kvdag = cx.block_on(cx.store().create_version(
                        &workflow,
                        VersionOrigin::Authored,
                        "v1",
                        spec,
                    ))?;
                    cx.block_on(cx.store().set_head_version(&workflow, &kvdag.version_id))?;
                    let run = cx.block_on(cx.store().create_run(new_run(&workflow, &kvdag)))?;
                    let cycle = ReviewCycleId::new("review_cycle:t1");
                    cx.block_on(cx.store().write(StoreWrite::ReviewCycleStarted {
                        id: cycle.clone(),
                        run: run.clone(),
                        kvdag_version: kvdag.version_id.clone(),
                        started_at_unix_ms: 1_700_000_000_000,
                    }))?;
                    if !findings.is_empty() {
                        cx.block_on(cx.store().write(StoreWrite::ReviewFindings {
                            cycle: cycle.clone(),
                            findings,
                        }))?;
                    }
                    cx.block_on(cx.store().write(StoreWrite::ReviewCycleUpdate {
                        id: cycle.clone(),
                        status: Some(status),
                        ended_at_unix_ms: None,
                        resulting_version: None,
                    }))?;
                    Ok::<_, StoreError>(Seeded {
                        workflow,
                        version: kvdag.version_id,
                        run,
                        cycle,
                    })
                })
                .expect("the in-memory store answers")
                .expect("the fixture is written")
        }

        fn new_run(workflow: &WorkflowId, kvdag: &Kvdag) -> NewRun {
            NewRun {
                workflow: workflow.clone(),
                version: kvdag.version_id.clone(),
                tier: Tier::High,
                args: std::collections::BTreeMap::from([(
                    "goal".to_string(),
                    "ship it".to_string(),
                )]),
                growth: GrowthLimits::default(),
                started_at_unix_ms: 1_699_999_000_000,
                assignments: resolve_assignments(kvdag, Tier::High, &HistoryIndex::new()),
                context_runs: Vec::new(),
                workspace_id: None,
                restore_from: None,
                restored: Vec::new(),
            }
        }

        fn apply(app: &mut App, run: &RunId, accept: &[&str]) -> String {
            app.handle_workflow_review_apply(
                "req".into(),
                WorkflowReviewApplyParams {
                    run_id: run.to_string(),
                    accept: accept.iter().map(|key| (*key).to_string()).collect(),
                },
            )
        }

        fn result(response: &str) -> serde_json::Value {
            let value: serde_json::Value = serde_json::from_str(response).unwrap();
            assert!(
                value.get("error").is_none(),
                "expected a success, got {value}"
            );
            value["result"].clone()
        }

        /// Every version this workflow has, newest first, as
        /// `(version, origin, parent, change_summary)`.
        fn versions(
            app: &mut App,
            workflow: &WorkflowId,
        ) -> Vec<(u32, String, Option<String>, String)> {
            let workflow = workflow.clone();
            app.workflow_store
                .call(move |cx| {
                    let head = cx
                        .block_on(cx.store().get_workflow(&workflow))?
                        .and_then(|row| row.head_version);
                    let workflow = workflow.clone();
                    let records =
                        cx.block_on(cx.store().list_version_chain(&workflow, head.as_ref()))?;
                    Ok::<_, StoreError>(
                        records
                            .into_iter()
                            .map(|record| {
                                (
                                    record.version,
                                    record.origin.as_str().to_string(),
                                    record
                                        .parent_version_id
                                        .as_ref()
                                        .map(|parent| parent.to_string()),
                                    record.change_summary,
                                )
                            })
                            .collect(),
                    )
                })
                .expect("the store answers")
                .expect("the chain reads")
        }

        fn head_nodes(app: &mut App, workflow: &WorkflowId) -> Vec<KvdagNode> {
            let workflow = workflow.clone();
            app.workflow_store
                .call(move |cx| {
                    let head = cx
                        .block_on(cx.store().get_workflow(&workflow))?
                        .and_then(|row| row.head_version)
                        .expect("the workflow has a head");
                    Ok::<_, StoreError>(cx.block_on(cx.store().load_version(&head))?.nodes)
                })
                .expect("the store answers")
                .expect("the head loads")
        }

        fn stored_findings(app: &mut App, cycle: &ReviewCycleId) -> Vec<ReviewFindingRecord> {
            let cycle = cycle.clone();
            app.workflow_store
                .call(move |cx| cx.block_on(cx.store().list_review_findings(&cycle)))
                .expect("the store answers")
                .expect("the findings read")
        }

        fn stored_cycle(app: &mut App, run: &RunId) -> crate::workflow::store::ReviewCycleRecord {
            let run = run.clone();
            app.workflow_store
                .call(move |cx| cx.block_on(cx.store().get_review_cycle(&run)))
                .expect("the store answers")
                .expect("the cycle reads")
                .expect("the cycle exists")
        }

        // ── the happy path ─────────────────────────────────────────────────

        /// The packet's whole point: an accepted finding mints a version whose
        /// origin is `self_improvement` and whose parent is **the run's**
        /// version, the head advances to it, and the finding is marked as
        /// applied in it.
        #[test]
        fn an_accepted_finding_mints_a_self_improvement_version_parented_on_the_runs_version() {
            let mut app = app();
            let seeded = seed(
                &mut app,
                vec![finding_seed(
                    "plan",
                    "prompt",
                    "improve",
                    serde_json::json!({"prompt_template": "Plan for {{goal}}, and name the risks"}),
                )],
                ReviewCycleStatus::AwaitingUser,
            );

            let response = apply(&mut app, &seeded.run, &["plan"]);
            let result = result(&response);
            let minted = result["version_id"].as_str().expect("a version was minted");
            assert_eq!(result["review"]["status"], "applied");
            assert_eq!(result["review"]["resulting_version_id"], minted);

            let chain = versions(&mut app, &seeded.workflow);
            assert_eq!(
                chain.len(),
                2,
                "the parent version is still there: {chain:?}"
            );
            let (version, origin, parent, summary) = chain[0].clone();
            assert_eq!(version, 2);
            assert_eq!(origin, "self_improvement");
            assert_eq!(parent.as_deref(), Some(seeded.version.as_str()));
            assert!(summary.contains("\"plan\" prompt/improve"), "{summary}");
            assert_eq!(chain[1].1, "authored", "v1 is untouched");

            let head = head_nodes(&mut app, &seeded.workflow);
            let plan = head
                .iter()
                .find(|node| node.key.as_str() == "plan")
                .expect("plan survives");
            assert_eq!(
                plan.prompt_template,
                "Plan for {{goal}}, and name the risks"
            );

            let findings = stored_findings(&mut app, &seeded.cycle);
            assert!(findings[0].accepted);
            assert_eq!(
                findings[0].applied_in.as_ref().map(|id| id.to_string()),
                Some(minted.to_string())
            );
            assert_eq!(
                stored_cycle(&mut app, &seeded.run).status,
                ReviewCycleStatus::Applied
            );
        }

        /// Per-finding acceptance: the declined finding leaves no trace in the
        /// compiled version, is not marked applied, and the accepted one is
        /// unaffected by its presence.
        #[test]
        fn a_declined_finding_leaves_no_trace_in_the_compiled_version() {
            let mut app = app();
            let seeded = seed(
                &mut app,
                vec![
                    finding_seed(
                        "plan",
                        "prompt",
                        "improve",
                        serde_json::json!({"role": "planner"}),
                    ),
                    finding_seed(
                        "implement",
                        "structural",
                        "improve",
                        serde_json::json!({"max_attempts": 5}),
                    ),
                ],
                ReviewCycleStatus::AwaitingUser,
            );

            let response = apply(&mut app, &seeded.run, &["plan"]);
            let minted = result(&response)["version_id"]
                .as_str()
                .expect("a version was minted")
                .to_string();

            let head = head_nodes(&mut app, &seeded.workflow);
            let plan = head
                .iter()
                .find(|node| node.key.as_str() == "plan")
                .unwrap();
            let implement = head
                .iter()
                .find(|node| node.key.as_str() == "implement")
                .unwrap();
            assert_eq!(plan.role, "planner");
            assert_eq!(
                implement.max_attempts, 2,
                "the declined structural finding never reached the document"
            );

            let findings = stored_findings(&mut app, &seeded.cycle);
            let implement_finding = findings
                .iter()
                .find(|finding| finding.node_key.as_str() == "implement")
                .unwrap();
            assert!(!implement_finding.accepted, "declined stays declined");
            assert_eq!(implement_finding.applied_in, None);
            let plan_finding = findings
                .iter()
                .find(|finding| finding.node_key.as_str() == "plan")
                .unwrap();
            assert_eq!(
                plan_finding.applied_in.as_ref().map(|id| id.to_string()),
                Some(minted)
            );
        }

        /// An empty `accept` declines the cycle: no version, no head move, no
        /// finding marked — and a successful response, because "no" is an
        /// answer.
        #[test]
        fn an_empty_accept_declines_the_cycle_and_mints_nothing() {
            let mut app = app();
            let seeded = seed(
                &mut app,
                vec![finding_seed(
                    "plan",
                    "prompt",
                    "improve",
                    serde_json::json!({"role": "planner"}),
                )],
                ReviewCycleStatus::AwaitingUser,
            );

            let result = result(&apply(&mut app, &seeded.run, &[]));
            assert!(result["version_id"].is_null());
            assert_eq!(result["review"]["status"], "declined");
            assert_eq!(versions(&mut app, &seeded.workflow).len(), 1);
            assert!(!stored_findings(&mut app, &seeded.cycle)[0].accepted);
            assert_eq!(
                stored_cycle(&mut app, &seeded.run).status,
                ReviewCycleStatus::Declined
            );
        }

        /// The attribution P5 stamped on the seed survives all the way into
        /// the minted version's own `change_summary`: an evidence-only finding
        /// is never presented as something a teammate said.
        #[test]
        fn the_minted_versions_summary_says_where_each_finding_came_from() {
            let mut app = app();
            let seeded = seed(
                &mut app,
                vec![finding_seed(
                    "plan",
                    "prompt",
                    "improve",
                    serde_json::json!({"role": "planner"}),
                )],
                ReviewCycleStatus::AwaitingUser,
            );
            apply(&mut app, &seeded.run, &["plan"]);
            let summary = versions(&mut app, &seeded.workflow)[0].3.clone();
            assert!(
                summary.contains("evidence only for builder: no_session_id"),
                "{summary}"
            );
            assert!(!summary.contains("own account"), "{summary}");
        }

        /// Two findings, one node key, one accept: acceptance is per node key,
        /// which is the granularity the store marks and the CLI offers.
        #[test]
        fn accepting_a_node_key_accepts_every_finding_about_that_node() {
            let mut app = app();
            let seeded = seed(
                &mut app,
                vec![
                    finding_seed(
                        "plan",
                        "prompt",
                        "improve",
                        serde_json::json!({"role": "planner"}),
                    ),
                    finding_seed(
                        "plan",
                        "structural",
                        "improve",
                        serde_json::json!({"demand": "peak"}),
                    ),
                ],
                ReviewCycleStatus::AwaitingUser,
            );
            apply(&mut app, &seeded.run, &["plan", "plan"]);
            let head = head_nodes(&mut app, &seeded.workflow);
            let plan = head
                .iter()
                .find(|node| node.key.as_str() == "plan")
                .unwrap();
            assert_eq!(plan.role, "planner");
            assert_eq!(plan.demand, Demand::Peak);
            assert!(stored_findings(&mut app, &seeded.cycle)
                .iter()
                .all(|finding| finding.accepted));
        }

        // ── refusals ───────────────────────────────────────────────────────

        #[test]
        fn a_run_that_does_not_exist_is_refused() {
            let mut app = app();
            let response = apply(&mut app, &RunId::new("workflow_run:nope"), &[]);
            assert_eq!(error_code(&response), "workflow_not_found");
        }

        #[test]
        fn a_run_with_no_review_cycle_is_refused() {
            let mut app = app();
            let seeded = seed(&mut app, Vec::new(), ReviewCycleStatus::AwaitingUser);
            let other = {
                let workflow = seeded.workflow.clone();
                let version = seeded.version.clone();
                app.workflow_store
                    .call(move |cx| {
                        let kvdag = cx.block_on(cx.store().load_version(&version))?;
                        cx.block_on(cx.store().create_run(new_run(&workflow, &kvdag)))
                    })
                    .expect("the store answers")
                    .expect("a second run with no review cycle")
            };
            let response = apply(&mut app, &other, &[]);
            assert_eq!(error_code(&response), WORKFLOW_REVIEW_NOT_FOUND_CODE);
        }

        /// A cycle that is still running, or one already decided, is not
        /// applicable — and an already-applied cycle must never mint a second
        /// version from the same findings.
        #[test]
        fn a_cycle_that_is_not_awaiting_the_user_is_refused() {
            for status in [
                ReviewCycleStatus::Running,
                ReviewCycleStatus::Applied,
                ReviewCycleStatus::Declined,
                ReviewCycleStatus::Failed,
            ] {
                let mut app = app();
                let seeded = seed(
                    &mut app,
                    vec![finding_seed(
                        "plan",
                        "prompt",
                        "improve",
                        serde_json::json!({"role": "planner"}),
                    )],
                    status,
                );
                let response = apply(&mut app, &seeded.run, &["plan"]);
                assert_eq!(
                    error_code(&response),
                    WORKFLOW_REVIEW_NOT_AWAITING_CODE,
                    "{status:?}"
                );
                assert_eq!(versions(&mut app, &seeded.workflow).len(), 1, "{status:?}");
            }
        }

        /// Applying twice is the same refusal: the first apply closed the
        /// cycle, so the second cannot mint a second version.
        #[test]
        fn a_second_apply_of_the_same_cycle_is_refused() {
            let mut app = app();
            let seeded = seed(
                &mut app,
                vec![finding_seed(
                    "plan",
                    "prompt",
                    "improve",
                    serde_json::json!({"role": "planner"}),
                )],
                ReviewCycleStatus::AwaitingUser,
            );
            result(&apply(&mut app, &seeded.run, &["plan"]));
            let response = apply(&mut app, &seeded.run, &["plan"]);
            assert_eq!(error_code(&response), WORKFLOW_REVIEW_NOT_AWAITING_CODE);
            assert_eq!(versions(&mut app, &seeded.workflow).len(), 2);
        }

        /// A typo'd node key must not silently decline everything.
        #[test]
        fn accepting_a_finding_this_cycle_never_produced_is_refused() {
            let mut app = app();
            let seeded = seed(
                &mut app,
                vec![finding_seed(
                    "plan",
                    "prompt",
                    "improve",
                    serde_json::json!({"role": "planner"}),
                )],
                ReviewCycleStatus::AwaitingUser,
            );
            let response = apply(&mut app, &seeded.run, &["pIan"]);
            assert_eq!(error_code(&response), "workflow_invalid_argument");
            assert_eq!(
                stored_cycle(&mut app, &seeded.run).status,
                ReviewCycleStatus::AwaitingUser,
                "the cycle is still there to decide"
            );
        }

        /// A compile refusal fails the *apply*: nothing is written, the cycle
        /// stays `awaiting_user`, and the smaller set the human is invited to
        /// accept still works.
        ///
        /// The finding here is about `.lead` — a real reviewable agent that is
        /// deliberately not a definition node, so there is nothing to apply it
        /// to and karvex says so rather than dropping it.
        #[test]
        fn a_compile_refusal_fails_the_apply_and_leaves_the_cycle_open() {
            let mut app = app();
            let seeded = seed(
                &mut app,
                vec![
                    finding_seed(
                        ".lead",
                        "prompt",
                        "improve",
                        serde_json::json!({"role": "lead"}),
                    ),
                    finding_seed(
                        "implement",
                        "structural",
                        "improve",
                        serde_json::json!({"max_attempts": 4}),
                    ),
                ],
                ReviewCycleStatus::AwaitingUser,
            );

            let response = apply(&mut app, &seeded.run, &[".lead"]);
            assert_eq!(error_code(&response), WORKFLOW_REVIEW_COMPILE_FAILED_CODE);
            let body: serde_json::Value = serde_json::from_str(&response).unwrap();
            assert!(
                body["error"]["message"].as_str().unwrap().contains(".lead"),
                "the refusal names the finding: {body}"
            );
            assert_eq!(
                versions(&mut app, &seeded.workflow).len(),
                1,
                "nothing minted"
            );
            assert_eq!(
                stored_cycle(&mut app, &seeded.run).status,
                ReviewCycleStatus::AwaitingUser
            );
            assert!(stored_findings(&mut app, &seeded.cycle)
                .iter()
                .all(|finding| !finding.accepted));

            // And the smaller set the human is invited to accept works.
            let response = apply(&mut app, &seeded.run, &["implement"]);
            assert!(result(&response)["version_id"].as_str().is_some());
        }

        /// A `replace` with no `replacement` cannot even be a row: the store's
        /// own `review_finding_replace_requires_replacement` event refuses the
        /// write (`0001_init.surql`), which is why the compiler's identical
        /// refusal is a second line of defence rather than the only one.
        /// Pinned here because that ordering is what makes "a replace without
        /// a replacement never reaches the store" true of the whole path.
        #[test]
        fn a_replace_without_a_replacement_is_refused_by_the_store_before_apply_ever_sees_it() {
            let mut app = app();
            let seeded = seed(&mut app, Vec::new(), ReviewCycleStatus::AwaitingUser);
            let cycle = seeded.cycle.clone();
            let refused = app
                .workflow_store
                .call(move |cx| {
                    cx.block_on(cx.store().write(StoreWrite::ReviewFindings {
                        cycle,
                        findings: vec![finding_seed(
                            "plan",
                            "structural",
                            "replace",
                            serde_json::json!({}),
                        )],
                    }))
                })
                .expect("the store answers");
            assert!(refused.is_err(), "the write is refused: {refused:?}");
            assert!(stored_findings(&mut app, &seeded.cycle).is_empty());
        }

        /// The compiled document passes the same authoring validation a
        /// hand-written one does — including P14's `isolation = "worktree"`
        /// rejection.
        #[test]
        fn a_finding_that_would_author_worktree_isolation_is_refused() {
            let mut app = app();
            let mut replace = finding_seed("plan", "structural", "replace", serde_json::json!({}));
            replace.replacement = Some(serde_json::json!({
                "key": "plan",
                "label": "Plan",
                "prompt_template": "Plan for: {{goal}}",
                "isolation": "worktree",
            }));
            let seeded = seed(&mut app, vec![replace], ReviewCycleStatus::AwaitingUser);
            let response = apply(&mut app, &seeded.run, &["plan"]);
            assert_eq!(error_code(&response), WORKFLOW_REVIEW_COMPILE_FAILED_CODE);
            assert_eq!(versions(&mut app, &seeded.workflow).len(), 1);
        }

        /// A finding whose stored vocabulary is outside the closed set is a
        /// refusal, not a skipped row: the human accepted it, and karvex will
        /// not guess what a level it does not know was supposed to mean.
        ///
        /// Tested against `accepted_findings` directly because the store's own
        /// `review_finding.level` ASSERT (`0001_init.surql`) makes the value
        /// unwritable through the writer — this check is the second line, for
        /// a row written by some future schema whose vocabulary widened.
        #[test]
        fn a_finding_with_an_unknown_level_or_verdict_is_refused() {
            let record = |level: &str, verdict: &str| ReviewFindingRecord {
                id: "review_finding:1".to_string(),
                cycle: ReviewCycleId::new("review_cycle:t1"),
                run_node: None,
                node_key: NodeKey::new("plan"),
                interview: None,
                interview_mode: InterviewMode::EvidenceOnly,
                level: level.to_string(),
                verdict: verdict.to_string(),
                rationale: String::new(),
                evidence: serde_json::json!({}),
                proposed_change: serde_json::json!({}),
                replacement: None,
                accepted: false,
                applied_in: None,
            };
            let accept = vec!["plan".to_string()];
            let error = accepted_findings(&[record("vibes", "improve")], &accept)
                .expect_err("an unknown level is refused");
            assert!(error.contains("vibes") && error.contains("plan"), "{error}");
            let error = accepted_findings(&[record("prompt", "delete")], &accept)
                .expect_err("an unknown verdict is refused");
            assert!(error.contains("delete"), "{error}");
            // And the known vocabulary still parses.
            let accepted = accepted_findings(&[record("prompt", "keep")], &accept)
                .expect("the closed vocabulary parses");
            assert_eq!(accepted.len(), 1);
            assert_eq!(
                accepted[0].verdict,
                crate::workflow::review::FindingVerdict::Keep
            );
        }

        // ── the restart property ───────────────────────────────────────────

        /// The strengthened `08` scenario 6: this handler never asks whether a
        /// run is live. Nothing in `App` is bound to the run — no
        /// `workflow_lead`, no pane, no resident graph — and the apply still
        /// succeeds, which is what makes "kill the server after
        /// `review.ready`, restart, apply" work.
        #[test]
        fn an_apply_needs_no_live_run_on_this_server() {
            let mut app = app();
            let seeded = seed(
                &mut app,
                vec![finding_seed(
                    "implement",
                    "structural",
                    "improve",
                    serde_json::json!({"demand": "critical", "timeout_ms": 600000}),
                )],
                ReviewCycleStatus::AwaitingUser,
            );
            assert!(
                app.workflow_lead.is_none(),
                "the fixture never started a lead run, exactly like a restarted server"
            );
            let result = result(&apply(&mut app, &seeded.run, &["implement"]));
            assert!(result["version_id"].as_str().is_some());
            let head = head_nodes(&mut app, &seeded.workflow);
            let implement = head
                .iter()
                .find(|node| node.key.as_str() == "implement")
                .unwrap();
            assert_eq!(implement.demand, Demand::Critical);
            assert_eq!(implement.timeout_ms, Some(600_000));
        }

        /// The parent is the run's version even when the workflow's head has
        /// moved on since the run — a human's later edit is not what the
        /// review looked at.
        #[test]
        fn the_parent_is_the_runs_version_even_when_the_head_moved_on() {
            let mut app = app();
            let seeded = seed(
                &mut app,
                vec![finding_seed(
                    "plan",
                    "prompt",
                    "improve",
                    serde_json::json!({"role": "planner"}),
                )],
                ReviewCycleStatus::AwaitingUser,
            );
            // A hand-authored v2 lands while the review is being decided.
            let workflow = seeded.workflow.clone();
            let authored_v2 = app
                .workflow_store
                .call(move |cx| {
                    let mut edited = spec();
                    edited.nodes[0].label = "Plan, by hand".to_string();
                    let kvdag = cx.block_on(cx.store().create_version(
                        &workflow,
                        VersionOrigin::Authored,
                        "hand edit",
                        edited,
                    ))?;
                    cx.block_on(cx.store().set_head_version(&workflow, &kvdag.version_id))?;
                    Ok::<_, StoreError>(kvdag.version_id)
                })
                .expect("the store answers")
                .expect("v2 is authored");

            apply(&mut app, &seeded.run, &["plan"]);
            let chain = versions(&mut app, &seeded.workflow);
            let (version, origin, parent, _) = chain[0].clone();
            assert_eq!(version, 3);
            assert_eq!(origin, "self_improvement");
            assert_eq!(
                parent.as_deref(),
                Some(seeded.version.as_str()),
                "parented on the run's version, not on the head {authored_v2}"
            );
            // And the hand edit is not carried into it: the compiled document
            // is the run's version plus the accepted findings, nothing else.
            let head = head_nodes(&mut app, &seeded.workflow);
            let plan = head
                .iter()
                .find(|node| node.key.as_str() == "plan")
                .unwrap();
            assert_eq!(plan.label, "plan");
            assert_eq!(plan.role, "planner");
        }
    }
}
