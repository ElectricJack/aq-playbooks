//! Writer format 3 (D10, PB-04): bounded `transition_v1` over a record
//! representation. The namespace row holds the State header (artifacts
//! stripped; they stay in the artifact tables) and its exact record counts
//! ([`Accounting`]);
//! every retained-history container record is one `state_records` row keyed by
//! an order-preserving encoding of its typed key. A transition binds the
//! header's lazy containers to the open row-locked transaction, runs the
//! engine plan, and writes only the touched records. Transitions that remove
//! retained history, change artifacts/catalog inputs or fall back to a full
//! container commit through the complete (`commit_state`) path instead.
use crate::profiling::ObservedTransaction;
use crate::{PostgresStorage, backend, check_name, lock_key};
use aq_playbooks_model::Artifact;
use aq_playbooks_runner::lazy::{
    ContainerChange, ContainerPath, KeyKind, RecordSource, Shape, TreeKey, header_bytes,
};
use aq_playbooks_runner::retention::{self, Window};
use aq_playbooks_runner::transition::{
    Accounting, StatePatch, TransitionOutcome, TransitionRequest, assemble_state, bind_state,
    header_state, schema, take_patch,
};
use aq_playbooks_runner::{Error, State, StorageError};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

pub(crate) const FORMAT: i32 = 3;
pub(crate) const MARKER_SQL: &str = "SELECT set_config('aq_playbooks.writer_namespace',$1,true),set_config('aq_playbooks.writer_format',$2,true)";

/// Value-free work counters for the record representation (all formats count
/// `load`/`compare_and_swap`). Cost tests and the PB-02 preflight use these.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransitionStats {
    /// `transition_v1` calls that ran the plan on a bound record State.
    pub transitions: u64,
    /// Committed through bounded record writes.
    pub incremental: u64,
    /// Committed through the complete path after the plan (see module docs).
    pub heavy: u64,
    /// Plan made no change.
    pub unchanged: u64,
    pub full_loads: u64,
    pub full_commits: u64,
    pub record_reads: u64,
    pub records_written: u64,
    /// Full-container fallbacks reported by the shared lazy containers.
    pub fallbacks: Vec<String>,
}

fn schema_cache() -> &'static [(&'static str, Shape)] {
    static SCHEMA: OnceLock<Vec<(&'static str, Shape)>> = OnceLock::new();
    SCHEMA.get_or_init(schema)
}
fn field(name: &str) -> Result<(&'static str, KeyKind), String> {
    let (field, shape) = schema_cache()
        .iter()
        .find(|(f, _)| *f == name)
        .ok_or_else(|| format!("unknown record field {name}"))?;
    let kind = match shape {
        Shape::Map(k) | Shape::Set(k) => *k,
        Shape::Seq => KeyKind::U64,
        Shape::Nested(..) => return Err(format!("{name}: nested containers are not stored")),
    };
    Ok((field, kind))
}
/// Order-preserving: bytea comparison is the typed key order of each kind.
pub(crate) fn encode_key(key: &TreeKey) -> Vec<u8> {
    match key {
        TreeKey::U64(v) => v.to_be_bytes().to_vec(),
        TreeKey::I64(v) => ((*v as u64) ^ (1 << 63)).to_be_bytes().to_vec(),
        TreeKey::Str(s) => s.as_bytes().to_vec(),
    }
}
pub(crate) fn decode_key(name: &str, bytes: &[u8]) -> Result<TreeKey, String> {
    let (_, kind) = field(name)?;
    let fixed = || -> Result<[u8; 8], String> {
        bytes
            .try_into()
            .map_err(|_| format!("{name}: invalid record key"))
    };
    Ok(match kind {
        KeyKind::U64 => TreeKey::U64(u64::from_be_bytes(fixed()?)),
        KeyKind::I64 => TreeKey::I64((u64::from_be_bytes(fixed()?) ^ (1 << 63)) as i64),
        KeyKind::Str => TreeKey::Str(
            String::from_utf8(bytes.to_vec()).map_err(|_| format!("{name}: invalid record key"))?,
        ),
    })
}
pub(crate) fn accounting_json(a: &Accounting) -> Value {
    let fields: serde_json::Map<String, Value> = a
        .fields
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(f, count)| (f.to_string(), json!(count)))
        .collect();
    json!({"header_len":a.header_len,"fields":fields})
}
/// Record counts from a namespace row. A row written before spec 0003's
/// "no limits" holds `[count, bytes]` per field and a queued-destination
/// count, which only served the removed quotas: the bytes and that count
/// are ignored.
pub(crate) fn accounting_from(value: &Value) -> Result<Accounting, StorageError> {
    let bad = || backend("invalid record accounting");
    let number = |v: &Value| v.as_u64().map(|n| n as usize).ok_or_else(bad);
    let mut fields = BTreeMap::new();
    for (name, stats) in value["fields"].as_object().ok_or_else(bad)? {
        let (name, _) = field(name).map_err(backend)?;
        let count = match stats {
            Value::Array(pair) if pair.len() == 2 => number(&pair[0])?,
            other => number(other)?,
        };
        fields.insert(name, count);
    }
    Ok(Accounting {
        header_len: number(&value["header_len"])?,
        fields,
    })
}

