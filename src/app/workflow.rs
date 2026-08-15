//! `App` glue for the workflow subsystem.
//!
//! Karvex no longer executes runs itself (`09-agent-teams-rework.md` §0): a run
//! is one Claude Code team lead in a pane, and `src/app/workflow_lead.rs` owns
//! its lifecycle. What is left here is what both the lead path and the
//! historical read paths share — the `[workflow]` policy, the wire projections
//! of the model's enums and of a stored run summary, the workspace cwd a
//! workflow pane is opened in, and the one place a workflow notice reaches the
//! user.
//!
//! Almost everything here is reachable only through the `workflow.*` API
//! surface, which is stubbed out when the `workflow` feature is off (the MSVC
//! cross-lint and slim source builds). The wire types these projections produce
//! are declared unconditionally — that is what keeps the published schema
//! artifact single-valued on both legs — so the projections have to *compile*
//! without the feature even though nothing can call them. Hence the
//! feature-scoped allow: the shipped configuration still lints every item below
//! for real, and only the feature-off leg is quiet.
#![cfg_attr(not(feature = "workflow"), allow(dead_code))]

use std::path::PathBuf;

use tracing::warn;

use crate::api::schema::{
    ErrorResponse, WorkflowDemand, WorkflowEdgeKind, WorkflowEvidence, WorkflowNodeStatus,
    WorkflowRunStatus, WorkflowSuccession, WorkflowTier,
};
use crate::app::state::{ToastKind, ToastNotification};
use crate::app::App;
#[cfg(feature = "workflow")]
use crate::workflow::model::InstancePath;
use crate::workflow::model::{
    Demand, EdgeKind, Evidence, NodeStatus, NoticeLevel, RunStatus, Succession, UserNotice,
};
use crate::workflow::tier::Tier;

/// Why a run could not be started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkflowStartError {
    /// One team lead at a time. A run is a lead session plus the teammates it
    /// owns, and a second lead launched into the same workspace would compete
    /// for the same panes and the same team-config match window
    /// (`09-agent-teams-rework.md` §3.1).
    RunInFlight,
}

impl WorkflowStartError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::RunInFlight => "workflow_run_in_flight",
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::RunInFlight => "another workflow run's team lead is still live on this server",
        }
    }
}

/// The half of the `[workflow]` config block the app enforces, as opposed to
/// the definition-time knobs the kvdag carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkflowPolicy {
    /// `workflow.history_context_runs` (§4 D21, D22): how many past run
    /// summaries a new run may be given as context.
    pub(crate) history_context_runs: usize,
    /// `workflow.max_parallel_nodes` (D-6, WI-R2): nothing schedules nodes any
    /// more, so this cannot be a cap the app enforces. Carried through to the
    /// lead prompt render as a concurrency hint instead of staying a value
    /// `config::model` parses and nothing else ever reads.
    pub(crate) max_parallel_nodes: usize,
    /// The watchdog's half of `[workflow]` — `watchdog_enabled`,
    /// `watchdog_tick_secs`, `stuck_threshold`, `drift_threshold`
    /// (`.local/prd/phase4-retarget-plan.md` §3.4, D-8). Distilled here for the
    /// same reason the two knobs above are: the poll that reads it runs every
    /// two seconds and `App` holds no `Config`.
    #[cfg_attr(not(feature = "workflow"), allow(dead_code))]
    pub(crate) watchdog: crate::workflow::watchdog::WatchdogPolicy,
    /// `workflow.review_max_interviews` (§3.5): the cap on how many members one
    /// review cycle interviews.
    pub(crate) review_max_interviews: usize,
}

impl Default for WorkflowPolicy {
    /// Mirrors `config::model`'s defaults so a `WorkflowPolicy` built without a
    /// config behaves like one built from the default config.
    fn default() -> Self {
        Self {
            history_context_runs: 3,
            max_parallel_nodes: 4,
            watchdog: crate::workflow::watchdog::WatchdogPolicy::from_config(
                &crate::config::Config::default(),
            ),
            review_max_interviews: 6,
        }
    }
}

