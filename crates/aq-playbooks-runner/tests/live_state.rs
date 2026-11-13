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
        let validate = |_: &State, _: &QuotaCounts, _: &QuotaCounts| Ok(());
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
    // Round 1 (I2): a recorded expiry is at least acceptance plus the policy
    // window, so the brief's 1_000 needs a 1_000 window.
    s.retention.policy.admission_replay_ms = 1_000;
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
fn audit(sequence: u64, run: &str) -> Receipt {
    Receipt {
        sequence,
        run: run.into(),
        step: "s".into(),
        at_ms: 1,
        kind: "terminal".into(),
        detail: json!({}),
    }
}
fn cohort(run: Run, intents: Vec<Intent>, receipts: Vec<Receipt>) -> SettledCohort {
    SettledCohort {
        root: run.root.clone(),
        finished_at_ms: 1,
        runs: vec![run],
        intents,
        receipts,
    }
}
/// The window and history state shared by the refusal tests. Policy
/// windows: receipts 2_000, events 3_000; recorded expiries "k" 3_000 and
/// "ev" 4_000; "fallback" and "ev-fallback" have none.
fn fence_state() -> State {
    let mut s = State::default();
    s.retention.clock_ms = Some(500);
    s.retention.policy.admission_replay_ms = 2_000;
    s.retention.policy.event_replay_ms = 3_000;
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
    s.receipts.push(audit(1, "run-a"));
    s.receipts.push(audit(2, "run-b"));
    s.sequence = 2;
    s.keyed_admissions.insert("k".into(), receipt("k", 0));
    s.keyed_admissions
        .insert("fallback".into(), receipt("fallback", 0));
    s.retention
        .admission_expires_at_ms
        .insert("k".into(), 3_000);
    s.events.insert("ev".into(), accepted_event("ev", 0));
    s.retention.event_expires_at_ms.insert("ev".into(), 4_000);
    s.events
        .insert("ev-fallback".into(), accepted_event("ev-fallback", 0));
    // Round 2 (C1): an installed contract whose window (5_000) is longer
    // than the policy's, and an event of it expiring at 9_000.
    let long = long_artifact();
    s.artifacts.insert(long.digest.clone(), long);
    s.events.insert("ev-long".into(), {
        let mut e = accepted_event("ev-long", 0);
        e.envelope.contract = ContractRef {
            name: long_contract().name,
            fingerprint: long_contract().fingerprint().unwrap(),
        };
        e
    });
    s.retention
        .event_expires_at_ms
        .insert("ev-long".into(), 9_000);
    s
}
fn long_contract() -> EventContract {
    EventContract {
        name: "test.long".into(),
        retention_ms: 5_000,
        ..event_contract()
    }
}
fn long_artifact() -> Artifact {
    let c = long_contract();
    artifact(
        "long",
        "done",
        BTreeMap::from([("done".into(), ret("ok"))]),
        Registry {
            commands: BTreeMap::new(),
            events: BTreeMap::from([(c.name.clone(), c)]),
        },
        BTreeMap::new(),
    )
}
fn tombstone(digest: &str) -> retention::EventTombstone {
    retention::EventTombstone {
        producer: "host".into(),
        epoch: 0,
        digest: digest.into(),
        disposed_at_ms: 5,
    }
}
/// An admin licence receipt of `kind` from `from_ms` to `to_ms`.
fn licence_receipt(kind: &str, sequence: u64, from_ms: i64, to_ms: i64) -> Receipt {
    Receipt {
        sequence,
        run: "admin".into(),
        step: "retention".into(),
        at_ms: to_ms,
        kind: kind.into(),
        detail: json!({"from_ms": from_ms, "to_ms": to_ms}),
    }
}
/// That receipt as history written by the commit.
fn licence_cohort(receipts: Vec<Receipt>) -> SettledCohort {
    SettledCohort {
        root: "admin".into(),
        finished_at_ms: 1,
        runs: vec![],
        intents: vec![],
        receipts,
    }
}
/// Write a licence receipt of `kind` as the commit's one new sequence
/// number, as the engine does.
fn license(n: &mut State, kind: &str, from_ms: i64, to_ms: i64) {
    n.sequence += 1;
    n.settled.push(licence_cohort(vec![licence_receipt(
        kind, n.sequence, from_ms, to_ms,
    )]));
}
/// The commit a rebase from 500 to 100 makes on `fence_state()`: clock 100,
/// each expiry capped at 100 plus its own window ("ev-long" by its
/// contract's 5_000), a stale floor.
fn rebased(n: &mut State) {
    n.retention.clock_ms = Some(100);
    n.retention
        .admission_expires_at_ms
        .insert("k".into(), 2_100);
    n.retention.event_expires_at_ms.insert("ev".into(), 3_100);
    n.retention
        .event_expires_at_ms
        .insert("ev-long".into(), 5_100);
    n.retention.stale_floor_ms = Some(50);
    license(n, retention::CLOCK_REBASE, 500, 100);
}
/// Rulings 1 and 2 (round 2): a commit with a licence receipt must be
/// exactly that licence and do nothing else; without one, the clock never
/// decreases and the floor never changes. Cases on `fence_state()` (clock
/// 500; windows 2_000 and 3_000; "k" 3_000, "ev" 4_000, "ev-long" 9_000
/// with a 5_000 contract window), each (label, refusal, change).
fn rebase_refusal_cases() -> Vec<Case> {
    let shortened = retention::REFUSE_EXPIRY;
    let licence = retention::REFUSE_LICENCE;
    let floor = retention::REFUSE_STALE_FLOOR;
    vec![
        (
            "clock lowered without a rebase",
            retention::REFUSE_CLOCK,
            Box::new(|n| n.retention.clock_ms = Some(100)),
        ),
        (
            "clock lowered, rebase receipt left live",
            retention::REFUSE_CLOCK,
            Box::new(|n| {
                n.retention.clock_ms = Some(100);
                n.sequence += 1;
                n.receipts.push(licence_receipt(
                    retention::CLOCK_REBASE,
                    n.sequence,
                    500,
                    100,
                ));
            }),
        ),
        (
            "clock lowered, rebase from another clock",
            licence,
            Box::new(|n| {
                n.retention.clock_ms = Some(100);
                license(n, retention::CLOCK_REBASE, 600, 100);
            }),
        ),
        (
            "clock lowered, rebase to another clock",
            licence,
            Box::new(|n| {
                n.retention.clock_ms = Some(100);
                license(n, retention::CLOCK_REBASE, 500, 200);
            }),
        ),
        (
            "review probe: a receipt from 7 to 500, the clock left at 500, expiries lowered",
            licence,
            Box::new(|n| {
                license(n, retention::CLOCK_REBASE, 7, 500);
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_500);
                n.retention.event_expires_at_ms.insert("ev".into(), 3_500);
            }),
        ),
        (
            "rebase upwards",
            licence,
            Box::new(|n| {
                n.retention.clock_ms = Some(600);
                license(n, retention::CLOCK_REBASE, 500, 600);
            }),
        ),
        (
            "replayed rebase receipt (not this commit's sequence)",
            licence,
            Box::new(|n| {
                rebased(n);
                let replayed = n.sequence - 1;
                n.settled[0].receipts[0].sequence = replayed;
            }),
        ),
        (
            "rebase that also admits",
            licence,
            Box::new(|n| {
                rebased(n);
                n.sequence += 1;
                n.keyed_admissions.insert("new".into(), receipt("new", 100));
            }),
        ),
        (
            "rebase that also accepts an event",
            licence,
            Box::new(|n| {
                rebased(n);
                n.events
                    .insert("ev-new".into(), accepted_event("ev-new", 100));
            }),
        ),
        (
            "rebase that also settles a run",
            licence,
            Box::new(|n| {
                rebased(n);
                let run = n.runs.remove("run-a").unwrap();
                let op = n.intents.remove("op-a").unwrap();
                n.receipts.retain(|r| r.sequence != 1);
                n.settled
                    .push(cohort(run, vec![op], vec![audit(1, "run-a")]));
            }),
        ),
        (
            "two licences in one commit",
            licence,
            Box::new(|n| {
                rebased(n);
                n.settled[0].receipts.push(licence_receipt(
                    retention::STALE_FLOOR_LOWERED,
                    77,
                    50,
                    10,
                ));
            }),
        ),
        (
            "expiry lowered without a rebase",
            shortened,
            Box::new(|n| {
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_100);
            }),
        ),
        (
            "expiry lowered with a rebase the clock did not take",
            licence,
            Box::new(|n| {
                license(n, retention::CLOCK_REBASE, 500, 100);
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_100);
            }),
        ),
        (
            "expiry capped below its cap",
            shortened,
            Box::new(|n| {
                rebased(n);
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_099);
            }),
        ),
        (
            "expiry lowered, but not to its cap",
            shortened,
            Box::new(|n| {
                rebased(n);
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_200);
            }),
        ),
        (
            "event expiry capped below its cap",
            shortened,
            Box::new(|n| {
                rebased(n);
                n.retention.event_expires_at_ms.insert("ev".into(), 3_099);
            }),
        ),
        (
            "event capped to the policy window, its contract window is longer",
            shortened,
            Box::new(|n| {
                rebased(n);
                n.retention
                    .event_expires_at_ms
                    .insert("ev-long".into(), 3_100);
            }),
        ),
        (
            "floor raised without a licence",
            floor,
            Box::new(|n| n.retention.stale_floor_ms = Some(50)),
        ),
        (
            "review probe C2: a rebase that also sets both windows to 1 and caps to 101",
            licence,
            Box::new(|n| {
                rebased(n);
                n.retention.policy.admission_replay_ms = 1;
                n.retention.policy.event_replay_ms = 1;
                n.retention.admission_expires_at_ms.insert("k".into(), 101);
                n.retention.event_expires_at_ms.insert("ev".into(), 101);
            }),
        ),
        (
            "rebase that also changes the policy",
            licence,
            Box::new(|n| {
                rebased(n);
                n.retention.policy.audit_ms += 1;
            }),
        ),
        (
            "rebase that also installs an artifact",
            licence,
            Box::new(|n| {
                rebased(n);
                let extra = n.artifacts.values().next().unwrap().clone();
                let mut content = extra.content.clone();
                content.compiler = "another".into();
                let a = Artifact::seal(content).unwrap();
                n.artifacts.insert(a.digest.clone(), a);
            }),
        ),
        (
            "rebase that also changes a producer epoch",
            licence,
            Box::new(|n| {
                rebased(n);
                n.retention.producer_epochs.insert("host".into(), 1);
            }),
        ),
        (
            "rebase that also moves the catalog",
            licence,
            Box::new(|n| {
                rebased(n);
                n.catalog_revision += 1;
            }),
        ),
        (
            "rebase that also changes the catalog history",
            licence,
            Box::new(|n| {
                rebased(n);
                n.catalog_history_at_ms.insert(99, 1);
            }),
        ),
        (
            "rebase that also changes an event's contract to shrink its cap",
            shortened,
            Box::new(|n| {
                rebased(n);
                let mut e = n.events.get("ev-long").unwrap().clone();
                e.envelope.contract.name = "test.other".into();
                n.events.insert("ev-long".into(), e);
                n.retention
                    .event_expires_at_ms
                    .insert("ev-long".into(), 3_100);
            }),
        ),
        (
            "rebase raises the floor past its target",
            floor,
            Box::new(|n| {
                rebased(n);
                n.retention.stale_floor_ms = Some(101);
            }),
        ),
    ]
}
/// After that commit no licence is left, and the floor changes only with
/// its own exact receipt. Changes of the stored state after the rebase
/// (clock 100, floor 50).
fn after_rebase_refusal_cases() -> Vec<Case> {
    let licence = retention::REFUSE_LICENCE;
    let floor = retention::REFUSE_STALE_FLOOR;
    vec![
        (
            "clock lowered again",
            retention::REFUSE_CLOCK,
            Box::new(|n| n.retention.clock_ms = Some(90)),
        ),
        (
            "stale floor lowered without a licence",
            floor,
            Box::new(|n| n.retention.stale_floor_ms = Some(40)),
        ),
        (
            "stale floor raised without a licence",
            floor,
            Box::new(|n| n.retention.stale_floor_ms = Some(60)),
        ),
        (
            "stale floor removed",
            floor,
            Box::new(|n| n.retention.stale_floor_ms = None),
        ),
        (
            "floor lowered to another value than its receipt",
            licence,
            Box::new(|n| {
                n.retention.stale_floor_ms = Some(30);
                license(n, retention::STALE_FLOOR_LOWERED, 50, 40);
            }),
        ),
        (
            "floor lowering from another floor",
            licence,
            Box::new(|n| {
                n.retention.stale_floor_ms = Some(40);
                license(n, retention::STALE_FLOOR_LOWERED, 60, 40);
            }),
        ),
        (
            "floor lowering that also moves the clock",
            licence,
            Box::new(|n| {
                n.retention.stale_floor_ms = Some(40);
                n.retention.clock_ms = Some(200);
                license(n, retention::STALE_FLOOR_LOWERED, 50, 40);
            }),
        ),
        (
            "floor lowering that also changes the policy",
            licence,
            Box::new(|n| {
                n.retention.policy.admission_replay_ms = 1;
                floor_lowered(n);
            }),
        ),
        (
            "floor lowering that also changes an expiry",
            retention::REFUSE_EXPIRY,
            Box::new(|n| {
                n.retention.stale_floor_ms = Some(40);
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 9_000);
                license(n, retention::STALE_FLOOR_LOWERED, 50, 40);
            }),
        ),
    ]
}
/// The floor lowering the fence accepts after `rebased`: 50 to 40.
fn floor_lowered(n: &mut State) {
    n.retention.stale_floor_ms = Some(40);
    license(n, retention::STALE_FLOOR_LOWERED, 50, 40);
}
fn rebase_fences_hold_on_cas(store: &dyn Storage) {
    let s = fence_state();
    let count = std::cell::Cell::new(0);
    let fresh = |state: &State| {
        count.set(count.get() + 1);
        let ns = format!("rebase-{}", count.get());
        store.acquire(&ns, "w").unwrap();
        let base = store.compare_and_swap(&ns, "w", 0, state).unwrap();
        (ns, base)
    };
    let mut failures = vec![];
    for (label, text, f) in rebase_refusal_cases() {
        let (ns, base) = fresh(&s);
        let mut next = s.clone();
        f(&mut next);
        let result = store.compare_and_swap(&ns, "w", base, &next);
        if !refused_with(result.clone(), text) {
            failures.push(format!("{label}: {result:?}"));
        } else {
            assert_eq!(store.load(&ns).unwrap().revision, base, "{label}");
        }
    }
    let mut next = s.clone();
    rebased(&mut next);
    for (label, text, f) in after_rebase_refusal_cases() {
        let (ns, base) = fresh(&s);
        let r = store.compare_and_swap(&ns, "w", base, &next).unwrap();
        let mut later = store.load(&ns).unwrap().state;
        f(&mut later);
        let result = store.compare_and_swap(&ns, "w", r, &later);
        if !refused_with(result.clone(), text) {
            failures.push(format!("{label}: {result:?}"));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    // A rebase, then a floor lowering with its own receipt.
    let (ns, base) = fresh(&s);
    let r = store.compare_and_swap(&ns, "w", base, &next).unwrap();
    let mut later = store.load(&ns).unwrap().state;
    floor_lowered(&mut later);
    store.compare_and_swap(&ns, "w", r, &later).unwrap();
    let stored = store.load(&ns).unwrap().state.retention;
    assert_eq!(
        (stored.clock_ms, stored.stale_floor_ms),
        (Some(100), Some(40))
    );
}
#[test]
fn memory_storage_opens_the_clock_fences_only_for_a_rebase() {
    rebase_fences_hold_on_cas(&MemoryStorage::default());
    let store = MemoryStorage::default();
    store.acquire("ok", "w").unwrap();
    let s = fence_state();
    let base = store.compare_and_swap("ok", "w", 0, &s).unwrap();
    let mut next = s.clone();
    rebased(&mut next);
    store.compare_and_swap("ok", "w", base, &next).unwrap();
    let r = store.load("ok").unwrap().state.retention;
    assert_eq!(
        (
            r.clock_ms,
            r.stale_floor_ms,
            r.admission_expires_at_ms.get("k").copied()
        ),
        (Some(100), Some(50), Some(2_100))
    );
}
#[test]
fn record_storage_opens_the_clock_fences_only_for_a_rebase() {
    rebase_fences_hold_on_cas(&RecordStorage::new());
    let store = RecordStorage::new();
    store.acquire("ok", "w").unwrap();
    let s = fence_state();
    let base = store.compare_and_swap("ok", "w", 0, &s).unwrap();
    let mut next = s.clone();
    rebased(&mut next);
    store.compare_and_swap("ok", "w", base, &next).unwrap();
    assert_eq!(
        store.load("ok").unwrap().state.retention.clock_ms,
        Some(100)
    );
}
/// The fence stays closed for every removal the rule does not allow: no
/// history record in the commit, no expired window entry; and an expiry
/// record never moves earlier.
fn removals_without_history_or_expiry_are_refused(store: &dyn Storage) {
    let s = fence_state();
    // Each case commits on a namespace of its own that holds `s`.
    let count = std::cell::Cell::new(0);
    let fresh = || {
        count.set(count.get() + 1);
        let ns = format!("refuse-{}", count.get());
        store.acquire(&ns, "w").unwrap();
        let base = store.compare_and_swap(&ns, "w", 0, &s).unwrap();
        (ns, base)
    };
    let cas = |next: &State| {
        let (ns, base) = fresh();
        let result = store.compare_and_swap(&ns, "w", base, next);
        if result.is_err() {
            // Nothing was written by the refusal.
            assert_eq!(store.load(&ns).unwrap().revision, base);
        }
        result
    };
    let early = retention::REFUSE_EARLY_REMOVAL;
    let shortened = retention::REFUSE_EXPIRY;
    let obligations = "unresolved run obligations cannot be disposed";
    // Every case is tried; the failures are reported together.
    let failures = std::cell::RefCell::new(vec![]);
    let refused = |label: &str, text: &str, f: &dyn Fn(&mut State)| {
        let mut next = s.clone();
        f(&mut next);
        let result = cas(&next);
        if !refused_with(result.clone(), text) {
            failures.borrow_mut().push(format!("{label}: {result:?}"));
        }
    };
    refused("intent", retention::REFUSE_RUN_REMOVAL, &|n| {
        n.intents.remove("op-a");
    });
    refused("keyed admission", early, &|n| {
        n.retention.clock_ms = Some(2_999);
        n.keyed_admissions.remove("k");
        n.retention.admission_expires_at_ms.remove("k");
    });
    refused("keyed admission, policy window", early, &|n| {
        n.retention.clock_ms = Some(1_999);
        n.keyed_admissions.remove("fallback");
    });
    refused("event", early, &|n| {
        n.retention.clock_ms = Some(3_999);
        n.events.remove("ev");
        n.retention.event_expires_at_ms.remove("ev");
    });
    refused("event, policy window", early, &|n| {
        n.retention.clock_ms = Some(2_999);
        n.events.remove("ev-fallback");
    });
    refused("event with a forged legacy tombstone", early, &|n| {
        n.events.remove("ev");
        n.retention.event_expires_at_ms.remove("ev");
        n.retention
            .event_tombstones
            .insert("ev".into(), tombstone("digest-ev"));
    });
    refused(
        "changed receipt",
        "immutable keyed admission receipt removed or changed",
        &|n| {
            n.keyed_admissions.get_mut("k").unwrap().run_id = "other".into();
            n.retention.clock_ms = Some(5_000);
        },
    );
    // Audit receipts leave only with their history.
    refused(
        "receipt of a live run",
        retention::REFUSE_RUN_REMOVAL,
        &|n| {
            n.receipts.retain(|r| r.sequence != 2);
        },
    );
    refused(
        "receipt of a settled run without its history item",
        retention::REFUSE_RUN_REMOVAL,
        &|n| {
            n.receipts.clear();
            n.receipts.push(audit(2, "run-b"));
            let run = n.runs.remove("run-a").unwrap();
            let op = n.intents.remove("op-a").unwrap();
            n.settled.push(cohort(run, vec![op], vec![]));
        },
    );
    // Expiry records: never lowered, deleted only with their item, and a new
    // one is at least the fallback.
    refused("lowered expiry", shortened, &|n| {
        n.retention
            .admission_expires_at_ms
            .insert("k".into(), 2_500);
    });
    refused("lowered event expiry", shortened, &|n| {
        n.retention.event_expires_at_ms.insert("ev".into(), 3_999);
    });
    refused("expiry deleted, item kept", shortened, &|n| {
        n.retention.admission_expires_at_ms.remove("k");
    });
    refused("event expiry deleted, event kept", shortened, &|n| {
        n.retention.event_expires_at_ms.remove("ev");
    });
    refused("new expiry below the fallback", shortened, &|n| {
        n.retention
            .admission_expires_at_ms
            .insert("fallback".into(), 1_999);
    });
    refused("new event expiry below the fallback", shortened, &|n| {
        n.retention
            .event_expires_at_ms
            .insert("ev-fallback".into(), 2_999);
    });
    refused("new expiry without its item", shortened, &|n| {
        n.retention
            .admission_expires_at_ms
            .insert("absent".into(), 1_000_000);
    });
    // History in the commit does not cover a run that is not finished, an
    // unsettled operation, an operation left live, or a run of another root.
    refused("running", obligations, &|n| {
        let running = n.runs.remove("run-b").unwrap();
        n.settled
            .push(cohort(running, vec![], vec![audit(2, "run-b")]));
        n.receipts.retain(|r| r.sequence != 2);
    });
    refused("unsettled operation", obligations, &|n| {
        let run = n.runs.remove("run-a").unwrap();
        n.intents.remove("op-a");
        n.settled.push(cohort(
            run,
            vec![intent("op-a", "run-a", IntentStatus::Accepted)],
            vec![],
        ));
    });
    refused("operation left live", obligations, &|n| {
        let run = n.runs.remove("run-a").unwrap();
        n.settled.push(cohort(run, vec![], vec![]));
    });
    refused("run of another root", obligations, &|n| {
        let run = n.runs.remove("run-a").unwrap();
        let op = n.intents.remove("op-a").unwrap();
        let mut c = cohort(run, vec![op], vec![]);
        c.root = "run-b".into();
        n.settled.push(c);
    });
    refused(
        "operation without history",
        retention::REFUSE_RUN_REMOVAL,
        &|n| {
            let run = n.runs.remove("run-a").unwrap();
            n.intents.remove("op-a");
            n.settled.push(cohort(run, vec![], vec![]));
        },
    );
    assert!(failures.borrow().is_empty(), "{:#?}", failures.borrow());
    // Allowed: expired entries with their records, an expiry moved later, a
    // new expiry at its fallback, and a run with its operation and receipt.
    let mut next = s.clone();
    next.retention.clock_ms = Some(4_000);
    for key in ["k", "fallback"] {
        next.keyed_admissions.remove(key);
    }
    next.retention.admission_expires_at_ms.remove("k");
    for key in ["ev", "ev-fallback"] {
        next.events.remove(key);
    }
    next.retention.event_expires_at_ms.remove("ev");
    let run = next.runs.remove("run-a").unwrap();
    let op = next.intents.remove("op-a").unwrap();
    next.receipts.retain(|r| r.sequence != 1);
    next.settled
        .push(cohort(run, vec![op], vec![audit(1, "run-a")]));
    next.keyed_admissions
        .insert("later".into(), receipt("later", 100));
    next.retention
        .admission_expires_at_ms
        .insert("later".into(), 2_100);
    let (ns, base) = fresh();
    let revision = store.compare_and_swap(&ns, "w", base, &next).unwrap();
    let mut later = next.clone();
    later.settled.clear();
    later
        .retention
        .admission_expires_at_ms
        .insert("later".into(), 9_000);
    store.compare_and_swap(&ns, "w", revision, &later).unwrap();
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
/// this change) are still allowed once the event has expired, and in-State
/// tombstones and fences may always be removed.
fn legacy_removals_are_allowed(store: &dyn Storage) {
    store.acquire("legacy", "w").unwrap();
    let mut s = State::default();
    s.retention.policy.event_replay_ms = 1_000_000;
    s.runs.insert("run-a".into(), completed_run("run-a"));
    s.receipts.push(audit(1, "run-a"));
    s.events.insert("ev".into(), accepted_event("ev", 0));
    s.retention
        .event_expires_at_ms
        .insert("ev".into(), 1_000_000);
    s.retention
        .expired_correlations
        .insert("c".into(), ("host".into(), 0));
    s.retention
        .event_tombstones
        .insert("old".into(), tombstone("d"));
    let r = store.compare_and_swap("legacy", "w", 0, &s).unwrap();
    let mut next = s.clone();
    next.retention.clock_ms = Some(1_000_000);
    next.runs.remove("run-a");
    next.receipts.clear();
    next.events.remove("ev");
    next.retention.event_expires_at_ms.remove("ev");
    next.retention.disposals.push(retention::Disposal {
        kind: "run".into(),
        key: "run-a".into(),
        at_ms: 5,
    });
    next.retention.event_tombstones.remove("old");
    next.retention.expired_correlations.remove("c");
    next.retention
        .event_tombstones
        .insert("ev".into(), tombstone("wrong"));
    assert!(refused_with(
        store.compare_and_swap("legacy", "w", r, &next),
        "event disposal digest conflict"
    ));
    next.retention
        .event_tombstones
        .insert("ev".into(), tombstone("digest-ev"));
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
type Case = (&'static str, &'static str, Box<dyn Fn(&mut State)>);
/// The same fences on `RecordStorage`'s bounded transitions: a removal with
/// no history and no expired entry, or an expiry record moved earlier, is
/// refused and writes nothing; an expired window entry leaves incrementally.
#[test]
fn record_storage_transitions_hold_the_removal_fences() {
    let store = RecordStorage::new();
    let mut s = fence_state();
    s.indexes.initialized = true;
    let count = std::cell::Cell::new(0);
    let fresh = || {
        count.set(count.get() + 1);
        let ns = format!("t-{}", count.get());
        store.acquire(&ns, "w").unwrap();
        let base = store.compare_and_swap(&ns, "w", 0, &s).unwrap();
        (ns, base)
    };
    let transition_in = |ns: &str, f: &dyn Fn(&mut State)| {
        let mut plan = |s: &mut State| {
            f(s);
            Ok(true)
        };
        let validate = |_: &State, _: &QuotaCounts, _: &QuotaCounts| Ok(());
        store
            .transition_v1(
                ns,
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
    let shortened = retention::REFUSE_EXPIRY;
    let cases: Vec<Case> = vec![
        (
            "run",
            retention::REFUSE_RUN_REMOVAL,
            Box::new(|s| {
                s.runs.remove("run-a");
            }),
        ),
        (
            "intent",
            retention::REFUSE_RUN_REMOVAL,
            Box::new(|s| {
                s.intents.remove("op-a");
            }),
        ),
        (
            "receipt, compacted",
            retention::REFUSE_RUN_REMOVAL,
            Box::new(|s| {
                s.receipts.retain(|r| r.sequence != 1);
            }),
        ),
        (
            "receipt, overwritten",
            retention::REFUSE_RUN_REMOVAL,
            Box::new(|s| {
                *s.receipts.get_mut(0).unwrap() = audit(3, "run-a");
            }),
        ),
        (
            "keyed admission",
            early,
            Box::new(|s| {
                s.retention.clock_ms = Some(2_999);
                s.keyed_admissions.remove("k");
                s.retention.admission_expires_at_ms.remove("k");
            }),
        ),
        (
            "event",
            early,
            Box::new(|s| {
                s.retention.clock_ms = Some(3_999);
                s.events.remove("ev");
                s.retention.event_expires_at_ms.remove("ev");
            }),
        ),
        (
            "forged tombstone",
            early,
            Box::new(|s| {
                s.events.remove("ev");
                s.retention.event_expires_at_ms.remove("ev");
                s.retention
                    .event_tombstones
                    .insert("ev".into(), tombstone("digest-ev"));
            }),
        ),
        (
            "changed receipt",
            "immutable keyed admission receipt removed or changed",
            Box::new(|s| {
                s.keyed_admissions.get_mut("k").unwrap().run_id = "other".into();
            }),
        ),
        (
            "clock",
            retention::REFUSE_CLOCK,
            Box::new(|s| {
                s.retention.clock_ms = Some(499);
            }),
        ),
        (
            "lowered expiry",
            shortened,
            Box::new(|s| {
                s.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_500);
            }),
        ),
        (
            "expiry deleted, item kept",
            shortened,
            Box::new(|s| {
                s.retention.event_expires_at_ms.remove("ev");
            }),
        ),
        (
            "new expiry below the fallback",
            shortened,
            Box::new(|s| {
                s.retention
                    .admission_expires_at_ms
                    .insert("fallback".into(), 1_999);
            }),
        ),
    ];
    let mut cases = cases;
    cases.extend(rebase_refusal_cases());
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|(label, text, f)| {
            let (ns, base) = fresh();
            let result = transition_in(&ns, f.as_ref());
            if result.is_err() {
                assert_eq!(store.load(&ns).unwrap().revision, base, "{label}");
            }
            (!refused_with(result.clone(), text)).then(|| format!("{label}: {result:?}"))
        })
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
    // Ruling 3: a licensed rebase commits through the bounded transition,
    // and the licence is that commit's own.
    let (ns, _) = fresh();
    transition_in(&ns, &rebased).unwrap();
    let r = store.load(&ns).unwrap().state.retention;
    assert_eq!(
        (
            r.clock_ms,
            r.stale_floor_ms,
            r.event_expires_at_ms.get("ev").copied()
        ),
        (Some(100), Some(50), Some(3_100))
    );
    for (label, text, f) in after_rebase_refusal_cases() {
        let result = transition_in(&ns, f.as_ref());
        assert!(refused_with(result.clone(), text), "{label}: {result:?}");
    }
    transition_in(&ns, &floor_lowered).unwrap();
    assert_eq!(
        store.load(&ns).unwrap().state.retention.stale_floor_ms,
        Some(40)
    );
    let (ns, _) = fresh();
    let transition = |f: &dyn Fn(&mut State)| transition_in(&ns, f);
    // Expired: both entries leave in one bounded transition.
    transition(&|s| {
        s.retention.clock_ms = Some(4_000);
        s.keyed_admissions.remove("k");
        s.retention.admission_expires_at_ms.remove("k");
        s.events.remove("ev");
        s.retention.event_expires_at_ms.remove("ev");
    })
    .unwrap();
    let state = store.load(&ns).unwrap().state;
    assert!(!state.keyed_admissions.contains_key("k") && !state.events.contains_key("ev"));
    // With history for the run, its operation and its receipt, the run
    // leaves.
    transition(&|s| {
        let run = s.runs.remove("run-a").unwrap();
        let op = s.intents.remove("op-a").unwrap();
        s.receipts.retain(|r| r.sequence != 1);
        s.settled
            .push(cohort(run, vec![op], vec![audit(1, "run-a")]));
    })
    .unwrap();
    let state = store.load(&ns).unwrap().state;
    assert!(!state.runs.contains_key("run-a"));
    assert_eq!(
        state
            .receipts
            .iter()
            .map(|r| r.sequence)
            .collect::<Vec<_>>(),
        vec![2]
    );
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

// --- Task 5: a settled cohort leaves the record in the commit that settles it

fn runs_page(e: &Engine<MemoryStorage>, run: Option<&str>) -> Vec<Value> {
    e.inspect(&InspectionQuery {
        kind: InspectionKind::Runs,
        run: run.map(String::from),
        after: None,
        limit: 1000,
    })
    .unwrap()
    .items
}
fn done_artifact() -> Artifact {
    artifact(
        "a",
        "done",
        BTreeMap::from([("done".into(), ret("ok"))]),
        Registry::default(),
        BTreeMap::new(),
    )
}
#[test]
fn finished_root_leaves_the_record_in_the_commit_that_finishes_it() {
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
    let id = e.admit("a", "main", Value::Null, authority(), 1).unwrap();
    let before = e.store.load("test").unwrap().revision;
    assert!(e.advance(&id, 2).unwrap());
    let v = e.store.load("test").unwrap();
    assert_eq!(v.revision, before + 1);
    assert!(
        v.state.runs.is_empty()
            && v.state.intents.is_empty()
            && v.state.receipts.is_empty()
            && v.state.budgets.is_empty()
    );
    assert!(
        v.state.indexes.root_members.is_empty()
            && v.state.indexes.run_roots.is_empty()
            && v.state.indexes.receipts.is_empty()
    );
    let run = &runs_page(&e, Some(&id))[0];
    assert_eq!(
        (run["status"].as_str(), run["outcome"].as_str()),
        (Some("completed"), Some("ok"))
    );
}
#[test]
fn cohort_stays_while_an_independent_child_runs() {
    let e = engine();
    let child = wait_artifact();
    e.install(child.clone()).unwrap();
    install(&e, parent_artifact(&child, CallMode::Independent), "parent");
    let id = e
        .admit("parent", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs.len(), 2);
    assert!(s.runs[&id].status.terminal());
    e.admit_event("host", event("e1", 2), 5).unwrap();
    e.drive(&mut FakeHost::default(), 5, 20).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    assert_eq!(runs_page(&e, None).len(), 2);
}
#[test]
fn cancelled_run_stays_until_its_operation_settles_then_late_results_are_not_found() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![DispatchResult::Unknown {
                reason: "lost".into(),
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 10).unwrap();
    e.cancel(&id, 1).unwrap();
    e.advance(&id, 1).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].status, RunStatus::Cancelling);
    let intent = s.intents.keys().next().unwrap().clone();
    let done = DispatchResult::Completed {
        outcome: "ok".into(),
        output: Value::Null,
    };
    e.record_result(&intent, done.clone(), 2).unwrap();
    e.advance(&id, 2).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    assert_eq!(runs_page(&e, Some(&id))[0]["status"], "cancelled");
    assert!(matches!(
        e.record_result(&intent, done, 3),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(e.advance(&id, 3), Err(Error::NotFound(_))));
    assert!(matches!(e.cancel(&id, 3), Err(Error::NotFound(_))));
}
fn inspect_all(e: &Engine<impl Storage>, kind: InspectionKind, run: Option<&str>) -> Vec<Value> {
    let mut items = vec![];
    let mut after = None;
    loop {
        let page = e
            .inspect(&InspectionQuery {
                kind,
                run: run.map(String::from),
                after,
                limit: 2,
            })
            .unwrap();
        items.extend(page.items);
        match page.next {
            Some(next) => after = Some(next),
            None => return items,
        }
    }
}
/// The one item of `kind` whose inspection key is `key`.
fn inspected(e: &Engine<MemoryStorage>, kind: InspectionKind, key: &str) -> Value {
    let items: Vec<Value> = inspect_all(e, kind, None)
        .into_iter()
        .filter(|v| match kind {
            InspectionKind::Receipts => format!("{:020}", v["sequence"].as_u64().unwrap()) == key,
            _ => v["id"] == key,
        })
        .collect();
    assert_eq!(items.len(), 1, "{kind:?} {key}");
    items.into_iter().next().unwrap()
}
#[test]
fn history_equals_the_projection_of_the_removed_cohort() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
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
                output: Value::Null,
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty() && s.intents.is_empty() && s.receipts.is_empty());
    let cohorts = e.store.settled_cohorts("test");
    assert_eq!(cohorts.len(), 1);
    let cohort = &cohorts[0];
    assert_eq!(cohort.root, id);
    assert_eq!(cohort.runs.len(), 1);
    assert_eq!(cohort.intents.len(), 1);
    assert!(cohort.receipts.len() >= 4, "{:?}", cohort.receipts);
    // Finish time: the terminal receipt's time.
    let terminal = cohort.runs[0].terminal_sequence.unwrap();
    let at = cohort
        .receipts
        .iter()
        .find(|r| r.sequence == terminal)
        .unwrap()
        .at_ms;
    assert_eq!(cohort.finished_at_ms, at);
    for run in &cohort.runs {
        assert_eq!(
            inspection::public_run(run),
            inspected(&e, InspectionKind::Runs, &run.id)
        );
    }
    for intent in &cohort.intents {
        assert_eq!(
            inspection::public_intent(intent),
            inspected(&e, InspectionKind::Effects, &intent.id)
        );
    }
    for receipt in &cohort.receipts {
        assert_eq!(
            inspection::public_receipt(receipt),
            inspected(
                &e,
                InspectionKind::Receipts,
                &format!("{:020}", receipt.sequence)
            )
        );
    }
    // The private fields stay readable from the retained cohort.
    assert_eq!(
        e.store.settled_run("test", &id).unwrap().outcome.as_deref(),
        Some("ok")
    );
    assert_eq!(cohort.intents[0].result, Some(("ok".into(), Value::Null)));
}
#[test]
fn admin_receipts_move_to_history_in_the_commit_that_writes_them() {
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
    e.set_activation_controls("a", ActivationControls::default(), 3)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.receipts.iter().all(|r| r.run != "admin"));
    let receipts = e
        .inspect(&InspectionQuery {
            kind: InspectionKind::Receipts,
            run: Some("admin".into()),
            after: None,
            limit: 10,
        })
        .unwrap();
    assert_eq!(receipts.items[0]["kind"], "activation_controls");
}
fn install_on<S: Storage>(e: &Engine<S>, a: Artifact, binding: &str) {
    let d = a.digest.clone();
    e.install(a).unwrap();
    e.activate(binding, &d, authority()).unwrap();
}
fn assert_same_engines(m: &Engine<MemoryStorage>, r: &Engine<RecordStorage>, step: &str) {
    assert_eq!(
        serde_json::to_value(m.snapshot().unwrap()).unwrap(),
        serde_json::to_value(r.snapshot().unwrap()).unwrap(),
        "snapshot after {step}"
    );
    for kind in [
        InspectionKind::Runs,
        InspectionKind::Effects,
        InspectionKind::Receipts,
    ] {
        assert_eq!(
            inspect_all(m, kind, None),
            inspect_all(r, kind, None),
            "{kind:?} after {step}"
        );
    }
}
#[test]
fn record_storage_settles_like_memory_storage() {
    let m = engine();
    let r = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
    let child = wait_artifact();
    let parent = parent_artifact(&child, CallMode::Independent);
    m.install(child.clone()).unwrap();
    r.install(child.clone()).unwrap();
    assert_same_engines(&m, &r, "install");
    install(&m, parent.clone(), "parent");
    install_on(&r, parent, "parent");
    assert_same_engines(&m, &r, "activate");
    let id = m
        .admit("parent", "main", Value::Null, authority(), 0)
        .unwrap();
    assert_eq!(
        r.admit("parent", "main", Value::Null, authority(), 0)
            .unwrap(),
        id
    );
    assert_same_engines(&m, &r, "admit");
    m.drive(&mut FakeHost::default(), 0, 20).unwrap();
    let transitions = r
        .store
        .stats
        .transitions
        .load(std::sync::atomic::Ordering::Relaxed);
    r.drive(&mut FakeHost::default(), 0, 20).unwrap();
    assert_same_engines(&m, &r, "drive 0");
    assert_eq!(m.snapshot().unwrap().runs.len(), 2);
    m.admit_event("host", event("e1", 2), 5).unwrap();
    r.admit_event("host", event("e1", 2), 5).unwrap();
    assert_same_engines(&m, &r, "event");
    m.drive(&mut FakeHost::default(), 5, 20).unwrap();
    r.drive(&mut FakeHost::default(), 5, 20).unwrap();
    assert_same_engines(&m, &r, "drive 5");
    // The settling commit took the record backend's transition path.
    assert!(
        r.store
            .stats
            .transitions
            .load(std::sync::atomic::Ordering::Relaxed)
            > transitions
    );
    assert!(r.snapshot().unwrap().runs.is_empty());
    assert_eq!(inspect_all(&r, InspectionKind::Runs, None).len(), 2);
}
/// A run key that is already history cannot become live again: the engine's
/// commit is refused, and the refusal leaves the revision and the lease.
#[test]
fn a_live_key_already_in_history_is_refused_through_the_engine() {
    let e = engine();
    install(&e, done_artifact(), "a");
    let v = e.store.load("test").unwrap();
    let next = format!("run-{:016x}", v.state.sequence + 1);
    let mut seeded = v.state.clone();
    seeded.settled.push(synthetic_cohort(&next, 0, &[]));
    e.store
        .compare_and_swap("test", "incarnation-1", v.revision, &seeded)
        .unwrap();
    let revision = e.store.load("test").unwrap().revision;
    let refused = e.admit("a", "main", Value::Null, authority(), 1);
    assert!(
        matches!(&refused, Err(Error::Storage(StorageError::Backend(m))) if m == history::ALSO_LIVE),
        "{refused:?}"
    );
    let after = e.store.load("test").unwrap();
    assert_eq!(after.revision, revision);
    assert!(after.state.runs.is_empty());
    // The lease is still this engine's.
    assert_eq!(
        e.store.acquire("test", "other"),
        Err(StorageError::Ownership)
    );
    e.set_activation_controls("a", ActivationControls::default(), 2)
        .unwrap();
    assert_eq!(e.store.load("test").unwrap().revision, revision + 1);
}
/// A run that moves from the live state to history between two inspection
/// pages is neither repeated nor skipped.
#[test]
fn a_run_moving_to_history_between_pages_is_neither_repeated_nor_skipped() {
    let e = engine();
    install(&e, done_artifact(), "a");
    let ids: Vec<String> = (0..3)
        .map(|i| e.admit("a", "main", Value::Null, authority(), i).unwrap())
        .collect();
    let query = |after: Option<String>| InspectionQuery {
        kind: InspectionKind::Runs,
        run: None,
        after,
        limit: 1,
    };
    let first = e.inspect(&query(None)).unwrap();
    assert_eq!(first.items[0]["id"], ids[0].as_str());
    assert_eq!(first.items[0]["status"], "admitted");
    // The run already paged and the next one both leave the state.
    assert!(e.advance(&ids[0], 5).unwrap());
    assert!(e.advance(&ids[1], 5).unwrap());
    let second = e.inspect(&query(first.next)).unwrap();
    assert_eq!(second.items[0]["id"], ids[1].as_str());
    assert_eq!(second.items[0]["status"], "completed");
    let third = e.inspect(&query(second.next)).unwrap();
    assert_eq!(third.items[0]["id"], ids[2].as_str());
    assert_eq!(third.items[0]["status"], "admitted");
    assert!(third.next.is_none());
    // Receipts too: a receipt page edge across the move.
    let receipts = |after: Option<String>| {
        e.inspect(&InspectionQuery {
            kind: InspectionKind::Receipts,
            run: None,
            after,
            limit: 1,
        })
        .unwrap()
    };
    let page = receipts(None);
    let mut seen = vec![page.items[0]["sequence"].as_u64().unwrap()];
    e.advance(&ids[2], 6).unwrap();
    let mut after = page.next;
    while let Some(cursor) = after {
        let page = receipts(Some(cursor));
        seen.extend(page.items.iter().map(|v| v["sequence"].as_u64().unwrap()));
        after = page.next;
    }
    let all: Vec<u64> = e
        .store
        .settled_cohorts("test")
        .iter()
        .flat_map(|c| c.receipts.iter().map(|r| r.sequence))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(seen, all);
}
/// Commands addressed to a run that has left the record get `NotFound`
/// (today's answer for a collected run), and a late wake is a no-op.
#[test]
fn commands_to_a_run_that_left_the_record_are_not_found() {
    let e = engine();
    let child = wait_artifact();
    install(&e, child, "waiter");
    let id = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].status, RunStatus::Waiting);
    // Its deadline timer is due at 100.
    assert!(!s.indexes.timers.is_empty());
    e.admit_event("host", event("e1", 2), 5).unwrap();
    e.drive(&mut FakeHost::default(), 5, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty() && s.waits.is_empty() && s.indexes.timers.is_empty());
    assert_eq!(runs_page(&e, Some(&id))[0]["outcome"], "ok");
    assert!(matches!(e.advance(&id, 6), Err(Error::NotFound(_))));
    assert!(matches!(e.cancel(&id, 6), Err(Error::NotFound(_))));
    // A late wake: driving past the old deadline finds nothing to do.
    let revision = e.store.load("test").unwrap().revision;
    assert_eq!(e.drive(&mut FakeHost::default(), 1_000, 20).unwrap(), 0);
    assert_eq!(e.store.load("test").unwrap().revision, revision);
}
/// A late result for an operation that left with its cohort, and a recovery
/// request for it, are `NotFound`.
#[test]
fn late_results_and_recovery_for_a_removed_operation_are_not_found() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    // The invoke step prepares the operation; cancelling the run cancels the
    // prepared operation, and the run then finishes cancelled.
    e.advance(&id, 0).unwrap();
    assert_eq!(e.snapshot().unwrap().intents.len(), 1);
    e.cancel(&id, 0).unwrap();
    e.advance(&id, 0).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    let cohort = &e.store.settled_cohorts("test")[0];
    assert_eq!(cohort.intents.len(), 1);
    assert_eq!(cohort.intents[0].status, IntentStatus::Cancelled);
    let intent = cohort.intents[0].id.clone();
    let late = DispatchResult::Completed {
        outcome: "ok".into(),
        output: Value::Null,
    };
    assert!(matches!(
        e.record_result(&intent, late, 1),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        e.request_recovery(&intent, "op", "late", vec!["ref".into()], 1),
        Err(Error::NotFound(_))
    ));
}
/// `activate` takes its time as the later of the last receipt and the clock:
/// once every receipt has left, the clock still dates the activation.
#[test]
fn activate_dates_by_the_clock_once_receipts_have_left() {
    let e = engine();
    install(&e, done_artifact(), "a");
    let id = e.admit("a", "main", Value::Null, authority(), 40).unwrap();
    e.advance(&id, 50).unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.receipts.is_empty());
    assert_eq!(s.retention.clock_ms, Some(50));
    e.activate("b", &done_artifact().digest, authority())
        .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.catalog_history_at_ms[&s.catalog_revision], 50);
    assert_eq!(s.retention.clock_ms, Some(50));
}
/// Settlement is found from the commit's dirty roots and the root indexes
/// (SC-6): settling one cohort reads the same records whether 20 or 80 other
/// runs are live, and scans no run, operation or root index container.
#[test]
fn settling_one_cohort_does_not_scan_other_live_runs() {
    use std::sync::atomic::Ordering;
    let measure = |others: usize| {
        let e = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
        install_on(&e, done_artifact(), "a");
        for i in 0..others {
            e.admit("a", "main", Value::Null, authority(), i as i64)
                .unwrap();
        }
        let id = e.admit("a", "main", Value::Null, authority(), 100).unwrap();
        let reads = e.store.stats.record_reads.load(Ordering::Relaxed);
        let fallbacks = e.store.stats.fallbacks().len();
        let transitions = e.store.stats.transitions.load(Ordering::Relaxed);
        assert!(e.advance(&id, 101).unwrap());
        assert_eq!(
            e.store.stats.transitions.load(Ordering::Relaxed),
            transitions + 1
        );
        assert!(!e.snapshot().unwrap().runs.contains_key(&id));
        assert_eq!(e.snapshot().unwrap().runs.len(), others);
        let scanned: BTreeSet<String> = e.store.stats.fallbacks()[fallbacks..]
            .iter()
            .map(|f| f.split(' ').next().unwrap().to_string())
            .collect();
        (
            e.store.stats.record_reads.load(Ordering::Relaxed) - reads,
            scanned,
        )
    };
    // Both sizes exceed the 16-record page a bounded index read takes (the
    // due-timer query reads one such page), so equal reads mean nothing
    // scales with the number of live runs.
    let (few, few_scanned) = measure(20);
    let (many, many_scanned) = measure(80);
    assert_eq!(few, many, "record reads of the settling commit");
    assert_eq!(few_scanned, many_scanned);
    // Only the positional receipt sequence and its offset indexes are
    // rewritten whole (SC-6: compaction is O(live receipts)).
    let allowed: BTreeSet<String> = ["receipts", "indexes.receipts", "indexes.all_receipts"]
        .into_iter()
        .map(String::from)
        .collect();
    assert!(many_scanned.is_subset(&allowed), "{many_scanned:?}");
}

