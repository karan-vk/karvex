//! The `App` half of the lead-run binding and the run projection.
//!
//! `09-agent-teams-rework.md` §3.1 and §3.4. Karvex no longer pumps a run: it
//! launches one Claude Code team lead into a pane, then watches the two
//! directories Claude Code owns and projects what it sees into the run's own
//! records. This module is the only place that does the IO for either half;
//! the decisions live in `workflow::binding::lead` and `workflow::projection`,
//! which are pure and tested without a PTY.
//!
//! Boundary note (CLAUDE.md guardrail): everything here is a shared runtime
//! fact and is persisted through the store, so `workflow.run.get` answers from
//! the same rows a headless server wrote. Nothing here is TUI-private, and no
//! name here is a UI-surface name.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::api::schema::{EventData, EventEnvelope, EventKind};
use crate::workflow::binding::lead::{
    self, LeadBinding, LeadSpawnError, LeadSpawnSpec, MatchStrength,
};
use crate::workflow::binding::spawn;
use crate::workflow::model::{Kvdag, NodeKey, NodeStatus, RunId, RunStatus, StoreWrite};
use crate::workflow::projection::{
    self, ObservedTask, ObservedTeam, ProjectionSnapshot, TaskStatus,
};

/// How often the run projection re-reads Claude Code's task and team files.
///
/// These are a handful of tiny local JSON files, so the cost is a few `stat`s
/// and reads; 2 s is the cadence `09-agent-teams-rework.md` §3.4 specifies and
/// is well inside what a directory of task files can be polled at without
/// being noticed. The read is done inline rather than on a worker thread —
/// unlike the git-status refresh, there is no slow subprocess to get off the
/// loop.
pub(crate) const RUN_PROJECTION_INTERVAL: Duration = Duration::from_secs(2);

/// Upper bound on how many task files one poll will read, so a runaway lead
/// cannot make the loop quadratic in its own task count.
const MAX_TASK_FILES: usize = 512;

/// Upper bound on how many team configs one poll will parse while hunting for
/// this run's team. `~/.claude/teams/` is global and accumulates.
const MAX_TEAM_CONFIGS: usize = 256;

/// The live lead-run: what karvex launched, and what it has recognised so far.
///
/// Deliberately small. Everything durable is in the store; this is only what
/// the poller needs between ticks.
#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct LiveLeadRun {
    pub(crate) run_id: RunId,
    /// The lead's own pane, as a public id.
    pub(crate) lead_pane_id: String,
    pub(crate) lead_terminal_id: crate::terminal::TerminalId,
    /// What the lead pane was started in — the cwd `match_team` recognises the
    /// team by, since Claude Code does not let karvex assign the session id.
    pub(crate) lead_cwd: PathBuf,
    pub(crate) spawned_at_unix_ms: u64,
    /// The team, once recognised. `None` while the lead is still starting.
    pub(crate) binding: Option<LeadBinding>,
    /// Whether a teammate has ever been observed on a tmux backend. Latched:
    /// a teammate that finishes and goes inactive does not un-prove that
    /// split-pane mode took.
    pub(crate) split_pane_confirmed: bool,
    /// What the projection has already recorded, so an unchanged poll writes
    /// nothing.
    pub(crate) snapshot: ProjectionSnapshot,
    /// The definition's node keys, for subject→node matching.
    pub(crate) node_keys: Vec<NodeKey>,
    /// Set once `workflow.run.finish` or lead-exit has closed the run, so a
    /// late poll cannot reopen it.
    pub(crate) closed: bool,
    next_poll_at: Option<Instant>,
}

impl LiveLeadRun {
    fn poll_due(&self, now: Instant) -> bool {
        !self.closed && self.next_poll_at.is_none_or(|due| now >= due)
    }

    fn rearm(&mut self, now: Instant) {
        self.next_poll_at = Some(now + RUN_PROJECTION_INTERVAL);
    }

    pub(crate) fn next_poll_deadline(&self) -> Option<Instant> {
        if self.closed {
            return None;
        }
        Some(self.next_poll_at.unwrap_or_else(Instant::now))
    }
}

