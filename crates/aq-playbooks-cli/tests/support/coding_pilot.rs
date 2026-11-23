use super::*;

pub(super) fn coding() -> anyhow::Result<Scenario> {
    let r = catalog();
    let implementation = work_child(WorkPilot::Implementation)?;
    let review = work_child(WorkPilot::Review)?;
    let repair = work_child(WorkPilot::Repair)?;
    let validation = validation(&r)?;
    let followup = followup(&r)?;
    let mut steps = candidate_steps(&r);
    add_publication(&mut steps, &r);
    let root = build(
        PilotSpec {
            id: "coding",
            input: Schema::Null,
            output: Schema::Null,
            start: "implementation",
        },
        r,
        steps,
        &[
            ("implementation", &implementation),
            ("review", &review),
            ("repair", &repair),
            ("validation", &validation),
            ("report", &followup),
        ],
    )?;
    Ok(base(
        vec![implementation, review, repair, validation, followup, root],
        "coding",
        BTreeMap::from([
            (
                "context.read".into(),
                vec![base_observation(1), base_observation(2)],
            ),
            (
                "work.ensure".into(),
                vec![done("completed"), done("completed"), done("completed")],
            ),
            (
                "candidate.prepare".into(),
                vec![done("prepared"), done("prepared")],
            ),
            ("check.request".into(), vec![done("passed"), done("passed")]),
            (
                "candidate.publish".into(),
                vec![done("conflict"), done("published")],
            ),
            ("message.send".into(), vec![done("queued")]),
        ]),
        vec![],
    ))
}
fn validation(r: &Registry) -> anyhow::Result<Artifact> {
    build(
        PilotSpec {
            id: "validation",
            input: subject(),
            output: result(),
            start: "check",
        },
        r.clone(),
        BTreeMap::from([
            (
                "check".into(),
                invoke(
                    &r.commands["check.request"],
                    fields(&[
                        ("subject", reference("input", &[])),
                        ("suite", lit(json!("integration"))),
                        ("execution_context", lit(json!("fake-ci"))),
                    ]),
                    "check",
                    &[("passed", "done")],
                ),
            ),
            ("done".into(), ret("success", reference("check", &[]))),
        ]),
        &[],
    )
}
fn followup(r: &Registry) -> anyhow::Result<Artifact> {
    build(
        PilotSpec {
            id: "post-delivery-report",
            input: Schema::Null,
            output: Schema::Null,
            start: "send",
        },
        r.clone(),
        BTreeMap::from([
            (
                "send".into(),
                invoke(
                    &r.commands["message.send"],
                    lit(
                        json!({"identity":"coding-delivered","recipient":"supervisor","visibility":"project","body":"Candidate delivered"}),
                    ),
                    "message",
                    &[("queued", "done")],
                ),
            ),
            ("done".into(), ret("success", lit(Value::Null))),
        ]),
        &[],
    )
}
fn candidate_steps(r: &Registry) -> BTreeMap<String, Step> {
    let mut steps = BTreeMap::new();
    steps.insert(
        "implementation".into(),
        edge(
            child_call(
                "implementation",
                lit(Value::Null),
                CallMode::Wait,
                "implementation_result",
            ),
            &[("success", "review"), ("escalated", "failure")],
        ),
    );
    steps.insert(
        "review".into(),
        edge(
            child_call("review", lit(Value::Null), CallMode::Wait, "review_result"),
            &[("success", "attempts"), ("escalated", "failure")],
        ),
    );
    steps.insert(
        "attempts".into(),
        edge(
            Operation::ForEach {
                collection: lit(json!(["repair-1", "repair-2", "exhausted"])),
                item: "attempt".into(),
                body: "observe_base".into(),
                next: "failure".into(),
                max_items: 3,
                concurrency: 1,
                on_failure: IterationFailure::Stop,
            },
            &[],
        ),
    );
    steps.insert(
        "can_repair".into(),
        edge(
            Operation::Decide {
                predicate: Expr::Eq {
                    left: Box::new(reference("attempt", &[])),
                    right: Box::new(lit(json!("exhausted"))),
                },
            },
            &[
                ("true", "failure"),
                ("false", "repair"),
                ("unknown", "failure"),
            ],
        ),
    );
    steps.insert("observe_base".into(),invoke(&r.commands["context.read"],lit(json!({"view":"project","subject":{"id":"main","revision":1},"snapshot":"","page":0})),"base",&[("observed","prepare")]));
    steps
}
fn add_publication(steps: &mut BTreeMap<String, Step>, r: &Registry) {
    steps.insert(
        "prepare".into(),
        invoke(
            &r.commands["candidate.prepare"],
            fields(&[
                ("sources", lit(json!([subject_value()]))),
                ("base", reference("base", &["subject"])),
            ]),
            "candidate",
            &[("prepared", "validate"), ("conflict", "can_repair")],
        ),
    );
    steps.insert(
        "validate".into(),
        edge(
            child_call(
                "validation",
                reference("candidate", &["subject"]),
                CallMode::Wait,
                "checks",
            ),
            &[("success", "publish"), ("escalated", "can_repair")],
        ),
    );
    steps.insert(
        "publish".into(),
        invoke(
            &r.commands["candidate.publish"],
            fields(&[
                ("candidate", reference("candidate", &["subject"])),
                ("destination", reference("base", &["subject"])),
                (
                    "evidence",
                    Expr::Array {
                        items: vec![reference("checks", &["subject"])],
                    },
                ),
            ]),
            "published",
            &[("published", "followup"), ("conflict", "can_repair")],
        ),
    );
    steps.insert(
        "repair".into(),
        edge(
            child_call(
                "repair",
                reference("attempt", &[]),
                CallMode::Wait,
                "repair_result",
            ),
            &[("success", "attempts"), ("escalated", "failure")],
        ),
    );
    steps.insert(
        "followup".into(),
        edge(
            child_call(
                "report",
                lit(Value::Null),
                CallMode::Independent,
                "report_handle",
            ),
            &[("started", "done")],
        ),
    );
    steps.insert("done".into(), ret("success", lit(Value::Null)));
}
