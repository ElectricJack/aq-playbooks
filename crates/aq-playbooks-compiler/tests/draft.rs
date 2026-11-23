//! A draft names contracts; the tool fills identities, header and definitions.
use aq_playbooks_compiler::draft::*;
use aq_playbooks_compiler::{COMPILER_VERSION, ChangeKind, CompileInput, Proposal, compile};
use aq_playbooks_model::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn strings(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}
fn command(
    name: &str,
    input: Schema,
    outcomes: &[(&str, Schema)],
    effect: Effect,
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
        recovery: if effect == Effect::Observation {
            Recovery::Observation
        } else {
            Recovery::NaturallyIdempotent
        },
        effect,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 4096,
        deadline_ms: 1000,
    }
}
fn task() -> Schema {
    Schema::object(BTreeMap::from([("task".into(), Schema::string(64))]))
}
fn catalog() -> Registry {
    let read = command(
        "work.read",
        task(),
        &[
            (
                "read",
                Schema::object(BTreeMap::from([("kind".into(), Schema::string(32))])),
            ),
            ("missing", Schema::Null),
        ],
        Effect::Observation,
    );
    let route = command(
        "work.route",
        Schema::object(BTreeMap::from([
            ("task".into(), Schema::string(64)),
            ("level".into(), Schema::string(32)),
        ])),
        &[("routed", Schema::Null), ("stale", Schema::Null)],
        Effect::Mutation,
    );
    // In the catalog, but not a command this source may use.
    let assess = command(
        "judgment.assess",
        Schema::Null,
        &[("assessed", Schema::Null)],
        Effect::Observation,
    );
    let needed = EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: "aq.work.route_needed".into(),
        version: "1.0.0".into(),
        payload: task(),
        trusted_producers: strings(&["host"]),
        scopes: strings(&["project"]),
        correlation_fields: BTreeSet::new(),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 4096,
        retention_ms: 86_400_000,
    };
    Registry {
        commands: [read, route, assess]
            .into_iter()
            .map(|c| (c.name.clone(), c))
            .collect(),
        events: BTreeMap::from([(needed.name.clone(), needed)]),
    }
}
fn record() -> SourceRecord {
    SourceRecord {
        id: "routing".into(),
        binding: "aq.routing".into(),
        scope: "project".into(),
        purpose: "Route each task to a level".into(),
        commands: strings(&["work.read", "work.route"]),
        events: strings(&["aq.work.route_needed"]),
        capabilities: strings(&["work.read", "work.route"]),
        facts_version: "facts-1".into(),
    }
}
const MARKDOWN: &str =
    "# Routing\n\nChores are simple.\nEverything else is standard.\n\n\n\n\n\n\n\nA last line.\n";
fn reference(root: &str, path: &[&str]) -> Value {
    json!({"op": "ref", "root": root, "path": path})
}
fn ret(outcome: &str) -> Value {
    json!({"op": {"kind": "return", "outcome": outcome, "output": {"op": "literal", "value": null}},
           "edges": {}, "on_error": null})
}
fn read_step() -> Value {
    json!({"op": {"kind": "invoke", "command": "work.read", "bind": "work", "retry": null,
                  "input": {"op": "object", "fields": {"task": reference("input", &["task"])}}},
           "edges": {"read": "route", "missing": "failed"}, "on_error": "failed"})
}
fn route_step() -> Value {
    json!({"op": {"kind": "invoke", "command": "work.route", "bind": "routed", "retry": null,
                  "input": {"op": "object", "fields": {
                      "task": reference("input", &["task"]),
                      "level": {"op": "lookup", "key": reference("work", &["kind"]),
                                "cases": [{"when": "chore", "then": {"op": "literal", "value": "simple"}}],
                                "default": {"op": "literal", "value": "standard"}}}}},
           "edges": {"routed": "done", "stale": "failed"}, "on_error": "failed"})
}
fn draft() -> Value {
    json!({
        "purpose": "Route each task by its kind",
        "entries": {"main": {
            "input": {"type": "object", "properties": {"task": {"type": "string", "max_length": 64}},
                      "required": ["task"]},
            "outcomes": {"routed": {"type": "null"}, "failed": {"type": "null"}},
            "start": "read"}},
        "steps": {"read": read_step(), "route": route_step(), "done": ret("routed"), "failed": ret("failed")},
        "triggers": [{"id": "needed", "event": "aq.work.route_needed", "entry": "main",
                      "input": {"op": "object", "fields": {"task": reference("event", &["task"])}}}],
        "source_map": {"route": 3}
    })
}
fn workspace() -> Workspace {
    Workspace {
        markdown: MARKDOWN.into(),
        record: record(),
        catalog: catalog(),
        facts: BTreeMap::from([("levels".into(), json!(["simple", "standard"]))]),
        draft: draft(),
    }
}
fn has(diagnostics: &[aq_playbooks_compiler::Diagnostic], expected: (&str, &str)) -> bool {
    diagnostics
        .iter()
        .any(|d| (d.code.as_str(), d.path.as_str()) == expected)
}

