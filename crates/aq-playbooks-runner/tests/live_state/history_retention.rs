use super::*;

fn history_keep_engine<S: Storage>(store: S, keep: u64) -> Engine<S> {
    let e = Engine::new(store, "test", "incarnation-1").unwrap();
    install_on(&e, done_artifact(), "a");
    e.set_retention_policy(RetentionPolicy {
        history_keep: keep,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e
}

fn history_items<S: Storage>(e: &Engine<S>, kind: InspectionKind) -> Vec<Value> {
    e.inspect(&InspectionQuery {
        kind,
        run: None,
        after: None,
        limit: 1000,
    })
    .unwrap()
    .items
}

/// With `history_keep` 5, after 20 finished runs inspection lists the
/// newest 5, and history holds exactly those runs' items (and the receipts
/// of no run: admin).
fn history_keeps_the_newest_runs<S: Storage>(store: S) {
    let e = history_keep_engine(store, 5);
    let runs: Vec<String> = (0..20)
        .map(|t| {
            let id = e.admit("a", "main", Value::Null, authority(), t).unwrap();
            assert!(e.advance(&id, t).unwrap());
            id
        })
        .collect();
    assert!(e.snapshot().unwrap().runs.is_empty());
    let listed: Vec<String> = history_items(&e, InspectionKind::Runs)
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(listed, runs[15..]);
    for item in history_items(&e, InspectionKind::Receipts) {
        let run = item["run"].as_str().unwrap();
        assert!(
            run == "admin" || runs[15..].iter().any(|r| r == run),
            "{item}"
        );
    }
    assert!(history_items(&e, InspectionKind::Receipts).len() >= 10);
}

#[test]
fn memory_storage_history_keeps_the_newest_runs() {
    history_keeps_the_newest_runs(MemoryStorage::default());
}

#[test]
fn record_storage_history_keeps_the_newest_runs() {
    history_keeps_the_newest_runs(RecordStorage::new());
}

/// One commit settling 100 cohorts (more than the trim's spare 64): the
/// trim keeps up, and history holds the newest `history_keep`.
fn trim_keeps_up_with_one_commit(store: &impl Storage) {
    store.acquire("ns", "w").unwrap();
    let mut state = State::default();
    state.retention.policy.history_keep = 5;
    state.settled = (0..100)
        .map(|i| synthetic_cohort(&format!("c{i:03}"), i, &[i as u64 + 1]))
        .collect();
    store.compare_and_swap("ns", "w", 0, &state).unwrap();
    let ids: Vec<Value> = page(store, InspectionKind::Runs, None, 1000)
        .items
        .iter()
        .map(|v| v["id"].clone())
        .collect();
    assert_eq!(ids, ["c095", "c096", "c097", "c098", "c099"]);
    assert_eq!(
        page(store, InspectionKind::Receipts, None, 1000)
            .items
            .len(),
        5
    );
}

#[test]
fn memory_storage_trim_keeps_up_with_one_commit() {
    trim_keeps_up_with_one_commit(&MemoryStorage::default());
}

#[test]
fn record_storage_trim_keeps_up_with_one_commit() {
    trim_keeps_up_with_one_commit(&RecordStorage::new());
}

/// `history_keep` 0: no history is written; finished runs are not found.
#[test]
fn history_keep_zero_writes_no_history() {
    let e = history_keep_engine(MemoryStorage::default(), 0);
    let id = e.admit("a", "main", Value::Null, authority(), 0).unwrap();
    assert!(e.advance(&id, 0).unwrap());
    assert!(e.snapshot().unwrap().runs.is_empty());
    for kind in [InspectionKind::Runs, InspectionKind::Receipts] {
        assert!(history_items(&e, kind).is_empty());
    }
    assert_eq!(RetentionPolicy::default().history_keep, 10_000);
}

/// `delete_history` (by time) still works beside the trim, and the trim
/// counts what it leaves: 10 runs keep the newest 5; deleting those
/// finished before 7 leaves 3; 4 more keep the newest 5 again.
fn delete_history_and_the_trim<S: Storage>(store: S) {
    let e = history_keep_engine(store, 5);
    let admit = |t: i64| {
        let id = e.admit("a", "main", Value::Null, authority(), t).unwrap();
        assert!(e.advance(&id, t).unwrap());
        id
    };
    let runs: Vec<String> = (0..10).map(admit).collect();
    let listed = |e: &Engine<S>| -> Vec<String> {
        history_items(e, InspectionKind::Runs)
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(listed(&e), runs[5..]);
    assert!(e.delete_history_before(7).unwrap() > 0);
    assert_eq!(listed(&e), runs[7..]);
    let more: Vec<String> = (10..14).map(admit).collect();
    let mut expected = runs[9..].to_vec();
    expected.extend(more);
    assert_eq!(listed(&e), expected);
}

#[test]
fn memory_storage_delete_history_and_the_trim() {
    delete_history_and_the_trim(MemoryStorage::default());
}

#[test]
fn record_storage_delete_history_and_the_trim() {
    delete_history_and_the_trim(RecordStorage::new());
}

/// A page that straddles a trim neither repeats nor panics: its cursor's
/// run is trimmed between pages; the next page starts after the cursor.
#[test]
fn a_page_across_a_trim_neither_repeats_nor_panics() {
    let e = history_keep_engine(MemoryStorage::default(), 3);
    let admit = |t: i64| {
        let id = e.admit("a", "main", Value::Null, authority(), t).unwrap();
        assert!(e.advance(&id, t).unwrap());
        id
    };
    let first: Vec<String> = (0..3).map(admit).collect();
    let query = |after: Option<String>| InspectionQuery {
        kind: InspectionKind::Runs,
        run: None,
        after,
        limit: 2,
    };
    let page_one = e.inspect(&query(None)).unwrap();
    assert_eq!(page_one.items.len(), 2);
    let cursor = page_one.next.clone().unwrap();
    let later: Vec<String> = (3..5).map(admit).collect();
    let page_two = e.inspect(&query(Some(cursor.clone()))).unwrap();
    let ids: Vec<String> = page_two
        .items
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, [first[2].clone(), later[0].clone()]);
    assert!(ids.iter().all(|id| *id > cursor));
}

/// Review I2: the trim keeps the newest runs by the namespace's sequence,
/// not by host finish time: after a host clock that ran ahead, the newest
/// three finished runs survive.
fn the_trim_orders_by_sequence_not_finish_time<S: Storage>(store: S) {
    let e = history_keep_engine(store, 3);
    let run = |t: i64| {
        let id = e.admit("a", "main", Value::Null, authority(), t).unwrap();
        assert!(e.advance(&id, t).unwrap());
        id
    };
    let pinned: Vec<String> = (0..3).map(|_| run(9_000_000_000_000)).collect();
    let later: Vec<String> = (0..2).map(|_| run(1_000)).collect();
    let listed: Vec<String> = history_items(&e, InspectionKind::Runs)
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        listed,
        [pinned[2].clone(), later[0].clone(), later[1].clone()]
    );
}

#[test]
fn memory_storage_trims_by_sequence_not_finish_time() {
    the_trim_orders_by_sequence_not_finish_time(MemoryStorage::default());
}

#[test]
fn record_storage_trims_by_sequence_not_finish_time() {
    the_trim_orders_by_sequence_not_finish_time(RecordStorage::new());
}

/// Review I3: nothing in history is exempt from the rolling bound. 1,000
/// events whose destination is blocked (each writes a `destination_blocked`
/// receipt, which goes to history when the event leaves) and 50 operator
/// actions (admin receipts) leave at most `history_keep` of each.
fn receipts_of_no_run_are_rolling_too<S: Storage>(store: S) {
    let e = Engine::new(store, "test", "incarnation-1").unwrap();
    install_on(&e, input_blocked_trigger_artifact(), "trigger");
    e.set_retention_policy(RetentionPolicy {
        replay_keep: 5,
        history_keep: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..1_000 {
        e.admit_event("host", correlated_event(&format!("b{i:04}"), "x"), i)
            .unwrap();
    }
    for i in 0..50 {
        e.set_activation_controls("trigger", ActivationControls::default(), 2_000 + i)
            .unwrap();
    }
    let receipts = history_items(&e, InspectionKind::Receipts);
    let of = |prefix: &str| {
        receipts
            .iter()
            .filter(|r| r["run"].as_str().unwrap().starts_with(prefix))
            .count()
    };
    // At most 10 in history, plus those of the 5 events still remembered
    // (live receipts, which leave with their event).
    let live: usize = e.snapshot().unwrap().receipts.len();
    assert_eq!(live, 5);
    for (prefix, allowed) in [("event:", live + 1..=10 + live), ("admin", 1..=10)] {
        let count = of(prefix);
        assert!(allowed.contains(&count), "{prefix}: {count}");
    }
    assert!(receipts.len() <= 20 + live, "{}", receipts.len());
    // The newest stay.
    let newest = receipts
        .iter()
        .filter(|r| r["run"] == "admin")
        .map(|r| r["sequence"].as_u64().unwrap())
        .max()
        .unwrap();
    assert_eq!(newest, e.snapshot().unwrap().sequence);
}

#[test]
fn memory_storage_receipts_of_no_run_are_rolling_too() {
    receipts_of_no_run_are_rolling_too(MemoryStorage::default());
}

#[test]
fn record_storage_receipts_of_no_run_are_rolling_too() {
    receipts_of_no_run_are_rolling_too(RecordStorage::new());
}

/// Review I2: the trim keeps the run that finished last, not the one
/// admitted last.
fn the_trim_keeps_the_last_finished_run<S: Storage>(store: S) {
    let e = history_keep_engine(store, 1);
    let first = e.admit("a", "main", Value::Null, authority(), 0).unwrap();
    let second = e.admit("a", "main", Value::Null, authority(), 0).unwrap();
    assert!(e.advance(&second, 1).unwrap());
    assert!(e.advance(&first, 2).unwrap());
    let listed: Vec<String> = history_items(&e, InspectionKind::Runs)
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(listed, [first]);
}

#[test]
fn memory_storage_trim_keeps_the_last_finished_run() {
    the_trim_keeps_the_last_finished_run(MemoryStorage::default());
}

#[test]
fn record_storage_trim_keeps_the_last_finished_run() {
    the_trim_keeps_the_last_finished_run(RecordStorage::new());
}
