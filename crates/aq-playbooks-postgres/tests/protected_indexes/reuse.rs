use super::*;

#[test]
fn unchanged_commit_reuses_only_freshly_verified_bytes_and_has_bounded_value_free_profile() {
    let n = namespace("verified-reuse");
    let expected = boundary(&n);
    let s = store();
    assert!(s.profiling_snapshot().is_none());
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &expected).unwrap();
    s.set_profiling(true);
    s.compare_and_swap(&n, "owner", 1, &expected).unwrap();
    let profile = s.profiling_snapshot().unwrap();
    let cas = &profile.operations["cas"];
    assert_eq!(cas.completed, 1);
    assert_eq!(cas.artifact_verifications, 1);
    assert_eq!(cas.artifact_canonicalizations, 0);
    assert_eq!(cas.unchanged_artifacts_skipped, 1);
    assert_eq!(cas.stages["artifact_locks"].sql_calls, 1);
    assert!(!cas.stages.contains_key("artifact_write"));
    let serialized = serde_json::to_string(&profile).unwrap();
    assert!(!serialized.contains(&n));
    assert!(cas.stages.len() <= 16);
    let digest = expected.artifacts.keys().next().unwrap();
    let mut db = client();
    let bytes: Vec<u8> = db
        .query_one(
            "SELECT canonical_bytes FROM aq_playbooks.artifacts WHERE digest=$1",
            &[digest],
        )
        .unwrap()
        .get(0);
    // This dedicated test connection bypasses the immutable-row trigger only for
    // its own corruption injection; other concurrent tests keep their triggers.
    db.batch_execute("SET session_replication_role=replica")
        .unwrap();
    db.execute(
        "UPDATE aq_playbooks.artifacts SET canonical_bytes=$2 WHERE digest=$1",
        &[digest, &b"corrupt".as_slice()],
    )
    .unwrap();
    db.batch_execute("SET session_replication_role=origin")
        .unwrap();
    assert!(s.compare_and_swap(&n, "owner", 2, &expected).is_err());
    db.batch_execute("SET session_replication_role=replica")
        .unwrap();
    db.execute(
        "UPDATE aq_playbooks.artifacts SET canonical_bytes=$2 WHERE digest=$1",
        &[digest, &bytes],
    )
    .unwrap();
    db.batch_execute("SET session_replication_role=origin")
        .unwrap();
    s.compare_and_swap(&n, "owner", 2, &expected).unwrap();
    drop(s);
    let cold = store();
    cold.acquire(&n, "new-owner").unwrap();
    cold.set_profiling(true);
    cold.compare_and_swap(&n, "new-owner", 3, &expected)
        .unwrap();
    assert_eq!(
        cold.profiling_snapshot().unwrap().operations["cas"].artifact_verifications,
        1
    );
}

