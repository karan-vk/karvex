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
use crate::workflow::binding::identity::{
    self, BindDecision, BindEvidence, BindInputs, IgnoredReason, ReportVerdict, RunExpectation,
    SessionEndpoint, SessionReport,
};
use crate::workflow::binding::lead::{self, LeadBinding, LeadSpawnError, LeadSpawnSpec};
use crate::workflow::binding::messaging::{self, MessagingSupport};
use crate::workflow::binding::spawn;
use crate::workflow::model::{Kvdag, NodeKey, NodeStatus, RunId, RunStatus, StoreWrite};
use crate::workflow::projection::{
    self, ObservedMemberIdentity, ObservedTask, ObservedTeam, ProjectionSnapshot, TaskStatus,
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

/// What the DAG calls the run's reserved `.lead` node.
///
/// The label a node shows is normally the author's; this one has no author, so
/// it says plainly what the node is rather than borrowing a workflow name.
const LEAD_NODE_LABEL: &str = "team lead";

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
    /// What the lead pane was started in — the cwd
    /// [`identity::match_team_window`] recognises the team by when the lead's
    /// own assertion never arrives.
    pub(crate) lead_cwd: PathBuf,
    /// The run's own directory, where its prompt, its `--settings`, and its
    /// prior-run context live. Held because the `.lead` node records it as the
    /// closest true answer to "where does this agent's work live" — the node
    /// directories that used to answer that went with the engine.
    pub(crate) run_dir: PathBuf,
    pub(crate) spawned_at_unix_ms: u64,
    /// The team, once recognised. `None` while the lead is still starting.
    pub(crate) binding: Option<LeadBinding>,
    /// How the team was recognised: the lead's own assertion, or one of the two
    /// fallback inferences. Recorded so a log line and the API can say which,
    /// and so a run bound by guesswork is visibly distinguishable from one bound
    /// by identity.
    pub(crate) bind_evidence: Option<BindEvidence>,
    /// The lead's own `SessionStart` self-report, once its hook has fired.
    /// Carries the messaging endpoint karvex steers the lead through.
    pub(crate) lead_endpoint: Option<SessionEndpoint>,
    /// Every other session of this run that has identified itself, by the
    /// karvex pane it runs in. Split-pane teammates inherit the lead's
    /// `--settings` and therefore the same hook, so they report themselves the
    /// same way; the pane id is what joins them to the team config's
    /// `tmuxPaneId` entries, which is where their *names* come from.
    ///
    /// Deliberately in memory rather than in the store: a messaging socket is
    /// bound by a live process and unlinked when it exits, so a persisted one
    /// would be a durable record of something that stopped being true.
    pub(crate) member_endpoints: std::collections::BTreeMap<String, SessionEndpoint>,
    /// Whether this machine can message the run's sessions at all, and why not
    /// when it cannot. Resolved once at launch from the version, the platform,
    /// and the documented kill switches.
    pub(crate) messaging: MessagingSupport,
    /// Whether a teammate has ever been observed on a tmux backend. Latched:
    /// a teammate that finishes and goes inactive does not un-prove that
    /// split-pane mode took.
    pub(crate) split_pane_confirmed: bool,
    /// What the projection has already recorded, so an unchanged poll writes
    /// nothing.
    pub(crate) snapshot: ProjectionSnapshot,
    /// The definition's node keys, for subject→node matching.
    pub(crate) node_keys: Vec<NodeKey>,
    /// Whether the lead has been handed its plan. Latched, so the plan is
    /// delivered exactly once no matter how many polls see the lead idle.
    pub(crate) seeded: bool,
    /// The instruction to deliver, held so the poller does not have to rebuild
    /// the run directory layout to say it.
    pub(crate) seed_prompt: String,
    /// Set once `workflow.run.finish` or lead-exit has closed the run, so a
    /// late poll cannot reopen it.
    pub(crate) closed: bool,
    /// The next `run_event.seq` this run will journal under.
    ///
    /// Per-run and monotonic, which is what the store's `(run, seq)` UNIQUE
    /// index requires. In memory because the live run is: karvex runs one lead
    /// at a time and nothing else writes this run's journal, so the counter
    /// cannot collide with a writer it does not know about.
    journal_seq: u64,
    next_poll_at: Option<Instant>,
}

impl LiveLeadRun {
    fn poll_due(&self, now: Instant) -> bool {
        !self.closed && self.next_poll_at.is_none_or(|due| now >= due)
    }

    fn rearm(&mut self, now: Instant) {
        self.next_poll_at = Some(now + RUN_PROJECTION_INTERVAL);
    }

