//! `run_history` against an actual PostgreSQL: page plans and commit
//! atomicity. Feature-gated like the storage suite; the caller supplies an
//! explicitly disposable database.
use super::*;
use crate::catalog_tests::support::{self, client, database, namespace};
use aq_playbooks_runner::{InspectionKind, Receipt, SettledCohort};
use serde_json::json;
use std::sync::atomic::Ordering;

fn cohort(id: &str, sequence: u64) -> SettledCohort {
    let mut run = support::boundary("history").runs["parent"].clone();
    run.id = id.into();
    run.root = id.into();
    SettledCohort {
        root: id.into(),
        finished_at_ms: 10,
        runs: vec![run],
        intents: vec![],
        receipts: vec![Receipt {
            sequence,
            run: id.into(),
            step: "s".into(),
            at_ms: 10,
            kind: "terminal".into(),
            detail: json!({}),
        }],
    }
}
fn history_rows(ns: &str) -> i64 {
    client()
        .query_one(
            "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1",
            &[&ns],
        )
        .unwrap()
        .get(0)
}

/// Every scan of `run_history` in `plan` is an index scan, and no `Sort`
/// node has `run_history` below it.
fn check_plan(plan: &Value, under_sort: bool, problems: &mut Vec<String>) {
    let node = plan["Node Type"].as_str().unwrap_or_default();
    if plan["Relation Name"] == "run_history" {
        if !matches!(node, "Index Scan" | "Index Only Scan") {
            problems.push(format!("{node} on run_history"));
        }
        if under_sort {
            problems.push("Sort over run_history".into());
        }
    }
    for child in plan["Plans"].as_array().into_iter().flatten() {
        check_plan(child, under_sort || node == "Sort", problems);
    }
}

/// Whether some node of `plan` reads `index` (an index scan, an index-only
/// scan, or the bitmap index scan under a bitmap heap scan, which names the
/// index but not the table).
fn uses_index(plan: &Value, index: &str) -> bool {
    plan["Index Name"] == index
        || plan["Plans"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|child| uses_index(child, index))
}

/// An inspection page costs the same however much history the namespace
/// holds: PostgreSQL reads `run_history` through an ordered index scan for
/// every kind and cursor, never a sort or a sequential scan. A run-filtered
/// page reads it through the run index.
#[test]
fn inspection_pages_read_history_through_ordered_index_scans() {
    let (ns, store) = (
        namespace("plans"),
        PostgresStorage::connect(&database()).unwrap(),
    );
    store.acquire(&ns, "w").unwrap();
    let mut live = State::default();
    for sequence in 1..=50 {
        live.receipts.push(Receipt {
            sequence: 1_000_000 + sequence,
            run: format!("r{}", sequence % 5),
            step: "s".into(),
            at_ms: 1,
            kind: "admitted".into(),
            detail: json!({}),
        });
    }
    store.compare_and_swap(&ns, "w", 0, &live).unwrap();
    let mut c = client();
    let mut tx = c.transaction().unwrap();
    tx.query_one(WRITE_MARKER_SQL, &[&ns]).unwrap();
    tx.execute("INSERT INTO aq_playbooks.run_history(namespace,kind,item_key,run_id,root_id,sequence,finished_at_ms,public_value) SELECT $1,k,lpad(i::text,20,'0'),'r'||(i%50),'r'||(i%50),i,i,jsonb_build_object('sequence',i) FROM generate_series(1,30000) i, unnest(ARRAY['runs','effects','receipts']) k",&[&ns]).unwrap();
    tx.commit().unwrap();
    c.batch_execute("ANALYZE aq_playbooks.run_history; ANALYZE aq_playbooks.inspection_rows")
        .unwrap();
    let mut problems = vec![];
    for kind in [
        InspectionKind::Runs,
        InspectionKind::Effects,
        InspectionKind::Receipts,
    ] {
        for run in [None, Some("r7".to_string())] {
            for after in [None, Some(format!("{:020}", 15_000))] {
                for limit in [3_i64, 1001] {
                    for (name, sql) in [("inspect", INSPECT_SQL), ("page", history::PAGE_SQL)] {
                        let explain = format!("EXPLAIN (FORMAT JSON) {sql}");
                        let plan: Value = c
                            .query_one(explain.as_str(), &[&ns, &kind.key(), &run, &after, &limit])
                            .unwrap()
                            .get(0);
                        let mut found = vec![];
                        if run.is_some() {
                            // A run's history is small: any plan that reads
                            // it through the run index is bounded, sorted
                            // or not.
                            if !uses_index(&plan[0]["Plan"], "run_history_by_run") {
                                found.push("run_history_by_run not used".into());
                            }
                        } else {
                            check_plan(&plan[0]["Plan"], false, &mut found);
                        }
                        problems.extend(found.into_iter().map(|p| {
                            format!(
                                "{name} {} run={run:?} after={after:?} limit={limit}: {p}",
                                kind.key()
                            )
                        }));
                    }
                }
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
    // The page itself: live receipts after every history receipt, in order.
    let page = store
        .inspect(
            &ns,
            &aq_playbooks_runner::InspectionQuery {
                kind: InspectionKind::Receipts,
                run: None,
                after: Some(format!("{:020}", 29_999)),
                limit: 3,
            },
        )
        .unwrap();
    let sequences: Vec<_> = page.items.iter().map(|v| v["sequence"].clone()).collect();
    assert_eq!(
        sequences,
        vec![json!(30_000), json!(1_000_001), json!(1_000_002)]
    );
}

/// A failure after the history insert rolls the history rows back with the
/// rest of the commit, and the cohort can be written by the retry.
#[test]
fn a_commit_failing_after_the_history_insert_leaves_no_rows() {
    for records in [false, true] {
        let ns = namespace("rollback");
        let store = if records {
            PostgresStorage::connect_records(&database()).unwrap()
        } else {
            PostgresStorage::connect(&database()).unwrap()
        };
        store.acquire(&ns, "w").unwrap();
        let state = State {
            settled: vec![cohort("a", 1)],
            ..State::default()
        };
        store
            .fail_catalog_before_snapshot
            .store(true, Ordering::SeqCst);
        let refused = store.compare_and_swap(&ns, "w", 0, &state).unwrap_err();
        assert!(
            refused.to_string().contains("synthetic rollback"),
            "{refused}"
        );
        assert_eq!(history_rows(&ns), 0, "records={records}");
        assert_eq!(store.compare_and_swap(&ns, "w", 0, &state).unwrap(), 1);
        assert_eq!(history_rows(&ns), 2, "records={records}");
    }
}
