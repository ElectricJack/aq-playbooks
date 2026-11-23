use super::*;

#[test]
fn replay_keep_must_be_positive() {
    let e = engine();
    assert!(matches!(
        e.set_retention_policy(RetentionPolicy {
            replay_keep: 0,
            ..RetentionPolicy::default()
        }),
        Err(Error::Refused(m)) if m == "replay_keep must be positive"
    ));
    assert_eq!(RetentionPolicy::default().replay_keep, 256);
    let stored: RetentionPolicy =
        serde_json::from_value(json!({"event_replay_ms": 1, "audit_ms": 1})).unwrap();
    assert_eq!((stored.replay_keep, stored.history_keep), (256, 10_000));
    // A State written before the rolling bounds decodes with an empty order.
    let legacy: State = serde_json::from_value(json!({
        "artifacts": {}, "bindings": {}, "catalog_revision": 0, "runs": {},
        "receipts": [], "intents": {}, "waits": {}, "events": {}, "budgets": {},
        "sequence": 0,
    }))
    .unwrap();
    assert!(legacy.retention.replay_order.is_empty() && legacy.retention.replay_held.is_empty());
}
/// With `replay_keep` 4: a duplicate inside the rolling set is recognised;
/// the 5th distinct unheld key evicts the 1st (oldest first, its window not
/// passed); a re-send of the 1st is then accepted as new, and with a
/// request time older than its window refused as stale. No floor: a
/// re-send whose request time is inside its window is new.
#[test]
fn the_fifth_unheld_key_evicts_the_first() {
    let e = keep_engine(4);
    for (i, key) in ["k0", "k1", "k2", "k3"].iter().enumerate() {
        keyed_done(&e, key, i as i64);
    }
    assert!(keyed_done(&e, "k0", 10).duplicate);
    assert_eq!(
        order_members(&e.snapshot().unwrap()),
        ["k:k0", "k:k1", "k:k2", "k:k3"]
    );
    keyed_done(&e, "k4", 11);
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:k1", "k:k2", "k:k3", "k:k4"]);
    assert_key_evicted(&s, "k0");
    assert!(keyed_done(&e, "k1", 12).duplicate);
    // The first is forgotten: with a request time older than its window it
    // is stale, writing nothing ...
    let before = e.store.load("test").unwrap().revision;
    assert!(stale(e.admit_once_at(
        keyed(&e, "k0"),
        Some(12 - 600_000),
        12
    )));
    assert_eq!(e.store.load("test").unwrap().revision, before);
    // ... and with one inside its window (or none) it is new.
    let again = e.admit_once_at(keyed(&e, "k0"), Some(0), 13).unwrap();
    assert!(!again.duplicate);
}
/// Entries live work still needs are neither evicted nor counted: keyed
/// receipts of live runs, an event a live waiter holds. A keyed receipt
/// joins the order when its run finishes.
#[test]
fn held_entries_are_not_evicted_and_do_not_count() {
    let e = keep_engine(2);
    let live: Vec<String> = (0..3)
        .map(|i| {
            e.admit_once(keyed(&e, &format!("live-{i}")), i)
                .unwrap()
                .receipt
                .run_id
        })
        .collect();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 3)
        .unwrap();
    e.advance(&waiter, 3).unwrap();
    // Matches the waiter: held while the waiter is live (it has not
    // advanced past its wait yet).
    let held = e.admit_event("host", event("held", 2), 4).unwrap();
    keyed_done(&e, "done-0", 5);
    keyed_done(&e, "done-1", 6);
    let s = e.snapshot().unwrap();
    assert_initial_holds(&s, &held.key);
    // A third finished key evicts the oldest unheld one, not a held one.
    keyed_done(&e, "done-2", 7);
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:done-1", "k:done-2"]);
    assert!(s.events.contains_key(&held.key));
    assert!(live.iter().all(|run| s.runs.contains_key(run)));
    // A live run finishes: its receipt joins the order (by acceptance, so
    // it is the oldest) and is evicted first.
    assert!(e.advance(&live[0], 8).unwrap());
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:done-1", "k:done-2"]);
    assert!(!s.keyed_admissions.contains_key("live-0"));
    // The waiter consumes the event and finishes: the event joins the
    // order by its acceptance (older than "done-1") and, its window not
    // passed, is evicted as the oldest.
    while e.snapshot().unwrap().runs.contains_key(&waiter) {
        e.advance(&waiter, 9).unwrap();
    }
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:done-1", "k:done-2"]);
    assert!(!s.events.contains_key(&held.key));
    assert!(s.retention.event_expires_at_ms.get(&held.key).is_none());
}
/// A burst of 10,000 keyed admissions with the clock frozen keeps the
/// unheld set at `replay_keep` (within the per-commit bound) and refuses
/// none of them for what is remembered.
#[test]
fn a_frozen_clock_burst_keeps_the_remembered_set_at_replay_keep() {
    let e = ret_engine();
    let keep = RetentionPolicy::default().replay_keep;
    for i in 0..10_000 {
        let d = keyed_done(&e, &format!("burst-{i:05}"), 0);
        assert!(!d.duplicate, "{i}");
        if i % 500 == 0 || i == 9_999 {
            let s = e.snapshot().unwrap();
            let order = s.retention.replay_order.len();
            assert!(
                order <= keep + retention::REPLAY_EVICTIONS_PER_COMMIT,
                "{i}: {order}"
            );
            assert_eq!(s.keyed_admissions.len(), order.min(i + 1), "{i}");
            if i >= keep {
                assert!(order >= keep, "{i}: {order}");
            }
        }
    }
    let s = e.snapshot().unwrap();
    assert_eq!(s.retention.replay_order.len(), keep);
    assert!(s.runs.is_empty());
}
/// An event whose contract declares a 24 h window is evicted by count
/// long before its window passes; a wait registered after its eviction
/// times out, as after an expired event.
#[test]
fn a_day_long_event_window_is_evicted_by_count() {
    let e = ret_engine();
    let mut day = wait_artifact().content;
    day.registry
        .events
        .get_mut("test.completed")
        .unwrap()
        .retention_ms = 86_400_000;
    // The waiter's contract reference must match the event's.
    let contract = day.registry.events["test.completed"].clone();
    if let Operation::Wait { event, .. } = &mut day.playbook.steps.get_mut("wait").unwrap().op {
        *event = Some(ContractRef {
            name: contract.name.clone(),
            fingerprint: contract.fingerprint().unwrap(),
        });
    }
    let day = Artifact::seal(day).unwrap();
    install(&e, day, "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        replay_keep: 2,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let envelope = |id: &str, invocation: &str| EventEnvelope {
        contract: ContractRef {
            name: contract.name.clone(),
            fingerprint: contract.fingerprint().unwrap(),
        },
        ..correlated_event(id, invocation)
    };
    let first = e
        .admit_event("host", envelope("day-0", "exact-invocation"), 0)
        .unwrap();
    assert_eq!(
        e.snapshot().unwrap().retention.event_expires_at_ms[&first.key],
        86_400_000
    );
    e.admit_event("host", envelope("day-1", "inv-1"), 1)
        .unwrap();
    e.admit_event("host", envelope("day-2", "inv-2"), 2)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(!s.events.contains_key(&first.key));
    assert_eq!(s.events.len(), 2);
    assert!(s.indexes.event_memberships.get(&first.key).is_none());
    // A wait for it registered after its eviction does not match; it times
    // out.
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 3)
        .unwrap();
    e.advance(&waiter, 3).unwrap();
    assert_eq!(
        e.snapshot().unwrap().runs[&waiter].status,
        RunStatus::Waiting
    );
    e.drive(&mut FakeHost::default(), 200, 20).unwrap();
    assert_eq!(runs_page(&e, Some(&waiter))[0]["outcome"], "error");
    // A re-send of the evicted event is new.
    assert!(
        !e.admit_event("host", envelope("day-0", "exact-invocation"), 201)
            .unwrap()
            .duplicate
    );
}
/// Format 3 (bound record containers): the work of a commit that evicts k
/// entries depends on k, not on the entries kept, and reads no container
/// whole. The measured commit lowers `replay_keep` by k.
#[test]
fn eviction_work_on_record_storage_depends_on_entries_evicted_not_on_entries_kept() {
    use std::sync::atomic::Ordering;
    let measure = |kept: usize, k: usize| {
        let e = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
        install_on(&e, done_artifact(), "a");
        let policy = |keep: usize| RetentionPolicy {
            replay_keep: keep,
            ..RetentionPolicy::default()
        };
        e.set_retention_policy(policy(kept + k)).unwrap();
        for i in 0..kept + k {
            let d = e
                .admit_once(keyed_on(&e, &format!("k-{i:03}")), i as i64)
                .unwrap();
            e.advance(&d.receipt.run_id, i as i64).unwrap();
        }
        let reads = e.store.stats.record_reads.load(Ordering::Relaxed);
        let fallbacks = e.store.stats.fallbacks().len();
        e.set_retention_policy(policy(kept)).unwrap();
        let s = e.snapshot().unwrap();
        assert_eq!(s.retention.replay_order.len(), kept);
        assert_eq!(s.keyed_admissions.len(), kept);
        let scanned: BTreeSet<String> = e.store.stats.fallbacks()[fallbacks..]
            .iter()
            .map(|f| f.split(' ').next().unwrap().to_string())
            .collect();
        (
            e.store.stats.record_reads.load(Ordering::Relaxed) - reads,
            scanned,
        )
    };
    let (none_few, scanned) = measure(20, 0);
    assert!(scanned.is_empty(), "{scanned:?}");
    let (none_many, _) = measure(80, 0);
    assert_eq!(none_few, none_many, "nothing evicted");
    let (eight, scanned) = measure(20, 8);
    assert!(scanned.is_empty(), "{scanned:?}");
    let (eight_many, _) = measure(80, 8);
    assert_eq!(eight, eight_many, "8 evicted");
    let (sixteen, _) = measure(20, 16);
    let (twenty_four, _) = measure(20, 24);
    // A fixed part (the head lookup) and the same per eviction.
    let per_eight = sixteen - eight;
    assert_eq!(
        twenty_four - sixteen,
        per_eight,
        "reads grow linearly with evictions"
    );
    assert!(per_eight > 0);
    let fixed = (eight - none_few) - per_eight;
    assert!(fixed <= 4, "fixed part {fixed}");
}
/// Held entries at the head of the order (their holds began after they
/// joined, as through an untracked uncertain completion): the engine takes
/// them out of the order rather than evicting them, then evicts the oldest
/// unheld one; the stored commit passes the licence.
#[test]
fn the_engine_takes_held_entries_at_the_head_out_of_the_order() {
    for (label, state, kept) in held_heads() {
        let store = MemoryStorage::default();
        seed_eviction_state(&store, "test", &state);
        store.release("test", "w").unwrap();
        let e = Engine::new(store, "test", "incarnation-1").unwrap();
        e.set_retention_policy(state.retention.policy.clone())
            .unwrap_or_else(|err| panic!("{label}: {err}"));
        let s = e.snapshot().unwrap();
        assert_eq!(order_members(&s), kept, "{label}");
        assert_eq!(
            (
                s.keyed_admissions.contains_key("k0"),
                s.events.contains_key("ew")
            ),
            (false, true),
            "{label}"
        );
        assert_eq!(
            s.keyed_admissions.contains_key("kh"),
            state.keyed_admissions.contains_key("kh"),
            "{label}"
        );
    }
}
/// An entry that leaves by its window, or is forgotten to be admitted
/// again, leaves the eviction order with it.
#[test]
fn dropped_and_forgotten_entries_leave_the_order() {
    let e = keep_engine(10);
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        replay_keep: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..3 {
        keyed_done(&e, &format!("old-{i}"), i);
    }
    e.admit_event("host", correlated_event("old-event", "nobody"), 0)
        .unwrap();
    assert_eq!(e.snapshot().unwrap().retention.replay_order.len(), 4);
    // Past the windows: the drop takes the receipts and the event.
    keyed_done(&e, "new", 2_000);
    assert_eq!(order_members(&e.snapshot().unwrap()), ["k:new"]);
    // An expired key admitted again is forgotten first, then new.
    keyed_done(&e, "new", 4_000);
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:new"]);
    assert_eq!(s.retention.replay_order.len(), 1);
}

