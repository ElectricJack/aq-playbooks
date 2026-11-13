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
fn refused_as_also_live<T>(result: std::result::Result<T, StorageError>) -> bool {
    matches!(result, Err(StorageError::Backend(m)) if m == history::ALSO_LIVE)
}
fn live_receipt(sequence: u64) -> Receipt {
    Receipt {
        sequence,
        run: "live".into(),
        step: "s".into(),
        at_ms: 1,
        kind: "admitted".into(),
        detail: json!({}),
    }
}
/// A commit may not leave an item key both live and in history: neither by
/// settling an item the stored State still holds, nor by storing a live item
/// whose key is already in history.
fn refuses_live_and_history_conflicts(store: &impl Storage) {
    store.acquire("ns", "w").unwrap();
    // Settled while still live: a run, and a receipt.
    let mut state = State::default();
    let cohort = synthetic_cohort("a", 10, &[1]);
    state.runs.insert("a".into(), cohort.runs[0].clone());
    state.settled.push(cohort);
    assert!(refused_as_also_live(
        store.compare_and_swap("ns", "w", 0, &state)
    ));
    let mut state = State::default();
    state.receipts.push(live_receipt(7));
    state.settled.push(synthetic_cohort("r", 10, &[7]));
    assert!(refused_as_also_live(
        store.compare_and_swap("ns", "w", 0, &state)
    ));
    assert!(page(store, InspectionKind::Runs, None, 10).items.is_empty());
    assert!(
        page(store, InspectionKind::Receipts, None, 10)
            .items
            .is_empty()
    );
    // Live again after it is in history: a run, and a receipt.
    let revision = store
        .compare_and_swap(
            "ns",
            "w",
            0,
            &State {
                settled: vec![synthetic_cohort("b", 10, &[2])],
                ..State::default()
            },
        )
        .unwrap();
    let mut state = State::default();
    state
        .runs
        .insert("b".into(), synthetic_cohort("b", 10, &[]).runs[0].clone());
    assert!(refused_as_also_live(
        store.compare_and_swap("ns", "w", revision, &state)
    ));
    let mut state = State::default();
    state.receipts.push(live_receipt(2));
    assert!(refused_as_also_live(
        store.compare_and_swap("ns", "w", revision, &state)
    ));
    assert_eq!(store.load("ns").unwrap().revision, revision);
}
#[test]
fn memory_storage_refuses_live_and_history_conflicts() {
    refuses_live_and_history_conflicts(&MemoryStorage::default());
}
#[test]
fn record_storage_refuses_live_and_history_conflicts() {
    let store = RecordStorage::new();
    refuses_live_and_history_conflicts(&store);
    // The same through bounded transitions.
    store.acquire("t", "w").unwrap();
    let mut initialized = State::default();
    initialized.indexes.initialized = true;
    let mut revision = store.compare_and_swap("t", "w", 0, &initialized).unwrap();
    let transition = |cohort: Option<SettledCohort>, live: Option<Run>| {
        let (mut cohort, mut live) = (cohort, live);
        let mut plan = |s: &mut State| {
            s.settled.extend(cohort.take());
            if let Some(run) = live.take() {
                s.runs.insert(run.id.clone(), run);
            }
            s.sequence += 1;
            Ok(true)
        };
        let validate = |_: &State, _: &QuotaCounts| Ok(());
        store.transition_v1(
            "t",
            "w",
            &mut TransitionRequest {
                operation: "history",
                plan: &mut plan,
                validate: &validate,
            },
        )
    };
    let also_live = |result: Result<TransitionOutcome>| matches!(result, Err(Error::Storage(StorageError::Backend(m))) if m == history::ALSO_LIVE);
    let c = synthetic_cohort("c", 10, &[]);
    assert!(also_live(transition(
        Some(c.clone()),
        Some(c.runs[0].clone())
    )));
    assert!(matches!(
        transition(Some(synthetic_cohort("d", 10, &[])), None).unwrap(),
        TransitionOutcome::Committed { .. }
    ));
    revision += 1;
    let d = synthetic_cohort("d", 10, &[]).runs[0].clone();
    assert!(also_live(transition(None, Some(d))));
    assert_eq!(store.load("t").unwrap().revision, revision);
}

// --- Task 4: retention clock, window fields and removal fences ------------