    /// Takes the next journal sequence number for this run.
    pub(crate) fn next_journal_seq(&mut self) -> u64 {
        let seq = self.journal_seq;
        self.journal_seq = self.journal_seq.saturating_add(1);
        seq
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

    /// The second half of the launch preflight: Claude Code must already trust
    /// the directory the lead will open in, or the lead loses its plan to the
    /// folder-trust dialog (see `lead::cwd_is_trusted`).
    pub(crate) fn preflight_cwd_trust_for_lead(&self, cwd: &Path) -> Result<(), LeadSpawnError> {
        let config = crate::integration::claude_dir()
            .ok()
            .and_then(|dir| dir.parent().map(|home| home.join(".claude.json")))
            .and_then(|path| std::fs::read_to_string(path).ok());
        if lead::cwd_is_trusted(config.as_deref(), cwd) {
            Ok(())
        } else {
            Err(LeadSpawnError::CwdNotTrusted(
                cwd.to_string_lossy().into_owned(),
            ))
        }
    }

    /// The messaging preflight.
    ///
    /// Not fatal: a run whose sessions cannot be messaged still runs, and the
    /// user still steers it by clicking its pane. What this prevents is the
    /// silent case — upstream is explicit that a kill switch leaves messaging
    /// off with no visible difference — so the answer is recorded on the run and
    /// reported to clients instead of a message verb quietly doing nothing.
    pub(crate) fn preflight_messaging_for_lead(&self) -> MessagingSupport {
        let executable = crate::detect::interactive_agent_executable(crate::detect::Agent::Claude);
        let version = std::process::Command::new(executable)
            .arg("--version")
            .output()
            .ok()
            .and_then(|output| {
                lead::parse_claude_version(String::from_utf8_lossy(&output.stdout).trim())
            });
        let Some(version) = version else {
            // `preflight_claude_for_lead` already refused a `claude` whose
            // version cannot be read, so reaching here means the second read
            // failed transiently. Reporting "too old" would be a lie; the
            // launch has already been allowed, so report the honest floor.
            return MessagingSupport::ClaudeTooOld {
                found: "unknown".to_string(),
                required: "2.1.224".to_string(),
            };
        };
        // The pane inherits this process's environment, so this is exactly the
        // environment the lead's `claude` will see.
        let env: Vec<(String, String)> = messaging::MESSAGING_KILL_SWITCH_VARS
            .iter()
            .filter_map(|name| {
                std::env::var(name)
                    .ok()
                    .map(|value| ((*name).to_string(), value))
            })
            .collect();
        messaging::classify_support(
            version,
            messaging::MessagingPlatform::current(),
            env.iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
    }

    /// Writes the run directory, the rendered lead prompt, and the run-scoped
    /// Claude Code settings the lead and its teammates launch with
    /// (§3.1 step 2, §3.1a).
    pub(crate) fn write_lead_run_files(
        &self,
        spec: &LeadSpawnSpec,
        prompt: &str,
    ) -> Result<(), LeadSpawnError> {
        std::fs::create_dir_all(&spec.run_dir)
            .map_err(|error| LeadSpawnError::RunDirUnwritable(error.to_string()))?;
        std::fs::write(spec.prompt_path(), prompt)
            .map_err(|error| LeadSpawnError::RunDirUnwritable(error.to_string()))?;
        let hook_command = lead::identity_hook_command(&kvx_executable(), &spec.run_id);
        let settings = lead::lead_settings_document(&hook_command);
        std::fs::write(
            spec.settings_path(),
            serde_json::to_vec_pretty(&settings)
                .map_err(|error| LeadSpawnError::RunDirUnwritable(error.to_string()))?,
        )
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
    ///
    /// Two things happen here, and only one of them is in memory. The
    /// [`LiveLeadRun`] is the poller's own state, but the pane karvex just
    /// launched is a durable fact about the run — it is what says a Claude Code
    /// team lead is executing it, from the first instant rather than from
    /// whenever that lead gets round to identifying itself.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn bind_lead_run(
        &mut self,
        run_id: RunId,
        kvdag: &Kvdag,
        spec: &LeadSpawnSpec,
        lead_pane_id: String,
        lead_terminal_id: crate::terminal::TerminalId,
        spawned_at_unix_ms: u64,
        messaging: MessagingSupport,
    ) {
        self.workflow_lead = Some(LiveLeadRun {
            run_id: run_id.clone(),
            lead_pane_id: lead_pane_id.clone(),
            lead_terminal_id: lead_terminal_id.clone(),
            lead_cwd: spec.cwd.clone(),
            run_dir: spec.run_dir.clone(),
            spawned_at_unix_ms,
            binding: None,
            bind_evidence: None,
            lead_endpoint: None,
            member_endpoints: std::collections::BTreeMap::new(),
            messaging,
            split_pane_confirmed: false,
            snapshot: ProjectionSnapshot::default(),
            node_keys: kvdag.nodes.iter().map(|node| node.key.clone()).collect(),
            seeded: false,
            seed_prompt: lead::lead_seed_prompt(&spec.prompt_path()),
            closed: false,
            journal_seq: 0,
            next_poll_at: None,
        });
        self.persist_workflow_write(StoreWrite::RunLeadPane {
            run: run_id.clone(),
            lead_pane_id,
            lead_terminal_id: lead_terminal_id.to_string(),
            lead_prompt_version: crate::workflow::lead_prompt::LEAD_PROMPT_VERSION,
        });
        self.mint_lead_node(&run_id, spawned_at_unix_ms);
    }

    /// Mints the run's reserved `.lead` node (§3.3, D-9).
    ///
    /// Created here rather than on first observation because the lead is a fact
    /// about the run from the instant its pane exists — the same reasoning
    /// [`Self::bind_lead_run`] persists the pane on — and because every later
    /// write against `.lead` is an `UPDATE` that needs the row to already be
    /// there. `Running` from the start: karvex just launched it, and a node that
    /// claimed to be `pending` while a `claude` was live in a pane would be the
    /// kind of lie this whole rework exists to remove.
    fn mint_lead_node(&mut self, run_id: &RunId, started_at_unix_ms: u64) {
        self.persist_workflow_write(StoreWrite::RunNodeCreated {
            run: run_id.clone(),
            key: NodeKey::new(crate::workflow::model::LEAD_INSTANCE_PATH),
            path: crate::workflow::model::InstancePath::new(
                crate::workflow::model::LEAD_INSTANCE_PATH,
            ),
            label: LEAD_NODE_LABEL.to_string(),
            inputs: std::collections::BTreeMap::new(),
            parent: None,
            depth: 0,
            status: NodeStatus::Running,
            demand: crate::workflow::model::Demand::Standard,
            assignment: crate::workflow::tier::resolve(
                crate::workflow::tier::Tier::Auto,
                crate::workflow::model::Demand::Critical,
                None,
            ),
            assignment_reason: "the run's team lead".to_string(),
            attempt: 1,
            proposal_id: String::new(),
        });
        // `RunNodeCreated` has no `started_at`; the update that carries it is
        // also the one that will carry the lead's identity, so the row is
        // never left claiming it started at no particular time.
        self.write_lead_node(
            run_id,
            NodeStatus::Running,
            None,
            Some(started_at_unix_ms),
            None,
        );
    }

    /// Copies the lead's own session identity onto the `.lead` node.
    ///
    /// The same three facts `run_member` records for every other member, on the
    /// row an interview addresses. Written only when the identity actually
    /// carries a session id: `run_node`'s binding columns are all-or-nothing,
    /// and a binding with an empty session id would read back as a lead karvex
    /// had identified when it had not.
    fn record_lead_node_identity(
        &mut self,
        run_id: &RunId,
        identity: &crate::workflow::projection::MemberIdentity,
        observed_at_unix_ms: u64,
    ) {
        let Some(session_id) = identity.session_id.clone() else {
            return;
        };
        let Some(run) = self.workflow_lead.as_ref() else {
            return;
        };
        let binding = crate::workflow::model::NodeBinding {
            pane_id: crate::workflow::model::PublicPaneId::new(run.lead_pane_id.clone()),
            terminal_id: run.lead_terminal_id.clone(),
            agent_session_id: session_id,
            transcript_path: identity
                .transcript_path
                .clone()
                .map(PathBuf::from)
                .unwrap_or_default(),
            // The lead has no node directory: the node contract that owned that
            // idea was deleted with the engine. Its run directory is the
            // closest true answer and is where its prompt actually lives.
            node_dir: run.run_dir.clone(),
            cwd: run.lead_cwd.clone(),
        };
        let _ = observed_at_unix_ms;
        self.write_lead_node(run_id, NodeStatus::Running, Some(binding), None, None);
    }

    /// Settles the `.lead` node when the run closes.
    ///
    /// Without this the DAG would show a lead still running inside a run that
    /// ended — and the watchdog, which samples the lead, would keep finding a
    /// live node to have an opinion about. The status mirrors the run's own,
    /// because the lead *is* the run: karvex never decided anything else about
    /// how it ended.
    fn close_lead_node(&mut self, run_id: &RunId, status: RunStatus, ended_at_unix_ms: u64) {
        // Only the run this server launched has a `.lead` node — it is minted
        // at spawn. `workflow.run.finish` is authorised by possession of a run
        // id and nothing else, so it can name a stored run that was never live
        // here; settling a row that does not exist would be reported as a store
        // failure and would degrade persistence over a run that is simply not
        // this server's.
        if self
            .workflow_lead
            .as_ref()
            .is_none_or(|run| &run.run_id != run_id)
        {
            return;
        }
        let node_status = match status {
            RunStatus::Succeeded => NodeStatus::Succeeded,
            RunStatus::Cancelled => NodeStatus::Cancelled,
            _ => NodeStatus::Failed,
        };
        self.write_lead_node(run_id, node_status, None, None, Some(ended_at_unix_ms));
    }

    /// The one `StoreWrite::RunNode` builder for `.lead`, so the node's
    /// unwritten columns are unwritten in exactly one place.
    ///
    /// `usage` is zeroed on every write and that is correct rather than lazy:
    /// nothing samples the lead's tokens or tool uses (the transcript-delta
    /// sampler is a later packet), and the honest record of an unmeasured
    /// quantity is zero, not a number carried over from somewhere else.
    fn write_lead_node(
        &mut self,
        run_id: &RunId,
        status: NodeStatus,
        binding: Option<crate::workflow::model::NodeBinding>,
        started_at_unix_ms: Option<u64>,
        ended_at_unix_ms: Option<u64>,
    ) {
        self.persist_workflow_write(StoreWrite::RunNode {
            run: run_id.clone(),
            path: crate::workflow::model::InstancePath::new(
                crate::workflow::model::LEAD_INSTANCE_PATH,
            ),
            status,
            attempt: 1,
            binding,
            usage: crate::workflow::model::NodeUsage::default(),
            evidence: None,
            succession: None,
            started_at_unix_ms,
            ended_at_unix_ms,
            restored_from: None,
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

    /// One workflow tick.
    ///
    /// Two pollers with two different lifetimes hang off this one call, which is
    /// why it is split (`phase4-retarget-plan.md` §5 P8):
    ///
    /// * the lead-run projection, which only exists while a run is live in this
    ///   server, and
    /// * the review cycles, which are started over runs that have *already*
    ///   ended and must keep ticking with no live run at all.
    ///
    /// Returns whether anything changed, so the caller knows whether to
    /// re-render.
    pub(crate) fn poll_run_projection(&mut self, now: Instant) -> bool {
        let mut changed = self.poll_lead_run(now);
        // Deliberately outside the live-run gate: a review interviews a run
        // that is over, so gating it on `workflow_lead` would mean it never
        // ticked at all.
        changed |= self.poll_review_cycles(now);
        changed
    }

    /// The live lead run's own tick.
    fn poll_lead_run(&mut self, now: Instant) -> bool {
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
        changed |= self.seed_lead_if_ready();
        changed |= self.absorb_run_projection(&claude_dir);
        // Layered on this poll rather than given a timer of its own (§3.4): the
        // watchdog samples at `watchdog_tick_secs`, which is a multiple of this
        // cadence, and it needs exactly the state the projection just refreshed.
        changed |= self.poll_run_watchdog(now);
        changed
    }

    // ── identity ───────────────────────────────────────────────────────────

    /// Ingests one session's `SessionStart` self-report (§3.1a).
    ///
    /// The report is checked against two identifiers karvex minted itself — the
    /// run id it baked into the hook command, and the pane id it put in the
    /// pane's environment — so accepting one is not a guess about which team
    /// belongs to which run. Returns whether anything changed.
    pub(crate) fn record_run_session_report(&mut self, report: &SessionReport) -> bool {
        let Some(run) = self.workflow_lead.as_ref() else {
            debug!(run = %report.run_id, "a session self-report arrived with no live run");
            return false;
        };
        if run.closed {
            return false;
        }
        let expected = RunExpectation {
            run_id: run.run_id.to_string(),
            lead_pane_id: run.lead_pane_id.clone(),
        };
        let run_id = run.run_id.clone();
        match identity::classify_report(report, &expected) {
            ReportVerdict::Lead {
                endpoint,
                team_name,
            } => {
                debug!(
                    run = %run_id,
                    session = %endpoint.session_id,
                    team = %team_name,
                    addressable = endpoint.messaging_socket.is_some(),
                    "the run's team lead identified itself"
                );
                let Some(run) = self.workflow_lead.as_mut() else {
                    return false;
                };
                // A bound run keeps the team it bound to — a binding is
                // recorded once and never re-derived — so a lead that comes
                // back under a *different* session id would leave karvex
                // observing one team while messaging another. That is a state
                // nothing here can repair, and the one thing worse than it is
                // reaching it quietly.
                if let Some(binding) = run.binding.as_ref() {
                    if binding.lead_session_id != endpoint.session_id {
                        warn!(
                            run = %run_id,
                            bound_session = %binding.lead_session_id,
                            reported_session = %endpoint.session_id,
                            bound_team = %binding.team_name,
                            "the run's lead reported a different session id than the one the \
                             run is bound to; karvex keeps observing the bound team and will \
                             message the session that just reported"
                        );
                    }
                }
                let changed = run.lead_endpoint.as_ref() != Some(&endpoint);
                run.lead_endpoint = Some(endpoint);
                changed
            }
            ReportVerdict::Member { pane_id, endpoint } => {
                debug!(
                    run = %run_id,
                    pane = %pane_id,
                    session = %endpoint.session_id,
                    addressable = endpoint.messaging_socket.is_some(),
                    "a run teammate identified itself"
                );
                let Some(run) = self.workflow_lead.as_mut() else {
                    return false;
                };
                let changed = run.member_endpoints.get(&pane_id) != Some(&endpoint);
                run.member_endpoints.insert(pane_id, endpoint);
                changed
            }
            ReportVerdict::Ignored(reason) => {
                // Never silent: "the hook fired and nothing happened" is the
                // exact failure this path replaces.
                if matches!(reason, IgnoredReason::Subagent) {
                    debug!(run = %run_id, %reason, "ignoring a session self-report");
                } else {
                    warn!(run = %run_id, %reason, "ignoring a session self-report");
                }
                false
            }
        }
    }

    /// Recognises the run's team, once (§3.1 step 4, reworked by §3.1a).
    ///
    /// Assertion first, inference second, deadline last. The deadline is the
    /// half the audit found missing: an unbound run used to poll forever, stay
    /// `running`, and wedge the single-live-run guard for every later run.
    fn bind_run_team(&mut self, claude_dir: &Path) -> bool {
        let Some(run) = self.workflow_lead.as_ref() else {
            return false;
        };
        if run.binding.is_some() || run.closed {
            return false;
        }
        let spawned_at = run.spawned_at_unix_ms;
        let lead_cwd = run.lead_cwd.clone();
        let run_id = run.run_id.clone();
        let asserted = run.lead_endpoint.clone();

        let teams: Vec<ObservedTeam> = read_team_configs(&claude_dir.join("teams"))
            .into_iter()
            .map(|(_, team)| team)
            .collect();
        let own_panes = self.public_pane_ids_for_projection();
        let decision = identity::decide_binding(&BindInputs {
            asserted: asserted.as_ref(),
            teams: &teams,
            spawned_at_unix_ms: spawned_at,
            now_unix_ms: crate::app::workflow::current_unix_ms(),
            deadline: bind_deadline(),
            lead_cwd: &lead_cwd,
            own_pane_ids: &own_panes,
            bound_elsewhere: &[],
        });

        let (binding, evidence) = match decision {
            BindDecision::Bound { binding, evidence } => (binding, evidence),
            BindDecision::Waiting => return false,
            BindDecision::Expired { waited_ms } => {
                return self.lead_run_failed_unbound(waited_ms);
            }
        };

        if !evidence.is_asserted() {
            warn!(
                run = %run_id,
                team = %binding.team_name,
                evidence = evidence.as_str(),
                "the run's lead never identified itself; falling back to matching a team by \
                 spawn window and cwd"
            );
        } else {
            debug!(
                run = %run_id,
                team = %binding.team_name,
                session = %binding.lead_session_id,
                evidence = evidence.as_str(),
                "bound the run to its Claude Code team"
            );
        }
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
            run.bind_evidence = Some(evidence);
            if matches!(evidence, BindEvidence::InferredOwnPane) {
                run.split_pane_confirmed = true;
            }
        }
        true
    }

    /// The bind deadline passed with nothing recognised.
    ///
    /// Closes the run as terminal with an explicit reason rather than leaving it
    /// `running`. The lead's pane is deliberately left open: whatever went wrong
    /// is visible in it, and closing it would destroy the evidence.
    fn lead_run_failed_unbound(&mut self, waited_ms: u64) -> bool {
        let Some(run) = self.workflow_lead.as_ref() else {
            return false;
        };
        let run_id = run.run_id.clone();
        let reason = identity::unbound_failure_reason(waited_ms);
        let ended_at_unix_ms = crate::app::workflow::current_unix_ms();
        warn!(run = %run_id, waited_ms, "{reason}");
        self.persist_workflow_write(StoreWrite::RunFailed {
            run: run_id.clone(),
            ended_at_unix_ms,
            failure: serde_json::json!({
                "kind": "lead_unbound",
                "detail": reason,
                "waited_ms": waited_ms,
                "resumable": false,
            }),
        });
        self.close_lead_node(&run_id, RunStatus::Failed, ended_at_unix_ms);
        if let Some(run) = self.workflow_lead.as_mut() {
            run.closed = true;
        }
        true
    }

    // ── messaging ──────────────────────────────────────────────────────────

    /// Every session of the live run karvex can address, newest observation
    /// wins. The lead is always first.
    pub(crate) fn run_message_targets(&self) -> Vec<RunMessageTarget> {
        let Some(run) = self.workflow_lead.as_ref().filter(|run| !run.closed) else {
            return Vec::new();
        };
        let mut targets = Vec::new();
        if let Some(endpoint) = run.lead_endpoint.as_ref() {
            targets.push(RunMessageTarget {
                name: lead::LEAD_TARGET_NAME.to_string(),
                pane_id: Some(run.lead_pane_id.clone()),
                endpoint: endpoint.clone(),
            });
        }
        for (pane_id, endpoint) in &run.member_endpoints {
            targets.push(RunMessageTarget {
                // The team config is where a teammate's *name* lives; the
                // endpoint is keyed by pane. Joining them here keeps the
                // addressing vocabulary the same one `workflow.run.get`
                // already publishes.
                name: self
                    .member_name_for_pane(pane_id)
                    .unwrap_or_else(|| pane_id.clone()),
                pane_id: Some(pane_id.clone()),
                endpoint: endpoint.clone(),
            });
        }
        targets
    }

    /// Sends one message into a run session's documented inbox socket.
    ///
    /// Delivery is the receiving session's decision, not karvex's: upstream's
    /// inbound controls can deliver, hold, or refuse, and the socket write
    /// succeeding only means the frame was accepted for that decision. The
    /// answer here says exactly that, rather than claiming the message was read.
    pub(crate) fn message_run_session(
        &mut self,
        target_name: &str,
        text: &str,
        priority: messaging::Priority,
    ) -> Result<RunMessageReceipt, RunMessageError> {
        let Some(run) = self.workflow_lead.as_ref().filter(|run| !run.closed) else {
            return Err(RunMessageError::NoLiveRun);
        };
        // Only the two checkable facts refuse. A *suspected* kill switch never
        // does: probed live, the same variable that kills messaging on an
        // account with no cached feature flags changes nothing on one that has
        // them, so refusing on the suspicion would break messaging on every
        // machine that merely exports `DO_NOT_TRACK`.
        if run.messaging.blocks_messaging() {
            return Err(RunMessageError::Unsupported(run.messaging.clone()));
        }
        let run_id = run.run_id.clone();
        let targets = self.run_message_targets();
        if targets.is_empty() {
            return Err(RunMessageError::NoAddressableSessions);
        }
        let Some(target) = targets
            .iter()
            .find(|target| target.name.eq_ignore_ascii_case(target_name))
            .cloned()
        else {
            return Err(RunMessageError::UnknownTarget {
                requested: target_name.to_string(),
                known: targets.iter().map(|target| target.name.clone()).collect(),
            });
        };

        let channel = match target.endpoint.messaging_socket.as_deref() {
            Some(socket) => {
                let envelope = messaging::Envelope {
                    session_id: target.endpoint.session_id.clone(),
                    from: messaging::sender_name(&run_id),
                    priority,
                    text: text.to_string(),
                };
                let frames = messaging::encode_message(
                    &envelope,
                    target.endpoint.messaging_token.as_deref(),
                )
                .map_err(|error| RunMessageError::BadMessage(error.to_string()))?;
                write_inbox_frames(Path::new(socket), &frames)
                    .map_err(|error| RunMessageError::WriteFailed(error.to_string()))?;
                messaging::DeliveryChannel::InboxSocket
            }
            // No socket, but karvex owns the pane. Typing into it is how karvex
            // steers every other agent, and it is the only channel left after a
            // server restart: a teammate's token exists nowhere but that
            // teammate's own hook environment, so an in-memory endpoint lost to
            // a restart cannot be recovered from disk.
            None => {
                let Some(pane_id) = target.pane_id.clone() else {
                    return Err(RunMessageError::TargetNotAddressable {
                        name: target.name.clone(),
                    });
                };
                let response = self.dispatch_runtime_mutation(
                    "workflow.run.message",
                    crate::api::schema::Method::AgentPrompt(
                        crate::api::schema::AgentPromptParams {
                            target: pane_id,
                            text: text.to_string(),
                            wait: None,
                        },
                    ),
                );
                if let Ok(error) =
                    serde_json::from_str::<crate::api::schema::ErrorResponse>(&response)
                {
                    return Err(RunMessageError::WriteFailed(error.error.message));
                }
                messaging::DeliveryChannel::PaneInput
            }
        };

        debug!(
            run = %run_id,
            target = %target.name,
            session = %target.endpoint.session_id,
            channel = channel.as_str(),
            "handed a message to a run session"
        );
        self.journal_run_message(&run_id, &target, channel, priority, text);
        Ok(RunMessageReceipt {
            target: target.name.clone(),
            session_id: target.endpoint.session_id.clone(),
            pane_id: target.pane_id.clone(),
            channel,
        })
    }

    /// Records one handed-over message in the run's journal.
    ///
    /// Load-bearing rather than decorative, for a reason the messaging spike
    /// pinned down: the two channels are not equivalent, and which one carried
    /// a message is a fact that cannot be reconstructed afterwards. A
    /// teammate's `CLAUDE_CODE_MESSAGING_TOKEN` exists only in that teammate's
    /// own hook environment — teammates never register in `~/.claude/sessions`,
    /// so there is no key file to read it back from — which means every
    /// endpoint this server holds dies with this server, and a run that
    /// outlives a karvex restart can only ever be reached by pane input. A
    /// reader of the journal has to be able to tell "Claude Code delivered this
    /// as a peer message, subject to the receiver's inbound controls" from
    /// "karvex typed this into a terminal, indistinguishable from the user".
    ///
    /// The text is deliberately *not* journalled — only its size. The journal
    /// is a record of what karvex did, the transcript is the record of what was
    /// said, and copying steering prose into the store would put user content
    /// in a place nothing prunes on content grounds.
    fn journal_run_message(
        &mut self,
        run_id: &RunId,
        target: &RunMessageTarget,
        channel: messaging::DeliveryChannel,
        priority: messaging::Priority,
        text: &str,
    ) {
        let seq = match self.workflow_lead.as_mut() {
            Some(run) => run.next_journal_seq(),
            None => return,
        };
        self.persist_workflow_write(StoreWrite::RunEvent {
            run: run_id.clone(),
            seq,
            kind: crate::workflow::model::RunEventKind::MessageDelivered,
            // A run session is addressed by name, not by instance path: a
            // teammate works on whatever tasks the lead gave it, and pinning
            // the message to one node would invent a link karvex did not make.
            path: None,
            payload: serde_json::json!({
                "target": target.name,
                "session_id": target.endpoint.session_id,
                "pane_id": target.pane_id,
                "channel": channel.as_str(),
                "priority": priority.as_str(),
                "text_bytes": text.len(),
            }),
            at_unix_ms: crate::app::workflow::current_unix_ms(),
        });
    }

    /// The `kind: "task"` journal entry for a reassignment (§3.4's "claimed",
    /// WI-R6 in `phase4-retarget-plan.md`'s amendment log): which task, from
    /// whom, to whom, when. Karvex can see that ownership moved; it cannot see
    /// why the lead did it, and the interview prompt already says so — so
    /// nothing beyond these four facts is recorded.
    fn journal_task_owner_change(
        &mut self,
        run_id: &RunId,
        path: &crate::workflow::model::InstancePath,
        owner_change: &crate::workflow::projection::ObservedOwnerChange,
        observed_at_unix_ms: u64,
    ) {
        let seq = match self.workflow_lead.as_mut() {
            Some(run) => run.next_journal_seq(),
            None => return,
        };
        self.persist_workflow_write(StoreWrite::RunEvent {
            run: run_id.clone(),
            seq,
            kind: crate::workflow::model::RunEventKind::Task,
            path: Some(path.clone()),
            payload: serde_json::json!({
                "owner_change": {
                    "from": owner_change.from,
                    "to": owner_change.to,
                },
            }),
            at_unix_ms: observed_at_unix_ms,
        });
    }

    /// What the live run made of a session that just reported itself, read back
    /// from the run rather than re-derived, so a response cannot claim a role
    /// the server did not record.
    pub(crate) fn run_session_report_outcome(
        &self,
        session_id: &str,
    ) -> (
        crate::api::schema::WorkflowSessionRole,
        Option<String>,
        bool,
    ) {
        use crate::api::schema::WorkflowSessionRole;

        let Some(run) = self.workflow_lead.as_ref() else {
            return (WorkflowSessionRole::Ignored, None, false);
        };
        if let Some(endpoint) = run
            .lead_endpoint
            .as_ref()
            .filter(|endpoint| endpoint.session_id == session_id)
        {
            return (
                WorkflowSessionRole::Lead,
                identity::team_name_for_session(&endpoint.session_id),
                endpoint.messaging_socket.is_some(),
            );
        }
        if let Some(endpoint) = run
            .member_endpoints
            .values()
            .find(|endpoint| endpoint.session_id == session_id)
        {
            return (
                WorkflowSessionRole::Member,
                None,
                endpoint.messaging_socket.is_some(),
            );
        }
        (WorkflowSessionRole::Ignored, None, false)
    }

    /// The messaging half of `workflow.run.get`, for the run live on this
    /// server. `None` for any other run: an inbox socket belongs to a running
    /// process, so reporting one for a stored run would be a durable record of
    /// something that stopped being true.
    pub(crate) fn run_messaging_info(
        &self,
        run_id: &RunId,
    ) -> Option<crate::api::schema::WorkflowRunMessagingInfo> {
        let run = self
            .workflow_lead
            .as_ref()
            .filter(|run| !run.closed && &run.run_id == run_id)?;
        let supported = run.messaging.is_available();
        // `reason`/`detail` are keyed off `Available`, not off `supported`. A
        // suspected kill switch *is* supported — karvex tries anyway, because
        // the variable provably does nothing on an account whose Claude Code
        // feature flags are cached — but it is still the one thing about this
        // run's messaging a user might need to know, and gating the words on
        // `supported` meant the only unverifiable case was also the only silent
        // one.
        let noteworthy = !matches!(run.messaging, MessagingSupport::Available);
        Some(crate::api::schema::WorkflowRunMessagingInfo {
            supported,
            reason: noteworthy.then(|| run.messaging.code().to_string()),
            detail: noteworthy.then(|| run.messaging.to_string()),
            targets: self
                .run_message_targets()
                .into_iter()
                .map(|target| {
                    let addressable = target.endpoint.messaging_socket.is_some();
                    crate::api::schema::WorkflowRunMessageTargetInfo {
                        name: target.name,
                        session_id: target.endpoint.session_id,
                        channel: if addressable {
                            messaging::DeliveryChannel::InboxSocket
                        } else {
                            messaging::DeliveryChannel::PaneInput
                        }
                        .as_str()
                        .to_string(),
                        pane_id: target.pane_id,
                        addressable,
                    }
                })
                .collect(),
        })
    }

    // ── member identity (§3.3) ─────────────────────────────────────────────

    /// What karvex knows about each team member's own Claude Code session,
    /// keyed by the name the team config gave it.
    ///
    /// This is the packet's whole reason for existing. Claude Code's team state
    /// carries `agentId` (`<name>@<team>`) and `leadSessionId`, and nothing else
    /// that identifies a teammate's session (`phase4-retarget-plan.md` §1.4c) —
    /// so a review cycle run tomorrow, over panes that closed today, would have
    /// nothing to resume. Karvex already *holds* the missing fact: spike S1
    /// proved the bundled `SessionStart` hook fires inside every split-pane
    /// teammate and lands its session id on that teammate's karvex pane, and
    /// that the team config's `tmuxPaneId` **is** karvex's own public pane id.
    /// All this does is copy it somewhere durable.
    ///
    /// The ladder, per member, is S1's — not the plan's, which S1 corrected:
    ///
    /// 1. the run-scoped `--settings` self-report karvex already collects
    ///    (`member_endpoints`, keyed by pane), which is the only source that
    ///    also carries Claude Code's own `transcript_path`;
    /// 2. the bundled hook's report, already on the pane's terminal as
    ///    `persisted_agent_session` — the source that works with no new code;
    /// 3. for the **lead only**, the team config's `leadSessionId`.
    ///
    /// The plan's fourth rung — `~/.claude/sessions/<pid>.json` — is *not* here:
    /// S1 sampled a live teammate 164 times over its whole life and it never
    /// registered, so that registry is lead-only, and for the lead the team
    /// config already answers with no IO at all. Nor is "the newest transcript
    /// under the cwd's project slug": lead and teammates share one project slug,
    /// so it cannot tell two teammates apart. A member this resolves nothing for
    /// records `None` and is `evidence_only` in a review — which is honest, and
    /// visible on the wire.
    fn member_identities(
        &self,
        claude_dir: &Path,
        team: &ObservedTeam,
    ) -> std::collections::BTreeMap<String, ObservedMemberIdentity> {
        let mut identities = std::collections::BTreeMap::new();
        let Some(run) = self.workflow_lead.as_ref() else {
            return identities;
        };
        for member in &team.members {
            let (pane_id, reported) = if member.is_lead() {
                // The lead has no pane in the *team's* accounting (its
                // `tmuxPaneId` is the `"leader"` sentinel), but it very much has
                // one in karvex's: the pane karvex launched it into.
                (Some(run.lead_pane_id.as_str()), run.lead_endpoint.as_ref())
            } else {
                match member.tmux_pane_id() {
                    Some(pane) => (Some(pane), run.member_endpoints.get(pane)),
                    // An in-process teammate is a session inside the lead's
                    // process with no pane of its own, so karvex can observe
                    // nothing about it. Recorded as unresolved rather than
                    // guessed at.
                    None => (None, None),
                }
            };
            let session_id = reported
                .map(|endpoint| endpoint.session_id.clone())
                .or_else(|| pane_id.and_then(|pane| self.pane_claude_session_id(pane)))
                .or_else(|| {
                    member
                        .is_lead()
                        .then(|| run.binding.as_ref().map(|b| b.lead_session_id.clone()))
                        .flatten()
                });
            let cwd = member.cwd.clone().or_else(|| {
                member
                    .is_lead()
                    .then(|| run.lead_cwd.to_string_lossy().into_owned())
            });
            let transcript_path = reported
                .and_then(|endpoint| endpoint.transcript_path.clone())
                .or_else(|| {
                    derived_transcript_path(claude_dir, cwd.as_deref(), session_id.as_deref())
                });
            let last_state = pane_id
                .and_then(|pane| self.pane_member_state(pane))
                .map(|state| state.as_str().to_string());
            identities.insert(
                member.name.clone(),
                ObservedMemberIdentity {
                    session_id,
                    transcript_path,
                    last_state,
                },
            );
        }
        identities
    }

    /// The claude session id karvex's own per-pane hook already recorded for a
    /// public pane id.
    ///
    /// The bundled `SessionStart` hook posts it, `agent_resume` validates it,
    /// and the pane's terminal has held it since long before this packet
    /// (`terminal/state.rs`, `phase4-retarget-plan.md` §1.3). Only a claude
    /// session counts: a pane that has been re-purposed for another agent must
    /// not lend its id to a Claude Code teammate.
    fn pane_claude_session_id(&self, pane_id: &str) -> Option<String> {
        let session = self
            .terminal_for_public_pane(pane_id)?
            .persisted_agent_session
            .as_ref()?;
        (session.agent == "claude"
            && session.session_ref.kind == crate::agent_resume::AgentSessionRefKind::Id)
            .then(|| session.session_ref.value.clone())
    }

    /// What karvex's own detection says the member's pane is doing, in the
    /// vocabulary `workflow.run.get` publishes.
    ///
    /// The same read the DAG overlay already does (`ui/workflow_dag.rs`), moved
    /// behind the server boundary so it is recorded rather than only rendered:
    /// a pane's state is gone the moment the pane is, and "this teammate sat
    /// idle for forty minutes while its task said `in_progress`" is the single
    /// most useful thing a review can put to it.
    fn pane_member_state(&self, pane_id: &str) -> Option<crate::api::schema::WorkflowMemberState> {
        let terminal = self.terminal_for_public_pane(pane_id)?;
        Some(match terminal.state {
            crate::detect::AgentState::Working => crate::api::schema::WorkflowMemberState::Working,
            crate::detect::AgentState::Idle => crate::api::schema::WorkflowMemberState::Idle,
            crate::detect::AgentState::Blocked => {
                crate::api::schema::WorkflowMemberState::NeedsInput
            }
            crate::detect::AgentState::Unknown => crate::api::schema::WorkflowMemberState::Unknown,
        })
    }

    /// The terminal behind a **public** pane id — the id Claude Code hands back
    /// through `tmuxPaneId`. `None` once the pane is gone, which is the normal
    /// answer for a finished run.
    fn terminal_for_public_pane(&self, pane_id: &str) -> Option<&crate::terminal::TerminalState> {
        let (ws_idx, pane) = self.parse_current_public_pane_id(pane_id)?;
        let attached = self
            .state
            .workspaces
            .get(ws_idx)?
            .pane_state(pane)?
            .attached_terminal_id
            .clone();
        self.state.terminals.get(&attached)
    }

    /// The team-config name of whichever member occupies this pane.
    fn member_name_for_pane(&self, pane_id: &str) -> Option<String> {
        self.workflow_lead
            .as_ref()?
            .snapshot
            .member_name_for_pane(pane_id)
            .map(str::to_string)
    }

    /// Hands the lead its plan, once.
    ///
    /// Gated on the team config existing rather than on a timer: Claude Code
    /// writes it as the session comes up, so a bound run is a running `claude`.
    /// The delivery goes through `agent.prompt`, the same path every other
    /// karvex agent is steered by, which means a failure to deliver surfaces as
    /// a delivery failure instead of as a lead that quietly sits idle.
    fn seed_lead_if_ready(&mut self) -> bool {
        let Some(run) = self.workflow_lead.as_ref() else {
            return false;
        };
        if run.seeded || run.closed || run.binding.is_none() {
            return false;
        }
        let ready = self
            .state
            .terminals
            .get(&run.lead_terminal_id)
            .is_some_and(|terminal| terminal.managed_agent_interactive_ready());
        if !ready {
            return false;
        }
        let pane_id = run.lead_pane_id.clone();
        let text = run.seed_prompt.clone();
        let run_id = run.run_id.clone();
        if let Some(run) = self.workflow_lead.as_mut() {
            run.seeded = true;
        }
        debug!(run = %run_id, pane = %pane_id, "handing the team lead its plan");
        let _ = self.dispatch_runtime_mutation(
            "workflow.lead.seed",
            crate::api::schema::Method::AgentPrompt(crate::api::schema::AgentPromptParams {
                target: pane_id,
                text,
                wait: None,
            }),
        );
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

        // Resolved before the fold, because a member whose session id has just
        // arrived is a member whose durable record changed even though the team
        // config's bytes did not — and the snapshot is the only thing that can
        // tell those two apart cheaply enough to keep an unchanged poll silent.
        let identities = match team.as_ref() {
            Some(team) => self.member_identities(claude_dir, team),
            None => std::collections::BTreeMap::new(),
        };
        let observed_at_unix_ms = crate::app::workflow::current_unix_ms();
        let delta = match self.workflow_lead.as_mut() {
            Some(run) => run.snapshot.absorb(
                &tasks,
                team.as_ref(),
                &node_keys,
                &identities,
                observed_at_unix_ms,
            ),
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
            let observed_at_unix_ms = crate::app::workflow::current_unix_ms();
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
                observed_at_unix_ms,
            });
            if let Some(owner_change) = &task.owner_change {
                self.journal_task_owner_change(
                    &run_id,
                    &task.path,
                    owner_change,
                    observed_at_unix_ms,
                );
            }
        }
        for projected in &delta.members {
            let member = &projected.member;
            self.persist_workflow_write(StoreWrite::RunMemberSnapshot {
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
                observed_at_unix_ms,
            });
            // The lead is a member like any other in the team config, and the
            // `.lead` node is the run-node face of that same row: it is what
            // gives `interrogation.run_node` — a *required* link — something to
            // point at when the lead itself is interviewed (D-9).
            if member.is_lead() {
                self.record_lead_node_identity(&run_id, &projected.identity, observed_at_unix_ms);
            }
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
        self.close_lead_node(run_id, RunStatus::Succeeded, ended_at_unix_ms);
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
        self.persist_workflow_write(StoreWrite::RunFailed {
            run: run_id.clone(),
            ended_at_unix_ms,
            failure: serde_json::json!({
                "kind": "lead_exited",
                "detail": "the run's team lead exited without calling `kvx workflow run finish`",
                "resumable": true,
                "team_name": team_name,
            }),
        });
        self.close_lead_node(&run_id, RunStatus::Failed, ended_at_unix_ms);
        if let Some(run) = self.workflow_lead.as_mut() {
            run.closed = true;
        }
        // Terminal, so it is a `run.finished` like any other close. Nothing
        // asked for this one, which is exactly why it has to be announced: the
        // run browser and every subscriber would otherwise show it as live
        // until something else happened to move them.
        if let Some(run) = self.stored_run_info_for_events(&run_id) {
            self.emit_workflow_run_event(
                EventKind::WorkflowRunFinished,
                EventData::WorkflowRunFinished { run },
            );
        }
        true
    }

    /// The run's wire record, for a lead-path event payload.
    ///
    /// Read back rather than synthesised, for the same reason
    /// [`Self::stored_run_nodes_for_events`] is: the row is what
    /// `workflow.run.get` would answer, and an event that disagreed with the
    /// record would be worse than no event.
    #[cfg(feature = "workflow")]
    fn stored_run_info_for_events(
        &mut self,
        run: &RunId,
    ) -> Option<crate::api::schema::WorkflowRunInfo> {
        let wanted = run.clone();
        let loaded = self.workflow_store.call(move |cx| {
            let record = cx.block_on(cx.store().get_run(&wanted))?;
            let limits = cx.block_on(cx.store().growth_limits(&wanted))?;
            Ok::<_, crate::workflow::store::StoreError>((record, limits))
        });
        match loaded {
            Ok(Ok((Some(record), limits))) => {
                Some(crate::app::api::workflows::wire_run_record(record, &limits))
            }
            Ok(Ok((None, _))) => None,
            Ok(Err(error)) => {
                warn!(%error, "the closed run could not be read back for its event");
                None
            }
            Err(unavailable) => {
                warn!(
                    ?unavailable,
                    "the workflow store is unavailable; no run event"
                );
                None
            }
        }
    }

    #[cfg(not(feature = "workflow"))]
    fn stored_run_info_for_events(
        &mut self,
        _run: &RunId,
    ) -> Option<crate::api::schema::WorkflowRunInfo> {
        None
    }

    // ── store and layout plumbing ──────────────────────────────────────────

    /// Hands one projection write to the store task.
    ///
    /// Direct rather than through the engine's `pending_writes` queue: the
    /// projection is not an engine effect and must not be dropped by that
    /// queue's budget, and a poll that produced a write has already decided
    /// the write is worth making.
    pub(crate) fn persist_workflow_write(&mut self, write: StoreWrite) {
        #[cfg(feature = "workflow")]
        {
            match self
                .workflow_store
                .call(move |cx| cx.block_on(cx.store().write(write)))
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    warn!(%error, "a run projection write was rejected by the store");
                    self.mark_workflow_persistence_degraded();
                }
                Err(unavailable) => {
                    warn!(
                        ?unavailable,
                        "the workflow store is unavailable; a projection write was lost"
                    );
                    self.mark_workflow_persistence_degraded();
                }
            }
        }
        #[cfg(not(feature = "workflow"))]
        let _ = write;
    }

    /// Reads back this run's `run_node` rows as wire records, for the event
    /// payloads.
    #[cfg(feature = "workflow")]
    pub(crate) fn stored_run_nodes_for_events(
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
    pub(crate) fn stored_run_nodes_for_events(
        &mut self,
        _run: &RunId,
    ) -> Option<Vec<crate::api::schema::WorkflowRunNodeInfo>> {
        None
    }

    /// Every public pane id this server currently knows about, for
    /// [`identity::match_team_window`]'s strong rule: a team holding one of these panes is
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
        self.close_lead_node(run_id, RunStatus::Cancelled, ended_at_unix_ms);
        if let Some(run) = self.workflow_lead.as_mut() {
            run.closed = true;
        }
        let _ = self.runtime_pane_close("workflow.lead.close", lead_pane_id);
        true
    }

    /// Test-only: the state `workflow.run` leaves behind once the lead's pane
    /// exists — without the pane.
    ///
    /// `workflow.run` execs a real `claude` and preflights its version (§3.1
    /// step 3), so no in-crate test can drive that handler end to end;
    /// `tests/workflow_lead_headless.rs` owns the launch with a stub on `PATH`.
    /// Everything the handlers and the overlays do *around* a live run still
    /// has to be testable, so this performs exactly the two steps the handler
    /// takes once the pane is up — `create_run` and [`Self::bind_lead_run`] —
    /// and nothing else. It is the replacement for the engine-era fixtures that
    /// bound a node and minted it a result token.
    #[cfg(all(test, feature = "workflow"))]
    pub(crate) fn test_bind_a_live_lead_run(
        &mut self,
        workflow_id: &str,
        workflow_name: &str,
    ) -> RunId {
        use crate::workflow::model::{Demand, WorkflowId};
        use crate::workflow::store::NewRun;
        use crate::workflow::tier::{resolve_assignments, HistoryIndex, Tier};

        let wanted = WorkflowId::new(workflow_id.to_string());
        let (version_id, kvdag) = self
            .workflow_store
            .call(move |cx| {
                let summary = cx
                    .block_on(cx.store().get_workflow(&wanted))?
                    .expect("the workflow row exists");
                let version_id = summary
                    .head_version
                    .expect("a created workflow has a head version");
                let kvdag = cx.block_on(cx.store().load_version(&version_id))?;
                Ok::<_, crate::workflow::store::StoreError>((version_id, kvdag))
            })
            .expect("the in-memory store is available")
            .expect("the head version loads");

        let started_at_unix_ms = crate::app::workflow::current_unix_ms();
        let assignments = resolve_assignments(&kvdag, Tier::Auto, &HistoryIndex::new());
        let run_id = self
            .workflow_store
            .call({
                let workflow = kvdag.workflow_id.clone();
                let growth = kvdag.growth;
                move |cx| {
                    cx.block_on(cx.store().create_run(NewRun {
                        workflow,
                        version: version_id,
                        tier: Tier::Auto,
                        args: std::collections::BTreeMap::new(),
                        growth,
                        started_at_unix_ms,
                        assignments,
                        context_runs: Vec::new(),
                        workspace_id: None,
                        restore_from: None,
                        restored: Vec::new(),
                    }))
                }
            })
            .expect("the in-memory store is available")
            .expect("the run row is created");

        let spec = LeadSpawnSpec {
            run_id: run_id.clone(),
            workflow_name: workflow_name.to_string(),
            run_dir: PathBuf::from("/runs").join(workflow_name),
            cwd: PathBuf::from("/repo"),
            assignment: crate::workflow::tier::resolve(Tier::Auto, Demand::Critical, None),
        };
        self.bind_lead_run(
            run_id.clone(),
            &kvdag,
            &spec,
            "w1:p1".to_string(),
            crate::terminal::TerminalId::alloc(),
            started_at_unix_ms,
            // The preflight execs `claude --version`, which a unit test has no
            // business doing; a live run whose machine can message its sessions
            // is the ordinary case this fixture stands in for.
            MessagingSupport::Available,
        );
        self.mark_lead_run_running(&run_id);
        run_id
    }

    /// Publishes one **run-level** workflow event and refreshes any surface
    /// that lists runs.
    ///
    /// The engine's `emit_workflow_event` used to be the single funnel for
    /// this: it published the wire envelope and, for the four run-level kinds,
    /// re-read the run browser's rows (`07-phase3-plan.md` §A7). The engine
    /// went and took the funnel with it, leaving `workflow.run.started` and
    /// `workflow.run.updated` with no producer at all and the browser stale for
    /// the whole life of a run. This is the replacement, and it is deliberately
    /// the *only* way a lead run publishes a run-level event, so the two can
    /// never drift apart again.
    ///
    /// Node-level events do not come through here on purpose: several per node
    /// per run, and none of them can move a row in the run list.
    ///
    /// Also the one funnel every `workflow.review.*` event passes through
    /// (`emit_review_event`/`emit_review_closed`), which is what makes it the
    /// right seam for `App::refresh_open_dag_review`
    /// (`.local/prd/phase4-retarget-plan.md` §5 packet P13's contract:
    /// "refresh only on `workflow.review.*` ... events") — a review cycle
    /// runs on a run that has already stopped polling, so nothing else
    /// would ever tell an open DAG snapshot its review just moved.
    pub(crate) fn emit_workflow_run_event(&mut self, kind: EventKind, data: EventData) {
        let review_run_id = review_event_run_id(kind, &data);
        self.emit_event(EventEnvelope { event: kind, data });
        self.refresh_workflow_runs_overlay(kind);
        if let Some(run_id) = review_run_id {
            self.refresh_open_dag_review(&run_id);
        }
    }

    /// Opens the DAG overlay on the run a team lead is executing right now.
    ///
    /// The same path the run browser's `Enter` takes: a lead run has no
    /// in-memory graph to mirror (`AppState::set_workflow_run_graph`), so the
    /// overlay is loaded from the store through `workflow.run.get` and kept
    /// current by `reload_open_lead_run` on every projection tick. Returns
    /// whether it opened, so the caller can fall through to whatever it did
    /// before.
    ///
    /// Phase C re-pointed the run browser at this and left the *other* entry
    /// points asking the engine's mirror, which nothing writes any more — so
    /// `keys.open_workflow_dag` fell through to the launcher and the launcher
    /// closed itself instead of showing the run it had just started.
    pub(crate) fn open_workflow_dag_on_the_live_run(&mut self) -> bool {
        let Some(run_id) = self.live_lead_run_id() else {
            return false;
        };
        match self.load_historical_run(&run_id) {
            Ok(()) => true,
            Err(error) => {
                tracing::debug!(%error, run = %run_id, "could not open the live run in the DAG");
                false
            }
        }
    }

    /// Whether this run is the live lead run.
    pub(crate) fn is_live_lead_run(&self, run_id: &RunId) -> bool {
        self.workflow_lead
            .as_ref()
            .is_some_and(|run| !run.closed && &run.run_id == run_id)
    }

    /// The run a live lead is executing, if any. The single-live-run guard is
    /// what makes this an `Option` rather than a set, and it is what a
    /// projection tick's "something changed" answer refers to — the tick has no
    /// other run to be about.
    pub(crate) fn live_lead_run_id(&self) -> Option<String> {
        self.workflow_lead
            .as_ref()
            .filter(|run| !run.closed)
            .map(|run| run.run_id.to_string())
    }
}

/// The run id a `workflow.review.*` event is about, or `None` for every
/// other event kind. `data` is matched by reference so
/// `emit_workflow_run_event` can extract this before `data` is moved into
/// the wire envelope — cloning the whole payload (findings and all) just to
/// read one field back out would be wasteful for what is, on a busy server,
/// a per-poll-tick check.
fn review_event_run_id(kind: EventKind, data: &EventData) -> Option<String> {
    match (kind, data) {
        (EventKind::WorkflowReviewStarted, EventData::WorkflowReviewStarted { run_id, .. })
        | (EventKind::WorkflowReviewReady, EventData::WorkflowReviewReady { run_id, .. })
        | (EventKind::WorkflowReviewClosed, EventData::WorkflowReviewClosed { run_id, .. }) => {
            Some(run_id.clone())
        }
        _ => None,
    }
}

/// One session of a live run that karvex can address by name.
///
/// Server-owned runtime fact: the name is the one the team roster publishes,
/// not a UI label, and the same vocabulary `workflow.run.get`'s member rows
/// already use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunMessageTarget {
    pub(crate) name: String,
    pub(crate) pane_id: Option<String>,
    pub(crate) endpoint: SessionEndpoint,
}

