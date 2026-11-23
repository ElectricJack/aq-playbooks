use super::*;

pub(super) fn locked_head(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    owner: &str,
    incarnation: i64,
) -> Result<postgres::Row, CatalogError> {
    let row = tx.query_opt("SELECT revision,owner,incarnation,owner_backend_pid=pg_backend_pid() AS same_backend,writer_format,catalog_projected_revision,catalog_metadata_revision,catalog_supported,protection_key_id,catalog_definition_count,catalog_activation_count,catalog_actual_definition_count,catalog_actual_activation_count FROM aq_playbooks.namespaces WHERE namespace=$1 FOR SHARE", &[&namespace]).map_err(backend)?.ok_or(CatalogError::Ownership)?;
    let lock = crate::records::owns_lock(tx, namespace)?;
    let expected = (Some(owner), incarnation, Some(true));
    let stored_owner: Option<String> = row.get("owner");
    let stored = (
        stored_owner.as_deref(),
        row.get::<_, i64>("incarnation"),
        row.get::<_, Option<bool>>("same_backend"),
    );
    if !lock || stored != expected {
        return Err(CatalogError::Ownership);
    }
    Ok(row)
}

pub(super) fn revisions(row: &postgres::Row) -> Result<(i64, i64), CatalogError> {
    if !matches!(
        row.get::<_, i32>("writer_format"),
        2 | crate::records::FORMAT
    ) {
        return Err(CatalogError::Unsupported);
    }
    let revision: i64 = row.get("revision");
    let projected: Option<i64> = row.get("catalog_projected_revision");
    let Some(projected) = projected else {
        return Err(CatalogError::Unsupported);
    };
    if projected != revision {
        return Err(CatalogError::Backend(
            "catalog projection revision mismatch".into(),
        ));
    }
    validate_counts(row)?;
    if !row.get::<_, bool>("catalog_supported") {
        return Err(CatalogError::Unsupported);
    }
    let metadata_revision: i64 = row
        .get::<_, Option<i64>>("catalog_metadata_revision")
        .filter(|value| *value > 0 && *value <= revision)
        .ok_or_else(|| CatalogError::Backend("catalog metadata revision invalid".into()))?;
    Ok((revision, metadata_revision))
}

pub(super) fn rows(
    tx: &mut ObservedTransaction<'_>,
    namespace: &str,
    query: &CatalogQuery,
) -> Result<Vec<postgres::Row>, CatalogError> {
    let limit = (query.limit + 1) as i64;
    let rows = if let Some(key) = &query.exact {
        tx.query("SELECT item_key,row_revision,plain_bytes,protection_key_id,protection_nonce,encrypted_bytes FROM aq_playbooks.catalog_rows WHERE namespace=$1 AND kind=$2 AND item_key=$3", &[&namespace,&query.kind.key(),&key]).map_err(backend)?
    } else if let Some(after) = &query.after {
        tx.query("SELECT item_key,row_revision,plain_bytes,protection_key_id,protection_nonce,encrypted_bytes FROM aq_playbooks.catalog_rows WHERE namespace=$1 AND kind=$2 AND item_key>$3 ORDER BY item_key LIMIT $4", &[&namespace,&query.kind.key(),&after,&limit]).map_err(backend)?
    } else {
        tx.query("SELECT item_key,row_revision,plain_bytes,protection_key_id,protection_nonce,encrypted_bytes FROM aq_playbooks.catalog_rows WHERE namespace=$1 AND kind=$2 ORDER BY item_key LIMIT $3", &[&namespace,&query.kind.key(),&limit]).map_err(backend)?
    };
    Ok(rows)
}

pub(super) struct Decoder<'a> {
    pub namespace: &'a str,
    pub revision: i64,
    pub kind: catalog_inspection::CatalogKind,
    pub protected: bool,
    pub keyring: Option<&'a ProtectionKeyring>,
}

impl Decoder<'_> {
    pub fn decode(&self, row: postgres::Row) -> Result<CatalogItem, CatalogError> {
        let key: String = row.get("item_key");
        let at: i64 = row.get("row_revision");
        if at <= 0 || at > self.revision {
            return Err(CatalogError::Backend("catalog row revision invalid".into()));
        }
        let plain: Option<Vec<u8>> = row.get("plain_bytes");
        let key_id: Option<String> = row.get("protection_key_id");
        let nonce: Option<Vec<u8>> = row.get("protection_nonce");
        let encrypted: Option<Vec<u8>> = row.get("encrypted_bytes");
        let bytes = match (self.protected, plain, key_id, nonce, encrypted) {
            (false, Some(bytes), None, None, None) => Zeroizing::new(bytes),
            (true, None, Some(key_id), Some(nonce), Some(bytes)) => self
                .keyring
                .ok_or(CatalogError::Unsupported)?
                .open_catalog(
                    protection::CatalogContext {
                        namespace: self.namespace,
                        kind: self.kind.key(),
                        key: &key,
                        revision: at as u64,
                    },
                    &key_id,
                    &nonce,
                    &bytes,
                )?,
            _ => {
                return Err(CatalogError::Backend(
                    "catalog protection envelope mismatch".into(),
                ));
            }
        };
        if bytes.len() > catalog_inspection::MAX_CATALOG_ROW_BYTES {
            return Err(CatalogError::Unsupported);
        }
        let item: CatalogItem = serde_json::from_slice(&bytes)
            .map_err(|_| CatalogError::Backend("invalid catalog row encoding".into()))?;
        item.validate()?;
        if item.key != key || item.kind() != self.kind {
            return Err(CatalogError::Backend(
                "catalog row identity mismatch".into(),
            ));
        }
        Ok(item)
    }
}
