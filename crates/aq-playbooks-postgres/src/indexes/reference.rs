use super::*;

#[derive(serde::Serialize)]
struct Row<'a> {
    kind: &'a str,
    lookup_key: &'a str,
    item_key: &'a str,
    scope: &'a str,
    sequence: u64,
    at_ms: Option<i64>,
}
impl<'a> Row<'a> {
    fn new(kind: &'a str, lookup_key: &'a str, item_key: &'a str) -> Self {
        Self {
            kind,
            lookup_key,
            item_key,
            scope: "",
            sequence: 0,
            at_ms: None,
        }
    }
    fn json(self) -> Value {
        serde_json::to_value(self).unwrap()
    }
}

pub(super) fn native_rows(state: &State) -> Vec<Value> {
    let mut projection = Reference {
        state,
        rows: vec![],
    };
    projection.cursors_and_ready();
    projection.queued_effects();
    projection.completions();
    projection.timers();
    projection.events();
    projection.waiting_runs();
    projection.receipts();
    projection.rows
}
fn run_scope<'a>(state: &'a State, id: &str) -> &'a str {
    state
        .runs
        .get(id)
        .map(|r| r.authority.scope.as_str())
        .unwrap_or("")
}

struct Reference<'a> {
    state: &'a State,
    rows: Vec<Value>,
}
impl Reference<'_> {
    fn cursors_and_ready(&mut self) {
        let idx = &self.state.indexes;
        if let Some(cursor) = &idx.scope_cursor {
            self.rows.push(Row::new("cursor", "ready", cursor).json());
        }
        for (kind, cursor) in &idx.effect_cursor {
            self.rows.push(Row::new("cursor", kind, cursor).json());
        }
        for (kind_scope, cursor) in &idx.effect_item_cursor {
            self.rows
                .push(Row::new("item_cursor", kind_scope, cursor).json());
        }
        for (scope, queue) in &idx.ready {
            if let Some((sequence, id)) = queue.first_key_value() {
                self.rows.push(
                    Row {
                        scope: scope,
                        sequence: *sequence,
                        ..Row::new("ready", "", id)
                    }
                    .json(),
                );
            }
        }
    }
    fn queued_effects(&mut self) {
        let idx = &self.state.indexes;
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
                    self.rows.push(
                        Row {
                            scope: scope,
                            ..Row::new(&format!("effect:{kind}"), "", id)
                        }
                        .json(),
                    );
                }
            }
        }
    }
    fn completions(&mut self) {
        let state = self.state;
        let idx = &self.state.indexes;
        for id in &idx.completion_ready {
            self.rows.push(
                Row {
                    scope: state
                        .intents
                        .get(id)
                        .map(|i| i.authority.scope.as_str())
                        .unwrap_or(""),
                    ..Row::new("effect:completion", "", id)
                }
                .json(),
            );
        }
    }
    fn timers(&mut self) {
        let state = self.state;
        let idx = &self.state.indexes;
        for (key, timer) in &idx.timers {
            self.rows.push(
                Row {
                    scope: run_scope(state, &timer.run),
                    at_ms: Some(timer.at_ms),
                    ..Row::new("timer", "", key)
                }
                .json(),
            );
        }
    }
    fn events(&mut self) {
        let state = self.state;
        let idx = &self.state.indexes;
        for (key, events) in &idx.events {
            for (seq, id) in events {
                self.rows.push(
                    Row {
                        scope: state
                            .events
                            .get(id)
                            .map(|e| e.envelope.scope.as_str())
                            .unwrap_or(""),
                        sequence: *seq,
                        at_ms: state.events.get(id).map(|e| e.accepted_at_ms),
                        ..Row::new("event", key, id)
                    }
                    .json(),
                );
            }
        }
    }
    fn waiting_runs(&mut self) {
        let state = self.state;
        let idx = &self.state.indexes;
        for (key, ids) in &idx.waiters {
            for id in ids {
                self.rows.push(
                    Row {
                        scope: run_scope(state, id),
                        ..Row::new("waiter", key, id)
                    }
                    .json(),
                );
            }
        }
        for id in &idx.runs {
            self.rows.push(
                Row {
                    scope: run_scope(state, id),
                    ..Row::new("run", "", id)
                }
                .json(),
            );
        }
    }
    fn receipts(&mut self) {
        let state = self.state;
        let idx = &self.state.indexes;
        for (run, receipts) in &idx.receipts {
            for (seq, offset) in receipts {
                self.rows.push(
                    Row {
                        sequence: *seq,
                        at_ms: state.receipts.get(*offset).map(|r| r.at_ms),
                        ..Row::new("receipt", run, &crate::projection::receipt_item_key(*seq))
                    }
                    .json(),
                );
            }
        }
        for (seq, offset) in &idx.all_receipts {
            self.rows.push(
                Row {
                    sequence: *seq,
                    at_ms: state.receipts.get(*offset).map(|r| r.at_ms),
                    ..Row::new(
                        "all_receipt",
                        "",
                        &crate::projection::receipt_item_key(*seq),
                    )
                }
                .json(),
            );
        }
    }
}
pub(super) fn public_rows(state: &State) -> Vec<Value> {
    inspection::rows(state).into_iter().map(|row|json!({"kind":row.kind.key(),"item_key":row.key,"run_id":row.run,"sequence":row.sequence,"public_value":row.value})).collect()
}
pub(super) fn delta(
    previous: Vec<Value>,
    next: Vec<Value>,
    keys: &[&str],
) -> (Vec<Value>, Vec<Value>) {
    let keyed = |rows: Vec<Value>| -> std::collections::BTreeMap<Vec<String>, Value> {
        rows.into_iter()
            .map(|row| {
                (
                    keys.iter()
                        .map(|key| row[*key].as_str().expect("internal row key").to_string())
                        .collect(),
                    row,
                )
            })
            .collect()
    };
    let previous = keyed(previous);
    let next = keyed(next);
    let changed = next
        .iter()
        .filter(|(key, row)| previous.get(*key) != Some(*row))
        .map(|(_, row)| row.clone())
        .collect();
    let deleted = previous
        .keys()
        .filter(|key| !next.contains_key(*key))
        .map(|key| {
            let fields: serde_json::Map<_, _> = keys
                .iter()
                .zip(key)
                .map(|(name, value)| ((*name).into(), Value::String(value.clone())))
                .collect();
            Value::Object(fields)
        })
        .collect();
    (changed, deleted)
}
