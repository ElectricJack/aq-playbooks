use super::*;

pub(super) fn page(
    store: &impl Storage,
    kind: InspectionKind,
    after: Option<String>,
    limit: usize,
) -> InspectionPage {
    store
        .inspect(
            "ns",
            &InspectionQuery {
                kind,
                run: None,
                after,
                limit,
            },
        )
        .unwrap()
}

#[test]
fn memory_history_is_written_with_the_commit_and_merged_into_inspection() {
    let store = MemoryStorage::default();
    store.acquire("ns", "w").unwrap();
    let mut state = State::default();
    state.settled.push(synthetic_cohort("run-a", 10, &[1, 2]));
    let revision = store.compare_and_swap("ns", "w", 0, &state).unwrap();
    let runs = page(&store, InspectionKind::Runs, None, 10);
    assert_eq!(
        (runs.revision, runs.items),
        (
            revision,
            vec![inspection::public_run(&state.settled[0].runs[0])]
        )
    );
    assert_eq!(
        page(&store, InspectionKind::Receipts, None, 10).items.len(),
        2
    );
    assert!(store.load("ns").unwrap().state.settled.is_empty());
    assert!(
        matches!(store.compare_and_swap("ns", "w", revision, &state),
        Err(StorageError::Backend(m)) if m.contains("already recorded"))
    );
}

#[test]
fn conflicting_commit_writes_no_history() {
    let store = MemoryStorage::default();
    store.acquire("ns", "w").unwrap();
    let mut state = State::default();
    state.settled.push(synthetic_cohort("run-a", 10, &[1]));
    assert_eq!(
        store.compare_and_swap("ns", "w", 7, &state),
        Err(StorageError::Conflict)
    );
    assert!(
        page(&store, InspectionKind::Runs, None, 10)
            .items
            .is_empty()
    );
}

#[test]
fn inspection_pages_across_live_and_history_without_gaps() {
    // live receipts at sequences 2 and 4, history receipts at 1, 3, 5
    let store = MemoryStorage::default();
    store.acquire("ns", "w").unwrap();
    let mut state = State::default();
    for seq in [2, 4] {
        state.receipts.push(Receipt {
            sequence: seq,
            run: "live".into(),
            step: "s".into(),
            at_ms: 1,
            kind: "admitted".into(),
            detail: json!({}),
        });
    }
    state.settled.push(synthetic_cohort("done", 10, &[1, 3, 5]));
    store.compare_and_swap("ns", "w", 0, &state).unwrap();
    let mut seen = vec![];
    let mut after = None;
    loop {
        let p = page(&store, InspectionKind::Receipts, after, 2);
        seen.extend(p.items.iter().map(|v| v["sequence"].as_u64().unwrap()));
        if p.next.is_none() {
            break;
        }
        after = p.next;
    }
    assert_eq!(seen, vec![1, 2, 3, 4, 5]);
}

#[test]
fn delete_history_before_removes_only_older_cohorts() {
    let store = MemoryStorage::default();
    store.acquire("ns", "w").unwrap();
    let state = State {
        settled: vec![
            synthetic_cohort("old", 10, &[1]),
            synthetic_cohort("new", 20, &[2]),
        ],
        ..State::default()
    };
    store.compare_and_swap("ns", "w", 0, &state).unwrap();
    assert_eq!(store.delete_history("ns", "w", 20).unwrap(), 2); // run + receipt of "old"
    let ids: Vec<_> = page(&store, InspectionKind::Runs, None, 10)
        .items
        .iter()
        .map(|v| v["id"].clone())
        .collect();
    assert_eq!(ids, vec![json!("new")]);
}

#[test]
fn engine_refuses_storage_that_does_not_persist_history() {
    // Forwards load/compare_and_swap/acquire/release only.
    struct NoHistory(MemoryStorage);
    impl Storage for NoHistory {
        fn load(&self, n: &str) -> std::result::Result<VersionedState, StorageError> {
            self.0.load(n)
        }
        fn compare_and_swap(
            &self,
            n: &str,
            o: &str,
            v: u64,
            s: &State,
        ) -> std::result::Result<u64, StorageError> {
            self.0.compare_and_swap(n, o, v, s)
        }
        fn acquire(&self, n: &str, o: &str) -> std::result::Result<(), StorageError> {
            self.0.acquire(n, o)
        }
        fn release(&self, n: &str, o: &str) -> std::result::Result<(), StorageError> {
            self.0.release(n, o)
        }
    }
    let refused = Engine::new(NoHistory(MemoryStorage::default()), "ns", "w")
        .err()
        .unwrap();
    assert!(matches!(refused, Error::Refused(m) if m.contains("history")));
}

