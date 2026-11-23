use super::*;

#[test]
fn catalog_v2_2_commits_the_generated_registry() {
    let registry: Registry = serde_json::from_str(catalog::REGISTRY_2_2_JSON).unwrap();
    assert_eq!(registry, catalog::catalog_v2_2());
    // The committed bytes are exactly what the generator writes.
    assert_eq!(registry, build_v2_2());
    assert_eq!(catalog::REGISTRY_2_2_JSON, pretty(&registry));
}

#[test]
fn manifest_pins_the_registry_and_its_predecessor() {
    let registry = catalog::catalog_v2_2();
    let manifest: Value = serde_json::from_str(catalog::MANIFEST_2_2_JSON).unwrap();
    let v21_manifest: Value = serde_json::from_str(catalog::MANIFEST_2_1_JSON).unwrap();
    assert_eq!(
        (catalog::VERSION_2_2, manifest["version"].as_str()),
        ("2.2.0", Some(catalog::VERSION_2_2))
    );
    assert_eq!(
        manifest["fingerprint"],
        digest(&(catalog::VERSION_2_2, &registry)).unwrap()
    );
    assert_ne!(manifest["fingerprint"], v21_manifest["fingerprint"]);
    assert_eq!(
        (
            &manifest["supersedes"]["version"],
            &manifest["supersedes"]["fingerprint"]
        ),
        (&json!("2.1.0"), &v21_manifest["fingerprint"])
    );
}

#[test]
fn manifest_lists_the_additions_and_preserves_context_metadata() {
    let manifest: Value = serde_json::from_str(catalog::MANIFEST_2_2_JSON).unwrap();
    let v21_manifest: Value = serde_json::from_str(catalog::MANIFEST_2_1_JSON).unwrap();
    assert_eq!(
        (&manifest["commands_added"], &manifest["events_added"]),
        (&json!(ADDED_COMMANDS), &json!(ADDED_EVENTS))
    );
    for key in ["context_views", "work_kinds", "work_change_variants"] {
        assert_eq!(manifest[key], v21_manifest[key], "{key}");
    }
}

