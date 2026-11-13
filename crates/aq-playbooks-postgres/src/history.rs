//! `aq_playbooks.run_history` (spec 0003 §2): the public inspection items of
//! settled cohorts, inserted once by the commit that removes them from the
//! State. A repeated item violates the primary key and fails that commit.
//! Every statement runs in the caller's transaction, which has set the
//! namespace writer marker the table's triggers require.
use crate::backend;
use crate::profiling::ObservedTransaction;
use aq_playbooks_runner::{HistoryItem, InspectionQuery, StorageError};
use serde_json::{Value, json};

/// Insert `items` in the commit's transaction; a key already recorded fails.
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
    tx.phase("history_write");
    tx.execute("INSERT INTO aq_playbooks.run_history(namespace,kind,item_key,run_id,root_id,sequence,finished_at_ms,public_value) SELECT $1,kind,item_key,run_id,root_id,sequence,finished_at_ms,public_value FROM jsonb_to_recordset($2) AS d(kind text,item_key text,run_id text,root_id text,sequence bigint,finished_at_ms bigint,public_value jsonb)",&[&namespace,&Value::Array(rows)]).map_err(backend)?;
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
    let rows = tx.query("SELECT item_key,run_id,root_id,sequence,finished_at_ms,public_value FROM aq_playbooks.run_history WHERE namespace=$1 AND kind=$2 AND ($3::text IS NULL OR run_id=$3) AND ($4::text IS NULL OR item_key>$4) ORDER BY item_key LIMIT $5",&[&namespace,&query.kind.key(),&query.run,&query.after,&limit]).map_err(backend)?;
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
