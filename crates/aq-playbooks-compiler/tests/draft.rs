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
fn has(diagnostics: &[aq_playbooks_compiler::Diagnostic], code: &str, path: &str) -> bool {
    diagnostics.iter().any(|d| d.code == code && d.path == path)
}

#[test]
fn a_draft_with_names_fills_to_the_same_artifact_as_a_hand_built_proposal() {
    let ws = workspace();
    let sealed = seal(&ws).unwrap_or_else(|d| panic!("{d:#?}"));
    // The same playbook, built by hand the way builders do today.
    let c = catalog();
    let mut playbook = draft();
    for (step, name) in [("read", "work.read"), ("route", "work.route")] {
        playbook["steps"][step]["op"]["command"] =
            json!({"name": name, "fingerprint": c.commands[name].fingerprint().unwrap()});
    }
    playbook["triggers"][0]["event"] = json!({"name": "aq.work.route_needed",
        "fingerprint": c.events["aq.work.route_needed"].fingerprint().unwrap()});
    let object = playbook.as_object_mut().unwrap();
    object.remove("source_map");
    object.insert("id".into(), json!("aq.routing"));
    object.insert("scope".into(), json!("project"));
    object.insert(
        "required_capabilities".into(),
        json!(["work.read", "work.route"]),
    );
    object.insert(
        "limits".into(),
        serde_json::to_value(Limits::default()).unwrap(),
    );
    let header = SourceHeader {
        format_version: FORMAT_VERSION,
        id: "aq.routing".into(),
        scope: "project".into(),
        interfaces: strings(&["main"]),
        definitions: strings(&["levels"]),
    };
    let mut registry = c.clone();
    registry.commands.remove("judgment.assess");
    let levels = json!(["simple", "standard"]);
    let input = CompileInput {
        source: format!(
            "---aq-playbook\n{}\n---\n{MARKDOWN}",
            serde_json::to_string(&header).unwrap()
        ),
        proposal: Proposal {
            playbook: serde_json::from_value(playbook).unwrap(),
            source_map: BTreeMap::from([(
                "steps.route".into(),
                SourceLocation { line: 6, column: 1 },
            )]),
        },
        registry,
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::from([(
            "levels".into(),
            CapturedDefinition {
                version: "facts-1".into(),
                digest: digest(&levels).unwrap(),
                value: levels,
            },
        )]),
        compiler: COMPILER_VERSION.into(),
    };
    let by_hand = compile(&input).unwrap_or_else(|d| panic!("{d:#?}"));
    assert_eq!(sealed.digest, by_hand.digest);
    assert_eq!(
        canonical_bytes(&sealed).unwrap(),
        canonical_bytes(&by_hand).unwrap()
    );
    assert_eq!(fill(&ws).unwrap().source, input.source);
    assert!(check(&ws).is_empty());
}

#[test]
fn an_unknown_command_names_the_allowed_ones() {
    let mut ws = workspace();
    ws.draft["steps"]["read"]["op"]["command"] = json!("work.reed");
    let diagnostics = check(&ws);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert_eq!(d.code, "unknown_command");
    assert_eq!(d.path, "draft.json:steps.read.op.command");
    assert!(d.message.contains("\"work.reed\""), "{}", d.message);
    assert!(d.message.contains("work.read, work.route"), "{}", d.message);
    assert!(!d.message.contains("fingerprint"), "{}", d.message);
    // In the catalog is not enough: the source record must allow it.
    ws.draft["steps"]["read"]["op"]["command"] = json!("judgment.assess");
    let diagnostics = check(&ws);
    assert!(has(
        &diagnostics,
        "unknown_command",
        "draft.json:steps.read.op.command"
    ));
    let (_, allowed) = diagnostics[0]
        .message
        .split_once("closest first): ")
        .unwrap();
    assert!(!allowed.contains("judgment"), "{}", diagnostics[0].message);
    assert!(allowed.contains("work.read") && allowed.contains("work.route"));
    // Events: a wait or a trigger on an event outside the record.
    let mut ws = workspace();
    ws.draft["triggers"][0]["event"] = json!("aq.work.route_neded");
    let diagnostics = check(&ws);
    assert!(
        has(
            &diagnostics,
            "unknown_event",
            "draft.json:triggers[0].event"
        ),
        "{diagnostics:#?}"
    );
    assert!(diagnostics[0].message.contains("aq.work.route_needed"));
    // A call: an authoring source is offered no callable playbooks.
    let mut ws = workspace();
    ws.draft["steps"]["read"]["op"] = json!({"kind": "call", "dependency": "review",
        "entry": "main", "input": {"op": "literal", "value": null}, "mode": "wait", "bind": "work"});
    let diagnostics = check(&ws);
    assert!(
        has(&diagnostics, "call", "draft.json:steps.read.op"),
        "{diagnostics:#?}"
    );
    assert!(
        diagnostics[0].message.contains("work.read, work.route"),
        "{diagnostics:#?}"
    );
}

