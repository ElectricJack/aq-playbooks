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
/// The fixed reasons a route may give; a playbook cannot build text.
const ROUTE_REASONS: [&str; 13] = [
    "classification_failed",
    "escalated_after_failure",
    "harness_excluded",
    "harness_preferred",
    "harnesses_limited",
    "held_for_harness",
    "hint_capped",
    "hint_respected",
    "level_from_kind",
    "nothing_allowed",
    "raised_for_risk",
    "risk_assumed",
    "level_lowered",
];

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
    let facts = without_required(
        object(&[
            ("kind", string(64)),
            ("title", string(512)),
            ("description", string(8192)),
            (
                "origin",
                enumeration(&["owner", "agent", "supervisor", "playbook", "unknown"]),
            ),
            ("level_hint", string(64)),
            ("charged_failures", range(0, 1000)),
            ("ended_attempts", range(0, 1000)),
            ("route", enumeration(&["none", "router", "owner"])),
        ]),
        &["level_hint"],
    );
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
        ("labels", array(string(128), 16)),
        ("flags", array(string(64), 32)),
        ("rationale", string(4096)),
    ])
}
fn work_route() -> CommandContract {
    let names = || array(string(64), 16);
    command(
        "work.route",
        object(&[
            ("subject", subject()),
            ("level", string(64)),
            ("harnesses", names()),
            ("providers", names()),
            ("prefer", names()),
            ("hold", Schema::Boolean),
            (
                "reasons",
                array(
                    object(&[("code", code(&ROUTE_REASONS)), ("detail", string(400))]),
                    16,
                ),
            ),
            (
                "classification",
                Schema::Nullable {
                    inner: Box::new(classification()),
                },
            ),
        ]),
        &[
            ("routed", result()),
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
        facts: BTreeMap::from([
            (
                "levels".into(),
                json!([
                    "simple-low",
                    "simple-high",
                    "standard-low",
                    "standard-medium",
                    "standard-high",
                    "complex-low",
                    "complex-high",
                    "deep-low",
                    "deep-high"
                ]),
            ),
            (
                "offers".into(),
                json!([
                    {"provider": "anthropic", "harness": "claude"},
                    {"provider": "openai", "harness": "codex"},
                    {"provider": "google", "harness": "gemini"},
                    {"provider": "ollama", "harness": "opencode"}
                ]),
            ),
            (
                "harnesses".into(),
                json!(["claude", "codex", "gemini", "opencode"]),
            ),
            (
                "providers".into(),
                json!(["anthropic", "google", "ollama", "openai"]),
            ),
        ]),
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

/// The route the draft computes for one classified task.
fn route_for(facts: Value, labels: [&str; 2], flags: &[&str]) -> Value {
    let ws = routing_workspace();
    let input: Expr =
        serde_json::from_value(ws.draft["steps"]["route"]["op"]["input"].clone()).unwrap();
    let subject = json!({"id": "task-1", "revision": 3});
    let answer = json!({"c": {"participant": "judgment:op-1", "outcome": "assessed", "output": {
        "reference": "judgment:op-1", "subject": subject, "labels": labels, "flags": flags,
        "rationale": "Because.", "provenance": fixtures()["assessed_guarantee"]["assessed"]["provenance"]}}});
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
    catalog::catalog_v2_2().commands["work.route"]
        .input
        .validate(&route)
        .unwrap();
    route
}
fn task_facts(kind: &str, hint: Option<&str>, charged_failures: i64) -> Value {
    let mut facts = json!({"kind": kind, "title": "A task", "description": "", "origin": "owner",
        "charged_failures": charged_failures, "ended_attempts": charged_failures, "route": "none"});
    if let Some(hint) = hint {
        facts["level_hint"] = json!(hint);
    }
    facts
}
fn codes(route: &Value) -> Vec<&str> {
    route["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["code"].as_str().unwrap())
        .collect()
}

#[test]
fn the_routing_draft_computes_the_documents_routes() {
    // "A bug fix that has already failed once for a real reason moves up to Complex."
    let r = route_for(task_facts("bugfix", None, 1), ["bugfix", "high"], &[]);
    assert_eq!(r["level"], "complex-high");
    assert_eq!(r["harnesses"], json!(["claude", "codex"]));
    assert_eq!(r["hold"], false);
    assert_eq!(
        codes(&r),
        [
            "level_from_kind",
            "escalated_after_failure",
            "harnesses_limited"
        ]
    );
    assert_eq!(r["classification"]["labels"], json!(["bugfix", "high"]));
    // Low risk, narrow and covered by a test: anyone, a local model first.
    let r = route_for(
        task_facts("docs", None, 0),
        ["docs", "low"],
        &["narrow", "covered_by_test"],
    );
    assert_eq!(r["level"], "standard-high");
    assert_eq!(r["harnesses"], json!([]));
    assert_eq!(r["prefer"], json!(["opencode"]));
    assert_eq!(codes(&r), ["level_from_kind", "harness_preferred"]);
    // Art goes to Codex and waits for it.
    let r = route_for(task_facts("art", None, 0), ["art", "medium"], &[]);
    assert_eq!(
        (&r["level"], &r["harnesses"], &r["hold"]),
        (&json!("standard-high"), &json!(["codex"]), &json!(true))
    );
    assert!(codes(&r).contains(&"held_for_harness"));
    // A level hint is respected, but not above Standard for chores.
    let r = route_for(
        task_facts("chore", Some("deep-high"), 0),
        ["chore", "low"],
        &[],
    );
    assert_eq!(r["level"], "standard-high");
    assert_eq!(codes(&r), ["hint_capped"]);
    let r = route_for(
        task_facts("feature", Some("standard-low"), 0),
        ["feature", "low"],
        &[],
    );
    assert_eq!(r["level"], "standard-low");
    assert_eq!(codes(&r), ["hint_respected"]);
    // Very high risk is at least Complex unless narrow and covered.
    let r = route_for(
        task_facts("feature", None, 0),
        ["feature", "very_high"],
        &[],
    );
    assert_eq!(r["level"], "complex-low");
    assert!(codes(&r).contains(&"raised_for_risk"));
    let r = route_for(
        task_facts("feature", None, 0),
        ["feature", "very_high"],
        &["narrow", "covered_by_test"],
    );
    assert_eq!(r["level"], "standard-high");
    // Medium risk may go to a local model only when narrow, covered and Standard.
    let r = route_for(
        task_facts("refactor", None, 0),
        ["refactor", "medium"],
        &["narrow", "covered_by_test"],
    );
    assert_eq!(r["harnesses"], json!(["claude", "codex", "opencode"]));
    // Repairs never go to a local model.
    let r = route_for(
        task_facts("repair", None, 0),
        ["repair", "low"],
        &["narrow", "covered_by_test"],
    );
    assert_eq!(r["harnesses"], json!(["claude", "codex", "gemini"]));
    assert_eq!(r["prefer"], json!([]));
    // Deep analysis keeps Claude; Design and planning go to Claude first.
    let r = route_for(
        task_facts("analysis", None, 0),
        ["analysis", "medium"],
        &["needs_deep_reasoning"],
    );
    assert_eq!(r["level"], "deep-high");
    assert_eq!(r["harnesses"], json!(["claude", "codex"]));
    let r = route_for(task_facts("plan", None, 0), ["plan", "medium"], &[]);
    assert_eq!(
        (&r["level"], &r["prefer"]),
        (&json!("complex-high"), &json!(["claude"]))
    );
    // A kind or risk the judgment could not tell is routed as medium risk.
    let r = route_for(task_facts("", None, 0), ["unknown", "unknown"], &[]);
    assert_eq!(r["level"], "standard-high");
    assert_eq!(r["harnesses"], json!(["claude", "codex"]));
    assert!(codes(&r).contains(&"risk_assumed"));
}
