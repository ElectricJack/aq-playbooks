use super::*;

/// An asynchronous operation whose dispatch lost its handle (dispatched at
/// 0, command deadline 100), with a 10 s event window (`replay_keep` 3):
/// its completion `done` and another event of its contract arrive at 500
/// and 600, while it is uncertain, then four unrelated events. Returns the
/// engine, the run, the two held keys and the host.
fn lapse_fixture() -> (Engine<MemoryStorage>, String, [String; 2], FakeHost) {
    let e = keep_engine(3);
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 600_000,
        event_replay_ms: 10_000,
        replay_keep: 3,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let (a, completion) = async_artifact();
    install(&e, a, "async");
    let run = e
        .admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    let op = only_intent(&e.snapshot().unwrap());
    assert_eq!((op.handle, op.deadline_at_ms), (None, 100));
    let done = e
        .admit_event("host", async_event(&completion, "done"), 500)
        .unwrap()
        .key;
    let other = EventEnvelope {
        correlation: BTreeMap::from([("operation".into(), "elsewhere".into())]),
        ..async_event(&completion, "other")
    };
    let other = e.admit_event("host", other, 600).unwrap().key;
    for i in 0..4 {
        e.admit_event(
            "host",
            correlated_event(&format!("unrelated-{i}"), &format!("inv-{i}")),
            700 + i,
        )
        .unwrap();
    }
    (e, run, [done, other], host)
}
fn held(s: &State, key: &str) -> bool {
    s.events.contains_key(key) && !order_members(s).contains(&format!("e:{key}"))
}
/// Ruling C: an operation whose dispatch lost its handle holds the later
/// events of its completion contract and scope only until it is past its
/// command deadline plus the event's window (100 + 10,000 here). Before
/// that they are held (never evicted, not counted); after it they are
/// ordinary unheld entries: they rejoin the eviction order at their
/// acceptance positions, as the oldest, and `replay_keep` pushes them out
/// (their own window has not passed yet: 10,500 and 10,600).
#[test]
fn a_lost_handle_hold_lapses_after_the_command_deadline_plus_the_window() {
    let (e, _, [done, other], _) = lapse_fixture();
    let s = e.snapshot().unwrap();
    assert!(held(&s, &done) && held(&s, &other));
    e.admit_event("host", correlated_event("before", "inv-b"), 10_099)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(held(&s, &done) && held(&s, &other), "held until 10,100");
    e.admit_event("host", correlated_event("after", "inv-a"), 10_100)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(
        !s.events.contains_key(&done) && !s.events.contains_key(&other),
        "lapsed: evicted by count as the oldest"
    );
    assert_eq!(order_members(&s).len(), 3);
    assert!(
        s.indexes
            .uncertain_completion_contracts
            .values()
            .all(BTreeSet::is_empty)
    );
}
/// Ruling C, the handle found before the hold lapses: the held completion
/// matches the handle and the run finishes `ok`, as before.
#[test]
fn a_handle_found_before_the_hold_lapses_takes_its_held_completion() {
    let (e, run, [done, _], mut host) = lapse_fixture();
    host.responses.insert(
        "reconcile:test.command".into(),
        vec![DispatchResult::Accepted {
            handle: "server-operation".into(),
        }],
    );
    e.recover(&mut host, 5_000).unwrap();
    for t in 5_000..5_010 {
        e.drive(&mut host, t, 20).unwrap();
    }
    assert!(!e.snapshot().unwrap().runs.contains_key(&run));
    let runs = inspect_all(&e, InspectionKind::Runs, Some(&run));
    assert_eq!(runs[0]["outcome"], "ok", "{runs:?}");
    assert!(!e.snapshot().unwrap().events.contains_key(&done));
}
/// Ruling C, the handle found after the hold lapsed and its completion was
/// evicted: binding the handle finds no completion. The operation then
/// resolves by the existing deadline and recovery path: it is past its
/// command deadline, so the next drive marks it `Unknown` again ("operation
/// deadline elapsed with unresolved external outcome"), and recovery
/// reconciles it with the host, whose answer (`ok`) completes it.
#[test]
fn a_handle_found_after_the_hold_lapsed_resolves_by_the_deadline_and_recovery_path() {
    let (e, run, [done, _], mut host) = lapse_fixture();
    e.admit_event("host", correlated_event("after", "inv-a"), 10_100)
        .unwrap();
    assert!(!e.snapshot().unwrap().events.contains_key(&done));
    host.responses.insert(
        "reconcile:test.command".into(),
        vec![
            DispatchResult::Accepted {
                handle: "server-operation".into(),
            },
            DispatchResult::Completed {
                outcome: "ok".into(),
                output: Value::Null,
            },
        ],
    );
    e.recover(&mut host, 10_200).unwrap();
    let op = only_intent(&e.snapshot().unwrap());
    assert_eq!(op.handle.as_deref(), Some("server-operation"));
    assert_eq!(op.completion_event, None, "its completion is gone");
    e.drive(&mut host, 10_201, 20).unwrap();
    let op = only_intent(&e.snapshot().unwrap());
    assert_eq!(op.status, IntentStatus::Unknown);
    assert_eq!(
        op.reason.as_deref(),
        Some("operation deadline elapsed with unresolved external outcome")
    );
    e.recover(&mut host, 10_202).unwrap();
    for t in 10_202..10_212 {
        e.drive(&mut host, t, 20).unwrap();
    }
    assert_eq!(host.reconciliations.len(), 2);
    let runs = inspect_all(&e, InspectionKind::Runs, Some(&run));
    assert_eq!(runs[0]["outcome"], "ok", "{runs:?}");
}
/// Review M1 (fix round 1): a lapsing hold re-judges the events it held in
/// batches of `window_drops_per_commit` per commit (10 here), not all in
/// the commit that lapses it; the remainder continues in later commits
/// (`more` set meanwhile). 25 events held while the handle was lost join
/// the eviction order 10, 10, 5.
#[test]
fn a_lapsing_hold_rejudges_its_events_in_bounded_batches() {
    let e = keep_engine(1_000);
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 600_000,
        event_replay_ms: 10_000,
        replay_keep: 1_000,
        window_drops_per_commit: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let (a, completion) = async_artifact();
    install(&e, a, "async");
    e.admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 10).unwrap();
    let held: Vec<String> = (0..25)
        .map(|i| {
            let envelope = EventEnvelope {
                correlation: BTreeMap::from([("operation".into(), format!("op-{i}"))]),
                ..async_event(&completion, &format!("held-{i:02}"))
            };
            e.admit_event("host", envelope, 500 + i).unwrap().key
        })
        .collect();
    let in_order = |e: &Engine<MemoryStorage>| {
        let s = e.snapshot().unwrap();
        let members = order_members(&s);
        held.iter()
            .filter(|k| members.contains(&format!("e:{k}")))
            .count()
    };
    assert_eq!(in_order(&e), 0);
    let mut progress = Vec::new();
    for t in 0..4 {
        let report = e.collect_garbage(10_100 + t, 10).unwrap();
        progress.push((in_order(&e), report.more));
    }
    assert!(
        matches!(
            progress.as_slice(),
            [(10, true), (20, true), (25, _), (25, false)]
        ),
        "{progress:?}"
    );
    assert!(e.snapshot().unwrap().retention.rejudge.is_empty());
}
/// The batched re-judge restarts from the earliest change: two operations
/// of one contract lost their handles (dispatched at 0 and 1, lapsing at
/// 10,100 and 10,101). The first lapse starts judging the 30 events both
/// held (still held by the second); the second lapse, a commit later,
/// re-judges them from the start, so every one of them joins the order.
#[test]
fn a_second_lapse_rejudges_from_the_earliest_change() {
    let e = keep_engine(1_000);
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 600_000,
        event_replay_ms: 10_000,
        replay_keep: 1_000,
        window_drops_per_commit: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let (a, completion) = async_artifact();
    install(&e, a, "async");
    for t in 0..2 {
        e.admit("async", "main", Value::Null, authority(), t)
            .unwrap();
        e.drive(&mut FakeHost::default(), t, 10).unwrap();
    }
    let held: Vec<String> = (0..30)
        .map(|i| {
            let envelope = EventEnvelope {
                correlation: BTreeMap::from([("operation".into(), format!("op-{i}"))]),
                ..async_event(&completion, &format!("held-{i:02}"))
            };
            e.admit_event("host", envelope, 500 + i).unwrap().key
        })
        .collect();
    let in_order = |e: &Engine<MemoryStorage>| {
        let members = order_members(&e.snapshot().unwrap());
        held.iter()
            .filter(|k| members.contains(&format!("e:{k}")))
            .count()
    };
    let mut joined = vec![];
    for t in 0..5 {
        e.collect_garbage(10_100 + t, 10).unwrap();
        joined.push(in_order(&e));
    }
    assert_eq!(joined, [0, 10, 20, 30, 30]);
}
