use super::*;

/// The plan's test: `collect_garbage` is optional; it runs the same bounded
/// work as every commit (here the window drop, 64 per commit) and reports
/// it. Its clock never moves back.
#[test]
fn collect_garbage_is_optional_and_reports_the_bounded_drop() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..70 {
        let d = e.admit_once(keyed(&e, &format!("k{i:03}")), i).unwrap();
        e.advance(&d.receipt.run_id, i).unwrap();
    }
    let report = e.collect_garbage(5_000, 10).unwrap();
    assert_eq!((report.runs, report.admissions, report.more), (0, 64, true));
    let report = e.collect_garbage(5_001, 10).unwrap();
    assert_eq!((report.admissions, report.more), (6, false));
    assert!(e.snapshot().unwrap().retention.disposals.is_empty());
    assert!(matches!(
        e.collect_garbage(4_000, 10),
        Err(Error::Refused(_))
    ));
}
/// `collect_garbage` creates nothing the rest of the engine has stopped
/// creating: an expired event leaves by the commit's window drop, with no
/// event tombstone, correlation fence or disposal record, and a redelivery
/// is new.
#[test]
fn collect_garbage_writes_no_tombstone_fence_or_disposal() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        audit_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_event("host", event("e1", 2), 0).unwrap();
    let report = e.collect_garbage(5_000, 100).unwrap();
    assert_eq!((report.events, report.runs), (1, 0));
    let s = e.snapshot().unwrap();
    assert!(!s.events.contains_key(&first.key));
    assert_eq!(
        (
            s.retention.event_tombstones.len(),
            s.retention.expired_correlations.len(),
            s.retention.disposals.len()
        ),
        (0, 0, 0),
        "legacy retention records"
    );
    assert!(
        !e.admit_event("host", event("e1", 2), 5_001)
            .unwrap()
            .duplicate
    );
}
/// On a namespace an older engine wrote, `collect_garbage` advances the
/// same drain a commit does: each call moves the next bounded batch of
/// settled cohorts to history (reported as `runs`) and drops legacy
/// tombstones, and reports `more` until the drain is done. No run leaves
/// without its history.
#[test]
fn collect_garbage_on_a_legacy_namespace_advances_the_drain() {
    let e = Engine::new(
        legacy_with(120, 30, |s| s.retention.policy.audit_ms = 1),
        "test",
        "i-1",
    )
    .unwrap();
    assert_eq!(e.snapshot().unwrap().runs.len(), 70);
    let mut moved = vec![];
    let mut calls = 0;
    loop {
        calls += 1;
        assert!(calls < 10);
        let report = e.collect_garbage(1_000_000 + calls, 1_000).unwrap();
        moved.push(report.runs);
        if !report.more {
            break;
        }
    }
    assert_eq!(&moved[..2], &[50, 20], "{moved:?}");
    let s = e.snapshot().unwrap();
    assert!(s.retention.drain.done && s.runs.is_empty());
    assert!(s.retention.event_tombstones.is_empty() && s.retention.disposals.is_empty());
    assert_eq!(legacy_ids(&e).len(), 120);
}
/// A finished run is never removed without its history, by any call: even
/// over a State whose drain claims to be done while settled cohorts remain
/// (the case the old collector removed without history), `collect_garbage`
/// removes nothing without history and writes no disposal record.
#[test]
fn collect_garbage_never_removes_a_finished_run_without_history() {
    let store = legacy_with(5, 0, |s| s.retention.policy.audit_ms = 1);
    let e = Engine::new(store.clone(), "test", "i-1").unwrap();
    // The drain has taken the 5 cohorts; put 5 more back behind a drain
    // marked done, as no engine writes it.
    let mut s = e.snapshot().unwrap();
    for i in 0..5 {
        let id = format!("run-0-stale-{i:04}");
        s.runs.insert(id.clone(), completed_run(&id));
        s.indexes
            .root_members
            .entry(id.clone())
            .or_default()
            .insert(id.clone());
        s.indexes.root_last_activity.insert(id.clone(), 1);
        s.indexes.run_roots.insert(id.clone(), id);
    }
    let revision = e.store.load("test").unwrap().revision;
    e.store
        .compare_and_swap("test", "i-1", revision, &s)
        .unwrap();
    assert!(e.snapshot().unwrap().retention.drain.done);
    let report = e.collect_garbage(1_000_000, 1_000).unwrap();
    assert_eq!(report.runs, 0);
    let s = e.snapshot().unwrap();
    assert_eq!(
        s.runs.len(),
        5,
        "left as they are, never removed without history"
    );
    assert!(s.retention.disposals.is_empty());
}
