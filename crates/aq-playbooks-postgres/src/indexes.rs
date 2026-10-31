use crate::backend;
use crate::profiling::ObservedTransaction;
use aq_playbooks_runner::{
    EffectQueueKind, IndexItem, IndexPage, IndexQuery, State, StorageError, inspection,
};
use postgres::Client;
use serde_json::{Value, json};

fn record(
    kind: &str,
    lookup: &str,
    key: &str,
    scope: &str,
    sequence: u64,
    at: Option<i64>,
) -> Value {
    json!({"kind":kind,"lookup_key":lookup,"item_key":key,"scope":scope,"sequence":sequence,"at_ms":at})
}
fn native_rows(state: &State) -> Vec<Value> {
    let mut rows = vec![];
    let idx = &state.indexes;
    if let Some(cursor) = &idx.scope_cursor {
        rows.push(record("cursor", "ready", cursor, "", 0, None));
    }
    for (kind, cursor) in &idx.effect_cursor {
        rows.push(record("cursor", kind, cursor, "", 0, None));
    }
    for (kind_scope, cursor) in &idx.effect_item_cursor {
        rows.push(record("item_cursor", kind_scope, cursor, "", 0, None));
    }
    for (scope, queue) in &idx.ready {
        if let Some((sequence, id)) = queue.first_key_value() {
            rows.push(record("ready", "", id, scope, *sequence, None));
        }
    }
    for (kind, scopes) in &idx.effects {
        for (scope, ids) in scopes {
            let cursor = idx.effect_item_cursor.get(&format!("{kind}:{scope}"));
            let selected = cursor
                .and_then(|cursor| {
                    ids.range((
                        std::ops::Bound::Excluded(cursor.clone()),
                        std::ops::Bound::Unbounded,
                    ))
                    .next()
                })
                .or_else(|| ids.first());
            if let Some(id) = selected {
                rows.push(record(&format!("effect:{kind}"), "", id, scope, 0, None));
            }
        }
    }
    for id in &idx.completion_ready {
        rows.push(record(
            "effect:completion",
            "",
            id,
            state
                .intents
                .get(id)
                .map(|i| i.authority.scope.as_str())
                .unwrap_or(""),
            0,
            None,
        ));
    }
    for (key, timer) in &idx.timers {
        rows.push(record(
            "timer",
            "",
            key,
            state
                .runs
                .get(&timer.run)
                .map(|r| r.authority.scope.as_str())
                .unwrap_or(""),
            0,
            Some(timer.at_ms),
        ));
    }
    for (key, events) in &idx.events {
        for (seq, id) in events {
            rows.push(record(
                "event",
                key,
                id,
                state
                    .events
                    .get(id)
                    .map(|e| e.envelope.scope.as_str())
                    .unwrap_or(""),
                *seq,
                state.events.get(id).map(|e| e.accepted_at_ms),
            ));
        }
    }
    for (key, ids) in &idx.waiters {
        for id in ids {
            rows.push(record(
                "waiter",
                key,
                id,
                state
                    .runs
                    .get(id)
                    .map(|r| r.authority.scope.as_str())
                    .unwrap_or(""),
                0,
                None,
            ));
        }
    }
    for id in &idx.runs {
        rows.push(record(
            "run",
            "",
            id,
            state
                .runs
                .get(id)
                .map(|r| r.authority.scope.as_str())
                .unwrap_or(""),
            0,
            None,
        ));
    }
    for (run, receipts) in &idx.receipts {
        for (seq, offset) in receipts {
            rows.push(record(
                "receipt",
                run,
                &offset.to_string(),
                "",
                *seq,
                state.receipts.get(*offset).map(|r| r.at_ms),
            ));
        }
    }
    for (seq, offset) in &idx.all_receipts {
        rows.push(record(
            "all_receipt",
            "",
            &offset.to_string(),
            "",
            *seq,
            state.receipts.get(*offset).map(|r| r.at_ms),
        ));
    }
    rows
}
fn public_rows(state: &State) -> Vec<Value> {
    inspection::rows(state).into_iter().map(|row|json!({"kind":row.kind.key(),"item_key":row.key,"run_id":row.run,"sequence":row.sequence,"public_value":row.value})).collect()
}
fn delta(previous: Vec<Value>, next: Vec<Value>, keys: &[&str]) -> (Vec<Value>, Vec<Value>) {
    let keyed = |rows: Vec<Value>| -> std::collections::BTreeMap<Vec<String>, Value> {
        rows.into_iter()
            .map(|row| {
                (
                    keys.iter()
                        .map(|key| row[*key].as_str().expect("internal row key").to_string())
                        .collect(),
                    row,
                )
            })
            .collect()
    };
    let previous = keyed(previous);
    let next = keyed(next);
    let changed = next
        .iter()
        .filter(|(key, row)| previous.get(*key) != Some(*row))
        .map(|(_, row)| row.clone())
        .collect();
    let deleted = previous
        .keys()
        .filter(|key| !next.contains_key(*key))
        .map(|key| {
            let fields: serde_json::Map<_, _> = keys
                .iter()
                .zip(key)
                .map(|(name, value)| ((*name).into(), Value::String(value.clone())))
                .collect();
            Value::Object(fields)
        })
        .collect();
    (changed, deleted)
}
pub(crate) fn sync(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    previous: &State,
    next: &State,
) -> Result<(), StorageError> {
    let (changed, deleted) = delta(
        native_rows(previous),
        native_rows(next),
        &["kind", "lookup_key", "item_key"],
    );
    if !deleted.is_empty() {
        tx.execute("DELETE FROM aq_playbooks.runtime_index old USING jsonb_to_recordset($2) AS d(kind text,lookup_key text,item_key text) WHERE old.namespace=$1 AND (old.kind,old.lookup_key,old.item_key)=(d.kind,d.lookup_key,d.item_key)",&[&namespace,&Value::Array(deleted)]).map_err(backend)?;
    }
    if !changed.is_empty() {
        tx.execute("INSERT INTO aq_playbooks.runtime_index(namespace,kind,lookup_key,item_key,scope,sequence,at_ms) SELECT $1,kind,lookup_key,item_key,scope,sequence,at_ms FROM jsonb_to_recordset($2) AS d(kind text,lookup_key text,item_key text,scope text,sequence bigint,at_ms bigint) ON CONFLICT(namespace,kind,lookup_key,item_key) DO UPDATE SET scope=EXCLUDED.scope,sequence=EXCLUDED.sequence,at_ms=EXCLUDED.at_ms",&[&namespace,&Value::Array(changed)]).map_err(backend)?;
    }
    let (changed, deleted) = delta(
        public_rows(previous),
        public_rows(next),
        &["kind", "item_key"],
    );
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
    let requested = match query {
        IndexQuery::Ready { limit, .. }
        | IndexQuery::DueTimers { limit, .. }
        | IndexQuery::Effects { limit, .. }
        | IndexQuery::CorrelatedEvents { limit, .. }
        | IndexQuery::Waiters { limit, .. }
        | IndexQuery::Runs { limit, .. }
        | IndexQuery::Receipts { limit, .. } => *limit,
    };
    let limit = requested.min(1000) as i64;
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
        IndexQuery::Receipts{run,after_sequence,..}=>{let after=i64::try_from(*after_sequence).map_err(|_|backend("index sequence exhausted"))?;let kind=if run.is_some(){"receipt"}else{"all_receipt"};let lookup=run.as_deref().unwrap_or("");client.query("SELECT item_key,scope,sequence,at_ms FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind=$2 AND lookup_key=$3 AND sequence>$4 ORDER BY sequence,item_key LIMIT $5",&[&namespace,&kind,&lookup,&after,&limit])},
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
    let next = items.last().map(|item| match query {
        IndexQuery::Ready { .. } | IndexQuery::Effects { .. } => item.scope.clone(),
        IndexQuery::Receipts { .. } | IndexQuery::CorrelatedEvents { .. } => {
            item.sequence.to_string()
        }
        _ => item.key.clone(),
    });
    Ok(IndexPage { items, next })
}
