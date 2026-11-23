use super::*;

pub(crate) struct Head {
    pub revision: i64,
    pub metadata_revision: i64,
    pub supported: bool,
    pub definition_count: i64,
    pub activation_count: i64,
}

pub(crate) struct Sync<'a> {
    pub namespace: &'a str,
    pub previous: &'a State,
    pub next: &'a State,
    pub revision: i64,
    pub previous_metadata_revision: Option<i64>,
    pub force_reencrypt: bool,
    pub keyring: Option<&'a ProtectionKeyring>,
}
pub(crate) fn sync(
    tx: &mut ObservedTransaction<'_>,
    input: Sync<'_>,
) -> Result<Head, StorageError> {
    input.apply(tx)
}

impl Sync<'_> {
    fn apply(&self, tx: &mut ObservedTransaction<'_>) -> Result<Head, StorageError> {
        let revision = self.revision;
        let namespace = self.namespace;
        let previous_rows = self
            .previous_metadata_revision
            .and_then(|_| catalog_inspection::project(self.previous).ok());
        let next_rows = catalog_inspection::project(self.next).ok();
        if self.previous_metadata_revision.is_none() {
            self.require_empty(tx)?;
        }
        let metadata_revision = match self.previous_metadata_revision {
            Some(existing) if previous_rows == next_rows => existing,
            _ => revision,
        };
        let Some(next_rows) = next_rows else {
            tx.execute(
                "DELETE FROM aq_playbooks.catalog_rows WHERE namespace=$1",
                &[&namespace],
            )
            .map_err(backend)?;
            return Ok(Head {
                revision,
                metadata_revision,
                supported: false,
                definition_count: 0,
                activation_count: 0,
            });
        };
        self.write_changes(tx, previous_rows.as_ref(), &next_rows)?;
        Ok(Head {
            revision,
            metadata_revision,
            supported: true,
            definition_count: next_rows
                .keys()
                .filter(|(kind, _)| *kind == catalog_inspection::CatalogKind::Definitions)
                .count()
                .try_into()
                .map_err(backend)?,
            activation_count: next_rows
                .keys()
                .filter(|(kind, _)| *kind == catalog_inspection::CatalogKind::Activations)
                .count()
                .try_into()
                .map_err(backend)?,
        })
    }

    fn write_changes(
        &self,
        tx: &mut ObservedTransaction<'_>,
        previous_rows: Option<&catalog_inspection::CatalogProjection>,
        next_rows: &catalog_inspection::CatalogProjection,
    ) -> Result<(), StorageError> {
        if let Some(previous) = previous_rows {
            self.remove_absent(tx, previous, next_rows)?;
        }
        for (identity, item) in next_rows {
            if self.force_reencrypt
                || previous_rows.and_then(|rows| rows.get(identity)) != Some(item)
            {
                self.write_row(tx, identity, item)?;
            }
        }
        Ok(())
    }

    fn require_empty(&self, tx: &mut ObservedTransaction<'_>) -> Result<(), StorageError> {
        let namespace = self.namespace;
        if tx
            .query_opt(
                "SELECT 1 FROM aq_playbooks.catalog_rows WHERE namespace=$1 LIMIT 1",
                &[&namespace],
            )
            .map_err(backend)?
            .is_some()
        {
            return Err(backend("unmaterialized catalog unexpectedly contains rows"));
        }
        Ok(())
    }

    fn remove_absent(
        &self,
        tx: &mut ObservedTransaction<'_>,
        previous: &catalog_inspection::CatalogProjection,
        next_rows: &catalog_inspection::CatalogProjection,
    ) -> Result<(), StorageError> {
        let namespace = self.namespace;
        for ((kind, key), _) in previous
            .iter()
            .filter(|(key, _)| !next_rows.contains_key(*key))
        {
            tx.execute("DELETE FROM aq_playbooks.catalog_rows WHERE namespace=$1 AND kind=$2 AND item_key=$3", &[&namespace,&kind.key(),&key]).map_err(backend)?;
        }
        Ok(())
    }

    fn write_row(
        &self,
        tx: &mut ObservedTransaction<'_>,
        (kind, key): &(catalog_inspection::CatalogKind, String),
        item: &CatalogItem,
    ) -> Result<(), StorageError> {
        let namespace = self.namespace;
        let revision = self.revision;
        let bytes = Zeroizing::new(serde_json::to_vec(item).map_err(backend)?);
        let (plain, key_id, nonce, encrypted) = if let Some(keys) = self.keyring {
            let sealed = keys.seal_catalog(
                protection::CatalogContext {
                    namespace,
                    kind: kind.key(),
                    key,
                    revision: revision as u64,
                },
                &bytes,
            )?;
            (
                None,
                Some(sealed.key_id),
                Some(sealed.nonce),
                Some(sealed.ciphertext),
            )
        } else {
            (Some(bytes.to_vec()), None, None, None)
        };
        tx.execute("INSERT INTO aq_playbooks.catalog_rows(namespace,kind,item_key,row_revision,plain_bytes,protection_key_id,protection_nonce,encrypted_bytes) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(namespace,kind,item_key) DO UPDATE SET row_revision=EXCLUDED.row_revision,plain_bytes=EXCLUDED.plain_bytes,protection_key_id=EXCLUDED.protection_key_id,protection_nonce=EXCLUDED.protection_nonce,encrypted_bytes=EXCLUDED.encrypted_bytes", &[&namespace,&kind.key(),&key,&revision,&plain,&key_id,&nonce,&encrypted]).map_err(backend)?;

        Ok(())
    }
}
