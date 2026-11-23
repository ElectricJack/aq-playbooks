use super::*;

/// Rulings 1 and 2 (round 2): a commit with a licence receipt must be
/// exactly that licence and do nothing else; without one, the clock never
/// decreases and the floor never changes. Cases on `fence_state()` (clock
/// 500; windows 2_000 and 3_000; "k" 3_000, "ev" 4_000, "ev-long" 9_000
/// with a 5_000 contract window), each (label, refusal, change).
pub(super) fn rebase_refusal_cases() -> Vec<Case> {
    invalid_clock_changes()
        .into_iter()
        .chain(mixed_work())
        .chain(invalid_expiry_changes())
        .chain(mixed_configuration_changes())
        .chain(mixed_catalog_changes())
        .collect()
}

fn invalid_clock_changes() -> [Case; 7] {
    [
        (
            "clock lowered without a rebase",
            retention::REFUSE_CLOCK,
            |n| n.retention.clock_ms = Some(100),
        ),
        (
            "clock lowered, rebase receipt left live",
            retention::REFUSE_CLOCK,
            |n| {
                n.retention.clock_ms = Some(100);
                n.sequence += 1;
                n.receipts.push(licence_receipt(
                    retention::CLOCK_REBASE,
                    n.sequence,
                    500,
                    100,
                ));
            },
        ),
        (
            "clock lowered, rebase from another clock",
            retention::REFUSE_LICENCE,
            |n| {
                n.retention.clock_ms = Some(100);
                license(n, retention::CLOCK_REBASE, 600, 100);
            },
        ),
        (
            "clock lowered, rebase to another clock",
            retention::REFUSE_LICENCE,
            |n| {
                n.retention.clock_ms = Some(100);
                license(n, retention::CLOCK_REBASE, 500, 200);
            },
        ),
        (
            "review probe: a receipt from 7 to 500, the clock left at 500, expiries lowered",
            retention::REFUSE_LICENCE,
            |n| {
                license(n, retention::CLOCK_REBASE, 7, 500);
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_500);
                n.retention.event_expires_at_ms.insert("ev".into(), 3_500);
            },
        ),
        ("rebase upwards", retention::REFUSE_LICENCE, |n| {
            n.retention.clock_ms = Some(600);
            license(n, retention::CLOCK_REBASE, 500, 600);
        }),
        (
            "replayed rebase receipt (not this commit's sequence)",
            retention::REFUSE_LICENCE,
            |n| {
                rebased(n);
                let replayed = n.sequence - 1;
                n.settled[0].receipts[0].sequence = replayed;
            },
        ),
    ]
}

fn mixed_work() -> [Case; 4] {
    [
        ("rebase that also admits", retention::REFUSE_LICENCE, |n| {
            rebased(n);
            n.sequence += 1;
            n.keyed_admissions.insert("new".into(), receipt("new", 100));
        }),
        (
            "rebase that also accepts an event",
            retention::REFUSE_LICENCE,
            |n| {
                rebased(n);
                n.events
                    .insert("ev-new".into(), accepted_event("ev-new", 100));
            },
        ),
        (
            "rebase that also settles a run",
            retention::REFUSE_LICENCE,
            |n| {
                rebased(n);
                let run = n.runs.remove("run-a").unwrap();
                let op = n.intents.remove("op-a").unwrap();
                n.receipts.retain(|r| r.sequence != 1);
                n.settled
                    .push(cohort(run, vec![op], vec![audit(1, "run-a")]));
            },
        ),
        (
            "two licences in one commit",
            retention::REFUSE_LICENCE,
            |n| {
                rebased(n);
                n.settled[0].receipts.push(licence_receipt(
                    retention::STALE_FLOOR_LOWERED,
                    77,
                    50,
                    10,
                ));
            },
        ),
    ]
}

fn invalid_expiry_changes() -> [Case; 6] {
    [
        (
            "expiry lowered without a rebase",
            retention::REFUSE_EXPIRY,
            |n| {
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_100);
            },
        ),
        (
            "expiry lowered with a rebase the clock did not take",
            retention::REFUSE_LICENCE,
            |n| {
                license(n, retention::CLOCK_REBASE, 500, 100);
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_100);
            },
        ),
        (
            "expiry capped below its cap",
            retention::REFUSE_EXPIRY,
            |n| {
                rebased(n);
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_099);
            },
        ),
        (
            "expiry lowered, but not to its cap",
            retention::REFUSE_EXPIRY,
            |n| {
                rebased(n);
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 2_200);
            },
        ),
        (
            "event expiry capped below its cap",
            retention::REFUSE_EXPIRY,
            |n| {
                rebased(n);
                n.retention.event_expires_at_ms.insert("ev".into(), 3_099);
            },
        ),
        (
            "event capped to the policy window, its contract window is longer",
            retention::REFUSE_EXPIRY,
            |n| {
                rebased(n);
                n.retention
                    .event_expires_at_ms
                    .insert("ev-long".into(), 3_100);
            },
        ),
    ]
}

