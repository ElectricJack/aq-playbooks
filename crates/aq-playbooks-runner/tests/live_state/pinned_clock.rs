use super::*;
const NORMAL: i64 = 10_000;
const PINNED: i64 = NORMAL + 1_000_000_000;
const DURING: i64 = NORMAL + 200;
const REBASED: i64 = NORMAL + 500;
struct LiveEntries {
    live: String,
    done: String,
    waiter: String,
    event: String,
}
fn json(s: &State) -> Value {
    serde_json::to_value(s).unwrap()
}
/// The pinned-clock scenario (ruling 3) on any backend. Windows: keyed
/// 1_000, events max(1, contract 1_000). Real time starts at `NORMAL`.
fn pinned_clock_is_rebased_and_normal_operation_resumes<S: Storage>(e: &Engine<S>) {
    install_on(e, wait_artifact(), "waiter");
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let fixtures = pin(e);
    requests_with_real_time_are_stale(e);
    let (before, pinned_event) = admit_during_pin(e);
    invalid_rebases_write_nothing(e, &before);
    let report = verify_rebase(e, &before);
    let after = e.snapshot().unwrap();
    verify_capped_entries(&after, &report, &fixtures.event, &pinned_event);
    verify_rebase_receipt(e, &after);
    let during = resume(e);
    verify_held_entries(e, &fixtures);
    let again = verify_early_forgotten_entries(e, &fixtures);
    verify_rolling_record(e, [&fixtures.live, &during, &again]);
}
fn pin(e: &Engine<impl Storage>) -> LiveEntries {
    // Normal operation: a finished keyed run, a keyed run still live, a
    // waiter holding an event, and an unheld event.
    let done = e
        .admit_once_at(keyed_on(e, "done"), Some(NORMAL), NORMAL)
        .unwrap();
    assert!(e.advance(&done.receipt.run_id, NORMAL).unwrap());
    let live = e
        .admit_once_at(keyed_on(e, "live"), Some(NORMAL), NORMAL)
        .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), NORMAL)
        .unwrap();
    e.advance(&waiter, NORMAL).unwrap();
    let held = e
        .admit_event_at("host", event("held", 1), 0, Some(NORMAL), NORMAL)
        .unwrap();
    e.admit_event_at(
        "host",
        correlated_event("free", "inv-free"),
        0,
        Some(NORMAL),
        NORMAL,
    )
    .unwrap();
    // One far-future host time pins the clock. Its drop forgets "done" and
    // "free" a whole window early; "live" and "held" are deferred from the
    // pin.
    e.admit("a", "main", Value::Null, authority(), PINNED)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!((s.retention.clock_ms, s.events.len()), (Some(PINNED), 1));
    assert!(!s.keyed_admissions.contains_key("done"));
    assert_eq!(
        s.retention.admission_expires_at_ms.get("live").copied(),
        Some(PINNED + 1_000)
    );
    LiveEntries {
        live: live.receipt.run_id,
        done: done.receipt.run_id,
        waiter,
        event: held.key,
    }
}
fn requests_with_real_time_are_stale(e: &Engine<impl Storage>) {
    // Back in real time: every request that carries a request time is
    // stale, and the refusal writes nothing.
    let before = e.snapshot().unwrap();
    let refused = e
        .admit_once_at(keyed_on(e, "during"), Some(DURING), DURING)
        .unwrap_err();
    assert!(
        matches!(refused, Error::Stale { clock_ms: Some(c), now_ms, .. } if c == PINNED && now_ms == DURING)
    );
    assert!(refused.to_string().contains("ahead of host time"));
    assert!(stale(e.admit_event_at(
        "host",
        correlated_event("during-e", "inv-during"),
        0,
        Some(DURING),
        DURING
    )));
    assert_eq!(json(&e.snapshot().unwrap()), json(&before));
}
fn admit_during_pin(e: &Engine<impl Storage>) -> (State, String) {
    // Requests without a request time go through, and what they write
    // expires from the pin: the record only grows.
    for i in 0..5 {
        let d = e
            .admit_once(keyed_on(e, &format!("pin-{i}")), DURING + i)
            .unwrap();
        e.advance(&d.receipt.run_id, DURING + i).unwrap();
    }
    // An unheld event accepted during the pin, of a contract whose window
    // (1_000) is longer than the policy's (1).
    let pin_event = e
        .admit_event("host", correlated_event("pin-e", "inv-pin"), DURING)
        .unwrap();
    e.admit("a", "main", Value::Null, authority(), DURING + 5_000)
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
            .is_some_and(|at| *at > PINNED)
    );
    (s, pin_event.key)
}
fn invalid_rebases_write_nothing(e: &Engine<impl Storage>, s: &State) {
    // The rebase: refused unless now <= to <= now + slack (0 here: a tenth
    // of the smallest window, 1 ms) and the floor is in the past.
    for (to, floor, now) in [
        (PINNED, NORMAL, REBASED),
        (PINNED + 1, NORMAL, REBASED),
        (REBASED - 1, NORMAL, REBASED),
        (REBASED + 1, NORMAL, REBASED),
        (REBASED, REBASED + 1, REBASED),
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
        e.rebase_retention_clock(REBASED, NORMAL, "", "pinned clock", REBASED)
            .is_err()
    );
    assert_eq!(json(&e.snapshot().unwrap()), json(s));
}
fn verify_rebase(e: &Engine<impl Storage>, s: &State) -> ClockRebaseReport {
    // A dry run reports the same and commits nothing.
    let preview = e
        .rebase_retention_clock_dry_run(REBASED, NORMAL, "ops", "pinned clock", REBASED)
        .unwrap();
    assert_eq!(json(&e.snapshot().unwrap()), json(s));
    let report = e
        .rebase_retention_clock(REBASED, NORMAL, "ops", "pinned clock", REBASED)
        .unwrap();
    assert_eq!(preview, report);
    let counts = (report.capped_admissions, report.capped_events);
    assert_eq!(
        (report.from_ms, report.to_ms, report.stale_floor_ms),
        (PINNED, REBASED, NORMAL)
    );
    assert_eq!(counts, (6, 2));
    // The run admitted at the pinned time has its duration basis capped.
    assert!(report.needs_operator_action.is_empty() && !report.adjusted_runs.is_empty());
    report
}
fn verify_capped_entries(
    s: &State,
    report: &ClockRebaseReport,
    held_event: &str,
    pinned_event: &str,
) {
    assert_eq!(
        (s.retention.clock_ms, s.retention.stale_floor_ms),
        (Some(REBASED), Some(NORMAL))
    );
    for key in ["live", "pin-0", "pin-4"] {
        assert_eq!(
            s.retention.admission_expires_at_ms.get(key).copied(),
            Some(REBASED + 1_000),
            "{key}"
        );
    }
    // Both events use their contract's retention window.
    for key in [held_event, pinned_event] {
        assert_eq!(
            s.retention.event_expires_at_ms.get(key).copied(),
            Some(REBASED + 1_000),
            "{key}"
        );
    }
    assert!(
        window_entries(s)
            .iter()
            .all(|(at, _)| *at <= REBASED + 1_000)
    );
    for id in &report.adjusted_runs {
        assert_eq!(s.runs[id].admitted_at_ms, REBASED, "{id}");
    }
}
fn verify_rebase_receipt(e: &Engine<impl Storage>, s: &State) {
    // The receipt is history: who, when, the old and new clock.
    let receipts = inspect_all(e, InspectionKind::Receipts, Some("admin"));
    let rebase = receipts
        .iter()
        .find(|r| r["kind"] == retention::CLOCK_REBASE)
        .expect("rebase receipt in history");
    assert_eq!(rebase["at_ms"], REBASED);
    assert!(s.receipts.iter().all(|r| r.kind != retention::CLOCK_REBASE));
}
fn resume(e: &Engine<impl Storage>) -> String {
    // Normal operation resumes. A request time is judged against real time
    // again: one created during the pin is new now.
    let during = e
        .admit_once_at(keyed_on(e, "during"), Some(DURING), REBASED)
        .unwrap();
    assert!(!during.duplicate);
    assert!(
        !e.admit_event_at(
            "host",
            correlated_event("during-e", "inv-during"),
            0,
            Some(DURING),
            REBASED
        )
        .unwrap()
        .duplicate
    );
    // Duplicates inside their window are still recognised.
    for key in ["during", "pin-0"] {
        assert!(
            e.admit_once_at(keyed_on(e, key), Some(DURING), REBASED + 10)
                .unwrap()
                .duplicate
        );
    }
    // Round 3: after an ordinary commit, a key still inside its (capped)
    // window is still remembered.
    e.admit("a", "main", Value::Null, authority(), REBASED + 500)
        .unwrap();
    assert!(
        e.admit_once_at(keyed_on(e, "pin-0"), Some(DURING), REBASED + 999)
            .unwrap()
            .duplicate
    );
    // An event inside its contract window is still a duplicate.
    assert!(
        e.admit_event("host", correlated_event("pin-e", "inv-pin"), REBASED + 10)
            .unwrap()
            .duplicate
    );
    during.receipt.run_id
}
fn verify_held_entries(e: &Engine<impl Storage>, fixtures: &LiveEntries) {
    // Held events and live runs are intact.
    assert!(
        e.admit_event_at("host", event("held", 1), 0, Some(NORMAL), REBASED + 10)
            .unwrap()
            .duplicate
    );
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&fixtures.waiter].status, RunStatus::Waiting);
    assert!(s.runs.contains_key(&fixtures.live));
    assert!(
        e.admit_once_at(keyed_on(e, "live"), Some(NORMAL), REBASED + 10)
            .unwrap()
            .duplicate
    );
}
fn verify_early_forgotten_entries(e: &Engine<impl Storage>, fixtures: &LiveEntries) -> String {
    // The floor: "done" was forgotten a window early by the pin. A retry
    // with its original request time is stale, not a second run. Without a
    // request time it is new, as for any key the engine has forgotten.
    let floor = e
        .admit_once_at(keyed_on(e, "done"), Some(NORMAL), REBASED + 10)
        .unwrap_err();
    assert!(
        matches!(floor, Error::Stale { floor_ms: Some(f), cutoff_ms, .. } if f == NORMAL && cutoff_ms == NORMAL)
    );
    assert!(stale(e.admit_event_at(
        "host",
        correlated_event("free", "inv-free"),
        0,
        Some(NORMAL),
        REBASED + 10
    )));
    let again = e.admit_once(keyed_on(e, "done"), REBASED + 10).unwrap();
    assert!(!again.duplicate);
    assert_ne!(again.receipt.run_id, fixtures.done);
    again.receipt.run_id
}
fn verify_rolling_record(e: &Engine<impl Storage>, runs: [&str; 3]) {
    // The record stops growing: what the pin wrote expires in real time.
    for run in runs {
        assert!(e.advance(run, REBASED + 20).unwrap());
    }
    for i in 0..10 {
        let at = REBASED + 1_001 + i * 200;
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
