//! Host catalog 2.2.0 conformance: 2.1.0 byte-identical, plus task facts
//! (`work.read`), routes (`work.route`), structured judgments
//! (`judgment.assess`, `aq.judgment.assessed`) and `aq.work.route_needed`.
#[allow(dead_code)]
#[path = "support/schemas.rs"]
mod schemas;
use aq_playbooks_cli::catalog;
use aq_playbooks_compiler::draft::{SourceRecord, Workspace, check, seal};
use aq_playbooks_model::*;
use schemas::{enumeration, object, result, strings, subject, text};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

const ADDED_COMMANDS: [&str; 4] = [
    "judgment.assess",
    "work.read",
    "work.route",
    "work.unroutable",
];
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
    let covered = validate_fixtures(&registry, &fixtures);
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
}

#[test]
fn the_assessed_guarantee_requires_one_declared_label_per_field() {
    let registry = catalog::catalog_v2_2();
    let fixtures = fixtures();
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
}

#[test]
fn only_a_completed_assessment_can_read_the_labels() {
    let ws = routing_workspace();
    // The labels are read by position only where `judgment.assess` answered
    // `assessed` (one label per requested field): in the `route` step, which
    // only the join's `completed` edge reaches, and the join accepts only
    // `assessed`.
    fn label_reads(value: &Value, found: &mut usize) {
        match value {
            Value::Object(o) => {
                if o.get("op") == Some(&json!("ref"))
                    && o.get("path")
                        .and_then(Value::as_array)
                        .is_some_and(|p| p.iter().any(|s| s == "labels"))
                {
                    *found += 1;
                }
                o.values().for_each(|v| label_reads(v, found));
            }
            Value::Array(a) => a.iter().for_each(|v| label_reads(v, found)),
            _ => {}
        }
    }
    for (name, step) in ws.draft["steps"].as_object().unwrap() {
        let mut found = 0;
        label_reads(step, &mut found);
        assert_eq!(found > 0, name == "route", "{name}");
    }
    let edges_to_route: Vec<(&String, &String)> = ws.draft["steps"]
        .as_object()
        .unwrap()
        .iter()
        .flat_map(|(n, s)| {
            s["edges"]
                .as_object()
                .unwrap()
                .iter()
                .filter(|(_, t)| *t == "route")
                .map(move |(e, _)| (n, e))
        })
        .collect();
    assert_eq!(
        edges_to_route,
        [(&"await".to_string(), &"completed".to_string())]
    );
    assert_eq!(
        ws.draft["steps"]["await"]["op"]["accepted_outcomes"],
        json!(["assessed"])
    );
}

#[test]
fn unassessed_labels_are_refused() {
    // The strict draft rules still bite: reading the labels where the
    // judgment may have failed is refused.
    let mut wrong = routing_workspace();
    wrong.draft["steps"]["await"]["op"]["accepted_outcomes"] = json!(["assessed", "failed"]);
    let diagnostics = check(&wrong);
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code == "expression_type" && d.message.contains("labels")),
        "{diagnostics:#?}"
    );
}

