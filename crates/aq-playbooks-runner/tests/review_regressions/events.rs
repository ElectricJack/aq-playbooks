use super::*;

#[test]
fn t6_partial_fanout_restarts_with_frozen_destinations_and_no_duplicate_runs() {
    let e = engine();
    let old = event_artifact("subscribed");
    install(&e, &old, "first");
    install(&e, &old, "second");
    let accepted = e.admit_event("host", envelope("event"), 0).unwrap();
    assert_eq!(accepted.destinations, 2);
    assert_eq!(e.fanout(&accepted.key, 1, 1).unwrap(), 1);
    let mut new = old.clone();
    new.content.playbook.purpose = "new activation".into();
    reseal(&mut new);
    install(&e, &new, "second");
    let store = e.store.clone();
    e.release().unwrap();
    let e = Engine::new(store, "review", "owner-two").unwrap();
    assert_eq!(e.fanout(&accepted.key, 1, 2).unwrap(), 1);
    assert_eq!(e.fanout(&accepted.key, 10, 3).unwrap(), 0);
    assert!(
        e.admit_event("host", envelope("event"), 4)
            .unwrap()
            .duplicate
    );
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs.len(), 2);
    assert!(s.runs.values().all(|r| r.artifact == old.digest));
}

/// T17, rewritten by Task 8b (no namespace limits): an event past the
/// deprecated `max_events` is admitted; a refused event (forged producer)
/// leaves the record byte for byte; the first event's artifacts and
/// destination remain and fan out.
#[test]
fn t17_refused_event_retains_prior_event_artifacts_and_destinations() {
    let e = Engine::with_config(
        MemoryStorage::retaining_settled(),
        "review",
        "owner-one",
        EngineConfig {
            max_events: 1,
            ..EngineConfig::default()
        },
    )
    .unwrap();
    let a = event_artifact("retained");
    install(&e, &a, "retained");
    let first = e.admit_event("host", envelope("first"), 0).unwrap();
    let before = canonical_bytes(&e.snapshot().unwrap()).unwrap();
    assert!(e.admit_event("intruder", envelope("forged"), 1).is_err());
    assert_eq!(canonical_bytes(&e.snapshot().unwrap()).unwrap(), before);
    assert!(
        !e.admit_event("host", envelope("second"), 1)
            .unwrap()
            .duplicate
    );
    let retained = e.snapshot().unwrap();
    assert!(retained.artifacts.contains_key(&a.digest));
    assert_eq!(e.fanout(&first.key, 1, 2).unwrap(), 1);
}

#[test]
fn ingress_rejects_missing_declared_subject_revision_and_correlation() {
    let e = engine();
    let a = event_artifact("typed-envelope");
    install(&e, &a, "typed-envelope");
    let mut event = envelope("missing-subject");
    event.subject = None;
    assert!(e.admit_event("host", event, 0).is_err());
    let mut event = envelope("missing-correlation");
    event.correlation.clear();
    assert!(e.admit_event("host", event, 0).is_err());
    let mut event = envelope("missing-revision");
    event.revision = None;
    assert!(e.admit_event("host", event, 0).is_err());
}

#[test]
fn retained_event_before_wait_is_driven_after_restart_without_manual_wakeup() {
    let e = engine();
    let c = event_contract();
    let mut a = artifact(
        "indexed-wait",
        "wait",
        BTreeMap::from([
            (
                "wait".into(),
                Step {
                    op: Operation::Wait {
                        event: Some(ContractRef {
                            name: c.name.clone(),
                            fingerprint: c.fingerprint().unwrap(),
                        }),
                        correlation: BTreeMap::from([("invocation".into(), Expr::literal("one"))]),
                        minimum_revision: Some(Expr::literal(1)),
                        deadline_ms: 100,
                        bind: "observation".into(),
                    },
                    edges: edges(&[("received", "ok"), ("timeout", "error")]),
                    on_error: Some("error".into()),
                },
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
    );
    a.content.registry.events.insert(c.name.clone(), c);
    reseal(&mut a);
    install(&e, &a, "indexed-wait");
    e.admit_event("host", envelope("before-wait"), 10).unwrap();
    let id = e
        .admit("indexed-wait", "main", Value::Null, authority(), 20)
        .unwrap();
    let store = e.store.clone();
    e.release().unwrap();
    let e = Engine::new(store, "review", "event-restart").unwrap();
    e.drive(&mut FakeHost::default(), 20, 20).unwrap();
    assert_eq!(settled(&e, &id).outcome.as_deref(), Some("ok"));
    assert!(
        e.query(&IndexQuery::Ready {
            scope_after: None,
            limit: 10
        })
        .unwrap()
        .items
        .is_empty()
    );
}