#[test]
fn protected_reuse_preserves_nonce_but_explicit_same_key_refresh_and_tamper_checks_remain() {
    let n = namespace("protected-reuse");
    let expected = secret_state(&n);
    let s = protected("old", false);
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &expected).unwrap();
    let digest = expected.artifacts.keys().next().unwrap();
    let mut db = client();
    let artifact_envelope = |db: &mut postgres::Client| {
        let r=db.query_one("SELECT nonce,ciphertext FROM aq_playbooks.encrypted_artifacts WHERE namespace=$1 AND digest=$2",&[&n,digest]).unwrap();
        (r.get::<_, Vec<u8>>(0), r.get::<_, Vec<u8>>(1))
    };
    let original = artifact_envelope(&mut db);
    s.set_profiling(true);
    s.compare_and_swap(&n, "owner", 1, &expected).unwrap();
    assert_eq!(artifact_envelope(&mut db), original);
    assert_eq!(
        s.profiling_snapshot().unwrap().operations["cas"].unchanged_artifacts_skipped,
        1
    );
    s.set_profiling(true);
    s.reencrypt_namespace(&n, "owner").unwrap();
    let refreshed = artifact_envelope(&mut db);
    assert_ne!(refreshed.0, original.0);
    assert_eq!(
        s.profiling_snapshot().unwrap().operations["cas"].unchanged_artifacts_skipped,
        0
    );
    let mut corrupted = refreshed.1.clone();
    corrupted[0] ^= 1;
    db.execute("UPDATE aq_playbooks.encrypted_artifacts SET ciphertext=$3 WHERE namespace=$1 AND digest=$2",&[&n,digest,&corrupted]).unwrap();
    assert!(s.compare_and_swap(&n, "owner", 3, &expected).is_err());
    db.execute("UPDATE aq_playbooks.encrypted_artifacts SET ciphertext=$3 WHERE namespace=$1 AND digest=$2",&[&n,digest,&refreshed.1]).unwrap();
    let mut forged = expected.clone();
    forged
        .artifacts
        .values_mut()
        .next()
        .unwrap()
        .content
        .playbook
        .purpose = "changed under same address".into();
    assert!(s.compare_and_swap(&n, "owner", 3, &forged).is_err());
    assert_eq!(s.load(&n).unwrap().revision, 3);
    assert_same(&s.load(&n).unwrap().state, &expected);
    let profile = serde_json::to_vec(&s.profiling_snapshot()).unwrap();
    assert!(!contains_secret(&profile));
}

/// The owner's hot path: a warm handle parses and verifies an artifact only
/// when its stored bytes change, so a commit or load costs no verification
/// however many artifacts the namespace keeps, and changed bytes still fail.
#[test]
fn warm_handle_verifies_no_unchanged_artifact_whatever_their_number() {
    for extra in [0, 12] {
        let n = namespace("warm-artifacts");
        let mut state = boundary(&n);
        for i in 0..extra {
            let a = artifact(&format!("{n}-extra-{i}"));
            state.artifacts.insert(a.digest.clone(), a);
        }
        let count = state.artifacts.len() as u64;
        let s = protected("old", false);
        s.acquire(&n, "owner").unwrap();
        s.compare_and_swap(&n, "owner", 0, &state).unwrap();
        s.set_profiling(true);
        s.compare_and_swap(&n, "owner", 1, &state).unwrap();
        let cold = s.profiling_snapshot().unwrap().operations["cas"].artifact_verifications;
        assert_eq!(cold, count, "the first read verifies each artifact");
        s.set_profiling(true);
        for revision in 2..6 {
            assert_same(&s.load(&n).unwrap().state, &state);
            s.compare_and_swap(&n, "owner", revision, &state).unwrap();
        }
        let warm = s.profiling_snapshot().unwrap();
        for operation in ["cas", "load"] {
            assert_eq!(warm.operations[operation].completed, 4);
            assert_eq!(
                warm.operations[operation].artifact_verifications, 0,
                "{operation} with {count} artifacts"
            );
        }
        // Changed bytes under an unchanged address are read, verified and refused.
        let digest = state.artifacts.keys().next().unwrap();
        let mut db = client();
        let r=db.query_one("SELECT nonce,ciphertext FROM aq_playbooks.encrypted_artifacts WHERE namespace=$1 AND digest=$2",&[&n,digest]).unwrap();
        let (nonce, mut ciphertext): (Vec<u8>, Vec<u8>) = (r.get(0), r.get(1));
        let original = ciphertext.clone();
        ciphertext[0] ^= 1;
        db.execute("UPDATE aq_playbooks.encrypted_artifacts SET ciphertext=$3 WHERE namespace=$1 AND digest=$2",&[&n,digest,&ciphertext]).unwrap();
        assert!(s.load(&n).is_err());
        db.execute("UPDATE aq_playbooks.encrypted_artifacts SET nonce=$3,ciphertext=$4 WHERE namespace=$1 AND digest=$2",&[&n,digest,&nonce,&original]).unwrap();
        assert_same(&s.load(&n).unwrap().state, &state);
    }
}
