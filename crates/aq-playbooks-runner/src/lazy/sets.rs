use super::*;

/// Ordered unique-member array (`BTreeSet` compatible).
pub struct LazySet<K>(SetRepr<K>);
enum SetRepr<K> {
    Plain(BTreeSet<K>),
    Bound(Box<Bound<K, ()>>, OnceLock<BTreeSet<K>>),
    Promoted(BTreeSet<K>),
}
fn set_encode<K: Serialize>(k: &K, _: &()) -> Result<Vec<u8>, String> {
    encode_value(k)
}
fn set_decode<K>(_: &K, _: &[u8]) -> Result<(), String> {
    Ok(())
}
impl<K: LazyKey> LazySet<K> {
    pub fn new() -> Self {
        Self(SetRepr::Plain(BTreeSet::new()))
    }
    pub fn bound(source: Arc<dyn RecordSource>, path: ContainerPath) -> Self {
        Self(SetRepr::Bound(
            Box::new(Bound::new(source, path, set_encode::<K>, set_decode::<K>)),
            OnceLock::new(),
        ))
    }
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        match &self.0 {
            SetRepr::Plain(m) | SetRepr::Promoted(m) => m.contains(key),
            SetRepr::Bound(b, _) => b.slot(&key.to_owned()).current.is_some(),
        }
    }
    pub fn insert(&mut self, key: K) -> bool {
        match &mut self.0 {
            SetRepr::Plain(m) | SetRepr::Promoted(m) => m.insert(key),
            SetRepr::Bound(b, v) => {
                v.take();
                b.slot_mut(&key).current.replace(()).is_none()
            }
        }
    }
    pub fn remove<Q>(&mut self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        match &mut self.0 {
            SetRepr::Plain(m) | SetRepr::Promoted(m) => m.remove(key),
            SetRepr::Bound(b, v) => {
                v.take();
                b.slot_mut(&key.to_owned()).current.take().is_some()
            }
        }
    }
    pub fn len(&self) -> usize {
        match &self.0 {
            SetRepr::Plain(m) | SetRepr::Promoted(m) => m.len(),
            SetRepr::Bound(b, _) => b.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn next_key<Q>(&self, after: Option<&Q>) -> Option<K>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        match &self.0 {
            SetRepr::Plain(m) | SetRepr::Promoted(m) => match after {
                Some(a) => m
                    .range::<Q, _>((std::ops::Bound::Excluded(a), std::ops::Bound::Unbounded))
                    .next()
                    .cloned(),
                None => m.iter().next().cloned(),
            },
            SetRepr::Bound(b, _) => b.next_after(after.map(|a| a.to_owned()).as_ref()),
        }
    }
    pub fn keys_after<Q>(&self, after: Option<&Q>, limit: usize) -> Vec<K>
    where
        K: Borrow<Q>,
        Q: Ord + ?Sized + ToOwned<Owned = K>,
    {
        keys_after(after, limit, |cursor| self.next_key(cursor))
    }
    fn changes(&mut self) -> Result<ContainerChange, String> {
        match &mut self.0 {
            SetRepr::Bound(b, _) => Ok(ContainerChange::Entries(b.changes()?)),
            SetRepr::Plain(m) | SetRepr::Promoted(m) => Ok(ContainerChange::Replace(
                m.iter()
                    .map(|k| Ok((k.to_tree(), encode_value(k)?)))
                    .collect::<Result<_, String>>()?,
            )),
        }
    }
}
impl<K: LazyKey> Default for LazySet<K> {
    fn default() -> Self {
        Self::new()
    }
}
impl<K: LazyKey> Clone for LazySet<K> {
    fn clone(&self) -> Self {
        Self(match &self.0 {
            SetRepr::Plain(m) => SetRepr::Plain(m.clone()),
            SetRepr::Promoted(m) => SetRepr::Promoted(m.clone()),
            SetRepr::Bound(b, _) => SetRepr::Bound(b.clone(), OnceLock::new()),
        })
    }
}
impl<K: LazyKey> fmt::Debug for LazySet<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            SetRepr::Plain(m) | SetRepr::Promoted(m) => m.fmt(f),
            SetRepr::Bound(b, _) => write!(f, "LazySet({:?})", b.path),
        }
    }
}
impl<K: LazyKey> PartialEq for LazySet<K> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl<K: LazyKey> Deref for LazySet<K> {
    type Target = BTreeSet<K>;
    #[track_caller]
    fn deref(&self) -> &BTreeSet<K> {
        match &self.0 {
            SetRepr::Plain(m) | SetRepr::Promoted(m) => m,
            SetRepr::Bound(b, view) => {
                let at = Location::caller();
                view.get_or_init(|| b.full(at).into_keys().collect())
            }
        }
    }
}
impl<K: LazyKey> DerefMut for LazySet<K> {
    #[track_caller]
    fn deref_mut(&mut self) -> &mut BTreeSet<K> {
        if let SetRepr::Bound(b, _) = &self.0 {
            let full = b.full(Location::caller()).into_keys().collect();
            self.0 = SetRepr::Promoted(full);
        }
        match &mut self.0 {
            SetRepr::Plain(m) | SetRepr::Promoted(m) => m,
            SetRepr::Bound(..) => unreachable!(),
        }
    }
}
impl<'a, K: LazyKey> IntoIterator for &'a LazySet<K> {
    type Item = &'a K;
    type IntoIter = std::collections::btree_set::Iter<'a, K>;
    #[track_caller]
    fn into_iter(self) -> Self::IntoIter {
        self.deref().iter()
    }
}
impl<K: LazyKey> IntoIterator for LazySet<K> {
    type Item = K;
    type IntoIter = std::collections::btree_set::IntoIter<K>;
    #[track_caller]
    fn into_iter(mut self) -> Self::IntoIter {
        std::mem::take(self.deref_mut()).into_iter()
    }
}
impl<K: LazyKey> From<BTreeSet<K>> for LazySet<K> {
    fn from(m: BTreeSet<K>) -> Self {
        Self(SetRepr::Plain(m))
    }
}
impl<K: LazyKey> FromIterator<K> for LazySet<K> {
    fn from_iter<I: IntoIterator<Item = K>>(iter: I) -> Self {
        Self(SetRepr::Plain(iter.into_iter().collect()))
    }
}
impl<K: LazyKey> Serialize for LazySet<K> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if header_mode() {
            return BTreeSet::<K>::new().serialize(s);
        }
        self.deref().serialize(s)
    }
}
impl<'de, K: LazyKey> Deserialize<'de> for LazySet<K> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(SetRepr::Plain(BTreeSet::deserialize(d)?)))
    }
}

impl<K: LazyKey> LazyField for LazySet<K> {
    fn shape(&self) -> Shape {
        Shape::Set(K::KIND)
    }
    fn bind_field(&mut self, source: Arc<dyn RecordSource>, field: &'static str) {
        *self = Self::bound(source, ContainerPath::field(field));
    }
    fn field_changes(&mut self, field: &'static str, out: &mut Changes) -> Result<(), String> {
        out.push((ContainerPath::field(field), self.changes()?));
        Ok(())
    }
    fn promoted(&self) -> bool {
        matches!(self.0, SetRepr::Promoted(_))
    }
}