// --- Task 5 fix round 1 ---------------------------------------------------

/// `RuntimeIndexes` fields of the engine that aq-server vendors
/// (`vendor/aq-playbooks-0173673`, pre-D10): no `root_intents` and no
/// backfill flag.
const VENDORED_INDEX_FIELDS: &[&str] = &[
    "ready",
    "ready_tokens",
    "scope_cursor",
    "dispatch_turn",
    "effect_cursor",
    "effect_item_cursor",
    "root_members",
    "root_active",
    "root_effects",
    "root_last_activity",
    "run_roots",
    "intent_roots",
    "effects",
    "effect_memberships",
    "timers",
    "due",
    "run_timer",
    "events",
    "event_memberships",
    "waiters",
    "run_subscriptions",
    "dependents",
    "run_dependencies",
    "completion_ready",
    "uncertain_completion_contracts",
    "uncertain_contract_membership",
    "dirty_runs",
    "dirty_intents",
    "runs",
    "receipts",
    "all_receipts",
    "triggers",
    "activation_triggers",
    "initialized",
];
/// `state` as a pre-D10 engine stored it: the index fields D10 and this task
/// added are absent, so they decode with their defaults.
fn vendored_shape(state: &State) -> State {
    let mut value = serde_json::to_value(state).unwrap();
    let indexes = value["indexes"].as_object_mut().unwrap();
    indexes.retain(|field, _| VENDORED_INDEX_FIELDS.contains(&field.as_str()));
    let fields: BTreeSet<&str> = indexes.keys().map(String::as_str).collect();
    assert_eq!(fields, VENDORED_INDEX_FIELDS.iter().copied().collect());
    serde_json::from_value(value).unwrap()
}
fn completed_host() -> FakeHost {
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
/// An invoke run whose operation has completed while the run is still live
/// (waiting to take its `ok` edge), on `e`.
fn run_with_completed_operation<S: Storage>(
    e: &Engine<S>,
    host: &mut FakeHost,
) -> (String, String) {
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    e.dispatch_next(host, 0).unwrap();
    let s = e.snapshot().unwrap();
    let intent = s.intents.values().next().unwrap().clone();
    assert_eq!(intent.status, IntentStatus::Completed);
    assert!(!s.runs[&id].status.terminal());
    (id, intent.id)
}
/// Store `state` over the namespace's current State (direct CAS).
fn overwrite<S: Storage>(store: &S, owner: &str, state: &State) {
    let revision = store.load("test").unwrap().revision;
    store
        .compare_and_swap("test", owner, revision, state)
        .unwrap();
}
/// C1: a namespace written by the pre-D10 engine has no `root_intents`.
/// Opening it backfills the index once, so a pre-upgrade run with an earlier
/// completed operation finishes, leaves with that operation in its history,
/// and a run admitted afterwards progresses.
fn pre_d10_namespace_is_backfilled_on_open<S: Storage + Clone>(store: S) {
    let e = Engine::new(store.clone(), "test", "incarnation-1").unwrap();
    install_on(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    let mut host = completed_host();
    let (old, intent) = run_with_completed_operation(&e, &mut host);
    let legacy = vendored_shape(&e.snapshot().unwrap());
    assert!(legacy.indexes.initialized && legacy.indexes.root_intents.is_empty());
    assert!(!legacy.indexes.root_intents_complete);
    overwrite(&e.store, "incarnation-1", &legacy);
    e.release().unwrap();
    let e = Engine::new(store, "test", "incarnation-2").unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.indexes.root_intents_complete);
    assert!(s.indexes.root_intents[&old].contains(&intent));
    let new = e
        .admit("invoke", "main", Value::Null, authority(), 1)
        .unwrap();
    for _ in 0..10 {
        e.drive(&mut host, 1, 20).unwrap();
    }
    let s = e.snapshot().unwrap();
    assert!(
        s.runs.is_empty() && s.intents.is_empty(),
        "{:?}",
        s.runs.keys().collect::<Vec<_>>()
    );
    let effects = inspect_all(&e, InspectionKind::Effects, Some(&old));
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0]["id"], intent.as_str());
    assert_eq!(effects[0]["status"], "completed");
    for id in [&old, &new] {
        let runs = inspect_all(&e, InspectionKind::Runs, Some(id));
        assert_eq!(runs[0]["outcome"], "ok", "{id}");
    }
}
#[test]
fn memory_storage_pre_d10_namespace_is_backfilled_on_open() {
    pre_d10_namespace_is_backfilled_on_open(MemoryStorage::retaining_settled());
}
#[test]
fn record_storage_pre_d10_namespace_is_backfilled_on_open() {
    pre_d10_namespace_is_backfilled_on_open(RecordStorage::new());
}
/// The backfill runs once: the flag is stored, and a second open does not
/// read the operations again (format 3 record reads of `intents`).
#[test]
fn root_intents_backfill_runs_once() {
    let store = RecordStorage::new();
    let e = Engine::new(store.clone(), "test", "incarnation-1").unwrap();
    install_on(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    let mut host = completed_host();
    run_with_completed_operation(&e, &mut host);
    overwrite(
        &e.store,
        "incarnation-1",
        &vendored_shape(&e.snapshot().unwrap()),
    );
    e.release().unwrap();
    let scanned = |store: &RecordStorage| {
        store
            .stats
            .fallbacks()
            .iter()
            .filter(|f| f.starts_with("intents "))
            .count()
    };
    let before = scanned(&store);
    let e = Engine::new(store.clone(), "test", "incarnation-2").unwrap();
    let first = scanned(&store) - before;
    assert_eq!(first, 1, "the backfill reads the operations once");
    e.release().unwrap();
    let e = Engine::new(store.clone(), "test", "incarnation-3").unwrap();
    assert_eq!(scanned(&store) - before, first, "the second open does not");
    assert!(e.snapshot().unwrap().indexes.root_intents_complete);
    // A new namespace is complete from its first rebuild.
    let fresh = Engine::new(MemoryStorage::default(), "fresh", "w").unwrap();
    assert!(fresh.snapshot().unwrap().indexes.root_intents_complete);
}
/// Ruling 2: a cohort whose settling commit is refused does not wedge the
/// ready queue. The refusal reaches the host (`drive` returns it) and the
/// failing run moves behind the others, so unrelated runs still progress.
#[test]
fn an_unsettleable_cohort_does_not_stop_unrelated_runs() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    install(&e, done_artifact(), "a");
    let mut host = completed_host();
    let (stuck, _) = run_with_completed_operation(&e, &mut host);
    // Break the index behind the engine's back (flag kept), so the commit
    // that finishes `stuck` is refused by the removal fence.
    let mut broken = e.snapshot().unwrap();
    broken.indexes.root_intents.clear();
    overwrite(&e.store, "incarnation-1", &broken);
    // Drive until the finishing commit of `stuck` is refused: `stuck` is
    // then at the head of the ready queue.
    let refused = (0..5).find_map(|_| e.drive(&mut host, 5, 20).err());
    assert!(
        matches!(&refused, Some(Error::Storage(StorageError::Backend(m))) if m == "unresolved run obligations cannot be disposed"),
        "{refused:?}"
    );
    let unrelated: Vec<String> = (0..3)
        .map(|i| {
            e.admit("a", "main", Value::Null, authority(), 1 + i)
                .unwrap()
        })
        .collect();
    let mut refusals = 0;
    for _ in 0..20 {
        match e.drive(&mut host, 5, 20) {
            Ok(_) => {}
            Err(Error::Storage(StorageError::Backend(m))) => {
                assert_eq!(m, "unresolved run obligations cannot be disposed");
                refusals += 1;
            }
            Err(other) => panic!("{other}"),
        }
    }
    assert!(refusals > 0, "the refusal surfaces to the host");
    let s = e.snapshot().unwrap();
    assert!(
        s.runs.contains_key(&stuck),
        "the unsettleable run stays live"
    );
    for id in &unrelated {
        assert!(!s.runs.contains_key(id), "{id} progressed and left");
        assert_eq!(runs_page(&e, Some(id))[0]["outcome"], "ok");
    }
}
/// Ruling 3 (M5): a run whose root has gone is an engine fault. The commit
/// fails with an internal error; the run is not sent down its playbook's
/// error branch and nothing is written.
#[test]
fn a_missing_root_fails_the_commit_instead_of_taking_an_error_branch() {
    let e = engine();
    let child = wait_artifact();
    e.install(child.clone()).unwrap();
    install(&e, parent_artifact(&child, CallMode::Independent), "parent");
    let root = e
        .admit("parent", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&root, 0).unwrap();
    e.advance(&root, 0).unwrap();
    let mut s = e.snapshot().unwrap();
    assert!(s.runs[&root].status.terminal());
    let member = s.runs.keys().find(|id| **id != root).unwrap().clone();
    // Remove only the root, with its history (the fence allows it: the
    // independent member is not its owned child).
    let run = s.runs.remove(&root).unwrap();
    let receipts: Vec<Receipt> = s
        .receipts
        .iter()
        .filter(|r| r.run == root)
        .cloned()
        .collect();
    s.receipts.retain(|r| r.run != root);
    s.indexes.receipts.clear();
    s.indexes.all_receipts.clear();
    for (offset, r) in s.receipts.clone().iter().enumerate() {
        s.indexes
            .receipts
            .entry(r.run.clone())
            .or_default()
            .insert(r.sequence, offset);
        s.indexes.all_receipts.insert(r.sequence, offset);
    }
    s.settled.push(SettledCohort {
        root: root.clone(),
        finished_at_ms: 0,
        runs: vec![run],
        intents: vec![],
        receipts,
    });
    overwrite(&e.store, "incarnation-1", &s);
    let before = e.store.load("test").unwrap();
    let refused = e.advance(&member, 1);
    assert!(
        matches!(&refused, Err(Error::Conflict(m)) if m.starts_with("engine invariant violated")),
        "{refused:?}"
    );
    let after = e.store.load("test").unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(
        after.state.runs[&member].step,
        before.state.runs[&member].step
    );
    assert!(after.state.receipts.iter().all(|r| r.kind != "error"));
}
fn trigger_artifact() -> Artifact {
    let c = event_contract();
    let mut a = done_artifact().content;
    a.registry.events.insert(c.name.clone(), c.clone());
    a.playbook.triggers.push(Trigger {
        id: "on-completed".into(),
        event: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        entry: "main".into(),
        input: Expr::literal(Value::Null),
    });
    Artifact::seal(a).unwrap()
}
fn event_run_id(id: &str) -> bool {
    id.strip_prefix("event-sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}
/// SC-7: an event accepted again after its window gets new destination,
/// run (and so child and operation) ids, in the same `event-sha256:` form;
/// the first run's history stays and the second run commits beside it.
#[test]
fn an_event_accepted_again_after_its_window_gets_new_run_ids() {
    let e = engine();
    install(&e, trigger_artifact(), "trigger");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_event("host", event("again", 2), 0).unwrap();
    assert_eq!(e.fanout(&first.key, 10, 0).unwrap(), 1);
    e.drive(&mut FakeHost::default(), 0, 20).unwrap();
    let first_run = runs_page(&e, None)[0]["id"].as_str().unwrap().to_string();
    // The window passes: GC drops the event; the legacy fences it leaves
    // (tombstone, correlation fences) go too, as Task 6 will not write them.
    while e.collect_garbage(2_000, 100).unwrap().more {}
    let mut s = e.snapshot().unwrap();
    assert!(!s.events.contains_key(&first.key));
    s.retention.event_tombstones.clear();
    s.retention.expired_correlations.clear();
    overwrite(&e.store, "incarnation-1", &s);
    let second = e.admit_event("host", event("again", 2), 2_001).unwrap();
    assert_eq!(second.key, first.key);
    assert!(!second.duplicate);
    assert_eq!(e.fanout(&second.key, 10, 2_001).unwrap(), 1);
    e.drive(&mut FakeHost::default(), 2_001, 20).unwrap();
    let ids: Vec<String> = runs_page(&e, None)
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(ids.contains(&first_run));
    assert!(ids.iter().all(|id| event_run_id(id)), "{ids:?}");
    assert!(e.snapshot().unwrap().runs.is_empty());
}
/// SC-7, existing data: a destination stored before the change keeps its
/// key, and its run id is `event-<that key>`.
#[test]
fn a_stored_destination_keeps_its_key_and_run_id() {
    let e = engine();
    install(&e, trigger_artifact(), "trigger");
    let accepted = e.admit_event("host", event("stored", 2), 0).unwrap();
    let mut s = e.snapshot().unwrap();
    let revision = s.bindings["trigger"].revision;
    let legacy = digest(&(&accepted.key, "trigger", revision, "on-completed")).unwrap();
    s.events.get_mut(&accepted.key).unwrap().destinations[0].key = legacy.clone();
    overwrite(&e.store, "incarnation-1", &s);
    assert_eq!(e.fanout(&accepted.key, 10, 1).unwrap(), 1);
    let id = format!("event-{legacy}");
    assert!(event_run_id(&id));
    assert!(e.snapshot().unwrap().runs.contains_key(&id));
}
/// M4: a finished run's inspection items are byte-for-byte the same while it
/// is still live (its cohort waits for an independent child) and after its
/// cohort has left as history.
#[test]
fn a_finished_runs_inspection_is_identical_before_and_after_removal() {
    let e = engine();
    let child = wait_artifact();
    e.install(child.clone()).unwrap();
    install(&e, parent_artifact(&child, CallMode::Independent), "parent");
    let root = e
        .admit("parent", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 20).unwrap();
    assert!(e.snapshot().unwrap().runs[&root].status.terminal());
    let pages = |e: &Engine<MemoryStorage>| -> Vec<Vec<u8>> {
        [
            InspectionKind::Runs,
            InspectionKind::Effects,
            InspectionKind::Receipts,
        ]
        .into_iter()
        .map(|kind| serde_json::to_vec(&inspect_all(e, kind, Some(&root))).unwrap())
        .collect()
    };
    let live = pages(&e);
    assert!(live[2].len() > 2, "the root has receipts");
    e.admit_event("host", event("e1", 2), 5).unwrap();
    e.drive(&mut FakeHost::default(), 5, 20).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    assert_eq!(pages(&e), live);
}

// --- Task 6: window entries with expiry, dropped 64 per commit ------------

/// The window order of `s` as (expiry, member), oldest first.
fn window_entries(s: &State) -> Vec<(i64, String)> {
    s.retention
        .expiry
        .keys()
        .map(|k| {
            let (at, member) = retention::window_entry(k).unwrap();
            (at, member.to_string())
        })
        .collect()
}
fn ret_engine() -> Engine<MemoryStorage> {
    let e = engine();
    install(&e, done_artifact(), "a");
    e
}
fn keyed_on<S: Storage>(e: &Engine<S>, key: &str) -> KeyedAdmissionRequest {
    KeyedAdmissionRequest {
        key: key.into(),
        binding: "a".into(),
        expected_activation_revision: e.snapshot().unwrap().bindings["a"].revision,
        entry: "main".into(),
        input: Value::Null,
        authority: authority(),
    }
}
fn keyed(e: &Engine<MemoryStorage>, key: &str) -> KeyedAdmissionRequest {
    keyed_on(e, key)
}
#[test]
fn window_drop_is_bounded_to_64_per_commit_oldest_first() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..65 {
        let d = e.admit_once(keyed(&e, &format!("k-{i:03}")), i).unwrap();
        e.advance(&d.receipt.run_id, i).unwrap();
    }
    assert_eq!(e.snapshot().unwrap().keyed_admissions.len(), 65);
    e.admit_once(keyed(&e, "k-new"), 2_000).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(
        s.keyed_admissions.keys().cloned().collect::<Vec<_>>(),
        vec!["k-064".to_string(), "k-new".to_string()]
    );
}
#[test]
fn expired_event_leaves_without_tombstone_or_fence_and_redelivery_is_new() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_event("host", event("e1", 2), 0).unwrap(); // expires at max(1, 1_000)
    e.admit("waiter", "main", Value::Null, authority(), 1_001)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(!s.events.contains_key(&first.key));
    assert!(s.retention.event_tombstones.is_empty() && s.retention.expired_correlations.is_empty());
    assert!(
        !e.admit_event("host", event("e1", 2), 1_002)
            .unwrap()
            .duplicate
    );
}
#[test]
fn wait_registered_after_its_event_expired_times_out() {
    // NR-1 option (a)
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e.admit_event("host", event("e1", 2), 0).unwrap();
    let id = e
        .admit("waiter", "main", Value::Null, authority(), 1_001)
        .unwrap();
    e.advance(&id, 1_001).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Waiting);
    e.advance(&id, 1_101).unwrap();
    e.advance(&id, 1_102).unwrap();
    assert_eq!(runs_page(&e, Some(&id))[0]["outcome"], "error");
}
#[test]
fn keyed_admission_of_a_live_run_outlives_its_window() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_once(keyed(&e, "k"), 0).unwrap();
    e.admit_once(keyed(&e, "other"), 2_000).unwrap();
    let again = e.admit_once(keyed(&e, "k"), 2_001).unwrap();
    assert!(again.duplicate);
    assert_eq!(again.receipt.run_id, first.receipt.run_id);
}
#[test]
fn deferred_entries_do_not_block_later_due_entries() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
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
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&waiter, 0).unwrap(); // live wait, minimum revision 2
    e.admit_event("host", event("held", 1), 0).unwrap(); // revision 1: no match; held by the waiter; window 1_000
    for i in 0..65 {
        let d = e
            .admit_once(keyed(&e, &format!("k-{i:03}")), 1_000 + i)
            .unwrap();
        e.advance(&d.receipt.run_id, 1_000 + i).unwrap();
    }
    e.admit_once(keyed(&e, "x"), 3_000).unwrap(); // event deferred to 4_000 + 63 drops
    e.admit_once(keyed(&e, "y"), 3_001).unwrap(); // the remaining 2 drop; the event is not re-examined
    let s = e.snapshot().unwrap();
    assert_eq!(s.keyed_admissions.len(), 2);
    assert_eq!(s.events.len(), 1);
    assert_eq!(
        s.retention
            .expiry
            .keys()
            .next()
            .and_then(|k| retention::window_entry(k))
            .map(|(at, _)| at),
        Some(4_000)
    );
}
#[test]
fn backwards_host_time_neither_refuses_work_nor_shortens_windows() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e.admit("a", "main", Value::Null, authority(), 5_000)
        .unwrap();
    let d = e.admit_once(keyed(&e, "late"), 10).unwrap(); // backwards: accepted, not refused
    e.advance(&d.receipt.run_id, 11).unwrap();
    e.admit("a", "main", Value::Null, authority(), 12).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.retention.clock_ms, Some(5_000));
    assert_eq!(
        s.retention.admission_expires_at_ms.get("late").copied(),
        Some(6_000)
    );
    assert!(s.keyed_admissions.contains_key("late"));
}
/// An event of `wait_artifact`'s contract with its own correlation.
fn correlated_event(id: &str, invocation: &str) -> EventEnvelope {
    EventEnvelope {
        correlation: BTreeMap::from([("invocation".into(), invocation.into())]),
        ..event(id, 2)
    }
}
#[test]
fn burst_at_a_frozen_clock_leaves_at_most_2_kb_of_window_entries_per_run() {
    let e = ret_engine();
    e.install(wait_artifact()).unwrap();
    let mut after_first = 0;
    for i in 0..200 {
        let d = e
            .admit_once(keyed(&e, &format!("burst-{i:04}")), 0)
            .unwrap();
        e.advance(&d.receipt.run_id, 0).unwrap();
        e.admit_event(
            "host",
            correlated_event(&format!("burst-{i:04}"), &format!("inv-{i:04}")),
            0,
        )
        .unwrap();
        if i == 0 {
            after_first = canonical_bytes(&e.snapshot().unwrap()).unwrap().len();
        }
    }
    let s = e.snapshot().unwrap();
    let after_last = canonical_bytes(&s).unwrap().len();
    let per_run = (after_last - after_first) / 199;
    eprintln!("burst: {per_run} bytes per run");
    assert!(per_run <= 2_048, "{per_run} bytes per run");
    assert_eq!((s.keyed_admissions.len(), s.events.len()), (200, 200));
    assert!(
        s.runs.is_empty()
            && s.intents.is_empty()
            && s.receipts.is_empty()
            && s.waits.is_empty()
            && s.budgets.is_empty()
    );
}
/// The 64-drop script: 65 finished keyed runs with a 1 s window, then one
/// admission past it.
fn drop_script<S: Storage>(e: &Engine<S>) {
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..65 {
        let d = e.admit_once(keyed_on(e, &format!("k-{i:03}")), i).unwrap();
        e.advance(&d.receipt.run_id, i).unwrap();
    }
    e.admit_once(keyed_on(e, "k-new"), 2_000).unwrap();
}
#[test]
fn record_storage_drops_like_memory_storage() {
    let m = engine();
    let r = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
    install(&m, done_artifact(), "a");
    install_on(&r, done_artifact(), "a");
    drop_script(&m);
    let transitions = r
        .store
        .stats
        .transitions
        .load(std::sync::atomic::Ordering::Relaxed);
    drop_script(&r);
    assert!(
        r.store
            .stats
            .transitions
            .load(std::sync::atomic::Ordering::Relaxed)
            > transitions
    );
    assert_same_engines(&m, &r, "the 64-drop script");
    assert_eq!(r.snapshot().unwrap().keyed_admissions.len(), 2);
}
#[test]
fn commits_without_host_time_drop_nothing() {
    let e = ret_engine();
    drop_script(&e);
    // k-064 expired at 1_064; the clock is 2_000.
    assert!(e.snapshot().unwrap().keyed_admissions.contains_key("k-064"));
    e.install(wait_artifact()).unwrap();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 2_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.keyed_admissions.contains_key("k-064"));
    assert_eq!(s.retention.clock_ms, Some(2_000));
    e.admit("a", "main", Value::Null, authority(), 2_000)
        .unwrap();
    assert!(!e.snapshot().unwrap().keyed_admissions.contains_key("k-064"));
}
/// Carried from Task 4: a changed keyed receipt is always refused, so an
/// expired key admitted again leaves first and is then accepted as new.
fn expired_key_admitted_again_is_new<S: Storage>(e: &Engine<S>) {
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_once(keyed_on(e, "k"), 0).unwrap();
    assert!(e.advance(&first.receipt.run_id, 0).unwrap());
    // No commit between: the old receipt is still in the State, expired.
    assert!(e.snapshot().unwrap().keyed_admissions.contains_key("k"));
    // A new activation revision changes the request's content: after the
    // window that is not a conflict.
    e.activate("a", &done_artifact().digest, authority())
        .unwrap();
    let again = e.admit_once(keyed_on(e, "k"), 2_000).unwrap();
    assert!(!again.duplicate);
    assert_ne!(again.receipt.run_id, first.receipt.run_id);
    assert_eq!(again.receipt.accepted_at_ms, 2_000);
    let s = e.snapshot().unwrap();
    assert_eq!(s.keyed_admissions["k"], again.receipt);
    assert_eq!(
        s.retention.admission_expires_at_ms.get("k").copied(),
        Some(3_000)
    );
    assert_eq!(window_entries(&s), vec![(3_000, "k:k".to_string())]);
    // The first run is history.
    assert!(
        inspect_all(e, InspectionKind::Runs, Some(&first.receipt.run_id))
            .iter()
            .any(|r| r["status"] == "completed")
    );
}
#[test]
fn memory_storage_admits_an_expired_key_again_as_new() {
    expired_key_admitted_again_is_new(&ret_engine());
}
#[test]
fn record_storage_admits_an_expired_key_again_as_new() {
    let r = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
    install_on(&r, done_artifact(), "a");
    expired_key_admitted_again_is_new(&r);
}
#[test]
fn an_expired_event_delivered_again_before_its_drop_is_new() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_event("host", event("e1", 2), 0).unwrap();
    let again = e.admit_event("host", event("e1", 2), 2_000).unwrap();
    assert_eq!(again.key, first.key);
    assert!(!again.duplicate);
    let s = e.snapshot().unwrap();
    assert_eq!(s.events[&again.key].accepted_at_ms, 2_000);
    assert_eq!(
        s.retention.event_expires_at_ms.get(&again.key).copied(),
        Some(3_000)
    );
    assert_eq!(
        s.indexes.events.values().map(BTreeMap::len).sum::<usize>(),
        2
    );
}
#[test]
fn an_expired_event_held_by_a_waiter_stays_and_its_redelivery_is_a_duplicate() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&waiter, 0).unwrap();
    let held = e.admit_event("host", event("held", 1), 0).unwrap();
    e.admit("waiter", "main", Value::Null, authority(), 1_500)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.events.contains_key(&held.key));
    // Deferred by its own window (1_000) from the clock.
    assert_eq!(
        s.retention.event_expires_at_ms.get(&held.key).copied(),
        Some(2_500)
    );
    // Expired at 2_500 but still held: a duplicate, as inside its window.
    let again = e.admit_event("host", event("held", 1), 2_600).unwrap();
    assert!(again.duplicate);
    assert_eq!(again.key, held.key);
}
/// A trigger artifact whose trigger input does not fit its bounds: its
/// destination is blocked by input when the event is accepted.
fn input_blocked_trigger_artifact() -> Artifact {
    let mut a = trigger_artifact().content;
    // The input (`null`, 4 bytes) exceeds the playbook's payload bound, which
    // is checked when the event is accepted.
    a.playbook.limits.max_payload_bytes = 1;
    Artifact::seal(a).unwrap()
}
#[test]
fn a_queued_destination_holds_an_expired_event_and_an_input_blocked_one_does_not() {
    let e = engine();
    install(&e, trigger_artifact(), "trigger");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let queued = e
        .admit_event("host", correlated_event("queued", "q"), 0)
        .unwrap();
    e.admit_event("host", correlated_event("other", "o"), 1_500)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(
        s.events.contains_key(&queued.key),
        "a queued destination holds it"
    );
    assert_eq!(
        s.retention.event_expires_at_ms.get(&queued.key).copied(),
        Some(2_500)
    );

    let e = engine();
    install(&e, input_blocked_trigger_artifact(), "blocked");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let blocked = e
        .admit_event("host", correlated_event("blocked", "b"), 0)
        .unwrap();
    let owner = format!("event:{}", blocked.key);
    assert_eq!(
        e.snapshot()
            .unwrap()
            .receipts
            .iter()
            .filter(|r| r.run == owner)
            .count(),
        1
    );
    e.admit_event("host", correlated_event("other", "o"), 1_500)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(
        !s.events.contains_key(&blocked.key),
        "an input-blocked destination does not hold it"
    );
    // Its receipts went to history with it.
    assert!(s.receipts.iter().all(|r| r.run != owner));
    assert_eq!(
        inspect_all(&e, InspectionKind::Receipts, Some(&owner)).len(),
        1
    );
}
/// Cost (SC-5): a commit with nothing expired reads the same records however
/// many window entries there are, and a commit dropping k entries reads in
/// proportion to k; no window container is read whole.
#[test]
fn window_work_on_record_storage_depends_on_entries_dropped_not_on_entries_kept() {
    use std::sync::atomic::Ordering;
    let measure = |kept: usize, due: usize| {
        let e = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
        install_on(&e, done_artifact(), "a");
        e.set_retention_policy(RetentionPolicy {
            admission_replay_ms: 1_000,
            ..RetentionPolicy::default()
        })
        .unwrap();
        // Due entries expire at 1_000 + i, kept ones at 1_100 + i.
        for i in 0..due {
            let d = e
                .admit_once(keyed_on(&e, &format!("due-{i:03}")), i as i64)
                .unwrap();
            e.advance(&d.receipt.run_id, i as i64).unwrap();
        }
        for i in 0..kept {
            let d = e
                .admit_once(keyed_on(&e, &format!("kept-{i:03}")), 100 + i as i64)
                .unwrap();
            e.advance(&d.receipt.run_id, 100 + i as i64).unwrap();
        }
        let reads = e.store.stats.record_reads.load(Ordering::Relaxed);
        let fallbacks = e.store.stats.fallbacks().len();
        e.admit("a", "main", Value::Null, authority(), 1_050)
            .unwrap();
        let s = e.snapshot().unwrap();
        assert_eq!(s.keyed_admissions.len(), kept);
        let scanned: BTreeSet<String> = e.store.stats.fallbacks()[fallbacks..]
            .iter()
            .map(|f| f.split(' ').next().unwrap().to_string())
            .collect();
        (
            e.store.stats.record_reads.load(Ordering::Relaxed) - reads,
            scanned,
        )
    };
    let (none_few, scanned) = measure(20, 0);
    assert!(scanned.is_empty(), "{scanned:?}");
    let (none_many, _) = measure(80, 0);
    assert_eq!(none_few, none_many, "nothing expired");
    let (eight, scanned) = measure(20, 8);
    assert!(scanned.is_empty(), "{scanned:?}");
    let (eight_many, _) = measure(80, 8);
    assert_eq!(eight, eight_many, "8 dropped");
    let (sixteen, _) = measure(20, 16);
    assert_eq!(
        sixteen - eight,
        eight - none_few,
        "reads grow linearly with drops"
    );
    assert!(eight > none_few);
}
/// Events on both backends: one held by a live waiter (deferred), one
/// delivered again after its window (it leaves first, then is new), one
/// dropped with its index entries.
fn event_drop_script<S: Storage>(e: &Engine<S>) {
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&waiter, 0).unwrap();
    e.admit_event("host", event("held", 1), 0).unwrap();
    e.admit_event("host", correlated_event("again", "a"), 0)
        .unwrap();
    e.admit_event("host", correlated_event("dropped", "d"), 0)
        .unwrap();
    assert!(
        !e.admit_event("host", correlated_event("again", "a"), 1_500)
            .unwrap()
            .duplicate
    );
    e.admit("waiter", "main", Value::Null, authority(), 1_600)
        .unwrap();
}
#[test]
fn record_storage_drops_events_like_memory_storage() {
    let m = engine();
    let r = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
    install(&m, wait_artifact(), "waiter");
    install_on(&r, wait_artifact(), "waiter");
    event_drop_script(&m);
    let transitions = r
        .store
        .stats
        .transitions
        .load(std::sync::atomic::Ordering::Relaxed);
    event_drop_script(&r);
    assert!(
        r.store
            .stats
            .transitions
            .load(std::sync::atomic::Ordering::Relaxed)
            > transitions
    );
    assert_same_engines(&m, &r, "the event script");
    let s = r.snapshot().unwrap();
    // "held" (deferred to 2_500) and the second "again" (2_500) remain.
    assert_eq!(s.events.len(), 2);
    assert_eq!(
        window_entries(&s)
            .iter()
            .map(|(at, _)| *at)
            .collect::<Vec<_>>(),
        vec![2_500, 2_500]
    );
    assert_eq!(s.indexes.event_memberships.len(), 2);
    // Two correlation index keys each; the dropped event's are gone.
    assert_eq!(s.indexes.events.len(), 4);
}
/// SC-10 at admission: an expired key whose run is still live is a
/// duplicate even before the drop has deferred it.
#[test]
fn an_expired_key_of_a_live_run_is_still_a_duplicate() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_once(keyed(&e, "k"), 0).unwrap();
    let again = e.admit_once(keyed(&e, "k"), 2_000).unwrap();
    assert!(again.duplicate);
    assert_eq!(again.receipt, first.receipt);
    // An exact replay writes nothing; the next commit's drop defers it.
    let s = e.snapshot().unwrap();
    assert_eq!(
        s.retention.admission_expires_at_ms.get("k").copied(),
        Some(1_000)
    );
    e.admit("a", "main", Value::Null, authority(), 2_000)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(
        s.retention.admission_expires_at_ms.get("k").copied(),
        Some(3_000)
    );
}
#[test]
fn backwards_host_time_does_not_shorten_an_event_window() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e.admit("waiter", "main", Value::Null, authority(), 5_000)
        .unwrap();
    let late = e.admit_event("host", event("late", 2), 10).unwrap();
    assert!(!late.duplicate);
    let s = e.snapshot().unwrap();
    assert_eq!(s.events[&late.key].accepted_at_ms, 10);
    assert_eq!(
        s.retention.event_expires_at_ms.get(&late.key).copied(),
        Some(6_000)
    );
    assert!(window_entries(&s).contains(&(6_000, format!("e:{}", late.key))));
}
/// A window written with a host time behind the clock, but whose window
/// still reaches past it, counts from the clock.
#[test]
fn a_keyed_window_written_behind_the_clock_counts_from_the_clock() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e.admit("a", "main", Value::Null, authority(), 5_000)
        .unwrap();
    e.admit_once(keyed(&e, "behind"), 4_500).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.keyed_admissions["behind"].accepted_at_ms, 4_500);
    assert_eq!(
        s.retention.admission_expires_at_ms.get("behind").copied(),
        Some(6_000)
    );
    assert!(window_entries(&s).contains(&(6_000, "k:behind".to_string())));
}

