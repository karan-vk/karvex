//! Resolves human-readable names for live agent sessions.
//!
//! Claude Code keeps a registry of its running sessions under
//! `<claude dir>/sessions/<pid>.json`, one small JSON document per process. The
//! document carries the session UUID Karvex already stores per pane (see
//! [`crate::agent_resume::PersistedAgentSession`]) alongside the session's
//! display `name`, which the agent renames while it runs. Mapping UUID to name
//! is what lets the sidebar tell several sessions in the same workspace apart.
//!
//! The registry is a foreign, concurrently-rewritten directory: files appear and
//! vanish as processes start and stop, and a read can land mid-write. Every
//! parse failure here is therefore expected traffic, not an error — unreadable,
//! truncated, or unrecognised entries are skipped and the rest of the directory
//! still resolves.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

/// Upper bound on registry files inspected in one refresh. Normal machines hold
/// one file per running session; a crashed process can leave its entry behind,
/// so this only exists to keep a pathological directory from stalling a refresh.
const MAX_REGISTRY_ENTRIES: usize = 512;

/// Largest registry document worth parsing. Entries are a few hundred bytes;
/// anything far past that is not the file this reader expects.
const MAX_REGISTRY_FILE_BYTES: u64 = 64 * 1024;

/// Characters of the session UUID used when a session has no name yet.
const SHORT_SESSION_ID_LEN: usize = 8;

/// The subset of a registry entry Karvex reads. Unknown fields are ignored on
/// purpose: the agent owns this format and adds fields between releases.
#[derive(Deserialize)]
struct RegistryEntry {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    name: Option<String>,
}

/// Reads the live session registry and returns a session-id to display-name map.
///
/// Returns an empty map when the registry directory is missing, which is the
/// normal state on a machine where no such agent has ever run.
pub(crate) fn read_session_names() -> HashMap<String, String> {
    let Ok(claude_dir) = crate::integration::claude_dir() else {
        return HashMap::new();
    };
    read_session_names_from_dir(&claude_dir.join("sessions"))
}

/// Directory-scoped core of [`read_session_names`], separated so tests can point
/// it at a fixture instead of the caller's real home directory.
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

/// Abbreviates a session UUID for display when the registry has no name for it.
///
/// The leading block of a UUID is already distinct between concurrent sessions,
/// which is all the sidebar needs to keep otherwise-identical rows apart.
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
}
