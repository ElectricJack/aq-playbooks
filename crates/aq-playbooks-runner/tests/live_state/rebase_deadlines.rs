use super::*;

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
    assert_invalid_rebases(&e, now);
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
    short_windows(&e);
    let pinned = PINNED;
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
    let t2 = REBASED;
    let report = rebase(&e);
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
    let expected = vec![
        adjustment(&waiter, "duration_basis", &waiter, (pinned, t2)),
        adjustment(&waiter, "wait", &wait.id, (pinned + 100, t2 + 100)),
        adjustment(&invoke, "duration_basis", &invoke, (pinned, t2)),
        adjustment(&invoke, "operation", &intent.id, (pinned + 100, t2 + 100)),
    ];
    assert_recorded_adjustments(&e, &report, expected);
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
    short_windows(&e);
    let pinned = PINNED;
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
    let report = rebase(&e);
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
    short_windows(&e);
    let pinned = PINNED;
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
    let t2 = REBASED;
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
    install(&e, joiner(), "joiner");
    short_windows(&e);
    let pinned = PINNED;
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
    let report = rebase(&e);
    assert!(report.adjusted_runs.contains(&run));
    assert!(report.needs_operator_action.is_empty());
    assert!(matches!(
        e.snapshot().unwrap().runs[&run].pending,
        Some(Pending::Join { deadline, .. }) if deadline == 15_000
    ));
}
/// N4 (round 3): a join driven at the pinned time (an accepted operation,
/// then `Join` with a 100 ms deadline) is capped at the target plus that
/// deadline.
#[test]
fn a_driven_join_at_the_pinned_time_is_capped_by_its_declared_deadline() {
    let e = ret_engine();
    let (a, _) = async_artifact();
    install(&e, a, "async");
    short_windows(&e);
    let pinned = PINNED;
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
    let report = rebase(&e);
    assert!(report.adjusted_runs.contains(&id));
    assert!(matches!(
        e.snapshot().unwrap().runs[&id].pending,
        Some(Pending::Join { deadline, .. }) if deadline == 10_100
    ));
}

fn joiner() -> Artifact {
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
        FixtureContracts::default(),
    )
}
fn assert_invalid_rebases(e: &Engine<MemoryStorage>, now: i64) {
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
}
fn adjustment(run: &str, kind: &str, item: &str, times: (i64, i64)) -> DeadlineAdjustment {
    let (original_ms, capped_ms) = times;
    DeadlineAdjustment {
        run: run.into(),
        kind: kind.into(),
        item: item.into(),
        original_ms,
        capped_ms,
    }
}
fn assert_recorded_adjustments(
    e: &Engine<MemoryStorage>,
    report: &ClockRebaseReport,
    mut expected: Vec<DeadlineAdjustment>,
) {
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
    expected.sort_by(|a, b| (&a.run, &a.kind).cmp(&(&b.run, &b.kind)));
    let mut got = report.adjustments.clone();
    got.sort_by(|a, b| (&a.run, &a.kind).cmp(&(&b.run, &b.kind)));
    assert_eq!(got, expected);
    assert_eq!(report.adjustments_not_listed, 0);
    for (field, value) in [
        ("adjustments", json!(report.adjustments)),
        ("adjusted_runs", json!(report.adjusted_runs)),
        ("needs_operator_action", json!([])),
    ] {
        assert_eq!(rebase.detail[field], value, "{field}");
    }
}
const PINNED: i64 = 1_000_000_000;
const REBASED: i64 = 10_000;
fn rebase(e: &Engine<MemoryStorage>) -> ClockRebaseReport {
    e.rebase_retention_clock(REBASED, REBASED - 1_000, "ops", "pinned clock", REBASED)
        .unwrap()
}
