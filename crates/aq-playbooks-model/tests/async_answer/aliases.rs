use super::*;

fn let_(name: &str, value: Expr, body: Expr) -> Expr {
    Expr::Let {
        bindings: vec![LetBinding {
            name: name.into(),
            value,
        }],
        body: Box::new(body),
    }
}
/// `content` whose `chosen` step returns `output`, typed `chosen_type`.
fn choosing(output: Expr, chosen_type: Schema, accepted: &[&str]) -> ArtifactContent {
    let mut c = content(
        r("j", &[]),
        Branches::chosen(ret("chosen", output)),
        chosen_type,
    );
    accepting(&mut c, accepted);
    c
}

#[test]
fn a_read_through_a_let_alias_is_held_to_the_same_check() {
    // Probe F4: the alias of `answer.c.output` read for `choice`, where every
    // outcome is accepted, is refused exactly as the direct read is.
    let alias = || let_("out", r("answer", &["c", "output"]), r("out", &["choice"]));
    let c = choosing(alias(), Schema::string(512), &[]);
    refused_only_as_a_draft(
        &c,
        "steps.chosen",
        &[
            "`out.choice` (`out` is `answer.c.output`) is not there on every way to this step",
            "outcome `failed` of member `c` on edge `completed` of join `read` has no `choice` in `answer.c.output`",
            "outcome `refused` of member `c` on edge `completed` of join `read` has no `choice`",
        ],
    );
    let direct = choosing(choice(), Schema::string(512), &[]);
    let lacks = |c: &ArtifactContent| {
        let message = draft(c)[0].message.clone();
        message.split_once(": ").unwrap().1.to_string()
    };
    let (alias_reasons, direct_reasons) = (lacks(&c), lacks(&direct));
    let ways = |m: &str| m.split_once("this step: ").unwrap().1.to_string();
    assert_eq!(ways(&alias_reasons), ways(&direct_reasons));
    // Probe F6: as a lookup key the alias would take the default silently.
    let keyed = choosing(
        let_(
            "out",
            r("answer", &["c", "output"]),
            Expr::Lookup {
                key: Box::new(r("out", &["choice"])),
                cases: vec![LookupCase {
                    when: json!("approve"),
                    then: lit(json!("a")),
                }],
                default: Box::new(lit(json!("b"))),
            },
        ),
        Schema::string(512),
        &[],
    );
    refused_only_as_a_draft(
        &keyed,
        "steps.chosen",
        &["a lookup key is held to this too"],
    );
}

#[test]
fn aliases_can_read_common_fields_and_probe_the_others_with_exists() {
    let alias = || let_("out", r("answer", &["c", "output"]), r("out", &["choice"]));
    // An alias of a field every way has is fine, and so is an alias of an
    // alias.
    assert_eq!(
        draft(&choosing(alias(), Schema::string(512), &["assessed"])),
        vec![]
    );
    let twice = let_(
        "a",
        r("answer", &["c"]),
        let_("b", r("a", &["output"]), r("b", &["choice"])),
    );
    assert_eq!(
        draft(&choosing(twice.clone(), Schema::string(512), &["assessed"])),
        vec![]
    );
    refused_only_as_a_draft(
        &choosing(twice, Schema::string(512), &[]),
        "steps.chosen",
        &[
            "(`b` is `answer.c.output`)",
            "outcome `failed` of member `c`",
        ],
    );
    // Probe F7: an alias used only inside `exists` is fine.
    let tested = let_(
        "x",
        choice(),
        Expr::Exists {
            value: Box::new(r("x", &[])),
        },
    );
    assert_eq!(draft(&choosing(tested, Schema::Boolean, &[])), vec![]);
}

#[test]
fn an_item_of_a_collection_derived_from_an_outcome_is_held_to_the_same_check() {
    // Probe F5: a `map` item that is `answer.c.output`, read for `choice`.
    let list = Schema::Array {
        items: Box::new(Schema::string(512)),
        max_items: 1,
    };
    let over = |make: fn(Box<Expr>, Box<Expr>) -> Expr| {
        make(
            Box::new(Expr::Array {
                items: vec![r("answer", &["c", "output"])],
            }),
            Box::new(r("o", &["choice"])),
        )
    };
    let map = |collection, value| Expr::Map {
        collection,
        item: "o".into(),
        value,
        max_items: 1,
    };
    refused_only_as_a_draft(
        &choosing(over(map), list.clone(), &[]),
        "steps.chosen",
        &[
            "`o.choice` (`o` is an item of its collection) is not there on every way",
            "outcome `refused` of member `c` on edge `completed` of join `read` has no `choice`",
        ],
    );
    assert_eq!(
        draft(&choosing(over(map), list.clone(), &["assessed"])),
        vec![]
    );
    // A `filter` predicate likewise.
    let filter = |collection, value: Box<Expr>| Expr::Filter {
        collection,
        item: "o".into(),
        predicate: Box::new(Expr::Eq {
            left: value,
            right: Box::new(lit(json!("approve"))),
        }),
        max_items: 1,
    };
    let filtered = choosing(
        over(filter),
        Schema::Array {
            items: Box::new(Schema::object(BTreeMap::new())),
            max_items: 1,
        },
        &[],
    );
    let issues = draft(&filtered);
    assert!(
        issues.iter().any(
            |i| i.code == "expression_type" && i.message.contains("`o.choice` (`o` is an item")
        ),
        "{issues:?}"
    );
}

#[test]
fn an_item_named_like_a_binding_gets_no_second_unbound_diagnostic() {
    // A3 re-review m1: `error` is reached where `answer` was never bound; an
    // item named `answer` there is refused by typing alone.
    let mut c = base();
    c.playbook.steps.insert(
        "error".into(),
        ret(
            "error",
            Expr::Map {
                collection: Box::new(lit(json!(["x"]))),
                item: "answer".into(),
                value: Box::new(r("answer", &[])),
                max_items: 1,
            },
        ),
    );
    let issues = draft(&c);
    assert!(!issues.is_empty());
    assert!(
        issues.iter().all(|i| !i.message.contains("is not bound")),
        "{issues:?}"
    );
}

#[test]
fn a_single_member_any_accepted_join_reads_its_member_on_completed() {
    // A2 re-review R1-M1: with one member, that member is the one that
    // qualified, so `answer.c` is there on `completed`.
    let mut c = base();
    let Operation::Join { mode, .. } = &mut c.playbook.steps.get_mut("read").unwrap().op else {
        unreachable!()
    };
    *mode = JoinMode::AnyAccepted;
    assert_eq!(c.validate_structure(), vec![]);
    assert_eq!(draft(&c), vec![]);
    // The member's outcome is still held to the accepted outcomes.
    accepting(&mut c, &["assessed", "refused"]);
    let issues = draft(&c);
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(
        issues[0]
            .message
            .contains("outcome `refused` of member `c`"),
        "{}",
        issues[0].message
    );
    assert!(
        !issues[0].message.contains("another member"),
        "{}",
        issues[0].message
    );
}
