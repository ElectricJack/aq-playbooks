use super::*;

#[test]
fn a_join_accepting_every_outcome_cannot_read_a_field_only_one_has() {
    // Probe P1: `accepted_outcomes: []` qualifies every outcome on `completed`.
    let mut c = content(
        r("j", &[]),
        Branches::chosen(returns_choice()),
        Schema::string(512),
    );
    accepting(&mut c, &[]);
    refused_only_as_a_draft(
        &c,
        "steps.chosen",
        &[
            "steps.chosen.output: `answer.c.output.choice` is not there on every way to this step",
            "outcome `failed` of member `c` on edge `completed` of join `read` has no `choice` in `answer.c.output`",
            "outcome `refused` of member `c` on edge `completed` of join `read` has no `choice`",
            "`accepted_outcomes` lists only outcomes that have it",
            "`exists`",
        ],
    );
    // Probe P2: two accepted outcomes of different shapes.
    accepting(&mut c, &["assessed", "refused"]);
    refused_only_as_a_draft(&c, "steps.chosen", &["outcome `refused` of member `c`"]);
    // A field both accepted outcomes have stays readable.
    c.playbook.steps.insert(
        "chosen".into(),
        ret("chosen", r("answer", &["c", "output", "reference"])),
    );
    c.playbook
        .entries
        .get_mut("main")
        .unwrap()
        .outcomes
        .insert("chosen".into(), Schema::string(4096));
    assert_eq!(draft(&c), vec![]);
}

#[test]
fn where_two_outcome_edges_meet_a_field_only_one_has_is_refused() {
    // Probe P3: `assessed` and `failed` of one invoke lead to one step.
    let mut c = content(
        r("j", &[]),
        Branches::chosen(returns_choice()),
        Schema::string(512),
    );
    let ask = c.playbook.steps.get_mut("ask").unwrap();
    ask.edges.insert("assessed".into(), "either".into());
    ask.edges.insert("failed".into(), "either".into());
    c.playbook
        .steps
        .insert("either".into(), ret("chosen", r("j", &["choice"])));
    refused_only_as_a_draft(
        &c,
        "steps.either",
        &[
            "`j.choice`",
            "outcome `failed` of step `ask` has no `choice` in `j`",
            "after the outcome edge of the step that returns it",
        ],
    );
}

#[test]
fn on_timeout_a_member_that_has_not_answered_has_no_key() {
    // Probe P5: `timeout` alone reads `answer.c.participant`.
    let c = content(
        r("j", &[]),
        Branches {
            chosen: returns_choice(),
            unanswered: returns_null("unanswered"),
            late: ret("late", r("answer", &["c", "participant"])),
        },
        Schema::string(512),
    );
    let mut c = c;
    c.playbook
        .entries
        .get_mut("main")
        .unwrap()
        .outcomes
        .insert("late".into(), Schema::string(128));
    refused_only_as_a_draft(
        &c,
        "steps.late",
        &[
            "on edge `timeout` of join `read`, when that member has not answered, `answer.c` is absent",
        ],
    );
    // Probe P6: `completed` and `timeout` meet at one step.
    let mut c = content(
        r("j", &[]),
        Branches::chosen(ret("chosen", r("answer", &["c", "participant"]))),
        Schema::string(128),
    );
    c.playbook
        .steps
        .get_mut("read")
        .unwrap()
        .edges
        .insert("timeout".into(), "chosen".into());
    c.playbook.steps.remove("late");
    refused_only_as_a_draft(&c, "steps.chosen", &["`answer.c` is absent"]);
}

#[test]
fn exists_may_test_what_a_lookup_key_may_not_read() {
    let mut c = content(
        r("j", &[]),
        Branches::chosen(ret(
            "chosen",
            Expr::Exists {
                value: Box::new(choice()),
            },
        )),
        Schema::Boolean,
    );
    accepting(&mut c, &[]);
    assert_eq!(draft(&c), vec![]);
    // Not exempt as a lookup key: a missing key would silently take the default.
    let table = Expr::Lookup {
        key: Box::new(choice()),
        cases: vec![LookupCase {
            when: json!("approve"),
            then: lit(json!("yes")),
        }],
        default: Box::new(lit(json!("no"))),
    };
    c.playbook
        .steps
        .insert("chosen".into(), ret("chosen", table.clone()));
    c.playbook
        .entries
        .get_mut("main")
        .unwrap()
        .outcomes
        .insert("chosen".into(), Schema::string(8));
    refused_only_as_a_draft(
        &c,
        "steps.chosen",
        &[
            "steps.chosen.output.lookup.key: `answer.c.output.choice`",
            "takes its default",
        ],
    );
    // Not even inside `exists`.
    c.playbook.steps.insert(
        "chosen".into(),
        ret(
            "chosen",
            Expr::Exists {
                value: Box::new(table),
            },
        ),
    );
    c.playbook
        .entries
        .get_mut("main")
        .unwrap()
        .outcomes
        .insert("chosen".into(), Schema::Boolean);
    refused_only_as_a_draft(&c, "steps.chosen", &["exists.value.lookup.key"]);
}

#[test]
fn a_field_optional_in_its_one_outcome_stays_readable_as_a_lookup_key() {
    let mut c = content(
        r("j", &[]),
        Branches::chosen(ret(
            "chosen",
            Expr::Lookup {
                key: Box::new(r("answer", &["c", "output", "rationale"])),
                cases: vec![LookupCase {
                    when: json!("covered"),
                    then: lit(json!("yes")),
                }],
                default: Box::new(lit(json!("no"))),
            },
        )),
        Schema::string(8),
    );
    let judgment = c.registry.commands.get_mut("judgment.request").unwrap();
    let Some(Schema::Object { required, .. }) = judgment.outcomes.get_mut("assessed") else {
        unreachable!()
    };
    assert!(required.remove("rationale"));
    let fingerprint = judgment.fingerprint().unwrap();
    let Operation::Invoke { command, .. } = &mut c.playbook.steps.get_mut("ask").unwrap().op else {
        unreachable!()
    };
    command.fingerprint = fingerprint;
    assert_eq!(draft(&c), vec![]);
}

#[test]
fn a_nullable_field_that_is_not_a_join_answer_gets_no_join_advice() {
    let mut c = content(
        r("j", &[]),
        Branches::chosen(returns_null("chosen")),
        Schema::string(256),
    );
    c.playbook
        .steps
        .get_mut("ask")
        .unwrap()
        .edges
        .insert("failed".into(), "chosen".into());
    c.playbook.steps.insert(
        "chosen".into(),
        ret("chosen", r("j", &["provenance", "model"])),
    );
    let issues = c.validate_structure();
    let issue = issues
        .iter()
        .find(|i| i.path == "steps.chosen")
        .unwrap_or_else(|| panic!("{issues:?}"));
    assert!(
        issue
            .message
            .contains("`j.provenance` may be null at this step"),
        "{}",
        issue.message
    );
    assert!(issue.message.contains("`exists`"), "{}", issue.message);
    assert!(!issue.message.contains("join"), "{}", issue.message);
}
