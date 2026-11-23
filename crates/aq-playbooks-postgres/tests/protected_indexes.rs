#![cfg(feature = "postgres-tests")]
mod support;
use aq_playbooks_model::{
    Artifact, ContractRef, EventContract, EventEnvelope, Expr, Operation, Schema, Step, digest,
};
use aq_playbooks_postgres::{PostgresStorage, ProtectionKeyring};
use aq_playbooks_runner::{indexes::TimerIndex, retention::Disposal, *};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Barrier},
};
use support::*;

fn keyring(active: &str, both: bool) -> ProtectionKeyring {
    let mut keys = BTreeMap::new();
    if both || active == "old" {
        keys.insert("old".into(), [1; 32]);
    }
    if both || active == "new" {
        keys.insert("new".into(), [2; 32]);
    }
    ProtectionKeyring::new(active, keys).unwrap()
}

fn protected(active: &str, both: bool) -> PostgresStorage {
    PostgresStorage::connect_protected(&database(), keyring(active, both)).unwrap()
}

fn secret_state(n: &str) -> State {
    let mut state = boundary(n);
    let mut content = state.artifacts.values().next().unwrap().content.clone();
    content.playbook.purpose = "SYNTHETIC_PROTECTED_SENTINEL".into();
    let artifact = Artifact::seal(content).unwrap();
    state.artifacts.clear();
    state
        .artifacts
        .insert(artifact.digest.clone(), artifact.clone());
    for run in state.runs.values_mut() {
        run.artifact = artifact.digest.clone();
        run.input = json!({"secret":"SYNTHETIC_PROTECTED_SENTINEL"});
        run.output = Some(json!("SYNTHETIC_PROTECTED_SENTINEL"));
        run.reason = Some("SYNTHETIC_PROTECTED_SENTINEL".into());
    }
    let intent = state.intents.get_mut("intent").unwrap();
    intent.input = json!("SYNTHETIC_PROTECTED_SENTINEL");
    intent.result = Some(("ok".into(), json!("SYNTHETIC_PROTECTED_SENTINEL")));
    intent.reason = Some("SYNTHETIC_PROTECTED_SENTINEL".into());
    state.receipts[0].kind = "unclassified".into();
    state.receipts[0].detail = json!({"secret":"SYNTHETIC_PROTECTED_SENTINEL"});
    state
}

fn contains_secret(bytes: &[u8]) -> bool {
    bytes
        .windows(b"SYNTHETIC_PROTECTED_SENTINEL".len())
        .any(|window| window == b"SYNTHETIC_PROTECTED_SENTINEL")
}

#[test]
fn protected_snapshots_artifacts_and_safe_projection_have_no_plaintext_values() {
    let n = namespace("protected");
    let expected = secret_state(&n);
    let s = protected("old", false);
    assert!(s.protection_available());
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &expected).unwrap();
    let artifact = expected.artifacts.keys().next().unwrap();
    let mut db = client();
    let row=db.query_one("SELECT snapshot,encrypted_snapshot,protection_key_id FROM aq_playbooks.namespaces WHERE namespace=$1",&[&n]).unwrap();
    assert!(row.get::<_, Option<Value>>(0).is_none());
    assert!(!contains_secret(&row.get::<_, Vec<u8>>(1)));
    assert_eq!(row.get::<_, String>(2), "old");
    assert_eq!(
        db.query_one(
            "SELECT count(*) FROM aq_playbooks.artifacts WHERE digest=$1",
            &[artifact]
        )
        .unwrap()
        .get::<_, i64>(0),
        0
    );
    assert!(!contains_secret(&db.query_one("SELECT ciphertext FROM aq_playbooks.encrypted_artifacts WHERE namespace=$1 AND digest=$2",&[&n,artifact]).unwrap().get::<_,Vec<u8>>(0)));
    let projections:String=db.query_one("SELECT COALESCE(jsonb_agg(public_value)::text,'') FROM aq_playbooks.inspection_rows WHERE namespace=$1",&[&n]).unwrap().get(0);
    assert!(!contains_secret(projections.as_bytes()));
    assert_same(&s.load(&n).unwrap().state, &expected);
    drop(s);
    let unconfigured = store();
    assert!(unconfigured.load(&n).is_err());
    assert!(unconfigured.acquire(&n, "missing-key").is_err());
    let wrong = PostgresStorage::connect_protected(
        &database(),
        ProtectionKeyring::new("old", BTreeMap::from([("old".into(), [9; 32])])).unwrap(),
    )
    .unwrap();
    assert!(wrong.load(&n).is_err());
    assert!(wrong.acquire(&n, "wrong-key").is_err());
    // Public inspection deliberately exposes only the precomputed safe projection.
    let page = unconfigured
        .inspect(
            &n,
            &InspectionQuery {
                kind: InspectionKind::Runs,
                run: None,
                after: None,
                limit: 1,
            },
        )
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(!contains_secret(&serde_json::to_vec(&page).unwrap()));
    let reopened = protected("old", false);
    reopened.acquire(&n, "correct-key").unwrap();
    assert_same(&reopened.load(&n).unwrap().state, &expected);
}

