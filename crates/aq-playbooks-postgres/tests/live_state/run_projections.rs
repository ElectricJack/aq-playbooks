use super::*;

struct RunRows<'a> {
    ns: &'a str,
    id: &'a str,
}
impl RunRows<'_> {
    fn count(&self, sql: &str) -> i64 {
        support::client()
            .query_one(sql, &[&self.ns, &self.id])
            .unwrap()
            .get(0)
    }
    fn assert_live(&self, format: &str) {
        // Live first: the projections hold the run.
        if format == "f2" {
            assert_eq!(
                self.count("SELECT count(*) FROM aq_playbooks.inspection_rows WHERE namespace=$1 AND run_id=$2"),
                2,
                "{format}: run and admitted receipt projected while live"
            );
        }
    }
    fn assert_settled(&self, format: &str) {
        if format == "f2" {
            assert_eq!(
                self.count("SELECT count(*) FROM aq_playbooks.inspection_rows WHERE namespace=$1 AND (run_id=$2 OR item_key=$2)"),
                0,
                "{format}: inspection_rows"
            );
            assert_eq!(
                self.count("SELECT count(*) FROM aq_playbooks.runtime_index WHERE namespace=$1 AND (item_key=$2 OR item_key LIKE '%' || $2)"),
                0,
                "{format}: runtime_index"
            );
        }
        assert_eq!(
            self.count("SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1 AND kind='runs' AND item_key=$2"),
            1,
            "{format}: run history row"
        );
    }
}
#[test]
fn finished_run_leaves_protected_snapshot_and_projections_and_writes_no_tombstone() {
    for (format, store) in both() {
        let ns = support::namespace("leaves");
        let e = Engine::new(store, &ns, "w").unwrap();
        let a = support::artifact("a");
        let digest = a.digest.clone();
        e.install(a).unwrap();
        let authority = pg_authority();
        e.activate_at("a", &digest, authority.clone(), 0).unwrap();
        let id = e.admit("a", "main", Value::Null, authority, 1).unwrap();
        let rows = RunRows { ns: &ns, id: &id };
        rows.assert_live(format);
        assert!(e.advance(&id, 2).unwrap());
        let state = e.store.load(&ns).unwrap().state;
        assert!(state.runs.is_empty(), "{format}");
        assert!(
            state.receipts.is_empty() && state.budgets.is_empty(),
            "{format}"
        );
        rows.assert_settled(format);
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
/// M4 on PostgreSQL: a finished root's inspection items are byte-for-byte
/// the same while its cohort waits for an independent child and after the
/// cohort has left as `run_history` rows (format 2 pages through jsonb).
#[test]
fn a_finished_runs_inspection_is_identical_before_and_after_removal_on_postgres() {
    for (format, store) in both() {
        let ns = support::namespace("same_items");
        let e = Engine::new(store, &ns, "w").unwrap();
        let child = pg_invoke_artifact();
        let parent = independent_parent(&child);
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
        let live = inspection_pages(&e, &root);
        e.drive(&mut ok_host(), 1, 20).unwrap();
        assert!(e.store.load(&ns).unwrap().state.runs.is_empty(), "{format}");
        assert!(history_rows(&ns) > 0, "{format}");
        assert_eq!(inspection_pages(&e, &root), live, "{format}");
    }
}
fn independent_parent(child: &aq_playbooks_model::Artifact) -> aq_playbooks_model::Artifact {
    use aq_playbooks_model::*;
    steps_artifact(
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
    )
}
fn inspection_pages(e: &Engine<PostgresStorage>, root: &str) -> Vec<Vec<u8>> {
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
                run: Some(root.to_owned()),
                after: None,
                limit: 1000,
            })
            .unwrap();
        serde_json::to_vec(&page.items).unwrap()
    })
    .collect()
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
