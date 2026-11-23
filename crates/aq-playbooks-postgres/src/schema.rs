//! Schema migration and planner statistics on adapter startup.
use super::{StorageError, backend};
use postgres::Client;
use sha2::{Digest, Sha256};

const MIGRATION_LOCK: i64 = 0x4151504c41594d47;

const MIGRATIONS: &[(i32, &str)] = &[
    // Ordering is part of the qualified schema.
    (1, include_str!("../migrations/0001_initial.sql")),
    (
        2,
        include_str!("../migrations/0002_indexes_and_protection.sql"),
    ),
    (
        3,
        include_str!("../migrations/0003_keyed_admission_writer.sql"),
    ),
    (4, include_str!("../migrations/0004_catalog_inspection.sql")),
    (5, include_str!("../migrations/0005_transition_records.sql")),
    (6, include_str!("../migrations/0006_run_history.sql")),
    (7, include_str!("../migrations/0007_run_history_trim.sql")),
];
/// The engine's tables without planner statistics: never analyzed
/// (`reltuples` negative), or holding rows but no column statistics, as a
/// fresh `pg_restore` leaves them (PostgreSQL 17 restores no statistics;
/// its index builds set `reltuples`). An analyzed empty table (`reltuples`
/// 0, no statistics to keep) is not one of them.
const UNANALYZED_TABLES_SQL: &str = "SELECT c.relname FROM pg_class c WHERE c.relnamespace='aq_playbooks'::regnamespace AND c.relkind IN ('r','p') AND (c.reltuples<0 OR (c.reltuples>0 AND NOT EXISTS(SELECT 1 FROM pg_stats s WHERE s.schemaname='aq_playbooks' AND s.tablename=c.relname))) ORDER BY c.relname";
/// Analyze the engine's tables that have no planner statistics
/// ([`UNANALYZED_TABLES_SQL`]). Without them the planner may answer an
/// indexed lookup by a prefix scan (the drain's receipt-row delete then
/// reads every receipt row of the namespace per named row). Each gets one
/// `ANALYZE` of its own, outside any commit, before the first commit. A
/// table that has statistics is not touched, so this costs one catalog
/// query after the first connection. `ANALYZE` takes SHARE UPDATE EXCLUSIVE, which blocks no
/// reader or writer, and reads a bounded sample. It is best effort: a role
/// that may not analyze a table it does not own gets a server warning and
/// the table is skipped; any error (a statement or lock timeout, say) is
/// reported on stderr, value-free, and ignored. Autovacuum analyzes such a
/// table later anyway.
fn analyze_unanalyzed(client: &mut Client) {
    let tables: Vec<String> = match client.query(UNANALYZED_TABLES_SQL, &[]) {
        Ok(rows) => rows.iter().map(|row| row.get(0)).collect(),
        Err(error) => {
            eprintln!("aq-playbooks: could not list tables without statistics: {error}");
            return;
        }
    };
    for table in tables {
        // `relname` comes from the catalog of our own schema; quoted anyway.
        let statement = format!("ANALYZE aq_playbooks.\"{}\"", table.replace('"', "\"\""));
        if let Err(error) = client.batch_execute(&statement) {
            eprintln!("aq-playbooks: ANALYZE aq_playbooks.{table} skipped: {error}");
            continue;
        }
        // A role without the right gets only a server warning: say so.
        let skipped = client
            .query(UNANALYZED_TABLES_SQL, &[])
            .map(|rows| rows.iter().any(|row| row.get::<_, String>(0) == table))
            .unwrap_or(false);
        if skipped {
            eprintln!(
                "aq-playbooks: ANALYZE aq_playbooks.{table} skipped (this role may not analyze it); run ANALYZE as its owner"
            );
        }
    }
}

pub(super) fn prepare(client: &mut Client) -> Result<(), StorageError> {
    let mut tx = client.transaction().map_err(backend)?;
    tx.query_one("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK])
        .map_err(backend)?;
    tx.batch_execute("CREATE SCHEMA IF NOT EXISTS aq_playbooks; CREATE TABLE IF NOT EXISTS aq_playbooks.schema_migrations (version integer PRIMARY KEY, checksum bytea NOT NULL, applied_at timestamptz NOT NULL DEFAULT clock_timestamp());").map_err(backend)?;
    let versions = tx
        .query(
            "SELECT version,checksum FROM aq_playbooks.schema_migrations ORDER BY version",
            &[],
        )
        .map_err(backend)?;
    verify_history(&versions)?;
    for (version, sql) in MIGRATIONS.iter().skip(versions.len()) {
        tx.batch_execute(sql).map_err(backend)?;
        let checksum = Sha256::digest(sql.as_bytes()).to_vec();
        tx.execute(
            "INSERT INTO aq_playbooks.schema_migrations(version,checksum) VALUES($1,$2)",
            &[version, &checksum],
        )
        .map_err(backend)?;
    }
    tx.commit().map_err(backend)?;
    analyze_unanalyzed(client);
    Ok(())
}

fn verify_history(versions: &[postgres::Row]) -> Result<(), StorageError> {
    for (offset, row) in versions.iter().enumerate() {
        let Some((version, sql)) = MIGRATIONS.get(offset) else {
            return Err(backend("unsupported aq-playbooks schema migration"));
        };
        if row.get::<_, i32>(0) != *version
            || row.get::<_, Vec<u8>>(1) != Sha256::digest(sql.as_bytes()).to_vec()
        {
            return Err(backend(
                "unsupported or altered aq-playbooks schema migration",
            ));
        }
    }
    Ok(())
}
