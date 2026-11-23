#[path = "fixtures/interrupted_storage.rs"]
mod interrupted_storage;
use aq_playbooks_model::*;
use aq_playbooks_runner::*;
use interrupted_storage::{CommitOutcome, InterruptedStorage};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
pub fn authority() -> Authority {
    Authority {
        actor: "owner".into(),
        scope: "project".into(),
        capabilities: BTreeSet::from(["execute".into()]),
    }
}
pub fn artifact() -> Artifact {
    Artifact::seal(ArtifactContent {
        format_version: 1,
        compiler: "AQ12 synthetic pure workflow fixture".into(),
        source: SourceBinding {
            digest: hash_bytes(b"synthetic"),
            header: SourceHeader {
                format_version: 1,
                id: "test".into(),
                scope: "project".into(),
                interfaces: BTreeSet::from(["main".into()]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: "test".into(),
            purpose: "pure embedding acceptance".into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Null,
                    outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
                    start: "done".into(),
                },
            )]),
            steps: BTreeMap::from([(
                "done".into(),
                Step {
                    op: Operation::Return {
                        outcome: "ok".into(),
                        output: Expr::literal(Value::Null),
                    },
                    edges: BTreeMap::new(),
                    on_error: None,
                },
            )]),
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: BTreeSet::from(["execute".into()]),
        },
        registry: Registry::default(),
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })
    .unwrap()
}
fn setup<S: Storage>(store: S) -> (Engine<S>, KeyedAdmissionRequest) {
    let e = Engine::new(store, "keyed", "owner").unwrap();
    let a = artifact();
    let digest = a.digest.clone();
    e.install(a).unwrap();
    e.activate("test", &digest, authority()).unwrap();
    let revision = e.snapshot().unwrap().bindings["test"].revision;
    (
        e,
        KeyedAdmissionRequest {
            key: "request-one".into(),
            binding: "test".into(),
            expected_activation_revision: revision,
            entry: "main".into(),
            input: Value::Null,
            authority: authority(),
        },
    )
}
#[test]
fn replay_is_read_only_and_changed_payload_authority_and_revision_conflict() {
    let (e, r) = setup(MemoryStorage::retaining_settled());
    let first = e.admit_once(r.clone(), 10).unwrap();
    assert!(!first.duplicate);
    let before = e.store.load("keyed").unwrap().revision;
    let second = e.admit_once(r.clone(), 99).unwrap();
    assert!(second.duplicate);
    assert_eq!(first.receipt, second.receipt);
    assert_eq!(before, e.store.load("keyed").unwrap().revision);
    let mut changed = r.clone();
    changed.input = Value::Bool(true);
    assert!(matches!(e.admit_once(changed, 11), Err(Error::Conflict(_))));
    let mut changed = r.clone();
    changed.authority.capabilities.insert("more".into());
    assert!(matches!(e.admit_once(changed, 11), Err(Error::Conflict(_))));
    let mut changed = r.clone();
    changed.expected_activation_revision += 1;
    assert!(matches!(e.admit_once(changed, 11), Err(Error::Conflict(_))));
    let a = artifact();
    e.activate("test", &a.digest, authority()).unwrap();
    assert_eq!(e.admit_once(r.clone(), 12).unwrap().receipt, first.receipt);
    let mut fresh = r;
    fresh.key = "new-key".into();
    assert!(matches!(e.admit_once(fresh, 12), Err(Error::Conflict(_))));
    assert_eq!(e.snapshot().unwrap().runs.len(), 1);
}
#[test]
fn first_admission_fences_future_revision_and_revoked_authority_but_history_remains() {
    let (e, r) = setup(MemoryStorage::retaining_settled());
    let mut future = r.clone();
    future.expected_activation_revision += 1;
    assert!(matches!(e.admit_once(future, 1), Err(Error::Conflict(_))));
    assert!(e.snapshot().unwrap().runs.is_empty());
    let first = e.admit_once(r.clone(), 2).unwrap();
    e.set_execution_authority("owner", "project", BTreeSet::from(["execute".into()]), 3)
        .unwrap();
    let replay = e.admit_once(r.clone(), 4).unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.receipt, first.receipt);
    let mut fresh = r;
    fresh.key = "new-key".into();
    assert!(matches!(e.admit_once(fresh, 4), Err(Error::Refused(_))));
    assert_eq!(e.snapshot().unwrap().runs.len(), 1);
}
/// Task 8b (no limits): a host's deprecated limits refuse nothing; a new
/// key is admitted and an exact replay is still a read-only lookup.
#[test]
fn deprecated_limits_refuse_no_admission_and_replay_stays_read_only() {
    let (mut e, r) = setup(MemoryStorage::retaining_settled());
    let first = e.admit_once(r.clone(), 1).unwrap();
    let state = e.snapshot().unwrap();
    e.config.max_receipts = state.receipts.len() + state.keyed_admissions.len();
    e.config.max_state_bytes = 1024;
    let mut new = r.clone();
    new.key = "second".into();
    assert!(!e.admit_once(new, 2).unwrap().duplicate);
    assert_eq!(e.snapshot().unwrap().runs.len(), 2);
    assert_eq!(e.snapshot().unwrap().keyed_admissions.len(), 2);
    let before = e.store.load("keyed").unwrap().revision;
    assert_eq!(e.admit_once(r, 3).unwrap().receipt, first.receipt);
    assert_eq!(before, e.store.load("keyed").unwrap().revision);
}
#[test]
fn before_cas_refusal_is_atomic_and_after_commit_unknown_replays_after_restart() {
    let store = InterruptedStorage::default();
    let (e, r) = setup(store.clone());
    store.next_commit(CommitOutcome::Conflict);
    assert!(matches!(
        e.admit_once(r.clone(), 1),
        Err(Error::Storage(StorageError::Conflict))
    ));
    let snapshot = e.snapshot().unwrap();
    assert!(snapshot.runs.is_empty());
    assert!(snapshot.keyed_admissions.is_empty());
    store.next_commit(CommitOutcome::LostResponse);
    assert!(matches!(
        e.admit_once(r.clone(), 2),
        Err(Error::Storage(StorageError::Backend(_)))
    ));
    let retained = e.admission_receipt(&r.key).unwrap().unwrap();
    assert_eq!(e.snapshot().unwrap().runs.len(), 1);
    e.release().unwrap();
    let restarted = Engine::new(store, "keyed", "new-owner").unwrap();
    let replay = restarted.admit_once(r, 3).unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.receipt, retained);
    assert_eq!(restarted.snapshot().unwrap().runs.len(), 1);
}
#[test]
fn gc_and_direct_storage_cas_cannot_forget_or_rebind_a_key() {
    let (e, r) = setup(MemoryStorage::retaining_settled());
    let first = e.admit_once(r.clone(), 1).unwrap();
    e.advance(&first.receipt.run_id, 2).unwrap();
    e.set_retention_policy(RetentionPolicy {
        terminal_run_ms: 1,
        audit_ms: 1,
        event_replay_ms: 1,
        max_tombstones: 100,
        ..RetentionPolicy::default()
    })
    .unwrap();
    // The finished run left the State as history in the commit that
    // finished it; the receipt stays (its window has not passed).
    assert!(e.snapshot().unwrap().runs.is_empty());
    assert_eq!(
        e.store
            .settled_run("keyed", &first.receipt.run_id)
            .unwrap()
            .status,
        RunStatus::Completed
    );
    assert_eq!(
        e.admission_receipt(&r.key).unwrap(),
        Some(first.receipt.clone())
    );
    for _ in 0..10 {
        e.collect_garbage(100, 1000).unwrap();
    }
    assert!(e.snapshot().unwrap().runs.is_empty());
    assert_eq!(e.admit_once(r, 101).unwrap().receipt, first.receipt);
    assert!(e.snapshot().unwrap().runs.is_empty());
    let versioned = e.store.load("keyed").unwrap();
    let mut changed = versioned.state.clone();
    changed.keyed_admissions.clear();
    assert!(
        e.store
            .compare_and_swap("keyed", "owner", versioned.revision, &changed)
            .is_err()
    );
    let mut changed = versioned.state;
    changed.keyed_admissions.values_mut().next().unwrap().run_id = "foreign".into();
    assert!(
        e.store
            .compare_and_swap("keyed", "owner", versioned.revision, &changed)
            .is_err()
    );
}
#[test]
fn bounds_and_activation_ceiling_refuse_before_retention() {
    let (e, r) = setup(MemoryStorage::retaining_settled());
    let mut bad = r.clone();
    bad.key = "x".repeat(257);
    assert!(e.admit_once(bad, 1).is_err());
    let mut bad = r.clone();
    bad.input = Value::String("x".repeat(1_048_576));
    assert!(e.admit_once(bad, 1).is_err());
    let mut bad = r.clone();
    bad.authority.actor = "other".into();
    assert!(e.admit_once(bad, 1).is_err());
    let mut bad = r;
    bad.authority.capabilities.insert("widen".into());
    assert!(e.admit_once(bad, 1).is_err());
    assert!(e.snapshot().unwrap().runs.is_empty());
    assert!(e.snapshot().unwrap().keyed_admissions.is_empty());
}

#[test]
fn simultaneous_same_key_cas_has_one_commit_and_exact_recovery() {
    let backend = InterruptedStorage::new(MemoryStorage::retaining_settled());
    let (e, r) = setup(backend.clone());
    let e = Arc::new(e);
    backend.synchronize_reads(2);
    let mut threads = vec![];
    for _ in 0..2 {
        let e = e.clone();
        let r = r.clone();
        threads.push(std::thread::spawn(move || e.admit_once(r, 1)));
    }
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .any(|r| matches!(r, Err(Error::Storage(StorageError::Conflict))))
    );
    let replay = e.admit_once(r, 2).unwrap();
    assert!(replay.duplicate);
    assert_eq!(e.snapshot().unwrap().runs.len(), 1);
}
#[test]
fn legacy_snapshot_without_receipts_decodes_without_inventing_admissions() {
    let mut value = serde_json::to_value(State::default()).unwrap();
    value.as_object_mut().unwrap().remove("keyed_admissions");
    let decoded: State = serde_json::from_value(value).unwrap();
    assert!(decoded.keyed_admissions.is_empty());
}
