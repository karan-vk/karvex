//! Resolves human-readable names for live agent sessions.
//!
//! Claude Code publishes the same fact — "this running agent calls itself X" —
//! in two places, keyed two different ways, and Karvex reads both.
//!
//! * `<claude dir>/sessions/<pid>.json`, one small JSON document per process,
//!   carries the session UUID Karvex already stores per pane (see
//!   [`crate::agent_resume::PersistedAgentSession`]) alongside the session's
//!   display `name`. This file is written for interactive sessions only.
//! * `<claude dir>/teams/<team>/config.json` lists a team's members. Subagents
//!   get no `sessions` entry at all, so this is the only place their names
//!   exist. Member records carry no session UUID; they identify a member by
//!   `tmuxPaneId`, which for a subagent launched through Karvex's tmux-compat
//!   surface is a real Karvex public pane id such as `w3:p12`.
//!
//! Mapping either key to a name is what lets a client tell several sessions in
//! the same workspace apart. [`AgentSessionNames`] holds both maps and owns the
//! precedence between them.
//!
//! Both are foreign, concurrently-rewritten directories: files appear and
//! vanish as processes start and stop, and a read can land mid-write. Every
//! parse failure here is therefore expected traffic, not an error — unreadable,
//! truncated, or unrecognised entries are skipped and the rest of the directory
//! still resolves.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

/// Upper bound on registry files inspected in one refresh. Normal machines hold
/// one file per running session; a crashed process can leave its entry behind,
/// so this only exists to keep a pathological directory from stalling a refresh.
const MAX_REGISTRY_ENTRIES: usize = 512;

/// Largest registry document worth parsing. Entries are a few hundred bytes;
/// anything far past that is not the file this reader expects.
const MAX_REGISTRY_FILE_BYTES: u64 = 64 * 1024;

/// Upper bound on team directories stat-ed in one refresh.
///
/// Teams accumulate one directory per past session and are never pruned, so this
/// directory grows without bound on a long-lived machine. A stat is cheap enough
/// that the bound sits far above any plausible history; past it the directory is
/// pathological and the excess is simply not looked at.
const MAX_TEAM_DIRS_SCANNED: usize = 4096;

/// Upper bound on team configs actually read and parsed in one refresh, applied
/// after sorting by recency so the freshest of the scanned teams are the ones
/// read. Parsing is the expensive half, so this bound is much tighter.
const MAX_TEAM_CONFIGS_READ: usize = 32;

/// Largest team config worth parsing. A member record embeds the prompt the
/// member was launched with, so these run far larger than a session registry
/// entry — tens of kilobytes for a working team.
const MAX_TEAM_CONFIG_FILE_BYTES: u64 = 1024 * 1024;

/// Upper bound on members read from a single team config.
const MAX_TEAM_MEMBERS: usize = 256;

/// How long after its last membership change a team config is still believed.
///
/// A team config is rewritten whenever a member joins or leaves, so a live
/// team's config stays recent. This bounds the remaining window inside one
/// runtime: a team whose host died hard leaves its members marked active
/// forever, and this stops that record being believed indefinitely by a runtime
/// that has been up for months. A week is far longer than any real agent session
/// and short enough to bound that window.
const MAX_TEAM_CONFIG_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The member backend that occupies a real Karvex pane. Other backends (the
/// in-process team lead, for one) name something that is not a pane.
const TEAM_MEMBER_PANE_BACKEND: &str = "tmux";

const TEAM_CONFIG_FILE_NAME: &str = "config.json";

/// Characters of the session UUID used when a session has no name yet.
const SHORT_SESSION_ID_LEN: usize = 8;

/// Display names resolved for live agent sessions, indexed by every key the
/// agent's own on-disk state offers.
///
/// Two indexes rather than one because the sources disagree about identity: the
/// session registry knows a session UUID, a team config knows only which pane a
/// member occupies. [`AgentSessionNames::resolve`] owns the precedence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentSessionNames {
    by_session_id: HashMap<String, String>,
    by_pane_id: HashMap<String, String>,
}

