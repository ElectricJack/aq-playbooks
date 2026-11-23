use super::*;

#[test]
fn a_computed_alias_is_held_to_the_same_check() {
    let w = |p: &[&str]| reference("work", p);
    let (direct, reasons) = at_peek(is_chore(w(&["kind"])));
    assert_eq!(direct.len(), 1, "{direct:#?}");
    assert!(
        reasons.contains(
            "outcome `missing` of step `read` has no `kind` in `work` (it has no fields)"
        ),
        "{reasons}"
    );
    let object = |field: Value| json!({"op": "object", "fields": {"k": field}});
    for (i, predicate) in refused_aliases().into_iter().enumerate() {
        let (diagnostics, alias_reasons) = at_peek(predicate);
        assert_eq!(diagnostics.len(), 1, "{i}: {diagnostics:#?}");
        assert_eq!(diagnostics[0].code, "expression_type", "{i}");
        assert_eq!(alias_reasons, reasons, "{i}: {}", diagnostics[0].message);
    }
    // Probe 16: as a lookup key, the silent default is refused too.
    let (diagnostics, alias_reasons) = at_peek(let1(
        "o",
        object(w(&[])),
        keyed_by(reference("o", &["k", "kind"])),
    ));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(alias_reasons, reasons);
    assert!(
        diagnostics[0]
            .message
            .contains("a lookup key is held to this too")
    );
    // `exists` stays the one exemption, and a field every way has is fine.
    for accepted in [
        let1(
            "o",
            object(w(&[])),
            json!({"op": "exists", "value": reference("o", &["k", "kind"])}),
        ),
        let1(
            "o",
            object(reference("input", &[])),
            is_chore(reference("o", &["k", "task"])),
        ),
        let1(
            "a",
            json!({"op": "array", "items": [reference("input", &[]), w(&[])]}),
            is_chore(reference("a", &["0", "task"])),
        ),
    ] {
        let (diagnostics, _) = at_peek(accepted.clone());
        assert!(diagnostics.is_empty(), "{accepted}: {diagnostics:#?}");
    }
    // A field read where it is embedded is refused there, once.
    let (diagnostics, _) = at_peek(let1(
        "o",
        object(w(&["kind"])),
        is_chore(reference("o", &["k"])),
    ));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert!(
        diagnostics[0]
            .message
            .starts_with("steps.peek.predicate.let.bindings[0].value.k: `work.kind`"),
        "{diagnostics:#?}"
    );
}

fn refused_aliases() -> [Value; 7] {
    let w = |p: &[&str]| reference("work", p);
    let object = |field: Value| json!({"op": "object", "fields": {"k": field}});
    [
        // Probe 10: an object holding the binding, read through.
        let1(
            "o",
            object(w(&[])),
            is_chore(reference("o", &["k", "kind"])),
        ),
        // Probe 11: a lookup that yields the binding.
        let1(
            "o",
            json!({"op": "lookup", "key": {"op": "literal", "value": "a"},
                   "cases": [{"when": "a", "then": w(&[])}], "default": w(&[])}),
            is_chore(reference("o", &["kind"])),
        ),
        // A lookup that yields the binding in one case and a literal otherwise.
        let1(
            "o",
            json!({"op": "lookup", "key": reference("input", &["task"]),
                   "cases": [{"when": "a", "then": w(&[])}],
                   "default": {"op": "literal", "value": {"kind": "chore", "kinds": []}}}),
            is_chore(reference("o", &["kind"])),
        ),
        // Probe 12, and a let nested deeper.
        let1(
            "o",
            let1("p", w(&[]), reference("p", &[])),
            is_chore(reference("o", &["kind"])),
        ),
        let1(
            "o",
            let1(
                "p",
                object(w(&[])),
                let1("q", reference("p", &["k"]), reference("q", &[])),
            ),
            is_chore(reference("o", &["kind"])),
        ),
        // An array of bindings, indexed by position.
        let1(
            "a",
            json!({"op": "array", "items": [reference("input", &[]), w(&[])]}),
            is_chore(reference("a", &["1", "kind"])),
        ),
        // Probe 13: a map item of an array of objects holding the binding.
        json!({"op": "contains", "value": {"op": "literal", "value": "chore"},
               "collection": {"op": "map", "item": "it", "max_items": 1,
                   "collection": {"op": "array", "items": [object(w(&[]))]},
                   "value": reference("it", &["k", "kind"])}}),
    ]
}
