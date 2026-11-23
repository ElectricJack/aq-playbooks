use super::*;

/// Write `rows` legacy tombstone rows of `kind` as the namespace writer.
fn legacy_rows(ns: &str, format: &TestFormat, kind: &str, rows: i64) {
    let mut c = support::client();
    let mut tx = c.transaction().unwrap();
    tx.query_one(
        "SELECT set_config('aq_playbooks.writer_namespace',$1,true),set_config('aq_playbooks.writer_format',$2,true)",
        &[&ns, &format.version],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO aq_playbooks.disposal_tombstones(namespace,kind,identity,content_digest,disposed_at_ms) SELECT $1,$2,'legacy-'||$2||'-'||i,NULL,1 FROM generate_series(1,$3::bigint) i",
        &[&ns, &kind, &rows],
    )
    .unwrap();
    tx.commit().unwrap();
}
fn legacy_runs_left(store: &PostgresStorage, ns: &str) -> usize {
    store
        .load(ns)
        .unwrap()
        .state
        .runs
        .keys()
        .filter(|id| id.starts_with("run-0-legacy-"))
        .count()
}
/// The brief's test, on protected format 2 and format 3: a legacy State of
/// 120 settled cohorts (indexes not initialized) and 1,200 legacy event
/// tombstone rows. The open commit moves 50 cohorts and deletes 500 rows;
/// two more commits finish both; history holds the 120 runs.
#[test]
fn legacy_protected_namespace_drains_records_and_sql_tombstones() {
    for format in &TEST_FORMATS {
        let label = format.label;
        let store = (format.connect)();
        let ns = support::namespace("drain");
        let state = with_legacy_cohorts(&State::default(), 120);
        store.acquire(&ns, "writer").unwrap();
        store.compare_and_swap(&ns, "writer", 0, &state).unwrap();
        store.release(&ns, "writer").unwrap();
        legacy_rows(&ns, format, "event", 1_200);
        // Artifact rows guard artifact bytes: the drain leaves them.
        legacy_rows(&ns, format, "artifact", 1);
        let e = Engine::new((format.connect)(), &ns, "w").unwrap();
        assert_eq!(
            (legacy_runs_left(&e.store, &ns), tombstone_rows(&ns)),
            (70, 701),
            "{label}"
        );
        e.install(support::artifact("a")).unwrap();
        e.install(support::artifact("a")).unwrap();
        assert_eq!(
            (legacy_runs_left(&e.store, &ns), tombstone_rows(&ns)),
            (0, 1),
            "{label}: the artifact row stays"
        );
        assert!(e.snapshot().unwrap().retention.drain.done, "{label}");
        assert_eq!(history_runs(&ns), 120, "{label}");
    }
}
/// Once a commit finds fewer rows than its bound, the handle issues no
/// delete for that namespace again: rows that appear behind its back stay
/// (a drained namespace pays nothing per commit); a new handle probes once.
#[test]
fn drained_tombstone_rows_cost_no_statement_per_commit() {
    for format in &TEST_FORMATS {
        let label = format.label;
        let store = (format.connect)();
        let ns = support::namespace("drained-rows");
        store.acquire(&ns, "setup").unwrap();
        store
            .compare_and_swap(&ns, "setup", 0, &State::default())
            .unwrap();
        store.release(&ns, "setup").unwrap();
        legacy_rows(&ns, format, "run", 3);
        let e = Engine::new((format.connect)(), &ns, "w").unwrap();
        assert_eq!(tombstone_rows(&ns), 0, "{label}");
        // Drained: rows written behind the handle's back are not looked for.
        legacy_rows(&ns, format, "run", 2);
        e.install(support::artifact("a")).unwrap();
        assert_eq!(tombstone_rows(&ns), 2, "{label}: no delete once drained");
        // A new handle probes once and finds them.
        e.release().unwrap();
        let e = Engine::new((format.connect)(), &ns, "w2").unwrap();
        assert_eq!(tombstone_rows(&ns), 0, "{label}");
        e.release().unwrap();
    }
}
/// The fixture of the runner's tests: a namespace the engine aq-server
/// vendors (0173673) wrote on its in-memory store (see that test).
fn vendored_fixture() -> State {
    serde_json::from_str(include_str!(
        "../../../aq-playbooks-runner/tests/fixtures/legacy_state_0173673.json"
    ))
    .unwrap()
}
fn vendored_authority() -> Authority {
    Authority {
        actor: "alice".into(),
        scope: "project".into(),
        capabilities: ["execute".to_string()].into(),
    }
}
/// Drive a namespace an older engine wrote (`ns` on `store`, already
/// holding it) to fully drained while new runs are admitted and served at
/// every step: no commit fails, every legacy run is listed exactly once at
/// every step, legacy duplicates are recognised, and in the end the record
/// and the legacy tombstone rows are drained and every legacy run is in
/// history. Returns the number of steps.
fn drain_while_serving(e: &Engine<PostgresStorage>, ns: &str, legacy: &State) -> usize {
    let all: std::collections::BTreeSet<String> = legacy.runs.keys().cloned().collect();
    let keyed = |key: &str| KeyedAdmissionRequest {
        key: key.into(),
        binding: "done".into(),
        expected_activation_revision: legacy.bindings["done"].revision,
        entry: "main".into(),
        input: Value::Null,
        authority: vendored_authority(),
    };
    let mut host = FakeHost::default();
    let mut step = 0;
    loop {
        verify_listed_once(e, &all, step);
        if e.snapshot().unwrap().retention.drain.done && tombstone_rows(ns) == 0 {
            break;
        }
        let t = 30_000 + step as i64;
        let id = e
            .admit("done", "main", Value::Null, vendored_authority(), t)
            .unwrap();
        e.drive(&mut host, t, 20).unwrap();
        assert!(!e.snapshot().unwrap().runs.contains_key(&id), "served");
        if legacy.keyed_admissions.contains_key("k-done") {
            assert!(e.admit_once(keyed("k-done"), t).unwrap().duplicate);
            assert!(e.admit_once(keyed("k-live"), t).unwrap().duplicate);
        }
        step += 1;
        assert!(step < 400, "the drain did not finish");
    }
    verify_drained(&e.snapshot().unwrap(), ns, legacy);
    step
}
/// Settle every live run the older engine left (the fixture's shapes): the
/// waiting run and both children receive their event, the asynchronous
/// operation's handle is found by recovery and its completion arrives, the
/// admitted and keyed runs advance. Every one finishes `ok` and leaves.
fn finish_live_legacy(e: &Engine<PostgresStorage>, legacy: &State, t: i64) {
    use aq_playbooks_model::EventEnvelope;
    let day = legacy.waits.values().find_map(|w| w.event.clone()).unwrap();
    let completion = legacy
        .events
        .values()
        .find(|e| e.envelope.event_id.starts_with("async-before"))
        .unwrap()
        .envelope
        .clone();
    let wake = EventEnvelope {
        event_id: "wake".into(),
        contract: day,
        correlation: BTreeMap::from([("invocation".into(), "exact-invocation".into())]),
        subject: None,
        revision: None,
        ..completion.clone()
    };
    let wake = EventEnvelope {
        payload: Value::Null,
        ..wake
    };
    e.admit_event("host", wake, t).unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "reconcile:test.command".into(),
            vec![DispatchResult::Accepted {
                handle: "server-operation".into(),
            }],
        )]),
        ..Default::default()
    };
    e.recover(&mut host, t + 1).unwrap();
    e.admit_event(
        "host",
        EventEnvelope {
            event_id: "completion".into(),
            correlation: BTreeMap::from([("operation".into(), "server-operation".into())]),
            ..completion
        },
        t + 2,
    )
    .unwrap();
    for t in t + 2..t + 10 {
        e.drive(&mut host, t, 50).unwrap();
    }
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty(), "{:?}", s.runs.keys().collect::<Vec<_>>());
    let runs = run_ids(&e.store, &e.namespace);
    for (id, run) in &legacy.runs {
        assert!(runs.contains(&Value::from(id.as_str())), "{id}");
        if !run.status.terminal() {
            let page = e
                .inspect(&InspectionQuery {
                    run: Some(id.clone()),
                    ..runs_query()
                })
                .unwrap();
            assert_eq!(page.items[0]["outcome"], "ok", "{id}: {:?}", page.items[0]);
        }
    }
}
/// The vendored engine's namespace (fixture) on protected format 2 and on
/// format 3, with the legacy tombstone rows its GC wrote (5 runs, 3 events)
/// plus 200 more, and bounds of 4 cohorts and 3 entries per commit: it
/// drains over many commits while new runs are served.
#[test]
fn vendored_namespace_drains_on_postgres_while_new_runs_are_served() {
    for format in &TEST_FORMATS {
        let label = format.label;
        let store = (format.connect)();
        let ns = support::namespace("vendored");
        let legacy = vendored_fixture();
        let mut stored = legacy.clone();
        stored.retention.policy.drain_cohorts_per_commit = 4;
        stored.retention.policy.drain_entries_per_commit = 3;
        store.acquire(&ns, "vendored").unwrap();
        store
            .compare_and_swap(&ns, "vendored", 0, &without_drain(&stored))
            .unwrap();
        store.release(&ns, "vendored").unwrap();
        legacy_rows(&ns, format, "run", 5);
        legacy_rows(&ns, format, "event", 203);
        let e = Engine::new((format.connect)(), &ns, "upgraded").unwrap();
        let steps = drain_while_serving(&e, &ns, &legacy);
        assert!(steps > 5, "{label}: {steps} steps");
        finish_live_legacy(&e, &legacy, 40_000);
    }
}
/// A namespace the vendored engine's own PostgreSQL crate wrote on protected
/// format 2 (key `k`, `[7; 32]`), named by `AQ_PLAYBOOKS_VENDORED_NAMESPACE`
/// in the test database: produced by the generator in
/// `.local/task9/generator/` (see the Task 9 report). Run with `--ignored`.
/// Its stored drain bounds are first lowered to 4 cohorts and 3 entries per
/// commit (one direct CAS, which drains nothing of the record), so the
/// drain spans many commits while new runs are served; then every live run
/// the vendored engine left is driven to completion.
#[test]
#[ignore = "needs a namespace written by the vendored engine"]
fn a_namespace_the_vendored_engine_wrote_on_postgres_drains() {
    let ns = std::env::var("AQ_PLAYBOOKS_VENDORED_NAMESPACE").unwrap();
    let keys = || {
        aq_playbooks_postgres::ProtectionKeyring::new(
            "k",
            BTreeMap::from([("k".into(), [7u8; 32])]),
        )
        .unwrap()
    };
    let store = PostgresStorage::connect_protected(&support::database(), keys()).unwrap();
    let legacy = store.load(&ns).unwrap().state;
    let before = tombstone_rows(&ns);
    assert!(before > 0, "the vendored GC wrote tombstone rows");
    let mut bounded = legacy.clone();
    bounded.retention.policy.drain_cohorts_per_commit = 4;
    bounded.retention.policy.drain_entries_per_commit = 3;
    store.acquire(&ns, "setup").unwrap();
    let revision = store.load(&ns).unwrap().revision;
    store
        .compare_and_swap(&ns, "setup", revision, &without_drain(&bounded))
        .unwrap();
    store.release(&ns, "setup").unwrap();
    let e = Engine::new(
        PostgresStorage::connect_protected(&support::database(), keys()).unwrap(),
        &ns,
        "upgraded",
    )
    .unwrap();
    assert!(!e.snapshot().unwrap().retention.drain.done);
    let steps = drain_while_serving(&e, &ns, &legacy);
    eprintln!(
        "VENDORED-PG legacy runs {} (live {}), rows {before}, steps {steps}, history runs {}",
        legacy.runs.len(),
        legacy
            .runs
            .values()
            .filter(|r| !r.status.terminal())
            .count(),
        history_runs(&ns)
    );
    assert!(steps > 1, "{steps} steps");
    finish_live_legacy(&e, &legacy, 40_000);
}

fn verify_listed_once(
    e: &Engine<PostgresStorage>,
    all: &std::collections::BTreeSet<String>,
    step: usize,
) {
    let mut ids = vec![];
    let mut after = None;
    loop {
        let page = e
            .inspect(&InspectionQuery {
                after,
                limit: 7,
                ..runs_query()
            })
            .unwrap();
        ids.extend(
            page.items
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_string())
                .filter(|id| all.contains(id)),
        );
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "step {step}: listed twice");
    assert_eq!(unique.len(), all.len(), "step {step}: missing");
}

fn verify_drained(s: &State, ns: &str, legacy: &State) {
    let r = &s.retention;
    assert!(r.event_tombstones.is_empty() && r.expired_correlations.is_empty());
    assert!(r.disposals.is_empty());
    // Runs of cohorts that were settled when the older engine left them.
    let finished = legacy
        .runs
        .values()
        .filter(|run| {
            legacy
                .runs
                .values()
                .all(|other| other.root != run.root || other.status.terminal())
        })
        .count();
    assert!(
        s.runs.values().all(|run| s
            .runs
            .values()
            .any(|other| other.root == run.root && !other.status.terminal())),
        "only live cohorts remain"
    );
    assert!(history_runs(ns) >= finished as i64);
}
