//! The review cycle's IO adapter: plan, interview, collect, synthesise.
//!
//! `phase4-retarget-plan.md` §3.5 and §5 packet **P10**. The decisions are pure
//! and already landed in [`crate::workflow::review`] and
//! [`crate::workflow::review_prompt`] (packet P5): who is worth interviewing,
//! in which mode, what each interview is asked, and how an answer is
//! attributed. The argv, the env, and the cycle-directory layout are P6's
//! ([`crate::workflow::binding::review`]). This module is the half that spawns
//! the panes, seeds them, watches them, and writes the `review_cycle` /
//! `review_finding` rows the store gained a writer for in P7.
//!
//! ## Why this poller is not gated on a live run
//!
//! A review interviews a run that has already ended — that is the whole point
//! of it — so by the time a cycle exists there is no `workflow_lead` left to
//! hang a poll off. [`crate::app::App::poll_run_projection`] therefore calls
//! this outside the live-run gate, and the event loop folds
//! [`crate::app::App::review_cycle_deadline`] into its own deadline so a server
//! with no live run still wakes for an interview that is running. Nothing in
//! this file may read `self.workflow_lead` to decide whether to do its work.
//!
//! ## The three couplings P5 named, and where each one lives here
//!
//! 1. **An answered *resumed* interview with no `interrogation` row is
//!    unattributable.** [`App::spawn_review_interview`] writes
//!    [`StoreWrite::InterrogationStarted`] for every resumed interview and
//!    records its id in [`LiveReviewCycle::interrogations`], which is the map
//!    handed to [`ReviewCycleState::attribution`] — never an empty one.
//! 2. **[`InterviewMode`] is immutable.** A member planned resumed whose
//!    interview then failed is written `evidence_only` *by the attribution*,
//!    and this file never rewrites the plan's assignment to say otherwise.
//! 3. **`Answered` is not `attributable`.** Every finding's mode comes from
//!    [`crate::workflow::review::Attribution`], never from
//!    [`InterviewPhase`].
//!
//! ## What the run's status is never touched by
//!
//! Nothing here. A review cycle reads a finished run and writes `review_cycle`,
//! `review_finding` and `interrogation` rows; there is no `StoreWrite` in this
//! file that can reach a `workflow_run` row's status, and a running cycle does
//! not block a new run (§6 D-4).
//!
//! Boundary note (`AGENTS.md`): a review cycle is a shared runtime fact, stored
//! and served over the JSON API. The overlay that displays one is the client's
//! business and does not belong here.

use std::time::Instant;

// Everything below the two poller seams is the review cycle itself, and the
// review cycle is the workflow subsystem: it reads the store, writes
// `review_cycle` rows, and spawns panes for a run the store knows about. With
// `--no-default-features` there is no store to read, so the whole orchestration
// — not a stubbed-out shell of it — is compiled away, and the seams answer
// "nothing in flight" because nothing can be.
#[cfg(feature = "workflow")]
use std::collections::BTreeMap;
#[cfg(feature = "workflow")]
use std::path::{Path, PathBuf};
#[cfg(feature = "workflow")]
use std::time::Duration;

#[cfg(feature = "workflow")]
use tracing::{debug, warn};

#[cfg(feature = "workflow")]
use crate::api::schema::{EventData, EventKind};
#[cfg(feature = "workflow")]
use crate::workflow::binding::review as binding;
#[cfg(feature = "workflow")]
use crate::workflow::model::{
    InstancePath, InterrogationId, InterviewMode, KvdagVersionId, NodeKey, NoticeLevel,
    PublicPaneId, ReviewCycleId, ReviewCycleStatus, RunId, RunStatus, StoreWrite, UserNotice,
};
#[cfg(feature = "workflow")]
use crate::workflow::review::{
    Attribution, InterviewPaneState, InterviewPhase, ObservedInterview, ReviewCycleState,
    ReviewPlan, ReviewPolicy, RunEvidence, SynthesisOutcome,
};
#[cfg(feature = "workflow")]
use crate::workflow::review_prompt::{
    self, InterviewAnswer, InterviewPromptInput, SynthesisPromptInput, SynthesisSource,
};

/// How often a live review cycle re-reads its interview panes.
///
/// The same 2 s cadence the run projection uses
/// ([`crate::app::workflow_lead::RUN_PROJECTION_INTERVAL`]): both are polling a
/// handful of in-memory terminal states, and a review that noticed a finished
/// interview a minute late would hold the whole cycle up for no reason.
#[cfg(feature = "workflow")]
pub(crate) const REVIEW_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The run id an interview or synthesis pane reports against.
///
/// **Not** [`crate::workflow::binding::spawn::RUN_ID_ENV_VAR`], deliberately:
/// that variable is what makes `kvx workflow run finish` need no argument, and
/// an interview pane that carried it could close the very run it is reviewing.
/// P6 froze that hazard as a test on `interview_env`; this is the review-scoped
/// replacement, so `kvx workflow review answer|report` can still name its own
/// run without inheriting the lead's authority.
///
/// (Plan defect, reported with this packet: P6's `interview_env` doc says the
/// answer handler "resolves the run and the bound member from the interrogation
/// row this value addresses", but P3 froze `run_id` + `member` as required wire
/// params and an `interrogation` row carries neither a member name nor — for an
/// evidence-only interview — any row at all. These two variables are the
/// smallest honest way to make the frozen wire shape callable from the pane.)
#[cfg(feature = "workflow")]
pub(crate) const REVIEW_RUN_ID_ENV_VAR: &str = "KARVEX_WORKFLOW_REVIEW_RUN_ID";

/// The team-roster name an interview pane answers for. Absent in the synthesis
/// pane, which is what tells `kvx` whether it is answering an interview or
/// reporting findings.
#[cfg(feature = "workflow")]
pub(crate) const REVIEW_MEMBER_ENV_VAR: &str = "KARVEX_WORKFLOW_REVIEW_MEMBER";

/// The cycle a synthesis pane reports findings for.
#[cfg(feature = "workflow")]
pub(crate) const REVIEW_CYCLE_ENV_VAR: &str = "KARVEX_WORKFLOW_REVIEW_CYCLE";

/// What the pane list calls the synthesis pane.
#[cfg(feature = "workflow")]
const SYNTHESIS_PANE_TITLE: &str = "review: synthesis";

/// The name a synthesised lead member takes when the run's team roster never
/// recorded one — the `.lead` node is a real `run_node` (P8) and is always
/// interviewable, even for a run whose team config was gone before the
/// projection could name its lead.
#[cfg(feature = "workflow")]
const FALLBACK_LEAD_NAME: &str = "lead";

/// One interview pane, as this server holds it between polls.
#[cfg(feature = "workflow")]
#[derive(Debug)]
struct InterviewPane {
    pane_id: String,
    terminal_id: crate::terminal::TerminalId,
    /// When the pane was launched. The interview deadline runs from here
    /// ([`ObservedInterview::started_at_unix_ms`]), not from the cycle's start.
    started_at_unix_ms: u64,
    /// The one `agent.prompt` seeding, latched: no positional prompt, ever
    /// (S2 amendment, `binding::review::interview_seed_prompt`).
    seeded: bool,
    seed_prompt: String,
    /// Whether the fork's own session id and transcript have been copied onto
    /// the interrogation row yet. Polled rather than read at spawn, because
    /// `transcript_path` does not exist until the fork's first turn (S2).
    identity_recorded: bool,
}

/// The synthesis pane. At most one is alive at a time; a failed attempt is
/// replaced, never run alongside its retry.
#[cfg(feature = "workflow")]
#[derive(Debug)]
struct SynthesisPane {
    #[allow(dead_code)] // held for parity with the interview panes and for logs
    pane_id: String,
    terminal_id: crate::terminal::TerminalId,
    seeded: bool,
    seed_prompt: String,
}

/// Where the cycle's synthesis half is.
#[cfg(feature = "workflow")]
#[derive(Debug)]
enum SynthesisState {
    /// Interviews are still running.
    Waiting,
    Running(Box<SynthesisPane>),
    /// `workflow.review.report` landed and the findings were written.
    Reported,
}

/// One review cycle this server is running.
///
/// Deliberately small, and deliberately *not* a mirror of the `review_cycle`
/// row: the row is the durable lifecycle and the store owns it; this is the
/// in-memory fold that decides when those writes happen. Everything a restarted
/// server needs to answer `workflow.review.get` is already in the store — what
/// is lost on restart is only the ability to keep *driving* the cycle, which is
/// honest, because the panes are gone too.
#[cfg(feature = "workflow")]
#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct LiveReviewCycle {
    pub(crate) cycle_id: ReviewCycleId,
    pub(crate) run_id: RunId,
    cycle_dir: PathBuf,
    /// The workspace the cycle's panes are spawned into, resolved once at
    /// start so a later focus change cannot scatter one cycle across two.
    ws_idx: usize,
    plan: ReviewPlan,
    state: ReviewCycleState,
    /// The `(model, effort)` every pane of this cycle runs at, resolved once
    /// from the run's own tier so the synthesis pane cannot silently differ
    /// from the interviews it reads.
    assignment: crate::workflow::tier::Assignment,
    evidence: RunEvidence,
    node_keys: Vec<NodeKey>,
    run_nodes: BTreeMap<NodeKey, InstancePath>,
    interviews: BTreeMap<String, InterviewPane>,
    /// Resumed interviews only (coupling 1). An evidence-only interview has no
    /// `interrogation` row because there was no session to fork.
    interrogations: BTreeMap<String, InterrogationId>,
    answers: BTreeMap<String, InterviewAnswer>,
    synthesis: SynthesisState,
    /// CI's escape hatch, resolved once at start (`E-11`'s read-once rule).
    command_override: Option<Vec<String>>,
    closed: bool,
    next_poll_at: Option<Instant>,
}

#[cfg(feature = "workflow")]
#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
impl LiveReviewCycle {
    fn poll_due(&self, now: Instant) -> bool {
        !self.closed && self.next_poll_at.is_none_or(|due| now >= due)
    }

    fn rearm(&mut self, now: Instant) {
        self.next_poll_at = Some(now + REVIEW_POLL_INTERVAL);
    }

    fn next_poll_deadline(&self) -> Option<Instant> {
        if self.closed {
            return None;
        }
        Some(self.next_poll_at.unwrap_or_else(Instant::now))
    }

    /// The attribution table, built from the interrogation rows that actually
    /// exist. Coupling 1 in one line: this map is what separates "answered" from
    /// "attributable".
    fn attribution(&self) -> Attribution {
        self.state.attribution(&self.interrogations)
    }
}

/// A refusal a review verb answers with: the wire code and the sentence.
#[cfg(feature = "workflow")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewRefusal {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

#[cfg(feature = "workflow")]
#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
impl ReviewRefusal {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

// ── the poller seams (§5 P8: outside the live-run gate, always) ────────────

#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
impl crate::app::App {
    /// One tick over every review cycle this server is running.
    ///
    /// Called from [`crate::app::App::poll_run_projection`] on **every** tick,
    /// with no live run required (P8's plan-defect fix). Returns whether
    /// anything changed, so the caller knows whether to re-render.
    pub(crate) fn poll_review_cycles(&mut self, now: Instant) -> bool {
        #[cfg(feature = "workflow")]
        {
            self.poll_review_cycles_impl(now)
        }
        #[cfg(not(feature = "workflow"))]
        {
            let _ = now;
            false
        }
    }

    /// When the review poller next needs the loop to wake it.
    ///
    /// `None` means nothing is in flight. Folded into the loop's
    /// min-of-all-deadlines like every other periodic task, and deliberately
    /// *not* gated on a connected client: a review is server-owned and runs
    /// headless.
    pub(crate) fn review_cycle_deadline(&self) -> Option<Instant> {
        #[cfg(feature = "workflow")]
        {
            self.workflow_reviews
                .iter()
                .filter_map(LiveReviewCycle::next_poll_deadline)
                .min()
        }
        #[cfg(not(feature = "workflow"))]
        {
            None
        }
    }
}

#[cfg(feature = "workflow")]
#[allow(clippy::too_many_lines)] // the cycle's lifecycle reads as one story
impl crate::app::App {
    fn poll_review_cycles_impl(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for index in 0..self.workflow_reviews.len() {
            changed |= self.poll_review_cycle(index, now);
        }
        self.workflow_reviews.retain(|cycle| !cycle.closed);
        changed
    }

