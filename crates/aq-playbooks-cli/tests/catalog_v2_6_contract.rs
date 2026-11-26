//! Host catalog 2.6.0 conformance and generator: the delivery lineage of
//! 2.3.0 with `work.read` 2.6.0, whose facts add the task's
//! `acceptance_criteria` and the `conclusion` of its latest result, so that a
//! delivery playbook can ask a judgment whether a result that changed nothing
//! meets the task.
use aq_playbooks_cli::catalog;
use aq_playbooks_compiler::{
    docs,
    draft::{SourceRecord, Workspace, seal},
};
use aq_playbooks_model::{Registry, Schema, canonical_bytes, digest, hash_bytes};
use aq_playbooks_simulation::{Case, run_case};
use serde::Serialize;
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::PathBuf};

const CHANGED_COMMAND: &str = "work.read";
/// The bound of a task's acceptance criteria in the graph.
const ACCEPTANCE_CRITERIA_BYTES: usize = 8192;
/// The bound of a result report's conclusion.
const CONCLUSION_BYTES: usize = 512;
const FIXTURES: &str = include_str!("../../../contracts/aq-host-2.6.0/fixtures.json");

fn strings(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).into()).collect()
}

fn build_catalog() -> Registry {
    let mut registry = catalog::catalog_v2_3();
    let read = registry.commands.get_mut(CHANGED_COMMAND).unwrap();
    read.version = catalog::VERSION_2_6.into();
    let Some(Schema::Object { properties, .. }) = read.outcomes.get_mut("observed") else {
        unreachable!("observed is an object")
    };
    let Some(Schema::Object {
        properties: facts,
        required,
    }) = properties.get_mut("facts")
    else {
        unreachable!("facts is an object")
    };
    for (name, bound) in [
        ("acceptance_criteria", ACCEPTANCE_CRITERIA_BYTES),
        ("conclusion", CONCLUSION_BYTES),
    ] {
        facts.insert(name.into(), Schema::string(bound));
        required.insert(name.into());
    }
    registry
}

fn notes() -> Value {
    let mut notes: Value = serde_json::from_str(docs::CATALOG_2_3_NOTES).unwrap();
    notes["about"] = json!(
        "What the host says about each contract of catalog 2.6.0, for an authoring agent. Rendered into CATALOG.md for the commands and events a source may use (aq_playbooks_compiler::docs::catalog). An empty string means the catalog does not say. Kept beside registry.json so that the registry, and every fingerprint, stays unchanged. 2.6.0 is catalog 2.3.0 with `work.read` 2.6.0, which adds the task's acceptance criteria and its latest result's conclusion; every other note is the one of 2.3.0."
    );
    let read = &mut notes["commands"][CHANGED_COMMAND];
    read["purpose"] = json!(
        "Reads the facts of a task as it is now, with those of its latest result: whether it changed anything, what its agent concluded, its branch and pull request, and how many times delivery has sent it back."
    );
    read["notes"].as_array_mut().unwrap().push(json!(
        "Two more facts, always present: `acceptance_criteria` (the task's acceptance criteria, at most 8192 bytes, `\"\"` when it has none; `description` is separate and does not repeat them) and `conclusion` (the conclusion the agent reported with the latest result, at most 512 bytes, `\"\"` when the task has no result yet). `conclusion` is the agent's own words, not a verdict: to decide whether a result meets the task, ask `judgment.assess` with both as context items (each fits a context item's `text`), and act on its answer."
    ));
    notes
}

fn manifest(registry: &Registry) -> Value {
    let base: Value = serde_json::from_str(catalog::MANIFEST_2_3_JSON).unwrap();
    json!({
        "version": catalog::VERSION_2_6,
        "fingerprint": digest(&(catalog::VERSION_2_6, registry)).unwrap(),
        "supersedes": {"version": base["version"], "fingerprint": base["fingerprint"]},
        "commands_changed": [CHANGED_COMMAND],
        "context_views": base["context_views"],
        "work_kinds": base["work_kinds"],
        "work_change_variants": base["work_change_variants"]
    })
}