fn assert_key_evicted(s: &State, key: &str) {
    let member = format!("k:{key}");
    assert!(!s.keyed_admissions.contains_key(key));
    assert!(!s.retention.admission_expires_at_ms.contains_key(key));
    assert!(window_entries(s).iter().all(|(_, m)| m != &member));
}

fn assert_initial_holds(s: &State, held: &str) {
    assert_eq!(order_members(s), ["k:done-0", "k:done-1"]);
    assert_eq!(s.retention.replay_held.len(), 3);
    assert_eq!(s.keyed_admissions.len(), 5);
    assert!(s.events.contains_key(held));
}

fn held_heads() -> [(&'static str, State, Vec<&'static str>); 2] {
    // States this engine wrote (drained): the drain would otherwise judge
    // the entries first (Task 9).
    let drained = retention::DrainState {
        done: true,
        roots_done: true,
        phase: retention::DRAIN_DONE,
        ..retention::DrainState::default()
    };
    let mut keyed_head = eviction_state();
    keyed_head.indexes.initialized = true;
    keyed_head.retention.drain = drained.clone();
    let mut event_head = held_event_state();
    event_head.indexes.initialized = true;
    event_head.retention.drain = drained;
    [
        (
            "a keyed receipt whose run is live",
            keyed_head,
            vec!["k:k1", "k:k2", "k:k3", "e:e4", "e:ew"],
        ),
        (
            "an event a waiter holds",
            event_head,
            vec!["k:k1", "k:k2", "k:k3", "k:k4"],
        ),
    ]
}
