//! Validate changed records before an incremental storage commit.
use super::*;
use crate::retention::{self, Window};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::collections::BTreeSet;

const FENCED: &[&str] = &["runs", "intents", "events", "keyed_admissions"];
type Touch<'a> = (&'a TreeKey, Option<&'a Vec<u8>>, Option<&'a Vec<u8>>);

fn decode<T: DeserializeOwned>(field: &str, bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|e| backend(format!("{field} record: {e}")))
}

fn written(change: &ContainerChange) -> impl Iterator<Item = (&TreeKey, &Vec<u8>)> {
    let (entries, replacement) = match change {
        ContainerChange::Entries(rows) => (rows.as_slice(), [].as_slice()),
        ContainerChange::Replace(rows) => ([].as_slice(), rows.as_slice()),
    };
    entries
        .iter()
        .filter_map(|(key, value)| Some((key, value.as_ref()?)))
        .chain(replacement.iter().map(|(key, value)| (key, value)))
}

fn changed_records(
    data: &RecordData,
    patch: &StatePatch,
    field: &str,
    mut check: impl FnMut(&[u8], &[u8]) -> Result<()>,
) -> Result<()> {
    let Some(old) = data.container(field) else {
        return Ok(());
    };
    for (_, change) in patch.changes.iter().filter(|(path, _)| path.field == field) {
        for (key, new) in written(change) {
            if let Some(before) = old.get(key)
                && before != new
            {
                check(before, new)?;
            }
        }
    }
    Ok(())
}

/// Verify the identity of every stored event the patch rewrites.
pub(super) fn verify_events(data: &RecordData, patch: &StatePatch) -> Result<()> {
    changed_records(data, patch, "events", |before, new| {
        Ok(retention::check_event_change(
            &decode("events", before)?,
            &decode("events", new)?,
        )?)
    })
}

/// A receipt must keep the bytes originally bound to its admission key.
pub(super) fn verify_keyed_admissions(data: &RecordData, patch: &StatePatch) -> Result<()> {
    changed_records(data, patch, "keyed_admissions", |_, _| {
        Err(backend(
            "immutable keyed admission receipt removed or changed",
        ))
    })
}

fn touched<'a>(old: Option<&'a Container>, change: &'a ContainerChange) -> Vec<Touch<'a>> {
    match change {
        ContainerChange::Entries(rows) => rows
            .iter()
            .map(|(key, new)| (key, old.and_then(|c| c.get(key)), new.as_ref()))
            .collect(),
        ContainerChange::Replace(rows) => {
            let new: BTreeMap<_, _> = rows.iter().map(|(k, v)| (k, v)).collect();
            let keys: BTreeSet<_> = old
                .into_iter()
                .flatten()
                .map(|(k, _)| k)
                .chain(new.keys().copied())
                .collect();
            keys.into_iter()
                .map(|key| (key, old.and_then(|c| c.get(key)), new.get(key).copied()))
                .collect()
        }
    }
}

/// Load only expiry and removal records needed to validate this commit.
pub(super) fn verify_patch_removals(
    data: &RecordData,
    patch: &StatePatch,
    next: &State,
) -> Result<()> {
    let caps = licence_caps(data, patch, next)?;
    let mut removed = Removals::default();
    for (path, change) in &patch.changes {
        let field = field_name(path)?;
        let rows = touched(data.container(field), change);
        match field {
            "retention.event_expires_at_ms" => Expiry {
                window: Window::Event,
                field,
                caps: &caps,
                next,
            }
            .check(rows)?,
            "retention.admission_expires_at_ms" => Expiry {
                window: Window::Admission,
                field,
                caps: &caps,
                next,
            }
            .check(rows)?,
            "receipts" => removed.receipts(rows)?,
            _ if FENCED.contains(&field) => removed.records(field, rows)?,
            _ => {}
        }
    }
    let previous = removed.previous(data)?;
    let evicted = evictions(data, patch, &previous, next)?;
    retention::check_removals(&previous, next, &|_| false, &|window, key| {
        evicted.contains(&(window, key.to_string()))
    })?;
    Ok(())
}

