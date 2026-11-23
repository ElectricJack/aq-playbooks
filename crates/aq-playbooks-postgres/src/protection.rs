//! Host-supplied encryption keys. Keys never enter the database, logs, serde
//! output or Debug implementations. Preserve retired keys with encrypted backups.
use crate::backend;
use aq_playbooks_runner::StorageError;
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, OsRng, Payload, rand_core::RngCore},
};
use std::collections::BTreeMap;
use zeroize::Zeroizing;

#[derive(Clone)]
pub struct ProtectionKeyring {
    active_key_id: String,
    keys: BTreeMap<String, Zeroizing<[u8; 32]>>,
}
pub(crate) struct CatalogContext<'a> {
    pub namespace: &'a str,
    pub kind: &'a str,
    pub key: &'a str,
    pub revision: u64,
}
#[cfg_attr(test, derive(Clone))]
pub(crate) struct SealedValue {
    pub key_id: String,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

impl ProtectionKeyring {
    pub fn new(
        active_key_id: impl Into<String>,
        keys: BTreeMap<String, [u8; 32]>,
    ) -> Result<Self, StorageError> {
        let active_key_id = active_key_id.into();
        let valid_ids = keys
            .keys()
            .all(|id| !id.is_empty() && id.len() <= 128 && !id.contains('\0'));
        if !valid_ids || !keys.contains_key(&active_key_id) {
            return Err(backend(
                "invalid protection keyring or unavailable active key",
            ));
        }
        Ok(Self {
            active_key_id,
            keys: keys
                .into_iter()
                .map(|(id, key)| (id, Zeroizing::new(key)))
                .collect(),
        })
    }
    pub fn active_key_id(&self) -> &str {
        &self.active_key_id
    }
    fn cipher(&self, key_id: &str) -> Result<XChaCha20Poly1305, StorageError> {
        let key = self
            .keys
            .get(key_id)
            .ok_or_else(|| backend("required protection key unavailable"))?;
        XChaCha20Poly1305::new_from_slice(key.as_ref())
            .map_err(|_| backend("invalid protection key"))
    }
    fn aad(namespace: &str, revision: u64, key_id: &str) -> Result<Vec<u8>, StorageError> {
        serde_json::to_vec(&(
            "aq-playbooks.snapshot.xchacha20poly1305.v1",
            namespace,
            revision,
            key_id,
        ))
        .map_err(backend)
    }
    pub(crate) fn seal(
        &self,
        namespace: &str,
        revision: u64,
        plaintext: &[u8],
    ) -> Result<SealedValue, StorageError> {
        let aad = Self::aad(namespace, revision, &self.active_key_id)?;
        self.seal_aad(&aad, plaintext)
    }
    fn seal_aad(&self, aad: &[u8], plaintext: &[u8]) -> Result<SealedValue, StorageError> {
        let mut nonce = [0u8; 24];
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| backend("protection nonce generation unavailable"))?;
        let ciphertext = self
            .cipher(&self.active_key_id)?
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| backend("protected value encryption failed"))?;
        Ok(SealedValue {
            key_id: self.active_key_id.clone(),
            nonce: nonce.to_vec(),
            ciphertext,
        })
    }
    pub(crate) fn open(
        &self,
        namespace: &str,
        revision: u64,
        sealed: &SealedValue,
    ) -> Result<Zeroizing<Vec<u8>>, StorageError> {
        let aad = Self::aad(namespace, revision, &sealed.key_id)?;
        self.open_aad(&aad, sealed)
    }
    fn open_aad(
        &self,
        aad: &[u8],
        sealed: &SealedValue,
    ) -> Result<Zeroizing<Vec<u8>>, StorageError> {
        if sealed.nonce.len() != 24 {
            return Err(backend("protected value authentication failed"));
        }
        self.cipher(&sealed.key_id)?
            .decrypt(
                XNonce::from_slice(&sealed.nonce),
                Payload {
                    msg: &sealed.ciphertext,
                    aad,
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| backend("protected value authentication failed"))
    }
    pub(crate) fn seal_artifact(
        &self,
        namespace: &str,
        digest: &str,
        plaintext: &[u8],
    ) -> Result<SealedValue, StorageError> {
        let aad = artifact_aad(namespace, digest, &self.active_key_id)?;
        self.seal_aad(&aad, plaintext)
    }
    pub(crate) fn open_artifact(
        &self,
        namespace: &str,
        digest: &str,
        sealed: &SealedValue,
    ) -> Result<Zeroizing<Vec<u8>>, StorageError> {
        let aad = artifact_aad(namespace, digest, &sealed.key_id)?;
        self.open_aad(&aad, sealed)
    }
    pub(crate) fn seal_catalog(
        &self,
        context: CatalogContext<'_>,
        plaintext: &[u8],
    ) -> Result<SealedValue, StorageError> {
        let aad = context.aad(&self.active_key_id)?;
        self.seal_aad(&aad, plaintext)
    }
    pub(crate) fn open_catalog(
        &self,
        context: CatalogContext<'_>,
        sealed: &SealedValue,
    ) -> Result<Zeroizing<Vec<u8>>, StorageError> {
        let aad = context.aad(&sealed.key_id)?;
        self.open_aad(&aad, sealed)
    }
}

