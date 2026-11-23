use super::*;

#[test]
fn window_drop_is_bounded_to_64_per_commit_oldest_first() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..65 {
        let d = e.admit_once(keyed(&e, &format!("k-{i:03}")), i).unwrap();
        e.advance(&d.receipt.run_id, i).unwrap();
    }
    assert_eq!(e.snapshot().unwrap().keyed_admissions.len(), 65);
    e.admit_once(keyed(&e, "k-new"), 2_000).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(
        s.keyed_admissions.keys().cloned().collect::<Vec<_>>(),
        vec!["k-064".to_string(), "k-new".to_string()]
    );
}

#[test]
fn keyed_admission_of_a_live_run_outlives_its_window() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_once(keyed(&e, "k"), 0).unwrap();
    e.admit_once(keyed(&e, "other"), 2_000).unwrap();
    let again = e.admit_once(keyed(&e, "k"), 2_001).unwrap();
    assert!(again.duplicate);
    assert_eq!(again.receipt.run_id, first.receipt.run_id);
}

#[test]
fn deferred_entries_do_not_block_later_due_entries() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    install(
        &e,
        artifact(
            "a",
            "done",
            BTreeMap::from([("done".into(), ret("ok"))]),
            FixtureContracts::default(),
        ),
        "a",
    );
    short_windows(&e);
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&waiter, 0).unwrap(); // live wait, minimum revision 2
    e.admit_event("host", event("held", 1), 0).unwrap(); // revision 1: no match; held by the waiter; window 1_000
    for i in 0..65 {
        let d = e
            .admit_once(keyed(&e, &format!("k-{i:03}")), 1_000 + i)
            .unwrap();
        e.advance(&d.receipt.run_id, 1_000 + i).unwrap();
    }
    e.admit_once(keyed(&e, "x"), 3_000).unwrap(); // event deferred to 4_000 + 63 drops
    e.admit_once(keyed(&e, "y"), 3_001).unwrap(); // the remaining 2 drop; the event is not re-examined
    let s = e.snapshot().unwrap();
    assert_eq!(s.keyed_admissions.len(), 2);
    assert_eq!(s.events.len(), 1);
    assert_eq!(
        s.retention
            .expiry
            .keys()
            .next()
            .and_then(|k| retention::window_entry(k))
            .map(|(at, _)| at),
        Some(4_000)
    );
}

#[test]
fn backwards_host_time_neither_refuses_work_nor_shortens_windows() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e.admit("a", "main", Value::Null, authority(), 5_000)
        .unwrap();
    let d = e.admit_once(keyed(&e, "late"), 10).unwrap(); // backwards: accepted, not refused
    e.advance(&d.receipt.run_id, 11).unwrap();
    e.admit("a", "main", Value::Null, authority(), 12).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.retention.clock_ms, Some(5_000));
    assert_eq!(
        s.retention.admission_expires_at_ms.get("late").copied(),
        Some(6_000)
    );
    assert!(s.keyed_admissions.contains_key("late"));
}

/// The 64-drop script: 65 finished keyed runs with a 1 s window, then one
/// admission past it.
fn drop_script<S: Storage>(e: &Engine<S>) {
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..65 {
        let d = e.admit_once(keyed_on(e, &format!("k-{i:03}")), i).unwrap();
        e.advance(&d.receipt.run_id, i).unwrap();
    }
    e.admit_once(keyed_on(e, "k-new"), 2_000).unwrap();
}

#[test]
fn record_storage_drops_like_memory_storage() {
    let m = engine();
    let r = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
    install(&m, done_artifact(), "a");
    install_on(&r, done_artifact(), "a");
    drop_script(&m);
    let transitions = r
        .store
        .stats
        .transitions
        .load(std::sync::atomic::Ordering::Relaxed);
    drop_script(&r);
    assert!(
        r.store
            .stats
            .transitions
            .load(std::sync::atomic::Ordering::Relaxed)
            > transitions
    );
    assert_same_engines(&m, &r, "the 64-drop script");
    assert_eq!(r.snapshot().unwrap().keyed_admissions.len(), 2);
}

#[test]
fn commits_without_host_time_drop_nothing() {
    let e = ret_engine();
    drop_script(&e);
    // k-064 expired at 1_064; the clock is 2_000.
    assert!(e.snapshot().unwrap().keyed_admissions.contains_key("k-064"));
    e.install(wait_artifact()).unwrap();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 2_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.keyed_admissions.contains_key("k-064"));
    assert_eq!(s.retention.clock_ms, Some(2_000));
    e.admit("a", "main", Value::Null, authority(), 2_000)
        .unwrap();
    assert!(!e.snapshot().unwrap().keyed_admissions.contains_key("k-064"));
}