/// A `work.read` answer of 2.6.0 for a task whose result changed nothing.
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
    let base: Value = serde_json::from_str(catalog::MANIFEST_2_3_JSON).unwrap();
    let v23 = catalog::catalog_v2_3();
    let unchanged_commands: serde_json::Map<_, _> = (registry.commands.iter())
        .filter(|(name, _)| name.as_str() != CHANGED_COMMAND)
        .map(|(name, command)| (name.clone(), json!(command.fingerprint().unwrap())))
        .collect();
    let unchanged_events: serde_json::Map<_, _> = (registry.events.iter())
        .map(|(name, event)| (name.clone(), json!(event.fingerprint().unwrap())))
        .collect();
    let read = &registry.commands[CHANGED_COMMAND];
    let mut without = observed();
    for name in ["acceptance_criteria", "conclusion"] {
        without["facts"].as_object_mut().unwrap().remove(name);
    }
    let mut no_result = observed();
    for (name, value) in [
        ("outcome", json!("")),
        ("branch", json!("")),
        ("head", json!("")),
        ("base", json!("")),
        ("conclusion", json!("")),
        ("acceptance_criteria", json!("")),
    ] {
        no_result["facts"][name] = value;
    }
    let mut longest = observed();
    longest["facts"]["conclusion"] = json!("x".repeat(CONCLUSION_BYTES));
    longest["facts"]["acceptance_criteria"] = json!("x".repeat(ACCEPTANCE_CRITERIA_BYTES));
    let mut long_conclusion = observed();
    long_conclusion["facts"]["conclusion"] = json!("x".repeat(CONCLUSION_BYTES + 1));
    let mut long_criteria = observed();
    long_criteria["facts"]["acceptance_criteria"] =
        json!("x".repeat(ACCEPTANCE_CRITERIA_BYTES + 1));
    let case = |value: &Value, valid: bool| json!({"contract": CHANGED_COMMAND, "kind": "output", "outcome": "observed", "valid": valid, "value": value});
    json!({
        "compatibility": {
            "base": {
                "version": base["version"], "fingerprint": base["fingerprint"],
                "registry_sha256": hash_bytes(catalog::REGISTRY_2_3_JSON.as_bytes())
            },
            "unchanged_commands": unchanged_commands,
            "unchanged_events": unchanged_events,
            "changed_commands": {CHANGED_COMMAND: {
                "capabilities": read.capabilities,
                "from": v23.commands[CHANGED_COMMAND].version,
                "to": read.version,
                "fingerprint": read.fingerprint().unwrap()
            }}
        },
        "cases": [
            case(&observed(), true),
            case(&no_result, true),
            case(&longest, true),
            case(&without, false),
            case(&long_conclusion, false),
            case(&long_criteria, false)
        ]
    })
}

fn pretty(value: &impl Serialize) -> String {
    let value = serde_json::to_value(value).unwrap();
    serde_json::to_string_pretty(&value).unwrap() + "\n"
}

#[test]
#[ignore = "writes contracts/aq-host-2.6.0; run after changing the generator"]
fn write_catalog_v2_6() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/aq-host-2.6.0");
    let registry = build_catalog();
    std::fs::write(dir.join("registry.json"), pretty(&registry)).unwrap();
    std::fs::write(dir.join("manifest.json"), pretty(&manifest(&registry))).unwrap();
    std::fs::write(dir.join("fixtures.json"), pretty(&fixtures(&registry))).unwrap();
    std::fs::write(dir.join("notes.json"), pretty(&notes())).unwrap();
}

#[test]
fn generated_catalog_files_are_pinned() {
    let registry = catalog::catalog_v2_6();
    assert_eq!(registry, build_catalog());
    assert_eq!(catalog::REGISTRY_2_6_JSON, pretty(&registry));
    assert_eq!(catalog::MANIFEST_2_6_JSON, pretty(&manifest(&registry)));
    assert_eq!(FIXTURES, pretty(&fixtures(&registry)));
    assert_eq!(docs::CATALOG_2_6_NOTES, pretty(&notes()));
    assert_eq!(docs::CATALOG_2_6_REGISTRY, catalog::REGISTRY_2_6_JSON);

    // Every 2.3.0 contract but work.read is byte-identical.
    let base = catalog::catalog_v2_3();
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

    let rendered = docs::catalog_with_extensions(
        &registry,
        docs::CATALOG_2_6_NOTES,
        &strings(&[CHANGED_COMMAND, "judgment.assess"]),
        &BTreeSet::new(),
        &[],
    )
    .unwrap();
    assert!(rendered.contains("`conclusion` (the conclusion the agent reported"));
    let contract = serde_json::to_value(read).unwrap();
    assert!(docs::published_notes("commands", CHANGED_COMMAND, &contract).is_some());
}

#[test]
fn fixture_values_validate_and_a_2_3_0_answer_does_not() {
    let registry = catalog::catalog_v2_6();
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
    // The bounds are bytes: 256 two-byte characters fit, 257 do not.
    let schema = &registry.commands[CHANGED_COMMAND].outcomes["observed"];
    let mut read = observed();
    read["facts"]["conclusion"] = json!("é".repeat(CONCLUSION_BYTES / 2));
    assert!(schema.validate(&read).is_ok());
    read["facts"]["conclusion"] = json!("é".repeat(CONCLUSION_BYTES / 2 + 1));
    assert!(schema.validate(&read).is_err());
    // A 2.6.0 answer does not fit 2.3.0, so an artifact sealed on 2.3.0 keeps
    // its 2.3.0 work.read.
    let v23 = catalog::catalog_v2_3();
    assert!(
        v23.commands[CHANGED_COMMAND].outcomes["observed"]
            .validate(&observed())
            .is_err()
    );
    // Each fact fits a judgment.assess context item.
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
    let item =
        json!({"label": "Acceptance criteria", "text": "x".repeat(ACCEPTANCE_CRITERIA_BYTES)});
    assert!(items.validate(&item).is_ok());
}

