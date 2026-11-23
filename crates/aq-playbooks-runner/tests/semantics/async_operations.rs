use super::*;

#[test]
fn owned_operation_join_uses_exact_pinned_completion_and_retained_admission() {
    let e = engine();
    let (a, c) = async_artifact();
    install(&e, a, "async");
    let id = e
        .admit("async", "main", Value::Null, authority(), 0)
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
    e.drive(&mut host, 0, 10).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Waiting);
    let mut other = c.clone();
    other.version = "2".into();
    let alternate = artifact(
        "alternate",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
        FixtureContracts {
            registry: Registry {
                commands: BTreeMap::new(),
                events: BTreeMap::from([(other.name.clone(), other.clone())]),
            },
            ..Default::default()
        },
    );
    e.install(alternate).unwrap();
    e.admit_event("host", async_event(&other, "wrong-contract"), 10)
        .unwrap();
    e.drive(&mut host, 10, 10).unwrap();
    assert_eq!(e.snapshot().unwrap().runs[&id].status, RunStatus::Waiting);
    e.admit_event("host", async_event(&c, "right-contract"), 20)
        .unwrap();
    e.drive(&mut host, 20, 20).unwrap();
    assert_eq!(settled(&e, &id).outcome.as_deref(), Some("ok"));
}

#[test]
fn timeout_fallback_waits_for_truthful_owned_operation_cancellation() {
    let e = engine();
    let (a, _) = async_artifact();
    install(&e, a, "async");
    let id = e
        .admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![DispatchResult::Accepted {
                handle: "server-operation".into(),
            }],
        )]),
        cancellation_responses: BTreeMap::from([(
            "test.command".into(),
            vec![CancellationResult::Accepted],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 10).unwrap();
    e.drive(&mut host, 101, 20).unwrap();
    let state = e.snapshot().unwrap();
    assert_eq!(state.runs[&id].status, RunStatus::Finishing);
    let intent = state.intents.values().next().unwrap();
    assert!(intent.cancel_requested);
    assert_eq!(intent.status, IntentStatus::Unknown);
    assert_eq!(host.cancelled.len(), 1);
    // An authoritative completion after the workflow's deadline settles the
    // original operation, but cannot replace the recorded timeout/fallback.
    let c = async_artifact().1;
    e.admit_event("host", async_event(&c, "late"), 110).unwrap();
    e.drive(&mut host, 110, 20).unwrap();
    assert_eq!(settled(&e, &id).outcome.as_deref(), Some("error"));
}

#[test]
fn operation_join_deadline_uses_ingress_time_even_when_processing_is_late() {
    for (accepted_at, expected) in [(99, "ok"), (101, "error")] {
        let e = engine();
        let (a, c) = async_artifact();
        install(&e, a, "async");
        let id = e
            .admit("async", "main", Value::Null, authority(), 0)
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
        e.drive(&mut host, 0, 10).unwrap();
        e.admit_event("host", async_event(&c, "completion"), accepted_at)
            .unwrap();
        e.drive(&mut host, 150, 30).unwrap();
        assert_eq!(settled(&e, &id).outcome.as_deref(), Some(expected));
        let branch = e.store.settled_cohorts("test")[0]
            .receipts
            .iter()
            .find(|r| r.step == "join" && r.kind == "branch")
            .cloned()
            .unwrap();
        assert_eq!(
            branch.detail["outcome"],
            if expected == "ok" {
                "completed"
            } else {
                "timeout"
            }
        );
    }
}

struct CrashOnCancel {
    host: FakeHost,
    after_send: bool,
    keys: Vec<String>,
}

impl Host for CrashOnCancel {
    fn authorize(&self, a: &Authority, c: &CommandContract) -> bool {
        self.host.authorize(a, c)
    }
    fn dispatch(&mut self, i: &Intent) -> DispatchResult {
        self.host.dispatch(i)
    }
    fn reconcile(&mut self, i: &Intent) -> DispatchResult {
        self.host.reconcile(i)
    }
    fn cancel(&mut self, i: &Intent) -> CancellationResult {
        self.keys.push(cancellation_key(i).unwrap());
        if self.after_send {
            self.host.cancel(i);
        }
        panic!("simulated runtime exit before cancellation receipt")
    }
}

#[test]
fn cancellation_dispatch_crashes_reconcile_owner_before_same_key_delivery_retry() {
    for after_send in [false, true] {
        let e = engine();
        let (a, _) = async_artifact();
        install(&e, a, "async");
        let id = e
            .admit("async", "main", Value::Null, authority(), 0)
            .unwrap();
        let mut host = CrashOnCancel {
            host: FakeHost {
                responses: BTreeMap::from([(
                    "test.command".into(),
                    vec![DispatchResult::Accepted {
                        handle: "server-operation".into(),
                    }],
                )]),
                ..Default::default()
            },
            after_send,
            keys: vec![],
        };
        e.drive(&mut host, 0, 10).unwrap();
        e.cancel(&id, 1).unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || e.dispatch_next(&mut host, 1)
            ))
            .is_err()
        );
        let state = e.snapshot().unwrap();
        let intent = state.intents.values().next().unwrap().clone();
        assert!(intent.cancel_dispatched);
        assert_eq!(state.runs[&id].status, RunStatus::Cancelling);
        assert_eq!(host.host.dispatched.len(), 1);
        host.host.responses.insert(
            "reconcile:test.command".into(),
            vec![DispatchResult::Accepted {
                handle: "server-operation".into(),
            }],
        );
        e.recover(&mut host.host, 2).unwrap();
        assert_eq!(host.host.reconciliations.len(), 1);
        assert!(!e.snapshot().unwrap().intents[&intent.id].cancel_dispatched);
        host.host
            .cancellation_responses
            .insert("test.command".into(), vec![CancellationResult::Confirmed]);
        e.dispatch_next(&mut host.host, 2).unwrap();
        e.drive(&mut host.host, 2, 10).unwrap();
        assert_eq!(settled(&e, &id).status, RunStatus::Cancelled);
        assert_eq!(host.host.dispatched.len(), 1);
        let cohort = &e.store.settled_cohorts("test")[0];
        let cancelled = cohort.intents.iter().find(|i| i.id == intent.id).unwrap();
        assert_eq!(host.keys, vec![cancellation_key(cancelled).unwrap()]);
    }
}

