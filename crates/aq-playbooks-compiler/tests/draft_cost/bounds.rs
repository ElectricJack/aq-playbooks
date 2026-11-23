use super::*;

fn chain(input: Value, steps: Vec<Value>) -> Workspace {
    let mut ws = draft(&mut Rng(7));
    let mut map = Map::new();
    let last = steps.len() - 1;
    for (i, value) in steps.into_iter().enumerate() {
        let next = if i == last {
            "done".to_string()
        } else {
            format!("s{}", i + 1)
        };
        map.insert(format!("s{i}"), decide(value, &next));
    }
    map.insert("done".into(), ret("done"));
    ws.draft["entries"]["main"]["input"] = input;
    ws.draft["steps"] = Value::Object(map);
    ws
}

// `refs` references to `root`, kept apart in an object of objects.
fn kept(root: &str, refs: usize) -> Value {
    let mut outer = Map::new();
    for i in 0..refs.div_ceil(40) {
        let mut inner = Map::new();
        for j in 0..40.min(refs - i * 40) {
            inner.insert(format!("g{j}"), reference(root));
        }
        outer.insert(format!("f{i}"), json!({"op": "object", "fields": inner}));
    }
    json!({"op": "object", "fields": outer})
}

fn doubling(depth: usize) -> Value {
    let mut bindings = vec![json!({"name": "b0", "value": {"op": "object",
            "fields": {"k": reference("input")}}})];
    for j in 1..depth {
        let p = format!("b{}", j - 1);
        bindings.push(json!({"name": format!("b{j}"), "value": {"op": "object",
                "fields": {"a": reference(&p), "b": reference(&p)}}}));
    }
    json!({"op": "let", "bindings": bindings, "body": reference(&format!("b{}", depth - 1))})
}

/// Drafts built to sit just under one bound each; every one seals.
pub(super) fn at_the_bounds() -> Vec<(&'static str, Workspace)> {
    vec![
        (
            // 254 steps, each with a 961-node input in scope: 244 094.
            "the scope bound",
            chain(wide(137, 6), vec![reference("input"); 253]),
        ),
        (
            // Five expressions of 390 references to a 502-node input: about
            // 196 000 each, 980 000 in all.
            "the whole-draft bound",
            chain(wide(50, 9), vec![kept("input", 390); 5]),
        ),
        (
            // One expression of 40 references to a 4 972-node input.
            "the expression bound, a large type",
            chain(wide(70, 70), vec![kept("input", 40)]),
        ),
        (
            // Near the byte cap: two literal arrays of 190 000 numbers, each
            // just under the expression bound.
            "the byte cap, long literal arrays",
            chain(
                wide(1, 1),
                vec![json!({"op": "literal", "value": vec![1; 190_000]}); 2],
            ),
        ),
        (
            // Near the byte cap: literal tables of 23 500 rows, each row an
            // object, 70 501 nodes a table, read through a name.
            "the byte cap, literal tables of rows",
            chain(
                wide(1, 1),
                vec![
                    json!({"op": "let",
                        "bindings": [{"name": "t", "value": {"op": "literal",
                            "value": vec![json!({"k": 1, "v": "x"}); 23_500]}}],
                        "body": {"op": "ref", "root": "t", "path": ["0", "k"]}});
                    2
                ],
            ),
        ),
        (
            // Near the byte cap: an input type of 32 582 nodes, in scope at
            // three steps and read whole twice.
            "the byte cap, a large input type",
            chain(wide(180, 180), vec![reference("input"); 2]),
        ),
        (
            // Near the byte cap: two literal arrays of 54 500 one-field rows,
            // the most rows with a field that fit. Serializing the artifact
            // to verify it costs about 2 KB a row.
            "the byte cap, one-field rows",
            chain(
                wide(1, 1),
                vec![json!({"op": "literal", "value": vec![json!({"": 1}); 54_500]}); 2],
            ),
        ),
        (
            // Near the byte cap: two literal arrays of 127 000 empty rows.
            "the byte cap, empty rows",
            chain(
                wide(1, 1),
                vec![json!({"op": "literal", "value": vec![json!({}); 127_000]}); 2],
            ),
        ),
        (
            // Forty entries over the same 100 steps, each with a 52-node
            // input in scope: 208 000.
            "the scope bound, many entries",
            {
                let mut ws = chain(wide(5, 9), vec![reference("input"); 99]);
                let main = ws.draft["entries"]["main"].clone();
                for i in 1..40 {
                    ws.draft["entries"][format!("e{i}")] = main.clone();
                }
                ws
            },
        ),
        (
            // Names that double fourteen times: about 147 000 nodes, six times.
            "the expression bound, doubling names",
            chain(wide(1, 1), vec![doubling(14); 6]),
        ),
    ]
}
