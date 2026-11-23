use super::*;

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
pub(super) fn held_event_state() -> State {
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
pub(super) fn eviction_state() -> State {
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

pub(super) fn eviction_refusal_cases() -> Vec<EvictionCase> {
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
pub(super) fn seed_eviction_state(store: &dyn Storage, ns: &str, state: &State) -> u64 {
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
pub(super) fn bulk_eviction_state() -> State {
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
