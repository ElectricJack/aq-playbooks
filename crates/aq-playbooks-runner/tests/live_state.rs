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

#[path = "live_state/pinned_clock.rs"]
mod pinned_clock;

#[path = "live_state/rebase_deadlines.rs"]
mod rebase_deadlines;

#[path = "fixtures/eviction_entries.rs"]
mod eviction_entries;
use eviction_entries::*;
#[path = "fixtures/eviction_cases.rs"]
mod eviction_cases;
#[path = "fixtures/eviction_commits.rs"]
mod eviction_commits;
use eviction_cases::{EvictionCase, common_refusals};

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
#[path = "live_state/replay_eviction.rs"]
mod replay_eviction;

fn only_intent(s: &State) -> Intent {
    assert_eq!(s.intents.len(), 1);
    s.intents.values().next().unwrap().clone()
}

#[path = "live_state/operation_holds.rs"]
mod operation_holds;

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

#[path = "live_state/eviction_fences.rs"]
mod eviction_fences;

use eviction_fences::{
    bulk_eviction_state, eviction_refusal_cases, eviction_state, held_event_state,
    seed_eviction_state,
};

use vendored_upgrade::vendored_shape;

#[path = "live_state/root_integrity.rs"]
mod root_integrity;
