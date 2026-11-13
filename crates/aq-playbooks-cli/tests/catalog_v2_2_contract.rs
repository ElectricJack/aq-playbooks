//! Host catalog 2.2.0 conformance: 2.1.0 byte-identical, plus task facts
//! (`work.read`), routes (`work.route`), structured judgments
//! (`judgment.assess`, `aq.judgment.assessed`) and `aq.work.route_needed`.
use aq_playbooks_cli::catalog::{self, enumeration, object, result, strings, subject, text};
use aq_playbooks_compiler::draft::{SourceRecord, Workspace, check, seal};
use aq_playbooks_model::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

const ADDED_COMMANDS: [&str; 3] = ["judgment.assess", "work.read", "work.route"];
const ADDED_EVENTS: [&str; 2] = ["aq.judgment.assessed", "aq.work.route_needed"];
/// The fixed reasons a route may give. A playbook cannot build text, so a
/// reason is a code plus, optionally, a literal sentence; `policy` is the
/// catch-all whose sentence is the document's own.
const ROUTE_REASONS: [&str; 16] = [
    "classification_failed",
    "deep_reserved",
    "escalated_after_failure",
    "harness_excluded",
    "harness_preferred",
    "harnesses_limited",
    "held_for_harness",
    "hint_capped",
    "hint_respected",
    "level_from_kind",
    "level_lowered",
    "policy",
    "raised_for_risk",
    "risk_assumed",
    "risk_relaxed",
    "second_opinion",
];
/// Every syntactically valid level: parent x effort (`aq_routing::Class`).
/// Whether Settings offers one is the host's check when a route is written.
const LEVELS: [&str; 16] = [
    "simple-low",
    "simple-medium",
    "simple-high",
    "simple-max",
    "standard-low",
    "standard-medium",
    "standard-high",
    "standard-max",
    "complex-low",
    "complex-medium",
    "complex-high",
    "complex-max",
    "deep-low",
    "deep-medium",
    "deep-high",
    "deep-max",
];
/// Harness and provider lists in a route: short enough for the 2 KiB route
/// the host keeps on the task.
const MAX_ROUTE_NAMES: usize = 8;

fn fixtures() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/aq-host-2.2.0/fixtures.json"
    ))
    .unwrap()
}
fn contracts_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/aq-host-2.2.0")
}

// ---- The generator: 2.1.0 plus the additive contracts. ----

fn string(max_length: usize) -> Schema {
    Schema::string(max_length)
}
fn code(values: &[&str]) -> Schema {
    Schema::String {
        max_length: 64,
        values: strings(values),
    }
}
fn array(items: Schema, max_items: usize) -> Schema {
    Schema::Array {
        items: Box::new(items),
        max_items,
    }
}
fn range(minimum: i64, maximum: i64) -> Schema {
    Schema::Integer { minimum, maximum }
}
/// `reference` and `subject`, then `extra`.
fn outcome(extra: &[(&str, Schema)]) -> Schema {
    let Schema::Object {
        mut properties,
        mut required,
    } = result()
    else {
        unreachable!("result() is an object")
    };
    for (name, schema) in extra {
        properties.insert((*name).into(), schema.clone());
        required.insert((*name).into());
    }
    Schema::Object {
        properties,
        required,
    }
}
fn level(extra: &[&str]) -> Schema {
    Schema::String {
        max_length: 64,
        values: LEVELS.iter().chain(extra).map(|s| (*s).into()).collect(),
    }
}
fn without_required(schema: Schema, optional: &[&str]) -> Schema {
    let Schema::Object {
        properties,
        mut required,
    } = schema
    else {
        unreachable!("an object")
    };
    for name in optional {
        required.remove(*name);
    }
    Schema::Object {
        properties,
        required,
    }
}
struct Shape {
    effect: Effect,
    recovery: Recovery,
    deadline_ms: u64,
    public: &'static [&'static str],
}
fn command(
    name: &str,
    input: Schema,
    outcomes: &[(&str, Schema)],
    shape: Shape,
    asynchronous: Option<AsyncContract>,
) -> CommandContract {
    CommandContract {
        name: name.into(),
        version: "1.0.0".into(),
        input,
        outcomes: outcomes
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect(),
        capabilities: strings(&[name]),
        scopes: strings(&["project"]),
        effect: shape.effect,
        recovery: shape.recovery,
        asynchronous,
        cancellation: false,
        public_result_fields: strings(shape.public),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 65_536,
        deadline_ms: shape.deadline_ms,
    }
}
fn event(name: &str, payload: Schema, correlation: &[&str], subject_path: &str) -> EventContract {
    EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: name.into(),
        version: "1.0.0".into(),
        payload,
        trusted_producers: strings(&["aq.host.bridge@1"]),
        scopes: strings(&["project"]),
        correlation_fields: strings(correlation),
        subject_field: Some(format!("{subject_path}.id")),
        revision_field: Some(format!("{subject_path}.revision")),
        max_payload_bytes: 65_536,
        retention_ms: 86_400_000,
    }
}

