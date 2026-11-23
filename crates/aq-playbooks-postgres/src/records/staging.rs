use super::*;
use aq_playbooks_runner::lazy::RecordChange;

type OldRows = BTreeMap<(String, Vec<u8>), Vec<u8>>;
type Fields = BTreeMap<(&'static str, Vec<u8>), Vec<u8>>;
type ExpiryChange = (&'static str, Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>);
pub(super) struct Retention<'a> {
    pub(super) clock_ms: Option<i64>,
    pub(super) policy: &'a aq_playbooks_runner::RetentionPolicy,
    pub(super) rebase: bool,
    pub(super) evicted: &'a BTreeSet<(Window, String)>,
}
pub(super) struct Request<'a> {
    pub(super) accounting: &'a Accounting,
    pub(super) patch: &'a StatePatch,
    pub(super) rules: Retention<'a>,
}
struct Store<'a, 'conn> {
    tx: &'a mut ObservedTransaction<'conn>,
    namespace: &'a str,
}
impl Store<'_, '_> {
    fn container(&mut self, name: &str) -> Result<BTreeMap<TreeKey, Vec<u8>>, StorageError> {
        let query =
            "SELECT sort_key,value FROM aq_playbooks.state_records WHERE namespace=$1 AND field=$2";
        let rows = self
            .tx
            .query(query, &[&self.namespace, &name])
            .map_err(backend)?;
        let mut current = BTreeMap::new();
        for row in rows {
            let key = decode_key(name, row.get::<_, &[u8]>(0)).map_err(backend)?;
            current.insert(key, row.get(1));
        }
        Ok(current)
    }
}
struct Changes {
    staged: Staged,
    deleted: Fields,
    written: Fields,
    tombstoned: BTreeSet<Vec<u8>>,
    expiry_changes: Vec<ExpiryChange>,
    replaced_old: Containers,
}
impl Request<'_> {
    /// Exact record counts of the patch against the current records (one read of
    /// every touched key), plus the keyed admission immutability check. A
    /// keyed admission or event deletion stays bounded only when the same patch
    /// deletes its expiry record with an expiry `clock_ms` has reached (and, for
    /// an event, writes no legacy tombstone for it); otherwise it sets
    /// `history_removed`, so the complete path runs `verify_removals`. In a
    /// commit that carries an admin licence (`rebase`), a lowered expiry is left
    /// to that path too (it is refused here otherwise).
    pub(super) fn stage(
        &self,
        tx: &mut ObservedTransaction<'_>,
        namespace: &str,
    ) -> Result<Staged, StorageError> {
        let mut staged = Staged {
            full_header: self.patch.header.clone(),
            accounting: self.accounting.clone(),
            replaced: false,
            history_removed: false,
            retired_event_tombstones: vec![],
            created: vec![],
            fields: vec![],
            keys: vec![],
            values: vec![],
            deleted_fields: vec![],
            deleted_keys: vec![],
        };
        staged.accounting.header_len = self.patch.header.len();

        let old = self.read_old(tx, namespace, &mut staged.replaced)?;
        let mut changes = Changes {
            staged,
            deleted: Fields::new(),
            written: Fields::new(),
            tombstoned: BTreeSet::new(),
            expiry_changes: vec![],
            replaced_old: Containers::new(),
        };
        let mut store = Store { tx, namespace };
        for (path, change) in &self.patch.changes {
            let (name, _) = field(path.field).map_err(backend)?;
            match change {
                ContainerChange::Entries(rows) => changes.entries(name, rows, &old)?,
                ContainerChange::Replace(rows) => changes.replace(&mut store, name, rows)?,
            }
        }
        changes.expiries(&self.rules)?;
        changes.deletions(&self.rules);
        Ok(changes.staged)
    }
    fn touched(&self, replaced: &mut bool) -> Result<(Vec<String>, Vec<Vec<u8>>), StorageError> {
        let mut touched_fields = vec![];
        let mut touched_keys = vec![];
        for (path, change) in &self.patch.changes {
            let (name, _) = field(path.field).map_err(backend)?;
            if path.outer.is_some() {
                return Err(backend("nested record container"));
            }
            match change {
                ContainerChange::Entries(rows) => {
                    touched_fields.extend(std::iter::repeat_n(name.to_owned(), rows.len()));
                    touched_keys.extend(rows.iter().map(|(key, _)| encode_key(key)));
                }
                ContainerChange::Replace(_) => *replaced = true,
            }
        }
        Ok((touched_fields, touched_keys))
    }
    fn read_old(
        &self,
        tx: &mut ObservedTransaction<'_>,
        namespace: &str,
        replaced: &mut bool,
    ) -> Result<OldRows, StorageError> {
        let (touched_fields, touched_keys) = self.touched(replaced)?;
        let mut old = OldRows::new();
        if touched_fields.is_empty() {
            return Ok(old);
        }
        let query = "SELECT r.field,r.sort_key,r.value FROM aq_playbooks.state_records r JOIN unnest($2::text[],$3::bytea[]) AS t(f,k) ON r.field=t.f AND r.sort_key=t.k WHERE r.namespace=$1";
        let rows = tx
            .query(query, &[&namespace, &touched_fields, &touched_keys])
            .map_err(backend)?;
        for row in rows {
            old.insert((row.get(0), row.get(1)), row.get(2));
        }
        Ok(old)
    }
}
struct Entry<'a> {
    name: &'static str,
    key: &'a TreeKey,
    encoded: Vec<u8>,
    before: Option<&'a Vec<u8>>,
    new: &'a Option<Vec<u8>>,
}
impl Entry<'_> {
    fn created_identity(&self) -> Option<(String, String)> {
        if self.before.is_some() {
            return None;
        }
        let TreeKey::Str(id) = self.key else {
            return None;
        };
        let (_, kind) = IDENTITIES.iter().find(|(field, _)| *field == self.name)?;
        Some((kind.to_string(), id.clone()))
    }
}
impl Changes {
    fn entries(
        &mut self,
        name: &'static str,
        rows: &[RecordChange],
        old: &OldRows,
    ) -> Result<(), StorageError> {
        let mut seen = BTreeSet::new();
        for (key, new) in rows {
            let encoded = encode_key(key);
            if !seen.insert(encoded.clone()) {
                return Err(backend("duplicate record key in one patch"));
            }
            let before = old.get(&(name.to_string(), encoded.clone()));
            self.entry(Entry {
                name,
                key,
                encoded,
                before,
                new,
            })?;
        }
        Ok(())
    }