// Every method below is reachable only through the `workflow.*` API surface,
// which is stubbed out when the `workflow` feature is off (the MSVC cross-lint
// and slim source builds). The code still *compiles* without the feature — the
// store calls inside it are individually gated — so gating the module too would
// cost more `cfg` noise at the loop's deadline and scheduled-task sites than it
// saves.
#[cfg_attr(not(feature = "workflow"), allow(dead_code))]
impl crate::app::App {
    /// The deadline arm for the run projection. Folded into the loop's
    /// min-of-all-deadlines the same way every other periodic task is.
    pub(crate) fn run_projection_deadline(&self) -> Option<Instant> {
        self.workflow_lead
            .as_ref()
            .and_then(LiveLeadRun::next_poll_deadline)
    }

    /// Whether a lead run is live. The `workflow.run` guard uses this the same
    /// way it uses the engine's `is_live()`.
    pub(crate) fn lead_run_is_live(&self) -> bool {
        self.workflow_lead.as_ref().is_some_and(|run| !run.closed)
    }

    // ── launch ─────────────────────────────────────────────────────────────

    /// Runs the preflight `09-agent-teams-rework.md` §4's last risk row calls
    /// for: agent teams need a Claude Code new enough to have them, and a lead
    /// that starts fine but silently never spawns a teammate is the failure
    /// this prevents.
    pub(crate) fn preflight_claude_for_lead(&self) -> Result<(), LeadSpawnError> {
        let executable = crate::detect::interactive_agent_executable(crate::detect::Agent::Claude);
        let output = std::process::Command::new(executable)
            .arg("--version")
            .output()
            .map_err(|error| LeadSpawnError::ClaudeUnavailable(error.to_string()))?;
        let text = String::from_utf8_lossy(&output.stdout);
        lead::check_claude_version(text.trim()).map(|_| ())
    }

    /// Writes the run directory and the rendered lead prompt (§3.1 step 2).
    pub(crate) fn write_lead_prompt(
        &self,
        spec: &LeadSpawnSpec,
        prompt: &str,
    ) -> Result<(), LeadSpawnError> {
        std::fs::create_dir_all(&spec.run_dir)
            .map_err(|error| LeadSpawnError::RunDirUnwritable(error.to_string()))?;
        std::fs::write(spec.prompt_path(), prompt)
            .map_err(|error| LeadSpawnError::RunDirUnwritable(error.to_string()))
    }

    /// Spawns the lead's pane, using the same placement rule node panes used:
    /// a split of the run workspace's focused pane, without stealing focus.
    ///
    /// Returns the lead's public pane id and terminal id.
    pub(crate) fn spawn_lead_pane(
        &mut self,
        ws_idx: usize,
        spec: &LeadSpawnSpec,
    ) -> Result<(String, crate::terminal::TerminalId), LeadSpawnError> {
        let target_pane = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.focused_pane_id())
            .ok_or(LeadSpawnError::NoTargetPane)?;

        let (rows, cols) = self.state.estimate_pane_size();
        let argv = lead::lead_argv(spec);
        let env = lead::lead_env(spec);
        let workspace = self
            .state
            .workspaces
            .get_mut(ws_idx)
            .ok_or(LeadSpawnError::NoTargetPane)?;
        let spawned = workspace.split_pane_argv_command(
            target_pane,
            ratatui::layout::Direction::Horizontal,
            rows.max(spawn::MIN_PANE_ROWS),
            cols.max(spawn::MIN_PANE_COLS),
            Some(spec.cwd.clone()),
            &argv,
            env,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.state.host_terminal_appearance,
            // The lead is not focused on spawn for the same reason node panes
            // were not: the user watches a run from the DAG view and decides
            // when to step into it.
            false,
        );
        let (tab_idx, new_pane) = match spawned {
            Some(Ok(spawned)) => spawned,
            Some(Err(error)) => return Err(LeadSpawnError::PaneLaunchFailed(error.to_string())),
            None => return Err(LeadSpawnError::NoTargetPane),
        };

