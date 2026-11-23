use super::*;

/// Ruling 2: a cohort whose settling commit is refused does not wedge the
/// ready queue. The refusal reaches the host (`drive` returns it) and the
/// failing run moves behind the others, so unrelated runs still progress.
#[test]
fn an_unsettleable_cohort_does_not_stop_unrelated_runs() {
    let e = engine();
    install(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    install(&e, done_artifact(), "a");
    let mut host = completed_host();
    let (stuck, _) = run_with_completed_operation(&e, &mut host);
    // Break the index behind the engine's back (flag kept), so the commit
    // that finishes `stuck` is refused by the removal fence.
    let mut broken = e.snapshot().unwrap();
    broken.indexes.root_intents.clear();
    overwrite(&e.store, "incarnation-1", &broken);
    // Drive until the finishing commit of `stuck` is refused: `stuck` is
    // then at the head of the ready queue.
    let refused = (0..5).find_map(|_| e.drive(&mut host, 5, 20).err());
    assert!(
        matches!(&refused, Some(Error::Storage(StorageError::Backend(m))) if m == "unresolved run obligations cannot be disposed"),
        "{refused:?}"
    );
    let unrelated: Vec<String> = (0..3)
        .map(|i| {
            e.admit("a", "main", Value::Null, authority(), 1 + i)
                .unwrap()
        })
        .collect();
    let mut refusals = 0;
    for _ in 0..20 {
        match e.drive(&mut host, 5, 20) {
            Ok(_) => {}
            Err(Error::Storage(StorageError::Backend(m))) => {
                assert_eq!(m, "unresolved run obligations cannot be disposed");
                refusals += 1;
            }
            Err(other) => panic!("{other}"),
        }
    }
    assert!(refusals > 0, "the refusal surfaces to the host");
    let s = e.snapshot().unwrap();
    assert!(
        s.runs.contains_key(&stuck),
        "the unsettleable run stays live"
    );
    for id in &unrelated {
        assert!(!s.runs.contains_key(id), "{id} progressed and left");
        assert_eq!(runs_page(&e, Some(id))[0]["outcome"], "ok");
    }
}

/// Ruling 3 (M5): a run whose root has gone is an engine fault. The commit
/// fails with an internal error; the run is not sent down its playbook's
/// error branch and nothing is written.
#[test]
fn a_missing_root_fails_the_commit_instead_of_taking_an_error_branch() {
    let e = engine();
    let child = wait_artifact();
    e.install(child.clone()).unwrap();
    install(&e, parent_artifact(&child, CallMode::Independent), "parent");
    let root = e
        .admit("parent", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&root, 0).unwrap();
    e.advance(&root, 0).unwrap();
    let mut s = e.snapshot().unwrap();
    assert!(s.runs[&root].status.terminal());
    let member = s.runs.keys().find(|id| **id != root).unwrap().clone();
    // Remove only the root, with its history (the fence allows it: the
    // independent member is not its owned child).
    let run = s.runs.remove(&root).unwrap();
    let receipts: Vec<Receipt> = s
        .receipts
        .iter()
        .filter(|r| r.run == root)
        .cloned()
        .collect();
    s.receipts.retain(|r| r.run != root);
    s.indexes.receipts.clear();
    s.indexes.all_receipts.clear();
    for (offset, r) in s.receipts.clone().iter().enumerate() {
        s.indexes
            .receipts
            .entry(r.run.clone())
            .or_default()
            .insert(r.sequence, offset);
        s.indexes.all_receipts.insert(r.sequence, offset);
    }
    s.settled.push(SettledCohort {
        root: root.clone(),
        finished_at_ms: 0,
        runs: vec![run],
        intents: vec![],
        receipts,
    });
    overwrite(&e.store, "incarnation-1", &s);
    let before = e.store.load("test").unwrap();
    let refused = e.advance(&member, 1);
    assert!(
        matches!(&refused, Err(Error::Conflict(m)) if m.starts_with("engine invariant violated")),
        "{refused:?}"
    );
    let after = e.store.load("test").unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(
        after.state.runs[&member].step,
        before.state.runs[&member].step
    );
    assert!(after.state.receipts.iter().all(|r| r.kind != "error"));
}

/// The backfill runs once: the flag is stored, and a second open does not
/// read the operations again (format 3 record reads of `intents`).
#[test]
fn root_intents_backfill_runs_once() {
    let store = RecordStorage::new();
    let e = Engine::new(store.clone(), "test", "incarnation-1").unwrap();
    install_on(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    let mut host = completed_host();
    run_with_completed_operation(&e, &mut host);
    overwrite(
        &e.store,
        "incarnation-1",
        &vendored_shape(&e.snapshot().unwrap()),
    );
    e.release().unwrap();
    let scanned = |store: &RecordStorage| {
        store
            .stats
            .fallbacks()
            .iter()
            .filter(|f| f.starts_with("intents "))
            .count()
    };
    let before = scanned(&store);
    let e = Engine::new(store.clone(), "test", "incarnation-2").unwrap();
    let first = scanned(&store) - before;
    assert_eq!(first, 1, "the backfill reads the operations once");
    e.release().unwrap();
    let e = Engine::new(store.clone(), "test", "incarnation-3").unwrap();
    assert_eq!(scanned(&store) - before, first, "the second open does not");
    assert!(e.snapshot().unwrap().indexes.root_intents_complete);
    // A new namespace is complete from its first rebuild.
    let fresh = Engine::new(MemoryStorage::default(), "fresh", "w").unwrap();
    assert!(fresh.snapshot().unwrap().indexes.root_intents_complete);
}