// --- Task 6 fix round 1 ---------------------------------------------------

/// Review I1 (probe shape): an expired event that a live wait still holds,
/// delivered again with no commit in between, is a duplicate every time, as
/// it is inside its window. Changed content still conflicts.
#[test]
fn a_held_expired_event_delivered_again_is_a_duplicate() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&waiter, 0).unwrap();
    let held = e.admit_event("host", event("held", 1), 0).unwrap();
    for at in [999, 1_000, 5_000, 50_000] {
        let again = e.admit_event("host", event("held", 1), at).unwrap();
        assert!(again.duplicate, "at {at}");
        assert_eq!(again.key, held.key, "at {at}");
    }
    let changed = EventEnvelope {
        subject: Some("other".into()),
        ..event("held", 1)
    };
    assert!(matches!(
        e.admit_event("host", changed, 50_001),
        Err(Error::Conflict(_))
    ));
    assert!(e.snapshot().unwrap().events.contains_key(&held.key));
}
/// A trigger whose destination cannot be fanned out (its dependency bytes
/// are missing), so the accepted event keeps a queued destination.
fn missing_dependency_trigger_artifact() -> Artifact {
    let child = artifact(
        "missing-child",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
        Registry::default(),
        BTreeMap::new(),
    );
    let mut parent = parent_artifact(&child, CallMode::Wait).content;
    let c = event_contract();
    parent.registry.events.insert(c.name.clone(), c.clone());
    parent.playbook.triggers.push(Trigger {
        id: "trigger".into(),
        event: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        entry: "main".into(),
        input: Expr::literal(Value::Null),
    });
    Artifact::seal(parent).unwrap()
}
/// Review I1, aq-server's quantum: the outbox re-sends an event whose fanout
/// is not complete, then fans out and ticks. Once that event's window has
/// passed (it is held by its queued destination), the re-send must not stop
/// the quantum, so an unrelated run still progresses in it.
#[test]
fn redelivering_a_held_expired_event_does_not_stop_the_quantum() {
    let e = ret_engine();
    install(&e, missing_dependency_trigger_artifact(), "parent");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let held = e.admit_event("host", event("outbox", 2), 0).unwrap();
    assert_eq!(
        e.fanout(&held.key, 10, 0).unwrap(),
        0,
        "the destination stays queued"
    );
    let unrelated = e.admit("a", "main", Value::Null, authority(), 0).unwrap();
    // Event, then Fanout, then Tick, each failing the quantum on error.
    let quantum = |at: i64| -> Result<()> {
        let again = e.admit_event("host", event("outbox", 2), at)?;
        assert!(again.duplicate, "at {at}");
        e.fanout(&held.key, 10, at)?;
        e.drive(&mut FakeHost::default(), at, 20)?;
        Ok(())
    };
    quantum(5_000).unwrap();
    assert!(
        !e.snapshot().unwrap().runs.contains_key(&unrelated),
        "the unrelated run finished"
    );
    assert_eq!(runs_page(&e, Some(&unrelated))[0]["outcome"], "ok");
    quantum(50_000).unwrap();
    assert!(e.snapshot().unwrap().events.contains_key(&held.key));
}
/// Review I2: a held event is deferred by its window each time, never by its
/// age, so it leaves within one window after its need ends.
#[test]
fn an_event_held_for_many_windows_leaves_within_one_window_after_its_need_ends() {
    let e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&waiter, 0).unwrap();
    let held = e.admit_event("host", event("held", 1), 0).unwrap(); // window 1_000 (contract)
    let expiry = |e: &Engine<MemoryStorage>| {
        e.snapshot()
            .unwrap()
            .retention
            .event_expires_at_ms
            .get(&held.key)
            .copied()
    };
    assert_eq!(expiry(&e), Some(1_000));
    // A commit at each expiry defers it by one window.
    for k in 1..=10 {
        e.admit("a", "main", Value::Null, authority(), k * 1_000)
            .unwrap();
        assert_eq!(expiry(&e), Some((k + 1) * 1_000), "deferral {k}");
    }
    // The need ends: the wait times out and the waiter leaves.
    e.drive(&mut FakeHost::default(), 10_050, 40).unwrap();
    assert!(!e.snapshot().unwrap().runs.contains_key(&waiter));
    assert!(e.snapshot().unwrap().events.contains_key(&held.key));
    e.admit("a", "main", Value::Null, authority(), 11_000)
        .unwrap();
    assert!(!e.snapshot().unwrap().events.contains_key(&held.key));
}
/// NR-1, both halves under a short window (1_000): a wait registered inside
/// the window matches the earlier event; one registered at its expiry times
/// out, because the admitting commit drops the event first.
#[test]
fn a_wait_inside_the_window_matches_and_one_after_it_times_out() {
    for (at, outcome) in [(999, "ok"), (1_000, "error")] {
        let e = engine();
        install(&e, wait_artifact(), "waiter");
        e.set_retention_policy(RetentionPolicy {
            event_replay_ms: 1,
            ..RetentionPolicy::default()
        })
        .unwrap();
        e.admit_event("host", event("e1", 2), 0).unwrap();
        let id = e
            .admit("waiter", "main", Value::Null, authority(), at)
            .unwrap();
        e.drive(&mut FakeHost::default(), at, 20).unwrap();
        if outcome == "error" {
            assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Waiting);
            e.drive(&mut FakeHost::default(), at + 101, 20).unwrap();
        }
        assert_eq!(
            runs_page(&e, Some(&id))[0]["outcome"],
            outcome,
            "wait at {at}"
        );
    }
}