    fn poll_review_cycle(&mut self, index: usize, now: Instant) -> bool {
        let Some(cycle) = self.workflow_reviews.get(index) else {
            return false;
        };
        if !cycle.poll_due(now) {
            return false;
        }
        if let Some(cycle) = self.workflow_reviews.get_mut(index) {
            cycle.rearm(now);
        }

        let mut changed = self.seed_review_panes(index);
        changed |= self.record_interview_identities(index);
        changed |= self.absorb_review_interviews(index);
        changed |= self.advance_review_synthesis(index);
        changed
    }

    /// Hands each pane its one instruction, once its agent is up.
    ///
    /// S2's rule, non-negotiable: the prompt is delivered through
    /// `agent.prompt`, never as a positional argument on the argv. A pane
    /// spawned through `KARVEX_WORKFLOW_REVIEW_COMMAND` is marked seeded at
    /// spawn instead — a CI stub is not a managed agent and would never report
    /// itself ready.
    fn seed_review_panes(&mut self, index: usize) -> bool {
        let mut ready: Vec<(String, String)> = Vec::new();
        {
            let Some(cycle) = self.workflow_reviews.get(index) else {
                return false;
            };
            for pane in cycle.interviews.values() {
                if pane.seeded {
                    continue;
                }
                if self.pane_agent_is_ready(&pane.terminal_id) {
                    ready.push((pane.pane_id.clone(), pane.seed_prompt.clone()));
                }
            }
            if let SynthesisState::Running(pane) = &cycle.synthesis {
                if !pane.seeded && self.pane_agent_is_ready(&pane.terminal_id) {
                    ready.push((pane.pane_id.clone(), pane.seed_prompt.clone()));
                }
            }
        }
        if ready.is_empty() {
            return false;
        }
        if let Some(cycle) = self.workflow_reviews.get_mut(index) {
            for pane in cycle.interviews.values_mut() {
                if ready.iter().any(|(id, _)| *id == pane.pane_id) {
                    pane.seeded = true;
                }
            }
            if let SynthesisState::Running(pane) = &mut cycle.synthesis {
                if ready.iter().any(|(id, _)| *id == pane.pane_id) {
                    pane.seeded = true;
                }
            }
        }
        for (pane_id, text) in ready {
            debug!(pane = %pane_id, "seeding a review pane");
            let _ = self.dispatch_runtime_mutation(
                "workflow.review.seed",
                crate::api::schema::Method::AgentPrompt(crate::api::schema::AgentPromptParams {
                    target: pane_id,
                    text,
                    wait: None,
                }),
            );
        }
        true
    }

    fn pane_agent_is_ready(&self, terminal_id: &crate::terminal::TerminalId) -> bool {
        self.state
            .terminals
            .get(terminal_id)
            .is_some_and(crate::terminal::TerminalState::managed_agent_interactive_ready)
    }

    /// Copies each fork's own identity onto its `interrogation` row.
    ///
    /// S2: `transcript_path` does not exist until the fork's first turn, and the
    /// fork's session id is a *different* id from the member's — so both are
    /// polled here and written through [`StoreWrite::InterrogationUpdate`],
    /// never read at spawn.
    fn record_interview_identities(&mut self, index: usize) -> bool {
        let mut updates: Vec<(String, InterrogationId, String)> = Vec::new();
        {
            let Some(cycle) = self.workflow_reviews.get(index) else {
                return false;
            };
            for (member, pane) in &cycle.interviews {
                if pane.identity_recorded {
                    continue;
                }
                let Some(interrogation) = cycle.interrogations.get(member) else {
                    continue;
                };
                let Some(session_id) = self.pane_agent_session_id(&pane.terminal_id) else {
                    continue;
                };
                updates.push((member.clone(), interrogation.clone(), session_id));
            }
        }
        if updates.is_empty() {
            return false;
        }
        for (member, interrogation, forked_session_id) in updates {
            if let Some(cycle) = self.workflow_reviews.get_mut(index) {
                if let Some(pane) = cycle.interviews.get_mut(&member) {
                    pane.identity_recorded = true;
                }
            }
            self.persist_review_write(StoreWrite::InterrogationUpdate {
                id: interrogation,
                forked_session_id: Some(forked_session_id),
                ended_at_unix_ms: None,
            });
        }
        true
    }

    /// The claude session id karvex's own per-pane hook recorded for a pane it
    /// owns the terminal id of. The fork's id, not the member's.
    fn pane_agent_session_id(&self, terminal_id: &crate::terminal::TerminalId) -> Option<String> {
        let session = self
            .state
            .terminals
            .get(terminal_id)?
            .persisted_agent_session
            .as_ref()?;
        (session.agent == "claude"
            && session.session_ref.kind == crate::agent_resume::AgentSessionRefKind::Id)
            .then(|| session.session_ref.value.clone())
    }

    /// One observation per interview, folded through the pure core.
    fn absorb_review_interviews(&mut self, index: usize) -> bool {
        let now_unix_ms = crate::app::workflow::current_unix_ms();
        let observed: Vec<ObservedInterview> = {
            let Some(cycle) = self.workflow_reviews.get(index) else {
                return false;
            };
            if cycle.state.interviews_settled() {
                return false;
            }
            cycle
                .interviews
                .iter()
                .map(|(member, pane)| ObservedInterview {
                    member: member.clone(),
                    started_at_unix_ms: pane.started_at_unix_ms,
                    pane_alive: self.state.terminals.contains_key(&pane.terminal_id),
                    pane_state: self.interview_pane_state(&pane.terminal_id),
                    // An answer is what `workflow.review.answer` recorded after
                    // parsing it. A file on disk karvex could not parse is not
                    // an answer, and neither is one nobody reported.
                    answer_recorded: cycle.answers.contains_key(member),
                })
                .collect()
        };

        let Some(cycle) = self.workflow_reviews.get_mut(index) else {
            return false;
        };
        let delta = cycle.state.absorb(&observed, now_unix_ms);
        if delta.is_empty() {
            return false;
        }
        for transition in &delta.interviews {
            debug!(
                cycle = %cycle.cycle_id,
                member = %transition.member,
                from = transition.from.as_str(),
                to = transition.to.as_str(),
                "review interview changed phase",
            );
        }
        for member in &delta.unknown_members {
            warn!(cycle = %cycle.cycle_id, %member, "a review pane karvex does not recognise");
        }
        true
    }

    fn interview_pane_state(
        &self,
        terminal_id: &crate::terminal::TerminalId,
    ) -> Option<InterviewPaneState> {
        let terminal = self.state.terminals.get(terminal_id)?;
        Some(match terminal.state {
            crate::detect::AgentState::Working => InterviewPaneState::Working,
            crate::detect::AgentState::Idle => InterviewPaneState::Idle,
            // `blocked` is a first-class interview outcome, not "slow"
            // (S2 amendment 2): a fork sitting on a permission dialog it will
            // never get answered has to end the interview, not extend it.
            crate::detect::AgentState::Blocked => InterviewPaneState::Blocked,
            crate::detect::AgentState::Unknown => InterviewPaneState::Unknown,
        })
    }

    /// Spawns synthesis when the last interview settles, and runs §3.5's
    /// failure ladder over it: a synthesis pane that dies without reporting is
    /// retried once, and a second failure fails the cycle.
    fn advance_review_synthesis(&mut self, index: usize) -> bool {
        enum Next {
            Nothing,
            Spawn,
            Failed,
        }
        let next = {
            let Some(cycle) = self.workflow_reviews.get(index) else {
                return false;
            };
            match &cycle.synthesis {
                SynthesisState::Reported => Next::Nothing,
                SynthesisState::Waiting => {
                    if cycle.state.interviews_settled() {
                        Next::Spawn
                    } else {
                        Next::Nothing
                    }
                }
                SynthesisState::Running(pane) => {
                    if self.state.terminals.contains_key(&pane.terminal_id) {
                        Next::Nothing
                    } else {
                        Next::Failed
                    }
                }
            }
        };
        match next {
            Next::Nothing => false,
            Next::Spawn => self.start_review_synthesis(index),
            Next::Failed => {
                let outcome = match self.workflow_reviews.get_mut(index) {
                    Some(cycle) => {
                        cycle.synthesis = SynthesisState::Waiting;
                        cycle.state.record_synthesis_failure()
                    }
                    None => return false,
                };
                match outcome {
                    SynthesisOutcome::Retry => {
                        warn!("the review synthesis pane exited without reporting; retrying once");
                        self.start_review_synthesis(index)
                    }
                    SynthesisOutcome::FailCycle => {
                        self.fail_review_cycle(
                            index,
                            "the review synthesis never reported findings",
                        );
                        true
                    }
                }
            }
        }
    }

    /// Renders the synthesis document over every answer the cycle collected and
    /// spawns the pane that turns it into findings.
    ///
    /// The attribution table is built *here*, from the interrogation rows that
    /// actually exist, and is handed to the prompt: what a synthesiser may claim
    /// about a member is karvex's decision, never the synthesiser's (coupling 3).
    fn start_review_synthesis(&mut self, index: usize) -> bool {
        let Some(cycle) = self.workflow_reviews.get(index) else {
            return false;
        };
        let attribution = cycle.attribution();
        let sources: Vec<SynthesisSource> = cycle
            .plan
            .assignments
            .iter()
            .map(|assignment| SynthesisSource {
                member: assignment.member.clone(),
                is_lead: assignment.is_lead,
                attribution: attribution.resolve(Some(assignment.member.as_str())),
                answer: cycle.answers.get(&assignment.member).cloned(),
            })
            .collect();
        let cycle_dir = cycle.cycle_dir.clone();
        let findings_path = binding::findings_path(&cycle_dir);
        let document = review_prompt::render_synthesis_prompt(&SynthesisPromptInput {
            run: &cycle.evidence,
            sources: &sources,
            node_keys: &cycle.node_keys,
            cycle_dir: &cycle_dir.to_string_lossy(),
            findings_path: &findings_path.to_string_lossy(),
        });
        let prompt_path = binding::synthesis_prompt_path(&cycle_dir);
        if let Err(error) = std::fs::write(&prompt_path, document) {
            warn!(%error, "the review synthesis document could not be written");
            self.fail_review_cycle(index, "the review synthesis document could not be written");
            return true;
        }

        let cycle_id = cycle.cycle_id.clone();
        let run_id = cycle.run_id.clone();
        let ws_idx = cycle.ws_idx;
        let argv = match &cycle.command_override {
            Some(argv) => argv.clone(),
            None => synthesis_argv(&self.synthesis_spawn_spec(index)),
        };
        let managed = cycle.command_override.is_none();
        let env = vec![
            (REVIEW_RUN_ID_ENV_VAR.to_string(), run_id.to_string()),
            (REVIEW_CYCLE_ENV_VAR.to_string(), cycle_id.to_string()),
        ];
        let spawned = self.spawn_review_pane(
            ws_idx,
            SYNTHESIS_PANE_TITLE,
            &cycle_dir,
            &argv,
            env,
            managed,
        );
        let (pane_id, terminal_id) = match spawned {
            Ok(spawned) => spawned,
            Err(error) => {
                warn!(%error, "the review synthesis pane could not be spawned");
                self.fail_review_cycle(index, "the review synthesis pane could not be spawned");
                return true;
            }
        };
        if let Some(cycle) = self.workflow_reviews.get_mut(index) {
            cycle.synthesis = SynthesisState::Running(Box::new(SynthesisPane {
                pane_id,
                terminal_id,
                seeded: !managed,
                seed_prompt: binding::interview_seed_prompt(&prompt_path),
            }));
        }
        true
    }

