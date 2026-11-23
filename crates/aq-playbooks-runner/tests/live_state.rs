//! Live state only (spec 0003): history written with the commit, inspection
//! over live state plus history.
#![allow(dead_code)]
#[path = "fixtures/async_operation.rs"]
mod async_operation;
use async_operation::*;
#[path = "fixtures/engine.rs"]
mod engine_fixtures;
#[path = "fixtures/removal_state.rs"]
mod removal_state;
use removal_state::{
    completed_run, refused_with, removal_fences_hold, synthetic_cohort, tombstone,
};

/// The planner fixture also carries a settled operation and an event without a recorded expiry.
fn fence_state() -> State {
    let mut s = removal_state::base_fence_state();
    s.intents.insert(
        "op-a".into(),
        intent("op-a", "run-a", IntentStatus::Completed),
    );
    s.events
        .insert("ev-fallback".into(), accepted_event("ev-fallback", 0));
    s
}

use aq_playbooks_model::*;
use aq_playbooks_runner::*;
use engine_fixtures::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

// --- Task 4: retention clock, window fields and removal fences ------------

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
        dispatch_sequence: None,
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

#[path = "live_state/legacy_snapshots.rs"]
mod legacy_snapshots;
#[test]
fn memory_storage_removal_fences() {
    removal_fences_hold(&MemoryStorage::default(), "ns")
}
#[test]
fn record_storage_removal_fences() {
    removal_fences_hold(&RecordStorage::new(), "ns")
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
            FixtureContracts::default(),
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
        FixtureContracts::default(),
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
fn cohort(run: Run, intents: Vec<Intent>, receipts: Vec<Receipt>) -> SettledCohort {
    SettledCohort {
        root: run.root.clone(),
        finished_at_ms: 1,
        runs: vec![run],
        intents,
        receipts,
    }
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
        FixtureContracts {
            registry: Registry {
                commands: BTreeMap::new(),
                events: BTreeMap::from([(c.name.clone(), c)]),
            },
            ..Default::default()
        },
    )
}
/// Legacy event tombstones and correlation fences (no engine writes them
/// any more) may always be removed; an expired event leaves by its window.
/// A legacy disposal record licenses nothing: a run removed without its
/// history is refused, disposal record or not.
fn legacy_records_license_no_removal(store: &dyn Storage) {
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
    next.retention.disposals.push(retention::Disposal {
        kind: "run".into(),
        key: "run-a".into(),
        at_ms: 5,
    });
    assert!(refused_with(
        store.compare_and_swap("legacy", "w", r, &next),
        retention::REFUSE_RUN_REMOVAL
    ));
    let mut next = s.clone();
    next.retention.clock_ms = Some(1_000_000);
    next.events.remove("ev");
    next.retention.event_expires_at_ms.remove("ev");
    next.retention.event_tombstones.remove("old");
    next.retention.expired_correlations.remove("c");
    store.compare_and_swap("legacy", "w", r, &next).unwrap();
}
#[test]
fn memory_storage_legacy_records_license_no_removal() {
    legacy_records_license_no_removal(&MemoryStorage::default());
}
#[test]
fn record_storage_legacy_records_license_no_removal() {
    legacy_records_license_no_removal(&RecordStorage::new());
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
        FixtureContracts::default(),
    )
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