// --- Task 7: stale requests refused by producer time ----------------------

#[test]
fn stale_producer_time_is_refused_and_missing_time_is_accepted_as_new() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
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
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 600_000,
        event_replay_ms: 600_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let now = 1_000_000;
    assert!(stale(e.admit_once_at(
        keyed(&e, "k1"),
        Some(now - 600_001),
        now
    )));
    // Ruling 1: the cut-off millisecond itself is stale.
    assert!(stale(e.admit_once_at(
        keyed(&e, "k1"),
        Some(now - 600_000),
        now
    )));
    let first = e
        .admit_once_at(keyed(&e, "k1"), Some(now - 599_999), now)
        .unwrap();
    assert!(!first.duplicate);
    assert!(
        e.admit_once_at(keyed(&e, "k1"), Some(0), now + 1)
            .unwrap()
            .duplicate
    );
    assert!(stale(e.admit_event_at(
        "host",
        event("e1", 2),
        0,
        Some(now - 600_001),
        now
    )));
    assert!(stale(e.admit_event_at(
        "host",
        event("e1", 2),
        0,
        Some(now - 600_000),
        now
    )));
    assert!(
        !e.admit_event_at("host", event("e1", 2), 0, Some(now - 599_999), now)
            .unwrap()
            .duplicate
    );
    e.advance(&first.receipt.run_id, now + 2).unwrap();
    let later = now + 600_001;
    e.admit("a", "main", Value::Null, authority(), later)
        .unwrap(); // drops k1
    let again = e.admit_once_at(keyed(&e, "k1"), None, later + 1).unwrap();
    assert!(!again.duplicate);
    assert_ne!(again.receipt.run_id, first.receipt.run_id);
}
/// A stale refusal: the typed error, whose text starts with `STALE_REQUEST`.
fn stale(result: Result<impl std::fmt::Debug>) -> bool {
    matches!(result, Err(e @ Error::Stale { .. }) if e.to_string().starts_with(STALE_REQUEST))
}
/// Carried item 1: an expired key or event the engine still holds in the
/// record (no commit has dropped it yet) is forgotten by the first of two
/// commits. A stale request is refused before that commit: nothing is
/// written and the expired entry stays.
fn a_stale_request_for_an_expired_entry_writes_nothing<S: Storage>(
    e: &Engine<S>,
    event_first: bool,
) {
    install_on(e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1, // the event's window is the contract's 1_000
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_once(keyed_on(e, "k"), 0).unwrap();
    assert!(e.advance(&first.receipt.run_id, 0).unwrap());
    let accepted = e.admit_event("host", event("e1", 2), 0).unwrap();
    let before = e.store.load("test").unwrap();
    assert!(before.state.keyed_admissions.contains_key("k"));
    assert!(before.state.events.contains_key(&accepted.key));
    // Judged against max(2_000, clock 0) - 1_000 = 1_000, which is itself
    // stale (ruling 1).
    for requested in [999, 1_000] {
        assert!(stale(e.admit_once_at(
            keyed_on(e, "k"),
            Some(requested),
            2_000
        )));
        assert!(stale(e.admit_event_at(
            "host",
            event("e1", 2),
            0,
            Some(requested),
            2_000
        )));
    }
    let after = e.store.load("test").unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.state.keyed_admissions["k"], first.receipt);
    assert!(after.state.events.contains_key(&accepted.key));
    assert_eq!(
        after.state.retention.clock_ms,
        before.state.retention.clock_ms
    );
    // A request time inside the window: the expired entries leave and the
    // repeats are new. The first of the two is forgotten by its own request
    // (the forget-then-new path); the second was dropped by that commit.
    let keyed_again = || {
        let again = e
            .admit_once_at(keyed_on(e, "k"), Some(1_001), 2_000)
            .unwrap();
        assert!(!again.duplicate);
        assert_ne!(again.receipt.run_id, first.receipt.run_id);
    };
    let event_again = || {
        let redelivered = e
            .admit_event_at("host", event("e1", 2), 0, Some(1_001), 2_000)
            .unwrap();
        assert!(!redelivered.duplicate);
        assert_eq!(
            e.snapshot().unwrap().events[&redelivered.key].accepted_at_ms,
            2_000
        );
    };
    if event_first {
        event_again();
        keyed_again();
    } else {
        keyed_again();
        event_again();
    }
}
#[test]
fn memory_storage_stale_request_for_an_expired_entry_writes_nothing() {
    for event_first in [false, true] {
        a_stale_request_for_an_expired_entry_writes_nothing(&ret_engine(), event_first);
    }
}
#[test]
fn record_storage_stale_request_for_an_expired_entry_writes_nothing() {
    for event_first in [false, true] {
        let r = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
        install_on(&r, done_artifact(), "a");
        a_stale_request_for_an_expired_entry_writes_nothing(&r, event_first);
    }
}
/// Carried item 3: an entry the engine still remembers answers as today,
/// whatever the request time: inside its window, after it while its run is
/// live, and an expired event that something still holds.
#[test]
fn a_remembered_entry_answers_as_today_whatever_its_request_time() {
    let e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    // Inside the window.
    let first = e.admit_once(keyed(&e, "k"), 0).unwrap();
    let inside = e
        .admit_once_at(keyed(&e, "k"), Some(i64::MIN), 500)
        .unwrap();
    assert!(inside.duplicate);
    assert_eq!(inside.receipt, first.receipt);
    let changed = KeyedAdmissionRequest {
        input: json!(1),
        ..keyed(&e, "k")
    };
    assert!(matches!(
        e.admit_once_at(changed, Some(i64::MIN), 500),
        Err(Error::Conflict(_))
    ));
    // Expired, but its run is still live (SC-10).
    let live = e
        .admit_once_at(keyed(&e, "k"), Some(i64::MIN), 5_000)
        .unwrap();
    assert!(live.duplicate);
    assert_eq!(live.receipt, first.receipt);
    // An expired event held by a live waiter (Task 6 ruling).
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 5_000)
        .unwrap();
    e.advance(&waiter, 5_000).unwrap();
    let held = e.admit_event("host", event("held", 1), 5_000).unwrap();
    for at in [6_000, 50_000] {
        let again = e
            .admit_event_at("host", event("held", 1), 0, Some(i64::MIN), at)
            .unwrap();
        assert!(again.duplicate, "at {at}");
        assert_eq!(again.key, held.key, "at {at}");
    }
}
/// Request time is not part of any digest or stored record, and it never
/// moves the retention clock, even when it lies in the future. A future
/// request time is never stale, and it cannot make a remembered entry new.
#[test]
fn request_time_enters_no_digest_record_or_clock() {
    let script = |requested: Option<i64>| {
        let e = ret_engine();
        install(&e, wait_artifact(), "waiter");
        let k = e.admit_once_at(keyed(&e, "k"), requested, 1_000).unwrap();
        let ev = e
            .admit_event_at("host", event("e1", 2), 0, requested, 1_000)
            .unwrap();
        assert!(!k.duplicate && !ev.duplicate);
        // A remembered key stays a duplicate.
        assert!(
            e.admit_once_at(keyed(&e, "k"), requested, 1_001)
                .unwrap()
                .duplicate
        );
        assert!(
            e.admit_event_at("host", event("e1", 2), 0, requested, 1_001)
                .unwrap()
                .duplicate
        );
        (k, ev, e.snapshot().unwrap())
    };
    let (k_none, ev_none, s_none) = script(None);
    let (k_future, ev_future, s_future) = script(Some(i64::MAX));
    let (k_now, _, s_now) = script(Some(1_000));
    assert_eq!(k_future.receipt, k_none.receipt);
    assert_eq!(k_now.receipt, k_none.receipt);
    assert_eq!(ev_future.key, ev_none.key);
    // The last host time (an event duplicate commits), not the request time.
    assert_eq!(s_future.retention.clock_ms, Some(1_001));
    let json = |s: &State| serde_json::to_value(s).unwrap();
    assert_eq!(json(&s_future), json(&s_none));
    assert_eq!(json(&s_now), json(&s_none));
    // The oldest possible request time does not overflow the check.
    let e = ret_engine();
    assert!(stale(e.admit_once_at(
        keyed(&e, "x"),
        Some(i64::MIN),
        1_000
    )));
}
/// Carried item 2: staleness is judged against the retention clock, as
/// expiry is. One far-future host time pins the clock, and then every
/// request with a real request time is refused as stale; the message names
/// both times. Requests without a request time still go through.
#[test]
fn a_pinned_clock_refuses_every_request_time_and_says_so() {
    let e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    let pinned: i64 = 9_000_000_000_000;
    let real: i64 = 1_700_000_000_000;
    e.admit("a", "main", Value::Null, authority(), pinned)
        .unwrap();
    for result in [
        e.admit_once_at(keyed(&e, "k"), Some(real), real)
            .map(|_| ()),
        e.admit_event_at("host", event("e1", 2), 0, Some(real), real)
            .map(|_| ()),
    ] {
        let Err(error) = result else {
            panic!("expected a stale refusal, got {result:?}");
        };
        let Error::Stale {
            requested_at_ms,
            clock_ms,
            now_ms,
            ..
        } = error
        else {
            panic!("expected Error::Stale, got {error:?}");
        };
        assert_eq!(
            (requested_at_ms, clock_ms, now_ms),
            (real, Some(pinned), real)
        );
        let message = error.to_string();
        assert!(message.starts_with(STALE_REQUEST), "{message}");
        assert!(message.contains(&real.to_string()), "{message}");
        assert!(message.contains(&pinned.to_string()), "{message}");
        assert!(message.contains("retention clock"), "{message}");
        assert!(message.contains("ahead of host time"), "{message}");
    }
    assert!(
        !e.admit_once_at(keyed(&e, "k"), None, real)
            .unwrap()
            .duplicate
    );
    assert!(
        !e.admit_event_at("host", event("e1", 2), 0, None, real)
            .unwrap()
            .duplicate
    );
    // The existing methods carry no request time, so they are not checked.
    assert!(!e.admit_once(keyed(&e, "k2"), real).unwrap().duplicate);
    assert!(
        !e.admit_event("host", event("e2", 2), real)
            .unwrap()
            .duplicate
    );
    assert_eq!(e.snapshot().unwrap().retention.clock_ms, Some(pinned));
}
/// Review I1 (ruling 1): a key accepted at window start S expires at S + w.
/// A retry carrying its original request time S that lands exactly on the
/// expiry millisecond is stale, not admitted a second time.
#[test]
fn a_retry_at_the_expiry_millisecond_is_stale_not_new() {
    let e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1, // the event's window is the contract's 1_000
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_once_at(keyed(&e, "k"), Some(1_000), 1_000).unwrap();
    assert!(e.advance(&first.receipt.run_id, 1_000).unwrap());
    let accepted = e
        .admit_event_at("host", event("e1", 2), 0, Some(1_000), 1_000)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(
        s.retention.admission_expires_at_ms.get("k").copied(),
        Some(2_000)
    );
    assert_eq!(
        s.retention.event_expires_at_ms.get(&accepted.key).copied(),
        Some(2_000)
    );
    // At 2_000 both are expired and unheld: not remembered, and the retry
    // is stale.
    assert!(stale(e.admit_once_at(keyed(&e, "k"), Some(1_000), 2_000)));
    assert!(stale(e.admit_event_at(
        "host",
        event("e1", 2),
        0,
        Some(1_000),
        2_000
    )));
    let s = e.snapshot().unwrap();
    assert_eq!(s.keyed_admissions["k"], first.receipt);
    assert!(s.events.contains_key(&accepted.key));
    // One millisecond earlier they are still remembered: duplicates.
    let e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = e.admit_once_at(keyed(&e, "k"), Some(1_000), 1_000).unwrap();
    e.advance(&first.receipt.run_id, 1_000).unwrap();
    e.admit_event_at("host", event("e1", 2), 0, Some(1_000), 1_000)
        .unwrap();
    assert!(
        e.admit_once_at(keyed(&e, "k"), Some(1_000), 1_999)
            .unwrap()
            .duplicate
    );
    assert!(
        e.admit_event_at("host", event("e1", 2), 0, Some(1_000), 1_999)
            .unwrap()
            .duplicate
    );
}
/// Ruling 2: hosts match the variant, which carries the request time, the
/// clock, the host time, the window and the cut-off.
#[test]
fn a_stale_refusal_is_typed_and_names_its_times() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e.admit("a", "main", Value::Null, authority(), 5_000)
        .unwrap();
    let error = e
        .admit_once_at(keyed(&e, "k"), Some(3_000), 4_500)
        .unwrap_err();
    let Error::Stale {
        requested_at_ms,
        cutoff_ms,
        clock_ms,
        now_ms,
        window_ms,
        floor_ms,
    } = error
    else {
        panic!("expected Error::Stale, got {error:?}");
    };
    assert_eq!(
        (
            requested_at_ms,
            cutoff_ms,
            clock_ms,
            now_ms,
            window_ms,
            floor_ms
        ),
        (3_000, 4_000, Some(5_000), 4_500, 1_000, None)
    );
    let message = error.to_string();
    assert!(message.starts_with(STALE_REQUEST), "{message}");
    for part in ["3000", "4000", "5000", "4500", "1000"] {
        assert!(message.contains(part), "{part}: {message}");
    }
    assert!(!matches!(error, Error::Refused(_)));
}