#[test]
fn authenticated_payload_tampering_never_falls_back_to_plaintext() {
    let n = namespace("tamper");
    let state = secret_state(&n);
    let s = protected("old", false);
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &state).unwrap();
    let mut db = client();
    let original: Vec<u8> = db
        .query_one(
            "SELECT encrypted_snapshot FROM aq_playbooks.namespaces WHERE namespace=$1",
            &[&n],
        )
        .unwrap()
        .get(0);
    let mut changed = original.clone();
    changed[0] ^= 1;
    {
        let mut tx = marked(&mut db, &n);
        tx.execute(
            "UPDATE aq_playbooks.namespaces SET encrypted_snapshot=$2 WHERE namespace=$1",
            &[&n, &changed],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    assert!(s.load(&n).is_err());
    assert!(s.compare_and_swap(&n, "owner", 1, &state).is_err());
    {
        let mut tx = marked(&mut db, &n);
        tx.execute(
            "UPDATE aq_playbooks.namespaces SET encrypted_snapshot=$2 WHERE namespace=$1",
            &[&n, &original],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let a = state.artifacts.keys().next().unwrap();
    let original:Vec<u8>=db.query_one("SELECT ciphertext FROM aq_playbooks.encrypted_artifacts WHERE namespace=$1 AND digest=$2",&[&n,a]).unwrap().get(0);
    let mut changed = original.clone();
    changed[0] ^= 1;
    db.execute("UPDATE aq_playbooks.encrypted_artifacts SET ciphertext=$3 WHERE namespace=$1 AND digest=$2",&[&n,a,&changed]).unwrap();
    // Another namespace's legitimate plaintext copy cannot bypass this namespace's AEAD.
    let other = namespace("plaintext-copy");
    let plain = store();
    plain.acquire(&other, "plain").unwrap();
    plain.compare_and_swap(&other, "plain", 0, &state).unwrap();
    assert!(s.load(&n).is_err());
    db.execute("UPDATE aq_playbooks.encrypted_artifacts SET ciphertext=$3 WHERE namespace=$1 AND digest=$2",&[&n,a,&original]).unwrap();
    assert_same(&s.load(&n).unwrap().state, &state);
}

#[path = "protected_indexes/catalog.rs"]
mod catalog;
#[path = "protected_indexes/dispatch.rs"]
mod dispatch;
#[path = "protected_indexes/events.rs"]
mod events;
#[path = "protected_indexes/gc.rs"]
mod gc;
#[path = "protected_indexes/native.rs"]
mod native;
#[path = "protected_indexes/reuse.rs"]
mod reuse;
#[path = "protected_indexes/rotation.rs"]
mod rotation;
#[path = "protected_indexes/upgrade.rs"]
mod upgrade;
