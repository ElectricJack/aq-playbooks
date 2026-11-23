use super::*;

pub(super) struct Rng(pub(super) u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// A number in `low..=high`.
    fn within(&mut self, low: usize, high: usize) -> usize {
        low + (self.next() % (high - low + 1) as u64) as usize
    }
}

fn strings(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}
/// An object schema of `outer` objects of `inner` scalars: `outer * (inner +
/// 1) + 1` nodes.
pub(super) fn wide(outer: usize, inner: usize) -> Value {
    let mut properties = Map::new();
    properties.insert("task".into(), json!({"type": "string", "max_length": 64}));
    for i in 0..outer {
        let mut q = Map::new();
        for j in 0..inner {
            q.insert(format!("q{j}"), json!({"type": "null"}));
        }
        properties.insert(
            format!("p{i}"),
            json!({"type": "object", "properties": q, "required": []}),
        );
    }
    json!({"type": "object", "properties": properties, "required": ["task"]})
}
/// `work.read`, whose `read` outcome is an object of `result` objects of
/// eight scalars, and `missing` is empty.
fn catalog(result: usize) -> Registry {
    let read = CommandContract {
        name: "work.read".into(),
        version: "1.0.0".into(),
        input: Schema::object(BTreeMap::new()),
        outcomes: BTreeMap::from([
            (
                "read".into(),
                serde_json::from_value(wide(result, 8)).unwrap(),
            ),
            ("missing".into(), Schema::object(BTreeMap::new())),
        ]),
        capabilities: strings(&["work.read"]),
        scopes: strings(&["project"]),
        effect: Effect::Observation,
        recovery: Recovery::Observation,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 65_536,
        deadline_ms: 1000,
    };
    Registry {
        commands: BTreeMap::from([(read.name.clone(), read)]),
        events: BTreeMap::new(),
    }
}
pub(super) fn reference(root: &str) -> Value {
    json!({"op": "ref", "root": root, "path": []})
}
pub(super) fn decide(value: Value, next: &str) -> Value {
    json!({"op": {"kind": "decide", "predicate": {"op": "exists", "value": value}},
           "edges": {"true": next, "false": next, "unknown": next}, "on_error": null})
}
fn read(bind: &str, next: &str) -> Value {
    json!({"op": {"kind": "invoke", "command": "work.read", "bind": bind, "retry": null,
                  "input": {"op": "object", "fields": {}}},
           "edges": {"read": next, "missing": next}, "on_error": "failed"})
}
pub(super) fn ret(outcome: &str) -> Value {
    json!({"op": {"kind": "return", "outcome": outcome, "output": {"op": "literal", "value": null}},
           "edges": {}, "on_error": null})
}

/// One expression that grows in one of the ways cost grows, at a size drawn
/// around the bounds; `roots` are the roots in scope it may share.
fn hostile(rng: &mut Rng, roots: &[String]) -> Value {
    let root = roots[rng.within(0, roots.len() - 1)].clone();
    match rng.within(0, 6) {
        // An object of objects, each field a reference: every copy of the
        // root's type is kept in the type of the whole.
        0 => {
            let (g, h) = (rng.within(1, 44), rng.within(1, 44));
            let mut outer = Map::new();
            for i in 0..g {
                let mut inner = Map::new();
                for j in 0..h {
                    inner.insert(format!("g{j}"), reference(&root));
                }
                outer.insert(format!("f{i}"), json!({"op": "object", "fields": inner}));
            }
            json!({"op": "object", "fields": outer})
        }
        // A flat array of references.
        1 => {
            let n = rng.within(1, 2000);
            json!({"op": "array", "items": vec![reference(&root); n]})
        }
        // Names that double at every level.
        2 => {
            let depth = rng.within(2, 17);
            let mut bindings = vec![json!({"name": "b0", "value": {"op": "object",
                "fields": {"k": reference(&root)}}})];
            for j in 1..depth {
                let p = format!("b{}", j - 1);
                bindings.push(json!({"name": format!("b{j}"), "value": {"op": "object",
                    "fields": {"a": reference(&p), "b": reference(&p)}}}));
            }
            json!({"op": "let", "bindings": bindings,
                   "body": reference(&format!("b{}", depth - 1))})
        }
        // A table whose every result is the root.
        3 => {
            let cases: Vec<Value> = (0..rng.within(1, 256))
                .map(|i| json!({"when": format!("k{i}"), "then": reference(&root)}))
                .collect();
            json!({"op": "lookup", "key": {"op": "ref", "root": "input", "path": ["task"]},
                   "cases": cases, "default": reference(&root)})
        }
        // A map whose body builds an object of references.
        4 => {
            let n = rng.within(1, 200);
            let mut fields = Map::new();
            for j in 0..n {
                fields.insert(format!("g{j}"), reference(&root));
            }
            json!({"op": "map", "item": "it", "max_items": 4,
                   "collection": {"op": "literal", "value": [1, 2, 3, 4]},
                   "value": {"op": "object", "fields": fields}})
        }
        // A large literal table bound once and read many times.
        5 => {
            let rows: Vec<Value> = (0..rng.within(1, 400))
                .map(|i| json!({"k": i, "v": "x"}))
                .collect();
            let reads = rng.within(1, 400);
            json!({"op": "let", "bindings": [{"name": "t", "value": {"op": "literal", "value": rows}}],
                   "body": {"op": "array", "items": vec![reference("t"); reads]}})
        }
        // Every binding at once, many times.
        _ => {
            let n = rng.within(1, 300);
            json!({"op": "array", "items": vec![reference("vars"); n]})
        }
    }
}

