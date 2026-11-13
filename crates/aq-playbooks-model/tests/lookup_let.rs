//! `lookup` and `let` expressions: evaluation, typing, structural bounds and
//! the stability of artifacts that do not use them.
use aq_playbooks_model::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const LEVELS: [&str; 3] = ["complex-high", "simple-high", "standard-high"];

fn lit(v: Value) -> Expr {
    Expr::literal(v)
}
fn r(root: &str, path: &[&str]) -> Expr {
    Expr::Ref {
        root: root.into(),
        path: path.iter().map(|s| s.to_string()).collect(),
    }
}
fn case(when: Value, then: Expr) -> LookupCase {
    LookupCase { when, then }
}
fn lookup(key: Expr, cases: Vec<LookupCase>, default: Expr) -> Expr {
    Expr::Lookup {
        key: Box::new(key),
        cases,
        default: Box::new(default),
    }
}
fn bind(name: &str, value: Expr) -> LetBinding {
    LetBinding {
        name: name.into(),
        value,
    }
}
fn let_(bindings: Vec<LetBinding>, body: Expr) -> Expr {
    Expr::Let {
        bindings,
        body: Box::new(body),
    }
}
fn lt(left: Expr, right: Expr) -> Expr {
    Expr::Lt {
        left: Box::new(left),
        right: Box::new(right),
    }
}
fn ev(expr: &Expr, context: &EvalContext) -> EvalValue {
    eval(expr, context, &EvalLimits::default()).unwrap()
}
fn known(v: Value) -> EvalValue {
    EvalValue::Known(v)
}
fn input_context(input: Value) -> EvalContext {
    BTreeMap::from([("input".into(), EvalValue::Known(input))])
}
/// The kind table used across tests: three named kinds, default Standard.
fn kind_table(key: Expr) -> Expr {
    lookup(
        key,
        vec![
            case(json!("chore"), lit(json!("simple-high"))),
            case(json!("design"), lit(json!("complex-high"))),
            case(json!("bugfix"), lit(json!("standard-high"))),
        ],
        lit(json!("standard-high")),
    )
}
fn level_schema(values: &[&str]) -> Schema {
    Schema::String {
        max_length: values.iter().map(|v| v.len()).max().unwrap(),
        values: values.iter().map(|v| v.to_string()).collect(),
    }
}

fn entry_input() -> Schema {
    Schema::object(BTreeMap::from([
        (
            "kind".into(),
            Schema::String {
                max_length: 16,
                values: ["bugfix", "chore", "design", "research"]
                    .map(String::from)
                    .into(),
            },
        ),
        (
            "risk".into(),
            Schema::String {
                max_length: 16,
                values: ["high", "low", "medium"].map(String::from).into(),
            },
        ),
        ("narrow".into(), Schema::Boolean),
        (
            "rank".into(),
            Schema::Integer {
                minimum: 0,
                maximum: 1000,
            },
        ),
        ("note".into(), Schema::string(4096)),
    ]))
}
fn route_command() -> CommandContract {
    CommandContract {
        name: "work.route".into(),
        version: "1.0.0".into(),
        input: Schema::object(BTreeMap::from([("level".into(), level_schema(&LEVELS))])),
        outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
        capabilities: BTreeSet::new(),
        scopes: BTreeSet::from(["project".into()]),
        effect: Effect::Observation,
        recovery: Recovery::Observation,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 1024,
        deadline_ms: 1000,
    }
}
/// One entry: `route` invokes `work.route` with `route_input` and binds
/// `routed`; `finish` returns `output` as outcome `done` typed `done`.
fn content(route_input: Expr, output: Expr, done: Schema) -> ArtifactContent {
    let c = route_command();
    let header = SourceHeader {
        format_version: FORMAT_VERSION,
        id: "lookups".into(),
        scope: "project".into(),
        interfaces: BTreeSet::from(["main".into()]),
        definitions: BTreeSet::new(),
    };
    ArtifactContent {
        format_version: FORMAT_VERSION,
        compiler: "test".into(),
        source: SourceBinding {
            digest: hash_bytes(b"source"),
            header,
        },
        playbook: Playbook {
            id: "lookups".into(),
            purpose: "lookup and let".into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: entry_input(),
                    outcomes: BTreeMap::from([("done".into(), done)]),
                    start: "route".into(),
                },
            )]),
            steps: BTreeMap::from([
                (
                    "route".into(),
                    Step {
                        op: Operation::Invoke {
                            command: ContractRef {
                                name: c.name.clone(),
                                fingerprint: c.fingerprint().unwrap(),
                            },
                            input: route_input,
                            bind: "routed".into(),
                            retry: None,
                        },
                        edges: BTreeMap::from([("ok".into(), "finish".into())]),
                        on_error: Some("finish".into()),
                    },
                ),
                (
                    "finish".into(),
                    Step {
                        op: Operation::Return {
                            outcome: "done".into(),
                            output,
                        },
                        edges: BTreeMap::new(),
                        on_error: None,
                    },
                ),
            ]),
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: BTreeSet::new(),
        },
        registry: Registry {
            commands: BTreeMap::from([(c.name.clone(), c)]),
            events: BTreeMap::new(),
        },
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    }
}
fn valid_route_input() -> Expr {
    Expr::Object {
        fields: BTreeMap::from([("level".into(), lit(json!("standard-high")))]),
    }
}
/// Issues for an artifact whose `finish` step returns `output` typed `done`.
fn returning(output: Expr, done: Schema) -> Vec<ValidationIssue> {
    content(valid_route_input(), output, done).validate_structure()
}
fn has_issue(issues: &[ValidationIssue], code: &str, path: &str, text: &str) -> bool {
    issues
        .iter()
        .any(|i| i.code == code && i.path == path && i.message.contains(text))
}