fn licence_caps(data: &RecordData, patch: &StatePatch, next: &State) -> Result<State> {
    if retention::licence(next)?.is_none() {
        return Ok(State::default());
    }
    let mut caps = header_state(&data.header)?;
    retention::check_licensed_header(&caps, next)?;
    for (path, change) in &patch.changes {
        let field = field_name(path)?;
        if FENCED.contains(&field)
            && written(change).any(|(key, _)| {
                data.container(field)
                    .is_none_or(|old| !old.contains_key(key))
            })
        {
            return Err(backend(retention::REFUSE_LICENCE));
        }
    }
    if let Some(stored) = data.container("events") {
        load_expiring_events(&mut caps, patch, stored)?;
    }
    Ok(caps)
}

fn load_expiring_events(caps: &mut State, patch: &StatePatch, stored: &Container) -> Result<()> {
    for (path, change) in &patch.changes {
        if field_name(path)? != "retention.event_expires_at_ms" {
            continue;
        }
        load_cap_events(caps, stored, change)?;
    }
    Ok(())
}

fn load_cap_events(caps: &mut State, stored: &Container, change: &ContainerChange) -> Result<()> {
    let keys: Vec<_> = match change {
        ContainerChange::Entries(rows) => rows.iter().map(|(key, _)| key).collect(),
        ContainerChange::Replace(rows) => rows.iter().map(|(key, _)| key).collect(),
    };
    for key in keys {
        if let (TreeKey::Str(id), Some(bytes)) = (key, stored.get(key)) {
            caps.events.insert(id.clone(), decode("events", bytes)?);
        }
    }
    Ok(())
}

struct Expiry<'a> {
    window: Window,
    field: &'a str,
    caps: &'a State,
    next: &'a State,
}
impl Expiry<'_> {
    fn check(&self, rows: Vec<Touch<'_>>) -> Result<()> {
        for (key, before, after) in rows {
            let id = string_key(self.field, key)?;
            let before = before.map(|b| decode(self.field, b)).transpose()?;
            let after = after.map(|b| decode(self.field, b)).transpose()?;
            if before != after {
                retention::ExpiryChange {
                    window: self.window,
                    key: id,
                    old: before,
                    new: after,
                }
                .check(self.caps, self.next)?;
            }
        }
        Ok(())
    }
}

fn string_key<'a>(field: &str, key: &'a TreeKey) -> Result<&'a str> {
    match key {
        TreeKey::Str(id) => Ok(id),
        _ => Err(backend(format!("{field}: unexpected record key"))),
    }
}

#[derive(Default)]
struct Removals {
    by_field: BTreeMap<&'static str, Map<String, Value>>,
    receipts: Vec<Value>,
}
impl Removals {
    fn records(&mut self, field: &'static str, rows: Vec<Touch<'_>>) -> Result<()> {
        for (key, before, after) in rows {
            if let (Some(before), None) = (before, after) {
                let id = string_key(field, key)?;
                self.by_field
                    .entry(field)
                    .or_default()
                    .insert(id.to_owned(), decode(field, before)?);
            }
        }
        Ok(())
    }

    fn receipts(&mut self, rows: Vec<Touch<'_>>) -> Result<()> {
        let mut written = BTreeSet::new();
        let mut stored = Vec::new();
        for (_, before, after) in rows {
            if let Some(after) = after {
                written.insert(decode::<Value>("receipts", after)?["sequence"].as_u64());
            }
            if let Some(before) = before {
                stored.push(decode::<Value>("receipts", before)?);
            }
        }
        self.receipts.extend(
            stored
                .into_iter()
                .filter(|r| !written.contains(&r["sequence"].as_u64())),
        );
        Ok(())
    }

