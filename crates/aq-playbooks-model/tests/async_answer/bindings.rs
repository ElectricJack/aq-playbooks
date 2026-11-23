use super::*;

/// `peek` decides on `predicate`; every edge leads to `error`.
fn peek(predicate: Expr) -> Step {
    Step {
        op: Operation::Decide { predicate },
        edges: edges(&[("true", "error"), ("false", "error"), ("unknown", "error")]),
        on_error: None,
    }
}
fn handle_is(value: &str) -> Expr {
    Expr::Eq {
        left: Box::new(r("j", &["handle"])),
        right: Box::new(lit(json!(value))),
    }
}
#[test]
fn a_read_of_a_binding_after_its_step_failed_is_refused_in_a_draft() {
    // `ask` binds `j` on its outcome edges; its `on_error` leads to `peek`,
    // where `j` was never bound.
    let with = |predicate: Expr| {
        let mut c = base();
        c.playbook.steps.get_mut("ask").unwrap().on_error = Some("peek".into());
        c.playbook.steps.insert("peek".into(), peek(predicate));
        c
    };
    let c = with(handle_is("h"));
    // Ordinary typing accepts it, so a stored artifact keeps loading.
    assert!(
        c.validate_structure()
            .iter()
            .all(|i| i.path != "steps.peek"),
        "{:?}",
        c.validate_structure()
    );
    assert!(stored_verifies(&c));
    let issues = draft(&c);
    assert_eq!(issues.len(), 1, "{issues:?}");
    let message = &issues[0].message;
    assert_eq!(issues[0].code, "expression_type");
    assert_eq!(issues[0].path, "steps.peek");
    assert!(
        message.starts_with("steps.peek.predicate.eq.left: `j.handle` is not there on every way"),
        "{message}"
    );
    assert!(
        message.contains("on the `on_error` exit of step `ask`, `j` is not bound"),
        "{message}"
    );
    // `exists` may test it.
    let tested = with(Expr::Exists {
        value: Box::new(r("j", &["handle"])),
    });
    assert_eq!(draft(&tested), vec![]);
    // A lookup key is not exempt: a missing key would take the default silently.
    let keyed = with(Expr::Lookup {
        key: Box::new(r("j", &["handle"])),
        cases: vec![LookupCase {
            when: json!("h"),
            then: lit(json!(true)),
        }],
        default: Box::new(lit(json!(false))),
    });
    let issues = draft(&keyed);
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(
        issues[0]
            .message
            .contains("a lookup key is held to this too"),
        "{}",
        issues[0].message
    );
    // Through `vars` too.
    let through_vars = with(Expr::Eq {
        left: Box::new(r("vars", &["j", "handle"])),
        right: Box::new(lit(json!("h"))),
    });
    assert!(
        ExpectedIssue {
            code: "expression_type",
            path: "steps.peek",
            text: "`j` is not bound"
        }
        .found_in(&draft(&through_vars))
    );
}

#[test]
fn a_read_of_a_binding_reached_around_its_step_is_refused_in_a_draft() {
    // `gate` leads to `ask` or straight to `peek`; `ask`'s `accepted` edge
    // also leads to `peek`, so `j` is bound on one way and not on the other.
    let mut c = base();
    c.playbook.entries.get_mut("main").unwrap().start = "gate".into();
    c.playbook.steps.insert(
        "gate".into(),
        Step {
            op: Operation::Decide {
                predicate: lit(json!(true)),
            },
            edges: edges(&[("true", "ask"), ("false", "peek"), ("unknown", "error")]),
            on_error: None,
        },
    );
    c.playbook
        .steps
        .get_mut("ask")
        .unwrap()
        .edges
        .insert("accepted".into(), "peek".into());
    c.playbook.steps.insert("peek".into(), {
        let mut s = peek(handle_is("h"));
        s.edges = edges(&[("true", "read"), ("false", "error"), ("unknown", "error")]);
        s
    });
    assert!(stored_verifies(&c));
    let issues = draft(&c);
    let at_peek = issues
        .iter()
        .find(|i| i.path == "steps.peek")
        .unwrap_or_else(|| panic!("{issues:?}"));
    assert!(
        at_peek.message.contains(
            "on the way from the start of entry `main` that does not pass step `ask`, `j` is not bound"
        ),
        "{}",
        at_peek.message
    );
    // The join after it reads the handle on the same ways.
    assert!(
        ExpectedIssue {
            code: "expression_type",
            path: "steps.read",
            text: "`j` is not bound"
        }
        .found_in(&issues)
    );
    // Bound on every way, it is accepted.
    c.playbook
        .steps
        .get_mut("gate")
        .unwrap()
        .edges
        .insert("false".into(), "ask".into());
    assert_eq!(draft(&c), vec![]);
}
