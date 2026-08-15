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
    fn next_journal_seq(&mut self) -> u64 {
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
            run_id,
            lead_pane_id,
            lead_terminal_id,
            lead_cwd: spec.cwd.clone(),
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
        changed |= self.seed_lead_if_ready();
        changed |= self.absorb_run_projection(&claude_dir);
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
                "the run's lead never identified itself; falling back to matching a team by                  spawn window and cwd"
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
        warn!(run = %run_id, waited_ms, "{reason}");
        self.persist_workflow_write(StoreWrite::RunFailed {
            run: run_id,
            ended_at_unix_ms: crate::app::workflow::current_unix_ms(),
            failure: serde_json::json!({
                "kind": "lead_unbound",
                "detail": reason,
                "waited_ms": waited_ms,
                "resumable": false,
            }),
        });
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
    pub(crate) fn emit_workflow_run_event(&mut self, kind: EventKind, data: EventData) {
        self.emit_event(EventEnvelope { event: kind, data });
        self.refresh_workflow_runs_overlay(kind);
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
                "the run has not identified any session yet, so there is nothing to message.                  Its lead reports itself through a SessionStart hook a second or two after the                  pane starts.",
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
                "{name} identified itself without a messaging socket, so Claude Code's                  cross-session messaging was not on in that session"
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
            .and_then(|value| value["result"]["workflow"]["workflow_id"].as_str().map(str::to_string))
            .unwrap_or_else(|| panic!("the workflow was created: {response}"));
        let run_id = app.test_bind_a_live_lead_run(&workflow_id, "ship-it");
        (app, run_id)
    }

    fn lead_reports_itself(app: &mut crate::app::App, socket: Option<&str>) {
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
            cwd: Some("/repo".to_string()),
            source: Some("startup".to_string()),
            messaging_socket: socket.map(str::to_string),
            messaging_token: socket.map(|_| "50093985aaaabbbbccccddddeeeeffff".to_string()),
            agent_id: None,
        });
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
