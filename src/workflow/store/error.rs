//! Store error vocabulary.

use std::fmt;
use std::io;

use crate::workflow::model::KvdagError;

/// `StoreError::Unavailable::reason` when another karvex server holds the
/// SurrealKv lock on the database directory.
pub const STORE_LOCKED: &str = "store_locked";

/// The API error code every `workflow.*` method returns while the subsystem is
/// unavailable. `ErrorBody.code` is a plain string, so this needs no schema
/// change.
pub const WORKFLOW_UNAVAILABLE_CODE: &str = "workflow_unavailable";

/// The error code for every other store failure — a genuine store-side
/// problem (a bad query, a decode failure, an invariant violation), not a
/// document the caller authored badly.
pub const WORKFLOW_STORE_ERROR_CODE: &str = "workflow_store_error";

/// The error code for a definition that fails `Kvdag::try_new`'s construction
/// invariants — a cycle, an `expand_allow` entry naming an unknown or
/// non-template node, an unresolved `{{slot}}`, and the rest of
/// [`KvdagError`]'s variants. This is the *same* code
/// `workflow.create`/`workflow.version.create` already use for a definition
/// document that fails `Definition::check` before ever reaching the store
/// (`INVALID_DEFINITION_CODE` in `src/app/api/workflows.rs`) — both are "the
/// document you authored did not validate", and a caller should not have to
/// know which validator caught it. Deliberately distinct from
/// [`WORKFLOW_STORE_ERROR_CODE`]: an author fixing an `expand_allow` typo is
/// not the same failure as the store itself misbehaving, and conflating them
/// under one code hid the authoring-error case behind generic store-failure
/// messaging (v0.10.0 retest, `expand_allow` authoring-validation finding).
pub const WORKFLOW_INVALID_DEFINITION_CODE: &str = "workflow_invalid_definition";

#[derive(Debug)]
pub enum StoreError {
    /// The subsystem cannot be used at all. Surfaced once in the TUI, never
    /// silent, and never degraded to an in-memory store — that would look like
    /// data loss.
    Unavailable {
        reason: String,
        holder: Option<String>,
    },
    Migration {
        version: String,
        message: String,
    },
    Query(String),
    Decode(String),
    NotFound {
        table: &'static str,
        id: String,
    },
    /// A `KvdagSpec` failed `Kvdag::try_new`'s construction invariants
    /// (`create_version` / `load_version`). Store-level, not engine-level: the
    /// engine never sees a graph that hasn't already passed this gate.
    InvalidGraph(KvdagError),
    /// A caller asked the store to write something the schema's own rules
    /// forbid — a run wider than the version it runs
    /// (`workflow_run.max_nodes <= kvdag_version.max_nodes`,
    /// `04-kvdag-and-execution.md` §3.4), or a materialised node with no
    /// resolved assignment. Distinct from [`Self::Query`]: nothing was
    /// attempted against the database, so there is no engine error to report.
    Invariant(String),
    Io(io::Error),
}

impl StoreError {
    pub fn store_locked(holder: Option<String>) -> Self {
        Self::Unavailable {
            reason: STORE_LOCKED.to_string(),
            holder,
        }
    }

    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }

    /// The `ErrorBody.code` a `workflow.*` handler reports for this failure.
    ///
    /// [`Self::InvalidGraph`] gets its own code rather than falling into the
    /// generic [`WORKFLOW_STORE_ERROR_CODE`] arm below: it is the store
    /// rejecting a document the caller authored, not the store failing on its
    /// own account, and a scripted caller matching on `code` deserves to tell
    /// the two apart.
    pub fn api_code(&self) -> &'static str {
        if self.is_unavailable() {
            WORKFLOW_UNAVAILABLE_CODE
        } else if matches!(self, Self::InvalidGraph(_)) {
            WORKFLOW_INVALID_DEFINITION_CODE
        } else {
            WORKFLOW_STORE_ERROR_CODE
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { reason, holder } => match holder {
                Some(holder) => {
                    write!(f, "workflow store unavailable ({reason}), held by {holder}")
                }
                None => write!(f, "workflow store unavailable ({reason})"),
            },
            Self::Migration { version, message } => {
                write!(f, "migration {version} failed: {message}")
            }
            Self::Query(message) => write!(f, "workflow store query failed: {message}"),
            Self::Decode(message) => write!(f, "workflow store decode failed: {message}"),
            Self::NotFound { table, id } => write!(f, "no {table} with id {id}"),
            Self::InvalidGraph(error) => write!(f, "invalid kvdag: {error}"),
            Self::Invariant(message) => write!(f, "workflow store invariant violated: {message}"),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidGraph(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<KvdagError> for StoreError {
    fn from(error: KvdagError) -> Self {
        Self::InvalidGraph(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::model::NodeKey;

    #[test]
    fn store_locked_carries_reason_and_holder() {
        let error = StoreError::store_locked(Some("pid 4242".to_string()));
        assert!(error.is_unavailable());
        assert_eq!(error.api_code(), WORKFLOW_UNAVAILABLE_CODE);
        assert!(error.to_string().contains(STORE_LOCKED));
        assert!(error.to_string().contains("pid 4242"));
    }

    #[test]
    fn invariant_violations_report_as_a_store_error_not_unavailable() {
        let error = StoreError::Invariant("run growth exceeds the version".to_string());
        assert!(!error.is_unavailable());
        assert_eq!(error.api_code(), WORKFLOW_STORE_ERROR_CODE);
        assert!(error.to_string().contains("run growth exceeds the version"));
    }

    #[test]
    fn other_failures_do_not_report_as_unavailable() {
        let error = StoreError::Query("SELECT exploded".to_string());
        assert!(!error.is_unavailable());
        assert_eq!(error.api_code(), WORKFLOW_STORE_ERROR_CODE);
    }

    /// v0.10.0 retest, `expand_allow` authoring-validation finding: an
    /// `expand_allow` entry naming a node that is not a template — a document
    /// the author got wrong — was reported under the same
    /// `workflow_store_error` code a genuine store I/O or query failure uses,
    /// so a scripted caller (or a human reading the code) could not tell "fix
    /// your document" from "the store is broken" apart. Fails before the fix:
    /// `api_code()` returned `WORKFLOW_STORE_ERROR_CODE` for every
    /// `InvalidGraph`.
    #[test]
    fn a_graph_construction_failure_reports_a_definition_validation_code() {
        let error = StoreError::InvalidGraph(KvdagError::ExpandTargetNotTemplate {
            node: NodeKey::new("fanout"),
            template: NodeKey::new("collect"),
        });
        assert!(!error.is_unavailable());
        assert_eq!(error.api_code(), WORKFLOW_INVALID_DEFINITION_CODE);
        assert_ne!(error.api_code(), WORKFLOW_STORE_ERROR_CODE);
    }

    /// The `From<KvdagError>` conversion used by `create_version`/
    /// `load_version` reaches the same code, not just a hand-built
    /// `StoreError::InvalidGraph`.
    #[test]
    fn the_kvdag_error_conversion_reports_the_same_code() {
        let error: StoreError = KvdagError::UnknownExpandTemplate {
            node: NodeKey::new("fanout"),
            template: NodeKey::new("ghost"),
        }
        .into();
        assert_eq!(error.api_code(), WORKFLOW_INVALID_DEFINITION_CODE);
    }
}
