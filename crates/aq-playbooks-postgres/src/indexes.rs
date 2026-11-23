use crate::backend;
use crate::profiling::ObservedTransaction;
use crate::projection::receipt_kind;
#[cfg(test)]
use aq_playbooks_runner::inspection;
use aq_playbooks_runner::{EffectQueueKind, IndexItem, IndexPage, IndexQuery, State, StorageError};
use postgres::Client;
use serde_json::{Value, json};

/// Delete the runtime-index rows of receipts named in `$2` (kind, run,
/// sequence as `item_key`) by sequence, whatever key they are stored under;
/// a row named with `keep` survives under exactly that key. With planner
/// statistics this is one lookup of `runtime_correlated_sequence` per named
/// row, then a delete by tuple id: the `OFFSET 0` keeps the planner from
/// flattening the lookup into a hash join over a scan of the namespace's
/// rows, which it otherwise prefers on small and mid-sized tables. Without
/// statistics it may still answer each lookup by a primary-key prefix scan
/// (no query shape avoids that), which is why opening the storage analyzes
/// tables that have none (`analyze_unanalyzed`).
pub(crate) const RECEIPT_ROWS_BY_SEQUENCE_DELETE: &str = "DELETE FROM aq_playbooks.runtime_index WHERE ctid=ANY(ARRAY(SELECT r.ctid FROM jsonb_to_recordset($2) AS d(kind text,lookup_key text,item_key text,keep boolean) CROSS JOIN LATERAL (SELECT old.ctid FROM aq_playbooks.runtime_index old WHERE old.namespace=$1 AND old.kind=d.kind AND old.lookup_key=d.lookup_key AND old.sequence=d.item_key::bigint AND (NOT d.keep OR old.item_key<>d.item_key) OFFSET 0) r))";
pub(crate) fn sync(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    previous: &State,
    next: &State,
) -> Result<(), StorageError> {
    let (changed, deleted) = crate::projection::native_delta(previous, next);
    let is_receipt = |row: &Value| row["kind"].as_str().is_some_and(receipt_kind);
    // Receipt rows go by sequence, whatever their stored key: a row an
    // engine before this keyed by the receipt's position is deleted when the
    // receipt leaves, or when this commit writes the receipt's row (never a
    // second row for one receipt). O(the receipts this commit adds or
    // removes); no other receipt's row is touched.
    let by_sequence: Vec<Value> = deleted
        .iter()
        .filter(|row| is_receipt(row))
        .map(|row| json!({"kind":row["kind"],"lookup_key":row["lookup_key"],"item_key":row["item_key"],"keep":false}))
        .chain(
            changed
                .iter()
                .filter(|row| is_receipt(row))
                .map(|row| json!({"kind":row["kind"],"lookup_key":row["lookup_key"],"item_key":row["item_key"],"keep":true})),
        )
        .collect();
    if !by_sequence.is_empty() {
        tx.execute(
            RECEIPT_ROWS_BY_SEQUENCE_DELETE,
            &[&namespace, &Value::Array(by_sequence)],
        )
        .map_err(backend)?;
    }
    let deleted: Vec<Value> = deleted.into_iter().filter(|row| !is_receipt(row)).collect();
    if !deleted.is_empty() {
        tx.execute("DELETE FROM aq_playbooks.runtime_index old USING jsonb_to_recordset($2) AS d(kind text,lookup_key text,item_key text) WHERE old.namespace=$1 AND (old.kind,old.lookup_key,old.item_key)=(d.kind,d.lookup_key,d.item_key)",&[&namespace,&Value::Array(deleted)]).map_err(backend)?;
    }
    if !changed.is_empty() {
        tx.execute("INSERT INTO aq_playbooks.runtime_index(namespace,kind,lookup_key,item_key,scope,sequence,at_ms) SELECT $1,kind,lookup_key,item_key,scope,sequence,at_ms FROM jsonb_to_recordset($2) AS d(kind text,lookup_key text,item_key text,scope text,sequence bigint,at_ms bigint) ON CONFLICT(namespace,kind,lookup_key,item_key) DO UPDATE SET scope=EXCLUDED.scope,sequence=EXCLUDED.sequence,at_ms=EXCLUDED.at_ms",&[&namespace,&Value::Array(changed)]).map_err(backend)?;
    }
    let (changed, deleted) = crate::projection::public_delta(previous, next);
    if !deleted.is_empty() {
        tx.execute("DELETE FROM aq_playbooks.inspection_rows old USING jsonb_to_recordset($2) AS d(kind text,item_key text) WHERE old.namespace=$1 AND (old.kind,old.item_key)=(d.kind,d.item_key)",&[&namespace,&Value::Array(deleted)]).map_err(backend)?;
    }
    if !changed.is_empty() {
        tx.execute("INSERT INTO aq_playbooks.inspection_rows(namespace,kind,item_key,run_id,sequence,public_value) SELECT $1,kind,item_key,run_id,sequence,public_value FROM jsonb_to_recordset($2) AS d(kind text,item_key text,run_id text,sequence bigint,public_value jsonb) ON CONFLICT(namespace,kind,item_key) DO UPDATE SET run_id=EXCLUDED.run_id,sequence=EXCLUDED.sequence,public_value=EXCLUDED.public_value",&[&namespace,&Value::Array(changed)]).map_err(backend)?;
    }
    Ok(())
}

