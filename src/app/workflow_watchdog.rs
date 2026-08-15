//! The watchdog's IO adapter: sample the live run, classify, deliver, record.
//!
//! `phase4-retarget-plan.md` §3.4 and §5 packet **P9**. The decisions are pure
//! and already landed in [`crate::workflow::watchdog`] (packet P4) — the
//! four-way [`ProgressClass`](crate::workflow::watchdog::ProgressClass)
//! taxonomy, the escalation ladder, and the never-spend-an-undelivered-rung
//! rule. This module is the half that touches the world: it reads the live
//! run's own durable rows, hands the pure layer an observation, and turns the
//! decision it gets back into a real message, a store write, a journal entry
//! and an event.
//!
//! The seam is deliberately one call from the projection poll rather than a
//! timer of its own: the watchdog samples at `watchdog_tick_secs`, a multiple
//! of the 2 s projection cadence, over exactly the state that poll refreshed.
//!
//! ## Where the sample comes from, and why it is the store
//!
//! Every fact one sample needs is already a durable row by the time this runs.
//! `run_node` holds the projected task — subject, status, owner, when it
//! started, and karvex's own `attention` column. `run_member` holds the fact
//! that used to be reachable only through a live pane: `last_state`, in
//! [`WorkflowMemberState`](crate::api::schema::WorkflowMemberState)'s
//! vocabulary, and `last_state_at`, which is when that state *started* (P8).
//! Reading the rows rather than the panes buys three things that matter more
//! than the round trip:
//!
//! * The watchdog and a `workflow.run.get` a client makes one millisecond
//!   later cannot disagree, because they read the same row.
//! * `state_age_ms` is a measured interval rather than a sample count. A pane
//!   read can only ever answer "idle now"; `last_state_at` answers "idle since",
//!   which is the number a message can honestly quote to an agent.
//! * `attention` is compared against what is actually persisted, so a write
//!   that failed is re-attempted on the next sample instead of being masked by
//!   an in-memory shadow copy that says it succeeded.
//!
//! ## What the tick refuses to do
//!
//! * **[`WatchdogVerdict::NotWatched`] touches nothing.** No attention write,
//!   no journal entry, no event, no message — including the `attention` the row
//!   already holds, which keeps whatever it held (P4's ruling).
//! * **`attention` is written only when it changes.** It is re-evaluated every
//!   tick and `None` means *clear*, not "leave it alone"; but a tick that
//!   concludes what the row already says writes nothing, so
//!   `run_node.watchdog_interventions` counts surfaced opinions rather than
//!   polls.
//! * **The ladder advances only through [`LadderState::after`].** Delivery
//!   feeds it [`DeliveryOutcome`], and a rung whose message was refused is
//!   retried verbatim on the next sample. A nudge nobody received must never
//!   look like a nudge that was ignored.
//! * **The run's node `status` is never touched.** It is Claude Code's
//!   projected fact; [`Attention`] is karvex's opinion about it, in its own
//!   column (D-10).
//!
//! ## Delivery
//!
//! Through [`App::message_run_session`](crate::app::App::message_run_session)
//! and nothing else, exactly as P4's ruling requires: `Ok` is
//! [`DeliveryOutcome::Delivered`], `Err` is
//! [`DeliveryOutcome::Undelivered`]. That path already owns both documented
//! channels — the recipient session's inbox socket when it reported one, and
//! `agent.prompt` into its pane when it did not — and it already journals
//! which of the two carried the message, which is a fact that cannot be
//! reconstructed afterwards (S1: a teammate's messaging token exists nowhere
//! but its own hook environment). A second delivery path here would duplicate
//! that authority and skip that journal, so a session karvex has no endpoint
//! for at all is `Undelivered`, twice, and then surfaced — which is the honest
//! answer, not a silent retry loop.
//!
//! Boundary note (`AGENTS.md`): everything here is a shared runtime fact —
//! `run_node.attention`, the `watchdog` journal kind, `workflow.node.watchdog`
//! — persisted through the store and served over the JSON API. Nothing here is
//! TUI-private.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::workflow::model::{InstancePath, NodeKey, RunId};
use crate::workflow::watchdog::LadderState;

/// What the watchdog remembers between samples.
///
/// Deliberately small, and deliberately *not* a second copy of anything the
/// store already holds. The ladder is the one fact that exists nowhere else —
/// it is bookkeeping about karvex's own attempts, not about the run — and the
/// member snapshot exists only so the *next* sample can tell "moved since we
/// last looked" from "has been sitting there all along".
#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
#[derive(Debug, Default)]
pub(crate) struct WatchdogState {
    /// The run this memory belongs to. A different run id wipes it: a ladder is
    /// a fact about one node of one run and must never be inherited by the
    /// next.
    run: Option<RunId>,
    next_sample_at: Option<Instant>,
    ladders: BTreeMap<InstancePath, LadderState>,
    /// Per member, the `(last_state, last_state_at)` pair the previous sample
    /// read. Compared as a pair rather than on the state word alone, because a
    /// teammate that went idle, worked, and went idle again between two samples
    /// reads as the same word and a different clock — which is movement.
    members: BTreeMap<String, MemberSample>,
    /// The authored `timeout_ms` per node key, read once from the run's
    /// immutable kvdag version. `None` means "not read yet"; an empty map means
    /// "read, and the definition authored no budgets", which is a different
    /// fact and must not cause a re-read every 20 seconds.
    budgets: Option<BTreeMap<NodeKey, u64>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MemberSample {
    last_state: Option<String>,
    last_state_at_unix_ms: Option<u64>,
}

#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
impl WatchdogState {
    /// Points the memory at `run`, clearing it when that is a different run.
    /// Returns whether the run changed, which is worth one debug line and
    /// nothing else.
    fn adopt(&mut self, run: &RunId) -> bool {
        if self.run.as_ref() == Some(run) {
            return false;
        }
        *self = Self {
            run: Some(run.clone()),
            ..Self::default()
        };
        true
    }

    fn due(&self, now: Instant) -> bool {
        self.next_sample_at.is_none_or(|due| now >= due)
    }

    /// The next sample is `tick_secs` after this one, floored at one second so
    /// a `watchdog_tick_secs = 0` misconfiguration cannot turn the 2 s
    /// projection poll into a store read per poll.
    fn rearm(&mut self, now: Instant, tick_secs: u64) {
        self.next_sample_at = Some(now + Duration::from_secs(tick_secs.max(1)));
    }
}

/// The slim build has no workflow store to sample and no run to sample over,
/// so the seam answers the honest "nothing changed" rather than forking the
/// poller's shape between the two feature legs.
#[cfg(not(feature = "workflow"))]
impl crate::app::App {
    pub(crate) fn poll_run_watchdog(&mut self, now: Instant) -> bool {
        let _ = now;
        false
    }
}

/// Everything that needs the store: the sample, the delivery, and the record.
#[cfg(feature = "workflow")]
mod live {
    use std::collections::BTreeMap;
    use std::time::Instant;

    use tracing::{debug, warn};

    use super::{MemberSample, WatchdogState};
    use crate::api::schema::{EventData, EventEnvelope, EventKind, WorkflowMemberState};
    use crate::detect::AgentState;
    use crate::workflow::binding::messaging::{DeliveryChannel, Priority};
    use crate::workflow::model::{
        Attention, InstancePath, NodeKey, NodeStatus, NoticeLevel, RunEventKind, RunId, StoreWrite,
        UserNotice, LEAD_INSTANCE_PATH,
    };
    use crate::workflow::projection::TaskStatus;
    use crate::workflow::store::{RunEdgeRecord, RunMemberRecord, RunNodeRecord};
    use crate::workflow::watchdog::{
        self, BlockingTask, DeliveryOutcome, Escalation, LadderState, MessageTarget, ObservedNode,
        OwnerState, PaneObservation, WatchdogAction, WatchdogDecision, WatchdogPolicy,
        WatchdogVerdict,
    };

    /// The three row sets one sample reads, kept together so the store is asked
    /// once rather than three times.
    #[derive(Debug, Clone, PartialEq)]
    pub(super) struct RunRows {
        pub(super) nodes: Vec<RunNodeRecord>,
        pub(super) members: Vec<RunMemberRecord>,
        pub(super) edges: Vec<RunEdgeRecord>,
    }

