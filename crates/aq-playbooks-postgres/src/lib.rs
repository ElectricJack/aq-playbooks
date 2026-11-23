//! PostgreSQL persistence for a single-owner engine namespace.
//!
//! Every runner boundary is one compare-and-swap transaction. The snapshot
//! contains receipts, waits, children, events and operation intents together.
//! Immutable canonical artifacts are retained through explicit dependency/audit
//! references and safe disposal boundaries. Session advisory locks prevent two connections
//! from owning the same namespace, even if they supply the same owner name.
//! This does not fence external hosts: retire a prior runner before replacement.
#[cfg(all(test, feature = "postgres-tests"))]
extern crate self as aq_playbooks_postgres;

mod catalog;
#[cfg(all(test, feature = "postgres-tests"))]
mod catalog_tests;
mod history;
#[cfg(all(test, feature = "postgres-tests"))]
mod history_tests;
mod indexes;
mod lease;
mod profiling;
mod projection;
mod protection;
mod records;
use profiling::ObservedTransaction;
pub use profiling::{OperationProfile, StageProfile, StorageProfile};
pub use records::TransitionStats;
mod retention;
mod schema;
mod snapshot;
use aq_playbooks_model::{Artifact, canonical_bytes};
use aq_playbooks_runner::{
    State,
    storage::{Storage, StorageError, VersionedState},
};
use postgres::{Client, NoTls};
pub use protection::ProtectionKeyring;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard},
};
use zeroize::Zeroizing;

#[cfg(all(test, feature = "postgres-tests"))]
const WRITE_MARKER_SQL: &str = "SELECT set_config('aq_playbooks.writer_namespace',$1,true),set_config('aq_playbooks.writer_format','2',true)";
use records::MARKER_SQL as WRITE_MARKER_FORMAT_SQL;
/// One format 2 inspection page over live rows and history. Each branch has
/// its own `ORDER BY item_key LIMIT`, so PostgreSQL merges two ordered index
/// scans (Merge Append) instead of sorting every matching history row: the
/// cost of a page does not grow with the number of finished runs.
pub(crate) const INSPECT_SQL: &str = "SELECT item_key,public_value FROM ((SELECT item_key,public_value FROM aq_playbooks.inspection_rows WHERE namespace=$1 AND kind=$2 AND ($3::text IS NULL OR run_id=$3) AND ($4::text IS NULL OR item_key>$4) ORDER BY item_key LIMIT $5) UNION ALL (SELECT item_key,public_value FROM aq_playbooks.run_history WHERE namespace=$1 AND kind=$2 AND ($3::text IS NULL OR run_id=$3) AND ($4::text IS NULL OR item_key>$4) ORDER BY item_key LIMIT $5)) u ORDER BY item_key LIMIT $5";
struct Lease {
    owner: String,
    incarnation: i64,
    catalog_uncertain: bool,
    /// Writer format of the namespace row (2 snapshot, 3 records).
    format: i32,
}
struct Connection {
    client: Client,
    leases: BTreeMap<String, Lease>,
    statements: Option<records::Statements>,
}

/// Holds one database session for its lifetime. Dropping it releases all locks;
/// reopening increments each acquired namespace's durable runtime incarnation.
pub struct PostgresStorage {
    connection: Mutex<Connection>,
    keyring: Option<ProtectionKeyring>,
    profiling: profiling::Sink,
    /// New unprotected namespaces use writer format 3 (records).
    records: bool,
    stats: Mutex<TransitionStats>,
    /// Namespaces whose legacy tombstone rows this handle has seen drained
    /// (a commit deleted fewer than its bound): their commits skip the
    /// delete. The rows themselves are the progress, so a new handle
    /// probes once again. A commit that writes legacy rows (legacy GC)
    /// takes its namespace out.
    tombstones_drained: Mutex<std::collections::BTreeSet<String>>,
    #[cfg(test)]
    lose_catalog_commit_reply: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    fail_catalog_before_snapshot: std::sync::atomic::AtomicBool,
}
fn backend(error: impl std::fmt::Display) -> StorageError {
    StorageError::Backend(error.to_string())
}
fn lock_key(namespace: &str) -> i64 {
    let digest = Sha256::digest(format!("aq-playbooks/namespace/{namespace}"));
    i64::from_be_bytes(digest[..8].try_into().expect("eight digest bytes"))
}
fn check_name(name: &str) -> Result<(), StorageError> {
    if !(1..=512).contains(&name.len()) || name.contains('\0') {
        return Err(backend(
            "namespace and owner must contain 1..512 bytes and no NUL",
        ));
    }
    Ok(())
}

