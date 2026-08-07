//! redb table definitions, record-key layout, and the small encode/decode
//! plumbing every table shares.
//!
//! redb is an ordered key/value store, not a query engine, so the *key layout*
//! is the schema. Two properties are bought by construction here rather than by
//! an index the caller has to remember to use:
//!
//! - **Child rows are prefixed by their parent's key.** Every row belonging to
//!   one run starts with that run's key followed by [`SEP`], so "read this
//!   run's journal" and "delete this whole run" are both one range scan. That
//!   is what keeps [`super::WorkflowStore::prune_run_history`] whole-run-only:
//!   there is no key shape that addresses part of a run.
//! - **Order is the key order.** `run_event` sorts by sequence number because
//!   its key ends in a zero-padded sequence number; `kvdag_node` sorts by node
//!   key for the same reason. Nothing re-sorts a scan except where the wanted
//!   order genuinely differs from the storage order.
//!
//! Rows are JSON (see [`encode`]). Row structs live in `records.rs`.

use redb::{ReadableTable, TableDefinition};
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::StoreError;

/// Every row table maps an opaque record key to a JSON-encoded row.
pub(super) type RowTable = TableDefinition<'static, &'static str, &'static [u8]>;

/// Separator between the segments of a composite record key. `\u{1f}` (ASCII
/// unit separator) is not producible by an author-written node key, an instance
/// path, or any key this module mints, so a composite key can never be split
/// two different ways.
pub(super) const SEP: char = '\u{1f}';

// ── tables ──────────────────────────────────────────────────────────────────
//
// Key layouts, in the order the tables are listed:
//
//   workflow          w<counter:012x>
//   kvdag_version     <workflow>-v<version:06>
//   kvdag_node        <version>SEP<node_key>
//   kvdag_edge        <version>SEP<from>SEP<to>SEP<kind>SEP<port>
//   workflow_run      <workflow>-r<counter:012x>
//   run_node          <run>SEP<instance_path>
//   run_edge          <run>SEP<from_path>SEP<to_path>SEP<kind>
//   run_event         <run>SEP<seq:020>
//   node_checkpoint   <run>SEP<instance_path>SEP<seq:020>
//   run_summary       <run>                       (survives pruning)
//   interrogation     <run>SEP<instance_path>SEP i<counter:012x>
//   review_cycle      c<counter:012x>             (survives pruning)
//   review_finding    <cycle>SEPf<counter:012x>   (survives pruning)
//
// `workflow` and `kvdag_version` keys are fixed-width up to their separator, so
// a prefix scan for one workflow's versions or runs cannot bleed into another
// workflow's.

pub(super) const WORKFLOW: RowTable = TableDefinition::new("workflow");
pub(super) const KVDAG_VERSION: RowTable = TableDefinition::new("kvdag_version");
pub(super) const KVDAG_NODE: RowTable = TableDefinition::new("kvdag_node");
pub(super) const KVDAG_EDGE: RowTable = TableDefinition::new("kvdag_edge");
pub(super) const WORKFLOW_RUN: RowTable = TableDefinition::new("workflow_run");
pub(super) const RUN_NODE: RowTable = TableDefinition::new("run_node");
pub(super) const RUN_EDGE: RowTable = TableDefinition::new("run_edge");
pub(super) const RUN_EVENT: RowTable = TableDefinition::new("run_event");
pub(super) const NODE_CHECKPOINT: RowTable = TableDefinition::new("node_checkpoint");
pub(super) const RUN_SUMMARY: RowTable = TableDefinition::new("run_summary");
pub(super) const INTERROGATION: RowTable = TableDefinition::new("interrogation");
pub(super) const REVIEW_CYCLE: RowTable = TableDefinition::new("review_cycle");
pub(super) const REVIEW_FINDING: RowTable = TableDefinition::new("review_finding");

/// Applied migration version -> when it was applied, in unix milliseconds.
pub(super) const SCHEMA_META: TableDefinition<'static, &'static str, u64> =
    TableDefinition::new("schema_meta");