        let mut terminal = new_pane.terminal;
        let terminal_id = terminal.id.clone();
        terminal.set_manual_label(spec.pane_title());
        // The lead is an interactive `claude`, so it gets the same managed-agent
        // confirmation a node's agent pane got — that is what makes karvex's
        // per-pane agent detection report the lead's live state.
        terminal.begin_managed_agent(
            spec.pane_title(),
            crate::detect::Agent::Claude,
            Instant::now(),
            spawn::NODE_AGENT_SETTLE_DELAY,
            spawn::NODE_AGENT_LAUNCH_WINDOW,
        );
        self.terminal_runtimes
            .insert(terminal_id.clone(), new_pane.runtime);
        self.state
            .remove_alias_shadowed_by_new_pane(new_pane.pane_id);
        self.state.terminals.insert(terminal_id.clone(), terminal);
        self.schedule_session_save();
        if let Some(pane) = self.pane_info(ws_idx, new_pane.pane_id) {
            self.emit_event(EventEnvelope {
                event: EventKind::PaneCreated,
                data: EventData::PaneCreated { pane },
            });
        }
        self.emit_layout_updated_event(ws_idx, tab_idx);

        let pane_id = self
            .public_pane_id(ws_idx, new_pane.pane_id)
            .ok_or(LeadSpawnError::NoTargetPane)?;
        Ok((pane_id, terminal_id))
    }

    /// Records the live lead run once its pane exists.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bind_lead_run(
        &mut self,
        run_id: RunId,
        kvdag: &Kvdag,
        spec: &LeadSpawnSpec,
        lead_pane_id: String,
        lead_terminal_id: crate::terminal::TerminalId,
        spawned_at_unix_ms: u64,
    ) {
        self.workflow_lead = Some(LiveLeadRun {
            run_id,
            lead_pane_id,
            lead_terminal_id,
            lead_cwd: spec.cwd.clone(),
            spawned_at_unix_ms,
            binding: None,
            split_pane_confirmed: false,
            snapshot: ProjectionSnapshot::default(),
            node_keys: kvdag.nodes.iter().map(|node| node.key.clone()).collect(),
            closed: false,
            next_poll_at: None,
        });
    }

    /// Moves the run row off the `pending` `create_run` writes. The lead is
    /// live the moment its pane is, and no engine will move it now.
    pub(crate) fn mark_lead_run_running(&mut self, run_id: &RunId) {
        self.persist_workflow_write(StoreWrite::RunStatus {
            run: run_id.clone(),
            status: RunStatus::Running,
            ended_at_unix_ms: None,
        });
    }

    // ── projection ─────────────────────────────────────────────────────────

    /// One projection tick. Returns whether anything changed, so the caller
    /// knows whether to re-render.
    pub(crate) fn poll_run_projection(&mut self, now: Instant) -> bool {
        let Some(run) = self.workflow_lead.as_ref() else {
            return false;
        };
        if !run.poll_due(now) {
            return false;
        }
        if let Some(run) = self.workflow_lead.as_mut() {
            run.rearm(now);
        }

        // §3.3's lead-exit case. Checked here rather than off a pane-exit
        // event so every way a lead can vanish — closed pane, crash, server
        // shutdown reconciliation — lands on the same path, at most one poll
        // interval late.
        if self.lead_terminal_is_gone() {
            return self.lead_run_ended_without_finishing(crate::app::workflow::current_unix_ms());
        }

        let Ok(claude_dir) = crate::integration::claude_dir() else {
            return false;
        };

        let mut changed = self.bind_run_team(&claude_dir);
        changed |= self.absorb_run_projection(&claude_dir);
        changed
    }

    /// Recognises the team the lead created, once (§3.1 step 4).
    fn bind_run_team(&mut self, claude_dir: &Path) -> bool {
        let Some(run) = self.workflow_lead.as_ref() else {
            return false;
        };
        if run.binding.is_some() {
            return false;
        }
        let spawned_at = run.spawned_at_unix_ms;
        let lead_cwd = run.lead_cwd.clone();
        let run_id = run.run_id.clone();

        let teams = read_team_configs(&claude_dir.join("teams"));
        let own_panes = self.public_pane_ids_for_projection();
        let Some((binding, strength)) = lead::match_team(
            teams.iter().map(|(_, team)| team),
            spawned_at,
            &lead_cwd,
            &own_panes,
            &[],
        ) else {
            return false;
        };

        debug!(
            run = %run_id,
            team = %binding.team_name,
            session = %binding.lead_session_id,
            ?strength,
            "bound the run to its Claude Code team"
        );
        self.persist_workflow_write(StoreWrite::RunLeadBinding {
            run: run_id.clone(),
            lead_session_id: binding.lead_session_id.clone(),
            team_name: binding.team_name.clone(),
            lead_pane_id: Some(run.lead_pane_id.clone()),
            lead_terminal_id: Some(run.lead_terminal_id.to_string()),
            lead_prompt_version: crate::workflow::lead_prompt::LEAD_PROMPT_VERSION,
        });
        if let Some(run) = self.workflow_lead.as_mut() {
            run.binding = Some(binding);
        }
        if matches!(strength, MatchStrength::OwnPane) {
            if let Some(run) = self.workflow_lead.as_mut() {
                run.split_pane_confirmed = true;
            }
        }
        true
    }

    /// Reads the bound team's task list and member list, and records whatever
    /// changed since the last poll.
    fn absorb_run_projection(&mut self, claude_dir: &Path) -> bool {
        let Some(run) = self.workflow_lead.as_ref() else {
            return false;
        };
        let Some(binding) = run.binding.clone() else {
            return false;
        };
        let run_id = run.run_id.clone();
        let node_keys = run.node_keys.clone();

        let tasks = read_tasks(&claude_dir.join("tasks").join(&binding.team_name));
        let team = read_team_config(
            &claude_dir
                .join("teams")
                .join(&binding.team_name)
                .join("config.json"),
        );

        // §4's "in-process teammates don't resume" risk row: karvex forces
        // split-pane mode, and this is where that is *asserted* rather than
        // assumed. `members[0]` is always the in-process lead, so the check has
        // to look for a tmux-backed teammate specifically.
        if let Some(team) = team.as_ref() {
            if team.split_pane_confirmed() {
                if let Some(run) = self.workflow_lead.as_mut() {
                    if !run.split_pane_confirmed {
                        run.split_pane_confirmed = true;
                        debug!(run = %run_id, "split-pane teammate mode confirmed from the team config");
                    }
                }
            }
        }

        let delta = match self.workflow_lead.as_mut() {
            Some(run) => run.snapshot.absorb(&tasks, team.as_ref(), &node_keys),
            None => return false,
        };
        if delta.tasks.is_empty() && delta.members.is_empty() {
            return false;
        }

        let mut created_paths = Vec::new();
        for task in &delta.tasks {
            if task.emergent {
                created_paths.push(task.path.clone());
            }
            self.persist_workflow_write(StoreWrite::RunTaskProjected {
                run: run_id.clone(),
                path: task.path.clone(),
                node_key: task
                    .node_key
                    .clone()
                    .unwrap_or_else(|| NodeKey::new(task.task.id.clone())),
                task_id: task.task.id.clone(),
                subject: task.task.subject.clone(),
                owner: task.task.owner.clone().unwrap_or_default(),
                status: node_status_for(&task.task.status),
                emergent: task.emergent,
                blocked_by: task.blocked_by.clone(),
                observed_at_unix_ms: crate::app::workflow::current_unix_ms(),
            });
        }
        for member in &delta.members {
            self.persist_workflow_write(StoreWrite::RunMemberSnapshot {
                run: run_id.clone(),
                name: member.name.clone(),
                agent_type: member.agent_type.clone(),
                model: member.model.clone().unwrap_or_default(),
                pane_id: member.tmux_pane_id().map(str::to_string),
                backend_type: member.backend_type.clone(),
                is_active: member.is_active,
                cwd: member.cwd.clone(),
                observed_at_unix_ms: crate::app::workflow::current_unix_ms(),
            });
        }

        self.emit_projected_node_events(&run_id, &delta, &created_paths);
        true
    }

    /// Re-reads the rows the projection just wrote and publishes them as
    /// `workflow.node.created` / `workflow.node.updated`.
    ///
    /// Read back rather than synthesised: the row is the truth a client would
    /// get from `workflow.run.get`, and an event that disagreed with it would
    /// be worse than no event.
    fn emit_projected_node_events(
        &mut self,
        run_id: &RunId,
        delta: &projection::ProjectionDelta,
        created_paths: &[crate::workflow::model::InstancePath],
    ) {
        if delta.tasks.is_empty() {
            return;
        }
        let touched: Vec<crate::workflow::model::InstancePath> =
            delta.tasks.iter().map(|task| task.path.clone()).collect();
        let Some(nodes) = self.stored_run_nodes_for_events(run_id) else {
            return;
        };
        for node in nodes {
            let path = crate::workflow::model::InstancePath::new(node.path.clone());
            if !touched.contains(&path) {
                continue;
            }
            let kind = if created_paths.contains(&path) {
                EventKind::WorkflowNodeCreated
            } else {
                EventKind::WorkflowNodeUpdated
            };
            let data = if created_paths.contains(&path) {
                EventData::WorkflowNodeCreated {
                    run_id: run_id.to_string(),
                    node,
                }
            } else {
                EventData::WorkflowNodeUpdated {
                    run_id: run_id.to_string(),
                    node,
                }
            };
            self.emit_event(EventEnvelope { event: kind, data });
        }
    }

    // ── close-out ──────────────────────────────────────────────────────────

    /// The lead's self-report (§3.3). Authorised by possession of the run id,
    /// which the lead has because karvex put `KARVEX_WORKFLOW_RUN_ID` in its
    /// pane.
    pub(crate) fn finish_lead_run(&mut self, run_id: &RunId, ended_at_unix_ms: u64) {
        self.persist_workflow_write(StoreWrite::RunStatus {
            run: run_id.clone(),
            status: RunStatus::Succeeded,
            ended_at_unix_ms: Some(ended_at_unix_ms),
        });
        if let Some(run) = self.workflow_lead.as_mut() {
            if &run.run_id == run_id {
                run.closed = true;
            }
        }
    }

    /// Stores the lead's summary through the same `run_summary` path the
    /// retired summariser used, so a finished lead run reads back exactly like
    /// a finished engine run did.
    pub(crate) fn persist_lead_run_summary(
        &mut self,
        run_id: &RunId,
        version: &crate::workflow::model::KvdagVersionId,
        text: String,
        outcome: String,
    ) {
        let token_estimate = u32::try_from(text.len() / 4).unwrap_or(u32::MAX);
        self.persist_workflow_write(StoreWrite::RunSummary {
            run: run_id.clone(),
            kvdag_version: version.clone(),
            text,
            outcome,
            // The lead writes prose, not a structured report. Highlights, gaps,
            // and per-node lines were the old summariser's schema; leaving them
            // empty is honest, and the prose carries what they carried.
            highlights: Vec::new(),
            open_gaps: Vec::new(),
            per_node: Vec::new(),
            token_estimate,
            generated_by_path: None,
        });
    }

    /// Reads the stored summary back as its wire shape.
    #[cfg(feature = "workflow")]
    pub(crate) fn stored_run_summary_info(
        &mut self,
        run_id: &RunId,
    ) -> Option<crate::api::schema::WorkflowRunSummaryInfo> {
        let wanted = run_id.clone();
        match self
            .workflow_store
            .call(move |cx| cx.block_on(cx.store().get_run_summary(&wanted)))
        {
            Ok(Ok(Some(record))) => Some(crate::app::workflow::wire_run_summary_record(record)),
            Ok(Ok(None)) => None,
            Ok(Err(error)) => {
                warn!(%error, "the run summary could not be read back");
                None
            }
            Err(unavailable) => {
                warn!(
                    ?unavailable,
                    "the workflow store is unavailable; no run summary"
                );
                None
            }
        }
    }

    #[cfg(not(feature = "workflow"))]
    pub(crate) fn stored_run_summary_info(
        &mut self,
        _run_id: &RunId,
    ) -> Option<crate::api::schema::WorkflowRunSummaryInfo> {
        None
    }

    /// The lead's pane went away without a `finish` (§3.3). The run closes with
    /// whatever the projection last recorded — the task and member snapshots
    /// are already durable, which is what makes it resumable later (§3.7).
    ///
    /// Recorded as terminal `failed` carrying a structured reason rather than a
    /// new status: the wire's `WorkflowRunStatus` cannot gain a variant before
    /// the protocol bump, and a `failure` payload says more than a status word
    /// would anyway.
    pub(crate) fn lead_run_ended_without_finishing(&mut self, ended_at_unix_ms: u64) -> bool {
        let Some(run) = self.workflow_lead.as_ref() else {
            return false;
        };
        if run.closed {
            return false;
        }
        let run_id = run.run_id.clone();
        let team_name = run
            .binding
            .as_ref()
            .map(|binding| binding.team_name.clone());
        warn!(
            run = %run_id,
            team = ?team_name,
            "the run's team lead exited without reporting; closing the run with its last snapshot"
        );
        self.persist_workflow_write(StoreWrite::RunStatus {
            run: run_id,
            status: RunStatus::Failed,
            ended_at_unix_ms: Some(ended_at_unix_ms),
        });
        if let Some(run) = self.workflow_lead.as_mut() {
            run.closed = true;
        }
        true
    }

    // ── store and layout plumbing ──────────────────────────────────────────

    /// Hands one projection write to the store task.
    ///
    /// Direct rather than through the engine's `pending_writes` queue: the
    /// projection is not an engine effect and must not be dropped by that
    /// queue's budget, and a poll that produced a write has already decided
    /// the write is worth making.
    fn persist_workflow_write(&mut self, write: StoreWrite) {
        #[cfg(feature = "workflow")]
        {
            match self
                .workflow_store
                .call(move |cx| cx.block_on(cx.store().write(write)))
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    warn!(%error, "a run projection write was rejected by the store");
                }
                Err(unavailable) => {
                    warn!(
                        ?unavailable,
                        "the workflow store is unavailable; a projection write was lost"
                    );
                }
            }
        }
        #[cfg(not(feature = "workflow"))]
        let _ = write;
    }

    /// Reads back this run's `run_node` rows as wire records, for the event
    /// payloads.
    #[cfg(feature = "workflow")]
    fn stored_run_nodes_for_events(
        &mut self,
        run: &RunId,
    ) -> Option<Vec<crate::api::schema::WorkflowRunNodeInfo>> {
        let wanted = run.clone();
        let loaded = self.workflow_store.call(move |cx| {
            let nodes = cx.block_on(cx.store().list_run_nodes(&wanted))?;
            let limits = cx.block_on(cx.store().growth_limits(&wanted))?;
            Ok::<_, crate::workflow::store::StoreError>((nodes, limits))
        });
        match loaded {
            Ok(Ok((nodes, limits))) => Some(
                nodes
                    .into_iter()
                    .map(|node| crate::app::api::workflows::wire_run_node_record(node, &limits))
                    .collect(),
            ),
            Ok(Err(error)) => {
                warn!(%error, "the run's nodes could not be read back for a projection event");
                None
            }
            Err(unavailable) => {
                warn!(
                    ?unavailable,
                    "the workflow store is unavailable; a projection event is not emitted"
                );
                None
            }
        }
    }

    #[cfg(not(feature = "workflow"))]
    fn stored_run_nodes_for_events(
        &mut self,
        _run: &RunId,
    ) -> Option<Vec<crate::api::schema::WorkflowRunNodeInfo>> {
        None
    }

    /// Every public pane id this server currently knows about, for
    /// `match_team`'s strong rule: a team holding one of these panes is
    /// provably this karvex's.
    fn public_pane_ids_for_projection(&self) -> Vec<String> {
        let mut ids = Vec::new();
        for workspace in &self.state.workspaces {
            for tab in &workspace.tabs {
                for pane_id in tab.layout.pane_ids() {
                    let Some(number) = workspace.public_pane_number(pane_id) else {
                        continue;
                    };
                    ids.push(crate::workspace::public_pane_id_for_number(
                        &workspace.id,
                        number,
                    ));
                }
            }
        }
        ids
    }

    /// Whether the lead's terminal has left the layout.
    fn lead_terminal_is_gone(&self) -> bool {
        self.workflow_lead.as_ref().is_some_and(|run| {
            !run.closed && !self.state.terminals.contains_key(&run.lead_terminal_id)
        })
    }

    /// `workflow.run.cancel` for a lead run (§3.3).
    ///
    /// No task-level kill choreography: teammates belong to the lead, so
    /// closing the lead's pane is the whole cancellation. The run's snapshot
    /// stays exactly as the last poll left it.
    pub(crate) fn cancel_lead_run(&mut self, run_id: &RunId, ended_at_unix_ms: u64) -> bool {
        let Some(run) = self.workflow_lead.as_ref() else {
            return false;
        };
        if run.closed || &run.run_id != run_id {
            return false;
        }
        let lead_pane_id = run.lead_pane_id.clone();
        self.persist_workflow_write(StoreWrite::RunStatus {
            run: run_id.clone(),
            status: RunStatus::Cancelled,
            ended_at_unix_ms: Some(ended_at_unix_ms),
        });
        if let Some(run) = self.workflow_lead.as_mut() {
            run.closed = true;
        }
        let _ = self.runtime_pane_close("workflow.lead.close", lead_pane_id);
        true
    }

    /// Whether this run is the live lead run.
    pub(crate) fn is_live_lead_run(&self, run_id: &RunId) -> bool {
        self.workflow_lead
            .as_ref()
            .is_some_and(|run| !run.closed && &run.run_id == run_id)
    }
}

