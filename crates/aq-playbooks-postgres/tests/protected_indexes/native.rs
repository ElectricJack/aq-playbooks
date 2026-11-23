use super::*;

fn indexed_state(n: &str) -> State {
    let mut state = boundary(n);
    state.runs.get_mut("parent").unwrap().authority.scope = "a".into();
    state.runs.get_mut("child").unwrap().authority.scope = "b".into();
    state.indexes.ready = BTreeMap::from([
        ("a".into(), BTreeMap::from([(2, "parent".into())])),
        ("b".into(), BTreeMap::from([(1, "child".into())])),
    ]);
    state.indexes.scope_cursor = Some("a".into());
    state.indexes.effects.insert(
        "dispatch".into(),
        BTreeMap::from([("a".into(), BTreeSet::from(["intent".into()]))]),
    );
    state
        .indexes
        .effect_cursor
        .insert("dispatch".into(), "a".into());
    state.indexes.timers.insert(
        "timer".into(),
        TimerIndex {
            key: "timer".into(),
            run: "child".into(),
            at_ms: 50,
            intent: None,
        },
    );
    state
        .indexes
        .due
        .insert(50, BTreeSet::from(["timer".into()]));
    let correlation = digest(&json!({"operation":"synthetic"})).unwrap();
    state
        .indexes
        .events
        .insert(correlation.clone(), BTreeMap::from([(7, "event".into())]));
    state
        .indexes
        .waiters
        .insert(correlation, BTreeSet::from(["parent".into()]));
    state.indexes.runs = state.runs.keys().cloned().collect();
    state
        .indexes
        .receipts
        .insert("parent".into(), BTreeMap::from([(1, 0)]));
    state.indexes.all_receipts.insert(1, 0);
    state
}

#[test]
fn native_indexes_match_memory_and_inspection_uses_bounded_keyset_pages() {
    let n = namespace("native-indexes");
    let state = indexed_state(&n);
    let s = store();
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &state).unwrap();
    let key = state.indexes.events.keys().next().unwrap().clone();
    for query in index_queries(key) {
        assert_eq!(
            serde_json::to_value(s.query(&n, &query).unwrap()).unwrap(),
            serde_json::to_value(state.indexes.query(&state, &query)).unwrap()
        );
    }
    let q = InspectionQuery {
        kind: InspectionKind::Runs,
        run: None,
        after: None,
        limit: 1,
    };
    let first = s.inspect(&n, &q).unwrap();
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.revision, 1);
    assert!(first.next.is_some());
    let second = s
        .inspect(
            &n,
            &InspectionQuery {
                after: first.next,
                ..q
            },
        )
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert!(second.next.is_none());
    assert_ne!(first.items[0]["id"], second.items[0]["id"]);
    let index_count:i64=client().query_one("SELECT count(*) FROM pg_indexes WHERE schemaname='aq_playbooks' AND tablename='runtime_index'",&[]).unwrap().get(0);
    assert!(index_count >= 4);
}

#[test]
fn native_same_scope_recovery_head_rotates_and_unchanged_history_rows_are_not_rewritten() {
    let n = namespace("recovery-fairness");
    let mut state = indexed_state(&n);
    state.indexes.effects.insert(
        "recovery".into(),
        BTreeMap::from([(
            "a".into(),
            BTreeSet::from(["intent-1".into(), "intent-2".into(), "intent-3".into()]),
        )]),
    );
    state
        .indexes
        .effect_item_cursor
        .insert("recovery:a".into(), "intent-1".into());
    let s = store();
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &state).unwrap();
    let query = IndexQuery::Effects {
        kind: EffectQueueKind::Recovery,
        scope_after: None,
        limit: 1,
    };
    assert_eq!(s.query(&n, &query).unwrap().items[0].key, "intent-2");
    let before:String=client().query_one("SELECT xmin::text FROM aq_playbooks.inspection_rows WHERE namespace=$1 AND kind='receipts' LIMIT 1",&[&n]).unwrap().get(0);
    state
        .indexes
        .effect_item_cursor
        .insert("recovery:a".into(), "intent-3".into());
    s.compare_and_swap(&n, "owner", 1, &state).unwrap();
    assert_eq!(s.query(&n, &query).unwrap().items[0].key, "intent-1");
    let after:String=client().query_one("SELECT xmin::text FROM aq_playbooks.inspection_rows WHERE namespace=$1 AND kind='receipts' LIMIT 1",&[&n]).unwrap().get(0);
    assert_eq!(before, after);
    assert_eq!(client().query_one("SELECT count(*) FROM aq_playbooks.runtime_index WHERE namespace=$1 AND kind='effect:recovery'",&[&n]).unwrap().get::<_,i64>(0),1);
}

#[test]
fn concurrent_inspection_revision_and_materialized_rows_commit_together() {
    let n = namespace("inspection-race");
    let s = Arc::new(store());
    s.acquire(&n, "owner").unwrap();
    let mut state = boundary(&n);
    state.runs.get_mut("parent").unwrap().visits = 1;
    s.compare_and_swap(&n, "owner", 0, &state).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let writer = {
        let s = s.clone();
        let n = n.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            for revision in 1..20 {
                state.runs.get_mut("parent").unwrap().visits = revision + 1;
                s.compare_and_swap(&n, "owner", revision, &state).unwrap();
            }
        })
    };
    let reader = store();
    barrier.wait();
    for _ in 0..40 {
        let page = reader
            .inspect(
                &n,
                &InspectionQuery {
                    kind: InspectionKind::Runs,
                    run: Some("parent".into()),
                    after: None,
                    limit: 1,
                },
            )
            .unwrap();
        assert_eq!(page.items[0]["visits"].as_u64(), Some(page.revision));
    }
    writer.join().unwrap();
}

fn index_queries(key: String) -> [IndexQuery; 11] {
    [
        IndexQuery::Ready {
            scope_after: None,
            limit: 1,
        },
        IndexQuery::Ready {
            scope_after: Some("b".into()),
            limit: 2,
        },
        IndexQuery::DueTimers {
            now_ms: 49,
            limit: 2,
        },
        IndexQuery::DueTimers {
            now_ms: 50,
            limit: 2,
        },
        IndexQuery::Effects {
            kind: EffectQueueKind::Dispatch,
            scope_after: None,
            limit: 1,
        },
        IndexQuery::Effects {
            kind: EffectQueueKind::Completion,
            scope_after: None,
            limit: 1,
        },
        IndexQuery::CorrelatedEvents {
            key: key.clone(),
            after_sequence: 0,
            limit: 1,
        },
        IndexQuery::Waiters { key, limit: 1 },
        IndexQuery::Runs {
            after: Some("child".into()),
            limit: 1,
        },
        IndexQuery::Receipts {
            run: None,
            after_sequence: 0,
            limit: 1,
        },
        IndexQuery::Receipts {
            run: Some("parent".into()),
            after_sequence: 0,
            limit: 1,
        },
    ]
}