/// Records of a complete State: (header without artifacts, records, accounting).
pub(crate) struct Export {
    pub header: Vec<u8>,
    pub fields: Vec<String>,
    pub keys: Vec<Vec<u8>>,
    pub values: Vec<Vec<u8>>,
    pub accounting: Accounting,
}
pub(crate) fn export(state: &State) -> Result<Export, StorageError> {
    let mut plain = state.clone();
    let patch = take_patch(&mut plain).map_err(storage)?;
    let mut accounting = Accounting {
        header_len: patch.header.len(),
        ..Accounting::default()
    };
    let mut out = Export {
        header: stripped_header(&mut plain)?,
        fields: vec![],
        keys: vec![],
        values: vec![],
        accounting: Accounting::default(),
    };
    for (path, change) in patch.changes {
        let ContainerChange::Replace(rows) = change else {
            return Err(backend("plain State export produced a partial container"));
        };
        let (name, _) = field(path.field).map_err(backend)?;
        if path.outer.is_some() {
            return Err(backend("nested record container"));
        }
        for (key, value) in rows {
            accounting.record(name, false, true).map_err(storage)?;
            out.fields.push(name.to_string());
            out.keys.push(encode_key(&key));
            out.values.push(value);
        }
    }
    out.accounting = accounting;
    Ok(out)
}
fn stripped_header(state: &mut State) -> Result<Vec<u8>, StorageError> {
    let artifacts = std::mem::take(&mut state.artifacts);
    let header = header_bytes(&*state).map_err(backend);
    state.artifacts = artifacts;
    header
}
fn same<T: serde::Serialize>(a: &T, b: &T) -> Result<bool, StorageError> {
    Ok(serde_json::to_value(a).map_err(backend)? == serde_json::to_value(b).map_err(backend)?)
}
fn storage(error: Error) -> StorageError {
    match error {
        Error::Storage(s) => s,
        other => StorageError::Backend(other.to_string()),
    }
}
pub(crate) fn empty_header() -> Result<(Vec<u8>, Value), StorageError> {
    let export = export(&State::default())?;
    Ok((export.header, accounting_json(&export.accounting)))
}

