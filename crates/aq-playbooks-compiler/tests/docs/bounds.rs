use super::*;

fn reference(root: &str, path: &[&str]) -> Value {
    json!({"op": "ref", "root": root, "path": path})
}
/// `b0 = {k: input}`, `bj = {a: b(j-1), b: b(j-1)}`: a value that doubles at
/// every level.
fn doubling(depth: usize, prefix: &str) -> Value {
    let mut bindings = vec![
        json!({"name": format!("{prefix}0"), "value": {"op": "object", "fields": {"k": reference("input", &[])}}}),
    ];
    for j in 1..depth {
        let before = format!("{prefix}{}", j - 1);
        bindings.push(
            json!({"name": format!("{prefix}{j}"), "value": {"op": "object",
            "fields": {"a": reference(&before, &[]), "b": reference(&before, &[])}}}),
        );
    }
    json!({"op": "let", "bindings": bindings,
           "body": {"op": "exists", "value": reference(&format!("{prefix}{}", depth - 1), &[])}})
}
fn decide(predicate: Value, next: &str) -> Value {
    json!({"op": {"kind": "decide", "predicate": predicate},
           "edges": {"true": next, "false": next, "unknown": "failed"}, "on_error": null})
}
/// The notify-when-ready workspace made to pass one bound the documents
/// name.
pub(super) fn past_the_bound(limit: &str) -> Workspace {
    let mut ws = workspace("notify-when-ready");
    let steps = ws.draft["steps"].as_object_mut().unwrap();
    match limit {
        "steps" => {
            let ret = steps["failed"].clone();
            let missing = MAX_DRAFT_STEPS + 1 - steps.len();
            for i in 0..missing {
                steps.insert(format!("r{i}"), ret.clone());
            }
        }
        "bytes" => {
            ws.draft["purpose"] = json!("x".repeat(MAX_DRAFT_JSON_BYTES));
        }
        "expression" => {
            steps["ready"]["op"]["predicate"] = json!({"op": "and", "values": [doubling(16, "b")]});
        }
        "total" => {
            decision_chain(steps, 8, |i| doubling(14, &format!("x{i}_")));
        }
        "scope" => {
            decision_chain(steps, 200, |_| json!({"op":"literal","value":true}));
            let input = &mut ws.draft["entries"]["main"]["input"]["properties"];
            for i in 0..1500 {
                input[format!("p{i}")] = json!({"type": "boolean"});
            }
        }
        "shapes" => {
            let rows = vec![json!({"k": "a"}); 12_000];
            let reads: Vec<Value> = (0..100)
                .map(|i| {
                    json!({"op": "map", "collection": reference("t", &[]), "item": format!("r{i}"),
                                "value": reference(&format!("r{i}"), &["k"]), "max_items": 12_000})
                })
                .collect();
            steps["ready"]["op"]["predicate"] = json!({"op": "let",
                "bindings": [{"name": "t", "value": {"op": "literal", "value": rows}}],
                "body": {"op": "exists", "value": {"op": "array", "items": reads}}});
        }
        other => panic!("no such bound in the test: {other}"),
    }
    ws
}

fn decision_chain(
    steps: &mut serde_json::Map<String, Value>,
    count: usize,
    predicate: impl Fn(usize) -> Value,
) {
    for i in 0..count {
        let next = if i + 1 < count {
            format!("d{}", i + 1)
        } else {
            "wait".into()
        };
        steps.insert(format!("d{i}"), decide(predicate(i), &next));
    }
    steps["ready"]["edges"]["true"] = json!("d0");
}
