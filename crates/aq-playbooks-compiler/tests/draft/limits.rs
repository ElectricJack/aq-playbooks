use super::*;

/// The fixture with `extra` decide steps chained in front of `read`.
fn chained(extra: usize) -> Workspace {
    let mut ws = workspace();
    let steps = ws.draft["steps"].as_object_mut().unwrap();
    for i in 0..extra {
        let next = if i + 1 == extra {
            "read".to_string()
        } else {
            format!("d{}", i + 1)
        };
        steps.insert(
            format!("d{i}"),
            decide(json!({"op": "literal", "value": true}), &next),
        );
    }
    ws.draft["entries"]["main"]["start"] = json!("d0");
    ws
}

#[test]
fn a_draft_is_bounded_in_steps_and_bytes_before_it_is_checked() {
    // 252 + the fixture's 4 steps = 256: accepted, and it checks.
    let ws = chained(252);
    assert_eq!(
        ws.draft["steps"].as_object().unwrap().len(),
        MAX_DRAFT_STEPS
    );
    assert!(check(&ws).is_empty(), "{:#?}", check(&ws));
    // 257: refused, with the limit and what to do.
    let diagnostics = check(&chained(253));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert_eq!(
        (d.code.as_str(), d.path.as_str()),
        ("draft_limit", "draft.json:steps")
    );
    assert!(
        d.message
            .contains("has 257 steps; a draft may have at most 256"),
        "{}",
        d.message
    );
    assert!(
        d.message.contains("Shorten it")
            && d.message.contains("ask the owner to split the document"),
        "{}",
        d.message
    );
    assert!(!d.message.contains("sub-workflow"), "{}", d.message);
    // Bytes: the serialized draft, and the file.
    let mut big = workspace();
    let padding = "x".repeat(MAX_DRAFT_JSON_BYTES);
    big.draft["steps"]["done"]["op"]["output"] = json!({"op": "literal", "value": padding});
    let diagnostics = check(&big);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        (diagnostics[0].code.as_str(), diagnostics[0].path.as_str()),
        ("draft_limit", "draft.json")
    );
    assert!(
        diagnostics[0]
            .message
            .contains("a draft may be at most 786432 bytes")
    );
    let dir = std::env::temp_dir().join(format!("aq-draft-{}-big", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_workspace_inputs(&dir, &big);
    std::fs::write(
        dir.join("draft.json"),
        serde_json::to_vec(&big.draft).unwrap(),
    )
    .unwrap();
    let diagnostics = Workspace::read(&dir).unwrap_err();
    assert_eq!(diagnostics[0].code, "draft_limit", "{diagnostics:#?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// `depth` nested bindings, each an object holding the previous one twice:
/// the value doubles at every level once names are replaced by their values.
fn doubling(depth: usize) -> Value {
    let mut bindings = vec![json!({"name": "b0", "value": {"op": "object",
        "fields": {"k": reference("input", &[])}}})];
    for j in 1..depth {
        let p = format!("b{}", j - 1);
        bindings.push(json!({"name": format!("b{j}"), "value": {"op": "object",
            "fields": {"a": reference(&p, &[]), "b": reference(&p, &[])}}}));
    }
    json!({"op": "let", "bindings": bindings,
           "body": {"op": "exists", "value": reference(&format!("b{}", depth - 1), &[])}})
}
/// The fixture with one decide step per predicate chained in front of `read`.
fn deciding(predicates: Vec<Value>) -> Workspace {
    let mut ws = workspace();
    let n = predicates.len();
    let steps = ws.draft["steps"].as_object_mut().unwrap();
    for (i, predicate) in predicates.into_iter().enumerate() {
        let next = if i + 1 == n {
            "read".to_string()
        } else {
            format!("d{}", i + 1)
        };
        steps.insert(format!("d{i}"), decide(predicate, &next));
    }
    ws.draft["entries"]["main"]["start"] = json!("d0");
    ws
}

#[test]
fn an_expression_that_expands_past_the_bound_is_refused_before_it_is_typed() {
    // Depth 15 expands to about 197 000 nodes: accepted, and it checks.
    assert!(check(&deciding(vec![doubling(15)])).is_empty());
    // Depth 16, about 393 000: refused, alone, before typing and the read
    // check run (either would follow every copy).
    let diagnostics = check(&deciding(vec![doubling(16)]));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert_eq!(
        (d.code.as_str(), d.path.as_str()),
        ("expression_limit", "draft.json:steps.d0")
    );
    assert!(
        d.message.starts_with("steps.d0.predicate: expands to "),
        "{}",
        d.message
    );
    assert!(
        d.message
            .contains("an expression may expand to at most 200000"),
        "{}",
        d.message
    );
    // Far past it the count saturates and still refuses at once.
    let diagnostics = check(&deciding(vec![doubling(70)]));
    assert!(
        diagnostics[0].message.contains("expands to too many nodes"),
        "{diagnostics:#?}"
    );
    // Five such expressions fit the whole draft's bound; six do not. (Six fit
    // while a reference to `input` counted as one node; it now counts the two
    // nodes of its type, so each expression is about 197 000.)
    assert!(check(&deciding(vec![doubling(15); 5])).is_empty());
    let diagnostics = check(&deciding(vec![doubling(15); 6]));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        (diagnostics[0].code.as_str(), diagnostics[0].path.as_str()),
        ("expression_limit", "draft.json")
    );
    assert!(
        diagnostics[0]
            .message
            .contains("a draft may expand to at most 1000000")
    );
}