impl AgentSessionNames {
    /// The name to show for a session, given the session's id and, when it has
    /// one, the public id of the pane hosting it.
    ///
    /// The session registry wins. It names the session itself, is rewritten by
    /// the running process as the name changes, and disappears when that process
    /// exits — so it cannot outlive its subject. A team config names a *pane*,
    /// which is a weaker claim: pane ids are reused, and the config is only
    /// rewritten on membership changes. So the pane index is consulted only
    /// where the authoritative index has nothing to say.
    pub fn resolve(&self, session_id: &str, pane_id: Option<&str>) -> Option<&str> {
        if let Some(name) = self.by_session_id.get(session_id) {
            return Some(name.as_str());
        }
        pane_id
            .and_then(|pane_id| self.by_pane_id.get(pane_id))
            .map(String::as_str)
    }

    #[cfg(test)]
    pub(crate) fn from_parts(
        by_session_id: HashMap<String, String>,
        by_pane_id: HashMap<String, String>,
    ) -> Self {
        Self {
            by_session_id,
            by_pane_id,
        }
    }

    #[cfg(test)]
    pub(crate) fn by_session_id(names: HashMap<String, String>) -> Self {
        Self::from_parts(names, HashMap::new())
    }

    #[cfg(test)]
    pub(crate) fn by_pane_id(names: HashMap<String, String>) -> Self {
        Self::from_parts(HashMap::new(), names)
    }
}

/// The subset of a registry entry Karvex reads. Unknown fields are ignored on
/// purpose: the agent owns this format and adds fields between releases.
#[derive(Deserialize)]
struct RegistryEntry {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    name: Option<String>,
}

/// The subset of a team config Karvex reads.
#[derive(Deserialize)]
struct TeamConfig {
    #[serde(default)]
    members: Vec<TeamMember>,
}

/// The subset of a team member record Karvex reads.
#[derive(Deserialize)]
struct TeamMember {
    name: Option<String>,
    #[serde(rename = "tmuxPaneId")]
    pane_id: Option<String>,
    #[serde(rename = "backendType")]
    backend_type: Option<String>,
    #[serde(rename = "isActive")]
    is_active: Option<bool>,
}

/// Reads every source of agent session names for the current user.
///
/// `runtime_started_at` is when this runtime came up; team configs untouched
/// since then are ignored, because the pane ids they name were handed out by a
/// previous runtime.
///
/// Returns empty maps when the agent's directory is missing, which is the normal
/// state on a machine where no such agent has ever run.
pub(crate) fn read_agent_session_names(runtime_started_at: SystemTime) -> AgentSessionNames {
    let Ok(claude_dir) = crate::integration::claude_dir() else {
        return AgentSessionNames::default();
    };
    AgentSessionNames {
        by_session_id: read_session_names_from_dir(&claude_dir.join("sessions")),
        by_pane_id: read_team_pane_names_from_dir(
            &claude_dir.join("teams"),
            SystemTime::now(),
            runtime_started_at,
        ),
    }
}

/// Directory-scoped read of the live session registry, returning a session-id to
/// display-name map. Separated from [`read_agent_session_names`] so tests can
/// point it at a fixture instead of the caller's real home directory.
pub(crate) fn read_session_names_from_dir(dir: &Path) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        // A missing registry is the common case, not a fault: no agent of this
        // kind has run for this user yet.
        return names;
    };

    for entry in entries.flatten().take(MAX_REGISTRY_ENTRIES) {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        // Metadata is only a cheap pre-filter; the file can still change size
        // between this check and the read below, which the parse handles.
        if entry
            .metadata()
            .is_ok_and(|metadata| metadata.len() > MAX_REGISTRY_FILE_BYTES)
        {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<RegistryEntry>(&contents) else {
            // Torn or partially written file: the next refresh picks it up.
            continue;
        };
        let Some(session_id) = parsed.session_id.filter(|id| !id.is_empty()) else {
            continue;
        };
        let Some(name) = parsed.name.map(|name| name.trim().to_string()) else {
            continue;
        };
        if name.is_empty() {
            // An unnamed session is absent, not empty-named; callers fall back
            // to `short_session_id`.
            continue;
        }
        names.insert(session_id, name);
    }

    names
}

