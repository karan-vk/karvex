//! The observation contract: Claude Code's own team/task files → karvex run
//! records.
//!
//! `09-agent-teams-rework.md` §3.4. Karvex no longer executes a workflow, so
//! the only way it knows what a run is doing is by reading two directories
//! Claude Code owns: `~/.claude/tasks/<team>/*.json` (one file per task) and
//! `~/.claude/teams/<team>/config.json` (the member roster). Those are a
//! foreign, experimental format on someone else's release cadence, which is
//! exactly why the parsing and diffing live here, alone, behind fixture tests
//! pinned to bytes captured live from Claude Code 2.1.226. When upstream adds
//! a field or renames a status, one module and one test file move.
//!
//! Pure by construction, in the sense `workflow::model`, `workflow::tier`, and
//! `workflow::lead_prompt` are: bytes and prior state in, typed values out. No
//! filesystem walking, no polling cadence, no store, no events. The caller
//! owns the IO and turns a [`ProjectionDelta`] into store writes — which is
//! why the delta must be *empty* when nothing changed, and deterministically
//! ordered when something did. A 2s poller that re-emitted its whole view
//! every tick would write to the store forever.
//!
//! Matching an observed task back to a definition node is *not* done here: it
//! is the other half of the render contract and lives with it, in
//! [`crate::workflow::lead_prompt::subject_node_key`].

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

use crate::workflow::lead_prompt::subject_node_key;
use crate::workflow::model::{InstancePath, NodeKey};

/// The `tmuxPaneId` Claude Code writes for the team lead. It is a sentinel,
/// not a pane: the lead runs in the session that spawned the team, so it has
/// no pane of its own in the team's own accounting.
pub const LEAD_PANE_SENTINEL: &str = "leader";

/// The reserved instance-path namespace for tasks the lead invented. The
/// leading `.` cannot collide with a definition node key, because a key is an
/// author-chosen identifier and karvex owns everything under `.`.
pub const EMERGENT_PATH_PREFIX: &str = ".task/";

// ── observed values ────────────────────────────────────────────────────────

/// Claude Code's own task status vocabulary. `Unknown` keeps a status string
/// this karvex does not know from failing the whole poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Unknown(String),
}

impl TaskStatus {
    /// Observed values as of 2.1.226 are `pending`, `in_progress`, and
    /// `completed`. Anything else — including a status field that is missing
    /// entirely — is [`TaskStatus::Unknown`] rather than a parse failure or an
    /// invented default: karvex never guesses progress on the team's behalf.
    pub fn parse(value: &str) -> Self {
        match value {
            "pending" => Self::Pending,
            "in_progress" => Self::InProgress,
            "completed" => Self::Completed,
            other => Self::Unknown(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Unknown(other) => other,
        }
    }
}

/// One task file under `~/.claude/tasks/<team>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedTask {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub active_form: Option<String>,
    pub owner: Option<String>,
    pub status: TaskStatus,
    pub blocks: Vec<String>,
    pub blocked_by: Vec<String>,
}

/// One `members[]` entry of `~/.claude/teams/<team>/config.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedMember {
    pub name: String,
    pub agent_id: Option<String>,
    pub agent_type: String,
    pub model: Option<String>,
    /// A karvex public pane id for a tmux-backed teammate; the literal
    /// `"leader"` for the lead, which is not a pane at all.
    pub pane_id: Option<String>,
    pub backend_type: String,
    pub is_active: bool,
    pub cwd: Option<String>,
    pub joined_at_unix_ms: Option<u64>,
}

impl ObservedMember {
    /// Whether this member is the team lead.
    ///
    /// The authoritative marker is the [`LEAD_PANE_SENTINEL`] pane id.
    /// `backendType` alone is *not* usable: an in-process **teammate** carries
    /// `"in-process"` too, and telling those apart is the entire point of
    /// [`ObservedTeam::split_pane_confirmed`]. The `agentType` clause is a
    /// narrow fallback for the day upstream renames the sentinel — it only
    /// fires for an in-process member that also calls itself `team-lead`.
    pub fn is_lead(&self) -> bool {
        self.pane_id.as_deref() == Some(LEAD_PANE_SENTINEL)
            || (self.backend_type == "in-process" && self.agent_type == "team-lead")
    }

    /// The karvex public pane id this member's session runs in, if it has one.
    ///
    /// `Some` only for a tmux-backed member whose pane id is a real pane, so
    /// the lead's `"leader"` sentinel and every in-process teammate answer
    /// `None`.
    pub fn tmux_pane_id(&self) -> Option<&str> {
        if self.backend_type != "tmux" {
            return None;
        }
        match self.pane_id.as_deref() {
            Some(LEAD_PANE_SENTINEL) | None => None,
            Some(pane) => Some(pane),
        }
    }
}

/// A parsed `~/.claude/teams/<team>/config.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedTeam {
    pub name: String,
    pub lead_session_id: String,
    pub created_at_unix_ms: u64,
    pub members: Vec<ObservedMember>,
}

impl ObservedTeam {
    /// The working directory the lead session was launched in.
    pub fn lead_cwd(&self) -> Option<&str> {
        self.members
            .iter()
            .find(|member| member.is_lead())
            .and_then(|member| member.cwd.as_deref())
    }

    /// Whether split-pane (tmux-backend) teammate mode actually took.
    ///
    /// The post-spawn assertion from §3.1: Claude Code's default teammate mode
    /// is `in-process` even inside tmux, and in-process teammates do not
    /// survive `/resume`, so karvex forces tmux mode and then has to *check*.
    /// The check has to scan every member — `members[0]` is always the lead,
    /// which is legitimately in-process, so inspecting it alone would report
    /// failure for a perfectly healthy split-pane team.
    pub fn split_pane_confirmed(&self) -> bool {
        self.members
            .iter()
            .any(|member| member.tmux_pane_id().is_some())
    }
}

// ── errors ─────────────────────────────────────────────────────────────────

/// Why a task or team file could not be turned into an observed value.
///
/// Deliberately tiny: an unreadable file is a transient fact about one poll,
/// not a run failure, and the caller's job is to log it and try again on the
/// next tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionError {
    /// The bytes are not the JSON shape expected.
    Json(String),
    /// The JSON parsed, but a field karvex cannot proceed without is absent.
    MissingField(&'static str),
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(message) => write!(f, "malformed JSON: {message}"),
            Self::MissingField(field) => write!(f, "missing required field `{field}`"),
        }
    }
}

impl std::error::Error for ProjectionError {}