/// A draft: `steps` steps in a chain (reads that bind, decides on a hostile
/// expression, some inside loops), an entry `input` of `outer * inner` nodes.
pub(super) fn draft(rng: &mut Rng) -> Workspace {
    let steps = rng.within(4, MAX_DRAFT_STEPS - 2);
    let (outer, inner) = match rng.within(0, 3) {
        0 => (rng.within(1, 70), rng.within(1, 70)),
        1 => (rng.within(1, 20), rng.within(1, 20)),
        _ => (1, 1),
    };
    // How many of the steps carry a hostile expression (few large ones, or
    // many, so that both the per-expression and the whole-draft bounds bind),
    // and how many bind a result (so that the scope bound binds too).
    let hostile_every = [4, 16, 64, 300][rng.within(0, 3)];
    let reads_in_ten = rng.within(0, 3);
    let mut generator = StepGenerator {
        steps,
        hostile_every,
        reads_in_ten,
        roots: vec!["input".into()],
        loops: vec![],
    };
    let mut map = Map::new();
    for i in 0..steps {
        map.insert(generator.name(i), generator.step(rng, i));
    }
    // `failed` is there when some step can fail into it.
    if serde_json::to_string(&map).unwrap().contains("\"failed\"") {
        map.insert("failed".into(), ret("failed"));
    }
    map.insert("done".into(), ret("done"));
    Workspace {
        markdown: "# Generated\n\nA line.\n".into(),
        record: SourceRecord {
            id: "generated".into(),
            binding: "aq.generated".into(),
            scope: "project".into(),
            purpose: "A generated draft".into(),
            commands: strings(&["work.read"]),
            events: BTreeSet::new(),
            capabilities: strings(&["work.read"]),
            facts_version: "facts-1".into(),
        },
        catalog: catalog(rng.within(1, 12)),
        facts: BTreeMap::new(),
        draft: json!({
            "purpose": "A generated draft",
            "entries": {"main": {
                "input": wide(outer, inner),
                "outcomes": {"done": {"type": "null"}, "failed": {"type": "null"}},
                "start": "s0"}},
            "steps": map,
            "triggers": [],
            "source_map": {}
        }),
    }
}

struct StepGenerator {
    steps: usize,
    hostile_every: usize,
    reads_in_ten: usize,
    roots: Vec<String>,
    loops: Vec<(usize, String)>,
}
impl StepGenerator {
    fn name(&self, i: usize) -> String {
        if i == self.steps {
            "done".into()
        } else {
            format!("s{i}")
        }
    }

    /// Close loops ending at this step, returning the innermost loop site.
    fn close_loops(&mut self, i: usize) -> Option<String> {
        let first = self.loops.pop_if(|(end, _)| *end == i)?;
        while self.loops.pop_if(|(end, _)| *end == i).is_some() {}
        Some(first.1)
    }

    fn step(&mut self, rng: &mut Rng, i: usize) -> Value {
        let back = self.close_loops(i);
        let can_loop = back.is_none() && self.loops.len() < 3 && i + 3 < self.steps;
        let next = back.unwrap_or_else(|| self.name(i + 1));
        match rng.within(0, 9) {
            kind if kind < self.reads_in_ten => {
                let bind = format!("w{i}");
                let step = read(&bind, &next);
                self.roots.push(bind);
                step
            }
            9 if can_loop => self.start_loop(rng, i),
            _ => decide(self.value(rng), &next),
        }
    }

    fn value(&self, rng: &mut Rng) -> Value {
        if rng.within(1, self.hostile_every) == 1 {
            hostile(rng, &self.roots)
        } else {
            reference(&self.roots[rng.within(0, self.roots.len() - 1)])
        }
    }

    /// Stay inside the enclosing loop and return to it if the two end together.
    fn start_loop(&mut self, rng: &mut Rng, i: usize) -> Value {
        let around = self.loops.last();
        let limit = around.map_or(self.steps - 1, |(end, _)| *end);
        let end = rng.within(i + 1, (i + 6).min(limit));
        let after = match around {
            Some((outer, site)) if *outer == end => site.clone(),
            _ => self.name(end + 1),
        };
        self.loops.push((end, self.name(i)));
        json!({"op": {"kind": "for_each", "collection": {"op": "literal", "value": [1, 2]},
            "item": format!("t{i}"), "body": self.name(i + 1), "next": after, "max_items": 2,
            "concurrency": 1, "on_failure": "stop"},
            "edges": {}, "on_error": "failed"})
    }
}