    /// The synthesis pane's spawn spec. It is not an interview — there is no
    /// member and no session to fork — but it is an ordinary `claude` in the
    /// same cycle directory, so it reuses P6's spec type and differs only in
    /// the one command it is allowed to run ([`synthesis_argv`]).
    fn synthesis_spawn_spec(&self, index: usize) -> binding::InterviewSpawnSpec {
        let cycle = &self.workflow_reviews[index];
        binding::InterviewSpawnSpec {
            member: "synthesis".to_string(),
            cycle_dir: cycle.cycle_dir.clone(),
            member_project_dir: None,
            assignment: cycle.assignment,
        }
    }

    /// `ReviewCycleUpdate{Failed}` + `workflow.review.closed` + one notice.
    /// The run's own status is untouched, structurally: no write in this file
    /// can reach it.
    fn fail_review_cycle(&mut self, index: usize, reason: &str) {
        let Some(cycle) = self.workflow_reviews.get_mut(index) else {
            return;
        };
        cycle.closed = true;
        let cycle_id = cycle.cycle_id.clone();
        let run_id = cycle.run_id.clone();
        self.persist_review_write(StoreWrite::ReviewCycleUpdate {
            id: cycle_id,
            status: Some(ReviewCycleStatus::Failed),
            ended_at_unix_ms: Some(crate::app::workflow::current_unix_ms()),
            resulting_version: None,
        });
        self.emit_review_event(EventKind::WorkflowReviewClosed, &run_id);
        self.show_workflow_notice(UserNotice {
            level: NoticeLevel::Warning,
            run: Some(run_id),
            path: None,
            message: format!("the run's review cycle failed: {reason}"),
        });
    }

    // ── starting a cycle ───────────────────────────────────────────────────

    /// `workflow.review.start`'s whole body, minus the wire encoding.
    ///
    /// Preconditions, in the order a user would ask them: the run exists, it is
    /// over, it has no cycle already in flight, and there is somebody to
    /// interview. Only then is anything written or spawned.
    pub(crate) fn start_review_cycle(
        &mut self,
        run_id: &RunId,
    ) -> Result<ReviewCycleId, ReviewRefusal> {
        if self
            .workflow_reviews
            .iter()
            .any(|cycle| !cycle.closed && cycle.run_id == *run_id)
        {
            return Err(ReviewRefusal::new(
                super::api::workflow_review::WORKFLOW_REVIEW_IN_FLIGHT_CODE,
                format!("a review cycle is already running for {run_id}"),
            ));
        }

        let inputs = self.load_review_inputs(run_id)?;
        if !run_status_is_terminal(inputs.status) {
            return Err(ReviewRefusal::new(
                super::api::workflow_review::WORKFLOW_REVIEW_RUN_NOT_TERMINAL_CODE,
                format!(
                    "{run_id} is still {}; a review interviews a run that has finished",
                    run_status_word(inputs.status),
                ),
            ));
        }
        if let Some(status) = inputs.existing_cycle {
            if matches!(
                status,
                ReviewCycleStatus::Running | ReviewCycleStatus::AwaitingUser
            ) {
                return Err(ReviewRefusal::new(
                    super::api::workflow_review::WORKFLOW_REVIEW_IN_FLIGHT_CODE,
                    format!(
                        "{run_id} already has a review cycle that is {}",
                        match status {
                            ReviewCycleStatus::AwaitingUser => "waiting for your decision",
                            _ => "running",
                        },
                    ),
                ));
            }
        }

        let policy = ReviewPolicy::from_config(self.workflow_policy.review_max_interviews);
        let plan = ReviewPlan::build(&inputs.identities, &inputs.evidence, &policy);
        if plan.is_empty() {
            return Err(ReviewRefusal::new(
                super::api::workflow_review::WORKFLOW_REVIEW_NO_INTERVIEWABLE_MEMBERS_CODE,
                format!(
                    "{run_id} has no interviewable members: karvex never recorded a team for it",
                ),
            ));
        }

        let ws_idx = self.review_workspace().ok_or_else(|| {
            ReviewRefusal::new(
                crate::workflow::binding::lead::LeadSpawnError::NoTargetPane.code(),
                crate::workflow::binding::lead::LeadSpawnError::NoTargetPane.to_string(),
            )
        })?;

        let command_override = match binding::parse_review_command_override(
            std::env::var(binding::REVIEW_COMMAND_ENV).ok().as_deref(),
        ) {
            Ok(binding::ReviewCommandOverride::Unset) => None,
            Ok(binding::ReviewCommandOverride::Command(argv)) => Some(argv),
            Err(error) => {
                return Err(ReviewRefusal::new(
                    super::api::workflow_review::WORKFLOW_REVIEW_NOT_FOUND_CODE,
                    error.notice,
                ))
            }
        };

        let started_at_unix_ms = crate::app::workflow::current_unix_ms();
        let cycle_id = mint_review_cycle_id(run_id, started_at_unix_ms);
        let run_dir = crate::workflow::binding::spawn::run_dir(
            &crate::workflow::binding::spawn::runs_root(),
            run_id,
        );
        let cycle_dir = binding::cycle_dir(&run_dir, &cycle_id);
        if let Err(error) = std::fs::create_dir_all(cycle_dir.join(binding::ANSWERS_DIR)) {
            return Err(ReviewRefusal::new(
                super::api::workflow_review::WORKFLOW_REVIEW_NOT_FOUND_CODE,
                format!("the review cycle directory could not be created: {error}"),
            ));
        }

        let assignment = interview_assignment(inputs.tier);
        let mut panes: BTreeMap<String, InterviewPane> = BTreeMap::new();
        let mut interrogations: BTreeMap<String, InterrogationId> = BTreeMap::new();
        let mut spawned_plan = ReviewPlan {
            assignments: Vec::new(),
            skipped: plan.skipped.clone(),
        };
        let mut pending_rows: Vec<StoreWrite> = Vec::new();

        for planned in &plan.assignments {
            let spec = binding::InterviewSpawnSpec {
                member: planned.member.clone(),
                cycle_dir: cycle_dir.clone(),
                member_project_dir: planned.member_cwd.as_ref().map(PathBuf::from),
                assignment,
            };
            let document = review_prompt::render_interview_prompt(&InterviewPromptInput {
                member: &planned.member,
                is_lead: planned.is_lead,
                mode: planned.mode,
                evidence_only_reason: planned.evidence_only_reason,
                run: &inputs.evidence,
                evidence: inputs.evidence.member(&planned.member),
                cycle_dir: &cycle_dir.to_string_lossy(),
                answer_path: &spec.answer_path().to_string_lossy(),
            });
            let prompt_path = spec.prompt_path();
            if let Err(error) = std::fs::write(&prompt_path, document) {
                warn!(member = %planned.member, %error, "an interview document could not be written");
                continue;
            }

            // The interrogation id is minted before the pane so the pane's env
            // can name it. The *row* is written only after the pane exists,
            // because `interrogation.pane_id` is not optional and a row naming
            // a pane that failed to spawn would be a record of something that
            // never happened.
            let interrogation = planned
                .is_resumed()
                .then(|| mint_interrogation_id(&cycle_id, &planned.member));
            let mut env = vec![
                (REVIEW_RUN_ID_ENV_VAR.to_string(), run_id.to_string()),
                (REVIEW_CYCLE_ENV_VAR.to_string(), cycle_id.to_string()),
                (REVIEW_MEMBER_ENV_VAR.to_string(), planned.member.clone()),
            ];
            if let Some(interrogation) = &interrogation {
                env.extend(binding::interview_env(&cycle_id, interrogation));
            }
            let argv = match (&command_override, &planned.source_session_id) {
                (Some(argv), _) => argv.clone(),
                (None, Some(session_id)) => binding::interview_argv(&spec, session_id),
                (None, None) => binding::evidence_only_argv(&spec),
            };
            let managed = command_override.is_none();
            let spawned =
                self.spawn_review_pane(ws_idx, &spec.pane_title(), &cycle_dir, &argv, env, managed);
            let (pane_id, terminal_id) = match spawned {
                Ok(spawned) => spawned,
                Err(error) => {
                    warn!(member = %planned.member, %error, "an interview pane could not be spawned");
                    continue;
                }
            };

            if let (Some(interrogation), Some(path), Some(session_id)) = (
                interrogation.as_ref(),
                inputs.interrogation_path(&planned.member),
                planned.source_session_id.as_ref(),
            ) {
                // Coupling 1: without this row an answered resumed interview is
                // unattributable, so it is enqueued for every resumed interview
                // that has a node to hang off — never skipped as an optimisation.
                interrogations.insert(planned.member.clone(), interrogation.clone());
                pending_rows.push(StoreWrite::InterrogationStarted {
                    id: interrogation.clone(),
                    run: run_id.clone(),
                    path,
                    source_session_id: session_id.clone(),
                    forked_session_id: None,
                    // The fork's own transcript does not exist until its first
                    // turn (S2), and `StoreWrite::InterrogationUpdate` — P1's
                    // shape, frozen before this packet — carries only the
                    // forked session id and an end stamp. So the column stays
                    // `NONE` rather than being filled with the *source*
                    // member's transcript, which would read as "this is the
                    // interview's record" and is not. Reported as a plan gap:
                    // `WorkflowReviewInfo.interview_paths` is empty until that
                    // write can carry a path.
                    transcript_path: None,
                    cwd: cycle_dir.to_string_lossy().into_owned(),
                    pane_id: PublicPaneId::new(pane_id.clone()),
                    reconstructed: false,
                    seeded_from_seq: None,
                    note: "review interview".to_string(),
                    started_at_unix_ms,
                });
            }

            panes.insert(
                planned.member.clone(),
                InterviewPane {
                    pane_id,
                    terminal_id,
                    started_at_unix_ms,
                    seeded: !managed,
                    seed_prompt: binding::interview_seed_prompt(&prompt_path),
                    identity_recorded: false,
                },
            );
            spawned_plan.assignments.push(planned.clone());
        }

        if spawned_plan.assignments.is_empty() {
            return Err(ReviewRefusal::new(
                super::api::workflow_review::WORKFLOW_REVIEW_NOT_FOUND_CODE,
                "no interview pane could be started for this review".to_string(),
            ));
        }

        self.persist_review_write(StoreWrite::ReviewCycleStarted {
            id: cycle_id.clone(),
            run: run_id.clone(),
            kvdag_version: inputs.version_id.clone(),
            started_at_unix_ms,
        });
        for row in pending_rows {
            self.persist_review_write(row);
        }

        let state = ReviewCycleState::from_plan(&spawned_plan, policy);
        self.workflow_reviews.push(LiveReviewCycle {
            cycle_id: cycle_id.clone(),
            run_id: run_id.clone(),
            cycle_dir,
            ws_idx,
            plan: spawned_plan,
            state,
            assignment,
            evidence: inputs.evidence,
            node_keys: inputs.node_keys,
            run_nodes: inputs.run_nodes,
            interviews: panes,
            interrogations,
            answers: BTreeMap::new(),
            synthesis: SynthesisState::Waiting,
            command_override,
            closed: false,
            next_poll_at: None,
        });

        self.emit_review_event(EventKind::WorkflowReviewStarted, run_id);
        let interviews = self
            .workflow_reviews
            .last()
            .map(|cycle| cycle.plan.assignments.len())
            .unwrap_or_default();
        self.show_workflow_notice(UserNotice {
            level: NoticeLevel::Info,
            run: Some(run_id.clone()),
            path: None,
            message: format!("review started: {interviews} interview(s) running"),
        });
        Ok(cycle_id)
    }

    /// The workspace a review's panes are opened in: the active one, exactly as
    /// a run's lead pane is placed.
    fn review_workspace(&self) -> Option<usize> {
        self.state.active.filter(|ws_idx| {
            self.state
                .workspaces
                .get(*ws_idx)
                .is_some_and(|workspace| !workspace.tabs.is_empty())
        })
    }

