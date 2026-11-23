use super::*;

#[test]
fn all_three_child_modes_are_atomic_and_independent_cancellation_survives() {
    for mode in [CallMode::Wait, CallMode::OwnedAsync, CallMode::Independent] {
        let e = engine();
        let child = artifact(
            "child",
            "ok",
            BTreeMap::from([("ok".into(), ret("ok"))]),
            FixtureContracts::default(),
        );
        e.install(child.clone()).unwrap();
        install(&e, parent_artifact(&child, mode.clone()), "parent");
        let id = e
            .admit("parent", "main", Value::Null, authority(), 0)
            .unwrap();
        e.advance(&id, 0).unwrap();
        let first = e.snapshot().unwrap();
        assert_eq!(first.runs.len(), 2);
        let child = first.runs.values().find(|r| r.id != id).unwrap().id.clone();
        e.advance(&id, 0).unwrap();
        assert_eq!(e.snapshot().unwrap().runs.len(), 2);
        if mode == CallMode::Independent {
            e.cancel(&id, 1).unwrap();
            e.advance(&id, 1).unwrap();
            assert_eq!(
                e.snapshot().unwrap().runs[&child].status,
                RunStatus::Admitted
            );
        }
        let mut host = FakeHost::default();
        e.drive(&mut host, 2, 20).unwrap();
        // Parent and child settled together and left as one cohort.
        assert!(settled(&e, &id).status.terminal());
        assert_eq!(settled(&e, &child).status, RunStatus::Completed);
        assert_eq!(e.store.settled_cohorts("test").len(), 1);
    }
}

#[test]
fn simulation_missing_mock_is_unknown_and_incomplete() {
    let mut simulation = Simulation::new(FakeHost::default()).unwrap();
    install(
        &simulation.engine,
        invoke_artifact(command(Recovery::Unsafe), None),
        "invoke",
    );
    simulation
        .engine
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let report = simulation.run(20).unwrap();
    assert!(report.incomplete);
    assert_eq!(report.unknown_operations.len(), 1);
    assert_eq!(simulation.host.dispatched.len(), 1);
}