/// Directory-scoped read of the team registry, returning a public-pane-id to
/// member-name map.
///
/// `now` and `runtime_started_at` are parameters rather than calls to
/// [`SystemTime::now`] so both staleness gates are testable without sleeping.
///
/// A member is believed only when every one of these holds, because each guards
/// a distinct way this directory lies:
///
/// * its backend occupies a real pane — the in-process team lead reports the
///   sentinel pane id `leader`, and other backends may report a foreign id;
/// * its pane id has Karvex's own public-pane-id shape — a genuine tmux id such
///   as `%3` names someone else's pane, not ours;
/// * it is still marked active — an exited member is either dropped from the
///   config or left behind with this flag cleared, and either way its pane id is
///   free to be handed to something else;
/// * its team config has been written since this runtime came up;
/// * its team config was touched inside [`MAX_TEAM_CONFIG_AGE`];
/// * no fresher team config claims the same pane id.
///
/// The runtime-start gate is the one that does not depend on the agent behaving
/// well. `isActive` is written by a process that may have died without clearing
/// it, and a pane id such as `w3:p12` is reused once this runtime's pane
/// counters start over — but a config that has not been written since this
/// runtime came up cannot be describing this runtime's panes, whatever its flags
/// say. Within a single run the two gates overlap; across a restart, only this
/// one holds.
pub(crate) fn read_team_pane_names_from_dir(
    dir: &Path,
    now: SystemTime,
    runtime_started_at: SystemTime,
) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        // No team has ever run for this user, which is the common case.
        return names;
    };

    // Stat first, parse second: the directory keeps one entry per past session
    // and is never pruned, so recency is what decides which handful of configs
    // are worth reading at all.
    let mut configs = Vec::new();
    for entry in entries.flatten().take(MAX_TEAM_DIRS_SCANNED) {
        let path = entry.path().join(TEAM_CONFIG_FILE_NAME);
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > MAX_TEAM_CONFIG_FILE_BYTES {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified < runtime_started_at {
            continue;
        }
        if now
            .duration_since(modified)
            .is_ok_and(|age| age > MAX_TEAM_CONFIG_AGE)
        {
            continue;
        }
        configs.push((modified, entry.file_name(), path));
    }

    // Freshest first, with the directory name breaking ties so two configs
    // written in the same clock tick still resolve the same way on every
    // refresh. The first claim on a pane id wins.
    configs.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));

    for (_, _, path) in configs.into_iter().take(MAX_TEAM_CONFIGS_READ) {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(config) = serde_json::from_str::<TeamConfig>(&contents) else {
            // Torn or partially written config: the next refresh picks it up.
            continue;
        };
        for member in config.members.into_iter().take(MAX_TEAM_MEMBERS) {
            if member.backend_type.as_deref() != Some(TEAM_MEMBER_PANE_BACKEND) {
                continue;
            }
            if member.is_active != Some(true) {
                continue;
            }
            let Some(pane_id) = member.pane_id.filter(|id| is_public_pane_id(id)) else {
                continue;
            };
            let Some(name) = member.name.map(|name| name.trim().to_string()) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            names.entry(pane_id).or_insert(name);
        }
    }

    names
}

/// Whether a foreign string has the shape of a Karvex public pane id.
///
/// Shape only: a pane id that no live pane carries simply never matches, so this
/// exists to reject ids that belong to a different namespace before they can
/// collide with ours.
fn is_public_pane_id(candidate: &str) -> bool {
    let Some((workspace_id, pane_number)) = candidate.rsplit_once(":p") else {
        return false;
    };
    if pane_number.is_empty() {
        return false;
    }
    let Some(workspace_number) = workspace_id.strip_prefix('w') else {
        return false;
    };
    !workspace_number.is_empty()
        && crate::workspace::decode_public_number(workspace_number).is_some()
        && crate::workspace::decode_public_number(pane_number).is_some()
}

