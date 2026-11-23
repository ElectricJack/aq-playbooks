use super::*;
use crate::KeyedAdmissionReceipt;
/// A plain (format 2) State at clock 1_000 with `due` expired keyed
/// receipts (each its own expiry) and `kept` unexpired ones.
fn state(kept: usize, due: usize) -> State {
    let mut s = State::default();
    s.retention.clock_ms = Some(1_000);
    let expiries = (0..due)
        .map(|i| 500 + i as i64)
        .chain((0..kept).map(|i| 2_000 + i as i64));
    for (i, at) in expiries.enumerate() {
        let key = format!("k-{i:05}");
        s.keyed_admissions.insert(
            key.clone(),
            KeyedAdmissionReceipt {
                key: key.clone(),
                request_digest: "digest".into(),
                run_id: format!("run-{i}"),
                activation_revision: 1,
                catalog_revision: 1,
                accepted_at_ms: 0,
            },
        );
        s.retention.admission_expires_at_ms.insert(key.clone(), at);
        window_insert(&mut s, at, format!("{WINDOW_ADMISSION}{key}"));
    }
    s
}
fn steps(kept: usize, due: usize) -> (usize, CommitReport) {
    let mut s = state(kept, due);
    let mut report = CommitReport::default();
    WINDOW_STEPS.with(|steps| steps.set(0));
    drop_expired(&mut s, WINDOW_DROPS_PER_COMMIT, &mut report).unwrap();
    assert_eq!(s.keyed_admissions.len(), kept + due - report.admissions);
    (WINDOW_STEPS.with(std::cell::Cell::get), report)
}
#[test]
fn window_work_on_a_plain_state_depends_on_entries_dropped_not_on_entries_kept() {
    // Nothing expired: one ordered lookup, however many entries are kept.
    assert_eq!(steps(10, 0).0, 1);
    assert_eq!(steps(10_000, 0).0, 1);
    // k dropped: one lookup, then one member and one lookup per entry.
    for k in [1, 8, 64] {
        let (work, report) = steps(10_000, k);
        assert_eq!((work, report.admissions), (1 + 2 * k, k), "k = {k}");
    }
    // More than 64 due: still 64.
    let (work, report) = steps(10_000, 200);
    assert_eq!((work, report.admissions), (1 + 2 * 64, 64));
}
#[test]
fn a_member_behind_its_items_expiry_moves_to_it() {
    let mut s = state(0, 1);
    // The record says 2_000; the member is still at 500.
    s.retention
        .admission_expires_at_ms
        .insert("k-00000".into(), 2_000);
    let mut report = CommitReport::default();
    drop_expired(&mut s, WINDOW_DROPS_PER_COMMIT, &mut report).unwrap();
    assert_eq!(report, CommitReport::default());
    assert!(s.keyed_admissions.contains_key("k-00000"));
    assert_eq!(
        s.retention.expiry.into_plain(),
        BTreeMap::from([(window_key(2_000, "k:k-00000"), ())])
    );
}
#[test]
fn window_keys_sort_by_expiry_then_member() {
    let at = [i64::MIN, -1, 0, 1, 999, 1_000, i64::MAX];
    let keys: Vec<String> = at.iter().map(|at| window_key(*at, "k:x")).collect();
    assert!(keys.windows(2).all(|w| w[0] < w[1]), "{keys:?}");
    assert!(window_key(5, "e:z") < window_key(6, "e:a"));
    assert!(window_key(5, "e:a") < window_key(5, "k:a"));
    for at in at {
        assert_eq!(window_entry(&window_key(at, "k:x")), Some((at, "k:x")));
    }
}
/// A plain (format 2) State with `unheld` finished keyed receipts in the
/// eviction order and `replay_keep` `keep`.
fn ordered(unheld: usize, keep: usize) -> State {
    let mut s = State::default();
    s.retention.clock_ms = Some(1_000);
    s.retention.policy.replay_keep = keep;
    for i in 0..unheld {
        let key = format!("k-{i:05}");
        let receipt = KeyedAdmissionReceipt {
            key: key.clone(),
            request_digest: "digest".into(),
            run_id: format!("run-{:016x}", i + 1),
            activation_revision: 1,
            catalog_revision: 1,
            accepted_at_ms: 0,
        };
        s.retention
            .replay_order
            .insert(admission_order_key(&receipt), ());
        s.keyed_admissions.insert(key.clone(), receipt);
        s.retention
            .admission_expires_at_ms
            .insert(key.clone(), 9_000);
        window_insert(&mut s, 9_000, format!("{WINDOW_ADMISSION}{key}"));
    }
    s
}
fn eviction_steps(unheld: usize, keep: usize) -> (usize, CommitReport) {
    let mut s = ordered(unheld, keep);
    let mut report = CommitReport::default();
    WINDOW_STEPS.with(|steps| steps.set(0));
    evict(&mut s, REPLAY_EVICTIONS_PER_COMMIT, &mut report).unwrap();
    assert_eq!(s.keyed_admissions.len(), unheld - report.evicted);
    assert_eq!(s.retention.replay_order.len(), unheld - report.evicted);
    assert_eq!(s.retention.expiry.len(), unheld - report.evicted);
    (WINDOW_STEPS.with(std::cell::Cell::get), report)
}
/// Eviction work depends on the entries evicted, not on those kept: at
/// or under `replay_keep` a commit only counts (no lookup), and k
/// evictions are one lookup plus two steps each, at most 64.
#[test]
fn eviction_work_on_a_plain_state_depends_on_entries_evicted_not_on_entries_kept() {
    assert_eq!(eviction_steps(10, 256).0, 0);
    assert_eq!(eviction_steps(256, 256).0, 0);
    assert_eq!(eviction_steps(5_000, 5_000).0, 0);
    for k in [1, 8, 64] {
        for kept in [10, 5_000] {
            let (work, report) = eviction_steps(kept + k, kept);
            assert_eq!(
                (work, report.evicted),
                (1 + 2 * k, k),
                "k = {k}, kept {kept}"
            );
        }
    }
    let (work, report) = eviction_steps(500, 100);
    assert_eq!((work, report.evicted), (1 + 2 * 64, 64));
}
/// The order is one record per entry: a burst in one millisecond adds
/// records, it does not grow one.
#[test]
fn a_same_millisecond_burst_is_one_record_per_entry() {
    let mut s = State::default();
    for i in 0..100 {
        window_insert(&mut s, 7, format!("{WINDOW_EVENT}e-{i:03}"));
    }
    assert_eq!(s.retention.expiry.len(), 100);
    assert!(s.retention.expiry.keys().all(|k| k.len() == 16 + 2 + 5));
}
/// A plain State with `n` settled one-run cohorts, indexed.
fn settled_cohorts(n: usize) -> State {
    let mut s = State::default();
    for i in 0..n {
        let id = format!("run-{i:04}");
        s.sequence += 1;
        s.receipts.push(Receipt {
            sequence: s.sequence,
            run: id.clone(),
            step: "ok".into(),
            at_ms: 1,
            kind: "terminal".into(),
            detail: serde_json::Value::Null,
        });
        s.runs.insert(
            id.clone(),
            Run {
                catalog_revision: 0,
                id: id.clone(),
                root: id.clone(),
                parent: None,
                origin: None,
                artifact: "artifact".into(),
                entry: "main".into(),
                dependencies: BTreeMap::new(),
                authority: crate::Authority {
                    actor: "a".into(),
                    scope: "project".into(),
                    capabilities: BTreeSet::new(),
                },
                step: "ok".into(),
                status: crate::RunStatus::Completed,
                input: serde_json::Value::Null,
                variables: BTreeMap::new(),
                outcome: Some("ok".into()),
                output: None,
                reason: None,
                admitted_at_ms: 1,
                visits: 1,
                depth: 0,
                children: vec![],
                pending: None,
                loops: vec![],
                attempts: BTreeMap::new(),
                pending_return: None,
                pending_failure: false,
                terminal_sequence: Some(s.sequence),
            },
        );
    }
    crate::indexes::rebuild(&mut s).unwrap();
    s
}
/// The brief's unit test (Review Focus 2): a commit with more settled
/// candidates than `COHORTS_PER_COMMIT` re-arms the drain's cohort scan,
/// which then moves the rest, so none is stranded.
#[test]
fn the_settle_bound_comes_from_the_policy() {
    assert_eq!(RetentionPolicy::default().settle_cohorts_per_commit, 50);
    let mut s = settled_cohorts(20);
    s.retention.policy.settle_cohorts_per_commit = 7;
    s.retention.drain = DrainState {
        done: true,
        roots_done: true,
        phase: DRAIN_DONE,
        ..DrainState::default()
    };
    let candidates: BTreeSet<String> = s.runs.keys().cloned().collect();
    let started = s.sequence;
    let report = finish_commit(&mut s, None, candidates, started).unwrap();
    // 7 settled by the commit; the re-armed drain takes its own bound
    // (50, less the 7) of the rest in the same commit.
    assert_eq!((report.cohorts, s.runs.len()), (20, 0));
    let mut s = settled_cohorts(20);
    s.retention.policy.settle_cohorts_per_commit = 7;
    s.retention.policy.drain_cohorts_per_commit = 7;
    s.retention.drain = DrainState {
        done: true,
        roots_done: true,
        phase: DRAIN_DONE,
        ..DrainState::default()
    };
    let candidates: BTreeSet<String> = s.runs.keys().cloned().collect();
    let started = s.sequence;
    let report = finish_commit(&mut s, None, candidates, started).unwrap();
    assert_eq!((report.cohorts, s.runs.len()), (7, 13));
    assert!(report.more);
}
#[test]
fn settle_cap_rearms_the_drain() {
    let mut s = settled_cohorts(60);
    s.retention.drain = DrainState {
        done: true,
        roots_done: true,
        phase: DRAIN_DONE,
        ..DrainState::default()
    };
    let candidates: BTreeSet<String> = s.runs.keys().cloned().collect();
    let mut report = CommitReport::default();
    settle(&mut s, candidates, COHORTS_PER_COMMIT, &mut report).unwrap();
    assert_eq!(s.runs.len(), 10);
    assert!(report.more);
    assert!(!s.retention.drain.done && !s.retention.drain.roots_done);
    assert_eq!(s.retention.drain.root_cursor, None);
    let started = s.sequence;
    drain_step(&mut s, started, &mut CommitReport::default()).unwrap();
    assert_eq!(s.runs.len(), 0);
    assert!(s.retention.drain.done);
}
/// A drained namespace pays one check per commit: `drain_step` does no
/// work, however large the State.
#[test]
fn a_drained_namespace_does_no_drain_work() {
    let mut s = settled_cohorts(200);
    s.retention.drain.done = true;
    DRAIN_STEPS.with(|steps| steps.set(0));
    let mut report = CommitReport::default();
    drain_step(&mut s, 0, &mut report).unwrap();
    assert_eq!(DRAIN_STEPS.with(std::cell::Cell::get), 0);
    assert_eq!((report, s.runs.len()), (CommitReport::default(), 200));
    s.retention.drain.done = false;
    drain_step(&mut s, 0, &mut CommitReport::default()).unwrap();
    assert_eq!(DRAIN_STEPS.with(std::cell::Cell::get), 1);
}
/// A State written by Task 6's first commit (`expiry` as expiry → set of
/// members) is refused when decoded, not silently emptied.
#[test]
fn the_first_task_6_expiry_shape_is_refused() {
    let old = serde_json::json!({"1000": ["k:x"]});
    assert!(serde_json::from_value::<LazyMap<String, ()>>(old).is_err());
}
