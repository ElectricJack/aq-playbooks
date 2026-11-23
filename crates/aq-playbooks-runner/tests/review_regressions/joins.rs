use super::*;

#[test]
fn named_heterogeneous_join_results_feed_a_typed_command_and_return() {
    let e = engine();
    let evidence = Schema::object(BTreeMap::from([("subject".into(), Schema::string(32))]));
    let review = returning_child("typed-review", evidence, json!({"subject":"candidate-1"}));
    let check = returning_child("typed-check", Schema::Boolean, json!(true));
    let parent = typed_parent(&[review.clone(), check.clone()]);
    for a in [&review, &check, &parent] {
        install(&e, a, &a.content.playbook.id);
    }
    let id = e
        .admit("joining", "main", Value::Null, authority(), 0)
        .unwrap();
    let expected = json!({"subject":"candidate-1","checked":true});
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "review.command".into(),
            vec![DispatchResult::Completed {
                outcome: "ok".into(),
                output: expected.clone(),
            }],
        )]),
        ..Default::default()
    };
    e.drive(&mut host, 1, 100).unwrap();
    let run = settled(&e, &id);
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.output, Some(expected.clone()));
    assert_eq!(host.dispatched.len(), 1);
    let cohort = &e.store.settled_cohorts("review")[0];
    let intent = cohort
        .intents
        .iter()
        .find(|intent| intent.run == id)
        .unwrap();
    assert_eq!(intent.input, expected);
    let joined = &run.variables["joined"];
    for key in ["review", "check"] {
        let participant = joined[key]["participant"].as_str().unwrap();
        assert!(run.children.iter().any(|id| id == participant));
    }
}

