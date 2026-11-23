use super::*;
use aq_playbooks_compiler::draft::{SourceRecord, Workspace, seal};
use aq_playbooks_model::{EvalContext, EvalLimits, EvalValue, Expr, Registry, eval};
use serde_json::json;

const REGISTRY_2_2: &str = include_str!("../../../../contracts/aq-host-2.2.0/registry.json");
const ROUTING_DRAFT: &str = include_str!("../../../aq-playbooks-cli/tests/data/routing_draft.json");

fn strings(xs: &[&str]) -> std::collections::BTreeSet<String> {
    xs.iter().map(|s| s.to_string()).collect()
}
fn facts() -> BTreeMap<String, Value> {
    let everyone = json!({"harnesses": ["claude", "codex", "gemini", "opencode"],
                              "providers": ["anthropic", "google", "ollama", "openai"]});
    BTreeMap::from([
        (
            "levels".into(),
            json!(["simple-high", "standard-high", "complex-high", "deep-high"]),
        ),
        (
            "offers".into(),
            json!({"simple-high": everyone, "standard-high": everyone,
                       "complex-high": everyone, "deep-high": everyone}),
        ),
        (
            "harnesses".into(),
            json!(["claude", "codex", "gemini", "opencode"]),
        ),
        (
            "providers".into(),
            json!(["anthropic", "google", "ollama", "openai"]),
        ),
    ])
}
struct Registrations<'a> {
    commands: &'a [&'a str],
    events: &'a [&'a str],
    capabilities: &'a [&'a str],
}
fn sealed(registrations: Registrations<'_>, draft: Value) -> Artifact {
    let ws = Workspace {
        markdown: "# Routing\n\nThe owner's routing document.\n".into(),
        record: SourceRecord {
            id: "routing".into(),
            binding: "aq.routing".into(),
            scope: "project".into(),
            purpose: "Route each task".into(),
            commands: strings(registrations.commands),
            events: strings(registrations.events),
            capabilities: strings(registrations.capabilities),
            facts_version: "settings-1".into(),
        },
        catalog: serde_json::from_str::<Registry>(REGISTRY_2_2).unwrap(),
        facts: facts(),
        draft,
    };
    seal(&ws).unwrap_or_else(|d| panic!("{d:#?}"))
}
/// The owner's routing document as a draft (the A4 feasibility draft),
/// sealed against catalog 2.2.0 with the owner's Settings as facts.
fn routing() -> Artifact {
    sealed(
        Registrations {
            commands: &["work.read", "work.route", "judgment.assess"],
            events: &["aq.work.route_needed"],
            capabilities: &[
                "work.read",
                "work.route",
                "judgment.assess",
                "operation.get",
            ],
        },
        serde_json::from_str(ROUTING_DRAFT).unwrap(),
    )
}
fn subject() -> Value {
    json!({"id": "task-1", "revision": 3})
}
fn output(r: &CaseReturn) -> Value {
    match r {
        CaseReturn::Answer { output, .. } | CaseReturn::AtOnce { output, .. } => output.clone(),
        CaseReturn::Pending { .. } => panic!("no output"),
    }
}
fn observed(filed_kind: &str) -> CaseReturn {
    CaseReturn::Answer {
        outcome: "observed".into(),
        output: json!({"reference": "task-1@3", "subject": subject(), "facts": {
                "kind": filed_kind, "title": "A task", "description": "", "origin": "owner",
                "level_hint": "", "charged_failures": 0, "ended_attempts": 0, "route": "none"}}),
    }
}
fn assessed(labels: [&str; 2], flags: &[&str]) -> CaseReturn {
    CaseReturn::Answer {
        outcome: "assessed".into(),
        output: json!({"reference": "judgment:op-1", "subject": subject(),
                "labels": labels, "flags": flags, "rationale": "Because.",
                "provenance": {"context_digest": "sha256:a", "credential_profile": "profile/a",
                    "model": "model-a", "output_digest": null, "prompt_digest": "sha256:c",
                    "provider": "provider-a",
                    "usage": {"cost_microunits": 1, "input_tokens": 1, "output_tokens": 1}}}),
    }
}
fn routed() -> CaseReturn {
    CaseReturn::Answer {
        outcome: "routed".into(),
        output: json!({"reference": "route-1", "subject": subject(),
                "harnesses": [], "providers": [], "prefer": []}),
    }
}
/// A step's input, evaluated as the draft writes it.
fn step_input(step: &str, roots: &[(&str, Value)]) -> Value {
    let draft: Value = serde_json::from_str(ROUTING_DRAFT).unwrap();
    let expr: Expr = serde_json::from_value(draft["steps"][step]["op"]["input"].clone()).unwrap();
    let mut context = EvalContext::from([(
        "definitions".to_string(),
        EvalValue::Known(json!(
            facts().into_iter().collect::<serde_json::Map<_, _>>()
        )),
    )]);
    for (root, value) in roots {
        context.insert(root.to_string(), EvalValue::Known(value.clone()));
    }
    match eval(&expr, &context, &EvalLimits::default()).unwrap() {
        EvalValue::Known(value) => value,
        other => panic!("{step}: {other:?}"),
    }
}
fn call(command: &str, input: Value) -> ExpectedCall {
    ExpectedCall {
        command: command.into(),
        input,
    }
}
/// Every call a run of the routing draft makes, each with its whole
/// input: read, ask, then `route` on an assessed answer or `assume`.
fn calls_for(read: &CaseReturn, answer: Option<&CaseReturn>) -> Vec<ExpectedCall> {
    let task = output(read);
    let route = match answer {
        Some(answer) => step_input(
            "route",
            &[
                ("task", task.clone()),
                (
                    "answer",
                    json!({"c": {"participant": "op", "outcome": "assessed", "output": output(answer)}}),
                ),
            ],
        ),
        None => step_input("assume", &[("task", task.clone())]),
    };
    vec![
        call("work.read", json!({"subject": subject()})),
        call("judgment.assess", step_input("ask", &[("task", task)])),
        call("work.route", route),
    ]
}
fn case(returns: Vec<(&str, Vec<CaseReturn>)>, outcome: &str, calls: Vec<ExpectedCall>) -> Case {
    Case {
        name: "a task".into(),
        situation: "A task needs a route".into(),
        start: CaseStart::Entry {
            entry: "main".into(),
            input: json!({"task": subject()}),
        },
        returns: returns
            .into_iter()
            .map(|(c, r)| (c.to_string(), r))
            .collect(),
        expect: Expectation {
            outcome: outcome.into(),
            calls,
        },
    }
}
fn classified(filed: &str, answer: CaseReturn) -> Case {
    let read = observed(filed);
    let calls = calls_for(&read, Some(&answer));
    case(
        vec![
            ("work.read", vec![read]),
            ("judgment.assess", vec![answer]),
            ("work.route", vec![routed()]),
        ],
        "routed",
        calls,
    )
}
fn feature_case() -> Case {
    classified("feature", assessed(["feature", "low"], &[]))
}
fn route_call(report: &CaseReport) -> &Call {
    let routes: Vec<&Call> = report
        .calls
        .iter()
        .filter(|c| c.command == "work.route")
        .collect();
    assert_eq!(routes.len(), 1, "{report:#?}");
    routes[0]
}
fn problem<'a>(report: &'a CaseReport, starts: &str) -> &'a str {
    report
        .problems
        .iter()
        .find(|p| p.starts_with(starts))
        .unwrap_or_else(|| panic!("no problem starting {starts:?}: {report:#?}"))
}

