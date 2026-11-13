#![cfg(feature = "postgres-tests")]
//! Live state only (spec 0003) on PostgreSQL: insert-once `run_history` rows
//! written with the commit, and inspection over live rows plus history.
use aq_playbooks_postgres::PostgresStorage;
use aq_playbooks_runner::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[allow(dead_code)]
mod support;

/// The server's message (`postgres::Error`'s `Display` is only "db error").
fn db_message(error: &postgres::Error) -> String {
    error
        .as_db_error()
        .map_or_else(|| error.to_string(), |db| db.message().to_string())
}
fn history_state(cohorts: Vec<SettledCohort>) -> State {
    State {
        settled: cohorts,
        ..State::default()
    }
}
fn synthetic_cohort(id: &str, finished_at_ms: i64, receipts: &[u64]) -> SettledCohort {
    let run = Run {
        catalog_revision: 1,
        id: id.into(),
        root: id.into(),
        parent: None,
        origin: None,
        artifact: "artifact".into(),
        entry: "main".into(),
        dependencies: BTreeMap::new(),
        authority: Authority {
            actor: "alice".into(),
            scope: "project".into(),
            capabilities: ["execute".to_string()].into(),
        },
        step: "ok".into(),
        status: RunStatus::Completed,
        input: Value::Null,
        variables: BTreeMap::new(),
        outcome: Some("ok".into()),
        output: Some(Value::Null),
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
        terminal_sequence: receipts.last().copied(),
    };
    SettledCohort {
        root: id.into(),
        finished_at_ms,
        runs: vec![run],
        intents: vec![],
        receipts: receipts
            .iter()
            .map(|&sequence| Receipt {
                sequence,
                run: id.into(),
                step: "ok".into(),
                at_ms: finished_at_ms,
                kind: "terminal".into(),
                detail: json!({"status": "completed", "outcome": "ok"}),
            })
            .collect(),
    }
}
fn both() -> [(&'static str, PostgresStorage); 2] {
    [("f2", support::protected()), ("f3", support::records())]
}
fn runs_query() -> InspectionQuery {
    InspectionQuery {
        kind: InspectionKind::Runs,
        run: None,
        after: None,
        limit: 1000,
    }
}
fn all_receipt_sequences(store: &PostgresStorage, ns: &str, limit: usize) -> Vec<u64> {
    let mut sequences = vec![];
    let mut after = None;
    loop {
        let page = store
            .inspect(
                ns,
                &InspectionQuery {
                    kind: InspectionKind::Receipts,
                    run: None,
                    after,
                    limit,
                },
            )
            .unwrap();
        assert!(page.items.len() <= limit);
        sequences.extend(page.items.iter().map(|v| v["sequence"].as_u64().unwrap()));
        match page.next {
            Some(next) => after = Some(next),
            None => return sequences,
        }
    }
}
fn run_ids(store: &PostgresStorage, ns: &str) -> Vec<Value> {
    store
        .inspect(ns, &runs_query())
        .unwrap()
        .items
        .iter()
        .map(|v| v["id"].clone())
        .collect()
}
fn history_rows(ns: &str) -> i64 {
    support::client()
        .query_one(
            "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1",
            &[&ns],
        )
        .unwrap()
        .get(0)
}
fn already_recorded<T>(result: std::result::Result<T, StorageError>) -> bool {
    matches!(result, Err(StorageError::Backend(m)) if m == history::ALREADY_RECORDED)
}
fn also_live<T>(result: std::result::Result<T, StorageError>) -> bool {
    matches!(result, Err(StorageError::Backend(m)) if m == history::ALSO_LIVE)
}
fn storage_error<T>(result: Result<T>) -> std::result::Result<T, StorageError> {
    result.map_err(|error| match error {
        Error::Storage(error) => error,
        other => StorageError::Backend(other.to_string()),
    })
}
fn live_receipt(sequence: u64) -> Receipt {
    Receipt {
        sequence,
        run: "live".into(),
        step: "s".into(),
        at_ms: 1,
        kind: "admitted".into(),
        detail: json!({}),
    }
}
/// A format 3 transition that carries `cohort` (if any) and stores `live` as
/// a live run (if any). `heavy` also changes a header field that sends the
/// commit through the complete path.
fn transition_with(
    store: &PostgresStorage,
    ns: &str,
    cohort: Option<SettledCohort>,
    live: Option<Run>,
    heavy: bool,
) -> std::result::Result<TransitionOutcome, StorageError> {
    let (mut cohort, mut live) = (cohort, live);
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
    let validate = |_: &State, _: &QuotaCounts| Ok(());
    storage_error(store.transition_v1(
        ns,
        "w",
        &mut TransitionRequest {
            operation: "history",
            plan: &mut plan,
            validate: &validate,
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

#[test]
fn protected_history_rows_are_plaintext_public_and_insert_once() {
    let (ns, store) = (support::namespace("history"), support::protected());
    store.acquire(&ns, "w").unwrap();
    // Protected values that inspection never projects.
    let mut cohort = synthetic_cohort("run-a", 10, &[1, 2]);
    let run = &mut cohort.runs[0];
    run.input = json!({"secret": "PROTECTED_SENTINEL_INPUT"});
    run.output = Some(json!("PROTECTED_SENTINEL_OUTPUT"));
    run.variables
        .insert("v".into(), json!("PROTECTED_SENTINEL_VARIABLE"));
    cohort.receipts[0].detail["private"] = json!("PROTECTED_SENTINEL_DETAIL");
    let revision = store
        .compare_and_swap(&ns, "w", 0, &history_state(vec![cohort]))
        .unwrap();
    let mut c = support::client();
    let rows: i64 = c
        .query_one(
            "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1",
            &[&ns],
        )
        .unwrap()
        .get(0);
    assert_eq!(rows, 3);
    let envelope = c.query_one("SELECT snapshot IS NULL, protection_key_id IS NOT NULL FROM aq_playbooks.namespaces WHERE namespace=$1", &[&ns]).unwrap();
    assert!(envelope.get::<_, bool>(0) && envelope.get::<_, bool>(1));
    let leaked: bool = c
        .query_one(
            "SELECT bool_or(public_value::text LIKE '%PROTECTED_SENTINEL%') FROM aq_playbooks.run_history WHERE namespace=$1",
            &[&ns],
        )
        .unwrap()
        .get(0);
    assert!(!leaked);
    let mut tx = support::marked(&mut c, &ns);
    let err = tx
        .execute(
            "UPDATE aq_playbooks.run_history SET finished_at_ms=0 WHERE namespace=$1",
            &[&ns],
        )
        .unwrap_err();
    assert!(db_message(&err).contains("run history rows are immutable"));
    assert!(already_recorded(store.compare_and_swap(
        &ns,
        "w",
        revision,
        &history_state(vec![synthetic_cohort("run-a", 10, &[1, 2])])
    )));
}
#[test]
fn history_union_pages_in_item_key_order() {
    for (label, store) in both() {
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        let mut state = history_state(vec![synthetic_cohort("done", 10, &[1, 3, 5])]);
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
        store.compare_and_swap(&ns, "w", 0, &state).unwrap();
        // page with limit 2 until next is None
        assert_eq!(
            all_receipt_sequences(&store, &ns, 2),
            vec![1, 2, 3, 4, 5],
            "{label}"
        );
    }
}
#[test]
fn rolled_back_commit_leaves_no_history_rows() {
    for (label, store) in both() {
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        assert_eq!(
            store.compare_and_swap(
                &ns,
                "w",
                9,
                &history_state(vec![synthetic_cohort("run-a", 10, &[1])])
            ),
            Err(StorageError::Conflict)
        );
        assert!(
            store.history_page(&ns, &runs_query()).unwrap().is_empty(),
            "{label}"
        );
    }
}
#[test]
fn delete_history_before_is_an_indexed_delete() {
    for (label, store) in both() {
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        let state = history_state(vec![
            synthetic_cohort("old", 10, &[1]),
            synthetic_cohort("new", 20, &[2]),
        ]);
        store.compare_and_swap(&ns, "w", 0, &state).unwrap();
        assert_eq!(
            store.delete_history(&ns, "other", 20),
            Err(StorageError::Ownership),
            "{label}"
        );
        assert_eq!(store.delete_history(&ns, "w", 20).unwrap(), 2, "{label}"); // run + receipt of "old"
        assert_eq!(run_ids(&store, &ns), vec![json!("new")], "{label}");
        assert_eq!(all_receipt_sequences(&store, &ns, 10), vec![2], "{label}");
    }
    let mut c = support::client();
    let mut tx = c.transaction().unwrap();
    tx.batch_execute("SET LOCAL enable_seqscan=off").unwrap();
    let plan: Vec<String> = tx
        .query(
            "EXPLAIN DELETE FROM aq_playbooks.run_history WHERE namespace=$1 AND finished_at_ms<$2",
            &[&"ns", &20_i64],
        )
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(
        plan.join("\n").contains("run_history_by_finish"),
        "{plan:?}"
    );
}
#[test]
fn history_rows_need_the_namespace_writer_and_cannot_be_truncated() {
    let (ns, store) = (support::namespace("writer"), support::store());
    store.acquire(&ns, "w").unwrap();
    store
        .compare_and_swap(
            &ns,
            "w",
            0,
            &history_state(vec![synthetic_cohort("a", 10, &[1])]),
        )
        .unwrap();
    let mut c = support::client();
    let insert = "INSERT INTO aq_playbooks.run_history(namespace,kind,item_key,run_id,root_id,sequence,finished_at_ms,public_value) VALUES($1,'runs','forged','forged','forged',0,1,'{}'::jsonb)";
    let err = c.execute(insert, &[&ns]).unwrap_err();
    assert!(db_message(&err).contains("writer"), "{err:?}");
    let err = c
        .execute(
            "DELETE FROM aq_playbooks.run_history WHERE namespace=$1",
            &[&ns],
        )
        .unwrap_err();
    assert!(db_message(&err).contains("writer"), "{err:?}");
    // The writer marker alone does not let a session without the lease delete.
    let mut tx = support::marked(&mut c, &ns);
    let err = tx
        .execute(
            "DELETE FROM aq_playbooks.run_history WHERE namespace=$1",
            &[&ns],
        )
        .unwrap_err();
    assert!(db_message(&err).contains("writer"), "{err:?}");
    drop(tx);
    let err = c
        .batch_execute("TRUNCATE aq_playbooks.run_history")
        .unwrap_err();
    assert_eq!(db_message(&err), "run_history is insert-only");
    assert_eq!(history_rows(&ns), 2); // run + receipt of "a"
}
/// Pins what PostgreSQL does with a commit that carries settled cohorts, on
/// every commit path: the history rows are written with the commit, and a
/// repeated item fails the commit (insert-once) without writing anything.
#[test]
fn settled_cohorts_are_written_on_every_commit_path() {
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
    assert_eq!(history_rows(&ns), 3);
    assert_eq!(store.load(&ns).unwrap().revision, revision);
    assert_eq!(run_ids(&store, &ns), vec![json!("a")]);

    // Format 3 transitions, incremental and complete.
    let (ns, store, mut revision) = transition_namespace("f3_transition");
    store.reset_transition_stats();
    let committed = transition_with(
        &store,
        &ns,
        Some(synthetic_cohort("inc", 10, &[1])),
        None,
        false,
    )
    .unwrap();
    assert_eq!(store.transition_stats().incremental, 1);
    revision += 1;
    assert!(matches!(committed, TransitionOutcome::Committed { revision: r } if r == revision));
    let committed = transition_with(
        &store,
        &ns,
        Some(synthetic_cohort("heavy", 11, &[2])),
        None,
        true,
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
        Some(synthetic_cohort("inc", 10, &[1])),
        None,
        false
    )));
    assert!(already_recorded(transition_with(
        &store,
        &ns,
        Some(synthetic_cohort("heavy", 11, &[2])),
        None,
        true
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
    for heavy in [false, true] {
        let (ns, store, revision) = transition_namespace(if heavy { "f3_heavy" } else { "f3_inc" });
        let c = synthetic_cohort("c", 10, &[]);
        assert!(
            also_live(transition_with(
                &store,
                &ns,
                Some(c.clone()),
                Some(c.runs[0].clone()),
                heavy
            )),
            "{heavy}"
        );
        assert_eq!(history_rows(&ns), 0);
        transition_with(
            &store,
            &ns,
            Some(synthetic_cohort("d", 10, &[])),
            None,
            heavy,
        )
        .unwrap();
        let d = synthetic_cohort("d", 10, &[]).runs[0].clone();
        store.reset_transition_stats();
        assert!(
            also_live(transition_with(&store, &ns, None, Some(d), heavy)),
            "{heavy}"
        );
        let stats = store.transition_stats();
        assert_eq!((stats.incremental, stats.heavy), (0, 0));
        assert_eq!(history_rows(&ns), 1);
        assert_eq!(store.load(&ns).unwrap().revision, revision + 1);
    }
}