    /// Spawns one review pane: a split of the active workspace's focused pane,
    /// without stealing focus, in the cycle directory.
    ///
    /// The cwd is the **cycle directory**, never the member's project directory
    /// (S2 amendment 3): every fork mints its own team config keyed on its cwd,
    /// and starting these panes in the member's project dir would collide with
    /// `match_team`'s weak `LeadCwd` rule and scatter forked transcripts through
    /// the user's repository. The member's directory is granted through
    /// `--add-dir` by [`binding::interview_argv`] instead.
    ///
    /// `managed` is false for a `KARVEX_WORKFLOW_REVIEW_COMMAND` stub: a CI
    /// script is not an interactive agent, and telling karvex's detection it is
    /// would leave the pane permanently "starting".
    fn spawn_review_pane(
        &mut self,
        ws_idx: usize,
        title: &str,
        cwd: &Path,
        argv: &[String],
        env: Vec<(String, String)>,
        managed: bool,
    ) -> Result<(String, crate::terminal::TerminalId), String> {
        let target_pane = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.focused_pane_id())
            .ok_or_else(|| "no pane to split for the review".to_string())?;
        let (rows, cols) = self.state.estimate_pane_size();
        let workspace = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .ok_or_else(|| "no pane to split for the review".to_string())?;
        let spawned = workspace.split_pane_argv_command(
            target_pane,
            ratatui::layout::Direction::Horizontal,
            rows.max(crate::workflow::binding::spawn::MIN_PANE_ROWS),
            cols.max(crate::workflow::binding::spawn::MIN_PANE_COLS),
            Some(cwd.to_path_buf()),
            argv,
            env,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.state.host_terminal_appearance,
            false,
        );
        let (tab_idx, new_pane) = match spawned {
            Some(Ok(spawned)) => spawned,
            Some(Err(error)) => return Err(error.to_string()),
            None => return Err("no pane to split for the review".to_string()),
        };

        let mut terminal = new_pane.terminal;
        let terminal_id = terminal.id.clone();
        terminal.set_manual_label(title.to_string());
        if managed {
            terminal.begin_managed_agent(
                title.to_string(),
                crate::detect::Agent::Claude,
                Instant::now(),
                crate::workflow::binding::spawn::NODE_AGENT_SETTLE_DELAY,
                crate::workflow::binding::spawn::NODE_AGENT_LAUNCH_WINDOW,
            );
        }
        self.terminal_runtimes
            .insert(terminal_id.clone(), new_pane.runtime);
        self.state
            .remove_alias_shadowed_by_new_pane(new_pane.pane_id);
        self.state.terminals.insert(terminal_id.clone(), terminal);
        self.schedule_session_save();
        if let Some(pane) = self.pane_info(ws_idx, new_pane.pane_id) {
            self.emit_event(crate::api::schema::EventEnvelope {
                event: EventKind::PaneCreated,
                data: EventData::PaneCreated { pane },
            });
        }
        self.emit_layout_updated_event(ws_idx, tab_idx);
        let pane_id = self
            .public_pane_id(ws_idx, new_pane.pane_id)
            .ok_or_else(|| "the review pane has no public id".to_string())?;
        Ok((pane_id, terminal_id))
    }

    // ── the two self-reports ───────────────────────────────────────────────

    /// `workflow.review.answer`: one interview pane reporting its own answers.
    ///
    /// A parse failure is a refusal with the parser's own sentence and the
    /// interview stays open, because that sentence is printed in the agent's
    /// pane and is the only correction it gets (§3.5).
    pub(crate) fn record_review_answer(
        &mut self,
        run_id: &RunId,
        member: &str,
        raw: &str,
    ) -> Result<(), ReviewRefusal> {
        let index = self.live_review_index(run_id)?;
        let phase = self.workflow_reviews[index]
            .state
            .interview(member)
            .map(|interview| interview.phase)
            .ok_or_else(|| {
                ReviewRefusal::new(
                    super::api::workflow_review::WORKFLOW_REVIEW_NOT_FOUND_CODE,
                    format!("this review cycle is not interviewing {member}"),
                )
            })?;
        if phase.is_terminal() {
            return Err(ReviewRefusal::new(
                super::api::workflow_review::WORKFLOW_REVIEW_INTERVIEW_CLOSED_CODE,
                match phase {
                    InterviewPhase::Answered => {
                        format!("{member}'s interview has already been answered")
                    }
                    _ => format!(
                        "{member}'s interview was closed before this answer arrived; \
                         its findings will be recorded as evidence-only",
                    ),
                },
            ));
        }
        let answer = review_prompt::parse_interview_answer(raw).map_err(|error| {
            ReviewRefusal::new(
                super::api::workflow_review::WORKFLOW_REVIEW_ANSWER_REFUSED_CODE,
                error.to_string(),
            )
        })?;
        self.workflow_reviews[index]
            .answers
            .insert(member.to_string(), answer);
        // Absorbed on the next poll rather than here: the phase transition is
        // the pure core's to make, and it makes it from an observation of the
        // pane, not from the handler's word.
        self.workflow_reviews[index].next_poll_at = None;
        Ok(())
    }

    /// `workflow.review.report`: the synthesis pane's findings.
    ///
    /// Every finding's `interview_mode` and `interview` are stamped by
    /// [`crate::workflow::review::finding_seed`] from karvex's own attribution
    /// table — never from anything the synthesiser wrote (coupling 3).
    pub(crate) fn record_review_findings(
        &mut self,
        run_id: &RunId,
        raw: &str,
    ) -> Result<usize, ReviewRefusal> {
        let index = self.live_review_index(run_id)?;
        if matches!(
            self.workflow_reviews[index].synthesis,
            SynthesisState::Reported
        ) {
            return Err(ReviewRefusal::new(
                super::api::workflow_review::WORKFLOW_REVIEW_NOT_AWAITING_CODE,
                "this review cycle has already reported its findings".to_string(),
            ));
        }
        let parsed = review_prompt::parse_findings(raw).map_err(|error| {
            ReviewRefusal::new(
                super::api::workflow_review::WORKFLOW_REVIEW_REPORT_REFUSED_CODE,
                error.to_string(),
            )
        })?;

        let cycle = &self.workflow_reviews[index];
        let attribution = cycle.attribution();
        let seeds: Vec<_> = parsed
            .iter()
            .map(|finding| {
                crate::workflow::review::finding_seed(finding, &attribution, &cycle.run_nodes)
            })
            .collect();
        let count = seeds.len();
        let cycle_id = cycle.cycle_id.clone();
        let interrogations: Vec<InterrogationId> = cycle.interrogations.values().cloned().collect();

        if !seeds.is_empty() {
            self.persist_review_write(StoreWrite::ReviewFindings {
                cycle: cycle_id.clone(),
                findings: seeds,
            });
        }
        let ended_at_unix_ms = crate::app::workflow::current_unix_ms();
        self.persist_review_write(StoreWrite::ReviewCycleUpdate {
            id: cycle_id,
            status: Some(ReviewCycleStatus::AwaitingUser),
            // Not ended: `awaiting_user` is the cycle waiting for a human, and
            // `review.apply` is what ends it.
            ended_at_unix_ms: None,
            resulting_version: None,
        });
        for interrogation in interrogations {
            self.persist_review_write(StoreWrite::InterrogationUpdate {
                id: interrogation,
                forked_session_id: None,
                ended_at_unix_ms: Some(ended_at_unix_ms),
            });
        }

        let cycle = &mut self.workflow_reviews[index];
        cycle.synthesis = SynthesisState::Reported;
        // The cycle stops being *driven* here. Everything a client asks for
        // from now on — the findings, the status, `review.apply` — is answered
        // from the store, which is what makes an apply survive a restart.
        cycle.closed = true;

        self.emit_review_event(EventKind::WorkflowReviewReady, run_id);
        self.show_workflow_notice(UserNotice {
            level: NoticeLevel::Info,
            run: Some(run_id.clone()),
            path: None,
            message: format!("review ready: {count} finding(s) waiting for your decision"),
        });
        Ok(count)
    }

    fn live_review_index(&self, run_id: &RunId) -> Result<usize, ReviewRefusal> {
        self.workflow_reviews
            .iter()
            .position(|cycle| !cycle.closed && cycle.run_id == *run_id)
            .ok_or_else(|| {
                ReviewRefusal::new(
                    super::api::workflow_review::WORKFLOW_REVIEW_NOT_FOUND_CODE,
                    format!("no review cycle is running for {run_id} on this server"),
                )
            })
    }

    /// How many of a live cycle's planned interviews are degraded, for the
    /// wire's `evidence_only_count` while the cycle is still being driven.
    ///
    /// Deliberately `None` once the cycle is closed, even for the poll or two
    /// before it is dropped: from `report` onwards the durable derivation (the
    /// members the findings themselves could not be attributed to) is the
    /// answer every client gets forever, and a field that changed value the
    /// moment this server forgot the cycle would be worse than either number.
    pub(crate) fn live_review_evidence_only_count(&self, run_id: &RunId) -> Option<u32> {
        self.workflow_reviews
            .iter()
            .find(|cycle| !cycle.closed && cycle.run_id == *run_id)
            .map(|cycle| {
                let attribution = cycle.attribution();
                cycle
                    .plan
                    .assignments
                    .iter()
                    .filter(|assignment| {
                        matches!(
                            attribution.resolve(Some(&assignment.member)).mode,
                            InterviewMode::EvidenceOnly
                        )
                    })
                    .count() as u32
            })
    }

    // ── plumbing ───────────────────────────────────────────────────────────

    /// Hands one review write to the store thread.
    ///
    /// A local copy of `workflow_lead`'s `persist_workflow_write` rather than a
    /// shared one: that method is private to the projection adapter, and
    /// widening it would put this packet's edit in a file P8 owns.
    fn persist_review_write(&mut self, write: StoreWrite) {
        match self
            .workflow_store
            .call(move |cx| cx.block_on(cx.store().write(write)))
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                warn!(%error, "a review cycle write was rejected by the store");
                self.mark_workflow_persistence_degraded();
            }
            Err(unavailable) => {
                warn!(
                    ?unavailable,
                    "the workflow store is unavailable; a review cycle write was lost"
                );
                self.mark_workflow_persistence_degraded();
            }
        }
    }

    /// Emits one `workflow.review.*` event, reading the cycle back from the
    /// store so the payload is the durable row rather than this server's
    /// in-memory opinion of it.
    fn emit_review_event(&mut self, kind: EventKind, run_id: &RunId) {
        let Some(review) = self.stored_review_info(run_id) else {
            return;
        };
        let data = match kind {
            EventKind::WorkflowReviewStarted => EventData::WorkflowReviewStarted {
                run_id: run_id.to_string(),
                review,
            },
            EventKind::WorkflowReviewReady => EventData::WorkflowReviewReady {
                run_id: run_id.to_string(),
                review,
            },
            _ => EventData::WorkflowReviewClosed {
                run_id: run_id.to_string(),
                review,
            },
        };
        self.emit_workflow_run_event(kind, data);
    }

    // ── reading the run back ───────────────────────────────────────────────

    /// Everything a review cycle needs about a finished run, in one store call.
    ///
    /// Read once and held: the run is over, so nothing here can change under
    /// the cycle, and re-reading it per poll would be a per-tick fan-out over a
    /// run's whole node set for no new information.
    fn load_review_inputs(&mut self, run_id: &RunId) -> Result<ReviewInputs, ReviewRefusal> {
        let wanted = run_id.clone();
        // One cadence for the whole subsystem: the review's idle arithmetic and the
        // watchdog's sampling must multiply by the same number, so it is read from
        // the watchdog policy rather than kept as a second copy (P9/P10 merge).
        let watchdog_tick_secs =
            u32::try_from(self.workflow_policy.watchdog.tick_secs).unwrap_or(u32::MAX);
        let now_unix_ms = crate::app::workflow::current_unix_ms();
        let loaded = self.workflow_store.call(move |cx| {
            let store = cx.store();
            let Some(run) = cx.block_on(store.get_run(&wanted))? else {
                return Ok::<_, crate::workflow::store::StoreError>(None);
            };
            let nodes = cx.block_on(store.list_run_nodes(&wanted))?;
            let members = cx.block_on(store.list_run_members(&wanted))?;
            let watchdog = cx.block_on(store.watchdog_journal(&wanted))?;
            let summary = cx.block_on(store.get_run_summary(&wanted))?;
            let cycle = cx.block_on(store.get_review_cycle(&wanted))?;
            let version = cx.block_on(store.get_version_record(&run.version))?;
            let graph = cx.block_on(store.load_version(&run.version))?;
            let mut evidence = BTreeMap::new();
            for node in &nodes {
                if let Some(found) = cx.block_on(store.node_evidence(
                    &wanted,
                    &node.instance_path,
                    now_unix_ms,
                    watchdog_tick_secs,
                ))? {
                    evidence.insert(node.instance_path.clone(), found);
                }
            }
            Ok(Some((
                run, nodes, members, watchdog, summary, cycle, version, graph, evidence,
            )))
        });
        let raw = match loaded {
            Ok(Ok(Some(raw))) => raw,
            Ok(Ok(None)) => {
                return Err(ReviewRefusal::new(
                    super::api::workflow_review::WORKFLOW_REVIEW_NOT_FOUND_CODE,
                    format!("no run with id {run_id}"),
                ))
            }
            Ok(Err(error)) => return Err(ReviewRefusal::new(error.api_code(), error.to_string())),
            Err(unavailable) => {
                return Err(ReviewRefusal::new(unavailable.code, unavailable.message))
            }
        };
        Ok(build_review_inputs(raw))
    }
}

