//! Live state only (spec 0003): history written with the commit, inspection
//! over live state plus history.
// The playbook helpers are copied from `semantics.rs` for the engine-level
// tests of later tasks; not every one is used by every test here.
#![allow(dead_code)]
use aq_playbooks_model::*;
use aq_playbooks_runner::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
fn set(xs: &[&str]) -> BTreeSet<String> {
    xs.iter().map(|s| s.to_string()).collect()
}
fn authority() -> Authority {
    Authority {
        actor: "alice".into(),
        scope: "project".into(),
        capabilities: set(&["execute"]),
    }
}
fn edges(xs: &[(&str, &str)]) -> BTreeMap<String, String> {
    xs.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}
fn ret(outcome: &str) -> Step {
    Step {
        op: Operation::Return {
            outcome: outcome.into(),
            output: Expr::literal(Value::Null),
        },
        edges: BTreeMap::new(),
        on_error: None,
    }
}
fn artifact(
    id: &str,
    start: &str,
    steps: BTreeMap<String, Step>,
    registry: Registry,
    dependencies: BTreeMap<String, CallableDependency>,
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
        registry,
        dependencies,
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })
    .unwrap()
}
fn install(e: &Engine<MemoryStorage>, a: Artifact, binding: &str) -> String {
    let d = a.digest.clone();
    e.install(a).unwrap();
    e.activate(binding, &d, authority()).unwrap();
    d
}
fn command(recovery: Recovery) -> CommandContract {
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
fn invoke_artifact(c: CommandContract, retry: Option<RetryPolicy>) -> Artifact {
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
        Registry {
            commands: BTreeMap::from([(c.name.clone(), c)]),
            events: BTreeMap::new(),
        },
        BTreeMap::new(),
    )
}
fn event_contract() -> EventContract {
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
fn wait_artifact() -> Artifact {
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
        Registry {
            commands: BTreeMap::new(),
            events: BTreeMap::from([(c.name.clone(), c)]),
        },
        BTreeMap::new(),
    )
}
fn event(id: &str, rev: u64) -> EventEnvelope {
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
fn child_dependency(a: &Artifact) -> CallableDependency {
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
fn parent_artifact(child: &Artifact, mode: CallMode) -> Artifact {
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
        Registry::default(),
        BTreeMap::from([("child".into(), child_dependency(child))]),
    )
}
fn engine() -> Engine<MemoryStorage> {
    Engine::new(MemoryStorage::retaining_settled(), "test", "incarnation-1").unwrap()
}
/// One Completed run `id`, its own root, with receipts at `receipts`.
fn synthetic_cohort(id: &str, finished_at_ms: i64, receipts: &[u64]) -> SettledCohort {
    let run = Run {
        catalog_revision: 1,
        id: id.into(),
        root: id.into(),
        parent: None,
        origin: None,
        artifact: "artifact".into(),
        entry: "main".into(),
        dependencies: BTreeMap::new(),
        authority: authority(),
        step: "ok".into(),
        status: RunStatus::Completed,
        input: Value::Null,
        variables: BTreeMap::new(),
        outcome: Some("ok".into()),
        output: Some(Value::Null),
        reason: None,
        admitted_at_ms: 1,
        visits: 1,
        depth: 0,
        children: vec![],
        pending: None,
        loops: vec![],
        attempts: BTreeMap::new(),
        pending_return: None,
        pending_failure: false,
        terminal_sequence: receipts.last().copied(),
    };
    SettledCohort {
        root: id.into(),
        finished_at_ms,
        runs: vec![run],
        intents: vec![],
        receipts: receipts
            .iter()
            .map(|&sequence| Receipt {
                sequence,
                run: id.into(),
                step: "ok".into(),
                at_ms: finished_at_ms,
                kind: "terminal".into(),
                detail: json!({"status": "completed", "outcome": "ok"}),
            })
            .collect(),
    }
}
fn page(
    store: &impl Storage,
    kind: InspectionKind,
    after: Option<String>,
    limit: usize,
) -> InspectionPage {
    store
        .inspect(
            "ns",
            &InspectionQuery {
                kind,
                run: None,
                after,
                limit,
            },
        )
        .unwrap()
}

#[test]
fn memory_history_is_written_with_the_commit_and_merged_into_inspection() {
    let store = MemoryStorage::default();
    store.acquire("ns", "w").unwrap();
    let mut state = State::default();
    state.settled.push(synthetic_cohort("run-a", 10, &[1, 2]));
    let revision = store.compare_and_swap("ns", "w", 0, &state).unwrap();
    let runs = page(&store, InspectionKind::Runs, None, 10);
    assert_eq!(runs.revision, revision);
    assert_eq!(
        runs.items,
        vec![inspection::public_run(&state.settled[0].runs[0])]
    );
    assert_eq!(
        page(&store, InspectionKind::Receipts, None, 10).items.len(),
        2
    );
    assert!(store.load("ns").unwrap().state.settled.is_empty());
    assert!(
        matches!(store.compare_and_swap("ns", "w", revision, &state),
        Err(StorageError::Backend(m)) if m.contains("already recorded"))
    );
}
#[test]
fn conflicting_commit_writes_no_history() {
    let store = MemoryStorage::default();
    store.acquire("ns", "w").unwrap();
    let mut state = State::default();
    state.settled.push(synthetic_cohort("run-a", 10, &[1]));
    assert_eq!(
        store.compare_and_swap("ns", "w", 7, &state),
        Err(StorageError::Conflict)
    );
    assert!(
        page(&store, InspectionKind::Runs, None, 10)
            .items
            .is_empty()
    );
}
#[test]
fn inspection_pages_across_live_and_history_without_gaps() {
    // live receipts at sequences 2 and 4, history receipts at 1, 3, 5
    let store = MemoryStorage::default();
    store.acquire("ns", "w").unwrap();
    let mut state = State::default();
    for seq in [2, 4] {
        state.receipts.push(Receipt {
            sequence: seq,
            run: "live".into(),
            step: "s".into(),
            at_ms: 1,
            kind: "admitted".into(),
            detail: json!({}),
        });
    }
    state.settled.push(synthetic_cohort("done", 10, &[1, 3, 5]));
    store.compare_and_swap("ns", "w", 0, &state).unwrap();
    let mut seen = vec![];
    let mut after = None;
    loop {
        let p = page(&store, InspectionKind::Receipts, after, 2);
        seen.extend(p.items.iter().map(|v| v["sequence"].as_u64().unwrap()));
        if p.next.is_none() {
            break;
        }
        after = p.next;
    }
    assert_eq!(seen, vec![1, 2, 3, 4, 5]);
}
#[test]
fn delete_history_before_removes_only_older_cohorts() {
    let store = MemoryStorage::default();
    store.acquire("ns", "w").unwrap();
    let state = State {
        settled: vec![
            synthetic_cohort("old", 10, &[1]),
            synthetic_cohort("new", 20, &[2]),
        ],
        ..State::default()
    };
    store.compare_and_swap("ns", "w", 0, &state).unwrap();
    assert_eq!(store.delete_history("ns", "w", 20).unwrap(), 2); // run + receipt of "old"
    let ids: Vec<_> = page(&store, InspectionKind::Runs, None, 10)
        .items
        .iter()
        .map(|v| v["id"].clone())
        .collect();
    assert_eq!(ids, vec![json!("new")]);
}
#[test]
fn engine_refuses_storage_that_does_not_persist_history() {
    // Forwards load/compare_and_swap/acquire/release only.
    struct NoHistory(MemoryStorage);
    impl Storage for NoHistory {
        fn load(&self, n: &str) -> std::result::Result<VersionedState, StorageError> {
            self.0.load(n)
        }
        fn compare_and_swap(
            &self,
            n: &str,
            o: &str,
            v: u64,
            s: &State,
        ) -> std::result::Result<u64, StorageError> {
            self.0.compare_and_swap(n, o, v, s)
        }
        fn acquire(&self, n: &str, o: &str) -> std::result::Result<(), StorageError> {
            self.0.acquire(n, o)
        }
        fn release(&self, n: &str, o: &str) -> std::result::Result<(), StorageError> {
            self.0.release(n, o)
        }
    }
    let refused = Engine::new(NoHistory(MemoryStorage::default()), "ns", "w")
        .err()
        .unwrap();
    assert!(matches!(refused, Error::Refused(m) if m.contains("history")));
}
