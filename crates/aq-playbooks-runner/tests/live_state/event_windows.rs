use super::*;

fn event_run_id(id: &str) -> bool {
    id.strip_prefix("event-sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}

/// SC-7: an event accepted again after its window gets new destination,
/// run (and so child and operation) ids, in the same `event-sha256:` form;
/// the first run's history stays and the second run commits beside it.
#[test]
fn an_event_accepted_again_after_its_window_gets_new_run_ids() {
    let e = engine();
    install(&e, trigger_artifact(), "trigger");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_event("host", event("again", 2), 0).unwrap();
    assert_eq!(e.fanout(&first.key, 10, 0).unwrap(), 1);
    e.drive(&mut FakeHost::default(), 0, 20).unwrap();
    let first_run = runs_page(&e, None)[0]["id"].as_str().unwrap().to_string();
    // The window passes: GC drops the event; the legacy fences it leaves
    // (tombstone, correlation fences) go too, as Task 6 will not write them.
    while e.collect_garbage(2_000, 100).unwrap().more {}
    let mut s = e.snapshot().unwrap();
    assert!(!s.events.contains_key(&first.key));
    s.retention.event_tombstones.clear();
    s.retention.expired_correlations.clear();
    overwrite(&e.store, "incarnation-1", &s);
    let second = e.admit_event("host", event("again", 2), 2_001).unwrap();
    assert_eq!(second.key, first.key);
    assert!(!second.duplicate);
    assert_eq!(e.fanout(&second.key, 10, 2_001).unwrap(), 1);
    e.drive(&mut FakeHost::default(), 2_001, 20).unwrap();
    assert_settled_event_runs(&e, &first_run);
}

/// SC-7, existing data: a destination stored before the change keeps its
/// key, and its run id is `event-<that key>`.
#[test]
fn a_stored_destination_keeps_its_key_and_run_id() {
    let e = engine();
    install(&e, trigger_artifact(), "trigger");
    let accepted = e.admit_event("host", event("stored", 2), 0).unwrap();
    let mut s = e.snapshot().unwrap();
    let revision = s.bindings["trigger"].revision;
    let legacy = digest(&(&accepted.key, "trigger", revision, "on-completed")).unwrap();
    s.events.get_mut(&accepted.key).unwrap().destinations[0].key = legacy.clone();
    overwrite(&e.store, "incarnation-1", &s);
    assert_eq!(e.fanout(&accepted.key, 10, 1).unwrap(), 1);
    let id = format!("event-{legacy}");
    assert!(event_run_id(&id));
    assert!(e.snapshot().unwrap().runs.contains_key(&id));
}

#[test]
fn expired_event_leaves_without_tombstone_or_fence_and_redelivery_is_new() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_event("host", event("e1", 2), 0).unwrap(); // expires at max(1, 1_000)
    e.admit("waiter", "main", Value::Null, authority(), 1_001)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(!s.events.contains_key(&first.key));
    assert!(s.retention.event_tombstones.is_empty() && s.retention.expired_correlations.is_empty());
    assert!(
        !e.admit_event("host", event("e1", 2), 1_002)
            .unwrap()
            .duplicate
    );
}

#[test]
fn wait_registered_after_its_event_expired_times_out() {
    // NR-1 option (a)
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e.admit_event("host", event("e1", 2), 0).unwrap();
    let id = e
        .admit("waiter", "main", Value::Null, authority(), 1_001)
        .unwrap();
    e.advance(&id, 1_001).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Waiting);
    e.advance(&id, 1_101).unwrap();
    e.advance(&id, 1_102).unwrap();
    assert_eq!(runs_page(&e, Some(&id))[0]["outcome"], "error");
}