pub(crate) fn query(
    client: &mut Client,
    namespace: &str,
    query: &IndexQuery,
) -> Result<IndexPage, StorageError> {
    let limit = query.page_limit() as i64;
    if limit == 0 {
        return Ok(IndexPage::default());
    }
    let rows=match query {
        IndexQuery::Ready{scope_after,..}=>{client.query("SELECT item_key,scope,sequence,at_ms FROM (SELECT item_key,scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='ready') q ORDER BY (scope>COALESCE($2::text,(SELECT item_key FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='cursor' AND lookup_key='ready'),'')) DESC,scope LIMIT $3",&[&namespace,scope_after,&limit])},
        IndexQuery::Effects{kind:EffectQueueKind::Completion,..}=>client.query("SELECT item_key,scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='effect:completion' ORDER BY item_key LIMIT $2",&[&namespace,&limit]),
        IndexQuery::Effects{kind,scope_after,..}=>{let cursor_kind=kind.key();let kind=format!("effect:{cursor_kind}");client.query("SELECT item_key,scope,sequence,at_ms FROM (SELECT item_key,scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind=$2) q ORDER BY (scope>COALESCE($3::text,(SELECT item_key FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='cursor' AND lookup_key=$5),'')) DESC,scope LIMIT $4",&[&namespace,&kind,scope_after,&limit,&cursor_kind])},
        IndexQuery::DueTimers{now_ms,..}=>client.query("SELECT item_key,scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='timer' AND at_ms<=$2 ORDER BY at_ms,item_key LIMIT $3",&[&namespace,now_ms,&limit]),
        IndexQuery::CorrelatedEvents{key,after_sequence,..}=>{let after=i64::try_from(*after_sequence).map_err(|_|backend("index sequence exhausted"))?;client.query("SELECT item_key,scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='event' AND lookup_key=$2 AND sequence>$3 ORDER BY sequence,item_key LIMIT $4",&[&namespace,key,&after,&limit])},
        IndexQuery::Waiters{key,..}=>client.query("SELECT item_key,scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='waiter' AND lookup_key=$2 ORDER BY item_key LIMIT $3",&[&namespace,key,&limit]),
        IndexQuery::Runs{after,..}=>client.query("SELECT item_key,scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='run' AND ($2::text IS NULL OR item_key>$2) ORDER BY item_key LIMIT $3",&[&namespace,after,&limit]),
        IndexQuery::Receipts{run,after_sequence,..}=>{let after=i64::try_from(*after_sequence).map_err(|_|backend("index sequence exhausted"))?;let kind=if run.is_some(){"receipt"}else{"all_receipt"};let lookup=run.as_deref().unwrap_or("");client.query("SELECT lpad(sequence::text,20,'0'),scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind=$2 AND lookup_key=$3 AND sequence>$4 ORDER BY sequence,item_key LIMIT $5",&[&namespace,&kind,&lookup,&after,&limit])},
    }.map_err(backend)?;
    let items: Vec<_> = rows
        .into_iter()
        .map(|row| IndexItem {
            key: row.get(0),
            scope: row.get(1),
            sequence: row.get::<_, i64>(2) as u64,
            at_ms: row.get(3),
        })
        .collect();
    Ok(query.page(items))
}

#[cfg(test)]
mod reference;
#[cfg(test)]
use reference::{delta, native_rows, public_rows};
#[cfg(test)]
mod projection_tests;

#[cfg(test)]
mod projection_field_tests {
    use super::*;
    fn candidates(v: &Value) -> Vec<Value> {
        let mut out = vec![Value::Null, json!("changed"), json!(true), json!(1)];
        match v {
            Value::String(s) => out.push(json!(format!("{s}-changed"))),
            Value::Number(n) => {
                if let Some(n) = n.as_u64() {
                    out.push(json!(n.saturating_add(1)));
                }
            }
            Value::Array(a) => {
                let mut a = a.clone();
                a.push(json!("changed"));
                out.push(Value::Array(a));
            }
            Value::Object(o) => {
                let mut o = o.clone();
                o.insert("changed".into(), json!("value"));
                out.push(Value::Object(o));
            }
            _ => {}
        }
        out
    }
    #[test]
    fn source_field_mutations_cannot_change_projection_without_being_detected() {
        // Golden source includes all serialized fields, including private ones.
        // Valid primitive mutations exercise the quick-equivalence gate against
        // the existing public sanitizer instead of reasserting its field list.
        let run = serde_json::json!({"catalog_revision":0,"id":"r","root":"r","parent":null,"origin":null,"artifact":"a","entry":"main","dependencies":{},"authority":{"actor":"a","scope":"s","capabilities":[]},"step":"end","status":"completed","input":null,"variables":{},"outcome":"ok","output":null,"reason":null,"admitted_at_ms":0,"visits":1,"depth":0,"children":[],"pending":null,"loops":[],"attempts":{},"pending_return":null,"pending_failure":false,"terminal_sequence":1});
        let mut old = State::default();
        old.runs
            .insert("r".into(), serde_json::from_value(run.clone()).unwrap());
        let mut checked = 0;
        for (field, value) in run.as_object().unwrap() {
            for replacement in candidates(value) {
                let mut changed = run.clone();
                changed[field] = replacement;
                if let Ok(run) = serde_json::from_value::<aq_playbooks_runner::Run>(changed) {
                    let mut next = old.clone();
                    next.runs.insert("r".into(), run);
                    assert_eq!(
                        crate::projection::public_delta(&old, &next),
                        delta(public_rows(&old), public_rows(&next), &["kind", "item_key"]),
                        "field {field}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 30);
    }
}
