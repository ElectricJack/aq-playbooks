use super::*;

fn triggered_wait_artifact() -> Artifact {
    let mut a = wait_artifact().content;
    let c = event_contract();
    a.playbook.triggers.push(Trigger {
        id: "start".into(),
        event: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        entry: "main".into(),
        input: Expr::literal(Value::Null),
    });
    Artifact::seal(a).unwrap()
}

#[test]
fn activation_controls_separate_calls_events_and_current_execution() {
    let e = engine();
    install(&e, triggered_wait_artifact(), "waiter");
    e.set_activation_controls(
        "waiter",
        ActivationControls {
            calls_enabled: false,
            event_starts_enabled: true,
            execution_enabled: true,
        },
        0,
    )
    .unwrap();
    assert!(
        e.admit("waiter", "main", Value::Null, authority(), 0)
            .is_err()
    );
    let event = e.admit_event("host", event("trigger", 2), 0).unwrap();
    assert_eq!(event.destinations, 1);
    e.set_activation_controls(
        "waiter",
        ActivationControls {
            calls_enabled: true,
            event_starts_enabled: false,
            execution_enabled: false,
        },
        1,
    )
    .unwrap();
    assert_eq!(
        e.admit_event("host", self::event("disabled", 2), 1)
            .unwrap()
            .destinations,
        0
    );
    let id = e
        .admit("waiter", "main", Value::Null, authority(), 1)
        .unwrap();
    e.advance(&id, 1).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Blocked);
    e.set_activation_controls("waiter", ActivationControls::default(), 2)
        .unwrap();
    e.drive(&mut FakeHost::default(), 2, 10).unwrap();
    // Re-enabled, the run received the retained event and finished.
    let run = settled(&e, &id);
    assert_ne!(run.reason.as_deref(), Some("execution authority revoked"));
    assert_eq!(run.outcome.as_deref(), Some("ok"));
}

#[test]
fn gc_preserves_bound_transitive_closure_then_collects_terminal_cohorts_in_pages() {
    let e = engine();
    let leaf = artifact(
        "leaf",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
        FixtureContracts::default(),
    );
    let middle = parent_artifact(&leaf, CallMode::Wait);
    let mut root = parent_artifact(&middle, CallMode::Wait).content;
    root.playbook.id = "root".into();
    root.source.header.id = "root".into();
    let root = Artifact::seal(root).unwrap();
    e.install(leaf.clone()).unwrap();
    e.install(middle.clone()).unwrap();
    install(&e, root, "root");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    assert_eq!(e.collect_garbage(100, 10).unwrap().artifacts, 0);
    let id = e
        .admit("root", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 50).unwrap();
    // The three-run cohort left the State when it settled, without GC.
    let state = e.snapshot().unwrap();
    assert!(state.runs.is_empty());
    assert_eq!(e.store.settled_cohorts("test")[0].runs.len(), 3);
    assert_eq!(inspected(&e, InspectionKind::Runs, None).len(), 3);
    // GC still pages over what is left and keeps the bound closure.
    for _ in 0..3 {
        assert_eq!(e.collect_garbage(100, 1).unwrap().runs, 0);
    }
    let state = e.snapshot().unwrap();
    assert!(state.runs.is_empty());
    assert!(!state.budgets.contains_key(&id));
    for digest in [&leaf.digest, &middle.digest] {
        assert!(state.artifacts.contains_key(digest));
    }
}

fn secret_input_artifact() -> Artifact {
    let mut c = command(Recovery::KeyedIdempotent);
    c.input = Schema::object(BTreeMap::from([("secret".into(), Schema::string(128))]));
    c.sensitive_input_fields = set(&["secret"]);
    let input_schema = c.input.clone();
    let mut content = invoke_artifact(c, None).content;
    content.playbook.entries.get_mut("main").unwrap().input = input_schema;
    if let Operation::Invoke { input, .. } =
        &mut content.playbook.steps.get_mut("invoke").unwrap().op
    {
        *input = Expr::reference("input");
    }
    Artifact::seal(content).unwrap()
}

