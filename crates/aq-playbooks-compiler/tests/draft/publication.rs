use super::*;

#[test]
fn the_header_is_written_by_the_tool_and_lines_shift() {
    let r = record();
    let (text, lines) = source_text(&r, &strings(&["main"]), &strings(&["levels"]), MARKDOWN);
    assert_eq!(lines, 3);
    assert!(text.ends_with(MARKDOWN));
    let header = aq_playbooks_compiler::parse_source_header(&text).unwrap();
    assert_eq!(
        header,
        SourceHeader {
            format_version: FORMAT_VERSION,
            id: "aq.routing".into(),
            scope: "project".into(),
            interfaces: strings(&["main"]),
            definitions: strings(&["levels"])
        }
    );
}

#[test]
fn mapped_steps_point_back_to_markdown() {
    let (_, lines) = source_text(
        &record(),
        &strings(&["main"]),
        &strings(&["levels"]),
        MARKDOWN,
    );
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
    assert!(has(
        &check(&ws),
        ("source_map", "draft.json:source_map.done")
    ));
}

#[test]
fn a_draft_cannot_override_the_source_header() {
    // The agent cannot write the header itself.
    let mut ws = workspace();
    ws.draft["header"] = json!({"id": "aq.routing"});
    assert!(has(&check(&ws), ("draft_format", "draft.json:header")));
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
        assert_eq!(
            (diagnostics[0].code.as_str(), &diagnostics[0].path),
            ("draft_identity", &format!("draft.json:{field}"))
        );
        assert!(
            diagnostics[0].message.contains("Leave"),
            "{}",
            diagnostics[0].message
        );
    }
}

#[test]
fn the_identity_written_by_the_tool_is_accepted() {
    // Carrying exactly what the tool writes is accepted.
    let mut ws = workspace();
    ws.draft["id"] = json!("aq.routing");
    ws.draft["scope"] = json!("project");
    ws.draft["required_capabilities"] = json!(["work.read", "work.route"]);
    assert!(check(&ws).is_empty(), "{:#?}", check(&ws));
}

#[test]
fn a_draft_refuses_embedded_identities_and_registries() {
    // Identities and registries cannot be smuggled in.
    let c = catalog();
    let mut ws = workspace();
    ws.draft["steps"]["read"]["op"]["command"] =
        json!({"name": "work.read", "fingerprint": c.commands["work.read"].fingerprint().unwrap()});
    let diagnostics = check(&ws);
    assert!(
        has(
            &diagnostics,
            ("draft_format", "draft.json:steps.read.op.command")
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
    assert!(has(&check(&ws), ("draft_format", "draft.json:registry")));
}

#[test]
fn a_return_operation_refuses_a_command_field() {
    // A stray `command` on an operation that takes none is an unknown field.
    let mut ws = workspace();
    ws.draft["steps"]["done"]["op"]["command"] = json!({"name": "work.read"});
    let diagnostics = check(&ws);
    assert!(
        has(&diagnostics, ("draft_format", "draft.json:steps.done.op")),
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
    assert_eq!(
        (c.change, c.abilities_changed),
        (ChangeKind::Presentation, false)
    );
    let mut behaviour = ws.clone();
    behaviour.draft["steps"]["route"]["op"]["input"]["fields"]["level"]["default"]["value"] =
        json!("complex");
    let c = compare(&before, &seal(&behaviour).unwrap()).unwrap();
    assert_eq!(
        (c.change, c.abilities_changed),
        (ChangeKind::Executable, false)
    );
    assert_eq!(c.diff.changed_steps, vec!["route"]);
}

#[test]
fn enabling_a_command_changes_the_abilities() {
    let ws = workspace();
    let before = seal(&ws).unwrap();
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
    assert_eq!(
        (c.change, c.abilities_changed),
        (ChangeKind::Executable, true)
    );
    assert_eq!(c.diff.changed_commands, vec!["work.route"]);
    assert_eq!(c.diff.capabilities_added, strings(&["work.route"]));
}

#[test]
fn comparing_an_artifact_to_itself_is_identical() {
    let before = seal(&workspace()).unwrap();
    let c = compare(&before, &before).unwrap();
    assert_eq!(
        (c.change, c.abilities_changed),
        (ChangeKind::Identical, false)
    );
}

#[test]
fn an_additional_entry_outcome_changes_the_interface() {
    let ws = workspace();
    let before = seal(&ws).unwrap();
    // A new outcome of the entry changes the interface.
    let mut interface = ws.clone();
    interface.draft["entries"]["main"]["outcomes"]["held"] = json!({"type": "null"});
    let c = compare(&before, &seal(&interface).unwrap()).unwrap();
    assert_eq!(
        (c.change, c.abilities_changed),
        (ChangeKind::Interface, false)
    );
}

#[test]
fn changing_a_used_contract_changes_abilities() {
    let ws = workspace();
    let before = seal(&ws).unwrap();
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
    assert_eq!(
        (c.change, c.abilities_changed),
        (ChangeKind::Executable, true)
    );
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