fn work_read() -> CommandContract {
    let facts = object(&[
        ("kind", string(64)),
        ("title", string(512)),
        ("description", string(8192)),
        (
            "origin",
            enumeration(&["owner", "agent", "supervisor", "playbook", "unknown"]),
        ),
        // `""` when none was asked for.
        ("level_hint", level(&[""])),
        ("charged_failures", range(0, 1000)),
        ("ended_attempts", range(0, 1000)),
        ("route", enumeration(&["none", "router", "owner"])),
    ]);
    command(
        "work.read",
        object(&[("subject", subject())]),
        &[
            ("observed", outcome(&[("facts", facts)])),
            (
                "refused",
                outcome(&[("reason", enumeration(&["unknown_task", "not_on_router"]))]),
            ),
        ],
        Shape {
            effect: Effect::Observation,
            recovery: Recovery::Observation,
            deadline_ms: 10_000,
            public: &["reference", "subject"],
        },
        None,
    )
}
fn classification() -> Schema {
    object(&[
        ("reference", text()),
        ("fields", array(string(64), 16)),
        ("labels", array(string(128), 16)),
        ("flags", array(string(64), 32)),
        ("rationale", string(4096)),
    ])
}
fn work_route() -> CommandContract {
    let names = || array(string(64), MAX_ROUTE_NAMES);
    let reason = without_required(
        object(&[("code", code(&ROUTE_REASONS)), ("detail", string(256))]),
        &["detail"],
    );
    command(
        "work.route",
        object(&[
            ("subject", subject()),
            ("level", level(&[])),
            ("harnesses", names()),
            ("providers", names()),
            ("prefer", names()),
            ("hold", Schema::Boolean),
            ("reasons", array(reason, 16)),
            (
                "classification",
                Schema::Nullable {
                    inner: Box::new(classification()),
                },
            ),
        ]),
        &[
            // What remained of each list after the host intersected it
            // with what the level offers.
            (
                "routed",
                outcome(&[
                    ("harnesses", names()),
                    ("providers", names()),
                    ("prefer", names()),
                ]),
            ),
            ("stale", result()),
            (
                "refused",
                outcome(&[(
                    "reason",
                    enumeration(&[
                        "not_offered",
                        "owner_route",
                        "not_on_router",
                        "unknown_task",
                    ]),
                )]),
            ),
        ],
        Shape {
            effect: Effect::Mutation,
            recovery: Recovery::KeyedIdempotent,
            deadline_ms: 10_000,
            public: &["reference", "subject"],
        },
        None,
    )
}
fn judgment_outcomes(v21: &Registry) -> [(&'static str, Schema); 3] {
    let Schema::Object { properties, .. } = &v21.commands["judgment.request"].outcomes["assessed"]
    else {
        unreachable!("judgment.request assessed is an object")
    };
    let provenance = properties["provenance"].clone();
    [
        (
            "assessed",
            outcome(&[
                ("labels", array(string(128), 16)),
                ("flags", array(string(64), 32)),
                ("rationale", string(4096)),
                ("provenance", provenance),
            ]),
        ),
        (
            "failed",
            outcome(&[(
                "reason",
                enumeration(&[
                    "indeterminate",
                    "provider_error",
                    "provider_unavailable",
                    "deadline_exceeded",
                ]),
            )]),
        ),
        (
            "refused",
            outcome(&[(
                "reason",
                enumeration(&["invalid_request", "budget_exceeded"]),
            )]),
        ),
    ]
}
fn judgment_completion(v21: &Registry) -> Schema {
    let outcomes = judgment_outcomes(v21);
    object(&[
        (
            "outcome",
            enumeration(&outcomes.iter().map(|(n, _)| *n).collect::<Vec<_>>()),
        ),
        (
            "output",
            Schema::Union {
                variants: outcomes.into_iter().map(|(_, s)| s).collect(),
            },
        ),
    ])
}
fn judgment_assess(v21: &Registry) -> CommandContract {
    let accepted = v21.commands["judgment.request"].outcomes["accepted"].clone();
    let mut outcomes = vec![("accepted", accepted)];
    outcomes.extend(judgment_outcomes(v21));
    command(
        "judgment.assess",
        object(&[
            ("subject", subject()),
            ("question", text()),
            (
                "context",
                object(&[(
                    "items",
                    array(
                        object(&[("label", string(128)), ("text", string(8192))]),
                        16,
                    ),
                )]),
            ),
            (
                "fields",
                array(
                    object(&[
                        ("id", string(64)),
                        ("description", string(1024)),
                        ("choices", array(string(128), 64)),
                    ]),
                    16,
                ),
            ),
            (
                "flags",
                array(
                    object(&[("id", string(64)), ("description", string(1024))]),
                    32,
                ),
            ),
            ("rationale_max_bytes", range(0, 4096)),
            (
                "budget",
                object(&[
                    ("deadline_ms", range(1000, 600_000)),
                    ("max_output_tokens", range(1, 65_536)),
                ]),
            ),
        ]),
        &outcomes,
        Shape {
            effect: Effect::Mutation,
            recovery: Recovery::KeyedIdempotent,
            deadline_ms: 60_000,
            public: &[],
        },
        Some(AsyncContract {
            completion_event: "aq.judgment.assessed".into(),
            completion: judgment_completion(v21),
            correlation_field: "operation".into(),
            reconciliation_command: "operation.get".into(),
        }),
    )
}
/// 2.1.0, unchanged, plus the five additive contracts.
fn build_v2_2() -> Registry {
    let v21 = catalog::catalog_v2_1();
    let mut registry = v21.clone();
    for c in [work_read(), work_route(), judgment_assess(&v21)] {
        registry.commands.insert(c.name.clone(), c);
    }
    for e in [
        event(
            "aq.judgment.assessed",
            judgment_completion(&v21),
            &["operation"],
            "output.subject",
        ),
        event(
            "aq.work.route_needed",
            object(&[("subject", subject())]),
            &[],
            "subject",
        ),
    ] {
        registry.events.insert(e.name.clone(), e);
    }
    registry
}
fn pretty(value: &impl serde::Serialize) -> String {
    // A Value orders keys, as every published bundle does.
    let value = serde_json::to_value(value).unwrap();
    serde_json::to_string_pretty(&value).unwrap() + "\n"
}
fn fingerprints<'a, T: 'a>(
    items: impl Iterator<Item = (&'a String, &'a T)>,
    keep: impl Fn(&str) -> bool,
    fingerprint: impl Fn(&T) -> Hash,
) -> Value {
    items
        .filter(|(n, _)| keep(n))
        .map(|(n, c)| (n.clone(), json!(fingerprint(c))))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

#[test]
#[ignore = "writes contracts/aq-host-2.2.0; run by hand after changing the generator"]
fn write_catalog_v2_2() {
    let registry = build_v2_2();
    let dir = contracts_dir();
    let v21_manifest: Value = serde_json::from_str(catalog::MANIFEST_2_1_JSON).unwrap();
    let manifest = json!({
        "version": "2.2.0",
        "fingerprint": digest(&("2.2.0", &registry)).unwrap(),
        "supersedes": {"version": "2.1.0", "fingerprint": v21_manifest["fingerprint"]},
        "commands_added": ADDED_COMMANDS,
        "events_added": ADDED_EVENTS,
        "context_views": v21_manifest["context_views"],
        "work_kinds": v21_manifest["work_kinds"],
        "work_change_variants": v21_manifest["work_change_variants"],
    });
    let added = |n: &str| ADDED_COMMANDS.contains(&n) || ADDED_EVENTS.contains(&n);
    let command_fp = |c: &CommandContract| c.fingerprint().unwrap();
    let event_fp = |e: &EventContract| e.fingerprint().unwrap();
    let mut fixtures = fixtures();
    fixtures["compatibility"] = json!({
        "base": {
            "version": "2.1.0",
            "fingerprint": v21_manifest["fingerprint"],
            "registry_sha256": hash_bytes(catalog::REGISTRY_2_1_JSON.as_bytes()),
        },
        "unchanged_commands": fingerprints(registry.commands.iter(), |n| !added(n), command_fp),
        "unchanged_events": fingerprints(registry.events.iter(), |n| !added(n), event_fp),
        "changed_commands": {},
        "added_commands": fingerprints(registry.commands.iter(), added, command_fp),
        "added_events": fingerprints(registry.events.iter(), added, event_fp),
    });
    std::fs::write(dir.join("registry.json"), pretty(&registry)).unwrap();
    std::fs::write(dir.join("manifest.json"), pretty(&manifest)).unwrap();
    std::fs::write(dir.join("fixtures.json"), pretty(&fixtures)).unwrap();
}

// ---- Conformance. ----

#[test]
fn catalog_v2_2_manifest_and_fixtures_are_pinned() {
    let registry: Registry = serde_json::from_str(catalog::REGISTRY_2_2_JSON).unwrap();
    let manifest: Value = serde_json::from_str(catalog::MANIFEST_2_2_JSON).unwrap();
    let v21_manifest: Value = serde_json::from_str(catalog::MANIFEST_2_1_JSON).unwrap();
    assert_eq!(registry, catalog::catalog_v2_2());
    // The committed bytes are exactly what the generator writes.
    assert_eq!(registry, build_v2_2());
    assert_eq!(catalog::REGISTRY_2_2_JSON, pretty(&registry));
    assert_eq!(catalog::VERSION_2_2, "2.2.0");
    assert_eq!(manifest["version"], catalog::VERSION_2_2);
    assert_eq!(
        manifest["fingerprint"],
        digest(&(catalog::VERSION_2_2, &registry)).unwrap()
    );
    assert_ne!(manifest["fingerprint"], v21_manifest["fingerprint"]);
    assert_eq!(manifest["supersedes"]["version"], "2.1.0");
    assert_eq!(
        manifest["supersedes"]["fingerprint"],
        v21_manifest["fingerprint"]
    );
    assert_eq!(manifest["commands_added"], json!(ADDED_COMMANDS));
    assert_eq!(manifest["events_added"], json!(ADDED_EVENTS));
    for key in ["context_views", "work_kinds", "work_change_variants"] {
        assert_eq!(manifest[key], v21_manifest[key], "{key}");
    }
    assert_eq!(registry.commands.len(), 15);
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
    let compat = &fixtures()["compatibility"];
    for name in ADDED_COMMANDS {
        let c = &registry.commands[name];
        assert_eq!(c.version, "1.0.0");
        assert_eq!(c.capabilities, strings(&[name]), "{name}");
        assert_eq!(c.scopes, strings(&["project"]), "{name}");
        assert_eq!(c.max_payload_bytes, 65_536, "{name}");
        assert!(!c.cancellation, "{name}");
        assert_eq!(
            compat["added_commands"][name],
            json!(c.fingerprint().unwrap())
        );
    }
    for name in ADDED_EVENTS {
        let e = &registry.events[name];
        assert_eq!(e.version, "1.0.0");
        assert_eq!(e.trusted_producers, strings(&["aq.host.bridge@1"]));
        assert_eq!(e.retention_ms, 86_400_000);
        assert_eq!(e.max_payload_bytes, 65_536);
        assert_eq!(
            compat["added_events"][name],
            json!(e.fingerprint().unwrap())
        );
    }
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
    for c in [read, route] {
        assert_eq!(c.public_result_fields, strings(&["reference", "subject"]));
    }
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
    assert!(needed.correlation_fields.is_empty());
    assert_eq!(needed.subject_field.as_deref(), Some("subject.id"));
    assert_eq!(needed.revision_field.as_deref(), Some("subject.revision"));
    let assessed = &registry.events["aq.judgment.assessed"];
    assert_eq!(assessed.correlation_fields, strings(&["operation"]));
    assert_eq!(assessed.subject_field.as_deref(), Some("output.subject.id"));
    assert_eq!(
        assessed.revision_field.as_deref(),
        Some("output.subject.revision")
    );
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
    let v22 = catalog::catalog_v2_2();
    let v21_manifest: Value = serde_json::from_str(catalog::MANIFEST_2_1_JSON).unwrap();
    assert_eq!(compat["base"]["version"], "2.1.0");
    assert_eq!(compat["base"]["fingerprint"], v21_manifest["fingerprint"]);
    assert_eq!(
        hash_bytes(catalog::REGISTRY_2_1_JSON.as_bytes()),
        compat["base"]["registry_sha256"]
    );
    assert_eq!(compat["changed_commands"], json!({}));
    assert_eq!(v21.commands.len(), 12);
    assert_eq!(v21.events.len(), 9);
    assert_eq!(compat["unchanged_commands"].as_object().unwrap().len(), 12);
    assert_eq!(compat["unchanged_events"].as_object().unwrap().len(), 9);
    for (name, old) in &v21.commands {
        let new = &v22.commands[name];
        assert_eq!(new, old, "{name}");
        assert_eq!(canonical_bytes(new).unwrap(), canonical_bytes(old).unwrap());
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

/// A 2.1.0 workspace using an async judgment, a check and a candidate.
fn v21_workspace(catalog: Registry) -> Workspace {
    let prepare = json!({"op": {"kind": "invoke", "command": "candidate.prepare", "bind": "candidate",
        "retry": null,
        "input": {"op": "literal", "value": {"base": {"id": "work-1", "revision": 1}, "sources": []}}},
        "edges": {"prepared": "ask", "conflict": "failed", "refused": "failed"}, "on_error": "failed"});
    let ask = json!({"op": {"kind": "invoke", "command": "judgment.request", "bind": "j", "retry": null,
        "input": {"op": "literal", "value": {"question": "Approve?",
            "subject": {"id": "work-1", "revision": 1},
            "context": {"text": "Summary.", "artifacts": []},
            "output": {"choices": ["approve", "decline"], "rationale_max_bytes": 256},
            "budget": {"deadline_ms": 60000, "max_cost_microunits": 1000,
                       "max_input_tokens": 1024, "max_output_tokens": 256}}}},
        "edges": {"accepted": "read", "assessed": "failed", "failed": "failed", "refused": "failed"},
        "on_error": "failed"});
    let read = json!({"op": {"kind": "join", "members": [{"op": "ref", "root": "j", "path": []}],
        "member_keys": ["c"], "mode": "all", "accepted_outcomes": ["assessed"],
        "deadline_ms": 60000, "remaining": "track", "bind": "answer"},
        "edges": {"completed": "done", "no_match": "failed", "timeout": "failed"}, "on_error": "failed"});
    let ret = |o: &str| {
        json!({"op": {"kind": "return", "outcome": o, "output": {"op": "literal", "value": null}},
               "edges": {}, "on_error": null})
    };
    Workspace {
        markdown: "# Prepare, then ask\n".into(),
        record: SourceRecord {
            id: "prepare-ask".into(),
            binding: "prepare-ask".into(),
            scope: "project".into(),
            purpose: "Prepare a candidate, then ask".into(),
            commands: strings(&["candidate.prepare", "judgment.request"]),
            events: BTreeSet::new(),
            capabilities: strings(&["candidate.prepare", "judgment.request.v2", "operation.get"]),
            facts_version: String::new(),
        },
        catalog,
        facts: BTreeMap::new(),
        draft: json!({
            "entries": {"main": {"input": {"type": "null"},
                "outcomes": {"done": {"type": "null"}, "failed": {"type": "null"}}, "start": "prepare"}},
            "steps": {"prepare": prepare, "ask": ask, "read": read, "done": ret("done"), "failed": ret("failed")},
            "triggers": [],
        }),
    }
}

#[test]
fn an_artifact_sealed_on_2_1_0_validates_against_the_2_2_0_registry() {
    let v22 = catalog::catalog_v2_2();
    let sealed = seal(&v21_workspace(catalog::catalog_v2_1())).unwrap_or_else(|d| panic!("{d:#?}"));
    let retained = &sealed.content.registry;
    assert!(retained.commands.contains_key("operation.get"));
    assert!(retained.events.contains_key("aq.judgment.completed"));
    // Every contract the artifact pins is the 2.2.0 contract of that name,
    // the check a host makes before it installs an artifact.
    for (name, c) in &retained.commands {
        assert_eq!(&v22.commands[name], c, "{name}");
    }
    for (name, e) in &retained.events {
        assert_eq!(&v22.events[name], e, "{name}");
    }
    let mut on_v22 = sealed.content.clone();
    on_v22.registry = Registry {
        commands: retained
            .commands
            .keys()
            .map(|n| (n.clone(), v22.commands[n].clone()))
            .collect(),
        events: retained
            .events
            .keys()
            .map(|n| (n.clone(), v22.events[n].clone()))
            .collect(),
    };
    assert_eq!(on_v22.validate_structure(), vec![]);
    let resealed = Artifact::seal(on_v22).unwrap();
    assert!(resealed.verify().is_ok());
    assert_eq!(resealed.digest, sealed.digest);
    // Filling the same draft from the 2.2.0 catalog seals the same artifact.
    let refilled = seal(&v21_workspace(v22)).unwrap_or_else(|d| panic!("{d:#?}"));
    assert_eq!(refilled.digest, sealed.digest);
}

#[test]
fn judgment_assess_completion_is_assignable_to_its_event() {
    let v22 = catalog::catalog_v2_2();
    let assess = &v22.commands["judgment.assess"];
    let a = assess.asynchronous.as_ref().unwrap();
    let event = &v22.events[&a.completion_event];
    assert!(a.completion.assignable_to(&event.payload));
    assert!(event.correlation_fields.contains(&a.correlation_field));
    // Each completion variant is exactly the business outcome of that name.
    let Schema::Object { properties, .. } = &a.completion else {
        panic!("completion is an object")
    };
    let Schema::Union { variants } = &properties["output"] else {
        panic!("output is a union")
    };
    let names = ["assessed", "failed", "refused"];
    assert_eq!(variants.len(), names.len());
    for (variant, name) in variants.iter().zip(names) {
        assert_eq!(variant, &assess.outcomes[name], "{name}");
    }
    // In an artifact, the structure check finds no async_contract issue.
    let ws = routing_workspace();
    let sealed = seal(&ws).unwrap_or_else(|d| panic!("{d:#?}"));
    let issues = sealed.content.validate_structure();
    assert!(
        issues.iter().all(|i| i.code != "async_contract"),
        "{issues:#?}"
    );
    assert_eq!(issues, vec![]);
}

/// Labels: one per requested field, in field order, each a declared choice of
/// its field; flags: declared flags only. Only such an answer is `assessed`.
fn satisfies(request: &Value, answer: &Value) -> bool {
    let fields = request["fields"].as_array().unwrap();
    let labels = answer["labels"].as_array().unwrap();
    let declared: Vec<&Value> = request["flags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| &f["id"])
        .collect();
    let flags = answer["flags"].as_array().unwrap();
    let distinct: BTreeSet<String> = flags.iter().map(|f| f.to_string()).collect();
    labels.len() == fields.len()
        && fields
            .iter()
            .zip(labels)
            .all(|(f, l)| f["choices"].as_array().unwrap().contains(l))
        && flags.iter().all(|f| declared.contains(&f))
        && distinct.len() == flags.len()
}

#[test]
fn valid_and_invalid_fixture_cases_validate_as_declared() {
    let registry = catalog::catalog_v2_2();
    let fixtures = fixtures();
    let mut covered = BTreeSet::new();
    for fixture in fixtures["cases"].as_array().unwrap() {
        let name = fixture["name"].as_str().unwrap();
        let contract = fixture["contract"].as_str().unwrap();
        let kind = fixture["kind"].as_str().unwrap();
        let schema = match kind {
            "input" => &registry.commands[contract].input,
            "output" => &registry.commands[contract].outcomes[fixture["outcome"].as_str().unwrap()],
            "event" => &registry.events[contract].payload,
            other => panic!("unknown fixture kind {other}"),
        };
        let valid = fixture["valid"].as_bool().unwrap();
        assert_eq!(schema.validate(&fixture["value"]).is_ok(), valid, "{name}");
        if valid {
            let place = match kind {
                "output" => format!("{contract}.{}", fixture["outcome"].as_str().unwrap()),
                "input" => format!("{contract}.input"),
                _ => contract.to_string(),
            };
            covered.insert(place);
        }
    }
    // A valid case for every new input, outcome and event.
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
    assert_eq!(covered, expected);
    // The assessed guarantee: one declared label per field, in field order.
    let assess = &registry.commands["judgment.assess"];
    let guarantee = &fixtures["assessed_guarantee"];
    let request = &guarantee["request"];
    assess.input.validate(request).unwrap();
    let assessed = &guarantee["assessed"];
    assess.outcomes["assessed"].validate(assessed).unwrap();
    assert!(satisfies(request, assessed));
    let completion = &assess.asynchronous.as_ref().unwrap().completion;
    let unsatisfiable = guarantee["unsatisfiable"].as_array().unwrap();
    assert!(unsatisfiable.len() >= 4);
    for case in unsatisfiable {
        let name = case["name"].as_str().unwrap();
        assert!(!satisfies(request, &case["answer"]), "{name}");
        // The host reports it as failed, never as assessed with a gap.
        let result = &case["result"];
        assert_eq!(result["outcome"], "failed", "{name}");
        assert_eq!(result["output"]["reason"], "indeterminate", "{name}");
        completion.validate(result).unwrap();
        assess.outcomes["failed"]
            .validate(&result["output"])
            .unwrap();
    }
}

// ---- Feasibility: the owner's routing document as a draft. ----

const ROUTING_DRAFT: &str = include_str!("data/routing_draft.json");
const ROUTING_MARKDOWN: &str = "# Routing\n\nThe owner's routing document (docs/specs/0011-routing-policy-default.md in aq-server).\n";

/// What Settings offers in the owner's live shape, with every level the
/// document names (Simple high, Standard high, Complex medium and high, Deep):
/// a local model (opencode on ollama) only at Simple and Standard, gemini up to
/// Complex, Deep only Claude and Codex.
fn routing_facts() -> BTreeMap<String, Value> {
    let everyone = json!({"harnesses": ["claude", "codex", "gemini", "opencode"],
                          "providers": ["anthropic", "google", "ollama", "openai"]});
    let hosted = json!({"harnesses": ["claude", "codex", "gemini"],
                        "providers": ["anthropic", "google", "openai"]});
    let deep = json!({"harnesses": ["claude", "codex"], "providers": ["anthropic", "openai"]});
    let offers = json!({
        "simple-low": everyone, "simple-high": everyone,
        "standard-low": everyone, "standard-medium": everyone, "standard-high": everyone,
        "complex-low": hosted, "complex-medium": hosted, "complex-high": hosted,
        "deep-low": deep, "deep-high": deep,
    });
    BTreeMap::from([
        (
            "levels".into(),
            json!([
                "simple-low",
                "simple-high",
                "standard-low",
                "standard-medium",
                "standard-high",
                "complex-low",
                "complex-medium",
                "complex-high",
                "deep-low",
                "deep-high"
            ]),
        ),
        ("offers".into(), offers),
        (
            "harnesses".into(),
            json!(["claude", "codex", "gemini", "opencode"]),
        ),
        (
            "providers".into(),
            json!(["anthropic", "google", "ollama", "openai"]),
        ),
    ])
}

fn routing_workspace() -> Workspace {
    Workspace {
        markdown: ROUTING_MARKDOWN.into(),
        record: SourceRecord {
            id: "routing".into(),
            binding: "aq.routing".into(),
            scope: "project".into(),
            purpose: "Route each task to a level and the agents that may take it".into(),
            commands: strings(&["work.read", "work.route", "judgment.assess"]),
            events: strings(&["aq.work.route_needed"]),
            capabilities: strings(&[
                "work.read",
                "work.route",
                "judgment.assess",
                "operation.get",
            ]),
            facts_version: "settings-1".into(),
        },
        catalog: catalog::catalog_v2_2(),
        facts: routing_facts(),
        draft: serde_json::from_str(ROUTING_DRAFT).unwrap(),
    }
}

#[test]
fn the_routing_document_draft_checks_against_catalog_2_2_0() {
    let ws = routing_workspace();
    let diagnostics = check(&ws);
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    let sealed = seal(&ws).unwrap_or_else(|d| panic!("{d:#?}"));
    assert!(sealed.verify().is_ok());
    let retained = &sealed.content.registry;
    for name in [
        "work.read",
        "work.route",
        "judgment.assess",
        "operation.get",
    ] {
        assert!(retained.commands.contains_key(name), "{name}");
    }
    for name in ["aq.work.route_needed", "aq.judgment.assessed"] {
        assert!(retained.events.contains_key(name), "{name}");
    }
    // The strict draft rules still bite: reading the labels where the
    // judgment may have failed is refused.
    let mut wrong = ws.clone();
    wrong.draft["steps"]["await"]["op"]["accepted_outcomes"] = json!(["assessed", "failed"]);
    let diagnostics = check(&wrong);
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code == "expression_type" && d.message.contains("labels")),
        "{diagnostics:#?}"
    );
    // And so is reading the judgment's answer on the fallback path, which
    // the judgment's `on_error` (nothing bound) also reaches.
    let mut wrong = ws.clone();
    wrong.draft["steps"]["assume"]["op"]["input"]["fields"]["classification"] = json!({
        "op": "object", "fields": {
            "reference": {"op": "literal", "value": ""},
            "fields": {"op": "literal", "value": []},
            "labels": {"op": "ref", "root": "answer", "path": ["c", "output", "labels"]},
            "flags": {"op": "literal", "value": []},
            "rationale": {"op": "literal", "value": ""}}});
    let diagnostics = check(&wrong);
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code == "expression_type" && d.path == "draft.json:steps.assume"),
        "{diagnostics:#?}"
    );
    // A route that gives a reason outside the fixed list does not check.
    let mut wrong = ws.clone();
    wrong.draft["steps"]["assume"]["op"]["input"]["fields"]["reasons"] = json!({"op": "literal",
        "value": [{"code": "because_i_said_so", "detail": "Free text cannot be a code."}]});
    let diagnostics = check(&wrong);
    assert!(
        diagnostics.iter().any(|d| d.code == "schema_compatibility"
            && d.path == "draft.json:steps.assume"
            && d.message.contains("because_i_said_so")),
        "{diagnostics:#?}"
    );
    // The task's priority is not a fact a playbook can read.
    let mut wrong = ws.clone();
    wrong.draft["steps"]["ask"]["op"]["input"]["fields"]["context"]["fields"]["items"]["items"]
        [0]["fields"]["text"] = json!({"op": "ref", "root": "task", "path": ["facts", "priority"]});
    let diagnostics = check(&wrong);
    assert!(
        diagnostics
            .iter()
            .any(|d| d.path == "draft.json:steps.ask" && d.message.contains("priority")),
        "{diagnostics:#?}"
    );
    // Also when the reason is chosen by the playbook's own filter and map.
    let mut wrong = ws;
    wrong.draft["steps"]["route"]["op"]["input"]["body"]["fields"]["reasons"]["collection"]["collection"]
        ["items"][0]["fields"]["code"] = json!({"op": "literal", "value": "because_i_said_so"});
    let diagnostics = check(&wrong);
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code == "schema_compatibility" && d.path == "draft.json:steps.route"),
        "{diagnostics:#?}"
    );
}

