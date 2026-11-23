use super::*;

/// Object container (`BTreeMap` compatible).
pub struct LazyMap<K, V>(MapRepr<K, V>);
enum MapRepr<K, V> {
    Plain(BTreeMap<K, V>),
    Bound(Box<Bound<K, V>>),
    /// Materialized after a full-container mutation; patched as a replacement.
    Promoted(BTreeMap<K, V>),
}
impl<K: LazyKey, V: Record> LazyMap<K, V> {
    pub fn new() -> Self {
        Self(MapRepr::Plain(BTreeMap::new()))
    }
    pub fn bound(source: Arc<dyn RecordSource>, path: ContainerPath) -> Self {
        Self(MapRepr::Bound(Box::new(Bound::new(
            source,
            path,
            map_encode::<K, V>,
            map_decode::<K, V>,
        ))))
    }
    pub fn is_bound(&self) -> bool {
        matches!(self.0, MapRepr::Bound(_))
    }
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        match &self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m.get(key),
            MapRepr::Bound(b) => b.slot(&key.to_owned()).current.as_ref(),
        }
    }
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        match &mut self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m.get_mut(key),
            MapRepr::Bound(b) => b.slot_mut(&key.to_owned()).current.as_mut(),
        }
    }
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        self.get(key).is_some()
    }
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        match &mut self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m.insert(key, value),
            MapRepr::Bound(b) => b.slot_mut(&key).current.replace(value),
        }
    }
    /// Insert a record the caller has just proven absent (for example a fresh
    /// sequence-derived identifier); bound containers skip the source read.
    pub fn insert_new(&mut self, key: K, value: V) {
        match &mut self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => {
                m.insert(key, value);
            }
            MapRepr::Bound(b) => {
                if lock(&b.cache).contains_key(&key) {
                    b.slot_mut(&key).current = Some(value);
                } else {
                    b.insert_absent(key, value)
                }
            }
        }
    }
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        match &mut self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m.remove(key),
            MapRepr::Bound(b) => b.slot_mut(&key.to_owned()).current.take(),
        }
    }
    pub fn len(&self) -> usize {
        match &self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m.len(),
            MapRepr::Bound(b) => b.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn entry(&mut self, key: K) -> Entry<'_, K, V> {
        Entry { map: self, key }
    }
    /// First key strictly after `after` (or the first key) in typed order,
    /// without materializing the container.
    pub fn next_key<Q>(&self, after: Option<&Q>) -> Option<K>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        match &self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => match after {
                Some(a) => m
                    .range::<Q, _>((std::ops::Bound::Excluded(a), std::ops::Bound::Unbounded))
                    .next()
                    .map(|(k, _)| k.clone()),
                None => m.keys().next().cloned(),
            },
            MapRepr::Bound(b) => b.next_after(after.map(|a| a.to_owned()).as_ref()),
        }
    }
    /// [`Self::next_key`] reading one record at a time from a bound source,
    /// for a caller that needs only the next key (the head of an order), so
    /// its cost does not depend on how many records follow it.
    pub fn next_key_by_one<Q>(&self, after: Option<&Q>) -> Option<K>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        match &self.0 {
            MapRepr::Bound(b) => b.next_after_paged(after.map(|a| a.to_owned()).as_ref(), 1),
            _ => self.next_key(after),
        }
    }
    /// Up to `limit` keys strictly after `after`, in typed order.
    pub fn keys_after<Q>(&self, after: Option<&Q>, limit: usize) -> Vec<K>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        keys_after(after, limit, |cursor| self.next_key(cursor))
    }
    /// Touched records `(key, original canonical bytes, current value)`; `None`
    /// unless bound (callers then use the complete map).
    pub fn touched(&self) -> Option<Vec<Touched<K, V>>> {
        match &self.0 {
            MapRepr::Bound(b) => Some(b.touched()),
            _ => None,
        }
    }
    pub fn into_plain(self) -> BTreeMap<K, V> {
        match self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m,
            MapRepr::Bound(b) => b.full(Location::caller()),
        }
    }
    fn changes(&mut self) -> Result<ContainerChange, String> {
        match &mut self.0 {
            MapRepr::Bound(b) => Ok(ContainerChange::Entries(b.changes()?)),
            MapRepr::Plain(m) | MapRepr::Promoted(m) => Ok(ContainerChange::Replace(
                m.iter()
                    .map(|(k, v)| Ok((k.to_tree(), encode_value(v)?)))
                    .collect::<Result<_, String>>()?,
            )),
        }
    }
}
impl<K: LazyKey, V: Record> Default for LazyMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}
impl<K: LazyKey, V: Record> Clone for LazyMap<K, V> {
    fn clone(&self) -> Self {
        Self(match &self.0 {
            MapRepr::Plain(m) => MapRepr::Plain(m.clone()),
            MapRepr::Promoted(m) => MapRepr::Promoted(m.clone()),
            MapRepr::Bound(b) => MapRepr::Bound(b.clone()),
        })
    }
}
impl<K: LazyKey, V: Record + fmt::Debug> fmt::Debug for LazyMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m.fmt(f),
            MapRepr::Bound(b) => write!(f, "LazyMap({:?})", b.path),
        }
    }
}
impl<K: LazyKey, V: Record + PartialEq> PartialEq for LazyMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl<K: LazyKey, V: Record> Deref for LazyMap<K, V> {
    type Target = BTreeMap<K, V>;
    #[track_caller]
    fn deref(&self) -> &BTreeMap<K, V> {
        match &self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m,
            MapRepr::Bound(b) => b.view(Location::caller()),
        }
    }
}
impl<K: LazyKey, V: Record> DerefMut for LazyMap<K, V> {
    #[track_caller]
    fn deref_mut(&mut self) -> &mut BTreeMap<K, V> {
        if let MapRepr::Bound(b) = &self.0 {
            let full = b.full(Location::caller());
            self.0 = MapRepr::Promoted(full);
        }
        match &mut self.0 {
            MapRepr::Plain(m) | MapRepr::Promoted(m) => m,
            MapRepr::Bound(_) => unreachable!(),
        }
    }
}
impl<K: LazyKey + Borrow<Q>, V: Record, Q: Ord + ?Sized + ToOwned<Owned = K>> Index<&Q>
    for LazyMap<K, V>
{
    type Output = V;
    fn index(&self, key: &Q) -> &V {
        self.get(key).expect("no entry found for key")
    }
}
impl<'a, K: LazyKey, V: Record> IntoIterator for &'a LazyMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = std::collections::btree_map::Iter<'a, K, V>;
    #[track_caller]
    fn into_iter(self) -> Self::IntoIter {
        self.deref().iter()
    }
}
impl<'a, K: LazyKey, V: Record> IntoIterator for &'a mut LazyMap<K, V> {
    type Item = (&'a K, &'a mut V);
    type IntoIter = std::collections::btree_map::IterMut<'a, K, V>;
    #[track_caller]
    fn into_iter(self) -> Self::IntoIter {
        self.deref_mut().iter_mut()
    }
}
impl<K: LazyKey, V: Record> From<BTreeMap<K, V>> for LazyMap<K, V> {
    fn from(m: BTreeMap<K, V>) -> Self {
        Self(MapRepr::Plain(m))
    }
}
impl<K: LazyKey, V: Record> FromIterator<(K, V)> for LazyMap<K, V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(MapRepr::Plain(iter.into_iter().collect()))
    }
}
impl<K: LazyKey, V: Record> Serialize for LazyMap<K, V> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if header_mode() {
            return BTreeMap::<K, V>::new().serialize(s);
        }
        self.deref().serialize(s)
    }
}
impl<'de, K: LazyKey, V: Record> Deserialize<'de> for LazyMap<K, V> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(MapRepr::Plain(BTreeMap::deserialize(d)?)))
    }
}
pub struct Entry<'a, K: LazyKey, V: Record> {
    map: &'a mut LazyMap<K, V>,
    key: K,
}
impl<'a, K: LazyKey, V: Record> Entry<'a, K, V> {
    pub fn and_modify(self, f: impl FnOnce(&mut V)) -> Self {
        if let Some(v) = self.map.get_mut::<K>(&self.key) {
            f(v);
        }
        self
    }
    pub fn or_insert_with(self, f: impl FnOnce() -> V) -> &'a mut V {
        if !self.map.contains_key::<K>(&self.key) {
            self.map.insert(self.key.clone(), f());
        }
        self.map.get_mut::<K>(&self.key).expect("present")
    }
    pub fn or_insert(self, value: V) -> &'a mut V {
        self.or_insert_with(|| value)
    }
    pub fn or_default(self) -> &'a mut V
    where
        V: Default,
    {
        self.or_insert_with(V::default)
    }
}