#[test]
fn lookup_evaluates_only_the_matching_case() {
    let ctx = input_context(json!({"kind": "bugfix"}));
    let table = lookup(
        r("input", &["kind"]),
        vec![
            case(json!("chore"), r("absent", &["level"])),
            case(json!("bugfix"), lit(json!("standard-high"))),
        ],
        r("absent", &[]),
    );
    assert_eq!(ev(&table, &ctx), known(json!("standard-high")));
    // An unevaluated case still costs its place in the table: a lookup charges
    // cases.len() to the element budget on top of its own node.
    let limits = |max_elements| EvalLimits {
        max_elements,
        ..EvalLimits::default()
    };
    // Nodes: lookup, key ref, chosen literal = 3; cases = 2.
    assert!(eval(&table, &ctx, &limits(5)).is_ok());
    assert!(eval(&table, &ctx, &limits(4)).is_err());
}

#[test]
fn lookup_default_on_no_match_and_unresolved_key_propagates() {
    let ctx = BTreeMap::from([
        ("input".into(), known(json!({"kind": "zzz"}))),
        ("stale".into(), EvalValue::Unknown("revision".into())),
    ]);
    assert_eq!(
        ev(&kind_table(r("input", &["kind"])), &ctx),
        known(json!("standard-high"))
    );
    assert_eq!(
        ev(&kind_table(r("stale", &[])), &ctx),
        EvalValue::Unknown("revision".into())
    );
    assert_eq!(
        ev(&kind_table(r("input", &["missing"])), &ctx),
        EvalValue::Missing
    );
    // An unresolved key never falls through to the default, even when the
    // default is a literal that could be produced without the key.
    assert_eq!(
        ev(
            &lookup(r("absent", &[]), vec![], lit(json!("standard-high"))),
            &ctx
        ),
        EvalValue::Missing
    );
}

#[test]
fn lookup_result_type_is_the_union_of_its_results() {
    let output = kind_table(r("input", &["kind"]));
    let issues = returning(output.clone(), level_schema(&LEVELS));
    assert!(issues.is_empty(), "{issues:?}");
    // Each level is in the inferred enum: dropping any one from the target
    // makes the lookup unassignable.
    for missing in LEVELS {
        let rest: Vec<_> = LEVELS.into_iter().filter(|l| *l != missing).collect();
        let issues = returning(output.clone(), level_schema(&rest));
        assert!(
            has_issue(&issues, "schema_compatibility", "steps.finish", ""),
            "{missing}: {issues:?}"
        );
    }
    // The enum is closed: an open string of the same length accepts it, an
    // enum-typed target narrower in length does not.
    assert!(returning(output.clone(), Schema::string(13)).is_empty());
    let mut short = level_schema(&LEVELS);
    if let Schema::String { max_length, .. } = &mut short {
        *max_length = 12;
    }
    assert!(!returning(output.clone(), short).is_empty());
    // Nested inside another expression the lookup is typed by inference.
    let nested = Expr::Contains {
        collection: Box::new(lit(json!(["complex-high"]))),
        value: Box::new(output),
    };
    assert!(returning(nested, Schema::Boolean).is_empty());
}

