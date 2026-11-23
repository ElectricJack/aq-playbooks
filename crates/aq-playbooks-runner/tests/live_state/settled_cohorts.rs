use super::*;

#[test]
fn finished_root_leaves_the_record_in_the_commit_that_finishes_it() {
    let e = engine();
    install(&e, done_artifact(), "a");
    let id = e.admit("a", "main", Value::Null, authority(), 1).unwrap();
    let before = e.store.load("test").unwrap().revision;
    assert!(e.advance(&id, 2).unwrap());
    let v = e.store.load("test").unwrap();
    assert_eq!(v.revision, before + 1);
    assert!(
        v.state.runs.is_empty()
            && v.state.intents.is_empty()
            && v.state.receipts.is_empty()
            && v.state.budgets.is_empty()
    );
    assert!(
        v.state.indexes.root_members.is_empty()
            && v.state.indexes.run_roots.is_empty()
            && v.state.indexes.receipts.is_empty()
    );
    let run = &runs_page(&e, Some(&id))[0];
    assert_eq!(
        (run["status"].as_str(), run["outcome"].as_str()),
        (Some("completed"), Some("ok"))
    );
}

#[test]
fn cohort_stays_while_an_independent_child_runs() {
    let e = engine();
    let child = wait_artifact();
    e.install(child.clone()).unwrap();
    install(&e, parent_artifact(&child, CallMode::Independent), "parent");
    let id = e
        .admit("parent", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs.len(), 2);
    assert!(s.runs[&id].status.terminal());
    e.admit_event("host", event("e1", 2), 5).unwrap();
    e.drive(&mut FakeHost::default(), 5, 20).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    assert_eq!(runs_page(&e, None).len(), 2);
}

#[test]
fn cancelled_run_stays_until_its_operation_settles_then_late_results_are_not_found() {
    let e = engine();
    let id = admit_invoke(&e);
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "test.command".into(),
            vec![DispatchResult::Unknown {
                reason: "lost".into(),
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 0, 10).unwrap();
    e.cancel(&id, 1).unwrap();
    e.advance(&id, 1).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].status, RunStatus::Cancelling);
    let intent = s.intents.keys().next().unwrap().clone();
    let done = DispatchResult::Completed {
        outcome: "ok".into(),
        output: Value::Null,
    };
    e.record_result(&intent, done.clone(), 2).unwrap();
    e.advance(&id, 2).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    assert_eq!(runs_page(&e, Some(&id))[0]["status"], "cancelled");
    assert!(matches!(
        e.record_result(&intent, done, 3),
        Err(Error::NotFound(_))
    ));
    assert_missing_run(&e, &id, 3);
}

/// The one item of `kind` whose inspection key is `key`.
fn inspected(e: &Engine<MemoryStorage>, kind: InspectionKind, key: &str) -> Value {
    let items: Vec<Value> = inspect_all(e, kind, None)
        .into_iter()
        .filter(|v| match kind {
            InspectionKind::Receipts => format!("{:020}", v["sequence"].as_u64().unwrap()) == key,
            _ => v["id"] == key,
        })
        .collect();
    assert_eq!(items.len(), 1, "{kind:?} {key}");
    items.into_iter().next().unwrap()
}

#[test]
fn history_equals_the_projection_of_the_removed_cohort() {
    let e = engine();
    let id = admit_invoke(&e);
    let mut host = completed_host();
    e.drive(&mut host, 0, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty() && s.intents.is_empty() && s.receipts.is_empty());
    let cohorts = e.store.settled_cohorts("test");
    assert_eq!(cohorts.len(), 1);
    let cohort = &cohorts[0];
    assert_cohort(cohort, &id);
    assert_history_rows(&e, cohort);
    // The private fields stay readable from the retained cohort.
    assert_eq!(
        e.store.settled_run("test", &id).unwrap().outcome.as_deref(),
        Some("ok")
    );
    assert_eq!(cohort.intents[0].result, Some(("ok".into(), Value::Null)));
}

#[test]
fn admin_receipts_move_to_history_in_the_commit_that_writes_them() {
    let e = engine();
    install(&e, done_artifact(), "a");
    e.set_activation_controls("a", ActivationControls::default(), 3)
        .unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.receipts.iter().all(|r| r.run != "admin"));
    let receipts = e
        .inspect(&InspectionQuery {
            kind: InspectionKind::Receipts,
            run: Some("admin".into()),
            after: None,
            limit: 10,
        })
        .unwrap();
    assert_eq!(receipts.items[0]["kind"], "activation_controls");
}

