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

/// The error code for every other store failure.
pub const WORKFLOW_STORE_ERROR_CODE: &str = "workflow_store_error";

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
    pub fn api_code(&self) -> &'static str {
        if self.is_unavailable() {
            WORKFLOW_UNAVAILABLE_CODE
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

    #[test]
    fn store_locked_carries_reason_and_holder() {
        let error = StoreError::store_locked(Some("pid 4242".to_string()));
        assert!(error.is_unavailable());
        assert_eq!(error.api_code(), WORKFLOW_UNAVAILABLE_CODE);
        assert!(error.to_string().contains(STORE_LOCKED));
        assert!(error.to_string().contains("pid 4242"));
    }

    #[test]
    fn other_failures_do_not_report_as_unavailable() {
        let error = StoreError::Query("SELECT exploded".to_string());
        assert!(!error.is_unavailable());
        assert_eq!(error.api_code(), WORKFLOW_STORE_ERROR_CODE);
    }
}