#[test]
fn protected_root_and_transitive_inputs_are_refused_before_any_snapshot_write() {
    let e = engine();
    let child = secret_input_artifact();
    install(&e, child.clone(), "secret");
    let before = e.store.load(&e.namespace).unwrap();
    let secret = json!({"secret":"never-persist-this-root-value"});
    assert!(matches!(
        e.admit("secret", "main", secret.clone(), authority(), 0),
        Err(Error::Refused(_))
    ));
    let after = e.store.load(&e.namespace).unwrap();
    assert_eq!(before.revision, after.revision);
    assert!(after.state.runs.is_empty());
    assert!(
        !String::from_utf8(canonical_bytes(&after.state).unwrap())
            .unwrap()
            .contains("never-persist-this-root-value")
    );
    let mut parent = parent_artifact(&child, CallMode::Wait).content;
    parent.playbook.entries.get_mut("main").unwrap().input =
        child.content.playbook.entries["main"].input.clone();
    if let Operation::Call { input, .. } = &mut parent.playbook.steps.get_mut("call").unwrap().op {
        *input = Expr::reference("input");
    }
    let parent = Artifact::seal(parent).unwrap();
    install(&e, parent, "parent");
    let before = e.store.load(&e.namespace).unwrap().revision;
    assert!(matches!(
        e.admit("parent", "main", secret, authority(), 1),
        Err(Error::Refused(_))
    ));
    let state = e.store.load(&e.namespace).unwrap();
    assert_eq!(before, state.revision);
    assert!(state.state.runs.is_empty());
}

#[test]
fn protected_event_fields_and_protected_trigger_closures_refuse_payload_persistence() {
    let e = engine();
    let mut c = event_contract();
    c.payload = Schema::object(BTreeMap::from([("secret".into(), Schema::string(128))]));
    c.sensitive_payload_fields = set(&["secret"]);
    let a = artifact(
        "event-registry",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
        FixtureContracts {
            registry: Registry {
                commands: BTreeMap::new(),
                events: BTreeMap::from([(c.name.clone(), c.clone())]),
            },
            ..Default::default()
        },
    );
    e.install(a).unwrap();
    let mut envelope = event("protected-event", 2);
    envelope.contract.fingerprint = c.fingerprint().unwrap();
    envelope.payload = json!({"secret":"never-persist-this-event-value"});
    let before = e.store.load(&e.namespace).unwrap().revision;
    assert!(matches!(
        e.admit_event("host", envelope.clone(), 0),
        Err(Error::Refused(_))
    ));
    assert_eq!(before, e.store.load(&e.namespace).unwrap().revision);
    assert!(e.snapshot().unwrap().events.is_empty());
    // Even when the event itself is public, a subscribed workflow's protected
    // pinned closure prevents its mapped root input from being stored in plaintext.
    let e = engine();
    let mut target = secret_input_artifact().content;
    let mut public_event = c.clone();
    public_event.sensitive_payload_fields.clear();
    target
        .registry
        .events
        .insert(public_event.name.clone(), public_event.clone());
    target.playbook.triggers.push(Trigger {
        id: "secret-trigger".into(),
        event: ContractRef {
            name: public_event.name.clone(),
            fingerprint: public_event.fingerprint().unwrap(),
        },
        entry: "main".into(),
        input: Expr::reference("event"),
    });
    install(&e, Artifact::seal(target).unwrap(), "target");
    envelope.contract.fingerprint = public_event.fingerprint().unwrap();
    assert!(matches!(
        e.admit_event("host", envelope, 0),
        Err(Error::Refused(_))
    ));
    let state = e.snapshot().unwrap();
    assert!(state.events.is_empty());
    assert!(state.runs.is_empty());
    assert!(
        !String::from_utf8(canonical_bytes(&state).unwrap())
            .unwrap()
            .contains("never-persist-this-event-value")
    );
}

#[test]
fn interval_cursor_survives_reconfiguration_and_reactivation_starts_a_fresh_interval() {
    use aq_playbooks_model::schedules::{BindingSchedule, ScheduleOccurrence};
    let engine = engine();
    let artifact = triggered_wait_artifact();
    let binding = &artifact.content.playbook.id;
    let reference = artifact.content.playbook.triggers[0].event.clone();
    let schedule = BindingSchedule {
        key: "daily".into(),
        every_ms: 30_000,
    };
    engine.install(artifact.clone()).unwrap();
    let configure = |now| {
        engine
            .configure_binding(
                binding,
                &artifact.digest,
                authority(),
                true,
                std::slice::from_ref(&schedule),
                Some(reference.clone()),
                now,
            )
            .unwrap()
    };
    let collect = |now| {
        engine
            .collect_interval_events(now, 1, |_, _, occurrence: ScheduleOccurrence| {
                let mut event = event(&occurrence.id, 1);
                event.occurred_at_ms = Some(occurrence.scheduled_at_ms);
                event
            })
            .unwrap()
    };
    assert!(configure(0));
    assert!(configure(20_000));
    assert_eq!(
        (collect(29_999), collect(30_000), collect(30_000)),
        (0, 1, 0)
    );
    let revision = engine.snapshot().unwrap().bindings[binding].revision;
    assert!(
        engine
            .deactivate_binding(binding, revision, 31_000)
            .unwrap()
    );
    assert!(!configure(40_000));
    assert_eq!(collect(65_000), 0);
    engine
        .activate_at(binding, &artifact.digest, authority(), 70_000)
        .unwrap();
    assert_eq!((collect(99_999), collect(100_000)), (0, 1));
}
