use super::*;

const ORDER: &str = "retention.replay_order";

struct Evictions<'a> {
    source: &'a PgSource,
    previous: State,
    added: Vec<String>,
    removed: bool,
}

/// The evictions of a bounded commit ([`retention::check_evictions`]),
/// judged while the transaction is still lent: the window entries the patch
/// deletes, their stored records and expiries, and the order keys it adds,
/// read one record each. A violation refuses the commit. A patch that
/// replaces one of these containers whole licenses nothing here: it takes
/// the complete path, where `verify_removals` judges it.
pub(super) fn licensed_evictions(
    source: &PgSource,
    header: &[u8],
    patch: &StatePatch,
    next: &State,
) -> Result<BTreeSet<(Window, String)>, Error> {
    let mut check = Evictions {
        source,
        previous: header_state(header)?,
        added: vec![],
        removed: false,
    };
    for (path, change) in &patch.changes {
        if !matches!(path.field, "keyed_admissions" | "events" | ORDER) {
            continue;
        }
        let ContainerChange::Entries(rows) = change else {
            return Ok(BTreeSet::new());
        };
        for (key, new) in rows {
            check.entry(path.field, key, new.is_some())?;
        }
    }
    check.licence(next)
}

impl Evictions<'_> {
    fn get(&self, field: &'static str, key: &str) -> Result<Option<Vec<u8>>, StorageError> {
        self.source
            .get(&ContainerPath::field(field), &TreeKey::Str(key.to_string()))
            .map_err(backend)
    }
    fn decode<T: serde::de::DeserializeOwned>(
        &self,
        field: &'static str,
        key: &str,
    ) -> Result<Option<T>, StorageError> {
        self.get(field, key)?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(backend))
            .transpose()
    }
    fn entry(&mut self, field: &'static str, key: &TreeKey, written: bool) -> Result<(), Error> {
        let TreeKey::Str(key) = key else {
            return Err(backend(format!("{field}: unexpected record key")).into());
        };
        if field == ORDER {
            if written && self.get(ORDER, key)?.is_none() {
                self.added.push(key.clone());
            }
        } else if !written {
            self.removal(field, key)?;
        }
        Ok(())
    }
    fn removal(&mut self, field: &'static str, key: &str) -> Result<(), Error> {
        let Some(bytes) = self.get(field, key)? else {
            return Ok(());
        };
        self.removed = true;
        if field == "events" {
            self.previous
                .events
                .insert(key.into(), serde_json::from_slice(&bytes).map_err(backend)?);
            if let Some(at) = self.decode("retention.event_expires_at_ms", key)? {
                self.previous
                    .retention
                    .event_expires_at_ms
                    .insert(key.into(), at);
            }
        } else {
            self.previous
                .keyed_admissions
                .insert(key.into(), serde_json::from_slice(&bytes).map_err(backend)?);
            if let Some(at) = self.decode("retention.admission_expires_at_ms", key)? {
                self.previous
                    .retention
                    .admission_expires_at_ms
                    .insert(key.into(), at);
            }
        }
        Ok(())
    }
    fn licence(mut self, next: &State) -> Result<BTreeSet<(Window, String)>, Error> {
        if !self.removed && self.added.is_empty() {
            return Ok(BTreeSet::new());
        }
        let added = std::mem::take(&mut self.added);
        Ok(retention::check_evictions(
            &self.previous,
            next,
            &mut added.into_iter(),
            &retention::Stored {
                ordered: &|key| Ok(self.get(ORDER, key)?.is_some()),
                memberships: &|key| self.decode("indexes.event_memberships", key),
                waiters: &|key| self.decode("indexes.waiters", key),
            },
        )?)
    }
}
