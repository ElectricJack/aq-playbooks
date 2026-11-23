use super::*;

pub(super) fn pg_authority() -> Authority {
    Authority {
        actor: "synthetic-test".into(),
        scope: "project".into(),
        capabilities: Default::default(),
    }
}
fn pg_command() -> aq_playbooks_model::CommandContract {
    use aq_playbooks_model::*;
    CommandContract {
        name: "test.command".into(),
        version: "1".into(),
        input: Schema::Null,
        outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
        capabilities: Default::default(),
        scopes: ["project".to_string()].into(),
        effect: Effect::Mutation,
        recovery: Recovery::KeyedIdempotent,
        asynchronous: None,
        cancellation: false,
        public_result_fields: Default::default(),
        sensitive_input_fields: Default::default(),
        sensitive_output_fields: Default::default(),
        max_payload_bytes: 1024,
        deadline_ms: 100_000,
    }
}
/// `label`'s fixture with `steps` from `start`.
pub(super) fn steps_artifact(
    label: &str,
    start: &str,
    steps: Vec<(&str, aq_playbooks_model::Step)>,
    dependencies: BTreeMap<String, aq_playbooks_model::CallableDependency>,
) -> aq_playbooks_model::Artifact {
    let mut content = support::artifact(label).content;
    content.playbook.entries.get_mut("main").unwrap().start = start.into();
    content.playbook.steps = steps.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    let c = pg_command();
    content.registry.commands.insert(c.name.clone(), c);
    content.dependencies = dependencies;
    aq_playbooks_model::Artifact::seal(content).unwrap()
}
pub(super) fn ret_ok() -> aq_playbooks_model::Step {
    support::artifact("a").content.playbook.steps["return"].clone()
}
/// invoke `test.command`, then return ok.
pub(super) fn pg_invoke_artifact() -> aq_playbooks_model::Artifact {
    use aq_playbooks_model::*;
    let c = pg_command();
    steps_artifact(
        "invoke",
        "invoke",
        vec![
            (
                "invoke",
                Step {
                    op: Operation::Invoke {
                        command: ContractRef {
                            name: c.name.clone(),
                            fingerprint: c.fingerprint().unwrap(),
                        },
                        input: Expr::literal(Value::Null),
                        bind: "result".into(),
                        retry: None,
                    },
                    edges: BTreeMap::from([("ok".into(), "return".into())]),
                    on_error: Some("return".into()),
                },
            ),
            ("return", ret_ok()),
        ],
        BTreeMap::new(),
    )
}
pub(super) fn ok_host() -> FakeHost {
    FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![
                DispatchResult::Completed {
                    outcome: "ok".into(),
                    output: Value::Null,
                },
                DispatchResult::Completed {
                    outcome: "ok".into(),
                    output: Value::Null,
                },
            ],
        )]),
        ..Default::default()
    }
}

// --- Task 6: window entries dropped 64 per commit --------------------------

pub(super) fn pg_keyed<S: Storage>(e: &Engine<S>, key: &str) -> KeyedAdmissionRequest {
    KeyedAdmissionRequest {
        key: key.into(),
        binding: "a".into(),
        expected_activation_revision: e.snapshot().unwrap().bindings["a"].revision,
        entry: "main".into(),
        input: Value::Null,
        authority: pg_authority(),
    }
}
pub(super) fn pg_event_contract() -> aq_playbooks_model::EventContract {
    use aq_playbooks_model::*;
    EventContract {
        sensitive_payload_fields: Default::default(),
        name: "test.signal".into(),
        version: "1".into(),
        payload: Schema::Null,
        trusted_producers: ["host".to_string()].into(),
        scopes: ["project".to_string()].into(),
        correlation_fields: ["invocation".to_string()].into(),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 1024,
        retention_ms: 1_000,
    }
}
pub(super) fn pg_event(id: &str) -> aq_playbooks_model::EventEnvelope {
    let c = pg_event_contract();
    aq_playbooks_model::EventEnvelope {
        producer: "host".into(),
        event_id: id.into(),
        contract: aq_playbooks_model::ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        scope: "project".into(),
        correlation: BTreeMap::from([("invocation".into(), id.into())]),
        subject: None,
        revision: None,
        causation: None,
        occurred_at_ms: None,
        observed_at_ms: None,
        payload: Value::Null,
    }
}

/// A waiter on `test.signal` with correlation `invocation = "held"`: it
/// holds the event `pg_event("held")` while it waits (it is never driven
/// here, so its deadline never fires).
pub(super) fn pg_waiter_artifact() -> aq_playbooks_model::Artifact {
    use aq_playbooks_model::*;
    let c = pg_event_contract();
    let mut content = support::artifact("waiter").content;
    content.playbook.entries.get_mut("main").unwrap().start = "wait".into();
    content.playbook.steps = BTreeMap::from([
        (
            "wait".to_string(),
            Step {
                op: Operation::Wait {
                    event: Some(ContractRef {
                        name: c.name.clone(),
                        fingerprint: c.fingerprint().unwrap(),
                    }),
                    correlation: BTreeMap::from([("invocation".into(), Expr::literal("held"))]),
                    minimum_revision: None,
                    deadline_ms: 100,
                    bind: "completion".into(),
                },
                edges: BTreeMap::from([
                    ("received".into(), "return".into()),
                    ("timeout".into(), "return".into()),
                ]),
                on_error: Some("return".into()),
            },
        ),
        ("return".to_string(), ret_ok()),
    ]);
    content.registry.events.insert(c.name.clone(), c);
    Artifact::seal(content).unwrap()
}