pub(crate) fn workflow_policy(config: &crate::config::Config) -> WorkflowPolicy {
    WorkflowPolicy {
        history_context_runs: config.workflow.history_context_runs,
        max_parallel_nodes: config.workflow.max_parallel_nodes,
        watchdog: crate::workflow::watchdog::WatchdogPolicy::from_config(config),
        review_max_interviews: config.workflow.review_max_interviews,
    }
}

impl App {
    /// The directory a workflow pane is opened in: the workspace's own
    /// resolved identity cwd, exactly as a hand-opened pane would get.
    pub(crate) fn workflow_node_cwd_for(&self, ws_idx: usize) -> PathBuf {
        let follow = self.state.workspaces.get(ws_idx).and_then(|workspace| {
            workspace.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
        });
        self.resolve_new_terminal_cwd(follow)
    }

    /// `04` §9: a store write failure degrades a run's journal, and that
    /// degradation is surfaced. The run itself is unaffected — the store is the
    /// record, not the executor — so this is a warning, and it is shown once
    /// per server rather than once per write.
    pub(crate) fn mark_workflow_persistence_degraded(&mut self) {
        if self.workflow_persistence_degraded {
            return;
        }
        self.workflow_persistence_degraded = true;
        warn!("workflow run persistence degraded: part of the journal was not written");
        self.show_workflow_notice(UserNotice {
            level: NoticeLevel::Warning,
            run: None,
            path: None,
            message: "the run's journal is incomplete: a durable write could not be stored"
                .to_string(),
        });
    }

    /// The bound `keys.open_workflow_dag` was pressed with nothing to show.
    pub(crate) fn notify_no_workflow_run(&mut self) {
        self.show_workflow_notice(UserNotice {
            level: NoticeLevel::Info,
            run: None,
            path: None,
            message: "no workflow run on this server — start one with kvx workflow run start"
                .to_string(),
        });
    }

    /// Shows one workflow notice through whichever delivery the user
    /// configured.
    ///
    /// This used to answer only for `ToastDelivery::Karvex` and drop the notice
    /// on the floor for every other setting, so a user who had deliberately
    /// asked for terminal or desktop notifications got *fewer* workflow
    /// notifications than the default. The escalation reuses exactly the
    /// notifier the agent-state path uses; no second notification model.
    pub(crate) fn show_workflow_notice(&mut self, notice: UserNotice) {
        let kind = match notice.level {
            NoticeLevel::Info => ToastKind::Finished,
            NoticeLevel::Warning | NoticeLevel::Error => ToastKind::NeedsAttention,
        };
        let title = match notice.path.as_ref() {
            Some(path) => format!("Workflow node {path}"),
            None => "Workflow run".to_string(),
        };
        match self.state.toast_config.delivery {
            crate::config::ToastDelivery::Karvex => {
                // One rendered slot, but a workflow batch routinely raises a
                // per-node notice immediately followed by the run-level one,
                // and assigning the slot destroyed the first (H4). `push_toast`
                // shows this one now or queues it behind whatever is showing;
                // the expiry pop in `App::expire_toast_or_show_next` is what
                // drains it. The `Terminal`/`System` arms below fire one OS
                // notification per notice and never contended for the slot, so
                // they are unchanged.
                if self.state.push_toast(ToastNotification {
                    kind,
                    title,
                    context: notice.message,
                    position: None,
                    target: None,
                }) {
                    self.arm_toast_deadline();
                }
            }
            crate::config::ToastDelivery::Terminal | crate::config::ToastDelivery::System
                if self.local_terminal_notifications =>
            {
                let notify = match self.state.toast_config.delivery {
                    crate::config::ToastDelivery::Terminal => {
                        crate::terminal_notify::show_notification
                    }
                    _ => crate::platform::show_desktop_notification,
                };
                let _ = notify(&title, Some(&notice.message));
            }
            _ => {}
        }
    }
}

