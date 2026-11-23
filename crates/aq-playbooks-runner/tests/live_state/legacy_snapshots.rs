use super::*;

#[test]
fn legacy_retention_state_and_policy_decode_with_defaults() {
    let policy: RetentionPolicy = serde_json::from_value(
        json!({"event_replay_ms":1,"terminal_run_ms":1,"audit_ms":1,"max_tombstones":1}),
    )
    .unwrap();
    assert_eq!(policy.admission_replay_ms, 604_800_000);
    assert_eq!(RetentionPolicy::default().admission_replay_ms, 604_800_000);
    let mut legacy = serde_json::to_value(State::default()).unwrap();
    for f in ["clock_ms", "expiry", "admission_expires_at_ms"] {
        legacy["retention"].as_object_mut().unwrap().remove(f);
    }
    let state: State = serde_json::from_value(legacy).unwrap();
    assert!(state.retention.clock_ms.is_none() && state.retention.expiry.is_empty());
}
/// `tests/fixtures/legacy_state_5046282.json` was written by the engine at
/// 5046282 (before the retention clock): a namespace after a GC, holding a
/// keyed admission receipt, a live run, a live event, event tombstones and
/// disposals. It is the format 2 snapshot (the State's JSON) and the format 3
/// representation (header plus records) of the same State.
#[test]
fn legacy_fixture_from_5046282_decodes_on_format_2_and_format_3() {
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/legacy_state_5046282.json")).unwrap();
    let snapshot = &fixture["format2_snapshot"];
    assert_original_retention_fields(snapshot);
    let f2: State = serde_json::from_value(snapshot.clone()).unwrap();
    let f3 = decode_records(&fixture);
    for state in [&f2, &f3] {
        assert_legacy_windows(state);
    }
    assert_eq!(
        serde_json::to_value(&f2).unwrap(),
        serde_json::to_value(&f3).unwrap()
    );
    // The decoded State commits on both in-memory backends, and the legacy
    // receipt leaves only once the clock passes its window.
    for store in [
        &MemoryStorage::default() as &dyn Storage,
        &RecordStorage::new(),
    ] {
        verify_receipt_removal(store, &f2);
    }
}

fn assert_original_retention_fields(snapshot: &Value) {
    for absent in ["clock_ms", "expiry", "admission_expires_at_ms"] {
        assert!(snapshot["retention"].get(absent).is_none(), "{absent}");
    }
    assert!(
        snapshot["retention"]["policy"]
            .get("admission_replay_ms")
            .is_none()
    );
}
fn decode_records(fixture: &Value) -> State {
    let header = fixture["format3_header"].as_str().unwrap().as_bytes();
    let records: Vec<(String, lazy::TreeKey, Vec<u8>)> = fixture["format3_records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r[0].as_str().unwrap().to_string(),
                serde_json::from_value(r[1].clone()).unwrap(),
                r[2].as_str().unwrap().as_bytes().to_vec(),
            )
        })
        .collect();
    transition::assemble_state(header, |field| {
        Ok(records
            .iter()
            .filter(|(f, _, _)| f == field)
            .map(|(_, k, v)| (k.clone(), v.clone()))
            .collect())
    })
    .unwrap()
}
fn assert_legacy_windows(state: &State) {
    let r = &state.retention;
    assert!(r.clock_ms.is_none() && r.expiry.is_empty() && r.admission_expires_at_ms.is_empty());
    assert_eq!(r.policy.admission_replay_ms, 604_800_000);
    assert_eq!(state.keyed_admissions["legacy-key"].accepted_at_ms, 10);
    // A legacy receipt expires by acceptance plus the default window.
    assert_eq!(
        retention::admission_expiry(state, "legacy-key"),
        Some(10 + 604_800_000)
    );
    let (key, event) = state.events.iter().next().unwrap();
    assert_eq!(
        retention::event_expiry(state, key),
        Some(
            r.event_expires_at_ms
                .get(key)
                .copied()
                .unwrap_or(event.accepted_at_ms + 86_400_000)
        )
    );
    assert!(!r.event_tombstones.is_empty() && !r.disposals.is_empty());
}
fn verify_receipt_removal(store: &dyn Storage, state: &State) {
    store.acquire("legacy", "w").unwrap();
    let r = store.compare_and_swap("legacy", "w", 0, state).unwrap();
    let mut next = store.load("legacy").unwrap().state;
    next.keyed_admissions.remove("legacy-key");
    next.retention.clock_ms = Some(10 + 604_800_000 - 1);
    assert!(refused_with(
        store.compare_and_swap("legacy", "w", r, &next),
        retention::REFUSE_EARLY_REMOVAL
    ));
    next.retention.clock_ms = Some(10 + 604_800_000);
    store.compare_and_swap("legacy", "w", r, &next).unwrap();
}