#[test]
fn burst_at_a_frozen_clock_leaves_at_most_2_kb_of_window_entries_per_run() {
    let e = ret_engine();
    e.install(wait_artifact()).unwrap();
    // Task 8b: above the burst's 400 entries, so nothing is evicted by
    // count and the bytes of every window entry are measured.
    e.set_retention_policy(RetentionPolicy {
        replay_keep: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let mut after_first = 0;
    for i in 0..200 {
        let d = e
            .admit_once(keyed(&e, &format!("burst-{i:04}")), 0)
            .unwrap();
        e.advance(&d.receipt.run_id, 0).unwrap();
        e.admit_event(
            "host",
            correlated_event(&format!("burst-{i:04}"), &format!("inv-{i:04}")),
            0,
        )
        .unwrap();
        if i == 0 {
            after_first = canonical_bytes(&e.snapshot().unwrap()).unwrap().len();
        }
    }
    let s = e.snapshot().unwrap();
    let after_last = canonical_bytes(&s).unwrap().len();
    let per_run = (after_last - after_first) / 199;
    eprintln!("burst: {per_run} bytes per run");
    assert!(per_run <= 2_048, "{per_run} bytes per run");
    assert_eq!((s.keyed_admissions.len(), s.events.len()), (200, 200));
    assert!(
        s.runs.is_empty()
            && s.intents.is_empty()
            && s.receipts.is_empty()
            && s.waits.is_empty()
            && s.budgets.is_empty()
    );
}

#[test]
fn an_expired_event_delivered_again_before_its_drop_is_new() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_event("host", event("e1", 2), 0).unwrap();
    let again = e.admit_event("host", event("e1", 2), 2_000).unwrap();
    assert_eq!(again.key, first.key);
    assert!(!again.duplicate);
    let s = e.snapshot().unwrap();
    assert_eq!(s.events[&again.key].accepted_at_ms, 2_000);
    assert_eq!(
        s.retention.event_expires_at_ms.get(&again.key).copied(),
        Some(3_000)
    );
    assert_eq!(
        s.indexes.events.values().map(BTreeMap::len).sum::<usize>(),
        2
    );
}

#[test]
fn an_expired_event_held_by_a_waiter_stays_and_its_redelivery_is_a_duplicate() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&waiter, 0).unwrap();
    let held = e.admit_event("host", event("held", 1), 0).unwrap();
    e.admit("waiter", "main", Value::Null, authority(), 1_500)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.events.contains_key(&held.key));
    // Deferred by its own window (1_000) from the clock.
    assert_eq!(
        s.retention.event_expires_at_ms.get(&held.key).copied(),
        Some(2_500)
    );
    // Expired at 2_500 but still held: a duplicate, as inside its window.
    let again = e.admit_event("host", event("held", 1), 2_600).unwrap();
    assert!(again.duplicate);
    assert_eq!(again.key, held.key);
}

#[test]
fn a_queued_destination_holds_an_expired_event_and_an_input_blocked_one_does_not() {
    let e = engine();
    install(&e, trigger_artifact(), "trigger");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let queued = e
        .admit_event("host", correlated_event("queued", "q"), 0)
        .unwrap();
    e.admit_event("host", correlated_event("other", "o"), 1_500)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(
        s.events.contains_key(&queued.key),
        "a queued destination holds it"
    );
    assert_eq!(
        s.retention.event_expires_at_ms.get(&queued.key).copied(),
        Some(2_500)
    );

    let e = engine();
    install(&e, input_blocked_trigger_artifact(), "blocked");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let blocked = e
        .admit_event("host", correlated_event("blocked", "b"), 0)
        .unwrap();
    let owner = format!("event:{}", blocked.key);
    assert_eq!(
        e.snapshot()
            .unwrap()
            .receipts
            .iter()
            .filter(|r| r.run == owner)
            .count(),
        1
    );
    e.admit_event("host", correlated_event("other", "o"), 1_500)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(
        !s.events.contains_key(&blocked.key),
        "an input-blocked destination does not hold it"
    );
    // Its receipts went to history with it.
    assert!(s.receipts.iter().all(|r| r.run != owner));
    assert_eq!(
        inspect_all(&e, InspectionKind::Receipts, Some(&owner)).len(),
        1
    );
}

