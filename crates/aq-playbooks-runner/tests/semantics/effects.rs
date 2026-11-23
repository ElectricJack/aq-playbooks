use super::*;

#[test]
fn durable_intent_replay_keeps_id_business_retry_creates_new_id() {
    let e = engine();
    install(
        &e,
        invoke_artifact(
            command(Recovery::KeyedIdempotent),
            Some(RetryPolicy {
                max_attempts: 2,
                backoff_ms: 10,
                outcomes: set(&["retry"]),
            }),
        ),
        "invoke",
    );
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    let first = e.snapshot().unwrap().intents.keys().next().unwrap().clone();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![
                DispatchResult::Unknown {
                    reason: "lost after host commit".into(),
                },
                DispatchResult::Completed {
                    outcome: "retry".into(),
                    output: Value::Null,
                },
                DispatchResult::Completed {
                    outcome: "ok".into(),
                    output: Value::Null,
                },
            ],
        )]),
        ..Default::default()
    };
    e.dispatch_next(&mut host, 0).unwrap();
    e.advance(&id, 0).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Blocked);
    e.recover(&mut host, 1).unwrap();
    e.dispatch_next(&mut host, 1).unwrap();
    e.advance(&id, 1).unwrap();
    e.advance(&id, 10).unwrap();
    assert_eq!(e.snapshot().unwrap().intents.len(), 1);
    e.drive(&mut host, 11, 30).unwrap();
    // The run and both operations have left the State as history.
    assert_eq!(settled(&e, &id).outcome.as_deref(), Some("ok"));
    assert!(e.snapshot().unwrap().intents.is_empty());
    assert_eq!(inspected(&e, InspectionKind::Effects, Some(&id)).len(), 2);
    assert_eq!((&host.dispatched[0], &host.dispatched[1]), (&first, &first));
    assert_ne!(host.dispatched[1], host.dispatched[2]);
}

#[test]
fn unsafe_ambiguity_is_never_automatically_repeated_or_cancelled() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::Unsafe), None),
        "invoke",
    );
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    e.recover(&mut host, 1).unwrap();
    e.drive(&mut host, 1, 10).unwrap();
    assert_eq!(host.dispatched.len(), 1);
    e.cancel(&id, 2).unwrap();
    e.advance(&id, 2).unwrap();
    assert_eq!(
        e.snapshot().unwrap().runs[&id].status,
        RunStatus::Cancelling
    );
}

#[test]
fn memory_store_cas_and_owner_fence_full_boundary() {
    let store = MemoryStorage::default();
    store.acquire("n", "one").unwrap();
    assert_eq!(store.acquire("n", "two"), Err(StorageError::Ownership));
    let old = store.load("n").unwrap();
    let mut proposed = old.state.clone();
    proposed.sequence = 42;
    store.compare_and_swap("n", "one", 0, &proposed).unwrap();
    assert_eq!(
        store.compare_and_swap("n", "one", 0, &State::default()),
        Err(StorageError::Conflict)
    );
    assert_eq!(store.load("n").unwrap().state.sequence, 42);
    store.release("n", "one").unwrap();
    assert_eq!(
        store.compare_and_swap("n", "one", 1, &proposed),
        Err(StorageError::Ownership)
    );
}

#[test]
fn invalid_host_output_blocks_with_a_safe_durable_receipt() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::Unsafe), None),
        "invoke",
    );
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![DispatchResult::Completed {
                outcome: "ok".into(),
                output: json!({"secret":"not-public"}),
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].status, RunStatus::Blocked);
    assert!(s.runs[&id].reason.as_ref().unwrap().contains("invalid"));
    assert_eq!(
        s.intents.values().next().unwrap().status,
        IntentStatus::Unknown
    );
    let receipts = &s.receipts;
    assert!(receipts.iter().any(|r| r.kind == "invalid_output"));
    assert!(
        !serde_json::to_string(receipts)
            .unwrap()
            .contains("not-public")
    );
}

#[test]
fn losing_namespace_ownership_prevents_reconciliation_handoff() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::Reconcilable), None),
        "invoke",
    );
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Blocked);
    e.release().unwrap();
    let _replacement = Engine::new(e.store.clone(), "test", "replacement").unwrap();
    assert!(matches!(
        e.recover(&mut host, 1),
        Err(Error::Storage(StorageError::Ownership))
    ));
    assert!(host.reconciliations.is_empty());
}

