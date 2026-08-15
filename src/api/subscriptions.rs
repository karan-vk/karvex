use regex::Regex;
use tracing::debug;

use crate::api::schema::{
    ErrorBody, ErrorResponse, Method, PaneAgentStatusChangedEvent, PaneOutputMatchedEvent,
    PaneScrollChangedEvent, PaneScrollInfo, Request, Subscription, SubscriptionEventData,
    SubscriptionEventEnvelope, SubscriptionEventKind,
};
use crate::api::server::{dispatch_to_app_with_timeout, APP_RESPONSE_TIMEOUT};
use crate::api::{ApiRequestSender, EventHub};

pub(super) fn output_match_read_source(
    source: &crate::api::schema::ReadSource,
) -> crate::api::schema::ReadSource {
    match source {
        crate::api::schema::ReadSource::Recent => crate::api::schema::ReadSource::RecentUnwrapped,
        other => *other,
    }
}

pub(super) fn match_output(
    text: &str,
    matcher: &crate::api::schema::OutputMatch,
    regex: Option<&Regex>,
) -> Option<String> {
    match matcher {
        crate::api::schema::OutputMatch::Substring { value } => text
            .lines()
            .find(|line| line.contains(value))
            .map(|line| line.to_string()),
        crate::api::schema::OutputMatch::Regex { .. } => regex.and_then(|re| {
            text.lines()
                .find(|line| re.is_match(line))
                .map(|line| line.to_string())
        }),
    }
}

pub(super) struct ActiveOutputMatchedSubscription {
    pane_id: String,
    source: crate::api::schema::ReadSource,
    lines: Option<u32>,
    matcher: crate::api::schema::OutputMatch,
    regex: Option<Regex>,
    strip_ansi: bool,
    currently_matching: bool,
    request_prefix: String,
}

pub(super) struct ActiveAgentStatusChangedSubscription {
    pane_id: String,
    status_filter: Option<crate::api::schema::AgentStatus>,
    last_status: Option<crate::api::schema::AgentStatus>,
    last_presentation: Option<PanePresentationSnapshot>,
    last_sequence: u64,
    initial_event: Option<PaneAgentStatusChangedEvent>,
    request_prefix: String,
}

pub(super) struct ActiveScrollChangedSubscription {
    pane_id: String,
    last_scroll: Option<PaneScrollInfo>,
    request_prefix: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PanePresentationSnapshot {
    title: Option<String>,
    display_agent: Option<String>,
    state_labels: std::collections::HashMap<String, String>,
}

impl PanePresentationSnapshot {
    fn from(pane: &crate::api::schema::PaneInfo) -> Self {
        Self {
            title: pane.title.clone(),
            display_agent: pane.display_agent.clone(),
            state_labels: pane.state_labels.clone(),
        }
    }

    fn from_event(
        title: &Option<String>,
        display_agent: &Option<String>,
        state_labels: &std::collections::HashMap<String, String>,
    ) -> Self {
        Self {
            title: title.clone(),
            display_agent: display_agent.clone(),
            state_labels: state_labels.clone(),
        }
    }
}

pub(super) struct ActiveEventSubscription {
    event_kind: crate::api::schema::EventKind,
    last_sequence: u64,
}

pub(super) enum ActiveSubscription {
    Event(ActiveEventSubscription),
    OutputMatched(ActiveOutputMatchedSubscription),
    AgentStatusChanged(Box<ActiveAgentStatusChangedSubscription>),
    ScrollChanged(ActiveScrollChangedSubscription),
}

/// One cursor over the hub's global sequence for every event-log-backed
/// subscription on a connection, so cross-type causal order survives a backlog.
///
/// Before this existed each `Subscription::*` that reads the event log carried
/// its own private cursor and the stream loop yielded **at most one event per
/// subscription per poll pass**. A run whose 11 `workflow.node.created` events
/// and single `workflow.run.finished` were all queued therefore delivered
/// `run.finished` in the first pass and the node events over the next eleven —
/// a client cache saw a finished run before its children existed. Per-type FIFO
/// held; cross-type causality did not.
///
/// A single cursor walked in hub-sequence order restores it: the hub
/// ([`EventHub`]) is already globally sequenced, so draining it in order and
/// fanning each event out to the kinds that asked for it is the whole fix.
pub(super) struct EventStreamCursor {
    /// One entry per subscribed `Event` subscription, in the order the client
    /// declared them, so a client that asked for the same kind twice still gets
    /// it twice.
    kinds: Vec<crate::api::schema::EventKind>,
    /// The highest hub sequence already delivered on this connection. Starts at
    /// `0`, matching the per-subscription cursors it replaces, so a new
    /// subscriber still replays whatever the ring retains.
    last_sequence: u64,
}

impl EventStreamCursor {
    pub(super) fn from_subscriptions(subscriptions: &[ActiveSubscription]) -> Option<Self> {
        let kinds: Vec<_> = subscriptions
            .iter()
            .filter_map(ActiveSubscription::event_kind)
            .collect();
        (!kinds.is_empty()).then_some(Self {
            kinds,
            last_sequence: 0,
        })
    }