/// Complete State of a format 3 namespace inside `tx` (artifacts included).
pub(crate) fn assemble(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    header: &[u8],
    artifacts: BTreeMap<String, Artifact>,
) -> Result<State, StorageError> {
    let mut records = all_records(tx, namespace)?;
    let mut state = assemble_state(header, |f| {
        Ok(records
            .remove(f)
            .map(|c| c.into_iter().collect())
            .unwrap_or_default())
    })
    .map_err(storage)?;
    if !records.is_empty() {
        return Err(backend("record outside the State schema"));
    }
    state.artifacts = artifacts;
    Ok(state)
}
type Containers = BTreeMap<&'static str, BTreeMap<TreeKey, Vec<u8>>>;
fn all_records(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
) -> Result<Containers, StorageError> {
    let mut records: Containers = BTreeMap::new();
    for row in tx
        .query(
            "SELECT field,sort_key,value FROM aq_playbooks.state_records WHERE namespace=$1",
            &[&namespace],
        )
        .map_err(backend)?
    {
        let name: String = row.get(0);
        let (name, _) = field(&name).map_err(backend)?;
        let key = decode_key(name, row.get::<_, &[u8]>(1)).map_err(backend)?;
        records.entry(name).or_default().insert(key, row.get(2));
    }
    Ok(records)
}
/// Replace every record of `namespace` with `export` (complete path).
pub(crate) fn replace_all(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    export: &Export,
) -> Result<(), StorageError> {
    tx.execute(
        "DELETE FROM aq_playbooks.state_records WHERE namespace=$1",
        &[&namespace],
    )
    .map_err(backend)?;
    Columns {
        fields: &export.fields,
        keys: &export.keys,
        values: &export.values,
    }
    .upsert(tx, namespace)
}
struct Columns<'a> {
    fields: &'a [String],
    keys: &'a [Vec<u8>],
    values: &'a [Vec<u8>],
}
impl Columns<'_> {
    fn upsert(self, tx: &mut ObservedTransaction<'_>, namespace: &str) -> Result<(), StorageError> {
        let Self {
            fields,
            keys,
            values,
        } = self;
        if fields.is_empty() {
            return Ok(());
        }
        tx.execute("INSERT INTO aq_playbooks.state_records(namespace,field,sort_key,value) SELECT $1,f,k,v FROM unnest($2::text[],$3::bytea[],$4::bytea[]) AS r(f,k,v) ON CONFLICT(namespace,field,sort_key) DO UPDATE SET value=EXCLUDED.value",&[&namespace,&fields,&keys,&values]).map_err(backend)?;
        Ok(())
    }
}

mod source;
pub(crate) use source::Statements;
use source::{Lent, PgSource};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
/// Deleting one of these records always takes the complete path, where
/// `retention::verify_removals` checks it against the whole State.
const HISTORY_REMOVALS: &[&str] = &["runs", "intents", "retention.disposals"];
/// Window entries a bounded commit may delete, each with its expiry record:
/// allowed only when the same patch deletes that record with an expiry the
/// clock has reached.
const WINDOWS: &[(&str, &str)] = &[
    ("keyed_admissions", "retention.admission_expires_at_ms"),
    ("events", "retention.event_expires_at_ms"),
];
const IDENTITIES: &[(&str, &str)] = &[("events", "event"), ("runs", "run"), ("intents", "intent")];