#[test]
fn lookup_refuses_duplicate_incompatible_or_too_many_cases() {
    let target = Schema::string(16);
    let key = || r("input", &["kind"]);
    let s = |v: &str| lit(json!(v));
    // Duplicate canonical `when`.
    let duplicate = lookup(
        key(),
        vec![
            case(json!("bugfix"), s("a")),
            case(json!("chore"), s("b")),
            case(json!("bugfix"), s("c")),
        ],
        s("d"),
    );
    let issues = returning(duplicate, target.clone());
    assert!(
        has_issue(&issues, "lookup_case", "steps.finish", "cases 0 and 2"),
        "{issues:?}"
    );
    // Canonically equal objects are duplicates whatever their key order.
    let objects = lookup(
        lit(json!({"a": 1, "b": 2})),
        vec![
            case(serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap(), s("x")),
            case(serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap(), s("y")),
        ],
        s("z"),
    );
    assert!(has_issue(
        &returning(objects, target.clone()),
        "lookup_case",
        "steps.finish",
        "cases 0 and 1"
    ));
    // A `when` the key's type cannot take: integer against a string key.
    let incompatible = lookup(key(), vec![case(json!(1), s("a"))], s("b"));
    let issues = returning(incompatible, target.clone());
    assert!(
        has_issue(&issues, "expression_type", "steps.finish", "case 0"),
        "{issues:?}"
    );
    // At most 256 cases.
    let ranks = |n: i64| {
        lookup(
            r("input", &["rank"]),
            (0..n).map(|i| case(json!(i), s("x"))).collect(),
            s("y"),
        )
    };
    assert!(returning(ranks(256), target.clone()).is_empty());
    let issues = returning(ranks(257), target.clone());
    assert!(
        has_issue(&issues, "expression_limit", "steps.finish", "257 cases"),
        "{issues:?}"
    );
    // Each `when` is at most 1024 canonical bytes (a string's two quotes count).
    let sized = |bytes: usize| {
        lookup(
            r("input", &["note"]),
            vec![case(json!("n".repeat(bytes - 2)), s("x"))],
            s("y"),
        )
    };
    assert!(returning(sized(1024), target.clone()).is_empty());
    let issues = returning(sized(1025), target);
    assert!(
        has_issue(&issues, "expression_limit", "steps.finish", "1025 bytes"),
        "{issues:?}"
    );
}

#[test]
fn lookup_keys_may_be_arrays_and_integers() {
    let pair = lookup(
        Expr::Array {
            items: vec![r("input", &["risk"]), r("input", &["narrow"])],
        },
        vec![
            case(json!(["high", true]), lit(json!("standard-high"))),
            case(json!(["high", false]), lit(json!("complex-high"))),
        ],
        lit(json!("simple-high")),
    );
    let ranks = lookup(
        r("input", &["rank"]),
        vec![
            case(json!(1), lit(json!("simple-high"))),
            case(json!(4), lit(json!("standard-high"))),
            case(json!(7), lit(json!("complex-high"))),
        ],
        lit(json!("standard-high")),
    );
    let ctx = input_context(json!({"risk": "high", "narrow": true, "rank": 7}));
    assert_eq!(ev(&pair, &ctx), known(json!("standard-high")));
    assert_eq!(ev(&ranks, &ctx), known(json!("complex-high")));
    let ctx = input_context(json!({"risk": "high", "narrow": false, "rank": 2}));
    assert_eq!(ev(&pair, &ctx), known(json!("complex-high")));
    assert_eq!(ev(&ranks, &ctx), known(json!("standard-high")));
    for e in [pair, ranks] {
        let issues = returning(e, level_schema(&LEVELS));
        assert!(issues.is_empty(), "{issues:?}");
    }
}

