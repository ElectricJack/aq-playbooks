use super::*;

#[test]
fn ordered_batch_catalog_locks_serialize_multi_artifact_installation_with_gc() {
    for iteration in 0..8 {
        let n = namespace(&format!("batch-gc-{iteration}"));
        let other = namespace(&format!("batch-install-{iteration}"));
        let artifacts = [artifact(&format!("{n}-one")), artifact(&format!("{n}-two"))];
        let before = State {
            artifacts: artifacts
                .iter()
                .map(|a| (a.digest.clone(), a.clone()))
                .collect(),
            ..State::default()
        };
        let mut removed = State::default();
        for a in &artifacts {
            removed
                .retention
                .artifact_disposals
                .insert(a.digest.clone(), 10);
        }
        let a = store();
        let b = store();
        a.acquire(&n, "gc").unwrap();
        b.acquire(&other, "installer").unwrap();
        a.compare_and_swap(&n, "gc", 0, &before).unwrap();
        let barrier = Arc::new(Barrier::new(3));
        std::thread::scope(|scope| {
            let dropper = scope.spawn(|| {
                barrier.wait();
                a.compare_and_swap(&n, "gc", 1, &removed)
            });
            let installer = scope.spawn(|| {
                barrier.wait();
                b.compare_and_swap(&other, "installer", 0, &before)
            });
            barrier.wait();
            dropper.join().unwrap().unwrap();
            installer.join().unwrap().unwrap();
        });
        assert!(a.load(&n).unwrap().state.artifacts.is_empty());
        assert_same(&b.load(&other).unwrap().state, &before);
        let mut db = client();
        assert_eq!(db.query_one("SELECT count(*) FROM aq_playbooks.disposal_tombstones WHERE namespace=$1 AND kind='artifact'",&[&n]).unwrap().get::<_,i64>(0),2);
        for artifact in &artifacts {
            let bytes: Vec<u8> = db
                .query_one(
                    "SELECT canonical_bytes FROM aq_playbooks.artifacts WHERE digest=$1",
                    &[&artifact.digest],
                )
                .unwrap()
                .get(0);
            assert_eq!(
                bytes,
                aq_playbooks_model::canonical_bytes(artifact).unwrap()
            );
        }
    }
}

#[test]
fn mixed_existing_and_new_catalog_batches_commit_without_lock_inversion() {
    for iteration in 0..8 {
        let n = namespace(&format!("mixed-left-{iteration}"));
        let other = namespace(&format!("mixed-right-{iteration}"));
        let existing_left = artifact(&format!("{n}-existing"));
        let existing_right = artifact(&format!("{other}-existing"));
        let fresh = artifact(&format!("{n}-new-shared"));
        let left = State {
            artifacts: BTreeMap::from([(existing_left.digest.clone(), existing_left.clone())]),
            ..State::default()
        };
        let right = State {
            artifacts: BTreeMap::from([(existing_right.digest.clone(), existing_right.clone())]),
            ..State::default()
        };
        let after = State {
            artifacts: [existing_left, existing_right, fresh]
                .into_iter()
                .map(|a| (a.digest.clone(), a))
                .collect(),
            ..State::default()
        };
        let a = store();
        let b = store();
        a.acquire(&n, "left").unwrap();
        b.acquire(&other, "right").unwrap();
        a.compare_and_swap(&n, "left", 0, &left).unwrap();
        b.compare_and_swap(&other, "right", 0, &right).unwrap();
        let barrier = Barrier::new(3);
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                a.compare_and_swap(&n, "left", 1, &after)
            });
            let second = scope.spawn(|| {
                barrier.wait();
                b.compare_and_swap(&other, "right", 1, &after)
            });
            barrier.wait();
            first.join().unwrap().unwrap();
            second.join().unwrap().unwrap();
        });
        assert_same(&a.load(&n).unwrap().state, &after);
        assert_same(&b.load(&other).unwrap().state, &after);
        for namespace in [&n, &other] {
            assert_eq!(
                client()
                    .query_one(
                        "SELECT count(*) FROM aq_playbooks.namespace_artifacts WHERE namespace=$1",
                        &[namespace]
                    )
                    .unwrap()
                    .get::<_, i64>(0),
                3
            );
        }
    }
}
