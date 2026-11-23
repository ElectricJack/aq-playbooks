use super::*;

/// Position-ordered array (`Vec` compatible); physical key is the position.
/// Bound sequences support append and positional access; any other mutation
/// (retain, truncate, removal) promotes to a full replacement.
pub struct LazySeq<T>(SeqRepr<T>);
enum SeqRepr<T> {
    Plain(Vec<T>),
    Bound(Box<Bound<u64, T>>, OnceLock<Vec<T>>),
    Promoted(Vec<T>),
}
impl<T: Record> LazySeq<T> {
    pub fn new() -> Self {
        Self(SeqRepr::Plain(Vec::new()))
    }
    pub fn bound(source: Arc<dyn RecordSource>, path: ContainerPath) -> Self {
        Self(SeqRepr::Bound(
            Box::new(Bound::new(
                source,
                path,
                map_encode::<u64, T>,
                map_decode::<u64, T>,
            )),
            OnceLock::new(),
        ))
    }
    pub fn len(&self) -> usize {
        match &self.0 {
            SeqRepr::Plain(v) | SeqRepr::Promoted(v) => v.len(),
            SeqRepr::Bound(b, _) => b.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn push(&mut self, value: T) {
        match &mut self.0 {
            SeqRepr::Plain(v) | SeqRepr::Promoted(v) => v.push(value),
            SeqRepr::Bound(b, view) => {
                view.take();
                let at = b.len() as u64;
                b.insert_absent(at, value);
            }
        }
    }
    pub fn get(&self, index: usize) -> Option<&T> {
        match &self.0 {
            SeqRepr::Plain(v) | SeqRepr::Promoted(v) => v.get(index),
            SeqRepr::Bound(b, _) => {
                if index >= b.len() {
                    return None;
                }
                b.slot(&(index as u64)).current.as_ref()
            }
        }
    }
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        match &mut self.0 {
            SeqRepr::Plain(v) | SeqRepr::Promoted(v) => v.get_mut(index),
            SeqRepr::Bound(b, view) => {
                if index >= b.len() {
                    return None;
                }
                view.take();
                b.slot_mut(&(index as u64)).current.as_mut()
            }
        }
    }
    pub fn last(&self) -> Option<&T> {
        self.len().checked_sub(1).and_then(|i| self.get(i))
    }
    fn changes(&mut self) -> Result<ContainerChange, String> {
        match &mut self.0 {
            SeqRepr::Bound(b, _) => Ok(ContainerChange::Entries(b.changes()?)),
            SeqRepr::Plain(v) | SeqRepr::Promoted(v) => Ok(ContainerChange::Replace(
                v.iter()
                    .enumerate()
                    .map(|(i, x)| Ok((TreeKey::U64(i as u64), encode_value(x)?)))
                    .collect::<Result<_, String>>()?,
            )),
        }
    }
}
impl<T: Record> Default for LazySeq<T> {
    fn default() -> Self {
        Self::new()
    }
}
impl<T: Record> Clone for LazySeq<T> {
    fn clone(&self) -> Self {
        Self(match &self.0 {
            SeqRepr::Plain(v) => SeqRepr::Plain(v.clone()),
            SeqRepr::Promoted(v) => SeqRepr::Promoted(v.clone()),
            SeqRepr::Bound(b, _) => SeqRepr::Bound(b.clone(), OnceLock::new()),
        })
    }
}
impl<T: Record + fmt::Debug> fmt::Debug for LazySeq<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            SeqRepr::Plain(v) | SeqRepr::Promoted(v) => v.fmt(f),
            SeqRepr::Bound(b, _) => write!(f, "LazySeq({:?})", b.path),
        }
    }
}
impl<T: Record + PartialEq> PartialEq for LazySeq<T> {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}
impl<T: Record> Deref for LazySeq<T> {
    type Target = Vec<T>;
    #[track_caller]
    fn deref(&self) -> &Vec<T> {
        match &self.0 {
            SeqRepr::Plain(v) | SeqRepr::Promoted(v) => v,
            SeqRepr::Bound(b, view) => {
                let at = Location::caller();
                view.get_or_init(|| b.full(at).into_values().collect())
            }
        }
    }
}
impl<T: Record> DerefMut for LazySeq<T> {
    #[track_caller]
    fn deref_mut(&mut self) -> &mut Vec<T> {
        if let SeqRepr::Bound(b, _) = &self.0 {
            let full = b.full(Location::caller()).into_values().collect();
            self.0 = SeqRepr::Promoted(full);
        }
        match &mut self.0 {
            SeqRepr::Plain(v) | SeqRepr::Promoted(v) => v,
            SeqRepr::Bound(..) => unreachable!(),
        }
    }
}
impl<T: Record> Index<usize> for LazySeq<T> {
    type Output = T;
    fn index(&self, index: usize) -> &T {
        self.get(index).expect("sequence index out of range")
    }
}
impl<'a, T: Record> IntoIterator for &'a LazySeq<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;
    #[track_caller]
    fn into_iter(self) -> Self::IntoIter {
        self.deref().iter()
    }
}
impl<T: Record> From<Vec<T>> for LazySeq<T> {
    fn from(v: Vec<T>) -> Self {
        Self(SeqRepr::Plain(v))
    }
}
impl<T: Record> Serialize for LazySeq<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if header_mode() {
            return Vec::<T>::new().serialize(s);
        }
        self.deref().serialize(s)
    }
}
impl<'de, T: Record> Deserialize<'de> for LazySeq<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(SeqRepr::Plain(Vec::deserialize(d)?)))
    }
}

impl<T: Record> LazyField for LazySeq<T> {
    fn shape(&self) -> Shape {
        Shape::Seq
    }
    fn bind_field(&mut self, source: Arc<dyn RecordSource>, field: &'static str) {
        *self = Self::bound(source, ContainerPath::field(field));
    }
    fn field_changes(&mut self, field: &'static str, out: &mut Changes) -> Result<(), String> {
        out.push((ContainerPath::field(field), self.changes()?));
        Ok(())
    }
    fn promoted(&self) -> bool {
        matches!(self.0, SeqRepr::Promoted(_))
    }
}

impl<T: Record> IntoIterator for LazySeq<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    #[track_caller]
    fn into_iter(mut self) -> Self::IntoIter {
        std::mem::take(self.deref_mut()).into_iter()
    }
}
impl<T: Record> std::ops::IndexMut<usize> for LazySeq<T> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        self.get_mut(index).expect("sequence index out of range")
    }
}