impl<K: LazyKey, V: Record> LazyField for LazyMap<K, V> {
    fn shape(&self) -> Shape {
        Shape::Map(K::KIND)
    }
    fn bind_field(&mut self, source: Arc<dyn RecordSource>, field: &'static str) {
        *self = Self::bound(source, ContainerPath::field(field));
    }
    fn field_changes(&mut self, field: &'static str, out: &mut Changes) -> Result<(), String> {
        out.push((ContainerPath::field(field), self.changes()?));
        Ok(())
    }
    fn promoted(&self) -> bool {
        matches!(self.0, MapRepr::Promoted(_))
    }
}
impl<K: LazyKey, V: Record> IntoIterator for LazyMap<K, V> {
    type Item = (K, V);
    type IntoIter = std::collections::btree_map::IntoIter<K, V>;
    #[track_caller]
    fn into_iter(mut self) -> Self::IntoIter {
        std::mem::take(self.deref_mut()).into_iter()
    }
}
impl<K: LazyKey + Borrow<Q>, V: Record, Q: Ord + ?Sized + ToOwned<Owned = K>> std::ops::IndexMut<&Q>
    for LazyMap<K, V>
{
    fn index_mut(&mut self, key: &Q) -> &mut V {
        self.get_mut(key).expect("no entry found for key")
    }
}
