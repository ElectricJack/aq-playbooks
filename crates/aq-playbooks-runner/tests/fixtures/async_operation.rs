use super::*;

pub(super) fn async_artifact() -> (Artifact, EventContract) {
    async_artifact_with(Recovery::Reconcilable)
}

/// [`async_artifact`] whose command recovers by `recovery`.
pub(super) fn async_artifact_with(recovery: Recovery) -> (Artifact, EventContract) {
    let (c, ev) = contracts(recovery);
    let mut get = command(Recovery::Observation);
    get.name = "operation.get".into();
    get.effect = Effect::Observation;
    let a = artifact(
        "async",
        "invoke",
        BTreeMap::from([
            (
                "invoke".into(),
                Step {
                    op: Operation::Invoke {
                        command: ContractRef {
                            name: c.name.clone(),
                            fingerprint: c.fingerprint().unwrap(),
                        },
                        input: Expr::literal(Value::Null),
                        bind: "handle".into(),
                        retry: None,
                    },
                    edges: edges(&[("accepted", "join"), ("ok", "ok"), ("retry", "error")]),
                    on_error: Some("error".into()),
                },
            ),
            (
                "join".into(),
                Step {
                    op: Operation::Join {
                        member_keys: vec![],
                        members: vec![Expr::reference("handle")],
                        mode: JoinMode::All,
                        accepted_outcomes: set(&["ok"]),
                        deadline_ms: 100,
                        remaining: RemainingPolicy::Cancel,
                        bind: "joined".into(),
                    },
                    edges: edges(&[
                        ("completed", "ok"),
                        ("no_match", "error"),
                        ("timeout", "fallback"),
                    ]),
                    on_error: Some("error".into()),
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
            ("fallback".into(), ret("error")),
        ]),
        FixtureContracts {
            registry: Registry {
                commands: BTreeMap::from([(c.name.clone(), c), (get.name.clone(), get)]),
                events: BTreeMap::from([(ev.name.clone(), ev.clone())]),
            },
            ..Default::default()
        },
    );
    (a, ev)
}

fn contracts(recovery: Recovery) -> (CommandContract, EventContract) {
    let admission = Schema::object(BTreeMap::from([("handle".into(), Schema::string(256))]));
    let completion = Schema::object(BTreeMap::from([
        (
            "outcome".into(),
            Schema::String {
                max_length: 10,
                values: set(&["ok", "retry"]),
            },
        ),
        ("output".into(), Schema::Null),
    ]));
    let mut ev = event_contract();
    ev.name = "async.completed".into();
    ev.payload = completion.clone();
    ev.correlation_fields = set(&["operation"]);
    ev.subject_field = None;
    ev.revision_field = None;
    let mut c = command(recovery);
    c.cancellation = true;
    c.outcomes.insert("accepted".into(), admission);
    c.asynchronous = Some(AsyncContract {
        completion_event: ev.name.clone(),
        completion,
        correlation_field: "operation".into(),
        reconciliation_command: "operation.get".into(),
    });
    (c, ev)
}

pub(super) fn async_event(c: &EventContract, id: &str) -> EventEnvelope {
    EventEnvelope {
        producer: "host".into(),
        event_id: id.into(),
        contract: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        scope: "project".into(),
        correlation: BTreeMap::from([("operation".into(), "server-operation".into())]),
        subject: None,
        revision: None,
        causation: None,
        occurred_at_ms: None,
        observed_at_ms: None,
        payload: json!({"outcome":"ok","output":null}),
    }
}