#[test]
fn record_storage_settles_like_memory_storage() {
    let m = engine();
    let r = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
    let child = wait_artifact();
    let parent = parent_artifact(&child, CallMode::Independent);
    m.install(child.clone()).unwrap();
    r.install(child.clone()).unwrap();
    assert_same_engines(&m, &r, "install");
    install(&m, parent.clone(), "parent");
    install_on(&r, parent, "parent");
    assert_same_engines(&m, &r, "activate");
    let id = m
        .admit("parent", "main", Value::Null, authority(), 0)
        .unwrap();
    assert_eq!(
        r.admit("parent", "main", Value::Null, authority(), 0)
            .unwrap(),
        id
    );
    assert_same_engines(&m, &r, "admit");
    m.drive(&mut FakeHost::default(), 0, 20).unwrap();
    let transitions = r
        .store
        .stats
        .transitions
        .load(std::sync::atomic::Ordering::Relaxed);
    r.drive(&mut FakeHost::default(), 0, 20).unwrap();
    assert_same_engines(&m, &r, "drive 0");
    assert_eq!(m.snapshot().unwrap().runs.len(), 2);
    m.admit_event("host", event("e1", 2), 5).unwrap();
    r.admit_event("host", event("e1", 2), 5).unwrap();
    assert_same_engines(&m, &r, "event");
    m.drive(&mut FakeHost::default(), 5, 20).unwrap();
    r.drive(&mut FakeHost::default(), 5, 20).unwrap();
    assert_same_engines(&m, &r, "drive 5");
    // The settling commit took the record backend's transition path.
    assert!(
        r.store
            .stats
            .transitions
            .load(std::sync::atomic::Ordering::Relaxed)
            > transitions
    );
    assert!(r.snapshot().unwrap().runs.is_empty());
    assert_eq!(inspect_all(&r, InspectionKind::Runs, None).len(), 2);
}

/// A run key that is already history cannot become live again: the engine's
/// commit is refused, and the refusal leaves the revision and the lease.
#[test]
fn a_live_key_already_in_history_is_refused_through_the_engine() {
    let e = engine();
    install(&e, done_artifact(), "a");
    let v = e.store.load("test").unwrap();
    let next = format!("run-{:016x}", v.state.sequence + 1);
    let mut seeded = v.state.clone();
    seeded.settled.push(synthetic_cohort(&next, 0, &[]));
    e.store
        .compare_and_swap("test", "incarnation-1", v.revision, &seeded)
        .unwrap();
    let revision = e.store.load("test").unwrap().revision;
    let refused = e.admit("a", "main", Value::Null, authority(), 1);
    assert!(
        matches!(&refused, Err(Error::Storage(StorageError::Backend(m))) if m == history::ALSO_LIVE),
        "{refused:?}"
    );
    let after = e.store.load("test").unwrap();
    assert_eq!(after.revision, revision);
    assert!(after.state.runs.is_empty());
    // The lease is still this engine's.
    assert_eq!(
        e.store.acquire("test", "other"),
        Err(StorageError::Ownership)
    );
    e.set_activation_controls("a", ActivationControls::default(), 2)
        .unwrap();
    assert_eq!(e.store.load("test").unwrap().revision, revision + 1);
}

/// A run that moves from the live state to history between two inspection
/// pages is neither repeated nor skipped.
#[test]
fn a_run_moving_to_history_between_pages_is_neither_repeated_nor_skipped() {
    let e = engine();
    install(&e, done_artifact(), "a");
    let ids: Vec<String> = (0..3)
        .map(|i| e.admit("a", "main", Value::Null, authority(), i).unwrap())
        .collect();
    let query = |after: Option<String>| InspectionQuery {
        kind: InspectionKind::Runs,
        run: None,
        after,
        limit: 1,
    };
    let first = e.inspect(&query(None)).unwrap();
    assert_run_page(&first, &ids[0], "admitted");
    // The run already paged and the next one both leave the state.
    assert!(e.advance(&ids[0], 5).unwrap());
    assert!(e.advance(&ids[1], 5).unwrap());
    let second = e.inspect(&query(first.next)).unwrap();
    assert_run_page(&second, &ids[1], "completed");
    let third = e.inspect(&query(second.next)).unwrap();
    assert_run_page(&third, &ids[2], "admitted");
    assert!(third.next.is_none());
    assert_receipt_pages_across_settlement(&e, &ids[2]);
}

