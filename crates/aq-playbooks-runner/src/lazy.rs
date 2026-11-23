//! Lazily bound State containers for the versioned transition capability (D10).
//!
//! Every retained-history container in [`crate::State`] uses one of these types.
//! A container is either *plain* (an ordinary in-memory map/set/vector; the
//! default, used by full `load`/`compare_and_swap` and `MemoryStorage`) or
//! *bound* to an authoritative [`RecordSource`] for one transition. Bound
//! containers read individual records and typed-order successors on demand,
//! track every touched record and emit an exact per-key patch. Any operation
//! that needs the complete container (iteration, ranges, retain, serialization)
//! records a fallback with its caller location, then materializes the whole
//! container from the source, so results always equal the reference State. A
//! source failure poisons the transition and must abort it (fail closed).
mod maps;
mod sequences;
mod sets;
pub use maps::{Entry, LazyMap};
pub use sequences::LazySeq;
pub use sets::LazySet;

use aq_playbooks_model::canonical_bytes;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};
use std::borrow::Borrow;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::{Deref, DerefMut, Index};
use std::panic::Location;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// One record write: its key and new canonical bytes (`None` deletes).
pub type RecordChange = (TreeKey, Option<Vec<u8>>);
/// A touched record: key, original canonical bytes and current value.
pub type Touched<K, V> = (K, Option<Vec<u8>>, Option<V>);
/// Typed physical key. A container's keys all share one variant, so the derived
/// order is the schema comparator: numeric for `U64`/`I64`, Rust string order
/// for `Str` (never lexical order of numeric spellings).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum TreeKey {
    U64(u64),
    I64(i64),
    Str(String),
}
impl TreeKey {
    /// Canonical JSON object-key spelling including quotes and escaping.
    pub fn json_key(&self) -> Vec<u8> {
        let text = match self {
            TreeKey::U64(v) => v.to_string(),
            TreeKey::I64(v) => v.to_string(),
            TreeKey::Str(s) => s.clone(),
        };
        serde_json::to_vec(&text).expect("string serialization is infallible")
    }
    pub fn kind(&self) -> KeyKind {
        match self {
            TreeKey::U64(_) => KeyKind::U64,
            TreeKey::I64(_) => KeyKind::I64,
            TreeKey::Str(_) => KeyKind::Str,
        }
    }
    /// Parse a canonical JSON object key for `kind`, refusing noncanonical
    /// numeric spellings (`09`, `+9`, `-0`) and out-of-range values.
    pub fn parse(kind: KeyKind, text: &str) -> Result<Self, String> {
        let refuse = || format!("noncanonical {kind:?} key {text:?}");
        match kind {
            KeyKind::Str => Ok(TreeKey::Str(text.to_string())),
            KeyKind::U64 => {
                let v: u64 = text.parse().map_err(|_| refuse())?;
                (v.to_string() == text)
                    .then_some(TreeKey::U64(v))
                    .ok_or_else(refuse)
            }
            KeyKind::I64 => {
                let v: i64 = text.parse().map_err(|_| refuse())?;
                (v.to_string() == text)
                    .then_some(TreeKey::I64(v))
                    .ok_or_else(refuse)
            }
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum KeyKind {
    U64,
    I64,
    Str,
}
/// Logical container shape in the schema certificate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Shape {
    /// JSON object keyed by the comparator.
    Map(KeyKind),
    /// JSON array of ordered unique members; physical key is the member.
    Set(KeyKind),
    /// JSON array in position order; physical key is the U64 position.
    Seq,
    /// JSON object whose values are themselves `Map(inner)` containers.
    Nested(KeyKind, KeyKind),
}
pub trait LazyKey:
    Ord + Clone + fmt::Debug + Serialize + DeserializeOwned + Send + Sync + 'static
{
    const KIND: KeyKind;
    fn to_tree(&self) -> TreeKey;
    fn from_tree(key: &TreeKey) -> Option<Self>;
}
impl LazyKey for String {
    const KIND: KeyKind = KeyKind::Str;
    fn to_tree(&self) -> TreeKey {
        TreeKey::Str(self.clone())
    }
    fn from_tree(key: &TreeKey) -> Option<Self> {
        match key {
            TreeKey::Str(s) => Some(s.clone()),
            _ => None,
        }
    }
}
impl LazyKey for u64 {
    const KIND: KeyKind = KeyKind::U64;
    fn to_tree(&self) -> TreeKey {
        TreeKey::U64(*self)
    }
    fn from_tree(key: &TreeKey) -> Option<Self> {
        match key {
            TreeKey::U64(v) => Some(*v),
            _ => None,
        }
    }
}
impl LazyKey for i64 {
    const KIND: KeyKind = KeyKind::I64;
    fn to_tree(&self) -> TreeKey {
        TreeKey::I64(*self)
    }
    fn from_tree(key: &TreeKey) -> Option<Self> {
        match key {
            TreeKey::I64(v) => Some(*v),
            _ => None,
        }
    }
}
/// Logical container address: a schema field, plus the outer key for the
/// inner maps of a [`Shape::Nested`] field.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContainerPath {
    pub field: &'static str,
    pub outer: Option<TreeKey>,
}
impl ContainerPath {
    pub fn field(field: &'static str) -> Self {
        Self { field, outer: None }
    }
}
/// Authoritative record access for one transition. Reads are complete for the
/// requested key/page or return an error; an error must abort the transition.
pub trait RecordSource: Send + Sync {
    /// Canonical value bytes of one record (for a nested outer key: any marker).
    fn get(&self, path: &ContainerPath, key: &TreeKey) -> Result<Option<Vec<u8>>, String>;
    /// Records strictly after `after` in typed order, at most `limit`.
    fn after(
        &self,
        path: &ContainerPath,
        after: Option<&TreeKey>,
        limit: usize,
    ) -> Result<Vec<(TreeKey, Vec<u8>)>, String>;
    fn len(&self, path: &ContainerPath) -> Result<usize, String>;
    /// Complete container (nested outer values are the inner maps' canonical JSON).
    fn scan(&self, path: &ContainerPath) -> Result<Vec<(TreeKey, Vec<u8>)>, String>;
    /// A full-container operation ran during the transition.
    fn fallback(&self, path: &ContainerPath, at: &'static Location<'static>);
    /// Poison the transition; the adapter must refuse to commit.
    fn fail(&self, error: String);
}
/// Changes to one container: exact per-key writes (`None` deletes), or a
/// complete replacement after a full-container fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerChange {
    Entries(Vec<(TreeKey, Option<Vec<u8>>)>),
    Replace(Vec<(TreeKey, Vec<u8>)>),
}
pub type Changes = Vec<(ContainerPath, ContainerChange)>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
fn encode_value<V: Serialize>(v: &V) -> Result<Vec<u8>, String> {
    canonical_bytes(v).map_err(|e| e.to_string())
}
fn decode_value<V: DeserializeOwned>(bytes: &[u8]) -> Result<V, String> {
    serde_json::from_slice(bytes).map_err(|e| format!("record decode: {e}"))
}

