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

// --- Task 4: retention clock, window fields and removal fences ------------

fn receipt(key: &str, accepted_at_ms: i64) -> KeyedAdmissionReceipt {
    KeyedAdmissionReceipt {
        key: key.into(),
        request_digest: format!("digest-{key}"),
        run_id: format!("run-{key}"),
        activation_revision: 1,
        catalog_revision: 1,
        accepted_at_ms,
    }
}
/// A terminal root run `id` with no children.
fn completed_run(id: &str) -> Run {
    synthetic_cohort(id, 1, &[]).runs.remove(0)
}
fn cohort_of(run: Run) -> SettledCohort {
    SettledCohort {
        root: run.id.clone(),
        finished_at_ms: 1,
        runs: vec![run],
        intents: vec![],
        receipts: vec![],
    }
}
fn refused_with<T>(result: std::result::Result<T, StorageError>, text: &str) -> bool {
    matches!(result, Err(StorageError::Backend(m)) if m == text)
}
fn removal_fences_hold(store: &PostgresStorage, ns: &str) {
    store.acquire(ns, "w").unwrap();
    let mut s = State::default();
    // Round 1 (I2): a recorded expiry is at least acceptance plus the policy
    // window, so the brief's 1_000 needs a 1_000 window.
    s.retention.policy.admission_replay_ms = 1_000;
    s.retention.clock_ms = Some(500);
    s.keyed_admissions.insert("k".into(), receipt("k", 0));
    s.retention
        .admission_expires_at_ms
        .insert("k".into(), 1_000);
    s.runs.insert("run-a".into(), completed_run("run-a"));
    let r = store.compare_and_swap(ns, "w", 0, &s).unwrap();
    let mut next = s.clone();
    next.keyed_admissions.remove("k");
    next.retention.clock_ms = Some(999);
    assert!(
        matches!(store.compare_and_swap(ns, "w", r, &next), Err(StorageError::Backend(m)) if m == retention::REFUSE_EARLY_REMOVAL)
    );
    next.retention.clock_ms = Some(1_000);
    let r = store.compare_and_swap(ns, "w", r, &next).unwrap();
    let mut gone = next.clone();
    gone.runs.remove("run-a");
    assert!(
        matches!(store.compare_and_swap(ns, "w", r, &gone), Err(StorageError::Backend(m)) if m == retention::REFUSE_RUN_REMOVAL)
    );
    gone.settled.push(SettledCohort {
        root: "run-a".into(),
        finished_at_ms: 1,
        runs: vec![completed_run("run-a")],
        intents: vec![],
        receipts: vec![],
    });
    let r = store.compare_and_swap(ns, "w", r, &gone).unwrap();
    let mut back = gone.clone();
    back.settled.clear();
    back.retention.clock_ms = Some(10);
    assert!(
        matches!(store.compare_and_swap(ns, "w", r, &back), Err(StorageError::Backend(m)) if m == retention::REFUSE_CLOCK)
    );
}
#[test]
fn removal_fences_hold_on_postgres() {
    removal_fences_hold(&support::protected(), &support::namespace("fence_f2"));
    removal_fences_hold(&support::records(), &support::namespace("fence_f3"));
}
fn audit(sequence: u64, run: &str) -> Receipt {
    Receipt {
        sequence,
        run: run.into(),
        step: "s".into(),
        at_ms: 1,
        kind: "terminal".into(),
        detail: json!({}),
    }
}
fn accepted_event(key: &str, accepted_at_ms: i64) -> AcceptedEvent {
    AcceptedEvent {
        key: key.into(),
        envelope: aq_playbooks_model::EventEnvelope {
            producer: "host".into(),
            event_id: key.into(),
            contract: aq_playbooks_model::ContractRef {
                name: "test.completed".into(),
                fingerprint: "fingerprint".into(),
            },
            scope: "project".into(),
            correlation: BTreeMap::new(),
            subject: None,
            revision: None,
            causation: None,
            occurred_at_ms: None,
            observed_at_ms: None,
            payload: Value::Null,
        },
        digest: format!("digest-{key}"),
        accepted_at_ms,
        sequence: 1,
        catalog_revision: 1,
        destinations: vec![],
    }
}
fn tombstone(digest: &str) -> retention::EventTombstone {
    retention::EventTombstone {
        producer: "host".into(),
        epoch: 0,
        digest: digest.into(),
        disposed_at_ms: 5,
    }
}
/// Policy windows: receipts 2_000, events 3_000; recorded expiries "k"
/// 3_000 and "ev" 4_000; "fallback" has none. Receipt 1 belongs to the
/// finished run-a, receipt 2 to the running run-b.
fn fence_state() -> State {
    let mut s = State::default();
    s.retention.clock_ms = Some(500);
    s.retention.policy.admission_replay_ms = 2_000;
    s.retention.policy.event_replay_ms = 3_000;
    s.runs.insert("run-a".into(), completed_run("run-a"));
    s.runs.insert("run-b".into(), {
        let mut run = completed_run("run-b");
        run.status = RunStatus::Running;
        run
    });
    s.receipts.push(audit(1, "run-a"));
    s.receipts.push(audit(2, "run-b"));
    s.keyed_admissions.insert("k".into(), receipt("k", 0));
    s.keyed_admissions
        .insert("fallback".into(), receipt("fallback", 0));
    s.retention
        .admission_expires_at_ms
        .insert("k".into(), 3_000);
    s.events.insert("ev".into(), accepted_event("ev", 0));
    s.retention.event_expires_at_ms.insert("ev".into(), 4_000);
    s
}
type Case = (&'static str, &'static str, Box<dyn Fn(&mut State)>);
/// The refusals every commit path must make, each as a change of
/// `fence_state()`.
fn refusal_cases() -> Vec<Case> {
    let early = retention::REFUSE_EARLY_REMOVAL;
    let shortened = retention::REFUSE_EXPIRY;
    vec![
        (
            "run",
            retention::REFUSE_RUN_REMOVAL,
            Box::new(|n| {
                n.runs.remove("run-a");
            }),
        ),
        (
            "running settled run",
            "unresolved run obligations cannot be disposed",
            Box::new(|n| {
                let running = n.runs.remove("run-b").unwrap();
                n.receipts.retain(|r| r.sequence != 2);
                let mut c = cohort_of(running);
                c.receipts.push(audit(2, "run-b"));
                n.settled.push(c);
            }),
        ),
        (
            "keyed admission",
            early,
            Box::new(|n| {
                n.retention.clock_ms = Some(2_999);
                n.keyed_admissions.remove("k");
                n.retention.admission_expires_at_ms.remove("k");
            }),
        ),
        (
            "keyed admission, policy window",
            early,
            Box::new(|n| {
                n.retention.clock_ms = Some(1_999);
                n.keyed_admissions.remove("fallback");
            }),
        ),
        (
            "event",
            early,
            Box::new(|n| {
                n.retention.clock_ms = Some(3_999);
                n.events.remove("ev");
                n.retention.event_expires_at_ms.remove("ev");
            }),
        ),
        (
            "forged legacy tombstone",
            early,
            Box::new(|n| {
                n.events.remove("ev");
                n.retention.event_expires_at_ms.remove("ev");
                n.retention
                    .event_tombstones
                    .insert("ev".into(), tombstone("digest-ev"));
            }),
        ),
        (
            "changed receipt",
            "immutable keyed admission receipt removed or changed",
            Box::new(|n| {
                n.keyed_admissions.get_mut("k").unwrap().run_id = "other".into();
            }),
        ),
        (
            "receipt of a live run",
            retention::REFUSE_RUN_REMOVAL,
            Box::new(|n| {
                n.receipts.retain(|r| r.sequence != 2);
            }),
        ),
        (
            "receipt overwritten",
            retention::REFUSE_RUN_REMOVAL,
            Box::new(|n| {
                *n.receipts.get_mut(1).unwrap() = audit(3, "run-b");
            }),
        ),
        (
            "receipt of a settled run without its history",
            retention::REFUSE_RUN_REMOVAL,
            Box::new(|n| {
                let run = n.runs.remove("run-a").unwrap();
                n.receipts.retain(|r| r.sequence != 1);
                n.settled.push(cohort_of(run));
            }),
        ),
        (
            "lowered expiry",
            shortened,
            Box::new(|n| {
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_500);
            }),
        ),
        (
            "expiry deleted, item kept",
            shortened,
            Box::new(|n| {
                n.retention.event_expires_at_ms.remove("ev");
            }),
        ),
        (
            "new expiry below the fallback, stored item",
            shortened,
            Box::new(|n| {
                n.retention
                    .admission_expires_at_ms
                    .insert("fallback".into(), 1_999);
            }),
        ),
        (
            "new expiry below the fallback, new item",
            shortened,
            Box::new(|n| {
                n.keyed_admissions.insert("new".into(), receipt("new", 100));
                n.retention
                    .admission_expires_at_ms
                    .insert("new".into(), 2_099);
            }),
        ),
        (
            "clock",
            retention::REFUSE_CLOCK,
            Box::new(|n| {
                n.retention.clock_ms = Some(499);
            }),
        ),
    ]
}
fn tombstone_rows(ns: &str) -> i64 {
    support::client()
        .query_one(
            "SELECT count(*) FROM aq_playbooks.disposal_tombstones WHERE namespace=$1",
            &[&ns],
        )
        .unwrap()
        .get(0)
}
/// No removal without a history record or an expired window entry, and no
/// expiry moved earlier, on every full commit path. A refusal writes
/// nothing and writes no tombstone.
#[test]
fn removals_without_history_or_expiry_are_refused_on_every_full_commit_path() {
    let stores = [
        ("f2_protected", support::protected()),
        ("f2_plain", support::store()),
        ("f3_cas", support::records()),
    ];
    for (label, store) in stores {
        let s = fence_state();
        let fresh = || {
            let ns = support::namespace(label);
            store.acquire(&ns, "w").unwrap();
            let base = store.compare_and_swap(&ns, "w", 0, &s).unwrap();
            (ns, base)
        };
        let mut failures = vec![];
        for (case, text, f) in refusal_cases() {
            let (ns, base) = fresh();
            let mut next = s.clone();
            f(&mut next);
            let result = store.compare_and_swap(&ns, "w", base, &next);
            if !refused_with(result.clone(), text) {
                failures.push(format!("{case}: {result:?}"));
            } else {
                assert_eq!(store.load(&ns).unwrap().revision, base, "{label} {case}");
                assert_eq!(
                    (history_rows(&ns), tombstone_rows(&ns)),
                    (0, 0),
                    "{label} {case}"
                );
            }
        }
        assert!(failures.is_empty(), "{label}: {failures:#?}");
        // Allowed: both receipts and the event expired with their records,
        // the run with its history and its receipt. No tombstone row.
        let (ns, base) = fresh();
        let mut next = s.clone();
        next.retention.clock_ms = Some(4_000);
        next.keyed_admissions.clear();
        next.retention.admission_expires_at_ms.remove("k");
        next.events.remove("ev");
        next.retention.event_expires_at_ms.remove("ev");
        let run = next.runs.remove("run-a").unwrap();
        next.receipts.retain(|r| r.sequence != 1);
        let mut c = cohort_of(run);
        c.receipts.push(audit(1, "run-a"));
        next.settled.push(c);
        store.compare_and_swap(&ns, "w", base, &next).unwrap();
        assert_eq!(history_rows(&ns), 2, "{label}");
        assert_eq!(tombstone_rows(&ns), 0, "{label}");
    }
}
/// Format 3 bounded transitions refuse the same commits, incrementally or
/// on the complete path; nothing is written and no commit is counted. An
/// expired window entry leaves incrementally; an event tombstone's row
/// leaves with it.
#[test]
fn format_3_transitions_hold_the_removal_fences() {
    let store = support::records();
    let mut s = fence_state();
    s.indexes.initialized = true;
    s.retention
        .event_tombstones
        .insert("old".into(), tombstone("d"));
    let fresh = || {
        let ns = support::namespace("f3_fence");
        store.acquire(&ns, "w").unwrap();
        let base = store.compare_and_swap(&ns, "w", 0, &s).unwrap();
        (ns, base)
    };
    let transition = |ns: &str, f: &dyn Fn(&mut State)| {
        let mut plan = |s: &mut State| {
            f(s);
            Ok(true)
        };
        let validate = |_: &State, _: &QuotaCounts| Ok(());
        storage_error(store.transition_v1(
            ns,
            "w",
            &mut TransitionRequest {
                operation: "fence",
                plan: &mut plan,
                validate: &validate,
            },
        ))
    };
    store.reset_transition_stats();
    let mut cases = refusal_cases();
    // Also when the commit is heavy for another reason.
    cases.push((
        "early removal, heavy",
        retention::REFUSE_EARLY_REMOVAL,
        Box::new(|n| {
            n.execution_denied.insert("x".into());
            n.retention.clock_ms = Some(2_999);
            n.keyed_admissions.remove("k");
            n.retention.admission_expires_at_ms.remove("k");
        }),
    ));
    // Refused by the bounded commit itself, before any complete path.
    let bounded = [
        "lowered expiry",
        "new expiry below the fallback, new item",
        "changed receipt",
        "clock",
    ];
    let mut failures = vec![];
    for (case, text, f) in &cases {
        let (ns, base) = fresh();
        store.set_profiling(true);
        let result = transition(&ns, f.as_ref());
        let complete_path = store
            .profiling_snapshot()
            .unwrap()
            .operations
            .contains_key("cas");
        store.set_profiling(false);
        if !refused_with(result.clone(), text) {
            failures.push(format!("{case}: {result:?}"));
        } else if bounded.contains(case) && complete_path {
            failures.push(format!("{case}: refused only on the complete path"));
        } else {
            assert_eq!(store.load(&ns).unwrap().revision, base, "{case}");
            assert_eq!(history_rows(&ns), 0, "{case}");
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    let stats = store.transition_stats();
    assert_eq!((stats.incremental, stats.heavy), (0, 0));
    let (ns, _) = fresh();
    // The in-State tombstone "old" has its row; removing it is bounded and
    // takes the row with it.
    let mut c = support::client();
    let mut tx = c.transaction().unwrap();
    tx.query_one(
        "SELECT set_config('aq_playbooks.writer_namespace',$1,true),set_config('aq_playbooks.writer_format','3',true)",
        &[&ns],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO aq_playbooks.disposal_tombstones(namespace,kind,identity,content_digest,disposed_at_ms) VALUES($1,'event','old','d',5)",
        &[&ns],
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(tombstone_rows(&ns), 1);
    transition(&ns, &|s| {
        s.retention.event_tombstones.remove("old");
    })
    .unwrap();
    assert_eq!(store.transition_stats().incremental, 1);
    assert_eq!(tombstone_rows(&ns), 0);
    // Expired: a receipt and an event leave in one bounded commit, and an
    // expiry moves later; no tombstone row is written.
    transition(&ns, &|s| {
        s.retention.clock_ms = Some(4_000);
        s.keyed_admissions.remove("k");
        s.retention.admission_expires_at_ms.remove("k");
        s.events.remove("ev");
        s.retention.event_expires_at_ms.remove("ev");
        s.keyed_admissions
            .insert("new".into(), receipt("new", 4_000));
        s.retention
            .admission_expires_at_ms
            .insert("new".into(), 6_000);
    })
    .unwrap();
    assert_eq!(store.transition_stats().incremental, 2);
    transition(&ns, &|s| {
        s.retention
            .admission_expires_at_ms
            .insert("new".into(), 7_000);
    })
    .unwrap();
    assert_eq!(store.transition_stats().incremental, 3);
    let state = store.load(&ns).unwrap().state;
    assert!(!state.keyed_admissions.contains_key("k") && state.events.is_empty());
    assert_eq!(tombstone_rows(&ns), 0);
    // Without its expiry record in the patch, an expired receipt leaves
    // through the complete path.
    transition(&ns, &|s| {
        s.keyed_admissions.remove("fallback");
    })
    .unwrap();
    assert_eq!(store.transition_stats().heavy, 1);
    // The run leaves with its history and its receipt, through the complete
    // path.
    transition(&ns, &|s| {
        let run = s.runs.remove("run-a").unwrap();
        s.receipts.retain(|r| r.sequence != 1);
        let mut c = cohort_of(run);
        c.receipts.push(audit(1, "run-a"));
        s.settled.push(c);
    })
    .unwrap();
    assert_eq!(store.transition_stats().heavy, 2);
    assert!(!store.load(&ns).unwrap().state.runs.contains_key("run-a"));
    assert_eq!(history_rows(&ns), 2);
}
#[test]
fn full_commit_reads_only_created_identities_from_disposal_tombstones() {
    for (label, store) in [
        ("f2", support::protected()),
        ("f2_plain", support::store()),
        ("f3", support::records()),
    ] {
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        let base = store
            .compare_and_swap(&ns, "w", 0, &State::default())
            .unwrap();
        // 1,000 legacy event tombstone rows, written as the namespace writer.
        let mut c = support::client();
        let mut tx = c.transaction().unwrap();
        let format = if label == "f3" { "3" } else { "2" };
        tx.query_one(
            "SELECT set_config('aq_playbooks.writer_namespace',$1,true),set_config('aq_playbooks.writer_format',$2,true)",
            &[&ns, &format],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO aq_playbooks.disposal_tombstones(namespace,kind,identity,content_digest,disposed_at_ms) SELECT $1,'event','event-'||i,'d',1 FROM generate_series(1,1000) i",
            &[&ns],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO aq_playbooks.disposal_tombstones(namespace,kind,identity,content_digest,disposed_at_ms) VALUES($1,'run','run-old',NULL,1)",
            &[&ns],
        )
        .unwrap();
        tx.commit().unwrap();
        let mut next = State::default();
        next.runs.insert("run-new".into(), completed_run("run-new"));
        let revision = store.compare_and_swap(&ns, "w", base, &next).unwrap();
        let mut reused = next.clone();
        reused
            .runs
            .insert("run-old".into(), completed_run("run-old"));
        assert!(
            refused_with(
                store.compare_and_swap(&ns, "w", revision, &reused),
                "disposed run identity cannot be reused"
            ),
            "{label}"
        );
        assert_eq!(store.load(&ns).unwrap().revision, revision, "{label}");
    }
}

// --- Task 5: a settled cohort leaves the record in the commit that settles it

fn count(sql: &str, ns: &str, id: &str) -> i64 {
    support::client()
        .query_one(sql, &[&ns, &id])
        .unwrap()
        .get(0)
}
#[test]
fn finished_run_leaves_protected_snapshot_and_projections_and_writes_no_tombstone() {
    for (format, store) in both() {
        let ns = support::namespace("leaves");
        let e = Engine::new(store, &ns, "w").unwrap();
        let a = support::artifact("a");
        let digest = a.digest.clone();
        e.install(a).unwrap();
        let authority = Authority {
            actor: "synthetic-test".into(),
            scope: "project".into(),
            capabilities: Default::default(),
        };
        e.activate_at("a", &digest, authority.clone(), 0).unwrap();
        let id = e.admit("a", "main", Value::Null, authority, 1).unwrap();
        // Live first: the projections hold the run.
        if format == "f2" {
            assert_eq!(
                count(
                    "SELECT count(*) FROM aq_playbooks.inspection_rows WHERE namespace=$1 AND run_id=$2",
                    &ns,
                    &id
                ),
                2,
                "{format}: run and admitted receipt projected while live"
            );
        }
        assert!(e.advance(&id, 2).unwrap());
        let state = e.store.load(&ns).unwrap().state;
        assert!(state.runs.is_empty(), "{format}");
        assert!(
            state.receipts.is_empty() && state.budgets.is_empty(),
            "{format}"
        );
        if format == "f2" {
            assert_eq!(
                count(
                    "SELECT count(*) FROM aq_playbooks.inspection_rows WHERE namespace=$1 AND (run_id=$2 OR item_key=$2)",
                    &ns,
                    &id
                ),
                0,
                "{format}: inspection_rows"
            );
            assert_eq!(
                count(
                    "SELECT count(*) FROM aq_playbooks.runtime_index WHERE namespace=$1 AND (item_key=$2 OR item_key LIKE '%' || $2)",
                    &ns,
                    &id
                ),
                0,
                "{format}: runtime_index"
            );
        }
        assert_eq!(
            count(
                "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1 AND kind='runs' AND item_key=$2",
                &ns,
                &id
            ),
            1,
            "{format}: run history row"
        );
        assert_eq!(tombstone_rows(&ns), 0, "{format}: disposal tombstones");
        let runs = e.inspect(&runs_query()).unwrap();
        assert_eq!(runs.items.len(), 1, "{format}");
        assert_eq!(runs.items[0]["status"], "completed", "{format}");
        assert!(
            matches!(e.advance(&id, 3), Err(Error::NotFound(_))),
            "{format}"
        );
    }
}

// --- Task 5 fix round 1 ---------------------------------------------------

fn pg_authority() -> Authority {
    Authority {
        actor: "synthetic-test".into(),
        scope: "project".into(),
        capabilities: Default::default(),
    }
}
fn pg_command() -> aq_playbooks_model::CommandContract {
    use aq_playbooks_model::*;
    CommandContract {
        name: "test.command".into(),
        version: "1".into(),
        input: Schema::Null,
        outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
        capabilities: Default::default(),
        scopes: ["project".to_string()].into(),
        effect: Effect::Mutation,
        recovery: Recovery::KeyedIdempotent,
        asynchronous: None,
        cancellation: false,
        public_result_fields: Default::default(),
        sensitive_input_fields: Default::default(),
        sensitive_output_fields: Default::default(),
        max_payload_bytes: 1024,
        deadline_ms: 100_000,
    }
}
/// `label`'s fixture with `steps` from `start`.
fn steps_artifact(
    label: &str,
    start: &str,
    steps: Vec<(&str, aq_playbooks_model::Step)>,
    dependencies: BTreeMap<String, aq_playbooks_model::CallableDependency>,
) -> aq_playbooks_model::Artifact {
    let mut content = support::artifact(label).content;
    content.playbook.entries.get_mut("main").unwrap().start = start.into();
    content.playbook.steps = steps.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    let c = pg_command();
    content.registry.commands.insert(c.name.clone(), c);
    content.dependencies = dependencies;
    aq_playbooks_model::Artifact::seal(content).unwrap()
}
fn ret_ok() -> aq_playbooks_model::Step {
    support::artifact("a").content.playbook.steps["return"].clone()
}
/// invoke `test.command`, then return ok.
fn pg_invoke_artifact() -> aq_playbooks_model::Artifact {
    use aq_playbooks_model::*;
    let c = pg_command();
    steps_artifact(
        "invoke",
        "invoke",
        vec![
            (
                "invoke",
                Step {
                    op: Operation::Invoke {
                        command: ContractRef {
                            name: c.name.clone(),
                            fingerprint: c.fingerprint().unwrap(),
                        },
                        input: Expr::literal(Value::Null),
                        bind: "result".into(),
                        retry: None,
                    },
                    edges: BTreeMap::from([("ok".into(), "return".into())]),
                    on_error: Some("return".into()),
                },
            ),
            ("return", ret_ok()),
        ],
        BTreeMap::new(),
    )
}
fn ok_host() -> FakeHost {
    FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![
                DispatchResult::Completed {
                    outcome: "ok".into(),
                    output: Value::Null,
                },
                DispatchResult::Completed {
                    outcome: "ok".into(),
                    output: Value::Null,
                },
            ],
        )]),
        ..Default::default()
    }
}
/// C1 on PostgreSQL: a namespace stored by the pre-D10 engine (no
/// `root_intents`, no backfill flag) is backfilled when it is opened, on
/// protected format 2 and on format 3; the pre-upgrade run finishes with its
/// earlier operation in its history and a later run progresses.
#[test]
fn pre_d10_namespace_is_backfilled_on_open_on_postgres() {
    for (format, store) in both() {
        let ns = support::namespace("pre_d10");
        let e = Engine::new(store, &ns, "w").unwrap();
        let a = pg_invoke_artifact();
        e.install(a.clone()).unwrap();
        e.activate_at("invoke", &a.digest, pg_authority(), 0)
            .unwrap();
        let mut host = ok_host();
        let old = e
            .admit("invoke", "main", Value::Null, pg_authority(), 0)
            .unwrap();
        e.advance(&old, 0).unwrap();
        e.dispatch_next(&mut host, 0).unwrap();
        let v = e.store.load(&ns).unwrap();
        let intent = v.state.intents.values().next().unwrap().clone();
        assert_eq!(intent.status, IntentStatus::Completed, "{format}");
        // The State as the vendored engine stored it.
        let mut value = serde_json::to_value(&v.state).unwrap();
        let indexes = value["indexes"].as_object_mut().unwrap();
        indexes.remove("root_intents");
        indexes.remove("root_intents_complete");
        let legacy: State = serde_json::from_value(value).unwrap();
        assert!(legacy.indexes.root_intents.is_empty() && !legacy.indexes.root_intents_complete);
        e.store
            .compare_and_swap(&ns, "w", v.revision, &legacy)
            .unwrap();
        let store = e.store;
        store.release(&ns, "w").unwrap();
        let e = Engine::new(store, &ns, "w2").unwrap();
        let s = e.store.load(&ns).unwrap().state;
        assert!(s.indexes.root_intents_complete, "{format}");
        assert!(
            s.indexes.root_intents[&old].contains(&intent.id),
            "{format}"
        );
        let new = e
            .admit("invoke", "main", Value::Null, pg_authority(), 1)
            .unwrap();
        for _ in 0..10 {
            e.drive(&mut host, 1, 20).unwrap();
        }
        let s = e.store.load(&ns).unwrap().state;
        assert!(s.runs.is_empty() && s.intents.is_empty(), "{format}");
        let effects = e
            .inspect(&InspectionQuery {
                kind: InspectionKind::Effects,
                run: Some(old.clone()),
                after: None,
                limit: 10,
            })
            .unwrap();
        assert_eq!(effects.items.len(), 1, "{format}");
        assert_eq!(effects.items[0]["id"], intent.id.as_str(), "{format}");
        for id in [&old, &new] {
            let runs = e
                .inspect(&InspectionQuery {
                    kind: InspectionKind::Runs,
                    run: Some(id.clone()),
                    after: None,
                    limit: 10,
                })
                .unwrap();
            assert_eq!(runs.items[0]["outcome"], "ok", "{format} {id}");
        }
    }
}
/// M4 on PostgreSQL: a finished root's inspection items are byte-for-byte
/// the same while its cohort waits for an independent child and after the
/// cohort has left as `run_history` rows (format 2 pages through jsonb).
#[test]
fn a_finished_runs_inspection_is_identical_before_and_after_removal_on_postgres() {
    use aq_playbooks_model::*;
    for (format, store) in both() {
        let ns = support::namespace("same_items");
        let e = Engine::new(store, &ns, "w").unwrap();
        let child = pg_invoke_artifact();
        let parent = steps_artifact(
            "parent",
            "call",
            vec![
                (
                    "call",
                    Step {
                        op: Operation::Call {
                            dependency: "child".into(),
                            entry: "main".into(),
                            input: Expr::literal(Value::Null),
                            mode: CallMode::Independent,
                            bind: "handle".into(),
                        },
                        edges: BTreeMap::from([("started".into(), "return".into())]),
                        on_error: Some("return".into()),
                    },
                ),
                ("return", ret_ok()),
            ],
            BTreeMap::from([(
                "child".into(),
                CallableDependency {
                    selection: DependencySelection::Exact,
                    binding: "child".into(),
                    artifact: child.digest.clone(),
                    interfaces: child.content.playbook.entries.clone(),
                    capabilities: Default::default(),
                    scope: "project".into(),
                    dependencies: Default::default(),
                },
            )]),
        );
        e.install(child).unwrap();
        e.install(parent.clone()).unwrap();
        e.activate_at("parent", &parent.digest, pg_authority(), 0)
            .unwrap();
        let root = e
            .admit("parent", "main", Value::Null, pg_authority(), 0)
            .unwrap();
        e.advance(&root, 0).unwrap();
        e.advance(&root, 0).unwrap();
        let s = e.store.load(&ns).unwrap().state;
        assert!(
            s.runs[&root].status.terminal() && s.runs.len() == 2,
            "{format}"
        );
        let pages = |e: &Engine<PostgresStorage>| -> Vec<Vec<u8>> {
            [
                InspectionKind::Runs,
                InspectionKind::Effects,
                InspectionKind::Receipts,
            ]
            .into_iter()
            .map(|kind| {
                let page = e
                    .inspect(&InspectionQuery {
                        kind,
                        run: Some(root.clone()),
                        after: None,
                        limit: 1000,
                    })
                    .unwrap();
                serde_json::to_vec(&page.items).unwrap()
            })
            .collect()
        };
        let live = pages(&e);
        e.drive(&mut ok_host(), 1, 20).unwrap();
        assert!(e.store.load(&ns).unwrap().state.runs.is_empty(), "{format}");
        assert!(history_rows(&ns) > 0, "{format}");
        assert_eq!(pages(&e), live, "{format}");
    }
}

