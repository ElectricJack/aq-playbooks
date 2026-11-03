//! Versioned transition capability `transition_v1` (D10, PB-04).
//!
//! A backend that implements the capability runs the engine's transition plan
//! against a State whose retained-history containers are bound to the
//! backend's authoritative records (see [`crate::lazy`]), validates quota from
//! exact maintained accounting, and commits only the touched records in one
//! atomic revision. Backends without the capability return
//! [`TransitionOutcome::Unsupported`] before running the plan, and the engine
//! then uses the full `load`/`compare_and_swap` path unchanged.
use crate::lazy::{
    Changes, ContainerChange, ContainerPath, LazyField, RecordSource, Shape, TreeKey,
    container_bytes, header_bytes, record_bytes,
};
use crate::storage::{Storage, StorageError, VersionedState};
use crate::{AcceptedEvent, EngineConfig, Error, Result, State};
use std::collections::BTreeMap;
use std::panic::Location;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Exact quota inputs; the full path derives them from the materialized State.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QuotaCounts {
    pub runs: usize,
    pub events: usize,
    pub queued_destinations: usize,
    /// Retained receipts plus keyed admission receipts.
    pub receipts: usize,
    pub state_bytes: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionOutcome {
    /// Capability unavailable for this namespace; the plan was not run.
    Unsupported,
    /// The plan made no change; nothing was committed.
    Unchanged,
    Committed {
        revision: u64,
    },
}
pub struct TransitionRequest<'a> {
    pub operation: &'static str,
    /// Runs the engine transition on the bound State; returns whether it changed.
    pub plan: &'a mut dyn FnMut(&mut State) -> Result<bool>,
    /// Shared quota validation, given the exact post-transition counts.
    pub validate: &'a dyn Fn(&State, &QuotaCounts) -> Result<()>,
}

const QUOTA_REFUSAL: &str = "namespace retention/admission quota exhausted";
pub fn check_quota(config: &EngineConfig, c: &QuotaCounts) -> Result<()> {
    if c.runs > config.max_runs
        || c.events > config.max_events
        || c.queued_destinations > config.max_queued_destinations
        || c.receipts > config.max_receipts
        || c.state_bytes > config.max_state_bytes
    {
        return Err(Error::Refused(QUOTA_REFUSAL.into()));
    }
    Ok(())
}
pub fn queued_destinations(event: &AcceptedEvent) -> usize {
    event
        .destinations
        .iter()
        .filter(|d| d.run.is_none())
        .count()
}
/// Reference quota counts from a fully materialized State.
pub fn quota_counts(state: &State) -> Result<QuotaCounts> {
    Ok(QuotaCounts {
        runs: state.runs.len(),
        events: state.events.len(),
        queued_destinations: state.events.values().map(queued_destinations).sum(),
        receipts: state
            .receipts
            .len()
            .saturating_add(state.keyed_admissions.len()),
        state_bytes: aq_playbooks_model::canonical_bytes(state)?.len(),
    })
}

