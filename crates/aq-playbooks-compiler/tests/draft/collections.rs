use super::*;

// Values that come out of `map`, `filter` and `difference`, and `for_each`
// items (A2 re-review round 3, I1; probes 25, 26, 28 and 30). The generated
// test in the model crate covers every composition; these pin what the
// author is told.
#[test]
fn what_comes_out_of_map_filter_and_difference_is_held_to_the_same_check() {
    let w = |p: &[&str]| reference("work", p);
    let (_, reasons) = at_peek(is_chore(w(&["kind"])));
    let over = |collection: Value, item: &str, value: Value| json!({"op": "map", "item": item, "max_items": 1, "collection": collection, "value": value});
    let one = |e: Value| json!({"op": "array", "items": [e]});
    let kept = json!({"op": "filter", "item": "x", "max_items": 1, "collection": one(w(&[])),
        "predicate": {"op": "literal", "value": true}});
    let has_chore = |collection: Value| {
        json!({"op": "contains", "collection": collection,
               "value": {"op": "literal", "value": "chore"}})
    };
    let copied = over(one(w(&[])), "x", reference("x", &[]));
    for (predicate, at, written) in [
        // Probe 25: an item of what `filter` keeps.
        (
            has_chore(over(kept, "y", reference("y", &["kind"]))),
            "steps.peek.predicate.contains.collection.map.value",
            "`y.kind` (`y` is an item of its collection)",
        ),
        // Probe 26: an item of a `map` result bound to a name.
        (
            let1(
                "m",
                copied.clone(),
                has_chore(over(reference("m", &[]), "y", reference("y", &["kind"]))),
            ),
            "steps.peek.predicate.let.body.contains.collection.map.value",
            "`y.kind` (`y` is an item of its collection)",
        ),
        // Probe 28: a position of a `map` result as a lookup key, which
        // would take the default silently.
        (
            let1("m", copied, keyed_by(reference("m", &["0", "kind"]))),
            "steps.peek.predicate.let.body.lookup.key",
            "`m.0.kind` (`m` is computed from other values)",
        ),
    ] {
        let (diagnostics, found) = at_peek(predicate);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        let message = &diagnostics[0].message;
        assert!(
            message.starts_with(&format!("{at}: {written} is not there")),
            "{message}"
        );
        assert_eq!(found, reasons, "{message}");
    }
    // A value read where it is bound and used through its name outside
    // `exists` is refused where it is read, and the name is given.
    let (diagnostics, found) = at_peek(let1(
        "k",
        w(&["kind"]),
        json!({"op": "and", "values": [
            {"op": "exists", "value": reference("k", &[])},
            is_chore(reference("k", &[]))]}),
    ));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert!(
        diagnostics[0].message.starts_with(
            "steps.peek.predicate.let.bindings[0].value: `work.kind` (its value is used \
             through `k`, outside any `exists`) is not there"
        ),
        "{diagnostics:#?}"
    );
    assert_eq!(found, reasons);
}

#[test]
fn a_difference_requires_its_identity_even_inside_exists() {
    let w = |path: &[&str]| reference("work", path);
    let (_, reasons) = at_peek(is_chore(w(&["kind"])));
    let one = |item: Value| json!({"op": "array", "items": [item]});
    // The identity of a `difference` is read in every item, and no `exists`
    // makes up for an item without it.
    let (diagnostics, found) = at_peek(json!({"op": "exists", "value": {
        "op": "difference", "max_items": 1, "identity": ["kind"],
        "left": one(w(&[])), "right": {"op": "literal", "value": []}}}));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let message = &diagnostics[0].message;
    assert!(
        message.starts_with("steps.peek.predicate.exists.value.difference.identity: `left.kind`"),
        "{message}"
    );
    assert!(message.contains("`exists` cannot test it"), "{message}");
    assert_eq!(found, "");
    assert!(message.contains(&reasons), "{message}");
}

