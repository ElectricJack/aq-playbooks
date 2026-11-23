use super::*;

/// The fence stays closed for every removal the rule does not allow: no
/// history record in the commit, no expired window entry; and an expiry
/// record never moves earlier.
fn removals_without_history_or_expiry_are_refused(store: &dyn Storage) {
    let s = fence_state();
    let cases = live_identity_cases()
        .into_iter()
        .chain(receipt_cases())
        .chain(expiry_cases())
        .chain(settlement_cases());
    let failures: Vec<_> = cases
        .enumerate()
        .filter_map(|(i, case)| {
            let ns = format!("refuse-{i}");
            let revision = seed_state(store, &ns, &s);
            check_refusal(store, &ns, (revision, &s), case)
        })
        .collect();
    assert!(failures.is_empty(), "{failures:#?}");
    verify_licensed_removals(store, &s);
}

fn live_identity_cases() -> [Case; 7] {
    let early = retention::REFUSE_EARLY_REMOVAL;
    [
        ("intent", retention::REFUSE_RUN_REMOVAL, |n| {
            n.intents.remove("op-a");
        }),
        ("keyed admission", early, |n| {
            n.retention.clock_ms = Some(2_999);
            n.keyed_admissions.remove("k");
            n.retention.admission_expires_at_ms.remove("k");
        }),
        ("keyed admission, policy window", early, |n| {
            n.retention.clock_ms = Some(1_999);
            n.keyed_admissions.remove("fallback");
        }),
        ("event", early, |n| {
            n.retention.clock_ms = Some(3_999);
            n.events.remove("ev");
            n.retention.event_expires_at_ms.remove("ev");
        }),
        ("event, policy window", early, |n| {
            n.retention.clock_ms = Some(2_999);
            n.events.remove("ev-fallback");
        }),
        ("event with a forged legacy tombstone", early, |n| {
            n.events.remove("ev");
            n.retention.event_expires_at_ms.remove("ev");
            n.retention
                .event_tombstones
                .insert("ev".into(), tombstone("digest-ev"));
        }),
        (
            "changed receipt",
            "immutable keyed admission receipt removed or changed",
            |n| {
                n.keyed_admissions.get_mut("k").unwrap().run_id = "other".into();
                n.retention.clock_ms = Some(5_000);
            },
        ),
    ]
}

fn receipt_cases() -> [Case; 2] {
    [
        // Audit receipts leave only with their history.
        (
            "receipt of a live run",
            retention::REFUSE_RUN_REMOVAL,
            |n| {
                n.receipts.retain(|r| r.sequence != 2);
            },
        ),
        (
            "receipt of a settled run without its history item",
            retention::REFUSE_RUN_REMOVAL,
            |n| {
                n.receipts.clear();
                n.receipts.push(audit(2, "run-b"));
                let run = n.runs.remove("run-a").unwrap();
                let op = n.intents.remove("op-a").unwrap();
                n.settled.push(cohort(run, vec![op], vec![]));
            },
        ),
    ]
}

fn expiry_cases() -> [Case; 7] {
    let shortened = retention::REFUSE_EXPIRY;
    [
        // Expiry records: never lowered, deleted only with their item, and a new
        // one is at least the fallback.
        ("lowered expiry", shortened, |n| {
            n.retention
                .admission_expires_at_ms
                .insert("k".into(), 2_500);
        }),
        ("lowered event expiry", shortened, |n| {
            n.retention.event_expires_at_ms.insert("ev".into(), 3_999);
        }),
        ("expiry deleted, item kept", shortened, |n| {
            n.retention.admission_expires_at_ms.remove("k");
        }),
        ("event expiry deleted, event kept", shortened, |n| {
            n.retention.event_expires_at_ms.remove("ev");
        }),
        ("new expiry below the fallback", shortened, |n| {
            n.retention
                .admission_expires_at_ms
                .insert("fallback".into(), 1_999);
        }),
        ("new event expiry below the fallback", shortened, |n| {
            n.retention
                .event_expires_at_ms
                .insert("ev-fallback".into(), 2_999);
        }),
        ("new expiry without its item", shortened, |n| {
            n.retention
                .admission_expires_at_ms
                .insert("absent".into(), 1_000_000);
        }),
    ]
}