/// Wire projection of one stored `run_summary` row.
///
/// Lives here rather than beside the handlers because both the
/// `workflow.summary.*` responses and the `workflow.run.summarized` event
/// project the same record, and two spellings of one mapping is how a field
/// starts appearing on one surface and not the other.
///
/// Feature-gated because its *input* is a store type: only `src/workflow/store`
/// is behind the `workflow` feature, and a slim build has no `RunSummaryRecord`
/// to project. The wire type it produces compiles unconditionally, which is what
/// keeps the schema artifact single-valued on both legs.
#[cfg(feature = "workflow")]
pub(crate) fn wire_run_summary_record(
    record: crate::workflow::store::RunSummaryRecord,
) -> crate::api::schema::WorkflowRunSummaryInfo {
    crate::api::schema::WorkflowRunSummaryInfo {
        run_id: record.run.to_string(),
        workflow_id: record.workflow.to_string(),
        workflow_name: record.workflow_name,
        version_id: record.version.to_string(),
        text: record.text,
        outcome: record.outcome,
        highlights: record.highlights,
        open_gaps: record.open_gaps,
        per_node: record
            .per_node
            .into_iter()
            .map(|line| crate::api::schema::WorkflowSummaryNodeLine {
                node_key: line.node_key,
                verdict: line.verdict,
                one_liner: line.one_liner,
            })
            .collect(),
        token_estimate: record.token_estimate,
        generated_by_path: record
            .generated_by_path
            .as_ref()
            .map(InstancePath::to_string),
        created_at_unix_ms: record.created_at_unix_ms,
        run_pruned: record.run_pruned,
    }
}

/// The user-facing message in a `workflow.node.steer` response, or `None` when
/// the steer was accepted.
///
/// The envelope is the only place the TUI learns that a delivery was refused —
/// `workflow_node_delivery_failed` for a pane that would not take the text and
/// `workflow_node_not_running` for a node with no pane at all.
pub(crate) fn steer_failure_message(response: &str) -> Option<String> {
    let error = serde_json::from_str::<ErrorResponse>(response).ok()?;
    Some(format!("steer not delivered: {}", error.error.message))
}

pub(crate) fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

pub(crate) fn wire_tier(tier: Tier) -> WorkflowTier {
    match tier {
        Tier::Auto => WorkflowTier::Auto,
        Tier::Max => WorkflowTier::Max,
        Tier::High => WorkflowTier::High,
        Tier::Medium => WorkflowTier::Medium,
        Tier::Low => WorkflowTier::Low,
    }
}

pub(crate) fn wire_demand(demand: Demand) -> WorkflowDemand {
    match demand {
        Demand::Peak => WorkflowDemand::Peak,
        Demand::Critical => WorkflowDemand::Critical,
        Demand::Standard => WorkflowDemand::Standard,
        Demand::Light => WorkflowDemand::Light,
    }
}

pub(crate) fn wire_edge_kind(kind: EdgeKind) -> WorkflowEdgeKind {
    match kind {
        EdgeKind::Sequence => WorkflowEdgeKind::Sequence,
        EdgeKind::Data => WorkflowEdgeKind::Data,
        EdgeKind::Conditional => WorkflowEdgeKind::Conditional,
    }
}

pub(crate) fn wire_run_status(status: RunStatus) -> WorkflowRunStatus {
    match status {
        RunStatus::Pending => WorkflowRunStatus::Pending,
        RunStatus::Running => WorkflowRunStatus::Running,
        RunStatus::Paused => WorkflowRunStatus::Paused,
        RunStatus::Succeeded => WorkflowRunStatus::Succeeded,
        RunStatus::Failed => WorkflowRunStatus::Failed,
        RunStatus::Cancelled => WorkflowRunStatus::Cancelled,
    }
}

pub(crate) fn wire_node_status(status: NodeStatus) -> WorkflowNodeStatus {
    match status {
        NodeStatus::Pending => WorkflowNodeStatus::Pending,
        NodeStatus::Ready => WorkflowNodeStatus::Ready,
        NodeStatus::Running => WorkflowNodeStatus::Running,
        NodeStatus::NeedsAttention => WorkflowNodeStatus::NeedsAttention,
        NodeStatus::Blocked => WorkflowNodeStatus::Blocked,
        NodeStatus::Succeeded => WorkflowNodeStatus::Succeeded,
        NodeStatus::Failed => WorkflowNodeStatus::Failed,
        NodeStatus::Skipped => WorkflowNodeStatus::Skipped,
        NodeStatus::Restored => WorkflowNodeStatus::Restored,
        NodeStatus::Cancelled => WorkflowNodeStatus::Cancelled,
    }
}

