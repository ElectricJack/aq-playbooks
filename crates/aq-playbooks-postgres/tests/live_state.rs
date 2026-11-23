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
/// The refusals every commit path must make, each as a change of
/// `fence_state()`.
fn refusal_cases() -> Vec<Case> {
    let early = retention::REFUSE_EARLY_REMOVAL;
    let shortened = retention::REFUSE_EXPIRY;
    let mut cases: Vec<Case> = vec![
        ("run", retention::REFUSE_RUN_REMOVAL, |n| {
            n.runs.remove("run-a");
        }),
        (
            "running settled run",
            "unresolved run obligations cannot be disposed",
            |n| {
                let running = n.runs.remove("run-b").unwrap();
                n.receipts.retain(|r| r.sequence != 2);
                let mut c = cohort_of(running);
                c.receipts.push(audit(2, "run-b"));
                n.settled.push(c);
            },
        ),
        ("keyed admission", early, |n| {
            n.retention.clock_ms = Some(2_999);
            n.keyed_admissions.remove("k");
            n.retention.admission_expires_at_ms.remove("k");
        }),
        ("keyed admission, policy window", early, |n| {
            n.retention.clock_ms = Some(1_999);
            n.keyed_admissions.remove("fallback");
        }),
        ("event", early, |n| {
            n.retention.clock_ms = Some(3_999);
            n.events.remove("ev");
            n.retention.event_expires_at_ms.remove("ev");
        }),
        ("forged legacy tombstone", early, |n| {
            n.events.remove("ev");
            n.retention.event_expires_at_ms.remove("ev");
            n.retention
                .event_tombstones
                .insert("ev".into(), tombstone("digest-ev"));
        }),
        (
            "changed receipt",
            "immutable keyed admission receipt removed or changed",
            |n| {
                n.keyed_admissions.get_mut("k").unwrap().run_id = "other".into();
            },
        ),
        (
            "receipt of a live run",
            retention::REFUSE_RUN_REMOVAL,
            |n| {
                n.receipts.retain(|r| r.sequence != 2);
            },
        ),
        ("receipt overwritten", retention::REFUSE_RUN_REMOVAL, |n| {
            *n.receipts.get_mut(1).unwrap() = audit(3, "run-b");
        }),
        (
            "receipt of a settled run without its history",
            retention::REFUSE_RUN_REMOVAL,
            |n| {
                let run = n.runs.remove("run-a").unwrap();
                n.receipts.retain(|r| r.sequence != 1);
                n.settled.push(cohort_of(run));
            },
        ),
        ("lowered expiry", shortened, |n| {
            n.retention
                .admission_expires_at_ms
                .insert("k".into(), 2_500);
        }),
        ("expiry deleted, item kept", shortened, |n| {
            n.retention.event_expires_at_ms.remove("ev");
        }),
        (
            "new expiry below the fallback, stored item",
            shortened,
            |n| {
                n.retention
                    .admission_expires_at_ms
                    .insert("fallback".into(), 1_999);
            },
        ),
        ("new expiry below the fallback, new item", shortened, |n| {
            n.keyed_admissions.insert("new".into(), receipt("new", 100));
            n.retention
                .admission_expires_at_ms
                .insert("new".into(), 2_099);
        }),
        ("clock", retention::REFUSE_CLOCK, |n| {
            n.retention.clock_ms = Some(499);
        }),
    ];
    cases.extend(rebase_refusal_cases());
    cases
}
/// A licensed rebase commits on every full path (protected and plain
/// format 2, format 3 compare-and-swap) and through a format 3 transition
/// (on the complete path); after it the fences are closed again.
#[test]
fn a_clock_rebase_is_licensed_on_every_commit_path() {
    let stores = [
        ("f2_protected", support::protected()),
        ("f2_plain", support::store()),
        ("f3_cas", support::records()),
    ];
    for (label, store) in stores {
        for (case, text, f) in after_rebase_refusal_cases() {
            let ns = support::namespace(label);
            store.acquire(&ns, "w").unwrap();
            let base = store.compare_and_swap(&ns, "w", 0, &fence_state()).unwrap();
            let mut next = fence_state();
            rebased(&mut next);
            let r = store.compare_and_swap(&ns, "w", base, &next).unwrap();
            let s = store.load(&ns).unwrap().state;
            assert_eq!(
                (
                    s.retention.clock_ms,
                    s.retention.stale_floor_ms,
                    s.retention.admission_expires_at_ms.get("k").copied()
                ),
                (Some(100), Some(50), Some(2_100)),
                "{label}"
            );
            assert_eq!(
                s.retention.event_expires_at_ms.get("ev-long").copied(),
                Some(5_100),
                "{label}"
            );
            assert_eq!(history_rows(&ns), 1, "{label}: the rebase receipt");
            let mut later = s.clone();
            f(&mut later);
            let result = store.compare_and_swap(&ns, "w", r, &later);
            assert!(
                refused_with(result.clone(), text),
                "{label} {case}: {result:?}"
            );
            assert_eq!(store.load(&ns).unwrap().revision, r, "{label} {case}");
        }
        let ns = support::namespace(label);
        store.acquire(&ns, "w").unwrap();
        let base = store.compare_and_swap(&ns, "w", 0, &fence_state()).unwrap();
        let mut next = fence_state();
        rebased(&mut next);
        let r = store.compare_and_swap(&ns, "w", base, &next).unwrap();
        let mut later = store.load(&ns).unwrap().state;
        floor_lowered(&mut later);
        store.compare_and_swap(&ns, "w", r, &later).unwrap();
        assert_eq!(
            store.load(&ns).unwrap().state.retention.stale_floor_ms,
            Some(40),
            "{label}"
        );
    }
    let store = support::records();
    let ns = support::namespace("f3_rebase");
    store.acquire(&ns, "w").unwrap();
    let mut s = fence_state();
    s.indexes.initialized = true;
    store.compare_and_swap(&ns, "w", 0, &s).unwrap();
    let transition = |f: &dyn Fn(&mut State)| {
        let mut plan = |s: &mut State| {
            f(s);
            Ok(true)
        };
        storage_error(store.transition_v1(
            &ns,
            "w",
            &mut TransitionRequest {
                operation: "rebase",
                plan: &mut plan,
            },
        ))
    };
    store.reset_transition_stats();
    transition(&rebased).unwrap();
    assert_eq!(store.transition_stats().heavy, 1);
    let r = store.load(&ns).unwrap().state.retention;
    assert_eq!(
        (
            r.clock_ms,
            r.stale_floor_ms,
            r.event_expires_at_ms.get("ev").copied()
        ),
        (Some(100), Some(50), Some(3_100))
    );
    for (case, text, f) in after_rebase_refusal_cases() {
        let result = transition(&f);
        assert!(refused_with(result.clone(), text), "{case}: {result:?}");
    }
    // The floor lowered with its own receipt, also on the complete path.
    transition(&floor_lowered).unwrap();
    assert_eq!(store.transition_stats().heavy, 2);
    assert_eq!(
        store.load(&ns).unwrap().state.retention.stale_floor_ms,
        Some(40)
    );
    // A rebase that lowers only the clock also takes the complete path.
    let ns = support::namespace("f3_rebase_clock_only");
    store.acquire(&ns, "w").unwrap();
    store.compare_and_swap(&ns, "w", 0, &s).unwrap();
    let mut plan = |s: &mut State| {
        s.retention.clock_ms = Some(100);
        license(s, retention::CLOCK_REBASE, 500, 100);
        Ok(true)
    };
    storage_error(store.transition_v1(
        &ns,
        "w",
        &mut TransitionRequest {
            operation: "rebase",
            plan: &mut plan,
        },
    ))
    .unwrap();
    assert_eq!(store.load(&ns).unwrap().state.retention.clock_ms, Some(100));
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
        storage_error(store.transition_v1(
            ns,
            "w",
            &mut TransitionRequest {
                operation: "fence",
                plan: &mut plan,
            },
        ))
    };
    store.reset_transition_stats();
    let mut cases = refusal_cases();
    // Also when the commit is heavy for another reason.
    cases.push((
        "early removal, heavy",
        retention::REFUSE_EARLY_REMOVAL,
        |n| {
            n.execution_denied.insert("x".into());
            n.retention.clock_ms = Some(2_999);
            n.keyed_admissions.remove("k");
            n.retention.admission_expires_at_ms.remove("k");
        },
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
        let result = transition(&ns, f);
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
    assert!(!state.keyed_admissions.contains_key("k") && !state.events.contains_key("ev"));
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

fn pg_keyed<S: Storage>(e: &Engine<S>, key: &str) -> KeyedAdmissionRequest {
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
            s.retention
                .expiry
                .keys()
                .map(|k| retention::window_entry(k).unwrap().0)
                .collect::<Vec<_>>(),
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
        if format == "f3" {
            // A burst in one millisecond: one small record per entry, and
            // each admission stays on the bounded path.
            e.store.reset_transition_stats();
            for i in 0..20 {
                e.admit_once(pg_keyed(&e, &format!("burst-{i:02}")), 2_003)
                    .unwrap();
            }
            assert_eq!(e.store.transition_stats().heavy, 0);
            let row = support::client()
                .query_one(
                    "SELECT count(*), max(octet_length(value)) FROM aq_playbooks.state_records WHERE namespace=$1 AND field='retention.expiry'",
                    &[&ns],
                )
                .unwrap();
            let (records, largest): (i64, i32) = (row.get(0), row.get(1));
            assert_eq!(records, 23, "k-064, k-new, the event and 20 burst entries");
            assert!(largest <= 8, "{largest} bytes");
        }
    }
}

/// A waiter on `test.signal` with correlation `invocation = "held"`: it
/// holds the event `pg_event("held")` while it waits (it is never driven
/// here, so its deadline never fires).
fn pg_waiter_artifact() -> aq_playbooks_model::Artifact {
    use aq_playbooks_model::*;
    let c = pg_event_contract();
    let mut content = support::artifact("waiter").content;
    content.playbook.entries.get_mut("main").unwrap().start = "wait".into();
    content.playbook.steps = BTreeMap::from([
        (
            "wait".to_string(),
            Step {
                op: Operation::Wait {
                    event: Some(ContractRef {
                        name: c.name.clone(),
                        fingerprint: c.fingerprint().unwrap(),
                    }),
                    correlation: BTreeMap::from([("invocation".into(), Expr::literal("held"))]),
                    minimum_revision: None,
                    deadline_ms: 100,
                    bind: "completion".into(),
                },
                edges: BTreeMap::from([
                    ("received".into(), "return".into()),
                    ("timeout".into(), "return".into()),
                ]),
                on_error: Some("return".into()),
            },
        ),
        ("return".to_string(), ret_ok()),
    ]);
    content.registry.events.insert(c.name.clone(), c);
    Artifact::seal(content).unwrap()
}
/// Ruling 3, the full scenario on PostgreSQL protected format 2 and format
/// 3: normal operation, one far-future host time, stale refusals that write
/// nothing and entries that do not expire, the rebase, then normal
/// operation with held events, live runs and duplicates intact and the
/// record no longer growing.
#[test]
fn a_pinned_clock_is_rebased_on_postgres() {
    for (format, store) in both() {
        let ns = support::namespace("rebase");
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
        let w = pg_waiter_artifact();
        let waiter_digest = w.digest.clone();
        e.install(w).unwrap();
        e.activate_at("waiter", &waiter_digest, pg_authority(), 0)
            .unwrap();
        e.set_retention_policy(RetentionPolicy {
            admission_replay_ms: 1_000,
            event_replay_ms: 1,
            ..RetentionPolicy::default()
        })
        .unwrap();
        let t0: i64 = 10_000;
        let pinned = t0 + 1_000_000_000;
        let done = e.admit_once_at(pg_keyed(&e, "done"), Some(t0), t0).unwrap();
        e.advance(&done.receipt.run_id, t0).unwrap();
        let live = e.admit_once_at(pg_keyed(&e, "live"), Some(t0), t0).unwrap();
        let waiter = e
            .admit("waiter", "main", Value::Null, pg_authority(), t0)
            .unwrap();
        e.advance(&waiter, t0).unwrap();
        let held = e
            .admit_event_at("host", pg_event("held"), 0, Some(t0), t0)
            .unwrap();
        e.admit("a", "main", Value::Null, pg_authority(), pinned)
            .unwrap();
        let s = e.snapshot().unwrap();
        assert!(!s.keyed_admissions.contains_key("done"), "{format}");
        assert!(s.events.contains_key(&held.key), "{format}");
        // Real time again: stale, and nothing written (M5).
        let t1 = t0 + 200;
        let revision = e.store.load(&ns).unwrap().revision;
        assert!(matches!(
            e.admit_once_at(pg_keyed(&e, "during"), Some(t1), t1),
            Err(Error::Stale { .. })
        ));
        assert!(matches!(
            e.admit_event_at("host", pg_event("during-e"), 0, Some(t1), t1),
            Err(Error::Stale { .. })
        ));
        assert_eq!(e.store.load(&ns).unwrap().revision, revision, "{format}");
        for i in 0..3 {
            let d = e
                .admit_once(pg_keyed(&e, &format!("pin-{i}")), t1 + i)
                .unwrap();
            e.advance(&d.receipt.run_id, t1 + i).unwrap();
        }
        e.admit("a", "main", Value::Null, pg_authority(), t1 + 5_000)
            .unwrap();
        assert_eq!(e.snapshot().unwrap().keyed_admissions.len(), 4, "{format}");
        // The rebase.
        let t2 = t0 + 500;
        let report = e
            .rebase_retention_clock(t2, t0, "ops", "pinned clock", t2)
            .unwrap();
        assert_eq!(
            (
                report.from_ms,
                report.capped_admissions,
                report.capped_events
            ),
            (pinned, 4, 1),
            "{format}"
        );
        let s = e.snapshot().unwrap();
        assert_eq!(
            (s.retention.clock_ms, s.retention.stale_floor_ms),
            (Some(t2), Some(t0)),
            "{format}"
        );
        assert_eq!(
            s.retention.event_expires_at_ms.get(&held.key).copied(),
            Some(t2 + 1_000),
            "{format}"
        );
        let rows: i64 = support::client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1 AND kind='receipts' AND public_value->>'kind'='retention_clock_rebased'",
                &[&ns],
            )
            .unwrap()
            .get(0);
        assert_eq!(rows, 1, "{format}: the rebase receipt is history");
        // Normal operation resumes.
        let during = e
            .admit_once_at(pg_keyed(&e, "during"), Some(t1), t2)
            .unwrap();
        assert!(!during.duplicate, "{format}");
        assert!(
            e.admit_once_at(pg_keyed(&e, "during"), Some(t1), t2 + 10)
                .unwrap()
                .duplicate
        );
        assert!(
            e.admit_once_at(pg_keyed(&e, "live"), Some(t0), t2 + 10)
                .unwrap()
                .duplicate
        );
        assert!(
            e.admit_event_at("host", pg_event("held"), 0, Some(t0), t2 + 10)
                .unwrap()
                .duplicate
        );
        let s = e.snapshot().unwrap();
        assert!(s.runs.contains_key(&waiter), "{format}");
        assert!(s.runs.contains_key(&live.receipt.run_id), "{format}");
        // The floor refuses a retry of the early-forgotten key.
        assert!(matches!(
            e.admit_once_at(pg_keyed(&e, "done"), Some(t0), t2 + 10),
            Err(Error::Stale { floor_ms: Some(f), .. }) if f == t0
        ));
        // The record stops growing.
        for run in [&live.receipt.run_id, &during.receipt.run_id] {
            e.advance(run, t2 + 20).unwrap();
        }
        for i in 0..10 {
            let at = t2 + 1_001 + i * 200;
            let d = e
                .admit_once_at(pg_keyed(&e, &format!("after-{i}")), Some(at), at)
                .unwrap();
            e.advance(&d.receipt.run_id, at).unwrap();
        }
        let s = e.snapshot().unwrap();
        let mut keys: Vec<String> = s.keyed_admissions.keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            (5..10).map(|i| format!("after-{i}")).collect::<Vec<_>>(),
            "{format}"
        );
    }
}

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
/// The engine on PostgreSQL with `replay_keep` 3: events past it evict the
/// oldest, on f3 incrementally (no heavy commit), on f2 protected through
/// the complete fence; an evicted event delivered again is new, a kept one
/// a duplicate. Finished keyed runs then evict the oldest too.
#[test]
fn evictions_on_postgres_are_incremental_on_f3_and_fenced_on_f2() {
    for (format, store) in both() {
        let ns = support::namespace("evict");
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
            replay_keep: 3,
            ..RetentionPolicy::default()
        })
        .unwrap();
        e.store.reset_transition_stats();
        for i in 0..6 {
            e.admit_event("host", pg_event(&format!("e{i}")), i)
                .unwrap();
        }
        let s = e.snapshot().unwrap();
        assert_eq!(s.events.len(), 3, "{format}");
        assert_eq!(s.retention.replay_order.len(), 3, "{format}");
        if format == "f3" {
            let stats = e.store.transition_stats();
            assert_eq!((stats.incremental, stats.heavy), (6, 0), "{format}");
        }
        assert!(
            e.admit_event("host", pg_event("e5"), 10).unwrap().duplicate,
            "{format}"
        );
        assert!(
            !e.admit_event("host", pg_event("e0"), 11).unwrap().duplicate,
            "{format}"
        );
        for i in 0..4 {
            let d = e
                .admit_once(pg_keyed(&e, &format!("k{i}")), 20 + i)
                .unwrap();
            e.advance(&d.receipt.run_id, 20 + i).unwrap();
        }
        let s = e.snapshot().unwrap();
        assert_eq!(s.retention.replay_order.len(), 3, "{format}");
        assert_eq!(s.keyed_admissions.len() + s.events.len(), 3, "{format}");
        assert!(s.events.is_empty(), "{format}");
        assert!(!s.keyed_admissions.contains_key("k0"), "{format}");
    }
}