    /// Every subscribed event newer than the cursor, in hub-sequence order.
    ///
    /// One event is emitted per *matching subscription*, not per matching kind,
    /// so a client that asked for the same type twice still gets it twice. The
    /// whole backlog is returned in one call: the hub retains at most
    /// `EventHub::MAX_EVENTS`, so this is bounded without a second queue.
    pub(super) fn drain(&mut self, event_hub: &EventHub) -> Vec<serde_json::Value> {
        let pending = event_hub.events_after(self.last_sequence);
        if pending.is_empty() {
            return Vec::new();
        }
        if self.last_sequence > 0 {
            if let Some((oldest, _)) = pending.first() {
                if *oldest > self.last_sequence + 1 {
                    // The ring dropped events this connection had not read yet.
                    // Not a wire-visible gap marker by design — a subscriber
                    // cannot act on it — but the server should be able to say
                    // so when someone asks why a cache diverged.
                    debug!(
                        cursor = self.last_sequence,
                        oldest_retained = *oldest,
                        "event subscription fell behind the retained ring"
                    );
                }
            }
        }

        let mut delivered = Vec::new();
        for (sequence, event) in pending {
            self.last_sequence = sequence;
            let copies = self
                .kinds
                .iter()
                .filter(|kind| **kind == event.event)
                .count();
            if copies == 0 {
                continue;
            }
            let Ok(value) = serde_json::to_value(&event) else {
                continue;
            };
            for _ in 1..copies {
                delivered.push(value.clone());
            }
            delivered.push(value);
        }
        delivered
    }
}

impl ActiveSubscription {
    pub(super) fn new(
        subscription: Subscription,
        request_id: &str,
        index: usize,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Result<Self, ErrorResponse> {
        match subscription {
            Subscription::WorkspaceCreated {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkspaceCreated,
                last_sequence: 0,
            })),
            Subscription::WorkspaceUpdated {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkspaceUpdated,
                last_sequence: 0,
            })),
            Subscription::WorkspaceMetadataUpdated {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkspaceMetadataUpdated,
                last_sequence: 0,
            })),
            Subscription::WorkspaceRenamed {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkspaceRenamed,
                last_sequence: 0,
            })),
            Subscription::WorkspaceMoved {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkspaceMoved,
                last_sequence: 0,
            })),
            Subscription::WorkspaceReordered {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkspaceReordered,
                last_sequence: 0,
            })),
            Subscription::WorkspaceClosed {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkspaceClosed,
                last_sequence: 0,
            })),
            Subscription::WorkspaceFocused {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkspaceFocused,
                last_sequence: 0,
            })),
            Subscription::WorktreeCreated {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorktreeCreated,
                last_sequence: 0,
            })),
            Subscription::WorktreeOpened {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorktreeOpened,
                last_sequence: 0,
            })),
            Subscription::WorktreeRemoved {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorktreeRemoved,
                last_sequence: 0,
            })),
            Subscription::TabCreated {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::TabCreated,
                last_sequence: 0,
            })),
            Subscription::TabClosed {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::TabClosed,
                last_sequence: 0,
            })),
            Subscription::TabFocused {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::TabFocused,
                last_sequence: 0,
            })),
            Subscription::TabRenamed {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::TabRenamed,
                last_sequence: 0,
            })),
            Subscription::TabMoved {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::TabMoved,
                last_sequence: 0,
            })),
            Subscription::PaneCreated {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::PaneCreated,
                last_sequence: 0,
            })),
            Subscription::PaneClosed {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::PaneClosed,
                last_sequence: 0,
            })),
            Subscription::PaneUpdated {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::PaneUpdated,
                last_sequence: 0,
            })),
            Subscription::PaneFocused {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::PaneFocused,
                last_sequence: 0,
            })),
            Subscription::PaneMoved {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::PaneMoved,
                last_sequence: 0,
            })),
            Subscription::PaneExited {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::PaneExited,
                last_sequence: 0,
            })),
            Subscription::PaneAgentDetected {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::PaneAgentDetected,
                last_sequence: 0,
            })),
            Subscription::LayoutUpdated {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::LayoutUpdated,
                last_sequence: 0,
            })),
            Subscription::WorkflowRunStarted {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkflowRunStarted,
                last_sequence: 0,
            })),
            Subscription::WorkflowRunUpdated {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkflowRunUpdated,
                last_sequence: 0,
            })),
            Subscription::WorkflowRunFinished {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkflowRunFinished,
                last_sequence: 0,
            })),
            Subscription::WorkflowNodeCreated {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkflowNodeCreated,
                last_sequence: 0,
            })),
            Subscription::WorkflowNodeUpdated {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkflowNodeUpdated,
                last_sequence: 0,
            })),
            Subscription::WorkflowNodeOutputCheckpoint {} => {
                Ok(Self::Event(ActiveEventSubscription {
                    event_kind: crate::api::schema::EventKind::WorkflowNodeOutputCheckpoint,
                    last_sequence: 0,
                }))
            }
            Subscription::WorkflowGrowthLimited {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkflowGrowthLimited,
                last_sequence: 0,
            })),
            Subscription::WorkflowRunSummarized {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkflowRunSummarized,
                last_sequence: 0,
            })),
            Subscription::WorkflowInterrogationStarted {} => {
                Ok(Self::Event(ActiveEventSubscription {
                    event_kind: crate::api::schema::EventKind::WorkflowInterrogationStarted,
                    last_sequence: 0,
                }))
            }
            Subscription::WorkflowInterrogationEnded {} => {
                Ok(Self::Event(ActiveEventSubscription {
                    event_kind: crate::api::schema::EventKind::WorkflowInterrogationEnded,
                    last_sequence: 0,
                }))
            }
            Subscription::WorkflowNodeWatchdog {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkflowNodeWatchdog,
                last_sequence: 0,
            })),
            Subscription::WorkflowReviewStarted {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkflowReviewStarted,
                last_sequence: 0,
            })),
            Subscription::WorkflowReviewReady {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkflowReviewReady,
                last_sequence: 0,
            })),
            Subscription::WorkflowReviewClosed {} => Ok(Self::Event(ActiveEventSubscription {
                event_kind: crate::api::schema::EventKind::WorkflowReviewClosed,
                last_sequence: 0,
            })),
            Subscription::PaneOutputMatched {
                pane_id,
                source,
                lines,
                r#match,
                strip_ansi,
            } => {
                let regex = match &r#match {
                    crate::api::schema::OutputMatch::Regex { value } => match Regex::new(value) {
                        Ok(regex) => Some(regex),
                        Err(err) => {
                            return Err(ErrorResponse {
                                id: request_id.to_string(),
                                error: ErrorBody {
                                    code: "invalid_regex".into(),
                                    message: err.to_string(),
                                },
                            });
                        }
                    },
                    crate::api::schema::OutputMatch::Substring { .. } => None,
                };

                let probe = pane_read(
                    format!("{request_id}:sub:{index}:probe"),
                    &pane_id,
                    source,
                    lines,
                    strip_ansi,
                    api_tx,
                );
                probe?;

                Ok(Self::OutputMatched(ActiveOutputMatchedSubscription {
                    pane_id,
                    source,
                    lines,
                    matcher: r#match,
                    regex,
                    strip_ansi,
                    currently_matching: false,
                    request_prefix: format!("{request_id}:sub:{index}"),
                }))
            }
            Subscription::PaneAgentStatusChanged {
                pane_id,
                agent_status,
            } => {
                let last_sequence = event_hub.current_sequence();
                let probe = pane_get(format!("{request_id}:sub:{index}:probe"), &pane_id, api_tx)?;
                let last_status = probe.agent_status;
                let last_presentation = PanePresentationSnapshot::from(&probe);
                let initial_event = agent_status
                    .is_some_and(|wanted| wanted == probe.agent_status)
                    .then_some(PaneAgentStatusChangedEvent {
                        pane_id: probe.pane_id.clone(),
                        workspace_id: probe.workspace_id,
                        agent_status: probe.agent_status,
                        agent: probe.agent,
                        title: probe.title,
                        display_agent: probe.display_agent,
                        state_labels: probe.state_labels,
                    });

                Ok(Self::AgentStatusChanged(Box::new(
                    ActiveAgentStatusChangedSubscription {
                        pane_id: probe.pane_id,
                        status_filter: agent_status,
                        last_status: Some(last_status),
                        last_presentation: Some(last_presentation),
                        last_sequence,
                        initial_event,
                        request_prefix: format!("{request_id}:sub:{index}"),
                    },
                )))
            }
            Subscription::PaneScrollChanged { pane_id } => {
                let probe = pane_get(format!("{request_id}:sub:{index}:probe"), &pane_id, api_tx)?;

                Ok(Self::ScrollChanged(ActiveScrollChangedSubscription {
                    pane_id: probe.pane_id,
                    last_scroll: probe.scroll,
                    request_prefix: format!("{request_id}:sub:{index}"),
                }))
            }
        }
    }

    /// The event-log kind this subscription reads, or `None` for the three
    /// subscriptions that poll the app instead of the hub. The stream loop uses
    /// it to split the two families apart: the event-log ones are served by one
    /// shared [`EventStreamCursor`], the rest keep polling per pass.
    pub(super) fn event_kind(&self) -> Option<crate::api::schema::EventKind> {
        match self {
            Self::Event(subscription) => Some(subscription.event_kind),
            Self::OutputMatched(_) | Self::AgentStatusChanged(_) | Self::ScrollChanged(_) => None,
        }
    }

    pub(super) fn poll(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Option<serde_json::Value> {
        match self {
            Self::Event(subscription) => subscription.poll(event_hub),
            Self::OutputMatched(subscription) => {
                serde_json::to_value(subscription.poll(api_tx)?).ok()
            }
            Self::AgentStatusChanged(subscription) => {
                serde_json::to_value(subscription.poll(api_tx, event_hub)?).ok()
            }
            Self::ScrollChanged(subscription) => {
                serde_json::to_value(subscription.poll(api_tx)?).ok()
            }
        }
    }

    pub(super) fn poll_for_wait(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Result<Option<serde_json::Value>, ErrorResponse> {
        match self {
            Self::AgentStatusChanged(subscription) => Ok(subscription
                .poll_result(api_tx, event_hub)?
                .and_then(|event| serde_json::to_value(event).ok())),
            _ => Ok(self.poll(api_tx, event_hub)),
        }
    }
}

impl ActiveEventSubscription {
    fn poll(&mut self, event_hub: &EventHub) -> Option<serde_json::Value> {
        for (sequence, event) in event_hub.events_after(self.last_sequence) {
            self.last_sequence = sequence;
            if event.event == self.event_kind {
                return serde_json::to_value(event).ok();
            }
        }
        None
    }
}

impl ActiveOutputMatchedSubscription {
    fn poll(&mut self, api_tx: &ApiRequestSender) -> Option<SubscriptionEventEnvelope> {
        let read = pane_read(
            format!("{}:read", self.request_prefix),
            &self.pane_id,
            output_match_read_source(&self.source),
            self.lines,
            self.strip_ansi,
            api_tx,
        )
        .ok()?;

        let matched_line = match_output(&read.text, &self.matcher, self.regex.as_ref());
        match matched_line {
            Some(matched_line) => {
                if self.currently_matching {
                    return None;
                }
                self.currently_matching = true;
                Some(SubscriptionEventEnvelope {
                    event: SubscriptionEventKind::PaneOutputMatched,
                    data: SubscriptionEventData::PaneOutputMatched(PaneOutputMatchedEvent {
                        pane_id: read.pane_id.clone(),
                        matched_line,
                        read,
                    }),
                })
            }
            None => {
                self.currently_matching = false;
                None
            }
        }
    }
}

impl ActiveAgentStatusChangedSubscription {
    fn poll(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Option<SubscriptionEventEnvelope> {
        self.poll_result(api_tx, event_hub).ok().flatten()
    }

    fn poll_result(
        &mut self,
        api_tx: &ApiRequestSender,
        event_hub: &EventHub,
    ) -> Result<Option<SubscriptionEventEnvelope>, ErrorResponse> {
        let mut saw_status_event = false;
        for (sequence, event) in event_hub.events_after(self.last_sequence) {
            self.last_sequence = sequence;
            let crate::api::schema::EventData::PaneAgentStatusChanged {
                pane_id,
                workspace_id,
                agent_status,
                agent,
                title,
                display_agent,
                state_labels,
            } = event.data
            else {
                continue;
            };
            if event.event != crate::api::schema::EventKind::PaneAgentStatusChanged {
                continue;
            }
            if pane_id != self.pane_id {
                continue;
            }
            saw_status_event = true;

            let current_presentation =
                PanePresentationSnapshot::from_event(&title, &display_agent, &state_labels);
            self.last_status = Some(agent_status);
            self.last_presentation = Some(current_presentation);
            if self
                .status_filter
                .is_some_and(|wanted| wanted != agent_status)
            {
                continue;
            }

            self.initial_event = None;
            return Ok(Some(SubscriptionEventEnvelope {
                event: SubscriptionEventKind::PaneAgentStatusChanged,
                data: SubscriptionEventData::PaneAgentStatusChanged(PaneAgentStatusChangedEvent {
                    pane_id,
                    workspace_id,
                    agent_status,
                    agent,
                    title,
                    display_agent,
                    state_labels,
                }),
            }));
        }

        if saw_status_event {
            self.initial_event = None;
        } else if event_hub.current_sequence() != self.last_sequence {
            return Ok(None);
        } else if let Some(event) = self.initial_event.take() {
            return Ok(Some(SubscriptionEventEnvelope {
                event: SubscriptionEventKind::PaneAgentStatusChanged,
                data: SubscriptionEventData::PaneAgentStatusChanged(event),
            }));
        }

        let before_snapshot_sequence = self.last_sequence;
        let pane = pane_get(
            format!("{}:pane", self.request_prefix),
            &self.pane_id,
            api_tx,
        );
        let after_snapshot_sequence = event_hub.current_sequence();
        if after_snapshot_sequence != before_snapshot_sequence {
            return Ok(None);
        }
        let pane = pane?;

        let event = self.event_from_snapshot(pane);
        if event.is_some() {
            self.last_sequence = after_snapshot_sequence;
        }
        Ok(event)
    }

    fn event_from_snapshot(
        &mut self,
        pane: crate::api::schema::PaneInfo,
    ) -> Option<SubscriptionEventEnvelope> {
        let current_status = pane.agent_status;
        let current_presentation = PanePresentationSnapshot::from(&pane);
        let previous_status = self.last_status.replace(current_status);
        let previous_presentation = self.last_presentation.replace(current_presentation.clone());
        let presentation_changed = previous_presentation
            .as_ref()
            .is_some_and(|previous| previous != &current_presentation);
        let status_changed = previous_status.is_some_and(|previous| previous != current_status);
        if !(status_changed || presentation_changed) {
            return None;
        }
        if self
            .status_filter
            .is_some_and(|wanted| wanted != current_status)
        {
            return None;
        }

        Some(SubscriptionEventEnvelope {
            event: SubscriptionEventKind::PaneAgentStatusChanged,
            data: SubscriptionEventData::PaneAgentStatusChanged(PaneAgentStatusChangedEvent {
                pane_id: pane.pane_id,
                workspace_id: pane.workspace_id,
                agent_status: current_status,
                agent: pane.agent,
                title: pane.title,
                display_agent: pane.display_agent,
                state_labels: pane.state_labels,
            }),
        })
    }
}

impl ActiveScrollChangedSubscription {
    fn poll(&mut self, api_tx: &ApiRequestSender) -> Option<SubscriptionEventEnvelope> {
        let pane = pane_get(
            format!("{}:pane", self.request_prefix),
            &self.pane_id,
            api_tx,
        )
        .ok()?;
        self.event_from_snapshot(pane)
    }

    fn event_from_snapshot(
        &mut self,
        pane: crate::api::schema::PaneInfo,
    ) -> Option<SubscriptionEventEnvelope> {
        let scroll = pane.scroll;
        if self.last_scroll == scroll {
            return None;
        }
        self.last_scroll = scroll;
        let scroll = scroll?;

        Some(SubscriptionEventEnvelope {
            event: SubscriptionEventKind::ScrollChanged,
            data: SubscriptionEventData::ScrollChanged(PaneScrollChangedEvent {
                pane_id: pane.pane_id,
                workspace_id: pane.workspace_id,
                scroll,
            }),
        })
    }
}

fn pane_read(
    request_id: String,
    pane_id: &str,
    source: crate::api::schema::ReadSource,
    lines: Option<u32>,
    strip_ansi: bool,
    api_tx: &ApiRequestSender,
) -> Result<crate::api::schema::PaneReadResult, ErrorResponse> {
    let response = dispatch_to_app_with_timeout(
        Request {
            id: request_id.clone(),
            method: Method::PaneRead(crate::api::schema::PaneReadParams {
                pane_id: pane_id.to_string(),
                source,
                lines,
                format: crate::api::schema::ReadFormat::Text,
                strip_ansi,
                intent: crate::api::schema::ReadIntent::Passive,
            }),
        },
        api_tx,
        Some(APP_RESPONSE_TIMEOUT),
    );
    let value: serde_json::Value = serde_json::from_str(&response).map_err(|_| ErrorResponse {
        id: request_id.clone(),
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode pane read response".into(),
        },
    })?;
    if value.get("error").is_some() {
        return serde_json::from_value(value).map_err(|_| ErrorResponse {
            id: request_id,
            error: ErrorBody {
                code: "internal_error".into(),
                message: "failed to decode pane read error".into(),
            },
        });
    }
    serde_json::from_value(value["result"]["read"].clone()).map_err(|_| ErrorResponse {
        id: request_id,
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode pane read result".into(),
        },
    })
}

