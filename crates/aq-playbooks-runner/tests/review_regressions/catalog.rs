use super::*;

#[test]
fn t4_transitive_dependency_bytes_stay_pinned_after_activation_and_restart() {
    let e = engine();
    let old = artifact("leaf", "ok", BTreeMap::from([("ok".into(), ret("ok"))]));
    let mut new = old.clone();
    new.content.playbook.purpose = "edited source, newly activated".into();
    reseal(&mut new);
    let middle = caller("middle", &old);
    let root = caller("root", &middle);
    for a in [&old, &middle, &root] {
        install(&e, a, &a.content.playbook.id);
    }
    let id = e
        .admit("root", "main", Value::Null, authority(), 0)
        .unwrap();
    e.install(new.clone()).unwrap();
    e.activate("leaf", &new.digest, authority()).unwrap();
    let store = e.store.clone();
    e.release().unwrap();
    let e = Engine::new(store, "review", "owner-two").unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 1, 50).unwrap();
    // The finished cohort is history; it ran the pinned bytes.
    assert!(e.snapshot().unwrap().runs.is_empty());
    let runs = settled_runs(&e);
    assert_eq!(settled(&e, &id).status, RunStatus::Completed);
    let digests: BTreeSet<_> = runs.iter().map(|run| &run.artifact).collect();
    assert!(digests.contains(&old.digest));
    assert!(!digests.contains(&new.digest));
    assert!(
        settled(&e, &id)
            .dependencies
            .values()
            .any(|d| d == &old.digest)
    );
}

#[test]
fn catalog_dependencies_select_compatible_versions_once_then_pin_across_restart() {
    let e = engine();
    let old = artifact(
        "catalog-child",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    let mut new = old.clone();
    new.content.playbook.purpose = "compatible newly activated implementation".into();
    reseal(&mut new);
    let mut parent = caller("catalog-parent", &old);
    parent
        .content
        .dependencies
        .get_mut("child")
        .unwrap()
        .selection = DependencySelection::Catalog {
        allowed_artifacts: BTreeSet::new(),
    };
    reseal(&mut parent);
    install(&e, &old, "catalog-child");
    install(&e, &parent, "catalog-parent");
    let first = e
        .admit("catalog-parent", "main", Value::Null, authority(), 0)
        .unwrap();
    install(&e, &new, "catalog-child");
    let second = e
        .admit("catalog-parent", "main", Value::Null, authority(), 1)
        .unwrap();
    let key = format!("{}/child", parent.digest);
    let state = e.snapshot().unwrap();
    assert_eq!(state.runs[&first].dependencies[&key], old.digest);
    assert_eq!(state.runs[&second].dependencies[&key], new.digest);
    let store = e.store.clone();
    e.release().unwrap();
    let e = Engine::new(store, "review", "owner-after-catalog-restart").unwrap();
    let mut host = FakeHost::default();
    e.drive(&mut host, 2, 100).unwrap();
    assert!(e.snapshot().unwrap().runs.is_empty());
    let runs = settled_runs(&e);
    for (root, expected) in [(&first, &old.digest), (&second, &new.digest)] {
        assert_eq!(settled(&e, root).status, RunStatus::Completed);
        let child = runs
            .iter()
            .find(|run| run.parent.as_ref() == Some(root))
            .unwrap();
        assert_eq!(&child.artifact, expected);
    }
}

#[test]
fn catalog_dependencies_reject_incompatible_versions_new_capabilities_and_digest_exclusions() {
    let e = engine();
    let old = artifact(
        "checked-catalog-child",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    let mut parent = caller("checked-catalog-parent", &old);
    parent
        .content
        .dependencies
        .get_mut("child")
        .unwrap()
        .selection = DependencySelection::Catalog {
        allowed_artifacts: BTreeSet::new(),
    };
    reseal(&mut parent);
    install(&e, &old, "checked-catalog-child");
    install(&e, &parent, "checked-catalog-parent");
    let mut incompatible = old.clone();
    incompatible
        .content
        .playbook
        .entries
        .get_mut("main")
        .unwrap()
        .outcomes
        .insert("ok".into(), Schema::string(32));
    incompatible
        .content
        .playbook
        .steps
        .get_mut("ok")
        .unwrap()
        .op = Operation::Return {
        outcome: "ok".into(),
        output: Expr::literal("incompatible result"),
    };
    reseal(&mut incompatible);
    install(&e, &incompatible, "checked-catalog-child");
    verify_catalog_refusal(&e, 0);
    let mut privileged = old.clone();
    privileged
        .content
        .playbook
        .required_capabilities
        .insert("additional".into());
    reseal(&mut privileged);
    e.install(privileged.clone()).unwrap();
    let mut grant = authority();
    grant.capabilities.insert("additional".into());
    e.activate("checked-catalog-child", &privileged.digest, grant)
        .unwrap();
    verify_catalog_refusal(&e, 1);
    install(&e, &old, "checked-catalog-child");
    if let DependencySelection::Catalog { allowed_artifacts } = &mut parent
        .content
        .dependencies
        .get_mut("child")
        .unwrap()
        .selection
    {
        allowed_artifacts.insert(hash_bytes(b"some-other-approved-version"));
    }
    reseal(&mut parent);
    install(&e, &parent, "checked-catalog-parent");
    verify_catalog_refusal(&e, 2);
}

#[test]
fn garbage_collection_preserves_transitively_bound_artifacts_without_runs() {
    let e = engine();
    let leaf = artifact(
        "retained-leaf",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    let middle = caller("retained-middle", &leaf);
    let root = caller("retained-root", &middle);
    let orphan = artifact(
        "unreferenced",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    for a in [&leaf, &middle, &orphan] {
        e.install(a.clone()).unwrap();
    }
    install(&e, &root, "retained-root");
    e.collect_garbage(1_000_000_000, 100).unwrap();
    let s = e.snapshot().unwrap();
    for a in [&leaf, &middle, &root] {
        assert!(s.artifacts.contains_key(&a.digest));
    }
    assert!(!s.artifacts.contains_key(&orphan.digest));
    assert!(s.runs.is_empty());
}

fn verify_catalog_refusal(e: &Engine<MemoryStorage>, now: i64) {
    assert!(
        e.admit(
            "checked-catalog-parent",
            "main",
            Value::Null,
            authority(),
            now
        )
        .is_err()
    );
    assert!(e.snapshot().unwrap().runs.is_empty());
}
