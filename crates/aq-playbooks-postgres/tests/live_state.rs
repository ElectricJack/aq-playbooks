#![cfg(feature = "postgres-tests")]
//! Live state only (spec 0003) on PostgreSQL: insert-once `run_history` rows
//! written with the commit, and inspection over live rows plus history.
use aq_playbooks_postgres::PostgresStorage;
use aq_playbooks_runner::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[path = "../../aq-playbooks-runner/tests/fixtures/removal_state.rs"]
mod removal_state;
#[allow(dead_code)]
mod support;
use removal_state::base_fence_state as fence_state;
use removal_state::{
    completed_run, refused_with, removal_fences_hold, synthetic_cohort, tombstone,
};

fn history_state(cohorts: Vec<SettledCohort>) -> State {
    State {
        settled: cohorts,
        ..State::default()
    }
}
struct TestFormat {
    label: &'static str,
    version: &'static str,
    connect: fn() -> PostgresStorage,
}
const TEST_FORMATS: [TestFormat; 2] = [
    TestFormat {
        label: "f2",
        version: "2",
        connect: support::protected,
    },
    TestFormat {
        label: "f3",
        version: "3",
        connect: support::records,
    },
];
fn both() -> [(&'static str, PostgresStorage); 2] {
    TEST_FORMATS.map(|format| (format.label, (format.connect)()))
}
fn runs_query() -> InspectionQuery {
    InspectionQuery {
        kind: InspectionKind::Runs,
        run: None,
        after: None,
        limit: 1000,
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

// --- Task 4: retention clock, window fields and removal fences ------------

fn cohort_of(run: Run) -> SettledCohort {
    SettledCohort {
        root: run.id.clone(),
        finished_at_ms: 1,
        runs: vec![run],
        intents: vec![],
        receipts: vec![],
    }
}
#[test]
fn removal_fences_hold_on_postgres() {
    removal_fences_hold(&support::protected(), &support::namespace("fence_f2"));
    removal_fences_hold(&support::records(), &support::namespace("fence_f3"));
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
fn long_contract() -> aq_playbooks_model::EventContract {
    aq_playbooks_model::EventContract {
        name: "test.long".into(),
        retention_ms: 5_000,
        ..pg_event_contract()
    }
}
fn long_artifact() -> aq_playbooks_model::Artifact {
    let mut content = support::artifact("long").content;
    let c = long_contract();
    content.registry.events.insert(c.name.clone(), c);
    aq_playbooks_model::Artifact::seal(content).unwrap()
}
#[path = "../../aq-playbooks-runner/tests/fixtures/rebase_cases.rs"]
mod rebase_cases;
use rebase_cases::*;
fn settle_during_rebase(n: &mut State) {
    let run = n.runs.remove("run-a").unwrap();
    n.receipts.retain(|r| r.sequence != 1);
    let mut c = cohort_of(run);
    c.receipts.push(audit(1, "run-a"));
    n.settled.push(c);
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

#[path = "live_state/workflows.rs"]
mod workflows;
use workflows::*;

// --- Task 8: max_tombstones is ignored ------------------------------------

/// Legacy disposal tombstone rows beyond `max_tombstones` refuse no full
/// commit and are not counted. Rows of runs are left to the drain, never to
/// the GC clock's cutoff deletes: a handle that has seen the namespace
/// drained does not look again (here they were inserted behind its back),
/// and a new handle deletes them `drain_entries_per_commit` per commit. On
/// f2 protected, f2 plain and f3.
#[test]
fn legacy_tombstone_rows_beyond_max_tombstones_refuse_no_commit_and_are_not_scanned() {
    for (label, store) in [
        ("f2", support::protected()),
        ("f2_plain", support::store()),
        ("f3", support::records()),
    ] {
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        let mut state = State::default();
        state.retention.policy.max_tombstones = 1;
        state.retention.policy.audit_ms = 1;
        state.retention.last_gc_at_ms = Some(10);
        let base = store.compare_and_swap(&ns, "w", 0, &state).unwrap();
        // 1,000 legacy run tombstone rows (no inspection row: the cutoff
        // delete would take them), written as the namespace writer.
        let mut c = support::client();
        let mut tx = c.transaction().unwrap();
        let format = if label == "f3" { "3" } else { "2" };
        tx.query_one(
            "SELECT set_config('aq_playbooks.writer_namespace',$1,true),set_config('aq_playbooks.writer_format',$2,true)",
            &[&ns, &format],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO aq_playbooks.disposal_tombstones(namespace,kind,identity,content_digest,disposed_at_ms) SELECT $1,'run','legacy-'||i,NULL,1 FROM generate_series(1,1000) i",
            &[&ns],
        )
        .unwrap();
        tx.commit().unwrap();
        let mut next = state.clone();
        next.runs.insert("run-new".into(), completed_run("run-new"));
        let revision = store.compare_and_swap(&ns, "w", base, &next).unwrap();
        assert_eq!(tombstone_rows(&ns), 1_000, "{label}");
        next.retention.last_gc_at_ms = Some(20);
        let revision = store.compare_and_swap(&ns, "w", revision, &next).unwrap();
        assert_eq!(tombstone_rows(&ns), 1_000, "{label}");
        store.release(&ns, "w").unwrap();
        let store = match label {
            "f2" => support::protected(),
            "f2_plain" => support::store(),
            _ => support::records(),
        };
        store.acquire(&ns, "w2").unwrap();
        next.sequence += 1;
        let revision = store.compare_and_swap(&ns, "w2", revision, &next).unwrap();
        assert_eq!(tombstone_rows(&ns), 500, "{label}");
        next.sequence += 1;
        store.compare_and_swap(&ns, "w2", revision, &next).unwrap();
        assert_eq!(tombstone_rows(&ns), 0, "{label}");
    }
}
// --- Task 8b (owner override): no namespace limits -----------------------

/// On f2 protected and f3, with every deprecated limit at its smallest:
/// direct, keyed and `invoke` admissions land, the runs drive to completion
/// and leave, and a clock rebase lands. Nothing is refused for capacity.
#[test]
fn nothing_is_refused_for_namespace_capacity_on_postgres() {
    for (format, store) in both() {
        let ns = support::namespace("unlimited");
        let e = Engine::with_config(
            store,
            &ns,
            "w",
            EngineConfig {
                max_runs: 1,
                max_events: 1,
                max_queued_destinations: 1,
                max_state_bytes: 1,
                max_receipts: 1,
            },
        )
        .unwrap();
        let a = support::artifact("a");
        e.install(a.clone()).unwrap();
        e.activate_at("a", &a.digest, pg_authority(), 0).unwrap();
        let invoke = pg_invoke_artifact();
        e.install(invoke.clone()).unwrap();
        e.activate_at("invoke", &invoke.digest, pg_authority(), 0)
            .unwrap();
        for i in 0..20 {
            e.admit_once(pg_keyed(&e, &format!("w{i}")), i).unwrap();
        }
        for t in 0..3 {
            e.admit("invoke", "main", Value::Null, pg_authority(), 200 + t)
                .unwrap();
        }
        let mut host = ok_host();
        host.responses
            .get_mut("test.command")
            .unwrap()
            .push(DispatchResult::Completed {
                outcome: "ok".into(),
                output: Value::Null,
            });
        e.drive(&mut host, 310, 200)
            .unwrap_or_else(|err| panic!("{format}: {err}"));
        assert!(e.snapshot().unwrap().runs.is_empty(), "{format}");
        // Pin the clock a year ahead, then rebase it.
        let now: i64 = 1_763_000_000_000;
        let pinned = now + 31_536_000_000;
        for i in 0..3 {
            let d = e
                .admit_once(pg_keyed(&e, &format!("p{i}")), pinned)
                .unwrap();
            assert!(e.advance(&d.receipt.run_id, pinned).unwrap(), "{format}");
        }
        e.rebase_retention_clock(now, now, "operator", "clock pinned a year ahead", now)
            .unwrap_or_else(|err| panic!("{format}: {err}"));
        assert_eq!(
            e.snapshot().unwrap().retention.clock_ms,
            Some(now),
            "{format}"
        );
    }
}

// --- Task 8b R1: the eviction licence on every PostgreSQL commit path -----

#[path = "../../aq-playbooks-runner/tests/fixtures/eviction_entries.rs"]
mod eviction_entries;
use eviction_entries::*;
fn eviction_base(keep: usize) -> State {
    let mut s = State::default();
    s.retention.clock_ms = Some(500);
    s.retention.policy.admission_replay_ms = EVICTION_WINDOW as u64;
    s.retention.policy.event_replay_ms = EVICTION_WINDOW as u64;
    s.retention.policy.replay_keep = keep;
    s.indexes.initialized = true;
    s.sequence = 100;
    s
}
/// As the runner's: `replay_keep` 5, order kh (held), k0..k3, e4, ew (held
/// by a waiter); kh2 waits in `replay_held`; ex is outside the order.
fn eviction_state() -> State {
    let mut s = eviction_base(5);
    keyed_entry(&mut s, "kh", 0, KeyPosition::OrderedLive);
    for i in 0..4 {
        keyed_entry(&mut s, &format!("k{i}"), i + 1, KeyPosition::Ordered);
    }
    event_entry(&mut s, "e4", 5, true);
    event_entry(&mut s, "ew", 7, true);
    hold_by_waiter(&mut s, "ew");
    keyed_entry(&mut s, "kh2", 9, KeyPosition::Held);
    event_entry(&mut s, "ex", 8, false);
    s
}
fn bulk_eviction_state() -> State {
    let mut s = eviction_base(1);
    for i in 0..70 {
        keyed_entry(&mut s, &format!("b{i:03}"), i + 1, KeyPosition::Ordered);
    }
    s
}
fn evict_bulk(n: &mut State, count: usize) {
    for i in 0..count {
        ReplayMember::Keyed(&format!("b{i:03}")).evict(n);
    }
}
#[path = "../../aq-playbooks-runner/tests/fixtures/eviction_cases.rs"]
mod eviction_cases;
#[path = "../../aq-playbooks-runner/tests/fixtures/eviction_commits.rs"]
mod eviction_commits;
use eviction_cases::{EvictionCase, common_refusals};
fn eviction_refusal_cases() -> Vec<EvictionCase> {
    let base = eviction_state();
    let mut held_event = eviction_base(4);
    event_entry(&mut held_event, "ew", 0, true);
    hold_by_waiter(&mut held_event, "ew");
    for i in 0..5 {
        keyed_entry(
            &mut held_event,
            &format!("k{i}"),
            i + 1,
            KeyPosition::Ordered,
        );
    }
    let mut cases = common_refusals(&base, &held_event);
    let backend_cases: [EvictionCase; 3] = [
        (
            "an order entry that inflates the count",
            retention::REFUSE_EVICTION,
            base.clone(),
            Box::new(|n| {
                n.retention
                    .replay_order
                    .insert(retention::replay_key(31, "k:ghost"), ());
                hold_out(n, "kh");
                ReplayMember::Keyed("k0").evict(n);
                ReplayMember::Keyed("k1").evict(n);
            }),
        ),
        (
            "a held event whose waiter index is dropped",
            retention::REFUSE_EVICTION,
            held_event.clone(),
            Box::new(|n| {
                n.indexes.waiters.clear();
                ReplayMember::Event("ew").evict(n);
            }),
        ),
        (
            "more than 64 in one commit",
            retention::REFUSE_EVICTION,
            bulk_eviction_state(),
            Box::new(|n| evict_bulk(n, 65)),
        ),
    ];
    cases.extend(backend_cases);
    cases
}
/// Store `state` as a namespace reaches it: entries join unheld, the holds
/// begin in a second commit.
fn seed_eviction_state(store: &PostgresStorage, ns: &str, state: &State) -> u64 {
    store.acquire(ns, "w").unwrap();
    let mut unheld = state.clone();
    unheld.runs.clear();
    unheld.waits.clear();
    unheld.indexes.waiters.clear();
    let first = store.compare_and_swap(ns, "w", 0, &unheld).unwrap();
    store.compare_and_swap(ns, "w", first, state).unwrap()
}
/// The eviction licence on f2 protected, f2 plain and f3 compare-and-swap.
#[test]
fn the_eviction_licence_holds_on_every_full_commit_path() {
    for (label, store) in [
        ("f2", support::protected()),
        ("f2_plain", support::store()),
        ("f3", support::records()),
    ] {
        let fresh = |state: &State| {
            let ns = support::namespace(label);
            let base = seed_eviction_state(&store, &ns, state);
            (ns, base)
        };
        let failures = eviction_commits::cas(&store, &fresh);
        assert!(failures.is_empty(), "{label}: {failures:#?}");
    }
}
/// On f3 bounded transitions: each refusal is made by the bounded commit
/// itself (no complete path) and writes nothing; a correct eviction, and 64
/// at once, commit incrementally.
#[test]
fn format_3_evictions_are_incremental_and_fenced() {
    let store = support::records();
    let fresh = |state: &State| {
        let ns = support::namespace("f3_evict");
        let base = seed_eviction_state(&store, &ns, state);
        (ns, base)
    };
    let mut failures = eviction_commits::refusals(&store, &fresh, |ns, _, _, mutate| {
        store.set_profiling(true);
        let result = eviction_commits::transition(&store, ns, mutate);
        let complete_path = store
            .profiling_snapshot()
            .unwrap()
            .operations
            .contains_key("cas");
        store.set_profiling(false);
        (result, complete_path)
    });
    store.reset_transition_stats();
    failures.extend(eviction_commits::allowances(&fresh, |ns, _, _, mutate| {
        eviction_commits::transition(&store, ns, mutate)
    }));
    assert!(failures.is_empty(), "{failures:#?}");
    let stats = store.transition_stats();
    assert_eq!((stats.incremental, stats.heavy), (2, 0));
}

#[path = "live_state/receipt_indexes.rs"]
mod receipt_indexes;

#[path = "live_state/planner_statistics.rs"]
mod planner_statistics;

#[path = "live_state/format_parity.rs"]
mod format_parity;

#[path = "live_state/history_retention.rs"]
mod history_retention;

/// `state` without its drain record, as an older engine stored it.
fn without_drain(state: &State) -> State {
    let mut value = serde_json::to_value(state).unwrap();
    value["retention"].as_object_mut().unwrap().remove("drain");
    serde_json::from_value(value).unwrap()
}

fn history_runs(ns: &str) -> i64 {
    support::client()
        .query_one(
            "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1 AND kind='runs'",
            &[&ns],
        )
        .unwrap()
        .get(0)
}

/// `state` with `count` settled legacy cohorts, as an older engine left
/// them: a completed root with its admitted and terminal receipts and its
/// budget, runtime indexes not initialized, no drain record.
fn with_legacy_cohorts(state: &State, count: usize) -> State {
    let mut state = state.clone();
    for i in 0..count {
        let id = format!("run-0-legacy-{i:04}");
        let mut run = completed_run(&id);
        for kind in ["admitted", "terminal"] {
            state.sequence += 1;
            state.receipts.push(Receipt {
                kind: kind.into(),
                ..audit(state.sequence, &id)
            });
        }
        run.terminal_sequence = Some(state.sequence);
        state.runs.insert(id.clone(), run);
        state.budgets.insert(id, Budget::default());
    }
    state.indexes.initialized = false;
    without_drain(&state)
}

#[path = "live_state/legacy_drain.rs"]
mod legacy_drain;

#[path = "live_state/history_queries.rs"]
mod history_queries;

#[path = "live_state/history_commits.rs"]
mod history_commits;

#[path = "live_state/event_windows.rs"]
mod event_windows;

#[path = "live_state/commit_fences.rs"]
mod commit_fences;

#[path = "live_state/run_projections.rs"]
mod run_projections;