mod write;
use write::{Head, Planned};
struct Opened<'a> {
    tx: ObservedTransaction<'a>,
    statements: Statements,
    head: Head,
    artifacts: BTreeMap<String, Artifact>,
}
impl PostgresStorage {
    /// Records representation is used for namespaces this connection creates.
    pub(crate) fn transition_records(
        &self,
        namespace: &str,
        owner: &str,
        request: &mut TransitionRequest<'_>,
    ) -> aq_playbooks_runner::Result<TransitionOutcome> {
        check_name(namespace)?;
        let mut conn = self.connection()?;
        let Some(Opened {
            tx,
            statements,
            head,
            artifacts,
        }) = self.open_records(&mut conn, namespace, owner)?
        else {
            return Ok(TransitionOutcome::Unsupported);
        };
        let lent = Lent::new(tx, namespace, statements, &head.accounting);
        let source = lent.source.clone();
        let mut state = bind_state(&head.header, source.clone())?;
        if !state.indexes.initialized {
            return Ok(TransitionOutcome::Unsupported);
        }
        state.artifacts = artifacts.clone();
        let outcome = Planned::run(request, &mut state, &source, &head.header);
        let fallback = self.count_plan(&source);
        let Some(plan) = outcome? else {
            self.count(|s| s.unchanged += 1);
            return Ok(TransitionOutcome::Unchanged);
        };
        let mut tx: ObservedTransaction<'_> = lent.reclaim()?;
        let prepared = head.prepare(&mut tx, namespace, (&state, &plan))?;
        source.check()?;
        let heavy = prepared.needs_full(fallback, &state, &artifacts)?;
        if heavy {
            let mut next = prepared.staged.rebuild(&mut tx, namespace, plan.patch)?;
            next.settled = std::mem::take(&mut state.settled);
            drop(tx);
            drop(conn);
            drop(state);
            let revision =
                self.commit_state(namespace, owner, head.expected as u64, &next, false)?;
            self.count(|s| s.heavy += 1);
            return Ok(TransitionOutcome::Committed { revision });
        }
        let published = prepared
            .incremental(namespace, &state, &plan.stripped)
            .publish(self, &mut tx)?;
        if let Err(error) = tx.commit() {
            if let Some(lease) = conn.leases.get_mut(namespace) {
                lease.catalog_uncertain = true;
            }
            return Err(backend(error).into());
        }
        Ok(published.committed(self, namespace))
    }
    fn open_records<'a>(
        &self,
        conn: &'a mut crate::Connection,
        namespace: &str,
        owner: &str,
    ) -> Result<Option<Opened<'a>>, Error> {
        let lease = conn
            .leases
            .get(namespace)
            .filter(|l| l.owner == owner)
            .ok_or(StorageError::Ownership)?;
        if lease.format != FORMAT {
            return Ok(None);
        }
        let incarnation = lease.incarnation;
        let statements = conn.record_statements()?;
        let mut tx = ObservedTransaction::new(
            conn.client.transaction().map_err(backend)?,
            self.profiling.clone(),
            "transition",
        );
        tx.query_one(MARKER_SQL, &[&namespace, &"3"])
            .map_err(backend)?;
        if !owns_lock(&mut tx, namespace)? {
            return Err(StorageError::Ownership.into());
        }
        let Some(head) = Head::read(&mut tx, namespace, (owner, incarnation))? else {
            return Ok(None);
        };
        let artifacts = self.read_artifacts(&mut tx, namespace, false)?;
        Ok(Some(Opened {
            tx,
            statements,
            head,
            artifacts,
        }))
    }
    fn count_plan(&self, source: &PgSource) -> bool {
        let reads = source.reads();
        let fallbacks = source.take_fallbacks();
        self.count(|s| {
            s.transitions += 1;
            s.record_reads += reads;
            s.fallbacks.extend(fallbacks.iter().cloned());
        });
        !fallbacks.is_empty()
    }
    pub(crate) fn count(&self, f: impl FnOnce(&mut TransitionStats)) {
        f(&mut lock(&self.stats));
    }
    /// Snapshot of the value-free work counters.
    pub fn transition_stats(&self) -> TransitionStats {
        lock(&self.stats).clone()
    }
    pub fn reset_transition_stats(&self) {
        *lock(&self.stats) = TransitionStats::default();
    }
}
/// Columns 1..=3 of a namespace row read as
/// `revision,owner,incarnation,owner_backend_pid=pg_backend_pid(),…`: this
/// session's lease of `owner` at `incarnation`.
pub(crate) fn owns_row(
    row: &postgres::Row,
    owner: &str,
    incarnation: i64,
) -> Result<(), StorageError> {
    let matches_owner = row.get::<_, Option<String>>(1).as_deref() == Some(owner)
        && row.get::<_, i64>(2) == incarnation;
    if !matches_owner || row.get::<_, Option<bool>>(3) != Some(true) {
        return Err(StorageError::Ownership);
    }
    Ok(())
}
pub(crate) fn owns_lock(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
) -> Result<bool, StorageError> {
    let advisory_key = lock_key(namespace) as u64;
    let high = (advisory_key >> 32) as i64;
    let low = (advisory_key & 0xffff_ffff) as i64;
    Ok(tx.query_one("SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype='advisory' AND pid=pg_backend_pid() AND classid::bigint=$1 AND objid::bigint=$2 AND objsubid=1 AND granted)", &[&high,&low]).map_err(backend)?.get(0))
}

