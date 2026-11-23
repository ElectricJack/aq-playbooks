//! Versioned transition capability `transition_v1` (D10, PB-04).
//!
//! A backend that implements the capability runs the engine's transition plan
//! against a State whose retained-history containers are bound to the
//! backend's authoritative records (see [`crate::lazy`]), keeps an exact count
//! of each container's records, and commits only the touched records in one
//! atomic revision. Backends without the capability return
//! [`TransitionOutcome::Unsupported`] before running the plan, and the engine
//! then uses the full `load`/`compare_and_swap` path unchanged.
use crate::lazy::{
    Changes, ContainerChange, ContainerPath, LazyField, RecordSource, Shape, TreeKey, header_bytes,
};
use crate::storage::{Storage, StorageError, VersionedState};
use crate::{Error, Result, State};
use std::collections::BTreeMap;
use std::panic::Location;

mod patch_checks;
use patch_checks::{verify_events, verify_keyed_admissions, verify_patch_removals};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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
            ("indexes.contract_events", &mut i.contract_events),
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
            ("retention.expiry", &mut r.expiry),
            (
                "retention.admission_expires_at_ms",
                &mut r.admission_expires_at_ms,
            ),
            ("retention.replay_order", &mut r.replay_order),
            ("retention.replay_held", &mut r.replay_held),
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
        let container = assemble_container(shape, rows)?;
        set_field(&mut value, field, container)?;
    }
    serde_json::from_value(value).map_err(|e| backend(format!("state assemble: {e}")))
}

fn assemble_container(shape: Shape, rows: Vec<(TreeKey, Vec<u8>)>) -> Result<serde_json::Value> {
    Ok(match shape {
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
    })
}
fn set_field(
    value: &mut serde_json::Value,
    field: &str,
    container: serde_json::Value,
) -> Result<()> {
    let mut slot = value;
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
    Ok(())
}
/// The number of records of each container of one namespace revision
/// (what a bound container's `len` reads), and the header's length.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Accounting {
    pub header_len: usize,
    pub fields: BTreeMap<&'static str, usize>,
}
impl Accounting {
    /// Records of `field`.
    pub fn count(&self, field: &str) -> usize {
        self.fields.get(field).copied().unwrap_or(0)
    }
    /// Account one record write: whether the record existed before and
    /// whether it exists after.
    pub fn record(&mut self, field: &'static str, old: bool, new: bool) -> Result<()> {
        shape_of(field).ok_or_else(|| backend(format!("unknown field {field}")))?;
        let count = self.fields.entry(field).or_default();
        *count = (*count + usize::from(new))
            .checked_sub(usize::from(old))
            .ok_or_else(|| backend(format!("{field}: record count underflow")))?;
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
    history: crate::history::HistoryRows,
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
            // An empty page costs one read, as an absent record does.
            self.stats.record_reads.fetch_add(1, Ordering::Relaxed);
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
                    let before = old.is_some_and(|c| c.contains_key(key));
                    next.record(field, before, new.is_some())?;
                }
            }
            ContainerChange::Replace(rows) => {
                for _ in old.into_iter().flatten() {
                    next.record(field, true, false)?;
                }
                for _ in rows {
                    next.record(field, false, true)?;
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
        apply_rows(container, change);
        if container.is_empty() {
            data.containers.remove(field);
        }
    }
    Ok(())
}
fn apply_rows(container: &mut Container, change: ContainerChange) {
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
    /// Maintained record counts of `namespace`.
    pub fn accounting(&self, namespace: &str) -> Result<Accounting> {
        Ok(self
            .lock()?
            .get(namespace)
            .map(|n| n.accounting.clone())
            .unwrap_or_default())
    }
}
impl Storage for RecordStorage {
    fn persists_history(&self) -> bool {
        true
    }
    fn history_page(
        &self,
        namespace: &str,
        query: &crate::InspectionQuery,
    ) -> std::result::Result<Vec<crate::HistoryItem>, StorageError> {
        let guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        match guard.get(namespace) {
            Some(ns) => crate::history::page(&ns.history, query),
            None => query.validate().map(|()| vec![]),
        }
    }
    fn delete_history(
        &self,
        namespace: &str,
        owner: &str,
        before_ms: i64,
    ) -> std::result::Result<u64, StorageError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| StorageError::Backend(e.to_string()))?;
        let ns = guard
            .get_mut(namespace)
            .filter(|ns| ns.owner.as_deref() == Some(owner))
            .ok_or(StorageError::Ownership)?;
        Ok(crate::history::delete_before(&mut ns.history, before_ms))
    }
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
        crate::retention::verify_removals(&previous, state)?;
        let revision = expected_revision
            .checked_add(1)
            .ok_or_else(|| StorageError::Backend("revision exhausted".into()))?;
        let items = crate::history_items(&state.settled);
        crate::history::check_new(&ns.history, &items)?;
        crate::history::check_not_live(state, &items)?;
        crate::history::check_unrecorded(
            &ns.history,
            &crate::history::new_live_keys(&previous, state),
        )?;
        // `settled` is not part of the stored header (serde skip).
        let (data, accounting) = export(state).map_err(to_storage)?;
        crate::history::insert(&mut ns.history, items, &state.retention.policy);
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
        // `settled` stays on the State for the removal check below.
        let items = crate::history_items(&state.settled);
        crate::history::check_not_live(&state, &items)?;
        source.check()?;
        let patch = take_patch(&mut state)?;
        source.check()?;
        crate::history::check_unrecorded(&ns.history, &crate::history::patch_live_keys(&patch)?)?;
        let accounting = stage(&ns.data, &ns.accounting, &patch)?;
        verify_keyed_admissions(&ns.data, &patch)?;
        verify_events(&ns.data, &patch)?;
        verify_patch_removals(&ns.data, &patch, &state)?;
        source.check()?;
        crate::history::check_new(&ns.history, &items)?;
        let policy = state.retention.policy.clone();
        drop(state);
        drop(source);
        let revision = ns
            .revision
            .checked_add(1)
            .ok_or_else(|| backend("revision exhausted"))?;
        apply(Arc::make_mut(&mut ns.data), patch)?;
        crate::history::insert(&mut ns.history, items, &policy);
        ns.accounting = accounting;
        ns.revision = revision;
        Ok(TransitionOutcome::Committed { revision })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delete_history_of_an_unknown_namespace_creates_no_entry() {
        let store = RecordStorage::new();
        assert_eq!(
            store.delete_history("unknown", "w", 10),
            Err(StorageError::Ownership)
        );
        assert!(!store.inner.lock().unwrap().contains_key("unknown"));
    }
}