// ── evidence assembly (no `App`, no store, so it is testable on its own) ────

/// What [`crate::app::App::start_review_cycle`] needs, assembled.
#[cfg(feature = "workflow")]
struct ReviewInputs {
    status: RunStatus,
    tier: crate::workflow::tier::Tier,
    version_id: KvdagVersionId,
    /// The status of the run's most recent stored cycle, if it has ever had one.
    existing_cycle: Option<ReviewCycleStatus>,
    evidence: RunEvidence,
    identities: Vec<crate::workflow::review::MemberIdentity>,
    node_keys: Vec<NodeKey>,
    run_nodes: BTreeMap<NodeKey, InstancePath>,
    /// Which `run_node` each member's `interrogation` row hangs off. An
    /// interrogation is not a run node itself (§4 D8) but it must name one, and
    /// the store rejects a row naming a node this run does not have.
    interrogation_paths: BTreeMap<String, InstancePath>,
}

#[cfg(feature = "workflow")]
impl ReviewInputs {
    fn interrogation_path(&self, member: &str) -> Option<InstancePath> {
        self.interrogation_paths.get(member).cloned()
    }
}

#[cfg(feature = "workflow")]
type RawReview = (
    crate::workflow::store::RunRecord,
    Vec<crate::workflow::store::RunNodeRecord>,
    Vec<crate::workflow::store::RunMemberRecord>,
    Vec<crate::workflow::store::WatchdogJournalEntry>,
    Option<crate::workflow::store::RunSummaryRecord>,
    Option<crate::workflow::store::ReviewCycleRecord>,
    Option<crate::workflow::store::VersionRecord>,
    crate::workflow::model::Kvdag,
    BTreeMap<InstancePath, crate::workflow::store::NodeEvidence>,
);

/// Turns the store's rows into the pure core's [`RunEvidence`] and roster.
///
/// Everything unmeasured stays empty rather than being guessed at: karvex has
/// no durable record of task *reassignments* today (nothing journals an owner
/// change), so `owner_changes` is empty and the interview document simply does
/// not claim anything about who took what from whom.
#[cfg(feature = "workflow")]
fn build_review_inputs(raw: RawReview) -> ReviewInputs {
    use crate::workflow::review::{
        InterventionEvidence, MemberEvidence, MemberIdentity, TaskEvidence,
    };

    let (run, nodes, members, watchdog, summary, cycle, version, graph, evidence) = raw;

    let subject_by_path: BTreeMap<InstancePath, String> = nodes
        .iter()
        .map(|node| {
            let subject = if node.subject.trim().is_empty() {
                node.label.clone()
            } else {
                node.subject.clone()
            };
            (node.instance_path.clone(), subject)
        })
        .collect();

    let mut interventions_by_path: BTreeMap<InstancePath, Vec<InterventionEvidence>> =
        BTreeMap::new();
    for entry in watchdog {
        interventions_by_path
            .entry(entry.path)
            .or_default()
            .push(intervention_evidence(entry.at_unix_ms, &entry.payload));
    }

    let lead_path = InstancePath::new(crate::workflow::model::LEAD_INSTANCE_PATH);
    let mut tasks_by_owner: BTreeMap<String, Vec<TaskEvidence>> = BTreeMap::new();
    let mut unowned_tasks: Vec<TaskEvidence> = Vec::new();
    let mut run_nodes: BTreeMap<NodeKey, InstancePath> = BTreeMap::new();
    let lead_node = nodes
        .iter()
        .find(|node| node.instance_path == lead_path)
        .cloned();

    for node in &nodes {
        run_nodes.insert(node.node_key.clone(), node.instance_path.clone());
        // `.lead` is the lead *member*, not one of its tasks: counting it as a
        // task would give the lead an unfinished task for every run that ended
        // any way other than `succeeded`.
        if node.instance_path == lead_path {
            continue;
        }
        let measured = evidence.get(&node.instance_path);
        let task = TaskEvidence {
            path: node.instance_path.clone(),
            node_key: (!node.emergent).then(|| node.node_key.clone()),
            subject: subject_by_path
                .get(&node.instance_path)
                .cloned()
                .unwrap_or_default(),
            status: node.status,
            emergent: node.emergent,
            attention: node.attention,
            owner: Some(node.owner.clone()).filter(|owner| !owner.trim().is_empty()),
            owner_changes: Vec::new(),
            unresolved_blockers: measured
                .map(|measured| {
                    measured
                        .blocked_by
                        .iter()
                        .map(|path| {
                            subject_by_path
                                .get(path)
                                .cloned()
                                .unwrap_or_else(|| path.to_string())
                        })
                        .collect()
                })
                .unwrap_or_default(),
            first_seen_at_unix_ms: node.started_at_unix_ms.unwrap_or_default(),
            last_change_at_unix_ms: node
                .ended_at_unix_ms
                .or(node.started_at_unix_ms)
                .unwrap_or_default(),
            in_progress_ms: measured.map(|m| m.time_in_progress_ms).unwrap_or_default(),
            idle_while_in_progress_ms: measured
                .map(|m| m.idle_while_in_progress_ms)
                .unwrap_or_default(),
            interventions: interventions_by_path
                .get(&node.instance_path)
                .cloned()
                .unwrap_or_default(),
        };
        match &task.owner {
            Some(owner) => tasks_by_owner.entry(owner.clone()).or_default().push(task),
            None => unowned_tasks.push(task),
        }
    }

    let mut identities: Vec<MemberIdentity> = Vec::new();
    let mut member_evidence: Vec<MemberEvidence> = Vec::new();
    let mut named_lead = false;
    for member in &members {
        let is_lead = member_is_lead(member);
        named_lead |= is_lead;
        // The lead's identity is on the `.lead` node (P8), which survives the
        // team config Claude Code deletes at session end; the `run_member` row
        // may well carry neither an id nor a transcript.
        let (session_id, transcript_path, cwd) = if is_lead {
            lead_identity(member, lead_node.as_ref(), &run)
        } else {
            (
                member.session_id.clone(),
                member.transcript_path.clone(),
                member.cwd.clone(),
            )
        };
        identities.push(MemberIdentity {
            name: member.name.clone(),
            is_lead,
            model: member.model.clone(),
            backend_type: member.backend_type.clone(),
            cwd,
            session_id,
            transcript_readable: transcript_is_readable(transcript_path.as_deref()),
            transcript_path,
        });
        member_evidence.push(MemberEvidence {
            name: member.name.clone(),
            tasks: tasks_by_owner.remove(&member.name).unwrap_or_default(),
            last_state: member.last_state.clone(),
            last_state_at_unix_ms: member.last_state_at_unix_ms,
        });
    }

    // A run whose team config vanished before the projection could name its
    // lead still has a `.lead` node, and the lead is the one member §3.5 always
    // wants to interview. Synthesised rather than dropped.
    if !named_lead {
        if let Some(lead) = &lead_node {
            let name = if members
                .iter()
                .any(|member| member.name == FALLBACK_LEAD_NAME)
            {
                crate::workflow::model::LEAD_INSTANCE_PATH.to_string()
            } else {
                FALLBACK_LEAD_NAME.to_string()
            };
            let transcript_path = lead.transcript_path.clone();
            identities.push(MemberIdentity {
                name: name.clone(),
                is_lead: true,
                model: lead.model.clone(),
                backend_type: "in-process".to_string(),
                cwd: lead.cwd.clone(),
                session_id: lead
                    .agent_session_id
                    .clone()
                    .or_else(|| run.lead_session_id.clone()),
                transcript_readable: transcript_is_readable(transcript_path.as_deref()),
                transcript_path,
            });
            member_evidence.push(MemberEvidence {
                name,
                tasks: Vec::new(),
                last_state: None,
                last_state_at_unix_ms: None,
            });
        }
    }

    // Whatever is left owned by a name the roster never recorded is still part
    // of the run's picture — it is reported as unowned rather than dropped.
    for (_, tasks) in std::mem::take(&mut tasks_by_owner) {
        unowned_tasks.extend(tasks);
    }

    let interrogation_paths = identities
        .iter()
        .filter_map(|identity| {
            let path = interrogation_path_for(
                identity,
                member_evidence
                    .iter()
                    .find(|evidence| evidence.name == identity.name),
                lead_node.as_ref().map(|_| lead_path.clone()),
                &nodes,
            )?;
            Some((identity.name.clone(), path))
        })
        .collect();

    ReviewInputs {
        status: run.status,
        tier: run.tier,
        version_id: run.version.clone(),
        existing_cycle: cycle.map(|cycle| cycle.status),
        node_keys: graph.nodes.iter().map(|node| node.key.clone()).collect(),
        run_nodes,
        interrogation_paths,
        identities,
        evidence: RunEvidence {
            run_id: run.id.clone(),
            workflow_name: run.workflow_name.clone(),
            kvdag_version: version.map(|version| version.version).unwrap_or_default(),
            status: run.status,
            started_at_unix_ms: run.started_at_unix_ms,
            ended_at_unix_ms: run.ended_at_unix_ms,
            summary: summary.map(|summary| summary.text),
            failure: run.failure.as_ref().map(failure_sentence),
            members: member_evidence,
            unowned_tasks,
        },
    }
}

/// Which `run_node` an interview's `interrogation` row names.
///
/// The member's own most-troubled task first, because that is what the
/// interview is actually about; the run's `.lead` node as the fallback, because
/// it is the one node every run this server launched has. `None` means the run
/// has no node at all to hang the row off — in which case no row is written and
/// the member's findings degrade to evidence-only, honestly, rather than the
/// store rejecting a dangling reference.
#[cfg(feature = "workflow")]
fn interrogation_path_for(
    identity: &crate::workflow::review::MemberIdentity,
    evidence: Option<&crate::workflow::review::MemberEvidence>,
    lead_path: Option<InstancePath>,
    nodes: &[crate::workflow::store::RunNodeRecord],
) -> Option<InstancePath> {
    if identity.is_lead {
        if let Some(lead) = lead_path.clone() {
            return Some(lead);
        }
    }
    let owned = evidence.and_then(|evidence| {
        evidence
            .tasks
            .iter()
            .max_by_key(|task| (task.highest_rung(), task.idle_while_in_progress_ms))
            .map(|task| task.path.clone())
    });
    owned
        .or(lead_path)
        .or_else(|| nodes.first().map(|node| node.instance_path.clone()))
}

/// The rule `projection::ObservedMember::is_lead` applies, re-applied to the
/// stored row: the `"leader"` sentinel first, then the narrow in-process
/// `team-lead` fallback.
#[cfg(feature = "workflow")]
fn member_is_lead(member: &crate::workflow::store::RunMemberRecord) -> bool {
    member.pane_id.as_deref() == Some(crate::workflow::projection::LEAD_PANE_SENTINEL)
        || (member.backend_type == "in-process" && member.agent_type == "team-lead")
}