/// Claude Code's task status vocabulary → karvex's node status.
///
/// `Unknown` maps to `Pending` rather than failing: an unrecognised status from
/// an experimental upstream should leave the node visible and unstarted, not
/// break the poll.
fn node_status_for(status: &TaskStatus) -> NodeStatus {
    match status {
        TaskStatus::Pending => NodeStatus::Pending,
        TaskStatus::InProgress => NodeStatus::Running,
        TaskStatus::Completed => NodeStatus::Succeeded,
        TaskStatus::Unknown(_) => NodeStatus::Pending,
    }
}

/// Every `<n>.json` under a team's task directory, parse failures skipped.
///
/// Skipping rather than failing is deliberate and matches
/// `agent_session_registry`'s discipline for the same reason: these files are
/// written by a foreign process that is rewriting them concurrently, so a
/// half-written file is expected traffic, not an error.
fn read_tasks(dir: &Path) -> Vec<ObservedTask> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut tasks = Vec::new();
    for entry in entries.flatten().take(MAX_TASK_FILES) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        match projection::parse_task(&bytes) {
            Ok(task) => tasks.push(task),
            Err(error) => {
                debug!(path = %path.display(), %error, "skipping an unreadable task file")
            }
        }
    }
    tasks
}

fn read_team_config(path: &Path) -> Option<ObservedTeam> {
    let bytes = std::fs::read(path).ok()?;
    projection::parse_team_config(&bytes).ok()
}