fn mixed_configuration_changes() -> [Case; 5] {
    [
        (
            "floor raised without a licence",
            retention::REFUSE_STALE_FLOOR,
            |n| n.retention.stale_floor_ms = Some(50),
        ),
        (
            "review probe C2: a rebase that also sets both windows to 1 and caps to 101",
            retention::REFUSE_LICENCE,
            |n| {
                rebased(n);
                n.retention.policy.admission_replay_ms = 1;
                n.retention.policy.event_replay_ms = 1;
                n.retention.admission_expires_at_ms.insert("k".into(), 101);
                n.retention.event_expires_at_ms.insert("ev".into(), 101);
            },
        ),
        (
            "rebase that also changes the policy",
            retention::REFUSE_LICENCE,
            |n| {
                rebased(n);
                n.retention.policy.audit_ms += 1;
            },
        ),
        (
            "rebase that also installs an artifact",
            retention::REFUSE_LICENCE,
            |n| {
                rebased(n);
                let extra = n.artifacts.values().next().unwrap().clone();
                let mut content = extra.content.clone();
                content.compiler = "another".into();
                let a = Artifact::seal(content).unwrap();
                n.artifacts.insert(a.digest.clone(), a);
            },
        ),
        (
            "rebase that also changes a producer epoch",
            retention::REFUSE_LICENCE,
            |n| {
                rebased(n);
                n.retention.producer_epochs.insert("host".into(), 1);
            },
        ),
    ]
}

fn mixed_catalog_changes() -> [Case; 4] {
    [
        (
            "rebase that also moves the catalog",
            retention::REFUSE_LICENCE,
            |n| {
                rebased(n);
                n.catalog_revision += 1;
            },
        ),
        (
            "rebase that also changes the catalog history",
            retention::REFUSE_LICENCE,
            |n| {
                rebased(n);
                n.catalog_history_at_ms.insert(99, 1);
            },
        ),
        (
            "rebase that also changes an event's contract to shrink its cap",
            // Task 8b fix round 1: an accepted event's envelope is immutable,
            // so this is refused before its cap is judged.
            retention::REFUSE_EVENT_CHANGE,
            |n| {
                rebased(n);
                let mut e = n.events.get("ev-long").unwrap().clone();
                e.envelope.contract.name = "test.other".into();
                n.events.insert("ev-long".into(), e);
                n.retention
                    .event_expires_at_ms
                    .insert("ev-long".into(), 3_100);
            },
        ),
        (
            "rebase raises the floor past its target",
            retention::REFUSE_STALE_FLOOR,
            |n| {
                rebased(n);
                n.retention.stale_floor_ms = Some(101);
            },
        ),
    ]
}

/// After that commit no licence is left, and the floor changes only with
/// its own exact receipt. Changes of the stored state after the rebase
/// (clock 100, floor 50).
pub(super) fn after_rebase_refusal_cases() -> Vec<Case> {
    unlicensed_clock_and_floor_changes()
        .into_iter()
        .chain(incorrect_floor_licences())
        .collect()
}

fn unlicensed_clock_and_floor_changes() -> [Case; 4] {
    [
        ("clock lowered again", retention::REFUSE_CLOCK, |n| {
            n.retention.clock_ms = Some(90)
        }),
        (
            "stale floor lowered without a licence",
            retention::REFUSE_STALE_FLOOR,
            |n| n.retention.stale_floor_ms = Some(40),
        ),
        (
            "stale floor raised without a licence",
            retention::REFUSE_STALE_FLOOR,
            |n| n.retention.stale_floor_ms = Some(60),
        ),
        ("stale floor removed", retention::REFUSE_STALE_FLOOR, |n| {
            n.retention.stale_floor_ms = None
        }),
    ]
}

fn incorrect_floor_licences() -> [Case; 5] {
    [
        (
            "floor lowered to another value than its receipt",
            retention::REFUSE_LICENCE,
            |n| {
                n.retention.stale_floor_ms = Some(30);
                license(n, retention::STALE_FLOOR_LOWERED, 50, 40);
            },
        ),
        (
            "floor lowering from another floor",
            retention::REFUSE_LICENCE,
            |n| {
                n.retention.stale_floor_ms = Some(40);
                license(n, retention::STALE_FLOOR_LOWERED, 60, 40);
            },
        ),
        (
            "floor lowering that also moves the clock",
            retention::REFUSE_LICENCE,
            |n| {
                n.retention.stale_floor_ms = Some(40);
                n.retention.clock_ms = Some(200);
                license(n, retention::STALE_FLOOR_LOWERED, 50, 40);
            },
        ),
        (
            "floor lowering that also changes the policy",
            retention::REFUSE_LICENCE,
            |n| {
                n.retention.policy.admission_replay_ms = 1;
                floor_lowered(n);
            },
        ),
        (
            "floor lowering that also changes an expiry",
            retention::REFUSE_EXPIRY,
            |n| {
                n.retention.stale_floor_ms = Some(40);
                n.retention
                    .admission_expires_at_ms
                    .insert("k".into(), 9_000);
                license(n, retention::STALE_FLOOR_LOWERED, 50, 40);
            },
        ),
    ]
}

/// The floor lowering the fence accepts after `rebased`: 50 to 40.
pub(super) fn floor_lowered(n: &mut State) {
    n.retention.stale_floor_ms = Some(40);
    license(n, retention::STALE_FLOOR_LOWERED, 50, 40);
}
fn rebase_fences_hold_on_cas(store: &dyn Storage) {
    let s = fence_state();
    let count = std::cell::Cell::new(0);
    let fresh = |state: &State| {
        count.set(count.get() + 1);
        let ns = format!("rebase-{}", count.get());
        let base = seed_state(store, &ns, state);
        (ns, base)
    };
    let mut next = s.clone();
    rebased(&mut next);
    let before = rebase_refusal_cases().into_iter().map(|case| {
        let (ns, revision) = fresh(&s);
        check_refusal(store, &ns, (revision, &s), case)
    });
    let after = after_rebase_refusal_cases().into_iter().map(|case| {
        let (ns, base) = fresh(&s);
        let revision = store.compare_and_swap(&ns, "w", base, &next).unwrap();
        let stored = store.load(&ns).unwrap().state;
        check_refusal(store, &ns, (revision, &stored), case)
    });
    let failures: Vec<_> = before.chain(after).flatten().collect();
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