pub(crate) fn wire_evidence(evidence: Evidence) -> WorkflowEvidence {
    match evidence {
        Evidence::SelfReport => WorkflowEvidence::SelfReport,
        Evidence::Hook => WorkflowEvidence::Hook,
        Evidence::Detection => WorkflowEvidence::Detection,
        Evidence::Restored => WorkflowEvidence::Restored,
    }
}

pub(crate) fn wire_succession(succession: &Succession) -> WorkflowSuccession {
    match succession {
        Succession::Satisfied => WorkflowSuccession::Satisfied,
        Succession::Blocked {
            reason,
            resume_when,
        } => WorkflowSuccession::Blocked {
            reason: reason.clone(),
            resume_when: resume_when.clone(),
        },
        Succession::NoFollowup { evidence } => WorkflowSuccession::NoFollowup {
            evidence: evidence.clone(),
        },
    }
}

/// `Succession::Blocked` is the only succession that carries a blocker; the
/// wire keeps it in its own field so a client can render it without matching on
/// the succession shape.
pub(crate) fn wire_blocker(succession: &Succession) -> Option<serde_json::Value> {
    match succession {
        Succession::Blocked {
            reason,
            resume_when,
        } => Some(serde_json::json!({
            "reason": reason,
            "resume_when": resume_when,
        })),
        Succession::Satisfied | Succession::NoFollowup { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_reads_the_workflow_config_block() {
        let config = crate::config::Config::default();
        assert_eq!(
            workflow_policy(&config),
            WorkflowPolicy {
                history_context_runs: config.workflow.history_context_runs,
                max_parallel_nodes: config.workflow.max_parallel_nodes,
                watchdog: crate::workflow::watchdog::WatchdogPolicy::from_config(&config),
                review_max_interviews: config.workflow.review_max_interviews,
            }
        );
        assert_eq!(workflow_policy(&config), WorkflowPolicy::default());
    }

    #[test]
    fn a_refused_steer_response_becomes_a_message() {
        let refused = r#"{"id":"x","error":{"code":"workflow_node_delivery_failed",
                          "message":"pane w1:p1 would not take the text"}}"#;
        let message = steer_failure_message(refused).expect("a refusal is a message");
        assert!(
            message.contains("would not take the text"),
            "the refusal has to carry the API's own reason: {message}"
        );

        let accepted = r#"{"id":"x","result":{"delivered":true}}"#;
        assert_eq!(steer_failure_message(accepted), None);
    }

    /// The accounting that replaced the store queue's `take_write_failures`
    /// counter (`src/app/workflow_store.rs`): `persist_workflow_write` waits
    /// for every write now, so a rejection degrades the run here, on the spot.
    /// Once per server, not once per write — a lead run writes several rows a
    /// poll, and a store that is refusing them is refusing all of them.
    #[test]
    fn a_degraded_journal_is_surfaced_once_per_server() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = crate::config::Config::default();
        // The shipped default delivery is `off`, and this notice is about the
        // delivery, so it has to be asked for.
        config.ui.toast.delivery = crate::config::ToastDelivery::Karvex;
        let mut app = App::new(&config, true, None, api_rx, crate::api::EventHub::default());

        app.mark_workflow_persistence_degraded();
        assert!(app.workflow_persistence_degraded);
        let shown = app
            .state
            .toast
            .as_ref()
            .expect("the degradation is shown, not only logged");
        assert_eq!(shown.kind, ToastKind::NeedsAttention);
        assert!(
            shown.context.contains("durable write"),
            "the notice says what was lost: {}",
            shown.context
        );

        app.mark_workflow_persistence_degraded();
        assert!(
            app.state.toast_queue.is_empty(),
            "a second failing write must not queue a second notice"
        );
    }

    #[test]
    fn a_second_run_is_refused_by_name() {
        assert_eq!(
            WorkflowStartError::RunInFlight.code(),
            "workflow_run_in_flight"
        );
        assert!(WorkflowStartError::RunInFlight.message().contains("lead"));
    }
}