/// Monotonic counters behind the minted record keys. One entry per key family.
pub(super) const SEQUENCE: TableDefinition<'static, &'static str, u64> =
    TableDefinition::new("sequence");

/// Every table above, so one migration can bring them all into existence and
/// read transactions never have to handle `TableDoesNotExist`.
pub(super) const ROW_TABLES: &[RowTable] = &[
    WORKFLOW,
    KVDAG_VERSION,
    KVDAG_NODE,
    KVDAG_EDGE,
    WORKFLOW_RUN,
    RUN_NODE,
    RUN_EDGE,
    RUN_EVENT,
    NODE_CHECKPOINT,
    RUN_SUMMARY,
    INTERROGATION,
    REVIEW_CYCLE,
    REVIEW_FINDING,
];

// ── key construction ────────────────────────────────────────────────────────

pub(super) const SEQ_WORKFLOW: &str = "workflow";
pub(super) const SEQ_RUN: &str = "workflow_run";
pub(super) const SEQ_INTERROGATION: &str = "interrogation";
pub(super) const SEQ_REVIEW_CYCLE: &str = "review_cycle";
pub(super) const SEQ_REVIEW_FINDING: &str = "review_finding";

pub(super) fn workflow_key(counter: u64) -> String {
    format!("w{counter:012x}")
}

/// Versions are addressed by number, so `find_version_id` is a point lookup and
/// "the tip of the chain" is the last key in the workflow's range.
pub(super) fn version_key(workflow: &str, version: u32) -> String {
    format!("{workflow}-v{version:06}")
}

pub(super) fn version_prefix(workflow: &str) -> String {
    format!("{workflow}-v")
}

/// The counter is monotonic, so run keys sort in creation order — which is also
/// `started_at` order, and breaks a same-millisecond tie deterministically.
pub(super) fn run_key(workflow: &str, counter: u64) -> String {
    format!("{workflow}-r{counter:012x}")
}

pub(super) fn run_prefix(workflow: &str) -> String {
    format!("{workflow}-r")
}

/// A row owned by one parent record: `run_node`, `run_event`, every checkpoint,
/// every `kvdag_node`. Deleting `child_prefix(parent)` deletes all of them.
pub(super) fn child_key(parent: &str, tail: &str) -> String {
    format!("{parent}{SEP}{tail}")
}

pub(super) fn child_prefix(parent: &str) -> String {
    format!("{parent}{SEP}")
}

pub(super) fn edge_key(version: &str, from: &str, to: &str, kind: &str, port: &str) -> String {
    format!("{version}{SEP}{from}{SEP}{to}{SEP}{kind}{SEP}{port}")
}

pub(super) fn run_edge_key(run: &str, from: &str, to: &str, kind: &str) -> String {
    format!("{run}{SEP}{from}{SEP}{to}{SEP}{kind}")
}

/// Zero-padded so lexicographic key order is numeric sequence order.
pub(super) fn seq_key(parent: &str, seq: u64) -> String {
    format!("{parent}{SEP}{seq:020}")
}

pub(super) fn checkpoint_key(run: &str, path: &str, seq: u64) -> String {
    format!("{run}{SEP}{path}{SEP}{seq:020}")
}

pub(super) fn checkpoint_prefix(run: &str, path: &str) -> String {
    format!("{run}{SEP}{path}{SEP}")
}

/// Exclusive upper bound for a prefix scan. `\u{10ffff}` is the largest scalar
/// value, so no `str` that starts with `prefix` can sort at or above it.
pub(super) fn prefix_end(prefix: &str) -> String {
    format!("{prefix}\u{10ffff}")
}

// ── row encoding ────────────────────────────────────────────────────────────

/// Rows are JSON, not a compact binary format, for two reasons that both
/// outrank the space it costs at this scale (a run's journal is thousands of
/// small rows, not millions):
///
/// - Several row fields are `serde_json::Value` — a checkpoint payload, a
///   node's output schema, an edge condition. `Value`'s `Deserialize` is
///   self-describing (`deserialize_any`), which a non-self-describing format
///   like bincode cannot satisfy at all.
/// - A row gains fields over time. JSON plus `#[serde(default)]` reads an older
///   row back without a migration; a positional binary encoding does not.
pub(super) fn encode<T: Serialize>(row: &T) -> Result<Vec<u8>, StoreError> {
    serde_json::to_vec(row).map_err(|error| StoreError::Query(error.to_string()))
}

