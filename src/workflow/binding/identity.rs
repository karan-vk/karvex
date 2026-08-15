//! Deterministic run identity: what the run's Claude Code sessions tell karvex
//! about themselves, and how a run binds to its team.
//!
//! ## Why this module exists
//!
//! `09-agent-teams-rework.md` §3.1 step 4 binds a run to a team by *inference*
//! — a `createdAt` inside a slack window plus a matching lead cwd. That was the
//! best available rule when the plan was written, and it is wrong in two ways
//! the audit pinned down: the freshness window had a floor but no ceiling, so a
//! session the user starts by hand in the same directory half an hour later is
//! still eligible; and a run whose team never appears polls forever, stays
//! `running`, and wedges the single-live-run guard.
//!
//! Claude Code documents a better channel, and it was verified live against
//! 2.1.232 on 2026-08-15 (see `docs/design/workflow-builder/09-agent-teams-rework.md`
//! §3.1a for the captures):
//!
//! * Every session exports `CLAUDE_CODE_MESSAGING_SOCKET` and
//!   `CLAUDE_CODE_MESSAGING_TOKEN` to its hooks **before any hook runs**,
//!   including `SessionStart`.
//! * A `SessionStart` hook receives `{"session_id", "transcript_path", "cwd",
//!   "hook_event_name", "source"}` on stdin.
//! * Hook entries from a `--settings` payload are *added* to the hooks in the
//!   user's settings, not substituted for them (the probe's hook ran as
//!   `sessionstart-hook-3.sh`, third of three).
//! * The team a session leads is named `session-` + the first eight characters
//!   of its session id.
//!
//! So karvex hands the lead a run-scoped `--settings` file carrying a
//! `SessionStart` hook that calls back with the run id, the pane id, the
//! session id, and the messaging endpoint. Binding becomes an *assertion* that
//! karvex can check against identifiers it minted itself, and the inference
//! rule survives only as a fallback for a session whose hook never fired.
//!
//! Everything here is a pure function over values so the whole policy is
//! testable without a PTY, a socket, or a `claude`.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use crate::workflow::binding::lead::{LeadBinding, TEAM_MATCH_SLACK_MS};
use crate::workflow::projection::ObservedTeam;

/// Team and task directories are named `session-` + the first
/// [`TEAM_NAME_SESSION_CHARS`] characters of the lead's session id. Documented
/// upstream and verified live: session `51ea857f-cb96-…` produced
/// `~/.claude/teams/session-51ea857f/`.
pub const TEAM_NAME_PREFIX: &str = "session-";

/// How much of the session id the team name carries.
pub const TEAM_NAME_SESSION_CHARS: usize = 8;

/// How long a run may go unbound before karvex gives up on it.
///
/// A run that never binds is the failure the audit found: the poller keeps
/// looking, the row stays `running`, and the single-live-run guard then refuses
/// every other run for the rest of the server's life. Two minutes is far longer
/// than a cold `claude` start (the live probe's hook fired ~1.2 s after spawn)
/// and short enough that a wedged run is a nuisance rather than a lockout.
pub const BIND_DEADLINE: Duration = Duration::from_secs(120);

/// How far *after* karvex's spawn instant a team's `createdAt` may sit and
/// still be believed to be this run's, for the fallback rule only.
///
/// The missing half of `TEAM_MATCH_SLACK_MS`. Without a ceiling the fallback
/// stays hungry for as long as the run is unbound, so any session the user
/// starts by hand in the same directory is a candidate. The ceiling is the bind
/// deadline: past it karvex is not looking any more anyway, and tying the two
/// together means there is exactly one window to reason about.
pub const TEAM_MATCH_CEILING_MS: u64 = BIND_DEADLINE.as_millis() as u64;

/// The Claude Code session-name karvex gives a run's lead.
///
/// `--name` is documented (`/rename`'s flag form) and is the name `/list-agents`
/// and `SendMessage` address the session by; without it Claude Code derives one
/// from the cwd's folder name, which is the same for every run in a repository.
/// A run-scoped name makes the lead individually addressable by a human too.
pub fn lead_session_name(run_id: &crate::workflow::model::RunId) -> String {
    format!("karvex-run-{}", run_slug(run_id))
}