#[test]
fn command_deadline_refuses_unissued_effect_and_preserves_issued_uncertainty() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    let first = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&first, 0).unwrap();
    let prepared = e
        .snapshot()
        .unwrap()
        .intents
        .values()
        .next()
        .unwrap()
        .clone();
    assert_eq!(prepared.deadline_at_ms, 100);
    let mut host = FakeHost::default();
    e.dispatch_next(&mut host, 100).unwrap();
    assert!(host.dispatched.is_empty());
    e.drive(&mut host, 100, 10).unwrap();
    assert_eq!(settled(&e, &first).outcome.as_deref(), Some("error"));
    let second = e
        .admit("invoke", "main", Value::Null, authority(), 200)
        .unwrap();
    e.drive(&mut host, 200, 10).unwrap();
    assert_eq!(host.dispatched.len(), 1);
    e.recover(&mut host, 301).unwrap();
    e.drive(&mut host, 301, 10).unwrap();
    let state = e.snapshot().unwrap();
    assert_eq!(host.dispatched.len(), 1);
    assert_eq!(state.runs[&second].status, RunStatus::Blocked);
    assert!(
        state
            .intents
            .values()
            .any(|i| i.status == IntentStatus::Unknown)
    );
}

#[test]
fn failed_transition_commit_has_no_partial_intent_and_never_dispatches() {
    let e = Engine::new(InterruptedStorage::default(), "atomic", "owner").unwrap();
    let artifact = invoke_artifact(command(Recovery::KeyedIdempotent), None);
    let digest = artifact.digest.clone();
    e.install(artifact).unwrap();
    e.activate("invoke", &digest, authority()).unwrap();
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let before = e.snapshot().unwrap();
    e.store.next_commit(CommitOutcome::Conflict);
    assert!(matches!(
        e.advance(&id, 0),
        Err(Error::Storage(StorageError::Conflict))
    ));
    let after = e.snapshot().unwrap();
    assert!(after.intents.is_empty());
    assert_eq!(after.receipts.len(), before.receipts.len());
    assert_eq!(after.runs[&id].visits, 0);
    e.advance(&id, 0).unwrap();
    let intent = e
        .snapshot()
        .unwrap()
        .intents
        .values()
        .next()
        .unwrap()
        .id
        .clone();
    let mut host = FakeHost::default();
    e.store.next_commit(CommitOutcome::Conflict);
    assert!(matches!(
        e.dispatch_next(&mut host, 0),
        Err(Error::Storage(StorageError::Conflict))
    ));
    assert!(host.dispatched.is_empty());
    assert_eq!(
        e.snapshot().unwrap().intents[&intent].status,
        IntentStatus::Prepared
    );
}

#[test]
fn unsafe_recovery_requires_audited_request_and_preserves_original_authority() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::Unsafe), None),
        "invoke",
    );
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    let intent = e
        .snapshot()
        .unwrap()
        .intents
        .values()
        .next()
        .unwrap()
        .clone();
    let request = e
        .request_recovery(
            &intent.id,
            "operator",
            "authoritative status available",
            vec!["evidence:1".into()],
            1,
        )
        .unwrap();
    assert_eq!(
        request,
        e.request_recovery(
            &intent.id,
            "operator",
            "authoritative status available",
            vec!["evidence:1".into()],
            2
        )
        .unwrap()
    );
    host.responses.insert(
        "reconcile:test.command".into(),
        vec![DispatchResult::Completed {
            outcome: "ok".into(),
            output: Value::Null,
        }],
    );
    e.recover(&mut host, 2).unwrap();
    assert!(e.snapshot().unwrap().recovery_requests[&request].consumed);
    e.drive(&mut host, 2, 10).unwrap();
    assert_eq!(settled(&e, &id).outcome.as_deref(), Some("ok"));
    let cohort = &e.store.settled_cohorts("test")[0];
    let settled_intent = cohort.intents.iter().find(|i| i.id == intent.id).unwrap();
    assert_eq!(settled_intent.authority, authority());
    assert!(
        cohort
            .receipts
            .iter()
            .any(|r| r.kind == "authorized_recovery")
    );
    // The consumed request left with its operation.
    assert!(e.snapshot().unwrap().recovery_requests.is_empty());
}
