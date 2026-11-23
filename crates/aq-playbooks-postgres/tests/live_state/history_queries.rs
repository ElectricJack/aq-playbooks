use super::*;

/// The server's message (`postgres::Error`'s `Display` is only "db error").
fn db_message(error: &postgres::Error) -> String {
    error
        .as_db_error()
        .map_or_else(|| error.to_string(), |db| db.message().to_string())
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
        let deleted = store.delete_history(&ns, "w", 20).unwrap();
        assert_eq!(deleted, 2, "{label}"); // run + receipt of "old"
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
