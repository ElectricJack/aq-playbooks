use super::*;

/// `max_tombstones` is accepted and ignored: a commit writing more disposal
/// tombstone rows than it allows is not refused (before spec 0003 this test
/// asserted that refusal). Expired unreferenced artifact metadata is still
/// collected by the commits that move the GC clock.
#[test]
fn max_tombstones_is_ignored_and_expired_unreferenced_artifact_metadata_is_collected() {
    let n = namespace("gc-quota");
    let first = artifact(&n);
    let second = artifact(&format!("{n}-second"));
    let original = State {
        artifacts: BTreeMap::from([
            (first.digest.clone(), first.clone()),
            (second.digest.clone(), second.clone()),
        ]),
        ..State::default()
    };
    let s = store();
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &original).unwrap();
    let mut next = disposed(&first, 100);
    next.retention.policy.max_tombstones = 1;
    next.retention
        .artifact_disposals
        .insert(second.digest.clone(), 100);
    next.retention.disposals.push(Disposal {
        kind: "artifact".into(),
        key: second.digest.clone(),
        at_ms: 100,
    });
    next.retention.policy.audit_ms = 1;
    next.retention.last_gc_at_ms = Some(100);
    s.compare_and_swap(&n, "owner", 1, &next).unwrap();
    assert_same(&s.load(&n).unwrap().state, &next);
    assert_eq!(
        client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.disposal_tombstones WHERE namespace=$1",
                &[&n]
            )
            .unwrap()
            .get::<_, i64>(0),
        2
    );
    next.retention.disposals.clear();
    next.retention.artifact_disposals.clear();
    next.retention.last_gc_at_ms = Some(102);
    s.compare_and_swap(&n, "owner", 2, &next).unwrap();
    assert_eq!(
        client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.disposal_tombstones WHERE namespace=$1",
                &[&n]
            )
            .unwrap()
            .get::<_, i64>(0),
        0
    );
    assert_eq!(
        client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.artifact_catalog WHERE digest=ANY($1)",
                &[&vec![first.digest, second.digest]]
            )
            .unwrap()
            .get::<_, i64>(0),
        0
    );
}

fn disposed(artifact: &Artifact, at: i64) -> State {
    let mut state = State::default();
    state
        .retention
        .artifact_disposals
        .insert(artifact.digest.clone(), at);
    state.retention.disposals.push(Disposal {
        kind: "artifact".into(),
        key: artifact.digest.clone(),
        at_ms: at,
    });
    state
}

#[test]
fn artifact_gc_and_installation_race_preserves_shared_bytes_and_disposal_records() {
    let n = namespace("gc-a");
    let m = namespace("gc-b");
    let c = namespace("installer");
    let artifact = artifact(&n);
    let initial = State {
        artifacts: BTreeMap::from([(artifact.digest.clone(), artifact.clone())]),
        ..State::default()
    };
    let a = store();
    a.acquire(&n, "a").unwrap();
    a.compare_and_swap(&n, "a", 0, &initial).unwrap();
    let b = Arc::new(store());
    b.acquire(&m, "b").unwrap();
    b.compare_and_swap(&m, "b", 0, &initial).unwrap();
    a.compare_and_swap(&n, "a", 1, &disposed(&artifact, 100))
        .unwrap();
    assert_eq!(
        client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.artifacts WHERE digest=$1",
                &[&artifact.digest]
            )
            .unwrap()
            .get::<_, i64>(0),
        1
    );
    let install = Arc::new(store());
    install.acquire(&c, "c").unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let gc = {
        let b = b.clone();
        let m = m.clone();
        let artifact = artifact.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            b.compare_and_swap(&m, "b", 1, &disposed(&artifact, 101))
                .unwrap();
        })
    };
    let admitted = {
        let install = install.clone();
        let c = c.clone();
        let initial = initial.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            install.compare_and_swap(&c, "c", 0, &initial).unwrap();
        })
    };
    barrier.wait();
    gc.join().unwrap();
    admitted.join().unwrap();
    assert_same(&install.load(&c).unwrap().state, &initial);
    assert_eq!(client().query_one("SELECT count(*) FROM aq_playbooks.disposal_tombstones WHERE namespace=$1 AND kind='artifact'",&[&n]).unwrap().get::<_,i64>(0),1);
}

#[test]
fn a_disposal_record_licenses_no_removal_of_unresolved_children_or_external_effects() {
    let n = namespace("unresolved-gc");
    let original = boundary(&n);
    let s = store();
    s.acquire(&n, "owner").unwrap();
    s.compare_and_swap(&n, "owner", 0, &original).unwrap();
    let a = original.artifacts.values().next().unwrap();
    let mut next = disposed(a, 100);
    next.sequence = original.sequence;
    for id in ["parent", "child"] {
        next.retention.disposals.push(Disposal {
            kind: "run".into(),
            key: id.into(),
            at_ms: 100,
        });
    }
    // Spec 0003: a disposal record licenses nothing; a run leaves only with
    // its history.
    assert_eq!(
        s.compare_and_swap(&n, "owner", 1, &next),
        Err(StorageError::Backend(retention::REFUSE_RUN_REMOVAL.into()))
    );
    assert_same(&s.load(&n).unwrap().state, &original);
}
