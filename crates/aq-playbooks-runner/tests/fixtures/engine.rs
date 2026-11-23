use aq_playbooks_model::*;
use aq_playbooks_runner::*;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn set(xs: &[&str]) -> BTreeSet<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

pub(super) fn authority() -> Authority {
    Authority {
        actor: "alice".into(),
        scope: "project".into(),
        capabilities: set(&["execute"]),
    }
}

pub(super) fn edges(xs: &[(&str, &str)]) -> BTreeMap<String, String> {
    xs.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

pub(super) fn ret(outcome: &str) -> Step {
    Step {
        op: Operation::Return {
            outcome: outcome.into(),
            output: Expr::literal(Value::Null),
        },
        edges: BTreeMap::new(),
        on_error: None,
    }
}

#[derive(Default)]
pub(super) struct FixtureContracts {
    pub(super) registry: Registry,
    pub(super) dependencies: BTreeMap<String, CallableDependency>,
}

pub(super) fn artifact(
    id: &str,
    start: &str,
    steps: BTreeMap<String, Step>,
    contracts: FixtureContracts,
) -> Artifact {
    Artifact::seal(ArtifactContent {
        format_version: 1,
        compiler: "test".into(),
        source: SourceBinding {
            digest: hash_bytes(b"source"),
            header: SourceHeader {
                format_version: 1,
                id: id.into(),
                scope: "project".into(),
                interfaces: set(&["main"]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: id.into(),
            purpose: "semantics test".into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Null,
                    outcomes: BTreeMap::from([
                        ("ok".into(), Schema::Null),
                        ("error".into(), Schema::Null),
                    ]),
                    start: start.into(),
                },
            )]),
            steps,
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: set(&["execute"]),
        },
        registry: contracts.registry,
        dependencies: contracts.dependencies,
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })
    .unwrap()
}

pub(super) fn install(e: &Engine<MemoryStorage>, a: Artifact, binding: &str) -> String {
    let d = a.digest.clone();
    e.install(a).unwrap();
    e.activate(binding, &d, authority()).unwrap();
    d
}

pub(super) fn command(recovery: Recovery) -> CommandContract {
    CommandContract {
        name: "test.command".into(),
        version: "1".into(),
        input: Schema::Null,
        outcomes: BTreeMap::from([("ok".into(), Schema::Null), ("retry".into(), Schema::Null)]),
        capabilities: set(&["execute"]),
        scopes: set(&["project"]),
        effect: Effect::Mutation,
        recovery,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 1024,
        deadline_ms: 100,
    }
}

pub(super) fn invoke_artifact(c: CommandContract, retry: Option<RetryPolicy>) -> Artifact {
    let reference = ContractRef {
        name: c.name.clone(),
        fingerprint: c.fingerprint().unwrap(),
    };
    artifact(
        "invoke",
        "invoke",
        BTreeMap::from([
            (
                "invoke".into(),
                Step {
                    op: Operation::Invoke {
                        command: reference,
                        input: Expr::literal(Value::Null),
                        bind: "result".into(),
                        retry,
                    },
                    edges: edges(&[("ok", "ok"), ("retry", "error")]),
                    on_error: Some("error".into()),
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
        FixtureContracts {
            registry: Registry {
                commands: BTreeMap::from([(c.name.clone(), c)]),
                events: BTreeMap::new(),
            },
            ..Default::default()
        },
    )
}

pub(super) fn event_contract() -> EventContract {
    EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: "test.completed".into(),
        version: "1".into(),
        payload: Schema::Null,
        trusted_producers: set(&["host"]),
        scopes: set(&["project"]),
        correlation_fields: set(&["invocation"]),
        subject_field: Some("subject".into()),
        revision_field: Some("revision".into()),
        max_payload_bytes: 1024,
        retention_ms: 1000,
    }
}

pub(super) fn wait_artifact() -> Artifact {
    let c = event_contract();
    artifact(
        "waiter",
        "wait",
        BTreeMap::from([
            (
                "wait".into(),
                Step {
                    op: Operation::Wait {
                        event: Some(ContractRef {
                            name: c.name.clone(),
                            fingerprint: c.fingerprint().unwrap(),
                        }),
                        correlation: BTreeMap::from([(
                            "invocation".into(),
                            Expr::literal("exact-invocation"),
                        )]),
                        minimum_revision: Some(Expr::literal(2)),
                        deadline_ms: 100,
                        bind: "completion".into(),
                    },
                    edges: edges(&[("received", "ok"), ("timeout", "error")]),
                    on_error: Some("error".into()),
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
        FixtureContracts {
            registry: Registry {
                commands: BTreeMap::new(),
                events: BTreeMap::from([(c.name.clone(), c)]),
            },
            ..Default::default()
        },
    )
}

pub(super) fn event(id: &str, rev: u64) -> EventEnvelope {
    let c = event_contract();
    EventEnvelope {
        producer: "host".into(),
        event_id: id.into(),
        contract: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        scope: "project".into(),
        correlation: BTreeMap::from([("invocation".into(), "exact-invocation".into())]),
        subject: Some("object".into()),
        revision: Some(rev),
        causation: None,
        occurred_at_ms: Some(0),
        observed_at_ms: None,
        payload: Value::Null,
    }
}

pub(super) fn child_dependency(a: &Artifact) -> CallableDependency {
    CallableDependency {
        selection: DependencySelection::Exact,
        binding: a.content.playbook.id.clone(),
        artifact: a.digest.clone(),
        interfaces: a.content.playbook.entries.clone(),
        capabilities: set(&["execute"]),
        scope: "project".into(),
        dependencies: BTreeSet::new(),
    }
}

pub(super) fn parent_artifact(child: &Artifact, mode: CallMode) -> Artifact {
    let synchronous = mode == CallMode::Wait;
    artifact(
        "parent",
        "call",
        BTreeMap::from([
            (
                "call".into(),
                Step {
                    op: Operation::Call {
                        dependency: "child".into(),
                        entry: "main".into(),
                        input: Expr::literal(Value::Null),
                        mode,
                        bind: "child_handle".into(),
                    },
                    edges: if synchronous {
                        edges(&[("ok", "ok"), ("error", "error")])
                    } else {
                        edges(&[("started", "ok")])
                    },
                    on_error: Some("error".into()),
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
        FixtureContracts {
            dependencies: BTreeMap::from([("child".into(), child_dependency(child))]),
            ..Default::default()
        },
    )
}

pub(super) fn engine() -> Engine<MemoryStorage> {
    Engine::new(MemoryStorage::retaining_settled(), "test", "incarnation-1").unwrap()
}
