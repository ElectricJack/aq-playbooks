use super::*;
use aq_playbooks_runner::history::ItemKey;

pub(super) struct Head {
    pub(super) expected: i64,
    pub(super) accounting: Accounting,
    pub(super) header: Vec<u8>,
    pub(super) catalog_metadata: Option<i64>,
}
impl Head {
    pub(super) fn read(
        tx: &mut ObservedTransaction<'_>,
        namespace: &str,
        (owner, incarnation): (&str, i64),
    ) -> Result<Option<Self>, Error> {
        let row = tx.query_opt("SELECT revision,owner,incarnation,owner_backend_pid=pg_backend_pid(),writer_format,state_header,state_accounting,catalog_projected_revision,catalog_metadata_revision,catalog_definition_count,catalog_activation_count,catalog_actual_definition_count,catalog_actual_activation_count FROM aq_playbooks.namespaces WHERE namespace=$1 FOR UPDATE", &[&namespace]).map_err(backend)?.ok_or(StorageError::Ownership)?;
        owns_row(&row, owner, incarnation)?;
        if row.get::<_, i32>("writer_format") != FORMAT {
            return Ok(None);
        }
        let expected: i64 = row.get(0);
        let catalog_projected: Option<i64> = row.get("catalog_projected_revision");
        let catalog_metadata: Option<i64> = row.get("catalog_metadata_revision");
        if catalog_projected.is_some_and(|p| p != expected)
            || catalog_projected.is_some() != catalog_metadata.is_some()
        {
            return Err(backend("catalog projection revision mismatch").into());
        }
        crate::catalog::validate_counts(&row)?;
        let header: Vec<u8> = row.get("state_header");
        let accounting = accounting_from(&row.get::<_, Value>("state_accounting"))?;
        Ok(Some(Self {
            expected,
            accounting,
            header,
            catalog_metadata,
        }))
    }
}

pub(super) struct Planned {
    pub(super) patch: StatePatch,
    pub(super) stripped: Vec<u8>,
    pub(super) evicted: BTreeSet<(Window, String)>,
}
impl Planned {
    pub(super) fn run(
        request: &mut TransitionRequest<'_>,
        state: &mut State,
        source: &PgSource,
        header: &[u8],
    ) -> Result<Option<Self>, Error> {
        let changed = (request.plan)(state)?;
        source.check()?;
        if !changed {
            return Ok(None);
        }
        // Bound lookups, while the transaction is still lent.
        aq_playbooks_runner::history::check_not_live(
            state,
            &aq_playbooks_runner::history_items(&state.settled),
        )?;
        source.check()?;
        let patch = take_patch(state)?;
        let evicted = licensed_evictions(source, header, &patch, state)?;
        let stripped = stripped_header(state)?;
        source.check()?;
        Ok(Some(Self {
            patch,
            stripped,
            evicted,
        }))
    }
}

fn changed_header(state: &State, previous: &State) -> Result<bool, StorageError> {
    Ok(!same(&state.bindings, &previous.bindings)?
        || state.execution_denied != previous.execution_denied
        || !same(&state.retention.policy, &previous.retention.policy)?
        || !same(
            &state.retention.artifact_disposals,
            &previous.retention.artifact_disposals,
        )?)
}

impl Staged {
    pub(super) fn rebuild(
        &self,
        tx: &mut ObservedTransaction<'_>,
        namespace: &str,
        patch: StatePatch,
    ) -> Result<State, Error> {
        let mut records = all_records(tx, namespace)?;
        apply(&mut records, patch)?;
        let next = assemble_state(&self.full_header, |f| {
            Ok(records
                .remove(f)
                .map(|c| c.into_iter().collect())
                .unwrap_or_default())
        })?;
        Ok(next)
    }
}

