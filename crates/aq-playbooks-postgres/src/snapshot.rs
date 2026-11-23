//! Full-state compare-and-swap, with the same publication fence for snapshots and records.
use super::*;
mod artifacts;
mod payload;

pub(super) struct Commit<'a> {
    pub(super) storage: &'a PostgresStorage,
    pub(super) namespace: &'a str,
    pub(super) owner: &'a str,
    pub(super) state: &'a State,
    pub(super) force_artifact_refresh: bool,
}

struct Locked {
    row: postgres::Row,
    format: i32,
}
struct Revision {
    expected: i64,
    next: i64,
}

struct Previous {
    state: State,
    catalog_metadata: Option<i64>,
    record_format: bool,
    refresh_artifacts: bool,
    indexes_materialized: bool,
    next_revision: i64,
}

impl Commit<'_> {
    pub(super) fn write(&self, expected_revision: u64) -> Result<u64, StorageError> {
        let namespace = self.namespace;
        let owner = self.owner;
        let expected =
            i64::try_from(expected_revision).map_err(|_| backend("revision exhausted"))?;
        let next = expected
            .checked_add(1)
            .ok_or_else(|| backend("revision exhausted"))?;
        let mut conn = self.storage.connection()?;
        let lease = conn
            .leases
            .get(namespace)
            .filter(|l| l.owner == owner)
            .ok_or(StorageError::Ownership)?;
        let incarnation = lease.incarnation;
        let format = lease.format;
        let mut tx = ObservedTransaction::new(
            conn.client.transaction().map_err(backend)?,
            self.storage.profiling.clone(),
            "cas",
        );
        let row = self.lock_row(&mut tx, incarnation, format)?;
        let previous = self.previous(&mut tx, row, Revision { expected, next })?;
        let changes = retention::Changes {
            previous: &previous.state,
            next: self.state,
        };
        tx.phase("artifact_locks");
        aq_playbooks_runner::retention::verify_removals(changes.previous, changes.next)?;
        changes.lock_artifacts(&mut tx)?;
        tx.phase("retention");
        let drain_limit = self.storage.tombstone_drain_limit(namespace, self.state);
        let rows = changes.verify_and_record(&mut tx, namespace, drain_limit)?;
        self.archive_history(&mut tx, &previous.state)?;
        self.persist_artifacts(&mut tx, &previous)?;
        tx.phase("artifact_disposal");
        retention::dispose_artifacts(&mut tx, namespace, &previous.state, self.state)?;
        let catalog = self.project(&mut tx, &previous)?;
        self.publish(&mut tx, &previous, &catalog)?;
        self.clean_plaintext(&mut tx)?;
        if let Err(error) = tx.commit() {
            conn.note_commit(namespace, true);
            return Err(backend(error));
        }
        #[cfg(test)]
        if self
            .storage
            .lose_catalog_commit_reply
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            conn.note_commit(namespace, true);
            return Err(backend("synthetic reply loss after actual catalog commit"));
        }
        conn.note_commit(namespace, false);
        self.storage
            .tombstones_committed(namespace, drain_limit, rows);
        Ok(next as u64)
    }
    fn lock_row(
        &self,
        tx: &mut ObservedTransaction<'_>,
        incarnation: i64,
        format: i32,
    ) -> Result<Locked, StorageError> {
        let namespace = self.namespace;
        tx.query_one(WRITE_MARKER_FORMAT_SQL, &[&namespace, &format.to_string()])
            .map_err(backend)?;
        if !records::owns_lock(tx, namespace)? {
            return Err(StorageError::Ownership);
        }
        let row = tx.query_opt("SELECT revision,owner,incarnation,owner_backend_pid=pg_backend_pid(),snapshot,protection_key_id,protection_nonce,encrypted_snapshot,indexes_materialized,catalog_projected_revision,catalog_metadata_revision,catalog_supported,catalog_definition_count,catalog_activation_count,catalog_actual_definition_count,catalog_actual_activation_count,writer_format,state_header FROM aq_playbooks.namespaces WHERE namespace=$1 FOR UPDATE", &[&namespace]).map_err(backend)?.ok_or(StorageError::Ownership)?;
        records::owns_row(&row, self.owner, incarnation)?;
        Ok(Locked { row, format })
    }
    fn previous(
        &self,
        tx: &mut ObservedTransaction<'_>,
        locked: Locked,
        revision: Revision,
    ) -> Result<Previous, StorageError> {
        let namespace = self.namespace;
        let Locked { row, format } = locked;
        let Revision {
            expected,
            next: next_revision,
        } = revision;
        if row.get::<_, i64>(0) != expected {
            return Err(StorageError::Conflict);
        }
        if row.get::<_, i32>("writer_format") != format {
            return Err(backend("namespace writer format changed under the lease"));
        }
        let record_format = format == records::FORMAT;
        let catalog_projected: Option<i64> = row.get("catalog_projected_revision");
        let catalog_metadata: Option<i64> = row.get("catalog_metadata_revision");
        if catalog_projected.is_some_and(|projected| projected != expected)
            || catalog_projected.is_some() != catalog_metadata.is_some()
        {
            return Err(backend("catalog projection revision mismatch"));
        }
        catalog::validate_counts(&row)?;
        let refresh_artifacts = self.force_artifact_refresh
            || self.storage.keyring.as_ref().is_some_and(|keys| {
                row.get::<_, Option<String>>("protection_key_id").as_deref()
                    != Some(keys.active_key_id())
            });
        let previous = if record_format {
            tx.phase("artifact_read");
            let artifacts = self.storage.read_artifacts(tx, namespace, false)?;
            tx.phase("records_read");
            records::assemble(tx, namespace, row.get("state_header"), artifacts)?
        } else {
            tx.phase("snapshot_decode");
            let mut previous = self.storage.decode(namespace, expected as u64, &row)?;
            tx.phase("artifact_read");
            previous.artifacts = self.storage.read_artifacts(
                tx,
                namespace,
                row.get::<_, Option<String>>("protection_key_id").is_some(),
            )?;
            previous
        };
        Ok(Previous {
            state: previous,
            catalog_metadata,
            record_format,
            refresh_artifacts,
            indexes_materialized: row.get("indexes_materialized"),
            next_revision,
        })
    }

    fn archive_history(
        &self,
        tx: &mut ObservedTransaction<'_>,
        previous: &State,
    ) -> Result<(), StorageError> {
        let state = self.state;
        let namespace = self.namespace;
        let items = aq_playbooks_runner::history_items(&state.settled);
        aq_playbooks_runner::history::check_not_live(state, &items)?;
        history::check_unrecorded(
            tx,
            namespace,
            &aq_playbooks_runner::history::new_live_keys(previous, state),
        )?;
        history::insert(tx, namespace, &items, &state.retention.policy)?;
        Ok(())
    }

    fn project(
        &self,
        tx: &mut ObservedTransaction<'_>,
        previous: &Previous,
    ) -> Result<catalog::Head, StorageError> {
        let namespace = self.namespace;
        let state = self.state;
        let empty = State::default();
        if !previous.record_format {
            tx.phase("indexes");
            indexes::sync(
                tx,
                namespace,
                if previous.indexes_materialized {
                    &previous.state
                } else {
                    &empty
                },
                state,
            )?;
        }
        tx.phase("catalog_projection");
        let catalog = catalog::sync(
            tx,
            catalog::Sync {
                namespace,
                previous: &previous.state,
                next: state,
                revision: previous.next_revision,
                previous_metadata_revision: previous.catalog_metadata,
                force_reencrypt: previous.refresh_artifacts,
                keyring: self.storage.keyring.as_ref(),
            },
        )?;
        #[cfg(test)]
        if self
            .storage
            .fail_catalog_before_snapshot
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(backend(
                "synthetic rollback after catalog projection writes",
            ));
        }
        Ok(catalog)
    }
    fn publish(
        &self,
        tx: &mut ObservedTransaction<'_>,
        previous: &Previous,
        catalog: &catalog::Head,
    ) -> Result<(), StorageError> {
        let namespace = self.namespace;
        let state = self.state;
        let next = previous.next_revision;
        let published = if previous.record_format {
            tx.phase("records_write");
            let export = records::export(state)?;
            records::replace_all(tx, namespace, &export)?;
            tx.execute("UPDATE aq_playbooks.namespaces SET state_header=$2,state_accounting=$3,revision=$4,catalog_projected_revision=$5,catalog_metadata_revision=$6,catalog_supported=$7,catalog_definition_count=$8,catalog_activation_count=$9,updated_at=clock_timestamp() WHERE namespace=$1 AND catalog_actual_definition_count=$8 AND catalog_actual_activation_count=$9",&[&namespace,&export.header,&records::accounting_json(&export.accounting),&next,&catalog.revision,&catalog.metadata_revision,&catalog.supported,&catalog.definition_count,&catalog.activation_count]).map_err(backend)?
        } else {
            tx.phase("snapshot_encode");
            let snapshot = self.storage.encode(namespace, next as u64, state)?;
            tx.phase("snapshot_write");
            tx.execute("UPDATE aq_playbooks.namespaces SET snapshot=$2,protection_key_id=$3,protection_nonce=$4,encrypted_snapshot=$5,revision=$6,writer_format=2,indexes_materialized=true,catalog_projected_revision=$7,catalog_metadata_revision=$8,catalog_supported=$9,catalog_definition_count=$10,catalog_activation_count=$11,updated_at=clock_timestamp() WHERE namespace=$1 AND catalog_actual_definition_count=$10 AND catalog_actual_activation_count=$11",&[&namespace,&snapshot.plaintext,&snapshot.key_id,&snapshot.nonce,&snapshot.ciphertext,&next,&catalog.revision,&catalog.metadata_revision,&catalog.supported,&catalog.definition_count,&catalog.activation_count]).map_err(backend)?
        };
        if published != 1 {
            return Err(backend(
                "catalog projection row count mismatch before State publication",
            ));
        }
        Ok(())
    }
}

impl Connection {
    fn note_commit(&mut self, namespace: &str, uncertain: bool) {
        if let Some(lease) = self.leases.get_mut(namespace) {
            lease.catalog_uncertain = uncertain;
        }
    }
}