// --- Task 6: window entries dropped 64 per commit --------------------------

fn pg_keyed(e: &Engine<PostgresStorage>, key: &str) -> KeyedAdmissionRequest {
    KeyedAdmissionRequest {
        key: key.into(),
        binding: "a".into(),
        expected_activation_revision: e.snapshot().unwrap().bindings["a"].revision,
        entry: "main".into(),
        input: Value::Null,
        authority: pg_authority(),
    }
}
fn pg_event_contract() -> aq_playbooks_model::EventContract {
    use aq_playbooks_model::*;
    EventContract {
        sensitive_payload_fields: Default::default(),
        name: "test.signal".into(),
        version: "1".into(),
        payload: Schema::Null,
        trusted_producers: ["host".to_string()].into(),
        scopes: ["project".to_string()].into(),
        correlation_fields: ["invocation".to_string()].into(),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 1024,
        retention_ms: 1_000,
    }
}
fn pg_event(id: &str) -> aq_playbooks_model::EventEnvelope {
    let c = pg_event_contract();
    aq_playbooks_model::EventEnvelope {
        producer: "host".into(),
        event_id: id.into(),
        contract: aq_playbooks_model::ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        scope: "project".into(),
        correlation: BTreeMap::from([("invocation".into(), id.into())]),
        subject: None,
        revision: None,
        causation: None,
        occurred_at_ms: None,
        observed_at_ms: None,
        payload: Value::Null,
    }
}
/// The 64-drop script on f2 protected and f3: 65 finished keyed runs with a
/// 1 s window and one event with a 1 s window, then one admission past them.
/// The dropping commit passes the removal fence on f2 and stays on the
/// bounded path on f3; an expired key admitted again leaves first and is
/// new; no tombstone row is written.
#[test]
fn window_drop_on_postgres_is_incremental_on_f3_and_fenced_on_f2() {
    for (format, store) in both() {
        let ns = support::namespace("window");
        let e = Engine::new(store, &ns, "w").unwrap();
        let mut content = support::artifact("a").content;
        content
            .registry
            .events
            .insert("test.signal".into(), pg_event_contract());
        let a = aq_playbooks_model::Artifact::seal(content).unwrap();
        let digest = a.digest.clone();
        e.install(a).unwrap();
        e.activate_at("a", &digest, pg_authority(), 0).unwrap();
        e.set_retention_policy(RetentionPolicy {
            admission_replay_ms: 1_000,
            event_replay_ms: 1,
            ..RetentionPolicy::default()
        })
        .unwrap();
        e.admit_event("host", pg_event("e1"), 0).unwrap();
        for i in 0..65 {
            let d = e.admit_once(pg_keyed(&e, &format!("k-{i:03}")), i).unwrap();
            e.advance(&d.receipt.run_id, i).unwrap();
        }
        let s = e.snapshot().unwrap();
        assert_eq!(
            (s.keyed_admissions.len(), s.events.len()),
            (65, 1),
            "{format}"
        );
        e.store.reset_transition_stats();
        e.admit_once(pg_keyed(&e, "k-new"), 2_000).unwrap();
        let s = e.snapshot().unwrap();
        // The event (1_000) and k-000..k-062 (1_000..1_062): 64, oldest first.
        assert!(s.events.is_empty(), "{format}");
        assert_eq!(
            s.keyed_admissions.keys().cloned().collect::<Vec<_>>(),
            vec![
                "k-063".to_string(),
                "k-064".to_string(),
                "k-new".to_string()
            ],
            "{format}"
        );
        assert!(
            s.retention.event_epochs.is_empty()
                && s.retention.event_expires_at_ms.is_empty()
                && s.indexes.events.is_empty()
                && s.indexes.event_memberships.is_empty(),
            "{format}"
        );
        assert_eq!(
            s.retention.expiry.keys().copied().collect::<Vec<_>>(),
            vec![1_063, 1_064, 3_000],
            "{format}"
        );
        assert!(
            s.retention.event_tombstones.is_empty() && s.retention.expired_correlations.is_empty(),
            "{format}"
        );
        let stats = e.store.transition_stats();
        if format == "f3" {
            assert_eq!((stats.incremental, stats.heavy), (1, 0), "{format}");
        }
        // An expired key admitted again: its receipt leaves in a commit of
        // its own, then the key is new (and the drop takes k-064's sibling).
        let again = e.admit_once(pg_keyed(&e, "k-064"), 2_001).unwrap();
        assert!(!again.duplicate, "{format}");
        let s = e.snapshot().unwrap();
        assert_eq!(
            s.keyed_admissions.keys().cloned().collect::<Vec<_>>(),
            vec!["k-064".to_string(), "k-new".to_string()],
            "{format}"
        );
        assert_eq!(s.keyed_admissions["k-064"], again.receipt, "{format}");
        if format == "f3" {
            let stats = e.store.transition_stats();
            assert_eq!((stats.incremental, stats.heavy), (3, 0), "{format}");
        }
        // A redelivered event after its window is new.
        assert!(
            !e.admit_event("host", pg_event("e1"), 2_002)
                .unwrap()
                .duplicate
        );
        assert_eq!(e.snapshot().unwrap().events.len(), 1, "{format}");
        assert_eq!(tombstone_rows(&ns), 0, "{format}");
    }
}