pub(super) fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, StoreError> {
    serde_json::from_slice(bytes).map_err(|error| StoreError::Decode(error.to_string()))
}

// ── typed table access ──────────────────────────────────────────────────────

/// Any redb table this module stores rows in, read through a read *or* a write
/// transaction.
pub(super) trait RowReader {
    fn row_bytes(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError>;
    fn range_bytes(&self, start: &str, end: &str) -> Result<Vec<(String, Vec<u8>)>, StoreError>;

    /// Whether a row exists, without paying to decode it.
    fn row_exists(&self, key: &str) -> Result<bool, StoreError> {
        Ok(self.row_bytes(key)?.is_some())
    }
}

impl<T> RowReader for T
where
    T: ReadableTable<&'static str, &'static [u8]>,
{
    fn row_bytes(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let found = self.get(key).map_err(storage_error)?;
        Ok(found.map(|value| value.value().to_vec()))
    }

    fn range_bytes(&self, start: &str, end: &str) -> Result<Vec<(String, Vec<u8>)>, StoreError> {
        let mut rows = Vec::new();
        for entry in self.range(start..end).map_err(storage_error)? {
            let (key, value) = entry.map_err(storage_error)?;
            rows.push((key.value().to_string(), value.value().to_vec()));
        }
        Ok(rows)
    }
}

/// One row by key, decoded.
pub(super) fn get_row<T, R>(table: &R, key: &str) -> Result<Option<T>, StoreError>
where
    T: DeserializeOwned,
    R: RowReader,
{
    match table.row_bytes(key)? {
        Some(bytes) => decode(&bytes).map(Some),
        None => Ok(None),
    }
}

/// Every row under `prefix`, decoded, in key order.
pub(super) fn scan_prefix<T, R>(table: &R, prefix: &str) -> Result<Vec<T>, StoreError>
where
    T: DeserializeOwned,
    R: RowReader,
{
    let end = prefix_end(prefix);
    table
        .range_bytes(prefix, &end)?
        .into_iter()
        .map(|(_, bytes)| decode(&bytes))
        .collect()
}

/// Every key under `prefix`, in key order. Used where the row itself is not
/// needed — pruning, and existence checks.
pub(super) fn scan_prefix_keys<R>(table: &R, prefix: &str) -> Result<Vec<String>, StoreError>
where
    R: RowReader,
{
    let end = prefix_end(prefix);
    Ok(table
        .range_bytes(prefix, &end)?
        .into_iter()
        .map(|(key, _)| key)
        .collect())
}

// ── error mapping ───────────────────────────────────────────────────────────

pub(super) fn storage_error(error: impl std::fmt::Display) -> StoreError {
    StoreError::Query(error.to_string())
}

/// Reads and bumps one of the [`SEQUENCE`] counters. Counters are allocated
/// inside the same write transaction as the row they name, so an aborted
/// transaction never burns an id.
pub(super) fn next_counter(
    table: &mut redb::Table<'_, &'static str, u64>,
    name: &str,
) -> Result<u64, StoreError> {
    let current = table
        .get(name)
        .map_err(storage_error)?
        .map_or(0, |value| value.value());
    let next = current + 1;
    table.insert(name, next).map_err(storage_error)?;
    Ok(next)
}

/// Deletes every row under `prefix`. The only shape of delete this store has:
/// callers pass a whole record's prefix, never a hand-built sub-range.
pub(super) fn delete_prefix(
    table: &mut redb::Table<'_, &'static str, &'static [u8]>,
    prefix: &str,
) -> Result<(), StoreError> {
    let keys = scan_prefix_keys(&*table, prefix)?;
    for key in keys {
        table.remove(key.as_str()).map_err(storage_error)?;
    }
    Ok(())
}

/// Inserts a row that must not already exist. Every journal table goes through
/// this: re-appending an existing `(run, seq)` is a caller bug, and silently
/// overwriting the earlier record would lose it.
pub(super) fn insert_new<T: Serialize>(
    table: &mut redb::Table<'_, &'static str, &'static [u8]>,
    key: &str,
    row: &T,
    what: &str,
) -> Result<(), StoreError> {
    let bytes = encode(row)?;
    let replaced = table
        .insert(key, bytes.as_slice())
        .map_err(storage_error)?
        .is_some();
    if replaced {
        return Err(StoreError::Query(format!("duplicate {what}: {key}")));
    }
    Ok(())
}

/// Inserts or replaces a row. Only the two mutable records use it: `workflow`
/// (its head pointer and timestamps) and `workflow_run`/`run_node` progress.
pub(super) fn put_row<T: Serialize>(
    table: &mut redb::Table<'_, &'static str, &'static [u8]>,
    key: &str,
    row: &T,
) -> Result<(), StoreError> {
    let bytes = encode(row)?;
    table.insert(key, bytes.as_slice()).map_err(storage_error)?;
    Ok(())
}

// ── time ────────────────────────────────────────────────────────────────────

/// Unix milliseconds. A clock before the epoch (only reachable with a badly
/// misconfigured host) clamps to 0 rather than wrapping.
pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

/// RFC 3339 in UTC, for the one journal field that is carried as text.
/// Hand-rolled because karvex has no date/time dependency and adding one for a
/// single formatter would cost more binary than the twenty lines below.
pub(super) fn rfc3339_utc(unix_ms: i64) -> String {
    let seconds = unix_ms.div_euclid(1_000);
    let millis = unix_ms.rem_euclid(1_000);
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        second_of_day / 3_600,
        (second_of_day % 3_600) / 60,
        second_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Howard Hinnant's `civil_from_days`: days since the unix epoch to a proleptic
/// Gregorian date.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_scan_cannot_bleed_into_a_sibling() {
        let run = run_key(&workflow_key(1), 1);
        let other = run_key(&workflow_key(1), 2);
        let prefix = child_prefix(&run);
        assert!(child_key(&run, "solo").starts_with(&prefix));
        assert!(!child_key(&other, "solo").starts_with(&prefix));
        // The bound has to exclude the sibling too, not just fail to match it.
        assert!(child_key(&other, "solo").as_str() >= prefix_end(&prefix).as_str());
    }

