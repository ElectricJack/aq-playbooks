use super::*;

#[test]
fn t18_receipts_default_deny_result_fields_and_refuse_protected_inputs_before_intent() {
    let e = engine();
    let c = command(
        Schema::Null,
        Schema::object(BTreeMap::from([("secret".into(), Schema::string(128))])),
    );
    let a = invoked(c, Expr::literal(Value::Null));
    install(&e, &a, "invoked");
    e.admit("invoked", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "review.command".into(),
            vec![DispatchResult::Completed {
                outcome: "ok".into(),
                output: json!({"secret":"must-not-appear-in-receipt"}),
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 10).unwrap();
    // The run finished and its receipts left as history: neither the
    // receipts it settled with nor their public history items carry it.
    assert_private_result_redacted(&e);
    let e = engine();
    let mut c = command(
        Schema::object(BTreeMap::from([("secret".into(), Schema::string(128))])),
        Schema::Null,
    );
    c.sensitive_input_fields = set(&["secret"]);
    let a = invoked(
        c,
        Expr::literal(json!({"secret":"never-persist-as-intent"})),
    );
    install(&e, &a, "invoked");
    assert!(matches!(
        e.admit("invoked", "main", Value::Null, authority(), 0),
        Err(Error::Refused(_))
    ));
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty());
    assert!(s.intents.is_empty());
    assert!(
        !String::from_utf8(canonical_bytes(&s.receipts).unwrap())
            .unwrap()
            .contains("never-persist")
    );
}

#[test]
fn command_payloads_obey_tighter_run_limits_on_both_sides() {
    let e = engine();
    let c = command(Schema::string(128), Schema::Null);
    let mut a = invoked(c, Expr::literal("too-large-for-run-limit"));
    a.content.playbook.limits.max_payload_bytes = 8;
    reseal(&mut a);
    install(&e, &a, "invoked");
    let id = e
        .admit("invoked", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    assert!(e.snapshot().unwrap().intents.is_empty());
    assert_eq!(e.snapshot().unwrap().runs[&id].step, "error");
    let e = engine();
    let c = command(Schema::Null, Schema::string(128));
    let mut a = invoked(c, Expr::literal(Value::Null));
    a.content.playbook.limits.max_payload_bytes = 8;
    reseal(&mut a);
    install(&e, &a, "invoked");
    let id = e
        .admit("invoked", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "review.command".into(),
            vec![DispatchResult::Completed {
                outcome: "ok".into(),
                output: json!("too-large-for-run-limit"),
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 10).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].status, RunStatus::Blocked);
    assert!(
        s.intents
            .values()
            .all(|i| i.result.is_none() && i.status == IntentStatus::Unknown)
    );
}

#[test]
fn t18_protected_output_contract_is_refused_before_external_dispatch() {
    let e = engine();
    let mut c = command(
        Schema::Null,
        Schema::object(BTreeMap::from([("secret".into(), Schema::string(128))])),
    );
    c.sensitive_output_fields = set(&["secret"]);
    let a = invoked(c, Expr::literal(Value::Null));
    install(&e, &a, "invoked");
    assert!(matches!(
        e.admit("invoked", "main", Value::Null, authority(), 0),
        Err(Error::Refused(_))
    ));
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "review.command".into(),
            vec![DispatchResult::Completed {
                outcome: "ok".into(),
                output: json!({"secret":"must-never-be-requested-without-protection"}),
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 10).unwrap();
    assert!(host.dispatched.is_empty());
    let s = e.snapshot().unwrap();
    assert!(s.intents.is_empty());
    assert!(s.runs.is_empty());
    assert!(
        !String::from_utf8(canonical_bytes(&s.receipts).unwrap())
            .unwrap()
            .contains("must-never")
    );
}

#[test]
fn tiny_driver_batches_fairly_interleave_runnable_steps_and_prepared_effects() {
    let e = engine();
    let busy = artifact(
        "busy",
        "loop",
        BTreeMap::from([
            (
                "loop".into(),
                Step {
                    op: Operation::ForEach {
                        collection: Expr::literal(json!(vec![0; 100])),
                        item: "busy_item".into(),
                        body: "body".into(),
                        next: "ok".into(),
                        max_items: 100,
                        concurrency: 1,
                        on_failure: IterationFailure::Stop,
                    },
                    edges: BTreeMap::new(),
                    on_error: Some("error".into()),
                },
            ),
            (
                "body".into(),
                Step {
                    op: Operation::Decide {
                        predicate: Expr::literal(true),
                    },
                    edges: edges(&[("true", "loop"), ("false", "loop"), ("unknown", "error")]),
                    on_error: None,
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
    );
    let invoked = invoked(
        command(Schema::Null, Schema::Null),
        Expr::literal(Value::Null),
    );
    install(&e, &busy, "busy");
    install(&e, &invoked, "invoked");
    let busy_id = e
        .admit("busy", "main", Value::Null, authority(), 0)
        .unwrap();
    let effect_id = e
        .admit("invoked", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&effect_id, 0).unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "review.command".into(),
            vec![DispatchResult::Completed {
                outcome: "ok".into(),
                output: Value::Null,
            }],
        )]),
        ..Default::default()
    };
    for _ in 0..12 {
        e.drive(&mut host, 0, 1).unwrap();
    }
    assert_eq!(
        host.dispatched.len(),
        1,
        "a busy ready queue must not starve prepared effects under limit one"
    );
    assert!(!e.snapshot().unwrap().runs[&busy_id].status.terminal());
}

#[test]
fn recovery_obeys_current_engine_authority_revocation() {
    let e = engine();
    let mut c = command(Schema::Null, Schema::Null);
    c.recovery = Recovery::Reconcilable;
    let a = invoked(c, Expr::literal(Value::Null));
    install(&e, &a, "invoked");
    let id = e
        .admit("invoked", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Blocked);
    e.set_execution_authority("reviewer", "project", set(&["execute"]), 1)
        .unwrap();
    host.responses.insert(
        "reconcile:review.command".into(),
        vec![DispatchResult::Completed {
            outcome: "ok".into(),
            output: Value::Null,
        }],
    );
    e.recover(&mut host, 2).unwrap();
    assert!(host.reconciliations.is_empty());
    assert!(
        e.snapshot()
            .unwrap()
            .intents
            .values()
            .all(|intent| intent.status == IntentStatus::Unknown)
    );
}

#[test]
fn unresolved_recovery_rotates_participants_within_one_scope() {
    let e = engine();
    let mut c = command(Schema::Null, Schema::Null);
    c.recovery = Recovery::Reconcilable;
    let a = invoked(c, Expr::literal(Value::Null));
    install(&e, &a, "invoked");
    for _ in 0..2 {
        e.admit("invoked", "main", Value::Null, authority(), 0)
            .unwrap();
    }
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 20).unwrap();
    assert_eq!(e.snapshot().unwrap().intents.len(), 2);
    e.recover(&mut host, 1).unwrap();
    e.recover(&mut host, 2).unwrap();
    assert_eq!(
        host.reconciliations.iter().collect::<BTreeSet<_>>().len(),
        2,
        "one unresolved operation must not starve the next in its scope"
    );
}

#[test]
fn persisted_protection_flag_cannot_override_the_actual_storage_provider() {
    let e = engine();
    let mut c = command(
        Schema::object(BTreeMap::from([("secret".into(), Schema::string(64))])),
        Schema::Null,
    );
    c.sensitive_input_fields = set(&["secret"]);
    let a = invoked(c, Expr::literal(json!({"secret":"synthetic-only"})));
    install(&e, &a, "invoked");
    let mut saved = e.store.load(&e.namespace).unwrap();
    saved.state.protection_available = true;
    e.store
        .compare_and_swap(&e.namespace, &e.owner, saved.revision, &saved.state)
        .unwrap();
    assert!(matches!(
        e.admit("invoked", "main", Value::Null, authority(), 0),
        Err(Error::Refused(_))
    ));
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    let s = e.snapshot().unwrap();
    // The spoofed transient flag remains in the test store's old snapshot;
    // admission used the actual provider=false and committed no input.
    assert!(s.runs.is_empty());
    assert!(s.intents.is_empty());
    assert!(host.dispatched.is_empty());
}

#[test]
fn execution_revocation_blocks_prepared_dispatch_without_discarding_the_intent() {
    let e = engine();
    let a = invoked(
        command(Schema::Null, Schema::Null),
        Expr::literal(Value::Null),
    );
    install(&e, &a, "invoked");
    let id = e
        .admit("invoked", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    let original = e
        .snapshot()
        .unwrap()
        .intents
        .values()
        .next()
        .unwrap()
        .id
        .clone();
    e.set_activation_controls(
        "invoked",
        ActivationControls {
            calls_enabled: true,
            event_starts_enabled: true,
            execution_enabled: false,
        },
        1,
    )
    .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "review.command".into(),
            vec![DispatchResult::Completed {
                outcome: "ok".into(),
                output: Value::Null,
            }],
        )]),
        ..Default::default()
    };
    e.dispatch_next(&mut host, 1).unwrap();
    assert!(host.dispatched.is_empty());
    assert!(e.snapshot().unwrap().intents.contains_key(&original));
    e.set_activation_controls("invoked", ActivationControls::default(), 2)
        .unwrap();
    e.recover(&mut host, 2).unwrap();
    e.drive(&mut host, 2, 20).unwrap();
    assert_eq!(host.dispatched, vec![original]);
    assert_eq!(settled(&e, &id).outcome.as_deref(), Some("ok"));
}

fn assert_private_result_redacted(e: &Engine<MemoryStorage>) {
    let receipts: Vec<Receipt> = e
        .store
        .settled_cohorts("review")
        .into_iter()
        .flat_map(|cohort| cohort.receipts)
        .collect();
    assert!(receipts.iter().any(|r| r.kind == "command_result"));
    assert!(
        !String::from_utf8(canonical_bytes(&receipts).unwrap())
            .unwrap()
            .contains("must-not-appear")
    );
    let history = e
        .inspect(&InspectionQuery {
            kind: InspectionKind::Receipts,
            run: None,
            after: None,
            limit: 1000,
        })
        .unwrap();
    assert!(!history.items.is_empty());
    assert!(
        !serde_json::to_string(&history)
            .unwrap()
            .contains("must-not-appear")
    );
}