/// The route the draft writes for one classified task: its `route` step's
/// input, evaluated after a `work.read` and an `assessed` answer.
fn route_for(task: &Task) -> Value {
    let ws = routing_workspace();
    let input: Expr =
        serde_json::from_value(ws.draft["steps"]["route"]["op"]["input"].clone()).unwrap();
    let subject = json!({"id": "task-1", "revision": 3});
    let facts = json!({"kind": task.filed_kind, "title": "A task", "description": "",
        "origin": "owner", "level_hint": task.hint, "charged_failures": task.failures,
        "ended_attempts": task.failures, "route": "none"});
    let answer = json!({"c": {"participant": "judgment:op-1", "outcome": "assessed", "output": {
        "reference": "judgment:op-1", "subject": subject, "labels": [task.kind, task.risk],
        "flags": task.flags, "rationale": "Because.",
        "provenance": fixtures()["assessed_guarantee"]["assessed"]["provenance"]}}});
    let v22 = catalog::catalog_v2_2();
    v22.commands["work.read"].outcomes["observed"]
        .validate(&json!({"reference": "task-1@3", "subject": subject, "facts": facts}))
        .unwrap();
    let context = EvalContext::from([
        (
            "task".into(),
            EvalValue::Known(json!({"reference": "task-1@3", "subject": subject, "facts": facts})),
        ),
        ("answer".into(), EvalValue::Known(answer)),
        (
            "definitions".into(),
            EvalValue::Known(json!(
                ws.facts.into_iter().collect::<serde_json::Map<_, _>>()
            )),
        ),
    ]);
    let EvalValue::Known(route) = eval(&input, &context, &EvalLimits::default()).unwrap() else {
        panic!("the route is known")
    };
    v22.commands["work.route"].input.validate(&route).unwrap();
    route
}
/// What the host keeps of a route at its level, as `work.route` documents:
/// each list intersected with what the level offers, or `None` (`refused
/// not_offered`) when the level is not offered or a restricting list is left
/// empty. A `prefer` entry never causes a refusal.
fn host_keeps(route: &Value) -> Option<Value> {
    let offers = &routing_facts()["offers"];
    let at = offers.get(route["level"].as_str().unwrap())?;
    let mut kept = json!({});
    for (list, offered) in [
        ("harnesses", "harnesses"),
        ("providers", "providers"),
        ("prefer", "harnesses"),
    ] {
        let asked = route[list].as_array().unwrap();
        let left: Vec<&Value> = asked
            .iter()
            .filter(|n| at[offered].as_array().unwrap().contains(n))
            .collect();
        if list != "prefer" && !asked.is_empty() && left.is_empty() {
            return None;
        }
        kept[list] = json!(left);
    }
    Some(kept)
}
struct Task {
    filed_kind: &'static str,
    hint: &'static str,
    failures: i64,
    kind: &'static str,
    risk: &'static str,
    flags: &'static [&'static str],
}
struct Expected {
    level: &'static str,
    harnesses: &'static [&'static str],
    prefer: &'static [&'static str],
    hold: bool,
    codes: &'static [&'static str],
}
const NT: &[&str] = &["narrow", "covered_by_test"];
const CC: &[&str] = &["claude", "codex"];
const ANY: &[&str] = &[];
fn task(kind: &'static str, risk: &'static str, flags: &'static [&'static str]) -> Task {
    Task {
        filed_kind: kind,
        hint: "",
        failures: 0,
        kind,
        risk,
        flags,
    }
}
fn expect(
    level: &'static str,
    harnesses: &'static [&'static str],
    codes: &'static [&'static str],
) -> Expected {
    Expected {
        level,
        harnesses,
        prefer: &[],
        hold: false,
        codes,
    }
}