impl State {
    /// Every retained-history container, by schema path. The list (with each
    /// field's [`Shape`]) is the schema certificate a record backend stores.
    pub fn lazy_fields(&mut self) -> Vec<(&'static str, &mut dyn LazyField)> {
        let i = &mut self.indexes;
        let r = &mut self.retention;
        vec![
            ("keyed_admissions", &mut self.keyed_admissions),
            ("runs", &mut self.runs),
            ("receipts", &mut self.receipts),
            ("intents", &mut self.intents),
            ("waits", &mut self.waits),
            ("events", &mut self.events),
            ("budgets", &mut self.budgets),
            ("indexes.root_members", &mut i.root_members),
            ("indexes.root_active", &mut i.root_active),
            ("indexes.root_effects", &mut i.root_effects),
            ("indexes.root_intents", &mut i.root_intents),
            ("indexes.root_last_activity", &mut i.root_last_activity),
            ("indexes.run_roots", &mut i.run_roots),
            ("indexes.intent_roots", &mut i.intent_roots),
            ("indexes.effects", &mut i.effects),
            ("indexes.effect_memberships", &mut i.effect_memberships),
            ("indexes.timers", &mut i.timers),
            ("indexes.due", &mut i.due),
            ("indexes.run_timer", &mut i.run_timer),
            ("indexes.events", &mut i.events),
            ("indexes.event_memberships", &mut i.event_memberships),
            ("indexes.waiters", &mut i.waiters),
            ("indexes.run_subscriptions", &mut i.run_subscriptions),
            ("indexes.dependents", &mut i.dependents),
            ("indexes.run_dependencies", &mut i.run_dependencies),
            ("indexes.runs", &mut i.runs),
            ("indexes.receipts", &mut i.receipts),
            ("indexes.all_receipts", &mut i.all_receipts),
            ("retention.event_epochs", &mut r.event_epochs),
            ("retention.event_expires_at_ms", &mut r.event_expires_at_ms),
            (
                "retention.expired_correlations",
                &mut r.expired_correlations,
            ),
            ("retention.event_tombstones", &mut r.event_tombstones),
            ("retention.disposals", &mut r.disposals),
        ]
    }
}
/// The schema certificate: lazy field paths and their container shapes.
pub fn schema() -> Vec<(&'static str, Shape)> {
    let mut state = State::default();
    state
        .lazy_fields()
        .into_iter()
        .map(|(name, field)| (name, field.shape()))
        .collect()
}
pub fn shape_of(field: &str) -> Option<Shape> {
    schema()
        .into_iter()
        .find(|(name, _)| *name == field)
        .map(|(_, shape)| shape)
}

