use super::*;
impl retention::Changes<'_> {
    pub(super) fn lock_artifacts(
        &self,
        tx: &mut ObservedTransaction<'_>,
    ) -> Result<(), StorageError> {
        let previous = self.previous;
        let state = self.next;
        let addresses: std::collections::BTreeSet<_> = previous
            .artifacts
            .keys()
            .chain(state.artifacts.keys())
            .cloned()
            .collect();
        let new_addresses: Vec<_> = state
            .artifacts
            .keys()
            .filter(|id| !previous.artifacts.contains_key(*id))
            .cloned()
            .collect();
        if !new_addresses.is_empty() {
            tx.execute("INSERT INTO aq_playbooks.artifact_catalog(digest) SELECT digest FROM unnest($1::text[]) AS pending(digest) ORDER BY digest ON CONFLICT DO NOTHING",&[&new_addresses]).map_err(backend)?;
        }
        if !addresses.is_empty() {
            // All previous references retain their catalog FK. New addresses are
            // created above. Keep union locks, in the same global order as GC.
            let addresses: Vec<_> = addresses.into_iter().collect();
            let locked=tx.query("SELECT digest FROM aq_playbooks.artifact_catalog WHERE digest=ANY($1) ORDER BY digest FOR UPDATE",&[&addresses]).map_err(backend)?;
            if locked.len() != addresses.len() {
                return Err(backend("artifact catalog reference disappeared"));
            }
        }
        Ok(())
    }
}

impl Commit<'_> {
    pub(super) fn persist_artifacts(
        &self,
        tx: &mut ObservedTransaction<'_>,
        previous: &Previous,
    ) -> Result<(), StorageError> {
        let namespace = self.namespace;
        let state = self.state;
        for (digest, artifact) in &state.artifacts {
            tx.phase("artifact_compare");
            // Previous bytes were freshly read, authenticated and fully verified
            // under this namespace's row/revision/owner locks. Equality checks the
            // entire typed artifact, never just its supplied address. No cache.
            if !previous.refresh_artifacts && previous.state.artifacts.get(digest) == Some(artifact)
            {
                tx.skipped();
                continue;
            }
            tx.phase("artifact_validation");
            let started = tx.clock();
            artifact.verify().map_err(backend)?;
            tx.verified(started);
            if &artifact.digest != digest {
                return Err(backend("artifact key does not match digest"));
            }
            let started = tx.clock();
            let bytes = canonical_bytes(artifact).map_err(backend)?;
            tx.canonicalized(started);
            tx.phase("artifact_write");
            self.write_bytes(tx, digest, &bytes)?;
            tx.execute("INSERT INTO aq_playbooks.namespace_artifacts(namespace,digest) VALUES($1,$2) ON CONFLICT DO NOTHING", &[&namespace,&digest]).map_err(backend)?;
        }
        Ok(())
    }

    fn write_bytes(
        &self,
        tx: &mut ObservedTransaction<'_>,
        digest: &str,
        bytes: &[u8],
    ) -> Result<(), StorageError> {
        let namespace = self.namespace;
        if let Some(keyring) = &self.storage.keyring {
            if let Some(existing) = tx.query_opt(
                "SELECT key_id,nonce,ciphertext FROM aq_playbooks.encrypted_artifacts WHERE namespace=$1 AND digest=$2",
                &[&namespace,&digest],
            ).map_err(backend)? {
                let key_id: String = existing.get(0);
                let nonce: Vec<u8> = existing.get(1);
                let ciphertext: Vec<u8> = existing.get(2);
                let stored = keyring.open_artifact(namespace, digest, &protection::SealedValue { key_id, nonce, ciphertext })?;
                if stored.as_slice() != bytes { return Err(backend("immutable protected artifact address conflict")); }
            }
            let sealed = keyring.seal_artifact(namespace, digest, bytes)?;
            tx.execute("INSERT INTO aq_playbooks.encrypted_artifacts(namespace,digest,key_id,nonce,ciphertext) VALUES($1,$2,$3,$4,$5) ON CONFLICT(namespace,digest) DO UPDATE SET key_id=EXCLUDED.key_id,nonce=EXCLUDED.nonce,ciphertext=EXCLUDED.ciphertext",&[&namespace,&digest,&sealed.key_id,&sealed.nonce,&sealed.ciphertext]).map_err(backend)?;
        } else {
            tx.execute("INSERT INTO aq_playbooks.artifacts(digest,canonical_bytes) VALUES($1,$2) ON CONFLICT DO NOTHING",&[&digest,&bytes]).map_err(backend)?;
            let stored: Vec<u8> = tx
                .query_one(
                    "SELECT canonical_bytes FROM aq_playbooks.artifacts WHERE digest=$1",
                    &[&digest],
                )
                .map_err(backend)?
                .get(0);
            if stored != bytes {
                return Err(backend("immutable artifact address conflict"));
            }
        }
        Ok(())
    }
    pub(super) fn clean_plaintext(
        &self,
        tx: &mut ObservedTransaction<'_>,
    ) -> Result<(), StorageError> {
        let state = self.state;
        if self.storage.keyring.is_some() {
            let addresses: Vec<_> = state.artifacts.keys().cloned().collect();
            tx.execute("DELETE FROM aq_playbooks.artifacts a WHERE digest=ANY($1) AND NOT EXISTS(SELECT 1 FROM aq_playbooks.namespace_artifacts n JOIN aq_playbooks.namespaces s ON s.namespace=n.namespace WHERE n.digest=a.digest AND (s.protection_key_id IS NULL OR NOT EXISTS(SELECT 1 FROM aq_playbooks.encrypted_artifacts e WHERE e.namespace=n.namespace AND e.digest=n.digest)))",&[&addresses]).map_err(backend)?;
        }
        Ok(())
    }
}