    /// One node, ready to hand to the pure layer, plus the attention its row
    /// currently holds so the caller can tell a change from a repeat.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct NodeSample {
        pub(super) node: ObservedNode,
        pub(super) stored_attention: Option<Attention>,
    }

    /// What one delivery attempt did, in the vocabulary the journal records.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Delivery {
        outcome: DeliveryOutcome,
        target: String,
        /// Which of the two documented channels carried it. `None` when nothing
        /// crossed either.
        channel: Option<DeliveryChannel>,
        /// Why it did not land, verbatim from the messaging path, so a run can
        /// explain itself without its reader having to guess.
        error: Option<String>,
    }

    impl crate::app::App {
        /// One watchdog sample over the live lead run.
        ///
        /// Called once per 2 s projection poll, after the projection refreshed
        /// (P8's seam). Most of those calls do nothing at all: the sample is on
        /// the `watchdog_tick_secs` cadence, which is a multiple of the poll's.
        /// Returns whether anything changed, so the caller knows whether to
        /// re-render.
        pub(crate) fn poll_run_watchdog(&mut self, now: Instant) -> bool {
            // The kill switch is checked before anything is read: with
            // `watchdog_enabled = false` there must be zero samples, not
            // samples whose verdicts are thrown away (§3.7).
            let policy = self.workflow_policy.watchdog;
            if !policy.enabled {
                return false;
            }
            let Some(run) = self.workflow_lead.as_ref() else {
                self.workflow_watchdog = WatchdogState::default();
                return false;
            };
            if run.closed {
                return false;
            }
            // Before the run binds there is no team, no member row and no
            // projected task, so every node would classify as `Unknown` and the
            // ladder would surface an unbound lead within a minute. The bind
            // deadline (§3.1a) already closes that run with a reason, and
            // binding normally takes a few seconds — so sampling here would put
            // a false "stuck" on the launch of every healthy run in order to
            // report, worse and earlier, a failure that is already owned.
            if run.binding.is_none() {
                return false;
            }
            let run_id = run.run_id.clone();
            if self.workflow_watchdog.adopt(&run_id) {
                debug!(run = %run_id, "the watchdog starts sampling this run");
            }
            if !self.workflow_watchdog.due(now) {
                return false;
            }
            self.workflow_watchdog.rearm(now, policy.tick_secs);
            self.sample_run_watchdog(&run_id, &policy)
        }

        /// The sample proper: read the rows, ask the pure layer, act on what it
        /// says.
        fn sample_run_watchdog(&mut self, run_id: &RunId, policy: &WatchdogPolicy) -> bool {
            let Some(rows) = self.read_watchdog_rows(run_id) else {
                return false;
            };
            let budgets = self.watchdog_budgets(run_id);
            let now_unix_ms = crate::app::workflow::current_unix_ms();

            // The member snapshot is rotated *before* the nodes are observed,
            // so every node in one sample is compared against the same previous
            // observation. Rotating per node would make the second node that
            // shares an owner miss the movement the first one consumed.
            let observed = member_samples(&rows.members);
            let previous = std::mem::replace(&mut self.workflow_watchdog.members, observed);
            let ladders = self.workflow_watchdog.ladders.clone();
            let samples = observe_run(&rows, &ladders, &previous, &budgets, now_unix_ms);

            let mut changed = false;
            for sample in samples {
                changed |= self.settle_watchdog_node(run_id, sample, policy, now_unix_ms);
            }
            changed
        }

        /// One node, one verdict, and everything that follows from it.
        fn settle_watchdog_node(
            &mut self,
            run_id: &RunId,
            sample: NodeSample,
            policy: &WatchdogPolicy,
            now_unix_ms: u64,
        ) -> bool {
            let NodeSample {
                node,
                stored_attention,
            } = sample;
            let decision = match watchdog::decide(&node, policy) {
                // The one rule with no exceptions: an unwatched node is not
                // touched. Not its attention, not its journal, not an event —
                // the column keeps whatever it held (P4's ruling).
                WatchdogVerdict::NotWatched(_) => return false,
                WatchdogVerdict::Watched(decision) => decision,
            };

            let delivery = match &decision.action {
                WatchdogAction::Say { rung, target, text } => {
                    Some(self.deliver_watchdog_rung(*rung, target, text))
                }
                // Nothing to say, so nothing to fail. The outcome is only ever
                // consulted for a `Say` (`LadderState::after`), which is what
                // stops the neutral value below from being read as a claim.
                WatchdogAction::Hold | WatchdogAction::Surface => None,
            };
            let outcome = delivery
                .as_ref()
                .map_or(DeliveryOutcome::Delivered, |delivery| delivery.outcome);

            // The ladder advances here and nowhere else.
            let advanced = node.ladder.after(&decision, outcome);
            self.workflow_watchdog
                .ladders
                .insert(node.path.clone(), advanced);

            let mut changed = false;
            // A rung — delivered or not — is journalled. A `Hold` is not: the
            // watchdog samples forever, and an entry per quiet sample would
            // bury a run's real history under its own heartbeat.
            if decision.rung().is_some() {
                self.journal_watchdog_rung(
                    run_id,
                    &node.path,
                    &decision,
                    delivery.as_ref(),
                    now_unix_ms,
                );
                changed = true;
            }
            if decision.attention != stored_attention {
                self.write_watchdog_attention(run_id, &node.path, decision.attention, now_unix_ms);
                self.emit_watchdog_event(run_id, &node.path);
                self.notice_for_watchdog(run_id, &node, &decision);
                changed = true;
            }
            changed
        }

        /// Hands one rung to the run's own messaging path and records what came
        /// back.
        ///
        /// P4's ruling, verbatim: `Ok` is delivered, `Err` is not, and nothing
        /// else decides. `message_run_session` already owns both documented
        /// channels — the recipient's inbox socket when it reported one, and
        /// `agent.prompt` into its pane when it did not — and the
        /// `message_delivered` journal entry that names which one carried it.
        /// A second delivery path here would duplicate that authority and skip
        /// that journal, so a session karvex holds no endpoint for at all is
        /// `Undelivered`, twice, and then surfaced.
        fn deliver_watchdog_rung(
            &mut self,
            rung: Escalation,
            target: &MessageTarget,
            text: &str,
        ) -> Delivery {
            let name = match target {
                MessageTarget::Lead => crate::workflow::binding::lead::LEAD_TARGET_NAME.to_string(),
                MessageTarget::Member { name } => name.clone(),
            };
            let priority = Priority::parse(rung.message_priority()).unwrap_or_default();
            match self.message_run_session(&name, text, priority) {
                Ok(receipt) => Delivery {
                    outcome: DeliveryOutcome::Delivered,
                    target: name,
                    channel: Some(receipt.channel),
                    error: None,
                },
                Err(error) => {
                    debug!(
                        target = %name,
                        rung = rung.rung(),
                        %error,
                        "a watchdog rung could not be delivered, so it is not spent"
                    );
                    Delivery {
                        outcome: DeliveryOutcome::Undelivered,
                        target: name,
                        channel: None,
                        error: Some(error.to_string()),
                    }
                }
            }
        }

        /// The `kind: "watchdog"` journal entry (§3.4).
        ///
        /// The pure layer authors `{class, rung, streak, attention}`; the
        /// adapter adds the half only it knows — whether the say happened, over
        /// which channel, and to whom. The text rides along, unlike the
        /// `message_delivered` entry beside it: this sentence is karvex's own
        /// composition rather than user content, and
        /// `review::InterventionEvidence` is built to quote it back to the
        /// teammate verbatim rather than paraphrase karvex's own words at it.
        fn journal_watchdog_rung(
            &mut self,
            run_id: &RunId,
            path: &InstancePath,
            decision: &WatchdogDecision,
            delivery: Option<&Delivery>,
            now_unix_ms: u64,
        ) {
            let seq = match self.workflow_lead.as_mut() {
                Some(run) => run.next_journal_seq(),
                None => return,
            };
            let mut payload = decision.journal_payload();
            if let Some(object) = payload.as_object_mut() {
                let delivered = delivery.is_some_and(|d| d.outcome == DeliveryOutcome::Delivered);
                object.insert("delivered".to_string(), serde_json::Value::Bool(delivered));
                object.insert(
                    "channel".to_string(),
                    json_or_null(
                        delivery
                            .and_then(|d| d.channel)
                            .map(|channel| channel.as_str().to_string()),
                    ),
                );
                object.insert(
                    "target".to_string(),
                    json_or_null(delivery.map(|d| d.target.clone())),
                );
                object.insert(
                    "undelivered_reason".to_string(),
                    json_or_null(delivery.and_then(|d| d.error.clone())),
                );
                object.insert(
                    "text".to_string(),
                    json_or_null(match &decision.action {
                        WatchdogAction::Say { text, .. } => Some(text.clone()),
                        WatchdogAction::Hold | WatchdogAction::Surface => None,
                    }),
                );
            }
            self.persist_workflow_write(StoreWrite::RunEvent {
                run: run_id.clone(),
                seq,
                kind: RunEventKind::Watchdog,
                path: Some(path.clone()),
                payload,
                at_unix_ms: now_unix_ms,
            });
        }

        /// Karvex's opinion about one node, in karvex's own column.
        fn write_watchdog_attention(
            &mut self,
            run_id: &RunId,
            path: &InstancePath,
            attention: Option<Attention>,
            now_unix_ms: u64,
        ) {
            self.persist_workflow_write(StoreWrite::RunNodeAttention {
                run: run_id.clone(),
                path: path.clone(),
                attention,
                observed_at_unix_ms: now_unix_ms,
            });
        }

        /// `workflow.node.watchdog`, read back off the row the write just made.
        ///
        /// Read back rather than synthesised, the same rule
        /// `emit_projected_node_events` follows: the row is the truth a client
        /// would get from `workflow.run.get`, and an event that disagreed with
        /// it would be worse than no event. (`WorkflowEvent::NodeWatchdog` is
        /// the model-level name for this event; nothing on this tree bridges
        /// that enum to the wire any more — the engine that produced it is
        /// gone — so the adapter emits the wire envelope directly, exactly as
        /// the projection does.)
        fn emit_watchdog_event(&mut self, run_id: &RunId, path: &InstancePath) {
            let Some(nodes) = self.stored_run_nodes_for_events(run_id) else {
                return;
            };
            let Some(node) = nodes.into_iter().find(|node| node.path == path.as_str()) else {
                warn!(run = %run_id, %path, "no stored node to publish a watchdog event from");
                return;
            };
            self.emit_event(EventEnvelope {
                event: EventKind::WorkflowNodeWatchdog,
                data: EventData::WorkflowNodeWatchdog {
                    run_id: run_id.to_string(),
                    node,
                },
            });
        }

        /// The two notices §3.4 asks for, and no others.
        ///
        /// Both ride the attention *change*, which is what keeps them to one
        /// each: rung 4 fires exactly once per node (the pure layer refuses to
        /// re-issue it), and a blocked lead writes `lead_blocked` once and then
        /// agrees with itself every 20 s until it moves.
        fn notice_for_watchdog(
            &mut self,
            run_id: &RunId,
            node: &ObservedNode,
            decision: &WatchdogDecision,
        ) {
            let message = match decision.attention {
                Some(Attention::Stuck) | Some(Attention::Unbound) => Some(format!(
                    "{} has made no progress for {} samples; karvex has stopped nudging it",
                    node_label(node),
                    decision.streak
                )),
                Some(Attention::BudgetExceeded) => Some(format!(
                    "{} has run past the timeout its definition authored",
                    node_label(node)
                )),
                Some(Attention::LeadBlocked) if node.is_lead => Some(
                    "the run's team lead is waiting for input; nothing will move until it is \
                     answered"
                        .to_string(),
                ),
                // A member waiting on a human, or on a task that has not
                // finished, is the run working as designed. It is on the node's
                // detail line and in `workflow.run.get`; a toast for it would
                // make the two that matter unreadable.
                _ => None,
            };
            let Some(message) = message else {
                return;
            };
            self.show_workflow_notice(UserNotice {
                level: NoticeLevel::Warning,
                run: Some(run_id.clone()),
                path: Some(node.path.clone()),
                message,
            });
        }

        /// Everything one sample reads, in one store round trip.
        fn read_watchdog_rows(&mut self, run_id: &RunId) -> Option<RunRows> {
            let wanted = run_id.clone();
            let loaded = self.workflow_store.call(move |cx| {
                let nodes = cx.block_on(cx.store().list_run_nodes(&wanted))?;
                let members = cx.block_on(cx.store().list_run_members(&wanted))?;
                let edges = cx.block_on(cx.store().list_run_edges(&wanted))?;
                Ok::<_, crate::workflow::store::StoreError>(RunRows {
                    nodes,
                    members,
                    edges,
                })
            });
            match loaded {
                Ok(Ok(rows)) => Some(rows),
                Ok(Err(error)) => {
                    warn!(%error, "the run's rows could not be read for a watchdog sample");
                    None
                }
                Err(unavailable) => {
                    warn!(
                        ?unavailable,
                        "the workflow store is unavailable; the watchdog did not sample"
                    );
                    None
                }
            }
        }

        /// The authored `timeout_ms` of every node in the run's definition.
        ///
        /// Read once per run and cached: a kvdag version is immutable by
        /// construction, so re-reading it every 20 s could only ever return the
        /// same answer. A version that cannot be read caches as empty rather
        /// than retrying forever — a budget karvex cannot see is one it must
        /// never claim was exceeded.
        fn watchdog_budgets(&mut self, run_id: &RunId) -> BTreeMap<NodeKey, u64> {
            if let Some(budgets) = self.workflow_watchdog.budgets.as_ref() {
                return budgets.clone();
            }
            let wanted = run_id.clone();
            let loaded = self.workflow_store.call(move |cx| {
                let Some(run) = cx.block_on(cx.store().get_run(&wanted))? else {
                    return Ok::<_, crate::workflow::store::StoreError>(None);
                };
                let kvdag = cx.block_on(cx.store().load_version(&run.version))?;
                Ok(Some(kvdag))
            });
            let budgets: BTreeMap<NodeKey, u64> = match loaded {
                Ok(Ok(Some(kvdag))) => kvdag
                    .nodes
                    .iter()
                    .filter_map(|node| node.timeout_ms.map(|budget| (node.key.clone(), budget)))
                    .collect(),
                Ok(Ok(None)) => BTreeMap::new(),
                Ok(Err(error)) => {
                    warn!(%error, "the run's definition could not be read for node budgets");
                    BTreeMap::new()
                }
                Err(unavailable) => {
                    warn!(
                        ?unavailable,
                        "the workflow store is unavailable; no node budgets"
                    );
                    BTreeMap::new()
                }
            };
            self.workflow_watchdog.budgets = Some(budgets.clone());
            budgets
        }
    }

    fn json_or_null(value: Option<String>) -> serde_json::Value {
        value.map_or(serde_json::Value::Null, serde_json::Value::String)
    }

    // ── observation ────────────────────────────────────────────────────────

    /// The `(last_state, last_state_at)` pair of every member of this sample.
    pub(super) fn member_samples(members: &[RunMemberRecord]) -> BTreeMap<String, MemberSample> {
        members
            .iter()
            .map(|member| {
                (
                    member.name.clone(),
                    MemberSample {
                        last_state: member.last_state.clone(),
                        last_state_at_unix_ms: member.last_state_at_unix_ms,
                    },
                )
            })
            .collect()
    }

    /// Turns one poll's rows into one [`ObservedNode`] per run node.
    ///
    /// Every node is offered to the pure layer, including the ones it will
    /// refuse: deciding *which* nodes are out of scope is a judgement
    /// (`SkipReason`), and duplicating that filter here is how the adapter and
    /// the pure layer start disagreeing about what is watched.
    pub(super) fn observe_run(
        rows: &RunRows,
        ladders: &BTreeMap<InstancePath, LadderState>,
        previous: &BTreeMap<String, MemberSample>,
        budgets: &BTreeMap<NodeKey, u64>,
        now_unix_ms: u64,
    ) -> Vec<NodeSample> {
        let by_name: BTreeMap<&str, &RunMemberRecord> = rows
            .members
            .iter()
            .map(|member| (member.name.as_str(), member))
            .collect();
        let by_path: BTreeMap<&str, &RunNodeRecord> = rows
            .nodes
            .iter()
            .map(|node| (node.instance_path.as_str(), node))
            .collect();

        // The lead is joined to its `run_member` row by session id, not by pane
        // id: the team config gives the lead the literal `"leader"` sentinel
        // instead of a pane (S1), so the pane karvex actually launched it into
        // is on the `.lead` node and nowhere else.
        let lead_node = by_path.get(LEAD_INSTANCE_PATH).copied();
        let lead_member = lead_node
            .and_then(|node| node.agent_session_id.as_deref())
            .and_then(|session| {
                rows.members
                    .iter()
                    .find(|member| member.session_id.as_deref() == Some(session))
            });
        let lead_pane = match (lead_node, lead_member) {
            (Some(node), Some(member)) => {
                pane_observation(member, node.pane_id.as_deref(), previous, now_unix_ms)
            }
            _ => None,
        };

        rows.nodes
            .iter()
            .map(|row| {
                let path = row.instance_path.clone();
                let is_lead = path.as_str() == LEAD_INSTANCE_PATH;
                let owner = if is_lead {
                    lead_owner(lead_member, lead_pane.as_ref())
                } else {
                    owner_state(row, &by_name, previous, now_unix_ms)
                };
                NodeSample {
                    stored_attention: row.attention,
                    node: ObservedNode {
                        task_id: row.task_id.clone(),
                        subject: row.subject.clone(),
                        status: task_status_for(row.status),
                        is_lead,
                        owner,
                        // A member inherits the lead's block rather than each
                        // node discovering it; the lead's own pane is already
                        // in `owner` when the node *is* the lead.
                        lead_pane: if is_lead { None } else { lead_pane.clone() },
                        blocked_by: blocking_tasks(&path, rows, &by_path),
                        budget_ms: budgets.get(&row.node_key).copied(),
                        elapsed_ms: row
                            .started_at_unix_ms
                            .map(|started| now_unix_ms.saturating_sub(started))
                            .unwrap_or_default(),
                        // Nothing samples a session's transcript yet, so karvex
                        // has measured no usage delta. Reporting a zero delta
                        // would be evidence of stillness it did not observe.
                        usage: None,
                        ladder: ladders.get(&path).copied().unwrap_or_default(),
                        // A closed run never reaches here (`poll_run_watchdog`
                        // returns first), so this is the observation, not a
                        // guess.
                        run_closed: false,
                        path,
                    },
                }
            })
            .collect()
    }

    /// Who the `.lead` node's owner is.
    ///
    /// A lead whose session karvex has not learned yet has samples and nobody
    /// to attribute them to, which is `NoPane` and therefore `Unknown` — the
    /// class whose surfaced attention is `Unbound`, in that word's own
    /// documented sense.
    fn lead_owner(member: Option<&RunMemberRecord>, pane: Option<&PaneObservation>) -> OwnerState {
        let Some(member) = member else {
            return OwnerState::NoPane {
                name: crate::workflow::binding::lead::LEAD_TARGET_NAME.to_string(),
            };
        };
        match pane {
            Some(pane) => OwnerState::Observed {
                name: member.name.clone(),
                pane: pane.clone(),
            },
            None => OwnerState::NoPane {
                name: member.name.clone(),
            },
        }
    }

    /// Who owns a projected task, and whether karvex can see them at all.
    fn owner_state(
        row: &RunNodeRecord,
        by_name: &BTreeMap<&str, &RunMemberRecord>,
        previous: &BTreeMap<String, MemberSample>,
        now_unix_ms: u64,
    ) -> OwnerState {
        let owner = row.owner.trim();
        if owner.is_empty() {
            return OwnerState::Unclaimed;
        }
        let Some(member) = by_name.get(owner) else {
            return OwnerState::Vanished {
                name: owner.to_string(),
            };
        };
        // A member the team config stopped calling active is a session that has
        // gone. Its `last_state` stops advancing rather than clearing (the
        // projection never regresses a resolved field), so without this it
        // would read as an eternal idle and be nudged forever.
        let pane = member
            .is_active
            .then(|| pane_observation(member, member.pane_id.as_deref(), previous, now_unix_ms))
            .flatten();
        match pane {
            Some(pane) => OwnerState::Observed {
                name: member.name.clone(),
                pane,
            },
            None => OwnerState::NoPane {
                name: member.name.clone(),
            },
        }
    }

    /// What karvex's own detection last said about a member's pane, as an
    /// observation with an age.
    fn pane_observation(
        member: &RunMemberRecord,
        pane_id: Option<&str>,
        previous: &BTreeMap<String, MemberSample>,
        now_unix_ms: u64,
    ) -> Option<PaneObservation> {
        let pane_id = pane_id?;
        let last_state = member.last_state.as_deref()?;
        let observed = MemberSample {
            last_state: Some(last_state.to_string()),
            last_state_at_unix_ms: member.last_state_at_unix_ms,
        };
        Some(PaneObservation {
            pane_id: pane_id.to_string(),
            state: agent_state_for(last_state),
            // `last_state_at` is when the state *started* (P8), so this is a
            // measured interval rather than a sample count — the number a
            // message can honestly quote at an agent.
            state_age_ms: now_unix_ms
                .saturating_sub(member.last_state_at_unix_ms.unwrap_or(now_unix_ms)),
            // A member this watchdog has never sampled before has not been
            // observed standing still, so the first sample of a run is movement
            // rather than the first tick of a stuck streak.
            changed_since_last_sample: previous.get(&member.name) != Some(&observed),
        })
    }

    /// `run_member.last_state`'s published vocabulary, back in the detection
    /// terms the pure layer classifies on. Written as a total match so a new
    /// member state cannot silently become `Unknown`.
    fn agent_state_for(last_state: &str) -> AgentState {
        match WorkflowMemberState::from_stored(last_state) {
            WorkflowMemberState::Working => AgentState::Working,
            WorkflowMemberState::Idle => AgentState::Idle,
            WorkflowMemberState::NeedsInput => AgentState::Blocked,
            WorkflowMemberState::Unknown => AgentState::Unknown,
        }
    }

    /// The tasks this node is still waiting on.
    ///
    /// `blockedBy` is materialised as `run_edge` rows by the projection, so the
    /// incoming edges are where a dependency lives. Only a source that is
    /// itself a *task* counts: an authored edge whose source the lead never
    /// turned into a task is a plan, not something a teammate is waiting on,
    /// and classifying that as an external wait would mute the watchdog for the
    /// rest of the run.
    fn blocking_tasks(
        path: &InstancePath,
        rows: &RunRows,
        by_path: &BTreeMap<&str, &RunNodeRecord>,
    ) -> Vec<BlockingTask> {
        rows.edges
            .iter()
            .filter(|edge| &edge.to == path)
            .filter_map(|edge| by_path.get(edge.from.as_str()).copied())
            .filter(|blocker| blocker.task_id.is_some() && !blocker.status.is_terminal())
            .map(|blocker| BlockingTask {
                id: blocker.task_id.clone().unwrap_or_default(),
                subject: blocker.subject.clone(),
            })
            .collect()
    }

    /// `run_node.status` back in Claude Code's own task vocabulary.
    ///
    /// The projection writes `in_progress` as `Running` and `completed` as
    /// `Succeeded`; every other value is a status this node never got from a
    /// task file, and is carried through as unknown rather than flattened into
    /// `pending` — the pure layer names it in its skip reason.
    fn task_status_for(status: NodeStatus) -> TaskStatus {
        match status {
            NodeStatus::Running => TaskStatus::InProgress,
            NodeStatus::Succeeded => TaskStatus::Completed,
            NodeStatus::Pending => TaskStatus::Pending,
            other => TaskStatus::Unknown(node_status_word(other).to_string()),
        }
    }

    fn node_status_word(status: NodeStatus) -> &'static str {
        match status {
            NodeStatus::Pending => "pending",
            NodeStatus::Ready => "ready",
            NodeStatus::Running => "running",
            NodeStatus::NeedsAttention => "needs_attention",
            NodeStatus::Blocked => "blocked",
            NodeStatus::Succeeded => "succeeded",
            NodeStatus::Failed => "failed",
            NodeStatus::Skipped => "skipped",
            NodeStatus::Restored => "restored",
            NodeStatus::Cancelled => "cancelled",
        }
    }

    /// How a notice names the node it is about.
    fn node_label(node: &ObservedNode) -> String {
        if node.is_lead {
            return "the run's team lead".to_string();
        }
        if node.subject.trim().is_empty() {
            return format!("node {}", node.path);
        }
        format!("task \"{}\"", node.subject)
    }
}

