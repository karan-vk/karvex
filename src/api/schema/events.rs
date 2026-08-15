use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::common::{AgentStatus, ReadSource};
use super::panes::{PaneInfo, PaneReadResult, PaneScrollInfo};
use super::tabs::TabInfo;
use super::workflows::{
    WorkflowGrowthLimitKind, WorkflowInterrogationInfo, WorkflowReviewInfo, WorkflowRunInfo,
    WorkflowRunNodeInfo, WorkflowRunSummaryInfo,
};
use super::workspaces::WorkspaceInfo;
use super::worktrees::WorktreeInfo;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EventsSubscribeParams {
    pub subscriptions: Vec<Subscription>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum Subscription {
    #[serde(rename = "workspace.created")]
    WorkspaceCreated {},
    #[serde(rename = "workspace.updated")]
    WorkspaceUpdated {},
    #[serde(rename = "workspace.metadata_updated")]
    WorkspaceMetadataUpdated {},
    #[serde(rename = "workspace.renamed")]
    WorkspaceRenamed {},
    #[serde(rename = "workspace.moved")]
    WorkspaceMoved {},
    #[serde(rename = "workspace.reordered")]
    WorkspaceReordered {},
    #[serde(rename = "workspace.closed")]
    WorkspaceClosed {},
    #[serde(rename = "workspace.focused")]
    WorkspaceFocused {},
    #[serde(rename = "worktree.created")]
    WorktreeCreated {},
    #[serde(rename = "worktree.opened")]
    WorktreeOpened {},
    #[serde(rename = "worktree.removed")]
    WorktreeRemoved {},
    #[serde(rename = "tab.created")]
    TabCreated {},
    #[serde(rename = "tab.closed")]
    TabClosed {},
    #[serde(rename = "tab.focused")]
    TabFocused {},
    #[serde(rename = "tab.renamed")]
    TabRenamed {},
    #[serde(rename = "tab.moved")]
    TabMoved {},
    #[serde(rename = "pane.created")]
    PaneCreated {},
    #[serde(rename = "pane.closed")]
    PaneClosed {},
    #[serde(rename = "pane.updated")]
    PaneUpdated {},
    #[serde(rename = "pane.focused")]
    PaneFocused {},
    #[serde(rename = "pane.moved")]
    PaneMoved {},
    #[serde(rename = "pane.exited")]
    PaneExited {},
    #[serde(rename = "pane.agent_detected")]
    PaneAgentDetected {},
    #[serde(rename = "pane.output_matched")]
    PaneOutputMatched {
        pane_id: String,
        source: ReadSource,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lines: Option<u32>,
        r#match: OutputMatch,
        #[serde(default = "super::common::default_true")]
        strip_ansi: bool,
    },
    #[serde(rename = "pane.agent_status_changed")]
    PaneAgentStatusChanged {
        pane_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_status: Option<AgentStatus>,
    },
    #[serde(rename = "pane.scroll_changed")]
    PaneScrollChanged { pane_id: String },
    #[serde(rename = "layout.updated")]
    LayoutUpdated {},
    #[serde(rename = "workflow.run.started")]
    WorkflowRunStarted {},
    #[serde(rename = "workflow.run.updated")]
    WorkflowRunUpdated {},
    #[serde(rename = "workflow.run.finished")]
    WorkflowRunFinished {},
    #[serde(rename = "workflow.node.created")]
    WorkflowNodeCreated {},
    #[serde(rename = "workflow.node.updated")]
    WorkflowNodeUpdated {},
    #[serde(rename = "workflow.node.output_checkpoint")]
    WorkflowNodeOutputCheckpoint {},
    #[serde(rename = "workflow.growth.limited")]
    WorkflowGrowthLimited {},
    #[serde(rename = "workflow.run.summarized")]
    WorkflowRunSummarized {},
    #[serde(rename = "workflow.interrogation.started")]
    WorkflowInterrogationStarted {},
    #[serde(rename = "workflow.interrogation.ended")]
    WorkflowInterrogationEnded {},
    // Phase 4 additions (`.local/prd/phase4-retarget-plan.md` §5 packet P3).
    #[serde(rename = "workflow.node.watchdog")]
    WorkflowNodeWatchdog {},
    #[serde(rename = "workflow.review.started")]
    WorkflowReviewStarted {},
    #[serde(rename = "workflow.review.ready")]
    WorkflowReviewReady {},
    #[serde(rename = "workflow.review.closed")]
    WorkflowReviewClosed {},
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EventsWaitParams {
    pub match_event: EventMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneWaitForOutputParams {
    pub pane_id: String,
    pub source: ReadSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    pub r#match: OutputMatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default = "super::common::default_true")]
    pub strip_ansi: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputMatch {
    Substring { value: String },
    Regex { value: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventMatch {
    WorkspaceCreated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
    },
    WorkspaceUpdated {
        workspace_id: String,
    },
    WorkspaceClosed {
        workspace_id: String,
    },
    WorkspaceRenamed {
        workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    WorkspaceMoved {
        workspace_id: String,
    },
    WorkspaceFocused {
        workspace_id: String,
    },
    TabCreated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tab_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
    },
    TabClosed {
        tab_id: String,
    },
    TabRenamed {
        tab_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    TabMoved {
        tab_id: String,
    },
    TabFocused {
        tab_id: String,
    },
    PaneCreated {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pane_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace_id: Option<String>,
    },
    PaneClosed {
        pane_id: String,
    },
    PaneFocused {
        pane_id: String,
    },
    PaneMoved {
        pane_id: String,
    },
    PaneOutputChanged {
        pane_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_revision: Option<u64>,
    },
    PaneExited {
        pane_id: String,
    },
    PaneAgentDetected {
        pane_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    PaneAgentStatusChanged {
        pane_id: String,
        agent_status: AgentStatus,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    WorkspaceCreated,
    WorkspaceUpdated,
    WorkspaceMetadataUpdated,
    WorkspaceClosed,
    WorkspaceRenamed,
    WorkspaceMoved,
    WorkspaceReordered,
    WorkspaceFocused,
    WorktreeCreated,
    WorktreeOpened,
    WorktreeRemoved,
    TabCreated,
    TabClosed,
    TabRenamed,
    TabMoved,
    TabFocused,
    PaneCreated,
    PaneClosed,
    PaneUpdated,
    PaneFocused,
    PaneMoved,
    PaneOutputChanged,
    PaneExited,
    PaneAgentDetected,
    PaneAgentStatusChanged,
    LayoutUpdated,
    WorkflowRunStarted,
    WorkflowRunUpdated,
    WorkflowRunFinished,
    WorkflowNodeCreated,
    WorkflowNodeUpdated,
    WorkflowNodeOutputCheckpoint,
    WorkflowGrowthLimited,
    WorkflowRunSummarized,
    WorkflowInterrogationStarted,
    WorkflowInterrogationEnded,
    WorkflowNodeWatchdog,
    WorkflowReviewStarted,
    WorkflowReviewReady,
    WorkflowReviewClosed,
}

impl EventKind {
    pub fn dot_name(self) -> &'static str {
        match self {
            EventKind::WorkspaceCreated => "workspace.created",
            EventKind::WorkspaceUpdated => "workspace.updated",
            EventKind::WorkspaceMetadataUpdated => "workspace.metadata_updated",
            EventKind::WorkspaceClosed => "workspace.closed",
            EventKind::WorkspaceRenamed => "workspace.renamed",
            EventKind::WorkspaceMoved => "workspace.moved",
            EventKind::WorkspaceReordered => "workspace.reordered",
            EventKind::WorkspaceFocused => "workspace.focused",
            EventKind::WorktreeCreated => "worktree.created",
            EventKind::WorktreeOpened => "worktree.opened",
            EventKind::WorktreeRemoved => "worktree.removed",
            EventKind::TabCreated => "tab.created",
            EventKind::TabClosed => "tab.closed",
            EventKind::TabRenamed => "tab.renamed",
            EventKind::TabMoved => "tab.moved",
            EventKind::TabFocused => "tab.focused",
            EventKind::PaneCreated => "pane.created",
            EventKind::PaneClosed => "pane.closed",
            EventKind::PaneUpdated => "pane.updated",
            EventKind::PaneFocused => "pane.focused",
            EventKind::PaneMoved => "pane.moved",
            EventKind::PaneOutputChanged => "pane.output_changed",
            EventKind::PaneExited => "pane.exited",
            EventKind::PaneAgentDetected => "pane.agent_detected",
            EventKind::PaneAgentStatusChanged => "pane.agent_status_changed",
            EventKind::LayoutUpdated => "layout.updated",
            EventKind::WorkflowRunStarted => "workflow.run.started",
            EventKind::WorkflowRunUpdated => "workflow.run.updated",
            EventKind::WorkflowRunFinished => "workflow.run.finished",
            EventKind::WorkflowNodeCreated => "workflow.node.created",
            EventKind::WorkflowNodeUpdated => "workflow.node.updated",
            EventKind::WorkflowNodeOutputCheckpoint => "workflow.node.output_checkpoint",
            EventKind::WorkflowGrowthLimited => "workflow.growth.limited",
            EventKind::WorkflowRunSummarized => "workflow.run.summarized",
            EventKind::WorkflowInterrogationStarted => "workflow.interrogation.started",
            EventKind::WorkflowInterrogationEnded => "workflow.interrogation.ended",
            EventKind::WorkflowNodeWatchdog => "workflow.node.watchdog",
            EventKind::WorkflowReviewStarted => "workflow.review.started",
            EventKind::WorkflowReviewReady => "workflow.review.ready",
            EventKind::WorkflowReviewClosed => "workflow.review.closed",
        }
    }
}

#[cfg(test)]
pub const KNOWN_EVENT_KINDS: &[EventKind] = &[
    EventKind::WorkspaceCreated,
    EventKind::WorkspaceUpdated,
    EventKind::WorkspaceMetadataUpdated,
    EventKind::WorkspaceClosed,
    EventKind::WorkspaceRenamed,
    EventKind::WorkspaceMoved,
    EventKind::WorkspaceReordered,
    EventKind::WorkspaceFocused,
    EventKind::WorktreeCreated,
    EventKind::WorktreeOpened,
    EventKind::WorktreeRemoved,
    EventKind::TabCreated,
    EventKind::TabClosed,
    EventKind::TabRenamed,
    EventKind::TabMoved,
    EventKind::TabFocused,
    EventKind::PaneCreated,
    EventKind::PaneClosed,
    EventKind::PaneUpdated,
    EventKind::PaneFocused,
    EventKind::PaneMoved,
    EventKind::PaneOutputChanged,
    EventKind::PaneExited,
    EventKind::PaneAgentDetected,
    EventKind::PaneAgentStatusChanged,
    EventKind::LayoutUpdated,
    EventKind::WorkflowRunStarted,
    EventKind::WorkflowRunUpdated,
    EventKind::WorkflowRunFinished,
    EventKind::WorkflowNodeCreated,
    EventKind::WorkflowNodeUpdated,
    EventKind::WorkflowNodeOutputCheckpoint,
    EventKind::WorkflowGrowthLimited,
    EventKind::WorkflowRunSummarized,
    EventKind::WorkflowInterrogationStarted,
    EventKind::WorkflowInterrogationEnded,
    EventKind::WorkflowNodeWatchdog,
    EventKind::WorkflowReviewStarted,
    EventKind::WorkflowReviewReady,
    EventKind::WorkflowReviewClosed,
];

pub const PLUGIN_HOOK_EVENT_KINDS: &[EventKind] = &[
    EventKind::WorkspaceCreated,
    EventKind::WorkspaceUpdated,
    EventKind::WorkspaceClosed,
    EventKind::WorkspaceRenamed,
    EventKind::WorkspaceMoved,
    EventKind::WorkspaceReordered,
    EventKind::WorkspaceFocused,
    EventKind::WorktreeCreated,
    EventKind::WorktreeOpened,
    EventKind::WorktreeRemoved,
    EventKind::TabCreated,
    EventKind::TabClosed,
    EventKind::TabRenamed,
    EventKind::TabMoved,
    EventKind::TabFocused,
    EventKind::PaneCreated,
    EventKind::PaneClosed,
    EventKind::PaneFocused,
    EventKind::PaneMoved,
    EventKind::PaneExited,
    EventKind::PaneAgentDetected,
    EventKind::PaneAgentStatusChanged,
];

#[cfg(test)]
pub fn known_event_names() -> Vec<&'static str> {
    KNOWN_EVENT_KINDS
        .iter()
        .copied()
        .map(EventKind::dot_name)
        .collect()
}

/// Event names that manifest `[[events]] on` hooks can reference. This is
/// intentionally narrower than `EventKind` until high-volume output-change hook
/// semantics are implemented.
pub fn plugin_hook_event_names() -> Vec<&'static str> {
    PLUGIN_HOOK_EVENT_KINDS
        .iter()
        .copied()
        .map(EventKind::dot_name)
        .collect()
}

#[cfg(test)]
mod known_event_name_tests {
    use super::*;

    #[test]
    fn known_event_names_stay_in_sync_with_event_kind() {
        let mut from_kind = KNOWN_EVENT_KINDS
            .iter()
            .map(|kind| kind.dot_name())
            .collect::<Vec<_>>();
        from_kind.sort_unstable();
        let mut known = known_event_names();
        known.sort_unstable();
        assert_eq!(
            from_kind, known,
            "known_event_names() out of sync with EventKind"
        );
    }

    #[test]
    fn plugin_hook_event_names_exclude_high_volume_events() {
        let names = plugin_hook_event_names();
        assert!(!names.contains(&"pane.output_changed"));
        assert!(!names.contains(&"layout.updated"));
        assert!(!names.contains(&"workspace.metadata_updated"));
        assert!(!names.contains(&"pane.updated"));
        assert!(names.contains(&"pane.moved"));
    }
}

#[cfg(test)]
mod workflow_event_tests {
    use super::*;
    use crate::api::schema::workflows::{
        WorkflowDemand, WorkflowNodeStatus, WorkflowRunStatus, WorkflowTier,
    };
    use crate::api::schema::{Method, Request};

    fn run() -> WorkflowRunInfo {
        WorkflowRunInfo {
            run_id: "workflow_run:1".into(),
            workflow_id: "workflow:1".into(),
            version_id: "kvdag_version:1".into(),
            tier: WorkflowTier::Auto,
            status: WorkflowRunStatus::Running,
            args: HashMap::new(),
            workspace_id: Some("w_1".into()),
            tab_id: Some("w_1:1".into()),
            started_at_unix_ms: 1,
            ended_at_unix_ms: None,
            total_tokens: 0,
            total_tool_uses: 0,
            nodes_total: 1,
            nodes_done: 0,
            failure: None,
            max_depth: 3,
            max_nodes: 24,
            nodes_live: 1,
            growth_limited: None,
            workflow_name: String::new(),
            context_runs: Vec::new(),
            restore_from_run: None,
            lead_session_id: None,
            team_name: None,
            lead_pane_id: None,
            lead_prompt_version: None,
        }
    }

    fn node() -> WorkflowRunNodeInfo {
        WorkflowRunNodeInfo {
            path: "plan".into(),
            node_key: "plan".into(),
            label: "Plan".into(),
            parent_path: None,
            depth: 0,
            status: WorkflowNodeStatus::Running,
            demand: WorkflowDemand::Standard,
            model: "sonnet".into(),
            effort: "low".into(),
            attempt: 1,
            pane_id: Some("w_1-2".into()),
            terminal_id: Some("term_1".into()),
            agent_session_id: None,
            cwd: None,
            node_dir: None,
            started_at_unix_ms: Some(1),
            ended_at_unix_ms: None,
            total_tokens: 0,
            tool_uses: 0,
            duration_ms: 0,
            evidence: None,
            succession: None,
            blocker: None,
            watchdog_interventions: 0,
            attention: None,
            assignment_reason: String::new(),
            delivery_failure: None,
            growth_limited: None,
            transcript_path: None,
            restored_from: None,
            task_id: None,
            subject: String::new(),
            owner: String::new(),
            emergent: false,
        }
    }

    fn interrogation() -> WorkflowInterrogationInfo {
        WorkflowInterrogationInfo {
            id: "interrogation:1".into(),
            run_id: "workflow_run:1".into(),
            path: "plan".into(),
            source_session_id: "11111111-1111-1111-1111-111111111111".into(),
            forked_session_id: Some("22222222-2222-2222-2222-222222222222".into()),
            pane_id: Some("w_1-3".into()),
            reconstructed: false,
            transcript_path: Some("/home/user/.claude/projects/p/11111111.jsonl".into()),
            cwd: "/repo".into(),
            started_at_unix_ms: 10,
            ended_at_unix_ms: None,
            note: String::new(),
        }
    }

    fn run_summary() -> WorkflowRunSummaryInfo {
        WorkflowRunSummaryInfo {
            run_id: "workflow_run:1".into(),
            workflow_id: "workflow:1".into(),
            workflow_name: "ship-feature".into(),
            version_id: "kvdag_version:1".into(),
            text: "Implemented dark mode.".into(),
            outcome: "succeeded".into(),
            highlights: Vec::new(),
            open_gaps: Vec::new(),
            per_node: Vec::new(),
            token_estimate: 400,
            generated_by_path: Some(".summary".into()),
            created_at_unix_ms: 20,
            run_pruned: false,
        }
    }

    /// `EventEnvelope.event` serialises via `EventKind`'s derived
    /// `rename_all = "snake_case"` (e.g. `"workflow_run_started"`), same as
    /// every existing `EventKind`; `dot_name()` is the separate mapping used
    /// by `known_event_names()`/`plugin_hook_event_names()`, not the wire
    /// field. This test exercises both.
    #[test]
    fn workflow_events_round_trip_and_dot_names_match() {
        for (event, dot_name) in [
            (
                EventEnvelope {
                    event: EventKind::WorkflowRunStarted,
                    data: EventData::WorkflowRunStarted { run: run() },
                },
                "workflow.run.started",
            ),
            (
                EventEnvelope {
                    event: EventKind::WorkflowRunUpdated,
                    data: EventData::WorkflowRunUpdated { run: run() },
                },
                "workflow.run.updated",
            ),
            (
                EventEnvelope {
                    event: EventKind::WorkflowRunFinished,
                    data: EventData::WorkflowRunFinished { run: run() },
                },
                "workflow.run.finished",
            ),
            (
                EventEnvelope {
                    event: EventKind::WorkflowNodeCreated,
                    data: EventData::WorkflowNodeCreated {
                        run_id: "workflow_run:1".into(),
                        node: node(),
                    },
                },
                "workflow.node.created",
            ),
            (
                EventEnvelope {
                    event: EventKind::WorkflowNodeUpdated,
                    data: EventData::WorkflowNodeUpdated {
                        run_id: "workflow_run:1".into(),
                        node: node(),
                    },
                },
                "workflow.node.updated",
            ),
            (
                EventEnvelope {
                    event: EventKind::WorkflowNodeOutputCheckpoint,
                    data: EventData::WorkflowNodeOutputCheckpoint {
                        run_id: "workflow_run:1".into(),
                        path: "plan".into(),
                        seq: 1,
                        summary: "produced a plan".into(),
                    },
                },
                "workflow.node.output_checkpoint",
            ),
            (
                EventEnvelope {
                    event: EventKind::WorkflowGrowthLimited,
                    data: EventData::WorkflowGrowthLimited {
                        run_id: "workflow_run:1".into(),
                        path: "fanout".into(),
                        template: "worker".into(),
                        limit: WorkflowGrowthLimitKind::MaxNodes,
                        limit_value: 12,
                        requested: 4,
                        accepted: 2,
                        message: "max_nodes 12 reached; 2 of 4 requested nodes created".into(),
                    },
                },
                "workflow.growth.limited",
            ),
            (
                EventEnvelope {
                    event: EventKind::WorkflowRunSummarized,
                    data: EventData::WorkflowRunSummarized {
                        run_id: "workflow_run:1".into(),
                        summary: run_summary(),
                    },
                },
                "workflow.run.summarized",
            ),
            (
                EventEnvelope {
                    event: EventKind::WorkflowInterrogationStarted,
                    data: EventData::WorkflowInterrogationStarted {
                        interrogation: interrogation(),
                    },
                },
                "workflow.interrogation.started",
            ),
            (
                EventEnvelope {
                    event: EventKind::WorkflowInterrogationEnded,
                    data: EventData::WorkflowInterrogationEnded {
                        interrogation: interrogation(),
                    },
                },
                "workflow.interrogation.ended",
            ),
        ] {
            assert_eq!(event.event.dot_name(), dot_name);
            let json = serde_json::to_value(&event).unwrap();
            let restored: EventEnvelope = serde_json::from_value(json).unwrap();
            assert_eq!(restored, event);
        }
    }

    #[test]
    fn workflow_event_subscriptions_use_dot_names() {
        let request = Request {
            id: "sub_workflow".into(),
            method: Method::EventsSubscribe(EventsSubscribeParams {
                subscriptions: vec![
                    Subscription::WorkflowRunStarted {},
                    Subscription::WorkflowRunUpdated {},
                    Subscription::WorkflowRunFinished {},
                    Subscription::WorkflowNodeCreated {},
                    Subscription::WorkflowNodeUpdated {},
                    Subscription::WorkflowNodeOutputCheckpoint {},
                    Subscription::WorkflowGrowthLimited {},
                    Subscription::WorkflowRunSummarized {},
                    Subscription::WorkflowInterrogationStarted {},
                    Subscription::WorkflowInterrogationEnded {},
                ],
            }),
        };
        let json = serde_json::to_string(&request).unwrap();
        for dot_name in [
            "workflow.run.started",
            "workflow.run.updated",
            "workflow.run.finished",
            "workflow.node.created",
            "workflow.node.updated",
            "workflow.node.output_checkpoint",
            "workflow.growth.limited",
            "workflow.run.summarized",
            "workflow.interrogation.started",
            "workflow.interrogation.ended",
        ] {
            assert!(
                json.contains(&format!("\"type\":\"{dot_name}\"")),
                "missing subscription type {dot_name} in {json}"
            );
        }
        let restored: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, request);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EventEnvelope {
    pub event: EventKind,
    pub data: EventData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum SubscriptionEventKind {
    #[serde(rename = "pane.output_matched")]
    PaneOutputMatched,
    #[serde(rename = "pane.agent_status_changed")]
    PaneAgentStatusChanged,
    #[serde(rename = "pane.scroll_changed")]
    ScrollChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SubscriptionEventEnvelope {
    pub event: SubscriptionEventKind,
    pub data: SubscriptionEventData,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum SubscriptionEventData {
    PaneOutputMatched(PaneOutputMatchedEvent),
    PaneAgentStatusChanged(PaneAgentStatusChangedEvent),
    ScrollChanged(PaneScrollChangedEvent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneOutputMatchedEvent {
    pub pane_id: String,
    pub matched_line: String,
    pub read: PaneReadResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneAgentStatusChangedEvent {
    pub pane_id: String,
    pub workspace_id: String,
    pub agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_agent: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub state_labels: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneScrollChangedEvent {
    pub pane_id: String,
    pub workspace_id: String,
    pub scroll: PaneScrollInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventData {
    WorkspaceCreated {
        workspace: WorkspaceInfo,
    },
    WorkspaceUpdated {
        workspace: WorkspaceInfo,
    },
    WorkspaceMetadataUpdated {
        workspace: WorkspaceInfo,
    },
    WorkspaceClosed {
        workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<WorkspaceInfo>,
    },
    WorkspaceRenamed {
        workspace_id: String,
        label: String,
    },
    WorkspaceMoved {
        workspace_id: String,
        insert_index: usize,
        workspaces: Vec<WorkspaceInfo>,
    },
    WorkspaceReordered {
        workspace_ids: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before_workspace_id: Option<String>,
        workspaces: Vec<WorkspaceInfo>,
    },
    WorkspaceFocused {
        workspace_id: String,
    },
    WorktreeCreated {
        workspace: WorkspaceInfo,
        worktree: WorktreeInfo,
    },
    WorktreeOpened {
        workspace: WorkspaceInfo,
        worktree: WorktreeInfo,
        already_open: bool,
    },
    WorktreeRemoved {
        workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<WorkspaceInfo>,
        worktree: WorktreeInfo,
        forced: bool,
    },
    TabCreated {
        tab: TabInfo,
    },
    TabClosed {
        tab_id: String,
        workspace_id: String,
    },
    TabRenamed {
        tab_id: String,
        workspace_id: String,
        label: String,
    },
    TabMoved {
        tab_id: String,
        workspace_id: String,
        insert_index: usize,
        tabs: Vec<TabInfo>,
    },
    TabFocused {
        tab_id: String,
        workspace_id: String,
    },
    PaneCreated {
        pane: PaneInfo,
    },
    PaneClosed {
        pane_id: String,
        workspace_id: String,
    },
    PaneUpdated {
        pane: PaneInfo,
    },
    PaneFocused {
        pane_id: String,
        workspace_id: String,
    },
    PaneMoved {
        previous_pane_id: String,
        previous_workspace_id: String,
        previous_tab_id: String,
        pane: Box<PaneInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_workspace: Option<WorkspaceInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        created_tab: Option<TabInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        closed_workspace_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        closed_tab_id: Option<String>,
    },
    PaneOutputChanged {
        pane_id: String,
        workspace_id: String,
        revision: u64,
    },
    PaneExited {
        pane_id: String,
        workspace_id: String,
    },
    PaneAgentDetected {
        pane_id: String,
        workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        released: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_status: Option<AgentStatus>,
    },
    PaneAgentStatusChanged {
        pane_id: String,
        workspace_id: String,
        agent_status: AgentStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_agent: Option<String>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        state_labels: HashMap<String, String>,
    },
    LayoutUpdated {
        layout: super::panes::PaneLayoutSnapshot,
    },
    WorkflowRunStarted {
        run: WorkflowRunInfo,
    },
    WorkflowRunUpdated {
        run: WorkflowRunInfo,
    },
    WorkflowRunFinished {
        run: WorkflowRunInfo,
    },
    WorkflowNodeCreated {
        run_id: String,
        node: WorkflowRunNodeInfo,
    },
    WorkflowNodeUpdated {
        run_id: String,
        node: WorkflowRunNodeInfo,
    },
    WorkflowNodeOutputCheckpoint {
        run_id: String,
        path: String,
        seq: u64,
        summary: String,
    },
    /// A growth guardrail refused or truncated a proposal.
    ///
    /// The only new event kind in Phase 2: an accepted proposal already emits
    /// `workflow.node.created`, whose `WorkflowRunNodeInfo` carries
    /// `parent_path` and `depth`, so a second "spawned" event would give
    /// clients two events for one fact. What no client can derive is the node
    /// that was asked for and never created.
    WorkflowGrowthLimited {
        run_id: String,
        /// The proposing node's instance path.
        path: String,
        template: String,
        limit: WorkflowGrowthLimitKind,
        limit_value: u32,
        requested: u32,
        accepted: u32,
        message: String,
    },
    /// The epilogue's summary was accepted and enqueued
    /// (`07-phase3-plan.md` §4 D1). There is no second `workflow.run.finished`
    /// and no `workflow.run.updated` — the run's status was already final.
    WorkflowRunSummarized {
        run_id: String,
        summary: WorkflowRunSummaryInfo,
    },
    WorkflowInterrogationStarted {
        interrogation: WorkflowInterrogationInfo,
    },
    WorkflowInterrogationEnded {
        interrogation: WorkflowInterrogationInfo,
    },
    /// One watchdog rung fired against a node
    /// (`.local/prd/phase4-retarget-plan.md` §3.4). Carries the node as of
    /// the intervention, so a subscriber sees `watchdog_interventions` and
    /// `attention` without a second `workflow.run.get` round trip.
    WorkflowNodeWatchdog {
        run_id: String,
        node: WorkflowRunNodeInfo,
    },
    /// A review cycle began (`workflow.review.start`).
    WorkflowReviewStarted {
        run_id: String,
        review: WorkflowReviewInfo,
    },
    /// A review cycle finished synthesis and is waiting on the human's
    /// per-finding accept.
    WorkflowReviewReady {
        run_id: String,
        review: WorkflowReviewInfo,
    },
    /// A review cycle reached a terminal status: `applied`, `declined`, or
    /// `failed`.
    WorkflowReviewClosed {
        run_id: String,
        review: WorkflowReviewInfo,
    },
}