thread_local! {
    static HEADER_MODE: Cell<bool> = const { Cell::new(false) };
}
/// Canonical bytes of `value` with every lazy container serialized empty. The
/// complete canonical length is this length plus each container's exact length
/// minus its two empty delimiter bytes.
pub fn header_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            HEADER_MODE.with(|h| h.set(self.0));
        }
    }
    let _reset = Reset(HEADER_MODE.with(|h| h.replace(true)));
    encode_value(value)
}
fn header_mode() -> bool {
    HEADER_MODE.with(|h| h.get())
}

struct Slot<V> {
    original: Option<Vec<u8>>,
    current: Option<V>,
}
impl<V: Clone> Clone for Slot<V> {
    fn clone(&self) -> Self {
        Self {
            original: self.original.clone(),
            current: self.current.clone(),
        }
    }
}
type Encode<K, V> = fn(&K, &V) -> Result<Vec<u8>, String>;
type Decode<K, V> = fn(&K, &[u8]) -> Result<V, String>;

/// Shared bound-container core: an overlay of touched records over a source.
struct Bound<K, V> {
    source: Arc<dyn RecordSource>,
    path: ContainerPath,
    cache: Mutex<BTreeMap<K, Box<Slot<V>>>>,
    base_len: OnceLock<usize>,
    view: OnceLock<BTreeMap<K, V>>,
    encode: Encode<K, V>,
    decode: Decode<K, V>,
}
impl<K: LazyKey, V: Clone> Clone for Bound<K, V> {
    fn clone(&self) -> Self {
        let cache = lock(&self.cache).clone();
        Self {
            source: self.source.clone(),
            path: self.path.clone(),
            cache: Mutex::new(cache),
            base_len: self.base_len.clone(),
            view: OnceLock::new(),
            encode: self.encode,
            decode: self.decode,
        }
    }
}
impl<K: LazyKey, V: Clone> Bound<K, V> {
    fn new(
        source: Arc<dyn RecordSource>,
        path: ContainerPath,
        encode: Encode<K, V>,
        decode: Decode<K, V>,
    ) -> Self {
        Self {
            source,
            path,
            cache: Mutex::new(BTreeMap::new()),
            base_len: OnceLock::new(),
            view: OnceLock::new(),
            encode,
            decode,
        }
    }
    fn decoded(&self, key: &K, bytes: Option<Vec<u8>>) -> Slot<V> {
        let current = match bytes.as_deref().map(|b| (self.decode)(key, b)) {
            Some(Ok(v)) => Some(v),
            Some(Err(e)) => {
                self.source.fail(e);
                None
            }
            None => None,
        };
        Slot {
            original: bytes,
            current,
        }
    }
    fn fetch(&self, key: &K) -> Slot<V> {
        match self.source.get(&self.path, &key.to_tree()) {
            Ok(bytes) => self.decoded(key, bytes),
            Err(e) => {
                self.source.fail(e);
                Slot {
                    original: None,
                    current: None,
                }
            }
        }
    }
    fn slot(&self, key: &K) -> &Slot<V> {
        let mut cache = lock(&self.cache);
        if !cache.contains_key(key) {
            let slot = self.fetch(key);
            cache.insert(key.clone(), Box::new(slot));
        }
        let ptr: *const Slot<V> = &**cache.get(key).expect("inserted");
        drop(cache);
        // SAFETY: slots are boxed, never removed or replaced through `&self`, and
        // only mutated through `&mut self`, which this shared borrow excludes.
        unsafe { &*ptr }
    }
    fn slot_mut(&mut self, key: &K) -> &mut Slot<V> {
        self.view.take();
        if !self
            .cache
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(key)
        {
            let slot = self.fetch(key);
            self.cache
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone(), Box::new(slot));
        }
        self.cache
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(key)
            .expect("inserted")
    }
    /// Insert a record known to be absent from the source without reading it.
    fn insert_absent(&mut self, key: K, value: V) {
        self.view.take();
        self.cache
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                key,
                Box::new(Slot {
                    original: None,
                    current: Some(value),
                }),
            );
    }
    fn base_len(&self) -> usize {
        *self
            .base_len
            .get_or_init(|| match self.source.len(&self.path) {
                Ok(n) => n,
                Err(e) => {
                    self.source.fail(e);
                    0
                }
            })
    }
    fn len(&self) -> usize {
        let base = self.base_len() as isize;
        let delta: isize = lock(&self.cache)
            .values()
            .map(|s| s.current.is_some() as isize - s.original.is_some() as isize)
            .sum();
        (base + delta).max(0) as usize
    }
    /// First live key strictly after `after` in typed order.
    fn next_after(&self, after: Option<&K>) -> Option<K> {
        self.next_after_paged(after, 16)
    }
    /// [`Self::next_after`], reading the source `page` records at a time.
    fn next_after_paged(&self, after: Option<&K>, page: usize) -> Option<K> {
        let mut cache = lock(&self.cache);
        let lower = after.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
        let overlay = cache
            .range::<K, _>((lower, std::ops::Bound::Unbounded))
            .find(|(_, slot)| slot.current.is_some())
            .map(|(key, _)| key.clone());
        let mut cursor = after.map(LazyKey::to_tree);
        loop {
            let page = self
                .source
                .after(&self.path, cursor.as_ref(), page)
                .map_err(|error| self.source.fail(error))
                .ok()?;
            if page.is_empty() {
                return overlay;
            }
            if let std::ops::ControlFlow::Break(key) =
                self.first_uncached(&mut cache, &page, overlay.as_ref())
            {
                return key;
            }
            cursor = page.last().map(|(k, _)| k.clone());
        }
    }
    fn first_uncached(
        &self,
        cache: &mut BTreeMap<K, Box<Slot<V>>>,
        page: &[(TreeKey, Vec<u8>)],
        overlay: Option<&K>,
    ) -> std::ops::ControlFlow<Option<K>> {
        for (tree_key, bytes) in page {
            let Some(key) = self.typed_key(tree_key) else {
                return std::ops::ControlFlow::Break(None);
            };
            if overlay.is_some_and(|o| &key >= o) {
                return std::ops::ControlFlow::Break(overlay.cloned());
            }
            if cache.contains_key(&key) {
                continue;
            }
            let slot = self.decoded(&key, Some(bytes.clone()));
            cache.insert(key.clone(), Box::new(slot));
            return std::ops::ControlFlow::Break(Some(key));
        }
        std::ops::ControlFlow::Continue(())
    }

    #[track_caller]
    fn full(&self, at: &'static Location<'static>) -> BTreeMap<K, V> {
        self.source.fallback(&self.path, at);
        let mut map = BTreeMap::new();
        match self.source.scan(&self.path) {
            Ok(rows) => map.extend(
                rows.into_iter()
                    .filter_map(|(key, bytes)| self.decode_row(&key, &bytes)),
            ),
            Err(error) => self.source.fail(error),
        }
        for (key, slot) in lock(&self.cache).iter() {
            match &slot.current {
                Some(v) => {
                    map.insert(key.clone(), v.clone());
                }
                None => {
                    map.remove(key);
                }
            }
        }
        map
    }
    fn typed_key(&self, key: &TreeKey) -> Option<K> {
        let typed = K::from_tree(key);
        if typed.is_none() {
            self.source
                .fail(format!("{:?}: comparator mismatch", self.path));
        }
        typed
    }

    fn decode_row(&self, key: &TreeKey, bytes: &[u8]) -> Option<(K, V)> {
        let key = self.typed_key(key)?;
        match (self.decode)(&key, bytes) {
            Ok(value) => Some((key, value)),
            Err(error) => {
                self.source.fail(error);
                None
            }
        }
    }
    fn view(&self, at: &'static Location<'static>) -> &BTreeMap<K, V> {
        self.view.get_or_init(|| self.full(at))
    }
    fn changes(&mut self) -> Result<Vec<RecordChange>, String> {
        let mut out = Vec::new();
        for (key, slot) in self
            .cache
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            let current = slot
                .current
                .as_ref()
                .map(|v| (self.encode)(key, v))
                .transpose()?;
            if current != slot.original {
                out.push((key.to_tree(), current));
            }
        }
        Ok(out)
    }
    fn touched(&self) -> Vec<(K, Option<Vec<u8>>, Option<V>)> {
        lock(&self.cache)
            .iter()
            .map(|(k, s)| (k.clone(), s.original.clone(), s.current.clone()))
            .collect()
    }
}

