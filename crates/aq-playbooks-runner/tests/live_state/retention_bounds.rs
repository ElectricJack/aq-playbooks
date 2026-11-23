use super::*;

/// The owner's rule "no hard-coded limits": the window drop, the eviction,
/// the history trim and the GC page bound are policy fields with the old
/// constants as defaults, read from the stored policy, and must be
/// positive.
#[test]
fn per_commit_bounds_are_policy_fields_read_from_the_stored_policy() {
    let defaults = RetentionPolicy::default();
    let bounds = |p: &RetentionPolicy| {
        (
            p.window_drops_per_commit,
            p.replay_evictions_per_commit,
            p.history_trim_per_commit,
            p.gc_page_limit,
            p.settle_cohorts_per_commit,
        )
    };
    assert_eq!(bounds(&defaults), (64, 64, 64, 1_000, 50));
    let decoded: RetentionPolicy = serde_json::from_value(
        json!({"event_replay_ms":1,"terminal_run_ms":1,"audit_ms":1,"max_tombstones":1}),
    )
    .unwrap();
    assert_eq!(bounds(&decoded), (64, 64, 64, 1_000, 50));
    check_window_drop_bound();
    check_eviction_bound();
    check_history_and_gc_bounds();
}
/// The eviction licence reads its bound from the policy: a commit that
/// evicts more than `replay_evictions_per_commit` entries is refused.
#[test]
fn the_eviction_licence_reads_its_bound_from_the_policy() {
    let store = MemoryStorage::default();
    let mut state = bulk_eviction_state();
    state.retention.policy.replay_evictions_per_commit = 2;
    let revision = seed_eviction_state(&store, "ns", &state);
    let mut next = store.load("ns").unwrap().state;
    for key in ["b000", "b001", "b002"] {
        evict_keyed(&mut next, key);
    }
    assert!(refused_with(
        store.compare_and_swap("ns", "w", revision, &next),
        retention::REFUSE_EVICTION
    ));
    let mut next = store.load("ns").unwrap().state;
    for key in ["b000", "b001"] {
        evict_keyed(&mut next, key);
    }
    store.compare_and_swap("ns", "w", revision, &next).unwrap();
}

fn check_window_drop_bound() {
    // The window drop: 10 expired keyed receipts, 3 per commit.
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        window_drops_per_commit: 3,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..10 {
        keyed_done(&e, &format!("k-{i:02}"), i);
    }
    e.admit("a", "main", Value::Null, authority(), 5_000)
        .unwrap();
    assert_eq!(e.snapshot().unwrap().keyed_admissions.len(), 7);
}

fn check_eviction_bound() {
    // The eviction: `replay_keep` lowered to 1 over 7 unheld entries, 2
    // evicted per commit; `collect_garbage` reports them and `more` until
    // the set is down to `replay_keep`.
    let v = keep_engine(10);
    for i in 0..7 {
        keyed_done(&v, &format!("v-{i:02}"), i);
    }
    v.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 600_000,
        replay_keep: 1,
        replay_evictions_per_commit: 2,
        ..RetentionPolicy::default()
    })
    .unwrap();
    assert_eq!(v.snapshot().unwrap().keyed_admissions.len(), 5);
    let r = v.collect_garbage(10, 10).unwrap();
    assert_eq!((r.evicted, r.more), (2, true));
    let r = v.collect_garbage(11, 10).unwrap();
    assert_eq!((r.evicted, r.more), (2, false));
    assert_eq!(v.snapshot().unwrap().keyed_admissions.len(), 1);
}

fn check_history_and_gc_bounds() {
    // The history trim: 6 finished runs, `history_keep` lowered to 1, one
    // more than written per commit.
    let h = Engine::new(MemoryStorage::default(), "test", "i").unwrap();
    install(&h, done_artifact(), "a");
    for t in 0..6 {
        let id = h.admit("a", "main", Value::Null, authority(), t).unwrap();
        h.advance(&id, t).unwrap();
    }
    h.set_retention_policy(RetentionPolicy {
        history_keep: 1,
        history_trim_per_commit: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let id = h.admit("a", "main", Value::Null, authority(), 10).unwrap();
    h.advance(&id, 10).unwrap();
    // 7 runs written; the finishing commit wrote 1 and trimmed 1 + 1.
    assert_eq!(inspect_all(&h, InspectionKind::Runs, None).len(), 5);
    // The GC page bound.
    h.set_retention_policy(RetentionPolicy {
        gc_page_limit: 5,
        ..RetentionPolicy::default()
    })
    .unwrap();
    assert!(matches!(
        h.collect_garbage(20, 6),
        Err(Error::Refused(m)) if m == "GC page limit must be 1..5"
    ));
    h.collect_garbage(20, 5).unwrap();
    for bad in [
        RetentionPolicy {
            window_drops_per_commit: 0,
            ..RetentionPolicy::default()
        },
        RetentionPolicy {
            replay_evictions_per_commit: 0,
            ..RetentionPolicy::default()
        },
        RetentionPolicy {
            history_trim_per_commit: 0,
            ..RetentionPolicy::default()
        },
        RetentionPolicy {
            gc_page_limit: 0,
            ..RetentionPolicy::default()
        },
        RetentionPolicy {
            settle_cohorts_per_commit: 0,
            ..RetentionPolicy::default()
        },
    ] {
        assert!(matches!(
            h.set_retention_policy(bad),
            Err(Error::Refused(m)) if m == "per-commit retention bounds must be positive"
        ));
    }
}