#[test]
fn an_async_command_brings_its_reconciliation_command_and_completion_event() {
    let catalog: Registry = serde_json::from_str(include_str!(
        "../../../contracts/aq-host-2.1.0/registry.json"
    ))
    .unwrap();
    let (registry, capabilities) =
        retain_registry(&catalog, &strings(&["judgment.request"]), &BTreeSet::new()).unwrap();
    assert_eq!(
        registry.commands.keys().cloned().collect::<BTreeSet<_>>(),
        strings(&["judgment.request", "operation.get"])
    );
    assert_eq!(
        registry.events.keys().cloned().collect::<BTreeSet<_>>(),
        strings(&["aq.judgment.completed"])
    );
    assert_eq!(
        capabilities,
        strings(&["judgment.request.v2", "operation.get"])
    );
    assert_eq!(
        registry.commands["operation.get"],
        catalog.commands["operation.get"]
    );
    let err =
        retain_registry(&catalog, &strings(&["judgment.reqest"]), &BTreeSet::new()).unwrap_err();
    assert!(err.contains("judgment.reqest"), "{err}");

    let ask = json!({"question": "Should this change be approved?",
        "subject": {"id": "work-1", "revision": 1},
        "context": {"text": "Work item summary.", "artifacts": []},
        "output": {"choices": ["approve", "decline"], "rationale_max_bytes": 2048},
        "budget": {"deadline_ms": 60000, "max_cost_microunits": 50000,
                   "max_input_tokens": 8192, "max_output_tokens": 512}});
    let ws = Workspace {
        markdown: "# Ask\n".into(),
        record: SourceRecord {
            id: "ask".into(),
            binding: "ask".into(),
            scope: "project".into(),
            purpose: "Ask".into(),
            commands: strings(&["judgment.request"]),
            events: BTreeSet::new(),
            capabilities: strings(&["judgment.request.v2", "operation.get"]),
            facts_version: String::new(),
        },
        catalog: catalog.clone(),
        facts: BTreeMap::new(),
        draft: json!({
            "purpose": "Ask once",
            "entries": {"main": {"input": {"type": "null"}, "outcomes": {"done": {"type": "null"}}, "start": "ask"}},
            "steps": {
                "ask": {"op": {"kind": "invoke", "command": "judgment.request", "bind": "j", "retry": null,
                               "input": {"op": "literal", "value": ask}},
                        "edges": {"accepted": "done", "assessed": "done", "failed": "done", "refused": "done"},
                        "on_error": "done"},
                "done": ret("done")},
            "triggers": []
        }),
    };
    let input = fill(&ws).unwrap_or_else(|d| panic!("{d:#?}"));
    assert_eq!(input.registry, registry);
    assert_eq!(input.proposal.playbook.required_capabilities, capabilities);
    assert!(check(&ws).is_empty(), "{:#?}", check(&ws));
    // The reconciliation command's capability must be one the source may use.
    let mut narrow = ws.clone();
    narrow.record.capabilities = strings(&["judgment.request.v2"]);
    let diagnostics = check(&narrow);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].code, "capability");
    assert_eq!(diagnostics[0].path, "draft.json:steps.ask.op.command");
    assert!(
        diagnostics[0].message.contains("operation.get"),
        "{}",
        diagnostics[0].message
    );
}

#[test]
fn the_header_is_written_by_the_tool_and_lines_shift() {
    let r = record();
    let (text, lines) = source_text(&r, &strings(&["main"]), &strings(&["levels"]), MARKDOWN);
    assert_eq!(lines, 3);
    assert!(text.ends_with(MARKDOWN));
    let header = aq_playbooks_compiler::parse_source_header(&text).unwrap();
    assert_eq!(header.id, "aq.routing");
    assert_eq!(header.scope, "project");
    assert_eq!(header.interfaces, strings(&["main"]));
    assert_eq!(header.definitions, strings(&["levels"]));
    // A diagnostic on a step mapped to source.md line 12 reports line 12 + header lines.
    let mut ws = workspace();
    ws.draft["source_map"] = json!({"done": 12});
    ws.draft["steps"]["done"]["op"]["outcome"] = json!("finished");
    let diagnostics = check(&ws);
    let d = diagnostics
        .iter()
        .find(|d| d.path == "draft.json:steps.done")
        .unwrap_or_else(|| panic!("{diagnostics:#?}"));
    // Ruling 2026-11-13 (A3 review I1), replacing the brief's `12 + lines`:
    // line and column are only ever positions in draft.json, so a mapped step
    // carries none; its message names the source.md line.
    assert_eq!((d.line, d.column), (None, None));
    assert!(d.message.contains("source.md line 12"), "{}", d.message);
    assert_eq!(
        fill(&ws).unwrap().proposal.source_map["steps.done"].line,
        12 + lines
    );
    // A line past the end of source.md is refused where it is written.
    ws.draft["source_map"] = json!({"done": 13});
    assert!(has(&check(&ws), "source_map", "draft.json:source_map.done"));
    // The agent cannot write the header itself.
    let mut ws = workspace();
    ws.draft["header"] = json!({"id": "aq.routing"});
    assert!(has(&check(&ws), "draft_format", "draft.json:header"));
}

