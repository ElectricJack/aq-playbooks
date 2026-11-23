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