/// What karvex knows after writing a message into a session's inbox.
///
/// Deliberately not "delivered": upstream's inbound controls decide between
/// delivered, held, and refused *after* the write, and the only receipt that
/// travels back over the socket is one addressed to another Claude Code
/// session's reply address, which karvex does not have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunMessageReceipt {
    pub(crate) target: String,
    pub(crate) session_id: String,
    pub(crate) pane_id: Option<String>,
    /// Which of the two channels carried it. Journalled because they are not
    /// equivalent: an inbox-socket message arrives labelled as another
    /// session's and is subject to the receiver's inbound controls, while pane
    /// input is indistinguishable from the user typing.
    pub(crate) channel: messaging::DeliveryChannel,
}

/// Why a message could not be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunMessageError {
    NoLiveRun,
    /// This machine cannot message Claude Code sessions at all.
    Unsupported(MessagingSupport),
    /// The run is live but nothing has identified itself yet.
    NoAddressableSessions,
    UnknownTarget {
        requested: String,
        known: Vec<String>,
    },
    /// The session identified itself but its messaging endpoint was absent —
    /// the feature-flag fetch had not completed when its hook ran.
    TargetNotAddressable {
        name: String,
    },
    BadMessage(String),
    WriteFailed(String),
}

impl std::fmt::Display for RunMessageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoLiveRun => f.write_str("no workflow run is live on this server"),
            Self::Unsupported(support) => write!(f, "{support}"),
            Self::NoAddressableSessions => f.write_str(
                "the run has not identified any session yet, so there is nothing to message. \
                 Its lead reports itself through a SessionStart hook a second or two after the \
                 pane starts.",
            ),
            Self::UnknownTarget { requested, known } => write!(
                f,
                "this run has no session called {requested:?}; it can address: {}",
                if known.is_empty() {
                    "nothing yet".to_string()
                } else {
                    known.join(", ")
                }
            ),
            Self::TargetNotAddressable { name } => write!(
                f,
                "{name} identified itself without a messaging socket, so Claude Code's \
                 cross-session messaging was not on in that session"
            ),
            Self::BadMessage(detail) => write!(f, "{detail}"),
            Self::WriteFailed(detail) => write!(
                f,
                "the session's inbox socket could not be written: {detail}"
            ),
        }
    }
}

