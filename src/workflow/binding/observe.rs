//! Detection, hook, and transcript-tail evidence, mapped back to
//! `EngineInput`.
//!
//! Step 3c fills this in against
//! `docs/design/workflow-builder/04-kvdag-and-execution.md` §4.3 and §6.1:
//! `AgentStatus` from the detector, `TurnEnded` from the Claude `stop` hook,
//! `PaneExited`, and `ProgressObserved` from material progress only.
//!
//! Every function here is a pure translation over facts the `App` glue has
//! already resolved to a public pane id or an instance path. Nothing in this
//! file reads runtime state, so the whole observe direction is testable over
//! synthetic events.

use std::time::Instant;

use sha2::{Digest, Sha256};
use tracing::debug;

use crate::app::actions::PaneStateUpdate;
use crate::detect::AgentState;
use crate::workflow::model::{
    EngineInput, InstancePath, NodeToken, ProgressDelta, PublicPaneId, RawJson,
};

/// The `source` the bundled Claude hook asset reports under. Claude has no
/// per-agent hook-event table; its integration installs exactly two hooks, and
/// `SessionStart` reports through `pane.report_agent_session`. A
/// `pane.report_agent` under this source is therefore the `stop` hook and
/// nothing else.
pub const CLAUDE_HOOK_SOURCE: &str = "karvex:claude";
pub const CLAUDE_AGENT_LABEL: &str = "claude";

// ── detector (§4.3 signal 3) ────────────────────────────────────────────────

/// One detector observation for a bound pane.
///
/// The sustained-idle rule counts detector ticks, so this is the sampler the
/// engine pump calls on its own cadence.
/// [`agent_status_from_pane_update`] alone cannot satisfy it:
/// `emit_pane_state_update` fires on *changes*, and a pane that has been idle
/// for three ticks produced exactly one change.
pub fn agent_status(pane: PublicPaneId, state: AgentState, at: Instant) -> EngineInput {
    EngineInput::AgentStatus { pane, state, at }
}

/// The `emit_pane_state_update` side of §4.3 signal 3.
///
/// Returns `None` for presentation-only updates (title, display agent, state
/// labels): those are not detector evidence and would otherwise reset or
/// advance the idle streak on a pure redraw.
pub fn agent_status_from_pane_update(
    pane: PublicPaneId,
    update: &PaneStateUpdate,
    at: Instant,
) -> Option<EngineInput> {
    if update.state == update.previous_state && !update.agent_released {
        return None;
    }
    // A released agent hands the pane back to the shell; the node is no longer
    // observing an agent, which the engine must see as "not idle".
    let state = if update.agent_released {
        AgentState::Unknown
    } else {
        update.state
    };
    Some(agent_status(pane, state, at))
}

// ── turn end (§4.3 signal 2) ────────────────────────────────────────────────

/// The fields of a `pane.report_agent` call, as they arrive on
/// `AppEvent::HookStateReported`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookStateReport<'a> {
    pub source: &'a str,
    pub agent_label: &'a str,
    pub state: AgentState,
}

/// Maps the Claude `stop` hook to `EngineInput::TurnEnded`.
///
/// Screen detection stays authoritative for the user-visible pane status; this
/// report exists only to feed the workflow binder, so anything that is not the
/// bundled Claude hook reporting an ended turn is ignored rather than treated
/// as weaker evidence.
pub fn turn_ended(pane: PublicPaneId, report: HookStateReport<'_>) -> Option<EngineInput> {
    if report.source != CLAUDE_HOOK_SOURCE || report.agent_label != CLAUDE_AGENT_LABEL {
        return None;
    }
    if report.state != AgentState::Idle {
        return None;
    }
    Some(EngineInput::TurnEnded { pane })
}

// ── pane exit ───────────────────────────────────────────────────────────────

/// `PaneExited` before a valid result is a `Failed` with the exit code (§4.3).
///
/// `AppEvent::PaneDied` carries no exit status today, so the `App` glue passes
/// `None` and the engine records the failure without a code.
pub fn pane_exited(pane: PublicPaneId, code: Option<i32>) -> EngineInput {
    EngineInput::PaneExited { pane, code }
}