#[test]
fn a_failed_judgment_cannot_be_read_at_route() {
    // And so is reading the judgment's answer on the fallback path, which
    // the judgment's `on_error` (nothing bound) also reaches.
    let mut wrong = routing_workspace();
    wrong.draft["steps"]["assume"]["op"]["input"]["body"]["fields"]["classification"] = json!({
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
}

#[test]
fn unknown_literal_reason_is_refused() {
    // A route that gives a reason outside the fixed list does not check.
    let mut wrong = routing_workspace();
    wrong.draft["steps"]["assume"]["op"]["input"]["body"]["fields"]["reasons"] = json!({"op": "literal",
        "value": [{"code": "because_i_said_so", "detail": "Free text cannot be a code."}]});
    let diagnostics = check(&wrong);
    assert!(
        diagnostics.iter().any(|d| d.code == "schema_compatibility"
            && d.path == "draft.json:steps.assume"
            && d.message.contains("because_i_said_so")),
        "{diagnostics:#?}"
    );
}

#[test]
fn unavailable_priority_is_refused() {
    // The task's priority is not a fact a playbook can read.
    let mut wrong = routing_workspace();
    wrong.draft["steps"]["ask"]["op"]["input"]["fields"]["context"]["fields"]["items"]["items"]
        [0]["fields"]["text"] = json!({"op": "ref", "root": "task", "path": ["facts", "priority"]});
    let diagnostics = check(&wrong);
    assert!(
        diagnostics
            .iter()
            .any(|d| d.path == "draft.json:steps.ask" && d.message.contains("priority")),
        "{diagnostics:#?}"
    );
}

#[test]
fn unknown_computed_reason_is_refused() {
    // Also when the reason is chosen by the playbook's own filter and map.
    let mut wrong = routing_workspace();
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

/// The route the draft writes for one task: the `route` step's input after
/// an `assessed` answer, or with `labels: None` the `assume` step's input,
/// which the run reaches when the task could not be classified.
fn route_for(task: &Task) -> Value {
    let ws = routing_workspace();
    let step = if task.labels.is_some() {
        "route"
    } else {
        "assume"
    };
    let input: Expr =
        serde_json::from_value(ws.draft["steps"][step]["op"]["input"].clone()).unwrap();
    let subject = json!({"id": "task-1", "revision": 3});
    let facts = reported_facts(task);
    let v22 = catalog::catalog_v2_2();
    let read = json!({"reference": "task-1@3", "subject": subject, "facts": facts});
    v22.commands["work.read"].outcomes["observed"]
        .validate(&read)
        .unwrap();
    let mut context = EvalContext::from([
        ("task".into(), EvalValue::Known(read)),
        (
            "definitions".into(),
            EvalValue::Known(json!(
                ws.facts.into_iter().collect::<serde_json::Map<_, _>>()
            )),
        ),
    ]);
    if let Some([kind, risk]) = task.labels {
        context.insert(
            "answer".into(),
            EvalValue::Known(
                json!({"c": {"participant": "judgment:op-1", "outcome": "assessed",
                "output": {"reference": "judgment:op-1", "subject": subject, "labels": [kind, risk],
                    "flags": task.flags, "rationale": "Because.",
                    "provenance": fixtures()["assessed_guarantee"]["assessed"]["provenance"]}}}),
            ),
        );
    }
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
        let restricts = list != "prefer" && !asked.is_empty();
        if restricts && left.is_empty() {
            return None;
        }
        kept[list] = json!(left);
    }
    Some(kept)
}
#[path = "catalog_v2_2/routing_cases.rs"]
mod routing_cases;
use routing_cases::{Expected, HARD, NT, ROWS, Task, failed, hint, task};

#[test]
fn the_routing_draft_routes_as_the_document_says() {
    assert!(ROWS.len() >= 16);
    the_rows_run_as_cases(ROWS);
    for (name, task, want) in ROWS {
        let r = route_for(task);
        let codes: Vec<&str> = r["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["code"].as_str().unwrap())
            .collect();
        for (field, expected) in [
            ("level", json!(want.level)),
            ("harnesses", json!(want.harnesses)),
            ("providers", json!([])),
            ("prefer", json!(want.prefer)),
            ("hold", json!(want.hold)),
        ] {
            assert_eq!(r[field], expected, "{name}: {field}");
        }
        assert_eq!(codes, want.codes, "{name}");
        if task.labels.is_some() {
            assert_eq!(
                r["classification"]["fields"],
                json!(["kind", "risk"]),
                "{name}"
            );
            assert_eq!(r["classification"]["reference"], "judgment:op-1", "{name}");
        } else {
            assert_eq!(r["classification"], Value::Null, "{name}");
        }
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
}

#[test]
fn hints_above_the_routing_ceiling_explain_why_they_were_lowered() {
    // A hint above its top is lowered for the true reason (lines 30, 31, 77).
    for (t, said) in [
        (
            failed(hint(task("bugfix", "high", HARD), "deep-high"), 2),
            "Never spend Deep on bug fixes",
        ),
        (
            hint(task("feature", "low", &[]), "deep-high"),
            "Ordinary implementation work doesn't go above Complex",
        ),
        (
            hint(task("docs", "low", &[]), "deep-high"),
            "Tests, docs, chores and syncs don't go above Standard",
        ),
        (
            hint(task("review", "low", &[]), "deep-high"),
            "Asking for Deep is not enough to get it",
        ),
    ] {
        let r = route_for(&t);
        let details: Vec<&str> = r["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["detail"].as_str())
            .collect();
        assert!(
            details.iter().any(|d| d.starts_with(said)),
            "{said}: {details:?}"
        );
    }
}

#[test]
fn an_unavailable_preference_keeps_the_route_usable() {
    // The host drops a preference the level does not offer; it never refuses.
    let spec = route_for(&task("spec", "low", NT));
    assert_eq!(host_keeps(&spec).unwrap()["prefer"], json!([]));
}

/// "If nothing allowed could ever take it, leave it unrouted and visible, say
/// why on the task, and raise it for my attention." Nothing in this document
/// allows nobody, so the draft never calls `work.unroutable`; a `not_offered`
/// refusal is raised by the host, and the run just ends.
#[test]
fn the_routing_draft_ends_plainly_when_its_route_is_refused() {
    let ws = routing_workspace();
    let steps = ws.draft["steps"].as_object().unwrap();
    for step in ["route", "assume"] {
        assert_eq!(steps[step]["edges"]["refused"], "refused");
        assert_eq!(steps[step]["edges"]["stale"], "skip");
    }
    assert_eq!(steps["refused"]["op"]["kind"], "return");
    assert!(
        steps
            .values()
            .all(|s| s["op"]["command"] != "work.unroutable")
    );
    let outcomes = ws.draft["entries"]["main"]["outcomes"].as_object().unwrap();
    assert!(!outcomes.contains_key("unservable"));
}

/// A document rule that allows nobody ("Secrets rotation is never given to an
/// agent") is written with `work.unroutable` and the shared reason schema.
#[test]
fn a_document_rule_that_allows_nobody_uses_work_unroutable() {
    let mut ws = routing_workspace();
    ws.record.commands = strings(&["work.read", "work.route", "work.unroutable"]);
    ws.record.capabilities = strings(&["work.read", "work.route", "work.unroutable"]);
    ws.draft = json!({
        "entries": {"main": {
            "input": ws.draft["entries"]["main"]["input"],
            "outcomes": {"routed": {"type": "null"}, "unroutable": {"type": "null"},
                         "ended": {"type": "null"}},
            "start": "read"}},
        "triggers": ws.draft["triggers"],
        "steps": {
            "read": {"op": {"kind": "invoke", "command": "work.read", "bind": "task", "retry": null,
                "input": {"op": "object", "fields": {"subject": {"op": "ref", "root": "input", "path": ["task"]}}}},
                "edges": {"observed": "secrets", "refused": "ended"}, "on_error": "ended"},
            "secrets": {"op": {"kind": "decide", "predicate": {"op": "eq",
                    "left": {"op": "ref", "root": "task", "path": ["facts", "kind"]},
                    "right": {"op": "literal", "value": "secrets_rotation"}}},
                "edges": {"true": "nobody", "false": "route", "unknown": "ended"}, "on_error": null},
            "nobody": {"op": {"kind": "invoke", "command": "work.unroutable", "bind": "noted", "retry": null,
                "input": {"op": "object", "fields": {
                    "subject": {"op": "ref", "root": "task", "path": ["subject"]},
                    "reasons": {"op": "literal", "value": [{"code": "policy",
                        "detail": "Secrets rotation is never given to an agent."}]}}}},
                "edges": {"recorded": "unroutable", "stale": "ended", "refused": "ended"}, "on_error": "ended"},
            "route": {"op": {"kind": "invoke", "command": "work.route", "bind": "routed", "retry": null,
                "input": {"op": "object", "fields": {
                    "subject": {"op": "ref", "root": "task", "path": ["subject"]},
                    "level": {"op": "literal", "value": "standard-high"},
                    "harnesses": {"op": "literal", "value": []},
                    "providers": {"op": "literal", "value": []},
                    "prefer": {"op": "literal", "value": []},
                    "hold": {"op": "literal", "value": false},
                    "reasons": {"op": "literal", "value": [{"code": "level_from_kind"}]},
                    "classification": {"op": "literal", "value": null}}}},
                "edges": {"routed": "routed", "stale": "ended", "refused": "ended"}, "on_error": "ended"},
            "routed": {"op": {"kind": "return", "outcome": "routed", "output": {"op": "literal", "value": null}}, "edges": {}, "on_error": null},
            "unroutable": {"op": {"kind": "return", "outcome": "unroutable", "output": {"op": "literal", "value": null}}, "edges": {}, "on_error": null},
            "ended": {"op": {"kind": "return", "outcome": "ended", "output": {"op": "literal", "value": null}}, "edges": {}, "on_error": null},
        },
    });
    let diagnostics = check(&ws);
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    // A reason outside the code list is refused here as for a route.
    ws.draft["steps"]["nobody"]["op"]["input"]["fields"]["reasons"]["value"][0]["code"] =
        json!("nobody_may");
    let diagnostics = check(&ws);
    assert!(
        diagnostics
            .iter()
            .any(|d| d.code == "schema_compatibility" && d.path == "draft.json:steps.nobody"),
        "{diagnostics:#?}"
    );
}

/// The routing draft against the draft cost bounds (A2 re-review round 4,
/// I2): a reference counts the type it reads, so a table bound once and its
/// result read many times stays far below the bounds.
#[test]
fn the_routing_draft_uses_a_small_part_of_the_cost_bounds() {
    let sealed = seal(&routing_workspace()).unwrap_or_else(|d| panic!("{d:#?}"));
    let size = sealed.content.draft_size();
    println!("routing draft: {size:?}");
    assert!(
        size.largest_expression * 10 < MAX_DRAFT_EXPANSION,
        "{size:?}"
    );
    assert!(
        size.expressions * 10 < MAX_DRAFT_EXPANSION_TOTAL,
        "{size:?}"
    );
    assert!(size.scope * 10 < MAX_DRAFT_SCOPE, "{size:?}");
}

/// The same rows as authoring cases, run by the simulation library on the
/// sealed draft from the trigger event. The runtime writes, as `work.route`'s
/// input, exactly the route the expressions evaluate to: an `assessed`
/// answer reaches `route` through the named join, and a failed judgment
/// takes `assume`.
fn the_rows_run_as_cases(rows: &[(&str, Task, Expected)]) {
    let artifact = seal(&routing_workspace()).unwrap_or_else(|d| panic!("{d:#?}"));
    let subject = json!({"id": "task-1", "revision": 3});
    for (name, task, want) in rows {
        let case = routing_case(name, task, &subject);
        let route = &case.expect.calls[2].input;
        assert_eq!(route["level"], want.level, "{name}");
        assert_eq!(route["harnesses"], json!(want.harnesses), "{name}");
        assert_eq!(route["prefer"], json!(want.prefer), "{name}");
        assert_eq!(route["hold"], want.hold, "{name}");
        let report = run_case(&artifact, &case).unwrap();
        assert!(report.passed, "{name}: {report:#?}");
        // The full list of calls holds each row to exactly one route.
        let routes: Vec<_> = report
            .calls
            .iter()
            .filter(|c| c.command == "work.route")
            .collect();
        assert_eq!(routes.len(), 1, "{name}");
        let route = routes[0];
        let step = if task.labels.is_some() {
            "route"
        } else {
            "assume"
        };
        assert_eq!(route.step, step, "{name}");
        assert_eq!(route.input, route_for(task), "{name}");
    }
}

/// The `ask` step's input for one task, as the draft writes it.
fn ask_for(task: &Task) -> Value {
    let ws = routing_workspace();
    let input: Expr =
        serde_json::from_value(ws.draft["steps"]["ask"]["op"]["input"].clone()).unwrap();
    let facts = reported_facts(task);
    let read = json!({"reference": "task-1@3", "subject": {"id": "task-1", "revision": 3},
        "facts": facts});
    let context = EvalContext::from([
        ("task".into(), EvalValue::Known(read)),
        (
            "definitions".into(),
            EvalValue::Known(json!(
                ws.facts.into_iter().collect::<serde_json::Map<_, _>>()
            )),
        ),
    ]);
    let EvalValue::Known(ask) = eval(&input, &context, &EvalLimits::default()).unwrap() else {
        panic!("the question is known")
    };
    ask
}

#[path = "catalog_v2_2/registry.rs"]
mod registry;
use registry::{array, build_v2_2, code, level, string};

#[path = "catalog_v2_2/manifest.rs"]
mod manifest;

fn reported_facts(task: &Task) -> Value {
    json!({"kind": task.filed_kind, "title": "A task", "description": "",
        "origin": "owner", "level_hint": task.hint, "charged_failures": task.failures,
        "ended_attempts": task.failures, "route": "none"})
}

use aq_playbooks_simulation::{Case, CaseReturn, CaseStart, Expectation, ExpectedCall, run_case};
fn expected_route_calls(task: &Task, subject: &Value) -> Expectation {
    Expectation {
        outcome: "routed".into(),
        calls: vec![
            ExpectedCall {
                command: "work.read".into(),
                input: json!({"subject": subject}),
            },
            ExpectedCall {
                command: "judgment.assess".into(),
                input: ask_for(task),
            },
            ExpectedCall {
                command: "work.route".into(),
                input: route_for(task),
            },
        ],
    }
}

fn routing_case(name: &str, task: &Task, subject: &Value) -> Case {
    let facts = reported_facts(task);
    let judgment = match task.labels {
        Some([kind, risk]) => CaseReturn::Answer {
            outcome: "assessed".into(),
            output: json!({"reference": "judgment:op-1", "subject": subject,
                    "labels": [kind, risk], "flags": task.flags, "rationale": "Because.",
                    "provenance": fixtures()["assessed_guarantee"]["assessed"]["provenance"]}),
        },
        None => CaseReturn::Answer {
            outcome: "failed".into(),
            output: json!({"reference": "judgment:op-1", "subject": subject,
                    "reason": "provider_unavailable"}),
        },
    };
    Case {
        name: name.to_string(),
        situation: name.to_string(),
        start: CaseStart::Event {
            event: "aq.work.route_needed".into(),
            payload: json!({"subject": subject}),
            correlation: BTreeMap::new(),
        },
        returns: BTreeMap::from([
            (
                "work.read".to_string(),
                vec![CaseReturn::Answer {
                    outcome: "observed".into(),
                    output: json!({"reference": "task-1@3", "subject": subject, "facts": facts}),
                }],
            ),
            ("judgment.assess".to_string(), vec![judgment]),
            (
                "work.route".to_string(),
                vec![CaseReturn::Answer {
                    outcome: "routed".into(),
                    output: json!({"reference": "route-1", "subject": subject,
                            "harnesses": [], "providers": [], "prefer": []}),
                }],
            ),
        ]),
        // Every call the run makes, each with its whole input: the read,
        // the question, and the one route the document gives.
        expect: expected_route_calls(task, subject),
    }
}

#[path = "support/catalog_fixtures.rs"]
mod catalog_fixtures;
use catalog_fixtures::{fingerprints, pretty, validate_fixtures};
