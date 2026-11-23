//! Host catalog 2.3.0 conformance (Epic 0, delivery by playbook): 2.2.0 plus
//! the delivery event `aq.work.result_ready`, the delivery commands, their
//! completion event `aq.delivery.completed`, and `work.read` extended with
//! the facts of a task's result.
#[allow(dead_code)]
#[path = "support/schemas.rs"]
mod schemas;
use aq_playbooks_cli::catalog;
use aq_playbooks_compiler::docs;
use aq_playbooks_compiler::draft::{SourceRecord, Workspace, check, seal};
use aq_playbooks_model::*;
use schemas::{enumeration, object, strings, text};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

const ADDED_COMMANDS: [&str; 10] = [
    "branch.publish",
    "main.sync",
    "pull_request.await_checks",
    "pull_request.close",
    "pull_request.merge",
    "pull_request.open",
    "supervisor.notify",
    "work.complete",
    "work.hold",
    "work.return",
];
const ADDED_EVENTS: [&str; 2] = ["aq.delivery.completed", "aq.work.result_ready"];
const CHANGED_COMMANDS: [&str; 1] = ["work.read"];
/// The codes a task is sent back or held for. A playbook cannot build text, so
/// the reason is a code and the sentence a fact value.
const DELIVERY_REASONS: [&str; 8] = [
    "checks_failed",
    "checks_timed_out",
    "merge_conflict",
    "not_mergeable",
    "policy",
    "publish_conflict",
    "pull_request_closed",
    "returns_exhausted",
];
const MERGE_METHODS: [&str; 3] = ["merge", "rebase", "squash"];
const EXAMPLE: &str = "deliver-by-playbook";

fn fixtures() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/aq-host-2.3.0/fixtures.json"
    ))
    .unwrap()
}
fn contracts_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/aq-host-2.3.0")
}

// ---- The generator: 2.2.0 plus the delivery contracts. ----

#[test]
#[ignore = "writes contracts/aq-host-2.3.0; run by hand after changing the generator"]
fn write_catalog_v2_3() {
    let registry = build_v2_3();
    let dir = contracts_dir();
    let v22 = catalog::catalog_v2_2();
    let v22_manifest: Value = serde_json::from_str(catalog::MANIFEST_2_2_JSON).unwrap();
    let manifest = json!({
        "version": "2.3.0",
        "fingerprint": digest(&("2.3.0", &registry)).unwrap(),
        "supersedes": {"version": "2.2.0", "fingerprint": v22_manifest["fingerprint"]},
        "commands_added": ADDED_COMMANDS,
        "commands_changed": CHANGED_COMMANDS,
        "events_added": ADDED_EVENTS,
        "context_views": v22_manifest["context_views"],
        "work_kinds": v22_manifest["work_kinds"],
        "work_change_variants": v22_manifest["work_change_variants"],
    });
    let added = |n: &str| ADDED_COMMANDS.contains(&n) || ADDED_EVENTS.contains(&n);
    let unchanged = |n: &str| !added(n) && !CHANGED_COMMANDS.contains(&n);
    let command_fp = |c: &CommandContract| c.fingerprint().unwrap();
    let event_fp = |e: &EventContract| e.fingerprint().unwrap();
    let mut fixtures = fixtures();
    fixtures["compatibility"] = json!({
        "base": {
            "version": "2.2.0",
            "fingerprint": v22_manifest["fingerprint"],
            "registry_sha256": hash_bytes(catalog::REGISTRY_2_2_JSON.as_bytes()),
        },
        "unchanged_commands": fingerprints(registry.commands.iter(), unchanged, command_fp),
        "unchanged_events": fingerprints(registry.events.iter(), unchanged, event_fp),
        "changed_commands": CHANGED_COMMANDS.iter().map(|n| {
            (n.to_string(), json!({
                "capabilities": v22.commands[*n].capabilities,
                "from": v22.commands[*n].version,
                "to": registry.commands[*n].version,
            }))
        }).collect::<serde_json::Map<_, _>>(),
        "added_commands": fingerprints(registry.commands.iter(), added, command_fp),
        "added_events": fingerprints(registry.events.iter(), added, event_fp),
    });
    std::fs::write(dir.join("registry.json"), pretty(&registry)).unwrap();
    std::fs::write(dir.join("manifest.json"), pretty(&manifest)).unwrap();
    std::fs::write(dir.join("fixtures.json"), pretty(&fixtures)).unwrap();
}

// ---- Conformance. ----

