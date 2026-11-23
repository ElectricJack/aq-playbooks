use super::*;

/// A format 3 transition that carries `cohort` (if any) and stores `live` as
/// a live run (if any). `heavy` also changes a header field that sends the
/// commit through the complete path.
struct HistoryUpdate {
    cohort: Option<SettledCohort>,
    live: Option<Run>,
    heavy: bool,
}
fn transition_with(
    store: &PostgresStorage,
    ns: &str,
    update: HistoryUpdate,
) -> std::result::Result<TransitionOutcome, StorageError> {
    let HistoryUpdate {
        mut cohort,
        mut live,
        heavy,
    } = update;
    let mut plan = |s: &mut State| {
        s.settled.extend(cohort.take());
        if let Some(run) = live.take() {
            s.runs.insert(run.id.clone(), run);
        }
        s.sequence += 1;
        if heavy {
            s.execution_denied.insert(format!("denied-{}", s.sequence));
        }
        Ok(true)
    };
    storage_error(store.transition_v1(
        ns,
        "w",
        &mut TransitionRequest {
            operation: "history",
            plan: &mut plan,
        },
    ))
}

/// A format 3 namespace whose State has initialized runtime indexes, so
/// `transition_v1` runs.
fn transition_namespace(label: &str) -> (String, PostgresStorage, u64) {
    let (ns, store) = (support::namespace(label), support::records());
    store.acquire(&ns, "w").unwrap();
    let mut initialized = State::default();
    initialized.indexes.initialized = true;
    let revision = store.compare_and_swap(&ns, "w", 0, &initialized).unwrap();
    (ns, store, revision)
}

/// Pins what PostgreSQL does with a commit that carries settled cohorts, on
/// every commit path: the history rows are written with the commit, and a
/// repeated item fails the commit (insert-once) without writing anything.
#[test]
fn settled_cohorts_are_written_on_every_commit_path() {
    full_commit_is_insert_once();
    transitions_are_insert_once();
}
fn full_commit_is_insert_once() {
    // Full compare-and-swap, unprotected format 2.
    let (ns, store) = (support::namespace("f2_plain"), support::store());
    store.acquire(&ns, "w").unwrap();
    let revision = store
        .compare_and_swap(
            &ns,
            "w",
            0,
            &history_state(vec![synthetic_cohort("a", 10, &[1, 2])]),
        )
        .unwrap();
    assert_eq!(history_rows(&ns), 3);
    assert!(already_recorded(store.compare_and_swap(
        &ns,
        "w",
        revision,
        &history_state(vec![
            synthetic_cohort("b", 11, &[3]),
            synthetic_cohort("a", 10, &[1, 2])
        ])
    )));
    let persisted = store.load(&ns).unwrap();
    assert_eq!(history_rows(&ns), 3);
    assert_eq!(persisted.revision, revision);
    assert_eq!(run_ids(&store, &ns), vec![json!("a")]);
}
fn transitions_are_insert_once() {
    // Format 3 transitions, incremental and complete.
    let (ns, store, mut revision) = transition_namespace("f3_transition");
    store.reset_transition_stats();
    let committed = transition_with(
        &store,
        &ns,
        HistoryUpdate {
            cohort: Some(synthetic_cohort("inc", 10, &[1])),
            live: None,
            heavy: false,
        },
    )
    .unwrap();
    assert_eq!(store.transition_stats().incremental, 1);
    revision += 1;
    assert!(matches!(committed, TransitionOutcome::Committed { revision: r } if r == revision));
    let committed = transition_with(
        &store,
        &ns,
        HistoryUpdate {
            cohort: Some(synthetic_cohort("heavy", 11, &[2])),
            live: None,
            heavy: true,
        },
    )
    .unwrap();
    assert_eq!(store.transition_stats().heavy, 1);
    revision += 1;
    assert!(matches!(committed, TransitionOutcome::Committed { revision: r } if r == revision));
    assert_eq!(history_rows(&ns), 4);
    assert_eq!(run_ids(&store, &ns), vec![json!("heavy"), json!("inc")]);
    assert!(already_recorded(transition_with(
        &store,
        &ns,
        HistoryUpdate {
            cohort: Some(synthetic_cohort("inc", 10, &[1])),
            live: None,
            heavy: false
        }
    )));
    assert!(already_recorded(transition_with(
        &store,
        &ns,
        HistoryUpdate {
            cohort: Some(synthetic_cohort("heavy", 11, &[2])),
            live: None,
            heavy: true
        }
    )));
    let stats = store.transition_stats();
    assert_eq!((stats.incremental, stats.heavy), (1, 1));
    assert_eq!(history_rows(&ns), 4);
    assert_eq!(store.load(&ns).unwrap().revision, revision);
}