/// Events on both backends: one held by a live waiter (deferred), one
/// delivered again after its window (it leaves first, then is new), one
/// dropped with its index entries.
fn event_drop_script<S: Storage>(e: &Engine<S>) {
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&waiter, 0).unwrap();
    e.admit_event("host", event("held", 1), 0).unwrap();
    e.admit_event("host", correlated_event("again", "a"), 0)
        .unwrap();
    e.admit_event("host", correlated_event("dropped", "d"), 0)
        .unwrap();
    assert!(
        !e.admit_event("host", correlated_event("again", "a"), 1_500)
            .unwrap()
            .duplicate
    );
    e.admit("waiter", "main", Value::Null, authority(), 1_600)
        .unwrap();
}

#[test]
fn record_storage_drops_events_like_memory_storage() {
    let m = engine();
    let r = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
    install(&m, wait_artifact(), "waiter");
    install_on(&r, wait_artifact(), "waiter");
    event_drop_script(&m);
    let transitions = r
        .store
        .stats
        .transitions
        .load(std::sync::atomic::Ordering::Relaxed);
    event_drop_script(&r);
    assert!(
        r.store
            .stats
            .transitions
            .load(std::sync::atomic::Ordering::Relaxed)
            > transitions
    );
    assert_same_engines(&m, &r, "the event script");
    let s = r.snapshot().unwrap();
    assert_retained_event_indexes(&s);
}

#[test]
fn backwards_host_time_does_not_shorten_an_event_window() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e.admit("waiter", "main", Value::Null, authority(), 5_000)
        .unwrap();
    let late = e.admit_event("host", event("late", 2), 10).unwrap();
    assert!(!late.duplicate);
    let s = e.snapshot().unwrap();
    assert_eq!(s.events[&late.key].accepted_at_ms, 10);
    assert_eq!(
        s.retention.event_expires_at_ms.get(&late.key).copied(),
        Some(6_000)
    );
    assert!(window_entries(&s).contains(&(6_000, format!("e:{}", late.key))));
}

/// Review I1 (probe shape): an expired event that a live wait still holds,
/// delivered again with no commit in between, is a duplicate every time, as
/// it is inside its window. Changed content still conflicts.
#[test]
fn a_held_expired_event_delivered_again_is_a_duplicate() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&waiter, 0).unwrap();
    let held = e.admit_event("host", event("held", 1), 0).unwrap();
    for at in [999, 1_000, 5_000, 50_000] {
        let again = e.admit_event("host", event("held", 1), at).unwrap();
        assert!(again.duplicate, "at {at}");
        assert_eq!(again.key, held.key, "at {at}");
    }
    let changed = EventEnvelope {
        subject: Some("other".into()),
        ..event("held", 1)
    };
    assert!(matches!(
        e.admit_event("host", changed, 50_001),
        Err(Error::Conflict(_))
    ));
    assert!(e.snapshot().unwrap().events.contains_key(&held.key));
}

/// A trigger whose destination cannot be fanned out (its dependency bytes
/// are missing), so the accepted event keeps a queued destination.
fn missing_dependency_trigger_artifact() -> Artifact {
    let child = artifact(
        "missing-child",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
        FixtureContracts::default(),
    );
    let mut parent = parent_artifact(&child, CallMode::Wait).content;
    let c = event_contract();
    parent.registry.events.insert(c.name.clone(), c.clone());
    parent.playbook.triggers.push(Trigger {
        id: "trigger".into(),
        event: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        entry: "main".into(),
        input: Expr::literal(Value::Null),
    });
    Artifact::seal(parent).unwrap()
}

