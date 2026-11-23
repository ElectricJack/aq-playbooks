use super::{accepted_event, completed_run, receipt};
use aq_playbooks_runner::{Pending, Run, RunStatus, State, WaitRecord, indexes, retention};
use std::collections::BTreeSet;

/// A held live admission can still be at the head of the order until the next commit.
pub(super) enum KeyPosition {
    Ordered,
    OrderedLive,
    Held,
}
pub(super) const EVICTION_WINDOW: i64 = 100_000;

pub(super) fn running_run(id: &str) -> Run {
    let mut run = completed_run(id);
    run.status = RunStatus::Running;
    run.outcome = None;
    run.terminal_sequence = None;
    run
}

/// A keyed admission with the given sequence, run state and replay position.
pub(super) fn keyed_entry(s: &mut State, key: &str, sequence: u64, position: KeyPosition) {
    let mut r = receipt(key, 0);
    r.run_id = format!("run-{sequence:016x}");
    s.keyed_admissions.insert(key.into(), r.clone());
    s.retention
        .admission_expires_at_ms
        .insert(key.into(), EVICTION_WINDOW);
    s.retention.expiry.insert(
        retention::window_key(EVICTION_WINDOW, &format!("k:{key}")),
        (),
    );
    if !matches!(position, KeyPosition::Ordered) {
        s.runs.insert(r.run_id.clone(), running_run(&r.run_id));
    }
    if !matches!(position, KeyPosition::Held) {
        s.retention
            .replay_order
            .insert(retention::admission_order_key(&r), ());
    } else {
        s.retention.replay_held.insert(r.run_id, key.into());
    }
}

pub(super) fn event_entry(s: &mut State, key: &str, sequence: u64, in_order: bool) {
    let mut e = accepted_event(key, 0);
    e.sequence = sequence;
    if in_order {
        s.retention
            .replay_order
            .insert(retention::event_order_key(&e), ());
    }
    s.events.insert(key.into(), e);
    s.retention
        .event_expires_at_ms
        .insert(key.into(), EVICTION_WINDOW);
    s.retention.expiry.insert(
        retention::window_key(EVICTION_WINDOW, &format!("e:{key}")),
        (),
    );
}

/// Event `key` held by the live waiter `run-w`: a run waiting on a wait
/// record whose correlation key is the event's, with the derived indexes.
pub(super) fn hold_by_waiter(s: &mut State, key: &str) {
    let e = s.events.get(key).unwrap().clone();
    let corr = indexes::correlation_key(
        &e.envelope.contract,
        &e.envelope.scope,
        &e.envelope.correlation,
    )
    .unwrap();
    let mut run = running_run("run-w");
    run.status = RunStatus::Waiting;
    run.pending = Some(Pending::Event {
        wait: "wait-w".into(),
    });
    s.runs.insert("run-w".into(), run);
    s.waits.insert(
        "wait-w".into(),
        WaitRecord {
            id: "wait-w".into(),
            run: "run-w".into(),
            step: "wait".into(),
            event: Some(e.envelope.contract.clone()),
            scope: e.envelope.scope.clone(),
            correlation: e.envelope.correlation.clone(),
            minimum_revision: None,
            deadline: EVICTION_WINDOW,
            bind: "completion".into(),
            winner: None,
            expired: false,
        },
    );
    s.indexes
        .event_memberships
        .insert(key.into(), vec![corr.clone()]);
    s.indexes
        .waiters
        .insert(corr, BTreeSet::from(["run-w".to_string()]));
}

/// A replay member, identified by the storage map it belongs to.
pub(super) enum ReplayMember<'a> {
    Keyed(&'a str),
    Event(&'a str),
}

impl ReplayMember<'_> {
    pub(super) fn evict(self, state: &mut State) {
        let (member, order_key) = match self {
            Self::Keyed(key) => {
                let receipt = state.keyed_admissions.remove(key).unwrap();
                state.retention.admission_expires_at_ms.remove(key);
                (format!("k:{key}"), retention::admission_order_key(&receipt))
            }
            Self::Event(key) => {
                let event = state.events.remove(key).unwrap();
                state.retention.event_expires_at_ms.remove(key);
                state.indexes.event_memberships.remove(key);
                (format!("e:{key}"), retention::event_order_key(&event))
            }
        };
        state
            .retention
            .expiry
            .remove(&retention::window_key(EVICTION_WINDOW, &member));
        state.retention.replay_order.remove(&order_key);
    }
}

/// Take keyed receipt `key`, whose run is live, out of the order into
/// `replay_held`, as `retention::evict` does with a held head.
pub(super) fn hold_out(n: &mut State, key: &str) {
    let r = n.keyed_admissions.get(key).unwrap().clone();
    n.retention
        .replay_order
        .remove(&retention::admission_order_key(&r));
    n.retention.replay_held.insert(r.run_id, key.into());
}

/// The correct eviction of `eviction_state()`.
pub(super) fn evicted_correctly(n: &mut State) {
    hold_out(n, "kh");
    ReplayMember::Keyed("k0").evict(n);
}
