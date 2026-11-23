use super::*;

#[test]
fn catalog_v2_3_manifest_and_fixtures_are_pinned() {
    let registry: Registry = serde_json::from_str(catalog::REGISTRY_2_3_JSON).unwrap();
    assert_eq!(registry, catalog::catalog_v2_3());
    // The committed bytes are exactly what the generator writes.
    assert_eq!(registry, build_v2_3());
    assert_eq!(catalog::REGISTRY_2_3_JSON, pretty(&registry));
}

#[test]
fn manifest_pins_release_and_predecessor() {
    let registry = catalog::catalog_v2_3();
    let manifest: Value = serde_json::from_str(catalog::MANIFEST_2_3_JSON).unwrap();
    assert_eq!(
        (catalog::VERSION_2_3, manifest["version"].as_str()),
        ("2.3.0", Some(catalog::VERSION_2_3))
    );
    assert_eq!(
        manifest["fingerprint"],
        digest(&(catalog::VERSION_2_3, &registry)).unwrap()
    );
}

#[test]
fn the_manifest_supersedes_2_2_with_a_new_fingerprint() {
    let manifest: Value = serde_json::from_str(catalog::MANIFEST_2_3_JSON).unwrap();
    let v22_manifest: Value = serde_json::from_str(catalog::MANIFEST_2_2_JSON).unwrap();
    assert_ne!(manifest["fingerprint"], v22_manifest["fingerprint"]);
    assert_eq!(
        (
            &manifest["supersedes"]["version"],
            &manifest["supersedes"]["fingerprint"]
        ),
        (&json!("2.2.0"), &v22_manifest["fingerprint"])
    );
}

#[test]
fn manifest_names_additions_and_preserves_context() {
    let manifest: Value = serde_json::from_str(catalog::MANIFEST_2_3_JSON).unwrap();
    let v22_manifest: Value = serde_json::from_str(catalog::MANIFEST_2_2_JSON).unwrap();
    for (field, names) in [
        ("commands_added", json!(ADDED_COMMANDS)),
        ("commands_changed", json!(CHANGED_COMMANDS)),
        ("events_added", json!(ADDED_EVENTS)),
    ] {
        assert_eq!(manifest[field], names, "{field}");
    }
    for key in ["context_views", "work_kinds", "work_change_variants"] {
        assert_eq!(manifest[key], v22_manifest[key], "{key}");
    }
}

#[test]
fn published_schemas_validate() {
    let registry = catalog::catalog_v2_3();
    assert_eq!(registry.commands.len(), 16 + ADDED_COMMANDS.len());
    assert_eq!(registry.events.len(), 11 + ADDED_EVENTS.len());
    for (name, command) in &registry.commands {
        assert_eq!(name, &command.name);
        command.input.validate_definition().unwrap();
        for output in command.outcomes.values() {
            output.validate_definition().unwrap();
        }
    }
    for (name, event) in &registry.events {
        assert_eq!(name, &event.name);
        event.payload.validate_definition().unwrap();
    }
}

#[test]
fn added_contracts_match_their_published_fingerprints() {
    let registry = catalog::catalog_v2_3();
    let compat = &fixtures()["compatibility"];
    for name in ADDED_COMMANDS {
        let c = &registry.commands[name];
        assert_eq!(
            (c.version.as_str(), &c.capabilities, &c.scopes),
            ("1.0.0", &strings(&[name]), &strings(&["project"])),
            "{name}"
        );
        assert_eq!(
            (&c.effect, &c.recovery, c.cancellation, c.max_payload_bytes),
            (&Effect::Mutation, &Recovery::KeyedIdempotent, false, 65_536),
            "{name}"
        );
        assert_eq!(
            compat["added_commands"][name],
            json!(c.fingerprint().unwrap())
        );
    }
    for name in ADDED_EVENTS {
        let e = &registry.events[name];
        assert_eq!(
            (e.version.as_str(), &e.trusted_producers),
            ("1.0.0", &strings(&["aq.host.bridge@1"]))
        );
        assert_eq!((e.retention_ms, e.max_payload_bytes), (86_400_000, 65_536));
        assert_eq!(
            compat["added_events"][name],
            json!(e.fingerprint().unwrap())
        );
    }
}

#[test]
fn command_deadlines_match_their_delivery_mode() {
    let registry = catalog::catalog_v2_3();
    // Synchronous commands answer within 10 s; asynchronous ones admit within 60 s.
    let asynchronous = [
        "main.sync",
        "branch.publish",
        "pull_request.open",
        "pull_request.await_checks",
        "pull_request.merge",
        "pull_request.close",
    ];
    for name in ADDED_COMMANDS {
        let c = &registry.commands[name];
        assert_eq!(
            c.asynchronous.is_some(),
            asynchronous.contains(&name),
            "{name}"
        );
        assert_eq!(
            c.deadline_ms,
            if asynchronous.contains(&name) {
                60_000
            } else {
                10_000
            },
            "{name}"
        );
    }
}

#[test]
fn result_ready_identifies_the_changed_task() {
    let registry = catalog::catalog_v2_3();
    // The event is level-triggered like `aq.work.route_needed`: no correlation.
    let ready = &registry.events["aq.work.result_ready"];
    assert_eq!(
        (
            &ready.correlation_fields,
            ready.subject_field.as_deref(),
            ready.revision_field.as_deref()
        ),
        (&BTreeSet::new(), Some("task_id"), Some("version"))
    );
    assert_eq!(
        ready.payload,
        object(&[
            ("task_id", text()),
            ("project_id", text()),
            ("attempt_id", text()),
            ("result_id", text()),
            ("version", range(0, i64::MAX)),
        ])
    );
}