fn pane_get(
    request_id: String,
    pane_id: &str,
    api_tx: &ApiRequestSender,
) -> Result<crate::api::schema::PaneInfo, ErrorResponse> {
    let response = dispatch_to_app_with_timeout(
        Request {
            id: request_id.clone(),
            method: Method::PaneGet(crate::api::schema::PaneTarget {
                pane_id: pane_id.to_string(),
            }),
        },
        api_tx,
        Some(APP_RESPONSE_TIMEOUT),
    );
    let value: serde_json::Value = serde_json::from_str(&response).map_err(|_| ErrorResponse {
        id: request_id.clone(),
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode pane get response".into(),
        },
    })?;
    if value.get("error").is_some() {
        let response =
            serde_json::from_value::<ErrorResponse>(value).map_err(|_| ErrorResponse {
                id: request_id,
                error: ErrorBody {
                    code: "internal_error".into(),
                    message: "failed to decode pane get error".into(),
                },
            })?;
        return Err(response);
    }
    serde_json::from_value(value["result"]["pane"].clone()).map_err(|_| ErrorResponse {
        id: request_id,
        error: ErrorBody {
            code: "internal_error".into(),
            message: "failed to decode pane get result".into(),
        },
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::api::schema::{AgentStatus, EventData, EventEnvelope, EventKind, PaneInfo};

    fn presentation_event(title: Option<&str>) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::PaneAgentStatusChanged,
            data: EventData::PaneAgentStatusChanged {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: title.map(str::to_string),
                display_agent: None,
                state_labels: HashMap::new(),
            },
        }
    }

    fn workflow_node(path: &str) -> crate::api::schema::WorkflowRunNodeInfo {
        crate::api::schema::WorkflowRunNodeInfo {
            path: path.into(),
            node_key: "worker".into(),
            label: path.into(),
            parent_path: None,
            depth: 0,
            status: crate::api::schema::WorkflowNodeStatus::Running,
            demand: crate::api::schema::WorkflowDemand::Standard,
            model: "sonnet".into(),
            effort: "low".into(),
            attempt: 1,
            pane_id: None,
            terminal_id: None,
            agent_session_id: None,
            cwd: None,
            node_dir: None,
            started_at_unix_ms: None,
            ended_at_unix_ms: None,
            total_tokens: 0,
            tool_uses: 0,
            duration_ms: 0,
            evidence: None,
            succession: None,
            blocker: None,
            watchdog_interventions: 0,
            assignment_reason: String::new(),
            delivery_failure: None,
            growth_limited: None,
            transcript_path: None,
            restored_from: None,
            task_id: None,
            subject: String::new(),
            owner: String::new(),
            emergent: false,
            attention: None,
        }
    }

    fn node_created_event(path: &str) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::WorkflowNodeCreated,
            data: EventData::WorkflowNodeCreated {
                run_id: "workflow_run:1".into(),
                node: workflow_node(path),
            },
        }
    }

    fn run_finished_event() -> EventEnvelope {
        EventEnvelope {
            event: EventKind::WorkflowRunFinished,
            data: EventData::WorkflowRunFinished {
                run: crate::api::schema::WorkflowRunInfo {
                    run_id: "workflow_run:1".into(),
                    workflow_id: "workflow:1".into(),
                    version_id: "kvdag_version:1".into(),
                    tier: crate::api::schema::WorkflowTier::Auto,
                    status: crate::api::schema::WorkflowRunStatus::Succeeded,
                    args: HashMap::new(),
                    workspace_id: None,
                    tab_id: None,
                    started_at_unix_ms: 1,
                    ended_at_unix_ms: Some(2),
                    total_tokens: 0,
                    total_tool_uses: 0,
                    nodes_total: 2,
                    nodes_done: 2,
                    failure: None,
                    max_depth: 3,
                    max_nodes: 24,
                    nodes_live: 2,
                    growth_limited: None,
                    workflow_name: String::new(),
                    context_runs: Vec::new(),
                    restore_from_run: None,
                    lead_session_id: None,
                    team_name: None,
                    lead_pane_id: None,
                    lead_prompt_version: None,
                },
            },
        }
    }

    fn event_subscription(kind: Subscription) -> ActiveSubscription {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        ActiveSubscription::new(kind, "test", 0, &api_tx, &event_hub).expect("event subscription")
    }

    /// The delivered `event` field of each line the stream loop would write.
    fn delivered_kinds(values: &[serde_json::Value]) -> Vec<String> {
        values
            .iter()
            .map(|value| {
                let kind = value["event"].as_str().unwrap_or_default().to_string();
                match value["data"]["node"]["path"].as_str() {
                    Some(path) => format!("{kind}({path})"),
                    None => kind,
                }
            })
            .collect()
    }

    /// The P1 regression, at the layer that caused it. `EventHub` is already a
    /// single globally-sequenced ring; it was the *delivery* layer that gave
    /// every subscribed type its own cursor and drained one event per type per
    /// poll pass, so a run's `workflow.run.finished` overtook the node events it
    /// summarises.
    #[test]
    fn event_subscriptions_deliver_cross_type_events_in_hub_order() {
        let event_hub = EventHub::default();
        let subscriptions = vec![
            event_subscription(Subscription::WorkflowNodeCreated {}),
            event_subscription(Subscription::WorkflowRunFinished {}),
        ];
        let mut cursor =
            EventStreamCursor::from_subscriptions(&subscriptions).expect("an event cursor");

        event_hub.push(node_created_event("fanout/worker/1"));
        event_hub.push(run_finished_event());
        event_hub.push(node_created_event("fanout/worker/2"));

        let first_pass = delivered_kinds(&cursor.drain(&event_hub));

        assert_eq!(
            first_pass,
            vec![
                "workflow_node_created(fanout/worker/1)".to_string(),
                "workflow_run_finished".to_string(),
                "workflow_node_created(fanout/worker/2)".to_string(),
            ],
            "one poll pass must deliver the whole backlog in hub order, not one event per type"
        );
        assert!(
            cursor.drain(&event_hub).is_empty(),
            "a drained backlog is not redelivered"
        );
    }

    /// The merged cursor must not collapse a client that asked for the same
    /// type twice into one delivery.
    #[test]
    fn a_duplicate_event_subscription_still_receives_every_event_once_each() {
        let event_hub = EventHub::default();
        let subscriptions = vec![
            event_subscription(Subscription::WorkflowNodeCreated {}),
            event_subscription(Subscription::WorkflowNodeCreated {}),
        ];
        let mut cursor =
            EventStreamCursor::from_subscriptions(&subscriptions).expect("an event cursor");

        event_hub.push(node_created_event("plan"));

        assert_eq!(
            delivered_kinds(&cursor.drain(&event_hub)),
            vec![
                "workflow_node_created(plan)".to_string(),
                "workflow_node_created(plan)".to_string(),
            ]
        );
    }

    /// The cursor starts at `0`, not at the hub's current sequence, so a
    /// subscriber that connects after the events were pushed still sees
    /// whatever the ring retained — exactly what the per-subscription cursors
    /// it replaces did.
    #[test]
    fn a_new_subscriber_still_replays_the_retained_ring() {
        let event_hub = EventHub::default();
        event_hub.push(node_created_event("plan"));
        event_hub.push(run_finished_event());

        let subscriptions = vec![
            event_subscription(Subscription::WorkflowNodeCreated {}),
            event_subscription(Subscription::WorkflowRunFinished {}),
        ];
        let mut cursor =
            EventStreamCursor::from_subscriptions(&subscriptions).expect("an event cursor");

        assert_eq!(
            delivered_kinds(&cursor.drain(&event_hub)),
            vec![
                "workflow_node_created(plan)".to_string(),
                "workflow_run_finished".to_string(),
            ]
        );
    }

    /// A connection with no event-log subscription keeps the old shape: no
    /// cursor at all, and the poll loop is unchanged.
    #[test]
    fn a_connection_without_event_subscriptions_has_no_cursor() {
        let subscriptions = vec![ActiveSubscription::ScrollChanged(
            ActiveScrollChangedSubscription {
                pane_id: "pane_1".into(),
                last_scroll: None,
                request_prefix: "test".into(),
            },
        )];

        assert!(EventStreamCursor::from_subscriptions(&subscriptions).is_none());
        assert!(subscriptions[0].event_kind().is_none());
    }

    fn pane_info_with_scroll(scroll: Option<PaneScrollInfo>) -> PaneInfo {
        PaneInfo {
            pane_id: "pane_1".into(),
            terminal_id: "terminal_1".into(),
            workspace_id: "workspace_1".into(),
            tab_id: "tab_1".into(),
            focused: true,
            cwd: None,
            foreground_cwd: None,
            label: None,
            agent: None,
            title: None,
            terminal_title: None,
            terminal_title_stripped: None,
            display_agent: None,
            agent_status: AgentStatus::Unknown,
            state_labels: HashMap::new(),
            tokens: HashMap::new(),
            agent_session: None,
            scroll,
            revision: 0,
        }
    }

    #[test]
    fn workspace_metadata_subscription_uses_dedicated_event_kind() {
        let event_hub = EventHub::default();
        let (api_tx, _api_rx) = tokio::sync::mpsc::unbounded_channel();
        let subscription = ActiveSubscription::new(
            Subscription::WorkspaceMetadataUpdated {},
            "test",
            0,
            &api_tx,
            &event_hub,
        )
        .expect("workspace metadata subscription");

        assert!(matches!(
            subscription,
            ActiveSubscription::Event(ActiveEventSubscription {
                event_kind: EventKind::WorkspaceMetadataUpdated,
                ..
            })
        ));
    }

    #[test]
    fn scroll_subscription_emits_when_scroll_snapshot_changes() {
        let at_bottom = PaneScrollInfo {
            offset_from_bottom: 0,
            max_offset_from_bottom: 40,
            viewport_rows: 20,
        };
        let scrolled_back = PaneScrollInfo {
            offset_from_bottom: 8,
            max_offset_from_bottom: 40,
            viewport_rows: 20,
        };
        let mut subscription = ActiveScrollChangedSubscription {
            pane_id: "pane_1".into(),
            last_scroll: Some(at_bottom),
            request_prefix: "test".into(),
        };

        assert!(subscription
            .event_from_snapshot(pane_info_with_scroll(Some(at_bottom)))
            .is_none());

        let event = subscription
            .event_from_snapshot(pane_info_with_scroll(Some(scrolled_back)))
            .expect("scroll event");
        assert_eq!(event.event, SubscriptionEventKind::ScrollChanged);
        let SubscriptionEventData::ScrollChanged(data) = event.data else {
            panic!("wrong event data");
        };
        assert_eq!(data.pane_id, "pane_1");
        assert_eq!(data.workspace_id, "workspace_1");
        assert_eq!(data.scroll, scrolled_back);
    }

    #[test]
    fn agent_status_subscription_replays_queued_metadata_set_and_expiry_events() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveAgentStatusChangedSubscription {
            pane_id: "pane_1".into(),
            status_filter: None,
            last_status: Some(AgentStatus::Working),
            last_presentation: Some(PanePresentationSnapshot {
                title: None,
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            last_sequence: event_hub.current_sequence(),
            initial_event: None,
            request_prefix: "test".into(),
        };

        event_hub.push(presentation_event(Some("short lived")));
        event_hub.push(presentation_event(None));

        let set_event = subscription
            .poll(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("set event");
        let SubscriptionEventData::PaneAgentStatusChanged(set_data) = set_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(set_data.title.as_deref(), Some("short lived"));

        let expiry_event = subscription
            .poll(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("expiry event");
        let SubscriptionEventData::PaneAgentStatusChanged(expiry_data) = expiry_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(expiry_data.title, None);
    }

    #[test]
    fn agent_status_subscription_prefers_setup_window_events_over_initial_snapshot() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveAgentStatusChangedSubscription {
            pane_id: "pane_1".into(),
            status_filter: Some(AgentStatus::Working),
            last_status: Some(AgentStatus::Working),
            last_presentation: Some(PanePresentationSnapshot {
                title: None,
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            last_sequence: event_hub.current_sequence(),
            initial_event: Some(PaneAgentStatusChangedEvent {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: None,
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            request_prefix: "test".into(),
        };

        event_hub.push(presentation_event(Some("short lived")));
        event_hub.push(presentation_event(None));

        let set_event = subscription
            .poll(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("set event");
        let SubscriptionEventData::PaneAgentStatusChanged(set_data) = set_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(set_data.title.as_deref(), Some("short lived"));

        let expiry_event = subscription
            .poll(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("expiry event");
        let SubscriptionEventData::PaneAgentStatusChanged(expiry_data) = expiry_event.data else {
            panic!("wrong event data");
        };
        assert_eq!(expiry_data.title, None);
    }

    #[test]
    fn agent_status_subscription_emits_setup_window_event_already_reflected_by_probe() {
        let event_hub = EventHub::default();
        let mut subscription = ActiveAgentStatusChangedSubscription {
            pane_id: "pane_1".into(),
            status_filter: Some(AgentStatus::Working),
            last_status: Some(AgentStatus::Working),
            last_presentation: Some(PanePresentationSnapshot {
                title: Some("short lived".into()),
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            last_sequence: event_hub.current_sequence(),
            initial_event: Some(PaneAgentStatusChangedEvent {
                pane_id: "pane_1".into(),
                workspace_id: "workspace_1".into(),
                agent_status: AgentStatus::Working,
                agent: Some("pi".into()),
                title: Some("short lived".into()),
                display_agent: None,
                state_labels: HashMap::new(),
            }),
            request_prefix: "test".into(),
        };

        event_hub.push(presentation_event(Some("short lived")));

        let event = subscription
            .poll(&tokio::sync::mpsc::unbounded_channel().0, &event_hub)
            .expect("setup-window event");
        let SubscriptionEventData::PaneAgentStatusChanged(data) = event.data else {
            panic!("wrong event data");
        };
        assert_eq!(data.title.as_deref(), Some("short lived"));
        assert!(subscription.initial_event.is_none());
    }
}