#[test]
fn let_binds_in_order_and_once() {
    let e = let_(
        vec![
            bind("a", r("input", &["kind"])),
            bind("b", kind_table(r("a", &[]))),
        ],
        Expr::Array {
            items: vec![r("a", &[]), r("b", &[])],
        },
    );
    let ctx = input_context(json!({"kind": "chore"}));
    assert_eq!(ev(&e, &ctx), known(json!(["chore", "simple-high"])));
    assert!(
        returning(
            e,
            Schema::Array {
                items: Box::new(Schema::string(16)),
                max_items: 2,
            }
        )
        .is_empty()
    );
    // A binding sees only the bindings before it.
    let forward = let_(
        vec![bind("a", r("b", &[])), bind("b", lit(json!(1)))],
        r("a", &[]),
    );
    assert!(has_issue(
        &returning(
            forward,
            Schema::Integer {
                minimum: 0,
                maximum: 9
            }
        ),
        "expression_type",
        "steps.finish",
        "b"
    ));
    // Each binding is evaluated once, however often the body uses it: an
    // expensive binding used three times fits a budget that three inlined
    // copies do not.
    let costly = Expr::Map {
        collection: Box::new(lit(json!((0..50).collect::<Vec<_>>()))),
        item: "n".into(),
        value: Box::new(r("n", &[])),
        max_items: 50,
    };
    let used_thrice = let_(
        vec![bind("xs", costly.clone())],
        Expr::Array {
            items: vec![r("xs", &[]), r("xs", &[]), r("xs", &[])],
        },
    );
    let inlined = Expr::Array {
        items: vec![costly.clone(), costly.clone(), costly],
    };
    let limits = EvalLimits {
        max_elements: 100,
        ..EvalLimits::default()
    };
    assert!(eval(&used_thrice, &BTreeMap::new(), &limits).is_ok());
    assert!(eval(&inlined, &BTreeMap::new(), &limits).is_err());
}

#[test]
fn let_unknown_binding_is_not_dropped_and_missing_binding_is_testable() {
    let ctx = BTreeMap::from([
        ("input".into(), known(json!({"kind": "chore"}))),
        ("stale".into(), EvalValue::Unknown("revision".into())),
    ]);
    // Unknown is never lost, even when the body does not use the binding,
    // as `and(false, unknown)` stays unknown.
    let unused = let_(
        vec![bind("s", r("stale", &[]))],
        lit(json!("standard-high")),
    );
    assert_eq!(ev(&unused, &ctx), EvalValue::Unknown("revision".into()));
    // Missing is an observation, bound as such: `exists` reads it as false,
    // and any other use propagates Missing exactly as the inlined reference.
    let optional = let_(
        vec![bind("hint", r("input", &["hint"]))],
        Expr::Exists {
            value: Box::new(r("hint", &[])),
        },
    );
    assert_eq!(ev(&optional, &ctx), known(json!(false)));
    let used = let_(vec![bind("hint", r("input", &["hint"]))], r("hint", &[]));
    assert_eq!(ev(&used, &ctx), EvalValue::Missing);
    // Evaluation refuses a name already in the context.
    let shadow = let_(vec![bind("input", lit(json!(1)))], r("input", &[]));
    assert!(eval(&shadow, &ctx, &EvalLimits::default()).is_err());
}

#[test]
fn let_refuses_reserved_and_shadowing_names() {
    let route = |e: Expr| content(e, lit(Value::Null), Schema::Null).validate_structure();
    let body = valid_route_input();
    for name in ["input", "event", "definitions", "vars", "routed"] {
        let issues = route(let_(vec![bind(name, lit(json!(1)))], body.clone()));
        assert!(
            has_issue(&issues, "binding", "steps.route", &format!("`{name}`")),
            "{name}: {issues:?}"
        );
    }
    // A name repeated in one let, or shadowing an enclosing let.
    let repeated = let_(
        vec![bind("a", lit(json!(1))), bind("a", lit(json!(2)))],
        body.clone(),
    );
    assert!(has_issue(&route(repeated), "binding", "steps.route", "`a`"));
    let nested = let_(
        vec![bind("a", lit(json!(1)))],
        let_(vec![bind("a", lit(json!(2)))], body.clone()),
    );
    let issues = route(nested);
    assert!(
        has_issue(&issues, "binding", "steps.route", "input.body"),
        "{issues:?}"
    );
    // At most 16 bindings.
    let many = |n: usize| {
        let_(
            (0..n)
                .map(|i| bind(&format!("b{i}"), lit(json!(i))))
                .collect(),
            body.clone(),
        )
    };
    assert!(route(many(16)).is_empty());
    let issues = route(many(17));
    assert!(
        has_issue(&issues, "expression_limit", "steps.route", "17 bindings"),
        "{issues:?}"
    );
}