fn settlement_cases() -> [Case; 5] {
    let obligations = "unresolved run obligations cannot be disposed";
    [
        // History in the commit does not cover a run that is not finished, an
        // unsettled operation, an operation left live, or a run of another root.
        ("running", obligations, |n| {
            let running = n.runs.remove("run-b").unwrap();
            n.settled
                .push(cohort(running, vec![], vec![audit(2, "run-b")]));
            n.receipts.retain(|r| r.sequence != 2);
        }),
        ("unsettled operation", obligations, |n| {
            let run = n.runs.remove("run-a").unwrap();
            n.intents.remove("op-a");
            n.settled.push(cohort(
                run,
                vec![intent("op-a", "run-a", IntentStatus::Accepted)],
                vec![],
            ));
        }),
        ("operation left live", obligations, |n| {
            let run = n.runs.remove("run-a").unwrap();
            n.settled.push(cohort(run, vec![], vec![]));
        }),
        ("run of another root", obligations, |n| {
            let run = n.runs.remove("run-a").unwrap();
            let op = n.intents.remove("op-a").unwrap();
            let mut c = cohort(run, vec![op], vec![]);
            c.root = "run-b".into();
            n.settled.push(c);
        }),
        (
            "operation without history",
            retention::REFUSE_RUN_REMOVAL,
            |n| {
                let run = n.runs.remove("run-a").unwrap();
                n.intents.remove("op-a");
                n.settled.push(cohort(run, vec![], vec![]));
            },
        ),
    ]
}

fn verify_licensed_removals(store: &dyn Storage, s: &State) {
    // Allowed: expired entries with their records, an expiry moved later, a
    // new expiry at its fallback, and a run with its operation and receipt.
    let mut next = s.clone();
    next.retention.clock_ms = Some(4_000);
    for key in ["k", "fallback"] {
        next.keyed_admissions.remove(key);
    }
    next.retention.admission_expires_at_ms.remove("k");
    for key in ["ev", "ev-fallback"] {
        next.events.remove(key);
    }
    next.retention.event_expires_at_ms.remove("ev");
    let run = next.runs.remove("run-a").unwrap();
    let op = next.intents.remove("op-a").unwrap();
    next.receipts.retain(|r| r.sequence != 1);
    next.settled
        .push(cohort(run, vec![op], vec![audit(1, "run-a")]));
    next.keyed_admissions
        .insert("later".into(), receipt("later", 100));
    next.retention
        .admission_expires_at_ms
        .insert("later".into(), 2_100);
    let ns = "allowed";
    let base = seed_state(store, ns, s);
    let revision = store.compare_and_swap(ns, "w", base, &next).unwrap();
    let mut later = next.clone();
    later.settled.clear();
    later
        .retention
        .admission_expires_at_ms
        .insert("later".into(), 9_000);
    store.compare_and_swap(ns, "w", revision, &later).unwrap();
}
#[test]
fn memory_storage_refuses_removals_without_history_or_expiry() {
    removals_without_history_or_expiry_are_refused(&MemoryStorage::default());
}
#[test]
fn record_storage_refuses_removals_without_history_or_expiry() {
    removals_without_history_or_expiry_are_refused(&RecordStorage::new());
}

#[test]
fn memory_storage_removal_fences() {
    removal_fences_hold(&MemoryStorage::default(), "ns")
}

#[test]
fn record_storage_removal_fences() {
    removal_fences_hold(&RecordStorage::new(), "ns")
}

#[test]
fn commits_advance_the_clock_only_with_host_time() {
    let e = engine();
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
    e.admit("a", "main", Value::Null, authority(), 5_000)
        .unwrap();
    e.admit("a", "main", Value::Null, authority(), 10).unwrap();
    assert_eq!(e.snapshot().unwrap().retention.clock_ms, Some(5_000));
}