/// Writes one already-encoded frame pair into a session's inbox socket.
///
/// Short timeouts on purpose: this runs on the server's event loop, and a
/// session that is not reading its socket must cost a render frame at worst.
/// The half-close is what tells Claude Code the connection is complete — it
/// processes the trailing buffer on `end`.
fn write_inbox_frames(socket: &Path, frames: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::net::UnixStream;

        let mut stream = UnixStream::connect(socket)?;
        stream.set_write_timeout(Some(INBOX_WRITE_TIMEOUT))?;
        stream.write_all(frames)?;
        stream.flush()?;
        // Half-close rather than a plain drop: Claude Code parses whatever is
        // left in its buffer when the peer ends, so this is what makes a frame
        // without a trailing newline still land.
        let _ = stream.shutdown(std::net::Shutdown::Write);
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, frames);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Claude Code does not offer cross-session messaging on native Windows",
        ))
    }
}

/// How long a write to a session's inbox may block the server loop.
///
/// Unix-only, like the socket it bounds: upstream offers no cross-session
/// messaging on native Windows, so a Windows build has no writer to time out.
#[cfg(unix)]
const INBOX_WRITE_TIMEOUT: Duration = Duration::from_millis(500);

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

/// Claude Code's own transcript path for a session, when the session never
/// told karvex where it was writing one.
///
/// Two guards, both deliberate. The path is only reported when the file is
/// actually there, so a derivation upstream has since changed reads back as
/// "not resolved" instead of as a path to nothing — the honest answer, and the
/// one that lets the *next* poll resolve it properly. And the check is a single
/// `stat` per unresolved member per poll, which is what keeps the 2 s
/// projection tick as cheap as it was (§5 P8's contract: `stat`-bounded, no
/// transcript reads).
pub(super) fn derived_transcript_path(
    claude_dir: &Path,
    cwd: Option<&str>,
    session_id: Option<&str>,
) -> Option<String> {
    let path = identity::transcript_path_for(claude_dir, cwd?, session_id?)?;
    path.is_file().then(|| path.to_string_lossy().into_owned())
}