/// Each row is a concrete task and the route the owner's routing document
/// (aq-server `docs/specs/0011-routing-policy-default.md`) gives it; the
/// comment quotes the sentence, with its line.
#[test]
fn the_routing_draft_routes_as_the_document_says() {
    let hint = |t: Task, hint| Task { hint, ..t };
    let failed = |t: Task, failures| Task { failures, ..t };
    let prefer = |e: Expected, prefer| Expected { prefer, ..e };
    let rows: Vec<(&str, Task, Expected)> = vec![
        // 16: "Features, bug fixes, refactors, tests and docs are Standard, high effort."
        // 66: "Anything not mentioned here, takes only low risk work." Low risk: anyone.
        (
            "feature, low",
            task("feature", "low", &[]),
            expect("standard-high", ANY, &["level_from_kind"]),
        ),
        // 61: "High and very high risk work goes only to Claude or Codex."
        // 52: "Medium and high risk work is at least Standard, high effort."
        (
            "refactor, high",
            task("refactor", "high", &[]),
            expect(
                "standard-high",
                CC,
                &["level_from_kind", "harnesses_limited"],
            ),
        ),
        // 62: "Medium risk work goes to Claude or Codex."
        (
            "bugfix, medium",
            task("bugfix", "medium", &[]),
            expect(
                "standard-high",
                CC,
                &["level_from_kind", "harnesses_limited"],
            ),
        ),
        // 64: "Low risk work that is narrow and covered by a test should go to a local model first."
        (
            "test, low, narrow and covered",
            task("test", "low", NT),
            prefer(
                expect(
                    "standard-high",
                    ANY,
                    &["level_from_kind", "harness_preferred"],
                ),
                &["opencode"],
            ),
        ),
        (
            "docs, low, narrow and covered",
            task("docs", "low", NT),
            prefer(
                expect(
                    "standard-high",
                    ANY,
                    &["level_from_kind", "harness_preferred"],
                ),
                &["opencode"],
            ),
        ),
        // 18: "Chores and syncs are Simple, high effort."
        (
            "chore, low",
            task("chore", "low", &[]),
            expect("simple-high", ANY, &["level_from_kind"]),
        ),
        // 18 + 52: a medium-risk sync is raised to the risk floor.
        (
            "sync, medium",
            task("sync", "medium", &[]),
            expect(
                "standard-high",
                CC,
                &["level_from_kind", "raised_for_risk", "harnesses_limited"],
            ),
        ),
        // 19: "Specs, designs, implementation plans and reviews of someone else's work are Complex, high effort."
        (
            "spec, medium",
            task("spec", "medium", &[]),
            expect(
                "complex-high",
                CC,
                &["level_from_kind", "harnesses_limited"],
            ),
        ),
        (
            "review, medium",
            task("review", "medium", &[]),
            expect(
                "complex-high",
                CC,
                &["level_from_kind", "harnesses_limited"],
            ),
        ),
        // 67: "Design and planning go to Claude first, and to Codex only when Claude can't take them."
        (
            "design, low",
            task("design", "low", NT),
            prefer(
                expect(
                    "complex-high",
                    CC,
                    &["level_from_kind", "harness_preferred"],
                ),
                &["claude"],
            ),
        ),
        (
            "plan, high",
            task("plan", "high", &[]),
            prefer(
                expect(
                    "complex-high",
                    CC,
                    &["level_from_kind", "harness_preferred"],
                ),
                &["claude"],
            ),
        ),
        // 21: "Research is Standard, high effort, unless it is clearly harder, in which case it is Complex, high effort."
        (
            "research, low",
            task("research", "low", &[]),
            expect("standard-high", ANY, &["level_from_kind"]),
        ),
        (
            "research, low, clearly harder",
            task("research", "low", &["clearly_harder"]),
            expect("complex-high", ANY, &["level_from_kind"]),
        ),
        // 23: "Deep is only for analysis, planning or spec writing that needs advanced mathematics or unusually hard reasoning."
        (
            "analysis, medium, deep reasoning",
            task("analysis", "medium", &["needs_deep_reasoning"]),
            expect(
                "deep-high",
                CC,
                &["level_from_kind", "deep_reserved", "harnesses_limited"],
            ),
        ),
        (
            "plan, medium, deep reasoning",
            task("plan", "medium", &["needs_deep_reasoning"]),
            prefer(
                expect(
                    "deep-high",
                    CC,
                    &["level_from_kind", "deep_reserved", "harness_preferred"],
                ),
                &["claude"],
            ),
        ),
        // Ambiguous (report): analysis without hard reasoning is not named; "Most tasks belong here" (17).
        (
            "analysis, low",
            task("analysis", "low", &[]),
            expect("standard-high", ANY, &["level_from_kind"]),
        ),
        // 24: "A big task is not a Deep task, and neither is a risky one."
        (
            "review, very high, deep reasoning flag",
            task("review", "very_high", &["needs_deep_reasoning"]),
            expect(
                "complex-high",
                CC,
                &["level_from_kind", "harnesses_limited"],
            ),
        ),
        // 69: "Art and visual design go to Codex, and wait for it rather than going elsewhere."
        (
            "art, medium",
            task("art", "medium", &[]),
            Expected {
                hold: true,
                ..expect(
                    "standard-high",
                    &["codex"],
                    &["level_from_kind", "held_for_harness"],
                )
            },
        ),
        (
            "art, low, narrow and covered",
            task("art", "low", NT),
            Expected {
                hold: true,
                ..expect(
                    "standard-high",
                    &["codex"],
                    &["level_from_kind", "held_for_harness"],
                )
            },
        ),
        // 73: "Repairs to a failed integration or build never go to a local model."
        (
            "repair, low, narrow and covered",
            task("repair", "low", NT),
            expect(
                "standard-high",
                &["claude", "codex", "gemini"],
                &["level_from_kind", "harness_excluded"],
            ),
        ),
        (
            "repair, medium, narrow and covered",
            task("repair", "medium", NT),
            expect(
                "standard-high",
                CC,
                &["level_from_kind", "harnesses_limited", "harness_excluded"],
            ),
        ),
        // 53: "Very high risk work is at least Complex, medium effort, ..."
        (
            "feature, very high",
            task("feature", "very_high", &[]),
            expect(
                "complex-medium",
                CC,
                &["level_from_kind", "raised_for_risk", "harnesses_limited"],
            ),
        ),
        // 53: "... unless it is narrow and covered by a test, in which case Standard, high effort is enough."
        (
            "feature, very high, narrow and covered",
            task("feature", "very_high", NT),
            expect(
                "standard-high",
                CC,
                &["level_from_kind", "risk_relaxed", "harnesses_limited"],
            ),
        ),
        // 62: "It may go to a local model only when the task is narrow, covered by a test, and Standard."
        (
            "refactor, medium, narrow and covered",
            task("refactor", "medium", NT),
            expect(
                "standard-high",
                &["claude", "codex", "opencode"],
                &["level_from_kind", "harnesses_limited"],
            ),
        ),
        (
            "refactor, medium, narrow and covered, asked for Complex",
            hint(task("refactor", "medium", NT), "complex-high"),
            expect("complex-high", CC, &["hint_respected", "harnesses_limited"]),
        ),
        // 31: "A bug fix that has already failed once for a real reason moves up to Complex."
        (
            "bugfix, high, no failure",
            task("bugfix", "high", &[]),
            expect(
                "standard-high",
                CC,
                &["level_from_kind", "harnesses_limited"],
            ),
        ),
        (
            "bugfix, high, one failure",
            failed(task("bugfix", "high", &[]), 1),
            expect(
                "complex-high",
                CC,
                &[
                    "level_from_kind",
                    "escalated_after_failure",
                    "harnesses_limited",
                ],
            ),
        ),
        (
            "bugfix, high, two failures",
            failed(task("bugfix", "high", &[]), 2),
            expect(
                "complex-high",
                CC,
                &[
                    "level_from_kind",
                    "escalated_after_failure",
                    "harnesses_limited",
                ],
            ),
        ),
        // Failures only escalate bug fixes.
        (
            "feature, low, one failure",
            failed(task("feature", "low", &[]), 1),
            expect("standard-high", ANY, &["level_from_kind"]),
        ),
        // 27: "If whoever filed the task asked for a level, respect it, ..."
        (
            "feature, asked for Standard low",
            hint(task("feature", "low", &[]), "standard-low"),
            expect("standard-low", ANY, &["hint_respected"]),
        ),
        (
            "chore, asked for Simple low",
            hint(task("chore", "low", &[]), "simple-low"),
            expect("simple-low", ANY, &["hint_respected"]),
        ),
        // 52: the risk floor still applies to a level asked for.
        (
            "bugfix, medium, asked for Simple low",
            hint(task("bugfix", "medium", &[]), "simple-low"),
            expect(
                "standard-high",
                CC,
                &["hint_respected", "raised_for_risk", "harnesses_limited"],
            ),
        ),
        // 28: "... but don't go above Standard for tests, docs, chores and syncs, ..."
        (
            "docs, asked for Complex",
            hint(task("docs", "low", &[]), "complex-high"),
            expect("standard-high", ANY, &["hint_capped"]),
        ),
        (
            "sync, asked for Deep",
            hint(task("sync", "low", &[]), "deep-high"),
            expect("standard-high", ANY, &["hint_capped"]),
        ),
        // 28: "... or above Complex for ordinary implementation work."
        (
            "feature, asked for Deep",
            hint(task("feature", "low", &[]), "deep-high"),
            expect("complex-high", ANY, &["hint_capped"]),
        ),
        // 23 + 31 + 71: an escalated bug fix never reaches Deep, asked for or not.
        (
            "bugfix, one failure, asked for Deep",
            failed(hint(task("bugfix", "high", &[]), "deep-high"), 1),
            expect("complex-high", CC, &["hint_capped", "harnesses_limited"]),
        ),
        // 23: a level asked for does not lift other work to Deep.
        (
            "review, asked for Deep",
            hint(task("review", "low", &[]), "deep-high"),
            expect("complex-high", ANY, &["deep_reserved"]),
        ),
        (
            "analysis, asked for Deep, no hard reasoning",
            hint(task("analysis", "low", &[]), "deep-high"),
            expect("complex-high", ANY, &["deep_reserved"]),
        ),
        (
            "analysis, asked for Deep, hard reasoning",
            hint(
                task("analysis", "low", &["needs_deep_reasoning"]),
                "deep-high",
            ),
            expect("deep-high", ANY, &["deep_reserved", "hint_respected"]),
        ),
        // A level Settings does not offer is not used (report: a reading).
        (
            "feature, asked for an unoffered level",
            hint(task("feature", "low", &[]), "complex-max"),
            expect("standard-high", ANY, &["level_from_kind", "policy"]),
        ),
        // 81: "If the risk or the kind of work can't be worked out, don't block the task.
        //      Route it as medium risk at Standard, high effort, on Claude or Codex, and
        //      say that the risk was assumed."
        (
            "unknown kind, low, narrow and covered",
            Task {
                kind: "unknown",
                ..task("", "low", NT)
            },
            expect(
                "standard-high",
                CC,
                &["level_from_kind", "risk_assumed", "harnesses_limited"],
            ),
        ),
        (
            "feature, unknown risk, narrow and covered",
            task("feature", "unknown", NT),
            expect(
                "standard-high",
                CC,
                &["level_from_kind", "risk_assumed", "harnesses_limited"],
            ),
        ),
        // 64 + C2: a local model first, but none is offered at Complex; the host drops it.
        (
            "spec, low, narrow and covered",
            task("spec", "low", NT),
            prefer(
                expect(
                    "complex-high",
                    ANY,
                    &["level_from_kind", "harness_preferred"],
                ),
                &["opencode"],
            ),
        ),
    ];
    assert!(rows.len() >= 16);
    for (name, task, want) in &rows {
        let r = route_for(task);
        let codes: Vec<&str> = r["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["code"].as_str().unwrap())
            .collect();
        assert_eq!(r["level"], want.level, "{name}");
        assert_eq!(r["harnesses"], json!(want.harnesses), "{name}");
        assert_eq!(r["providers"], json!([]), "{name}");
        assert_eq!(r["prefer"], json!(want.prefer), "{name}");
        assert_eq!(r["hold"], want.hold, "{name}");
        assert_eq!(codes, want.codes, "{name}");
        assert_eq!(
            r["classification"]["fields"],
            json!(["kind", "risk"]),
            "{name}"
        );
        assert_eq!(r["classification"]["reference"], "judgment:op-1", "{name}");
        // Every reason says why, in the document's words.
        for reason in r["reasons"].as_array().unwrap() {
            assert!(
                reason["detail"].as_str().is_some_and(|d| !d.is_empty()),
                "{name}"
            );
        }
        // Under the owner's Settings, nothing the document allows is refused.
        assert!(host_keeps(&r).is_some(), "{name}: {r}");
    }
    // The host drops a preference the level does not offer; it never refuses.
    let spec = route_for(&task("spec", "low", NT));
    assert_eq!(host_keeps(&spec).unwrap()["prefer"], json!([]));
}

/// The playbook ends `unservable`, with its reason, exactly when `work.route`
/// is refused `not_offered`: "If nothing allowed could ever take it, leave it
/// unrouted and visible, say why on the task, and raise it for my attention."
#[test]
fn the_routing_draft_ends_unservable_only_when_nothing_is_offered() {
    let ws = routing_workspace();
    let steps = &ws.draft["steps"];
    for (step, decide) in [("route", "route_refused"), ("assume", "assume_refused")] {
        assert_eq!(steps[step]["edges"]["refused"], decide);
        assert_eq!(steps[step]["edges"]["stale"], "skip");
        assert_eq!(steps[decide]["edges"]["true"], "unservable");
        assert_eq!(steps[decide]["edges"]["false"], "skip");
        let predicate = &steps[decide]["op"]["predicate"];
        assert_eq!(predicate["right"]["value"], "not_offered");
    }
    let unservable = &steps["unservable"]["op"];
    assert_eq!(unservable["outcome"], "unservable");
    let schema: Schema =
        serde_json::from_value(ws.draft["entries"]["main"]["outcomes"]["unservable"].clone())
            .unwrap();
    schema.validate(&unservable["output"]["value"]).unwrap();
    assert_eq!(
        unservable["output"]["value"]["reasons"][0]["code"],
        "nothing_offered"
    );
    // Nothing else reaches it.
    let reaching: Vec<&String> = steps
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, s)| {
            s["edges"]
                .as_object()
                .unwrap()
                .values()
                .any(|t| t == "unservable")
                || s["on_error"] == "unservable"
        })
        .map(|(n, _)| n)
        .collect();
    assert_eq!(reaching, ["assume_refused", "route_refused"]);
}