// ── wire structs ───────────────────────────────────────────────────────────
//
// No `deny_unknown_fields` anywhere below, on purpose: agent teams are
// experimental and will gain fields. An unknown field is not an error, it is
// a field karvex has not learned yet.

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TaskWire {
    id: Option<String>,
    subject: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    active_form: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    blocks: Vec<String>,
    #[serde(default)]
    blocked_by: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TeamWire {
    name: Option<String>,
    lead_session_id: Option<String>,
    created_at: Option<u64>,
    #[serde(default)]
    members: Vec<MemberWire>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MemberWire {
    name: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    agent_type: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tmux_pane_id: Option<String>,
    #[serde(default)]
    backend_type: String,
    #[serde(default)]
    is_active: bool,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    joined_at: Option<u64>,
}

// ── parsers ────────────────────────────────────────────────────────────────

/// Parses one `~/.claude/tasks/<team>/<n>.json` file.
///
/// `id` and `subject` are required — without them a task has neither identity
/// nor a way back to a definition node. Everything else is optional, because
/// everything else genuinely is: `owner` is absent until a teammate claims the
/// task, and `activeForm` is absent on tasks the lead wrote tersely.
pub fn parse_task(bytes: &[u8]) -> Result<ObservedTask, ProjectionError> {
    let wire: TaskWire =
        serde_json::from_slice(bytes).map_err(|err| ProjectionError::Json(err.to_string()))?;
    let id = wire.id.ok_or(ProjectionError::MissingField("id"))?;
    let subject = wire
        .subject
        .ok_or(ProjectionError::MissingField("subject"))?;
    Ok(ObservedTask {
        id,
        subject,
        description: wire.description,
        active_form: wire.active_form,
        owner: wire.owner,
        status: wire
            .status
            .as_deref()
            .map(TaskStatus::parse)
            .unwrap_or_else(|| TaskStatus::Unknown(String::new())),
        blocks: wire.blocks,
        blocked_by: wire.blocked_by,
    })
}

/// Parses `~/.claude/teams/<team>/config.json`.
///
/// `name`, `leadSessionId`, and `createdAt` are required: they are the run's
/// binding to the Claude Code session, and a config without them cannot be
/// snapshotted into a run record usefully. A member without a `name` is the
/// same kind of hole, since the name is what labels its pane.
pub fn parse_team_config(bytes: &[u8]) -> Result<ObservedTeam, ProjectionError> {
    let wire: TeamWire =
        serde_json::from_slice(bytes).map_err(|err| ProjectionError::Json(err.to_string()))?;
    let name = wire.name.ok_or(ProjectionError::MissingField("name"))?;
    let lead_session_id = wire
        .lead_session_id
        .ok_or(ProjectionError::MissingField("leadSessionId"))?;
    let created_at_unix_ms = wire
        .created_at
        .ok_or(ProjectionError::MissingField("createdAt"))?;
    let mut members = Vec::with_capacity(wire.members.len());
    for member in wire.members {
        members.push(ObservedMember {
            name: member
                .name
                .ok_or(ProjectionError::MissingField("members[].name"))?,
            agent_id: member.agent_id,
            agent_type: member.agent_type,
            model: member.model,
            pane_id: member.tmux_pane_id,
            backend_type: member.backend_type,
            is_active: member.is_active,
            cwd: member.cwd,
            joined_at_unix_ms: member.joined_at,
        });
    }
    Ok(ObservedTeam {
        name,
        lead_session_id,
        created_at_unix_ms,
        members,
    })
}

// ── the diff ───────────────────────────────────────────────────────────────

/// One task, resolved against the run's definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskProjection {
    pub task: ObservedTask,
    /// The definition node this task belongs to, matched by subject prefix.
    pub node_key: Option<NodeKey>,
    /// `false` when `node_key` is `Some`: the definition planned this one.
    pub emergent: bool,
    /// The instance path karvex records this task under: the node key for a
    /// planned task, the reserved `.task/<id>` namespace for an emergent one.
    pub path: InstancePath,
    /// `blockedBy` resolved from Claude Code task ids to instance paths,
    /// dropping ids that name no observed task.
    pub blocked_by: Vec<InstancePath>,
    /// Set when `task.owner` differs from what karvex last recorded for this
    /// same task id — never on the task's first observation, which is an
    /// initial claim, not a reassignment (`phase4-retarget-plan.md` amendment
    /// log, WI-R6). The caller journals this as the fact it is: which task,
    /// from whom, to whom. Karvex can see that ownership moved; it cannot see
    /// why, so that is all this carries.
    pub owner_change: Option<ObservedOwnerChange>,
}

/// A task's `owner` moved between members (or in or out of being unclaimed)
/// since the last poll. `None` on either side means *unclaimed*, matching
/// `ObservedTask::owner` itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedOwnerChange {
    pub from: Option<String>,
    pub to: Option<String>,
}

/// What karvex's own per-pane knowledge adds to a team member, as one poll
/// observed it.
///
/// Everything here is a fact about the member's *session*, and none of it
/// exists in Claude Code's own team state: `members[]` carries `agentId`
/// (`<name>@<team>`) and nothing else that identifies a session
/// (`phase4-retarget-plan.md` §1.4c). Karvex learns it from the pane the
/// member runs in — its own `SessionStart` self-report, or the bundled hook's
/// report that karvex already lands on `PaneState.agent_session` (S1) — and
/// this is the shape the adapter hands in.
///
/// Every field is optional and *absence means "not resolved this poll"*, never
/// "resolved to nothing": [`ProjectionSnapshot::absorb`] merges an observation
/// over what it already knew rather than replacing it, so a pane that has
/// closed cannot erase the session id captured while it was alive. That
/// durability is the entire reason this packet exists — a review that happens
/// tomorrow needs the session id of a pane that died today.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservedMemberIdentity {
    /// The member's own Claude Code session id.
    pub session_id: Option<String>,
    /// Where Claude Code writes this session's transcript. Recorded rather
    /// than re-derived on read because the derivation rule is Claude Code's,
    /// not karvex's, and it can change under us.
    pub transcript_path: Option<String>,
    /// The member pane's agent state right now, in the vocabulary the caller
    /// publishes. Free text here on purpose: this module has no opinion about
    /// karvex's own detection vocabulary and must not acquire one.
    pub last_state: Option<String>,
}

/// [`ObservedMemberIdentity`] as the snapshot has accumulated it: best known
/// value per field, plus the moment `last_state` was first seen to hold its
/// current value.
///
/// `last_state_at_unix_ms` is stamped here rather than by the caller because
/// this is where the previous state lives — the caller would have to keep a
/// second copy of the snapshot to know whether the state it just read is a
/// change or a repeat.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberIdentity {
    pub session_id: Option<String>,
    pub transcript_path: Option<String>,
    pub last_state: Option<String>,
    pub last_state_at_unix_ms: Option<u64>,
}

