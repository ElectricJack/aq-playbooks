//! `aq_playbooks.run_history` (spec 0003 §2): the public inspection items of
//! settled cohorts, inserted once by the commit that removes them from the
//! State. A repeated item violates the primary key and fails that commit.
//! History is a rolling log: the commit that writes history deletes the
//! oldest finished runs beyond `policy.history_keep` (migration 7: the
//! `run_history_runs_by_finish` index and the `run_history_counts` table).
//! Every statement runs in the caller's transaction, which has set the
//! namespace writer marker the table's triggers require.
use crate::backend;
use crate::profiling::ObservedTransaction;
use aq_playbooks_runner::history::{self, ALREADY_RECORDED, ALSO_LIVE, ItemKey};
use aq_playbooks_runner::{HistoryItem, InspectionQuery, StorageError};
use postgres::error::SqlState;
use serde_json::{Value, json};

/// One history page: the primary key (or `run_history_by_run`) in key order.
pub(crate) const PAGE_SQL: &str = "SELECT item_key,run_id,root_id,sequence,finished_at_ms,public_value FROM aq_playbooks.run_history WHERE namespace=$1 AND kind=$2 AND ($3::text IS NULL OR run_id=$3) AND ($4::text IS NULL OR item_key>$4) ORDER BY item_key LIMIT $5";

/// The oldest finished runs of a namespace's history, in finish order:
/// `run_history_runs_by_finish`, no sort.
pub(crate) const OLDEST_RUNS_SQL: &str = "SELECT item_key FROM aq_playbooks.run_history WHERE namespace=$1 AND kind='runs' ORDER BY finished_at_ms,item_key LIMIT $2";
/// The other items of runs being trimmed: `run_history_by_run`.
pub(crate) const TRIM_ITEMS_SQL: &str = "DELETE FROM aq_playbooks.run_history WHERE namespace=$1 AND kind IN ('effects','receipts') AND run_id=ANY($2)";
/// Then their `runs` rows: the primary key.
pub(crate) const TRIM_RUNS_SQL: &str =
    "DELETE FROM aq_playbooks.run_history WHERE namespace=$1 AND kind='runs' AND item_key=ANY($2)";