/// Carried from Task 4: a changed keyed receipt is always refused, so an
/// expired key admitted again leaves first and is then accepted as new.
fn expired_key_admitted_again_is_new<S: Storage>(e: &Engine<S>) {
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_once(keyed_on(e, "k"), 0).unwrap();
    assert!(e.advance(&first.receipt.run_id, 0).unwrap());
    // No commit between: the old receipt is still in the State, expired.
    assert!(e.snapshot().unwrap().keyed_admissions.contains_key("k"));
    // A new activation revision changes the request's content: after the
    // window that is not a conflict.
    e.activate("a", &done_artifact().digest, authority())
        .unwrap();
    let again = e.admit_once(keyed_on(e, "k"), 2_000).unwrap();
    assert_reissued(e, &first, &again);
    // The first run is history.
    assert!(
        inspect_all(e, InspectionKind::Runs, Some(&first.receipt.run_id))
            .iter()
            .any(|r| r["status"] == "completed")
    );
}

#[test]
fn memory_storage_admits_an_expired_key_again_as_new() {
    expired_key_admitted_again_is_new(&ret_engine());
}

#[test]
fn record_storage_admits_an_expired_key_again_as_new() {
    let r = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
    install_on(&r, done_artifact(), "a");
    expired_key_admitted_again_is_new(&r);
}

/// Cost (SC-5): a commit with nothing expired reads the same records however
/// many window entries there are, and a commit dropping k entries reads in
/// proportion to k; no window container is read whole.
#[test]
fn window_work_on_record_storage_depends_on_entries_dropped_not_on_entries_kept() {
    use std::sync::atomic::Ordering;
    let measure = |kept: usize, due: usize| {
        let e = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
        install_on(&e, done_artifact(), "a");
        e.set_retention_policy(RetentionPolicy {
            admission_replay_ms: 1_000,
            ..RetentionPolicy::default()
        })
        .unwrap();
        // Due entries expire at 1_000 + i, kept ones at 1_100 + i.
        for i in 0..due {
            let d = e
                .admit_once(keyed_on(&e, &format!("due-{i:03}")), i as i64)
                .unwrap();
            e.advance(&d.receipt.run_id, i as i64).unwrap();
        }
        for i in 0..kept {
            let d = e
                .admit_once(keyed_on(&e, &format!("kept-{i:03}")), 100 + i as i64)
                .unwrap();
            e.advance(&d.receipt.run_id, 100 + i as i64).unwrap();
        }
        let reads = e.store.stats.record_reads.load(Ordering::Relaxed);
        let fallbacks = e.store.stats.fallbacks().len();
        e.admit("a", "main", Value::Null, authority(), 1_050)
            .unwrap();
        let s = e.snapshot().unwrap();
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
    assert_eq!(none_few, none_many, "nothing expired");
    let (eight, scanned) = measure(20, 8);
    assert!(scanned.is_empty(), "{scanned:?}");
    let (eight_many, _) = measure(80, 8);
    assert_eq!(eight, eight_many, "8 dropped");
    let (sixteen, _) = measure(20, 16);
    assert_eq!(
        sixteen - eight,
        eight - none_few,
        "reads grow linearly with drops"
    );
    assert!(eight > none_few);
}

/// SC-10 at admission: an expired key whose run is still live is a
/// duplicate even before the drop has deferred it.
#[test]
fn an_expired_key_of_a_live_run_is_still_a_duplicate() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_once(keyed(&e, "k"), 0).unwrap();
    let again = e.admit_once(keyed(&e, "k"), 2_000).unwrap();
    assert!(again.duplicate);
    assert_eq!(again.receipt, first.receipt);
    // An exact replay writes nothing; the next commit's drop defers it.
    let s = e.snapshot().unwrap();
    assert_eq!(
        s.retention.admission_expires_at_ms.get("k").copied(),
        Some(1_000)
    );
    e.admit("a", "main", Value::Null, authority(), 2_000)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(
        s.retention.admission_expires_at_ms.get("k").copied(),
        Some(3_000)
    );
}

/// A window written with a host time behind the clock, but whose window
/// still reaches past it, counts from the clock.
#[test]
fn a_keyed_window_written_behind_the_clock_counts_from_the_clock() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e.admit("a", "main", Value::Null, authority(), 5_000)
        .unwrap();
    e.admit_once(keyed(&e, "behind"), 4_500).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.keyed_admissions["behind"].accepted_at_ms, 4_500);
    assert_eq!(
        s.retention.admission_expires_at_ms.get("behind").copied(),
        Some(6_000)
    );
    assert!(window_entries(&s).contains(&(6_000, "k:behind".to_string())));
}

fn assert_reissued(
    e: &Engine<impl Storage>,
    first: &KeyedAdmissionDisposition,
    again: &KeyedAdmissionDisposition,
) {
    assert!(!again.duplicate);
    assert_ne!(again.receipt.run_id, first.receipt.run_id);
    assert_eq!(again.receipt.accepted_at_ms, 2_000);
    let s = e.snapshot().unwrap();
    assert_eq!(s.keyed_admissions["k"], again.receipt);
    assert_eq!(
        s.retention.admission_expires_at_ms.get("k").copied(),
        Some(3_000)
    );
    assert_eq!(window_entries(&s), vec![(3_000, "k:k".to_string())]);
}