/// The stable, filesystem- and name-safe tail of a run id.
///
/// Run ids are `workflow_run:<id>`; only the tail is interesting and only its
/// first eight characters are needed to tell one live run from another.
fn run_slug(run_id: &crate::workflow::model::RunId) -> String {
    let raw = run_id.0.rsplit(':').next().unwrap_or(&run_id.0);
    raw.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase()
}

/// The team a session id leads, derived rather than searched for.
///
/// `None` for a session id too short to name a team: karvex would rather have
/// no team name than a truncated one that silently reads an unrelated
/// directory.
pub fn team_name_for_session(session_id: &str) -> Option<String> {
    let head: String = session_id
        .chars()
        .take(TEAM_NAME_SESSION_CHARS)
        .collect::<String>();
    if head.chars().count() < TEAM_NAME_SESSION_CHARS {
        return None;
    }
    if !head
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return None;
    }
    Some(format!("{TEAM_NAME_PREFIX}{head}"))
}

// ── the hook payload ───────────────────────────────────────────────────────

/// The JSON Claude Code writes to a `SessionStart` hook's stdin.
///
/// Only the fields karvex acts on are modelled; the rest is ignored on purpose,
/// because this is a foreign, experimental payload and an added field must not
/// break a run's binding.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct HookInput {
    pub session_id: String,
    pub transcript_path: Option<String>,
    pub cwd: Option<String>,
    pub hook_event_name: Option<String>,
    /// `startup`, `resume`, `clear`, … — Claude Code's own word for why the
    /// session started.
    pub source: Option<String>,
    /// Present when the hook is running for an in-process subagent rather than
    /// for the session itself. Such a report is never an identity assertion.
    pub agent_id: Option<String>,
}

/// Parses a `SessionStart` payload.
///
/// Lenient by design: a payload that is not an object, or carries no session
/// id, is [`None`] rather than an error, because the only sensible thing a hook
/// can do about a payload it does not understand is exit quietly.
pub fn parse_hook_input(raw: &str) -> Option<HookInput> {
    let parsed: HookInput = serde_json::from_str(raw.trim()).ok()?;
    if parsed.session_id.trim().is_empty() {
        return None;
    }
    Some(parsed)
}

/// One session's self-report, as it reaches the server.
///
/// This is the wire-shaped value: the hook fills it from its stdin payload plus
/// the environment Claude Code exported to it, and the server decides what it
/// means. Nothing here is trusted on its face — [`classify_report`] checks
/// every field karvex minted itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionReport {
    /// The run the hook was configured for. karvex bakes this into the hook
    /// command in the run's `--settings`, so it survives into the teammate
    /// sessions Claude Code spawns with the same settings file.
    pub run_id: String,
    /// The karvex pane the session is running in, from `KARVEX_PANE_ID`.
    pub pane_id: Option<String>,
    pub session_id: String,
    pub cwd: Option<String>,
    pub source: Option<String>,
    /// `CLAUDE_CODE_MESSAGING_SOCKET`. Absent when the messaging feature flag
    /// has not resolved yet, or is switched off — which is a fact worth
    /// recording rather than a reason to reject the report.
    pub messaging_socket: Option<String>,
    /// `CLAUDE_CODE_MESSAGING_TOKEN`, the session's own child token.
    pub messaging_token: Option<String>,
    /// Set when the hook ran for an in-process subagent.
    pub agent_id: Option<String>,
}

/// What karvex expects a genuine report for this run to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunExpectation {
    pub run_id: String,
    /// The pane karvex launched the lead into.
    pub lead_pane_id: String,
}

/// How karvex resolved to send to, or observe, a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEndpoint {
    pub session_id: String,
    pub messaging_socket: Option<String>,
    pub messaging_token: Option<String>,
}

/// What a self-report turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportVerdict {
    /// The run's team lead identifying itself. Carries the team name derived
    /// from the session id, which is what binding needs.
    Lead {
        endpoint: SessionEndpoint,
        team_name: String,
    },
    /// A session running in one of this run's other panes: a split-pane
    /// teammate, which inherits the lead's `--settings` and therefore the same
    /// hook. Recorded by pane id, because the teammate's *name* only exists in
    /// the team config karvex reads separately.
    Member {
        pane_id: String,
        endpoint: SessionEndpoint,
    },
    /// Not this run's, or not a session at all.
    Ignored(IgnoredReason),
}