/// Commands addressed to a run that has left the record get `NotFound`
/// (today's answer for a collected run), and a late wake is a no-op.
#[test]
fn commands_to_a_run_that_left_the_record_are_not_found() {
    let e = engine();
    let child = wait_artifact();
    install(&e, child, "waiter");
    let id = e
        .admit("waiter", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&id].status, RunStatus::Waiting);
    // Its deadline timer is due at 100.
    assert!(!s.indexes.timers.is_empty());
    e.admit_event("host", event("e1", 2), 5).unwrap();
    e.drive(&mut FakeHost::default(), 5, 20).unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.runs.is_empty() && s.waits.is_empty() && s.indexes.timers.is_empty());
    assert_eq!(runs_page(&e, Some(&id))[0]["outcome"], "ok");
    assert_missing_run(&e, &id, 6);
    // A late wake: driving past the old deadline finds nothing to do.
    let revision = e.store.load("test").unwrap().revision;
    assert_eq!(e.drive(&mut FakeHost::default(), 1_000, 20).unwrap(), 0);
    assert_eq!(e.store.load("test").unwrap().revision, revision);
}

/// A late result for an operation that left with its cohort, and a recovery
/// request for it, are `NotFound`.
#[test]
fn late_results_and_recovery_for_a_removed_operation_are_not_found() {
    let e = engine();
    let id = admit_invoke(&e);
    // The invoke step prepares the operation; cancelling the run cancels the
    // prepared operation, and the run then finishes cancelled.
    e.advance(&id, 0).unwrap();
    assert_eq!(e.snapshot().unwrap().intents.len(), 1);
    e.cancel(&id, 0).unwrap();
    e.advance(&id, 0).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    let cohort = &e.store.settled_cohorts("test")[0];
    assert_eq!(cohort.intents.len(), 1);
    assert_eq!(cohort.intents[0].status, IntentStatus::Cancelled);
    let intent = cohort.intents[0].id.clone();
    let late = DispatchResult::Completed {
        outcome: "ok".into(),
        output: Value::Null,
    };
    assert!(matches!(
        e.record_result(&intent, late, 1),
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        e.request_recovery(&intent, "op", "late", vec!["ref".into()], 1),
        Err(Error::NotFound(_))
    ));
}

/// `activate` takes its time as the later of the last receipt and the clock:
/// once every receipt has left, the clock still dates the activation.
#[test]
fn activate_dates_by_the_clock_once_receipts_have_left() {
    let e = engine();
    install(&e, done_artifact(), "a");
    let id = e.admit("a", "main", Value::Null, authority(), 40).unwrap();
    e.advance(&id, 50).unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.receipts.is_empty());
    assert_eq!(s.retention.clock_ms, Some(50));
    e.activate("b", &done_artifact().digest, authority())
        .unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.catalog_history_at_ms[&s.catalog_revision], 50);
    assert_eq!(s.retention.clock_ms, Some(50));
}

/// Settlement is found from the commit's dirty roots and the root indexes
/// (SC-6): settling one cohort reads the same records whether 20 or 80 other
/// runs are live, and scans no run, operation or root index container.
#[test]
fn settling_one_cohort_does_not_scan_other_live_runs() {
    // Both sizes exceed the 16-record page a bounded index read takes (the
    // due-timer query reads one such page), so equal reads mean nothing
    // scales with the number of live runs.
    let (few, few_scanned) = settlement_reads(20);
    let (many, many_scanned) = settlement_reads(80);
    assert_eq!(few, many, "record reads of the settling commit");
    assert_eq!(few_scanned, many_scanned);
    // Only the positional receipt sequence and its offset indexes are
    // rewritten whole (SC-6: compaction is O(live receipts)).
    let allowed: BTreeSet<String> = ["receipts", "indexes.receipts", "indexes.all_receipts"]
        .into_iter()
        .map(String::from)
        .collect();
    assert!(many_scanned.is_subset(&allowed), "{many_scanned:?}");
}

