//! Snapshot envelope and canonical artifact decoding for the stored state.
use super::*;

/// Digest to (the exact canonical bytes verified, the artifact).
pub(crate) type VerifiedArtifacts = BTreeMap<String, (Zeroizing<Vec<u8>>, Artifact)>;

pub(crate) struct SnapshotEnvelope {
    pub(crate) plaintext: Option<Value>,
    pub(crate) key_id: Option<String>,
    pub(crate) nonce: Option<Vec<u8>>,
    pub(crate) ciphertext: Option<Vec<u8>>,
}

impl PostgresStorage {
    pub(crate) fn encode(
        &self,
        namespace: &str,
        revision: u64,
        state: &State,
    ) -> Result<SnapshotEnvelope, StorageError> {
        let value = state.to_value_without_artifacts().map_err(backend)?;
        if let Some(keyring) = &self.keyring {
            let plaintext = Zeroizing::new(serde_json::to_vec(&value).map_err(backend)?);
            let sealed = keyring.seal(namespace, revision, &plaintext)?;
            Ok(SnapshotEnvelope {
                plaintext: None,
                key_id: Some(sealed.key_id),
                nonce: Some(sealed.nonce),
                ciphertext: Some(sealed.ciphertext),
            })
        } else {
            Ok(SnapshotEnvelope {
                plaintext: Some(value),
                key_id: None,
                nonce: None,
                ciphertext: None,
            })
        }
    }
    pub(crate) fn decode(
        &self,
        namespace: &str,
        revision: u64,
        row: &postgres::Row,
    ) -> Result<State, StorageError> {
        let plaintext: Option<Value> = row.get("snapshot");
        if let Some(key_id) = row.get::<_, Option<String>>("protection_key_id") {
            if plaintext.is_some() {
                return Err(backend("invalid protected snapshot envelope"));
            }
            let keyring = self
                .keyring
                .as_ref()
                .ok_or_else(|| backend("protected namespace requires configured keys"))?;
            let nonce: Vec<u8> = row.get("protection_nonce");
            let ciphertext: Vec<u8> = row.get("encrypted_snapshot");
            let bytes = keyring.open(
                namespace,
                revision,
                &protection::SealedValue {
                    key_id,
                    nonce,
                    ciphertext,
                },
            )?;
            serde_json::from_slice(&bytes)
                .map_err(|_| backend("invalid authenticated snapshot format"))
        } else {
            serde_json::from_value(plaintext.ok_or_else(|| backend("missing snapshot payload"))?)
                .map_err(backend)
        }
    }
    pub(crate) fn read_artifacts(
        &self,
        tx: &mut ObservedTransaction<'_>,
        namespace: &str,
        protected: bool,
    ) -> Result<BTreeMap<String, Artifact>, StorageError> {
        let mut artifacts = BTreeMap::new();
        let mut cache = self
            .verified_artifacts
            .lock()
            .map_err(|_| backend("artifact cache mutex poisoned"))?;
        // Only the artifacts of this read stay cached: removed ones leave.
        let mut known = cache.remove(namespace).unwrap_or_default();
        let mut current = VerifiedArtifacts::new();
        let rows = tx.query("SELECT n.digest,a.canonical_bytes,e.key_id,e.nonce,e.ciphertext FROM aq_playbooks.namespace_artifacts n LEFT JOIN aq_playbooks.artifacts a ON a.digest=n.digest LEFT JOIN aq_playbooks.encrypted_artifacts e ON e.namespace=n.namespace AND e.digest=n.digest WHERE n.namespace=$1 ORDER BY n.digest",&[&namespace]).map_err(backend)?;
        for artifact_row in rows {
            let digest: String = artifact_row.get(0);
            let bytes = if protected {
                let sealed = protection::SealedValue {
                    key_id: artifact_row
                        .get::<_, Option<String>>(2)
                        .ok_or_else(|| backend("missing protected artifact envelope"))?,
                    nonce: artifact_row.get(3),
                    ciphertext: artifact_row.get(4),
                };
                self.keyring
                    .as_ref()
                    .ok_or_else(|| backend("protected namespace requires configured keys"))?
                    .open_artifact(namespace, &digest, &sealed)?
            } else {
                Zeroizing::new(
                    artifact_row
                        .get::<_, Option<Vec<u8>>>(1)
                        .ok_or_else(|| backend("missing plaintext artifact payload"))?,
                )
            };
            let artifact = match known.remove(&digest) {
                // The same bytes this handle already parsed and verified.
                Some((verified, artifact)) if *verified == *bytes => artifact,
                _ => {
                    let started = tx.clock();
                    let artifact = Artifact::from_canonical_bytes(&bytes).map_err(backend)?;
                    tx.verified(started);
                    artifact
                }
            };
            if artifact.digest != digest {
                return Err(backend("artifact address integrity mismatch"));
            }
            artifacts.insert(digest.clone(), artifact.clone());
            current.insert(digest, (bytes, artifact));
        }
        cache.insert(namespace.to_owned(), current);
        Ok(artifacts)
    }
}