// ── self-report (§4.3 signal 1) ─────────────────────────────────────────────

/// Why a `workflow.node.report` call was not turned into an `EngineInput`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportRejected {
    /// The path names no node that has been spawned in this run.
    UnknownNode,
    /// The presented token does not match the one minted for that node.
    InvalidToken,
    /// The node reported no result at all.
    MissingResult,
}

impl ReportRejected {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownNode => "workflow_node_not_found",
            Self::InvalidToken => "workflow_node_token_invalid",
            Self::MissingResult => "workflow_node_result_missing",
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            Self::UnknownNode => "no running node has that instance path",
            Self::InvalidToken => "the node token does not match this node",
            Self::MissingResult => "the report carried no result document",
        }
    }
}

impl std::fmt::Display for ReportRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for ReportRejected {}

/// Authenticates and translates `workflow.node.report`.
///
/// The engine never sees the mint and so cannot re-check the token; the binder
/// owns both ends. `expected` is the token [`super::spawn::mint_node_token`]
/// produced for this node, or `None` when the node has never been spawned.
pub fn node_self_report(
    path: &str,
    token: &str,
    expected: Option<&NodeToken>,
    result: Option<serde_json::Value>,
) -> Result<EngineInput, ReportRejected> {
    let path = path.trim();
    if path.is_empty() {
        return Err(ReportRejected::UnknownNode);
    }
    let Some(expected) = expected else {
        debug!(path, "workflow node report for a node with no minted token");
        return Err(ReportRejected::UnknownNode);
    };
    if !tokens_match(expected.0.as_bytes(), token.as_bytes()) {
        return Err(ReportRejected::InvalidToken);
    }
    // A caller that passes no result at all is a shape error and never reaches
    // the engine. An explicit `null` is different: it is the wire's "I tried to
    // finish and have no result artifact", which §4.3 answers with
    // `NeedsAttention` in the engine. Rejecting it here would put completion
    // authority back on the client and leave the node `Running` forever.
    let Some(result) = result else {
        return Err(ReportRejected::MissingResult);
    };
    Ok(EngineInput::NodeSelfReport {
        path: InstancePath(path.to_string()),
        token: expected.clone(),
        result: RawJson(result),
    })
}

/// Length-independent, byte-independent comparison, so a wrong token leaks no
/// prefix through timing. Local sockets make this cheap insurance rather than a
/// hard requirement, but a capability check should never short-circuit.
fn tokens_match(expected: &[u8], presented: &[u8]) -> bool {
    // Folded in as a boolean, not as `len ^ len` truncated to a byte: two
    // lengths 256 apart would xor to zero and compare equal.
    let mut diff = u8::from(expected.len() != presented.len());
    for index in 0..expected.len().max(presented.len()) {
        let left = expected.get(index).copied().unwrap_or(0);
        let right = presented.get(index).copied().unwrap_or(0);
        diff |= left ^ right;
    }
    diff == 0
}

// ── materiality (§6.1) ──────────────────────────────────────────────────────

/// Wraps a progress observation, dropping immaterial ones before they reach the
/// engine.
///
/// §6.1 is explicit that the agent producing text, the process being alive, and
/// the screen merely redrawing are *not* progress. A delta with no tool call,
/// no token, no artifact change, and no screen digest carries none of the four
/// material signals, so it is not an observation at all.
pub fn progress_observed(path: InstancePath, delta: ProgressDelta) -> Option<EngineInput> {
    if delta.tool_calls == 0
        && delta.tokens == 0
        && delta.artifact_changes == 0
        && delta.screen_digest.is_none()
    {
        return None;
    }
    Some(EngineInput::ProgressObserved { path, delta })
}