pub(super) fn refused_as_also_live<T>(result: std::result::Result<T, StorageError>) -> bool {
    matches!(result, Err(StorageError::Backend(m)) if m == history::ALSO_LIVE)
}

pub(super) fn live_receipt(sequence: u64) -> Receipt {
    Receipt {
        sequence,
        run: "live".into(),
        step: "s".into(),
        at_ms: 1,
        kind: "admitted".into(),
        detail: json!({}),
    }
}

/// A commit may not leave an item key both live and in history: neither by
/// settling an item the stored State still holds, nor by storing a live item
/// whose key is already in history.
fn refuses_live_and_history_conflicts(store: &impl Storage) {
    store.acquire("ns", "w").unwrap();
    // Settled while still live: a run, and a receipt.
    let mut state = State::default();
    let cohort = synthetic_cohort("a", 10, &[1]);
    state.runs.insert("a".into(), cohort.runs[0].clone());
    state.settled.push(cohort);
    assert!(refused_as_also_live(
        store.compare_and_swap("ns", "w", 0, &state)
    ));
    let mut state = State::default();
    state.receipts.push(live_receipt(7));
    state.settled.push(synthetic_cohort("r", 10, &[7]));
    assert!(refused_as_also_live(
        store.compare_and_swap("ns", "w", 0, &state)
    ));
    assert!(page(store, InspectionKind::Runs, None, 10).items.is_empty());
    assert!(
        page(store, InspectionKind::Receipts, None, 10)
            .items
            .is_empty()
    );
    // Live again after it is in history: a run, and a receipt.
    let revision = store
        .compare_and_swap(
            "ns",
            "w",
            0,
            &State {
                settled: vec![synthetic_cohort("b", 10, &[2])],
                ..State::default()
            },
        )
        .unwrap();
    let mut state = State::default();
    state
        .runs
        .insert("b".into(), synthetic_cohort("b", 10, &[]).runs[0].clone());
    assert!(refused_as_also_live(
        store.compare_and_swap("ns", "w", revision, &state)
    ));
    let mut state = State::default();
    state.receipts.push(live_receipt(2));
    assert!(refused_as_also_live(
        store.compare_and_swap("ns", "w", revision, &state)
    ));
    assert_eq!(store.load("ns").unwrap().revision, revision);
}

#[test]
fn memory_storage_refuses_live_and_history_conflicts() {
    refuses_live_and_history_conflicts(&MemoryStorage::default());
}

#[test]
fn record_storage_refuses_live_and_history_conflicts() {
    let store = RecordStorage::new();
    refuses_live_and_history_conflicts(&store);
    // The same through bounded transitions.
    store.acquire("t", "w").unwrap();
    let mut initialized = State::default();
    initialized.indexes.initialized = true;
    let mut revision = store.compare_and_swap("t", "w", 0, &initialized).unwrap();
    let transition = |cohort: Option<SettledCohort>, live: Option<Run>| {
        let (mut cohort, mut live) = (cohort, live);
        let mut plan = |s: &mut State| {
            s.settled.extend(cohort.take());
            if let Some(run) = live.take() {
                s.runs.insert(run.id.clone(), run);
            }
            s.sequence += 1;
            Ok(true)
        };
        store.transition_v1(
            "t",
            "w",
            &mut TransitionRequest {
                operation: "history",
                plan: &mut plan,
            },
        )
    };
    let also_live = |result: Result<TransitionOutcome>| matches!(result, Err(Error::Storage(StorageError::Backend(m))) if m == history::ALSO_LIVE);
    let c = synthetic_cohort("c", 10, &[]);
    assert!(also_live(transition(
        Some(c.clone()),
        Some(c.runs[0].clone())
    )));
    assert!(matches!(
        transition(Some(synthetic_cohort("d", 10, &[])), None).unwrap(),
        TransitionOutcome::Committed { .. }
    ));
    revision += 1;
    let d = synthetic_cohort("d", 10, &[]).runs[0].clone();
    assert!(also_live(transition(None, Some(d))));
    assert_eq!(store.load("t").unwrap().revision, revision);
}
