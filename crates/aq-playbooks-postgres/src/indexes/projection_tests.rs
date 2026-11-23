use super::*;
use aq_playbooks_runner::{Intent, Receipt, Run, indexes::TimerIndex};
use std::collections::{BTreeMap, BTreeSet};
fn fixture() -> State {
    let run:Run=serde_json::from_value(json!({"catalog_revision":0,"id":"run-a","root":"run-a","parent":null,"origin":null,"artifact":"artifact","entry":"main","dependencies":{},"authority":{"actor":"actor","scope":"a","capabilities":[]},"step":"return","status":"completed","input":"private","variables":{},"outcome":"ok","output":null,"reason":null,"admitted_at_ms":1,"visits":1,"depth":0,"children":[],"pending":null,"loops":[],"attempts":{},"pending_return":null,"pending_failure":false,"terminal_sequence":2})).unwrap();
    let intent:Intent=serde_json::from_value(json!({"id":"intent-1","run":"run-a","step":"invoke","invocation":1,"deadline_at_ms":1000,"command":{"name":"command","version":"1","input":{"type":"null"},"outcomes":{"ok":{"type":"null"}},"capabilities":[],"scopes":["a"],"effect":"mutation","recovery":"keyed_idempotent","asynchronous":null,"cancellation":false,"public_result_fields":[],"sensitive_input_fields":[],"sensitive_output_fields":[],"max_payload_bytes":1024,"deadline_ms":1000},"input":null,"input_digest":"private","authority":{"actor":"actor","scope":"a","capabilities":[]},"status":"prepared","cancel_requested":false,"cancel_dispatched":false,"handle":null,"result":null,"reason":null,"late_results":[],"completion_event":null})).unwrap();
    let mut s = State::default();
    s.runs.insert(run.id.clone(), run);
    s.intents.insert(intent.id.clone(), intent.clone());
    let mut second = intent;
    second.id = "intent-2".into();
    s.intents.insert(second.id.clone(), second);
    s.receipts = vec![
        Receipt {
            sequence: 1,
            run: "run-a".into(),
            step: "first".into(),
            at_ms: 1,
            kind: "intent".into(),
            detail: json!({"operation":"intent-1","private":"secret"}),
        },
        Receipt {
            sequence: 10,
            run: "run-a".into(),
            step: "second".into(),
            at_ms: 10,
            kind: "unclassified".into(),
            detail: json!({"private":"secret"}),
        },
        Receipt {
            sequence: 2,
            run: "run-a".into(),
            step: "third".into(),
            at_ms: 2,
            kind: "terminal".into(),
            detail: json!({"status":"completed","outcome":"ok"}),
        },
    ]
    .into();
    fixture_indexes(&mut s.indexes);
    s
}
fn fixture_indexes(idx: &mut aq_playbooks_runner::RuntimeIndexes) {
    idx.scope_cursor = Some("a".into());
    idx.effect_cursor.insert("dispatch".into(), "b".into());
    idx.effect_item_cursor
        .insert("dispatch:a".into(), "intent-1".into());
    idx.ready
        .insert("a".into(), BTreeMap::from([(1, "run-a".into())]));
    idx.effects.insert(
        "dispatch".into(),
        BTreeMap::from([(
            "a".into(),
            BTreeSet::from(["intent-1".into(), "intent-2".into()]),
        )]),
    );
    idx.completion_ready.insert("intent-2".into());
    idx.timers.insert(
        "timer".into(),
        TimerIndex {
            key: "timer".into(),
            run: "run-a".into(),
            at_ms: 100,
            intent: None,
        },
    );
    idx.events.insert(
        "correlation".into(),
        BTreeMap::from([(2, "event-1".into())]),
    );
    idx.waiters
        .insert("correlation".into(), BTreeSet::from(["run-a".into()]));
    idx.runs.insert("run-a".into());
    idx.receipts
        .insert("run-a".into(), BTreeMap::from([(1, 0), (10, 1), (2, 2)]));
    idx.all_receipts = BTreeMap::from([(1, 0), (10, 1), (2, 2)]).into();
}