#[test]
fn gc_preserves_possible_async_completion_while_lost_admission_handle_is_unknown() {
    let e = engine();
    let (a, event_contract) = async_artifact();
    install(&e, a, "async");
    e.set_retention_policy(RetentionPolicy {
        event_replay_ms: 1,
        terminal_run_ms: 1,
        audit_ms: 1,
        max_tombstones: 10,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let id = e
        .admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    let event = e
        .admit_event(
            "host",
            async_event(&event_contract, "lost-admission-completion"),
            1,
        )
        .unwrap();
    // Past the event's own window (1,001) but inside the hold: the
    // operation's command deadline (100) plus that window (1,000). After
    // the hold lapses (1,100) the event is an ordinary entry (Task 10,
    // ruling C).
    for _ in 0..10 {
        if !e.collect_garbage(1050, 1).unwrap().more {
            break;
        }
    }
    assert!(e.snapshot().unwrap().events.contains_key(&event.key));
    host.responses.insert(
        "reconcile:test.command".into(),
        vec![DispatchResult::Accepted {
            handle: "server-operation".into(),
        }],
    );
    e.recover(&mut host, 1051).unwrap();
    e.drive(&mut host, 1051, 20).unwrap();
    assert_eq!(settled(&e, &id).outcome.as_deref(), Some("ok"));
}

#[test]
fn confirmed_async_admission_recovery_resumes_accepted_branch_before_completion() {
    let first = engine();
    let (artifact, completion) = async_artifact();
    install(&first, artifact, "async");
    let run = first
        .admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![DispatchResult::Unknown {
                reason: "lost reply after external admission".into(),
            }],
        )]),
        ..Default::default()
    };
    first.drive(&mut host, 0, 10).unwrap();
    let before = first.snapshot().unwrap();
    assert_eq!(before.runs[&run].status, RunStatus::Blocked);
    let intent = before.intents.values().next().unwrap().clone();
    assert!(
        matches!(&before.runs[&run].pending,Some(Pending::Effect {intent:pending}) if pending==&intent.id)
    );
    first.release().unwrap();
    let resumed = Engine::new(first.store.clone(), "test", "incarnation-2").unwrap();
    host.responses.insert(
        "reconcile:test.command".into(),
        vec![DispatchResult::Accepted {
            handle: "server-operation".into(),
        }],
    );
    assert_eq!(resumed.recover(&mut host, 1).unwrap(), 1);
    let recovered = resumed.snapshot().unwrap();
    let admitted = &recovered.intents[&intent.id];
    assert_eq!(admitted.status, IntentStatus::Accepted);
    assert_eq!(admitted.authority, intent.authority);
    assert_eq!(admitted.handle.as_deref(), Some("server-operation"));
    // A known admission must resume accepted→notify/join, even before the
    // human/external completion exists. It must not remain silently blocked.
    let resumed_run = &recovered.runs[&run];
    assert_eq!(resumed_run.status, RunStatus::Waiting);
    assert!(resumed_run.reason.is_none() && admitted.reason.is_none());
    resumed.drive(&mut host, 1, 10).unwrap();
    let waiting = resumed.snapshot().unwrap();
    let joined = &waiting.runs[&run];
    assert_eq!(
        (
            &joined.status,
            joined.step.as_str(),
            joined.outcome.as_ref()
        ),
        (&RunStatus::Waiting, "join", None)
    );
    assert_eq!(host.dispatched.len(), 1);
    assert_eq!(host.reconciliations, vec![intent.id.clone()]);
    assert_eq!(waiting.runs.len(), 1);
    let recoveries = resumed.recover(&mut host, 2).unwrap();
    assert_eq!(recoveries, 0);
    let event = async_event(&completion, "confirmed-after-recovery");
    resumed.admit_event("host", event.clone(), 3).unwrap();
    resumed.drive(&mut host, 3, 20).unwrap();
    assert_eq!(settled(&resumed, &run).outcome.as_deref(), Some("ok"));
    assert!(resumed.admit_event("host", event, 4).unwrap().duplicate);
    assert_eq!(host.dispatched.len(), 1);
}