fn receipt(key: &str, accepted_at_ms: i64) -> KeyedAdmissionReceipt {
    KeyedAdmissionReceipt {
        key: key.into(),
        request_digest: format!("digest-{key}"),
        run_id: format!("run-{key}"),
        activation_revision: 1,
        catalog_revision: 1,
        accepted_at_ms,
    }
}
/// A terminal root run `id` with no children.
fn completed_run(id: &str) -> Run {
    synthetic_cohort(id, 1, &[]).runs.remove(0)
}
fn intent(id: &str, run: &str, status: IntentStatus) -> Intent {
    Intent {
        id: id.into(),
        run: run.into(),
        step: "invoke".into(),
        invocation: 1,
        deadline_at_ms: 100,
        command: command(Recovery::NaturallyIdempotent),
        input: Value::Null,
        input_digest: format!("digest-{id}"),
        authority: authority(),
        status,
        cancel_requested: false,
        cancel_dispatched: false,
        handle: None,
        result: None,
        reason: None,
        late_results: vec![],
        completion_event: None,
    }
}
fn accepted_event(key: &str, accepted_at_ms: i64) -> AcceptedEvent {
    let envelope = event(key, 2);
    AcceptedEvent {
        key: key.into(),
        digest: format!("digest-{key}"),
        envelope,
        accepted_at_ms,
        sequence: 1,
        catalog_revision: 1,
        destinations: vec![],
    }
}
fn refused_with<T>(result: std::result::Result<T, StorageError>, text: &str) -> bool {
    matches!(result, Err(StorageError::Backend(m)) if m == text)
}