/// Header bytes plus per-container changes for one commit (or an export).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePatch {
    pub header: Vec<u8>,
    pub changes: Changes,
}
fn backend(e: impl std::fmt::Display) -> Error {
    Error::Storage(StorageError::Backend(e.to_string()))
}
/// Deserialize a header (lazy fields empty) without binding.
pub fn header_state(header: &[u8]) -> Result<State> {
    serde_json::from_slice(header).map_err(|e| backend(format!("state header: {e}")))
}
/// Deserialize a header and bind every lazy container to `source`.
pub fn bind_state(header: &[u8], source: Arc<dyn RecordSource>) -> Result<State> {
    let mut state = header_state(header)?;
    for (name, field) in state.lazy_fields() {
        field.bind_field(source.clone(), name);
    }
    Ok(state)
}
/// Header plus container changes. For a plain State every container is a
/// complete replacement, so this is also the full export of a State.
pub fn take_patch(state: &mut State) -> Result<StatePatch> {
    let header = header_bytes(&*state).map_err(backend)?;
    let mut changes = Vec::new();
    for (name, field) in state.lazy_fields() {
        field.field_changes(name, &mut changes).map_err(backend)?;
    }
    Ok(StatePatch { header, changes })
}
/// Rebuild a complete State from a header and each container's records.
pub fn assemble_state(
    header: &[u8],
    mut records: impl FnMut(&'static str) -> Result<Vec<(TreeKey, Vec<u8>)>>,
) -> Result<State> {
    let mut value: serde_json::Value =
        serde_json::from_slice(header).map_err(|e| backend(format!("state header: {e}")))?;
    for (field, shape) in schema() {
        let rows = records(field)?;
        let container = match shape {
            Shape::Map(_) | Shape::Nested(..) => {
                let mut map = serde_json::Map::new();
                for (key, bytes) in rows {
                    let text = match key {
                        TreeKey::U64(v) => v.to_string(),
                        TreeKey::I64(v) => v.to_string(),
                        TreeKey::Str(s) => s,
                    };
                    map.insert(text, serde_json::from_slice(&bytes).map_err(backend)?);
                }
                serde_json::Value::Object(map)
            }
            Shape::Set(_) | Shape::Seq => serde_json::Value::Array(
                rows.into_iter()
                    .map(|(_, bytes)| serde_json::from_slice(&bytes).map_err(backend))
                    .collect::<Result<_>>()?,
            ),
        };
        let mut slot = &mut value;
        let mut parts = field.split('.').peekable();
        while let Some(part) = parts.next() {
            let object = slot
                .as_object_mut()
                .ok_or_else(|| backend(format!("state header: {field} parent is not an object")))?;
            if parts.peek().is_none() {
                object.insert(part.to_string(), container);
                break;
            }
            slot = object
                .entry(part.to_string())
                .or_insert_with(|| serde_json::Value::Object(Default::default()));
        }
    }
    serde_json::from_value(value).map_err(|e| backend(format!("state assemble: {e}")))
}

/// Per-container record sums: `pairs` is Σ record bytes (see [`record_bytes`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContainerStats {
    pub count: usize,
    pub pairs: usize,
}
/// Exact maintained accounting for one namespace revision.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Accounting {
    pub header_len: usize,
    pub fields: BTreeMap<&'static str, ContainerStats>,
    pub queued_destinations: usize,
}
fn queued_bytes(bytes: &[u8]) -> Result<usize> {
    let event: AcceptedEvent = serde_json::from_slice(bytes).map_err(backend)?;
    Ok(queued_destinations(&event))
}
impl Accounting {
    pub fn counts(&self) -> QuotaCounts {
        let count = |f: &str| self.fields.get(f).map_or(0, |s| s.count);
        let state_bytes = self.header_len
            + self
                .fields
                .values()
                .map(|s| container_bytes(s.pairs, s.count) - 2)
                .sum::<usize>();
        QuotaCounts {
            runs: count("runs"),
            events: count("events"),
            queued_destinations: self.queued_destinations,
            receipts: count("receipts").saturating_add(count("keyed_admissions")),
            state_bytes,
        }
    }
    /// Account one record write: `old`/`new` are canonical value bytes.
    pub fn record(
        &mut self,
        field: &'static str,
        key: &TreeKey,
        old: Option<&[u8]>,
        new: Option<&[u8]>,
    ) -> Result<()> {
        let shape = shape_of(field).ok_or_else(|| backend(format!("unknown field {field}")))?;
        let stats = self.fields.entry(field).or_default();
        if let Some(old) = old {
            stats.count -= 1;
            stats.pairs -= record_bytes(shape, key, old);
            if field == "events" {
                self.queued_destinations -= queued_bytes(old)?;
            }
        }
        if let Some(new) = new {
            stats.count += 1;
            stats.pairs += record_bytes(shape, key, new);
            if field == "events" {
                self.queued_destinations += queued_bytes(new)?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// In-memory record backend (reference/test double for the capability).

type Container = BTreeMap<TreeKey, Vec<u8>>;
#[derive(Debug, Clone, Default)]
struct RecordData {
    header: Vec<u8>,
    containers: BTreeMap<&'static str, Container>,
}
impl RecordData {
    fn container(&self, field: &str) -> Option<&Container> {
        self.containers.get(field)
    }
}
#[derive(Debug, Default)]
struct RecordNamespace {
    owner: Option<String>,
    revision: u64,
    data: Arc<RecordData>,
    accounting: Accounting,
}
/// Observability for the in-memory record backend.
#[derive(Debug, Default)]
pub struct RecordStats {
    pub full_loads: AtomicUsize,
    pub full_commits: AtomicUsize,
    pub transitions: AtomicUsize,
    pub record_reads: AtomicUsize,
    pub fallbacks: Mutex<Vec<String>>,
}
impl RecordStats {
    pub fn fallbacks(&self) -> Vec<String> {
        self.fallbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}
/// Record-addressed in-memory storage implementing `transition_v1`.
#[derive(Clone, Default)]
pub struct RecordStorage {
    inner: Arc<Mutex<BTreeMap<String, RecordNamespace>>>,
    pub stats: Arc<RecordStats>,
    unsupported: bool,
}
impl RecordStorage {
    pub fn new() -> Self {
        Self::default()
    }
    /// Same records, but the capability reports `Unsupported`.
    pub fn without_transition() -> Self {
        Self {
            unsupported: true,
            ..Self::default()
        }
    }
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, RecordNamespace>>> {
        self.inner.lock().map_err(backend)
    }
}
struct TxSource {
    data: Arc<RecordData>,
    stats: Arc<RecordStats>,
    poison: Mutex<Option<String>>,
}
impl TxSource {
    fn container(&self, path: &ContainerPath) -> std::result::Result<Option<&Container>, String> {
        if path.outer.is_some() {
            return Err(format!(
                "{path:?}: nested containers are not in this schema"
            ));
        }
        Ok(self.data.container(path.field))
    }
    fn check(&self) -> Result<()> {
        match self
            .poison
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            Some(e) => Err(backend(format!("transition source failed: {e}"))),
            None => Ok(()),
        }
    }
}
impl RecordSource for TxSource {
    fn get(
        &self,
        path: &ContainerPath,
        key: &TreeKey,
    ) -> std::result::Result<Option<Vec<u8>>, String> {
        self.stats.record_reads.fetch_add(1, Ordering::Relaxed);
        Ok(self.container(path)?.and_then(|c| c.get(key)).cloned())
    }
    fn after(
        &self,
        path: &ContainerPath,
        after: Option<&TreeKey>,
        limit: usize,
    ) -> std::result::Result<Vec<(TreeKey, Vec<u8>)>, String> {
        use std::ops::Bound::{Excluded, Unbounded};
        let Some(c) = self.container(path)? else {
            return Ok(Vec::new());
        };
        let rows: Vec<_> = match after {
            Some(a) => c.range((Excluded(a.clone()), Unbounded)),
            None => c.range::<TreeKey, _>(..),
        }
        .take(limit)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
        self.stats
            .record_reads
            .fetch_add(rows.len().max(1), Ordering::Relaxed);
        Ok(rows)
    }
    fn len(&self, path: &ContainerPath) -> std::result::Result<usize, String> {
        Ok(self.container(path)?.map_or(0, |c| c.len()))
    }
    fn scan(&self, path: &ContainerPath) -> std::result::Result<Vec<(TreeKey, Vec<u8>)>, String> {
        Ok(self
            .container(path)?
            .map(|c| c.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default())
    }
    fn fallback(&self, path: &ContainerPath, at: &'static Location<'static>) {
        self.stats
            .fallbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("{} at {}:{}", path.field, at.file(), at.line()));
    }
    fn fail(&self, error: String) {
        self.poison
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert(error);
    }
}
fn field_name(path: &ContainerPath) -> Result<&'static str> {
    if path.outer.is_some() {
        return Err(backend("nested container change outside schema"));
    }
    Ok(path.field)
}
/// Apply a patch to record data and accounting, after validation.
fn stage(data: &RecordData, accounting: &Accounting, patch: &StatePatch) -> Result<Accounting> {
    let mut next = accounting.clone();
    next.header_len = patch.header.len();
    for (path, change) in &patch.changes {
        let field = field_name(path)?;
        let old = data.container(field);
        match change {
            ContainerChange::Entries(rows) => {
                for (key, new) in rows {
                    let before = old.and_then(|c| c.get(key)).map(Vec::as_slice);
                    next.record(field, key, before, new.as_deref())?;
                }
            }
            ContainerChange::Replace(rows) => {
                for (key, bytes) in old.into_iter().flatten() {
                    next.record(field, key, Some(bytes), None)?;
                }
                for (key, bytes) in rows {
                    next.record(field, key, None, Some(bytes))?;
                }
            }
        }
    }
    Ok(next)
}
fn apply(data: &mut RecordData, patch: StatePatch) -> Result<()> {
    data.header = patch.header;
    for (path, change) in patch.changes {
        let field = field_name(&path)?;
        let container = data.containers.entry(field).or_default();
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
        if container.is_empty() {
            data.containers.remove(field);
        }
    }
    Ok(())
}
/// Incremental equivalent of `admission::verify_receipt_retention`.
fn verify_keyed_admissions(data: &RecordData, patch: &StatePatch) -> Result<()> {
    let refuse = || {
        Error::Storage(StorageError::Backend(
            "immutable keyed admission receipt removed or changed".into(),
        ))
    };
    let Some(old) = data.container("keyed_admissions") else {
        return Ok(());
    };
    for (path, change) in &patch.changes {
        if path.field != "keyed_admissions" {
            continue;
        }
        match change {
            ContainerChange::Entries(rows) => {
                for (key, new) in rows {
                    if let Some(before) = old.get(key)
                        && new.as_ref() != Some(before)
                    {
                        return Err(refuse());
                    }
                }
            }
            ContainerChange::Replace(rows) => {
                let rows: BTreeMap<_, _> = rows.iter().map(|(k, v)| (k, v)).collect();
                if old.iter().any(|(k, v)| rows.get(k) != Some(&v)) {
                    return Err(refuse());
                }
            }
        }
    }
    Ok(())
}
fn assemble(data: &RecordData) -> Result<State> {
    if data.header.is_empty() {
        return Ok(State::default());
    }
    assemble_state(&data.header, |field| {
        Ok(data
            .container(field)
            .map(|c| c.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default())
    })
}
fn export(state: &State) -> Result<(RecordData, Accounting)> {
    let mut plain = state.clone();
    let patch = take_patch(&mut plain)?;
    let empty = RecordData::default();
    let accounting = stage(&empty, &Accounting::default(), &patch)?;
    let mut data = RecordData::default();
    apply(&mut data, patch)?;
    Ok((data, accounting))
}
impl RecordStorage {
    /// Maintained accounting for `namespace` (for quota proofs).
    pub fn accounting(&self, namespace: &str) -> Result<Accounting> {
        Ok(self
            .lock()?
            .get(namespace)
            .map(|n| n.accounting.clone())
            .unwrap_or_default())
    }
}
impl Storage for RecordStorage {
    fn load(&self, namespace: &str) -> std::result::Result<VersionedState, StorageError> {
        self.stats.full_loads.fetch_add(1, Ordering::Relaxed);
        let guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let Some(ns) = guard.get(namespace) else {
            return Ok(VersionedState::default());
        };
        let state = assemble(&ns.data).map_err(|e| match e {
            Error::Storage(s) => s,
            other => StorageError::Backend(other.to_string()),
        })?;
        Ok(VersionedState {
            revision: ns.revision,
            state,
        })
    }
    fn compare_and_swap(
        &self,
        namespace: &str,
        owner: &str,
        expected_revision: u64,
        state: &State,
    ) -> std::result::Result<u64, StorageError> {
        let to_storage = |e: Error| match e {
            Error::Storage(s) => s,
            other => StorageError::Backend(other.to_string()),
        };
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let ns = guard.entry(namespace.into()).or_default();
        if ns.owner.as_deref() != Some(owner) {
            return Err(StorageError::Ownership);
        }
        if ns.revision != expected_revision {
            return Err(StorageError::Conflict);
        }
        let previous = assemble(&ns.data).map_err(to_storage)?;
        crate::retention::verify_artifact_removals(&previous, state)?;
        crate::admission::verify_receipt_retention(&previous, state)?;
        let revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| StorageError::Backend("revision exhausted".into()))?;
        let (data, accounting) = export(state).map_err(to_storage)?;
        ns.data = Arc::new(data);
        ns.accounting = accounting;
        ns.revision = revision;
        self.stats.full_commits.fetch_add(1, Ordering::Relaxed);
        Ok(revision)
    }
    fn acquire(&self, namespace: &str, owner: &str) -> std::result::Result<(), StorageError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let ns = guard.entry(namespace.into()).or_default();
        if ns.owner.as_deref().is_some_and(|x| x != owner) {
            return Err(StorageError::Ownership);
        }
        ns.owner = Some(owner.into());
        Ok(())
    }
    fn release(&self, namespace: &str, owner: &str) -> std::result::Result<(), StorageError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let ns = guard.entry(namespace.into()).or_default();
        if ns.owner.as_deref() != Some(owner) {
            return Err(StorageError::Ownership);
        }
        ns.owner = None;
        Ok(())
    }
    fn query(
        &self,
        namespace: &str,
        query: &crate::IndexQuery,
    ) -> std::result::Result<crate::IndexPage, StorageError> {
        let to_storage = |e: Error| match e {
            Error::Storage(s) => s,
            other => StorageError::Backend(other.to_string()),
        };
        let data = {
            let guard = self
                .inner
                .lock()
                .map_err(|e| StorageError::Backend(e.to_string()))?;
            guard.get(namespace).map(|ns| ns.data.clone())
        };
        let bounded = match data {
            Some(data) if !self.unsupported && !data.header.is_empty() => {
                let source = Arc::new(TxSource {
                    data: data.clone(),
                    stats: self.stats.clone(),
                    poison: Mutex::new(None),
                });
                let state = bind_state(&data.header, source.clone()).map_err(to_storage)?;
                if state.indexes.initialized {
                    let page = state.indexes.query(&state, query);
                    source.check().map_err(to_storage)?;
                    Some(page)
                } else {
                    None
                }
            }
            _ => None,
        };
        match bounded {
            Some(page) => Ok(page),
            None => {
                let state = self.load(namespace)?.state;
                Ok(state.indexes.query(&state, query))
            }
        }
    }
    fn transition_v1_support(&self, _namespace: &str) -> std::result::Result<bool, StorageError> {
        Ok(!self.unsupported)
    }
    fn transition_v1(
        &self,
        namespace: &str,
        owner: &str,
        request: &mut TransitionRequest<'_>,
    ) -> Result<TransitionOutcome> {
        if self.unsupported {
            return Ok(TransitionOutcome::Unsupported);
        }
        let mut guard = self.lock()?;
        let ns = guard.entry(namespace.into()).or_default();
        if ns.owner.as_deref() != Some(owner) {
            return Err(StorageError::Ownership.into());
        }
        if ns.data.header.is_empty() {
            return Ok(TransitionOutcome::Unsupported);
        }
        let source = Arc::new(TxSource {
            data: ns.data.clone(),
            stats: self.stats.clone(),
            poison: Mutex::new(None),
        });
        let mut state = bind_state(&ns.data.header, source.clone())?;
        if !state.indexes.initialized {
            return Ok(TransitionOutcome::Unsupported);
        }
        self.stats.transitions.fetch_add(1, Ordering::Relaxed);
        let changed = (request.plan)(&mut state)?;
        source.check()?;
        if !changed {
            return Ok(TransitionOutcome::Unchanged);
        }
        let patch = take_patch(&mut state)?;
        source.check()?;
        let accounting = stage(&ns.data, &ns.accounting, &patch)?;
        (request.validate)(&state, &accounting.counts())?;
        verify_keyed_admissions(&ns.data, &patch)?;
        let previous = header_state(&ns.data.header)?;
        crate::retention::verify_artifact_removals(&previous, &state)?;
        source.check()?;
        drop(state);
        drop(source);
        let revision = ns
            .revision
            .checked_add(1)
            .ok_or_else(|| backend("revision exhausted"))?;
        apply(Arc::make_mut(&mut ns.data), patch)?;
        ns.accounting = accounting;
        ns.revision = revision;
        Ok(TransitionOutcome::Committed { revision })
    }
}
