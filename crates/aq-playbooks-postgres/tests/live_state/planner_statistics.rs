use super::*;

struct PlannerDatabase {
    name: String,
    url: String,
}

impl PlannerDatabase {
    /// A database of its own (`CREATE DATABASE`), so the tests below can reset
    /// planner statistics without touching other tests' tables. Returns its
    /// name and its connection string.
    fn new(label: &str) -> Self {
        let name = format!("t10_{}_{}", label, std::process::id());
        let mut c = support::client();
        c.batch_execute(&format!("DROP DATABASE IF EXISTS {name}"))
            .unwrap();
        c.batch_execute(&format!("CREATE DATABASE {name}")).unwrap();
        let url = format!("{} dbname={name}", support::database());
        Self { name, url }
    }
    fn client(&self) -> postgres::Client {
        postgres::Client::connect(&self.url, postgres::NoTls).unwrap()
    }
    /// As a fresh `pg_restore` leaves it: PostgreSQL 17 restores no column
    /// statistics, while its index builds set `reltuples`. Needs a superuser,
    /// as the disposable test server's `postgres` is.
    fn forget_statistics(&self) {
        let mut c = self.client();
        // `reltuples` as the restore's index builds set it, then no statistics.
        c.batch_execute("ANALYZE aq_playbooks.runtime_index")
            .unwrap();
        c.batch_execute(
            "DELETE FROM pg_statistic WHERE starelid IN (SELECT oid FROM pg_class WHERE relnamespace='aq_playbooks'::regnamespace)",
        )
        .unwrap();
    }
    /// Whether `table` has column statistics.
    fn has_statistics(&self, table: &str) -> bool {
        self.client()
            .query_one(
                "SELECT EXISTS(SELECT 1 FROM pg_stats WHERE schemaname='aq_playbooks' AND tablename=$1)",
                &[&table],
            )
            .unwrap()
            .get(0)
    }
    fn reltuples(&self, table: &str) -> f32 {
        self.client()
            .query_one(
                "SELECT reltuples FROM pg_class WHERE oid=('aq_playbooks.'||$1)::regclass",
                &[&table],
            )
            .unwrap()
            .get(0)
    }
    /// The plan of the drain's receipt-row delete for one named `all_receipt`
    /// row of namespace `ns`.
    fn receipt_delete_plan(&self, ns: &str) -> String {
        let rows = json!([{"kind":"all_receipt","lookup_key":"","item_key":"00000000000000000005","keep":false}]);
        self.client()
            .query(
                "EXPLAIN DELETE FROM aq_playbooks.runtime_index WHERE ctid=ANY(ARRAY(SELECT r.ctid FROM jsonb_to_recordset($2) AS d(kind text,lookup_key text,item_key text,keep boolean) CROSS JOIN LATERAL (SELECT old.ctid FROM aq_playbooks.runtime_index old WHERE old.namespace=$1 AND old.kind=d.kind AND old.lookup_key=d.lookup_key AND old.sequence=d.item_key::bigint AND (NOT d.keep OR old.item_key<>d.item_key) OFFSET 0) r))",
                &[&ns, &rows],
            )
            .unwrap()
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Review I1: the receipt-row delete stays an index lookup per named row
/// only when `runtime_index` has statistics. A database restored without
/// them (and not yet reached by autovacuum) is analyzed when the storage
/// connects, before any commit: every engine table never analyzed, or
/// holding rows but no column statistics, gets its own `ANALYZE`.
#[test]
fn opening_the_storage_analyzes_engine_tables_that_have_no_statistics() {
    let db = PlannerDatabase::new("analyze");
    let ns = support::namespace("analyze");
    let store = PostgresStorage::connect(&db.url).unwrap();
    store.acquire(&ns, "w").unwrap();
    let mut state = State::default();
    for sequence in 1..=4_000u64 {
        let run = format!("run-{}", sequence % 4);
        state.receipts.push(live_receipt(sequence));
        let offset = state.receipts.len() - 1;
        state.receipts[offset].run = run.clone();
        state
            .indexes
            .receipts
            .entry(run)
            .or_default()
            .insert(sequence, offset);
        state.indexes.all_receipts.insert(sequence, offset);
    }
    store.compare_and_swap(&ns, "w", 0, &state).unwrap();
    store.release(&ns, "w").unwrap();
    drop(store);
    db.forget_statistics();
    assert!(
        db.reltuples("runtime_index") >= 4_000.0,
        "as a restore leaves it"
    );
    assert!(!db.has_statistics("runtime_index"));
    // Without statistics the planner reads every receipt row per lookup.
    assert!(
        !db.receipt_delete_plan(&ns)
            .contains("runtime_correlated_sequence")
    );
    let store = PostgresStorage::connect(&db.url).unwrap();
    assert!(db.has_statistics("runtime_index"));
    assert!(db.reltuples("run_history") >= 0.0, "empty tables too");
    let plan = db.receipt_delete_plan(&ns);
    assert!(plan.contains("runtime_correlated_sequence"), "{plan}");
    // Tables that have statistics are not analyzed again.
    let analyzed = |c: &mut postgres::Client| -> i64 {
        c.query_one(
            "SELECT coalesce(sum(analyze_count),0)::bigint FROM pg_stat_user_tables WHERE schemaname='aq_playbooks'",
            &[],
        )
        .unwrap()
        .get(0)
    };
    let mut c = db.client();
    let before = analyzed(&mut c);
    drop(PostgresStorage::connect(&db.url).unwrap());
    std::thread::sleep(std::time::Duration::from_millis(600));
    let mut c = db.client();
    assert_eq!(analyzed(&mut c), before);
    drop(store);
    drop(c);
    support::client()
        .batch_execute(&format!("DROP DATABASE {} WITH (FORCE)", db.name))
        .unwrap();
}
/// A role that may write the engine's tables but not analyze them (it does
/// not own them) still opens: the `ANALYZE` is skipped with a warning and
/// never fails the connection.
#[test]
fn a_role_that_cannot_analyze_still_opens() {
    let db = PlannerDatabase::new("noanalyze");
    drop(PostgresStorage::connect(&db.url).unwrap());
    let role = format!("t10_writer_{}", std::process::id());
    let mut c = db.client();
    c.batch_execute(&format!(
        "DROP ROLE IF EXISTS {role};
         CREATE ROLE {role} LOGIN PASSWORD 'synthetic-writer';
         GRANT CONNECT, CREATE ON DATABASE {} TO {role};
         GRANT USAGE, CREATE ON SCHEMA aq_playbooks TO {role};
         GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA aq_playbooks TO {role};",
        db.name
    ))
    .unwrap();
    // Never analyzed, as a database the owner has not opened yet.
    db.client()
        .batch_execute(
            "UPDATE pg_class SET reltuples=-1 WHERE relnamespace='aq_playbooks'::regnamespace AND relkind='r'",
        )
        .unwrap();
    let writer = format!("{} user={role} password=synthetic-writer", db.url);
    let store = PostgresStorage::connect(&writer).unwrap();
    assert!(db.reltuples("namespaces") < 0.0, "skipped, not analyzed");
    drop(store);
    drop(c);
    let mut admin = support::client();
    admin
        .batch_execute(&format!("DROP DATABASE {} WITH (FORCE)", db.name))
        .unwrap();
    admin.batch_execute(&format!("DROP ROLE {role}")).unwrap();
}