/// A playbook that judges a result which changed nothing: it asks
/// `judgment.assess` whether the conclusion shows the acceptance criteria are
/// met, then completes the task or sends it back. The judgment's `rationale`
/// (at most 4096 bytes) does not fit `work.return`'s `detail` (1024), so the
/// reason given back is the document's sentence.
#[test]
fn a_delivery_playbook_can_judge_an_unchanged_result() {
    let commands = [
        "judgment.assess",
        "work.complete",
        "work.read",
        "work.return",
    ];
    let mut capabilities = strings(&commands);
    capabilities.insert("operation.get".into());
    let artifact = seal(&Workspace {
        markdown: "# Unchanged results\n\nWhen a successful result changed nothing, ask whether it shows the task's acceptance criteria are met. Complete the task if so; otherwise send it back with the reason.\n".into(),
        record: SourceRecord {
            id: "judge-unchanged".into(),
            binding: "aq.judge-unchanged".into(),
            scope: "project".into(),
            purpose: "Judge a result that changed nothing".into(),
            commands: strings(&commands),
            events: strings(&["aq.work.result_ready"]),
            capabilities,
            facts_version: "test-1".into(),
        },
        catalog: catalog::catalog_v2_6(),
        facts: Default::default(),
        draft: serde_json::from_str(include_str!("data/judge-unchanged-draft.json")).unwrap(),
    })
    .unwrap_or_else(|faults| panic!("{faults:#?}"));
    let bytes = canonical_bytes(&artifact).unwrap();
    assert!(aq_playbooks_model::Artifact::from_canonical_bytes(&bytes).is_ok());
    assert_eq!(
        artifact.content.registry.commands[CHANGED_COMMAND].version,
        "2.6.0"
    );

    let read = observed();
    let facts = &read["facts"];
    let subject = &read["subject"];
    let ask = json!({
        "subject": subject,
        "question": "Does the agent's conclusion show that the task's acceptance criteria are met without a change?",
        "context": {"items": [
            {"label": "Acceptance criteria", "text": facts["acceptance_criteria"]},
            {"label": "Conclusion", "text": facts["conclusion"]}
        ]},
        "fields": [],
        "flags": [{"id": "met", "description": "The conclusion shows every acceptance criterion is met."}],
        "rationale_max_bytes": 1024,
        "budget": {"deadline_ms": 180000, "max_output_tokens": 1024}
    });
    let provenance = json!({
        "context_digest": format!("sha256:{}", "a".repeat(64)), "credential_profile": "profile/judge",
        "model": "model-a", "output_digest": null, "prompt_digest": format!("sha256:{}", "c".repeat(64)),
        "provider": "anthropic", "usage": {"cost_microunits": 0, "input_tokens": 400, "output_tokens": 60}
    });
    let target = json!({"task": "task-1", "result_id": "result-1"});
    for (flags, rationale, last) in [
        (
            json!(["met"]),
            "Every command ran.",
            json!({"command": "work.complete", "input": target}),
        ),
        (
            json!([]),
            "The conclusion does not say the commands ran.",
            json!({
                "command": "work.return",
                "input": {"task": "task-1", "result_id": "result-1", "reason": "policy",
                    "detail": "The result changed nothing and its conclusion does not show that the acceptance criteria are met.",
                    "update_from_main": false}
            }),
        ),
    ] {
        let met = flags.as_array().is_some_and(|f| !f.is_empty());
        let case: Case = serde_json::from_value(json!({
            "name": if met { "criteria met" } else { "criteria not met" },
            "situation": "A successful result that changed nothing.",
            "start": {"kind": "event", "event": "aq.work.result_ready", "payload": {
                "task_id": "task-1", "project_id": "project-1", "attempt_id": "attempt-1",
                "result_id": "result-1", "version": 3}},
            "returns": {
                "work.read": [{"outcome": "observed", "output": read}],
                "judgment.assess": [{"outcome": "assessed", "output": {
                    "reference": "judgment:op-1", "subject": subject, "labels": [], "flags": flags,
                    "rationale": rationale, "provenance": provenance}}],
                last["command"].as_str().unwrap(): [{
                    "outcome": if met { "completed" } else { "returned" }, "output": {}}]
            },
            "expect": {"outcome": if met { "delivered" } else { "returned" }, "calls": [
                {"command": "work.read", "input": {"subject": {"id": "task-1", "revision": 3}}},
                {"command": "judgment.assess", "input": ask},
                last
            ]}
        }))
        .unwrap();
        let report = run_case(&artifact, &case).unwrap();
        assert!(report.passed, "{report:#?}");
    }
}
