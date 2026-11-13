//! `aq_playbooks.run_history` (spec 0003 §2): the public inspection items of
//! settled cohorts, inserted once by the commit that removes them from the
//! State. A repeated item violates the primary key and fails that commit.
//! Every statement runs in the caller's transaction, which has set the
//! namespace writer marker the table's triggers require.
use crate::backend;
use crate::profiling::ObservedTransaction;
use aq_playbooks_runner::history::{ALREADY_RECORDED, ALSO_LIVE, ItemKey};
use aq_playbooks_runner::{HistoryItem, InspectionQuery, StorageError};
use postgres::error::SqlState;
use serde_json::{Value, json};

/// One history page: the primary key (or `run_history_by_run`) in key order.
pub(crate) const PAGE_SQL: &str = "SELECT item_key,run_id,root_id,sequence,finished_at_ms,public_value FROM aq_playbooks.run_history WHERE namespace=$1 AND kind=$2 AND ($3::text IS NULL OR run_id=$3) AND ($4::text IS NULL OR item_key>$4) ORDER BY item_key LIMIT $5";

/// Insert `items` in the commit's transaction; a key already recorded fails
/// the commit with [`ALREADY_RECORDED`].
pub(crate) fn insert(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    items: &[HistoryItem],
) -> Result<(), StorageError> {
    if items.is_empty() {
        return Ok(());
    }
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
/// `run_history_by_finish`); returns how many.
pub(crate) fn delete_before(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    before_ms: i64,
) -> Result<u64, StorageError> {
    tx.execute(
        "DELETE FROM aq_playbooks.run_history WHERE namespace=$1 AND finished_at_ms<$2",
        &[&namespace, &before_ms],
    )
    .map_err(backend)
}
