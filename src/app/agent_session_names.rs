//! Periodic resolution of agent session display names.
//!
//! Session names live in the agent's own on-disk registry, which is foreign
//! state that changes without notifying Karvex. Resolution is therefore a poll:
//! read the registry off the loop thread, then fold the result back into
//! terminal state through the normal internal-event path.
//!
//! The poll is demand-gated. Nothing is read unless some terminal is actually on
//! an id-style agent session, so installs where no such agent ever runs do no
//! filesystem work at all.

use std::collections::HashMap;
use std::time::Instant;

use super::{App, AGENT_SESSION_NAME_REFRESH_INTERVAL};
use crate::app::AppState;
use crate::events::AppEvent;
use crate::terminal::TerminalId;

impl AppState {
    /// Applies a session-id to display-name map across every terminal.
    ///
    /// Returns the terminals whose resolved name actually changed. Pure state
    /// work: no filesystem access and no runtime dependency, so it is testable
    /// without PTYs.
    pub(crate) fn apply_agent_session_names(
        &mut self,
        names: &HashMap<String, String>,
    ) -> Vec<TerminalId> {
        self.terminals
            .values_mut()
            .filter_map(|terminal| {
                terminal
                    .apply_resolved_agent_session_names(names)
                    .then(|| terminal.id.clone())
            })
            .collect()
    }
}

impl App {
    /// When the next registry read is due, or `None` while one is in flight or
    /// no terminal has a session to resolve.
    ///
    /// Feeds the loop's sleep deadline so a name that changes on disk still
    /// surfaces in an otherwise idle session.
    pub(crate) fn agent_session_name_refresh_deadline(&self) -> Option<Instant> {
        (!self.agent_session_name_refresh_in_flight && self.has_resolvable_agent_session())
            .then_some(self.last_agent_session_name_refresh + AGENT_SESSION_NAME_REFRESH_INTERVAL)
    }

    fn has_resolvable_agent_session(&self) -> bool {
        self.state
            .terminals
            .values()
            .any(|terminal| terminal.agent_session_id().is_some())
    }