fn equivalent(previous: &State, next: &State) {
    assert_eq!(
        crate::projection::native_delta(previous, next),
        delta(
            native_rows(previous),
            native_rows(next),
            &["kind", "lookup_key", "item_key"]
        )
    );
    assert_eq!(
        crate::projection::public_delta(previous, next),
        delta(
            public_rows(previous),
            public_rows(next),
            &["kind", "item_key"]
        )
    );
}
#[test]
fn borrowed_deltas_match_reference_for_empty_unchanged_and_materialization() {
    let s = fixture();
    equivalent(&State::default(), &s);
    equivalent(&s, &s);
    equivalent(&s, &State::default());
}
#[test]
fn borrowed_deltas_match_reference_for_scopes_removals_offsets_and_private_edits() {
    let old = fixture();
    for case in 0..9 {
        let mut next = old.clone();
        match case {
            0 => next.runs.get_mut("run-a").unwrap().authority.scope = "changed".into(),
            1 => {
                next.runs.clear();
            }
            2 => {
                next.receipts.remove(0);
            }
            3 => next.receipts.reverse(),
            4 => {
                next.runs.get_mut("run-a").unwrap().input = json!("private changed");
                next.receipts[1].detail = json!({"private":"different secret"});
            }
            5 => {
                next.intents.get_mut("intent-1").unwrap().invocation += 1;
            }
            6 => {
                next.indexes
                    .effect_item_cursor
                    .insert("dispatch:a".into(), "intent-2".into());
            }
            7 => {
                next.runs.get_mut("run-a").unwrap().reason = Some("private reason".into());
            }
            8 => {
                next.indexes.all_receipts.insert(100, 0);
                next.indexes
                    .receipts
                    .get_mut("run-a")
                    .unwrap()
                    .insert(100, 0);
            }
            _ => unreachable!(),
        }
        equivalent(&old, &next);
    }
}
/// `runs` live runs of `per_run` receipts each, in sequence order, with
/// the receipt indexes as the engine keeps them (positions in
/// `receipts`).
fn receipts_state(runs: usize, per_run: usize) -> State {
    let mut s = State::default();
    let mut sequence = 0;
    for run in 0..runs {
        for _ in 0..per_run {
            sequence += 1;
            s.receipts.push(Receipt {
                sequence,
                run: format!("run-{run:03}"),
                step: "s".into(),
                at_ms: sequence as i64,
                kind: "admitted".into(),
                detail: json!({}),
            });
        }
    }
    reindex_receipts(&mut s);
    s
}
fn reindex_receipts(s: &mut State) {
    s.indexes.receipts.clear();
    s.indexes.all_receipts.clear();
    let live: Vec<(String, u64)> = s
        .receipts
        .iter()
        .map(|r| (r.run.clone(), r.sequence))
        .collect();
    for (offset, (run, sequence)) in live.into_iter().enumerate() {
        s.indexes
            .receipts
            .entry(run)
            .or_default()
            .insert(sequence, offset);
        s.indexes.all_receipts.insert(sequence, offset);
    }
}
/// The rows of receipt kinds a commit changes or deletes.
fn receipt_rows(previous: &State, next: &State) -> (usize, usize) {
    let receipt = |row: &&Value| matches!(row["kind"].as_str(), Some("receipt" | "all_receipt"));
    let (changed, deleted) = crate::projection::native_delta(previous, next);
    (
        changed.iter().filter(receipt).count(),
        deleted.iter().filter(receipt).count(),
    )
}
/// Task 9 review I1: removing the oldest cohort's receipts compacts
/// `receipts`, which moves every later receipt to a new position. The
/// runtime-index rows of receipts are keyed by sequence, so the commit
/// writes the rows of the receipts it removes or adds, not of every
/// receipt that moved.
#[test]
fn receipt_rows_change_only_for_the_receipts_a_commit_removes_or_adds() {
    for runs in [10, 40] {
        let previous = receipts_state(runs, 5);
        let mut next = previous.clone();
        next.receipts.retain(|r| r.run != "run-000");
        reindex_receipts(&mut next);
        // 5 receipts, one `receipt` and one `all_receipt` row each.
        assert_eq!(receipt_rows(&previous, &next), (0, 10), "{runs} runs");
        next.receipts.push(Receipt {
            sequence: 10_000,
            run: "run-001".into(),
            step: "s".into(),
            at_ms: 10_000,
            kind: "admitted".into(),
            detail: json!({}),
        });
        reindex_receipts(&mut next);
        assert_eq!(receipt_rows(&previous, &next), (2, 10), "{runs} runs");
    }
}
#[test]
fn borrowed_deltas_preserve_last_duplicate_identity_and_redaction_semantics() {
    let old = fixture();
    let mut next = old.clone();
    let mut duplicate = next.runs["run-a"].clone();
    duplicate.status = aq_playbooks_runner::RunStatus::Failed;
    next.runs.insert("zz-alias".into(), duplicate);
    let mut receipt = next.receipts[0].clone();
    receipt.detail = json!({"operation":"new","secret":"never project"});
    next.receipts.push(receipt);
    next.indexes
        .ready
        .insert("z".into(), BTreeMap::from([(999, "run-a".into())]));
    next.indexes.effects.insert(
        "completion".into(),
        BTreeMap::from([("a".into(), BTreeSet::from(["intent-2".into()]))]),
    );
    equivalent(&old, &next);
    let (changed, _) = crate::projection::public_delta(&old, &next);
    assert!(
        !serde_json::to_string(&changed)
            .unwrap()
            .contains("never project")
    );
}