    fn previous(self, data: &RecordData) -> Result<State> {
        let header = header_state(&data.header)?;
        if self.by_field.is_empty() && self.receipts.is_empty() {
            return Ok(header);
        }
        let mut value = serde_json::to_value(header).map_err(backend)?;
        for (&field, records) in &self.by_field {
            load_expiries(data, &mut value, field, records)?;
            value[field] = Value::Object(records.clone());
        }
        value["receipts"] = Value::Array(self.receipts);
        if let Some(runs) = self.by_field.get("runs") {
            value["runs"] = Value::Object(with_children(data, runs)?);
            let removed = self.by_field.get("intents").cloned().unwrap_or_default();
            value["intents"] = Value::Object(with_intents(data, runs, removed)?);
        }
        serde_json::from_value(value).map_err(|e| backend(format!("removed records: {e}")))
    }
}

fn load_expiries(
    data: &RecordData,
    value: &mut Value,
    field: &str,
    records: &Map<String, Value>,
) -> Result<()> {
    let (name, path) = match field {
        "events" => ("event_expires_at_ms", "retention.event_expires_at_ms"),
        "keyed_admissions" => (
            "admission_expires_at_ms",
            "retention.admission_expires_at_ms",
        ),
        _ => return Ok(()),
    };
    for key in records.keys() {
        if let Some(bytes) = data
            .container(path)
            .and_then(|c| c.get(&TreeKey::Str(key.clone())))
        {
            value["retention"][name][key] = decode(path, bytes)?;
        }
    }
    Ok(())
}

fn with_children(data: &RecordData, runs: &Map<String, Value>) -> Result<Map<String, Value>> {
    let mut all = runs.clone();
    let stored = data.container("runs");
    let children = runs
        .values()
        .filter_map(|run| run["children"].as_array())
        .flatten()
        .filter_map(Value::as_str);
    for child in children {
        if let Some(bytes) = stored.and_then(|c| c.get(&TreeKey::Str(child.into()))) {
            all.insert(child.into(), decode("runs", bytes)?);
        }
    }
    Ok(all)
}

fn with_intents(
    data: &RecordData,
    runs: &Map<String, Value>,
    mut intents: Map<String, Value>,
) -> Result<Map<String, Value>> {
    for (key, bytes) in data.container("intents").into_iter().flatten() {
        let intent: Value = decode("intents", bytes)?;
        if intent["run"]
            .as_str()
            .is_some_and(|run| runs.contains_key(run))
            && let TreeKey::Str(id) = key
        {
            intents.insert(id.clone(), intent);
        }
    }
    Ok(intents)
}

fn evictions(
    data: &RecordData,
    patch: &StatePatch,
    previous: &State,
    next: &State,
) -> Result<BTreeSet<(Window, String)>> {
    let stored = data.container("retention.replay_order");
    let mut added = Vec::new();
    for (_, change) in patch
        .changes
        .iter()
        .filter(|(path, _)| path.field == "retention.replay_order")
    {
        for (key, _) in written(change) {
            if stored.is_none_or(|c| !c.contains_key(key))
                && let TreeKey::Str(id) = key
            {
                added.push(id.clone());
            }
        }
    }
    Ok(retention::check_evictions(
        previous,
        next,
        &mut added.into_iter(),
        &retention::Stored {
            ordered: &|key| Ok(stored.is_some_and(|c| c.contains_key(&TreeKey::Str(key.into())))),
            memberships: &|key| {
                stored_index(data, "indexes.event_memberships", key, "event memberships")
            },
            waiters: &|key| stored_index(data, "indexes.waiters", key, "waiters"),
        },
    )?)
}

fn stored_index<T: DeserializeOwned>(
    data: &RecordData,
    field: &str,
    key: &str,
    label: &str,
) -> std::result::Result<Option<T>, StorageError> {
    data.container(field)
        .and_then(|c| c.get(&TreeKey::Str(key.into())))
        .map(|bytes| {
            serde_json::from_slice(bytes)
                .map_err(|e| StorageError::Backend(format!("{label} record: {e}")))
        })
        .transpose()
}