/// The lead's identity, preferring the `.lead` node's record over the team
/// roster's — the node is written by karvex from the lead's own self-report and
/// outlives the config the roster came from.
#[cfg(feature = "workflow")]
fn lead_identity(
    member: &crate::workflow::store::RunMemberRecord,
    lead_node: Option<&crate::workflow::store::RunNodeRecord>,
    run: &crate::workflow::store::RunRecord,
) -> (Option<String>, Option<String>, Option<String>) {
    let session_id = member
        .session_id
        .clone()
        .or_else(|| lead_node.and_then(|node| node.agent_session_id.clone()))
        .or_else(|| run.lead_session_id.clone());
    let transcript_path = member
        .transcript_path
        .clone()
        .or_else(|| lead_node.and_then(|node| node.transcript_path.clone()));
    let cwd = member
        .cwd
        .clone()
        .or_else(|| lead_node.and_then(|node| node.cwd.clone()));
    (session_id, transcript_path, cwd)
}

/// Whether the source transcript is still there.
///
/// The `stat` is the honesty guard, exactly as it is for the identity ladder:
/// claude owns that file and run history outlives claude's retention, so a
/// recorded path is not a readable one and only the second can justify a
/// `--resume`.
#[cfg(feature = "workflow")]
fn transcript_is_readable(path: Option<&str>) -> bool {
    path.map(str::trim)
        .filter(|path| !path.is_empty())
        .is_some_and(|path| Path::new(path).is_file())
}

/// The run's recorded failure, as one sentence.
#[cfg(feature = "workflow")]
fn failure_sentence(failure: &serde_json::Value) -> String {
    failure
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            failure
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| failure.to_string())
}