#[test]
fn exhausted_step_budget_finishes_once_without_reentering_error_graph() {
    let e = engine();
    let mut a = invoke_artifact(command(Recovery::KeyedIdempotent), None).content;
    a.playbook.limits.max_step_visits = 1;
    install(&e, Artifact::seal(a).unwrap(), "invoke");
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![DispatchResult::Completed {
                outcome: "ok".into(),
                output: Value::Null,
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 20).unwrap();
    assert_eq!(settled(&e, &id).status, RunStatus::Failed);
    let before = inspected(&e, InspectionKind::Receipts, Some(&id));
    assert_eq!(
        before
            .iter()
            .filter(|r| r["kind"] == "engine_failure")
            .count(),
        1
    );
    // The failed run has left; it is never re-entered.
    for _ in 0..10 {
        assert!(matches!(e.advance(&id, 1), Err(Error::NotFound(_))));
    }
    assert_eq!(before, inspected(&e, InspectionKind::Receipts, Some(&id)));
}

#[test]
fn history_budget_stops_without_disposing_existing_records() {
    let e = engine();
    let mut a = artifact(
        "small",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
        FixtureContracts::default(),
    )
    .content;
    a.playbook.limits.max_payload_bytes = 32;
    a.playbook.limits.max_history_bytes = 128;
    let digest = install(&e, Artifact::seal(a).unwrap(), "small");
    let id = e
        .admit("small", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    let s = e.snapshot().unwrap();
    let run = settled(&e, &id);
    assert_eq!(run.status, RunStatus::Failed);
    assert!(s.artifacts.contains_key(&digest));
    assert!(run.reason.as_ref().unwrap().contains("history"));
}

#[test]
fn missing_dependency_destination_is_durably_blocked_then_resumes_exact_bytes() {
    let e = engine();
    let child = artifact(
        "missing-child",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
        FixtureContracts::default(),
    );
    let mut parent = parent_artifact(&child, CallMode::Wait).content;
    let c = event_contract();
    parent.registry.events.insert(c.name.clone(), c.clone());
    parent.playbook.triggers.push(Trigger {
        id: "trigger".into(),
        event: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        entry: "main".into(),
        input: Expr::literal(Value::Null),
    });
    install(&e, Artifact::seal(parent).unwrap(), "parent");
    let disposition = e
        .admit_event("host", event("missing-dependency", 2), 0)
        .unwrap();
    assert_eq!(disposition.destinations, 1);
    assert_eq!(e.fanout(&disposition.key, 1, 0).unwrap(), 0);
    let blocked = e.snapshot().unwrap();
    assert!(blocked.runs.is_empty());
    assert!(
        blocked
            .receipts
            .iter()
            .any(|r| r.kind == "destination_blocked")
    );
    e.install(child.clone()).unwrap();
    assert_eq!(e.fanout(&disposition.key, 1, 1).unwrap(), 1);
    let mut host = FakeHost::default();
    e.drive(&mut host, 1, 30).unwrap();
    // The event's run and its child completed and left as history.
    assert!(e.snapshot().unwrap().runs.is_empty());
    let runs = inspected(&e, InspectionKind::Runs, None);
    assert_eq!(runs.len(), 2);
    assert!(runs.iter().all(|r| r["status"] == "completed"));
}

#[test]
fn iteration_failure_policy_skips_or_stops_without_resetting_captured_membership() {
    for (policy, expected, calls) in [
        (IterationFailure::Continue, "ok", 2),
        (IterationFailure::Stop, "error", 1),
    ] {
        let e = engine();
        let a = iteration_artifact(policy);
        install(&e, a, "iteration");
        let id = e
            .admit("iteration", "main", Value::Null, authority(), 0)
            .unwrap();
        let mut host = FakeHost {
            responses: BTreeMap::from([(
                "test.command".into(),
                vec![
                    DispatchResult::Rejected {
                        reason: "denied".into(),
                    },
                    DispatchResult::Rejected {
                        reason: "denied".into(),
                    },
                ],
            )]),
            ..Default::default()
        };
        e.drive(&mut host, 0, 50).unwrap();
        let run = settled(&e, &id);
        assert_eq!(
            (run.outcome.as_deref(), host.dispatched.len()),
            (Some(expected), calls)
        );
        assert!(run.loops.is_empty());
        assert!(!run.variables.contains_key("item"));
    }
}

#[test]
fn retry_allowances_are_scoped_to_full_loop_invocation_paths() {
    let e = engine();
    let mut content = invoke_artifact(
        command(Recovery::KeyedIdempotent),
        Some(RetryPolicy {
            max_attempts: 2,
            backoff_ms: 10,
            outcomes: set(&["retry"]),
        }),
    )
    .content;
    content.playbook.entries.get_mut("main").unwrap().start = "loop".into();
    content
        .playbook
        .steps
        .get_mut("invoke")
        .unwrap()
        .edges
        .insert("ok".into(), "loop".into());
    content.playbook.steps.insert(
        "loop".into(),
        Step {
            op: Operation::ForEach {
                collection: Expr::literal(json!([null, null])),
                item: "item".into(),
                body: "invoke".into(),
                next: "ok".into(),
                max_items: 2,
                concurrency: 1,
                on_failure: IterationFailure::Stop,
            },
            edges: BTreeMap::new(),
            on_error: Some("error".into()),
        },
    );
    install(&e, Artifact::seal(content).unwrap(), "invoke");
    let id = e
        .admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap();
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            ["retry", "ok", "retry", "ok"]
                .into_iter()
                .map(|outcome| DispatchResult::Completed {
                    outcome: outcome.into(),
                    output: Value::Null,
                })
                .collect(),
        )]),
        ..Default::default()
    };
    for now in [0, 10] {
        e.drive(&mut host, now, 50).unwrap();
    }
    // Both items have reserved their iteration while the run is live; the
    // budget leaves with the cohort.
    assert_eq!(e.snapshot().unwrap().budgets[&id].iterations, 2);
    e.drive(&mut host, 20, 50).unwrap();
    let run = settled(&e, &id);
    assert_eq!(run.outcome.as_deref(), Some("ok"));
    assert_eq!(host.dispatched.len(), 4);
    assert_eq!(run.attempts.values().copied().collect::<Vec<_>>(), [2, 2]);
    assert!(!e.snapshot().unwrap().budgets.contains_key(&id));
}