    /// Starts a registry read if one is due.
    ///
    /// The read happens on a worker thread: the registry is a directory of small
    /// files, but it is still someone else's filesystem, and the render loop
    /// must not block on it.
    pub(crate) fn start_agent_session_name_refresh_if_due(&mut self, now: Instant) {
        let Some(deadline) = self.agent_session_name_refresh_deadline() else {
            return;
        };
        if now < deadline {
            return;
        }

        self.agent_session_name_refresh_in_flight = true;
        self.last_agent_session_name_refresh = now;
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let names = crate::agent_session_registry::read_session_names();
            let _ = event_tx.blocking_send(AppEvent::AgentSessionNamesRefreshed { names });
        });
    }

    /// Folds a completed registry read into terminal state.
    ///
    /// Returns whether any terminal's resolved name changed, so an unchanged
    /// registry (the common case between renames) costs no repaint and emits no
    /// pane updates.
    pub(crate) fn apply_agent_session_names(&mut self, names: HashMap<String, String>) -> bool {
        self.agent_session_name_refresh_in_flight = false;

        let changed_terminals = self.state.apply_agent_session_names(&names);
        if changed_terminals.is_empty() {
            return false;
        }

        // The session name is a shared runtime fact, so API subscribers hear
        // about it the same way they hear about any other pane change.
        for (ws_idx, pane_id) in self.panes_for_terminals(&changed_terminals) {
            self.emit_pane_updated(ws_idx, pane_id);
        }
        true
    }

    /// Locates the panes attached to the given terminals, as `(workspace index,
    /// pane id)` pairs suitable for `emit_pane_updated`.
    fn panes_for_terminals(
        &self,
        terminal_ids: &[TerminalId],
    ) -> Vec<(usize, crate::layout::PaneId)> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, workspace)| {
                workspace
                    .tabs
                    .iter()
                    .flat_map(|tab| tab.panes.iter())
                    .filter(|(_, pane)| terminal_ids.contains(&pane.attached_terminal_id))
                    .map(move |(pane_id, _)| (ws_idx, *pane_id))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_resume::{AgentSessionRef, PersistedAgentSession};
    use crate::detect::AgentState;
    use crate::terminal::TerminalState;
    use crate::workspace::Workspace;

    const SESSION_ID: &str = "f593fc46-5328-4998-a7b1-80bb1b3e7e3b";
    const OTHER_SESSION_ID: &str = "11111111-2222-3333-4444-555555555555";

    fn id_session(session_id: &str) -> PersistedAgentSession {
        PersistedAgentSession {
            source: "karvex:claude".into(),
            agent: "claude".into(),
            session_ref: AgentSessionRef::id(session_id).expect("valid session id"),
        }
    }

    /// One workspace whose panes have real terminals attached, matching how the
    /// rest of the app-level tests build PTY-free state.
    fn state_with_panes(pane_count: usize) -> AppState {
        let mut state = AppState::test_new();
        let mut workspace = Workspace::test_new("karvex");
        for _ in 1..pane_count {
            workspace.test_split(ratatui::layout::Direction::Horizontal);
        }
        state.workspaces.push(workspace);
        state.ensure_test_terminals();
        state
    }

    /// Terminal ids of the workspace's panes in layout order, which is stable
    /// across runs in a way that hashed terminal ids are not.
    fn terminal_ids(state: &AppState) -> Vec<crate::terminal::TerminalId> {
        let tab = &state.workspaces[0].tabs[0];
        tab.layout
            .pane_ids()
            .iter()
            .filter_map(|pane_id| tab.panes.get(pane_id))
            .map(|pane| pane.attached_terminal_id.clone())
            .collect()
    }

    fn terminal<'a>(state: &'a AppState, id: &crate::terminal::TerminalId) -> &'a TerminalState {
        state.terminals.get(id).expect("terminal")
    }

    fn put_on_session(state: &mut AppState, id: &crate::terminal::TerminalId, session_id: &str) {
        state
            .terminals
            .get_mut(id)
            .expect("terminal")
            .set_persisted_agent_session(id_session(session_id));
    }

    #[test]
    fn a_resolved_name_replaces_the_short_id_fallback() {
        let mut state = state_with_panes(1);
        let id = terminal_ids(&state)[0].clone();
        put_on_session(&mut state, &id, SESSION_ID);

        // Before resolution the row still distinguishes itself by short id.
        assert_eq!(
            terminal(&state, &id)
                .agent_session_display_name()
                .as_deref(),
            Some("f593fc46")
        );

        let names = HashMap::from([(SESSION_ID.to_string(), "toilet-presence-sensor".to_string())]);
        let changed = state.apply_agent_session_names(&names);

        assert_eq!(changed, vec![id.clone()]);
        assert_eq!(
            terminal(&state, &id).agent_session_name(),
            Some("toilet-presence-sensor")
        );
        assert_eq!(
            terminal(&state, &id)
                .agent_session_display_name()
                .as_deref(),
            Some("toilet-presence-sensor")
        );
    }

    #[test]
    fn sibling_panes_in_one_workspace_resolve_to_their_own_names() {
        let mut state = state_with_panes(2);
        let ids = terminal_ids(&state);
        put_on_session(&mut state, &ids[0], SESSION_ID);
        put_on_session(&mut state, &ids[1], OTHER_SESSION_ID);

        let names = HashMap::from([
            (SESSION_ID.to_string(), "sensor-pcb".to_string()),
            (OTHER_SESSION_ID.to_string(), "docs-pass".to_string()),
        ]);
        state.apply_agent_session_names(&names);

        // The whole point of the feature: same workspace, same agent, two rows
        // that no longer read identically.
        assert_eq!(
            terminal(&state, &ids[0])
                .agent_session_display_name()
                .as_deref(),
            Some("sensor-pcb")
        );
        assert_eq!(
            terminal(&state, &ids[1])
                .agent_session_display_name()
                .as_deref(),
            Some("docs-pass")
        );
    }

    #[test]
    fn a_name_resolved_for_another_session_is_never_shown() {
        let mut state = state_with_panes(1);
        let id = terminal_ids(&state)[0].clone();
        put_on_session(&mut state, &id, SESSION_ID);

        let names = HashMap::from([("some-other-session".to_string(), "wrong".to_string())]);
        let changed = state.apply_agent_session_names(&names);

        assert!(changed.is_empty());
        assert_eq!(terminal(&state, &id).agent_session_name(), None);
        assert_eq!(
            terminal(&state, &id)
                .agent_session_display_name()
                .as_deref(),
            Some("f593fc46")
        );
    }

    #[test]
    fn a_session_change_drops_the_previous_sessions_name_immediately() {
        let mut state = state_with_panes(1);
        let id = terminal_ids(&state)[0].clone();
        put_on_session(&mut state, &id, SESSION_ID);
        state.apply_agent_session_names(&HashMap::from([(
            SESSION_ID.to_string(),
            "first".to_string(),
        )]));
        assert_eq!(terminal(&state, &id).agent_session_name(), Some("first"));

        // The pane resumes onto a different session before the next refresh.
        put_on_session(&mut state, &id, OTHER_SESSION_ID);

        assert_eq!(terminal(&state, &id).agent_session_name(), None);
        assert_eq!(
            terminal(&state, &id)
                .agent_session_display_name()
                .as_deref(),
            Some("11111111")
        );
    }

    #[test]
    fn an_unchanged_registry_reports_no_changed_terminals() {
        let mut state = state_with_panes(1);
        let id = terminal_ids(&state)[0].clone();
        put_on_session(&mut state, &id, SESSION_ID);
        let names = HashMap::from([(SESSION_ID.to_string(), "stable".to_string())]);

        assert_eq!(state.apply_agent_session_names(&names), vec![id]);
        assert!(state.apply_agent_session_names(&names).is_empty());
    }

    #[test]
    fn a_name_that_disappears_from_the_registry_falls_back_to_the_short_id() {
        let mut state = state_with_panes(1);
        let id = terminal_ids(&state)[0].clone();
        put_on_session(&mut state, &id, SESSION_ID);
        state.apply_agent_session_names(&HashMap::from([(
            SESSION_ID.to_string(),
            "named".to_string(),
        )]));

        // The agent exited, so its registry entry is gone.
        let changed = state.apply_agent_session_names(&HashMap::new());

        assert_eq!(changed, vec![id.clone()]);
        assert_eq!(
            terminal(&state, &id)
                .agent_session_display_name()
                .as_deref(),
            Some("f593fc46")
        );
    }

    #[test]
    fn terminals_without_an_agent_session_resolve_to_nothing() {
        let mut state = state_with_panes(1);
        let id = terminal_ids(&state)[0].clone();

        let names = HashMap::from([(SESSION_ID.to_string(), "irrelevant".to_string())]);
        assert!(state.apply_agent_session_names(&names).is_empty());
        assert_eq!(terminal(&state, &id).agent_session_id(), None);
        assert_eq!(terminal(&state, &id).agent_session_display_name(), None);
    }

    #[test]
    fn a_path_style_session_has_no_id_to_resolve() {
        let mut state = state_with_panes(1);
        let id = terminal_ids(&state)[0].clone();
        state
            .terminals
            .get_mut(&id)
            .expect("terminal")
            .set_persisted_agent_session(PersistedAgentSession {
                source: "karvex:pi".into(),
                agent: "pi".into(),
                session_ref: AgentSessionRef::path("/tmp/session.jsonl")
                    .expect("valid session path"),
            });

        // Agents that identify a session by transcript path expose no id, so
        // there is nothing to look up and the row element stays absent.
        assert_eq!(terminal(&state, &id).agent_session_id(), None);
        assert_eq!(terminal(&state, &id).agent_session_display_name(), None);
    }

    #[test]
    fn a_live_hook_session_is_resolved_like_a_persisted_one() {
        let mut state = state_with_panes(1);
        let id = terminal_ids(&state)[0].clone();
        state
            .terminals
            .get_mut(&id)
            .expect("terminal")
            .set_hook_authority_with_session_ref(
                "karvex:claude".into(),
                "claude".into(),
                AgentState::Working,
                None,
                AgentSessionRef::id(SESSION_ID),
                None,
            );

        // A pane whose session is only known from a live hook report, with
        // nothing persisted yet, still resolves a name.
        assert_eq!(terminal(&state, &id).agent_session_id(), Some(SESSION_ID));

        state.apply_agent_session_names(&HashMap::from([(
            SESSION_ID.to_string(),
            "from-hook".to_string(),
        )]));
        assert_eq!(
            terminal(&state, &id).agent_session_name(),
            Some("from-hook")
        );
    }
}
