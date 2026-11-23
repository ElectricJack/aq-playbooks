use super::*;

/// Review I1 and re-review round 1: an operation whose admission handle is
/// lost holds only the events accepted after its dispatch (its completion
/// cannot come earlier). Events accepted before it are never held and stay
/// in the eviction order; events accepted while the handle is lost are held
/// (never evicted, not counted) and rejoin at their acceptance positions
/// once the handle is found.
#[test]
fn uncertain_completion_holds_are_tracked_both_ways() {
    let e = keep_engine(3);
    let (a, completion) = async_artifact();
    install(&e, a, "async");
    let of_contract = |id: &str, op: &str| EventEnvelope {
        correlation: BTreeMap::from([("operation".into(), op.into())]),
        ..async_event(&completion, id)
    };
    let accept = |envelope: EventEnvelope, t: i64| e.admit_event("host", envelope, t).unwrap().key;
    let before: Vec<String> = (0..2)
        .map(|i| accept(of_contract(&format!("before-{i}"), &format!("op-{i}")), i))
        .collect();
    // The dispatch loses the admission handle.
    e.admit("async", "main", Value::Null, authority(), 2)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 2, 10).unwrap();
    assert_eq!(
        order_members(&e.snapshot().unwrap()),
        event_members(&before),
        "events accepted before the dispatch are not held"
    );
    let during: Vec<String> = (0..2)
        .map(|i| {
            accept(
                of_contract(&format!("during-{i}"), &format!("op-d{i}")),
                3 + i,
            )
        })
        .collect();
    let others: Vec<String> = (0..4)
        .map(|i| {
            accept(
                correlated_event(&format!("other-{i}"), &format!("inv-{i}")),
                5 + i,
            )
        })
        .collect();
    let s = e.snapshot().unwrap();
    // Held: never evicted, not counted. The unheld oldest are evicted.
    assert!(during.iter().all(|k| s.events.contains_key(k)));
    assert!(before.iter().all(|k| !s.events.contains_key(k)));
    assert_eq!(order_members(&s), event_members(&others[1..]));
    // The handle is found: the held events rejoin as older than the others
    // and are evicted first.
    accept_handle(&mut host, "reconcile:test.command");
    e.recover(&mut host, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert!(
        during.iter().all(|k| !s.events.contains_key(k)),
        "rejoined and evicted"
    );
    assert_eq!(order_members(&s), event_members(&others[1..]));
}
/// Re-review round 1: an asynchronous dispatch, and the handle that binds
/// right after it, re-judge no event accepted before the dispatch. On bound
/// record containers their work does not depend on how many such events of
/// the completion contract the record holds.
#[test]
fn an_async_dispatch_rejudges_no_earlier_event() {
    use std::sync::atomic::Ordering;
    let measure = |earlier: usize| {
        let e = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
        let (run, _completion) = prior_completions(&e, earlier);
        e.advance(&run, 100).unwrap();
        let order = e.snapshot().unwrap().retention.replay_order.len();
        assert_eq!(order, earlier);
        let reads = e.store.stats.record_reads.load(Ordering::Relaxed);
        let fallbacks = e.store.stats.fallbacks().len();
        let mut host = FakeHost {
            responses: BTreeMap::from([(
                "test.command".into(),
                vec![DispatchResult::Accepted {
                    handle: "server-operation".into(),
                }],
            )]),
            ..Default::default()
        };
        // The dispatch commit, the host call, and the commit binding the
        // handle.
        assert!(e.dispatch_next(&mut host, 101).unwrap().is_some());
        let s = e.snapshot().unwrap();
        assert!(s.intents.values().any(|i| i.handle.is_some()));
        assert_eq!(s.retention.replay_order.len(), earlier);
        assert!(e.store.stats.fallbacks()[fallbacks..].is_empty());
        e.store.stats.record_reads.load(Ordering::Relaxed) - reads
    };
    assert_eq!(measure(10), measure(60));
}

// --- Task 9, first commit: dispatch sequences --------------------------------