#[test]
fn a_case_records_each_call_with_its_input() {
    let artifact = routing();
    let report = run_case(&artifact, &feature_case()).unwrap();
    assert!(report.passed, "{report:#?}");
    assert_eq!(report.outcome.as_deref(), Some("routed"));
    let seen: Vec<(&str, &str, Option<&str>)> = report
        .calls
        .iter()
        .map(|c| (c.step.as_str(), c.command.as_str(), c.outcome.as_deref()))
        .collect();
    assert_eq!(
        seen,
        [
            ("read", "work.read", Some("observed")),
            ("ask", "judgment.assess", Some("assessed")),
            ("route", "work.route", Some("routed")),
        ]
    );
    assert_eq!(report.calls[0].input, json!({"subject": subject()}));
    assert_eq!(
        report.calls[1].input["context"]["items"][0]["text"],
        "A task"
    );
    let route = &report.calls[2].input;
    assert_eq!(route["subject"], subject());
    assert_eq!(route["level"], "standard-high");
    assert_eq!(route["classification"]["labels"], json!(["feature", "low"]));
}

/// The A2 runtime proof: the route's level comes from the judgment's
/// first label, read through the named join, not from the kind the
/// filer gave (always "feature" here).
#[test]
fn an_async_answer_reaches_the_playbook_through_the_join() {
    let artifact = routing();
    for (kind, flags, level) in [
        ("chore", &[][..], "simple-high"),
        ("feature", &[][..], "standard-high"),
        ("research", &["clearly_harder"][..], "complex-high"),
        ("spec", &["needs_deep_reasoning"][..], "deep-high"),
    ] {
        let report = run_case(
            &artifact,
            &classified("feature", assessed([kind, "low"], flags)),
        )
        .unwrap();
        assert!(report.passed, "{kind}: {report:#?}");
        let route = route_call(&report);
        assert_eq!(route.step, "route", "{kind}");
        assert_eq!(route.input["level"], level, "{kind}");
        assert_eq!(route.input["classification"]["labels"][0], kind);
    }
}

