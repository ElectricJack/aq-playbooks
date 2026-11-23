use super::{State, eviction_entries::*, retention};
use serde_json::json;

pub(super) type EvictionCase = (&'static str, &'static str, State, Box<dyn Fn(&mut State)>);

pub(super) fn common_refusals(base: &State, held_event: &State) -> Vec<EvictionCase> {
    held_and_ordered_removals(base, held_event)
        .into_iter()
        .chain(forged_order_entries(base))
        .chain(changed_events(base))
        .collect()
}

fn held_and_ordered_removals(base: &State, held_event: &State) -> [EvictionCase; 5] {
    [
        (
            "a held keyed receipt",
            retention::REFUSE_EVICTION,
            base.clone(),
            Box::new(|n| ReplayMember::Keyed("kh").evict(n)),
        ),
        (
            "an event a live waiter holds",
            retention::REFUSE_EVICTION,
            held_event.clone(),
            Box::new(|n| ReplayMember::Event("ew").evict(n)),
        ),
        (
            "fewer than replay_keep left",
            retention::REFUSE_EVICTION,
            base.clone(),
            Box::new(|n| {
                hold_out(n, "kh");
                ReplayMember::Keyed("k0").evict(n);
                ReplayMember::Keyed("k1").evict(n);
            }),
        ),
        (
            "not the oldest",
            retention::REFUSE_EVICTION,
            base.clone(),
            Box::new(|n| ReplayMember::Keyed("k1").evict(n)),
        ),
        (
            "an entry outside the order, not the oldest",
            retention::REFUSE_EARLY_REMOVAL,
            base.clone(),
            Box::new(|n| ReplayMember::Event("ex").evict(n)),
        ),
    ]
}

fn forged_order_entries(base: &State) -> [EvictionCase; 3] {
    [
        (
            "an order entry without its item",
            retention::REFUSE_EVICTION,
            base.clone(),
            Box::new(|n| {
                n.retention
                    .replay_order
                    .insert(retention::replay_key(30, "k:ghost"), ());
            }),
        ),
        (
            "an order entry for a held item",
            retention::REFUSE_EVICTION,
            base.clone(),
            Box::new(|n| {
                let r = n.keyed_admissions.get("kh2").unwrap().clone();
                n.retention
                    .replay_order
                    .insert(retention::admission_order_key(&r), ());
            }),
        ),
        (
            "an order entry at another sequence",
            retention::REFUSE_EVICTION,
            base.clone(),
            Box::new(|n| {
                n.retention
                    .replay_order
                    .insert(retention::replay_key(30, "k:k3"), ());
            }),
        ),
    ]
}

fn changed_events(base: &State) -> [EvictionCase; 2] {
    [
        (
            "an event re-sequenced to the head",
            retention::REFUSE_EVENT_CHANGE,
            base.clone(),
            Box::new(|n| {
                let mut e = n.events.get("e4").unwrap().clone();
                n.retention
                    .replay_order
                    .remove(&retention::event_order_key(&e));
                e.sequence = 0;
                n.retention
                    .replay_order
                    .insert(retention::event_order_key(&e), ());
                n.events.insert("e4".into(), e);
            }),
        ),
        (
            "an event's envelope changed",
            retention::REFUSE_EVENT_CHANGE,
            base.clone(),
            Box::new(|n| n.events.get_mut("e4").unwrap().envelope.payload = json!(1)),
        ),
    ]
}