fn read_team_config(path: &Path) -> Option<ObservedTeam> {
    let bytes = std::fs::read(path).ok()?;
    projection::parse_team_config(&bytes).ok()
}

/// How long a run may stay unbound before it is failed.
///
/// [`identity::BIND_DEADLINE`] unless `KARVEX_WORKFLOW_BIND_DEADLINE_MS`
/// overrides it. The override is a support and test hatch, not a setting: a
/// machine where `claude` reliably takes longer than two minutes to reach its
/// `SessionStart` hook has a different problem, and the headless tests need to
/// exercise the expiry in seconds rather than minutes. An unparseable or zero
/// value falls back to the default rather than failing every run instantly.
fn bind_deadline() -> Duration {
    std::env::var("KARVEX_WORKFLOW_BIND_DEADLINE_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(identity::BIND_DEADLINE, Duration::from_millis)
}

/// The `kvx` binary the run's hook should call back into.
///
/// The running executable rather than the `kvx` on `PATH`: a run must report
/// itself to *this* server, and a machine mid-upgrade can easily have a
/// different `kvx` first on the path. Falls back to the bare command only when
/// the current executable cannot be resolved at all.
fn kvx_executable() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("kvx"))
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

    /// The seam `App::refresh_open_dag_review` (packet P13) relies on:
    /// exactly the three `workflow.review.*` event kinds carry a run id out
    /// of `emit_workflow_run_event`, and nothing else does — a node-level or
    /// run-level (non-review) event must never trigger a review refetch.
    #[test]
    fn only_review_events_carry_a_run_id_out_for_the_dag_refresh() {
        let review = crate::api::schema::WorkflowReviewInfo {
            id: "review_cycle:1".to_string(),
            run_id: "workflow_run:1".to_string(),
            workflow_id: "workflow:1".to_string(),
            version_id: "kvdag_version:1".to_string(),
            status: crate::api::schema::WorkflowReviewStatus::AwaitingUser,
            started_at_unix_ms: 1,
            ended_at_unix_ms: None,
            resulting_version_id: None,
            interview_paths: Vec::new(),
            evidence_only_count: 0,
        };

        assert_eq!(
            review_event_run_id(
                EventKind::WorkflowReviewStarted,
                &EventData::WorkflowReviewStarted {
                    run_id: "workflow_run:1".to_string(),
                    review: review.clone(),
                },
            ),
            Some("workflow_run:1".to_string())
        );
        assert_eq!(
            review_event_run_id(
                EventKind::WorkflowReviewReady,
                &EventData::WorkflowReviewReady {
                    run_id: "workflow_run:1".to_string(),
                    review: review.clone(),
                },
            ),
            Some("workflow_run:1".to_string())
        );
        assert_eq!(
            review_event_run_id(
                EventKind::WorkflowReviewClosed,
                &EventData::WorkflowReviewClosed {
                    run_id: "workflow_run:1".to_string(),
                    review,
                },
            ),
            Some("workflow_run:1".to_string())
        );

        let run = crate::api::schema::WorkflowRunInfo {
            run_id: "workflow_run:1".to_string(),
            workflow_id: "workflow:1".to_string(),
            version_id: "kvdag_version:1".to_string(),
            tier: crate::api::schema::WorkflowTier::Auto,
            status: crate::api::schema::WorkflowRunStatus::Succeeded,
            args: Default::default(),
            workspace_id: None,
            tab_id: None,
            started_at_unix_ms: 1,
            ended_at_unix_ms: Some(2),
            total_tokens: 0,
            total_tool_uses: 0,
            nodes_total: 0,
            nodes_done: 0,
            failure: None,
            max_depth: 0,
            max_nodes: 0,
            nodes_live: 0,
            growth_limited: None,
            workflow_name: String::new(),
            context_runs: Vec::new(),
            restore_from_run: None,
            lead_session_id: None,
            team_name: None,
            lead_pane_id: None,
            lead_prompt_version: None,
        };
        assert_eq!(
            review_event_run_id(
                EventKind::WorkflowRunFinished,
                &EventData::WorkflowRunFinished { run },
            ),
            None,
            "a non-review run-level event must never trigger a review refetch"
        );
    }
}