#[test]
fn a_pending_answer_takes_the_timeout_path() {
    let artifact = routing();
    let read = observed("bugfix");
    let pending = case(
        vec![
            ("work.read", vec![read.clone()]),
            (
                "judgment.assess",
                vec![CaseReturn::Pending { pending: true }],
            ),
            ("work.route", vec![routed()]),
        ],
        "routed",
        calls_for(&read, None),
    );
    let report = run_case(&artifact, &pending).unwrap();
    assert!(report.passed, "{report:#?}");
    let ask = report
        .calls
        .iter()
        .find(|c| c.command == "judgment.assess")
        .unwrap();
    assert_eq!(ask.outcome, None, "no answer came");
    let route = route_call(&report);
    assert_eq!(route.step, "assume");
    assert_eq!(route.input["level"], "standard-high");
    assert_eq!(route.input["classification"], Value::Null);
    let codes: Vec<&Value> = route.input["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| &r["code"])
        .collect();
    assert!(codes.contains(&&json!("risk_assumed")), "{codes:?}");
}

#[test]
fn a_call_without_a_return_fails_the_case_and_names_the_step() {
    let artifact = routing();
    let mut missing = feature_case();
    missing.returns.remove("work.route");
    let report = run_case(&artifact, &missing).unwrap();
    assert!(!report.passed, "{report:#?}");
    problem(
        &report,
        "step route called work.route but the case gives no return for it",
    );
    let route = route_call(&report);
    assert_eq!(route.outcome, None);
    assert_eq!(
        route.input["level"], "standard-high",
        "the input is still recorded"
    );
}

/// A case lists every call the run makes, in order: a call it does not
/// list fails it, and so does one it lists that the run does not make.
#[test]
fn every_call_the_run_makes_must_be_listed_in_order() {
    let artifact = routing();
    let full = classified("feature", assessed(["design", "low"], &[]));
    assert!(run_case(&artifact, &full).unwrap().passed);
    // The review's probe: naming only the route, even with its whole
    // input, no longer passes.
    let mut only_route = full.clone();
    only_route.expect.calls = vec![full.expect.calls[2].clone()];
    let report = run_case(&artifact, &only_route).unwrap();
    assert!(!report.passed);
    let p = problem(&report, "expect.calls[0]");
    assert!(p.contains("work.route") && p.contains("work.read"), "{p}");
    // No calls listed at all.
    let mut none = full.clone();
    none.expect.calls = vec![];
    let report = run_case(&artifact, &none).unwrap();
    assert!(!report.passed);
    let p = problem(
        &report,
        "expect.calls: the run called work.read at step read",
    );
    assert!(p.contains("lists 0"), "{p}");
    // Out of order.
    let mut swapped = full.clone();
    swapped.expect.calls.swap(0, 1);
    let report = run_case(&artifact, &swapped).unwrap();
    assert!(!report.passed);
    problem(&report, "expect.calls[0]");
    // One call more than the run makes.
    let mut more = full.clone();
    more.expect.calls.push(full.expect.calls[2].clone());
    let report = run_case(&artifact, &more).unwrap();
    assert!(!report.passed);
    let p = problem(&report, "expect.calls[3]");
    assert!(p.contains("made 3 calls"), "{p}");
}

#[test]
fn an_expected_input_must_equal_the_call_exactly() {
    let artifact = routing();
    let full = classified("feature", assessed(["design", "low"], &[]));
    // The review's probe: the level is wrong, and the case fails with
    // the place, what it expects and what the call had.
    let mut wrong = full.clone();
    wrong.expect.calls[2].input["level"] = json!("deep-high");
    let report = run_case(&artifact, &wrong).unwrap();
    assert!(!report.passed);
    let p = problem(&report, "expect.calls[2].input");
    assert!(
        p.contains("input.level: the case expects \"deep-high\", the call has \"complex-high\""),
        "{p}"
    );
    // A part of the input is not enough.
    let mut part = full.clone();
    part.expect.calls[2].input = json!({"level": "complex-high"});
    let report = run_case(&artifact, &part).unwrap();
    assert!(!report.passed);
    let p = problem(&report, "expect.calls[2].input");
    assert!(
        p.contains(
            "input.harnesses: the case does not list it; the call has [\"claude\",\"codex\"]"
        ),
        "{p}"
    );
    // An array must be equal, not a part of it.
    let mut fewer = full.clone();
    fewer.expect.calls[2].input["harnesses"] = json!(["claude"]);
    let report = run_case(&artifact, &fewer).unwrap();
    assert!(!report.passed);
    let p = problem(&report, "expect.calls[2].input");
    assert!(p.contains("input.harnesses"), "{p}");
}

#[test]
fn a_return_the_run_never_uses_fails_the_case() {
    let artifact = routing();
    let mut extra = classified("feature", assessed(["design", "low"], &[]));
    extra.returns.get_mut("work.route").unwrap().push(routed());
    let report = run_case(&artifact, &extra).unwrap();
    assert!(!report.passed, "{report:#?}");
    let p = problem(&report, "returns.\"work.route\"[1]");
    assert!(p.contains("never used"), "{p}");
}

#[test]
fn the_calls_and_each_input_are_required() {
    let case =
        serde_json::to_value(classified("feature", assessed(["design", "low"], &[]))).unwrap();
    let mut no_calls = case.clone();
    no_calls["expect"].as_object_mut().unwrap().remove("calls");
    let error = serde_json::from_value::<Case>(no_calls)
        .unwrap_err()
        .to_string();
    assert!(error.contains("calls"), "{error}");
    let mut no_input = case;
    no_input["expect"]["calls"][0]
        .as_object_mut()
        .unwrap()
        .remove("input");
    let error = serde_json::from_value::<Case>(no_input)
        .unwrap_err()
        .to_string();
    assert!(error.contains("input"), "{error}");
}

#[test]
fn a_triggered_case_starts_from_the_event() {
    let artifact = routing();
    let mut c = classified("feature", assessed(["chore", "low"], &[]));
    c.start = CaseStart::Event {
        event: "aq.work.route_needed".into(),
        payload: json!({"subject": {"id": "task-1", "revision": 2}}),
        correlation: BTreeMap::new(),
    };
    c.expect.calls[0].input = json!({"subject": {"id": "task-1", "revision": 2}});
    let report = run_case(&artifact, &c).unwrap();
    assert!(report.passed, "{report:#?}");
    // An event the playbook has no trigger for starts nothing.
    c.start = CaseStart::Event {
        event: "aq.judgment.assessed".into(),
        payload: json!({}),
        correlation: BTreeMap::new(),
    };
    let report = run_case(&artifact, &c).unwrap();
    assert!(!report.passed);
    problem(&report, "start.event");
}

/// Any event a trigger names can start a case, with the correlation
/// values the case gives; `aq.work.changed` is correlated by `operation`.
#[test]
fn a_case_starts_from_any_trigger_event_with_its_correlation() {
    let routing: Value = serde_json::from_str(ROUTING_DRAFT).unwrap();
    let ret = |outcome: &str| {
        json!({"op": {"kind": "return", "outcome": outcome,
            "output": {"op": "literal", "value": null}}, "edges": {}, "on_error": null})
    };
    let draft = json!({
        "entries": {"main": {"input": routing["entries"]["main"]["input"],
            "outcomes": {"seen": {"type": "null"}, "failed": {"type": "null"}}, "start": "read"}},
        "triggers": [{"id": "changed", "event": "aq.work.changed", "entry": "main",
            "input": {"op": "object", "fields": {"task": {"op": "ref", "root": "event", "path": ["subject"]}}}}],
        "steps": {
            "read": {"op": {"kind": "invoke", "command": "work.read", "bind": "task", "retry": null,
                "input": {"op": "object", "fields": {"subject": {"op": "ref", "root": "input", "path": ["task"]}}}},
                "edges": {"observed": "seen", "refused": "failed"}, "on_error": "failed"},
            "seen": ret("seen"),
            "failed": ret("failed"),
        },
    });
    let artifact = sealed(
        Registrations {
            commands: &["work.read"],
            events: &["aq.work.changed"],
            capabilities: &["work.read"],
        },
        draft,
    );
    let changed = |correlation: &[(&str, &str)]| Case {
        name: "changed".into(),
        situation: "A task changed".into(),
        start: CaseStart::Event {
            event: "aq.work.changed".into(),
            payload: json!({"outcome": "changed", "reference": "task-1@3", "subject": subject()}),
            correlation: correlation
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        },
        returns: BTreeMap::from([("work.read".into(), vec![observed("feature")])]),
        expect: Expectation {
            outcome: "seen".into(),
            calls: vec![call("work.read", json!({"subject": subject()}))],
        },
    };
    let report = run_case(&artifact, &changed(&[("operation", "op-7")])).unwrap();
    assert!(report.passed, "{report:#?}");
    let report = run_case(&artifact, &changed(&[])).unwrap();
    assert!(!report.passed);
    assert!(problem(&report, "start.correlation").contains("operation"));
    let report = run_case(
        &artifact,
        &changed(&[("operation", "op-7"), ("other", "x")]),
    )
    .unwrap();
    assert!(!report.passed);
    assert!(problem(&report, "start.correlation").contains("other"));
}

#[test]
fn fewer_than_five_or_more_than_forty_cases_are_refused() {
    let artifact = routing();
    let one = feature_case();
    let named = |n: usize| {
        (0..n)
            .map(|i| Case {
                name: format!("case {i}"),
                ..one.clone()
            })
            .collect::<Vec<_>>()
    };
    for n in [0, 4, 41] {
        let error = run_cases(&artifact, &named(n), CASES_TRANSITION_BUDGET)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("at least 5") && error.contains("at most 40"),
            "{error}"
        );
    }
    for n in [MIN_CASES, MAX_CASES] {
        let reports = run_cases(&artifact, &named(n), CASES_TRANSITION_BUDGET).unwrap();
        assert_eq!(reports.len(), n);
        assert!(reports.iter().all(|r| r.passed));
    }
    // Each case has its own name.
    let mut twins = named(5);
    twins[1].name = twins[0].name.clone();
    let error = run_cases(&artifact, &twins, CASES_TRANSITION_BUDGET)
        .unwrap_err()
        .to_string();
    assert!(error.contains("case 0"), "{error}");
}