#[test]
fn max_of_two_ranks_without_arithmetic() {
    let max = |a: i64, b: i64| {
        let_(
            vec![bind("a", lit(json!(a))), bind("b", lit(json!(b)))],
            lookup(
                lt(r("a", &[]), r("b", &[])),
                vec![case(json!(true), r("b", &[]))],
                r("a", &[]),
            ),
        )
    };
    assert_eq!(ev(&max(5, 7), &BTreeMap::new()), known(json!(7)));
    assert_eq!(ev(&max(7, 5), &BTreeMap::new()), known(json!(7)));
}

#[test]
fn an_invoke_input_using_lookup_and_let_validates_against_an_enum_target() {
    let input: Expr = serde_json::from_value(json!({
        "op": "let",
        "bindings": [
            {"name": "kind", "value": {"op": "ref", "root": "input", "path": ["kind"]}},
            {"name": "base", "value": {
                "op": "lookup",
                "key": {"op": "ref", "root": "kind"},
                "cases": [
                    {"when": "chore", "then": {"op": "literal", "value": "simple-high"}},
                    {"when": "design", "then": {"op": "literal", "value": "complex-high"}}
                ],
                "default": {"op": "literal", "value": "standard-high"}
            }}
        ],
        "body": {"op": "object", "fields": {"level": {"op": "ref", "root": "base"}}}
    }))
    .unwrap();
    let c = content(input.clone(), lit(Value::Null), Schema::Null);
    assert_eq!(c.validate_structure(), vec![]);
    let artifact = Artifact::seal(c).unwrap();
    artifact.verify().unwrap();
    let bytes = canonical_bytes(&artifact).unwrap();
    assert_eq!(Artifact::from_canonical_bytes(&bytes).unwrap(), artifact);
    assert_eq!(
        ev(&input, &input_context(json!({"kind": "chore"}))),
        known(json!({"level": "simple-high"}))
    );
    // A level outside the command's enum is refused at the invoke step.
    let wider = let_(
        vec![bind(
            "level",
            lookup(
                r("input", &["kind"]),
                vec![case(json!("research"), lit(json!("deep-high")))],
                lit(json!("standard-high")),
            ),
        )],
        Expr::Object {
            fields: BTreeMap::from([("level".into(), r("level", &[]))]),
        },
    );
    let issues = content(wider, lit(Value::Null), Schema::Null).validate_structure();
    assert!(
        has_issue(&issues, "schema_compatibility", "steps.route", ""),
        "{issues:?}"
    );
}

/// Canonical bytes of pilot artifacts written by `aq-playbooks examples` at
/// 92fcc42, before `lookup` and `let` existed. Adding expression variants must
/// not change the bytes, digest or executable fingerprint of an artifact that
/// does not use them.
#[test]
fn existing_artifacts_keep_their_bytes_digest_and_fingerprint() {
    for (bytes, digest, fingerprint) in [
        (
            &include_bytes!("fixtures/coding.artifact.json")[..],
            "sha256:4a91ea931ca19ff66c5fa4d37a24350613a1d484dd65a1a5d299025a53f8bbdf",
            "sha256:c16cb61c26284a2b63c12b0b92882adcb3502272e277d260319b3401d6ac1765",
        ),
        (
            &include_bytes!("fixtures/repair.artifact.json")[..],
            "sha256:809a461b365c4f0f0a5b41d8e00891f5db49dda71cede56b12bfbc0c43e31b92",
            "sha256:4c5465ba9f51e79eed9cb47c22748e0e83c881930f027b7b781d5e479e6ba16e",
        ),
    ] {
        // Round trip reproduces the exact bytes and re-verifies both hashes.
        let artifact = Artifact::from_canonical_bytes(bytes).unwrap();
        assert_eq!(artifact.digest, digest);
        assert_eq!(artifact.executable_fingerprint, fingerprint);
        let resealed = Artifact::seal(artifact.content.clone()).unwrap();
        assert_eq!(resealed.digest, digest);
        assert_eq!(resealed.executable_fingerprint, fingerprint);
        assert_eq!(canonical_bytes(&resealed).unwrap(), bytes);
    }
}
