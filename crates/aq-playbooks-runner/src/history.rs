//! Run history (spec 0003 §2). A settled cohort leaves the State in the commit
//! that settles it and is handed to storage in that same commit, which writes
//! its public inspection items once as history. History holds public
//! projections only; it is inserted once, never updated, and deleted by the
//! host by finish time.
use crate::inspection::{self, InspectionKind, InspectionQuery, InspectionRow};
use crate::{Intent, Receipt, Run, StorageError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One public inspection item of a finished run. `key` and `sequence` are
/// exactly those of [`inspection::rows`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryItem {
    pub kind: InspectionKind,
    pub key: String,
    pub run: String,
    pub root: String,
    pub sequence: u64,
    pub finished_at_ms: i64,
    pub value: serde_json::Value,
}
/// A root and everything that belonged to it, removed from the State by the
/// commit that carries it (`State::settled`).
#[derive(Debug, Clone)]
pub struct SettledCohort {
    pub root: String,
    pub finished_at_ms: i64,
    pub runs: Vec<Run>,
    pub intents: Vec<Intent>,
    pub receipts: Vec<Receipt>,
}
impl SettledCohort {
    pub fn history_items(&self) -> Vec<HistoryItem> {
        let item = |row: InspectionRow| HistoryItem {
            kind: row.kind,
            key: row.key,
            run: row.run,
            root: self.root.clone(),
            sequence: row.sequence,
            finished_at_ms: self.finished_at_ms,
            value: row.value,
        };
        self.runs
            .iter()
            .map(inspection::run_row)
            .chain(self.intents.iter().map(inspection::intent_row))
            .chain(self.receipts.iter().map(inspection::receipt_row))
            .map(item)
            .collect()
    }
}
pub fn history_items(settled: &[SettledCohort]) -> Vec<HistoryItem> {
    settled
        .iter()
        .flat_map(SettledCohort::history_items)
        .collect()
}

/// History rows of one namespace in the in-memory backends, by
/// `(kind.key(), key)`.
pub(crate) type HistoryRows = BTreeMap<(String, String), HistoryItem>;
fn row_key(item: &HistoryItem) -> (String, String) {
    (item.kind.key().to_string(), item.key.clone())
}
/// Refuse a commit whose history repeats an item already recorded (or repeats
/// one within itself). Runs before anything is inserted.
pub(crate) fn check_new(rows: &HistoryRows, items: &[HistoryItem]) -> Result<(), StorageError> {
    let mut seen = std::collections::BTreeSet::new();
    for item in items {
        let key = row_key(item);
        if rows.contains_key(&key) || !seen.insert(key) {
            return Err(StorageError::Backend(
                "run history item already recorded".into(),
            ));
        }
    }
    Ok(())
}
pub(crate) fn insert(rows: &mut HistoryRows, items: Vec<HistoryItem>) {
    for item in items {
        rows.insert(row_key(&item), item);
    }
}
/// At most `limit + 1` items matching `query`, in key order.
pub(crate) fn page(
    rows: &HistoryRows,
    query: &InspectionQuery,
) -> Result<Vec<HistoryItem>, StorageError> {
    use std::ops::Bound::{Excluded, Included, Unbounded};
    query.validate()?;
    let kind = query.kind.key().to_string();
    let start = match &query.after {
        Some(after) => Excluded((kind.clone(), after.clone())),
        None => Included((kind.clone(), String::new())),
    };
    Ok(rows
        .range((start, Unbounded))
        .take_while(|((k, _), _)| *k == kind)
        .map(|(_, item)| item)
        .filter(|item| query.run.as_ref().is_none_or(|run| &item.run == run))
        .take(query.limit + 1)
        .cloned()
        .collect())
}
/// Delete the items of runs finished before `before_ms`; returns how many.
pub(crate) fn delete_before(rows: &mut HistoryRows, before_ms: i64) -> u64 {
    let before = rows.len();
    rows.retain(|_, item| item.finished_at_ms >= before_ms);
    (before - rows.len()) as u64
}