/// Abbreviates a session UUID for display when nothing resolved a name for it.
///
/// The leading block of a UUID is already distinct between concurrent sessions,
/// which is all a client needs to keep otherwise-identical rows apart.
pub(crate) fn short_session_id(session_id: &str) -> String {
    session_id.chars().take(SHORT_SESSION_ID_LEN).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).expect("write registry fixture");
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("karvex-session-registry-{label}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create registry fixture dir");
        dir
    }

    #[test]
    fn reads_named_sessions_and_skips_unnamed_ones() {
        let dir = temp_dir("named");
        write(
            &dir,
            "100.json",
            r#"{"pid":100,"sessionId":"aaaa-1","name":"toilet-presence-sensor","status":"busy"}"#,
        );
        write(&dir, "101.json", r#"{"pid":101,"sessionId":"aaaa-2"}"#);
        write(
            &dir,
            "102.json",
            r#"{"pid":102,"sessionId":"aaaa-3","name":"   "}"#,
        );

        let names = read_session_names_from_dir(&dir);

        assert_eq!(
            names,
            HashMap::from([("aaaa-1".to_string(), "toilet-presence-sensor".to_string())])
        );
    }

    #[test]
    fn a_torn_file_does_not_hide_its_healthy_neighbours() {
        let dir = temp_dir("torn");
        write(&dir, "200.json", r#"{"pid":200,"sessionId":"bbbb-1","na"#);
        write(&dir, "201.json", r#"not json at all"#);
        write(
            &dir,
            "202.json",
            r#"{"sessionId":"bbbb-3","name":"review"}"#,
        );
        // Non-JSON siblings in the same directory are ignored outright.
        write(
            &dir,
            "203.txt",
            r#"{"sessionId":"bbbb-4","name":"ignored"}"#,
        );

        let names = read_session_names_from_dir(&dir);

        assert_eq!(
            names,
            HashMap::from([("bbbb-3".to_string(), "review".to_string())])
        );
    }

    #[test]
    fn unknown_fields_and_missing_session_ids_are_tolerated() {
        let dir = temp_dir("lenient");
        write(
            &dir,
            "300.json",
            r#"{"pid":300,"sessionId":"cccc-1","name":"planner","futureField":{"nested":true}}"#,
        );
        write(&dir, "301.json", r#"{"pid":301,"name":"orphan"}"#);
        write(&dir, "302.json", r#"{"sessionId":"","name":"empty id"}"#);

        let names = read_session_names_from_dir(&dir);

        assert_eq!(
            names,
            HashMap::from([("cccc-1".to_string(), "planner".to_string())])
        );
    }

    #[test]
    fn a_missing_registry_directory_resolves_to_no_names() {
        let dir = temp_dir("missing").join("does-not-exist");
        assert!(read_session_names_from_dir(&dir).is_empty());
    }

    #[test]
    fn an_empty_registry_directory_resolves_to_no_names() {
        let dir = temp_dir("empty");
        assert!(read_session_names_from_dir(&dir).is_empty());
    }

    #[test]
    fn short_session_id_takes_the_leading_uuid_block() {
        assert_eq!(
            short_session_id("f593fc46-5328-4998-a7b1-80bb1b3e7e3b"),
            "f593fc46"
        );
        // Shorter-than-expected ids are returned whole rather than padded.
        assert_eq!(short_session_id("abc"), "abc");
        assert_eq!(short_session_id(""), "");
    }

    /// Writes `<teams dir>/<team>/config.json` and back-dates it by `age`, which
    /// is how the staleness horizon is exercised without sleeping.
    fn write_team(teams_dir: &Path, team: &str, contents: &str, age: Duration) {
        let dir = teams_dir.join(team);
        std::fs::create_dir_all(&dir).expect("create team fixture dir");
        let path = dir.join(TEAM_CONFIG_FILE_NAME);
        std::fs::write(&path, contents).expect("write team fixture");
        let file = std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open team fixture");
        file.set_modified(now() - age)
            .expect("set team fixture mtime");
    }

    /// A fixed clock for the team fixtures, far enough from the epoch that
    /// back-dating a config by weeks cannot underflow.
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    /// A runtime that came up long before any fixture, so tests exercising the
    /// other gates are not incidentally filtered by the runtime-start one.
    fn runtime_start() -> SystemTime {
        now() - MAX_TEAM_CONFIG_AGE - Duration::from_secs(365 * 24 * 60 * 60)
    }

    const FRESH: Duration = Duration::from_secs(60);

    fn member(name: &str, pane_id: &str, backend: &str, active: bool) -> String {
        format!(
            r#"{{"agentId":"{name}@team","name":"{name}","tmuxPaneId":"{pane_id}","backendType":"{backend}","isActive":{active},"cwd":"/tmp"}}"#
        )
    }

    fn team_config(members: &[String]) -> String {
        format!(
            r#"{{"name":"team","leadSessionId":"lead","members":[{}]}}"#,
            members.join(",")
        )
    }

    #[test]
    fn team_members_resolve_their_pane_ids_to_their_names() {
        let dir = temp_dir("teams-basic");
        write_team(
            &dir,
            "session-1",
            &team_config(&[
                member("wf-authoring", "w3:p12", "tmux", true),
                member("wf-phase3", "w3:p13", "tmux", true),
            ]),
            FRESH,
        );

        let names = read_team_pane_names_from_dir(&dir, now(), runtime_start());

        assert_eq!(
            names,
            HashMap::from([
                ("w3:p12".to_string(), "wf-authoring".to_string()),
                ("w3:p13".to_string(), "wf-phase3".to_string()),
            ])
        );
    }

    #[test]
    fn members_that_do_not_occupy_a_pane_are_ignored() {
        let dir = temp_dir("teams-non-panes");
        write_team(
            &dir,
            "session-1",
            &team_config(&[
                // The team lead runs in-process and reports a sentinel, not a
                // pane id.
                member("team-lead", "leader", "in-process", true),
                // A pane-shaped id from a backend that is not ours names
                // someone else's pane.
                member("foreign", "w9:p1", "ssh", true),
                // A genuine tmux pane id belongs to a different namespace.
                member("real-tmux", "%3", "tmux", true),
                // Shapes that are not public pane ids at all.
                member("no-pane-part", "w3", "tmux", true),
                member("empty-number", "w3:p", "tmux", true),
                member("bad-workspace", "x3:p12", "tmux", true),
                member("nameless", "w3:p20", "tmux", true)
                    .replace(r#""name":"nameless""#, r#""name":"   ""#),
                member("kept", "w3:p12", "tmux", true),
            ]),
            FRESH,
        );

        let names = read_team_pane_names_from_dir(&dir, now(), runtime_start());

        assert_eq!(
            names,
            HashMap::from([("w3:p12".to_string(), "kept".to_string())])
        );
    }

    #[test]
    fn an_exited_member_stops_claiming_its_pane_id() {
        let dir = temp_dir("teams-inactive");
        write_team(
            &dir,
            "session-1",
            &team_config(&[
                member("gone", "w3:p12", "tmux", false),
                member("still-here", "w3:p13", "tmux", true),
            ]),
            FRESH,
        );

        // A member marked inactive has released its pane, and that pane id is
        // free to be handed to something unrelated.
        let names = read_team_pane_names_from_dir(&dir, now(), runtime_start());

        assert_eq!(
            names,
            HashMap::from([("w3:p13".to_string(), "still-here".to_string())])
        );
    }

    #[test]
    fn a_torn_team_config_does_not_hide_its_healthy_neighbours() {
        let dir = temp_dir("teams-torn");
        write_team(&dir, "session-torn", r#"{"name":"team","memb"#, FRESH);
        write_team(&dir, "session-text", "not json at all", FRESH);
        write_team(&dir, "session-empty", "", FRESH);
        write_team(
            &dir,
            "session-ok",
            &team_config(&[member("healthy", "w3:p12", "tmux", true)]),
            FRESH,
        );
        // A team directory with no config at all is skipped, not fatal.
        std::fs::create_dir_all(dir.join("session-bare")).expect("create bare team dir");

        let names = read_team_pane_names_from_dir(&dir, now(), runtime_start());

        assert_eq!(
            names,
            HashMap::from([("w3:p12".to_string(), "healthy".to_string())])
        );
    }

    #[test]
    fn unknown_member_fields_and_absent_flags_are_tolerated() {
        let dir = temp_dir("teams-lenient");
        write_team(
            &dir,
            "session-1",
            r#"{"name":"team","futureField":{"nested":true},"members":[
                {"name":"named","tmuxPaneId":"w3:p12","backendType":"tmux","isActive":true,"futureMemberField":[1,2]},
                {"name":"no-flag","tmuxPaneId":"w3:p13","backendType":"tmux"},
                {"name":"no-backend","tmuxPaneId":"w3:p14","isActive":true}
            ]}"#,
            FRESH,
        );

        // An absent `isActive` is not an assertion of liveness, so only the
        // member that actually claims to be active is believed.
        let names = read_team_pane_names_from_dir(&dir, now(), runtime_start());

        assert_eq!(
            names,
            HashMap::from([("w3:p12".to_string(), "named".to_string())])
        );
    }

    #[test]
    fn a_team_config_past_the_staleness_horizon_never_claims_a_pane_id() {
        let dir = temp_dir("teams-stale");
        // A host that died hard leaves its members marked active forever. Pane
        // ids are reused across Karvex restarts, so believing this record would
        // mislabel whatever pane later inherits `w3:p12`.
        write_team(
            &dir,
            "session-crashed",
            &team_config(&[member("long-dead", "w3:p12", "tmux", true)]),
            MAX_TEAM_CONFIG_AGE + Duration::from_secs(60),
        );

        assert!(read_team_pane_names_from_dir(&dir, now(), runtime_start()).is_empty());
    }

    #[test]
    fn a_team_config_untouched_since_this_runtime_started_never_claims_a_pane_id() {
        let dir = temp_dir("teams-previous-runtime");
        // Recent, well inside the age horizon, and still marked active — the
        // shape a hard-killed host leaves behind. But this runtime handed out
        // `w3:p12` itself after coming up, so whatever that record describes, it
        // is not this pane.
        write_team(
            &dir,
            "session-previous",
            &team_config(&[member("previous-runtimes-agent", "w3:p12", "tmux", true)]),
            Duration::from_secs(2 * 60 * 60),
        );

        let runtime_started_at = now() - Duration::from_secs(60 * 60);

        assert!(read_team_pane_names_from_dir(&dir, now(), runtime_started_at).is_empty());
        // The same config is believed by a runtime that predates it.
        assert_eq!(
            read_team_pane_names_from_dir(&dir, now(), now() - Duration::from_secs(3 * 60 * 60)),
            HashMap::from([("w3:p12".to_string(), "previous-runtimes-agent".to_string())])
        );
    }

    #[test]
    fn the_freshest_team_config_wins_a_reused_pane_id() {
        let dir = temp_dir("teams-recycled");
        write_team(
            &dir,
            "session-old",
            &team_config(&[member("yesterdays-agent", "w3:p12", "tmux", true)]),
            Duration::from_secs(24 * 60 * 60),
        );
        write_team(
            &dir,
            "session-new",
            &team_config(&[member("todays-agent", "w3:p12", "tmux", true)]),
            FRESH,
        );

        // Two live-looking teams cannot both own one pane; the more recently
        // changed membership is the one that reflects reality.
        let names = read_team_pane_names_from_dir(&dir, now(), runtime_start());

        assert_eq!(
            names,
            HashMap::from([("w3:p12".to_string(), "todays-agent".to_string())])
        );
    }

    #[test]
    fn a_missing_teams_directory_resolves_to_no_names() {
        let dir = temp_dir("teams-missing").join("does-not-exist");
        assert!(read_team_pane_names_from_dir(&dir, now(), runtime_start()).is_empty());
    }

    #[test]
    fn an_oversized_team_config_is_skipped() {
        let dir = temp_dir("teams-oversized");
        let padding = "x".repeat(MAX_TEAM_CONFIG_FILE_BYTES as usize);
        write_team(
            &dir,
            "session-huge",
            &format!(
                r#"{{"name":"team","padding":"{padding}","members":[{}]}}"#,
                member("too-big", "w3:p12", "tmux", true)
            ),
            FRESH,
        );

        assert!(read_team_pane_names_from_dir(&dir, now(), runtime_start()).is_empty());
    }

    #[test]
    fn the_session_registry_outranks_a_team_config_for_the_same_pane() {
        let names = AgentSessionNames::from_parts(
            HashMap::from([("session-1".to_string(), "from-registry".to_string())]),
            HashMap::from([("w3:p12".to_string(), "from-team".to_string())]),
        );

        // The registry names the session itself and vanishes with its process;
        // the team config only names a pane, so it is the weaker claim.
        assert_eq!(
            names.resolve("session-1", Some("w3:p12")),
            Some("from-registry")
        );
        // With nothing in the registry, the pane claim is what is left.
        assert_eq!(
            names.resolve("session-2", Some("w3:p12")),
            Some("from-team")
        );
        // And a pane nobody claims resolves to nothing, so callers fall back to
        // the short session id.
        assert_eq!(names.resolve("session-2", Some("w3:p99")), None);
        assert_eq!(names.resolve("session-2", None), None);
    }
}