/// Task 8b re-review round 2 (Important): a recovery re-dispatch keeps the
/// operation's first dispatch sequence. A completion accepted while the
/// handle was unknown stays held through the recovery (its operation was
/// issued once and may still answer), is not evicted by newer events, and
/// is delivered once the re-dispatch binds the same idempotent handle.
#[test]
fn a_recovery_redispatch_keeps_the_hold_on_a_completion_accepted_while_the_handle_was_unknown() {
    let e = keep_engine(3);
    let (a, completion) = async_artifact_with(Recovery::KeyedIdempotent);
    install(&e, a, "async");
    let run = e
        .admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    // The dispatch loses the admission handle.
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    let first = only_intent(&e.snapshot().unwrap());
    assert_eq!(
        (first.status.clone(), first.handle.clone()),
        (IntentStatus::Unknown, None)
    );
    let dispatched = first.dispatch_sequence.unwrap();
    // Its completion arrives while the handle is unknown: held.
    let done = e
        .admit_event("host", async_event(&completion, "done"), 1)
        .unwrap()
        .key;
    let held = |s: &State| {
        s.events.contains_key(&done) && !order_members(s).contains(&format!("e:{done}"))
    };
    assert!(held(&e.snapshot().unwrap()));
    // Recovery prepares the idempotent operation again.
    e.recover(&mut host, 2).unwrap();
    assert_eq!(
        only_intent(&e.snapshot().unwrap()).status,
        IntentStatus::Prepared
    );
    // More than `replay_keep` newer unheld events.
    for i in 0..5 {
        e.admit_event(
            "host",
            correlated_event(&format!("other-{i}"), &format!("inv-{i}")),
            3 + i,
        )
        .unwrap();
    }
    assert!(held(&e.snapshot().unwrap()), "held through the recovery");
    // The re-dispatch binds the same idempotent handle.
    accept_handle(&mut host, "test.command");
    e.dispatch_next(&mut host, 10).unwrap();
    let again = only_intent(&e.snapshot().unwrap());
    assert_eq!(again.handle.as_deref(), Some("server-operation"));
    assert_eq!(
        again.dispatch_sequence,
        Some(dispatched),
        "the first dispatch is kept"
    );
    for t in 10..20 {
        e.drive(&mut host, t, 20).unwrap();
    }
    let runs = inspect_all(&e, InspectionKind::Runs, Some(&run));
    assert_eq!(runs[0]["outcome"], "ok", "{runs:?}");
}
/// Task 8b re-review round 2 (Minor): an asynchronous operation stopped
/// ("authority revoked") before it was dispatched was never issued, so
/// nothing can complete it: it is not uncertain and holds no event. Events
/// of its contract stay in the eviction order and are evicted by count.
#[test]
fn an_operation_never_dispatched_holds_no_event() {
    let e = keep_engine(3);
    let (a, completion) = async_artifact();
    install(&e, a, "async");
    let run = e
        .admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        revoked: set(&["execute"]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 10).unwrap();
    let s = e.snapshot().unwrap();
    assert_undispatched(&s, &run, &host);
    let keys: Vec<String> = (0..5)
        .map(|i| {
            let envelope = EventEnvelope {
                correlation: BTreeMap::from([("operation".into(), format!("op-{i}"))]),
                ..async_event(&completion, &format!("c{i}"))
            };
            e.admit_event("host", envelope, 1 + i).unwrap().key
        })
        .collect();
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), event_members(&keys[2..]));
    assert!(!s.events.contains_key(&keys[0]) && !s.events.contains_key(&keys[1]));
}
/// Carried hazard (Task 8b re-review round 2): an asynchronous operation
/// dispatched by an engine from before Task 8b has no dispatch sequence.
/// Opening the namespace fills it once from the operation's own first
/// `dispatch` receipt, so it holds exactly the events accepted after its
/// dispatch: a completion accepted while its handle is lost stays held,
/// and the commit that binds the handle judges no earlier event (record
/// reads on bound containers do not depend on how many there are).
#[test]
fn a_legacy_dispatched_operation_gets_its_dispatch_sequence_on_open() {
    use std::sync::atomic::Ordering;
    let measure = |earlier: usize| {
        let store = RecordStorage::new();
        let e = Engine::new(store.clone(), "test", "incarnation-1").unwrap();
        let (run, completion) = prior_completions(&e, earlier);
        let mut host = FakeHost::default();
        e.drive(&mut host, 100, 10).unwrap();
        let (id, dispatched) = as_old_engine(e, &run);
        let e = Engine::new(store, "test", "incarnation-2").unwrap();
        assert_backfill(&e, &id, dispatched, earlier);
        assert_completion_held(&e, &completion);
        accept_handle(&mut host, "reconcile:test.command");
        let reads = e.store.stats.record_reads.load(Ordering::Relaxed);
        let fallbacks = e.store.stats.fallbacks().len();
        e.recover(&mut host, 102).unwrap();
        assert!(e.store.stats.fallbacks()[fallbacks..].is_empty());
        let work = e.store.stats.record_reads.load(Ordering::Relaxed) - reads;
        for t in 102..110 {
            e.drive(&mut host, t, 20).unwrap();
        }
        let runs = inspect_all(&e, InspectionKind::Runs, Some(&run));
        assert_eq!(runs[0]["outcome"], "ok", "{runs:?}");
        work
    };
    assert_eq!(measure(10), measure(60));
}