#[test]
fn accepted_recovery_does_not_uncancel_a_run_or_bypass_revoked_authority() {
    let e = engine();
    let (artifact, _) = async_artifact();
    install(&e, artifact, "async");
    let run = e
        .admit("async", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 0, 10).unwrap();
    host.responses.insert(
        "reconcile:test.command".into(),
        vec![DispatchResult::Accepted {
            handle: "server-operation".into(),
        }],
    );
    host.revoked.insert("execute".into());
    assert_eq!(e.recover(&mut host, 1).unwrap(), 0);
    assert!(host.reconciliations.is_empty());
    assert_eq!(e.snapshot().unwrap().runs[&run].status, RunStatus::Blocked);
    host.revoked.clear();
    e.cancel(&run, 2).unwrap();
    e.recover(&mut host, 2).unwrap();
    let cancelling = e.snapshot().unwrap();
    assert_eq!(cancelling.runs[&run].status, RunStatus::Cancelling);
    assert!(cancelling.runs[&run].outcome.is_none());
    assert!(cancelling.intents.values().next().unwrap().cancel_requested);
    host.cancellation_responses
        .insert("test.command".into(), vec![CancellationResult::Confirmed]);
    e.drive(&mut host, 3, 10).unwrap();
    assert_eq!(settled(&e, &run).status, RunStatus::Cancelled);
    assert_eq!(host.dispatched.len(), 1);
}