#[cfg(all(test, feature = "workflow"))]
mod live_run_tests {
    use super::*;
    use crate::api::schema::Method;
    use crate::workflow::binding::identity::SessionReport;
    use crate::workflow::binding::messaging::{MessagingSupport, Priority};

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
        app
    }

    /// A live lead run with nothing reported yet: the state `workflow.run`
    /// leaves behind the instant the lead's pane exists.
    fn app_with_a_live_run() -> (crate::app::App, RunId) {
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
        (app, run_id)
    }

    fn lead_reports_itself(app: &mut crate::app::App, socket: Option<&str>) {
        lead_reports_itself_with_transcript(app, socket, None)
    }

    fn lead_reports_itself_with_transcript(
        app: &mut crate::app::App,
        socket: Option<&str>,
        transcript: Option<&str>,
    ) {
        let pane_id = app
            .workflow_lead
            .as_ref()
            .map(|run| run.lead_pane_id.clone())
            .expect("a live run");
        let run_id = app
            .workflow_lead
            .as_ref()
            .map(|run| run.run_id.to_string())
            .expect("a live run");
        app.record_run_session_report(&SessionReport {
            run_id,
            pane_id: Some(pane_id),
            session_id: "51ea857f-cb96-4372-ae75-bab1640c8428".to_string(),
            transcript_path: transcript.map(str::to_string),
            cwd: Some("/repo".to_string()),
            source: Some("startup".to_string()),
            messaging_socket: socket.map(str::to_string),
            messaging_token: socket.map(|_| "50093985aaaabbbbccccddddeeeeffff".to_string()),
            agent_id: None,
        });
    }

    // ── member identity (§3.3, packet P8) ──────────────────────────────────

    /// A real two-member config, from `projection.rs`'s own live fixture: the
    /// lead is in-process behind the `"leader"` sentinel, and only the teammate
    /// proves split-pane mode took.
    const TEAM_CONFIG: &str = r#"{
  "name": "session-3cb241fe",
  "createdAt": 1786376746139,
  "leadSessionId": "3cb241fe-2c3a-4dd8-b8a0-5dd83dfc5aa2",
  "members": [
    { "agentId": "team-lead@session-3cb241fe", "name": "team-lead", "agentType": "team-lead",
      "joinedAt": 1786376746139, "tmuxPaneId": "leader",
      "cwd": "/repo", "backendType": "in-process" },
    { "agentId": "research@session-3cb241fe", "name": "research",
      "joinedAt": 1786376797068, "tmuxPaneId": "w1:p4",
      "agentType": "Explore", "model": "sonnet",
      "cwd": "/repo", "backendType": "tmux", "isActive": true }
  ]
}"#;

    const TEAM_NAME: &str = "session-3cb241fe";
    const LEAD_SESSION_ID: &str = "3cb241fe-2c3a-4dd8-b8a0-5dd83dfc5aa2";
    const TEAMMATE_PANE: &str = "w1:p4";
    const TEAMMATE_SESSION_ID: &str = "7694e312-4ac2-41d7-90ec-1277e61689df";

    /// A throwaway `CLAUDE_CONFIG_DIR`-shaped tree holding one team config.
    ///
    /// Written to disk rather than mocked because the poller's whole job is to
    /// read Claude Code's own files, and the read is what the empty-delta
    /// discipline has to survive.
    struct ClaudeDir(PathBuf);

    impl ClaudeDir {
        fn with_team(label: &str, config: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "karvex-p8-{label}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            ));
            let team = root.join("teams").join(TEAM_NAME);
            std::fs::create_dir_all(&team).expect("the fixture team directory is writable");
            std::fs::write(team.join("config.json"), config).expect("the team config is writable");
            std::fs::create_dir_all(root.join("tasks").join(TEAM_NAME))
                .expect("the fixture task directory is writable");
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ClaudeDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Puts the run in the state `bind_run_team` would have left it in, without
    /// a `claude` to bind to. The binding is the only thing `absorb_run_projection`
    /// needs that the launch path does not already provide.
    fn bind_run_to_the_fixture_team(app: &mut crate::app::App) {
        let run = app.workflow_lead.as_mut().expect("a live run");
        run.binding = Some(LeadBinding {
            team_name: TEAM_NAME.to_string(),
            lead_session_id: LEAD_SESSION_ID.to_string(),
        });
    }

    fn teammate_reports_itself(app: &mut crate::app::App, transcript: Option<&str>) {
        let run_id = app
            .workflow_lead
            .as_ref()
            .map(|run| run.run_id.to_string())
            .expect("a live run");
        app.record_run_session_report(&SessionReport {
            run_id,
            pane_id: Some(TEAMMATE_PANE.to_string()),
            session_id: TEAMMATE_SESSION_ID.to_string(),
            transcript_path: transcript.map(str::to_string),
            cwd: Some("/repo".to_string()),
            source: Some("startup".to_string()),
            messaging_socket: None,
            messaging_token: None,
            agent_id: None,
        });
    }

    fn stored_members(
        app: &mut crate::app::App,
        run: &RunId,
    ) -> Vec<crate::workflow::store::RunMemberRecord> {
        let wanted = run.clone();
        app.workflow_store
            .call(move |cx| cx.block_on(cx.store().list_run_members(&wanted)))
            .expect("the in-memory store is available")
            .expect("the run's members read back")
    }

    fn stored_member(
        app: &mut crate::app::App,
        run: &RunId,
        name: &str,
    ) -> crate::workflow::store::RunMemberRecord {
        stored_members(app, run)
            .into_iter()
            .find(|member| member.name == name)
            .unwrap_or_else(|| panic!("the run has a member called {name}"))
    }

    fn stored_node(
        app: &mut crate::app::App,
        run: &RunId,
        path: &str,
    ) -> Option<crate::workflow::store::RunNodeRecord> {
        let wanted = run.clone();
        app.workflow_store
            .call(move |cx| cx.block_on(cx.store().list_run_nodes(&wanted)))
            .expect("the in-memory store is available")
            .expect("the run's nodes read back")
            .into_iter()
            .find(|node| node.instance_path.as_str() == path)
    }

    #[test]
    fn a_teammate_that_reported_its_session_keeps_it_after_its_pane_is_gone() {
        let (mut app, run_id) = app_with_a_live_run();
        let claude = ClaudeDir::with_team("teammate-identity", TEAM_CONFIG);
        bind_run_to_the_fixture_team(&mut app);
        teammate_reports_itself(
            &mut app,
            Some("/home/dev/.claude/projects/-repo/7694e312.jsonl"),
        );

        assert!(app.absorb_run_projection(claude.path()));
        let research = stored_member(&mut app, &run_id, "research");
        assert_eq!(research.session_id.as_deref(), Some(TEAMMATE_SESSION_ID));
        assert_eq!(
            research.transcript_path.as_deref(),
            Some("/home/dev/.claude/projects/-repo/7694e312.jsonl")
        );
        assert_eq!(research.pane_id.as_deref(), Some(TEAMMATE_PANE));

        // The pane closes and its endpoint goes with it — a socket path stops
        // being true when the process exits. The session id must not: a review
        // that runs tomorrow is the entire reason it was captured.
        app.workflow_lead
            .as_mut()
            .expect("a live run")
            .member_endpoints
            .clear();
        app.absorb_run_projection(claude.path());
        let research = stored_member(&mut app, &run_id, "research");
        assert_eq!(
            research.session_id.as_deref(),
            Some(TEAMMATE_SESSION_ID),
            "a closed pane must not erase what it reported while it was open"
        );
    }

    #[test]
    fn a_member_whose_hook_never_fired_records_none_and_the_run_still_projects() {
        let (mut app, run_id) = app_with_a_live_run();
        let claude = ClaudeDir::with_team("no-report", TEAM_CONFIG);
        bind_run_to_the_fixture_team(&mut app);

        assert!(app.absorb_run_projection(claude.path()));
        let research = stored_member(&mut app, &run_id, "research");
        assert_eq!(
            research.session_id, None,
            "an unresolved member is recorded as unresolved, never guessed at"
        );
        assert_eq!(research.transcript_path, None);
        // The rest of the projection is unaffected: the roster is still there,
        // and so is the member's placement.
        assert_eq!(stored_members(&mut app, &run_id).len(), 2);
        assert_eq!(research.backend_type, "tmux");
    }

    #[test]
    fn the_lead_gets_a_reserved_node_carrying_its_own_session_id() {
        let (mut app, run_id) = app_with_a_live_run();
        let claude = ClaudeDir::with_team("lead-node", TEAM_CONFIG);

        // Minted the instant the lead's pane exists, so the DAG shows the node
        // the run actually starts at even before any team config is on disk.
        let node = stored_node(&mut app, &run_id, ".lead").expect("the lead node is minted");
        assert_eq!(node.status, NodeStatus::Running);
        assert_eq!(node.label, LEAD_NODE_LABEL);
        assert!(
            node.agent_session_id.is_none(),
            "nothing has identified itself yet"
        );

        // Bound but silent: the lead is the one member whose session id the
        // team config does carry (`leadSessionId`), and S1 killed the
        // `~/.claude/sessions` registry the plan named as the fallback, so this
        // *is* the fallback.
        bind_run_to_the_fixture_team(&mut app);
        app.absorb_run_projection(claude.path());
        let node = stored_node(&mut app, &run_id, ".lead").expect("the lead node is still there");
        assert_eq!(
            node.agent_session_id.as_deref(),
            Some(LEAD_SESSION_ID),
            "the lead's identity reached its own node"
        );
        assert_eq!(node.pane_id.as_deref(), Some("w1:p1"));
        assert_eq!(
            node.transcript_path, None,
            "an unwritten transcript is nothing, not an empty path"
        );
        // The same identity is on the lead's `run_member` row, which is what a
        // review reads when it ranks who is worth interviewing.
        let lead = stored_member(&mut app, &run_id, "team-lead");
        assert_eq!(lead.session_id.as_deref(), Some(LEAD_SESSION_ID));
    }

    #[test]
    fn the_leads_own_report_outranks_the_team_config_and_brings_its_transcript() {
        let (mut app, run_id) = app_with_a_live_run();
        let claude = ClaudeDir::with_team("lead-report", TEAM_CONFIG);
        bind_run_to_the_fixture_team(&mut app);

        let run = app
            .workflow_lead
            .as_ref()
            .map(|run| (run.run_id.to_string(), run.lead_pane_id.clone()))
            .expect("a live run");
        app.record_run_session_report(&SessionReport {
            run_id: run.0,
            pane_id: Some(run.1),
            session_id: LEAD_SESSION_ID.to_string(),
            transcript_path: Some("/home/dev/.claude/projects/-repo/3cb241fe.jsonl".to_string()),
            cwd: Some("/repo".to_string()),
            source: Some("startup".to_string()),
            messaging_socket: None,
            messaging_token: None,
            agent_id: None,
        });
        app.absorb_run_projection(claude.path());

        let node = stored_node(&mut app, &run_id, ".lead").expect("the lead node is there");
        assert_eq!(
            node.transcript_path.as_deref(),
            Some("/home/dev/.claude/projects/-repo/3cb241fe.jsonl"),
            "the session's own answer is the authority, and \
             `interrogation.run_node` needs a node whose transcript is recorded"
        );
        let lead = stored_member(&mut app, &run_id, "team-lead");
        assert_eq!(
            lead.transcript_path.as_deref(),
            Some("/home/dev/.claude/projects/-repo/3cb241fe.jsonl"),
            "karvex used to throw this away for claude (S1); a review cannot \
             interview a member whose transcript was never recorded"
        );
    }

    #[test]
    fn the_lead_node_is_excluded_from_the_runs_own_node_counters() {
        let (mut app, run_id) = app_with_a_live_run();
        let run = app
            .workflow_store
            .call({
                let wanted = run_id.clone();
                move |cx| cx.block_on(cx.store().get_run(&wanted))
            })
            .expect("the in-memory store is available")
            .expect("the run row reads back")
            .expect("the run exists");
        assert_eq!(
            run.nodes_total, 1,
            "the reserved lead node is karvex's, not the author's, so it does \
             not move the run's progress denominator"
        );
    }

    #[test]
    fn the_lead_node_settles_when_the_run_does() {
        let (mut app, run_id) = app_with_a_live_run();
        app.finish_lead_run(&run_id, 1_700_000_000_000);
        let node = stored_node(&mut app, &run_id, ".lead").expect("the lead node is there");
        assert_eq!(
            node.status,
            NodeStatus::Succeeded,
            "a lead still `running` inside a finished run is the kind of lie \
             this rework exists to remove"
        );
        assert_eq!(node.ended_at_unix_ms, Some(1_700_000_000_000));
    }

    #[test]
    fn finishing_a_run_this_server_never_launched_does_not_degrade_persistence() {
        let (mut app, run_id) = app_with_a_live_run();
        // `workflow.run.finish` is authorised by possession of a run id alone,
        // so it can name a run that has no `.lead` node — one from before this
        // landed, or one another server launched. Settling a row that is not
        // there must not be reported to the user as a lost durable write.
        app.workflow_lead = None;
        app.finish_lead_run(&run_id, 1_700_000_000_000);
        assert!(!app.workflow_persistence_degraded);
    }

    #[test]
    fn a_poll_that_observed_nothing_new_writes_nothing() {
        let (mut app, run_id) = app_with_a_live_run();
        let claude = ClaudeDir::with_team("quiet-poll", TEAM_CONFIG);
        bind_run_to_the_fixture_team(&mut app);
        teammate_reports_itself(&mut app, None);

        assert!(app.absorb_run_projection(claude.path()));
        let before = stored_member(&mut app, &run_id, "research");
        // The 2 s poller runs forever; a tick that saw no change must not touch
        // the store at all (`projection.rs`'s empty-delta discipline).
        assert!(!app.absorb_run_projection(claude.path()));
        assert!(!app.absorb_run_projection(claude.path()));
        let after = stored_member(&mut app, &run_id, "research");
        assert_eq!(before.last_seen_at_unix_ms, after.last_seen_at_unix_ms);
    }

    #[test]
    fn a_members_session_id_reaches_workflow_run_get() {
        let (mut app, run_id) = app_with_a_live_run();
        let claude = ClaudeDir::with_team("wire", TEAM_CONFIG);
        bind_run_to_the_fixture_team(&mut app);
        teammate_reports_itself(&mut app, None);
        app.absorb_run_projection(claude.path());

        let response = app.dispatch_api_request(
            "test.workflow.run.get",
            Method::WorkflowRunGet(crate::api::schema::WorkflowRunTarget {
                run_id: run_id.to_string(),
            }),
        );
        let value: serde_json::Value =
            serde_json::from_str(&response).expect("the response is JSON");
        let members = value["result"]["graph"]["members"]
            .as_array()
            .unwrap_or_else(|| panic!("the run's members are on the wire: {response}"));
        let research = members
            .iter()
            .find(|member| member["name"] == "research")
            .expect("the teammate is on the wire");
        assert_eq!(research["session_id"], TEAMMATE_SESSION_ID);
    }

    #[test]
    fn a_teammate_is_resolved_from_the_pane_state_the_bundled_hook_already_fills() {
        let (mut app, run_id) = app_with_a_live_run();
        // Two workspaces: the first stands in for the lead's own pane, the
        // second for the split pane Claude Code put the teammate in.
        for label in ["lead-pane", "teammate-pane"] {
            app.state
                .workspaces
                .push(crate::workspace::Workspace::test_new(label));
        }
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        let lead_pane = app
            .public_pane_id(0, app.state.workspaces[0].tabs[0].root_pane)
            .expect("the lead's pane has a public id");
        let teammate_pane = app
            .public_pane_id(1, app.state.workspaces[1].tabs[0].root_pane)
            .expect("the teammate's pane has a public id");
        assert_ne!(lead_pane, teammate_pane);

        // What the bundled `SessionStart` hook already lands on a teammate's
        // pane today, with no new code at all (spike S1). This packet's whole
        // job is to copy it somewhere that outlives the pane.
        let teammate_terminal = app.state.workspaces[1]
            .terminal_id(app.state.workspaces[1].tabs[0].root_pane)
            .expect("the teammate's pane has a terminal")
            .clone();
        {
            let terminal = app
                .state
                .terminals
                .get_mut(&teammate_terminal)
                .expect("the teammate's terminal exists");
            terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "karvex:claude".to_string(),
                agent: "claude".to_string(),
                session_ref: crate::agent_resume::AgentSessionRef::id(TEAMMATE_SESSION_ID)
                    .expect("a real session id"),
            });
            terminal.state = crate::detect::AgentState::Idle;
        }
        app.workflow_lead
            .as_mut()
            .expect("a live run")
            .lead_pane_id
            .clone_from(&lead_pane);

        let claude = ClaudeDir::with_team(
            "pane-state",
            &TEAM_CONFIG.replace(TEAMMATE_PANE, &teammate_pane),
        );
        // The transcript Claude Code would have written for that session. The
        // derivation is only trusted when the file is actually there.
        let project = claude.path().join("projects").join("-repo");
        std::fs::create_dir_all(&project).expect("the fixture project dir is writable");
        std::fs::write(project.join(format!("{TEAMMATE_SESSION_ID}.jsonl")), "{}\n")
            .expect("the fixture transcript is writable");
        bind_run_to_the_fixture_team(&mut app);

        assert!(app.absorb_run_projection(claude.path()));
        let research = stored_member(&mut app, &run_id, "research");
        assert_eq!(
            research.session_id.as_deref(),
            Some(TEAMMATE_SESSION_ID),
            "the session id karvex already holds per pane is the primary source"
        );
        assert_eq!(
            research.transcript_path,
            Some(
                project
                    .join(format!("{TEAMMATE_SESSION_ID}.jsonl"))
                    .to_string_lossy()
                    .into_owned()
            ),
            "derived from the session id and cwd, and only once the file exists"
        );
        assert_eq!(research.last_state.as_deref(), Some("idle"));
        assert!(research.last_state_at_unix_ms.is_some());
    }

    #[test]
    fn a_derived_transcript_that_is_not_on_disk_is_recorded_as_nothing() {
        let (mut app, run_id) = app_with_a_live_run();
        let claude = ClaudeDir::with_team("no-transcript", TEAM_CONFIG);
        bind_run_to_the_fixture_team(&mut app);
        teammate_reports_itself(&mut app, None);

        app.absorb_run_projection(claude.path());
        let research = stored_member(&mut app, &run_id, "research");
        assert_eq!(research.session_id.as_deref(), Some(TEAMMATE_SESSION_ID));
        assert_eq!(
            research.transcript_path, None,
            "a path to a file that is not there would be worse than no path: \
             the review would plan a resumed interview it cannot run"
        );
    }

    fn journalled_messages(app: &mut crate::app::App, run: &RunId) -> Vec<serde_json::Value> {
        let wanted = run.clone();
        app.workflow_store
            .call(move |cx| cx.block_on(cx.store().list_run_events(&wanted)))
            .expect("the in-memory store is available")
            .expect("the run's journal reads back")
            .into_iter()
            .filter(|event| {
                matches!(
                    event.kind,
                    crate::workflow::model::RunEventKind::MessageDelivered
                )
            })
            .map(|event| event.payload)
            .collect()
    }

    /// S3's binding correction, at the two places a user could ever hear about
    /// it. A kill-switch variable only disables messaging on an account whose
    /// Claude Code feature flags have never been fetched, so karvex must not
    /// refuse over one — but it must still say what it saw, or the one case it
    /// cannot verify is also the one case nobody is told about.
    #[test]
    fn a_suspected_kill_switch_is_reported_to_clients_and_still_lets_the_run_be_messaged() {
        let (mut app, run_id) = app_with_a_live_run();
        if let Some(run) = app.workflow_lead.as_mut() {
            run.messaging = MessagingSupport::KillSwitchSuspected {
                variable: "DISABLE_TELEMETRY".to_string(),
                value: "1".to_string(),
            };
        }
        lead_reports_itself(&mut app, None);

        let info = app
            .run_messaging_info(&run_id)
            .expect("the live run publishes its messaging state");
        assert!(
            info.supported,
            "a suspicion is not a refusal: probed live, the variable changed nothing on an \
             account with cached feature flags"
        );
        assert_eq!(info.reason.as_deref(), Some("kill_switch_suspected"));
        assert!(
            info.detail
                .as_deref()
                .is_some_and(|detail| detail.contains("DISABLE_TELEMETRY")),
            "the client is told which variable: {:?}",
            info.detail
        );

        // And the send is refused for the *honest* reason — that session came
        // up without an inbox socket — never for the suspicion.
        let error = app
            .message_run_session("team-lead", "rebase first", Priority::Next)
            .expect_err("the lead reported no socket, and no pane exists in this fixture");
        assert!(
            !matches!(error, RunMessageError::Unsupported(_)),
            "a suspicion must never block a send: {error}"
        );
    }

    /// The other half: the two facts karvex *can* check do refuse, and they say
    /// so rather than letting the verb quietly do nothing.
    #[test]
    fn a_platform_without_cross_session_messaging_refuses_the_send_and_names_the_reason() {
        let (mut app, run_id) = app_with_a_live_run();
        if let Some(run) = app.workflow_lead.as_mut() {
            run.messaging = MessagingSupport::UnsupportedPlatform {
                platform: "native Windows",
            };
        }
        lead_reports_itself(&mut app, Some("/run/user/1000/cc-socks/1.sock"));

        let info = app.run_messaging_info(&run_id).expect("a live run");
        assert!(!info.supported);
        assert_eq!(info.reason.as_deref(), Some("unsupported_platform"));

        let error = app
            .message_run_session("team-lead", "rebase first", Priority::Next)
            .expect_err("native Windows has no inbox socket to write to");
        assert!(matches!(error, RunMessageError::Unsupported(_)), "{error}");
        assert!(error.to_string().contains("Windows"), "{error}");
    }

    /// Naming a session that does not exist has to list the ones that do. An
    /// unknown target is the most likely mistake a client or a lead can make,
    /// and "no" without "here is what there is" would be unusable.
    #[test]
    fn an_unknown_target_is_refused_with_the_roster_that_does_exist() {
        let (mut app, _run_id) = app_with_a_live_run();
        lead_reports_itself(&mut app, Some("/run/user/1000/cc-socks/1.sock"));

        let error = app
            .message_run_session("backend", "rebase first", Priority::Next)
            .expect_err("no session is called backend");
        match &error {
            RunMessageError::UnknownTarget { requested, known } => {
                assert_eq!(requested, "backend");
                assert_eq!(known, &vec![lead::LEAD_TARGET_NAME.to_string()]);
            }
            other => panic!("expected an unknown-target refusal, got {other}"),
        }
        assert!(error.to_string().contains("team-lead"), "{error}");
    }

    /// Nothing has identified itself yet, so there is no session to address —
    /// and saying that is the whole point. The verb existing at all is only
    /// defensible if it refuses out loud.
    #[test]
    fn a_run_whose_sessions_have_not_reported_refuses_rather_than_pretending() {
        let (mut app, _run_id) = app_with_a_live_run();
        let error = app
            .message_run_session("team-lead", "rebase first", Priority::Next)
            .expect_err("no session has reported");
        assert!(
            matches!(error, RunMessageError::NoAddressableSessions),
            "{error}"
        );
    }

    /// The journal entry the watchdog ladder and any post-mortem depend on.
    ///
    /// Which channel carried a message cannot be reconstructed afterwards: an
    /// inbox socket belongs to a live process, a teammate's token exists
    /// nowhere but its own hook environment, and after a karvex restart pane
    /// input is all that is left. So the channel is recorded per message, at
    /// the moment karvex knows it.
    // The inbox socket is a Unix domain socket; `write_inbox_frames` has no
    // Windows implementation, and neither has this fixture.
    #[cfg(unix)]
    #[test]
    fn every_delivered_message_records_the_channel_that_carried_it() {
        let (mut app, run_id) = app_with_a_live_run();
        let socket_dir = std::env::temp_dir().join(format!(
            "karvex-inbox-{}-{}",
            std::process::id(),
            crate::app::workflow::current_unix_ms()
        ));
        std::fs::create_dir_all(&socket_dir).expect("a temp dir");
        let socket = socket_dir.join("inbox.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket).expect("bind a stand-in inbox");
        let reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut frames = String::new();
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.read_to_string(&mut frames);
            }
            frames
        });
        lead_reports_itself(&mut app, socket.to_str());

        let receipt = app
            .message_run_session("team-lead", "rebase before you continue", Priority::Now)
            .expect("the socket accepts the frames");
        assert_eq!(receipt.channel, messaging::DeliveryChannel::InboxSocket);

        let frames = reader.join().expect("the stand-in inbox read its frames");
        assert!(frames.contains("\"type\":\"auth\""), "{frames}");
        assert!(frames.contains("rebase before you continue"), "{frames}");

        let journalled = journalled_messages(&mut app, &run_id);
        assert_eq!(journalled.len(), 1, "one message, one journal entry");
        let entry = &journalled[0];
        assert_eq!(entry["channel"], "inbox_socket");
        assert_eq!(entry["target"], "team-lead");
        assert_eq!(entry["priority"], "now");
        assert_eq!(entry["session_id"], "51ea857f-cb96-4372-ae75-bab1640c8428");
        // The steering text itself stays out of the store: the journal records
        // what karvex did, the transcript records what was said.
        assert_eq!(entry["text_bytes"], "rebase before you continue".len());
        assert!(
            !entry.to_string().contains("rebase before you continue"),
            "the journal must not carry the message text: {entry}"
        );

        let _ = std::fs::remove_dir_all(&socket_dir);
    }

    /// A refused message must leave no journal entry, because the journal is
    /// the record of what actually crossed a channel.
    #[test]
    fn a_refused_message_is_not_journalled_as_a_delivery() {
        let (mut app, run_id) = app_with_a_live_run();
        lead_reports_itself(&mut app, Some("/nonexistent/karvex/inbox.sock"));

        let error = app
            .message_run_session("team-lead", "rebase first", Priority::Next)
            .expect_err("there is no socket at that path");
        assert!(matches!(error, RunMessageError::WriteFailed(_)), "{error}");
        assert!(journalled_messages(&mut app, &run_id).is_empty());
    }

    /// The lead's self-report is what makes the run addressable at all, and the
    /// roster name is the one the API already publishes rather than a second
    /// vocabulary.
    #[test]
    fn the_leads_own_report_is_what_makes_the_run_addressable() {
        let (mut app, _run_id) = app_with_a_live_run();
        assert!(
            app.run_message_targets().is_empty(),
            "nothing is addressable before anything reports"
        );
        lead_reports_itself(&mut app, Some("/run/user/1000/cc-socks/1.sock"));

        let targets = app.run_message_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, lead::LEAD_TARGET_NAME);
        assert_eq!(
            targets[0].endpoint.messaging_socket.as_deref(),
            Some("/run/user/1000/cc-socks/1.sock")
        );
        assert_eq!(
            targets[0].endpoint.messaging_token.as_deref(),
            Some("50093985aaaabbbbccccddddeeeeffff"),
            "the token is captured when the hook fires; it exists nowhere else"
        );
    }
}