/// Commits without a host time leave the clock where it is.
#[test]
fn commits_without_host_time_do_not_move_the_clock() {
    let e = engine();
    let a = artifact(
        "a",
        "done",
        BTreeMap::from([("done".into(), ret("ok"))]),
        FixtureContracts::default(),
    );
    let digest = a.digest.clone();
    e.install(a).unwrap();
    e.activate("a", &digest, authority()).unwrap();
    e.set_retention_policy(RetentionPolicy::default()).unwrap();
    assert_eq!(e.snapshot().unwrap().retention.clock_ms, None);
    e.admit("a", "main", Value::Null, authority(), 7).unwrap();
    e.activate("b", &digest, authority()).unwrap();
    assert_eq!(e.snapshot().unwrap().retention.clock_ms, Some(7));
}

/// Legacy event tombstones and correlation fences (no engine writes them
/// any more) may always be removed; an expired event leaves by its window.
/// A legacy disposal record licenses nothing: a run removed without its
/// history is refused, disposal record or not.
fn legacy_records_license_no_removal(store: &dyn Storage) {
    store.acquire("legacy", "w").unwrap();
    let mut s = State::default();
    s.retention.policy.event_replay_ms = 1_000_000;
    s.runs.insert("run-a".into(), completed_run("run-a"));
    s.receipts.push(audit(1, "run-a"));
    s.events.insert("ev".into(), accepted_event("ev", 0));
    s.retention
        .event_expires_at_ms
        .insert("ev".into(), 1_000_000);
    s.retention
        .expired_correlations
        .insert("c".into(), ("host".into(), 0));
    s.retention
        .event_tombstones
        .insert("old".into(), tombstone("d"));
    let r = store.compare_and_swap("legacy", "w", 0, &s).unwrap();
    let mut next = s.clone();
    next.retention.clock_ms = Some(1_000_000);
    next.runs.remove("run-a");
    next.receipts.clear();
    next.retention.disposals.push(retention::Disposal {
        kind: "run".into(),
        key: "run-a".into(),
        at_ms: 5,
    });
    assert!(refused_with(
        store.compare_and_swap("legacy", "w", r, &next),
        retention::REFUSE_RUN_REMOVAL
    ));
    let mut next = s.clone();
    next.retention.clock_ms = Some(1_000_000);
    next.events.remove("ev");
    next.retention.event_expires_at_ms.remove("ev");
    next.retention.event_tombstones.remove("old");
    next.retention.expired_correlations.remove("c");
    store.compare_and_swap("legacy", "w", r, &next).unwrap();
}

#[test]
fn memory_storage_legacy_records_license_no_removal() {
    legacy_records_license_no_removal(&MemoryStorage::default());
}

#[test]
fn record_storage_legacy_records_license_no_removal() {
    legacy_records_license_no_removal(&RecordStorage::new());
}

/// `check_not_live` on a State whose runtime indexes are initialised (the
/// branch every engine commit takes): a receipt is live when the receipt
/// index points at a receipt with that sequence.
#[test]
fn check_not_live_reads_receipts_through_the_initialised_index() {
    let mut state = State::default();
    state.indexes.initialized = true;
    state.receipts.push(live_receipt(7));
    state.receipts.push(live_receipt(9));
    state.indexes.all_receipts.insert(7, 0);
    state.indexes.all_receipts.insert(9, 1);
    let items = |sequence: u64| synthetic_cohort("r", 10, &[sequence]).history_items();
    let receipts_only = |sequence: u64| {
        items(sequence)
            .into_iter()
            .filter(|i| i.kind == InspectionKind::Receipts)
            .collect::<Vec<_>>()
    };
    assert!(refused_as_also_live(history::check_not_live(
        &state,
        &receipts_only(7)
    )));
    assert!(refused_as_also_live(history::check_not_live(
        &state,
        &receipts_only(9)
    )));
    assert!(history::check_not_live(&state, &receipts_only(8)).is_ok());
    // An index entry that points at a receipt with another sequence is not
    // taken as live (it is re-checked against the receipt).
    state.indexes.all_receipts.insert(8, 1);
    assert!(history::check_not_live(&state, &receipts_only(8)).is_ok());
    // Runs and operations are map lookups on either branch.
    state.runs.insert("r".into(), completed_run("r"));
    assert!(refused_as_also_live(history::check_not_live(
        &state,
        &items(8)
    )));
}