/// Digest of a `ReadSource::Detection` snapshot. Comparing digests, not the
/// snapshots themselves, is what keeps the watchdog's screen check bounded.
pub fn screen_digest(snapshot: &str) -> String {
    let digest = Sha256::digest(snapshot.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    use crate::terminal::EffectivePresentation;

    fn pane() -> PublicPaneId {
        PublicPaneId("w1:p2".to_string())
    }

    fn presentation() -> EffectivePresentation {
        EffectivePresentation {
            title: None,
            display_agent: None,
            state_labels: HashMap::new(),
        }
    }

    fn pane_update(previous: AgentState, state: AgentState) -> PaneStateUpdate {
        PaneStateUpdate {
            pane_id: crate::layout::PaneId::from_raw(1),
            ws_idx: 0,
            previous_agent_label: Some("claude".to_string()),
            previous_known_agent: Some(crate::detect::Agent::Claude),
            previous_state: previous,
            previous_seen: false,
            previous_presentation: presentation(),
            agent_label: Some("claude".to_string()),
            known_agent: Some(crate::detect::Agent::Claude),
            state,
            seen: false,
            presentation: presentation(),
            agent_name_changed: false,
            agent_released: false,
            agent_release_status: None,
        }
    }

    #[test]
    fn agent_status_carries_the_observed_state() {
        let at = Instant::now();
        assert_eq!(
            agent_status(pane(), AgentState::Working, at),
            EngineInput::AgentStatus {
                pane: pane(),
                state: AgentState::Working,
                at,
            }
        );
    }

    #[test]
    fn pane_update_translates_only_real_state_changes() {
        let at = Instant::now();
        let changed = pane_update(AgentState::Working, AgentState::Idle);
        assert_eq!(
            agent_status_from_pane_update(pane(), &changed, at),
            Some(EngineInput::AgentStatus {
                pane: pane(),
                state: AgentState::Idle,
                at,
            })
        );

        let mut presentation_only = pane_update(AgentState::Idle, AgentState::Idle);
        presentation_only.presentation.title = Some("new title".to_string());
        assert_eq!(
            agent_status_from_pane_update(pane(), &presentation_only, at),
            None
        );
    }

    #[test]
    fn a_released_agent_is_reported_as_no_longer_idle() {
        let at = Instant::now();
        let mut released = pane_update(AgentState::Idle, AgentState::Idle);
        released.agent_released = true;
        released.agent_label = None;
        assert_eq!(
            agent_status_from_pane_update(pane(), &released, at),
            Some(EngineInput::AgentStatus {
                pane: pane(),
                state: AgentState::Unknown,
                at,
            })
        );
    }

    #[test]
    fn the_claude_stop_hook_is_a_turn_end() {
        assert_eq!(
            turn_ended(
                pane(),
                HookStateReport {
                    source: CLAUDE_HOOK_SOURCE,
                    agent_label: CLAUDE_AGENT_LABEL,
                    state: AgentState::Idle,
                }
            ),
            Some(EngineInput::TurnEnded { pane: pane() })
        );
    }

    #[test]
    fn other_hook_reports_are_not_turn_ends() {
        for report in [
            HookStateReport {
                source: "karvex:kimi",
                agent_label: "kimi",
                state: AgentState::Idle,
            },
            HookStateReport {
                source: "custom:claude",
                agent_label: CLAUDE_AGENT_LABEL,
                state: AgentState::Idle,
            },
            HookStateReport {
                source: CLAUDE_HOOK_SOURCE,
                agent_label: CLAUDE_AGENT_LABEL,
                state: AgentState::Working,
            },
            HookStateReport {
                source: CLAUDE_HOOK_SOURCE,
                agent_label: CLAUDE_AGENT_LABEL,
                state: AgentState::Blocked,
            },
        ] {
            assert_eq!(turn_ended(pane(), report), None, "{report:?}");
        }
    }

    #[test]
    fn pane_exit_passes_the_code_through() {
        assert_eq!(
            pane_exited(pane(), Some(1)),
            EngineInput::PaneExited {
                pane: pane(),
                code: Some(1),
            }
        );
        assert_eq!(
            pane_exited(pane(), None),
            EngineInput::PaneExited {
                pane: pane(),
                code: None,
            }
        );
    }

    #[test]
    fn a_self_report_needs_the_minted_token() {
        let minted = NodeToken("d0b3".to_string());
        let result = serde_json::json!({ "plan": "step one" });

        assert_eq!(
            node_self_report("plan", "d0b3", Some(&minted), Some(result.clone())),
            Ok(EngineInput::NodeSelfReport {
                path: InstancePath("plan".to_string()),
                token: minted.clone(),
                result: RawJson(result.clone()),
            })
        );
        assert_eq!(
            node_self_report("plan", "d0b4", Some(&minted), Some(result.clone())),
            Err(ReportRejected::InvalidToken)
        );
        assert_eq!(
            node_self_report("plan", "d0b", Some(&minted), Some(result.clone())),
            Err(ReportRejected::InvalidToken)
        );
        assert_eq!(
            node_self_report("plan", "d0b3", None, Some(result)),
            Err(ReportRejected::UnknownNode)
        );
    }

    #[test]
    fn a_self_report_without_a_result_is_rejected() {
        let minted = NodeToken("d0b3".to_string());
        assert_eq!(
            node_self_report("plan", "d0b3", Some(&minted), None),
            Err(ReportRejected::MissingResult),
            "an internal caller that passes no result at all is a shape error"
        );
        assert_eq!(
            node_self_report("  ", "d0b3", Some(&minted), Some(serde_json::json!({}))),
            Err(ReportRejected::UnknownNode)
        );
    }

    /// §4.3 makes the engine the completion authority, and an explicit `null`
    /// on the wire is the node saying "I tried to finish and have no result
    /// artifact". Rejecting it here would hand that decision back to the
    /// client, which is what left a `runner = "command"` node stuck `Running`
    /// with the server never told it had reported.
    #[test]
    fn an_authenticated_report_of_no_result_reaches_the_engine() {
        let minted = NodeToken("d0b3".to_string());
        assert_eq!(
            node_self_report("plan", "d0b3", Some(&minted), Some(serde_json::Value::Null)),
            Ok(EngineInput::NodeSelfReport {
                path: InstancePath("plan".to_string()),
                token: minted.clone(),
                result: RawJson(serde_json::Value::Null),
            })
        );
        assert_eq!(
            node_self_report(
                "plan",
                "wrong",
                Some(&minted),
                Some(serde_json::Value::Null)
            ),
            Err(ReportRejected::InvalidToken),
            "a null result is still authenticated"
        );
    }

    #[test]
    fn a_self_report_path_is_trimmed_not_rewritten() {
        let minted = NodeToken("d0b3".to_string());
        let input = node_self_report(
            " research/2/verify ",
            "d0b3",
            Some(&minted),
            Some(serde_json::json!({})),
        )
        .unwrap();
        assert_eq!(
            input,
            EngineInput::NodeSelfReport {
                path: InstancePath("research/2/verify".to_string()),
                token: minted,
                result: RawJson(serde_json::json!({})),
            }
        );
    }

    #[test]
    fn only_material_progress_reaches_the_engine() {
        let path = InstancePath("plan".to_string());
        assert_eq!(
            progress_observed(path.clone(), ProgressDelta::default()),
            None
        );
        for delta in [
            ProgressDelta {
                tool_calls: 1,
                ..ProgressDelta::default()
            },
            ProgressDelta {
                tokens: 12,
                ..ProgressDelta::default()
            },
            ProgressDelta {
                artifact_changes: 1,
                ..ProgressDelta::default()
            },
            ProgressDelta {
                screen_digest: Some("abc".to_string()),
                ..ProgressDelta::default()
            },
        ] {
            assert_eq!(
                progress_observed(path.clone(), delta.clone()),
                Some(EngineInput::ProgressObserved {
                    path: path.clone(),
                    delta,
                })
            );
        }
    }

    #[test]
    fn screen_digest_is_stable_and_change_sensitive() {
        let first = screen_digest("> plan\n");
        assert_eq!(first, screen_digest("> plan\n"));
        assert_ne!(first, screen_digest("> plan!\n"));
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}