#[test]
fn every_async_delivery_completion_is_assignable_to_its_event() {
    let v23 = catalog::catalog_v2_3();
    let mut seen = 0;
    for name in ADDED_COMMANDS {
        let c = &v23.commands[name];
        let Some(a) = &c.asynchronous else { continue };
        seen += 1;
        assert_eq!(a.completion_event, "aq.delivery.completed", "{name}");
        assert_eq!(a.correlation_field, "operation");
        assert_eq!(a.reconciliation_command, "operation.get");
        let event = &v23.events[&a.completion_event];
        assert!(a.completion.assignable_to(&event.payload), "{name}");
        assert!(event.correlation_fields.contains(&a.correlation_field));
        // Each completion variant is exactly the business outcome of that name.
        let Schema::Object { properties, .. } = &a.completion else {
            panic!("completion is an object")
        };
        let Schema::Union { variants } = &properties["output"] else {
            panic!("output is a union")
        };
        let Schema::String { values, .. } = &properties["outcome"] else {
            panic!("outcome is an enumeration")
        };
        let business: Vec<&String> = c.outcomes.keys().filter(|n| *n != "accepted").collect();
        assert_eq!(values.iter().collect::<Vec<_>>(), business, "{name}");
        assert_eq!(variants.len(), business.len(), "{name}");
        for variant in variants {
            assert!(c.outcomes.values().any(|o| o == variant), "{name}");
        }
    }
    assert_eq!(seen, 6);
    // The structure check of an artifact finds no async_contract issue.
    let sealed = seal(&delivery_workspace()).unwrap_or_else(|d| panic!("{d:#?}"));
    assert_eq!(sealed.content.validate_structure(), vec![]);
}

#[test]
fn valid_and_invalid_fixture_cases_validate_as_declared() {
    let registry = catalog::catalog_v2_3();
    let fixtures = fixtures();
    let covered = schemas::assert_fixture_cases(&registry, &fixtures);
    // A valid case for every input and outcome of an added command, the
    // observed outcome of work.read, and every added event.
    let mut expected = BTreeSet::new();
    for name in ADDED_COMMANDS {
        expected.insert(format!("{name}.input"));
        for outcome in registry.commands[name].outcomes.keys() {
            expected.insert(format!("{name}.{outcome}"));
        }
    }
    for name in ADDED_EVENTS {
        expected.insert(name.to_string());
    }
    let covered_new: BTreeSet<String> = covered
        .iter()
        .filter(|c| !c.starts_with("work.route") && !c.starts_with("judgment.assess"))
        .filter(|c| !c.starts_with("work.unroutable") && !c.starts_with("aq.judgment"))
        .filter(|c| !c.starts_with("aq.work.route_needed") && !c.starts_with("work.read"))
        .cloned()
        .collect();
    assert_eq!(covered_new, expected);
    for place in ["work.read.input", "work.read.observed", "work.read.refused"] {
        assert!(covered.contains(place), "{place}");
    }
}

// ---- Notes: what the authoring agent reads. ----