/// The adapter's own tests: an `App` with the in-memory store, a live lead run,
/// and rows written through exactly the writes the projection uses.
///
/// The ladder is walked by advancing a `now` the tick gate reads, never by
/// sleeping: a scenario that takes four samples takes four microseconds here.
/// What is deliberately *not* re-tested is any judgement — `workflow::watchdog`
/// owns 31 tests over the taxonomy, the rungs and the texts, and repeating them
/// through the adapter would only pin the adapter's ability to call a function.
/// These pin the halves the pure layer cannot have an opinion about: what was
/// read, what was said, what was written, and what was deliberately left alone.
#[cfg(all(test, feature = "workflow"))]
mod tests {
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    use crate::api::schema::Method;
    use crate::workflow::binding::identity::SessionReport;
    use crate::workflow::binding::lead::LeadBinding;
    use crate::workflow::model::{
        Attention, InstancePath, NodeKey, NodeStatus, RunEventKind, RunId, StoreWrite,
    };
    use crate::workflow::projection::{
        ObservedMember, ObservedMemberIdentity, ObservedTask, ObservedTeam, TaskStatus,
    };
    use crate::workflow::store::{RunEventRecord, RunNodeRecord};

    const TEAM_NAME: &str = "session-3cb241fe";
    const LEAD_SESSION_ID: &str = "3cb241fe-2c3a-4dd8-b8a0-5dd83dfc5aa2";
    const LEAD_PANE: &str = "w1:p1";
    const TEAMMATE: &str = "research";
    const TEAMMATE_PANE: &str = "w1:p4";
    const TEAMMATE_SESSION_ID: &str = "7694e312-4ac2-41d7-90ec-1277e61689df";
    /// The definition's one node key, and therefore the instance path its task
    /// projects onto once the subject prefix matches.
    const PLAN_PATH: &str = "plan";