/// Header-only invariants of `retention::verify_and_record` and
/// `retention::verify_removals` for a transition that removes no retained
/// history (other than expired window entries, see `stage`) and leaves
/// artifacts unchanged.
fn verify_header(previous: &State, next: &State) -> Result<(), StorageError> {
    if previous
        .retention
        .clock_ms
        .is_some_and(|old| next.retention.clock_ms.is_none_or(|new| new < old))
    {
        return Err(backend(aq_playbooks_runner::retention::REFUSE_CLOCK));
    }
    aq_playbooks_runner::retention::check_floor_unchanged(previous, next)?;
    if next.sequence < previous.sequence {
        return Err(backend("namespace invocation sequence cannot decrease"));
    }
    if previous
        .retention
        .last_gc_at_ms
        .is_some_and(|old| next.retention.last_gc_at_ms.is_none_or(|new| new < old))
    {
        return Err(backend("retention GC clock cannot decrease"));
    }
    for (producer, epoch) in &previous.retention.producer_epochs {
        if next
            .retention
            .producer_epochs
            .get(producer)
            .copied()
            .unwrap_or(0)
            < *epoch
        {
            return Err(backend("producer replay epoch cannot decrease"));
        }
    }
    Ok(())
}

struct Staged {
    full_header: Vec<u8>,
    accounting: Accounting,
    replaced: bool,
    history_removed: bool,
    /// Event tombstone records this patch deletes.
    retired_event_tombstones: Vec<String>,
    created: Vec<(String, String)>,
    fields: Vec<String>,
    keys: Vec<Vec<u8>>,
    values: Vec<Vec<u8>>,
    deleted_fields: Vec<String>,
    deleted_keys: Vec<Vec<u8>>,
}
mod evictions;
use evictions::licensed_evictions;
mod staging;
fn apply(records: &mut Containers, patch: StatePatch) -> Result<(), Error> {
    for (path, change) in patch.changes {
        let (name, _) = field(path.field).map_err(backend)?;
        let container = records.entry(name).or_default();
        match change {
            ContainerChange::Entries(rows) => {
                apply_entries(container, rows);
            }
            ContainerChange::Replace(rows) => *container = rows.into_iter().collect(),
        }
    }
    Ok(())
}

fn apply_entries(
    container: &mut BTreeMap<TreeKey, Vec<u8>>,
    rows: Vec<aq_playbooks_runner::lazy::RecordChange>,
) {
    for (key, new) in rows {
        match new {
            Some(bytes) => container.insert(key, bytes),
            None => container.remove(&key),
        };
    }
}