/// Why a self-report was not acted on. Every variant is a thing that has to be
/// distinguishable in a log line, because "the hook fired and nothing happened"
/// is the failure mode this whole path exists to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IgnoredReason {
    /// The report named a different run.
    OtherRun { reported: String },
    /// No session id, so there is nothing to identify.
    NoSessionId,
    /// The session id is too short to derive a team name from.
    UnusableSessionId { session_id: String },
    /// An in-process subagent, not a session with its own inbox.
    Subagent,
    /// A session in no karvex pane at all — it cannot be this run's, because
    /// every session this run owns is in a pane karvex made.
    NoPane,
}

impl fmt::Display for IgnoredReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OtherRun { reported } => write!(f, "the report names run {reported}"),
            Self::NoSessionId => f.write_str("the report carries no session id"),
            Self::UnusableSessionId { session_id } => {
                write!(
                    f,
                    "the session id {session_id:?} is too short to name a team"
                )
            }
            Self::Subagent => {
                f.write_str("the report is an in-process subagent's, not a session's")
            }
            Self::NoPane => f.write_str("the report carries no karvex pane id"),
        }
    }
}

/// Decides what a self-report is, against identifiers karvex minted itself.
///
/// The run id and the pane id are both karvex's own: the run id is baked into
/// the hook command in the run's settings file, and the pane id comes from
/// `KARVEX_PANE_ID`, which only karvex sets. A report that matches both is not
/// a guess about which team belongs to which run — it is the session saying so.
pub fn classify_report(report: &SessionReport, expected: &RunExpectation) -> ReportVerdict {
    if report.agent_id.as_deref().is_some_and(|id| !id.is_empty()) {
        return ReportVerdict::Ignored(IgnoredReason::Subagent);
    }
    if report.run_id != expected.run_id {
        return ReportVerdict::Ignored(IgnoredReason::OtherRun {
            reported: report.run_id.clone(),
        });
    }
    if report.session_id.trim().is_empty() {
        return ReportVerdict::Ignored(IgnoredReason::NoSessionId);
    }
    let endpoint = SessionEndpoint {
        session_id: report.session_id.clone(),
        messaging_socket: non_empty(report.messaging_socket.as_deref()),
        messaging_token: non_empty(report.messaging_token.as_deref()),
    };
    match report.pane_id.as_deref().map(str::trim) {
        Some(pane) if pane == expected.lead_pane_id => {
            let Some(team_name) = team_name_for_session(&report.session_id) else {
                return ReportVerdict::Ignored(IgnoredReason::UnusableSessionId {
                    session_id: report.session_id.clone(),
                });
            };
            ReportVerdict::Lead {
                endpoint,
                team_name,
            }
        }
        Some(pane) if !pane.is_empty() => ReportVerdict::Member {
            pane_id: pane.to_string(),
            endpoint,
        },
        _ => ReportVerdict::Ignored(IgnoredReason::NoPane),
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

// ── binding ────────────────────────────────────────────────────────────────

/// How a run's team was recognised, strongest first.
///
/// Ordered so a caller can log the weakest evidence it accepted, and so the
/// asserted path is visibly distinct from the two inference tiers it replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BindEvidence {
    /// A team whose leader member started in the lead pane's cwd, inside the
    /// spawn window. The weakest rule, kept only for a lead whose hook never
    /// fired.
    InferredLeadCwd,
    /// A team holding a pane karvex created for this run.
    InferredOwnPane,
    /// The lead session said so, through the run's own `SessionStart` hook.
    Asserted,
}

impl BindEvidence {
    /// Whether this binding came from the session rather than from a guess.
    pub fn is_asserted(self) -> bool {
        matches!(self, Self::Asserted)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::InferredLeadCwd => "inferred_lead_cwd",
            Self::InferredOwnPane => "inferred_own_pane",
            Self::Asserted => "asserted",
        }
    }
}

/// What one binding attempt concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindDecision {
    Bound {
        binding: LeadBinding,
        evidence: BindEvidence,
    },
    /// Nothing recognisable yet, and the deadline has not passed.
    Waiting,
    /// The deadline passed with nothing recognised. The run has to fail here
    /// rather than keep polling: an unbound run that stays `running` blocks
    /// every later run behind the single-live-run guard.
    Expired { waited_ms: u64 },
}