/// No commit leaves an item key both live and in history, on any path:
/// neither settling an item the stored State still holds, nor storing a live
/// item whose key is already in history. Nothing is written by the refusal.
#[test]
fn history_that_is_also_live_is_refused_on_every_commit_path() {
    history_conflicts_on_full_commits();
    history_conflicts_on_transitions();
}
fn history_conflicts_on_full_commits() {
    let stores = [
        ("f2_protected", support::protected()),
        ("f2_plain", support::store()),
        ("f3_cas", support::records()),
    ];
    for (label, store) in stores {
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        let cohort = synthetic_cohort("a", 10, &[1]);
        let mut state = history_state(vec![cohort.clone()]);
        state.runs.insert("a".into(), cohort.runs[0].clone());
        assert!(
            also_live(store.compare_and_swap(&ns, "w", 0, &state)),
            "{label}"
        );
        let mut state = history_state(vec![synthetic_cohort("r", 10, &[7])]);
        state.receipts.push(live_receipt(7));
        assert!(
            also_live(store.compare_and_swap(&ns, "w", 0, &state)),
            "{label}"
        );
        assert_eq!(history_rows(&ns), 0, "{label}");
        let revision = store
            .compare_and_swap(
                &ns,
                "w",
                0,
                &history_state(vec![synthetic_cohort("b", 10, &[2])]),
            )
            .unwrap();
        let mut state = State::default();
        state
            .runs
            .insert("b".into(), synthetic_cohort("b", 10, &[]).runs[0].clone());
        assert!(
            also_live(store.compare_and_swap(&ns, "w", revision, &state)),
            "{label}"
        );
        let mut state = State::default();
        state.receipts.push(live_receipt(2));
        assert!(
            also_live(store.compare_and_swap(&ns, "w", revision, &state)),
            "{label}"
        );
        assert_eq!(history_rows(&ns), 2, "{label}");
        assert_eq!(store.load(&ns).unwrap().revision, revision, "{label}");
    }
}
fn history_conflicts_on_transitions() {
    for heavy in [false, true] {
        let (ns, store, revision) = transition_namespace(if heavy { "f3_heavy" } else { "f3_inc" });
        let c = synthetic_cohort("c", 10, &[]);
        assert!(
            also_live(transition_with(
                &store,
                &ns,
                HistoryUpdate {
                    cohort: Some(c.clone()),
                    live: Some(c.runs[0].clone()),
                    heavy
                }
            )),
            "{heavy}"
        );
        assert_eq!(history_rows(&ns), 0);
        transition_with(
            &store,
            &ns,
            HistoryUpdate {
                cohort: Some(synthetic_cohort("d", 10, &[])),
                live: None,
                heavy,
            },
        )
        .unwrap();
        let d = synthetic_cohort("d", 10, &[]).runs[0].clone();
        store.reset_transition_stats();
        assert!(
            also_live(transition_with(
                &store,
                &ns,
                HistoryUpdate {
                    cohort: None,
                    live: Some(d),
                    heavy
                }
            )),
            "{heavy}"
        );
        let stats = store.transition_stats();
        assert_eq!((stats.incremental, stats.heavy), (0, 0));
        assert_eq!(history_rows(&ns), 1);
        assert_eq!(store.load(&ns).unwrap().revision, revision + 1);
    }
}