/// Change the finished-run count of `namespace` by `delta`.
fn count_runs(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    delta: i64,
) -> Result<(), StorageError> {
    // The row's CHECK (runs >= 0) also judges an upsert's proposed row, so
    // only an increase may insert it.
    if delta > 0 {
        tx.execute("INSERT INTO aq_playbooks.run_history_counts(namespace,runs) VALUES($1,$2) ON CONFLICT(namespace) DO UPDATE SET runs=aq_playbooks.run_history_counts.runs+EXCLUDED.runs",&[&namespace,&delta]).map_err(backend)?;
    } else if delta < 0 {
        tx.execute(
            "UPDATE aq_playbooks.run_history_counts SET runs=runs+$2 WHERE namespace=$1",
            &[&namespace, &delta],
        )
        .map_err(backend)?;
    }
    Ok(())
}
/// Write a commit's history in its transaction under `keep`
/// (`policy.history_keep`): insert `items` (none when `keep` is 0; a key
/// already recorded fails the commit with [`ALREADY_RECORDED`]), then delete
/// the oldest finished runs beyond `keep`, at most
/// [`history::HISTORY_TRIM_RUNS_PER_COMMIT`] more than the commit writes, each run's
/// items before its `runs` row. Every statement is index-backed; nothing
/// counts rows.
pub(crate) fn insert(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    items: &[HistoryItem],
    keep: u64,
) -> Result<(), StorageError> {
    if items.is_empty() {
        return Ok(());
    }
    let written = history::runs_written(items);
    if keep > 0 {
        write(tx, namespace, items)?;
        count_runs(tx, namespace, written as i64)?;
    }
    trim(tx, namespace, keep, written)
}
fn trim(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    keep: u64,
    written: usize,
) -> Result<(), StorageError> {
    let runs: i64 = tx
        .query_opt(
            "SELECT runs FROM aq_playbooks.run_history_counts WHERE namespace=$1",
            &[&namespace],
        )
        .map_err(backend)?
        .map_or(0, |row| row.get(0));
    let budget = history::trim_budget(runs.max(0) as u64, written, keep);
    if budget == 0 {
        return Ok(());
    }
    tx.phase("history_trim");
    let victims: Vec<String> = tx
        .query(OLDEST_RUNS_SQL, &[&namespace, &(budget as i64)])
        .map_err(backend)?
        .into_iter()
        .map(|row| row.get(0))
        .collect();
    tx.execute(TRIM_ITEMS_SQL, &[&namespace, &victims])
        .map_err(backend)?;
    let trimmed = tx
        .execute(TRIM_RUNS_SQL, &[&namespace, &victims])
        .map_err(backend)?;
    count_runs(tx, namespace, -(trimmed as i64))
}
fn write(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    items: &[HistoryItem],
) -> Result<(), StorageError> {
    let rows = items
        .iter()
        .map(|item| {
            json!({"kind":item.kind.key(),"item_key":item.key,"run_id":item.run,"root_id":item.root,
                "sequence":item.sequence,"finished_at_ms":item.finished_at_ms,"public_value":item.value})
        })
        .collect();
    tx.phase("history");
    tx.execute("INSERT INTO aq_playbooks.run_history(namespace,kind,item_key,run_id,root_id,sequence,finished_at_ms,public_value) SELECT $1,kind,item_key,run_id,root_id,sequence,finished_at_ms,public_value FROM jsonb_to_recordset($2) AS d(kind text,item_key text,run_id text,root_id text,sequence bigint,finished_at_ms bigint,public_value jsonb)",&[&namespace,&Value::Array(rows)]).map_err(|error| {
        let repeated = error.as_db_error().is_some_and(|db| {
            *db.code() == SqlState::UNIQUE_VIOLATION && db.constraint() == Some("run_history_pkey")
        });
        if repeated { StorageError::Backend(ALREADY_RECORDED.into()) } else { backend(error) }
    })?;
    Ok(())
}
/// Refuse live `keys` already recorded as history ([`ALSO_LIVE`]): one
/// primary-key probe per key the commit writes as live.
pub(crate) fn check_unrecorded(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    keys: &[ItemKey],
) -> Result<(), StorageError> {
    if keys.is_empty() {
        return Ok(());
    }
    let (kinds, item_keys): (Vec<_>, Vec<_>) = keys
        .iter()
        .map(|(kind, key)| (kind.key().to_string(), key.clone()))
        .unzip();
    tx.phase("history");
    if tx.query_opt("SELECT 1 FROM aq_playbooks.run_history h JOIN unnest($2::text[],$3::text[]) AS k(kind,item_key) ON h.kind=k.kind AND h.item_key=k.item_key WHERE h.namespace=$1 LIMIT 1",&[&namespace,&kinds,&item_keys]).map_err(backend)?.is_some() {
        return Err(StorageError::Backend(ALSO_LIVE.into()));
    }
    Ok(())
}
/// At most `query.limit + 1` history items matching `query`, in key order.
pub(crate) fn page(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    query: &InspectionQuery,
) -> Result<Vec<HistoryItem>, StorageError> {
    query.validate()?;
    let limit = (query.limit + 1) as i64;
    let rows = tx
        .query(
            PAGE_SQL,
            &[
                &namespace,
                &query.kind.key(),
                &query.run,
                &query.after,
                &limit,
            ],
        )
        .map_err(backend)?;
    Ok(rows
        .into_iter()
        .map(|row| HistoryItem {
            kind: query.kind,
            key: row.get(0),
            run: row.get(1),
            root: row.get(2),
            sequence: row.get::<_, i64>(3) as u64,
            finished_at_ms: row.get(4),
            value: row.get(5),
        })
        .collect())
}
/// Delete the items of runs finished before `before_ms` (index
/// `run_history_by_finish`), keeping the finished-run count; returns how
/// many items.
pub(crate) fn delete_before(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    before_ms: i64,
) -> Result<u64, StorageError> {
    let row = tx
        .query_one(
            "WITH d AS (DELETE FROM aq_playbooks.run_history WHERE namespace=$1 AND finished_at_ms<$2 RETURNING kind) SELECT count(*),count(*) FILTER (WHERE kind='runs') FROM d",
            &[&namespace, &before_ms],
        )
        .map_err(backend)?;
    let (items, runs): (i64, i64) = (row.get(0), row.get(1));
    count_runs(tx, namespace, -runs)?;
    Ok(items as u64)
}