/// On protected format 2 and format 3, `collect_garbage` over an expired
/// event writes no `disposal_tombstones` row and no tombstone, fence or
/// disposal record: the event leaves by the commit's window drop, and a
/// redelivery is new.
#[test]
fn collect_garbage_writes_no_disposal_tombstone_row_on_postgres() {
    for (format, store) in both() {
        let ns = support::namespace("gc_rows");
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
            event_replay_ms: 1,
            audit_ms: 1,
            ..RetentionPolicy::default()
        })
        .unwrap();
        let first = e.admit_event("host", pg_event("gone"), 0).unwrap();
        let report = e.collect_garbage(10_000, 100).unwrap();
        assert_eq!(report.events, 1, "{format}");
        let s = e.snapshot().unwrap();
        assert!(!s.events.contains_key(&first.key), "{format}");
        assert_eq!(
            (
                s.retention.event_tombstones.len(),
                s.retention.expired_correlations.len(),
                s.retention.disposals.len()
            ),
            (0, 0, 0),
            "{format}: legacy retention records"
        );
        assert_eq!(tombstone_rows(&ns), 0, "{format}");
        assert!(
            !e.admit_event("host", pg_event("gone"), 10_001)
                .unwrap()
                .duplicate,
            "{format}"
        );
    }
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