#[test]
fn bounded_driver_uses_persisted_fair_scope_queue_and_noop_does_not_commit() {
    let store = MemoryStorage::retaining_settled();
    let e = Engine::new(store.clone(), "fair", "one").unwrap();
    for scope in ["alpha", "beta"] {
        let mut a = artifact(
            scope,
            "ok",
            BTreeMap::from([("ok".into(), ret("ok"))]),
            FixtureContracts::default(),
        )
        .content;
        a.playbook.scope = scope.into();
        a.source.header.scope = scope.into();
        let a = Artifact::seal(a).unwrap();
        let hash = a.digest.clone();
        let mut auth = authority();
        auth.scope = scope.into();
        e.install(a).unwrap();
        e.activate(scope, &hash, auth.clone()).unwrap();
        for _ in 0..3 {
            e.admit(scope, "main", Value::Null, auth.clone(), 0)
                .unwrap();
        }
    }
    let mut host = FakeHost::default();
    assert_eq!(e.drive(&mut host, 5, 2).unwrap(), 2);
    let metrics = e.metrics.snapshot();
    assert_eq!(metrics.queue_latency.count, 2);
    assert_eq!(metrics.queue_latency.total_us, 10_000);
    assert_eq!(metrics.queue_latency.max_us, 5_000);
    let s = e.snapshot().unwrap();
    // The two finished runs (one per scope) have left as history.
    let terminal_scopes = store
        .settled_cohorts("fair")
        .iter()
        .flat_map(|c| &c.runs)
        .filter(|r| r.status.terminal())
        .map(|r| r.authority.scope.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(terminal_scopes, set(&["alpha", "beta"]));
    assert_eq!(s.runs.len(), 4);
    assert!(s.indexes.ready_tokens.len() == 4);
    e.release().unwrap();
    let resumed = Engine::new(store.clone(), "fair", "two").unwrap();
    resumed.drive(&mut host, 5, 10).unwrap();
    let revision = store.load("fair").unwrap().revision;
    assert_eq!(resumed.drive(&mut host, 5, 10).unwrap(), 0);
    assert_eq!(store.load("fair").unwrap().revision, revision);
}

fn iteration_artifact(policy: IterationFailure) -> Artifact {
    let c = command(Recovery::KeyedIdempotent);
    artifact(
        "iteration",
        "loop",
        BTreeMap::from([
            (
                "loop".into(),
                Step {
                    op: Operation::ForEach {
                        collection: Expr::literal(json!([null, null])),
                        item: "item".into(),
                        body: "invoke".into(),
                        next: "ok".into(),
                        max_items: 2,
                        concurrency: 1,
                        on_failure: policy,
                    },
                    edges: BTreeMap::new(),
                    on_error: Some("error".into()),
                },
            ),
            (
                "invoke".into(),
                Step {
                    op: Operation::Invoke {
                        command: ContractRef {
                            name: c.name.clone(),
                            fingerprint: c.fingerprint().unwrap(),
                        },
                        input: Expr::literal(Value::Null),
                        bind: "result".into(),
                        retry: None,
                    },
                    edges: edges(&[("ok", "loop"), ("retry", "loop")]),
                    on_error: Some("error".into()),
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
        FixtureContracts {
            registry: Registry {
                commands: BTreeMap::from([(c.name.clone(), c)]),
                events: BTreeMap::new(),
            },
            ..Default::default()
        },
    )
}
