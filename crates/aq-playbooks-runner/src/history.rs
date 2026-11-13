//! Run history (spec 0003 §2). A settled cohort leaves the State in the commit
//! that settles it and is handed to storage in that same commit, which writes
//! its public inspection items once as history. History holds public
//! projections only; it is inserted once, never updated, and deleted by the
//! host by finish time.
use crate::inspection::{self, InspectionKind, InspectionQuery, InspectionRow};
use crate::lazy::{ContainerChange, TreeKey};
use crate::transition::StatePatch;
use crate::{Intent, Receipt, Run, State, StorageError};
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

/// Refusal of a commit that would record an item a second time.
pub const ALREADY_RECORDED: &str = "run history item already recorded";
/// Refusal of a commit that would leave an item key both live and in history.
pub const ALSO_LIVE: &str = "run history item key is also live";

/// An inspection item key: `(kind, key)` as in [`inspection::rows`].
pub type ItemKey = (InspectionKind, String);
fn receipt_key(sequence: u64) -> String {
    format!("{sequence:020}")
}
fn also_live() -> StorageError {
    StorageError::Backend(ALSO_LIVE.into())
}
/// Refuse history `items` whose key is live in `state`, the State the commit
/// stores. Bounded on a bound State: map lookups, and receipts through the
/// runtime index once it is initialized (a scan of `receipts` otherwise).
pub fn check_not_live(state: &State, items: &[HistoryItem]) -> Result<(), StorageError> {
    for item in items {
        let live = match item.kind {
            InspectionKind::Runs => state.runs.contains_key(&item.key),
            InspectionKind::Effects => state.intents.contains_key(&item.key),
            InspectionKind::Receipts if state.indexes.initialized => state
                .indexes
                .all_receipts
                .get(&item.sequence)
                .and_then(|&offset| state.receipts.get(offset))
                .is_some_and(|r| r.sequence == item.sequence),
            InspectionKind::Receipts => state.receipts.iter().any(|r| r.sequence == item.sequence),
        };
        if live {
            return Err(also_live());
        }
    }
    Ok(())
}
/// Item keys live in `next` and not in `previous` (complete States).
pub fn new_live_keys(previous: &State, next: &State) -> Vec<ItemKey> {
    let old_receipts: std::collections::BTreeSet<u64> =
        previous.receipts.iter().map(|r| r.sequence).collect();
    let runs = next
        .runs
        .keys()
        .filter(|id| !previous.runs.contains_key(*id))
        .map(|id| (InspectionKind::Runs, id.clone()));
    let intents = next
        .intents
        .keys()
        .filter(|id| !previous.intents.contains_key(*id))
        .map(|id| (InspectionKind::Effects, id.clone()));
    let receipts = next
        .receipts
        .iter()
        .filter(|r| !old_receipts.contains(&r.sequence))
        .map(|r| (InspectionKind::Receipts, receipt_key(r.sequence)));
    runs.chain(intents).chain(receipts).collect()
}
/// Item keys a transition patch writes as live (bounded by the patch).
pub fn patch_live_keys(patch: &StatePatch) -> Result<Vec<ItemKey>, StorageError> {
    let mut keys = vec![];
    for (path, change) in &patch.changes {
        let kind = match (path.outer.is_none(), path.field) {
            (true, "runs") => InspectionKind::Runs,
            (true, "intents") => InspectionKind::Effects,
            (true, "receipts") => InspectionKind::Receipts,
            _ => continue,
        };
        let written: Vec<(&TreeKey, &[u8])> = match change {
            ContainerChange::Entries(rows) => rows
                .iter()
                .filter_map(|(k, v)| v.as_deref().map(|v| (k, v)))
                .collect(),
            ContainerChange::Replace(rows) => rows.iter().map(|(k, v)| (k, v.as_slice())).collect(),
        };
        for (key, value) in written {
            keys.push(match (kind, key) {
                (InspectionKind::Receipts, _) => {
                    let receipt: Receipt = serde_json::from_slice(value).map_err(|e| {
                        StorageError::Backend(format!("receipt record decode: {e}"))
                    })?;
                    (kind, receipt_key(receipt.sequence))
                }
                (_, TreeKey::Str(id)) => (kind, id.clone()),
                _ => return Err(StorageError::Backend("unexpected record key".into())),
            });
        }
    }
    Ok(keys)
}
/// Refuse live `keys` already recorded as history.
pub(crate) fn check_unrecorded(rows: &HistoryRows, keys: &[ItemKey]) -> Result<(), StorageError> {
    if keys
        .iter()
        .any(|(kind, key)| rows.contains_key(&(kind.key().to_string(), key.clone())))
    {
        return Err(also_live());
    }
    Ok(())
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
            return Err(StorageError::Backend(ALREADY_RECORDED.into()));
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