#[test]
fn a_for_each_item_carries_what_its_collection_holds_and_only_in_the_body() {
    // Probe 30: a loop over `[work]` whose body keys a lookup on `t.kind`.
    let w = |p: &[&str]| reference("work", p);
    let (_, reasons) = at_peek(is_chore(w(&["kind"])));
    let mut ws = partial(json!({"op": "literal", "value": true}));
    ws.draft["steps"]["peek"] = json!({"op": {"kind": "for_each",
        "collection": {"op": "array", "items": [w(&[])]}, "item": "t", "body": "note",
        "next": "after", "max_items": 1, "concurrency": 1, "on_failure": "stop"},
        "edges": {}, "on_error": "failed"});
    ws.draft["steps"]["note"] = decide(keyed_by(reference("t", &["kind"])), "peek");
    ws.draft["steps"]["after"] = decide(json!({"op": "literal", "value": true}), "route");
    let diagnostics = check(&ws);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert_eq!(
        (d.code.as_str(), d.path.as_str()),
        ("expression_type", "draft.json:steps.note")
    );
    assert!(
        d.message.starts_with(
            "steps.note.predicate.lookup.key: `t.kind` (`t` is an item of the collection of \
             for_each `peek`) is not there on every way to this step: "
        ),
        "{}",
        d.message
    );
    assert!(d.message.contains(&reasons), "{}", d.message);
    assert!(
        d.message.contains("a lookup key is held to this too"),
        "{}",
        d.message
    );
    // A field every outcome has is read through the item without complaint.
    ws.draft["steps"]["note"] = decide(keyed_by(reference("t", &["kinds", "0"])), "peek");
    ws.catalog
        .commands
        .get_mut("work.read")
        .unwrap()
        .outcomes
        .insert("missing".into(), kinds());
    assert!(check(&ws).is_empty(), "{:#?}", check(&ws));
    // The item is bound in the body only: after the loop it is not there.
    ws.draft["steps"]["after"] = decide(is_chore(reference("t", &["kind"])), "route");
    let diagnostics = check(&ws);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert!(
        diagnostics[0].message.starts_with(
            "steps.after.predicate.eq.left: `t.kind` (`t` is an item of the collection of \
             for_each `peek`) is not there on every way to this step: on the `next` exit of \
             for_each `peek`, `t` is not bound."
        ),
        "{diagnostics:#?}"
    );
}

#[test]
fn a_failure_in_a_loop_body_is_followed_to_where_the_loop_takes_it() {
    // `read` runs before the loop and again in its body. Its `missing`
    // outcome leaves the loop by `noted`, so on ordinary edges `after` sees
    // only the `read` outcome, which has `kind`. But a failure of `noted`
    // inside the loop is the loop's to handle: with `on_failure: continue`
    // it goes on to the next item, or to `after`, with `work` as `missing`
    // left it.
    let looped = |on_failure: &str| {
        let mut ws = partial(json!({"op": "literal", "value": true}));
        let steps = ws.draft["steps"].as_object_mut().unwrap();
        steps.remove("peek");
        let mut noted = steps.remove("route").unwrap();
        noted["edges"] = json!({"routed": "done", "stale": "failed"});
        steps.insert("noted".into(), noted);
        steps.insert(
            "each".into(),
            json!({"op": {"kind": "for_each",
                "collection": {"op": "literal", "value": ["a"]}, "item": "t", "body": "again",
                "next": "after", "max_items": 1, "concurrency": 1, "on_failure": on_failure},
                "edges": {}, "on_error": "failed"}),
        );
        steps.insert(
            "again".into(),
            decide(json!({"op": "literal", "value": true}), "read"),
        );
        steps.insert("after".into(), decide(work_is_chore(), "done"));
        ws.draft["steps"]["read"]["edges"] = json!({"read": "each", "missing": "noted"});
        ws
    };
    let diagnostics = check(&looped("continue"));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert_eq!(
        (d.code.as_str(), d.path.as_str()),
        ("expression_type", "draft.json:steps.after")
    );
    assert!(
        d.message
            .contains("outcome `missing` of step `read` has no `kind` in `work`"),
        "{}",
        d.message
    );
    // With `stop` the failure leaves by the loop's `on_error`, and `after`
    // is reached only with the outcome that has the field.
    let diagnostics = check(&looped("stop"));
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
}