fn admit_invoke(e: &Engine<MemoryStorage>) -> String {
    install(
        e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    e.admit("invoke", "main", Value::Null, authority(), 0)
        .unwrap()
}

fn assert_missing_run(e: &Engine<MemoryStorage>, id: &str, time: i64) {
    assert!(matches!(e.advance(id, time), Err(Error::NotFound(_))));
    assert!(matches!(e.cancel(id, time), Err(Error::NotFound(_))));
}

fn assert_cohort(cohort: &SettledCohort, id: &str) {
    assert_eq!(cohort.root, id);
    assert_eq!((cohort.runs.len(), cohort.intents.len()), (1, 1));
    assert!(cohort.receipts.len() >= 4, "{:?}", cohort.receipts);
    // Finish time: the terminal receipt's time.
    let terminal = cohort.runs[0].terminal_sequence.unwrap();
    let at = cohort
        .receipts
        .iter()
        .find(|r| r.sequence == terminal)
        .unwrap()
        .at_ms;
    assert_eq!(cohort.finished_at_ms, at);
}
fn assert_history_rows(e: &Engine<MemoryStorage>, cohort: &SettledCohort) {
    for run in &cohort.runs {
        assert_eq!(
            inspection::public_run(run),
            inspected(e, InspectionKind::Runs, &run.id)
        );
    }
    for intent in &cohort.intents {
        assert_eq!(
            inspection::public_intent(intent),
            inspected(e, InspectionKind::Effects, &intent.id)
        );
    }
    for receipt in &cohort.receipts {
        assert_eq!(
            inspection::public_receipt(receipt),
            inspected(
                e,
                InspectionKind::Receipts,
                &format!("{:020}", receipt.sequence)
            )
        );
    }
}

fn assert_run_page(page: &InspectionPage, id: &str, status: &str) {
    let item = &page.items[0];
    assert_eq!(
        (item["id"].as_str(), item["status"].as_str()),
        (Some(id), Some(status))
    );
}

fn assert_receipt_pages_across_settlement(e: &Engine<MemoryStorage>, run: &str) {
    // Receipts too: a receipt page edge across the move.
    let receipts = |after: Option<String>| {
        e.inspect(&InspectionQuery {
            kind: InspectionKind::Receipts,
            run: None,
            after,
            limit: 1,
        })
        .unwrap()
    };
    let page = receipts(None);
    let mut seen = vec![page.items[0]["sequence"].as_u64().unwrap()];
    e.advance(run, 6).unwrap();
    let mut after = page.next;
    while let Some(cursor) = after {
        let page = receipts(Some(cursor));
        seen.extend(page.items.iter().map(|v| v["sequence"].as_u64().unwrap()));
        after = page.next;
    }
    let all: Vec<u64> = e
        .store
        .settled_cohorts("test")
        .iter()
        .flat_map(|c| c.receipts.iter().map(|r| r.sequence))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(seen, all);
}

fn settlement_reads(others: usize) -> (usize, BTreeSet<String>) {
    use std::sync::atomic::Ordering;

    let e = Engine::new(RecordStorage::new(), "test", "incarnation-1").unwrap();
    install_on(&e, done_artifact(), "a");
    for i in 0..others {
        e.admit("a", "main", Value::Null, authority(), i as i64)
            .unwrap();
    }
    let id = e.admit("a", "main", Value::Null, authority(), 100).unwrap();
    let reads = e.store.stats.record_reads.load(Ordering::Relaxed);
    let fallbacks = e.store.stats.fallbacks().len();
    let transitions = e.store.stats.transitions.load(Ordering::Relaxed);
    assert!(e.advance(&id, 101).unwrap());
    assert_eq!(
        e.store.stats.transitions.load(Ordering::Relaxed),
        transitions + 1
    );
    assert!(!e.snapshot().unwrap().runs.contains_key(&id));
    assert_eq!(e.snapshot().unwrap().runs.len(), others);
    let scanned: BTreeSet<String> = e.store.stats.fallbacks()[fallbacks..]
        .iter()
        .map(|f| f.split(' ').next().unwrap().to_string())
        .collect();
    (
        e.store.stats.record_reads.load(Ordering::Relaxed) - reads,
        scanned,
    )
}

/// M4: a finished run's inspection items are byte-for-byte the same while it
/// is still live (its cohort waits for an independent child) and after its
/// cohort has left as history.
#[test]
fn a_finished_runs_inspection_is_identical_before_and_after_removal() {
    let e = engine();
    let child = wait_artifact();
    e.install(child.clone()).unwrap();
    install(&e, parent_artifact(&child, CallMode::Independent), "parent");
    let root = e
        .admit("parent", "main", Value::Null, authority(), 0)
        .unwrap();
    e.drive(&mut FakeHost::default(), 0, 20).unwrap();
    assert!(e.snapshot().unwrap().runs[&root].status.terminal());
    let pages = |e: &Engine<MemoryStorage>| -> Vec<Vec<u8>> {
        [
            InspectionKind::Runs,
            InspectionKind::Effects,
            InspectionKind::Receipts,
        ]
        .into_iter()
        .map(|kind| serde_json::to_vec(&inspect_all(e, kind, Some(&root))).unwrap())
        .collect()
    };
    let live = pages(&e);
    assert!(live[2].len() > 2, "the root has receipts");
    e.admit_event("host", event("e1", 2), 5).unwrap();
    e.drive(&mut FakeHost::default(), 5, 20).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    assert_eq!(pages(&e), live);
}