impl MemberIdentity {
    /// Folds one poll's observation over what is already known.
    ///
    /// Never regresses a resolved field to `None`, which is the same rule the
    /// store's `run_member` writer enforces on its own side (P7). Two layers
    /// hold it on purpose: the store's rule protects the row from a write, and
    /// this one stops the write from being *made*, which is what keeps an
    /// unchanged poll silent.
    fn merged(&self, observed: &ObservedMemberIdentity, now_unix_ms: u64) -> Self {
        let last_state = observed
            .last_state
            .clone()
            .or_else(|| self.last_state.clone());
        // Only a *changed* state re-stamps the clock. A member observed idle
        // for forty minutes must read as one forty-minute idle, not as the
        // most recent two-second poll.
        let last_state_at_unix_ms = if last_state == self.last_state {
            self.last_state_at_unix_ms
        } else {
            Some(now_unix_ms)
        };
        Self {
            session_id: observed
                .session_id
                .clone()
                .or_else(|| self.session_id.clone()),
            transcript_path: observed
                .transcript_path
                .clone()
                .or_else(|| self.transcript_path.clone()),
            last_state,
            last_state_at_unix_ms,
        }
    }
}

/// One member, resolved against what karvex knows about the pane it runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberProjection {
    pub member: ObservedMember,
    pub identity: MemberIdentity,
}

/// What one poll observed, relative to what karvex last recorded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectionDelta {
    pub tasks: Vec<TaskProjection>,
    pub members: Vec<MemberProjection>,
}

impl ProjectionDelta {
    /// Nothing changed since the last poll, so the caller writes nothing and
    /// emits nothing.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty() && self.members.is_empty()
    }
}

/// The comparable half of a task: exactly the fields whose change is worth a
/// store write and an event. `description`, `activeForm`, and `blocks` are
/// deliberately excluded — `blocks` is the inverse of `blockedBy` and would
/// double-report every edge change, and the other two are prose the lead
/// rewrites freely.
///
/// `blocked_by` is stored *resolved*, not as raw ids, so that a task also
/// re-reports when a blocker it names is deleted or re-subjected: the edge it
/// projects changed even though its own bytes did not.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskFingerprint {
    subject: String,
    owner: Option<String>,
    status: TaskStatus,
    blocked_by: Vec<InstancePath>,
}

/// The comparable half of a member: identity, placement, and liveness. The
/// lead's `prompt` and `subscriptions` are not projected at all.
///
/// `identity` is here for the same reason the rest of it is: a teammate whose
/// session id has just been resolved is a member whose durable record changed,
/// even though the team config's bytes did not. Carrying it in the fingerprint
/// is what makes that resolution produce exactly one store write instead of
/// either none or one per poll.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MemberFingerprint {
    agent_type: String,
    model: Option<String>,
    pane_id: Option<String>,
    backend_type: String,
    is_active: bool,
    cwd: Option<String>,
    identity: MemberIdentity,
}

/// A member karvex resolved nothing new about on this poll.
const EMPTY_IDENTITY: ObservedMemberIdentity = ObservedMemberIdentity {
    session_id: None,
    transcript_path: None,
    last_state: None,
};

/// Everything karvex has already recorded, so a poll that observed nothing new
/// produces an empty delta and therefore no store writes and no events.
///
/// Holds only fingerprints, never whole observations, so the caller can keep
/// one per live run cheaply.
#[derive(Debug, Clone, Default)]
pub struct ProjectionSnapshot {
    tasks: BTreeMap<String, TaskFingerprint>,
    members: BTreeMap<String, MemberFingerprint>,
}

impl ProjectionSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Which member is running in a given karvex pane, by the name the team
    /// config gave it.
    ///
    /// The join between the two identity spaces a run has: Claude Code names a
    /// teammate, karvex names a pane, and the team config's `tmuxPaneId` is the
    /// only place the two meet. `None` for a pane no member occupies, and for
    /// the in-process lead, whose `tmuxPaneId` is the `"leader"` sentinel rather
    /// than a pane.
    pub fn member_name_for_pane(&self, pane_id: &str) -> Option<&str> {
        if pane_id.is_empty() || pane_id == LEAD_PANE_SENTINEL {
            return None;
        }
        self.members
            .iter()
            .find(|(_, fingerprint)| {
                fingerprint.backend_type == "tmux"
                    && fingerprint.pane_id.as_deref() == Some(pane_id)
            })
            .map(|(name, _)| name.as_str())
    }

    /// Folds one poll's observations in, returning only what changed.
    ///
    /// Deterministic by contract, because the caller turns the result straight
    /// into ordered store writes and events: tasks come out in numeric id
    /// order where the ids are numbers (which they are, being filenames
    /// `1.json`, `2.json`, …) and lexicographic order otherwise, with numeric
    /// ids sorting ahead of non-numeric ones; members come out by name.
    ///
    /// A task that vanishes from the source directory produces no delta entry.
    /// Deletion is not part of the projection — the run record is append-only,
    /// and a deleted task is still something the team did.
    ///
    /// `identities` is keyed by member name and carries what karvex knows about
    /// each member's *session* — the half Claude Code's files never hold
    /// (§1.4c). A name the map does not mention is a member karvex has resolved
    /// nothing new about this poll, which is not the same as a member it has
    /// resolved nothing about: see [`MemberIdentity::merged`].
    pub fn absorb(
        &mut self,
        tasks: &[ObservedTask],
        team: Option<&ObservedTeam>,
        node_keys: &[NodeKey],
        identities: &BTreeMap<String, ObservedMemberIdentity>,
        now_unix_ms: u64,
    ) -> ProjectionDelta {
        let mut delta = ProjectionDelta::default();

        let mut ordered: Vec<&ObservedTask> = tasks.iter().collect();
        ordered.sort_by(|left, right| task_order(&left.id).cmp(&task_order(&right.id)));

        // Resolve every observed id to a path first: `blockedBy` names peers,
        // including ones that sort after the task naming them.
        let paths: BTreeMap<&str, InstancePath> = ordered
            .iter()
            .map(|task| (task.id.as_str(), resolve(task, node_keys).1))
            .collect();

        for task in ordered {
            let (node_key, path) = resolve(task, node_keys);
            let blocked_by: Vec<InstancePath> = task
                .blocked_by
                .iter()
                .filter_map(|id| paths.get(id.as_str()).cloned())
                .collect();
            let fingerprint = TaskFingerprint {
                subject: task.subject.clone(),
                owner: task.owner.clone(),
                status: task.status.clone(),
                blocked_by: blocked_by.clone(),
            };
            let known = self.tasks.get(&task.id).cloned();
            if known.as_ref() == Some(&fingerprint) {
                continue;
            }
            // Only a task karvex has already fingerprinted can have been
            // *reassigned* — the first sighting of a task is an initial
            // claim, not a change of hands, even when it already carries an
            // owner.
            let owner_change = known
                .as_ref()
                .filter(|previous| previous.owner != fingerprint.owner)
                .map(|previous| ObservedOwnerChange {
                    from: previous.owner.clone(),
                    to: fingerprint.owner.clone(),
                });
            self.tasks.insert(task.id.clone(), fingerprint);
            delta.tasks.push(TaskProjection {
                task: task.clone(),
                emergent: node_key.is_none(),
                node_key,
                path,
                blocked_by,
                owner_change,
            });
        }

        if let Some(team) = team {
            let mut members: Vec<&ObservedMember> = team.members.iter().collect();
            members.sort_by(|left, right| left.name.cmp(&right.name));
            for member in members {
                let known = self.members.get(&member.name).cloned();
                let identity = known
                    .as_ref()
                    .map(|fingerprint| fingerprint.identity.clone())
                    .unwrap_or_default()
                    .merged(
                        identities.get(&member.name).unwrap_or(&EMPTY_IDENTITY),
                        now_unix_ms,
                    );
                let fingerprint = MemberFingerprint {
                    agent_type: member.agent_type.clone(),
                    model: member.model.clone(),
                    pane_id: member.pane_id.clone(),
                    backend_type: member.backend_type.clone(),
                    is_active: member.is_active,
                    cwd: member.cwd.clone(),
                    identity,
                };
                if known.as_ref() == Some(&fingerprint) {
                    continue;
                }
                let identity = fingerprint.identity.clone();
                self.members.insert(member.name.clone(), fingerprint);
                delta.members.push(MemberProjection {
                    member: member.clone(),
                    identity,
                });
            }
        }

        delta
    }
}