/// Everything one binding attempt looks at.
pub struct BindInputs<'a> {
    /// The lead's own self-report, if its hook has fired.
    pub asserted: Option<&'a SessionEndpoint>,
    /// Every team config currently on disk, for the fallback rule.
    pub teams: &'a [ObservedTeam],
    /// When karvex launched the lead's pane.
    pub spawned_at_unix_ms: u64,
    /// Now, on the same clock.
    pub now_unix_ms: u64,
    /// How long the run may stay unbound. [`BIND_DEADLINE`] in production; a
    /// parameter rather than a constant read inside so the expiry rule is
    /// testable in milliseconds instead of minutes.
    pub deadline: Duration,
    pub lead_cwd: &'a Path,
    /// Public pane ids this server currently owns.
    pub own_pane_ids: &'a [String],
    /// Teams already claimed by another run.
    pub bound_elsewhere: &'a [String],
}

/// The whole binding rule, as one pure decision.
///
/// Assertion first: if the lead's hook has reported a session id, the team name
/// follows from it by a documented derivation and no search is needed. karvex
/// deliberately does **not** require the team config to exist on disk before
/// binding — the assertion is the identity, and the projection reads the
/// directory the assertion names.
pub fn decide_binding(inputs: &BindInputs<'_>) -> BindDecision {
    if let Some(endpoint) = inputs.asserted {
        if let Some(team_name) = team_name_for_session(&endpoint.session_id) {
            return BindDecision::Bound {
                binding: LeadBinding {
                    team_name,
                    lead_session_id: endpoint.session_id.clone(),
                },
                evidence: BindEvidence::Asserted,
            };
        }
    }

    if let Some((binding, evidence)) = match_team_window(
        inputs.teams,
        inputs.spawned_at_unix_ms,
        inputs.lead_cwd,
        inputs.own_pane_ids,
        inputs.bound_elsewhere,
    ) {
        return BindDecision::Bound { binding, evidence };
    }

    let waited_ms = inputs.now_unix_ms.saturating_sub(inputs.spawned_at_unix_ms);
    if waited_ms >= deadline_ms(inputs.deadline) {
        BindDecision::Expired { waited_ms }
    } else {
        BindDecision::Waiting
    }
}

/// The deadline in milliseconds, saturating rather than wrapping on an absurd
/// override.
fn deadline_ms(deadline: Duration) -> u64 {
    u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX)
}

/// The fallback rule: recognise the team a freshly spawned lead created, from
/// every team config on disk.
///
/// This is `09` §3.1 step 4's heuristic with the audit's missing ceiling added.
/// It exists for exactly one case — a lead whose `SessionStart` hook did not
/// fire, because the user's settings disable hooks, because `claude` predates
/// the hook payload karvex reads, or because the hook could not reach the
/// server — and it is deliberately the weaker answer whenever both are
/// available.
pub fn match_team_window(
    teams: &[ObservedTeam],
    spawned_at_unix_ms: u64,
    lead_cwd: &Path,
    own_pane_ids: &[String],
    bound_elsewhere: &[String],
) -> Option<(LeadBinding, BindEvidence)> {
    let floor = spawned_at_unix_ms.saturating_sub(TEAM_MATCH_SLACK_MS);
    let ceiling = spawned_at_unix_ms.saturating_add(TEAM_MATCH_CEILING_MS);
    let mut best: Option<(BindEvidence, u64, LeadBinding)> = None;

    for team in teams {
        if bound_elsewhere.iter().any(|name| name == &team.name) {
            continue;
        }
        // Freshness is mandatory, not one signal among several: karvex pane ids
        // are per-server, so a long-dead karvex's team config can name `w1:p2`
        // while this server also has a `w1:p2` — observed live, where that
        // collision alone bound a run to a months-old team.
        if team.created_at_unix_ms < floor || team.created_at_unix_ms > ceiling {
            continue;
        }
        let claims_our_pane = team
            .members
            .iter()
            .filter_map(|member| member.tmux_pane_id())
            .any(|pane| own_pane_ids.iter().any(|own| own == pane));
        let cwd_matches = team
            .lead_cwd()
            .is_some_and(|cwd| Path::new(cwd) == lead_cwd);

        let evidence = if claims_our_pane {
            BindEvidence::InferredOwnPane
        } else if cwd_matches {
            BindEvidence::InferredLeadCwd
        } else {
            continue;
        };

        let candidate = LeadBinding {
            team_name: team.name.clone(),
            lead_session_id: team.lead_session_id.clone(),
        };
        let better = match &best {
            None => true,
            Some((best_evidence, best_created, _)) => {
                evidence > *best_evidence
                    || (evidence == *best_evidence && team.created_at_unix_ms < *best_created)
            }
        };
        if better {
            best = Some((evidence, team.created_at_unix_ms, candidate));
        }
    }

    best.map(|(evidence, _, binding)| (binding, evidence))
}

