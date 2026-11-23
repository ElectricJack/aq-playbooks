use super::*;

fn unbound(ws: &Workspace, step: &str, texts: &[&str]) {
    let diagnostics = check(ws);
    let d = diagnostics
        .iter()
        .find(|d| d.path == format!("draft.json:steps.{step}"))
        .unwrap_or_else(|| panic!("{diagnostics:#?}"));
    assert_eq!(d.code, "expression_type", "{diagnostics:#?}");
    for text in texts {
        assert!(d.message.contains(text), "{text}: {}", d.message);
    }
    // The stored form of the same draft still verifies.
    let mut input = fill(ws).unwrap();
    input.proposal.source_map.clear();
    let header = aq_playbooks_compiler::parse_source_header(&input.source).unwrap();
    let content = ArtifactContent {
        format_version: FORMAT_VERSION,
        compiler: input.compiler.clone(),
        source: SourceBinding {
            digest: hash_bytes(input.source.as_bytes()),
            header,
        },
        playbook: input.proposal.playbook,
        registry: input.registry,
        dependencies: input.dependencies,
        definitions: input.definitions,
        source_map: BTreeMap::new(),
    };
    assert!(Artifact::seal(content).unwrap().verify().is_ok());
}

#[test]
fn a_shared_step_after_branches_cannot_read_what_only_one_branch_bound() {
    // The routing draft's natural single final step: `done` returns
    // `routed.level`, but the `missing` way goes through `assume`, which binds
    // `assumed`, never `routed`.
    let mut ws = object_outcomes();
    ws.draft["entries"]["main"]["outcomes"]["routed"] =
        serde_json::to_value(Schema::string(32)).unwrap();
    ws.draft["steps"]["read"]["edges"]["missing"] = json!("assume");
    let mut assume = route_step();
    assume["op"]["bind"] = json!("assumed");
    ws.draft["steps"]["assume"] = assume;
    ws.draft["steps"]["done"]["op"]["output"] = reference("routed", &["level"]);
    unbound(
        &ws,
        "done",
        &[
            "`routed.level` is not there on every way to this step",
            "the way from the start of entry `main` that does not pass step `route`, `routed` is not bound",
        ],
    );
    // Written as one return per route, it checks.
    ws.draft["steps"]["assume"]["edges"]["routed"] = json!("assumed_done");
    ws.draft["steps"]["assumed_done"] = json!({"op": {"kind": "return", "outcome": "routed",
        "output": reference("assumed", &["level"])}, "edges": {}, "on_error": null});
    assert!(check(&ws).is_empty(), "{:#?}", check(&ws));
}

#[test]
fn a_loop_cannot_read_on_its_first_pass_what_a_later_step_of_its_body_binds() {
    let mut ws = object_outcomes();
    ws.draft["entries"]["main"]["start"] = json!("each");
    ws.draft["steps"]["each"] = json!({"op": {"kind": "for_each",
        "collection": {"op": "literal", "value": ["a", "b"]}, "item": "t", "body": "note",
        "next": "done", "max_items": 2, "concurrency": 1, "on_failure": "stop"},
        "edges": {}, "on_error": "failed"});
    ws.draft["steps"]["note"] = decide(work_is_chore(), "read");
    ws.draft["steps"]["read"]["edges"] = json!({"read": "each", "missing": "each"});
    ws.draft["steps"].as_object_mut().unwrap().remove("route");
    unbound(
        &ws,
        "note",
        &[
            "`work.kind`",
            "that does not pass step `read`, `work` is not bound",
        ],
    );
    // Tested with `exists` first, it checks.
    ws.draft["steps"]["note"] = decide(
        json!({"op": "exists", "value": reference("work", &["kind"])}),
        "read",
    );
    assert!(check(&ws).is_empty(), "{:#?}", check(&ws));
}

#[test]
fn after_a_for_each_that_may_not_run_its_body_bindings_are_not_bound() {
    // Over an empty collection the body never runs, so `after` is reached
    // with `work` never bound.
    let mut ws = object_outcomes();
    ws.draft["entries"]["main"]["start"] = json!("each");
    ws.draft["steps"]["each"] = json!({"op": {"kind": "for_each",
        "collection": {"op": "literal", "value": []}, "item": "t", "body": "read",
        "next": "after", "max_items": 1, "concurrency": 1, "on_failure": "stop"},
        "edges": {}, "on_error": "failed"});
    ws.draft["steps"]["read"]["edges"] = json!({"read": "each", "missing": "each"});
    ws.draft["steps"]["after"] = decide(work_is_chore(), "done");
    ws.draft["steps"].as_object_mut().unwrap().remove("route");
    unbound(
        &ws,
        "after",
        &[
            "`work.kind`",
            "that does not pass step `read`, `work` is not bound",
        ],
    );
}

#[test]
fn the_entry_step_reached_again_by_a_back_edge_cannot_read_what_the_loop_binds() {
    // `each` is the entry's start and the anchor its body returns to; its
    // collection reads `work`, which only the body binds.
    let mut ws = object_outcomes();
    ws.draft["entries"]["main"]["start"] = json!("each");
    ws.draft["steps"]["each"] = json!({"op": {"kind": "for_each",
        "collection": reference("work", &["kinds"]), "item": "t", "body": "read",
        "next": "done", "max_items": 2, "concurrency": 1, "on_failure": "stop"},
        "edges": {}, "on_error": "failed"});
    ws.draft["steps"]["read"]["edges"] = json!({"read": "each", "missing": "each"});
    ws.draft["steps"].as_object_mut().unwrap().remove("route");
    unbound(
        &ws,
        "each",
        &[
            "`work.kinds`",
            "that does not pass step `read`, `work` is not bound",
        ],
    );
}