/// All the cases of a save share one transition budget, and running out
/// of it is its own failure, named by the case it stopped at.
#[test]
fn the_cases_share_a_total_transition_budget() {
    let artifact = routing();
    let one = feature_case();
    let cases: Vec<Case> = (0..5)
        .map(|i| Case {
            name: format!("case {i}"),
            ..one.clone()
        })
        .collect();
    let error = run_cases(&artifact, &cases, 30).unwrap_err().to_string();
    assert!(
        error.starts_with("the cases exceeded the total budget of 30 transitions at case "),
        "{error}"
    );
    assert!(error.contains("\"case 2\""), "{error}");
    assert!(run_cases(&artifact, &cases, CASES_TRANSITION_BUDGET).is_ok());
}

#[test]
fn a_sentence_is_quoted_from_its_lines() {
    let markdown = "# Title\n\n- Research is Standard, unless it is clearly harder,\n  in which case it is Complex.\n\nLast.\n";
    let one = quote(markdown, 6, None).unwrap();
    assert_eq!(
        serde_json::to_value(&one).unwrap(),
        json!({"line": 6, "text": "Last."})
    );
    let two = quote(markdown, 3, Some(4)).unwrap();
    assert_eq!(
        serde_json::to_value(&two).unwrap(),
        json!({"line": 3, "end_line": 4,
                   "text": "- Research is Standard, unless it is clearly harder,\n  in which case it is Complex."})
    );
    assert_eq!(quote(markdown, 6, Some(6)).unwrap(), one);
    let past = quote(markdown, 6, Some(7)).unwrap_err();
    assert!(past.contains("1 to 6"), "{past}");
    assert!(quote(markdown, 2, None).unwrap_err().contains("blank"));
    assert!(quote(markdown, 3, Some(5)).unwrap_err().contains("blank"));
    assert!(quote(markdown, 4, Some(3)).is_err());
    let long = "x\n".repeat(20);
    assert!(quote(&long, 1, Some(8)).is_ok());
    assert!(quote(&long, 1, Some(9)).unwrap_err().contains("8 lines"));
    // The outcome holds its sentences in document order, apart.
    let question = |sentences: Vec<Sentence>| Outcome::Question {
        question: "Which?".into(),
        sentences,
    };
    assert!(question(vec![two.clone(), one.clone()]).check().is_ok());
    assert!(question(vec![one.clone(), two.clone()]).check().is_err());
    assert!(
        question(vec![two.clone(), quote(markdown, 4, None).unwrap()])
            .check()
            .is_err()
    );
    let bad = json!({"kind": "question", "question": "Which?",
                         "sentences": [{"line": 6, "end_line": 6, "text": "Last."}]});
    assert!(Outcome::from_bytes(&serde_json::to_vec(&bad).unwrap()).is_err());
    let unable = json!({"kind": "unable", "reason": "No command does it.", "line": 3,
            "end_line": 4, "sentence": two.text});
    assert!(Outcome::from_bytes(&serde_json::to_vec(&unable).unwrap()).is_ok());
}

/// Run by hand in release: `cargo test --release -p aq-playbooks-simulation
/// -- --ignored forty_routing_cases --nocapture`.
#[test]
#[ignore = "a timing, run by hand in release"]
fn forty_routing_cases_take_this_long() {
    let artifact = routing();
    let one = classified("feature", assessed(["design", "low"], &[]));
    let cases: Vec<Case> = (0..MAX_CASES)
        .map(|i| Case {
            name: format!("case {i}"),
            ..one.clone()
        })
        .collect();
    let started = std::time::Instant::now();
    let reports = run_cases(&artifact, &cases, CASES_TRANSITION_BUDGET).unwrap();
    println!("40 routing cases: {:?}", started.elapsed());
    assert!(reports.iter().all(|r| r.passed));
}