#[test]
fn unknown_fields_are_still_refused_with_a_draft_position() {
    let dir = std::env::temp_dir().join(format!("aq-draft-{}-unknown", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ws = workspace();
    let mut text = serde_json::to_string_pretty(&ws.draft).unwrap();
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
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let read = Workspace::read(&dir).unwrap_or_else(|d| panic!("{d:#?}"));
    assert_eq!(read, ws);
    // "stepz" at top level.
    text = text.replacen("\"steps\"", "\"stepz\"", 1);
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let diagnostics = Workspace::read(&dir).unwrap_err();
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert_eq!(d.code, "draft_format");
    assert_eq!(d.path, "draft.json:stepz");
    let line = text.lines().position(|l| l.contains("\"stepz\"")).unwrap() as u32 + 1;
    assert_eq!(d.line, Some(line));
    assert!(d.column.is_some());
    assert!(d.message.contains("unknown field `stepz`"), "{}", d.message);
    assert!(
        d.message.starts_with(&format!("draft.json line {line}")),
        "{}",
        d.message
    );
    // Deeper, the JSON path names the step.
    let text = serde_json::to_string_pretty(&ws.draft).unwrap().replacen(
        "\"on_error\": \"failed\"",
        "\"on_eror\": \"failed\"",
        1,
    );
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let diagnostics = Workspace::read(&dir).unwrap_err();
    assert_eq!(
        diagnostics[0].path, "draft.json:steps.read.on_eror",
        "{diagnostics:#?}"
    );
    // Inside an operation, which serde reads whole and reports at the end of
    // the step, the operation is named, at its own line.
    let text = serde_json::to_string_pretty(&ws.draft).unwrap().replacen(
        "\"command\": \"work.read\"",
        "\"comand\": \"work.read\"",
        1,
    );
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let diagnostics = Workspace::read(&dir).unwrap_err();
    assert_eq!(
        diagnostics[0].path, "draft.json:steps.read.op",
        "{diagnostics:#?}"
    );
    assert!(
        diagnostics[0].message.contains("unknown field `comand`"),
        "{diagnostics:#?}"
    );
    let lines: Vec<&str> = text.lines().collect();
    let comand = lines.iter().position(|l| l.contains("\"comand\"")).unwrap();
    let op = lines[..comand]
        .iter()
        .rposition(|l| l.trim() == "\"op\": {")
        .unwrap() as u32
        + 1;
    assert_eq!(diagnostics[0].line, Some(op), "{diagnostics:#?}");
    // And inside an expression, the expression.
    let text =
        serde_json::to_string_pretty(&ws.draft)
            .unwrap()
            .replacen("\"default\"", "\"defualt\"", 1);
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let diagnostics = Workspace::read(&dir).unwrap_err();
    assert_eq!(
        diagnostics[0].path, "draft.json:steps.route.op.input.fields.level",
        "{diagnostics:#?}"
    );
    assert!(
        diagnostics[0].message.contains("`defualt`"),
        "{diagnostics:#?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_draft_claiming_another_scope_or_capabilities_is_refused() {
    for (field, value) in [
        ("id", json!("other.playbook")),
        ("scope", json!("installation")),
        (
            "required_capabilities",
            json!(["work.read", "work.route", "judgment.assess"]),
        ),
    ] {
        let mut ws = workspace();
        ws.draft[field] = value;
        let diagnostics = check(&ws);
        assert_eq!(diagnostics.len(), 1, "{field}: {diagnostics:#?}");
        assert_eq!(diagnostics[0].code, "draft_identity");
        assert_eq!(diagnostics[0].path, format!("draft.json:{field}"));
        assert!(
            diagnostics[0].message.contains("Leave"),
            "{}",
            diagnostics[0].message
        );
    }
    // Carrying exactly what the tool writes is accepted.
    let mut ws = workspace();
    ws.draft["id"] = json!("aq.routing");
    ws.draft["scope"] = json!("project");
    ws.draft["required_capabilities"] = json!(["work.read", "work.route"]);
    assert!(check(&ws).is_empty(), "{:#?}", check(&ws));
    // Identities and registries cannot be smuggled in.
    let c = catalog();
    let mut ws = workspace();
    ws.draft["steps"]["read"]["op"]["command"] =
        json!({"name": "work.read", "fingerprint": c.commands["work.read"].fingerprint().unwrap()});
    let diagnostics = check(&ws);
    assert!(
        has(
            &diagnostics,
            "draft_format",
            "draft.json:steps.read.op.command"
        ),
        "{diagnostics:#?}"
    );
    assert!(
        diagnostics[0].message.contains("as a string"),
        "{}",
        diagnostics[0].message
    );
    let mut ws = workspace();
    ws.draft["registry"] = serde_json::to_value(&c).unwrap();
    assert!(has(&check(&ws), "draft_format", "draft.json:registry"));
    // A stray `command` on an operation that takes none is an unknown field.
    let mut ws = workspace();
    ws.draft["steps"]["done"]["op"]["command"] = json!({"name": "work.read"});
    let diagnostics = check(&ws);
    assert!(
        has(&diagnostics, "draft_format", "draft.json:steps.done.op"),
        "{diagnostics:#?}"
    );
    assert!(
        diagnostics[0].message.contains("unknown field `command`"),
        "{diagnostics:#?}"
    );
}

#[test]
fn compare_names_wording_only_behaviour_and_abilities() {
    let ws = workspace();
    let before = seal(&ws).unwrap();
    let mut wording = ws.clone();
    wording.markdown = MARKDOWN.replace("Chores are simple.", "A chore is simple.");
    let c = compare(&before, &seal(&wording).unwrap()).unwrap();
    assert_eq!(c.change, ChangeKind::Presentation);
    assert!(!c.abilities_changed);
    let mut behaviour = ws.clone();
    behaviour.draft["steps"]["route"]["op"]["input"]["fields"]["level"]["default"]["value"] =
        json!("complex");
    let c = compare(&before, &seal(&behaviour).unwrap()).unwrap();
    assert_eq!(c.change, ChangeKind::Executable);
    assert!(!c.abilities_changed);
    assert_eq!(c.diff.changed_steps, vec!["route"]);
    // A version that never routes, then one that does: a new command.
    let mut reads_only = ws.clone();
    reads_only.draft["steps"]["read"]["edges"]["read"] = json!("done");
    reads_only.draft["steps"]
        .as_object_mut()
        .unwrap()
        .remove("route");
    reads_only.draft["source_map"] = json!({});
    let older = seal(&reads_only).unwrap_or_else(|d| panic!("{d:#?}"));
    let c = compare(&older, &before).unwrap();
    assert_eq!(c.change, ChangeKind::Executable);
    assert!(c.abilities_changed);
    assert_eq!(c.diff.changed_commands, vec!["work.route"]);
    assert_eq!(c.diff.capabilities_added, strings(&["work.route"]));
    let c = compare(&before, &before).unwrap();
    assert_eq!(c.change, ChangeKind::Identical);
    assert!(!c.abilities_changed);
    // A new outcome of the entry changes the interface.
    let mut interface = ws.clone();
    interface.draft["entries"]["main"]["outcomes"]["held"] = json!({"type": "null"});
    let c = compare(&before, &seal(&interface).unwrap()).unwrap();
    assert_eq!(c.change, ChangeKind::Interface);
    assert!(!c.abilities_changed);
    // The same command name with a new contract that needs more.
    let mut contract = ws.clone();
    contract
        .catalog
        .commands
        .get_mut("work.route")
        .unwrap()
        .capabilities
        .insert("work.read".into());
    let c = compare(&before, &seal(&contract).unwrap()).unwrap();
    assert_eq!(c.change, ChangeKind::Executable);
    assert!(c.abilities_changed);
    assert_eq!(c.diff.changed_commands, vec!["work.route"]);
    assert!(c.diff.capabilities_added.is_empty());
}

#[test]
fn unfill_then_fill_round_trips() {
    let ws = workspace();
    let artifact = seal(&ws).unwrap();
    let draft = unfill(&artifact);
    let text = serde_json::to_string(&draft).unwrap();
    assert!(!text.contains("fingerprint"), "{text}");
    assert!(!text.contains("sha256:"), "{text}");
    for field in ["id", "scope", "required_capabilities", "limits"] {
        assert!(draft.get(field).is_none(), "{field}: {text}");
    }
    assert_eq!(draft["source_map"], json!({"route": 3}));
    assert_eq!(draft["steps"]["read"]["op"]["command"], json!("work.read"));
    assert_eq!(draft["triggers"][0]["event"], json!("aq.work.route_needed"));
    let again = Workspace {
        draft: draft.clone(),
        ..ws
    };
    let resealed = seal(&again).unwrap_or_else(|d| panic!("{d:#?}"));
    assert_eq!(
        canonical_bytes(&resealed).unwrap(),
        canonical_bytes(&artifact).unwrap()
    );
    assert_eq!(unfill(&resealed), draft);
}