// A binding read on a way that never bound it (A2 re-review R1-I2). Every
// outcome of these commands has the field, so ordinary typing accepts each
// read and only the draft check can refuse it.
fn kinds() -> Schema {
    Schema::object(BTreeMap::from([
        ("kind".into(), Schema::string(32)),
        (
            "kinds".into(),
            Schema::Array {
                items: Box::new(Schema::string(32)),
                max_items: 2,
            },
        ),
    ]))
}
fn object_outcomes() -> Workspace {
    let mut ws = workspace();
    let read = ws.catalog.commands.get_mut("work.read").unwrap();
    read.outcomes = BTreeMap::from([("read".into(), kinds()), ("missing".into(), kinds())]);
    let level = Schema::object(BTreeMap::from([("level".into(), Schema::string(32))]));
    let route = ws.catalog.commands.get_mut("work.route").unwrap();
    route.outcomes = BTreeMap::from([("routed".into(), level.clone()), ("stale".into(), level)]);
    ws.draft["source_map"] = json!({});
    ws
}
fn decide(predicate: Value, next: &str) -> Value {
    json!({"op": {"kind": "decide", "predicate": predicate},
           "edges": {"true": next, "false": next, "unknown": next}, "on_error": null})
}
fn work_is_chore() -> Value {
    is_chore(reference("work", &["kind"]))
}

#[test]
fn a_reused_binding_name_is_reported_without_the_costly_read_check() {
    let mut ws = workspace();
    ws.draft["steps"]["route"]["op"]["bind"] = json!("work");
    let diagnostics = check(&ws);
    assert!(
        diagnostics.iter().any(|d| d.code == "binding"),
        "{diagnostics:#?}"
    );
    // The fault names the other step that binds the name (fix round 2, N3).
    let binding = diagnostics.iter().find(|d| d.code == "binding").unwrap();
    assert!(
        binding.message.contains("binds `work`, which step `")
            && binding.message.contains("` already binds"),
        "{binding:#?}"
    );
    assert!(
        diagnostics
            .iter()
            .all(|d| !d.message.contains("is not there on every way")),
        "{diagnostics:#?}"
    );
}

// Computed aliases (A2 re-review round 2, I1). `read` reaches `peek` on both
// outcomes and `missing` is `{}`, with no `kind`; ordinary typing accepts
// every read here, because `kind` is merely optional in the merged type.
fn is_chore(value: Value) -> Value {
    json!({"op": "eq", "left": value, "right": {"op": "literal", "value": "chore"}})
}
fn let1(name: &str, value: Value, body: Value) -> Value {
    json!({"op": "let", "bindings": [{"name": name, "value": value}], "body": body})
}
fn keyed_by(key: Value) -> Value {
    json!({"op": "lookup", "key": key,
           "cases": [{"when": "chore", "then": {"op": "literal", "value": true}}],
           "default": {"op": "literal", "value": false}})
}
fn partial(predicate: Value) -> Workspace {
    let mut ws = object_outcomes();
    ws.catalog
        .commands
        .get_mut("work.read")
        .unwrap()
        .outcomes
        .insert("missing".into(), Schema::object(BTreeMap::new()));
    ws.draft["steps"]["read"]["edges"] = json!({"read": "peek", "missing": "peek"});
    ws.draft["steps"]["peek"] = decide(predicate, "route");
    ws.draft["steps"]["route"]["op"]["input"]["fields"]["level"]["key"] =
        reference("input", &["task"]);
    ws
}
/// The diagnostics at `peek`, and the reasons ("…this step: <reasons>.
/// Read it only…") of the first.
fn at_peek(predicate: Value) -> (Vec<aq_playbooks_compiler::Diagnostic>, String) {
    let diagnostics: Vec<_> = check(&partial(predicate))
        .into_iter()
        .filter(|d| d.path == "draft.json:steps.peek")
        .collect();
    let reasons = diagnostics
        .first()
        .and_then(|d| d.message.split_once("this step: "))
        .and_then(|(_, rest)| rest.split_once(". Read it only"))
        .map(|(reasons, _)| reasons.to_string())
        .unwrap_or_default();
    (diagnostics, reasons)
}

fn write_workspace_inputs(dir: &std::path::Path, ws: &Workspace) {
    std::fs::write(dir.join("source.md"), &ws.markdown).unwrap();
    std::fs::write(
        dir.join("record.json"),
        serde_json::to_vec(&ws.record).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("catalog.json"),
        serde_json::to_vec(&ws.catalog).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("facts.json"),
        serde_json::to_vec(&ws.facts).unwrap(),
    )
    .unwrap();
}

#[path = "draft/aliases.rs"]
mod aliases;

#[path = "draft/branches.rs"]
mod branches;

#[path = "draft/collections.rs"]
mod collections;

#[path = "draft/contracts.rs"]
mod contracts;

#[path = "draft/limits.rs"]
mod limits;

#[path = "draft/positions.rs"]
mod positions;

#[path = "draft/publication.rs"]
mod publication;