fn prior_completions(e: &Engine<RecordStorage>, earlier: usize) -> (String, EventContract) {
    let (a, completion) = async_artifact();
    install_on(e, a, "async");
    e.set_retention_policy(RetentionPolicy {
        replay_keep: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..earlier {
        let envelope = EventEnvelope {
            correlation: BTreeMap::from([("operation".into(), format!("op-{i}"))]),
            ..async_event(&completion, &format!("e{i}"))
        };
        e.admit_event("host", envelope, i as i64).unwrap();
    }
    let run = e
        .admit("async", "main", Value::Null, authority(), 100)
        .unwrap();
    (run, completion)
}

fn as_old_engine(e: Engine<RecordStorage>, run: &str) -> (String, u64) {
    // As the old engine stored it: no dispatch sequence.
    let mut legacy = e.snapshot().unwrap();
    let id = only_intent(&legacy).id;
    let dispatched = legacy.intents[&id].dispatch_sequence.take().unwrap();
    legacy.indexes.dispatch_sequences_complete = false;
    // The old engine wrote a `dispatch` receipt for every dispatch; a
    // later one (a recovery re-dispatch) does not move the first.
    legacy.sequence += 1;
    let again = Receipt {
        sequence: legacy.sequence,
        run: run.to_owned(),
        step: "invoke".into(),
        at_ms: 100,
        kind: "dispatch".into(),
        detail: json!({"operation": id}),
    };
    legacy.receipts.push(again);
    let offset = legacy.receipts.len() - 1;
    legacy
        .indexes
        .receipts
        .entry(run.to_owned())
        .or_default()
        .insert(legacy.sequence, offset);
    legacy.indexes.all_receipts.insert(legacy.sequence, offset);
    overwrite(&e.store, "incarnation-1", &legacy);
    e.release().unwrap();
    (id, dispatched)
}

fn assert_backfill(e: &Engine<RecordStorage>, id: &str, dispatched: u64, earlier: usize) {
    let s = e.snapshot().unwrap();
    assert_eq!(s.intents[id].dispatch_sequence, Some(dispatched));
    assert!(s.indexes.dispatch_sequences_complete);
    assert_eq!(
        s.retention.replay_order.len(),
        earlier,
        "earlier events are not held"
    );
}

fn assert_completion_held(e: &Engine<RecordStorage>, completion: &EventContract) {
    let done = e
        .admit_event("host", async_event(completion, "done"), 101)
        .unwrap()
        .key;
    assert!(
        !order_members(&e.snapshot().unwrap()).contains(&format!("e:{done}")),
        "held"
    );
}

fn assert_undispatched(s: &State, run: &str, host: &FakeHost) {
    let op = only_intent(s);
    assert_eq!(
        (op.status, op.handle, op.dispatch_sequence),
        (IntentStatus::Unknown, None, None)
    );
    assert_eq!(s.runs[run].status, RunStatus::Blocked);
    assert!(host.dispatched.is_empty());
    assert!(
        s.indexes
            .uncertain_completion_contracts
            .values()
            .all(BTreeSet::is_empty)
    );
}

fn event_members(keys: &[String]) -> Vec<String> {
    keys.iter().map(|key| format!("e:{key}")).collect()
}
fn accept_handle(host: &mut FakeHost, command: &str) {
    host.responses.insert(
        command.into(),
        vec![DispatchResult::Accepted {
            handle: "server-operation".into(),
        }],
    );
}