impl PostgresStorage {
    /// `Storage::query` for a format 3 namespace: the shared runtime indexes
    /// over the bound records (`None` for other formats / absent namespaces).
    pub(crate) fn query_records(
        &self,
        conn: &mut crate::Connection,
        namespace: &str,
        query: &aq_playbooks_runner::IndexQuery,
    ) -> Result<Option<aq_playbooks_runner::IndexPage>, StorageError> {
        let statements = conn.record_statements()?;
        let mut tx = ObservedTransaction::new(
            conn.client.transaction().map_err(backend)?,
            self.profiling.clone(),
            "query",
        );
        let Some(row) = tx
            .query_opt(
                "SELECT writer_format,state_header,state_accounting FROM aq_playbooks.namespaces WHERE namespace=$1 FOR SHARE",
                &[&namespace],
            )
            .map_err(backend)?
        else {
            return Ok(None);
        };
        if row.get::<_, i32>(0) != FORMAT {
            return Ok(None);
        }
        let header: Vec<u8> = row.get(1);
        let accounting = accounting_from(&row.get::<_, Value>(2))?;
        let lent = Lent::new(tx, namespace, statements, &accounting);
        let source = lent.source.clone();
        let state = bind_state(&header, source.clone()).map_err(storage)?;
        let page = if state.indexes.initialized {
            let page = state.indexes.query(&state, query);
            source.check().map_err(storage)?;
            drop(state);
            let tx = lent.reclaim().map_err(storage)?;
            tx.commit().map_err(backend)?;
            page
        } else {
            drop(state);
            let mut tx = lent.reclaim().map_err(storage)?;
            let state = assemble(&mut tx, namespace, &header, BTreeMap::new())?;
            tx.commit().map_err(backend)?;
            state.indexes.query(&state, query)
        };
        let reads = source.reads();
        self.count(|s| s.record_reads += reads);
        Ok(Some(page))
    }
}
/// Inspection page of a complete format 3 State (O(state); see D10 limits)
/// merged with `history`, as `inspection::page_with_history` does: key order,
/// and a key present in both is returned once (the live item).
pub(crate) fn inspect(
    state: &State,
    query: &aq_playbooks_runner::InspectionQuery,
    revision: u64,
    history: Vec<aq_playbooks_runner::HistoryItem>,
) -> aq_playbooks_runner::InspectionPage {
    let kind = query.kind.key();
    let mut rows: Vec<(String, Value)> = crate::projection::public_delta(&State::default(), state)
        .0
        .into_iter()
        .filter(|r| r["kind"].as_str() == Some(kind))
        .filter(|r| {
            query
                .run
                .as_deref()
                .is_none_or(|run| r["run_id"].as_str() == Some(run))
        })
        .filter_map(|r| {
            let key = r["item_key"].as_str()?.to_string();
            query
                .after
                .as_deref()
                .is_none_or(|after| key.as_str() > after)
                .then(|| (key, r["public_value"].clone()))
        })
        .chain(
            history
                .into_iter()
                .filter(|item| {
                    item.kind == query.kind
                        && query.run.as_ref().is_none_or(|run| &item.run == run)
                        && query.after.as_ref().is_none_or(|after| &item.key > after)
                })
                .map(|item| (item.key, item.value)),
        )
        .collect();
    // Stable: for a key present in both, the live item stays first.
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.dedup_by(|later, earlier| later.0 == earlier.0);
    let more = rows.len() > query.limit;
    rows.truncate(query.limit);
    aq_playbooks_runner::InspectionPage {
        next: if more {
            rows.last().map(|r| r.0.clone())
        } else {
            None
        },
        items: rows.into_iter().map(|r| r.1).collect(),
        revision,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    /// A format 3 row written before the namespace limits were removed
    /// holds `[count, bytes]` per container and a queued-destination count;
    /// it decodes to the same record counts as a row written now.
    #[test]
    fn record_counts_decode_from_rows_written_before_and_after_no_limits() {
        let before = json!({"header_len": 120, "queued_destinations": 3,
            "fields": {"runs": [2, 900], "events": [5, 4000]}});
        let after = json!({"header_len": 120, "fields": {"runs": 2, "events": 5}});
        let (a, b) = (
            accounting_from(&before).unwrap(),
            accounting_from(&after).unwrap(),
        );
        assert_eq!(a, b);
        assert_eq!(
            (a.count("runs"), a.count("events"), a.header_len),
            (2, 5, 120)
        );
        assert_eq!(accounting_json(&a), after);
        assert!(accounting_from(&json!({"header_len": 1, "fields": {"runs": [1]}})).is_err());
    }
}
