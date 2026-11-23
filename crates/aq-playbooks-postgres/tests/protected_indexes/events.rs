use super::*;

/// Spec 0003: an expired event leaves with no tombstone and no
/// `disposal_tombstones` row. After a restart a repeat of its identity
/// carrying its producer time is stale; a retired epoch refuses it; an
/// explicit new epoch has a new identity.
#[test]
fn an_expired_event_identity_is_judged_by_time_after_restart_and_a_new_epoch_has_a_new_identity() {
    let n = namespace("event-retention");
    let (artifact, authority, event) = event_fixture(&n);
    let mut engine = Engine::new(store(), &n, "first").unwrap();
    engine.install(artifact.clone()).unwrap();
    engine
        .activate("retained", &artifact.digest, authority)
        .unwrap();
    engine
        .set_retention_policy(retention::RetentionPolicy {
            event_replay_ms: 1,
            terminal_run_ms: 1,
            audit_ms: 1,
            max_tombstones: 1,
            ..retention::RetentionPolicy::default()
        })
        .unwrap();
    for epoch in 0..4 {
        let now = epoch as i64 * 100;
        assert!(
            !engine
                .admit_event_in_epoch("fixture.producer", event.clone(), epoch, now)
                .unwrap()
                .duplicate
        );
        assert_eq!(engine.collect_garbage(now + 10, 100).unwrap().events, 1);
        assert_eq!(event_tombstone_count(&n), 0);
        assert!(
            engine
                .snapshot()
                .unwrap()
                .retention
                .event_tombstones
                .is_empty()
        );
        engine.release().unwrap();
        engine = Engine::new(store(), &n, format!("restarted-{epoch}")).unwrap();
        assert!(matches!(
            engine.admit_event_at(
                "fixture.producer",
                event.clone(),
                epoch,
                Some(now),
                now + 11
            ),
            Err(Error::Stale { .. })
        ));
        engine
            .advance_producer_epoch(
                "fixture.producer",
                epoch + 1,
                "operator",
                "new authenticated replay identity",
                now + 12,
            )
            .unwrap();
        assert_eq!(event_tombstone_count(&n), 0);
        assert!(
            engine
                .admit_event_in_epoch("fixture.producer", event.clone(), epoch, now + 13)
                .is_err()
        );
    }
}

fn event_fixture(n: &str) -> (Artifact, Authority, EventEnvelope) {
    let mut content = artifact(n).content;
    let contract = EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: "fixture.changed".into(),
        version: "1".into(),
        payload: Schema::Null,
        trusted_producers: BTreeSet::from(["fixture.producer".into()]),
        scopes: BTreeSet::from(["project".into()]),
        correlation_fields: BTreeSet::new(),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 1024,
        retention_ms: 1,
    };
    content
        .registry
        .events
        .insert(contract.name.clone(), contract.clone());
    let artifact = Artifact::seal(content).unwrap();
    let authority = Authority {
        actor: "fixture".into(),
        scope: "project".into(),
        capabilities: BTreeSet::new(),
    };
    let event = EventEnvelope {
        producer: "fixture.producer".into(),
        event_id: "stable-id".into(),
        contract: ContractRef {
            name: contract.name.clone(),
            fingerprint: contract.fingerprint().unwrap(),
        },
        scope: "project".into(),
        correlation: BTreeMap::new(),
        subject: None,
        revision: None,
        causation: None,
        occurred_at_ms: None,
        observed_at_ms: None,
        payload: Value::Null,
    };
    (artifact, authority, event)
}

fn event_tombstone_count(namespace: &str) -> i64 {
    client().query_one("SELECT count(*) FROM aq_playbooks.disposal_tombstones WHERE namespace=$1 AND kind='event'",&[&namespace]).unwrap().get::<_,i64>(0)
}
