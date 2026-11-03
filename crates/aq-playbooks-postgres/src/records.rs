//! Writer format 3 (D10, PB-04): bounded `transition_v1` over a record
//! representation. The namespace row holds the State header (artifacts
//! stripped; they stay in the artifact tables) and its exact [`Accounting`];
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
use aq_playbooks_runner::transition::{
    Accounting, ContainerStats, StatePatch, TransitionOutcome, TransitionRequest, assemble_state,
    bind_state, header_state, schema, take_patch,
};
use aq_playbooks_runner::{Error, State, StorageError};
use postgres::Statement;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::panic::Location;
use std::sync::{Arc, Mutex, OnceLock};

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
        .filter(|(_, s)| s.count > 0)
        .map(|(f, s)| (f.to_string(), json!([s.count, s.pairs])))
        .collect();
    json!({"header_len":a.header_len,"queued_destinations":a.queued_destinations,"fields":fields})
}
pub(crate) fn accounting_from(value: &Value) -> Result<Accounting, StorageError> {
    let bad = || backend("invalid record accounting");
    let number = |v: &Value| v.as_u64().map(|n| n as usize).ok_or_else(bad);
    let mut fields = BTreeMap::new();
    for (name, stats) in value["fields"].as_object().ok_or_else(bad)? {
        let (name, _) = field(name).map_err(backend)?;
        let pair = stats.as_array().filter(|p| p.len() == 2).ok_or_else(bad)?;
        fields.insert(
            name,
            ContainerStats {
                count: number(&pair[0])?,
                pairs: number(&pair[1])?,
            },
        );
    }
    Ok(Accounting {
        header_len: number(&value["header_len"])?,
        queued_destinations: number(&value["queued_destinations"])?,
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
            accounting
                .record(name, &key, None, Some(&value))
                .map_err(storage)?;
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
    upsert(tx, namespace, &export.fields, &export.keys, &export.values)
}
fn upsert(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    fields: &[String],
    keys: &[Vec<u8>],
    values: &[Vec<u8>],
) -> Result<(), StorageError> {
    if fields.is_empty() {
        return Ok(());
    }
    tx.execute("INSERT INTO aq_playbooks.state_records(namespace,field,sort_key,value) SELECT $1,f,k,v FROM unnest($2::text[],$3::bytea[],$4::bytea[]) AS r(f,k,v) ON CONFLICT(namespace,field,sort_key) DO UPDATE SET value=EXCLUDED.value",&[&namespace,&fields,&keys,&values]).map_err(backend)?;
    Ok(())
}

/// Prepared once per connection, outside any transaction.
#[derive(Clone)]
pub(crate) struct Statements {
    get: Statement,
    first: Statement,
    after: Statement,
    scan: Statement,
}
impl Statements {
    pub(crate) fn prepare(client: &mut postgres::Client) -> Result<Self, StorageError> {
        let mut p = |sql: &str| client.prepare(sql).map_err(backend);
        Ok(Self {
            get: p(
                "SELECT value FROM aq_playbooks.state_records WHERE namespace=$1 AND field=$2 AND sort_key=$3",
            )?,
            first: p(
                "SELECT sort_key,value FROM aq_playbooks.state_records WHERE namespace=$1 AND field=$2 ORDER BY sort_key LIMIT $3",
            )?,
            after: p(
                "SELECT sort_key,value FROM aq_playbooks.state_records WHERE namespace=$1 AND field=$2 AND sort_key>$3 ORDER BY sort_key LIMIT $4",
            )?,
            scan: p(
                "SELECT sort_key,value FROM aq_playbooks.state_records WHERE namespace=$1 AND field=$2 ORDER BY sort_key",
            )?,
        })
    }
}

/// The row-locked transaction, lent to the bound State for the plan.
struct PgSource {
    // SAFETY: the 'static lifetime is erased from a transaction borrowing the
    // storage connection guard. `Lent` reclaims (or drops) it before that guard
    // is released; after reclamation every access fails and poisons.
    tx: Mutex<Option<ObservedTransaction<'static>>>,
    namespace: String,
    statements: Statements,
    counts: BTreeMap<&'static str, usize>,
    reads: Mutex<u64>,
    fallbacks: Mutex<Vec<String>>,
    poison: Mutex<Option<String>>,
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
impl PgSource {
    fn rows(
        &self,
        statement: &Statement,
        params: &[&(dyn postgres::types::ToSql + Sync)],
    ) -> Result<Vec<postgres::Row>, String> {
        let mut tx = lock(&self.tx);
        let tx = tx
            .as_mut()
            .ok_or_else(|| "transition record source used after the plan".to_string())?;
        tx.query(statement, params).map_err(|e| e.to_string())
    }
    fn path(path: &ContainerPath) -> Result<&'static str, String> {
        if path.outer.is_some() {
            return Err(format!("{path:?}: nested containers are not stored"));
        }
        Ok(path.field)
    }
    fn pairs(
        &self,
        name: &str,
        rows: Vec<postgres::Row>,
    ) -> Result<Vec<(TreeKey, Vec<u8>)>, String> {
        *lock(&self.reads) += rows.len().max(1) as u64;
        rows.into_iter()
            .map(|r| Ok((decode_key(name, r.get::<_, &[u8]>(0))?, r.get(1))))
            .collect()
    }
    fn check(&self) -> Result<(), Error> {
        match lock(&self.poison).clone() {
            Some(e) => Err(Error::Storage(backend(format!(
                "transition source failed: {e}"
            )))),
            None => Ok(()),
        }
    }
}
impl RecordSource for PgSource {
    fn get(&self, path: &ContainerPath, key: &TreeKey) -> Result<Option<Vec<u8>>, String> {
        let name = Self::path(path)?;
        let rows = self.rows(
            &self.statements.get,
            &[&self.namespace, &name, &encode_key(key)],
        )?;
        *lock(&self.reads) += 1;
        Ok(rows.into_iter().next().map(|r| r.get(0)))
    }
    fn after(
        &self,
        path: &ContainerPath,
        after: Option<&TreeKey>,
        limit: usize,
    ) -> Result<Vec<(TreeKey, Vec<u8>)>, String> {
        let name = Self::path(path)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = match after {
            None => self.rows(&self.statements.first, &[&self.namespace, &name, &limit])?,
            Some(key) => self.rows(
                &self.statements.after,
                &[&self.namespace, &name, &encode_key(key), &limit],
            )?,
        };
        self.pairs(name, rows)
    }
    fn len(&self, path: &ContainerPath) -> Result<usize, String> {
        let name = Self::path(path)?;
        Ok(self.counts.get(name).copied().unwrap_or(0))
    }
    fn scan(&self, path: &ContainerPath) -> Result<Vec<(TreeKey, Vec<u8>)>, String> {
        let name = Self::path(path)?;
        let rows = self.rows(&self.statements.scan, &[&self.namespace, &name])?;
        self.pairs(name, rows)
    }
    fn fallback(&self, path: &ContainerPath, at: &'static Location<'static>) {
        lock(&self.fallbacks).push(format!("{} at {}:{}", path.field, at.file(), at.line()));
    }
    fn fail(&self, error: String) {
        lock(&self.poison).get_or_insert(error);
    }
}
/// Reclaims the lent transaction on every exit path, before the connection
/// guard (declared earlier) is released.
struct Lent(Arc<PgSource>);
impl Lent {
    fn reclaim(&self) -> Result<ObservedTransaction<'static>, Error> {
        lock(&self.0.tx)
            .take()
            .ok_or_else(|| Error::Storage(backend("transition transaction already reclaimed")))
    }
}
impl Drop for Lent {
    fn drop(&mut self) {
        drop(lock(&self.0.tx).take());
    }
}

const HISTORY_REMOVALS: &[&str] = &[
    "events",
    "runs",
    "intents",
    "retention.event_tombstones",
    "retention.disposals",
];
const IDENTITIES: &[(&str, &str)] = &[("events", "event"), ("runs", "run"), ("intents", "intent")];

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
        let lease = conn
            .leases
            .get(namespace)
            .filter(|l| l.owner == owner)
            .ok_or(StorageError::Ownership)?;
        if lease.format != FORMAT {
            return Ok(TransitionOutcome::Unsupported);
        }
        let incarnation = lease.incarnation;
        if conn.statements.is_none() {
            conn.statements = Some(Statements::prepare(&mut conn.client)?);
        }
        let statements = conn.statements.clone().expect("prepared above");
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
        let row = tx.query_opt("SELECT revision,owner,incarnation,owner_backend_pid=pg_backend_pid(),writer_format,state_header,state_accounting,catalog_projected_revision,catalog_metadata_revision,catalog_definition_count,catalog_activation_count,catalog_actual_definition_count,catalog_actual_activation_count FROM aq_playbooks.namespaces WHERE namespace=$1 FOR UPDATE", &[&namespace]).map_err(backend)?.ok_or(StorageError::Ownership)?;
        if row.get::<_, Option<String>>(1).as_deref() != Some(owner)
            || row.get::<_, i64>(2) != incarnation
            || row.get::<_, Option<bool>>(3) != Some(true)
        {
            return Err(StorageError::Ownership.into());
        }
        if row.get::<_, i32>("writer_format") != FORMAT {
            return Ok(TransitionOutcome::Unsupported);
        }
        let expected: i64 = row.get(0);
        let catalog_projected: Option<i64> = row.get("catalog_projected_revision");
        let catalog_metadata: Option<i64> = row.get("catalog_metadata_revision");
        if catalog_projected.is_some_and(|p| p != expected)
            || catalog_projected.is_some() != catalog_metadata.is_some()
        {
            return Err(backend("catalog projection revision mismatch").into());
        }
        crate::catalog::validate_counts(&row)?;
        let header: Vec<u8> = row.get("state_header");
        let accounting = accounting_from(&row.get::<_, Value>("state_accounting"))?;
        let artifacts = self.read_artifacts(&mut tx, namespace, false)?;
        let source = Arc::new(PgSource {
            // SAFETY: see `PgSource::tx`; `lent` is dropped before `conn`.
            tx: Mutex::new(Some(unsafe {
                std::mem::transmute::<ObservedTransaction<'_>, ObservedTransaction<'static>>(tx)
            })),
            namespace: namespace.to_string(),
            statements,
            counts: accounting
                .fields
                .iter()
                .map(|(f, s)| (*f, s.count))
                .collect(),
            reads: Mutex::new(0),
            fallbacks: Mutex::new(vec![]),
            poison: Mutex::new(None),
        });
        let lent = Lent(source.clone());
        let mut state = bind_state(&header, source.clone())?;
        if !state.indexes.initialized {
            return Ok(TransitionOutcome::Unsupported);
        }
        state.artifacts = artifacts.clone();
        let outcome: Result<Option<(StatePatch, Vec<u8>)>, Error> = (|| {
            let changed = (request.plan)(&mut state)?;
            source.check()?;
            if !changed {
                return Ok(None);
            }
            let patch = take_patch(&mut state)?;
            let stripped = stripped_header(&mut state)?;
            source.check()?;
            Ok(Some((patch, stripped)))
        })();
        let reads = *lock(&source.reads);
        let fallbacks = std::mem::take(&mut *lock(&source.fallbacks));
        self.count(|s| {
            s.transitions += 1;
            s.record_reads += reads;
            s.fallbacks.extend(fallbacks.iter().cloned());
        });
        let Some((patch, stripped)) = outcome? else {
            self.count(|s| s.unchanged += 1);
            return Ok(TransitionOutcome::Unchanged);
        };
        let mut tx: ObservedTransaction<'_> = lent.reclaim()?;
        let previous = header_state(&header)?;
        let staged = stage(&mut tx, namespace, &accounting, &patch)?;
        (request.validate)(&state, &staged.accounting.counts())?;
        source.check()?;
        let heavy = !fallbacks.is_empty()
            || staged.replaced
            || staged.history_removed
            || state.artifacts != artifacts
            || !same(&state.bindings, &previous.bindings)?
            || state.execution_denied != previous.execution_denied
            || !same(&state.retention.policy, &previous.retention.policy)?
            || !same(
                &state.retention.artifact_disposals,
                &previous.retention.artifact_disposals,
            )?
            || catalog_metadata.is_none();
        if heavy {
            let mut records = all_records(&mut tx, namespace)?;
            apply(&mut records, patch)?;
            let next = assemble_state(&staged.full_header, |f| {
                Ok(records
                    .remove(f)
                    .map(|c| c.into_iter().collect())
                    .unwrap_or_default())
            })?;
            drop(tx);
            drop(lent);
            drop(conn);
            drop(state);
            let revision = self.commit_state(namespace, owner, expected as u64, &next, false)?;
            self.count(|s| s.heavy += 1);
            return Ok(TransitionOutcome::Committed { revision });
        }
        verify_header(&previous, &state)?;
        let next = expected
            .checked_add(1)
            .ok_or_else(|| backend("revision exhausted"))?;
        if !staged.created.is_empty() {
            let (kinds, ids): (Vec<_>, Vec<_>) = staged.created.iter().cloned().unzip();
            if let Some(reused) = tx.query_opt("SELECT kind FROM aq_playbooks.disposal_tombstones WHERE namespace=$1 AND (kind,identity) IN (SELECT * FROM unnest($2::text[],$3::text[])) LIMIT 1",&[&namespace,&kinds,&ids]).map_err(backend)? {
                return Err(backend(format!("disposed {} identity cannot be reused", reused.get::<_, String>(0))).into());
            }
        }
        if state.retention.last_gc_at_ms != previous.retention.last_gc_at_ms {
            crate::retention::collect(&mut tx, namespace, &state)?;
        }
        if !staged.deleted_fields.is_empty() {
            tx.execute("DELETE FROM aq_playbooks.state_records r USING unnest($2::text[],$3::bytea[]) AS d(f,k) WHERE r.namespace=$1 AND r.field=d.f AND r.sort_key=d.k",&[&namespace,&staged.deleted_fields,&staged.deleted_keys]).map_err(backend)?;
        }
        upsert(
            &mut tx,
            namespace,
            &staged.fields,
            &staged.keys,
            &staged.values,
        )?;
        let published = tx.execute("UPDATE aq_playbooks.namespaces SET state_header=$2,state_accounting=$3,revision=$4,catalog_projected_revision=$4,updated_at=clock_timestamp() WHERE namespace=$1 AND revision=$5",&[&namespace,&stripped,&accounting_json(&staged.accounting),&next,&expected]).map_err(backend)?;
        if published != 1 {
            return Err(backend("record namespace publication mismatch").into());
        }
        let written = (staged.fields.len() + staged.deleted_fields.len()) as u64;
        if let Err(error) = tx.commit() {
            if let Some(lease) = conn.leases.get_mut(namespace) {
                lease.catalog_uncertain = true;
            }
            return Err(backend(error).into());
        }
        self.count(|s| {
            s.incremental += 1;
            s.records_written += written;
        });
        Ok(TransitionOutcome::Committed {
            revision: next as u64,
        })
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
pub(crate) fn owns_lock(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
) -> Result<bool, StorageError> {
    let advisory_key = lock_key(namespace) as u64;
    let high = (advisory_key >> 32) as i64;
    let low = (advisory_key & 0xffff_ffff) as i64;
    Ok(tx.query_one("SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype='advisory' AND pid=pg_backend_pid() AND classid::bigint=$1 AND objid::bigint=$2 AND objsubid=1 AND granted)", &[&high,&low]).map_err(backend)?.get(0))
}

/// Header-only invariants of `retention::verify_and_record` for a transition
/// that removes no retained history and leaves artifacts unchanged.
fn verify_header(previous: &State, next: &State) -> Result<(), StorageError> {
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
    created: Vec<(String, String)>,
    fields: Vec<String>,
    keys: Vec<Vec<u8>>,
    values: Vec<Vec<u8>>,
    deleted_fields: Vec<String>,
    deleted_keys: Vec<Vec<u8>>,
}
/// Exact accounting of the patch against the current records (one read of
/// every touched key), plus the keyed admission immutability check.
fn stage(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    accounting: &Accounting,
    patch: &StatePatch,
) -> Result<Staged, StorageError> {
    let mut staged = Staged {
        full_header: patch.header.clone(),
        accounting: accounting.clone(),
        replaced: false,
        history_removed: false,
        created: vec![],
        fields: vec![],
        keys: vec![],
        values: vec![],
        deleted_fields: vec![],
        deleted_keys: vec![],
    };
    staged.accounting.header_len = patch.header.len();
    let mut touched_fields = vec![];
    let mut touched_keys = vec![];
    for (path, change) in &patch.changes {
        let (name, _) = field(path.field).map_err(backend)?;
        if path.outer.is_some() {
            return Err(backend("nested record container"));
        }
        match change {
            ContainerChange::Entries(rows) => {
                for (key, _) in rows {
                    touched_fields.push(name.to_string());
                    touched_keys.push(encode_key(key));
                }
            }
            ContainerChange::Replace(_) => staged.replaced = true,
        }
    }
    let mut old: BTreeMap<(String, Vec<u8>), Vec<u8>> = BTreeMap::new();
    if !touched_fields.is_empty() {
        for row in tx.query("SELECT r.field,r.sort_key,r.value FROM aq_playbooks.state_records r JOIN unnest($2::text[],$3::bytea[]) AS t(f,k) ON r.field=t.f AND r.sort_key=t.k WHERE r.namespace=$1",&[&namespace,&touched_fields,&touched_keys]).map_err(backend)? {
            old.insert((row.get(0), row.get(1)), row.get(2));
        }
    }
    let mut replaced_old: BTreeMap<&'static str, BTreeMap<TreeKey, Vec<u8>>> = BTreeMap::new();
    for (path, change) in &patch.changes {
        let (name, _) = field(path.field).map_err(backend)?;
        match change {
            ContainerChange::Entries(rows) => {
                let mut seen = BTreeSet::new();
                for (key, new) in rows {
                    let encoded = encode_key(key);
                    if !seen.insert(encoded.clone()) {
                        return Err(backend("duplicate record key in one patch"));
                    }
                    let before = old.get(&(name.to_string(), encoded.clone()));
                    staged
                        .accounting
                        .record(name, key, before.map(Vec::as_slice), new.as_deref())
                        .map_err(storage)?;
                    if name == "keyed_admissions" && before.is_some() && before != new.as_ref() {
                        return Err(backend(
                            "immutable keyed admission receipt removed or changed",
                        ));
                    }
                    match new {
                        Some(value) => {
                            if before.is_none()
                                && let Some((_, kind)) = IDENTITIES.iter().find(|(f, _)| *f == name)
                                && let TreeKey::Str(id) = key
                            {
                                staged.created.push((kind.to_string(), id.clone()));
                            }
                            staged.fields.push(name.to_string());
                            staged.keys.push(encoded);
                            staged.values.push(value.clone());
                        }
                        None => {
                            if before.is_some() && HISTORY_REMOVALS.contains(&name) {
                                staged.history_removed = true;
                            }
                            staged.deleted_fields.push(name.to_string());
                            staged.deleted_keys.push(encoded);
                        }
                    }
                }
            }
            ContainerChange::Replace(rows) => {
                if !replaced_old.contains_key(name) {
                    let mut current = BTreeMap::new();
                    for row in tx.query("SELECT sort_key,value FROM aq_playbooks.state_records WHERE namespace=$1 AND field=$2",&[&namespace,&name]).map_err(backend)? {
                        current.insert(decode_key(name, row.get::<_, &[u8]>(0)).map_err(backend)?, row.get::<_, Vec<u8>>(1));
                    }
                    replaced_old.insert(name, current);
                }
                let current = &replaced_old[name];
                for (key, bytes) in current {
                    staged
                        .accounting
                        .record(name, key, Some(bytes), None)
                        .map_err(storage)?;
                }
                for (key, bytes) in rows {
                    staged
                        .accounting
                        .record(name, key, None, Some(bytes))
                        .map_err(storage)?;
                }
                if name == "keyed_admissions" {
                    let next: BTreeMap<_, _> = rows.iter().map(|(k, v)| (k, v)).collect();
                    if current.iter().any(|(k, v)| next.get(k) != Some(&v)) {
                        return Err(backend(
                            "immutable keyed admission receipt removed or changed",
                        ));
                    }
                }
            }
        }
    }
    Ok(staged)
}
fn apply(records: &mut Containers, patch: StatePatch) -> Result<(), Error> {
    for (path, change) in patch.changes {
        let (name, _) = field(path.field).map_err(backend)?;
        let container = records.entry(name).or_default();
        match change {
            ContainerChange::Entries(rows) => {
                for (key, new) in rows {
                    match new {
                        Some(bytes) => container.insert(key, bytes),
                        None => container.remove(&key),
                    };
                }
            }
            ContainerChange::Replace(rows) => *container = rows.into_iter().collect(),
        }
    }
    Ok(())
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
        if conn.statements.is_none() {
            conn.statements = Some(Statements::prepare(&mut conn.client)?);
        }
        let statements = conn.statements.clone().expect("prepared above");
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
        let source = Arc::new(PgSource {
            // SAFETY: see `PgSource::tx`; `lent` is dropped before `conn`.
            tx: Mutex::new(Some(unsafe {
                std::mem::transmute::<ObservedTransaction<'_>, ObservedTransaction<'static>>(tx)
            })),
            namespace: namespace.to_string(),
            statements,
            counts: accounting
                .fields
                .iter()
                .map(|(f, s)| (*f, s.count))
                .collect(),
            reads: Mutex::new(0),
            fallbacks: Mutex::new(vec![]),
            poison: Mutex::new(None),
        });
        let lent = Lent(source.clone());
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
        let reads = *lock(&source.reads);
        self.count(|s| s.record_reads += reads);
        Ok(Some(page))
    }
}
/// Inspection page of a complete format 3 State (O(state); see D10 limits).
pub(crate) fn inspect(
    state: &State,
    query: &aq_playbooks_runner::InspectionQuery,
    revision: u64,
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
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
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