fn artifact_aad(namespace: &str, digest: &str, key_id: &str) -> Result<Vec<u8>, StorageError> {
    serde_json::to_vec(&(
        "aq-playbooks.artifact.xchacha20poly1305.v1",
        namespace,
        digest,
        key_id,
    ))
    .map_err(backend)
}

impl CatalogContext<'_> {
    fn aad(&self, key_id: &str) -> Result<Vec<u8>, StorageError> {
        let CatalogContext {
            namespace,
            kind,
            key,
            revision,
        } = self;
        serde_json::to_vec(&(
            "aq-playbooks.catalog.xchacha20poly1305.v1",
            namespace,
            kind,
            key,
            revision,
            key_id,
        ))
        .map_err(backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn keys(active: &str) -> ProtectionKeyring {
        ProtectionKeyring::new(
            active,
            BTreeMap::from([("old".into(), [1; 32]), ("new".into(), [2; 32])]),
        )
        .unwrap()
    }
    #[test]
    fn persisted_v1_associated_data_is_byte_identical() {
        let catalog = CatalogContext {
            namespace: "n",
            kind: "definitions",
            key: "address",
            revision: 7,
        };
        assert_eq!(
            ProtectionKeyring::aad("n", 7, "old").unwrap(),
            br#"["aq-playbooks.snapshot.xchacha20poly1305.v1","n",7,"old"]"#
        );
        assert_eq!(
            artifact_aad("n", "address", "old").unwrap(),
            br#"["aq-playbooks.artifact.xchacha20poly1305.v1","n","address","old"]"#
        );
        assert_eq!(
            catalog.aad("old").unwrap(),
            br#"["aq-playbooks.catalog.xchacha20poly1305.v1","n","definitions","address",7,"old"]"#
        );
    }
    #[test]
    fn authenticated_context_nonce_and_key_rotation() {
        let old = keys("old");
        let first = old.seal("n", 7, b"synthetic protected payload").unwrap();
        let again = old.seal("n", 7, b"synthetic protected payload").unwrap();
        assert_ne!(first.nonce, again.nonce);
        let rotated = keys("new");
        assert_eq!(
            &*rotated.open("n", 7, &first).unwrap(),
            b"synthetic protected payload"
        );
        for (namespace, revision) in [("other", 7), ("n", 8)] {
            assert!(rotated.open(namespace, revision, &first).is_err());
        }
        let mut altered = first.clone();
        altered.ciphertext[0] ^= 1;
        assert!(old.open("n", 7, &altered).is_err());
        let wrong =
            ProtectionKeyring::new("old", BTreeMap::from([("old".into(), [3; 32])])).unwrap();
        assert!(wrong.open("n", 7, &first).is_err());
        let missing =
            ProtectionKeyring::new("new", BTreeMap::from([("new".into(), [2; 32])])).unwrap();
        assert!(missing.open("n", 7, &first).is_err());
        assert_eq!(rotated.seal("n", 8, b"next").unwrap().key_id, "new");
    }
    #[test]
    fn catalog_envelopes_bind_namespace_kind_address_revision_key_and_domain() {
        let keys = keys("old");
        let context = || CatalogContext {
            namespace: "n",
            kind: "definitions",
            key: "address",
            revision: 7,
        };
        let row = keys.seal_catalog(context(), b"metadata").unwrap();
        assert_eq!(&*keys.open_catalog(context(), &row).unwrap(), b"metadata");
        for changed in [
            CatalogContext {
                namespace: "other",
                ..context()
            },
            CatalogContext {
                kind: "activations",
                ..context()
            },
            CatalogContext {
                key: "other",
                ..context()
            },
            CatalogContext {
                revision: 8,
                ..context()
            },
        ] {
            assert!(keys.open_catalog(changed, &row).is_err());
        }
        let other_key_id = SealedValue {
            key_id: "new".into(),
            ..row.clone()
        };
        assert!(keys.open_catalog(context(), &other_key_id).is_err());
        let missing =
            ProtectionKeyring::new("new", BTreeMap::from([("new".into(), [2; 32])])).unwrap();
        assert!(missing.open_catalog(context(), &row).is_err());
        assert!(keys.open("n", 7, &row).is_err());
    }
}
