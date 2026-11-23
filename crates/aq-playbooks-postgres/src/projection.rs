//! Borrowed projection comparison. Unchanged records never materialize public
//! JSON or clone stored native row strings. Temporary maps remain O(history).
//! Changed-source candidates use canonical public sanitizers before actual
//! changed/deleted rows enter SQL. This is not O(1) persistence.
use aq_playbooks_runner::{Intent, Receipt, Run, State, inspection};
use serde_json::{Value, json};
use std::{borrow::Cow, collections::BTreeMap};
mod native;
pub(crate) use native::native_delta;

/// The `item_key` of a receipt's runtime-index rows: its sequence, 20
/// zero-padded digits. An engine before this keyed them by the receipt's
/// position in the record (an unpadded number, never 20 digits), so the two
/// keyings never collide; such rows are deleted by sequence when their
/// receipt leaves ([`crate::indexes::sync`]).
pub(crate) fn receipt_item_key(sequence: u64) -> String {
    format!("{sequence:020}")
}
/// Whether `kind` is a receipt kind of the runtime index.
pub(crate) fn receipt_kind(kind: &str) -> bool {
    matches!(kind, "receipt" | "all_receipt")
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct PublicKey<'a>(&'static str, Cow<'a, str>);
enum Source<'a> {
    Run(&'a Run),
    Intent(&'a Intent),
    Receipt(&'a Receipt),
}
struct PublicRow<'a> {
    run: &'a str,
    sequence: u64,
    source: Source<'a>,
}
fn public_rows(state: &State) -> BTreeMap<PublicKey<'_>, PublicRow<'_>> {
    let mut rows = BTreeMap::new();
    for r in state.runs.values() {
        rows.insert(
            PublicKey("runs", Cow::Borrowed(&r.id)),
            PublicRow {
                run: &r.id,
                sequence: 0,
                source: Source::Run(r),
            },
        );
    }
    for i in state.intents.values() {
        rows.insert(
            PublicKey("effects", Cow::Borrowed(&i.id)),
            PublicRow {
                run: &i.run,
                sequence: i.invocation,
                source: Source::Intent(i),
            },
        );
    }
    for r in &state.receipts {
        rows.insert(
            PublicKey("receipts", Cow::Owned(receipt_item_key(r.sequence))),
            PublicRow {
                run: &r.run,
                sequence: r.sequence,
                source: Source::Receipt(r),
            },
        );
    }
    rows
}
impl PublicRow<'_> {
    fn unchanged(&self, old: &PublicRow<'_>) -> bool {
        self.run == old.run && self.sequence == old.sequence && self.source.same(&old.source)
    }
}

impl Source<'_> {
    fn same(&self, other: &Source<'_>) -> bool {
        match (self, other) {
            (Self::Run(a), Source::Run(b)) => {
                run_fields(a) == run_fields(b) && same_pending(&a.pending, &b.pending)
            }
            (Self::Intent(a), Source::Intent(b)) => intent_fields(a) == intent_fields(b),
            (Self::Receipt(a), Source::Receipt(b)) => receipt_fields(a) == receipt_fields(b),
            _ => false,
        }
    }
}

// Compare only the input fields used by the public sanitizers, without cloning
// event payloads, results or other private state into temporary JSON.
fn run_fields(r: &Run) -> impl PartialEq + '_ {
    (
        (&r.id, &r.artifact, &r.entry, &r.step),
        (
            &r.status,
            &r.outcome,
            r.reason.is_some(),
            r.visits,
            r.admitted_at_ms,
        ),
        (&r.root, &r.parent, &r.origin, &r.children, &r.dependencies),
    )
}
fn intent_fields(i: &Intent) -> impl PartialEq + '_ {
    (
        (&i.id, &i.run, &i.step, &i.command),
        (
            &i.status,
            &i.handle,
            i.reason.is_some(),
            i.deadline_at_ms,
            i.cancel_requested,
            i.cancel_dispatched,
        ),
    )
}
fn receipt_fields(r: &Receipt) -> impl PartialEq + '_ {
    (r.sequence, &r.run, &r.step, r.at_ms, &r.kind, &r.detail)
}
fn same_pending(
    a: &Option<aq_playbooks_runner::Pending>,
    b: &Option<aq_playbooks_runner::Pending>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            matches!((serde_json::to_value(a),serde_json::to_value(b)),(Ok(a),Ok(b)) if a == b)
        }
        _ => false,
    }
}

fn public_json(k: &PublicKey<'_>, r: &PublicRow<'_>) -> Value {
    let value = match r.source {
        Source::Run(run) => inspection::public_run(run),
        Source::Intent(intent) => inspection::public_intent(intent),
        Source::Receipt(receipt) => inspection::public_receipt(receipt),
    };
    json!({"kind":k.0,"item_key":k.1,"run_id":r.run,"sequence":r.sequence,"public_value":value})
}
pub(crate) fn public_delta(previous: &State, next: &State) -> (Vec<Value>, Vec<Value>) {
    let previous = public_rows(previous);
    let next = public_rows(next);
    let mut changed = vec![];
    for (key, row) in &next {
        if let Some(old) = previous.get(key) {
            if row.unchanged(old) {
                continue;
            }
            let new = public_json(key, row);
            if public_json(key, old) != new {
                changed.push(new)
            }
        } else {
            changed.push(public_json(key, row))
        }
    }
    let deleted = previous
        .keys()
        .filter(|key| !next.contains_key(*key))
        .map(|key| json!({"kind":key.0,"item_key":key.1}))
        .collect();
    (changed, deleted)
}