// --- Task 7 round 1, ruling 3: rebasing a pinned retention clock ----------

/// The pinned-clock scenario (ruling 3) on any backend. Windows: keyed
/// 1_000, events max(1, contract 1_000). Real time starts at `t0`.
fn pinned_clock_is_rebased_and_normal_operation_resumes<S: Storage>(e: &Engine<S>) {
    install_on(e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let t0: i64 = 10_000;
    let pinned: i64 = t0 + 1_000_000_000;
    // Normal operation: a finished keyed run, a keyed run still live, a
    // waiter holding an event, and an unheld event.
    let done = e.admit_once_at(keyed_on(e, "done"), Some(t0), t0).unwrap();
    assert!(e.advance(&done.receipt.run_id, t0).unwrap());
    let live = e.admit_once_at(keyed_on(e, "live"), Some(t0), t0).unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), t0)
        .unwrap();
    e.advance(&waiter, t0).unwrap();
    let held = e
        .admit_event_at("host", event("held", 1), 0, Some(t0), t0)
        .unwrap();
    e.admit_event_at(
        "host",
        correlated_event("free", "inv-free"),
        0,
        Some(t0),
        t0,
    )
    .unwrap();
    // One far-future host time pins the clock. Its drop forgets "done" and
    // "free" a whole window early; "live" and "held" are deferred from the
    // pin.
    e.admit("a", "main", Value::Null, authority(), pinned)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.retention.clock_ms, Some(pinned));
    assert!(!s.keyed_admissions.contains_key("done"));
    assert_eq!(s.events.len(), 1);
    assert_eq!(
        s.retention.admission_expires_at_ms.get("live").copied(),
        Some(pinned + 1_000)
    );
    // Back in real time: every request that carries a request time is
    // stale, and the refusal writes nothing.
    let t1 = t0 + 200;
    let before = e.snapshot().unwrap();
    let refused = e
        .admit_once_at(keyed_on(e, "during"), Some(t1), t1)
        .unwrap_err();
    assert!(
        matches!(refused, Error::Stale { clock_ms: Some(c), now_ms, .. } if c == pinned && now_ms == t1)
    );
    assert!(refused.to_string().contains("ahead of host time"));
    assert!(stale(e.admit_event_at(
        "host",
        correlated_event("during-e", "inv-during"),
        0,
        Some(t1),
        t1
    )));
    let json = |s: &State| serde_json::to_value(s).unwrap();
    assert_eq!(json(&e.snapshot().unwrap()), json(&before));
    // Requests without a request time go through, and what they write
    // expires from the pin: the record only grows.
    for i in 0..5 {
        let d = e
            .admit_once(keyed_on(e, &format!("pin-{i}")), t1 + i)
            .unwrap();
        e.advance(&d.receipt.run_id, t1 + i).unwrap();
    }
    // An unheld event accepted during the pin, of a contract whose window
    // (1_000) is longer than the policy's (1).
    let pin_event = e
        .admit_event("host", correlated_event("pin-e", "inv-pin"), t1)
        .unwrap();
    e.admit("a", "main", Value::Null, authority(), t1 + 5_000)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(
        s.keyed_admissions.len(),
        6,
        "pin-0..4 and live never expire"
    );
    assert!(
        s.retention
            .admission_expires_at_ms
            .get("pin-0")
            .is_some_and(|at| *at > pinned)
    );
    // The rebase: refused unless now <= to <= now + slack (0 here: a tenth
    // of the smallest window, 1 ms) and the floor is in the past.
    let t2 = t0 + 500;
    for (to, floor, now) in [
        (pinned, t0, t2),
        (pinned + 1, t0, t2),
        (t2 - 1, t0, t2),
        (t2 + 1, t0, t2),
        (t2, t2 + 1, t2),
    ] {
        assert!(
            matches!(
                e.rebase_retention_clock(to, floor, "ops", "pinned clock", now),
                Err(Error::Refused(_))
            ),
            "to {to} floor {floor} now {now}"
        );
    }
    assert!(
        e.rebase_retention_clock(t2, t0, "", "pinned clock", t2)
            .is_err()
    );
    assert_eq!(json(&e.snapshot().unwrap()), json(&s));
    // A dry run reports the same and commits nothing.
    let preview = e
        .rebase_retention_clock_dry_run(t2, t0, "ops", "pinned clock", t2)
        .unwrap();
    assert_eq!(json(&e.snapshot().unwrap()), json(&s));
    let report = e
        .rebase_retention_clock(t2, t0, "ops", "pinned clock", t2)
        .unwrap();
    assert_eq!(preview, report);
    assert_eq!(
        (report.from_ms, report.to_ms, report.stale_floor_ms),
        (pinned, t2, t0)
    );
    assert_eq!((report.capped_admissions, report.capped_events), (6, 2));
    // The run admitted at the pinned time has its duration basis capped.
    assert!(report.needs_operator_action.is_empty());
    assert!(!report.adjusted_runs.is_empty());
    let s = e.snapshot().unwrap();
    assert_eq!(s.retention.clock_ms, Some(t2));
    assert_eq!(s.retention.stale_floor_ms, Some(t0));
    for key in ["live", "pin-0", "pin-4"] {
        assert_eq!(
            s.retention.admission_expires_at_ms.get(key).copied(),
            Some(t2 + 1_000),
            "{key}"
        );
    }
    assert_eq!(
        s.retention.event_expires_at_ms.get(&held.key).copied(),
        Some(t2 + 1_000)
    );
    // Capped by its contract's window, not the policy's.
    assert_eq!(
        s.retention.event_expires_at_ms.get(&pin_event.key).copied(),
        Some(t2 + 1_000)
    );
    assert!(window_entries(&s).iter().all(|(at, _)| *at <= t2 + 1_000));
    for id in &report.adjusted_runs {
        assert_eq!(s.runs[id].admitted_at_ms, t2, "{id}");
    }
    // The receipt is history: who, when, the old and new clock.
    let receipts = inspect_all(e, InspectionKind::Receipts, Some("admin"));
    let rebase = receipts
        .iter()
        .find(|r| r["kind"] == retention::CLOCK_REBASE)
        .expect("rebase receipt in history");
    assert_eq!(rebase["at_ms"], t2);
    assert!(s.receipts.iter().all(|r| r.kind != retention::CLOCK_REBASE));
    // Normal operation resumes. A request time is judged against real time
    // again: one created during the pin is new now.
    let during = e
        .admit_once_at(keyed_on(e, "during"), Some(t1), t2)
        .unwrap();
    assert!(!during.duplicate);
    assert!(
        !e.admit_event_at(
            "host",
            correlated_event("during-e", "inv-during"),
            0,
            Some(t1),
            t2
        )
        .unwrap()
        .duplicate
    );
    // Duplicates inside their window are still recognised.
    assert!(
        e.admit_once_at(keyed_on(e, "during"), Some(t1), t2 + 10)
            .unwrap()
            .duplicate
    );
    assert!(
        e.admit_once_at(keyed_on(e, "pin-0"), Some(t1), t2 + 10)
            .unwrap()
            .duplicate
    );
    // Round 3: after an ordinary commit, a key still inside its (capped)
    // window is still remembered.
    e.admit("a", "main", Value::Null, authority(), t2 + 500)
        .unwrap();
    assert!(
        e.admit_once_at(keyed_on(e, "pin-0"), Some(t1), t2 + 999)
            .unwrap()
            .duplicate
    );
    // An event inside its contract window is still a duplicate.
    assert!(
        e.admit_event("host", correlated_event("pin-e", "inv-pin"), t2 + 10)
            .unwrap()
            .duplicate
    );
    // Held events and live runs are intact.
    assert!(
        e.admit_event_at("host", event("held", 1), 0, Some(t0), t2 + 10)
            .unwrap()
            .duplicate
    );
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&waiter].status, RunStatus::Waiting);
    assert!(s.runs.contains_key(&live.receipt.run_id));
    assert!(
        e.admit_once_at(keyed_on(e, "live"), Some(t0), t2 + 10)
            .unwrap()
            .duplicate
    );
    // The floor: "done" was forgotten a window early by the pin. A retry
    // with its original request time is stale, not a second run. Without a
    // request time it is new, as for any key the engine has forgotten.
    let floor = e
        .admit_once_at(keyed_on(e, "done"), Some(t0), t2 + 10)
        .unwrap_err();
    assert!(
        matches!(floor, Error::Stale { floor_ms: Some(f), cutoff_ms, .. } if f == t0 && cutoff_ms == t0)
    );
    assert!(stale(e.admit_event_at(
        "host",
        correlated_event("free", "inv-free"),
        0,
        Some(t0),
        t2 + 10
    )));
    let again = e.admit_once(keyed_on(e, "done"), t2 + 10).unwrap();
    assert!(!again.duplicate);
    assert_ne!(again.receipt.run_id, done.receipt.run_id);
    // The record stops growing: what the pin wrote expires in real time.
    for run in [
        &live.receipt.run_id,
        &during.receipt.run_id,
        &again.receipt.run_id,
    ] {
        assert!(e.advance(run, t2 + 20).unwrap());
    }
    for i in 0..10 {
        let at = t2 + 1_001 + i * 200;
        let d = e
            .admit_once_at(keyed_on(e, &format!("after-{i}")), Some(at), at)
            .unwrap();
        e.advance(&d.receipt.run_id, at).unwrap();
    }
    let s = e.snapshot().unwrap();
    let mut keys: Vec<String> = s.keyed_admissions.keys().cloned().collect();
    keys.sort();
    assert_eq!(
        keys,
        (5..10).map(|i| format!("after-{i}")).collect::<Vec<_>>(),
        "only the last window's keys stay"
    );
    assert!(window_entries(&s).len() <= 7, "{:?}", window_entries(&s));
}
#[test]
fn memory_storage_pinned_clock_is_rebased_and_normal_operation_resumes() {
    pinned_clock_is_rebased_and_normal_operation_resumes(&ret_engine());
}
#[test]
fn record_storage_pinned_clock_is_rebased_and_normal_operation_resumes() {
    let r = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
    install_on(&r, done_artifact(), "a");
    pinned_clock_is_rebased_and_normal_operation_resumes(&r);
}
/// Ruling 2 (round 2): a mistaken rebase cannot do lasting damage. It
/// needs a clock pinned by more than the smallest window, a target from
/// host time to the slack after it, a floor in the past, an actor and a
/// reason; a second rebase never lowers the floor; and a floor set too high
/// can be lowered with its own receipt.
#[test]
fn rebase_preconditions_and_the_floor() {
    let e = ret_engine();
    // Default windows: 1 day and 7 days, so the slack is one minute.
    assert!(matches!(
        e.rebase_retention_clock(10, 5, "ops", "r", 10),
        Err(Error::Refused(_))
    ));
    e.admit("a", "main", Value::Null, authority(), 5_000)
        .unwrap();
    // Not pinned: ahead by 1 s, less than the smallest window.
    let not_pinned = e
        .rebase_retention_clock(4_000, 3_000, "ops", "r", 4_000)
        .unwrap_err();
    assert!(
        not_pinned.to_string().contains("not pinned"),
        "{not_pinned}"
    );
    let pinned = 1_000_000_000_000;
    e.admit("a", "main", Value::Null, authority(), pinned)
        .unwrap();
    let now = 1_000_000;
    for (to, floor, actor, reason) in [
        (now + 60_001, now - 1, "ops", "r"),
        (now - 1, now - 1, "ops", "r"),
        (now, now + 1, "ops", "r"),
        // A floor after the host time, though not after the target.
        (now + 60_000, now + 1, "ops", "r"),
        (now, now - 1, "ops", ""),
        (now, now - 1, "", "r"),
    ] {
        assert!(
            matches!(
                e.rebase_retention_clock(to, floor, actor, reason, now),
                Err(Error::Refused(_))
            ),
            "to {to} floor {floor}"
        );
    }
    let report = e
        .rebase_retention_clock(now + 60_000, now - 100, "ops", "r", now)
        .unwrap();
    assert_eq!(report.stale_floor_ms, now - 100);
    // A second pin and rebase with a lower floor keeps the higher one.
    e.admit("a", "main", Value::Null, authority(), pinned)
        .unwrap();
    let later = now + 100_000;
    let report = e
        .rebase_retention_clock(later, now - 500, "ops", "r", later)
        .unwrap();
    assert_eq!(report.stale_floor_ms, now - 100);
    // Lowering the floor: only below the current one, with its own receipt,
    // and nothing else changes.
    // Not below the current floor: refused by the engine itself.
    for to in [now - 100, now] {
        assert!(matches!(
            e.lower_stale_floor(to, "ops", "r", later),
            Err(Error::Refused(_))
        ));
    }
    assert!(e.lower_stale_floor(now - 200, "ops", "", later).is_err());
    let before = e.snapshot().unwrap();
    assert_eq!(
        e.lower_stale_floor(now - 200, "ops", "too high", later)
            .unwrap(),
        (now - 100, now - 200)
    );
    let s = e.snapshot().unwrap();
    assert_eq!(s.retention.stale_floor_ms, Some(now - 200));
    assert_eq!(s.retention.clock_ms, before.retention.clock_ms);
    assert_eq!(s.runs.len(), before.runs.len());
    let receipts = inspect_all(&e, InspectionKind::Receipts, Some("admin"));
    assert!(
        receipts
            .iter()
            .any(|r| r["kind"] == retention::STALE_FLOOR_LOWERED)
    );
    // No floor to lower.
    let fresh = ret_engine();
    assert!(fresh.lower_stale_floor(0, "ops", "r", 1).is_err());
}
/// Ruling 3 (round 2): a wait registered at the pinned time gets its
/// deadline capped at the target plus its declared duration, so it times
/// out normally after the rebase, and the run settles and leaves the
/// record. An operation deadline set then is capped the same way.
#[test]
fn a_wait_registered_at_the_pinned_time_times_out_after_the_rebase() {
    let e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    install(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let pinned: i64 = 1_000_000_000;
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), pinned)
        .unwrap();
    e.advance(&waiter, pinned).unwrap();
    let invoke = e
        .admit("invoke", "main", Value::Null, authority(), pinned)
        .unwrap();
    e.advance(&invoke, pinned).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&waiter].status, RunStatus::Waiting);
    let wait = s.waits.values().find(|w| w.run == waiter).unwrap().clone();
    assert_eq!(wait.deadline, pinned + 100);
    let intent = s
        .intents
        .values()
        .find(|i| i.run == invoke)
        .unwrap()
        .clone();
    assert_eq!(intent.deadline_at_ms, pinned + 100);
    let t2: i64 = 10_000;
    let report = e
        .rebase_retention_clock(t2, t2 - 1_000, "ops", "pinned clock", t2)
        .unwrap();
    // Each run's duration basis, the wait and the operation deadline.
    assert_eq!(report.capped_deadlines, 4);
    assert_eq!(report.adjusted_runs, {
        let mut ids = vec![waiter.clone(), invoke.clone()];
        ids.sort();
        ids
    });
    assert!(report.needs_operator_action.is_empty());
    let s = e.snapshot().unwrap();
    assert_eq!(s.waits[&wait.id].deadline, t2 + 100);
    assert_eq!(s.intents[&intent.id].deadline_at_ms, t2 + 100);
    assert_eq!(s.runs[&waiter].admitted_at_ms, t2);
    // The receipt (in full, as the commit wrote it) names the runs.
    let rebase = e
        .store
        .settled_cohorts("test")
        .into_iter()
        .flat_map(|c| c.receipts)
        .find(|r| r.kind == retention::CLOCK_REBASE)
        .unwrap();
    assert_eq!(rebase.detail["capped_deadlines"], 4);
    // N2 (round 3): each capped value with its original, for audit.
    let adjustment =
        |run: &str, kind: &str, item: &str, original_ms: i64, capped_ms: i64| DeadlineAdjustment {
            run: run.into(),
            kind: kind.into(),
            item: item.into(),
            original_ms,
            capped_ms,
        };
    let mut expected = vec![
        adjustment(&waiter, "duration_basis", &waiter, pinned, t2),
        adjustment(&waiter, "wait", &wait.id, pinned + 100, t2 + 100),
        adjustment(&invoke, "duration_basis", &invoke, pinned, t2),
        adjustment(&invoke, "operation", &intent.id, pinned + 100, t2 + 100),
    ];
    expected.sort_by(|a, b| (&a.run, &a.kind).cmp(&(&b.run, &b.kind)));
    let mut got = report.adjustments.clone();
    got.sort_by(|a, b| (&a.run, &a.kind).cmp(&(&b.run, &b.kind)));
    assert_eq!(got, expected);
    assert_eq!(report.adjustments_not_listed, 0);
    assert_eq!(rebase.detail["adjustments"], json!(report.adjustments));
    assert_eq!(rebase.detail["adjusted_runs"], json!(report.adjusted_runs));
    assert_eq!(rebase.detail["needs_operator_action"], json!([]));
    // Before its deadline the wait still waits; after it, it times out, the
    // run settles and leaves the record.
    e.drive(&mut FakeHost::default(), t2 + 99, 20).unwrap();
    assert_eq!(
        e.snapshot().unwrap().runs[&waiter].status,
        RunStatus::Waiting
    );
    e.drive(&mut FakeHost::default(), t2 + 101, 20).unwrap();
    assert!(!e.snapshot().unwrap().runs.contains_key(&waiter));
    assert_eq!(runs_page(&e, Some(&waiter))[0]["outcome"], "error");
}
/// A deadline whose declared duration cannot be found (here a retry on a
/// step with no retry policy, written directly) is left as it is and its
/// run listed for the operator.
#[test]
fn a_deadline_without_its_declared_duration_needs_operator_action() {
    let e = ret_engine();
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let pinned: i64 = 1_000_000_000;
    let run = e
        .admit("a", "main", Value::Null, authority(), pinned)
        .unwrap();
    let mut s = e.snapshot().unwrap();
    {
        let mut r = s.runs[&run].clone();
        r.status = RunStatus::Waiting;
        r.pending = Some(Pending::Retry {
            deadline: pinned + 5_000,
        });
        s.runs.insert(run.clone(), r);
    }
    overwrite(&e.store, "incarnation-1", &s);
    let report = e
        .rebase_retention_clock(10_000, 9_000, "ops", "pinned clock", 10_000)
        .unwrap();
    assert_eq!(report.needs_operator_action, vec![run.clone()]);
    let s = e.snapshot().unwrap();
    assert!(matches!(
        s.runs[&run].pending,
        Some(Pending::Retry { deadline }) if deadline == pinned + 5_000
    ));
}
/// A business retry scheduled at the pinned time is capped at the target
/// plus its declared backoff.
#[test]
fn a_retry_scheduled_at_the_pinned_time_is_capped_by_its_backoff() {
    let e = ret_engine();
    install(
        &e,
        invoke_artifact(
            command(Recovery::KeyedIdempotent),
            Some(RetryPolicy {
                max_attempts: 2,
                backoff_ms: 10_000,
                outcomes: set(&["retry"]),
            }),
        ),
        "invoke",
    );
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let pinned: i64 = 1_000_000_000;
    let id = e
        .admit("invoke", "main", Value::Null, authority(), pinned)
        .unwrap();
    e.advance(&id, pinned).unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![DispatchResult::Completed {
                outcome: "retry".into(),
                output: Value::Null,
            }],
        )]),
        ..Default::default()
    };
    e.dispatch_next(&mut host, pinned).unwrap();
    e.advance(&id, pinned).unwrap();
    assert!(matches!(
        e.snapshot().unwrap().runs[&id].pending,
        Some(Pending::Retry { deadline }) if deadline == pinned + 10_000
    ));
    let t2: i64 = 10_000;
    let report = e
        .rebase_retention_clock(t2, t2 - 1, "ops", "pinned clock", t2)
        .unwrap();
    assert!(report.adjusted_runs.contains(&id));
    assert!(matches!(
        e.snapshot().unwrap().runs[&id].pending,
        Some(Pending::Retry { deadline }) if deadline == t2 + 10_000
    ));
}
/// A join set at the pinned time (written directly: its step's declared
/// deadline is 5_000) is capped at the target plus that duration.
#[test]
fn a_join_set_at_the_pinned_time_is_capped_by_its_declared_deadline() {
    let e = engine();
    install(
        &e,
        artifact(
            "joiner",
            "join",
            BTreeMap::from([
                (
                    "join".into(),
                    Step {
                        op: Operation::Join {
                            member_keys: vec![],
                            members: vec![Expr::literal(json!({"child": "x"}))],
                            mode: JoinMode::All,
                            accepted_outcomes: set(&["ok"]),
                            deadline_ms: 5_000,
                            remaining: RemainingPolicy::Cancel,
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
            Registry::default(),
            BTreeMap::new(),
        ),
        "joiner",
    );
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let pinned: i64 = 1_000_000_000;
    let run = e
        .admit("joiner", "main", Value::Null, authority(), pinned)
        .unwrap();
    let mut s = e.snapshot().unwrap();
    {
        let mut r = s.runs[&run].clone();
        r.status = RunStatus::Waiting;
        r.pending = Some(Pending::Join {
            members: vec![],
            member_keys: vec![],
            mode: JoinMode::All,
            accepted: set(&["ok"]),
            deadline: pinned + 5_000,
            remaining: RemainingPolicy::Cancel,
            bind: "joined".into(),
        });
        s.runs.insert(run.clone(), r);
    }
    overwrite(&e.store, "incarnation-1", &s);
    let report = e
        .rebase_retention_clock(10_000, 9_000, "ops", "pinned clock", 10_000)
        .unwrap();
    assert!(report.adjusted_runs.contains(&run));
    assert!(report.needs_operator_action.is_empty());
    assert!(matches!(
        e.snapshot().unwrap().runs[&run].pending,
        Some(Pending::Join { deadline, .. }) if deadline == 15_000
    ));
}
/// The owned-operation join fixture of `semantics.rs`.
fn async_artifact() -> (Artifact, EventContract) {
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
    let mut c = command(Recovery::Reconcilable);
    c.cancellation = true;
    c.outcomes.insert("accepted".into(), admission);
    c.asynchronous = Some(AsyncContract {
        completion_event: ev.name.clone(),
        completion,
        correlation_field: "operation".into(),
        reconciliation_command: "operation.get".into(),
    });
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
        Registry {
            commands: BTreeMap::from([(c.name.clone(), c), (get.name.clone(), get)]),
            events: BTreeMap::from([(ev.name.clone(), ev.clone())]),
        },
        BTreeMap::new(),
    );
    (a, ev)
}
/// N4 (round 3): a join driven at the pinned time (an accepted operation,
/// then `Join` with a 100 ms deadline) is capped at the target plus that
/// deadline.
#[test]
fn a_driven_join_at_the_pinned_time_is_capped_by_its_declared_deadline() {
    let e = ret_engine();
    let (a, _) = async_artifact();
    install(&e, a, "async");
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let pinned: i64 = 1_000_000_000;
    let id = e
        .admit("async", "main", Value::Null, authority(), pinned)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![DispatchResult::Accepted {
                handle: "server-operation".into(),
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, pinned, 10).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].step, "join");
    assert!(matches!(
        s.runs[&id].pending,
        Some(Pending::Join { deadline, .. }) if deadline == pinned + 100
    ));
    let report = e
        .rebase_retention_clock(10_000, 9_000, "ops", "pinned clock", 10_000)
        .unwrap();
    assert!(report.adjusted_runs.contains(&id));
    assert!(matches!(
        e.snapshot().unwrap().runs[&id].pending,
        Some(Pending::Join { deadline, .. }) if deadline == 10_100
    ));
}

// --- Task 8: quotas count live work

#[test]
fn max_runs_counts_live_runs_only() {
    let e = Engine::with_config(
        MemoryStorage::retaining_settled(),
        "test",
        "i-1",
        EngineConfig {
            max_runs: 2,
            ..EngineConfig::default()
        },
    )
    .unwrap();
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
    for t in 0..5 {
        let id = e.admit("a", "main", Value::Null, authority(), t).unwrap();
        e.advance(&id, t).unwrap();
    }
    e.admit("a", "main", Value::Null, authority(), 10).unwrap();
    e.admit("a", "main", Value::Null, authority(), 11).unwrap();
    assert!(
        matches!(e.admit("a", "main", Value::Null, authority(), 12), Err(Error::Refused(m)) if m.contains("quota"))
    );
}
#[test]
fn max_receipts_counts_the_window_and_frees_when_it_passes() {
    let e = Engine::with_config(
        MemoryStorage::retaining_settled(),
        "test",
        "i-1",
        EngineConfig {
            // Deviation from the brief (10): the ten window receipts plus
            // the receipt of the run each keyed admission starts.
            max_receipts: 11,
            ..EngineConfig::default()
        },
    )
    .unwrap();
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
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..10 {
        let d = e.admit_once(keyed(&e, &format!("k{i}")), i).unwrap();
        e.advance(&d.receipt.run_id, i).unwrap();
    }
    assert!(
        matches!(e.admit_once(keyed(&e, "k10"), 10), Err(Error::Refused(m)) if m.contains("quota"))
    );
    assert!(!e.admit_once(keyed(&e, "k10"), 2_000).unwrap().duplicate);
}
#[test]
fn max_tombstones_and_terminal_run_ms_are_accepted_and_ignored() {
    let e = engine();
    e.set_retention_policy(RetentionPolicy {
        max_tombstones: 0,
        terminal_run_ms: 0,
        ..RetentionPolicy::default()
    })
    .unwrap();
    assert_eq!(e.snapshot().unwrap().retention.policy.max_tombstones, 0);
}
#[test]
fn retention_windows_must_be_positive() {
    let e = engine();
    for policy in [
        RetentionPolicy {
            event_replay_ms: 0,
            ..RetentionPolicy::default()
        },
        RetentionPolicy {
            admission_replay_ms: 0,
            ..RetentionPolicy::default()
        },
        RetentionPolicy {
            audit_ms: 0,
            ..RetentionPolicy::default()
        },
    ] {
        assert!(
            matches!(e.set_retention_policy(policy), Err(Error::Refused(m)) if m == "retention windows must be positive")
        );
    }
}
/// A refusal names the quota that is full, what it counts and its limit, in
/// the counts the refused commit would have held; it writes nothing.
#[test]
fn a_quota_refusal_names_what_is_full() {
    type Step = dyn Fn(&Engine<MemoryStorage>, i64) -> Result<()>;
    let refusal = |config: EngineConfig, fill: usize, step: &Step| {
        let mut e = engine();
        install(&e, done_artifact(), "a");
        e.set_retention_policy(RetentionPolicy {
            admission_replay_ms: 1_000,
            ..RetentionPolicy::default()
        })
        .unwrap();
        for t in 0..fill {
            step(&e, t as i64).unwrap();
        }
        e.config = config;
        let before = e.store.load("test").unwrap().revision;
        let message = match step(&e, fill as i64) {
            Err(Error::Refused(m)) => m,
            other => panic!("{other:?}"),
        };
        assert_eq!(e.store.load("test").unwrap().revision, before);
        message
    };
    let admit = |e: &Engine<MemoryStorage>, t: i64| {
        e.admit("a", "main", Value::Null, authority(), t)
            .map(|_| ())
    };
    let admit_keyed = |e: &Engine<MemoryStorage>, t: i64| {
        let d = e.admit_once(keyed(e, &format!("k{t}")), t)?;
        e.advance(&d.receipt.run_id, t).map(|_| ())
    };
    let runs = refusal(
        EngineConfig {
            max_runs: 2,
            ..EngineConfig::default()
        },
        2,
        &admit,
    );
    assert_eq!(
        runs,
        "the namespace is full for new work: 3 runs in the record (quota max_runs 2)"
    );
    // Two finished keyed runs leave two window receipts; the third
    // admission holds them, its own receipt and its run's receipt.
    let receipts = refusal(
        EngineConfig {
            max_receipts: 3,
            ..EngineConfig::default()
        },
        2,
        &admit_keyed,
    );
    assert_eq!(
        receipts,
        "the namespace is full for new work: 4 receipts in the record (quota max_receipts 3)"
    );
    let bytes = refusal(
        EngineConfig {
            max_state_bytes: 1,
            ..EngineConfig::default()
        },
        0,
        &admit,
    );
    assert!(
        bytes.starts_with("the namespace is full for new work: ")
            && bytes.ends_with(" bytes in the state record (quota max_state_bytes 1)"),
        "{bytes}"
    );
    for message in [runs, receipts, bytes] {
        for advice in ["GC", "garbage", "epoch", "tombstone"] {
            assert!(!message.contains(advice), "{message}");
        }
    }
}
/// Event tombstones written by legacy GC beyond `max_tombstones` neither
/// stop a later GC nor a policy change: the tombstone quota is ignored.
#[test]
fn legacy_tombstones_beyond_max_tombstones_stop_neither_gc_nor_policy_changes() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    let policy = RetentionPolicy {
        event_replay_ms: 1,
        max_tombstones: 1,
        ..RetentionPolicy::default()
    };
    e.set_retention_policy(policy.clone()).unwrap();
    for i in 0..3 {
        e.admit_event("host", event(&format!("old-{i}"), 1), 0)
            .unwrap();
    }
    assert_eq!(e.collect_garbage(2_000, 10).unwrap().events, 3);
    assert_eq!(e.snapshot().unwrap().retention.event_tombstones.len(), 3);
    e.set_retention_policy(policy).unwrap();
}
/// A namespace written before this change holds finished runs. They count
/// toward `max_runs` until they leave (the drain of Task 9; here, GC by
/// `audit_ms`, with `terminal_run_ms` ignored). At its quota such a
/// namespace refuses new work, saying what is full, and still commits the
/// work that frees it: a live run finishes and leaves, and GC moves the
/// finished runs out.
#[test]
fn a_legacy_namespace_at_its_run_quota_still_commits_the_work_that_drains_it() {
    let legacy = 51;
    let e = Engine::with_config(
        MemoryStorage::retaining_settled(),
        "test",
        "i-1",
        EngineConfig {
            max_runs: legacy + 1,
            ..EngineConfig::default()
        },
    )
    .unwrap();
    install(&e, done_artifact(), "a");
    e.set_retention_policy(RetentionPolicy {
        terminal_run_ms: u64::MAX,
        audit_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let live = e.admit("a", "main", Value::Null, authority(), 1).unwrap();
    let mut s = e.snapshot().unwrap();
    for i in 0..legacy {
        let id = format!("old-{i:03}");
        s.runs.insert(id.clone(), completed_run(&id));
        s.sequence += 1;
        s.receipts.push(audit(s.sequence, &id));
    }
    s.indexes.initialized = false;
    overwrite(&e.store, "i-1", &s);
    assert_eq!(quota_counts(&s).unwrap().runs, e.config.max_runs);
    assert!(matches!(
        e.admit("a", "main", Value::Null, authority(), 2),
        Err(Error::Refused(m)) if m.contains("53 runs in the record")
    ));
    // The live run finishes and leaves at the quota.
    assert!(e.advance(&live, 3).unwrap());
    assert_eq!(e.snapshot().unwrap().runs.len(), legacy);
    // GC moves the finished runs out at the quota, by audit_ms.
    e.admit("a", "main", Value::Null, authority(), 4).unwrap();
    assert_eq!(e.collect_garbage(1_000, 1_000).unwrap().runs, legacy);
    assert_eq!(e.snapshot().unwrap().runs.len(), 1);
    e.admit("a", "main", Value::Null, authority(), 1_001)
        .unwrap();
    assert_eq!(e.snapshot().unwrap().runs.len(), 2);
}

// --- Task 8 fix round 1: a quota refuses only growth

/// Review I2(a): a pinned clock fills the record with entries that never
/// drop; the licensed rebase that recovers it lands at a full
/// `max_state_bytes`, though its licence adds bytes (the stale floor).
#[test]
fn a_clock_rebase_lands_at_a_full_state_record() {
    let now: i64 = 1_763_000_000_000;
    let pinned = now + 31_536_000_000;
    let pinned_engine = || {
        let e = ret_engine();
        e.set_retention_policy(RetentionPolicy {
            admission_replay_ms: 1_000,
            ..RetentionPolicy::default()
        })
        .unwrap();
        // Real epoch milliseconds: the pin has as many digits as `now`, so
        // the capped expiries do not shrink the record.
        for i in 0..3 {
            let d = e.admit_once(keyed(&e, &format!("p{i}")), pinned).unwrap();
            assert!(e.advance(&d.receipt.run_id, pinned).unwrap());
        }
        e
    };
    let full = quota_counts(&pinned_engine().snapshot().unwrap())
        .unwrap()
        .state_bytes;
    // At the configured limit, and with the record at its hard ceiling
    // (where only the licence's exemption lets it land).
    for limit in [full, full * 8 / 9] {
        let mut e = pinned_engine();
        assert_eq!(e.snapshot().unwrap().keyed_admissions.len(), 3);
        e.config.max_state_bytes = limit;
        e.rebase_retention_clock(now, now, "operator", "clock pinned a year ahead", now)
            .unwrap_or_else(|err| panic!("at {limit}: {err}"));
        let s = e.snapshot().unwrap();
        assert_eq!(s.retention.clock_ms, Some(now));
        assert!(quota_counts(&s).unwrap().state_bytes > full);
        // The record is over its limit by the licence's bytes: new work
        // that grows it is refused, saying so.
        assert!(matches!(
            e.admit("a", "main", Value::Null, authority(), now + 1),
            Err(Error::Refused(m)) if m.contains("bytes in the state record")
        ));
    }
}
/// Review I2(b): an operator lowers `max_runs` below the live count. New
/// work is refused, but every commit that does not add a run lands: runs
/// finish and leave, a run is cancelled, an event completes a waiting run,
/// and a window drop goes through.
#[test]
fn over_a_lowered_limit_commits_that_do_not_grow_the_count_land() {
    let mut e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..2 {
        let d = e.admit_once(keyed(&e, &format!("k{i}")), i).unwrap();
        e.advance(&d.receipt.run_id, i).unwrap();
    }
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 2)
        .unwrap();
    e.advance(&waiter, 2).unwrap();
    let a1 = e.admit("a", "main", Value::Null, authority(), 3).unwrap();
    let a2 = e.admit("a", "main", Value::Null, authority(), 4).unwrap();
    let a3 = e.admit("a", "main", Value::Null, authority(), 5).unwrap();
    let live = |e: &Engine<MemoryStorage>| quota_counts(&e.snapshot().unwrap()).unwrap();
    assert_eq!(live(&e).runs, 4);
    e.config.max_runs = 1;
    let refused = |e: &Engine<MemoryStorage>, t: i64| {
        matches!(
            e.admit("a", "main", Value::Null, authority(), t),
            Err(Error::Refused(m)) if m.contains("(quota max_runs 1)")
        )
    };
    assert!(refused(&e, 10));
    // A run finishes and leaves.
    assert!(e.advance(&a1, 11).unwrap());
    // A run is cancelled.
    e.cancel(&a2, 12).unwrap();
    // An event completes the waiting run.
    e.admit_event("host", event("done", 2), 13).unwrap();
    for _ in 0..3 {
        if e.snapshot().unwrap().runs.contains_key(&waiter) {
            assert!(e.advance(&waiter, 14).unwrap());
        }
    }
    assert_eq!(runs_page(&e, Some(&waiter))[0]["outcome"], "ok");
    let s = e.snapshot().unwrap();
    assert!(!s.runs.contains_key(&waiter) && !s.runs.contains_key(&a1));
    let counts = live(&e);
    assert_eq!(counts.runs, 2, "{counts:?}");
    assert!(refused(&e, 15));
    // Receipts over a lowered limit too: the commit that finishes a run
    // and drops the passed keyed window lands (still over, but fewer).
    e.config.max_receipts = 1;
    let before = live(&e).receipts;
    assert!(before > 1, "{before}");
    assert!(e.advance(&a3, 2_000).unwrap());
    assert!(live(&e).receipts < before);
    assert!(!e.snapshot().unwrap().keyed_admissions.contains_key("k0"));
    // Review M1: the count is of runs in the record, not only in progress.
    assert!(matches!(
        e.admit("a", "main", Value::Null, authority(), 2_001),
        Err(Error::Refused(m)) if m.contains(
            "the namespace is full for new work: 2 runs in the record (quota max_runs 1)"
        )
    ));
}

// --- Task 8 fix round 2: limits gate admission, not progress

fn ok_host(n: usize) -> FakeHost {
    FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![
                DispatchResult::Completed {
                    outcome: "ok".into(),
                    output: Value::Null,
                };
                n
            ],
        )]),
        ..Default::default()
    }
}
fn full_for_new_work(result: Result<String>, what: &str) -> bool {
    matches!(result, Err(Error::Refused(m)) if m.starts_with("the namespace is full for new work: ") && m.contains(what))
}
/// The review's reproduction: three admitted `invoke` runs and the keyed
/// window fill `max_receipts` exactly (128; ceiling 144). Dispatching writes
/// a receipt without finishing a run (an invoke run holds up to 5 receipts
/// before it leaves, so the three need 12 above the limit), yet every run
/// drives to completion and leaves; a new admission is refused while the
/// quota is full. (With the window empty the limit would be 3, whose ceiling
/// is 3 + 3 / 8 = 3: no room. See the round 2 report.)
#[test]
fn at_a_full_receipt_quota_admitted_runs_drive_to_completion() {
    let mut e = engine();
    install(&e, done_artifact(), "a");
    install(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    for i in 0..125 {
        let d = e.admit_once(keyed(&e, &format!("w{i}")), i).unwrap();
        assert!(e.advance(&d.receipt.run_id, i).unwrap());
    }
    let runs: Vec<String> = (0..3)
        .map(|t| {
            e.admit("invoke", "main", Value::Null, authority(), 100 + t)
                .unwrap()
        })
        .collect();
    let full = quota_counts(&e.snapshot().unwrap()).unwrap().receipts;
    assert_eq!(full, 128);
    e.config.max_receipts = full;
    let admit =
        |e: &Engine<MemoryStorage>, t| e.admit("invoke", "main", Value::Null, authority(), t);
    assert!(matches!(admit(&e, 200), Err(Error::Refused(m)) if m.contains("receipts")));
    let mut host = ok_host(3);
    e.drive(&mut host, 210, 200).unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty(), "{:?}", s.runs.keys().collect::<Vec<_>>());
    for id in &runs {
        assert_eq!(runs_page(&e, Some(id))[0]["outcome"], "ok", "{id}");
    }
    assert_eq!(quota_counts(&s).unwrap().receipts, 125);
}
/// At a full `max_receipts`, a waiting run can be cancelled and another can
/// time out (both write receipts before the run finishes); new work is
/// refused.
#[test]
fn at_a_full_receipt_quota_a_waiting_run_can_be_cancelled_and_can_time_out() {
    let mut e = engine();
    install(&e, wait_artifact(), "waiter");
    install(&e, done_artifact(), "a");
    for t in 0..14 {
        e.admit("a", "main", Value::Null, authority(), t).unwrap();
    }
    let waiters: Vec<String> = (0..2)
        .map(|_| {
            let id = e
                .admit("waiter", "main", Value::Null, authority(), 0)
                .unwrap();
            e.advance(&id, 0).unwrap();
            id
        })
        .collect();
    e.config.max_receipts = quota_counts(&e.snapshot().unwrap()).unwrap().receipts;
    let admit =
        |e: &Engine<MemoryStorage>, t| e.admit("waiter", "main", Value::Null, authority(), t);
    assert!(matches!(admit(&e, 1), Err(Error::Refused(m)) if m.contains("receipts")));
    e.cancel(&waiters[0], 10).unwrap();
    e.drive(&mut FakeHost::default(), 10, 20).unwrap();
    e.drive(&mut FakeHost::default(), 200, 20).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    assert_eq!(runs_page(&e, Some(&waiters[0]))[0]["status"], "cancelled");
    assert_eq!(runs_page(&e, Some(&waiters[1]))[0]["outcome"], "error");
}
/// At a full `max_state_bytes`, registering a wait (which grows the record)
/// lands; a new admission is refused.
#[test]
fn at_a_full_state_record_registering_a_wait_lands() {
    let mut e = engine();
    install(&e, wait_artifact(), "waiter");
    let ids: Vec<String> = (0..3)
        .map(|t| {
            e.admit("waiter", "main", Value::Null, authority(), t)
                .unwrap()
        })
        .collect();
    let id = ids[0].clone();
    e.config.max_state_bytes = quota_counts(&e.snapshot().unwrap()).unwrap().state_bytes;
    let admit =
        |e: &Engine<MemoryStorage>, t| e.admit("waiter", "main", Value::Null, authority(), t);
    assert!(matches!(admit(&e, 1), Err(Error::Refused(m)) if m.contains("bytes")));
    assert!(e.advance(&id, 2).unwrap());
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Waiting);
    let limit = e.config.max_state_bytes;
    let bytes = quota_counts(&e.snapshot().unwrap()).unwrap().state_bytes;
    assert!(bytes > limit);
    assert!(full_for_new_work(admit(&e, 3), "bytes"));
    // The two refusals say which: new work at the limit, and the ceiling.
    e.config.max_receipts = 1;
    assert!(full_for_new_work(
        admit(&e, 4),
        "receipts in the record (quota max_receipts 1)"
    ));
}
/// Work already admitted is bounded too: past `limit + limit / 8` it is
/// refused with the ceiling message.
#[test]
fn work_already_admitted_is_refused_past_the_hard_ceiling() {
    let mut e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    let runs: Vec<String> = (0..16)
        .map(|t| {
            e.admit("invoke", "main", Value::Null, authority(), t)
                .unwrap()
        })
        .collect();
    e.config.max_receipts = quota_counts(&e.snapshot().unwrap()).unwrap().receipts;
    assert_eq!(e.config.max_receipts, 16);
    // Each first step plans an operation, writes a receipt and leaves its
    // run live: the third would make 19 > 16 + 2.
    let mut refusal = None;
    for id in &runs {
        if let Err(err) = e.advance(id, 20) {
            refusal = Some(err);
            break;
        }
    }
    assert!(
        matches!(&refusal, Some(Error::Refused(m)) if m == "the namespace is past its hard ceiling: 19 receipts in the record (ceiling 18: quota max_receipts 16 plus one eighth)"),
        "{refusal:?}"
    );
    assert_eq!(quota_counts(&e.snapshot().unwrap()).unwrap().receipts, 18);
    let _ = runs;
}
/// The upgrade's own bookkeeping is never refused by a quota: a pre-D10
/// namespace written before the retention clock opens (the `root_intents`
/// fill grows the record) and its first host-time commit, which starts the
/// clock, lands. Once with the stored record at its configured
/// `max_state_bytes`, once at its hard ceiling.
#[test]
fn a_namespace_at_its_configured_state_bytes_opens_and_takes_its_first_commits() {
    let e = Engine::new(MemoryStorage::retaining_settled(), "test", "incarnation-1").unwrap();
    install_on(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    install_on(&e, wait_artifact(), "waiter");
    let mut host = completed_host();
    let (old, _) = run_with_completed_operation(&e, &mut host);
    e.admit_event("host", event("seen", 1), 0).unwrap();
    let mut legacy = vendored_shape(&e.snapshot().unwrap());
    legacy.retention.clock_ms = None;
    e.release().unwrap();
    let full = quota_counts(&legacy).unwrap().state_bytes;
    for limit in [full, full * 8 / 9] {
        // Stored by an engine without the retention clock: a fresh store.
        let store = MemoryStorage::retaining_settled();
        store.acquire("test", "w").unwrap();
        store.compare_and_swap("test", "w", 0, &legacy).unwrap();
        store.release("test", "w").unwrap();
        assert!(limit == full || quota_ceiling(limit) <= full);
        let e = Engine::with_config(
            store,
            "test",
            "incarnation-2",
            EngineConfig {
                max_state_bytes: limit,
                ..EngineConfig::default()
            },
        )
        .unwrap_or_else(|err| panic!("open at {limit}: {err}"));
        let s = e.snapshot().unwrap();
        assert!(s.indexes.root_intents_complete);
        assert!(
            quota_counts(&s).unwrap().state_bytes > full,
            "the fill grew it"
        );
        // The first host-time commit starts the clock (`null` becomes a
        // seven-digit time): a redelivery inside the event's window, a
        // duplicate that writes nothing else.
        let before = quota_counts(&s).unwrap().state_bytes;
        assert!(
            e.admit_event("host", event("seen", 1), 1_000_000)
                .unwrap_or_else(|err| panic!("first commit at {limit}: {err}"))
                .duplicate
        );
        let s = e.snapshot().unwrap();
        assert_eq!(s.retention.clock_ms, Some(1_000_000));
        assert!(
            quota_counts(&s).unwrap().state_bytes > before,
            "the clock grew it"
        );
        assert!(e.advance(&old, 1_000_001).unwrap());
    }
}
/// A new event, a new artifact and an activation are new work: at a full
/// quota they are refused against the configured limit, though the ceiling
/// would have room.
#[test]
fn new_events_artifacts_and_activations_are_new_work() {
    let mut e = engine();
    install(&e, wait_artifact(), "waiter");
    for i in 0..8 {
        e.admit_event("host", event(&format!("e{i}"), 1), 0)
            .unwrap();
    }
    e.config.max_events = 8;
    let refused = e
        .admit_event("host", event("e8", 1), 1)
        .map(|_| String::new());
    assert!(full_for_new_work(
        refused,
        "9 events in the record (quota max_events 8)"
    ));
    e.config = EngineConfig {
        max_state_bytes: quota_counts(&e.snapshot().unwrap()).unwrap().state_bytes,
        ..EngineConfig::default()
    };
    assert!(full_for_new_work(
        e.install(done_artifact()).map(|_| String::new()),
        "bytes in the state record"
    ));
    let waiter = e.snapshot().unwrap().bindings["waiter"].artifact.clone();
    assert!(full_for_new_work(
        e.activate("other", &waiter, authority())
            .map(|_| String::new()),
        "bytes in the state record"
    ));
}