#[test]
fn the_notes_cover_every_added_or_changed_contract_and_render() {
    let registry = catalog::catalog_v2_3();
    let all: Value = serde_json::from_str(docs::CATALOG_2_3_NOTES).unwrap();
    let mut commands: BTreeSet<String> = ADDED_COMMANDS.iter().map(|s| s.to_string()).collect();
    commands.extend(CHANGED_COMMANDS.iter().map(|s| s.to_string()));
    let events: BTreeSet<String> = ADDED_EVENTS.iter().map(|s| s.to_string()).collect();
    // The notes cover every contract of the catalog, exactly.
    assert_eq!(
        all["commands"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>(),
        registry.commands.keys().cloned().collect::<BTreeSet<_>>()
    );
    assert_eq!(
        all["events"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>(),
        registry.events.keys().cloned().collect::<BTreeSet<_>>()
    );
    // Every added or changed contract is precisely described: a purpose, every
    // input field, every outcome and every payload field say something.
    for name in &commands {
        let notes = &all["commands"][name];
        assert!(
            !notes["purpose"].as_str().unwrap().trim().is_empty(),
            "{name}"
        );
        assert_field_notes(name, &notes["input"]);
        assert_field_notes(name, &notes["outcomes"]);
    }
    for name in &events {
        let notes = &all["events"][name];
        assert!(
            !notes["purpose"].as_str().unwrap().trim().is_empty(),
            "{name}"
        );
        assert!(!notes["sent"].as_str().unwrap().trim().is_empty(), "{name}");
        assert_field_notes(name, &notes["payload"]);
    }
    // The renderer accepts them, and the rendered file names every contract.
    let rendered = docs::catalog_2_3(&commands, &events).unwrap_or_else(|e| panic!("{e}"));
    for name in commands.iter().chain(&events) {
        assert!(rendered.contains(&format!("### `{name}`")), "{name}");
    }
    // The notes on `work.read` describe the result facts.
    for fact in result_facts().iter().map(|(n, _)| *n) {
        assert!(rendered.contains(&format!("`{fact}`")), "{fact}");
    }
}

// ---- Feasibility: the example delivery playbook. ----

fn example_file(name: &str) -> String {
    std::fs::read_to_string(contracts_dir().join("examples").join(EXAMPLE).join(name))
        .unwrap_or_else(|e| panic!("{EXAMPLE}/{name}: {e}"))
}
fn example_json(name: &str) -> Value {
    serde_json::from_str(&example_file(name)).unwrap_or_else(|e| panic!("{EXAMPLE}/{name}: {e}"))
}
fn delivery_workspace() -> Workspace {
    let record: SourceRecord = serde_json::from_value(example_json("record.json")).unwrap();
    let (catalog, _) = aq_playbooks_compiler::draft::retain_registry(
        &catalog::catalog_v2_3(),
        &record.commands,
        &record.events,
    )
    .unwrap();
    let facts: BTreeMap<String, Value> =
        serde_json::from_value(example_json("facts.json")).unwrap();
    Workspace {
        markdown: example_file("source.md"),
        record,
        catalog,
        facts,
        draft: example_json("draft.json"),
    }
}

#[test]
fn the_example_delivery_playbook_checks_and_seals_against_catalog_2_3_0() {
    let ws = delivery_workspace();
    let diagnostics = check(&ws);
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    let sealed = seal(&ws).unwrap_or_else(|d| panic!("{d:#?}"));
    assert!(sealed.verify().is_ok());
    // The catalog file the example ships is the one the host places.
    assert_eq!(
        example_json("catalog.json"),
        serde_json::to_value(&ws.catalog).unwrap()
    );
    // It uses the whole delivery path.
    let used: BTreeSet<&str> = ws.draft["steps"]
        .as_object()
        .unwrap()
        .values()
        .filter_map(|s| s["op"]["command"].as_str())
        .collect();
    for name in [
        "work.read",
        "branch.publish",
        "pull_request.open",
        "pull_request.await_checks",
        "pull_request.merge",
        "work.complete",
        "work.return",
        "work.hold",
        "supervisor.notify",
    ] {
        assert!(used.contains(name), "{name}");
    }
    assert_eq!(ws.draft["triggers"][0]["event"], "aq.work.result_ready");
    // Sending a task back is decided by `lt` on the `returns` fact.
    let decide = &ws.draft["steps"]["may_return"]["op"]["predicate"];
    assert_eq!(decide["op"], "lt");
    assert_eq!(decide["left"]["path"], json!(["facts", "returns"]));
    // The strict rules still bite: a fact the catalog does not have is refused.
    let mut wrong = ws.clone();
    wrong.draft["steps"]["may_return"]["op"]["predicate"]["left"]["path"] =
        json!(["facts", "returned"]);
    assert!(!check(&wrong).is_empty());
    // So is a return reason that is not a code.
    let mut wrong = ws;
    wrong.draft["steps"]["send_back"]["op"]["input"]["fields"]["reason"] =
        json!({"op": "literal", "value": "because_i_said_so"});
    let diagnostics = check(&wrong);
    assert!(
        diagnostics.iter().any(|d| d.code == "schema_compatibility"),
        "{diagnostics:#?}"
    );
}

#[test]
fn work_read_observed_with_result_facts_is_valid_and_a_2_2_0_answer_is_not() {
    let v23 = catalog::catalog_v2_3();
    let v22 = catalog::catalog_v2_2();
    let facts = json!({"kind": "feature", "title": "A task", "description": "", "origin": "owner",
        "level_hint": "", "charged_failures": 0, "ended_attempts": 0, "route": "router",
        "outcome": "completed", "changed": true, "branch": "aq/task-1", "head": "a1", "base": "b2",
        "main_head": "c3", "returns": 1,
        "pull_request": {"number": 7, "url": "https://example.test/pull/7", "state": "open",
            "mergeable": "mergeable", "checks": {"state": "failed", "failed": ["build"]}}});
    let read = json!({"reference": "task-1@3", "subject": {"id": "task-1", "revision": 3}, "facts": facts});
    v23.commands["work.read"].outcomes["observed"]
        .validate(&read)
        .unwrap();
    let mut no_pr = read.clone();
    no_pr["facts"]["pull_request"] = Value::Null;
    v23.commands["work.read"].outcomes["observed"]
        .validate(&no_pr)
        .unwrap();
    let mut old = read;
    for (name, _) in result_facts() {
        old["facts"].as_object_mut().unwrap().remove(name);
    }
    assert!(
        v23.commands["work.read"].outcomes["observed"]
            .validate(&old)
            .is_err()
    );
    v22.commands["work.read"].outcomes["observed"]
        .validate(&old)
        .unwrap();
}

#[path = "support/catalog_fixtures.rs"]
mod catalog_fixtures;
use catalog_fixtures::{fingerprints, pretty};

#[path = "catalog_v2_3/registry.rs"]
mod registry;
use registry::{build_v2_3, range, result_facts};

#[path = "catalog_v2_3/manifest.rs"]
mod manifest;

fn assert_field_notes(name: &str, notes: &Value) {
    for (field, text) in notes.as_object().unwrap() {
        assert!(!text.as_str().unwrap().trim().is_empty(), "{name}.{field}");
    }
}