/// Review I1, aq-server's quantum: the outbox re-sends an event whose fanout
/// is not complete, then fans out and ticks. Once that event's window has
/// passed (it is held by its queued destination), the re-send must not stop
/// the quantum, so an unrelated run still progresses in it.
#[test]
fn redelivering_a_held_expired_event_does_not_stop_the_quantum() {
    let e = ret_engine();
    install(&e, missing_dependency_trigger_artifact(), "parent");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let held = e.admit_event("host", event("outbox", 2), 0).unwrap();
    assert_eq!(
        e.fanout(&held.key, 10, 0).unwrap(),
        0,
        "the destination stays queued"
    );
    let unrelated = e.admit("a", "main", Value::Null, authority(), 0).unwrap();
    // Event, then Fanout, then Tick, each failing the quantum on error.
    let quantum = |at: i64| -> Result<()> {
        let again = e.admit_event("host", event("outbox", 2), at)?;
        assert!(again.duplicate, "at {at}");
        e.fanout(&held.key, 10, at)?;
        e.drive(&mut FakeHost::default(), at, 20)?;
        Ok(())
    };
    quantum(5_000).unwrap();
    assert!(
        !e.snapshot().unwrap().runs.contains_key(&unrelated),
        "the unrelated run finished"
    );
    assert_eq!(runs_page(&e, Some(&unrelated))[0]["outcome"], "ok");
    quantum(50_000).unwrap();
    assert!(e.snapshot().unwrap().events.contains_key(&held.key));
}

/// Review I2: a held event is deferred by its window each time, never by its
/// age, so it leaves within one window after its need ends.
#[test]
fn an_event_held_for_many_windows_leaves_within_one_window_after_its_need_ends() {
    let e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&waiter, 0).unwrap();
    let held = e.admit_event("host", event("held", 1), 0).unwrap(); // window 1_000 (contract)
    let expiry = |e: &Engine<MemoryStorage>| {
        e.snapshot()
            .unwrap()
            .retention
            .event_expires_at_ms
            .get(&held.key)
            .copied()
    };
    assert_eq!(expiry(&e), Some(1_000));
    // A commit at each expiry defers it by one window.
    for k in 1..=10 {
        e.admit("a", "main", Value::Null, authority(), k * 1_000)
            .unwrap();
        assert_eq!(expiry(&e), Some((k + 1) * 1_000), "deferral {k}");
    }
    // The need ends: the wait times out and the waiter leaves.
    e.drive(&mut FakeHost::default(), 10_050, 40).unwrap();
    assert!(!e.snapshot().unwrap().runs.contains_key(&waiter));
    assert!(e.snapshot().unwrap().events.contains_key(&held.key));
    e.admit("a", "main", Value::Null, authority(), 11_000)
        .unwrap();
    assert!(!e.snapshot().unwrap().events.contains_key(&held.key));
}

/// NR-1, both halves under a short window (1_000): a wait registered inside
/// the window matches the earlier event; one registered at its expiry times
/// out, because the admitting commit drops the event first.
#[test]
fn a_wait_inside_the_window_matches_and_one_after_it_times_out() {
    for (at, outcome) in [(999, "ok"), (1_000, "error")] {
        let e = engine();
        install(&e, wait_artifact(), "waiter");
        e.set_retention_policy(RetentionPolicy {
            event_replay_ms: 1,
            ..RetentionPolicy::default()
        })
        .unwrap();
        e.admit_event("host", event("e1", 2), 0).unwrap();
        let id = e
            .admit("waiter", "main", Value::Null, authority(), at)
            .unwrap();
        e.drive(&mut FakeHost::default(), at, 20).unwrap();
        if outcome == "error" {
            assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Waiting);
            e.drive(&mut FakeHost::default(), at + 101, 20).unwrap();
        }
        assert_eq!(
            runs_page(&e, Some(&id))[0]["outcome"],
            outcome,
            "wait at {at}"
        );
    }
}

fn assert_settled_event_runs(e: &Engine<MemoryStorage>, first_run: &str) {
    let ids: Vec<String> = runs_page(e, None)
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(ids.iter().any(|id| id == first_run));
    assert!(ids.iter().all(|id| event_run_id(id)), "{ids:?}");
    assert!(e.snapshot().unwrap().runs.is_empty());
}

fn assert_retained_event_indexes(s: &State) {
    // "held" (deferred to 2_500) and the second "again" (2_500) remain.
    assert_eq!(s.events.len(), 2);
    assert_eq!(
        window_entries(s)
            .iter()
            .map(|(at, _)| *at)
            .collect::<Vec<_>>(),
        vec![2_500, 2_500]
    );
    assert_eq!(s.indexes.event_memberships.len(), 2);
    // Two correlation index keys each; the dropped event's are gone.
    assert_eq!(s.indexes.events.len(), 4);
}
