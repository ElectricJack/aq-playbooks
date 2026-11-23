//! Claim an advisory-locked namespace and initialize the appropriate storage format.
use super::*;

impl PostgresStorage {
    pub(super) fn claim_namespace(
        &self,
        client: &mut Client,
        namespace: &str,
        owner: &str,
    ) -> Result<Lease, StorageError> {
        let mut tx = client.transaction().map_err(backend)?;
        let format = self.namespace_format(&mut tx, namespace)?;
        tx.query_one(WRITE_MARKER_FORMAT_SQL, &[&namespace, &format.to_string()])
            .map_err(backend)?;
        self.ensure_namespace(&mut tx, namespace, format)?;
        let incarnation: i64 = tx.query_one("UPDATE aq_playbooks.namespaces SET owner=$2, owner_backend_pid=pg_backend_pid(), incarnation=incarnation+1, acquired_at=clock_timestamp() WHERE namespace=$1 RETURNING incarnation", &[&namespace,&owner]).map_err(backend)?.get(0);
        tx.commit().map_err(backend)?;
        Ok(Lease {
            owner: owner.into(),
            incarnation,
            format,
            catalog_uncertain: false,
        })
    }
    fn namespace_format(
        &self,
        tx: &mut postgres::Transaction<'_>,
        namespace: &str,
    ) -> Result<i32, StorageError> {
        let format: i32 = match tx
            .query_opt(
                "SELECT writer_format FROM aq_playbooks.namespaces WHERE namespace=$1",
                &[&namespace],
            )
            .map_err(backend)?
        {
            Some(row) => row.get(0),
            None if self.records && self.keyring.is_none() => records::FORMAT,
            None => 2,
        };
        Ok(format)
    }
    fn ensure_namespace(
        &self,
        tx: &mut postgres::Transaction<'_>,
        namespace: &str,
        format: i32,
    ) -> Result<(), StorageError> {
        if format == records::FORMAT {
            let (header, accounting) = records::empty_header()?;
            tx.execute("INSERT INTO aq_playbooks.namespaces(namespace,snapshot,indexes_materialized,writer_format,state_header,state_accounting) VALUES($1,'null'::jsonb,false,3,$2,$3) ON CONFLICT DO NOTHING",&[&namespace,&header,&accounting]).map_err(backend)?;
        } else {
            let empty = self.encode(namespace, 0, &State::default())?;
            tx.execute("INSERT INTO aq_playbooks.namespaces(namespace,snapshot,protection_key_id,protection_nonce,encrypted_snapshot,indexes_materialized,writer_format) VALUES($1,$2,$3,$4,$5,true,2) ON CONFLICT DO NOTHING",&[&namespace,&empty.plaintext,&empty.key_id,&empty.nonce,&empty.ciphertext]).map_err(backend)?;
            let existing=tx.query_one("SELECT revision,snapshot,protection_key_id,protection_nonce,encrypted_snapshot FROM aq_playbooks.namespaces WHERE namespace=$1",&[&namespace]).map_err(backend)?;
            self.decode(
                namespace,
                existing.get::<_, i64>("revision") as u64,
                &existing,
            )?;
        }
        Ok(())
    }
}