/// One `watchdog` journal payload, read as tolerantly as it is honest to.
///
/// The payload's shape is the watchdog adapter's (§3.4: `{class, rung, streak,
/// delivery}`) and `delivery` is read both as a bare channel word and as an
/// object, because either is a reasonable thing for that packet to have
/// written. What is *not* tolerated is inventing a delivery: anything this
/// cannot read as a channel is recorded as undelivered, so the interview never
/// tells a teammate "we nudged you" about a rung that never landed.
#[cfg(feature = "workflow")]
fn intervention_evidence(
    at_unix_ms: u64,
    payload: &serde_json::Value,
) -> crate::workflow::review::InterventionEvidence {
    let rung = payload
        .get("rung")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default()
        .min(u64::from(u8::MAX)) as u8;
    let kind = payload
        .get("class")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let delivery = payload.get("delivery");
    let channel = match delivery {
        Some(serde_json::Value::String(channel)) => Some(channel.clone()),
        Some(serde_json::Value::Object(_)) => delivery
            .and_then(|delivery| delivery.get("channel"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        _ => None,
    }
    .filter(|channel| !matches!(channel.as_str(), "" | "none" | "undelivered"));
    let delivered = delivery
        .and_then(|delivery| delivery.get("delivered"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(channel.is_some());
    crate::workflow::review::InterventionEvidence {
        at_unix_ms,
        rung,
        kind,
        channel,
        delivered,
        text: payload
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    }
}

/// The synthesis pane's argv.
///
/// **Not** [`binding::evidence_only_argv`], and this is not a stylistic choice:
/// P6's shared tail allows exactly one command,
/// [`binding::INTERVIEW_ALLOWED_TOOL`] (`kvx workflow review answer`), because
/// that is the only command an *interview* is told to run. The synthesis pane
/// is told to run `kvx workflow review report`, and S2's whole first amendment
/// is that a fork facing an unapproved `Bash` dialog stalls forever and karvex
/// reports it as `blocked`. Reusing the interview argv here would have produced
/// exactly that stall, on every cycle, at the last step.
///
/// Everything else is P6's arrangement verbatim: no positional prompt, no
/// `--settings`, `acceptEdits` so the findings file can be written unattended.
#[cfg(feature = "workflow")]
fn synthesis_argv(spec: &binding::InterviewSpawnSpec) -> Vec<String> {
    vec![
        crate::detect::interactive_agent_executable(crate::detect::Agent::Claude).to_string(),
        "--model".to_string(),
        spec.assignment.model.as_str().to_string(),
        "--effort".to_string(),
        spec.assignment.effort.as_str().to_string(),
        "--permission-mode".to_string(),
        binding::INTERVIEW_PERMISSION_MODE.to_string(),
        "--allowedTools".to_string(),
        SYNTHESIS_ALLOWED_TOOL.to_string(),
    ]
}

/// The one command the synthesis pane may run without an approval dialog, the
/// exact counterpart of [`binding::INTERVIEW_ALLOWED_TOOL`] and narrowed the
/// same way: the report verb and nothing broader.
#[cfg(feature = "workflow")]
const SYNTHESIS_ALLOWED_TOOL: &str = "Bash(kvx workflow review report:*)";

// ── small shared helpers ───────────────────────────────────────────────────

/// The interview pane's `(model, effort)`.
///
/// Resolved at the run's own tier and `Light` demand (§3.5's "`--model
/// <light>`"): an interview reads a document and writes five paragraphs about
/// a run it already lived through, which is the cheapest useful thing an agent
/// does in this subsystem.
#[cfg(feature = "workflow")]
fn interview_assignment(tier: crate::workflow::tier::Tier) -> crate::workflow::tier::Assignment {
    crate::workflow::tier::resolve(tier, crate::workflow::model::Demand::Light, None)
}

/// `review_cycle:<run>-<started at>`.
///
/// Minted by the app rather than by the database because
/// [`StoreWrite::ReviewCycleStarted`] and every later write address the row by
/// id; the run key plus the start instant is unique for the one thing that can
/// collide, which is two cycles over the same run.
#[cfg(feature = "workflow")]
fn mint_review_cycle_id(run: &RunId, started_at_unix_ms: u64) -> ReviewCycleId {
    let key = crate::workflow::binding::spawn::path_segment(
        run.as_str()
            .split_once(':')
            .map_or(run.as_str(), |(_, key)| key),
    );
    ReviewCycleId::new(format!("review_cycle:{key}-{started_at_unix_ms}"))
}

/// `interrogation:<cycle>-<member>`. One interview per member per cycle, so the
/// pair is the natural key.
#[cfg(feature = "workflow")]
fn mint_interrogation_id(cycle: &ReviewCycleId, member: &str) -> InterrogationId {
    let cycle_key = cycle
        .as_str()
        .split_once(':')
        .map_or(cycle.as_str(), |(_, key)| key);
    InterrogationId::new(format!(
        "interrogation:{}-{}",
        crate::workflow::binding::spawn::path_segment(cycle_key),
        crate::workflow::binding::spawn::path_segment(member),
    ))
}

/// Whether a run has finished. A review interviews a run that is over: while it
/// is still running its own team is still writing the record the interview
/// would be about.
#[cfg(feature = "workflow")]
fn run_status_is_terminal(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled
    )
}

#[cfg(feature = "workflow")]
fn run_status_word(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Pending => "pending",
        RunStatus::Running => "running",
        RunStatus::Paused => "paused",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

#[cfg(all(test, feature = "workflow"))]
impl crate::app::App {
    /// The live cycle's plan, as `(member, mode)` pairs in ranked order.
    pub(crate) fn test_review_plan(&self, run_id: &RunId) -> Vec<(String, InterviewMode)> {
        self.workflow_reviews
            .iter()
            .find(|cycle| cycle.run_id == *run_id)
            .map(|cycle| {
                cycle
                    .plan
                    .assignments
                    .iter()
                    .map(|assignment| (assignment.member.clone(), assignment.mode))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Which members the live cycle capped out.
    pub(crate) fn test_review_skipped(&self, run_id: &RunId) -> Vec<String> {
        self.workflow_reviews
            .iter()
            .find(|cycle| cycle.run_id == *run_id)
            .map(|cycle| {
                cycle
                    .plan
                    .skipped
                    .iter()
                    .map(|skipped| skipped.member.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// One interview's phase, for the failure-ladder tests.
    pub(crate) fn test_interview_phase(
        &self,
        run_id: &RunId,
        member: &str,
    ) -> Option<InterviewPhase> {
        self.workflow_reviews
            .iter()
            .find(|cycle| cycle.run_id == *run_id)
            .and_then(|cycle| cycle.state.interview(member))
            .map(|interview| interview.phase)
    }

    /// Whether the cycle recorded an `interrogation` row for this member —
    /// the map `attribution` is handed, and therefore the difference between
    /// "answered" and "attributable" (coupling 1).
    pub(crate) fn test_interview_is_attributable(&self, run_id: &RunId, member: &str) -> bool {
        self.workflow_reviews
            .iter()
            .find(|cycle| cycle.run_id == *run_id)
            .is_some_and(|cycle| {
                matches!(
                    cycle.attribution().resolve(Some(member)).mode,
                    InterviewMode::Resumed
                )
            })
    }

    /// Pushes every interview's start stamp back, so a test can reach the
    /// 15-minute interview deadline without waiting for it.
    pub(crate) fn test_age_review_interviews(&mut self, run_id: &RunId, by_ms: u64) {
        if let Some(cycle) = self
            .workflow_reviews
            .iter_mut()
            .find(|cycle| cycle.run_id == *run_id)
        {
            for pane in cycle.interviews.values_mut() {
                pane.started_at_unix_ms = pane.started_at_unix_ms.saturating_sub(by_ms);
            }
            cycle.next_poll_at = None;
        }
    }

    /// Closes every interview pane, the way a user closing them would.
    pub(crate) fn test_close_review_panes(&mut self, run_id: &RunId) {
        let terminals: Vec<_> = self
            .workflow_reviews
            .iter()
            .filter(|cycle| cycle.run_id == *run_id)
            .flat_map(|cycle| {
                cycle
                    .interviews
                    .values()
                    .map(|pane| pane.terminal_id.clone())
                    .collect::<Vec<_>>()
            })
            .collect();
        for terminal in terminals {
            self.state.terminals.remove(&terminal);
            self.terminal_runtimes.remove(&terminal);
        }
        if let Some(cycle) = self
            .workflow_reviews
            .iter_mut()
            .find(|cycle| cycle.run_id == *run_id)
        {
            cycle.next_poll_at = None;
        }
    }

    /// Closes the synthesis pane, for the two-strikes ladder.
    pub(crate) fn test_close_synthesis_pane(&mut self, run_id: &RunId) -> bool {
        let terminal = self
            .workflow_reviews
            .iter()
            .find(|cycle| cycle.run_id == *run_id)
            .and_then(|cycle| match &cycle.synthesis {
                SynthesisState::Running(pane) => Some(pane.terminal_id.clone()),
                _ => None,
            });
        let Some(terminal) = terminal else {
            return false;
        };
        self.state.terminals.remove(&terminal);
        self.terminal_runtimes.remove(&terminal);
        if let Some(cycle) = self
            .workflow_reviews
            .iter_mut()
            .find(|cycle| cycle.run_id == *run_id)
        {
            cycle.next_poll_at = None;
        }
        true
    }

    /// Whether a synthesis pane is running for this cycle.
    pub(crate) fn test_synthesis_is_running(&self, run_id: &RunId) -> bool {
        self.workflow_reviews
            .iter()
            .find(|cycle| cycle.run_id == *run_id)
            .is_some_and(|cycle| matches!(cycle.synthesis, SynthesisState::Running(_)))
    }

    pub(crate) fn test_review_cycle_dir(&self, run_id: &RunId) -> Option<PathBuf> {
        self.workflow_reviews
            .iter()
            .find(|cycle| cycle.run_id == *run_id)
            .map(|cycle| cycle.cycle_dir.clone())
    }
}

#[cfg(all(test, feature = "workflow"))]
mod tests {
    use super::*;
    use crate::workflow::model::{NodeStatus, RunStatus};

    /// Every test here is a `#[tokio::test]`: spawning a review pane spawns a
    /// real PTY, and `pane.rs`'s reader needs a reactor to attach to.
    ///
    /// Every review pane in these tests is a `/bin/cat`, never a `claude`:
    /// `KARVEX_WORKFLOW_REVIEW_COMMAND` is the documented CI escape hatch and
    /// this is exactly what it exists for. Set per test process — `cargo
    /// nextest` runs each test in its own — so it cannot leak between them.
    fn use_a_stub_review_command() {
        std::env::set_var(binding::REVIEW_COMMAND_ENV, r#"["/bin/cat"]"#);
    }

    /// The runs directory every cycle writes into, isolated per test.
    struct RunsDir(PathBuf);

    impl RunsDir {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "karvex-p10-{label}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or_default()
            ));
            std::fs::create_dir_all(&root).expect("the fixture runs directory is writable");
            std::env::set_var(
                crate::workflow::binding::spawn::RUNS_DIR_ENV_VAR,
                root.as_os_str(),
            );
            Self(root)
        }

        fn transcript(&self, name: &str) -> String {
            let path = self.0.join(format!("{name}.jsonl"));
            std::fs::write(&path, "{}\n").expect("the fixture transcript is writable");
            path.to_string_lossy().into_owned()
        }
    }

    impl Drop for RunsDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// One member to seed onto the run.
    struct MemberSeed {
        name: &'static str,
        session_id: Option<String>,
        transcript_path: Option<String>,
    }

    impl MemberSeed {
        fn evidence_only(name: &'static str) -> Self {
            Self {
                name,
                session_id: None,
                transcript_path: None,
            }
        }

        fn resumable(name: &'static str, transcript_path: String) -> Self {
            Self {
                name,
                session_id: Some(format!("{name}-session")),
                transcript_path: Some(transcript_path),
            }
        }
    }

    fn test_app() -> crate::app::App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = crate::app::App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.workflow_store = crate::app::workflow_store::WorkflowStoreHandle::in_memory();
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("review")];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app
    }

    fn create_workflow(app: &mut crate::app::App) -> String {
        let response = app.dispatch_api_request(
            "test.workflow.create",
            crate::api::schema::Method::WorkflowCreate(crate::api::schema::WorkflowCreateParams {
                definition: crate::api::schema::WorkflowDefinitionDocument {
                    format: crate::api::schema::WorkflowDefinitionFormat::Toml,
                    text: r#"
name = "ship-it"
description = "a test workflow"
default_tier = "low"

[[node]]
key = "plan"
label = "Plan"
runner = "command"
command = ["/bin/true"]
prompt_template = "plan"
output_schema = { type = "object" }
"#
                    .to_string(),
                },
            }),
        );
        serde_json::from_str::<serde_json::Value>(&response)
            .ok()
            .and_then(|value| {
                value["result"]["workflow"]["workflow_id"]
                    .as_str()
                    .map(str::to_string)
            })
            .unwrap_or_else(|| panic!("the workflow was created: {response}"))
    }

    /// A run that is over, with a team recorded against it — and, crucially,
    /// **no live lead left**: P8's whole point is that a review ticks with
    /// `workflow_lead` set to `None`.
    fn app_with_a_finished_run(members: Vec<MemberSeed>) -> (crate::app::App, RunId) {
        let mut app = test_app();
        let workflow_id = create_workflow(&mut app);
        let run_id = app.test_bind_a_live_lead_run(&workflow_id, "ship-it");
        seed_members(&mut app, &run_id, members);
        app.finish_lead_run(&run_id, crate::app::workflow::current_unix_ms());
        app.workflow_lead = None;
        (app, run_id)
    }

    fn seed_members(app: &mut crate::app::App, run_id: &RunId, members: Vec<MemberSeed>) {
        let observed_at_unix_ms = crate::app::workflow::current_unix_ms();
        for member in members {
            app.persist_review_write(StoreWrite::RunMemberSnapshot {
                run: run_id.clone(),
                name: member.name.to_string(),
                agent_type: "Explore".to_string(),
                model: "sonnet".to_string(),
                pane_id: Some(format!("w1:p{}", member.name.len())),
                backend_type: "tmux".to_string(),
                is_active: true,
                cwd: Some("/repo".to_string()),
                session_id: member.session_id,
                transcript_path: member.transcript_path,
                last_state: Some("idle".to_string()),
                last_state_at_unix_ms: Some(observed_at_unix_ms),
                observed_at_unix_ms,
            });
        }
    }

    /// A task the run projected, so a member has something to have been
    /// measured about.
    fn seed_task(app: &mut crate::app::App, run_id: &RunId, owner: &str) {
        app.persist_review_write(StoreWrite::RunTaskProjected {
            run: run_id.clone(),
            path: InstancePath::new("plan"),
            node_key: NodeKey::new("plan"),
            task_id: "7".to_string(),
            subject: "Plan the work".to_string(),
            owner: owner.to_string(),
            status: NodeStatus::Running,
            emergent: false,
            blocked_by: Vec::new(),
            observed_at_unix_ms: crate::app::workflow::current_unix_ms(),
        });
    }

    fn answer_document() -> String {
        serde_json::json!({
            "account": "I planned the work and handed it over.",
            "what_happened": "Two tasks, one of them reassigned.",
            "blockers": "Nothing blocked me.",
            "upstream_gaps": "The brief never said which repo.",
            "brief_changes": "Name the repository in the brief.",
        })
        .to_string()
    }

    fn findings_document() -> String {
        serde_json::json!({
            "findings": [{
                "node_key": "plan",
                "source_member": "research",
                "level": "prompt",
                "verdict": "improve",
                "rationale": "The brief never named the repository.",
                "evidence": {"unfinished_tasks": 1},
                "proposed_change": {"prompt_template": "plan, in <repo>"},
            }]
        })
        .to_string()
    }

    // ── preconditions ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_review_refuses_a_run_that_is_still_running() {
        let _runs = RunsDir::new("live");
        use_a_stub_review_command();
        let mut app = test_app();
        let workflow_id = create_workflow(&mut app);
        let run_id = app.test_bind_a_live_lead_run(&workflow_id, "ship-it");
        seed_members(
            &mut app,
            &run_id,
            vec![MemberSeed::evidence_only("research")],
        );

        let refusal = app
            .start_review_cycle(&run_id)
            .expect_err("a live run has no record to review yet");
        assert_eq!(
            refusal.code,
            crate::app::api::workflow_review::WORKFLOW_REVIEW_RUN_NOT_TERMINAL_CODE,
        );
    }

    #[tokio::test]
    async fn a_second_review_of_the_same_run_is_refused_while_the_first_runs() {
        let _runs = RunsDir::new("double");
        use_a_stub_review_command();
        let (mut app, run_id) =
            app_with_a_finished_run(vec![MemberSeed::evidence_only("research")]);

        app.start_review_cycle(&run_id)
            .expect("the first cycle starts");
        let refusal = app
            .start_review_cycle(&run_id)
            .expect_err("a run has at most one live review cycle");
        assert_eq!(
            refusal.code,
            crate::app::api::workflow_review::WORKFLOW_REVIEW_IN_FLIGHT_CODE,
        );
    }

    #[tokio::test]
    async fn a_run_with_nobody_to_interview_is_refused_by_name() {
        let _runs = RunsDir::new("nobody");
        use_a_stub_review_command();
        let mut app = test_app();
        let workflow_id = create_workflow(&mut app);
        // A run created without a lead pane: no `run_member` rows and no
        // `.lead` node, which is what a pre-rework run reads like.
        let run_id = {
            let wanted = crate::workflow::model::WorkflowId::new(workflow_id);
            let (version_id, kvdag) = app
                .workflow_store
                .call(move |cx| {
                    let summary = cx
                        .block_on(cx.store().get_workflow(&wanted))?
                        .expect("the workflow row exists");
                    let version = summary.head_version.expect("a head version");
                    let kvdag = cx.block_on(cx.store().load_version(&version))?;
                    Ok::<_, crate::workflow::store::StoreError>((version, kvdag))
                })
                .expect("the store is available")
                .expect("the head version loads");
            let started_at_unix_ms = crate::app::workflow::current_unix_ms();
            let assignments = crate::workflow::tier::resolve_assignments(
                &kvdag,
                crate::workflow::tier::Tier::Auto,
                &crate::workflow::tier::HistoryIndex::new(),
            );
            app.workflow_store
                .call(move |cx| {
                    cx.block_on(cx.store().create_run(crate::workflow::store::NewRun {
                        workflow: kvdag.workflow_id.clone(),
                        version: version_id,
                        tier: crate::workflow::tier::Tier::Auto,
                        args: BTreeMap::new(),
                        growth: kvdag.growth,
                        started_at_unix_ms,
                        assignments,
                        context_runs: Vec::new(),
                        workspace_id: None,
                        restore_from: None,
                        restored: Vec::new(),
                    }))
                })
                .expect("the store is available")
                .expect("the run row is created")
        };
        app.persist_review_write(StoreWrite::RunStatus {
            run: run_id.clone(),
            status: RunStatus::Succeeded,
            ended_at_unix_ms: Some(crate::app::workflow::current_unix_ms()),
        });

        let refusal = app
            .start_review_cycle(&run_id)
            .expect_err("there is nobody to interview");
        assert_eq!(
            refusal.code,
            crate::app::api::workflow_review::WORKFLOW_REVIEW_NO_INTERVIEWABLE_MEMBERS_CODE,
        );
    }

    // ── the plan ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_member_with_no_session_id_is_planned_as_evidence_only() {
        let runs = RunsDir::new("evidence-only");
        use_a_stub_review_command();
        let (mut app, run_id) = app_with_a_finished_run(vec![
            MemberSeed::evidence_only("research"),
            MemberSeed::resumable("build", runs.transcript("build")),
        ]);

        app.start_review_cycle(&run_id).expect("the cycle starts");
        let plan: BTreeMap<String, InterviewMode> =
            app.test_review_plan(&run_id).into_iter().collect();
        assert_eq!(plan.get("research"), Some(&InterviewMode::EvidenceOnly));
        assert_eq!(plan.get("build"), Some(&InterviewMode::Resumed));
    }

    #[tokio::test]
    async fn a_recorded_session_whose_transcript_is_gone_is_evidence_only() {
        let _runs = RunsDir::new("gone-transcript");
        use_a_stub_review_command();
        let (mut app, run_id) = app_with_a_finished_run(vec![MemberSeed {
            name: "research",
            session_id: Some("research-session".to_string()),
            transcript_path: Some("/nonexistent/research.jsonl".to_string()),
        }]);

        app.start_review_cycle(&run_id).expect("the cycle starts");
        let plan: BTreeMap<String, InterviewMode> =
            app.test_review_plan(&run_id).into_iter().collect();
        assert_eq!(
            plan.get("research"),
            Some(&InterviewMode::EvidenceOnly),
            "an id with no readable transcript cannot be resumed",
        );
    }

    #[tokio::test]
    async fn the_interview_cap_holds() {
        let _runs = RunsDir::new("cap");
        use_a_stub_review_command();
        let (mut app, run_id) = app_with_a_finished_run(vec![
            MemberSeed::evidence_only("research"),
            MemberSeed::evidence_only("build"),
            MemberSeed::evidence_only("verify"),
        ]);
        app.workflow_policy.review_max_interviews = 1;

        app.start_review_cycle(&run_id).expect("the cycle starts");
        assert_eq!(app.test_review_plan(&run_id).len(), 1);
        // Three teammates plus the run's own `.lead`, one interviewed.
        assert_eq!(app.test_review_skipped(&run_id).len(), 3);
    }

    // ── the failure ladder ─────────────────────────────────────────────────

    #[tokio::test]
    async fn a_silent_interview_times_out_and_the_cycle_still_reaches_synthesis() {
        let _runs = RunsDir::new("silent");
        use_a_stub_review_command();
        let (mut app, run_id) =
            app_with_a_finished_run(vec![MemberSeed::evidence_only("research")]);
        app.start_review_cycle(&run_id).expect("the cycle starts");

        app.test_age_review_interviews(&run_id, 16 * 60 * 1000);
        app.poll_review_cycles(Instant::now());
        assert_eq!(
            app.test_interview_phase(&run_id, "research"),
            Some(InterviewPhase::Failed(
                crate::workflow::review::InterviewFailure::TimedOut
            )),
        );
        // The cycle carries on: one silent interview degrades that member, it
        // does not stall the review.
        assert!(app.test_synthesis_is_running(&run_id));
    }

    #[tokio::test]
    async fn an_interview_pane_that_dies_degrades_that_member_only() {
        let runs = RunsDir::new("pane-gone");
        use_a_stub_review_command();
        let (mut app, run_id) = app_with_a_finished_run(vec![
            MemberSeed::resumable("research", runs.transcript("research")),
            MemberSeed::resumable("build", runs.transcript("build")),
        ]);
        app.start_review_cycle(&run_id).expect("the cycle starts");
        app.record_review_answer(&run_id, "research", &answer_document())
            .expect("the answer parses");
        app.test_close_review_panes(&run_id);
        app.poll_review_cycles(Instant::now());

        assert_eq!(
            app.test_interview_phase(&run_id, "research"),
            Some(InterviewPhase::Answered),
            "an interview that answered and then exited answered",
        );
        assert_eq!(
            app.test_interview_phase(&run_id, "build"),
            Some(InterviewPhase::Failed(
                crate::workflow::review::InterviewFailure::PaneGone
            )),
        );
        assert!(
            app.test_interview_is_attributable(&run_id, "research"),
            "an answered resumed interview with an interrogation row is attributable",
        );
        assert!(
            !app.test_interview_is_attributable(&run_id, "build"),
            "a resumed interview that failed is evidence-only, and the plan is not rewritten",
        );
    }

    #[tokio::test]
    async fn a_synthesis_that_dies_twice_fails_the_cycle() {
        let _runs = RunsDir::new("synthesis-twice");
        use_a_stub_review_command();
        let (mut app, run_id) =
            app_with_a_finished_run(vec![MemberSeed::evidence_only("research")]);
        app.start_review_cycle(&run_id).expect("the cycle starts");
        app.test_age_review_interviews(&run_id, 16 * 60 * 1000);
        app.poll_review_cycles(Instant::now());
        assert!(app.test_synthesis_is_running(&run_id));

        assert!(app.test_close_synthesis_pane(&run_id));
        app.poll_review_cycles(Instant::now());
        assert!(
            app.test_synthesis_is_running(&run_id),
            "the first synthesis failure is a retry",
        );

        assert!(app.test_close_synthesis_pane(&run_id));
        app.poll_review_cycles(Instant::now());
        assert!(
            app.workflow_reviews.is_empty(),
            "the second failure fails the cycle and it stops being driven",
        );
        let review = app
            .stored_review_info(&run_id)
            .expect("the cycle is stored");
        assert_eq!(
            review.status,
            crate::api::schema::WorkflowReviewStatus::Failed
        );
        // §3.5, structurally: nothing in the review path can reach a run's
        // status, so a failed review leaves the run exactly as it found it.
        let run = app
            .workflow_store
            .call({
                let wanted = run_id.clone();
                move |cx| cx.block_on(cx.store().get_run(&wanted))
            })
            .expect("the store is available")
            .expect("the run reads back")
            .expect("the run exists");
        assert_eq!(run.status, RunStatus::Succeeded);
    }

    // ── the two self-reports ───────────────────────────────────────────────

    #[tokio::test]
    async fn a_malformed_answer_is_refused_and_the_interview_stays_open() {
        let _runs = RunsDir::new("malformed");
        use_a_stub_review_command();
        let (mut app, run_id) =
            app_with_a_finished_run(vec![MemberSeed::evidence_only("research")]);
        app.start_review_cycle(&run_id).expect("the cycle starts");

        let refusal = app
            .record_review_answer(&run_id, "research", r#"{"account": "only one field"}"#)
            .expect_err("an incomplete answer is refused");
        assert_eq!(
            refusal.code,
            crate::app::api::workflow_review::WORKFLOW_REVIEW_ANSWER_REFUSED_CODE,
        );
        assert!(
            refusal.message.contains("what_happened"),
            "the refusal names the field: it is the agent's only correction ({})",
            refusal.message,
        );

        app.poll_review_cycles(Instant::now());
        assert_eq!(
            app.test_interview_phase(&run_id, "research"),
            Some(InterviewPhase::Running),
            "a refused answer leaves the interview open to retry",
        );

        app.record_review_answer(&run_id, "research", &answer_document())
            .expect("the corrected answer is accepted");
    }

    #[tokio::test]
    async fn an_answer_for_a_member_this_cycle_never_planned_is_refused() {
        let _runs = RunsDir::new("stranger");
        use_a_stub_review_command();
        let (mut app, run_id) =
            app_with_a_finished_run(vec![MemberSeed::evidence_only("research")]);
        app.start_review_cycle(&run_id).expect("the cycle starts");

        let refusal = app
            .record_review_answer(&run_id, "stranger", &answer_document())
            .expect_err("only a planned interview may answer");
        assert_eq!(
            refusal.code,
            crate::app::api::workflow_review::WORKFLOW_REVIEW_NOT_FOUND_CODE,
        );
    }

    #[tokio::test]
    async fn findings_reach_review_get_and_the_cycle_awaits_the_user() {
        let runs = RunsDir::new("findings");
        use_a_stub_review_command();
        let (mut app, run_id) = app_with_a_finished_run(vec![MemberSeed::resumable(
            "research",
            runs.transcript("r"),
        )]);
        seed_task(&mut app, &run_id, "research");
        app.start_review_cycle(&run_id).expect("the cycle starts");
        app.record_review_answer(&run_id, "research", &answer_document())
            .expect("the answer parses");
        // The run's `.lead` is always a candidate too; it never answers here,
        // so its pane going away is what settles the cycle.
        app.test_close_review_panes(&run_id);
        app.poll_review_cycles(Instant::now());
        assert!(app.test_synthesis_is_running(&run_id));

        let count = app
            .record_review_findings(&run_id, &findings_document())
            .expect("the findings parse");
        assert_eq!(count, 1);

        let review = app
            .stored_review_info(&run_id)
            .expect("the cycle is stored");
        assert_eq!(
            review.status,
            crate::api::schema::WorkflowReviewStatus::AwaitingUser,
        );
        let cycle_id = crate::workflow::model::ReviewCycleId::new(review.id.clone());
        let findings = app.stored_review_findings(&cycle_id);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].node_key, "plan");
        assert_eq!(
            findings[0].interview_mode,
            crate::api::schema::WorkflowReviewInterviewMode::Resumed,
            "an answered resumed interview with an interrogation row may be quoted",
        );
        assert!(findings[0].interrogation_id.is_some());
    }

    #[tokio::test]
    async fn a_finding_attributed_to_a_failed_interview_is_written_evidence_only() {
        let runs = RunsDir::new("attribution");
        use_a_stub_review_command();
        let (mut app, run_id) = app_with_a_finished_run(vec![MemberSeed::resumable(
            "research",
            runs.transcript("research"),
        )]);
        seed_task(&mut app, &run_id, "research");
        app.start_review_cycle(&run_id).expect("the cycle starts");
        // The interview never answers: its pane goes away.
        app.test_close_review_panes(&run_id);
        app.poll_review_cycles(Instant::now());

        app.record_review_findings(&run_id, &findings_document())
            .expect("the findings parse");
        let review = app
            .stored_review_info(&run_id)
            .expect("the cycle is stored");
        let cycle_id = crate::workflow::model::ReviewCycleId::new(review.id.clone());
        let findings = app.stored_review_findings(&cycle_id);
        assert_eq!(
            findings[0].interview_mode,
            crate::api::schema::WorkflowReviewInterviewMode::EvidenceOnly,
            "a member planned resumed whose interview failed is evidence-only",
        );
        assert!(findings[0].interrogation_id.is_none());
        assert_eq!(
            review.evidence_only_count, 1,
            "the durable count is the members the findings could not be attributed to",
        );
    }

    #[tokio::test]
    async fn a_malformed_findings_document_is_refused_and_the_cycle_stays_open() {
        let _runs = RunsDir::new("bad-findings");
        use_a_stub_review_command();
        let (mut app, run_id) =
            app_with_a_finished_run(vec![MemberSeed::evidence_only("research")]);
        app.start_review_cycle(&run_id).expect("the cycle starts");

        let refusal = app
            .record_review_findings(
                &run_id,
                r#"{"findings": [{"node_key": "plan", "level": "prompt",
                    "verdict": "replace", "rationale": "gone",
                    "evidence": {}, "proposed_change": {}}]}"#,
            )
            .expect_err("replace without a replacement never reaches the store");
        assert_eq!(
            refusal.code,
            crate::app::api::workflow_review::WORKFLOW_REVIEW_REPORT_REFUSED_CODE,
        );
        assert!(
            !app.workflow_reviews.is_empty(),
            "a refused report leaves the cycle running",
        );
    }

    // ── the seam ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn the_review_poller_runs_with_no_live_lead_run() {
        let _runs = RunsDir::new("no-lead");
        use_a_stub_review_command();
        let (mut app, run_id) =
            app_with_a_finished_run(vec![MemberSeed::evidence_only("research")]);
        assert!(
            app.workflow_lead.is_none(),
            "the fixture leaves no live run"
        );

        app.start_review_cycle(&run_id).expect("the cycle starts");
        assert!(
            app.review_cycle_deadline().is_some(),
            "a running cycle asks the loop to wake it",
        );
        // The whole tick, through the public entry point, with no lead at all.
        app.poll_run_projection(Instant::now());
        assert_eq!(
            app.test_interview_phase(&run_id, "research"),
            Some(InterviewPhase::Running),
        );
    }

    #[tokio::test]
    async fn a_running_review_does_not_block_a_new_run() {
        let _runs = RunsDir::new("not-blocking");
        use_a_stub_review_command();
        let (mut app, run_id) =
            app_with_a_finished_run(vec![MemberSeed::evidence_only("research")]);
        app.start_review_cycle(&run_id).expect("the cycle starts");

        assert!(
            !app.lead_run_is_live(),
            "a review cycle is not a live run and must not read as one (§6 D-4)",
        );
    }

    #[tokio::test]
    async fn the_interview_pane_runs_in_the_cycle_directory_and_writes_its_document_there() {
        let _runs = RunsDir::new("layout");
        use_a_stub_review_command();
        let (mut app, run_id) =
            app_with_a_finished_run(vec![MemberSeed::evidence_only("research")]);
        app.start_review_cycle(&run_id).expect("the cycle starts");

        let cycle_dir = app.test_review_cycle_dir(&run_id).expect("a live cycle");
        assert!(
            binding::interview_prompt_path(&cycle_dir, "research").is_file(),
            "the interview document is written before the pane is spawned",
        );
        assert!(
            cycle_dir.join(binding::ANSWERS_DIR).is_dir(),
            "the answers directory exists before an agent is told to write into it",
        );
    }

    #[tokio::test]
    async fn the_run_s_lead_is_interviewable_even_when_the_roster_never_named_it() {
        let _runs = RunsDir::new("lead-fallback");
        use_a_stub_review_command();
        let (mut app, run_id) =
            app_with_a_finished_run(vec![MemberSeed::evidence_only("research")]);

        app.start_review_cycle(&run_id).expect("the cycle starts");
        let members: Vec<String> = app
            .test_review_plan(&run_id)
            .into_iter()
            .map(|(member, _)| member)
            .collect();
        assert!(
            members.iter().any(|member| member == "lead"),
            "the `.lead` node is what makes the lead interviewable (P8): {members:?}",
        );
    }

    #[test]
    fn the_synthesis_pane_may_run_the_report_verb_the_interview_may_not() {
        let spec = binding::InterviewSpawnSpec {
            member: "synthesis".to_string(),
            cycle_dir: PathBuf::from("/runs/r/review/c"),
            member_project_dir: None,
            assignment: interview_assignment(crate::workflow::tier::Tier::Low),
        };
        let argv = synthesis_argv(&spec);
        assert!(
            argv.iter().any(|arg| arg == SYNTHESIS_ALLOWED_TOOL),
            "the synthesis pane reports findings, not answers: {argv:?}",
        );
        assert!(
            !argv
                .iter()
                .any(|arg| arg == binding::INTERVIEW_ALLOWED_TOOL),
            "reusing the interview's allow-list would stall synthesis on a Bash \
             approval dialog forever (S2 amendment 1): {argv:?}",
        );
        assert!(
            !argv
                .iter()
                .any(|arg| arg == "--resume" || arg == "--fork-session"),
            "there is no session to fork for synthesis: {argv:?}",
        );
        assert!(
            !argv
                .iter()
                .any(|arg| arg.contains("interview.md") || arg.contains(".md")),
            "no positional prompt, ever: {argv:?}",
        );
    }
}
