use super::*;

/// The brief's test: the open commit and each later commit move at most 50
/// settled legacy cohorts to history and drop at most 500 tombstones; the
/// drain then records that it is done; every legacy run is in history.
#[test]
fn legacy_namespace_drains_50_cohorts_and_500_entries_per_commit() {
    let e = Engine::new(legacy(120, 1_200), "test", "i-1").unwrap(); // commit 1
    let count = |e: &Engine<MemoryStorage>| {
        let s = e.snapshot().unwrap();
        (s.runs.len(), s.retention.event_tombstones.len())
    };
    assert_eq!(count(&e), (70, 700));
    assert!(!e.snapshot().unwrap().retention.drain.done);
    e.admit("a", "main", Value::Null, authority(), 1).unwrap();
    assert_eq!(count(&e), (21, 200)); // 20 legacy cohorts left plus the new, still live run
    e.admit("a", "main", Value::Null, authority(), 2).unwrap();
    assert_eq!(count(&e), (2, 0));
    assert!(e.snapshot().unwrap().retention.drain.done);
    assert_eq!(legacy_ids(&e).len(), 120);
}
/// The brief's test, with live legacy runs of three shapes: five admitted
/// runs, and a parent waiting on its child, which waits for an event. The
/// drain moves the settled cohorts and leaves the live ones; each live one
/// settles normally and leaves in the commit that settles it.
#[test]
fn legacy_live_cohorts_survive_the_drain_and_leave_when_they_finish() {
    let store = MemoryStorage::retaining_settled();
    let e = Engine::new(store.clone(), "test", "setup").unwrap();
    install(&e, done_artifact(), "a");
    let child = wait_artifact();
    install(&e, child.clone(), "waiter");
    install(&e, parent_artifact(&child, CallMode::Wait), "parent");
    let parent = e
        .admit("parent", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 20).unwrap();
    let live: Vec<String> = (0..5)
        .map(|_| e.admit("a", "main", Value::Null, authority(), 0).unwrap())
        .collect();
    e.release().unwrap();
    let mut s = store.load("test").unwrap().state;
    let kid = s.runs[&parent].children[0].clone();
    assert_eq!(s.runs[&parent].status, RunStatus::Waiting);
    assert_eq!(s.runs[&kid].status, RunStatus::Waiting);
    inject_legacy(&mut s, 60, 0);
    store_legacy(&store, s);
    let e = Engine::new(store, "test", "i-1").unwrap();
    e.install(done_artifact()).unwrap(); // one more commit (a no-op install)
    let s = e.snapshot().unwrap();
    assert_eq!(legacy_ids(&e).len(), 60);
    assert!(s.runs.keys().all(|id| !id.starts_with("run-0-legacy-")));
    assert_eq!(s.runs.len(), 7);
    for id in &live {
        assert!(e.advance(id, 1).unwrap());
        assert!(!e.snapshot().unwrap().runs.contains_key(id), "{id} left");
    }
    e.admit_event("host", event("for-the-child", 2), 1).unwrap();
    for _ in 0..4 {
        e.drive(&mut FakeHost::default(), 2, 10).unwrap();
    }
    assert!(e.snapshot().unwrap().runs.is_empty());
    let runs = inspect_all(&e, InspectionKind::Runs, None);
    assert_eq!(runs.len(), 67);
    let outcome = |id: &str| {
        runs.iter()
            .find(|r| r["id"] == id)
            .map(|r| r["outcome"].clone())
    };
    assert_eq!(outcome(&parent), Some(json!("ok")));
    assert_eq!(outcome(&kid), Some(json!("ok")));
}
/// Mid-drain, inspection shows every legacy run exactly once, whether it is
/// still in the record or already in history.
#[test]
fn inspection_lists_each_legacy_run_once_at_every_commit_of_the_drain() {
    let e = Engine::new(
        legacy_with(120, 0, |s| s.retention.policy.drain_cohorts_per_commit = 7),
        "test",
        "i-1",
    )
    .unwrap();
    let left = |e: &Engine<MemoryStorage>| {
        e.snapshot()
            .unwrap()
            .runs
            .keys()
            .filter(|id| id.starts_with("run-0-legacy-"))
            .count()
    };
    let listed_once = |e: &Engine<MemoryStorage>, step: usize| {
        let mut ids = legacy_ids(e);
        assert_eq!(ids.len(), 120, "step {step}");
        ids.dedup();
        assert_eq!(ids.len(), 120, "step {step}: listed twice");
    };
    assert_eq!(left(&e), 113);
    let mut step = 0;
    while !e.snapshot().unwrap().retention.drain.done {
        listed_once(&e, step);
        let before = left(&e);
        // Two commits: each moves at most 7 cohorts, its own included.
        let id = e.admit("a", "main", Value::Null, authority(), 1).unwrap();
        listed_once(&e, step);
        assert!(e.advance(&id, 1).unwrap());
        assert!(before - left(&e) <= 7 + 6, "step {step}");
        step += 1;
        assert!(step < 30);
    }
    listed_once(&e, step);
    assert_eq!(left(&e), 0);
}
/// Drained cohorts go through the rolling history trim and sort as the
/// oldest: with `history_keep` 30, history keeps the new runs finished
/// during the drain and the 25 newest legacy runs.
#[test]
fn drained_legacy_runs_are_the_oldest_in_the_rolling_history() {
    let e = Engine::new(
        legacy_with(120, 0, |s| s.retention.policy.history_keep = 30),
        "test",
        "i-1",
    )
    .unwrap();
    let new: Vec<String> = (0..5)
        .map(|t| {
            let id = e.admit("a", "main", Value::Null, authority(), t).unwrap();
            assert!(e.advance(&id, t).unwrap());
            id
        })
        .collect();
    assert!(e.snapshot().unwrap().retention.drain.done);
    let listed: BTreeSet<String> = inspect_all(&e, InspectionKind::Runs, None)
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    let expected: BTreeSet<String> = (95..120)
        .map(|i| format!("run-0-legacy-{i:04}"))
        .chain(new)
        .collect();
    assert_eq!(listed, expected);
}
/// Receipts owned by no run that an older engine left (`admin`) leave in
/// bounded steps, counted against the entries bound, each dated by its own
/// time; an admin receipt written during the drain still leaves with its
/// own commit.
#[test]
fn legacy_admin_receipts_leave_in_bounded_steps_dated_by_their_own_time() {
    let store = legacy_with(0, 0, |s| {
        for i in 0..1_200 {
            s.sequence += 1;
            s.receipts.push(Receipt {
                sequence: s.sequence,
                run: "admin".into(),
                step: "controls".into(),
                at_ms: i,
                kind: "activation_controls".into(),
                detail: json!({}),
            });
        }
    });
    let e = Engine::new(store, "test", "i-1").unwrap();
    assert_eq!(e.snapshot().unwrap().receipts.len(), 700);
    e.set_activation_controls("a", ActivationControls::default(), 5_000)
        .unwrap();
    assert_eq!(
        e.snapshot().unwrap().receipts.len(),
        200,
        "its own left too"
    );
    e.install(done_artifact()).unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.receipts.is_empty() && s.retention.drain.done);
    let admin = inspect_all(&e, InspectionKind::Receipts, Some("admin"));
    assert_eq!(admin.len(), 1_201);
    // Each row carries its own time: deleting history before 600 removes
    // exactly the 600 receipts written before it.
    assert_eq!(e.delete_history_before(600).unwrap(), 600);
}
/// Legacy keyed receipts and events (no expiry record, no window or
/// eviction-order key) are indexed one entry at a time here (entries bound
/// 1): each gets its expiry record and window key, and joins the eviction
/// order unless live work holds it (a live run's receipt goes to
/// `replay_held`; an event a live wait needs stays out). Throughout, a
/// duplicate of each is still recognised. Afterwards the rolling bound
/// applies to them like to any entry.
#[test]
fn legacy_keyed_receipts_and_events_join_the_window_and_the_eviction_order() {
    let e = Engine::new(MemoryStorage::retaining_settled(), "test", "setup").unwrap();
    install(&e, done_artifact(), "a");
    install(&e, wait_artifact(), "waiter");
    let request = |key: &str| KeyedAdmissionRequest {
        key: key.into(),
        binding: "a".into(),
        expected_activation_revision: e.snapshot().unwrap().bindings["a"].revision,
        entry: "main".into(),
        input: Value::Null,
        authority: authority(),
    };
    let done = e.admit_once(request("k-done"), 10).unwrap();
    assert!(e.advance(&done.receipt.run_id, 10).unwrap());
    let live = e.admit_once(request("k-live"), 11).unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 12)
        .unwrap();
    e.advance(&waiter, 12).unwrap();
    let free = e
        .admit_event("host", correlated_event("free", "nobody"), 13)
        .unwrap()
        .key;
    let mut legacy = old_engine_shape(&e.snapshot().unwrap());
    legacy.retention.policy.drain_entries_per_commit = 1;
    e.release().unwrap();
    let store = fresh_legacy(MemoryStorage::retaining_settled(), &legacy);
    let e = Engine::new(store, "test", "i-1").unwrap();
    let mut commits = 0;
    while !e.snapshot().unwrap().retention.drain.done {
        assert!(e.admit_once(request("k-done"), 20).unwrap().duplicate);
        assert!(e.admit_once(request("k-live"), 20).unwrap().duplicate);
        assert!(
            e.admit_event("host", correlated_event("free", "nobody"), 20)
                .unwrap()
                .duplicate
        );
        commits += 1;
        assert!(commits < 20);
    }
    assert_indexed_legacy(&e.snapshot().unwrap(), &done.receipt, &live.receipt, &free);
    // The live run finishes: its receipt joins the order. With
    // `replay_keep` 1 the oldest legacy entries are then evicted.
    assert!(e.advance(&live.receipt.run_id, 30).unwrap());
    assert_eq!(order_members(&e.snapshot().unwrap()).len(), 3);
    e.set_retention_policy(RetentionPolicy {
        replay_keep: 1,
        ..e.snapshot().unwrap().retention.policy
    })
    .unwrap();
    // The newest by acceptance is the event; both keyed receipts go.
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), vec![format!("e:{free}")]);
    assert!(s.keyed_admissions.is_empty());
    assert!(!e.admit_once(request("k-done"), 31).unwrap().duplicate);
}
/// The drain's bounds are policy fields (defaults 50 and 500), read from
/// the stored policy, and must be positive.
#[test]
fn drain_bounds_come_from_the_stored_policy_and_must_be_positive() {
    let policy = RetentionPolicy::default();
    assert_eq!(
        (
            policy.drain_cohorts_per_commit,
            policy.drain_entries_per_commit
        ),
        (50, 500)
    );
    let decoded: RetentionPolicy = serde_json::from_value(
        json!({"event_replay_ms":1,"terminal_run_ms":1,"audit_ms":1,"max_tombstones":1}),
    )
    .unwrap();
    assert_eq!(
        (
            decoded.drain_cohorts_per_commit,
            decoded.drain_entries_per_commit
        ),
        (50, 500)
    );
    let e = Engine::new(
        legacy_with(120, 30, |s| {
            s.retention.policy.drain_cohorts_per_commit = 10;
            s.retention.policy.drain_entries_per_commit = 3;
        }),
        "test",
        "i-1",
    )
    .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(
        (s.runs.len(), s.retention.event_tombstones.len()),
        (110, 27)
    );
    for bad in [
        RetentionPolicy {
            drain_cohorts_per_commit: 0,
            ..RetentionPolicy::default()
        },
        RetentionPolicy {
            drain_entries_per_commit: 0,
            ..RetentionPolicy::default()
        },
    ] {
        assert!(matches!(
            e.set_retention_policy(bad),
            Err(Error::Refused(m)) if m == "drain bounds must be positive"
        ));
    }
}
/// Progress is stored: a restart continues where the drain stopped, and no
/// legacy run is written to history twice.
#[test]
fn a_drain_continues_after_a_restart() {
    let store = legacy(120, 0);
    let e = Engine::new(store.clone(), "test", "i-1").unwrap();
    assert_eq!(e.snapshot().unwrap().runs.len(), 70);
    e.release().unwrap();
    let e = Engine::new(store.clone(), "test", "i-2").unwrap();
    assert_eq!(e.snapshot().unwrap().runs.len(), 20);
    e.release().unwrap();
    let e = Engine::new(store, "test", "i-3").unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty() && s.retention.drain.done);
    assert_eq!(legacy_ids(&e).len(), 120);
}
/// A namespace this engine created has nothing to drain: its first commit
/// (the open commit) records the drain as done.
#[test]
fn a_new_namespace_is_drained_by_its_first_commit() {
    let e = engine();
    assert!(e.snapshot().unwrap().retention.drain.done);
}

fn assert_indexed_legacy(
    s: &State,
    done: &KeyedAdmissionReceipt,
    live: &KeyedAdmissionReceipt,
    free: &str,
) {
    for (key, receipt) in [("k-done", done), ("k-live", live)] {
        let at = s.retention.admission_expires_at_ms.get(key).copied();
        assert_eq!(at, Some(receipt.accepted_at_ms + 604_800_000), "{key}");
        assert!(window_entries(s).contains(&(at.unwrap(), format!("k:{key}"))));
    }
    assert!(
        s.retention
            .replay_order
            .contains_key(&retention::admission_order_key(done))
    );
    assert_eq!(
        s.retention.replay_held.get(&live.run_id),
        Some(&"k-live".to_string())
    );
    assert!(
        s.retention
            .replay_order
            .contains_key(&retention::event_order_key(&s.events[free]))
    );
    let at = retention::event_expiry(s, free).unwrap();
    assert!(window_entries(s).contains(&(at, format!("e:{free}"))));
    assert_eq!(order_members(s).len(), 2, "{:?}", order_members(s));
}