pub(super) struct Incremental<'a> {
    pub(super) namespace: &'a str,
    pub(super) state: &'a State,
    pub(super) previous: &'a State,
    pub(super) staged: &'a Staged,
    pub(super) expected: i64,
    pub(super) stripped: &'a [u8],
    pub(super) live_keys: &'a [ItemKey],
}
pub(super) struct Published {
    pub(super) next: i64,
    pub(super) drain_limit: Option<usize>,
    pub(super) rows: crate::retention::TombstoneRows,
    pub(super) written: u64,
}
impl Incremental<'_> {
    pub(super) fn publish(
        &self,
        storage: &PostgresStorage,
        tx: &mut ObservedTransaction<'_>,
    ) -> Result<Published, Error> {
        let Self {
            namespace,
            state,
            previous,
            staged,
            expected,
            stripped,
            live_keys,
        } = *self;
        verify_header(previous, state)?;
        let next = expected
            .checked_add(1)
            .ok_or_else(|| backend("revision exhausted"))?;
        if !staged.created.is_empty() {
            let (kinds, ids): (Vec<_>, Vec<_>) = staged.created.iter().cloned().unzip();
            if let Some(reused) = tx.query_opt("SELECT kind FROM aq_playbooks.disposal_tombstones WHERE namespace=$1 AND (kind,identity) IN (SELECT * FROM unnest($2::text[],$3::text[])) LIMIT 1",&[&namespace,&kinds,&ids]).map_err(backend)? {
                return Err(backend(format!("disposed {} identity cannot be reused", reused.get::<_, String>(0))).into());
            }
        }
        if state.retention.last_gc_at_ms != previous.retention.last_gc_at_ms {
            crate::retention::collect(tx, namespace, state)?;
        }
        // The drain of legacy tombstone rows, after the identity check
        // above; a bounded commit writes no legacy row.
        let drain_limit = storage.tombstone_drain_limit(namespace, state);
        let rows = crate::retention::TombstoneRows {
            deleted: match drain_limit {
                Some(limit) => Some(crate::retention::drain_tombstones(tx, namespace, limit)?),
                None => None,
            },
        };
        let retired: Vec<&String> = staged.retired_event_tombstones.iter().collect();
        crate::retention::forget_event_tombstones(tx, namespace, &retired)?;
        if !staged.deleted_fields.is_empty() {
            tx.execute("DELETE FROM aq_playbooks.state_records r USING unnest($2::text[],$3::bytea[]) AS d(f,k) WHERE r.namespace=$1 AND r.field=d.f AND r.sort_key=d.k",&[&namespace,&staged.deleted_fields,&staged.deleted_keys]).map_err(backend)?;
        }
        Columns {
            fields: &staged.fields,
            keys: &staged.keys,
            values: &staged.values,
        }
        .upsert(tx, namespace)?;
        crate::history::check_unrecorded(tx, namespace, live_keys)?;
        crate::history::insert(
            tx,
            namespace,
            &aq_playbooks_runner::history_items(&state.settled),
            &state.retention.policy,
        )?;
        let published = tx.execute("UPDATE aq_playbooks.namespaces SET state_header=$2,state_accounting=$3,revision=$4,catalog_projected_revision=$4,updated_at=clock_timestamp() WHERE namespace=$1 AND revision=$5",&[&namespace,&stripped,&accounting_json(&staged.accounting),&next,&expected]).map_err(backend)?;
        if published != 1 {
            return Err(backend("record namespace publication mismatch").into());
        }
        let written = (staged.fields.len() + staged.deleted_fields.len()) as u64;
        Ok(Published {
            next,
            drain_limit,
            rows,
            written,
        })
    }
}
impl Published {
    pub(super) fn committed(self, storage: &PostgresStorage, namespace: &str) -> TransitionOutcome {
        let Self {
            next,
            drain_limit,
            rows,
            written,
        } = self;
        storage.tombstones_committed(namespace, drain_limit, rows);
        storage.count(|s| {
            s.incremental += 1;
            s.records_written += written;
        });
        TransitionOutcome::Committed {
            revision: next as u64,
        }
    }
}
pub(super) struct Prepared {
    pub(super) staged: Staged,
    previous: State,
    live_keys: Vec<ItemKey>,
    rebase: bool,
    expected: i64,
    catalog_projected: bool,
}
impl Head {
    pub(super) fn prepare(
        &self,
        tx: &mut ObservedTransaction<'_>,
        namespace: &str,
        (state, plan): (&State, &Planned),
    ) -> Result<Prepared, Error> {
        let previous = header_state(&self.header)?;
        // A commit with an admin licence (clock rebase, floor lowering)
        // takes the complete path: only `verify_removals` judges a licence.
        let rebase = aq_playbooks_runner::retention::carries_licence(state);
        let staged = staging::Request {
            patch: &plan.patch,
            accounting: &self.accounting,
            rules: staging::Retention {
                clock_ms: state.retention.clock_ms,
                policy: &state.retention.policy,
                rebase,
                evicted: &plan.evicted,
            },
        }
        .stage(tx, namespace)?;
        let live_keys = aq_playbooks_runner::history::patch_live_keys(&plan.patch)?;
        Ok(Prepared {
            staged,
            previous,
            live_keys,
            rebase,
            expected: self.expected,
            catalog_projected: self.catalog_metadata.is_some(),
        })
    }
}
impl Prepared {
    pub(super) fn needs_full(
        &self,
        fallback: bool,
        state: &State,
        artifacts: &BTreeMap<String, Artifact>,
    ) -> Result<bool, StorageError> {
        Ok(fallback
            || self.rebase
            || self.staged.replaced
            || self.staged.history_removed
            || &state.artifacts != artifacts
            || changed_header(state, &self.previous)?
            || !self.catalog_projected)
    }

    pub(super) fn incremental<'a>(
        &'a self,
        namespace: &'a str,
        state: &'a State,
        stripped: &'a [u8],
    ) -> Incremental<'a> {
        Incremental {
            namespace,
            state,
            previous: &self.previous,
            staged: &self.staged,
            expected: self.expected,
            stripped,
            live_keys: &self.live_keys,
        }
    }
}