// A literal is read as the value it is (A2 re-review round 4, I1; probes L1,
// L1b and L2): each row keeps its own fields and an array its own length, as
// for the same value written with `object` and `array` expressions.
#[test]
fn a_literal_table_is_read_row_by_row_and_an_array_to_its_end() {
    let table = |rows: Value, body: Value| json!({"op": "exists", "value": let1("t", json!({"op": "literal", "value": rows}), body)});
    let rows = json!([{"k": "chore"}, {}]);
    let over = |op: &str, field: &str, body: Value| json!({"op": op, "item": "r", "max_items": 2, "collection": reference("t", &[]), field: body});
    // L1: a lookup keyed on a field one row lacks would take its default
    // for that row without a trace.
    let (diagnostics, reasons) = at_peek(table(
        rows.clone(),
        over("map", "value", keyed_by(reference("r", &["k"]))),
    ));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let message = &diagnostics[0].message;
    assert!(
        message.starts_with(
            "steps.peek.predicate.exists.value.let.body.map.value.lookup.key: `r.k` (`r` is an \
             item of its collection) is not there on every way to this step: "
        ),
        "{message}"
    );
    assert_eq!(reasons, "`k` is not in an object written with no fields");
    assert!(
        message.contains("a lookup key is held to this too"),
        "{message}"
    );
    // L1b: outside `exists`, a filter on that field is Missing for that row.
    let Value::Object(mut unwrapped) = table(
        rows.clone(),
        json!({"op": "contains", "value": {"op": "literal", "value": {"k": "chore"}},
            "collection": over("filter", "predicate", is_chore(reference("r", &["k"])))}),
    ) else {
        unreachable!()
    };
    let (diagnostics, reasons) = at_peek(unwrapped.remove("value").unwrap());
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(reasons, "`k` is not in an object written with no fields");
    // L2: a position past the end of a literal array, as a lookup key.
    let (diagnostics, reasons) = at_peek(table(
        json!(["chore", "x"]),
        keyed_by(reference("t", &["5"])),
    ));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        reasons,
        "position `5` is past the end of an array written with 2 items"
    );
    // A table whose rows all have the field, and a position that is there,
    // check clean.
    for accepted in [
        table(
            json!([{"k": "chore"}, {"k": "fix"}]),
            over("map", "value", keyed_by(reference("r", &["k"]))),
        ),
        table(json!(["chore", "x"]), keyed_by(reference("t", &["1"]))),
    ] {
        let (diagnostics, _) = at_peek(accepted.clone());
        assert!(diagnostics.is_empty(), "{accepted}: {diagnostics:#?}");
    }
}

// What the diagnostic tells the author to do (A2 re-review round 4, m1):
// `exists` is a test, and testing does not make a read allowed.
#[test]
fn the_remedy_does_not_promise_that_exists_makes_a_read_allowed() {
    let w = |p: &[&str]| reference("work", p);
    // Probe E1: the value read in the branch its own `exists` selects.
    let (diagnostics, _) = at_peek(json!({"op": "lookup",
        "key": {"op": "exists", "value": w(&["kind"])},
        "cases": [{"when": true, "then": is_chore(w(&["kind"]))}],
        "default": {"op": "literal", "value": false}}));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let message = &diagnostics[0].message;
    assert!(
        message.starts_with("steps.peek.predicate.lookup.cases[0].then.eq.left: `work.kind`"),
        "{message}"
    );
    for text in [
        "after the outcome edge of the step that returns it",
        "`exists` only tells you whether it is there: testing it does not make a read of it allowed",
        "branch on the edge first and read the value in the step that edge leads to",
    ] {
        assert!(message.contains(text), "{text}: {message}");
    }
    assert!(!message.contains("Or test it first"), "{message}");
}
