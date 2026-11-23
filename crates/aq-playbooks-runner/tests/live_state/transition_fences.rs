use super::*;

/// The same fences on `RecordStorage`'s bounded transitions: a removal with
/// no history and no expired entry, or an expiry record moved earlier, is
/// refused and writes nothing; an expired window entry leaves incrementally.
#[test]
fn record_storage_transitions_hold_the_removal_fences() {
    let store = RecordStorage::new();
    let mut initial = fence_state();
    initial.indexes.initialized = true;
    verify_refusals(&store, &initial);
    verify_rebase_and_floor(&store, &initial);
    verify_settlement_and_expiry(&store, &initial);
}

fn transition_in(
    store: &RecordStorage,
    ns: &str,
    f: &dyn Fn(&mut State),
) -> std::result::Result<transition::TransitionOutcome, StorageError> {
    let mut plan = |s: &mut State| {
        f(s);
        Ok(true)
    };
    store
        .transition_v1(
            ns,
            "w",
            &mut TransitionRequest {
                operation: "fence",
                plan: &mut plan,
            },
        )
        .map_err(|e| match e {
            Error::Storage(e) => e,
            other => StorageError::Backend(other.to_string()),
        })
}

fn removal_cases() -> [Case; 12] {
    let early = retention::REFUSE_EARLY_REMOVAL;
    let shortened = retention::REFUSE_EXPIRY;
    [
        ("run", retention::REFUSE_RUN_REMOVAL, |s| {
            s.runs.remove("run-a");
        }),
        ("intent", retention::REFUSE_RUN_REMOVAL, |s| {
            s.intents.remove("op-a");
        }),
        ("receipt, compacted", retention::REFUSE_RUN_REMOVAL, |s| {
            s.receipts.retain(|r| r.sequence != 1);
        }),
        ("receipt, overwritten", retention::REFUSE_RUN_REMOVAL, |s| {
            *s.receipts.get_mut(0).unwrap() = audit(3, "run-a");
        }),
        ("keyed admission", early, |s| {
            s.retention.clock_ms = Some(2_999);
            s.keyed_admissions.remove("k");
            s.retention.admission_expires_at_ms.remove("k");
        }),
        ("event", early, |s| {
            s.retention.clock_ms = Some(3_999);
            s.events.remove("ev");
            s.retention.event_expires_at_ms.remove("ev");
        }),
        ("forged tombstone", early, |s| {
            s.events.remove("ev");
            s.retention.event_expires_at_ms.remove("ev");
            s.retention
                .event_tombstones
                .insert("ev".into(), tombstone("digest-ev"));
        }),
        (
            "changed receipt",
            "immutable keyed admission receipt removed or changed",
            |s| {
                s.keyed_admissions.get_mut("k").unwrap().run_id = "other".into();
            },
        ),
        ("clock", retention::REFUSE_CLOCK, |s| {
            s.retention.clock_ms = Some(499);
        }),
        ("lowered expiry", shortened, |s| {
            s.retention
                .admission_expires_at_ms
                .insert("k".into(), 2_500);
        }),
        ("expiry deleted, item kept", shortened, |s| {
            s.retention.event_expires_at_ms.remove("ev");
        }),
        ("new expiry below the fallback", shortened, |s| {
            s.retention
                .admission_expires_at_ms
                .insert("fallback".into(), 1_999);
        }),
    ]
}

fn verify_refusals(store: &RecordStorage, initial: &State) {
    let failures: Vec<String> = removal_cases()
        .into_iter()
        .chain(rebase_refusal_cases())
        .enumerate()
        .filter_map(|(i, (label, text, f))| {
            let ns = format!("refusal-{i}");
            let base = seed_state(store, &ns, initial);
            let result = transition_in(store, &ns, &f);
            if result.is_err() {
                assert_eq!(store.load(&ns).unwrap().revision, base, "{label}");
            }
            (!refused_with(result.clone(), text)).then(|| format!("{label}: {result:?}"))
        })
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
}

fn verify_rebase_and_floor(store: &RecordStorage, initial: &State) {
    // Ruling 3: a licensed rebase commits through the bounded transition,
    // and the licence is that commit's own.
    let ns = "rebase";
    seed_state(store, ns, initial);
    transition_in(store, ns, &rebased).unwrap();
    let r = store.load(ns).unwrap().state.retention;
    assert_eq!(
        (
            r.clock_ms,
            r.stale_floor_ms,
            r.event_expires_at_ms.get("ev").copied()
        ),
        (Some(100), Some(50), Some(3_100))
    );
    for (label, text, f) in after_rebase_refusal_cases() {
        let result = transition_in(store, ns, &f);
        assert!(refused_with(result.clone(), text), "{label}: {result:?}");
    }
    transition_in(store, ns, &floor_lowered).unwrap();
    assert_eq!(
        store.load(ns).unwrap().state.retention.stale_floor_ms,
        Some(40)
    );
}

fn verify_settlement_and_expiry(store: &RecordStorage, initial: &State) {
    let ns = "expiry";
    seed_state(store, ns, initial);
    let transition = |f: &dyn Fn(&mut State)| transition_in(store, ns, f);
    // Expired: both entries leave in one bounded transition.
    transition(&|s| {
        s.retention.clock_ms = Some(4_000);
        s.keyed_admissions.remove("k");
        s.retention.admission_expires_at_ms.remove("k");
        s.events.remove("ev");
        s.retention.event_expires_at_ms.remove("ev");
    })
    .unwrap();
    let state = store.load(ns).unwrap().state;
    assert!(!state.keyed_admissions.contains_key("k") && !state.events.contains_key("ev"));
    // With history for the run, its operation and its receipt, the run
    // leaves.
    transition(&|s| {
        let run = s.runs.remove("run-a").unwrap();
        let op = s.intents.remove("op-a").unwrap();
        s.receipts.retain(|r| r.sequence != 1);
        s.settled
            .push(cohort(run, vec![op], vec![audit(1, "run-a")]));
    })
    .unwrap();
    let state = store.load(ns).unwrap().state;
    assert!(!state.runs.contains_key("run-a"));
    assert_eq!(
        state
            .receipts
            .iter()
            .map(|r| r.sequence)
            .collect::<Vec<_>>(),
        vec![2]
    );
}
