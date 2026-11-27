//! Host catalog 2.7.0 conformance and generator: the delivery lineage of
//! 2.6.0 with `work.read` 2.7.0, whose `conclusion` fact holds up to 4096
//! bytes (the result report's bound) instead of 512, so that a judgment sees
//! the evidence the agent reported.
use aq_playbooks_cli::catalog;
use aq_playbooks_compiler::docs;
use aq_playbooks_model::{Registry, Schema, digest, hash_bytes};
use serde::Serialize;
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::PathBuf};

const CHANGED_COMMAND: &str = "work.read";
/// The bound of a result report's conclusion (aq-admission 0.1.14).
const CONCLUSION_BYTES: usize = 4096;
/// The bound 2.6.0 gave it.
const CONCLUSION_BYTES_2_6: usize = 512;
const FIXTURES: &str = include_str!("../../../contracts/aq-host-2.7.0/fixtures.json");

fn build_catalog() -> Registry {
    let mut registry = catalog::catalog_v2_6();
    let read = registry.commands.get_mut(CHANGED_COMMAND).unwrap();
    read.version = catalog::VERSION_2_7.into();
    let Some(Schema::Object { properties, .. }) = read.outcomes.get_mut("observed") else {
        unreachable!("observed is an object")
    };
    let Some(Schema::Object {
        properties: facts, ..
    }) = properties.get_mut("facts")
    else {
        unreachable!("facts is an object")
    };
    facts.insert("conclusion".into(), Schema::string(CONCLUSION_BYTES));
    registry
}

fn notes() -> Value {
    let mut notes: Value = serde_json::from_str(docs::CATALOG_2_6_NOTES).unwrap();
    notes["about"] = json!(
        "What the host says about each contract of catalog 2.7.0, for an authoring agent. Rendered into CATALOG.md for the commands and events a source may use (aq_playbooks_compiler::docs::catalog). An empty string means the catalog does not say. Kept beside registry.json so that the registry, and every fingerprint, stays unchanged. 2.7.0 is catalog 2.6.0 with `work.read` 2.7.0, whose `conclusion` holds up to 4096 bytes; every other note is the one of 2.6.0."
    );
    let read = notes["commands"][CHANGED_COMMAND]["notes"]
        .as_array_mut()
        .unwrap();
    let last = read.last_mut().unwrap();
    let text = last.as_str().unwrap();
    let old = "the latest result, at most 512 bytes,";
    assert!(text.contains(old), "the 2.6.0 conclusion note moved");
    *last = text
        .replace(old, "the latest result, at most 4096 bytes,")
        .into();
    notes
}

fn manifest(registry: &Registry) -> Value {
    let base: Value = serde_json::from_str(catalog::MANIFEST_2_6_JSON).unwrap();
    json!({
        "version": catalog::VERSION_2_7,
        "fingerprint": digest(&(catalog::VERSION_2_7, registry)).unwrap(),
        "supersedes": {"version": base["version"], "fingerprint": base["fingerprint"]},
        "commands_changed": [CHANGED_COMMAND],
        "context_views": base["context_views"],
        "work_kinds": base["work_kinds"],
        "work_change_variants": base["work_change_variants"]
    })
}

/// A `work.read` answer of 2.7.0 for a task whose result changed nothing.
fn observed() -> Value {
    json!({
        "reference": "task-1@3", "subject": {"id": "task-1", "revision": 3},
        "facts": {"kind": "", "title": "Verify the install guide", "description": "Check that the guide's commands still work.",
            "origin": "owner", "level_hint": "", "charged_failures": 0, "ended_attempts": 0, "route": "router",
            "acceptance_criteria": "Every command in the guide runs without error.",
            "outcome": "success", "changed": false, "branch": "aq/task-1", "head": "a1", "base": "a1",
            "main_head": "a1", "returns": 0, "pull_request": null,
            "conclusion": "Outcome: done; Changes: none; every command in the guide ran with exit code 0."}
    })
}

fn fixtures(registry: &Registry) -> Value {
    let base: Value = serde_json::from_str(catalog::MANIFEST_2_6_JSON).unwrap();
    let v26 = catalog::catalog_v2_6();
    let unchanged_commands: serde_json::Map<_, _> = (registry.commands.iter())
        .filter(|(name, _)| name.as_str() != CHANGED_COMMAND)
        .map(|(name, command)| (name.clone(), json!(command.fingerprint().unwrap())))
        .collect();
    let unchanged_events: serde_json::Map<_, _> = (registry.events.iter())
        .map(|(name, event)| (name.clone(), json!(event.fingerprint().unwrap())))
        .collect();
    let read = &registry.commands[CHANGED_COMMAND];
    let mut no_result = observed();
    no_result["facts"]["conclusion"] = json!("");
    let mut longest = observed();
    longest["facts"]["conclusion"] = json!("x".repeat(CONCLUSION_BYTES));
    let mut too_long = observed();
    too_long["facts"]["conclusion"] = json!("x".repeat(CONCLUSION_BYTES + 1));
    let case = |value: &Value, valid: bool| json!({"contract": CHANGED_COMMAND, "kind": "output", "outcome": "observed", "valid": valid, "value": value});
    json!({
        "compatibility": {
            "base": {
                "version": base["version"], "fingerprint": base["fingerprint"],
                "registry_sha256": hash_bytes(catalog::REGISTRY_2_6_JSON.as_bytes())
            },
            "unchanged_commands": unchanged_commands,
            "unchanged_events": unchanged_events,
            "changed_commands": {CHANGED_COMMAND: {
                "capabilities": read.capabilities,
                "from": v26.commands[CHANGED_COMMAND].version,
                "to": read.version,
                "fingerprint": read.fingerprint().unwrap()
            }}
        },
        "cases": [
            case(&observed(), true),
            case(&no_result, true),
            case(&longest, true),
            case(&too_long, false)
        ]
    })
}