/// Which definition node an observed task belongs to, and the instance path it
/// is recorded under.
fn resolve(task: &ObservedTask, node_keys: &[NodeKey]) -> (Option<NodeKey>, InstancePath) {
    match subject_node_key(&task.subject, node_keys) {
        Some(key) => {
            let path = InstancePath::new(key.as_str());
            (Some(key), path)
        }
        None => (None, emergent_path(&task.id)),
    }
}

/// The reserved path for a task the definition never planned.
///
/// The id comes from a filename in a directory karvex does not own, so it is
/// untrusted input on a path-shaped value. Every character outside
/// `[A-Za-z0-9_-]` is replaced with `_` rather than rejected: replacing keeps
/// every observed task projectable, where rejecting would silently drop work
/// the team actually did. `/`, `.`, and `..` therefore cannot appear, so no id
/// can escape the `.task/` namespace or forge a definition node's path. Two
/// pathological ids can collide onto one path; real ids are `1`, `2`, `3`.
fn emergent_path(id: &str) -> InstancePath {
    let mut sanitised = String::with_capacity(id.len());
    for ch in id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            sanitised.push(ch);
        } else {
            sanitised.push('_');
        }
    }
    if sanitised.is_empty() {
        sanitised.push('_');
    }
    InstancePath::new(format!("{EMERGENT_PATH_PREFIX}{sanitised}"))
}

