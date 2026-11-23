use super::*;

#[test]
fn max_tombstones_and_terminal_run_ms_are_accepted_and_ignored() {
    let e = engine();
    e.set_retention_policy(RetentionPolicy {
        max_tombstones: 0,
        terminal_run_ms: 0,
        ..RetentionPolicy::default()
    })
    .unwrap();
    assert_eq!(e.snapshot().unwrap().retention.policy.max_tombstones, 0);
}
#[test]
fn retention_windows_must_be_positive() {
    let e = engine();
    for policy in [
        RetentionPolicy {
            event_replay_ms: 0,
            ..RetentionPolicy::default()
        },
        RetentionPolicy {
            admission_replay_ms: 0,
            ..RetentionPolicy::default()
        },
        RetentionPolicy {
            audit_ms: 0,
            ..RetentionPolicy::default()
        },
    ] {
        assert!(
            matches!(e.set_retention_policy(policy), Err(Error::Refused(m)) if m == "retention windows must be positive")
        );
    }
}

/// A `max_tombstones` of 1 stops neither a GC that forgets three events nor
/// a policy change (the field is ignored), and GC writes no tombstone.
#[test]
fn max_tombstones_stops_neither_gc_nor_policy_changes_and_gc_writes_no_tombstone() {
    let e = engine();
    install(&e, wait_artifact(), "waiter");
    let policy = RetentionPolicy {
        event_replay_ms: 1,
        max_tombstones: 1,
        ..RetentionPolicy::default()
    };
    e.set_retention_policy(policy.clone()).unwrap();
    for i in 0..3 {
        e.admit_event("host", event(&format!("old-{i}"), 1), 0)
            .unwrap();
    }
    assert_eq!(e.collect_garbage(2_000, 10).unwrap().events, 3);
    assert!(e.snapshot().unwrap().retention.event_tombstones.is_empty());
    e.set_retention_policy(policy).unwrap();
}
/// A namespace written before this change holds finished runs until the
/// drain moves them to history (Task 9; before it, GC moved them out by
/// `audit_ms`). They never make new work wait, whatever a host's deprecated
/// `max_runs` says (Task 8b, no limits): new work is admitted, a live run
/// finishes and leaves, and the drain moves the finished runs out (GC,
/// which would remove them without history, leaves them to it).
#[test]
fn a_legacy_namespace_holding_finished_runs_admits_new_work_and_drains() {
    let legacy = 51;
    let e = Engine::with_config(
        MemoryStorage::retaining_settled(),
        "test",
        "i-1",
        EngineConfig {
            max_runs: legacy + 1,
            ..EngineConfig::default()
        },
    )
    .unwrap();
    install(&e, done_artifact(), "a");
    e.set_retention_policy(RetentionPolicy {
        terminal_run_ms: u64::MAX,
        audit_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let live = e.admit("a", "main", Value::Null, authority(), 1).unwrap();
    let mut s = e.snapshot().unwrap();
    for i in 0..legacy {
        let id = format!("old-{i:03}");
        s.runs.insert(id.clone(), completed_run(&id));
        s.sequence += 1;
        s.receipts.push(audit(s.sequence, &id));
    }
    s.indexes.initialized = false;
    overwrite(&e.store, "i-1", &s);
    // The commit that rebuilds the indexes re-arms the drain and takes its
    // first step: 50 of the 51 legacy runs leave. `collect_garbage`, an
    // ordinary commit, takes the next: the last one goes to history.
    let admitted = e.admit("a", "main", Value::Null, authority(), 2).unwrap();
    assert_eq!(e.collect_garbage(1_000, 1_000).unwrap().runs, 1);
    assert!(e.advance(&live, 3).unwrap());
    assert!(e.advance(&admitted, 3).unwrap());
    e.admit("a", "main", Value::Null, authority(), 4).unwrap();
    assert_eq!(e.snapshot().unwrap().runs.len(), 1);
    let history = inspect_all(&e, InspectionKind::Runs, None);
    assert_eq!(
        history
            .iter()
            .filter(|r| r["id"].as_str().unwrap().starts_with("old-"))
            .count(),
        legacy
    );
}
/// A pre-D10 namespace written before the retention clock opens (the
/// `root_intents` fill grows the record) and takes its first commits under
/// the smallest deprecated `max_state_bytes` a host could pass: nothing
/// judges the record's size (Task 8b, no limits).
#[test]
fn a_pre_d10_namespace_opens_and_takes_its_first_commits_whatever_its_configured_limits() {
    let e = Engine::new(MemoryStorage::retaining_settled(), "test", "incarnation-1").unwrap();
    install_on(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    install_on(&e, wait_artifact(), "waiter");
    let mut host = completed_host();
    let (old, _) = run_with_completed_operation(&e, &mut host);
    e.admit_event("host", event("seen", 1), 0).unwrap();
    let mut legacy = vendored_shape(&e.snapshot().unwrap());
    legacy.retention.clock_ms = None;
    e.release().unwrap();
    let store = MemoryStorage::retaining_settled();
    store.acquire("test", "w").unwrap();
    store.compare_and_swap("test", "w", 0, &legacy).unwrap();
    store.release("test", "w").unwrap();
    let e = Engine::with_config(store, "test", "incarnation-2", tiny_limits()).unwrap();
    assert!(e.snapshot().unwrap().indexes.root_intents_complete);
    // The first host-time commit starts the clock: a redelivery inside the
    // event's window, a duplicate that writes nothing else.
    assert!(
        e.admit_event("host", event("seen", 1), 1_000_000)
            .unwrap()
            .duplicate
    );
    assert_eq!(e.snapshot().unwrap().retention.clock_ms, Some(1_000_000));
    assert!(e.advance(&old, 1_000_001).unwrap());
}

// --- Task 8b (owner override): no namespace limits

/// Every deprecated namespace limit at its smallest value: a host may still
/// pass them, and nothing reads them.
fn tiny_limits() -> EngineConfig {
    EngineConfig {
        max_runs: 1,
        max_events: 1,
        max_queued_destinations: 1,
        max_state_bytes: 1,
        max_receipts: 1,
    }
}
fn unlimited_engine() -> Engine<MemoryStorage> {
    Engine::with_config(
        MemoryStorage::retaining_settled(),
        "test",
        "incarnation-1",
        tiny_limits(),
    )
    .unwrap()
}
/// A chain of `n` decisions, then `ok`: each decision writes a `branch`
/// receipt, so a run holds `n + 1` receipts just before it finishes.
fn chain_artifact(n: usize) -> Artifact {
    let mut steps = BTreeMap::new();
    for i in 0..n {
        let next = if i + 1 == n {
            "ok".to_string()
        } else {
            format!("d{}", i + 1)
        };
        steps.insert(
            format!("d{i}"),
            Step {
                op: Operation::Decide {
                    predicate: Expr::literal(true),
                },
                edges: edges(&[("true", next.as_str()), ("false", "ok"), ("unknown", "ok")]),
                on_error: None,
            },
        );
    }
    steps.insert("ok".into(), ret("ok"));
    artifact("chain", "d0", steps, FixtureContracts::default())
}
/// Task 8 re-review round 2's deadlock: two parents waiting on a child,
/// with a host's `max_runs` at 2. Nothing counts runs, so both start their
/// children and finish.
#[test]
fn calling_parents_complete_whatever_max_runs_says() {
    let e = Engine::with_config(
        MemoryStorage::retaining_settled(),
        "test",
        "incarnation-1",
        EngineConfig {
            max_runs: 2,
            ..EngineConfig::default()
        },
    )
    .unwrap();
    let child = done_artifact();
    e.install(child.clone()).unwrap();
    install(&e, parent_artifact(&child, CallMode::Wait), "parent");
    let parents: Vec<String> = (0..2)
        .map(|t| {
            e.admit("parent", "main", Value::Null, authority(), t)
                .unwrap()
        })
        .collect();
    e.drive(&mut FakeHost::default(), 10, 100).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    for parent in &parents {
        assert_eq!(runs_page(&e, Some(parent))[0]["outcome"], "ok", "{parent}");
    }
}
/// 1,000 root runs admitted and left in progress at once (direct and keyed)
/// are all accepted, then all finish.
#[test]
fn a_thousand_root_runs_in_progress_are_all_accepted_then_all_finish() {
    let e = unlimited_engine();
    install(&e, chain_artifact(2), "chain");
    install(&e, done_artifact(), "a");
    let mut runs = vec![];
    for t in 0..500 {
        runs.push(
            e.admit("chain", "main", Value::Null, authority(), t)
                .unwrap(),
        );
        let d = e.admit_once(keyed(&e, &format!("k{t:03}")), t).unwrap();
        assert!(!d.duplicate);
        runs.push(d.receipt.run_id);
    }
    assert_eq!(e.snapshot().unwrap().runs.len(), 1_000);
    while e.drive(&mut FakeHost::default(), 1_000, 1_000).unwrap() > 0 {}
    assert!(e.snapshot().unwrap().runs.is_empty());
    for id in &runs {
        assert_eq!(runs_page(&e, Some(id))[0]["outcome"], "ok", "{id}");
    }
}
/// No commit is refused for namespace capacity: with every deprecated limit
/// at its smallest, a direct and a keyed admission, an event with its
/// fan-out, a child start, a run holding 1,500 receipts, a waiting run's
/// wait, an install and an activation all land.
#[test]
fn nothing_is_refused_for_namespace_capacity() {
    let e = unlimited_engine();
    install(&e, done_artifact(), "a");
    install(&e, trigger_artifact(), "trigger");
    install(&e, wait_artifact(), "waiter");
    install(&e, chain_artifact(1_600), "chain");
    let child = done_artifact();
    install(&e, parent_artifact(&child, CallMode::Wait), "parent");
    let direct = e.admit("a", "main", Value::Null, authority(), 0).unwrap();
    let keyed_run = e.admit_once(keyed(&e, "k"), 0).unwrap().receipt.run_id;
    let parent = e
        .admit("parent", "main", Value::Null, authority(), 0)
        .unwrap();
    let waiter = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    let long = e
        .admit("chain", "main", Value::Null, authority(), 0)
        .unwrap();
    for i in 0..20 {
        let d = e
            .admit_event("host", event(&format!("e{i}"), 1), 1)
            .unwrap();
        assert_eq!(e.fanout(&d.key, 10, 1).unwrap(), 1);
    }
    let mut peak = 0;
    while e.drive(&mut FakeHost::default(), 2, 16).unwrap() > 0 {
        peak = peak.max(e.snapshot().unwrap().receipts.len());
    }
    assert!(peak > 1_500, "{peak}");
    for id in [&direct, &keyed_run, &parent, &long] {
        assert_eq!(runs_page(&e, Some(id))[0]["outcome"], "ok", "{id}");
    }
    assert_eq!(
        e.snapshot().unwrap().runs[&waiter].status,
        RunStatus::Waiting
    );
    let mut other = done_artifact().content;
    other.playbook.purpose = "another".into();
    let other = Artifact::seal(other).unwrap();
    let digest = other.digest.clone();
    e.install(other).unwrap();
    e.activate("other", &digest, authority()).unwrap();
}
/// A stored policy with the deprecated limit fields decodes and behaves as
/// one without them, and an engine given a host's limits behaves as one
/// given none: the same script leaves the same record and history.
#[test]
fn deprecated_limit_fields_decode_and_change_nothing() {
    let without: RetentionPolicy = serde_json::from_value(json!({
        "event_replay_ms": 1_000, "audit_ms": 604_800_000,
    }))
    .unwrap();
    let with: RetentionPolicy = serde_json::from_value(json!({
        "event_replay_ms": 1_000, "audit_ms": 604_800_000,
        "terminal_run_ms": 0, "max_tombstones": 0,
    }))
    .unwrap();
    let config: EngineConfig = serde_json::from_value(json!({})).unwrap();
    assert_eq!(
        serde_json::to_value(&config).unwrap(),
        serde_json::to_value(EngineConfig::default()).unwrap()
    );
    let script = |config: EngineConfig, policy: RetentionPolicy| {
        let e = Engine::with_config(
            MemoryStorage::retaining_settled(),
            "test",
            "incarnation-1",
            config,
        )
        .unwrap();
        install(&e, done_artifact(), "a");
        install(&e, trigger_artifact(), "trigger");
        e.set_retention_policy(policy).unwrap();
        for t in 0..5 {
            e.admit("a", "main", Value::Null, authority(), t).unwrap();
            let d = e
                .admit_event("host", event(&format!("e{t}"), 1), t)
                .unwrap();
            e.fanout(&d.key, 10, t).unwrap();
        }
        e.drive(&mut FakeHost::default(), 3_000, 100).unwrap();
        let mut s = serde_json::to_value(e.snapshot().unwrap()).unwrap();
        for field in ["terminal_run_ms", "max_tombstones"] {
            s["retention"]["policy"][field] = Value::Null;
        }
        (s, runs_page(&e, None))
    };
    let reference = script(EngineConfig::default(), without);
    assert_eq!(script(tiny_limits(), with), reference);
}
