use super::*;

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
            FixtureContracts::default(),
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
    // The cut-off millisecond itself is stale, as is the preceding one.
    for age in [600_001, 600_000] {
        assert!(stale(e.admit_once_at(
            keyed(&e, "k1"),
            Some(now - age),
            now
        )));
    }
    let first = e
        .admit_once_at(keyed(&e, "k1"), Some(now - 599_999), now)
        .unwrap();
    assert!(!first.duplicate);
    assert!(
        e.admit_once_at(keyed(&e, "k1"), Some(0), now + 1)
            .unwrap()
            .duplicate
    );
    for age in [600_001, 600_000] {
        assert!(stale(e.admit_event_at(
            "host",
            event("e1", 2),
            0,
            Some(now - age),
            now
        )));
    }
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
/// Carried item 1: an expired key or event the engine still holds in the
/// record (no commit has dropped it yet) is forgotten by the first of two
/// commits. A stale request is refused before that commit: nothing is
/// written and the expired entry stays.
fn a_stale_request_for_an_expired_entry_writes_nothing<S: Storage>(
    e: &Engine<S>,
    event_first: bool,
) {
    install_on(e, wait_artifact(), "waiter");
    short_windows(e);
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
    assert_eq!(after.state.keyed_admissions["k"], first.receipt);
    assert!(after.state.events.contains_key(&accepted.key));
    assert_eq!(
        (after.revision, after.state.retention.clock_ms),
        (before.revision, before.state.retention.clock_ms)
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
    short_windows(&e);
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
    for receipt in [k_future.receipt, k_now.receipt] {
        assert_eq!(receipt, k_none.receipt);
    }
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
        for part in [
            &real.to_string(),
            &pinned.to_string(),
            "retention clock",
            "ahead of host time",
        ] {
            assert!(message.contains(part), "{part}: {message}");
        }
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
    let (e, first, accepted) = at_expiry();
    let s = e.snapshot().unwrap();
    assert_eq!(
        (
            s.retention.admission_expires_at_ms.get("k").copied(),
            s.retention.event_expires_at_ms.get(&accepted.key).copied()
        ),
        (Some(2_000), Some(2_000))
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
    let (e, _, _) = at_expiry();
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
fn at_expiry() -> (
    Engine<MemoryStorage>,
    KeyedAdmissionDisposition,
    EventDisposition,
) {
    let e = ret_engine();
    install(&e, wait_artifact(), "waiter");
    short_windows(&e);
    let first = e.admit_once_at(keyed(&e, "k"), Some(1_000), 1_000).unwrap();
    assert!(e.advance(&first.receipt.run_id, 1_000).unwrap());
    let accepted = e
        .admit_event_at("host", event("e1", 2), 0, Some(1_000), 1_000)
        .unwrap();
    (e, first, accepted)
}
