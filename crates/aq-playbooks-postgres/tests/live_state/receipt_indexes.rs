use super::*;

/// The runtime-index rows of receipts (`receipt` and `all_receipt`) the
/// namespace holds: (kind, run, sequence) each.
fn receipt_index_rows(ns: &str) -> Vec<(String, String, i64)> {
    support::client()
        .query(
            "SELECT kind,lookup_key,sequence FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind IN ('receipt','all_receipt') ORDER BY kind,lookup_key,sequence",
            &[&ns],
        )
        .unwrap()
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect()
}
/// The same rows as the State's receipt indexes say they must be.
fn expected_receipt_index_rows(s: &State) -> Vec<(String, String, i64)> {
    let mut rows: Vec<(String, String, i64)> = s
        .indexes
        .receipts
        .iter()
        .flat_map(|(run, receipts)| {
            receipts
                .keys()
                .map(|sequence| ("receipt".to_string(), run.clone(), *sequence as i64))
        })
        .chain(
            s.indexes
                .all_receipts
                .keys()
                .map(|sequence| ("all_receipt".to_string(), String::new(), *sequence as i64)),
        )
        .collect();
    rows.sort();
    rows
}
/// The newest transaction id so far; rows a later commit inserts or updates
/// carry a larger `xmin` (a disposable test database never wraps).
fn current_xid() -> i64 {
    support::client()
        .query_one("SELECT pg_current_xact_id()::text::bigint", &[])
        .unwrap()
        .get(0)
}
/// Receipt index rows inserted or updated after `xid`.
fn receipt_rows_written_since(ns: &str, xid: i64) -> i64 {
    support::client()
        .query_one(
            "SELECT count(*) FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind IN ('receipt','all_receipt') AND xmin::text::bigint > $2",
            &[&ns, &(xid % 4_294_967_296)],
        )
        .unwrap()
        .get(0)
}
/// Rewrite the namespace's receipt index rows as an engine before this
/// change wrote them: `item_key` is the receipt's position in the State's
/// receipts (they are in sequence order).
fn rewrite_receipt_rows_by_position(ns: &str) {
    let mut c = support::client();
    let mut tx = support::marked(&mut c, ns);
    tx.execute(
        "UPDATE aq_playbooks.runtime_index r SET item_key=(o.position-1)::text FROM (SELECT sequence,row_number() OVER (ORDER BY sequence) AS position FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='all_receipt') o WHERE r.namespace=$1 AND r.kind IN ('receipt','all_receipt') AND r.sequence=o.sequence",
        &[&ns],
    )
    .unwrap();
    tx.commit().unwrap();
}
/// Every receipt index query answers as the State's own indexes do.
fn receipt_queries_match(e: &Engine<PostgresStorage>, ns: &str) {
    let s = e.snapshot().unwrap();
    let runs: Vec<Option<String>> = std::iter::once(None)
        .chain(s.indexes.receipts.keys().cloned().map(Some))
        .collect();
    for run in runs {
        let query = IndexQuery::Receipts {
            run,
            after_sequence: 0,
            limit: 1000,
        };
        assert_eq!(
            serde_json::to_value(e.store.query(ns, &query).unwrap()).unwrap(),
            serde_json::to_value(s.indexes.query(&s, &query)).unwrap(),
            "{query:?}"
        );
    }
}
/// `waiters` live runs waiting (each with its receipts in the record), the
/// first admitted earliest; then the commit that times it out removes its
/// cohort, whose receipts are the oldest in the record. Returns the receipt
/// index rows that commit inserted or updated.
fn rows_written_when_the_oldest_cohort_leaves(waiters: usize, legacy_rows: bool) -> i64 {
    let ns = support::namespace("receipt_rows");
    let e = Engine::new(support::protected(), &ns, "w").unwrap();
    let w = pg_waiter_artifact();
    let digest = w.digest.clone();
    e.install(w).unwrap();
    e.activate_at("waiter", &digest, pg_authority(), 0).unwrap();
    let first = e
        .admit("waiter", "main", Value::Null, pg_authority(), 0)
        .unwrap();
    e.advance(&first, 0).unwrap();
    for _ in 1..waiters {
        let id = e
            .admit("waiter", "main", Value::Null, pg_authority(), 1_000)
            .unwrap();
        e.advance(&id, 1_000).unwrap();
    }
    assert_eq!(e.snapshot().unwrap().runs.len(), waiters);
    if legacy_rows {
        rewrite_receipt_rows_by_position(&ns);
        assert!(
            support::client()
                .query_one(
                    "SELECT bool_and(item_key !~ '^[0-9]{20}$') FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind IN ('receipt','all_receipt')",
                    &[&ns],
                )
                .unwrap()
                .get::<_, bool>(0)
        );
    }
    receipt_queries_match(&e, &ns);
    let xid = current_xid();
    e.drive(&mut FakeHost::default(), 150, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert!(
        !s.runs.contains_key(&first),
        "the first waiter timed out and left"
    );
    assert_eq!(s.runs.len(), waiters - 1);
    let written = receipt_rows_written_since(&ns, xid);
    assert_eq!(receipt_index_rows(&ns), expected_receipt_index_rows(&s));
    receipt_queries_match(&e, &ns);
    // The others time out and leave too; no receipt row is left behind
    // (rows of either keying are deleted with their receipts).
    e.drive(&mut FakeHost::default(), 1_200, 10 * waiters)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty());
    assert_eq!(receipt_index_rows(&ns), expected_receipt_index_rows(&s));
    receipt_queries_match(&e, &ns);
    written
}
/// Task 9 review I1: on protected format 2 the runtime-index rows of
/// receipts are keyed by the receipt's sequence, not by its position in
/// the record. A commit that removes the oldest cohort (which moves every
/// later receipt) writes the rows of the receipts it adds, never those of
/// the receipts that moved: the same for 10 and for 40 live runs.
#[test]
fn a_commit_that_removes_a_cohort_writes_receipt_index_rows_for_that_cohort_only() {
    let few = rows_written_when_the_oldest_cohort_leaves(10, false);
    let many = rows_written_when_the_oldest_cohort_leaves(40, false);
    assert_eq!(few, many, "rows written do not depend on the record");
    assert!(few <= 10, "{few} rows");
}
/// Rows an engine before this change wrote (keyed by position) are left as
/// they are: no commit rewrites them. A receipt's row, of either keying,
/// is deleted when the receipt leaves; the queries answer the same from
/// both.
#[test]
fn receipt_index_rows_keyed_by_position_are_replaced_as_their_receipts_leave() {
    let few = rows_written_when_the_oldest_cohort_leaves(10, true);
    let many = rows_written_when_the_oldest_cohort_leaves(40, true);
    assert_eq!(few, many, "rows written do not depend on the record");
    assert!(few <= 10, "{few} rows");
}
/// A commit that writes every receipt row afresh (its namespace's indexes
/// are not marked materialized) over rows keyed by position leaves one row
/// per receipt, not two.
#[test]
fn rewriting_receipt_rows_over_rows_keyed_by_position_leaves_one_row_each() {
    let ns = support::namespace("receipt_rewrite");
    let e = Engine::new(support::protected(), &ns, "w").unwrap();
    let w = pg_waiter_artifact();
    let digest = w.digest.clone();
    e.install(w).unwrap();
    e.activate_at("waiter", &digest, pg_authority(), 0).unwrap();
    for _ in 0..5 {
        let id = e
            .admit("waiter", "main", Value::Null, pg_authority(), 0)
            .unwrap();
        e.advance(&id, 0).unwrap();
    }
    rewrite_receipt_rows_by_position(&ns);
    let mut c = support::client();
    let mut tx = support::marked(&mut c, &ns);
    tx.execute(
        "UPDATE aq_playbooks.namespaces SET indexes_materialized=false WHERE namespace=$1",
        &[&ns],
    )
    .unwrap();
    tx.commit().unwrap();
    e.admit("waiter", "main", Value::Null, pg_authority(), 1)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(receipt_index_rows(&ns), expected_receipt_index_rows(&s));
    receipt_queries_match(&e, &ns);
}