    fn entry(&mut self, entry: Entry<'_>) -> Result<(), StorageError> {
        let Entry {
            name,
            ref encoded,
            before,
            new,
            ..
        } = entry;
        self.staged
            .accounting
            .record(name, before.is_some(), new.is_some())
            .map_err(storage)?;
        if name == "receipts" && before.is_some() {
            self.staged.history_removed = true;
        }
        if WINDOWS.iter().any(|(_, expiry)| *expiry == name) && before != new.as_ref() {
            self.expiry_changes
                .push((name, encoded.clone(), before.cloned(), new.clone()));
        }
        if let Some(value) = new {
            self.written.insert((name, encoded.clone()), value.clone());
        }
        verify_change(name, before, new.as_ref())?;
        match new {
            Some(value) => self.write(entry, value),
            None => self.delete(entry),
        }
        Ok(())
    }

    fn write(&mut self, entry: Entry<'_>, value: &[u8]) {
        if let Some(identity) = entry.created_identity() {
            self.staged.created.push(identity);
        }
        let Entry { name, encoded, .. } = entry;
        if name == "retention.event_tombstones" {
            self.tombstoned.insert(encoded.clone());
        }
        self.staged.fields.push(name.to_string());
        self.staged.keys.push(encoded);
        self.staged.values.push(value.to_vec());
    }

    fn delete(&mut self, entry: Entry<'_>) {
        let Entry {
            name,
            key,
            encoded,
            before,
            ..
        } = entry;
        if let Some(before) = before {
            self.staged.history_removed |= HISTORY_REMOVALS.contains(&name);
            if name == "retention.event_tombstones"
                && let TreeKey::Str(id) = key
            {
                self.staged.retired_event_tombstones.push(id.clone());
            }
            self.deleted.insert((name, encoded.clone()), before.clone());
        }
        self.staged.deleted_fields.push(name.to_string());
        self.staged.deleted_keys.push(encoded);
    }
    fn replace(
        &mut self,
        store: &mut Store<'_, '_>,
        name: &'static str,
        rows: &[(TreeKey, Vec<u8>)],
    ) -> Result<(), StorageError> {
        if !self.replaced_old.contains_key(name) {
            let current = store.container(name)?;
            self.replaced_old.insert(name, current);
        }
        let current = &self.replaced_old[name];
        for _ in current {
            self.staged
                .accounting
                .record(name, true, false)
                .map_err(storage)?;
        }
        for _ in rows {
            self.staged
                .accounting
                .record(name, false, true)
                .map_err(storage)?;
        }
        // A replaced container takes the complete path, where removals
        // are checked; a changed receipt is refused here.
        if name == "keyed_admissions"
            && rows
                .iter()
                .any(|(k, v)| current.get(k).is_some_and(|before| before != v))
        {
            return Err(backend(
                "immutable keyed admission receipt removed or changed",
            ));
        }
        Ok(())
    }
}
fn verify_change(
    name: &str,
    before: Option<&Vec<u8>>,
    new: Option<&Vec<u8>>,
) -> Result<(), StorageError> {
    let (Some(before), Some(new)) = (before, new) else {
        return Ok(());
    };
    if before == new {
        return Ok(());
    }
    match name {
        "events" => {
            let before = serde_json::from_slice(before).map_err(backend)?;
            let new = serde_json::from_slice(new).map_err(backend)?;
            retention::check_event_change(&before, &new)?;
        }
        "keyed_admissions" => {
            return Err(backend(
                "immutable keyed admission receipt removed or changed",
            ));
        }
        _ => {}
    }
    Ok(())
}
mod expiry;