impl PostgresStorage {
    /// Connects and transactionally applies the crate-owned schema migrations,
    /// then analyzes the engine's tables that have no planner statistics yet
    /// at connection time.
    /// Credentials are never included in adapter error messages. This convenience
    /// constructor uses NoTls; use `from_client` with a host-configured TLS client
    /// for remote production databases requiring transport encryption.
    pub fn connect(config: &str) -> Result<Self, StorageError> {
        Self::from_client(Client::connect(config, NoTls).map_err(backend)?)
    }

    pub fn connect_protected(
        config: &str,
        keyring: ProtectionKeyring,
    ) -> Result<Self, StorageError> {
        Self::from_client_protected(Client::connect(config, NoTls).map_err(backend)?, keyring)
    }
    pub fn from_client(client: Client) -> Result<Self, StorageError> {
        Self::initialize(client, None)
    }
    /// Like `connect`, but namespaces this connection creates use the bounded
    /// record representation (writer format 3) and support `transition_v1`.
    pub fn connect_records(config: &str) -> Result<Self, StorageError> {
        Self::from_client_records(Client::connect(config, NoTls).map_err(backend)?)
    }
    pub fn from_client_records(client: Client) -> Result<Self, StorageError> {
        let mut storage = Self::initialize(client, None)?;
        storage.records = true;
        Ok(storage)
    }
    pub fn from_client_protected(
        client: Client,
        keyring: ProtectionKeyring,
    ) -> Result<Self, StorageError> {
        Self::initialize(client, Some(keyring))
    }
    fn initialize(
        mut client: Client,
        keyring: Option<ProtectionKeyring>,
    ) -> Result<Self, StorageError> {
        schema::prepare(&mut client)?;
        Ok(Self {
            connection: Mutex::new(Connection {
                client,
                leases: BTreeMap::new(),
                statements: None,
            }),
            keyring,
            profiling: Default::default(),
            records: false,
            stats: Mutex::new(TransitionStats::default()),
            tombstones_drained: Mutex::new(Default::default()),
            #[cfg(test)]
            lose_catalog_commit_reply: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            fail_catalog_before_snapshot: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Disabled by default. Value-free fixed-stage aggregate diagnostics only.
    pub fn set_profiling(&self, enabled: bool) {
        if let Ok(mut p) = self.profiling.lock() {
            *p = enabled.then(StorageProfile::default);
        }
    }
    pub fn profiling_snapshot(&self) -> Option<StorageProfile> {
        self.profiling.lock().ok().and_then(|p| p.clone())
    }
    pub fn reencrypt_namespace(&self, namespace: &str, owner: &str) -> Result<u64, StorageError> {
        if self.keyring.is_none() {
            return Err(backend("key rotation requires configured protection"));
        }
        let current = self.load(namespace)?;
        snapshot::Commit {
            storage: self,
            namespace,
            owner,
            state: &current.state,
            force_artifact_refresh: true,
        }
        .write(current.revision)
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, StorageError> {
        self.connection
            .lock()
            .map_err(|_| backend("storage connection mutex poisoned"))
    }

    /// Durable incarnation is useful in diagnostics; it does not confer host
    /// authority or claim external-system fencing.
    pub fn incarnation(&self, namespace: &str) -> Result<Option<u64>, StorageError> {
        let mut conn = self.connection()?;
        conn.client
            .query_opt(
                "SELECT incarnation FROM aq_playbooks.namespaces WHERE namespace=$1",
                &[&namespace],
            )
            .map_err(backend)
            .map(|row| row.map(|r| r.get::<_, i64>(0) as u64))
    }
}

impl Storage for PostgresStorage {
    fn persists_history(&self) -> bool {
        true
    }
    fn history_page(
        &self,
        namespace: &str,
        query: &aq_playbooks_runner::InspectionQuery,
    ) -> Result<Vec<aq_playbooks_runner::HistoryItem>, StorageError> {
        check_name(namespace)?;
        query.validate()?;
        let mut conn = self.connection()?;
        let mut tx = ObservedTransaction::new(
            conn.client.transaction().map_err(backend)?,
            self.profiling.clone(),
            "history_page",
        );
        let items = history::page(&mut tx, namespace, query)?;
        tx.commit().map_err(backend)?;
        Ok(items)
    }
    /// An ordinary indexed delete by finish time, by the lease owner.
    fn delete_history(
        &self,
        namespace: &str,
        owner: &str,
        before_ms: i64,
    ) -> Result<u64, StorageError> {
        check_name(namespace)?;
        let mut conn = self.connection()?;
        let lease = conn
            .leases
            .get(namespace)
            .filter(|l| l.owner == owner)
            .ok_or(StorageError::Ownership)?;
        let incarnation = lease.incarnation;
        let format = lease.format.to_string();
        let mut tx = ObservedTransaction::new(
            conn.client.transaction().map_err(backend)?,
            self.profiling.clone(),
            "delete_history",
        );
        tx.query_one(WRITE_MARKER_FORMAT_SQL, &[&namespace, &format])
            .map_err(backend)?;
        if !records::owns_lock(&mut tx, namespace)? {
            return Err(StorageError::Ownership);
        }
        let row = tx.query_opt("SELECT revision,owner,incarnation,owner_backend_pid=pg_backend_pid() FROM aq_playbooks.namespaces WHERE namespace=$1", &[&namespace]).map_err(backend)?.ok_or(StorageError::Ownership)?;
        records::owns_row(&row, owner, incarnation)?;
        let deleted = history::delete_before(&mut tx, namespace, before_ms)?;
        tx.commit().map_err(backend)?;
        Ok(deleted)
    }
    fn inspect_catalog(
        &self,
        namespace: &str,
        owner: &str,
        query: &aq_playbooks_runner::CatalogQuery,
    ) -> Result<aq_playbooks_runner::CatalogPage, aq_playbooks_runner::CatalogError> {
        self.current_catalog(namespace, owner, query)
    }
    fn protection_available(&self) -> bool {
        self.keyring.is_some()
    }
    fn query(
        &self,
        namespace: &str,
        query: &aq_playbooks_runner::IndexQuery,
    ) -> Result<aq_playbooks_runner::IndexPage, StorageError> {
        check_name(namespace)?;
        let mut conn = self.connection()?;
        if let Some(page) = self.query_records(&mut conn, namespace, query)? {
            return Ok(page);
        }
        if conn
            .client
            .query_opt(
                "SELECT indexes_materialized FROM aq_playbooks.namespaces WHERE namespace=$1",
                &[&namespace],
            )
            .map_err(backend)?
            .is_some_and(|row| !row.get::<_, bool>(0))
        {
            return Err(backend(
                "namespace indexes require exclusive owner initialization",
            ));
        }
        indexes::query(&mut conn.client, namespace, query)
    }
    fn inspect(
        &self,
        namespace: &str,
        query: &aq_playbooks_runner::InspectionQuery,
    ) -> Result<aq_playbooks_runner::InspectionPage, StorageError> {
        check_name(namespace)?;
        query.validate()?;
        let mut conn = self.connection()?;
        let mut tx = conn.client.transaction().map_err(backend)?;
        let Some(row) = tx
            .query_opt(
                "SELECT revision,indexes_materialized,writer_format,state_header FROM aq_playbooks.namespaces WHERE namespace=$1 FOR SHARE",
                &[&namespace],
            )
            .map_err(backend)?
        else {
            return Ok(aq_playbooks_runner::InspectionPage {
                items: vec![],
                next: None,
                revision: 0,
            });
        };
        let revision = row.get::<_, i64>(0) as u64;
        if row.get::<_, i32>(2) == records::FORMAT {
            let mut tx = ObservedTransaction::new(tx, self.profiling.clone(), "inspect");
            let artifacts = self.read_artifacts(&mut tx, namespace, false)?;
            let state = records::assemble(&mut tx, namespace, row.get(3), artifacts)?;
            // Read after the State: an item that left it is found here.
            let history = history::page(&mut tx, namespace, query)?;
            tx.commit().map_err(backend)?;
            return Ok(records::inspect(&state, query, revision, history));
        }
        if !row.get::<_, bool>(1) {
            return Err(backend(
                "namespace indexes require exclusive owner initialization",
            ));
        }
        let limit = (query.limit + 1) as i64;
        // Live rows and history in one statement (one snapshot). No commit
        // leaves a key in both (`check_not_live`, `check_unrecorded`), so the
        // page needs no de-duplication.
        let rows = tx
            .query(
                INSPECT_SQL,
                &[
                    &namespace,
                    &query.kind.key(),
                    &query.run,
                    &query.after,
                    &limit,
                ],
            )
            .map_err(backend)?;
        let more = rows.len() > query.limit;
        let selected: Vec<_> = rows.into_iter().take(query.limit).collect();
        let next = if more {
            selected.last().map(|r| r.get::<_, String>(0))
        } else {
            None
        };
        let items = selected.into_iter().map(|r| r.get(1)).collect();
        tx.commit().map_err(backend)?;
        Ok(aq_playbooks_runner::InspectionPage {
            items,
            next,
            revision,
        })
    }
    fn acquire(&self, namespace: &str, owner: &str) -> Result<(), StorageError> {
        check_name(namespace)?;
        check_name(owner)?;
        let mut conn = self.connection()?;
        if let Some(lease) = conn.leases.get(namespace) {
            return if lease.owner == owner {
                Ok(())
            } else {
                Err(StorageError::Ownership)
            };
        }
        let key = lock_key(namespace);
        let locked: bool = conn
            .client
            .query_one("SELECT pg_try_advisory_lock($1)", &[&key])
            .map_err(backend)?
            .get(0);
        if !locked {
            return Err(StorageError::Ownership);
        }
        let result = self.claim_namespace(&mut conn.client, namespace, owner);
        match result {
            Ok(lease) => {
                conn.leases.insert(namespace.into(), lease);
                Ok(())
            }
            Err(error) => {
                let _ = conn
                    .client
                    .query_one("SELECT pg_advisory_unlock($1)", &[&key]);
                Err(error)
            }
        }
    }

    fn release(&self, namespace: &str, owner: &str) -> Result<(), StorageError> {
        let mut conn = self.connection()?;
        let lease = conn
            .leases
            .get(namespace)
            .filter(|l| l.owner == owner)
            .ok_or(StorageError::Ownership)?;
        let incarnation = lease.incarnation;
        let format = lease.format.to_string();
        let mut tx = conn.client.transaction().map_err(backend)?;
        tx.query_one(WRITE_MARKER_FORMAT_SQL, &[&namespace, &format])
            .map_err(backend)?;
        let changed = tx.execute("UPDATE aq_playbooks.namespaces SET owner=NULL,owner_backend_pid=NULL WHERE namespace=$1 AND owner=$2 AND incarnation=$3 AND owner_backend_pid=pg_backend_pid()", &[&namespace,&owner,&incarnation]).map_err(backend)?;
        if changed != 1 {
            return Err(StorageError::Ownership);
        }
        tx.commit().map_err(backend)?;
        let unlocked: bool = conn
            .client
            .query_one("SELECT pg_advisory_unlock($1)", &[&lock_key(namespace)])
            .map_err(backend)?
            .get(0);
        conn.leases.remove(namespace);
        if !unlocked {
            return Err(StorageError::Ownership);
        }
        Ok(())
    }

    fn load(&self, namespace: &str) -> Result<VersionedState, StorageError> {
        check_name(namespace)?;
        let mut conn = self.connection()?;
        let mut tx = ObservedTransaction::new(
            conn.client.transaction().map_err(backend)?,
            self.profiling.clone(),
            "load",
        );
        // The row lock gives the snapshot and its artifact references one view,
        // without holding an unbounded repeatable-read transaction.
        let Some(row) = tx.query_opt("SELECT revision,snapshot,protection_key_id,protection_nonce,encrypted_snapshot,writer_format,state_header FROM aq_playbooks.namespaces WHERE namespace=$1 FOR SHARE", &[&namespace]).map_err(backend)? else {
            return Ok(VersionedState::default());
        };
        self.count(|s| s.full_loads += 1);
        let revision = row.get::<_, i64>("revision") as u64;
        if row.get::<_, i32>("writer_format") == records::FORMAT {
            tx.phase("artifact_read");
            let artifacts = self.read_artifacts(&mut tx, namespace, false)?;
            tx.phase("records_read");
            let state = records::assemble(&mut tx, namespace, row.get("state_header"), artifacts)?;
            tx.commit().map_err(backend)?;
            return Ok(VersionedState { revision, state });
        }
        let protected = row.get::<_, Option<String>>("protection_key_id").is_some();
        tx.phase("snapshot_decode");
        let mut state = self.decode(namespace, revision, &row)?;
        tx.phase("artifact_read");
        state.artifacts = self.read_artifacts(&mut tx, namespace, protected)?;
        tx.commit().map_err(backend)?;
        Ok(VersionedState { revision, state })
    }

    fn compare_and_swap(
        &self,
        namespace: &str,
        owner: &str,
        expected_revision: u64,
        state: &State,
    ) -> Result<u64, StorageError> {
        self.count(|s| s.full_commits += 1);
        snapshot::Commit {
            storage: self,
            namespace,
            owner,
            state,
            force_artifact_refresh: false,
        }
        .write(expected_revision)
    }
    fn transition_v1(
        &self,
        namespace: &str,
        owner: &str,
        request: &mut aq_playbooks_runner::transition::TransitionRequest<'_>,
    ) -> aq_playbooks_runner::Result<aq_playbooks_runner::transition::TransitionOutcome> {
        self.transition_records(namespace, owner, request)
    }
    fn transition_v1_support(&self, namespace: &str) -> Result<bool, StorageError> {
        check_name(namespace)?;
        let mut conn = self.connection()?;
        Ok(conn
            .client
            .query_opt(
                "SELECT writer_format FROM aq_playbooks.namespaces WHERE namespace=$1",
                &[&namespace],
            )
            .map_err(backend)?
            .is_some_and(|row| row.get::<_, i32>(0) == records::FORMAT))
    }
}
impl PostgresStorage {
    /// The bound of this commit's delete of legacy tombstone rows
    /// (`policy.drain_entries_per_commit`), or `None` when this handle has
    /// seen them drained.
    pub(crate) fn tombstone_drain_limit(&self, namespace: &str, state: &State) -> Option<usize> {
        let drained = self
            .tombstones_drained
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(namespace);
        (!drained).then_some(state.retention.policy.drain_entries_per_commit)
    }
    /// After a commit: remember that the rows are drained when its delete
    /// found fewer than its bound (no engine writes such rows any more; a
    /// new handle looks once more).
    pub(crate) fn tombstones_committed(
        &self,
        namespace: &str,
        limit: Option<usize>,
        rows: retention::TombstoneRows,
    ) {
        let mut drained = self
            .tombstones_drained
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let (Some(limit), Some(deleted)) = (limit, rows.deleted)
            && deleted < limit as u64
        {
            drained.insert(namespace.to_string());
        }
    }
}