#[test]
fn legacy_retention_state_and_policy_decode_with_defaults() {
    let policy: RetentionPolicy = serde_json::from_value(
        json!({"event_replay_ms":1,"terminal_run_ms":1,"audit_ms":1,"max_tombstones":1}),
    )
    .unwrap();
    assert_eq!(policy.admission_replay_ms, 604_800_000);
    assert_eq!(RetentionPolicy::default().admission_replay_ms, 604_800_000);
    let mut legacy = serde_json::to_value(State::default()).unwrap();
    for f in ["clock_ms", "expiry", "admission_expires_at_ms"] {
        legacy["retention"].as_object_mut().unwrap().remove(f);
    }
    let state: State = serde_json::from_value(legacy).unwrap();
    assert!(state.retention.clock_ms.is_none() && state.retention.expiry.is_empty());
}
/// `tests/fixtures/legacy_state_5046282.json` was written by the engine at
/// 5046282 (before the retention clock): a namespace after a GC, holding a
/// keyed admission receipt, a live run, a live event, event tombstones and
/// disposals. It is the format 2 snapshot (the State's JSON) and the format 3
/// representation (header plus records) of the same State.
#[test]
fn legacy_fixture_from_5046282_decodes_on_format_2_and_format_3() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/legacy_state_5046282.json")).unwrap();
    let snapshot = &fixture["format2_snapshot"];
    for absent in ["clock_ms", "expiry", "admission_expires_at_ms"] {
        assert!(snapshot["retention"].get(absent).is_none(), "{absent}");
    }
    assert!(
        snapshot["retention"]["policy"]
            .get("admission_replay_ms")
            .is_none()
    );
    let f2: State = serde_json::from_value(snapshot.clone()).unwrap();
    let header = fixture["format3_header"].as_str().unwrap().as_bytes();
    let records: Vec<(String, lazy::TreeKey, Vec<u8>)> = fixture["format3_records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r[0].as_str().unwrap().to_string(),
                serde_json::from_value(r[1].clone()).unwrap(),
                r[2].as_str().unwrap().as_bytes().to_vec(),
            )
        })
        .collect();
    let f3 = transition::assemble_state(header, |field| {
        Ok(records
            .iter()
            .filter(|(f, _, _)| f == field)
            .map(|(_, k, v)| (k.clone(), v.clone()))
            .collect())
    })
    .unwrap();
    for state in [&f2, &f3] {
        let r = &state.retention;
        assert!(
            r.clock_ms.is_none() && r.expiry.is_empty() && r.admission_expires_at_ms.is_empty()
        );
        assert_eq!(r.policy.admission_replay_ms, 604_800_000);
        assert_eq!(state.keyed_admissions["legacy-key"].accepted_at_ms, 10);
        // A legacy receipt expires by acceptance plus the default window.
        assert_eq!(
            retention::admission_expiry(state, "legacy-key"),
            Some(10 + 604_800_000)
        );
        let (key, event) = state.events.iter().next().unwrap();
        assert_eq!(
            retention::event_expiry(state, key),
            Some(
                r.event_expires_at_ms
                    .get(key)
                    .copied()
                    .unwrap_or(event.accepted_at_ms + 86_400_000)
            )
        );
        assert!(!r.event_tombstones.is_empty() && !r.disposals.is_empty());
    }
    assert_eq!(
        serde_json::to_value(&f2).unwrap(),
        serde_json::to_value(&f3).unwrap()
    );
    // The decoded State commits on both in-memory backends, and the legacy
    // receipt leaves only once the clock passes its window.
    for store in [
        &MemoryStorage::default() as &dyn Storage,
        &RecordStorage::new(),
    ] {
        store.acquire("legacy", "w").unwrap();
        let r = store.compare_and_swap("legacy", "w", 0, &f2).unwrap();
        let mut next = store.load("legacy").unwrap().state;
        next.keyed_admissions.remove("legacy-key");
        next.retention.clock_ms = Some(10 + 604_800_000 - 1);
        assert!(refused_with(
            store.compare_and_swap("legacy", "w", r, &next),
            retention::REFUSE_EARLY_REMOVAL
        ));
        next.retention.clock_ms = Some(10 + 604_800_000);
        store.compare_and_swap("legacy", "w", r, &next).unwrap();
    }
}
fn removal_fences_hold<S: Storage>(store: S) {
    store.acquire("ns", "w").unwrap();
    let mut s = State::default();
    s.retention.clock_ms = Some(500);
    s.keyed_admissions.insert("k".into(), receipt("k", 0));
    s.retention
        .admission_expires_at_ms
        .insert("k".into(), 1_000);
    s.runs.insert("run-a".into(), completed_run("run-a"));
    let r = store.compare_and_swap("ns", "w", 0, &s).unwrap();
    let mut next = s.clone();
    next.keyed_admissions.remove("k");
    next.retention.clock_ms = Some(999);
    assert!(
        matches!(store.compare_and_swap("ns", "w", r, &next), Err(StorageError::Backend(m)) if m == retention::REFUSE_EARLY_REMOVAL)
    );
    next.retention.clock_ms = Some(1_000);
    let r = store.compare_and_swap("ns", "w", r, &next).unwrap();
    let mut gone = next.clone();
    gone.runs.remove("run-a");
    assert!(
        matches!(store.compare_and_swap("ns", "w", r, &gone), Err(StorageError::Backend(m)) if m == retention::REFUSE_RUN_REMOVAL)
    );
    gone.settled.push(SettledCohort {
        root: "run-a".into(),
        finished_at_ms: 1,
        runs: vec![completed_run("run-a")],
        intents: vec![],
        receipts: vec![],
    });
    let r = store.compare_and_swap("ns", "w", r, &gone).unwrap();
    let mut back = gone.clone();
    back.settled.clear();
    back.retention.clock_ms = Some(10);
    assert!(
        matches!(store.compare_and_swap("ns", "w", r, &back), Err(StorageError::Backend(m)) if m == retention::REFUSE_CLOCK)
    );
}
#[test]
fn memory_storage_removal_fences() {
    removal_fences_hold(MemoryStorage::default())
}
#[test]
fn record_storage_removal_fences() {
    removal_fences_hold(RecordStorage::new())
}
#[test]
fn commits_advance_the_clock_only_with_host_time() {
    let e = engine();
    install(
        &e,
        artifact(
            "a",
            "done",
            BTreeMap::from([("done".into(), ret("ok"))]),
            Registry::default(),
            BTreeMap::new(),
        ),
        "a",
    );
    e.admit("a", "main", Value::Null, authority(), 5_000)
        .unwrap();
    e.admit("a", "main", Value::Null, authority(), 10).unwrap();
    assert_eq!(e.snapshot().unwrap().retention.clock_ms, Some(5_000));
}
/// Commits without a host time leave the clock where it is.
#[test]
fn commits_without_host_time_do_not_move_the_clock() {
    let e = engine();
    let a = artifact(
        "a",
        "done",
        BTreeMap::from([("done".into(), ret("ok"))]),
        Registry::default(),
        BTreeMap::new(),
    );
    let digest = a.digest.clone();
    e.install(a).unwrap();
    e.activate("a", &digest, authority()).unwrap();
    e.set_retention_policy(RetentionPolicy::default()).unwrap();
    assert_eq!(e.snapshot().unwrap().retention.clock_ms, None);
    e.admit("a", "main", Value::Null, authority(), 7).unwrap();
    e.activate("b", &digest, authority()).unwrap();
    assert_eq!(e.snapshot().unwrap().retention.clock_ms, Some(7));
}
/// The fence stays closed for every removal the rule does not allow: no
/// history record in the commit, no expired window entry.
fn removals_without_history_or_expiry_are_refused(store: &dyn Storage) {
    store.acquire("refuse", "w").unwrap();
    let mut s = State::default();
    s.retention.clock_ms = Some(500);
    s.runs.insert("run-a".into(), completed_run("run-a"));
    s.runs.insert("run-b".into(), {
        let mut run = completed_run("run-b");
        run.status = RunStatus::Running;
        run
    });
    s.intents.insert(
        "op-a".into(),
        intent("op-a", "run-a", IntentStatus::Completed),
    );
    s.keyed_admissions.insert("k".into(), receipt("k", 0));
    s.keyed_admissions
        .insert("fallback".into(), receipt("fallback", 0));
    s.retention
        .admission_expires_at_ms
        .insert("k".into(), 1_000);
    s.events.insert("ev".into(), accepted_event("ev", 0));
    s.retention.event_expires_at_ms.insert("ev".into(), 1_000);
    s.events
        .insert("ev-fallback".into(), accepted_event("ev-fallback", 0));
    s.retention.policy.admission_replay_ms = 2_000;
    s.retention.policy.event_replay_ms = 3_000;
    let base = store.compare_and_swap("refuse", "w", 0, &s).unwrap();
    let cas = |next: &State| store.compare_and_swap("refuse", "w", base, next);
    let early = retention::REFUSE_EARLY_REMOVAL;
    let mut next = s.clone();
    next.intents.remove("op-a");
    assert!(
        refused_with(cas(&next), retention::REFUSE_RUN_REMOVAL),
        "intent"
    );
    let mut next = s.clone();
    next.retention.clock_ms = Some(999);
    next.keyed_admissions.remove("k");
    assert!(refused_with(cas(&next), early), "keyed admission");
    let mut next = s.clone();
    next.retention.clock_ms = Some(1_999);
    next.keyed_admissions.remove("fallback");
    assert!(
        refused_with(cas(&next), early),
        "keyed admission, policy window"
    );
    let mut next = s.clone();
    next.retention.clock_ms = Some(999);
    next.events.remove("ev");
    assert!(refused_with(cas(&next), early), "event");
    let mut next = s.clone();
    next.retention.clock_ms = Some(2_999);
    next.events.remove("ev-fallback");
    assert!(refused_with(cas(&next), early), "event, policy window");
    let mut next = s.clone();
    next.keyed_admissions.get_mut("k").unwrap().run_id = "other".into();
    next.retention.clock_ms = Some(5_000);
    assert!(
        refused_with(
            cas(&next),
            "immutable keyed admission receipt removed or changed"
        ),
        "changed receipt"
    );
    // History in the commit does not cover a run that is not finished, or
    // one whose operation is unsettled.
    let mut next = s.clone();
    let running = next.runs.remove("run-b").unwrap();
    next.settled.push(SettledCohort {
        root: "run-b".into(),
        finished_at_ms: 1,
        runs: vec![running],
        intents: vec![],
        receipts: vec![],
    });
    assert!(
        refused_with(cas(&next), "unresolved run obligations cannot be disposed"),
        "running"
    );
    let mut next = s.clone();
    next.runs.remove("run-a");
    next.intents.remove("op-a");
    next.settled.push(SettledCohort {
        root: "run-a".into(),
        finished_at_ms: 1,
        runs: vec![completed_run("run-a")],
        intents: vec![intent("op-a", "run-a", IntentStatus::Accepted)],
        receipts: vec![],
    });
    assert!(cas(&next).is_err(), "unsettled operation");
    let mut next = s.clone();
    next.runs.remove("run-a");
    next.settled.push(SettledCohort {
        root: "run-a".into(),
        finished_at_ms: 1,
        runs: vec![completed_run("run-a")],
        intents: vec![],
        receipts: vec![],
    });
    next.intents.get_mut("op-a").unwrap().status = IntentStatus::Accepted;
    assert!(
        cas(&next).is_err(),
        "unsettled live operation of a settled run"
    );
    // An operation of a settled run leaves only with its own history item.
    let mut next = s.clone();
    next.runs.remove("run-a");
    next.intents.remove("op-a");
    next.settled.push(SettledCohort {
        root: "run-a".into(),
        finished_at_ms: 1,
        runs: vec![completed_run("run-a")],
        intents: vec![],
        receipts: vec![],
    });
    assert!(
        refused_with(cas(&next), retention::REFUSE_RUN_REMOVAL),
        "operation without history"
    );
    // Nothing was written by any refusal.
    assert_eq!(store.load("refuse").unwrap().revision, base);
    // Allowed: expired entries, history for run and operation, and the
    // legacy disposal records; tombstones and fences always.
    let mut next = s.clone();
    next.retention.clock_ms = Some(3_000);
    for key in ["k", "fallback"] {
        next.keyed_admissions.remove(key);
    }
    for key in ["ev", "ev-fallback"] {
        next.events.remove(key);
    }
    next.runs.remove("run-a");
    next.intents.remove("op-a");
    next.settled.push(SettledCohort {
        root: "run-a".into(),
        finished_at_ms: 1,
        runs: vec![completed_run("run-a")],
        intents: vec![intent("op-a", "run-a", IntentStatus::Completed)],
        receipts: vec![],
    });
    store.compare_and_swap("refuse", "w", base, &next).unwrap();
}
#[test]
fn memory_storage_refuses_removals_without_history_or_expiry() {
    removals_without_history_or_expiry_are_refused(&MemoryStorage::default());
}
#[test]
fn record_storage_refuses_removals_without_history_or_expiry() {
    removals_without_history_or_expiry_are_refused(&RecordStorage::new());
}
/// Legacy removals (a disposal or a matching tombstone, written by GC before
/// this change) are still allowed, and in-State tombstones and fences may
/// always be removed.
fn legacy_removals_are_allowed(store: &dyn Storage) {
    store.acquire("legacy", "w").unwrap();
    let mut s = State::default();
    s.runs.insert("run-a".into(), completed_run("run-a"));
    s.events.insert("ev".into(), accepted_event("ev", 0));
    s.retention
        .event_expires_at_ms
        .insert("ev".into(), 1_000_000);
    s.retention
        .expired_correlations
        .insert("c".into(), ("host".into(), 0));
    s.retention.event_tombstones.insert(
        "old".into(),
        retention::EventTombstone {
            producer: "host".into(),
            epoch: 0,
            digest: "d".into(),
            disposed_at_ms: 1,
        },
    );
    let r = store.compare_and_swap("legacy", "w", 0, &s).unwrap();
    let mut next = s.clone();
    next.runs.remove("run-a");
    next.events.remove("ev");
    next.retention.disposals.push(retention::Disposal {
        kind: "run".into(),
        key: "run-a".into(),
        at_ms: 5,
    });
    next.retention.event_tombstones.remove("old");
    next.retention.expired_correlations.remove("c");
    next.retention.event_tombstones.insert(
        "ev".into(),
        retention::EventTombstone {
            producer: "host".into(),
            epoch: 0,
            digest: "wrong".into(),
            disposed_at_ms: 5,
        },
    );
    assert!(store.compare_and_swap("legacy", "w", r, &next).is_err());
    next.retention
        .event_tombstones
        .get_mut("ev")
        .unwrap()
        .digest = "digest-ev".into();
    store.compare_and_swap("legacy", "w", r, &next).unwrap();
}
#[test]
fn memory_storage_allows_legacy_removals() {
    legacy_removals_are_allowed(&MemoryStorage::default());
}
#[test]
fn record_storage_allows_legacy_removals() {
    legacy_removals_are_allowed(&RecordStorage::new());
}
/// The same fences on `RecordStorage`'s bounded transitions: a removal with
/// no history and no expired entry is refused and writes nothing; an expired
/// window entry leaves incrementally.
#[test]
fn record_storage_transitions_hold_the_removal_fences() {
    let store = RecordStorage::new();
    store.acquire("t", "w").unwrap();
    let mut s = State::default();
    s.indexes.initialized = true;
    s.retention.clock_ms = Some(500);
    s.runs.insert("run-a".into(), completed_run("run-a"));
    s.intents.insert(
        "op-a".into(),
        intent("op-a", "run-a", IntentStatus::Completed),
    );
    s.keyed_admissions.insert("k".into(), receipt("k", 0));
    s.retention
        .admission_expires_at_ms
        .insert("k".into(), 1_000);
    s.events.insert("ev".into(), accepted_event("ev", 0));
    s.retention.event_expires_at_ms.insert("ev".into(), 1_000);
    let base = store.compare_and_swap("t", "w", 0, &s).unwrap();
    let transition = |f: &dyn Fn(&mut State)| {
        let mut plan = |s: &mut State| {
            f(s);
            Ok(true)
        };
        let validate = |_: &State, _: &QuotaCounts| Ok(());
        store
            .transition_v1(
                "t",
                "w",
                &mut TransitionRequest {
                    operation: "fence",
                    plan: &mut plan,
                    validate: &validate,
                },
            )
            .map_err(|e| match e {
                Error::Storage(e) => e,
                other => StorageError::Backend(other.to_string()),
            })
    };
    let early = retention::REFUSE_EARLY_REMOVAL;
    assert!(refused_with(
        transition(&|s| {
            s.runs.remove("run-a");
        }),
        retention::REFUSE_RUN_REMOVAL
    ));
    assert!(refused_with(
        transition(&|s| {
            s.intents.remove("op-a");
        }),
        retention::REFUSE_RUN_REMOVAL
    ));
    assert!(refused_with(
        transition(&|s| {
            s.retention.clock_ms = Some(999);
            s.keyed_admissions.remove("k");
            s.retention.admission_expires_at_ms.remove("k");
        }),
        early
    ));
    assert!(refused_with(
        transition(&|s| {
            s.retention.clock_ms = Some(999);
            s.events.remove("ev");
            s.retention.event_expires_at_ms.remove("ev");
        }),
        early
    ));
    assert!(refused_with(
        transition(&|s| {
            s.keyed_admissions.get_mut("k").unwrap().run_id = "other".into();
        }),
        "immutable keyed admission receipt removed or changed"
    ));
    assert!(refused_with(
        transition(&|s| {
            s.retention.clock_ms = Some(499);
        }),
        retention::REFUSE_CLOCK
    ));
    assert_eq!(store.load("t").unwrap().revision, base);
    // Expired: both entries leave in one bounded transition.
    transition(&|s| {
        s.retention.clock_ms = Some(1_000);
        s.keyed_admissions.remove("k");
        s.retention.admission_expires_at_ms.remove("k");
        s.events.remove("ev");
        s.retention.event_expires_at_ms.remove("ev");
    })
    .unwrap();
    let state = store.load("t").unwrap().state;
    assert!(state.keyed_admissions.is_empty() && state.events.is_empty());
    // With history for the run and its operation, the cohort leaves.
    transition(&|s| {
        s.runs.remove("run-a");
        s.intents.remove("op-a");
        s.settled.push(SettledCohort {
            root: "run-a".into(),
            finished_at_ms: 1,
            runs: vec![completed_run("run-a")],
            intents: vec![intent("op-a", "run-a", IntentStatus::Completed)],
            receipts: vec![],
        });
    })
    .unwrap();
    assert!(store.load("t").unwrap().state.runs.is_empty());
}
/// `check_not_live` on a State whose runtime indexes are initialised (the
/// branch every engine commit takes): a receipt is live when the receipt
/// index points at a receipt with that sequence.
#[test]
fn check_not_live_reads_receipts_through_the_initialised_index() {
    let mut state = State::default();
    state.indexes.initialized = true;
    state.receipts.push(live_receipt(7));
    state.receipts.push(live_receipt(9));
    state.indexes.all_receipts.insert(7, 0);
    state.indexes.all_receipts.insert(9, 1);
    let items = |sequence: u64| synthetic_cohort("r", 10, &[sequence]).history_items();
    let receipts_only = |sequence: u64| {
        items(sequence)
            .into_iter()
            .filter(|i| i.kind == InspectionKind::Receipts)
            .collect::<Vec<_>>()
    };
    assert!(refused_as_also_live(history::check_not_live(
        &state,
        &receipts_only(7)
    )));
    assert!(refused_as_also_live(history::check_not_live(
        &state,
        &receipts_only(9)
    )));
    assert!(history::check_not_live(&state, &receipts_only(8)).is_ok());
    // An index entry that points at a receipt with another sequence is not
    // taken as live (it is re-checked against the receipt).
    state.indexes.all_receipts.insert(8, 1);
    assert!(history::check_not_live(&state, &receipts_only(8)).is_ok());
    // Runs and operations are map lookups on either branch.
    state.runs.insert("r".into(), completed_run("r"));
    assert!(refused_as_also_live(history::check_not_live(
        &state,
        &items(8)
    )));
}