#[test]
fn t12_any_join_uses_recorded_acceptance_order_and_tracks_unselected_children() {
    let e = engine();
    let one = artifact("one", "ok", BTreeMap::from([("ok".into(), ret("ok"))]));
    let two = artifact("two", "ok", BTreeMap::from([("ok".into(), ret("ok"))]));
    let parent = joined_parent(
        &[one.clone(), two.clone()],
        JoinMode::AnyAccepted,
        RemainingPolicy::Track,
    );
    for a in [&one, &two, &parent] {
        install(&e, a, &a.content.playbook.id);
    }
    let id = e
        .admit("joining", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    e.advance(&id, 0).unwrap();
    let s = e.snapshot().unwrap();
    let first = s.runs[&id].children[0].clone();
    let second = s.runs[&id].children[1].clone();
    e.advance(&second, 10).unwrap();
    e.advance(&first, 20).unwrap();
    e.advance(&id, 30).unwrap();
    let s = e.snapshot().unwrap();
    let joined = s.runs[&id].variables["joined"].as_object().unwrap();
    assert_eq!(joined.keys().collect::<Vec<_>>(), vec![&second]);
    assert_eq!(s.runs[&first].status, RunStatus::Completed);
}

#[test]
fn t13_independent_calls_consume_origin_budget_without_becoming_owned() {
    let (s, id) = run_limited_calls(ChildLimit::Total);
    assert_eq!(s.runs.len(), 2);
    assert_eq!(s.budgets[&id].children, 1);
    assert_eq!(s.runs[&id].status, RunStatus::Failed);
    let child = s.runs.values().find(|r| r.id != id).unwrap();
    assert_eq!(child.parent, None);
    assert_eq!(child.root, id);
    assert_eq!(child.status, RunStatus::Admitted);
}

#[test]
fn aggregate_concurrent_children_include_independent_roots() {
    let (s, id) = run_limited_calls(ChildLimit::Concurrent);
    assert_eq!(s.runs.len(), 2);
    assert_eq!(s.budgets[&id].children, 1);
    assert_eq!(s.runs[&id].step, "error");
}

#[test]
fn completed_children_before_join_remain_drivable_after_restart() {
    let e = engine();
    let one = artifact(
        "completed-one",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    let two = artifact(
        "completed-two",
        "ok",
        BTreeMap::from([("ok".into(), ret("ok"))]),
    );
    let parent = joined_parent(
        &[one.clone(), two.clone()],
        JoinMode::All,
        RemainingPolicy::Track,
    );
    for a in [&one, &two, &parent] {
        install(&e, a, &a.content.playbook.id);
    }
    let id = e
        .admit("joining", "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    e.advance(&id, 0).unwrap();
    for child in e.snapshot().unwrap().runs[&id].children.clone() {
        e.advance(&child, 1).unwrap();
    }
    let store = e.store.clone();
    e.release().unwrap();
    let e = Engine::new(store, "review", "child-restart").unwrap();
    e.drive(&mut FakeHost::default(), 2, 20).unwrap();
    assert_eq!(settled(&e, &id).outcome.as_deref(), Some("ok"));
}

fn returning_child(id: &str, schema: Schema, value: Value) -> Artifact {
    let mut child = artifact(id, "ok", BTreeMap::from([("ok".into(), ret("ok"))]));
    child
        .content
        .playbook
        .entries
        .get_mut("main")
        .unwrap()
        .outcomes
        .insert("ok".into(), schema);
    child.content.playbook.steps.get_mut("ok").unwrap().op = Operation::Return {
        outcome: "ok".into(),
        output: Expr::literal(value),
    };
    reseal(&mut child);
    child
}
fn typed_parent(children: &[Artifact]) -> Artifact {
    let mut parent = joined_parent(children, JoinMode::All, RemainingPolicy::Track);
    if let Operation::Join { member_keys, .. } =
        &mut parent.content.playbook.steps.get_mut("join").unwrap().op
    {
        *member_keys = vec!["review".into(), "check".into()];
    }
    let result_schema = Schema::object(BTreeMap::from([
        ("subject".into(), Schema::string(32)),
        ("checked".into(), Schema::Boolean),
    ]));
    let c = command(result_schema.clone(), result_schema.clone());
    parent
        .content
        .playbook
        .entries
        .get_mut("main")
        .unwrap()
        .outcomes
        .insert("ok".into(), result_schema);
    parent.content.playbook.steps.get_mut("ok").unwrap().op = Operation::Invoke {
        command: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        input: Expr::Object {
            fields: BTreeMap::from([
                (
                    "subject".into(),
                    Expr::Ref {
                        root: "joined".into(),
                        path: vec!["review".into(), "output".into(), "subject".into()],
                    },
                ),
                (
                    "checked".into(),
                    Expr::Ref {
                        root: "joined".into(),
                        path: vec!["check".into(), "output".into()],
                    },
                ),
            ]),
        },
        bind: "published".into(),
        retry: None,
    };
    parent.content.playbook.steps.get_mut("ok").unwrap().edges = edges(&[("ok", "done")]);
    parent
        .content
        .playbook
        .steps
        .get_mut("ok")
        .unwrap()
        .on_error = Some("error".into());
    parent.content.playbook.steps.insert(
        "done".into(),
        Step {
            op: Operation::Return {
                outcome: "ok".into(),
                output: Expr::reference("published"),
            },
            edges: BTreeMap::new(),
            on_error: None,
        },
    );
    parent.content.registry.commands.insert(c.name.clone(), c);
    reseal(&mut parent);
    parent
}

enum ChildLimit {
    Total,
    Concurrent,
}
fn run_limited_calls(limit: ChildLimit) -> (State, String) {
    let e = engine();
    let (child_name, parent_name) = match limit {
        ChildLimit::Total => ("independent", "limited"),
        ChildLimit::Concurrent => ("concurrent-child", "concurrent-parent"),
    };
    let child = artifact(child_name, "ok", BTreeMap::from([("ok".into(), ret("ok"))]));
    let mut parent = artifact(
        parent_name,
        "first",
        BTreeMap::from([
            (
                "first".into(),
                call("child", CallMode::Independent, "first_handle", "second"),
            ),
            (
                "second".into(),
                call("child", CallMode::Independent, "second_handle", "ok"),
            ),
            ("ok".into(), ret("ok")),
            ("error".into(), ret("error")),
        ]),
    );
    match limit {
        ChildLimit::Total => parent.content.playbook.limits.max_children = 1,
        ChildLimit::Concurrent => parent.content.playbook.limits.max_concurrent_children = 1,
    }
    parent
        .content
        .dependencies
        .insert("child".into(), dependency(&child));
    reseal(&mut parent);
    install(&e, &child, child_name);
    install(&e, &parent, parent_name);
    let id = e
        .admit(parent_name, "main", Value::Null, authority(), 0)
        .unwrap();
    e.advance(&id, 0).unwrap();
    e.advance(&id, 1).unwrap();
    let s = e.snapshot().unwrap();
    (s, id)
}