/// One schema-certified lazy container field of the State.
pub trait LazyField {
    fn shape(&self) -> Shape;
    /// Bind an (empty, header-deserialized) field to the source.
    fn bind_field(&mut self, source: Arc<dyn RecordSource>, field: &'static str);
    /// Emit this field's changes (a plain field emits a full replacement,
    /// which doubles as the export used to build a fresh record store).
    fn field_changes(&mut self, field: &'static str, out: &mut Changes) -> Result<(), String>;
    /// True when a bound field was materialized by a full-container mutation.
    fn promoted(&self) -> bool;
}
pub trait Record: Clone + Serialize + DeserializeOwned + Send + Sync + 'static {}
impl<T: Clone + Serialize + DeserializeOwned + Send + Sync + 'static> Record for T {}

fn map_encode<K, V: Serialize>(_: &K, v: &V) -> Result<Vec<u8>, String> {
    encode_value(v)
}
fn map_decode<K, V: DeserializeOwned>(_: &K, b: &[u8]) -> Result<V, String> {
    decode_value(b)
}
fn keys_after<K: LazyKey, Q: ?Sized>(
    after: Option<&Q>,
    limit: usize,
    mut next: impl FnMut(Option<&Q>) -> Option<K>,
) -> Vec<K>
where
    K: Borrow<Q>,
{
    let mut out = Vec::new();
    while out.len() < limit {
        let cursor = out.last().map(Borrow::borrow).or(after);
        let Some(key) = next(cursor) else { break };
        out.push(key);
    }
    out
}
