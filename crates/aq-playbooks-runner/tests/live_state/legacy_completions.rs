use super::*;

/// The carried hazard, bounded: an older engine's namespace with many
/// events of one completion contract, before and after the dispatch of an
/// asynchronous operation of that contract whose handle is lost (no
/// dispatch sequence, the old engine's shape). The drain judges the legacy
/// events a bounded number per commit (entries bound 8 here); a change of
/// the contract's uncertain operations before the drain has reached an
/// event leaves it to the drain. So when the host finds the handle in the
/// first commits after the upgrade (`recover`, mid-drain), no commit — the
/// open commit, the binding one, nor a drain commit — judges all of them:
/// on bound record containers the most any one commit reads does not
/// depend on how many there are. With the handle found after the drain,
/// the events it held rejoin through `contract_events`, which the drain
/// filled. Either way every event ends unheld in the eviction order and the
/// run finishes `ok`.
fn legacy_hazard(before: usize, after: usize, bind_mid_drain: bool) -> usize {
    let LegacyCompletion {
        store,
        mut host,
        run,
        done,
    } = legacy_completion(before, after);
    let stats = store.stats.clone();
    let mut work = vec![];
    let e = measure(&stats, &mut work, || {
        Engine::new(store, "test", "i-2").unwrap()
    });
    host.responses.insert(
        "reconcile:test.command".into(),
        vec![DispatchResult::Accepted {
            handle: "server-operation".into(),
        }],
    );
    if bind_mid_drain {
        measure(&stats, &mut work, || {
            e.recover(&mut host, 1_002).unwrap();
        });
    }
    let mut t = 1_003;
    while !e.snapshot().unwrap().retention.drain.done {
        measure(&stats, &mut work, || e.install(done_artifact()).unwrap());
        t += 1;
        assert!(t < 2_000);
    }
    if !bind_mid_drain {
        e.recover(&mut host, t).unwrap();
    }
    for t in t..t + 5 {
        e.drive(&mut host, t, 20).unwrap();
    }
    let s = e.snapshot().unwrap();
    let runs = inspect_all(&e, InspectionKind::Runs, Some(&run));
    assert_eq!(runs[0]["outcome"], "ok", "{runs:?}");
    // Every event (the delivered completion too) is unheld and in the
    // eviction order.
    assert_eq!(
        s.retention.replay_order.len(),
        before + after + 1,
        "{work:?}"
    );
    assert!(s.events.contains_key(&done) && s.indexes.contract_events_complete);
    work.into_iter().max().unwrap()
}
struct LegacyCompletion {
    store: RecordStorage,
    host: FakeHost,
    run: String,
    done: String,
}
fn legacy_completion(before: usize, after: usize) -> LegacyCompletion {
    let e = Engine::new(RecordStorage::new(), "test", "i-1").unwrap();
    let (a, completion) = async_artifact();
    install_on(&e, a, "async");
    install_on(&e, done_artifact(), "a");
    let accept = |label: &str, i: usize, t: i64| {
        let envelope = EventEnvelope {
            correlation: BTreeMap::from([("operation".into(), format!("{label}-{i}"))]),
            ..async_event(&completion, &format!("{label}{i}"))
        };
        e.admit_event("host", envelope, t).unwrap();
    };
    for i in 0..before {
        accept("b", i, i as i64);
    }
    let run = e
        .admit("async", "main", Value::Null, authority(), 1_000)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 1_000, 10).unwrap();
    for i in 0..after {
        accept("a", i, 1_001);
    }
    let done = e
        .admit_event("host", async_event(&completion, "done"), 1_001)
        .unwrap()
        .key;
    let mut legacy = old_engine_shape(&e.snapshot().unwrap());
    legacy.retention.policy.drain_entries_per_commit = 8;
    legacy.retention.policy.replay_keep = 100_000;
    e.release().unwrap();
    let store = fresh_legacy(RecordStorage::new(), &legacy);
    LegacyCompletion {
        store,
        host,
        run,
        done,
    }
}
fn measure<T>(
    stats: &transition::RecordStats,
    work: &mut Vec<usize>,
    operation: impl FnOnce() -> T,
) -> T {
    use std::sync::atomic::Ordering;
    let before = stats.record_reads.load(Ordering::Relaxed);
    let result = operation();
    work.push(stats.record_reads.load(Ordering::Relaxed) - before);
    result
}
#[test]
fn legacy_events_are_ordered_in_bounded_steps_and_never_all_rejudged_at_once() {
    // A drain commit reads the same for any number of events (8 judged).
    // The binding commits re-judge only held events the drain has already
    // reached (at most a few commits' bound), so their cost varies with
    // which events those are, not with how many there are: three times the
    // events cost no more (10 % allowed for which ones were reached).
    let small = legacy_hazard(40, 40, true);
    let large = legacy_hazard(120, 120, true);
    assert!(large <= small + small / 10, "{large} vs {small}");
    legacy_hazard(20, 20, false);
}
/// The same when the open commit rebuilds the indexes (a State whose
/// indexes were never initialized): the rebuild puts every event into
/// `contract_events` at once, and the drain still judges the older
/// engine's events itself. When the handle of the operation that held 40
/// of them is found mid-drain, they do not all join the eviction order in
/// that commit: only those the drain has reached; the drain then judges the
/// rest, 4 per commit here.
#[test]
fn a_rebuilt_legacy_namespace_leaves_its_events_to_the_drain() {
    let e = engine();
    let (a, completion) = async_artifact();
    install(&e, a, "async");
    e.admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    for i in 0..40 {
        let envelope = EventEnvelope {
            correlation: BTreeMap::from([("operation".into(), format!("after-{i}"))]),
            ..async_event(&completion, &format!("after-{i}"))
        };
        e.admit_event("host", envelope, 1).unwrap();
    }
    let mut legacy = old_engine_shape(&e.snapshot().unwrap());
    legacy.retention.policy.drain_entries_per_commit = 4;
    legacy.indexes.initialized = false;
    e.release().unwrap();
    let e = Engine::new(
        fresh_legacy(MemoryStorage::retaining_settled(), &legacy),
        "test",
        "i-1",
    )
    .unwrap();
    let held = e.snapshot().unwrap();
    assert!(
        held.indexes.contract_events_complete,
        "the rebuild filled it"
    );
    assert!(held.retention.replay_order.len() <= 4);
    host.responses.insert(
        "reconcile:test.command".into(),
        vec![DispatchResult::Accepted {
            handle: "server-operation".into(),
        }],
    );
    e.recover(&mut host, 2).unwrap();
    // `recover` is two commits: the drain reached at most 3 × 4 events.
    let s = e.snapshot().unwrap();
    assert!(s.intents.values().all(|i| i.handle.is_some()));
    assert!(
        s.retention.replay_order.len() <= 12,
        "{} joined at once",
        s.retention.replay_order.len()
    );
    while !e.snapshot().unwrap().retention.drain.done {
        e.install(done_artifact()).unwrap();
    }
    assert_eq!(e.snapshot().unwrap().retention.replay_order.len(), 40);
}