#[test]
fn catalog_v2_3_keeps_every_2_2_0_contract_but_work_read_byte_identical() {
    let compat = &fixtures()["compatibility"];
    let v22 = catalog::catalog_v2_2();
    let v22_manifest: Value = serde_json::from_str(catalog::MANIFEST_2_2_JSON).unwrap();
    assert_eq!(
        (&compat["base"]["version"], &compat["base"]["fingerprint"]),
        (&json!("2.2.0"), &v22_manifest["fingerprint"])
    );
    assert_eq!(
        hash_bytes(catalog::REGISTRY_2_2_JSON.as_bytes()),
        compat["base"]["registry_sha256"]
    );
    assert_eq!((v22.commands.len(), v22.events.len()), (16, 11));
    for (field, count) in [
        ("unchanged_commands", 15),
        ("unchanged_events", 11),
        ("changed_commands", 1),
    ] {
        assert_eq!(compat[field].as_object().unwrap().len(), count, "{field}");
    }
}

#[test]
fn previous_contracts_keep_their_bytes_and_fingerprints() {
    let compat = &fixtures()["compatibility"];
    let v22 = catalog::catalog_v2_2();
    let v23 = catalog::catalog_v2_3();
    for (name, old) in &v22.commands {
        let new = &v23.commands[name];
        match compat["unchanged_commands"].get(name) {
            Some(pinned) => {
                assert_eq!(new, old, "{name}");
                assert_eq!(canonical_bytes(new).unwrap(), canonical_bytes(old).unwrap());
                assert_eq!(&json!(old.fingerprint().unwrap()), pinned, "{name}");
            }
            None => {
                assert_eq!(name, "work.read");
                assert_changed_read(&compat["changed_commands"][name], new, old);
            }
        }
    }
    for (name, old) in &v22.events {
        let new = &v23.events[name];
        assert_eq!(new, old, "{name}");
        assert_eq!(
            compat["unchanged_events"][name],
            json!(old.fingerprint().unwrap()),
            "{name}"
        );
    }
}

#[test]
fn additions_match_the_manifest() {
    let v22 = catalog::catalog_v2_2();
    let v23 = catalog::catalog_v2_3();
    let added: BTreeSet<&str> = v23
        .commands
        .keys()
        .filter(|n| !v22.commands.contains_key(*n))
        .map(String::as_str)
        .collect();
    assert_eq!(added, BTreeSet::from(ADDED_COMMANDS));
    let added: BTreeSet<&str> = v23
        .events
        .keys()
        .filter(|n| !v22.events.contains_key(*n))
        .map(String::as_str)
        .collect();
    assert_eq!(added, BTreeSet::from(ADDED_EVENTS));
}

#[test]
fn old_commands_are_published_verbatim() {
    let v22 = catalog::catalog_v2_2();
    // Each old entry other than work.read appears unchanged in the 2.3.0 file.
    for name in v22.commands.keys().filter(|n| *n != "work.read") {
        let entry =
            serde_json::to_string_pretty(&serde_json::to_value(&v22.commands[name]).unwrap())
                .unwrap()
                .replace('\n', "\n    ");
        assert!(
            catalog::REGISTRY_2_3_JSON.contains(&format!("\"{name}\": {entry}")),
            "{name}"
        );
    }
}

fn assert_changed_read(changed: &Value, new: &CommandContract, old: &CommandContract) {
    assert_eq!(
        (changed["from"].as_str(), changed["to"].as_str()),
        (Some(old.version.as_str()), Some(new.version.as_str()))
    );
    assert_eq!(
        (&json!(new.capabilities), &new.capabilities),
        (&changed["capabilities"], &old.capabilities)
    );
    assert_ne!(new.fingerprint().unwrap(), old.fingerprint().unwrap());
    // Only the version and the facts of `observed` change, by
    // adding fields: nothing 2.2.0 said is taken away.
    let (p, r) = object_fields(&new.outcomes["observed"]);
    let (op, or) = object_fields(&old.outcomes["observed"]);
    let (f, fr) = object_fields(&p["facts"]);
    let (of, ofr) = object_fields(&op["facts"]);
    assert_eq!(r, or);
    for field in ["reference", "subject"] {
        assert_eq!(p[field], op[field], "{field}");
    }
    for (field, schema) in of {
        assert_eq!(&f[field], schema, "facts.{field}");
    }
    assert!(ofr.is_subset(fr));
    let added: BTreeSet<&str> = f
        .keys()
        .filter(|k| !of.contains_key(*k))
        .map(String::as_str)
        .collect();
    assert_eq!(
        added,
        result_facts()
            .iter()
            .map(|(n, _)| *n)
            .collect::<BTreeSet<_>>()
    );
    assert_eq!(
        &fr.difference(ofr)
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        &added
    );
    assert_eq!(
        (&new.input, &new.outcomes["refused"], new.deadline_ms),
        (&old.input, &old.outcomes["refused"], old.deadline_ms)
    );
    assert_eq!(
        (&new.effect, &new.recovery),
        (&Effect::Observation, &Recovery::Observation)
    );
}

fn object_fields(schema: &Schema) -> (&BTreeMap<String, Schema>, &BTreeSet<String>) {
    let Schema::Object {
        properties,
        required,
    } = schema
    else {
        panic!("expected object, got {schema:?}")
    };
    (properties, required)
}