/// Sort key giving numeric task ids their numeric order and everything else a
/// stable lexicographic one, with numbers first.
fn task_order(id: &str) -> (u8, u128, &str) {
    match id.parse::<u128>() {
        Ok(number) => (0, number, id),
        Err(_) => (1, 0, id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── fixtures, captured live from Claude Code 2.1.226 ───────────────────

    /// A planned task, claimed by a teammate and finished.
    const TASK_CLAIMED: &str = r#"{
  "id": "1",
  "subject": "research: Survey options",
  "description": "Survey the available options (research task). Blocked by nothing.",
  "activeForm": "Surveying options",
  "owner": "research",
  "status": "completed",
  "blocks": ["2"],
  "blockedBy": []
}"#;

    /// A planned task, blocked by the first one and not yet claimed.
    const TASK_BLOCKED: &str = r#"{
  "id": "2",
  "subject": "build: Implement it",
  "description": "Build the thing (build task). Blocked by task 1.",
  "activeForm": "Building it",
  "owner": "build",
  "status": "in_progress",
  "blocks": [],
  "blockedBy": ["1"]
}"#;

    /// The regression that matters: `owner` is *absent*, not null, on a task
    /// nobody has claimed. And this one has no `node-id:` prefix, so it is
    /// emergent.
    const TASK_UNCLAIMED: &str = r#"{
  "id": "3",
  "subject": "an unplanned cleanup task",
  "description": "Unplanned cleanup work. Blocked by nothing.",
  "activeForm": "Doing unplanned cleanup",
  "status": "pending",
  "blocks": [],
  "blockedBy": []
}"#;

    /// A real two-member config: the lead is in-process with the `"leader"`
    /// sentinel, and only the teammate proves tmux mode.
    const TEAM_CONFIG: &str = r#"{
  "name": "session-3cb241fe",
  "createdAt": 1786376746139,
  "leadAgentId": "team-lead@session-3cb241fe",
  "leadSessionId": "3cb241fe-2c3a-4dd8-b8a0-5dd83dfc5aa2",
  "members": [
    { "agentId": "team-lead@session-3cb241fe", "name": "team-lead", "agentType": "team-lead",
      "joinedAt": 1786376746139, "tmuxPaneId": "leader",
      "cwd": "/tmp/kvx-teams-live-3620431", "subscriptions": [], "backendType": "in-process" },
    { "agentId": "research@session-3cb241fe", "name": "research", "color": "blue",
      "joinedAt": 1786376797068, "tmuxPaneId": "w1:p4", "subscriptions": [],
      "agentType": "Explore", "model": "sonnet",
      "prompt": "You are a teammate named …", "planModeRequired": false,
      "cwd": "/tmp/kvx-teams-live-3620431", "backendType": "tmux", "isActive": false }
  ]
}"#;

    /// The same config a moment after spawn, before any teammate exists — or
    /// after split-pane mode silently failed to take.
    const TEAM_CONFIG_LEAD_ONLY: &str = r#"{
  "name": "session-3cb241fe",
  "createdAt": 1786376746139,
  "leadAgentId": "team-lead@session-3cb241fe",
  "leadSessionId": "3cb241fe-2c3a-4dd8-b8a0-5dd83dfc5aa2",
  "members": [
    { "agentId": "team-lead@session-3cb241fe", "name": "team-lead", "agentType": "team-lead",
      "joinedAt": 1786376746139, "tmuxPaneId": "leader",
      "cwd": "/tmp/kvx-teams-live-3620431", "subscriptions": [], "backendType": "in-process" }
  ]
}"#;

    fn task(json: &str) -> ObservedTask {
        parse_task(json.as_bytes()).expect("fixture task parses")
    }

    fn team(json: &str) -> ObservedTeam {
        parse_team_config(json.as_bytes()).expect("fixture team config parses")
    }

    fn keys() -> Vec<NodeKey> {
        vec![NodeKey::new("research"), NodeKey::new("build")]
    }

    // ── task parsing ───────────────────────────────────────────────────────

    #[test]
    fn parse_task_reads_a_claimed_task() {
        let task = task(TASK_CLAIMED);
        assert_eq!(task.id, "1");
        assert_eq!(task.subject, "research: Survey options");
        assert!(task.description.starts_with("Survey the available options"));
        assert_eq!(task.active_form.as_deref(), Some("Surveying options"));
        assert_eq!(task.owner.as_deref(), Some("research"));
        assert_eq!(task.status, TaskStatus::Completed);
        assert_eq!(task.blocks, vec!["2".to_string()]);
        assert!(task.blocked_by.is_empty());
    }

    #[test]
    fn parse_task_leaves_owner_none_when_the_task_is_unclaimed() {
        let task = task(TASK_UNCLAIMED);
        assert_eq!(task.owner, None);
        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(task.subject, "an unplanned cleanup task");
    }

    #[test]
    fn parse_task_keeps_an_unrecognised_status_as_unknown() {
        let json = r#"{ "id": "9", "subject": "x", "status": "abandoned" }"#;
        let task = task(json);
        assert_eq!(task.status, TaskStatus::Unknown("abandoned".to_string()));
        assert_eq!(task.status.as_str(), "abandoned");
    }

    #[test]
    fn parse_task_treats_an_absent_status_as_unknown_rather_than_pending() {
        let task = task(r#"{ "id": "9", "subject": "x" }"#);
        assert_eq!(task.status, TaskStatus::Unknown(String::new()));
    }

    #[test]
    fn parse_task_ignores_unknown_fields() {
        let json = r#"{
  "id": "4",
  "subject": "build: Implement it",
  "status": "pending",
  "priority": "high",
  "labels": ["new", "upstream"],
  "metadata": { "attempts": 2, "nested": { "deep": true } }
}"#;
        let task = task(json);
        assert_eq!(task.id, "4");
        assert_eq!(task.status, TaskStatus::Pending);
    }

    #[test]
    fn parse_task_rejects_malformed_json_without_panicking() {
        let err = parse_task(b"{ not json").expect_err("malformed JSON is an error");
        assert!(matches!(err, ProjectionError::Json(_)));
        assert!(err.to_string().starts_with("malformed JSON: "));
    }

    #[test]
    fn parse_task_reports_a_missing_id_as_a_missing_field() {
        let err = parse_task(br#"{ "subject": "x" }"#).expect_err("id is required");
        assert_eq!(err, ProjectionError::MissingField("id"));
        let err = parse_task(br#"{ "id": "1" }"#).expect_err("subject is required");
        assert_eq!(err, ProjectionError::MissingField("subject"));
    }

    // ── team config parsing ────────────────────────────────────────────────

    #[test]
    fn parse_team_config_reads_the_lead_and_the_teammate() {
        let team = team(TEAM_CONFIG);
        assert_eq!(team.name, "session-3cb241fe");
        assert_eq!(team.lead_session_id, "3cb241fe-2c3a-4dd8-b8a0-5dd83dfc5aa2");
        assert_eq!(team.created_at_unix_ms, 1_786_376_746_139);
        assert_eq!(team.members.len(), 2);

        let lead = &team.members[0];
        assert_eq!(lead.name, "team-lead");
        assert_eq!(lead.agent_type, "team-lead");
        assert_eq!(lead.backend_type, "in-process");
        assert_eq!(lead.pane_id.as_deref(), Some("leader"));
        assert_eq!(lead.model, None);
        assert!(!lead.is_active);
        assert!(lead.is_lead());
        assert_eq!(lead.joined_at_unix_ms, Some(1_786_376_746_139));

        let mate = &team.members[1];
        assert_eq!(mate.name, "research");
        assert_eq!(mate.agent_type, "Explore");
        assert_eq!(mate.model.as_deref(), Some("sonnet"));
        assert_eq!(mate.backend_type, "tmux");
        assert_eq!(mate.agent_id.as_deref(), Some("research@session-3cb241fe"));
        assert!(!mate.is_lead());

        assert_eq!(team.lead_cwd(), Some("/tmp/kvx-teams-live-3620431"));
    }

    #[test]
    fn split_pane_confirmed_is_true_for_a_real_two_member_team() {
        assert!(team(TEAM_CONFIG).split_pane_confirmed());
    }

    #[test]
    fn split_pane_confirmed_is_false_when_only_the_lead_is_present() {
        // The exact assertion that catches the in-process default winning:
        // members[0] is in-process even in a healthy team, so the check must
        // look for a *teammate* with a real pane.
        assert!(!team(TEAM_CONFIG_LEAD_ONLY).split_pane_confirmed());
    }

    #[test]
    fn split_pane_confirmed_is_false_when_the_teammate_stayed_in_process() {
        let json = TEAM_CONFIG.replace(
            r#""cwd": "/tmp/kvx-teams-live-3620431", "backendType": "tmux", "isActive": false"#,
            r#""cwd": "/tmp/kvx-teams-live-3620431", "backendType": "in-process", "isActive": false"#,
        );
        let team = team(&json);
        assert_eq!(team.members.len(), 2);
        assert!(!team.split_pane_confirmed());
    }

    #[test]
    fn tmux_pane_id_is_none_for_the_lead_and_some_for_the_teammate() {
        let team = team(TEAM_CONFIG);
        assert_eq!(team.members[0].tmux_pane_id(), None);
        assert_eq!(team.members[1].tmux_pane_id(), Some("w1:p4"));
    }

    #[test]
    fn parse_team_config_ignores_unknown_fields_and_defaults_an_empty_roster() {
        let json = r#"{
  "name": "session-x",
  "createdAt": 1,
  "leadSessionId": "s",
  "somethingUpstreamAddedLater": { "a": [1, 2, 3] }
}"#;
        let team = team(json);
        assert!(team.members.is_empty());
        assert_eq!(team.lead_cwd(), None);
        assert!(!team.split_pane_confirmed());
    }

    #[test]
    fn parse_team_config_rejects_malformed_json_and_reports_missing_fields() {
        let err = parse_team_config(b"[]").expect_err("an array is not a team config");
        assert!(matches!(err, ProjectionError::Json(_)));
        let err = parse_team_config(br#"{ "name": "x", "createdAt": 1 }"#)
            .expect_err("leadSessionId is required");
        assert_eq!(err, ProjectionError::MissingField("leadSessionId"));
    }

    // ── the diff ───────────────────────────────────────────────────────────

    #[test]
    fn absorb_matches_planned_tasks_and_first_classes_the_emergent_one() {
        let tasks = vec![task(TASK_CLAIMED), task(TASK_BLOCKED), task(TASK_UNCLAIMED)];
        let mut snapshot = ProjectionSnapshot::new();
        let delta = snapshot.absorb(&tasks, None, &keys(), &BTreeMap::new(), 0);

        assert_eq!(delta.tasks.len(), 3);
        assert!(delta.members.is_empty());

        let first = &delta.tasks[0];
        assert_eq!(first.task.id, "1");
        assert_eq!(first.node_key, Some(NodeKey::new("research")));
        assert!(!first.emergent);
        assert_eq!(first.path, InstancePath::new("research"));

        let second = &delta.tasks[1];
        assert_eq!(second.task.id, "2");
        assert_eq!(second.node_key, Some(NodeKey::new("build")));
        assert!(!second.emergent);
        assert_eq!(second.path, InstancePath::new("build"));

        let third = &delta.tasks[2];
        assert_eq!(third.task.id, "3");
        assert_eq!(third.node_key, None);
        assert!(third.emergent);
        assert_eq!(third.path, InstancePath::new(".task/3"));
    }

    #[test]
    fn absorb_resolves_blocked_by_to_instance_paths() {
        let tasks = vec![task(TASK_CLAIMED), task(TASK_BLOCKED), task(TASK_UNCLAIMED)];
        let mut snapshot = ProjectionSnapshot::new();
        let delta = snapshot.absorb(&tasks, None, &keys(), &BTreeMap::new(), 0);

        assert!(delta.tasks[0].blocked_by.is_empty());
        assert_eq!(
            delta.tasks[1].blocked_by,
            vec![InstancePath::new("research")]
        );
        assert!(delta.tasks[2].blocked_by.is_empty());
    }

    #[test]
    fn absorb_drops_blocked_by_ids_that_name_no_observed_task() {
        // The lead deleted task 1; task 2 still names it.
        let tasks = vec![task(TASK_BLOCKED)];
        let mut snapshot = ProjectionSnapshot::new();
        let delta = snapshot.absorb(&tasks, None, &keys(), &BTreeMap::new(), 0);
        assert_eq!(delta.tasks.len(), 1);
        assert!(delta.tasks[0].blocked_by.is_empty());
    }

    #[test]
    fn absorb_is_idempotent_for_identical_input() {
        let tasks = vec![task(TASK_CLAIMED), task(TASK_BLOCKED), task(TASK_UNCLAIMED)];
        let team = team(TEAM_CONFIG);
        let mut snapshot = ProjectionSnapshot::new();

        let first = snapshot.absorb(&tasks, Some(&team), &keys(), &BTreeMap::new(), 0);
        assert_eq!(first.tasks.len(), 3);
        assert_eq!(first.members.len(), 2);

        // This is what stops the 2s poller writing to the store forever.
        let second = snapshot.absorb(&tasks, Some(&team), &keys(), &BTreeMap::new(), 0);
        assert_eq!(second, ProjectionDelta::default());
        assert!(second.is_empty());
        let third = snapshot.absorb(&tasks, Some(&team), &keys(), &BTreeMap::new(), 0);
        assert!(third.is_empty());
    }

    #[test]
    fn absorb_reports_only_the_task_whose_status_changed() {
        let mut tasks = vec![task(TASK_CLAIMED), task(TASK_BLOCKED), task(TASK_UNCLAIMED)];
        let mut snapshot = ProjectionSnapshot::new();
        assert_eq!(
            snapshot
                .absorb(&tasks, None, &keys(), &BTreeMap::new(), 0)
                .tasks
                .len(),
            3
        );

        tasks[1].status = TaskStatus::Completed;
        let delta = snapshot.absorb(&tasks, None, &keys(), &BTreeMap::new(), 0);
        assert_eq!(delta.tasks.len(), 1);
        assert_eq!(delta.tasks[0].task.id, "2");
        assert_eq!(delta.tasks[0].task.status, TaskStatus::Completed);
        assert!(delta.members.is_empty());
    }

    #[test]
    fn absorb_ignores_a_description_only_edit() {
        let mut tasks = vec![task(TASK_CLAIMED)];
        let mut snapshot = ProjectionSnapshot::new();
        assert_eq!(
            snapshot
                .absorb(&tasks, None, &keys(), &BTreeMap::new(), 0)
                .tasks
                .len(),
            1
        );

        tasks[0].description = "reworded by the lead".to_string();
        tasks[0].active_form = Some("Rewording".to_string());
        assert!(snapshot
            .absorb(&tasks, None, &keys(), &BTreeMap::new(), 0)
            .is_empty());
    }

    #[test]
    fn absorb_reports_a_task_whose_owner_appeared() {
        let mut tasks = vec![task(TASK_UNCLAIMED)];
        let mut snapshot = ProjectionSnapshot::new();
        assert_eq!(
            snapshot
                .absorb(&tasks, None, &keys(), &BTreeMap::new(), 0)
                .tasks
                .len(),
            1
        );

        tasks[0].owner = Some("cleanup".to_string());
        let delta = snapshot.absorb(&tasks, None, &keys(), &BTreeMap::new(), 0);
        assert_eq!(delta.tasks.len(), 1);
        assert_eq!(delta.tasks[0].task.owner.as_deref(), Some("cleanup"));
        // Karvex already fingerprinted this task as unclaimed on the first
        // poll, so this second poll's claim is itself an observed change of
        // hands (from nobody to `cleanup`) — WI-R6 wants exactly this
        // journalled, not just reassignments between two named members.
        let change = delta.tasks[0]
            .owner_change
            .as_ref()
            .expect("an owner appearing after karvex already saw it unclaimed is a change");
        assert_eq!(change.from, None);
        assert_eq!(change.to.as_deref(), Some("cleanup"));
    }

    #[test]
    fn absorb_reports_an_owner_change_when_a_task_moves_hands() {
        let mut tasks = vec![task(TASK_CLAIMED)];
        let mut snapshot = ProjectionSnapshot::new();
        let first = snapshot.absorb(&tasks, None, &keys(), &BTreeMap::new(), 0);
        assert_eq!(
            first.tasks[0].owner_change, None,
            "a task's very first observation is never a reassignment, even \
             though it already carries an owner"
        );
        let original_owner = tasks[0].owner.clone();

        tasks[0].owner = Some("someone-else".to_string());
        let delta = snapshot.absorb(&tasks, None, &keys(), &BTreeMap::new(), 0);
        assert_eq!(delta.tasks.len(), 1);
        let change = delta.tasks[0]
            .owner_change
            .as_ref()
            .expect("the owner moved between two named members");
        assert_eq!(change.from, original_owner);
        assert_eq!(change.to.as_deref(), Some("someone-else"));
    }

    #[test]
    fn absorb_reports_an_owner_change_when_a_task_becomes_unclaimed_again() {
        let mut tasks = vec![task(TASK_CLAIMED)];
        let mut snapshot = ProjectionSnapshot::new();
        let original_owner = tasks[0].owner.clone();
        snapshot.absorb(&tasks, None, &keys(), &BTreeMap::new(), 0);

        tasks[0].owner = None;
        let delta = snapshot.absorb(&tasks, None, &keys(), &BTreeMap::new(), 0);
        let change = delta.tasks[0]
            .owner_change
            .as_ref()
            .expect("dropping an owner is still a change of hands");
        assert_eq!(change.from, original_owner);
        assert_eq!(change.to, None, "unclaimed is `None`, not an empty string");
    }

    #[test]
    fn absorb_reports_only_the_member_whose_active_flag_changed() {
        let tasks = vec![task(TASK_CLAIMED)];
        let mut team = team(TEAM_CONFIG);
        let mut snapshot = ProjectionSnapshot::new();
        assert_eq!(
            snapshot
                .absorb(&tasks, Some(&team), &keys(), &BTreeMap::new(), 0)
                .members
                .len(),
            2
        );

        team.members[1].is_active = true;
        let delta = snapshot.absorb(&tasks, Some(&team), &keys(), &BTreeMap::new(), 0);
        assert!(delta.tasks.is_empty());
        assert_eq!(delta.members.len(), 1);
        assert_eq!(delta.members[0].member.name, "research");
        assert!(delta.members[0].member.is_active);
    }

    #[test]
    fn absorb_reports_a_teammate_that_joined_after_the_lead() {
        let tasks = Vec::new();
        let mut snapshot = ProjectionSnapshot::new();
        let lead_only = team(TEAM_CONFIG_LEAD_ONLY);
        let delta = snapshot.absorb(&tasks, Some(&lead_only), &keys(), &BTreeMap::new(), 0);
        assert_eq!(delta.members.len(), 1);
        assert_eq!(delta.members[0].member.name, "team-lead");

        let full = team(TEAM_CONFIG);
        let delta = snapshot.absorb(&tasks, Some(&full), &keys(), &BTreeMap::new(), 0);
        assert_eq!(delta.members.len(), 1);
        assert_eq!(delta.members[0].member.name, "research");
    }

    // ── member identity (§3.3, packet P8) ──────────────────────────────────

    fn identity(session: Option<&str>, state: Option<&str>) -> ObservedMemberIdentity {
        ObservedMemberIdentity {
            session_id: session.map(str::to_string),
            transcript_path: session.map(|id| format!("/home/dev/.claude/projects/-p/{id}.jsonl")),
            last_state: state.map(str::to_string),
        }
    }

    fn identities(
        entries: &[(&str, ObservedMemberIdentity)],
    ) -> BTreeMap<String, ObservedMemberIdentity> {
        entries
            .iter()
            .map(|(name, identity)| ((*name).to_string(), identity.clone()))
            .collect()
    }

    #[test]
    fn a_members_session_id_reaches_the_delta_that_persists_it() {
        let tasks = Vec::new();
        let team = team(TEAM_CONFIG);
        let mut snapshot = ProjectionSnapshot::new();
        let delta = snapshot.absorb(
            &tasks,
            Some(&team),
            &keys(),
            &identities(&[("research", identity(Some("7694e312"), Some("working")))]),
            5_000,
        );
        let research = delta
            .members
            .iter()
            .find(|member| member.member.name == "research")
            .expect("the teammate is in the delta");
        assert_eq!(research.identity.session_id.as_deref(), Some("7694e312"));
        assert_eq!(
            research.identity.transcript_path.as_deref(),
            Some("/home/dev/.claude/projects/-p/7694e312.jsonl")
        );
        assert_eq!(research.identity.last_state.as_deref(), Some("working"));
        assert_eq!(research.identity.last_state_at_unix_ms, Some(5_000));
        // The lead was observed in the same poll and resolved nothing, which is
        // recorded as unresolved rather than borrowed from the teammate.
        let lead = delta
            .members
            .iter()
            .find(|member| member.member.name == "team-lead")
            .expect("the lead is in the delta");
        assert_eq!(lead.identity, MemberIdentity::default());
    }

    #[test]
    fn a_session_id_that_arrives_late_is_one_write_and_then_silence() {
        let tasks = Vec::new();
        let team = team(TEAM_CONFIG);
        let mut snapshot = ProjectionSnapshot::new();
        // The team config lands before the teammate's hook has fired.
        assert_eq!(
            snapshot
                .absorb(&tasks, Some(&team), &keys(), &BTreeMap::new(), 1_000)
                .members
                .len(),
            2
        );

        let resolved = identities(&[("research", identity(Some("7694e312"), Some("idle")))]);
        let delta = snapshot.absorb(&tasks, Some(&team), &keys(), &resolved, 2_000);
        assert_eq!(delta.members.len(), 1);
        assert_eq!(delta.members[0].member.name, "research");
        assert_eq!(
            delta.members[0].identity.session_id.as_deref(),
            Some("7694e312")
        );

        // The same identity re-observed is not news. This is the discipline the
        // 2 s poller depends on: without it every tick would write a row.
        assert!(snapshot
            .absorb(&tasks, Some(&team), &keys(), &resolved, 3_000)
            .is_empty());
    }

    #[test]
    fn a_closed_pane_cannot_erase_the_session_id_it_reported() {
        let tasks = Vec::new();
        let team = team(TEAM_CONFIG);
        let mut snapshot = ProjectionSnapshot::new();
        snapshot.absorb(
            &tasks,
            Some(&team),
            &keys(),
            &identities(&[("research", identity(Some("7694e312"), Some("working")))]),
            1_000,
        );

        // The teammate's pane is gone, so karvex observes nothing about it. The
        // whole packet exists so that a review tomorrow can still resume a pane
        // that died today.
        let delta = snapshot.absorb(&tasks, Some(&team), &keys(), &BTreeMap::new(), 9_000);
        assert!(delta.is_empty(), "an unresolvable member writes nothing");
        let delta = snapshot.absorb(
            &tasks,
            Some(&team),
            &keys(),
            &identities(&[("research", identity(None, None))]),
            9_000,
        );
        assert!(delta.is_empty());
    }

    #[test]
    fn last_state_at_is_stamped_when_the_state_changes_and_not_when_it_repeats() {
        let tasks = Vec::new();
        let team = team(TEAM_CONFIG);
        let mut snapshot = ProjectionSnapshot::new();
        snapshot.absorb(
            &tasks,
            Some(&team),
            &keys(),
            &identities(&[("research", identity(Some("7694e312"), Some("idle")))]),
            1_000,
        );
        // Idle again, four seconds later: the same idle, not a new one. A
        // teammate that has sat idle for forty minutes has to read as forty
        // minutes, which is the number the watchdog and the review both use.
        assert!(snapshot
            .absorb(
                &tasks,
                Some(&team),
                &keys(),
                &identities(&[("research", identity(Some("7694e312"), Some("idle")))]),
                5_000,
            )
            .is_empty());

        let delta = snapshot.absorb(
            &tasks,
            Some(&team),
            &keys(),
            &identities(&[("research", identity(Some("7694e312"), Some("working")))]),
            9_000,
        );
        assert_eq!(delta.members.len(), 1);
        assert_eq!(delta.members[0].identity.last_state_at_unix_ms, Some(9_000));
    }

    #[test]
    fn absorb_orders_tasks_numerically_and_members_by_name() {
        let mut tasks = vec![task(TASK_UNCLAIMED), task(TASK_CLAIMED), task(TASK_BLOCKED)];
        tasks.push(ObservedTask {
            id: "10".to_string(),
            subject: "a tenth task".to_string(),
            description: String::new(),
            active_form: None,
            owner: None,
            status: TaskStatus::Pending,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        });
        tasks.push(ObservedTask {
            id: "alpha".to_string(),
            subject: "a non-numeric id".to_string(),
            description: String::new(),
            active_form: None,
            owner: None,
            status: TaskStatus::Pending,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        });

        let team = team(TEAM_CONFIG);
        let mut snapshot = ProjectionSnapshot::new();
        let delta = snapshot.absorb(&tasks, Some(&team), &keys(), &BTreeMap::new(), 0);

        let ids: Vec<&str> = delta.tasks.iter().map(|t| t.task.id.as_str()).collect();
        assert_eq!(ids, vec!["1", "2", "3", "10", "alpha"]);
        let names: Vec<&str> = delta
            .members
            .iter()
            .map(|m| m.member.name.as_str())
            .collect();
        assert_eq!(names, vec!["research", "team-lead"]);
    }

    #[test]
    fn an_emergent_task_id_cannot_escape_the_reserved_namespace() {
        for hostile in [
            "../../etc/passwd",
            "..",
            "a/b",
            "research",
            "with space",
            "..\\windows",
            "",
        ] {
            let path = emergent_path(hostile);
            let value = path.as_str();
            assert!(
                value.starts_with(EMERGENT_PATH_PREFIX),
                "{value} left the reserved namespace"
            );
            let tail = &value[EMERGENT_PATH_PREFIX.len()..];
            assert!(!tail.is_empty(), "empty id produced an empty tail");
            assert!(
                tail.chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'),
                "{value} kept a path-hostile character"
            );
        }
    }

    #[test]
    fn a_hostile_task_id_reaches_the_delta_sanitised() {
        let tasks = vec![ObservedTask {
            id: "../research".to_string(),
            subject: "not a planned subject".to_string(),
            description: String::new(),
            active_form: None,
            owner: None,
            status: TaskStatus::Pending,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        }];
        let mut snapshot = ProjectionSnapshot::new();
        let delta = snapshot.absorb(&tasks, None, &keys(), &BTreeMap::new(), 0);
        assert_eq!(delta.tasks.len(), 1);
        assert!(delta.tasks[0].emergent);
        assert_eq!(delta.tasks[0].path, InstancePath::new(".task/___research"));
        // The raw id is preserved on the observation itself; only the path is
        // sanitised.
        assert_eq!(delta.tasks[0].task.id, "../research");
    }

    #[test]
    fn a_resubjected_task_moves_between_planned_and_emergent() {
        let mut tasks = vec![task(TASK_CLAIMED)];
        let mut snapshot = ProjectionSnapshot::new();
        assert!(
            !snapshot
                .absorb(&tasks, None, &keys(), &BTreeMap::new(), 0)
                .tasks[0]
                .emergent
        );

        tasks[0].subject = "the lead reworded the prefix away".to_string();
        let delta = snapshot.absorb(&tasks, None, &keys(), &BTreeMap::new(), 0);
        assert_eq!(delta.tasks.len(), 1);
        assert!(delta.tasks[0].emergent);
        assert_eq!(delta.tasks[0].path, InstancePath::new(".task/1"));
    }

    #[test]
    fn a_blockers_re_subjecting_re_reports_the_task_that_names_it() {
        let mut tasks = vec![task(TASK_CLAIMED), task(TASK_BLOCKED)];
        let mut snapshot = ProjectionSnapshot::new();
        assert_eq!(
            snapshot
                .absorb(&tasks, None, &keys(), &BTreeMap::new(), 0)
                .tasks
                .len(),
            2
        );

        // Task 1 loses its prefix, so its path — and therefore task 2's edge —
        // changes even though task 2's own bytes did not.
        tasks[0].subject = "reworded".to_string();
        let delta = snapshot.absorb(&tasks, None, &keys(), &BTreeMap::new(), 0);
        let ids: Vec<&str> = delta.tasks.iter().map(|t| t.task.id.as_str()).collect();
        assert_eq!(ids, vec!["1", "2"]);
        assert_eq!(
            delta.tasks[1].blocked_by,
            vec![InstancePath::new(".task/1")]
        );
    }
}