#[test]
fn every_published_schema_is_well_formed() {
    let registry = catalog::catalog_v2_2();
    assert_eq!(registry.commands.len(), 16);
    assert_eq!(registry.events.len(), 11);
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
fn added_contracts_match_the_advertised_identities() {
    let registry = catalog::catalog_v2_2();
    let compat = &fixtures()["compatibility"];
    for name in ADDED_COMMANDS {
        let c = &registry.commands[name];
        assert_eq!(
            (c.version.as_str(), &c.capabilities, &c.scopes),
            ("1.0.0", &strings(&[name]), &strings(&["project"])),
            "{name}"
        );
        assert_eq!(
            (c.max_payload_bytes, c.cancellation),
            (65_536, false),
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
fn work_commands_declare_their_effects_and_public_results() {
    let registry = catalog::catalog_v2_2();
    let read = &registry.commands["work.read"];
    assert_eq!(
        (&read.effect, &read.recovery, read.deadline_ms),
        (&Effect::Observation, &Recovery::Observation, 10_000)
    );
    let route = &registry.commands["work.route"];
    assert_eq!(
        (&route.effect, &route.recovery, route.deadline_ms),
        (&Effect::Mutation, &Recovery::KeyedIdempotent, 10_000)
    );
    let unroutable = &registry.commands["work.unroutable"];
    assert_eq!(
        (
            &unroutable.effect,
            &unroutable.recovery,
            unroutable.deadline_ms
        ),
        (&Effect::Mutation, &Recovery::KeyedIdempotent, 10_000)
    );
    for c in [read, route, unroutable] {
        assert_eq!(c.public_result_fields, strings(&["reference", "subject"]));
    }
}

#[test]
fn unroutable_uses_the_same_reasons_as_route() {
    let registry = catalog::catalog_v2_2();
    let route = &registry.commands["work.route"];
    let unroutable = &registry.commands["work.unroutable"];
    // An unroutable task gives its reasons exactly as a route does.
    let Schema::Object {
        properties: unroutable_input,
        ..
    } = &unroutable.input
    else {
        panic!("work.unroutable input is an object")
    };
    let Schema::Object {
        properties: route_input,
        ..
    } = &route.input
    else {
        panic!("work.route input is an object")
    };
    assert_eq!(unroutable_input["reasons"], route_input["reasons"]);
}

#[test]
fn judgment_completions_and_route_events_declare_their_identities() {
    let registry = catalog::catalog_v2_2();
    let assess = &registry.commands["judgment.assess"];
    let a = assess.asynchronous.as_ref().unwrap();
    assert_eq!(
        (
            a.completion_event.as_str(),
            a.correlation_field.as_str(),
            a.reconciliation_command.as_str()
        ),
        ("aq.judgment.assessed", "operation", "operation.get")
    );
    let needed = &registry.events["aq.work.route_needed"];
    assert_eq!(
        (
            &needed.correlation_fields,
            needed.subject_field.as_deref(),
            needed.revision_field.as_deref()
        ),
        (
            &BTreeSet::new(),
            Some("subject.id"),
            Some("subject.revision")
        )
    );
    let assessed = &registry.events["aq.judgment.assessed"];
    assert_eq!(
        (
            &assessed.correlation_fields,
            assessed.subject_field.as_deref(),
            assessed.revision_field.as_deref()
        ),
        (
            &strings(&["operation"]),
            Some("output.subject.id"),
            Some("output.subject.revision")
        )
    );
}

#[test]
fn route_classification_preserves_assessed_answers() {
    let registry = catalog::catalog_v2_2();
    let route = &registry.commands["work.route"];
    let assess = &registry.commands["judgment.assess"];
    // A route's classification is exactly what an assessed answer holds.
    let Schema::Object { properties, .. } = &route.input else {
        panic!("work.route input is an object")
    };
    let Schema::Nullable { inner } = &properties["classification"] else {
        panic!("classification is nullable")
    };
    let Schema::Object {
        properties: answer, ..
    } = &assess.outcomes["assessed"]
    else {
        panic!("assessed is an object")
    };
    let Schema::Object {
        properties: kept, ..
    } = inner.as_ref()
    else {
        panic!("classification is an object")
    };
    for field in ["labels", "flags", "rationale"] {
        assert_eq!(kept[field], answer[field], "{field}");
    }
    // ...plus the judgment's reference and the field ids, to show it alone.
    assert_eq!(kept["reference"], answer["reference"]);
    assert_eq!(kept["fields"], array(string(64), 16));
}

#[test]
fn work_facts_expose_the_closed_levels() {
    let registry = catalog::catalog_v2_2();
    let route = &registry.commands["work.route"];
    let Schema::Object { properties, .. } = &route.input else {
        panic!("work.route input is an object")
    };
    let read = &registry.commands["work.read"];
    // Levels are the closed set of classes; no hint is "".
    assert_eq!(properties["level"], level(&[]));
    let Schema::Object {
        properties: observed,
        ..
    } = &read.outcomes["observed"]
    else {
        panic!("observed is an object")
    };
    let Schema::Object {
        properties: facts,
        required,
    } = &observed["facts"]
    else {
        panic!("facts is an object")
    };
    assert_eq!(facts["level_hint"], level(&[""]));
    assert!(required.contains("level_hint"));
    assert!(!facts.contains_key("priority"));
}

#[test]
fn route_reasons_admit_the_documented_codes() {
    let registry = catalog::catalog_v2_2();
    let route = &registry.commands["work.route"];
    let Schema::Object { properties, .. } = &route.input else {
        panic!("work.route input is an object")
    };
    // A reason's sentence is optional; no code pins "nothing allowed".
    let Schema::Array { items, .. } = &properties["reasons"] else {
        panic!("reasons is an array")
    };
    let Schema::Object {
        properties: reason,
        required,
    } = items.as_ref()
    else {
        panic!("a reason is an object")
    };
    assert_eq!(required, &strings(&["code"]));
    assert_eq!(reason["detail"], string(256));
    assert_eq!(reason["code"], code(&ROUTE_REASONS));
    for c in ["policy", "risk_relaxed", "deep_reserved", "second_opinion"] {
        assert!(ROUTE_REASONS.contains(&c), "{c}");
    }
    assert!(!ROUTE_REASONS.contains(&"nothing_allowed"));
}

#[test]
fn routed_returns_the_remaining_offers() {
    let registry = catalog::catalog_v2_2();
    let route = &registry.commands["work.route"];
    let Schema::Object { properties, .. } = &route.input else {
        panic!("work.route input is an object")
    };
    // `routed` reports what remained of each list at the level.
    let Schema::Object {
        properties: remained,
        ..
    } = &route.outcomes["routed"]
    else {
        panic!("routed is an object")
    };
    for list in ["harnesses", "providers", "prefer"] {
        assert_eq!(remained[list], properties[list], "{list}");
        assert_eq!(
            properties[list],
            array(string(64), MAX_ROUTE_NAMES),
            "{list}"
        );
    }
}
#[test]
fn catalog_v2_2_keeps_every_2_1_0_contract_byte_identical() {
    let compat = &fixtures()["compatibility"];
    let v21 = catalog::catalog_v2_1();

    let v21_manifest: Value = serde_json::from_str(catalog::MANIFEST_2_1_JSON).unwrap();
    assert_eq!(
        (&compat["base"]["version"], &compat["base"]["fingerprint"]),
        (&json!("2.1.0"), &v21_manifest["fingerprint"])
    );
    assert_eq!(
        hash_bytes(catalog::REGISTRY_2_1_JSON.as_bytes()),
        compat["base"]["registry_sha256"]
    );
    assert_eq!(compat["changed_commands"], json!({}));
    assert_eq!((v21.commands.len(), v21.events.len()), (12, 9));
    assert_eq!(
        (
            compat["unchanged_commands"].as_object().unwrap().len(),
            compat["unchanged_events"].as_object().unwrap().len()
        ),
        (12, 9)
    );
}

#[test]
fn prior_commands_and_events_match_the_published_fingerprints() {
    let compat = &fixtures()["compatibility"];
    let v21 = catalog::catalog_v2_1();
    let v22 = catalog::catalog_v2_2();
    for (name, old) in &v21.commands {
        let new = &v22.commands[name];
        assert_eq!(
            (new, canonical_bytes(new).unwrap()),
            (old, canonical_bytes(old).unwrap()),
            "{name}"
        );
        assert_eq!(
            new.fingerprint().unwrap(),
            old.fingerprint().unwrap(),
            "{name}"
        );
        assert_eq!(
            compat["unchanged_commands"][name],
            json!(old.fingerprint().unwrap()),
            "{name}"
        );
    }
    for (name, old) in &v21.events {
        let new = &v22.events[name];
        assert_eq!(new, old, "{name}");
        assert_eq!(
            new.fingerprint().unwrap(),
            old.fingerprint().unwrap(),
            "{name}"
        );
        assert_eq!(
            compat["unchanged_events"][name],
            json!(old.fingerprint().unwrap()),
            "{name}"
        );
    }
}

#[test]
fn catalog_additions_are_exactly_the_documented_sets() {
    let v21 = catalog::catalog_v2_1();
    let v22 = catalog::catalog_v2_2();
    // Everything else is an addition, named in the manifest and fixtures.
    let added: BTreeSet<&str> = v22
        .commands
        .keys()
        .filter(|n| !v21.commands.contains_key(*n))
        .map(String::as_str)
        .collect();
    assert_eq!(added, BTreeSet::from(ADDED_COMMANDS));
    let added: BTreeSet<&str> = v22
        .events
        .keys()
        .filter(|n| !v21.events.contains_key(*n))
        .map(String::as_str)
        .collect();
    assert_eq!(added, BTreeSet::from(ADDED_EVENTS));
}

#[test]
fn the_published_file_contains_the_previous_command_text() {
    let v21 = catalog::catalog_v2_1();
    // Each old entry's published text appears unchanged in the 2.2.0 file.
    for name in v21.commands.keys() {
        let entry =
            serde_json::to_string_pretty(&serde_json::to_value(&v21.commands[name]).unwrap())
                .unwrap()
                .replace('\n', "\n    ");
        assert!(
            catalog::REGISTRY_2_2_JSON.contains(&format!("\"{name}\": {entry}")),
            "{name}"
        );
    }
}