/// The reason a run that never bound is failed with, in one sentence a user can
/// act on.
pub fn unbound_failure_reason(waited_ms: u64) -> String {
    format!(
        "the run's team lead never identified itself within {}s, so karvex has no team to observe. \
         The lead's pane is still open: check it for a folder-trust prompt, a login prompt, or a \
         `claude` that failed to start, then start the run again.",
        waited_ms / 1000
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::RunId;
    use crate::workflow::projection::ObservedMember;

    fn member(name: &str, pane: &str, backend: &str, cwd: &str) -> ObservedMember {
        ObservedMember {
            name: name.to_string(),
            agent_id: Some(format!("{name}@session-abc")),
            agent_type: "Explore".to_string(),
            model: Some("sonnet".to_string()),
            pane_id: Some(pane.to_string()),
            backend_type: backend.to_string(),
            is_active: true,
            cwd: Some(cwd.to_string()),
            joined_at_unix_ms: Some(1),
        }
    }

    fn team(name: &str, session: &str, created: u64, members: Vec<ObservedMember>) -> ObservedTeam {
        ObservedTeam {
            name: name.to_string(),
            lead_session_id: session.to_string(),
            created_at_unix_ms: created,
            members,
        }
    }

    fn expectation() -> RunExpectation {
        RunExpectation {
            run_id: "workflow_run:abc".to_string(),
            lead_pane_id: "w1:p2".to_string(),
        }
    }

    fn lead_report() -> SessionReport {
        SessionReport {
            run_id: "workflow_run:abc".to_string(),
            pane_id: Some("w1:p2".to_string()),
            session_id: "51ea857f-cb96-4372-ae75-bab1640c8428".to_string(),
            cwd: Some("/home/dev/project".to_string()),
            source: Some("startup".to_string()),
            messaging_socket: Some("/run/user/1000/cc-socks/266617.sock".to_string()),
            messaging_token: Some("50093985aaaabbbbccccddddeeeeffff".to_string()),
            agent_id: None,
        }
    }

    #[test]
    fn the_team_name_is_the_documented_session_derivation() {
        // Captured live: session 51ea857f-… produced ~/.claude/teams/session-51ea857f/.
        assert_eq!(
            team_name_for_session("51ea857f-cb96-4372-ae75-bab1640c8428").as_deref(),
            Some("session-51ea857f")
        );
    }

    #[test]
    fn a_session_id_too_short_to_name_a_team_yields_none_rather_than_a_truncation() {
        assert_eq!(team_name_for_session("51ea85"), None);
        assert_eq!(team_name_for_session(""), None);
        // A path separator would escape the teams directory entirely.
        assert_eq!(team_name_for_session("../../etc/passwd"), None);
    }

    #[test]
    fn a_lead_gets_a_run_scoped_session_name() {
        let name = lead_session_name(&RunId::new("workflow_run:9f2c1ab4d5"));
        assert_eq!(name, "karvex-run-9f2c1ab4");
        // Claude Code shows the name to the user and addresses messages by it,
        // so it must not carry anything a shell or a name matcher would choke on.
        assert!(name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-'));
    }

    #[test]
    fn the_live_session_start_payload_parses() {
        // Byte-for-byte from the 2.1.232 probe.
        let raw = r#"{"session_id":"93fd3b8e-c050-46bf-820b-c4a20762d1a5","transcript_path":"/home/karan/.claude/projects/-home-karan-code-karvex/93fd3b8e.jsonl","cwd":"/home/karan/code/karvex","hook_event_name":"SessionStart","source":"startup"}"#;
        let parsed = parse_hook_input(raw).expect("the live payload parses");
        assert_eq!(parsed.session_id, "93fd3b8e-c050-46bf-820b-c4a20762d1a5");
        assert_eq!(parsed.source.as_deref(), Some("startup"));
        assert_eq!(parsed.hook_event_name.as_deref(), Some("SessionStart"));
    }

    #[test]
    fn an_unknown_field_does_not_break_the_payload() {
        let raw = r#"{"session_id":"aaaaaaaa-1111","invented_upstream_field":42}"#;
        assert!(parse_hook_input(raw).is_some());
    }

    #[test]
    fn a_payload_with_no_session_id_is_not_a_report() {
        assert_eq!(parse_hook_input(r#"{"cwd":"/tmp"}"#), None);
        assert_eq!(parse_hook_input("not json"), None);
        assert_eq!(parse_hook_input(""), None);
    }

    #[test]
    fn the_lead_pane_reporting_its_session_is_the_lead() {
        match classify_report(&lead_report(), &expectation()) {
            ReportVerdict::Lead {
                endpoint,
                team_name,
            } => {
                assert_eq!(team_name, "session-51ea857f");
                assert_eq!(endpoint.session_id, "51ea857f-cb96-4372-ae75-bab1640c8428");
                assert_eq!(
                    endpoint.messaging_socket.as_deref(),
                    Some("/run/user/1000/cc-socks/266617.sock")
                );
            }
            other => panic!("expected a lead verdict, got {other:?}"),
        }
    }

    #[test]
    fn another_pane_of_the_same_run_is_a_member_not_the_lead() {
        let mut report = lead_report();
        report.pane_id = Some("w1:p7".to_string());
        match classify_report(&report, &expectation()) {
            ReportVerdict::Member { pane_id, endpoint } => {
                assert_eq!(pane_id, "w1:p7");
                assert!(endpoint.messaging_socket.is_some());
            }
            other => panic!("expected a member verdict, got {other:?}"),
        }
    }

    #[test]
    fn a_report_for_another_run_is_never_this_runs_identity() {
        let mut report = lead_report();
        report.run_id = "workflow_run:other".to_string();
        assert_eq!(
            classify_report(&report, &expectation()),
            ReportVerdict::Ignored(IgnoredReason::OtherRun {
                reported: "workflow_run:other".to_string()
            })
        );
    }

    #[test]
    fn an_in_process_subagents_hook_run_is_not_an_identity_assertion() {
        let mut report = lead_report();
        report.agent_id = Some("research@session-51ea857f".to_string());
        assert_eq!(
            classify_report(&report, &expectation()),
            ReportVerdict::Ignored(IgnoredReason::Subagent)
        );
    }

    #[test]
    fn a_report_from_no_pane_is_ignored() {
        let mut report = lead_report();
        report.pane_id = None;
        assert_eq!(
            classify_report(&report, &expectation()),
            ReportVerdict::Ignored(IgnoredReason::NoPane)
        );
    }

    #[test]
    fn a_lead_whose_session_id_cannot_name_a_team_is_refused_rather_than_half_bound() {
        let mut report = lead_report();
        report.session_id = "abc".to_string();
        assert!(matches!(
            classify_report(&report, &expectation()),
            ReportVerdict::Ignored(IgnoredReason::UnusableSessionId { .. })
        ));
    }

    #[test]
    fn an_empty_messaging_endpoint_is_absent_rather_than_an_empty_string() {
        // The variables are unset when the messaging feature flag has not
        // resolved; an empty string would look like a socket path to a caller.
        let mut report = lead_report();
        report.messaging_socket = Some(String::new());
        report.messaging_token = Some("   ".to_string());
        match classify_report(&report, &expectation()) {
            ReportVerdict::Lead { endpoint, .. } => {
                assert_eq!(endpoint.messaging_socket, None);
                assert_eq!(endpoint.messaging_token, None);
            }
            other => panic!("expected a lead verdict, got {other:?}"),
        }
    }

    fn inputs<'a>(
        asserted: Option<&'a SessionEndpoint>,
        teams: &'a [ObservedTeam],
        now: u64,
    ) -> BindInputs<'a> {
        BindInputs {
            asserted,
            teams,
            spawned_at_unix_ms: 1_000_000,
            now_unix_ms: now,
            deadline: BIND_DEADLINE,
            lead_cwd: Path::new("/home/dev/project"),
            own_pane_ids: &[],
            bound_elsewhere: &[],
        }
    }

    #[test]
    fn an_assertion_binds_without_any_team_config_on_disk() {
        let endpoint = SessionEndpoint {
            session_id: "51ea857f-cb96-4372-ae75-bab1640c8428".to_string(),
            messaging_socket: None,
            messaging_token: None,
        };
        let decision = decide_binding(&inputs(Some(&endpoint), &[], 1_000_100));
        match decision {
            BindDecision::Bound { binding, evidence } => {
                assert_eq!(binding.team_name, "session-51ea857f");
                assert_eq!(evidence, BindEvidence::Asserted);
                assert!(evidence.is_asserted());
            }
            other => panic!("expected an asserted binding, got {other:?}"),
        }
    }

    #[test]
    fn an_assertion_outranks_a_plausible_team_on_disk() {
        let teams = vec![team(
            "session-guess000",
            "guess000-0000-0000-0000-000000000000",
            1_000_000,
            vec![member(
                "team-lead",
                "leader",
                "in-process",
                "/home/dev/project",
            )],
        )];
        let endpoint = SessionEndpoint {
            session_id: "51ea857f-cb96-4372-ae75-bab1640c8428".to_string(),
            messaging_socket: None,
            messaging_token: None,
        };
        match decide_binding(&inputs(Some(&endpoint), &teams, 1_000_100)) {
            BindDecision::Bound { binding, evidence } => {
                assert_eq!(binding.team_name, "session-51ea857f");
                assert_eq!(evidence, BindEvidence::Asserted);
            }
            other => panic!("expected the assertion to win, got {other:?}"),
        }
    }

    #[test]
    fn with_no_assertion_the_documented_fallback_still_binds() {
        let teams = vec![team(
            "session-aaaa1111",
            "aaaa1111-0000-0000-0000-000000000000",
            1_000_000,
            vec![member(
                "team-lead",
                "leader",
                "in-process",
                "/home/dev/project",
            )],
        )];
        match decide_binding(&inputs(None, &teams, 1_000_100)) {
            BindDecision::Bound { binding, evidence } => {
                assert_eq!(binding.team_name, "session-aaaa1111");
                assert_eq!(evidence, BindEvidence::InferredLeadCwd);
                assert!(!evidence.is_asserted());
            }
            other => panic!("expected the fallback to bind, got {other:?}"),
        }
    }

    /// The audit's 4.2: the freshness window had a floor and no ceiling, so a
    /// session the user starts by hand in the same directory long after the
    /// spawn was eligible forever.
    #[test]
    fn a_team_created_long_after_the_spawn_is_no_longer_eligible() {
        let late = 1_000_000 + TEAM_MATCH_CEILING_MS + 1;
        let teams = vec![team(
            "session-later000",
            "later000-0000-0000-0000-000000000000",
            late,
            vec![member(
                "team-lead",
                "leader",
                "in-process",
                "/home/dev/project",
            )],
        )];
        assert!(matches!(
            decide_binding(&inputs(None, &teams, late)),
            BindDecision::Expired { .. }
        ));
    }

    #[test]
    fn a_team_created_before_the_spawn_window_is_still_not_adopted() {
        let teams = vec![team(
            "session-old00000",
            "old00000-0000-0000-0000-000000000000",
            1_000_000,
            vec![member(
                "team-lead",
                "leader",
                "in-process",
                "/home/dev/project",
            )],
        )];
        assert_eq!(
            match_team_window(&teams, 1_060_000, Path::new("/home/dev/project"), &[], &[]),
            None
        );
    }

    #[test]
    fn a_stale_team_naming_a_colliding_pane_id_is_not_adopted() {
        let teams = vec![team(
            "session-stale000",
            "stale000-0000-0000-0000-000000000000",
            1,
            vec![
                member("team-lead", "leader", "in-process", "/somewhere/else"),
                member("gate-mate", "w1:p2", "tmux", "/somewhere/else"),
            ],
        )];
        assert_eq!(
            match_team_window(
                &teams,
                9_999_999,
                Path::new("/home/dev/project"),
                &["w1:p2".to_string()],
                &[]
            ),
            None,
            "a pane id is not globally unique and must not override freshness"
        );
    }

    #[test]
    fn a_team_holding_one_of_our_panes_beats_a_cwd_match() {
        let teams = vec![
            team(
                "session-cwdonly0",
                "cwdonly0-0000-0000-0000-000000000000",
                1_000_000,
                vec![member(
                    "team-lead",
                    "leader",
                    "in-process",
                    "/home/dev/project",
                )],
            ),
            team(
                "session-ourpane0",
                "ourpane0-0000-0000-0000-000000000000",
                1_000_500,
                vec![
                    member("team-lead", "leader", "in-process", "/elsewhere"),
                    member("research", "w1:p4", "tmux", "/elsewhere"),
                ],
            ),
        ];
        let (binding, evidence) = match_team_window(
            &teams,
            1_000_000,
            Path::new("/home/dev/project"),
            &["w1:p4".to_string()],
            &[],
        )
        .expect("our own pane id is proof");
        assert_eq!(binding.team_name, "session-ourpane0");
        assert_eq!(evidence, BindEvidence::InferredOwnPane);
    }

    #[test]
    fn a_team_already_bound_to_another_run_is_never_re_adopted() {
        let teams = vec![team(
            "session-taken000",
            "taken000-0000-0000-0000-000000000000",
            1_000_000,
            vec![member(
                "team-lead",
                "leader",
                "in-process",
                "/home/dev/project",
            )],
        )];
        assert_eq!(
            match_team_window(
                &teams,
                1_000_000,
                Path::new("/home/dev/project"),
                &[],
                &["session-taken000".to_string()]
            ),
            None
        );
    }

    #[test]
    fn two_equally_plausible_teams_resolve_to_the_earlier_one_deterministically() {
        let teams = vec![
            team(
                "session-later000",
                "later000-0000-0000-0000-000000000000",
                1_000_900,
                vec![member(
                    "team-lead",
                    "leader",
                    "in-process",
                    "/home/dev/project",
                )],
            ),
            team(
                "session-first000",
                "first000-0000-0000-0000-000000000000",
                1_000_100,
                vec![member(
                    "team-lead",
                    "leader",
                    "in-process",
                    "/home/dev/project",
                )],
            ),
        ];
        let (binding, _) =
            match_team_window(&teams, 1_000_000, Path::new("/home/dev/project"), &[], &[])
                .expect("one of them matches");
        assert_eq!(binding.team_name, "session-first000");
    }

    /// The audit's other half of 4.2: a run whose team never appears used to
    /// poll forever, stay `running`, and wedge the single-live-run guard.
    #[test]
    fn a_run_that_never_binds_expires_instead_of_polling_forever() {
        assert_eq!(
            decide_binding(&inputs(None, &[], 1_000_100)),
            BindDecision::Waiting
        );
        let deadline = 1_000_000 + BIND_DEADLINE.as_millis() as u64;
        assert_eq!(
            decide_binding(&inputs(None, &[], deadline)),
            BindDecision::Expired {
                waited_ms: BIND_DEADLINE.as_millis() as u64
            }
        );
    }

    /// The deadline is a parameter so the expiry rule can be exercised in
    /// milliseconds, and so a support hatch can shorten it without a rebuild.
    #[test]
    fn the_deadline_is_an_input_rather_than_a_hardcoded_two_minutes() {
        let mut short = inputs(None, &[], 1_000_500);
        short.deadline = Duration::from_millis(400);
        assert_eq!(
            decide_binding(&short),
            BindDecision::Expired { waited_ms: 500 }
        );
        let mut long = inputs(None, &[], 1_000_500);
        long.deadline = Duration::from_secs(3600);
        assert_eq!(decide_binding(&long), BindDecision::Waiting);
    }

    #[test]
    fn the_unbound_failure_names_the_wait_and_the_next_step() {
        let message = unbound_failure_reason(120_000);
        assert!(message.contains("120s"));
        assert!(message.contains("folder-trust"));
    }
}
