//! Borrowed runtime index rows used to build the PostgreSQL change set.
use super::*;
use aq_playbooks_runner::RuntimeIndexes;
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct NativeKey<'a>(Cow<'a, str>, Cow<'a, str>, Cow<'a, str>);
#[derive(Debug, PartialEq, Eq)]
struct NativeFields<'a> {
    scope: Cow<'a, str>,
    sequence: u64,
    at: Option<i64>,
}
#[derive(Default)]
struct NativeRows<'a>(BTreeMap<NativeKey<'a>, NativeFields<'a>>);

impl<'a> NativeKey<'a> {
    fn new(
        kind: impl Into<Cow<'a, str>>,
        lookup: impl Into<Cow<'a, str>>,
        key: impl Into<Cow<'a, str>>,
    ) -> Self {
        Self(kind.into(), lookup.into(), key.into())
    }
}
impl<'a> NativeFields<'a> {
    fn new(scope: impl Into<Cow<'a, str>>, sequence: u64, at: Option<i64>) -> Self {
        Self {
            scope: scope.into(),
            sequence,
            at,
        }
    }
}

fn native_json(k: &NativeKey<'_>, f: &NativeFields<'_>) -> Value {
    json!({"kind":k.0,"lookup_key":k.1,"item_key":k.2,"scope":f.scope,"sequence":f.sequence,"at_ms":f.at})
}
pub(crate) fn native_delta(previous: &State, next: &State) -> (Vec<Value>, Vec<Value>) {
    let previous = native_rows(previous);
    let next = native_rows(next);
    let changed = next
        .0
        .iter()
        .filter(|(key, row)| previous.0.get(*key) != Some(*row))
        .map(|(key, row)| native_json(key, row))
        .collect();
    let deleted = previous
        .0
        .keys()
        .filter(|key| !next.0.contains_key(*key))
        .map(|key| json!({"kind":key.0,"lookup_key":key.1,"item_key":key.2}))
        .collect();
    (changed, deleted)
}
fn native_rows(state: &State) -> NativeRows<'_> {
    let mut rows = NativeRows::default();
    let idx = &state.indexes;
    rows.cursors(idx);
    rows.ready(idx);
    rows.effects(state);
    rows.timers(state);
    rows.events(state);
    rows.runs(state);
    rows.receipts(state);
    rows
}

impl<'a> NativeRows<'a> {
    fn cursors(&mut self, idx: &'a RuntimeIndexes) {
        if let Some(cursor) = &idx.scope_cursor {
            self.0.insert(
                NativeKey::new("cursor", "ready", cursor),
                NativeFields::new("", 0, None),
            );
        }
        for (kind, cursor) in &idx.effect_cursor {
            self.0.insert(
                NativeKey::new("cursor", kind, cursor),
                NativeFields::new("", 0, None),
            );
        }
        for (kind_scope, cursor) in &idx.effect_item_cursor {
            self.0.insert(
                NativeKey::new("item_cursor", kind_scope, cursor),
                NativeFields::new("", 0, None),
            );
        }
    }
    fn ready(&mut self, idx: &'a RuntimeIndexes) {
        for (scope, queue) in &idx.ready {
            if let Some((sequence, id)) = queue.first_key_value() {
                self.0.insert(
                    NativeKey::new("ready", "", id),
                    NativeFields::new(scope, *sequence, None),
                );
            }
        }
    }
    fn effects(&mut self, state: &'a State) {
        let idx = &state.indexes;
        for (kind, scopes) in &idx.effects {
            for (scope, ids) in scopes {
                let cursor = idx.effect_item_cursor.get(&format!("{kind}:{scope}"));
                let selected = cursor
                    .and_then(|cursor| {
                        ids.range((
                            std::ops::Bound::Excluded(cursor.clone()),
                            std::ops::Bound::Unbounded,
                        ))
                        .next()
                    })
                    .or_else(|| ids.first());
                if let Some(id) = selected {
                    self.0.insert(
                        NativeKey::new(format!("effect:{kind}"), "", id),
                        NativeFields::new(scope, 0, None),
                    );
                }
            }
        }
        for id in &idx.completion_ready {
            self.0.insert(
                NativeKey::new("effect:completion", "", id),
                NativeFields::new(
                    state
                        .intents
                        .get(id)
                        .map(|i| i.authority.scope.as_str())
                        .unwrap_or(""),
                    0,
                    None,
                ),
            );
        }
    }
    fn timers(&mut self, state: &'a State) {
        let idx = &state.indexes;
        for (key, timer) in &idx.timers {
            self.0.insert(
                NativeKey::new("timer", "", key),
                NativeFields::new(
                    state
                        .runs
                        .get(&timer.run)
                        .map(|r| r.authority.scope.as_str())
                        .unwrap_or(""),
                    0,
                    Some(timer.at_ms),
                ),
            );
        }
    }
    fn events(&mut self, state: &'a State) {
        let idx = &state.indexes;
        for (key, events) in &idx.events {
            for (seq, id) in events {
                self.0.insert(
                    NativeKey::new("event", key, id),
                    NativeFields::new(
                        state
                            .events
                            .get(id)
                            .map(|e| e.envelope.scope.as_str())
                            .unwrap_or(""),
                        *seq,
                        state.events.get(id).map(|e| e.accepted_at_ms),
                    ),
                );
            }
        }
    }
    fn runs(&mut self, state: &'a State) {
        let idx = &state.indexes;
        for (key, ids) in &idx.waiters {
            for id in ids {
                self.0.insert(
                    NativeKey::new("waiter", key, id),
                    NativeFields::new(
                        state
                            .runs
                            .get(id)
                            .map(|r| r.authority.scope.as_str())
                            .unwrap_or(""),
                        0,
                        None,
                    ),
                );
            }
        }
        for id in &idx.runs {
            self.0.insert(
                NativeKey::new("run", "", id),
                NativeFields::new(
                    state
                        .runs
                        .get(id)
                        .map(|r| r.authority.scope.as_str())
                        .unwrap_or(""),
                    0,
                    None,
                ),
            );
        }
    }
    fn receipts(&mut self, state: &'a State) {
        let idx = &state.indexes;
        // Receipt rows are keyed by the receipt's sequence, its stable identity:
        // a commit that compacts `receipts` (a cohort leaving) moves receipts to
        // new positions but changes none of their rows.
        for (run, receipts) in &idx.receipts {
            for (seq, offset) in receipts {
                self.0.insert(
                    NativeKey::new("receipt", run, receipt_item_key(*seq)),
                    NativeFields::new("", *seq, state.receipts.get(*offset).map(|r| r.at_ms)),
                );
            }
        }
        for (seq, offset) in &idx.all_receipts {
            self.0.insert(
                NativeKey::new("all_receipt", "", receipt_item_key(*seq)),
                NativeFields::new("", *seq, state.receipts.get(*offset).map(|r| r.at_ms)),
            );
        }
    }
}
