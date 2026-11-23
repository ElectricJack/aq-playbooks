use super::*;

#[test]
fn garbage_collection_bounds_preserved_candidate_examination() {
    let e = engine();
    let a = artifact("live", "ok", BTreeMap::from([("ok".into(), ret("ok"))]));
    install(&e, &a, "live");
    for _ in 0..3 {
        e.admit("live", "main", Value::Null, authority(), 0)
            .unwrap();
    }
    let page = e.collect_garbage(10, 1).unwrap();
    assert!(
        page.preserved <= 1,
        "page limit bounds examined retained candidates as well as disposals"
    );
    assert!(page.more, "bounded scan must report remaining candidates");
}

#[test]
fn retired_producer_epochs_need_no_new_tombstones_for_late_gc() {
    let e = engine();
    let mut a = artifact(
        "epoch-catalog",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    let c = event_contract();
    a.content.registry.events.insert(c.name.clone(), c);
    reseal(&mut a);
    install(&e, &a, "epoch-catalog");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e.admit_event("host", envelope("old-epoch"), 0).unwrap();
    e.advance_producer_epoch(
        "host",
        1,
        "operator",
        "producer changed durable replay epoch",
        1,
    )
    .unwrap();
    e.collect_garbage(20_000, 10).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(
        (s.events.len(), s.retention.event_tombstones.len()),
        (0, 0),
        "retired epoch fence already refuses these replay identities"
    );
    assert!(
        e.admit_event("host", envelope("old-epoch"), 20_001)
            .is_err()
    );
    assert!(
        e.admit_event_in_epoch("host", envelope("explicit-new-epoch"), 1, 20_001)
            .is_ok()
    );
}

#[test]
fn expired_replay_identities_are_judged_by_time_and_retired_epochs_refuse_old_ones() {
    let e = engine();
    let mut a = artifact(
        "replay-catalog",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    let c = event_contract();
    a.content.registry.events.insert(c.name.clone(), c);
    reseal(&mut a);
    install(&e, &a, "replay-catalog");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e.admit_event("host", envelope("original"), 0).unwrap();
    e.collect_garbage(20_000, 10).unwrap();
    // Spec 0003: the expired identity leaves with no tombstone. A repeat
    // with its producer time is stale; without one it is new.
    assert!(e.snapshot().unwrap().retention.event_tombstones.is_empty());
    assert!(matches!(
        e.admit_event_at("host", envelope("original"), 0, Some(0), 20_001),
        Err(Error::Stale { .. })
    ));
    assert!(
        !e.admit_event("host", envelope("original"), 20_001)
            .unwrap()
            .duplicate
    );
    e.advance_producer_epoch("host", 1, "operator", "new explicit replay epoch", 20_002)
        .unwrap();
    assert!(e.admit_event("host", envelope("original"), 20_003).is_err());
    let replay = e
        .admit_event_in_epoch("host", envelope("original"), 1, 20_003)
        .unwrap();
    assert!(!replay.duplicate);
    assert!(
        e.admit_event_in_epoch("host", envelope("original"), 1, 20_004)
            .unwrap()
            .duplicate
    );
}

#[test]
fn completed_cohort_gc_removes_ready_timer_and_dependency_index_residue() {
    let e = engine();
    let one = artifact("gc-one", "ok", BTreeMap::from([("ok".into(), ret("ok"))]));
    let two = artifact("gc-two", "ok", BTreeMap::from([("ok".into(), ret("ok"))]));
    let parent = joined_parent(
        &[one.clone(), two.clone()],
        JoinMode::All,
        RemainingPolicy::Track,
    );
    for a in [&one, &two, &parent] {
        install(&e, a, &a.content.playbook.id);
    }
    let id = e
        .admit("joining", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 50).unwrap();
    assert_eq!(settled(&e, &id).status, RunStatus::Completed);
    // The settling commit itself left no index residue.
    let state = e.snapshot().unwrap();
    assert_execution_indexes_empty(&state);
    assert_eq!(
        (
            state.indexes.root_members.len(),
            state.indexes.root_intents.len(),
            state.indexes.intent_roots.len(),
            state.indexes.run_roots.len()
        ),
        (0, 0, 0, 0)
    );
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for _ in 0..20 {
        if !e.collect_garbage(100, 1).unwrap().more {
            break;
        }
    }
    assert_execution_indexes_empty(&e.snapshot().unwrap());
    assert!(
        e.query(&IndexQuery::Ready {
            scope_after: None,
            limit: 10
        })
        .unwrap()
        .items
        .is_empty()
    );
}

fn assert_execution_indexes_empty(s: &State) {
    let index = &s.indexes;
    let counts = [
        ("runs", s.runs.len()),
        ("indexed runs", index.runs.len()),
        ("ready tokens", index.ready_tokens.len()),
        ("timers", index.timers.len()),
        ("durable dependency keys", index.dependents.len()),
        ("waiters", index.waiters.len()),
        ("run dependencies", index.run_dependencies.len()),
        ("run subscriptions", index.run_subscriptions.len()),
    ];
    for (name, count) in counts {
        assert_eq!(count, 0, "remaining {name}");
    }
}
