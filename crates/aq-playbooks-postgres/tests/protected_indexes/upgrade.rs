use super::*;

#[test]
fn version_one_schema_upgrades_without_losing_state_and_owner_materializes_indexes() {
    let database_name = namespace("v1_database");
    let mut admin = client();
    admin
        .batch_execute(&format!("CREATE DATABASE {database_name}"))
        .unwrap();
    let mut config: postgres::Config = database().parse().unwrap();
    config.dbname(&database_name);
    let n = namespace("legacy");
    let expected = boundary(&n);
    seed_legacy_database(&config, &n, &expected);
    {
        let storage =
            PostgresStorage::from_client(config.connect(postgres::NoTls).unwrap()).unwrap();
        assert_same(&storage.load(&n).unwrap().state, &expected);
        assert!(
            storage
                .inspect(
                    &n,
                    &InspectionQuery {
                        kind: InspectionKind::Runs,
                        run: None,
                        after: None,
                        limit: 10
                    }
                )
                .is_err()
        );
        let engine = Engine::new(storage, &n, "upgrade-owner").unwrap();
        assert_eq!(
            engine
                .store
                .query(
                    &n,
                    &IndexQuery::Runs {
                        after: None,
                        limit: 10
                    }
                )
                .unwrap()
                .items
                .len(),
            2
        );
        assert_eq!(
            engine
                .store
                .inspect(
                    &n,
                    &InspectionQuery {
                        kind: InspectionKind::Runs,
                        run: None,
                        after: None,
                        limit: 10
                    }
                )
                .unwrap()
                .items
                .len(),
            2
        );
        engine.release().unwrap();
    }
    admin
        .batch_execute(&format!("DROP DATABASE {database_name} WITH (FORCE)"))
        .unwrap();
}

fn seed_legacy_database(config: &postgres::Config, n: &str, expected: &State) {
    let artifact = expected.artifacts.values().next().unwrap();
    let mut old = config.connect(postgres::NoTls).unwrap();
    old.batch_execute("CREATE SCHEMA aq_playbooks; CREATE TABLE aq_playbooks.schema_migrations(version integer PRIMARY KEY,checksum bytea NOT NULL,applied_at timestamptz NOT NULL DEFAULT clock_timestamp());").unwrap();
    let sql = include_str!("../../migrations/0001_initial.sql");
    old.batch_execute(sql).unwrap();
    old.execute(
        "INSERT INTO aq_playbooks.schema_migrations(version,checksum) VALUES(1,$1)",
        &[&Sha256::digest(sql.as_bytes()).to_vec()],
    )
    .unwrap();
    let mut snapshot = serde_json::to_value(expected).unwrap();
    snapshot["artifacts"] = json!({});
    snapshot.as_object_mut().unwrap().remove("keyed_admissions");
    old.execute(
        "INSERT INTO aq_playbooks.namespaces(namespace,revision,snapshot) VALUES($1,1,$2)",
        &[&n, &snapshot],
    )
    .unwrap();
    old.execute(
        "INSERT INTO aq_playbooks.artifacts(digest,canonical_bytes) VALUES($1,$2)",
        &[
            &artifact.digest,
            &aq_playbooks_model::canonical_bytes(artifact).unwrap(),
        ],
    )
    .unwrap();
    old.execute(
        "INSERT INTO aq_playbooks.namespace_artifacts(namespace,digest) VALUES($1,$2)",
        &[&n, &artifact.digest],
    )
    .unwrap();
}
