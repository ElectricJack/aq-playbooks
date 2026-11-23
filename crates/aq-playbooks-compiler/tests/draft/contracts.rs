use super::*;

#[test]
fn a_draft_with_names_fills_to_the_same_artifact_as_a_hand_built_proposal() {
    let ws = workspace();
    let sealed = seal(&ws).unwrap_or_else(|d| panic!("{d:#?}"));
    let input = hand_built_input();
    let by_hand = compile(&input).unwrap_or_else(|d| panic!("{d:#?}"));
    assert_eq!(
        (&sealed.digest, canonical_bytes(&sealed).unwrap()),
        (&by_hand.digest, canonical_bytes(&by_hand).unwrap())
    );
    assert_eq!(fill(&ws).unwrap().source, input.source);
    assert!(check(&ws).is_empty());
}

fn hand_built_input() -> CompileInput {
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
    CompileInput {
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
    }
}

#[test]
fn an_unknown_command_names_the_allowed_ones() {
    let mut ws = workspace();
    ws.draft["steps"]["read"]["op"]["command"] = json!("work.reed");
    let diagnostics = check(&ws);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert_eq!(
        (d.code.as_str(), d.path.as_str()),
        ("unknown_command", "draft.json:steps.read.op.command")
    );
    for fragment in ["\"work.reed\"", "work.read, work.route"] {
        assert!(d.message.contains(fragment), "{}", d.message);
    }
    assert!(!d.message.contains("fingerprint"), "{}", d.message);
}

#[test]
fn an_unpermitted_command_lists_only_permitted_suggestions() {
    let mut ws = workspace();
    // In the catalog is not enough: the source record must allow it.
    ws.draft["steps"]["read"]["op"]["command"] = json!("judgment.assess");
    let diagnostics = check(&ws);
    assert!(has(
        &diagnostics,
        ("unknown_command", "draft.json:steps.read.op.command")
    ));
    let (_, allowed) = diagnostics[0]
        .message
        .split_once("closest first): ")
        .unwrap();
    assert!(!allowed.contains("judgment"), "{}", diagnostics[0].message);
    assert!(allowed.contains("work.read") && allowed.contains("work.route"));
}

#[test]
fn an_unknown_trigger_event_names_the_allowed_event() {
    // Events: a wait or a trigger on an event outside the record.
    let mut ws = workspace();
    ws.draft["triggers"][0]["event"] = json!("aq.work.route_neded");
    let diagnostics = check(&ws);
    assert!(
        has(
            &diagnostics,
            ("unknown_event", "draft.json:triggers[0].event")
        ),
        "{diagnostics:#?}"
    );
    assert!(diagnostics[0].message.contains("aq.work.route_needed"));
}

#[test]
fn calls_offer_the_available_commands() {
    // A call: an authoring source is offered no callable playbooks.
    let mut ws = workspace();
    ws.draft["steps"]["read"]["op"] = json!({"kind": "call", "dependency": "review",
        "entry": "main", "input": {"op": "literal", "value": null}, "mode": "wait", "bind": "work"});
    let diagnostics = check(&ws);
    assert!(
        has(&diagnostics, ("call", "draft.json:steps.read.op")),
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
        "../../../../contracts/aq-host-2.1.0/registry.json"
    ))
    .unwrap();
    let (registry, capabilities) =
        retain_registry(&catalog, &strings(&["judgment.request"]), &BTreeSet::new()).unwrap();
    assert_eq!(
        (
            registry.commands.keys().cloned().collect::<BTreeSet<_>>(),
            registry.events.keys().cloned().collect::<BTreeSet<_>>()
        ),
        (
            strings(&["judgment.request", "operation.get"]),
            strings(&["aq.judgment.completed"])
        )
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

    let ws = judgment_workspace(catalog);
    let input = fill(&ws).unwrap_or_else(|d| panic!("{d:#?}"));
    assert_eq!(input.registry, registry);
    assert_eq!(input.proposal.playbook.required_capabilities, capabilities);
    assert!(check(&ws).is_empty(), "{:#?}", check(&ws));
    // The reconciliation command's capability must be one the source may use.
    let mut narrow = ws.clone();
    narrow.record.capabilities = strings(&["judgment.request.v2"]);
    let diagnostics = check(&narrow);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        (diagnostics[0].code.as_str(), diagnostics[0].path.as_str()),
        ("capability", "draft.json:steps.ask.op.command")
    );
    assert!(
        diagnostics[0].message.contains("operation.get"),
        "{}",
        diagnostics[0].message
    );
}

fn judgment_workspace(catalog: Registry) -> Workspace {
    let ask = json!({"question": "Should this change be approved?",
        "subject": {"id": "work-1", "revision": 1},
        "context": {"text": "Work item summary.", "artifacts": []},
        "output": {"choices": ["approve", "decline"], "rationale_max_bytes": 2048},
        "budget": {"deadline_ms": 60000, "max_cost_microunits": 50000,
                   "max_input_tokens": 8192, "max_output_tokens": 512}});
    Workspace {
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
        catalog,
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
    }
}