/// Every readable team config, for the one-time binding hunt.
fn read_team_configs(dir: &Path) -> Vec<(String, ObservedTeam)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut teams = Vec::new();
    for entry in entries.flatten().take(MAX_TEAM_CONFIGS) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(team) = read_team_config(&entry.path().join("config.json")) else {
            continue;
        };
        teams.push((name, team));
    }
    teams
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_task_statuses_map_onto_node_statuses() {
        assert_eq!(node_status_for(&TaskStatus::Pending), NodeStatus::Pending);
        assert_eq!(
            node_status_for(&TaskStatus::InProgress),
            NodeStatus::Running
        );
        assert_eq!(
            node_status_for(&TaskStatus::Completed),
            NodeStatus::Succeeded
        );
    }

    #[test]
    fn an_unknown_upstream_status_leaves_the_node_pending_rather_than_failing() {
        assert_eq!(
            node_status_for(&TaskStatus::Unknown("triaged".to_string())),
            NodeStatus::Pending
        );
    }

    #[test]
    fn reading_a_missing_task_directory_is_empty_rather_than_an_error() {
        assert!(read_tasks(Path::new("/nonexistent/karvex/tasks")).is_empty());
        assert!(read_team_configs(Path::new("/nonexistent/karvex/teams")).is_empty());
        assert!(read_team_config(Path::new("/nonexistent/karvex/config.json")).is_none());
    }
}