fn pretty(value: &impl Serialize) -> String {
    let value = serde_json::to_value(value).unwrap();
    serde_json::to_string_pretty(&value).unwrap() + "\n"
}

#[test]
#[ignore = "writes contracts/aq-host-2.7.0; run after changing the generator"]
fn write_catalog_v2_7() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/aq-host-2.7.0");
    let registry = build_catalog();
    std::fs::write(dir.join("registry.json"), pretty(&registry)).unwrap();
    std::fs::write(dir.join("manifest.json"), pretty(&manifest(&registry))).unwrap();
    std::fs::write(dir.join("fixtures.json"), pretty(&fixtures(&registry))).unwrap();
    std::fs::write(dir.join("notes.json"), pretty(&notes())).unwrap();
}

#[test]
fn generated_catalog_files_are_pinned() {
    let registry = catalog::catalog_v2_7();
    assert_eq!(registry, build_catalog());
    assert_eq!(catalog::REGISTRY_2_7_JSON, pretty(&registry));
    assert_eq!(catalog::MANIFEST_2_7_JSON, pretty(&manifest(&registry)));
    assert_eq!(FIXTURES, pretty(&fixtures(&registry)));
    assert_eq!(docs::CATALOG_2_7_NOTES, pretty(&notes()));
    assert_eq!(docs::CATALOG_2_7_REGISTRY, catalog::REGISTRY_2_7_JSON);

    // Every 2.6.0 contract but work.read is byte-identical.
    let base = catalog::catalog_v2_6();
    assert_eq!(
        base.commands.keys().collect::<Vec<_>>(),
        registry.commands.keys().collect::<Vec<_>>()
    );
    for (name, old) in &base.commands {
        assert_eq!(
            old == &registry.commands[name],
            name != CHANGED_COMMAND,
            "{name}"
        );
    }
    assert_eq!(base.events, registry.events);
    let read = &registry.commands[CHANGED_COMMAND];
    assert_eq!(
        read.capabilities,
        base.commands[CHANGED_COMMAND].capabilities
    );

    let commands: BTreeSet<String> = [CHANGED_COMMAND, "judgment.assess"]
        .map(String::from)
        .into();
    let rendered = docs::catalog_with_extensions(
        &registry,
        docs::CATALOG_2_7_NOTES,
        &commands,
        &BTreeSet::new(),
        &[],
    )
    .unwrap();
    assert!(rendered.contains("the latest result, at most 4096 bytes,"));
    let contract = serde_json::to_value(read).unwrap();
    assert!(docs::published_notes("commands", CHANGED_COMMAND, &contract).is_some());
}

#[test]
fn fixture_values_validate_and_a_long_conclusion_does_not_fit_2_6_0() {
    let registry = catalog::catalog_v2_7();
    let fixtures: Value = serde_json::from_str(FIXTURES).unwrap();
    for fixture in fixtures["cases"].as_array().unwrap() {
        let contract = &registry.commands[fixture["contract"].as_str().unwrap()];
        let schema = &contract.outcomes[fixture["outcome"].as_str().unwrap()];
        assert_eq!(
            schema.validate(&fixture["value"]).is_ok(),
            fixture["valid"].as_bool().unwrap(),
            "{fixture}"
        );
    }
    // The bound is bytes: 2048 two-byte characters fit, 2049 do not.
    let schema = &registry.commands[CHANGED_COMMAND].outcomes["observed"];
    let mut read = observed();
    read["facts"]["conclusion"] = json!("é".repeat(CONCLUSION_BYTES / 2));
    assert!(schema.validate(&read).is_ok());
    read["facts"]["conclusion"] = json!("é".repeat(CONCLUSION_BYTES / 2 + 1));
    assert!(schema.validate(&read).is_err());
    // A conclusion over 2.6.0's bound does not fit 2.6.0, so an artifact
    // sealed on 2.6.0 keeps its 2.6.0 work.read.
    let v26 = &catalog::catalog_v2_6().commands[CHANGED_COMMAND].outcomes["observed"];
    assert!(v26.validate(&observed()).is_ok());
    read["facts"]["conclusion"] = json!("x".repeat(CONCLUSION_BYTES_2_6 + 1));
    assert!(schema.validate(&read).is_ok() && v26.validate(&read).is_err());
    // The longest conclusion fits a judgment.assess context item.
    let assess = &registry.commands["judgment.assess"].input;
    let Schema::Object { properties, .. } = assess else {
        unreachable!()
    };
    let Schema::Object { properties, .. } = &properties["context"] else {
        unreachable!()
    };
    let Schema::Array { items, .. } = &properties["items"] else {
        unreachable!()
    };
    let item = json!({"label": "Conclusion", "text": "x".repeat(CONCLUSION_BYTES)});
    assert!(items.validate(&item).is_ok());
}
