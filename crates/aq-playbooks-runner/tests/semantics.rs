#[path = "fixtures/interrupted_storage.rs"]
pub mod interrupted_storage;
use interrupted_storage::{CommitOutcome, InterruptedStorage};
#[path = "fixtures/async_operation.rs"]
mod async_operation;
use async_operation::*;
#[path = "fixtures/engine.rs"]
mod engine_fixtures;
use aq_playbooks_model::*;
use aq_playbooks_runner::*;
use engine_fixtures::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
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
/// Every public inspection item of `kind` (live state plus history).
fn inspected(e: &Engine<MemoryStorage>, kind: InspectionKind, run: Option<&str>) -> Vec<Value> {
    let mut items = vec![];
    let mut after = None;
    loop {
        let page = e
            .inspect(&InspectionQuery {
                kind,
                run: run.map(String::from),
                after,
                limit: 1000,
            })
            .unwrap();
        items.extend(page.items);
        match page.next {
            Some(next) => after = Some(next),
            None => return items,
        }
    }
}
/// A run's public projection, live or in history.
fn inspected_run(e: &Engine<MemoryStorage>, id: &str) -> Value {
    let runs = inspected(e, InspectionKind::Runs, Some(id));
    assert_eq!(runs.len(), 1, "run {id}");
    runs.into_iter().next().unwrap()
}
#[path = "semantics/activation.rs"]
mod activation;
#[path = "semantics/async_operations.rs"]
mod async_operations;
#[path = "semantics/effects.rs"]
mod effects;
#[path = "semantics/workflows.rs"]
mod workflows;

#[test]
fn event_before_wait_restart_stale_revision_and_backlog_deadline() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    let id = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.admit_event("host", event("stale", 1), 10).unwrap();
    let accepted = e.admit_event("host", event("eligible", 2), 99).unwrap();
    assert_eq!(accepted.destinations, 0);
    assert!(
        e.admit_event("host", event("eligible", 2), 101)
            .unwrap()
            .duplicate
    );
    assert_eq!(e.metrics.snapshot().duplicate_events, 1);
    e.release().unwrap();
    let resumed = Engine::new(e.store.clone(), "test", "incarnation-2").unwrap();
    resumed.advance(&id, 0).unwrap();
    // The wait has its winner; the run finishes on its next step and leaves.
    assert_eq!(
        resumed
            .snapshot()
            .unwrap()
            .waits
            .values()
            .next()
            .unwrap()
            .winner
            .as_deref(),
        Some(accepted.key.as_str())
    );
    resumed.advance(&id, 150).unwrap();
    assert!(matches!(resumed.advance(&id, 151), Err(Error::NotFound(_))));
    assert_eq!(settled(&resumed, &id).outcome.as_deref(), Some("ok"));
    assert!(resumed.snapshot().unwrap().waits.is_empty());
}

#[test]
fn expiry_wins_over_late_event_and_identity_conflicts() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    let id = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    e.admit_event("host", event("late", 2), 101).unwrap();
    e.advance(&id, 110).unwrap();
    e.advance(&id, 111).unwrap();
    // The finished run has left the State; history has its outcome.
    assert!(!e.snapshot().unwrap().runs.contains_key(&id));
    assert_eq!(inspected_run(&e, &id)["outcome"], "error");
    let mut changed = event("late", 3);
    assert!(matches!(
        e.admit_event("host", changed.clone(), 112),
        Err(Error::Conflict(_))
    ));
    changed.producer = "impostor".into();
    assert!(matches!(
        e.admit_event("host", changed, 113),
        Err(Error::Refused(_))
    ));
}

#[test]
fn an_expired_event_identity_is_judged_by_time_and_the_epoch_fence_refuses_old_epochs() {
    let e = engine();
    // Activated, so GC's artifact phase keeps the event contract.
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let envelope = event("expired", 2);
    let disposition = e.admit_event("host", envelope.clone(), 0).unwrap();
    // The commit's window drop forgets it; spec 0003 writes no tombstone.
    assert_eq!(e.collect_garbage(2000, 10).unwrap().events, 1);
    let s = e.snapshot().unwrap();
    assert!(!s.events.contains_key(&disposition.key));
    assert!(s.retention.event_tombstones.is_empty());
    // A repeat with its producer time is stale and writes nothing.
    let stale = e.admit_event_at("host", envelope.clone(), 0, Some(0), 2001);
    assert!(matches!(stale, Err(Error::Stale { .. })), "{stale:?}");
    assert!(!e.snapshot().unwrap().events.contains_key(&disposition.key));
    e.advance_producer_epoch(
        "host",
        1,
        "operator",
        "producer retired all earlier deliveries",
        2002,
    )
    .unwrap();
    assert!(e.snapshot().unwrap().retention.event_tombstones.is_empty());
    assert!(e.admit_event("host", envelope, 2003).is_err());
}

/// Spec 0003 (NR-1): a wait registered after its event's window has passed
/// is no longer refused; it takes its timeout edge. The event's captured
/// replay horizon (the contract's 1_000 ms) still outlives a shorter policy.
#[test]
fn expired_general_wait_history_times_out_and_captured_replay_horizon_survives_policy_change() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let envelope = event("history", 2);
    let first = e.admit_event("host", envelope.clone(), 0).unwrap();
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    assert!(
        e.admit_event("host", envelope.clone(), 999)
            .unwrap()
            .duplicate
    );
    // After its window the key is forgotten: the same event is new.
    assert!(!e.admit_event("host", envelope, 1001).unwrap().duplicate);
    let id = e
        .admit("waiter", "main", Value::Null, authority(), 2001)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(!s.events.contains_key(&first.key));
    assert!(s.retention.expired_correlations.is_empty());
    e.drive(&mut FakeHost::default(), 2001, 10).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Waiting);
    e.drive(&mut FakeHost::default(), 2102, 10).unwrap();
    assert_eq!(settled(&e, &id).outcome.as_deref(), Some("error"));
    assert!(e.snapshot().unwrap().waits.is_empty());
}

/// Spec 0003 (NR-1): the event leaves at its window without a correlation
/// fence, so retiring the producer epoch keeps none, and the invocation's
/// late wait takes its timeout edge.
#[test]
fn producer_epoch_retirement_leaves_a_wait_after_an_expired_event_to_time_out() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let id = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.admit_event("host", event("before-delayed-wait", 2), 0)
        .unwrap();
    for _ in 0..10 {
        if !e.collect_garbage(2000, 1).unwrap().more {
            break;
        }
    }
    e.advance_producer_epoch("host", 1, "operator", "delivery horizon retired", 2000)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.events.is_empty());
    assert!(s.retention.expired_correlations.is_empty() && s.retention.event_tombstones.is_empty());
    e.drive(&mut FakeHost::default(), 2001, 10).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Waiting);
    e.drive(&mut FakeHost::default(), 2102, 10).unwrap();
    assert_eq!(settled(&e, &id).outcome.as_deref(), Some("error"));
}