    #[test]
    fn instance_paths_that_share_a_prefix_stay_separate() {
        let run = run_key(&workflow_key(1), 1);
        let prefix = checkpoint_prefix(&run, "solo");
        assert!(checkpoint_key(&run, "solo", 3).starts_with(&prefix));
        assert!(!checkpoint_key(&run, "solo2", 3).starts_with(&prefix));
    }

    #[test]
    fn sequence_numbers_sort_numerically_as_keys() {
        let run = run_key(&workflow_key(1), 1);
        assert!(seq_key(&run, 9) < seq_key(&run, 10));
        assert!(seq_key(&run, 2) < seq_key(&run, 1_000));
    }

    #[test]
    fn run_keys_sort_in_creation_order() {
        let workflow = workflow_key(1);
        assert!(run_key(&workflow, 9) < run_key(&workflow, 16));
        assert!(run_key(&workflow, 255) < run_key(&workflow, 256));
    }

    #[test]
    fn version_keys_sort_by_version_number() {
        let workflow = workflow_key(3);
        assert!(version_key(&workflow, 2) < version_key(&workflow, 10));
        assert!(version_key(&workflow, 2).starts_with(&version_prefix(&workflow)));
    }

    #[test]
    fn timestamps_render_as_utc_rfc3339() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_utc(1_000), "1970-01-01T00:00:01.000Z");
        // 2024-02-29T12:34:56.789Z — a leap day, which is where a hand-rolled
        // civil calendar goes wrong if it goes wrong at all.
        assert_eq!(rfc3339_utc(1_709_210_096_789), "2024-02-29T12:34:56.789Z");
    }
}