    // ── harness ────────────────────────────────────────────────────────────

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
        // Every threshold at one, so one sample is one rung and a scenario reads
        // as the ladder rather than as arithmetic. The defaults (3 and 5) are
        // the pure layer's to test, and it does.
        app.workflow_policy.watchdog.stuck_threshold = 1;
        app.workflow_policy.watchdog.drift_threshold = 1;
        app.workflow_policy.watchdog.tick_secs = 1;
        app
    }

    /// A live, bound lead run over a one-node definition, with nothing
    /// projected and nothing reported yet.
    fn app_with_a_bound_run() -> (crate::app::App, RunId) {
        let mut app = test_app();
        let response = app.dispatch_api_request(
            "test.workflow.create",
            Method::WorkflowCreate(crate::api::schema::WorkflowCreateParams {
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
        let workflow_id = serde_json::from_str::<serde_json::Value>(&response)
            .ok()
            .and_then(|value| {
                value["result"]["workflow"]["workflow_id"]
                    .as_str()
                    .map(str::to_string)
            })
            .unwrap_or_else(|| panic!("the workflow was created: {response}"));
        let run_id = app.test_bind_a_live_lead_run(&workflow_id, "ship-it");
        if let Some(run) = app.workflow_lead.as_mut() {
            run.binding = Some(LeadBinding {
                team_name: TEAM_NAME.to_string(),
                lead_session_id: LEAD_SESSION_ID.to_string(),
            });
        }
        (app, run_id)
    }

    fn team(teammate_active: bool) -> ObservedTeam {
        ObservedTeam {
            name: TEAM_NAME.to_string(),
            lead_session_id: LEAD_SESSION_ID.to_string(),
            created_at_unix_ms: 1_700_000_000_000,
            members: vec![
                ObservedMember {
                    name: "team-lead".to_string(),
                    agent_id: Some(format!("team-lead@{TEAM_NAME}")),
                    agent_type: "team-lead".to_string(),
                    model: None,
                    // The sentinel Claude Code writes for the in-process lead:
                    // never a pane, which is why the `.lead` node carries the
                    // pane karvex actually launched it into.
                    pane_id: Some("leader".to_string()),
                    backend_type: "in-process".to_string(),
                    is_active: true,
                    cwd: Some("/repo".to_string()),
                    joined_at_unix_ms: Some(1_700_000_000_000),
                },
                ObservedMember {
                    name: TEAMMATE.to_string(),
                    agent_id: Some(format!("{TEAMMATE}@{TEAM_NAME}")),
                    agent_type: "Explore".to_string(),
                    model: Some("sonnet".to_string()),
                    pane_id: Some(TEAMMATE_PANE.to_string()),
                    backend_type: "tmux".to_string(),
                    is_active: teammate_active,
                    cwd: Some("/repo".to_string()),
                    joined_at_unix_ms: Some(1_700_000_000_000),
                },
            ],
        }
    }

    fn task(status: TaskStatus, owner: Option<&str>) -> ObservedTask {
        ObservedTask {
            id: "1".to_string(),
            subject: "plan: Draft the approach".to_string(),
            description: "Write the plan.".to_string(),
            active_form: None,
            owner: owner.map(str::to_string),
            status,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        }
    }

    fn identity(session_id: &str, last_state: &str) -> ObservedMemberIdentity {
        ObservedMemberIdentity {
            session_id: Some(session_id.to_string()),
            transcript_path: None,
            last_state: Some(last_state.to_string()),
        }
    }

    /// What karvex knows about each member's session this poll.
    ///
    /// The lead is always present, because it always is: karvex launched it,
    /// and a scenario that left it unidentified would be testing the *lead's*
    /// ladder by accident every time it meant to test a teammate's. Scenarios
    /// that are about the lead say so by passing it an idle state.
    fn identities(lead: &str, teammate: Option<&str>) -> BTreeMap<String, ObservedMemberIdentity> {
        let mut map = BTreeMap::from([("team-lead".to_string(), identity(LEAD_SESSION_ID, lead))]);
        if let Some(state) = teammate {
            map.insert(TEAMMATE.to_string(), identity(TEAMMATE_SESSION_ID, state));
        }
        map
    }

    /// What the run's team looks like to karvex right now, in one call.
    ///
    /// Runs the *real* [`ProjectionSnapshot::absorb`] and then makes the same
    /// two writes `absorb_run_projection` makes from its delta, so the rows the
    /// watchdog reads are the rows the projection would have written — and, in
    /// particular, so `last_state_at` is stamped by the projection's own
    /// only-when-it-changes rule rather than by the test.
    fn project(
        app: &mut crate::app::App,
        tasks: &[ObservedTask],
        team: &ObservedTeam,
        identities: &BTreeMap<String, ObservedMemberIdentity>,
        now_unix_ms: u64,
    ) {
        let (run_id, node_keys) = {
            let run = app.workflow_lead.as_ref().expect("a live run");
            (run.run_id.clone(), run.node_keys.clone())
        };
        let delta = {
            let run = app.workflow_lead.as_mut().expect("a live run");
            run.snapshot
                .absorb(tasks, Some(team), &node_keys, identities, now_unix_ms)
        };
        for task in &delta.tasks {
            app.persist_workflow_write(StoreWrite::RunTaskProjected {
                run: run_id.clone(),
                path: task.path.clone(),
                node_key: task
                    .node_key
                    .clone()
                    .unwrap_or_else(|| NodeKey::new(task.task.id.clone())),
                task_id: task.task.id.clone(),
                subject: task.task.subject.clone(),
                owner: task.task.owner.clone().unwrap_or_default(),
                status: match task.task.status {
                    TaskStatus::InProgress => NodeStatus::Running,
                    TaskStatus::Completed => NodeStatus::Succeeded,
                    _ => NodeStatus::Pending,
                },
                emergent: task.emergent,
                blocked_by: task.blocked_by.clone(),
                observed_at_unix_ms: now_unix_ms,
            });
        }
        for projected in &delta.members {
            let member = &projected.member;
            app.persist_workflow_write(StoreWrite::RunMemberSnapshot {
                run: run_id.clone(),
                name: member.name.clone(),
                agent_type: member.agent_type.clone(),
                model: member.model.clone().unwrap_or_default(),
                pane_id: member.tmux_pane_id().map(str::to_string),
                backend_type: member.backend_type.clone(),
                is_active: member.is_active,
                cwd: member.cwd.clone(),
                session_id: projected.identity.session_id.clone(),
                transcript_path: projected.identity.transcript_path.clone(),
                last_state: projected.identity.last_state.clone(),
                last_state_at_unix_ms: projected.identity.last_state_at_unix_ms,
                observed_at_unix_ms: now_unix_ms,
            });
        }
    }

    /// The `.lead` node's identity, as `record_lead_node_identity` writes it.
    fn identify_the_lead(app: &mut crate::app::App, run_id: &RunId) {
        app.persist_workflow_write(StoreWrite::RunNode {
            run: run_id.clone(),
            path: InstancePath::new(crate::workflow::model::LEAD_INSTANCE_PATH),
            status: NodeStatus::Running,
            attempt: 1,
            binding: Some(crate::workflow::model::NodeBinding {
                pane_id: crate::workflow::model::PublicPaneId::new(LEAD_PANE),
                terminal_id: crate::terminal::TerminalId::alloc(),
                agent_session_id: LEAD_SESSION_ID.to_string(),
                transcript_path: std::path::PathBuf::new(),
                node_dir: std::path::PathBuf::from("/runs/ship-it"),
                cwd: std::path::PathBuf::from("/repo"),
            }),
            usage: crate::workflow::model::NodeUsage::default(),
            evidence: None,
            succession: None,
            started_at_unix_ms: None,
            ended_at_unix_ms: None,
            restored_from: None,
        });
    }

    fn report(app: &mut crate::app::App, pane_id: &str, session_id: &str, socket: Option<&str>) {
        let run_id = app
            .workflow_lead
            .as_ref()
            .map(|run| run.run_id.to_string())
            .expect("a live run");
        app.record_run_session_report(&SessionReport {
            run_id,
            pane_id: Some(pane_id.to_string()),
            session_id: session_id.to_string(),
            transcript_path: None,
            cwd: Some("/repo".to_string()),
            source: Some("startup".to_string()),
            messaging_socket: socket.map(str::to_string),
            messaging_token: socket.map(|_| "50093985aaaabbbbccccddddeeeeffff".to_string()),
            agent_id: None,
        });
    }

    fn nodes(app: &mut crate::app::App, run: &RunId) -> Vec<RunNodeRecord> {
        let wanted = run.clone();
        app.workflow_store
            .call(move |cx| cx.block_on(cx.store().list_run_nodes(&wanted)))
            .expect("the in-memory store is available")
            .expect("the run's nodes read back")
    }

    fn node(app: &mut crate::app::App, run: &RunId, path: &str) -> RunNodeRecord {
        nodes(app, run)
            .into_iter()
            .find(|node| node.instance_path.as_str() == path)
            .unwrap_or_else(|| panic!("the run has a node at {path}"))
    }

    fn watchdog_journal(app: &mut crate::app::App, run: &RunId) -> Vec<RunEventRecord> {
        let wanted = run.clone();
        app.workflow_store
            .call(move |cx| cx.block_on(cx.store().list_run_events(&wanted)))
            .expect("the in-memory store is available")
            .expect("the run's journal reads back")
            .into_iter()
            .filter(|event| event.kind == RunEventKind::Watchdog)
            .collect()
    }

    fn rungs(app: &mut crate::app::App, run: &RunId) -> Vec<u64> {
        watchdog_journal(app, run)
            .into_iter()
            .filter_map(|event| event.payload["rung"].as_u64())
            .collect()
    }

    /// Drives `count` watchdog samples, one tick apart.
    fn sample(app: &mut crate::app::App, from: Instant, count: u64) -> Instant {
        let mut now = from;
        for _ in 0..count {
            app.poll_run_watchdog(now);
            now += Duration::from_secs(2);
        }
        now
    }

    /// A stand-in for a session's inbox socket that keeps accepting.
    ///
    /// The real one belongs to a live `claude`; what matters here is only that
    /// `message_run_session` finds something to connect to, because that is
    /// exactly the difference between `Delivered` and `Undelivered`.
    #[cfg(unix)]
    struct FakeInbox {
        dir: std::path::PathBuf,
        path: std::path::PathBuf,
        frames: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[cfg(unix)]
    impl FakeInbox {
        fn bind(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "karvex-p9-{label}-{}-{}",
                std::process::id(),
                crate::app::workflow::current_unix_ms()
            ));
            std::fs::create_dir_all(&dir).expect("a temp dir");
            let path = dir.join("inbox.sock");
            let listener =
                std::os::unix::net::UnixListener::bind(&path).expect("bind a stand-in inbox");
            let frames = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = std::sync::Arc::clone(&frames);
            std::thread::spawn(move || {
                use std::io::Read;
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let mut body = String::new();
                    let _ = stream.read_to_string(&mut body);
                    if let Ok(mut sink) = sink.lock() {
                        sink.push(body);
                    }
                }
            });
            Self { dir, path, frames }
        }

        fn path(&self) -> String {
            self.path.to_string_lossy().into_owned()
        }

        fn received(&self) -> Vec<String> {
            self.frames
                .lock()
                .map(|frames| frames.clone())
                .unwrap_or_default()
        }
    }

    #[cfg(unix)]
    impl Drop for FakeInbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    // ── the ladder ─────────────────────────────────────────────────────────

    /// The whole feature in one scenario: an idle teammate on an `in_progress`
    /// task walks 1 → 2 → 3 → 4, each rung is one journal entry, the first two
    /// reach the teammate and the third reaches the lead, and only the last one
    /// writes an opinion into `run_node.attention`.
    ///
    /// The rung-3 target matters as much as the rung numbers: karvex cannot
    /// restart, reassign or respawn anybody, so the ladder's last message is to
    /// the only actor that can.
    #[cfg(unix)]
    #[test]
    fn an_idle_owner_walks_the_ladder_and_each_rung_is_one_delivered_message() {
        let (mut app, run_id) = app_with_a_bound_run();
        let lead_inbox = FakeInbox::bind("lead");
        let teammate_inbox = FakeInbox::bind("teammate");
        identify_the_lead(&mut app, &run_id);
        report(
            &mut app,
            LEAD_PANE,
            LEAD_SESSION_ID,
            Some(&lead_inbox.path()),
        );
        report(
            &mut app,
            TEAMMATE_PANE,
            TEAMMATE_SESSION_ID,
            Some(&teammate_inbox.path()),
        );

        // Idle since twenty minutes ago, on a task the team still calls
        // `in_progress`: the canonical stuck case, and the only one a teammate
        // is ever messaged about.
        let now_unix_ms = crate::app::workflow::current_unix_ms();
        project(
            &mut app,
            &[task(TaskStatus::InProgress, Some(TEAMMATE))],
            &team(true),
            &identities("working", Some("idle")),
            now_unix_ms - 20 * 60_000,
        );

        // Five samples: one to establish the baseline, then one per rung.
        sample(&mut app, Instant::now(), 5);

        assert_eq!(
            rungs(&mut app, &run_id),
            vec![1, 2, 3, 4],
            "the ladder walks every rung in order and skips none"
        );
        let journal = watchdog_journal(&mut app, &run_id);
        for entry in &journal {
            assert_eq!(entry.payload["class"], "local_loop", "{:?}", entry.payload);
            assert_eq!(
                entry.run_node.is_some(),
                true,
                "every rung names the node it was about: {:?}",
                entry.payload
            );
        }
        for rung in 0..3 {
            assert_eq!(
                journal[rung].payload["delivered"],
                true,
                "rung {} was delivered: {:?}",
                rung + 1,
                journal[rung].payload
            );
            assert_eq!(journal[rung].payload["channel"], "inbox_socket");
        }
        assert_eq!(
            journal[0].payload["target"], TEAMMATE,
            "rungs 1 and 2 are the teammate's"
        );
        assert_eq!(journal[1].payload["target"], TEAMMATE);
        assert_eq!(
            journal[2].payload["target"], "team-lead",
            "rung 3 asks the only actor that can reassign or respawn"
        );
        assert_eq!(
            journal[3].payload["delivered"], false,
            "rung 4 has no message at all: {:?}",
            journal[3].payload
        );
        assert!(
            journal[3].payload["text"].is_null(),
            "and therefore no text: {:?}",
            journal[3].payload
        );

        // The messages karvex composed actually crossed the sockets, framed so
        // the receiver can tell runtime steering from its human.
        let to_teammate = teammate_inbox.received();
        assert_eq!(to_teammate.len(), 2, "two rungs reached the teammate");
        assert!(
            to_teammate[0].contains("[karvex · watchdog]"),
            "{to_teammate:?}"
        );
        assert!(
            to_teammate[0].contains("plan: Draft the approach"),
            "a nudge names the task the teammate sees in its own list: {to_teammate:?}"
        );
        assert_eq!(lead_inbox.received().len(), 1, "one escalation to the lead");

        // And only the last rung is an opinion. A node being talked to is not
        // yet a node karvex has given up on.
        let plan = node(&mut app, &run_id, PLAN_PATH);
        assert_eq!(plan.attention, Some(Attention::Stuck));
        assert_eq!(
            plan.status,
            NodeStatus::Running,
            "the projected status is Claude Code's and is never overwritten"
        );
        assert_eq!(
            plan.watchdog_interventions, 1,
            "the counter moves with the opinion, not with the message; the rung history \
             is the journal's"
        );
    }

    /// A rung whose message did not land is not spent.
    ///
    /// This is the honesty rule the whole ladder rests on: karvex holds no
    /// endpoint for this teammate, so rung 1 is composed, refused, and composed
    /// again — identically — and only after the second failure does the ladder
    /// give up on talking and surface. A nudge nobody received must never look
    /// like a nudge that was ignored.
    #[test]
    fn an_undelivered_rung_is_retried_verbatim_and_two_of_them_surface() {
        let (mut app, run_id) = app_with_a_bound_run();
        identify_the_lead(&mut app, &run_id);
        // Nothing reported: the run has no addressable session at all.
        let now_unix_ms = crate::app::workflow::current_unix_ms();
        project(
            &mut app,
            &[task(TaskStatus::InProgress, Some(TEAMMATE))],
            &team(true),
            &identities("working", Some("idle")),
            now_unix_ms - 20 * 60_000,
        );

        sample(&mut app, Instant::now(), 4);

        let journal = watchdog_journal(&mut app, &run_id);
        assert_eq!(
            rungs(&mut app, &run_id),
            vec![1, 1, 4],
            "rung 1 is retried rather than consumed, and two failures surface"
        );
        assert!(
            journal
                .iter()
                .all(|event| event.payload["rung"] != serde_json::json!(2)),
            "rung 2 is never reached: no rung is spent on a message that did not land"
        );
        let first = &journal[0].payload;
        let second = &journal[1].payload;
        assert_eq!(first["delivered"], false);
        assert_eq!(first["channel"], serde_json::Value::Null);
        assert_eq!(first["target"], TEAMMATE);
        // The retry is the same *rung*, recomposed. "Verbatim" cannot mean
        // byte-identical here and should not: rung 1's text quotes how long the
        // pane has been idle and how many samples that is, so replaying the
        // first attempt's sentence would tell the teammate a stale number in
        // order to look consistent. What must not change is which rung is being
        // spent, and it does not.
        for text in [&first["text"], &second["text"]] {
            let text = text.as_str().unwrap_or_default();
            assert!(text.starts_with("[karvex · watchdog]"), "{text}");
            assert!(
                text.contains("still working: name the next concrete step"),
                "both attempts are rung 1's nudge, not two different rungs: {text}"
            );
        }
        assert_eq!(first["streak"], 1);
        assert_eq!(
            second["streak"], 2,
            "and the measurement it quotes is the current one, not the first one"
        );
        assert!(
            first["undelivered_reason"]
                .as_str()
                .is_some_and(|reason| !reason.is_empty()),
            "a run has to be able to explain why it could not reach a session: {first:?}"
        );
        assert_eq!(
            node(&mut app, &run_id, PLAN_PATH).attention,
            Some(Attention::Stuck)
        );
    }

    /// The lead's ladder is 1 → 2 → 4: there is nobody above it to escalate to,
    /// and a rung 3 addressed to itself would be a message to nobody.
    #[cfg(unix)]
    #[test]
    fn the_lead_walks_its_own_shorter_ladder() {
        let (mut app, run_id) = app_with_a_bound_run();
        let inbox = FakeInbox::bind("lead-ladder");
        identify_the_lead(&mut app, &run_id);
        report(&mut app, LEAD_PANE, LEAD_SESSION_ID, Some(&inbox.path()));
        let now_unix_ms = crate::app::workflow::current_unix_ms();
        project(
            &mut app,
            &[],
            &team(true),
            &identities("idle", None),
            now_unix_ms - 30 * 60_000,
        );

        sample(&mut app, Instant::now(), 4);

        assert_eq!(
            rungs(&mut app, &run_id),
            vec![1, 2, 4],
            "the lead is never escalated to itself"
        );
        assert_eq!(inbox.received().len(), 2);
        let lead = node(
            &mut app,
            &run_id,
            crate::workflow::model::LEAD_INSTANCE_PATH,
        );
        assert_eq!(lead.attention, Some(Attention::Stuck));
    }

    // ── the three ways to say nothing ──────────────────────────────────────

    /// A teammate that is working is a run working. Nothing is said, written,
    /// journalled or emitted.
    #[test]
    fn a_working_owner_produces_no_rung_no_attention_and_no_journal() {
        let (mut app, run_id) = app_with_a_bound_run();
        identify_the_lead(&mut app, &run_id);
        let now_unix_ms = crate::app::workflow::current_unix_ms();
        project(
            &mut app,
            &[task(TaskStatus::InProgress, Some(TEAMMATE))],
            &team(true),
            &identities("working", Some("working")),
            now_unix_ms - 20 * 60_000,
        );

        sample(&mut app, Instant::now(), 6);

        assert!(watchdog_journal(&mut app, &run_id).is_empty());
        let plan = node(&mut app, &run_id, PLAN_PATH);
        assert_eq!(plan.attention, None);
        assert_eq!(plan.watchdog_interventions, 0);
    }

    /// `NotWatched` means touch nothing — including an `attention` a previous
    /// sample wrote.
    ///
    /// Asserted against a pre-set opinion rather than against an empty column,
    /// because "wrote nothing" and "wrote `None`" are indistinguishable on a
    /// row that was already empty, and they are very different behaviours: the
    /// second would silently clear the record of a node that stopped being
    /// watched because its task completed.
    #[test]
    fn a_not_watched_node_is_not_touched_at_all() {
        let (mut app, run_id) = app_with_a_bound_run();
        identify_the_lead(&mut app, &run_id);
        let now_unix_ms = crate::app::workflow::current_unix_ms();
        // A completed task, owned by a teammate that has been idle for ages.
        project(
            &mut app,
            &[task(TaskStatus::Completed, Some(TEAMMATE))],
            &team(true),
            &identities("working", Some("idle")),
            now_unix_ms - 40 * 60_000,
        );
        app.persist_workflow_write(StoreWrite::RunNodeAttention {
            run: run_id.clone(),
            path: InstancePath::new(PLAN_PATH),
            attention: Some(Attention::NeedsInput),
            observed_at_unix_ms: now_unix_ms,
        });
        let before = node(&mut app, &run_id, PLAN_PATH);

        sample(&mut app, Instant::now(), 6);

        let after = node(&mut app, &run_id, PLAN_PATH);
        assert_eq!(
            after.attention,
            Some(Attention::NeedsInput),
            "a completed task's column keeps whatever it held"
        );
        assert_eq!(
            after.watchdog_interventions, before.watchdog_interventions,
            "and no write means no intervention counted"
        );
        assert!(
            watchdog_journal(&mut app, &run_id).is_empty(),
            "nor a journal entry"
        );
    }

    /// The kill switch is a kill switch: zero samples, not samples whose
    /// verdicts are discarded.
    #[test]
    fn the_kill_switch_produces_no_samples_at_all() {
        let (mut app, run_id) = app_with_a_bound_run();
        app.workflow_policy.watchdog.enabled = false;
        identify_the_lead(&mut app, &run_id);
        let now_unix_ms = crate::app::workflow::current_unix_ms();
        project(
            &mut app,
            &[task(TaskStatus::InProgress, Some(TEAMMATE))],
            &team(true),
            &identities("working", Some("idle")),
            now_unix_ms - 40 * 60_000,
        );

        let mut now = Instant::now();
        for _ in 0..8 {
            assert!(!app.poll_run_watchdog(now));
            now += Duration::from_secs(2);
        }

        assert!(watchdog_journal(&mut app, &run_id).is_empty());
        assert_eq!(node(&mut app, &run_id, PLAN_PATH).attention, None);
    }

    // ── re-evaluation ──────────────────────────────────────────────────────

    /// Attention is an opinion re-formed every tick, not a latch.
    ///
    /// A node that starts moving again clears it — and the clearing write must
    /// not count as an intervention, because karvex did not intervene, it just
    /// stopped being worried.
    #[test]
    fn attention_clears_when_the_node_moves_again() {
        let (mut app, run_id) = app_with_a_bound_run();
        identify_the_lead(&mut app, &run_id);
        let now_unix_ms = crate::app::workflow::current_unix_ms();
        project(
            &mut app,
            &[task(TaskStatus::InProgress, Some(TEAMMATE))],
            &team(true),
            &identities("working", Some("idle")),
            now_unix_ms - 20 * 60_000,
        );
        let now = sample(&mut app, Instant::now(), 4);
        let surfaced = node(&mut app, &run_id, PLAN_PATH);
        assert_eq!(surfaced.attention, Some(Attention::Stuck));
        let interventions = surfaced.watchdog_interventions;

        // The teammate picks the work back up.
        project(
            &mut app,
            &[task(TaskStatus::InProgress, Some(TEAMMATE))],
            &team(true),
            &identities("working", Some("working")),
            crate::app::workflow::current_unix_ms(),
        );
        sample(&mut app, now, 1);

        let moving = node(&mut app, &run_id, PLAN_PATH);
        assert_eq!(
            moving.attention, None,
            "the watchdog re-evaluates every tick; `None` clears"
        );
        assert_eq!(
            moving.watchdog_interventions, interventions,
            "clearing is not intervening"
        );
    }

    /// A node whose owner is waiting on a human is surfaced once and never
    /// messaged: a prompt cannot answer a permission dialog.
    #[test]
    fn an_external_wait_is_surfaced_and_never_messaged() {
        let (mut app, run_id) = app_with_a_bound_run();
        identify_the_lead(&mut app, &run_id);
        let now_unix_ms = crate::app::workflow::current_unix_ms();
        project(
            &mut app,
            &[task(TaskStatus::InProgress, Some(TEAMMATE))],
            &team(true),
            &identities("working", Some("needs_input")),
            now_unix_ms - 20 * 60_000,
        );

        sample(&mut app, Instant::now(), 4);

        assert!(
            rungs(&mut app, &run_id).is_empty(),
            "an external wait never walks the ladder"
        );
        assert_eq!(
            node(&mut app, &run_id, PLAN_PATH).attention,
            Some(Attention::NeedsInput),
            "it is surfaced instead, in karvex's own column"
        );
    }

    /// An owner that left the roster has nobody to nudge, so the ladder skips
    /// straight past the two teammate-facing rungs to the lead.
    #[cfg(unix)]
    #[test]
    fn an_inactive_owner_is_escalated_rather_than_nudged() {
        let (mut app, run_id) = app_with_a_bound_run();
        let inbox = FakeInbox::bind("vanished");
        identify_the_lead(&mut app, &run_id);
        report(&mut app, LEAD_PANE, LEAD_SESSION_ID, Some(&inbox.path()));
        let now_unix_ms = crate::app::workflow::current_unix_ms();
        project(
            &mut app,
            &[task(TaskStatus::InProgress, Some(TEAMMATE))],
            &team(false),
            &identities("working", Some("idle")),
            now_unix_ms - 20 * 60_000,
        );

        sample(&mut app, Instant::now(), 3);

        assert_eq!(
            rungs(&mut app, &run_id),
            vec![3, 4],
            "rungs 1 and 2 are skipped: there is nobody to send them to"
        );
        let journal = watchdog_journal(&mut app, &run_id);
        assert_eq!(journal[0].payload["class"], "unknown");
        assert_eq!(journal[0].payload["target"], "team-lead");
        assert_eq!(
            node(&mut app, &run_id, PLAN_PATH).attention,
            Some(Attention::Unbound),
            "samples with no session to attribute them to are `unbound`, not `stuck`"
        );
    }

    // ── cadence and lifecycle ──────────────────────────────────────────────

    /// The watchdog is layered on the 2 s poll, not driven by it.
    #[test]
    fn the_sample_is_on_its_own_cadence_not_the_polls() {
        let (mut app, run_id) = app_with_a_bound_run();
        app.workflow_policy.watchdog.tick_secs = 20;
        identify_the_lead(&mut app, &run_id);
        let now_unix_ms = crate::app::workflow::current_unix_ms();
        project(
            &mut app,
            &[task(TaskStatus::InProgress, Some(TEAMMATE))],
            &team(true),
            &identities("working", Some("idle")),
            now_unix_ms - 20 * 60_000,
        );

        // Ten polls two seconds apart is one twenty-second window, so exactly
        // two samples land in it: the baseline and the first rung.
        sample(&mut app, Instant::now(), 11);

        assert_eq!(
            rungs(&mut app, &run_id),
            vec![1],
            "eleven polls at a 20 s cadence are two samples, not eleven"
        );
    }

    /// A run that has not recognised its team yet is not sampled.
    ///
    /// Not a detail: before the bind there is no member row and no task, so
    /// every node would classify `Unknown` and the ladder would put a red mark
    /// on the launch of every healthy run — to report, worse and earlier, the
    /// failure the bind deadline already owns.
    #[test]
    fn an_unbound_run_is_not_sampled_at_all() {
        let (mut app, run_id) = app_with_a_bound_run();
        if let Some(run) = app.workflow_lead.as_mut() {
            run.binding = None;
        }

        sample(&mut app, Instant::now(), 6);

        assert!(watchdog_journal(&mut app, &run_id).is_empty());
        assert_eq!(
            node(
                &mut app,
                &run_id,
                crate::workflow::model::LEAD_INSTANCE_PATH
            )
            .attention,
            None
        );
    }

    /// A closed run is never classified, and the memory of it goes when the run
    /// does.
    #[test]
    fn a_closed_run_stops_the_watchdog() {
        let (mut app, run_id) = app_with_a_bound_run();
        identify_the_lead(&mut app, &run_id);
        let now_unix_ms = crate::app::workflow::current_unix_ms();
        project(
            &mut app,
            &[task(TaskStatus::InProgress, Some(TEAMMATE))],
            &team(true),
            &identities("working", Some("idle")),
            now_unix_ms - 20 * 60_000,
        );
        if let Some(run) = app.workflow_lead.as_mut() {
            run.closed = true;
        }

        sample(&mut app, Instant::now(), 6);
        assert!(watchdog_journal(&mut app, &run_id).is_empty());

        app.workflow_lead = None;
        assert!(!app.poll_run_watchdog(Instant::now()));
    }
}
