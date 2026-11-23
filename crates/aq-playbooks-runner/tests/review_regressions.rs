//! Cross-boundary regression tests added during independent implementation review.
use aq_playbooks_model::*;
use aq_playbooks_runner::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
#[allow(dead_code)]
#[path = "fixtures/engine.rs"]
mod engine_fixtures;
use engine_fixtures::{edges, ret, set};

fn authority() -> Authority {
    Authority {
        actor: "reviewer".into(),
        scope: "project".into(),
        capabilities: set(&["execute"]),
    }
}

fn artifact(id: &str, start: &str, steps: BTreeMap<String, Step>) -> Artifact {
    Artifact::seal(ArtifactContent {
        format_version: 1,
        compiler: "review-tests".into(),
        source: SourceBinding {
            digest: hash_bytes(id.as_bytes()),
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
            purpose: "Independent regressions".into(),
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
        registry: Registry::default(),
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })
    .unwrap()
}

fn reseal(a: &mut Artifact) {
    *a = Artifact::seal(a.content.clone()).unwrap();
}

fn dependency(a: &Artifact) -> CallableDependency {
    CallableDependency {
        selection: DependencySelection::Exact,
        binding: a.content.playbook.id.clone(),
        artifact: a.digest.clone(),
        interfaces: a.content.playbook.entries.clone(),
        capabilities: a.content.playbook.required_capabilities.clone(),
        scope: "project".into(),
        dependencies: a
            .content
            .dependencies
            .values()
            .flat_map(|d| std::iter::once(d.artifact.clone()).chain(d.dependencies.iter().cloned()))
            .collect(),
    }
}

fn call(dep: &str, mode: CallMode, bind: &str, next: &str) -> Step {
    Step {
        op: Operation::Call {
            dependency: dep.into(),
            entry: "main".into(),
            input: Expr::literal(Value::Null),
            mode: mode.clone(),
            bind: bind.into(),
        },
        edges: if mode == CallMode::Wait {
            edges(&[("ok", next), ("error", "error")])
        } else {
            edges(&[("started", next)])
        },
        on_error: Some("error".into()),
    }
}

fn caller(id: &str, child: &Artifact) -> Artifact {
    let mut a = artifact(
        id,
        "call",
        BTreeMap::from([
            (
                "call".into(),
                call("child", CallMode::Wait, "child_result", "ok"),
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
    );
    a.content
        .dependencies
        .insert("child".into(), dependency(child));
    reseal(&mut a);
    a
}

fn engine() -> Engine<MemoryStorage> {
    Engine::new(MemoryStorage::retaining_settled(), "review", "owner-one").unwrap()
}

/// A finished run as its cohort left the State (spec 0003: settled cohorts
/// leave in the commit that settles them). Asserts that it has left.
fn settled(e: &Engine<MemoryStorage>, id: &str) -> Run {
    assert!(
        !e.snapshot().unwrap().runs.contains_key(id),
        "{id} is still live"
    );
    e.store
        .settled_run(&e.namespace, id)
        .unwrap_or_else(|| panic!("{id} is not in history"))
}

/// Every run that has left the State, oldest cohort first.
fn settled_runs(e: &Engine<MemoryStorage>) -> Vec<Run> {
    e.store
        .settled_cohorts(&e.namespace)
        .into_iter()
        .flat_map(|cohort| cohort.runs)
        .collect()
}

fn install(e: &Engine<MemoryStorage>, a: &Artifact, binding: &str) {
    e.install(a.clone()).unwrap();
    e.activate(binding, &a.digest, authority()).unwrap();
}

fn event_contract() -> EventContract {
    EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: "review.signal".into(),
        version: "1".into(),
        payload: Schema::Null,
        trusted_producers: set(&["host"]),
        scopes: set(&["project"]),
        correlation_fields: set(&["invocation"]),
        subject_field: Some("subject".into()),
        revision_field: Some("revision".into()),
        max_payload_bytes: 1024,
        retention_ms: 10_000,
    }
}

fn envelope(id: &str) -> EventEnvelope {
    let c = event_contract();
    EventEnvelope {
        producer: "host".into(),
        event_id: id.into(),
        contract: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        scope: "project".into(),
        correlation: BTreeMap::from([("invocation".into(), "one".into())]),
        subject: Some("subject-one".into()),
        revision: Some(1),
        causation: None,
        occurred_at_ms: Some(0),
        observed_at_ms: None,
        payload: Value::Null,
    }
}

fn event_artifact(id: &str) -> Artifact {
    let mut a = artifact(id, "ok", BTreeMap::from([("ok".into(), ret("ok"))]));
    let c = event_contract();
    a.content.registry.events.insert(c.name.clone(), c.clone());
    a.content.playbook.triggers.push(Trigger {
        id: "on-signal".into(),
        event: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        entry: "main".into(),
        input: Expr::literal(Value::Null),
    });
    reseal(&mut a);
    a
}

fn joined_parent(children: &[Artifact], mode: JoinMode, remaining: RemainingPolicy) -> Artifact {
    let mut a = artifact(
        "joining",
        "first",
        BTreeMap::from([
            (
                "first".into(),
                call("one", CallMode::OwnedAsync, "one_handle", "second"),
            ),
            (
                "second".into(),
                call("two", CallMode::OwnedAsync, "two_handle", "join"),
            ),
            (
                "join".into(),
                Step {
                    op: Operation::Join {
                        member_keys: vec![],
                        members: vec![Expr::reference("one_handle"), Expr::reference("two_handle")],
                        mode,
                        accepted_outcomes: set(&["ok"]),
                        deadline_ms: 100,
                        remaining,
                        bind: "joined".into(),
                    },
                    edges: edges(&[
                        ("completed", "ok"),
                        ("no_match", "error"),
                        ("timeout", "error"),
                    ]),
                    on_error: Some("error".into()),
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
    );
    for (key, child) in ["one", "two"].into_iter().zip(children) {
        a.content.dependencies.insert(key.into(), dependency(child));
    }
    reseal(&mut a);
    a
}

fn command(input: Schema, output: Schema) -> CommandContract {
    CommandContract {
        name: "review.command".into(),
        version: "1".into(),
        input,
        outcomes: BTreeMap::from([("ok".into(), output)]),
        capabilities: set(&["execute"]),
        scopes: set(&["project"]),
        effect: Effect::Mutation,
        recovery: Recovery::KeyedIdempotent,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 4096,
        deadline_ms: 1000,
    }
}

fn invoked(c: CommandContract, input: Expr) -> Artifact {
    let mut a = artifact(
        "invoked",
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
                        input,
                        bind: "result".into(),
                        retry: None,
                    },
                    edges: edges(&[("ok", "ok")]),
                    on_error: Some("error".into()),
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
    );
    a.content.registry.commands.insert(c.name.clone(), c);
    reseal(&mut a);
    a
}

#[test]
fn t26_nested_captured_iteration_consumes_shared_budget_and_restores_lexical_roots() {
    let e = engine();
    let a = nested_iteration_artifact();
    install(&e, &a, "nested");
    let id = e
        .admit("nested", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    // The budget leaves with the cohort: read it while the run is live.
    let mut iterations = 0;
    for _ in 0..50 {
        if e.drive(&mut host, 0, 1).unwrap() == 0 {
            break;
        }
        if let Some(budget) = e.snapshot().unwrap().budgets.get(&id) {
            iterations = budget.iterations;
        }
    }
    assert_eq!(iterations, 5);
    let run = settled(&e, &id);
    assert_eq!(run.outcome.as_deref(), Some("ok"));
    assert!(!run.variables.contains_key("inner_item"));
    assert!(!run.variables.contains_key("outer_item"));
}

fn nested_iteration_artifact() -> Artifact {
    let mut a = artifact(
        "nested",
        "outer",
        BTreeMap::from([
            (
                "outer".into(),
                Step {
                    op: Operation::ForEach {
                        collection: Expr::literal(json!([[1, 2], [3]])),
                        item: "outer_item".into(),
                        body: "inner".into(),
                        next: "ok".into(),
                        max_items: 2,
                        concurrency: 1,
                        on_failure: IterationFailure::Stop,
                    },
                    edges: BTreeMap::new(),
                    on_error: Some("error".into()),
                },
            ),
            (
                "inner".into(),
                Step {
                    op: Operation::ForEach {
                        collection: Expr::reference("outer_item"),
                        item: "inner_item".into(),
                        body: "body".into(),
                        next: "outer".into(),
                        max_items: 2,
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
                    edges: edges(&[("true", "inner"), ("false", "inner"), ("unknown", "error")]),
                    on_error: None,
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
    );
    a.content.playbook.limits.max_iterations = 5;
    reseal(&mut a);
    a
}

#[path = "review_regressions/catalog.rs"]
mod catalog;
#[path = "review_regressions/effects.rs"]
mod effects;
#[path = "review_regressions/events.rs"]
mod events;
#[path = "review_regressions/history.rs"]
mod history;
#[path = "review_regressions/joins.rs"]
mod joins;
