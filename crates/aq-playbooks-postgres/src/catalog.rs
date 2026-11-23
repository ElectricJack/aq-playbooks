//! Current metadata is derived inside the same State CAS, never from a local
//! current-catalog cache. Reads require this session's actual live owner lease.
use super::*;
use aq_playbooks_runner::{
    CatalogError, CatalogPage, CatalogQuery, CatalogStamp,
    catalog_inspection::{self, CatalogItem},
};

mod publish;
mod read;
pub(crate) use publish::{Head, Sync, sync};

impl PostgresStorage {
    pub(crate) fn current_catalog(
        &self,
        namespace: &str,
        owner: &str,
        query: &CatalogQuery,
    ) -> Result<CatalogPage, CatalogError> {
        query.validate()?;
        check_name(namespace)?;
        check_name(owner)?;
        let mut conn = self.connection()?;
        let incarnation = conn
            .leases
            .get(namespace)
            .filter(|lease| lease.owner == owner && !lease.catalog_uncertain)
            .ok_or(CatalogError::Ownership)?
            .incarnation;
        let mut tx = ObservedTransaction::new(
            conn.client.transaction().map_err(backend)?,
            self.profiling.clone(),
            "catalog_read",
        );
        let row = read::locked_head(&mut tx, namespace, owner, incarnation)?;
        let (revision, metadata_revision) = read::revisions(&row)?;
        let stamp = CatalogStamp {
            namespace: namespace.into(),
            kind: query.kind,
            incarnation: incarnation as u64,
            metadata_revision: metadata_revision as u64,
        };
        if query
            .expected
            .as_ref()
            .is_some_and(|expected| expected != &stamp)
        {
            return Err(CatalogError::Conflict);
        }
        tx.phase("catalog_page");
        let rows = read::rows(&mut tx, namespace, query)?;
        let decoder = read::Decoder {
            namespace,
            revision,
            kind: query.kind,
            protected: row.get::<_, Option<String>>("protection_key_id").is_some(),
            keyring: self.keyring.as_ref(),
        };
        let items = rows
            .into_iter()
            .map(|row| decoder.decode(row))
            .collect::<Result<Vec<_>, _>>()?;
        let page = catalog_inspection::make_page(query, stamp, revision as u64, items, false)?;
        tx.commit().map_err(backend)?;
        Ok(page)
    }
}

pub(crate) fn validate_counts(row: &postgres::Row) -> Result<(), StorageError> {
    for (expected, actual) in [
        (
            "catalog_definition_count",
            "catalog_actual_definition_count",
        ),
        (
            "catalog_activation_count",
            "catalog_actual_activation_count",
        ),
    ] {
        let expected: i64 = row.get(expected);
        let actual: i64 = row.get(actual);
        if expected < 0 || expected != actual {
            return Err(backend("catalog projection row count mismatch"));
        }
    }
    Ok(())
}