#[path = "live_state/settled_cohorts.rs"]
mod settled_cohorts;

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
/// An event of `wait_artifact`'s contract with its own correlation.
fn correlated_event(id: &str, invocation: &str) -> EventEnvelope {
    EventEnvelope {
        correlation: BTreeMap::from([("invocation".into(), invocation.into())]),
        ..event(id, 2)
    }
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

fn short_windows(e: &Engine<impl Storage>) {
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
}

/// A stale refusal: the typed error, whose text starts with `STALE_REQUEST`.
fn stale(result: Result<impl std::fmt::Debug>) -> bool {
    matches!(result, Err(e @ Error::Stale { .. }) if e.to_string().starts_with(STALE_REQUEST))
}

#[path = "live_state/event_windows.rs"]
mod event_windows;

#[path = "live_state/keyed_windows.rs"]
mod keyed_windows;

#[path = "live_state/stale_requests.rs"]
mod stale_requests;

// --- Task 7 round 1, ruling 3: rebasing a pinned retention clock ----------

#[path = "live_state/pinned_clock.rs"]
mod pinned_clock;

#[path = "live_state/rebase_deadlines.rs"]
mod rebase_deadlines;

// --- Task 8b R1: the eviction licence (fence tests, every runner path) ----

#[path = "fixtures/eviction_entries.rs"]
mod eviction_entries;
use eviction_entries::*;
/// "eu" at the head of the order, held by an operation of its contract
/// whose admission handle is lost (an uncertain completion), `replay_keep`
/// 4, then k0..k4.
fn uncertain_event_state() -> State {
    let mut s = held_event_state();
    s.events.remove("ew");
    s.retention.event_expires_at_ms.remove("ew");
    s.retention.replay_order.clear();
    s.runs.clear();
    s.waits.clear();
    s.indexes.waiters.clear();
    s.indexes.event_memberships.clear();
    s.retention.expiry.clear();
    for i in 0..5 {
        let r = s.keyed_admissions.get(&format!("k{i}")).unwrap().clone();
        s.retention
            .replay_order
            .insert(retention::admission_order_key(&r), ());
    }
    // Accepted after the operation's dispatch (sequence 0), so it may be
    // its completion; still the head ("e:eu" sorts before "k:k0").
    event_entry(&mut s, "eu", 1, true);
    let e = s.events.get("eu").unwrap().clone();
    let mut op = intent("op-u", "run-u", IntentStatus::Unknown);
    op.dispatch_sequence = Some(0);
    op.command.asynchronous = Some(AsyncContract {
        completion_event: e.envelope.contract.name.clone(),
        completion: Schema::Null,
        correlation_field: "invocation".into(),
        reconciliation_command: "test.command".into(),
    });
    s.intents.insert("op-u".into(), op);
    s.runs.insert("run-u".into(), running_run("run-u"));
    let contract = indexes::trigger_key(&e.envelope.contract, &e.envelope.scope).unwrap();
    s.indexes
        .uncertain_completion_contracts
        .insert(contract.clone(), BTreeSet::from(["op-u".to_string()]));
    s.indexes
        .uncertain_contract_membership
        .insert("op-u".into(), contract);
    s
}
/// "ew" (held by its waiter) at the head of the order, `replay_keep` 4,
/// then k0..k4.
fn held_event_state() -> State {
    let mut held_event = State::default();
    held_event.retention.clock_ms = Some(500);
    held_event.retention.policy.admission_replay_ms = EVICTION_WINDOW as u64;
    held_event.retention.policy.event_replay_ms = EVICTION_WINDOW as u64;
    held_event.retention.policy.replay_keep = 4;
    event_entry(&mut held_event, "ew", 0, true);
    hold_by_waiter(&mut held_event, "ew");
    for i in 0..5 {
        keyed_entry(
            &mut held_event,
            &format!("k{i}"),
            i + 1,
            KeyPosition::Ordered,
        );
    }
    held_event.sequence = 20;
    held_event
}
/// The eviction fence state: `replay_keep` 5; the order holds, oldest
/// first, "kh" (sequence 0, its run live: held, as an entry whose hold began
/// after it joined), "k0".."k3" (1..4), "e4" (5) and "ew" (7, held by a
/// live waiter): 7 entries. "kh2" (live run) waits in `replay_held`; "ex"
/// (8, unheld) is outside the order. The correct eviction takes "kh" out
/// of the order and evicts "k0", leaving 5.
fn eviction_state() -> State {
    let mut s = State::default();
    s.retention.clock_ms = Some(500);
    s.retention.policy.admission_replay_ms = EVICTION_WINDOW as u64;
    s.retention.policy.event_replay_ms = EVICTION_WINDOW as u64;
    s.retention.policy.replay_keep = 5;
    keyed_entry(&mut s, "kh", 0, KeyPosition::OrderedLive);
    for i in 0..4 {
        keyed_entry(&mut s, &format!("k{i}"), i + 1, KeyPosition::Ordered);
    }
    event_entry(&mut s, "e4", 5, true);
    event_entry(&mut s, "ew", 7, true);
    hold_by_waiter(&mut s, "ew");
    keyed_entry(&mut s, "kh2", 9, KeyPosition::Held);
    event_entry(&mut s, "ex", 8, false);
    s.sequence = 20;
    s
}
#[path = "fixtures/eviction_cases.rs"]
mod eviction_cases;
/// Each a (label, refusal, previous State, change): every way an eviction
/// can break the licence.
#[path = "fixtures/eviction_commits.rs"]
mod eviction_commits;
use eviction_cases::{EvictionCase, common_refusals};
fn eviction_refusal_cases() -> Vec<EvictionCase> {
    let base = eviction_state();
    let held_event = held_event_state();
    let mut cases = common_refusals(&base, &held_event);
    cases.extend(admission_and_order_boundaries(&base));
    cases.extend(eviction_evidence(&base));
    cases
}

fn admission_and_order_boundaries(base: &State) -> [EvictionCase; 4] {
    [
        (
            "at replay_keep after a policy change",
            retention::REFUSE_EVICTION,
            base.clone(),
            Box::new(|n| {
                n.retention.policy.replay_keep = 6;
                evicted_correctly(n);
            }),
        ),
        (
            "older than a kept entry",
            retention::REFUSE_EVICTION,
            base.clone(),
            Box::new(|n| {
                hold_out(n, "kh");
                ReplayMember::Keyed("k1").evict(n);
            }),
        ),
        (
            "its order key kept",
            retention::REFUSE_EARLY_REMOVAL,
            base.clone(),
            Box::new(|n| {
                hold_out(n, "kh");
                n.keyed_admissions.remove("k0");
                n.retention.admission_expires_at_ms.remove("k0");
            }),
        ),
        (
            "an order entry that inflates the count",
            retention::REFUSE_EVICTION,
            base.clone(),
            Box::new(|n| {
                n.retention
                    .replay_order
                    .insert(retention::replay_key(8, "e:ex"), ());
                n.retention
                    .replay_order
                    .insert(retention::replay_key(31, "k:ghost"), ());
                hold_out(n, "kh");
                ReplayMember::Keyed("k0").evict(n);
                ReplayMember::Keyed("k1").evict(n);
            }),
        ),
    ]
}

fn eviction_evidence(base: &State) -> [EvictionCase; 5] {
    [
        (
            "an event's digest changed",
            retention::REFUSE_EVENT_CHANGE,
            base.clone(),
            Box::new(|n| n.events.get_mut("e4").unwrap().digest = "forged".into()),
        ),
        (
            "an event's acceptance time changed",
            retention::REFUSE_EVENT_CHANGE,
            base.clone(),
            Box::new(|n| n.events.get_mut("e4").unwrap().accepted_at_ms = 7),
        ),
        (
            "an event an uncertain completion holds, its index dropped",
            retention::REFUSE_EVICTION,
            uncertain_event_state(),
            Box::new(|n| {
                n.indexes.uncertain_completion_contracts.clear();
                n.indexes.uncertain_contract_membership.clear();
                ReplayMember::Event("eu").evict(n);
            }),
        ),
        (
            "a held event whose waiter index entry is dropped",
            retention::REFUSE_EVICTION,
            held_event_state(),
            Box::new(|n| {
                n.indexes.waiters.clear();
                ReplayMember::Event("ew").evict(n);
            }),
        ),
        (
            "more than 64 in one commit",
            retention::REFUSE_EVICTION,
            bulk_eviction_state(),
            Box::new(|n| {
                for i in 0..65 {
                    ReplayMember::Keyed(&format!("b{i:03}")).evict(n);
                }
            }),
        ),
    ]
}
/// Store `state` as a namespace reaches it: its entries join the order
/// unheld (the live runs and waiters absent), then the holds begin in a
/// second commit (a held entry can never be written into the order).
/// Returns the revision.
fn seed_eviction_state(store: &dyn Storage, ns: &str, state: &State) -> u64 {
    store.acquire(ns, "w").unwrap();
    let mut unheld = state.clone();
    unheld.runs.clear();
    unheld.waits.clear();
    unheld.intents.clear();
    unheld.indexes.waiters.clear();
    unheld.indexes.uncertain_completion_contracts.clear();
    unheld.indexes.uncertain_contract_membership.clear();
    let first = store.compare_and_swap(ns, "w", 0, &unheld).unwrap();
    store.compare_and_swap(ns, "w", first, state).unwrap()
}
/// `replay_keep` 1 and 70 unheld keyed receipts in the order.
fn bulk_eviction_state() -> State {
    let mut s = State::default();
    s.retention.clock_ms = Some(500);
    s.retention.policy.admission_replay_ms = EVICTION_WINDOW as u64;
    s.retention.policy.event_replay_ms = EVICTION_WINDOW as u64;
    s.retention.policy.replay_keep = 1;
    for i in 0..70 {
        keyed_entry(&mut s, &format!("b{i:03}"), i + 1, KeyPosition::Ordered);
    }
    s.sequence = 100;
    s
}
/// The eviction licence on a store's compare-and-swap: every refusal
/// case is refused with its text and writes nothing; the correct
/// eviction, and 64 at once, are accepted.
fn eviction_fences_hold_on_cas(store: &dyn Storage) {
    let count = std::cell::Cell::new(0);
    let fresh = |state: &State| {
        count.set(count.get() + 1);
        let ns = format!("evict-{}", count.get());
        let base = seed_eviction_state(store, &ns, state);
        (ns, base)
    };
    let failures = eviction_commits::cas(store, &fresh);
    assert!(failures.is_empty(), "{failures:#?}");
}
#[test]
fn memory_storage_holds_the_eviction_licence() {
    eviction_fences_hold_on_cas(&MemoryStorage::default());
}
#[test]
fn record_storage_holds_the_eviction_licence() {
    eviction_fences_hold_on_cas(&RecordStorage::new());
}
/// The same licence on `RecordStorage`'s bounded transitions (the patch
/// fence).
#[test]
fn record_storage_transitions_hold_the_eviction_licence() {
    let store = RecordStorage::new();
    let count = std::cell::Cell::new(0);
    let fresh = |state: &State| {
        count.set(count.get() + 1);
        let ns = format!("t-evict-{}", count.get());
        let mut state = state.clone();
        state.indexes.initialized = true;
        let base = seed_eviction_state(&store, &ns, &state);
        (ns, base)
    };
    let mut failures = eviction_commits::refusals(&store, &fresh, |ns, _, _, mutate| {
        (eviction_commits::transition(&store, ns, mutate), false)
    });
    let full = store
        .stats
        .full_commits
        .load(std::sync::atomic::Ordering::Relaxed);
    failures.extend(eviction_commits::allowances(&fresh, |ns, _, _, mutate| {
        eviction_commits::transition(&store, ns, mutate)
    }));
    assert!(failures.is_empty(), "{failures:#?}");
    // Four seeding full commits; the evictions themselves are bounded.
    assert_eq!(
        store
            .stats
            .full_commits
            .load(std::sync::atomic::Ordering::Relaxed),
        full + 4
    );
}

// --- Task 8b R1: repeat protection is a rolling set (engine) -------------

fn keep_engine(keep: usize) -> Engine<MemoryStorage> {
    let e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 600_000,
        replay_keep: keep,
        ..RetentionPolicy::default()
    })
    .unwrap();
    e
}
/// Admit keyed `key` at `t` and finish its run.
fn keyed_done(e: &Engine<MemoryStorage>, key: &str, t: i64) -> KeyedAdmissionDisposition {
    let d = e.admit_once(keyed(e, key), t).unwrap();
    if !d.duplicate {
        assert!(e.advance(&d.receipt.run_id, t).unwrap());
    }
    d
}
fn order_members(s: &State) -> Vec<String> {
    s.retention
        .replay_order
        .keys()
        .map(|k| retention::replay_entry(k).unwrap().1.to_string())
        .collect()
}
#[test]
fn replay_keep_must_be_positive() {
    let e = engine();
    assert!(matches!(
        e.set_retention_policy(RetentionPolicy {
            replay_keep: 0,
            ..RetentionPolicy::default()
        }),
        Err(Error::Refused(m)) if m == "replay_keep must be positive"
    ));
    assert_eq!(RetentionPolicy::default().replay_keep, 256);
    let stored: RetentionPolicy =
        serde_json::from_value(json!({"event_replay_ms": 1, "audit_ms": 1})).unwrap();
    assert_eq!((stored.replay_keep, stored.history_keep), (256, 10_000));
    // A State written before the rolling bounds decodes with an empty order.
    let legacy: State = serde_json::from_value(json!({
        "artifacts": {}, "bindings": {}, "catalog_revision": 0, "runs": {},
        "receipts": [], "intents": {}, "waits": {}, "events": {}, "budgets": {},
        "sequence": 0,
    }))
    .unwrap();
    assert!(legacy.retention.replay_order.is_empty() && legacy.retention.replay_held.is_empty());
}
/// With `replay_keep` 4: a duplicate inside the rolling set is recognised;
/// the 5th distinct unheld key evicts the 1st (oldest first, its window not
/// passed); a re-send of the 1st is then accepted as new, and with a
/// request time older than its window refused as stale. No floor: a
/// re-send whose request time is inside its window is new.
#[test]
fn the_fifth_unheld_key_evicts_the_first() {
    let e = keep_engine(4);
    for (i, key) in ["k0", "k1", "k2", "k3"].iter().enumerate() {
        keyed_done(&e, key, i as i64);
    }
    assert!(keyed_done(&e, "k0", 10).duplicate);
    assert_eq!(
        order_members(&e.snapshot().unwrap()),
        ["k:k0", "k:k1", "k:k2", "k:k3"]
    );
    keyed_done(&e, "k4", 11);
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:k1", "k:k2", "k:k3", "k:k4"]);
    assert!(!s.keyed_admissions.contains_key("k0"));
    assert!(!s.retention.admission_expires_at_ms.contains_key("k0"));
    assert!(window_entries(&s).iter().all(|(_, m)| m != "k:k0"));
    assert!(keyed_done(&e, "k1", 12).duplicate);
    // The first is forgotten: with a request time older than its window it
    // is stale, writing nothing ...
    let before = e.store.load("test").unwrap().revision;
    assert!(stale(e.admit_once_at(
        keyed(&e, "k0"),
        Some(12 - 600_000),
        12
    )));
    assert_eq!(e.store.load("test").unwrap().revision, before);
    // ... and with one inside its window (or none) it is new.
    let again = e.admit_once_at(keyed(&e, "k0"), Some(0), 13).unwrap();
    assert!(!again.duplicate);
}
/// Entries live work still needs are neither evicted nor counted: keyed
/// receipts of live runs, an event a live waiter holds. A keyed receipt
/// joins the order when its run finishes.
#[test]
fn held_entries_are_not_evicted_and_do_not_count() {
    let e = keep_engine(2);
    let live: Vec<String> = (0..3)
        .map(|i| {
            e.admit_once(keyed(&e, &format!("live-{i}")), i)
                .unwrap()
                .receipt
                .run_id
        })
        .collect();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 3)
        .unwrap();
    e.advance(&waiter, 3).unwrap();
    // Matches the waiter: held while the waiter is live (it has not
    // advanced past its wait yet).
    let held = e.admit_event("host", event("held", 2), 4).unwrap();
    keyed_done(&e, "done-0", 5);
    keyed_done(&e, "done-1", 6);
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:done-0", "k:done-1"]);
    assert_eq!(s.retention.replay_held.len(), 3);
    assert_eq!(s.keyed_admissions.len(), 5);
    assert!(s.events.contains_key(&held.key));
    // A third finished key evicts the oldest unheld one, not a held one.
    keyed_done(&e, "done-2", 7);
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:done-1", "k:done-2"]);
    assert!(s.events.contains_key(&held.key));
    assert!(live.iter().all(|run| s.runs.contains_key(run)));
    // A live run finishes: its receipt joins the order (by acceptance, so
    // it is the oldest) and is evicted first.
    assert!(e.advance(&live[0], 8).unwrap());
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:done-1", "k:done-2"]);
    assert!(!s.keyed_admissions.contains_key("live-0"));
    // The waiter consumes the event and finishes: the event joins the
    // order by its acceptance (older than "done-1") and, its window not
    // passed, is evicted as the oldest.
    while e.snapshot().unwrap().runs.contains_key(&waiter) {
        e.advance(&waiter, 9).unwrap();
    }
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:done-1", "k:done-2"]);
    assert!(!s.events.contains_key(&held.key));
    assert!(s.retention.event_expires_at_ms.get(&held.key).is_none());
}
/// A burst of 10,000 keyed admissions with the clock frozen keeps the
/// unheld set at `replay_keep` (within the per-commit bound) and refuses
/// none of them for what is remembered.
#[test]
fn a_frozen_clock_burst_keeps_the_remembered_set_at_replay_keep() {
    let e = ret_engine();
    let keep = RetentionPolicy::default().replay_keep;
    for i in 0..10_000 {
        let d = keyed_done(&e, &format!("burst-{i:05}"), 0);
        assert!(!d.duplicate, "{i}");
        if i % 500 == 0 || i == 9_999 {
            let s = e.snapshot().unwrap();
            let order = s.retention.replay_order.len();
            assert!(
                order <= keep + retention::REPLAY_EVICTIONS_PER_COMMIT,
                "{i}: {order}"
            );
            assert_eq!(s.keyed_admissions.len(), order.min(i + 1), "{i}");
            if i >= keep {
                assert!(order >= keep, "{i}: {order}");
            }
        }
    }
    let s = e.snapshot().unwrap();
    assert_eq!(s.retention.replay_order.len(), keep);
    assert!(s.runs.is_empty());
}
/// An event whose contract declares a 24 h window is evicted by count
/// long before its window passes; a wait registered after its eviction
/// times out, as after an expired event.
#[test]
fn a_day_long_event_window_is_evicted_by_count() {
    let e = ret_engine();
    let mut day = wait_artifact().content;
    day.registry
        .events
        .get_mut("test.completed")
        .unwrap()
        .retention_ms = 86_400_000;
    // The waiter's contract reference must match the event's.
    let contract = day.registry.events["test.completed"].clone();
    if let Operation::Wait { event, .. } = &mut day.playbook.steps.get_mut("wait").unwrap().op {
        *event = Some(ContractRef {
            name: contract.name.clone(),
            fingerprint: contract.fingerprint().unwrap(),
        });
    }
    let day = Artifact::seal(day).unwrap();
    install(&e, day, "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        replay_keep: 2,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let envelope = |id: &str, invocation: &str| EventEnvelope {
        contract: ContractRef {
            name: contract.name.clone(),
            fingerprint: contract.fingerprint().unwrap(),
        },
        ..correlated_event(id, invocation)
    };
    let first = e
        .admit_event("host", envelope("day-0", "exact-invocation"), 0)
        .unwrap();
    assert_eq!(
        e.snapshot().unwrap().retention.event_expires_at_ms[&first.key],
        86_400_000
    );
    e.admit_event("host", envelope("day-1", "inv-1"), 1)
        .unwrap();
    e.admit_event("host", envelope("day-2", "inv-2"), 2)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(!s.events.contains_key(&first.key));
    assert_eq!(s.events.len(), 2);
    assert!(s.indexes.event_memberships.get(&first.key).is_none());
    // A wait for it registered after its eviction does not match; it times
    // out.
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 3)
        .unwrap();
    e.advance(&waiter, 3).unwrap();
    assert_eq!(
        e.snapshot().unwrap().runs[&waiter].status,
        RunStatus::Waiting
    );
    e.drive(&mut FakeHost::default(), 200, 20).unwrap();
    assert_eq!(runs_page(&e, Some(&waiter))[0]["outcome"], "error");
    // A re-send of the evicted event is new.
    assert!(
        !e.admit_event("host", envelope("day-0", "exact-invocation"), 201)
            .unwrap()
            .duplicate
    );
}
/// Format 3 (bound record containers): the work of a commit that evicts k
/// entries depends on k, not on the entries kept, and reads no container
/// whole. The measured commit lowers `replay_keep` by k.
#[test]
fn eviction_work_on_record_storage_depends_on_entries_evicted_not_on_entries_kept() {
    use std::sync::atomic::Ordering;
    let measure = |kept: usize, k: usize| {
        let e = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
        install_on(&e, done_artifact(), "a");
        let policy = |keep: usize| RetentionPolicy {
            replay_keep: keep,
            ..RetentionPolicy::default()
        };
        e.set_retention_policy(policy(kept + k)).unwrap();
        for i in 0..kept + k {
            let d = e
                .admit_once(keyed_on(&e, &format!("k-{i:03}")), i as i64)
                .unwrap();
            e.advance(&d.receipt.run_id, i as i64).unwrap();
        }
        let reads = e.store.stats.record_reads.load(Ordering::Relaxed);
        let fallbacks = e.store.stats.fallbacks().len();
        e.set_retention_policy(policy(kept)).unwrap();
        let s = e.snapshot().unwrap();
        assert_eq!(s.retention.replay_order.len(), kept);
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
    assert_eq!(none_few, none_many, "nothing evicted");
    let (eight, scanned) = measure(20, 8);
    assert!(scanned.is_empty(), "{scanned:?}");
    let (eight_many, _) = measure(80, 8);
    assert_eq!(eight, eight_many, "8 evicted");
    let (sixteen, _) = measure(20, 16);
    let (twenty_four, _) = measure(20, 24);
    // A fixed part (the head lookup) and the same per eviction.
    let per_eight = sixteen - eight;
    assert_eq!(
        twenty_four - sixteen,
        per_eight,
        "reads grow linearly with evictions"
    );
    assert!(per_eight > 0);
    let fixed = (eight - none_few) - per_eight;
    assert!(fixed <= 4, "fixed part {fixed}");
}
/// Held entries at the head of the order (their holds began after they
/// joined, as through an untracked uncertain completion): the engine takes
/// them out of the order rather than evicting them, then evicts the oldest
/// unheld one; the stored commit passes the licence.
#[test]
fn the_engine_takes_held_entries_at_the_head_out_of_the_order() {
    // States this engine wrote (drained): the drain would otherwise judge
    // the entries first (Task 9).
    let drained = retention::DrainState {
        done: true,
        roots_done: true,
        phase: retention::DRAIN_DONE,
        ..retention::DrainState::default()
    };
    let mut keyed_head = eviction_state();
    keyed_head.indexes.initialized = true;
    keyed_head.retention.drain = drained.clone();
    let mut event_head = State::default();
    event_head.retention.clock_ms = Some(500);
    event_head.retention.policy.admission_replay_ms = EVICTION_WINDOW as u64;
    event_head.retention.policy.event_replay_ms = EVICTION_WINDOW as u64;
    event_head.retention.policy.replay_keep = 4;
    event_entry(&mut event_head, "ew", 0, true);
    hold_by_waiter(&mut event_head, "ew");
    for i in 0..5 {
        keyed_entry(
            &mut event_head,
            &format!("k{i}"),
            i + 1,
            KeyPosition::Ordered,
        );
    }
    event_head.sequence = 20;
    event_head.indexes.initialized = true;
    event_head.retention.drain = drained;
    for (label, state, kept) in [
        (
            "a keyed receipt whose run is live",
            keyed_head,
            vec!["k:k1", "k:k2", "k:k3", "e:e4", "e:ew"],
        ),
        (
            "an event a waiter holds",
            event_head,
            vec!["k:k1", "k:k2", "k:k3", "k:k4"],
        ),
    ] {
        let store = MemoryStorage::default();
        seed_eviction_state(&store, "test", &state);
        store.release("test", "w").unwrap();
        let e = Engine::new(store, "test", "incarnation-1").unwrap();
        e.set_retention_policy(state.retention.policy.clone())
            .unwrap_or_else(|err| panic!("{label}: {err}"));
        let s = e.snapshot().unwrap();
        assert_eq!(order_members(&s), kept, "{label}");
        assert!(!s.keyed_admissions.contains_key("k0"), "{label}");
        assert!(s.events.contains_key("ew"), "{label}");
        assert_eq!(
            s.keyed_admissions.contains_key("kh"),
            state.keyed_admissions.contains_key("kh"),
            "{label}"
        );
    }
}
/// An entry that leaves by its window, or is forgotten to be admitted
/// again, leaves the eviction order with it.
#[test]
fn dropped_and_forgotten_entries_leave_the_order() {
    let e = keep_engine(10);
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        replay_keep: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    for i in 0..3 {
        keyed_done(&e, &format!("old-{i}"), i);
    }
    e.admit_event("host", correlated_event("old-event", "nobody"), 0)
        .unwrap();
    assert_eq!(e.snapshot().unwrap().retention.replay_order.len(), 4);
    // Past the windows: the drop takes the receipts and the event.
    keyed_done(&e, "new", 2_000);
    assert_eq!(order_members(&e.snapshot().unwrap()), ["k:new"]);
    // An expired key admitted again is forgotten first, then new.
    keyed_done(&e, "new", 4_000);
    let s = e.snapshot().unwrap();
    assert_eq!(order_members(&s), ["k:new"]);
    assert_eq!(s.retention.replay_order.len(), 1);
}

fn only_intent(s: &State) -> Intent {
    assert_eq!(s.intents.len(), 1);
    s.intents.values().next().unwrap().clone()
}

#[path = "live_state/operation_holds.rs"]
mod operation_holds;

// --- Task 9: the drain of namespaces written by an older engine -------------

/// Inject into `s` `cohorts` settled one-run cohorts as an older engine left
/// them in the record (`run-0-legacy-NNNN`, which sort before the engine's
/// `run-<16 hex>` ids), each with an admitted and a terminal receipt and a
/// budget, and `tombstones` event tombstones.
fn inject_legacy(s: &mut State, cohorts: usize, tombstones: usize) {
    for i in 0..cohorts {
        let id = format!("run-0-legacy-{i:04}");
        let mut run = completed_run(&id);
        for kind in ["admitted", "terminal"] {
            s.sequence += 1;
            s.receipts.push(Receipt {
                sequence: s.sequence,
                run: id.clone(),
                step: "main".into(),
                at_ms: 1,
                kind: kind.into(),
                detail: json!({}),
            });
        }
        run.terminal_sequence = Some(s.sequence);
        s.runs.insert(id.clone(), run);
        s.budgets.insert(id, Budget::default());
    }
    for i in 0..tombstones {
        s.retention
            .event_tombstones
            .insert(format!("tomb-{i:04}"), tombstone(&format!("d-{i}")));
    }
}
/// Store `s` over the namespace as an older engine stored it: indexes not
/// initialized, and no drain state.
fn store_legacy<S: Storage>(store: &S, mut s: State) {
    s.indexes.initialized = false;
    let mut value = serde_json::to_value(&s).unwrap();
    value["retention"].as_object_mut().unwrap().remove("drain");
    let s: State = serde_json::from_value(value).unwrap();
    store.acquire("test", "writer").unwrap();
    let revision = store.load("test").unwrap().revision;
    store
        .compare_and_swap("test", "writer", revision, &s)
        .unwrap();
    store.release("test", "writer").unwrap();
}
/// A legacy namespace (the brief's helper): `"a"` installed and activated
/// through a throwaway engine, then `cohorts` settled legacy cohorts and
/// `tombstones` event tombstones; `edit` may change the State first.
fn legacy_with(cohorts: usize, tombstones: usize, edit: impl FnOnce(&mut State)) -> MemoryStorage {
    let store = MemoryStorage::retaining_settled();
    let e = Engine::new(store.clone(), "test", "setup").unwrap();
    install(&e, done_artifact(), "a");
    e.release().unwrap();
    let mut s = store.load("test").unwrap().state;
    inject_legacy(&mut s, cohorts, tombstones);
    edit(&mut s);
    store_legacy(&store, s);
    store
}
fn legacy(cohorts: usize, tombstones: usize) -> MemoryStorage {
    legacy_with(cohorts, tombstones, |_| {})
}
/// A new namespace `"test"` in `store` holding `state` (an older engine's
/// State, which has no retention clock), written by direct CAS.
fn fresh_legacy<S: Storage>(store: S, state: &State) -> S {
    store.acquire("test", "writer").unwrap();
    store.compare_and_swap("test", "writer", 0, state).unwrap();
    store.release("test", "writer").unwrap();
    store
}
fn legacy_ids(e: &Engine<impl Storage>) -> Vec<String> {
    inspect_all(e, InspectionKind::Runs, None)
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .filter(|id| id.starts_with("run-0-legacy-"))
        .collect()
}
/// `state` as the engine aq-server vendors (0173673) stores it: none of the
/// fields this branch added (window, eviction order, clock, drain, new
/// policy fields, new indexes, dispatch sequences).
fn old_engine_shape(state: &State) -> State {
    let mut value = serde_json::to_value(vendored_shape(state)).unwrap();
    let retention = value["retention"].as_object_mut().unwrap();
    for f in [
        "clock_ms",
        "expiry",
        "admission_expires_at_ms",
        "stale_floor_ms",
        "replay_order",
        "replay_held",
        "drain",
    ] {
        retention.remove(f);
    }
    let policy = retention["policy"].as_object_mut().unwrap();
    for f in [
        "admission_replay_ms",
        "replay_keep",
        "history_keep",
        "drain_cohorts_per_commit",
        "drain_entries_per_commit",
    ] {
        policy.remove(f);
    }
    for intent in value["intents"].as_object_mut().unwrap().values_mut() {
        intent.as_object_mut().unwrap().remove("dispatch_sequence");
    }
    serde_json::from_value(value).unwrap()
}

#[path = "live_state/legacy_completions.rs"]
mod legacy_completions;
#[path = "live_state/legacy_drain.rs"]
mod legacy_drain;
#[path = "live_state/vendored_upgrade.rs"]
mod vendored_upgrade;

#[path = "fixtures/rebase_cases.rs"]
mod rebase_cases;
#[path = "live_state/rebase_fences.rs"]
mod rebase_fences;
use rebase_cases::*;

#[path = "live_state/transition_fences.rs"]
mod transition_fences;

#[path = "live_state/fences.rs"]
mod fences;
#[path = "live_state/removal_fences.rs"]
mod removal_fences;
use fences::{check_refusal, seed_state};

#[path = "live_state/history_retention.rs"]
mod history_retention;

#[path = "live_state/history_storage.rs"]
mod history_storage;
use history_storage::{live_receipt, page, refused_as_also_live};

fn settle_during_rebase(n: &mut State) {
    let run = n.runs.remove("run-a").unwrap();
    let op = n.intents.remove("op-a").unwrap();
    n.receipts.retain(|r| r.sequence != 1);
    n.settled
        .push(cohort(run, vec![op], vec![audit(1, "run-a")]));
}

#[path = "live_state/namespace_capacity.rs"]
mod namespace_capacity;

#[path = "live_state/retention_bounds.rs"]
mod retention_bounds;

#[path = "live_state/garbage_collection.rs"]
mod garbage_collection;

#[path = "live_state/completion_lapses.rs"]
mod completion_lapses;
