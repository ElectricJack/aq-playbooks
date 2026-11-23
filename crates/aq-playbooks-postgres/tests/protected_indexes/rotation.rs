use super::*;

#[test]
fn rotation_reencrypts_snapshot_and_artifacts_and_old_backups_require_old_keys() {
    let n = namespace("rotation");
    let state = secret_state(&n);
    {
        let s = protected("old", false);
        s.acquire(&n, "first").unwrap();
        s.compare_and_swap(&n, "first", 0, &state).unwrap();
    }
    let mut db = client();
    let backup=db.query_one("SELECT revision,protection_key_id,protection_nonce,encrypted_snapshot FROM aq_playbooks.namespaces WHERE namespace=$1",&[&n]).unwrap();
    let artifact_backup=db.query_one("SELECT digest,key_id,nonce,ciphertext FROM aq_playbooks.encrypted_artifacts WHERE namespace=$1",&[&n]).unwrap();
    {
        let rotated = protected("new", true);
        rotated.acquire(&n, "rotation").unwrap();
        assert_eq!(rotated.reencrypt_namespace(&n, "rotation").unwrap(), 2);
    }
    assert_eq!(
        db.query_one(
            "SELECT protection_key_id FROM aq_playbooks.namespaces WHERE namespace=$1",
            &[&n]
        )
        .unwrap()
        .get::<_, String>(0),
        "new"
    );
    assert_eq!(
        db.query_one(
            "SELECT key_id FROM aq_playbooks.encrypted_artifacts WHERE namespace=$1",
            &[&n]
        )
        .unwrap()
        .get::<_, String>(0),
        "new"
    );
    assert_same(&protected("new", false).load(&n).unwrap().state, &state);
    assert!(protected("old", false).load(&n).is_err());
    let revision: i64 = backup.get(0);
    let key: String = backup.get(1);
    let nonce: Vec<u8> = backup.get(2);
    let ciphertext: Vec<u8> = backup.get(3);
    {
        let mut tx = marked(&mut db, &n);
        tx.execute("UPDATE aq_playbooks.namespaces SET revision=$2,catalog_projected_revision=$2,protection_key_id=$3,protection_nonce=$4,encrypted_snapshot=$5 WHERE namespace=$1",&[&n,&revision,&key,&nonce,&ciphertext]).unwrap();
        tx.commit().unwrap();
    }
    let digest: String = artifact_backup.get(0);
    let key: String = artifact_backup.get(1);
    let nonce: Vec<u8> = artifact_backup.get(2);
    let ciphertext: Vec<u8> = artifact_backup.get(3);
    db.execute("UPDATE aq_playbooks.encrypted_artifacts SET key_id=$3,nonce=$4,ciphertext=$5 WHERE namespace=$1 AND digest=$2",&[&n,&digest,&key,&nonce,&ciphertext]).unwrap();
    assert_same(&protected("old", false).load(&n).unwrap().state, &state);
    assert!(protected("new", false).load(&n).is_err());
}

#[test]
fn enabling_protection_removes_plaintext_only_after_every_referencing_namespace_is_protected() {
    let n = namespace("upgrade-one");
    let m = namespace("upgrade-two");
    let artifact = artifact(&n);
    let state = State {
        artifacts: BTreeMap::from([(artifact.digest.clone(), artifact.clone())]),
        ..State::default()
    };
    for namespace in [&n, &m] {
        let plain = store();
        plain.acquire(namespace, "plain").unwrap();
        plain
            .compare_and_swap(namespace, "plain", 0, &state)
            .unwrap();
    }
    {
        let encrypted = protected("old", false);
        encrypted.acquire(&n, "upgrade").unwrap();
        encrypted.reencrypt_namespace(&n, "upgrade").unwrap();
    }
    assert_eq!(
        client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.artifacts WHERE digest=$1",
                &[&artifact.digest]
            )
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    assert_same(&store().load(&m).unwrap().state, &state);
    {
        let encrypted = protected("old", false);
        encrypted.acquire(&m, "upgrade").unwrap();
        encrypted.reencrypt_namespace(&m, "upgrade").unwrap();
    }
    assert_eq!(
        client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.artifacts WHERE digest=$1",
                &[&artifact.digest]
            )
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    assert_same(&protected("old", false).load(&n).unwrap().state, &state);
    assert_same(&protected("old", false).load(&m).unwrap().state, &state);
}

#[test]
fn ordinary_commit_with_new_active_key_refreshes_unchanged_artifacts() {
    let n = namespace("ordinary-key-change");
    let expected = secret_state(&n);
    let digest = expected.artifacts.keys().next().unwrap();
    {
        let original = protected("old", false);
        original.acquire(&n, "old-owner").unwrap();
        original
            .compare_and_swap(&n, "old-owner", 0, &expected)
            .unwrap();
    }
    let current = protected("new", true);
    current.acquire(&n, "new-owner").unwrap();
    current.set_profiling(true);
    current
        .compare_and_swap(&n, "new-owner", 1, &expected)
        .unwrap();
    assert_eq!(
        current.profiling_snapshot().unwrap().operations["cas"].unchanged_artifacts_skipped,
        0
    );
    let key: String = client()
        .query_one(
            "SELECT key_id FROM aq_playbooks.encrypted_artifacts WHERE namespace=$1 AND digest=$2",
            &[&n, digest],
        )
        .unwrap()
        .get(0);
    assert_eq!(key, "new");
    drop(current);
    assert_same(&protected("new", false).load(&n).unwrap().state, &expected);
    assert!(protected("old", false).load(&n).is_err());
}
