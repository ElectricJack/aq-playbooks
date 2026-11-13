//! Reading an asynchronous answer: a named join over an operation handle types
//! the answer per outcome on its `completed` edge, and the handle is joined by
//! its binding (`{op: ref, root: <bind>}`, no path), exactly as the runner
//! resolves it.
use aq_playbooks_model::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const REGISTRY_2_1: &str = include_str!("../../../contracts/aq-host-2.1.0/registry.json");

fn lit(v: Value) -> Expr {
    Expr::literal(v)
}
fn r(root: &str, path: &[&str]) -> Expr {
    Expr::Ref {
        root: root.into(),
        path: path.iter().map(|s| s.to_string()).collect(),
    }
}
fn edges(xs: &[(&str, &str)]) -> BTreeMap<String, String> {
    xs.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
fn ret(outcome: &str, output: Expr) -> Step {
    Step {
        op: Operation::Return {
            outcome: outcome.into(),
            output,
        },
        edges: BTreeMap::new(),
        on_error: None,
    }
}
/// The 2.1.0 judgment command with what it needs to be valid: its completion
/// event and its reconciliation command.
fn registry() -> Registry {
    let all: Registry = serde_json::from_str(REGISTRY_2_1).unwrap();
    let keep = ["judgment.request", "operation.get"];
    Registry {
        commands: all
            .commands
            .into_iter()
            .filter(|(name, _)| keep.contains(&name.as_str()))
            .collect(),
        events: all
            .events
            .into_iter()
            .filter(|(name, _)| name == "aq.judgment.completed")
            .collect(),
    }
}
fn judgment_input() -> Value {
    json!({
        "budget": {"deadline_ms": 60000, "max_cost_microunits": 50000, "max_input_tokens": 8192, "max_output_tokens": 512},
        "context": {"artifacts": [], "text": "Work item summary."},
        "output": {"choices": ["approve", "decline"], "rationale_max_bytes": 2048},
        "question": "Should this change be approved?",
        "subject": {"id": "work-1", "revision": 1},
    })
}
/// `ask` invokes `judgment.request` and binds `j`; `read` joins `member` under
/// the key `c` and binds `answer`. `completed`, `no_match` and `timeout` lead
/// to `chosen`, `unanswered` and `late`; the caller supplies those steps.
fn content(
    member: Expr,
    chosen: Step,
    unanswered: Step,
    late: Step,
    chosen_type: Schema,
) -> ArtifactContent {
    let registry = registry();
    let judgment = &registry.commands["judgment.request"];
    let header = SourceHeader {
        format_version: FORMAT_VERSION,
        id: "ask-then-read".into(),
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
            id: "ask-then-read".into(),
            purpose: "ask, then read the answer".into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Null,
                    outcomes: BTreeMap::from([
                        ("chosen".into(), chosen_type),
                        ("unanswered".into(), Schema::Null),
                        ("late".into(), Schema::Null),
                        ("error".into(), Schema::Null),
                    ]),
                    start: "ask".into(),
                },
            )]),
            steps: BTreeMap::from([
                (
                    "ask".into(),
                    Step {
                        op: Operation::Invoke {
                            command: ContractRef {
                                name: judgment.name.clone(),
                                fingerprint: judgment.fingerprint().unwrap(),
                            },
                            input: lit(judgment_input()),
                            bind: "j".into(),
                            retry: None,
                        },
                        edges: edges(&[
                            ("accepted", "read"),
                            ("assessed", "error"),
                            ("failed", "error"),
                            ("refused", "error"),
                        ]),
                        on_error: Some("error".into()),
                    },
                ),
                (
                    "read".into(),
                    Step {
                        op: Operation::Join {
                            members: vec![member],
                            member_keys: vec!["c".into()],
                            mode: JoinMode::All,
                            accepted_outcomes: BTreeSet::from(["assessed".into()]),
                            deadline_ms: 60_000,
                            remaining: RemainingPolicy::Track,
                            bind: "answer".into(),
                        },
                        edges: edges(&[
                            ("completed", "chosen"),
                            ("no_match", "unanswered"),
                            ("timeout", "late"),
                        ]),
                        on_error: Some("error".into()),
                    },
                ),
                ("chosen".into(), chosen),
                ("unanswered".into(), unanswered),
                ("late".into(), late),
                ("error".into(), ret("error", lit(Value::Null))),
            ]),
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: BTreeSet::from(["judgment.request.v2".into()]),
        },
        registry,
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    }
}
fn choice() -> Expr {
    r("answer", &["c", "output", "choice"])
}
fn returns_choice() -> Step {
    ret("chosen", choice())
}
fn returns_null(outcome: &str) -> Step {
    ret(outcome, lit(Value::Null))
}
fn has_issue(issues: &[ValidationIssue], code: &str, path: &str, text: &str) -> bool {
    issues
        .iter()
        .any(|i| i.code == code && i.path == path && i.message.contains(text))
}

#[test]
fn join_over_an_invoke_handle_narrows_completed_to_the_accepted_outcome() {
    let valid = content(
        r("j", &[]),
        returns_choice(),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::string(512),
    );
    assert_eq!(valid.validate_structure(), vec![]);
    // The answer is exactly `assessed`'s `choice`: a string of at most 512 bytes.
    let narrower = content(
        r("j", &[]),
        returns_choice(),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::string(511),
    )
    .validate_structure();
    assert!(
        has_issue(&narrower, "schema_compatibility", "steps.chosen", "512"),
        "{narrower:?}"
    );
    // The same handle written through `vars` is the same member.
    let through_vars = content(
        r("vars", &["j"]),
        returns_choice(),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::string(512),
    );
    assert_eq!(through_vars.validate_structure(), vec![]);
}

#[test]
fn the_handle_path_member_form_is_refused_by_validation() {
    let issues = content(
        r("j", &["handle"]),
        returns_null("chosen"),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::Null,
    )
    .validate_structure();
    assert!(
        issues.iter().any(|i| i.code == "join_member_type"),
        "{issues:?}"
    );
    // The diagnostic alone is enough to fix the draft: it names the step, the
    // member as written, and the form that is accepted.
    let message = &issues
        .iter()
        .find(|i| i.code == "join_member_type")
        .unwrap();
    assert_eq!(message.path, "steps.read");
    for text in [
        "members[0]",
        "`j.handle`",
        r#"{"op":"ref","root":"j","path":[]}"#,
    ] {
        assert!(
            message.message.contains(text),
            "{text}: {}",
            message.message
        );
    }
    // The same form through `vars` is refused the same way.
    let issues = content(
        r("vars", &["j", "handle"]),
        returns_null("chosen"),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::Null,
    )
    .validate_structure();
    assert!(
        has_issue(&issues, "join_member_type", "steps.read", "`vars.j.handle`"),
        "{issues:?}"
    );
}

#[test]
fn the_handle_path_is_refused_in_a_join_without_member_keys() {
    let mut c = content(
        r("j", &["handle"]),
        returns_null("chosen"),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::Null,
    );
    let Operation::Join { member_keys, .. } = &mut c.playbook.steps.get_mut("read").unwrap().op
    else {
        unreachable!()
    };
    member_keys.clear();
    let issues = c.validate_structure();
    assert!(
        has_issue(&issues, "join_member_type", "steps.read", "`j.handle`"),
        "{issues:?}"
    );
}

#[test]
fn no_match_and_timeout_edges_do_not_see_a_narrowed_answer() {
    let issues = content(
        r("j", &[]),
        returns_choice(),
        returns_choice(),
        returns_choice(),
        Schema::string(512),
    )
    .validate_structure();
    for step in ["steps.unanswered", "steps.late"] {
        assert!(
            has_issue(&issues, "expression_type", step, ""),
            "{step}: {issues:?}"
        );
    }
    assert!(
        !issues.iter().any(|i| i.path == "steps.chosen"),
        "{issues:?}"
    );
}

#[test]
fn a_read_the_answer_cannot_have_names_the_reference_and_what_is_there() {
    let issues = content(
        r("j", &[]),
        returns_choice(),
        returns_choice(),
        returns_null("late"),
        Schema::string(512),
    )
    .validate_structure();
    let unanswered = issues
        .iter()
        .find(|i| i.path == "steps.unanswered")
        .unwrap_or_else(|| panic!("{issues:?}"));
    // Step, location, the reference as written, and why it cannot be read here.
    for text in [
        "steps.unanswered.output",
        "`answer.c.output.choice`",
        "`answer.c.output` may be null",
        "`completed` edge",
    ] {
        assert!(
            unanswered.message.contains(text),
            "{text}: {}",
            unanswered.message
        );
    }
    // A field no outcome has: the fields that do exist are listed.
    let issues = content(
        r("j", &[]),
        ret("chosen", r("answer", &["c", "output", "verdict"])),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::string(512),
    )
    .validate_structure();
    assert!(
        has_issue(
            &issues,
            "expression_type",
            "steps.chosen",
            "unknown field verdict: `answer.c.output` has fields choice, provenance, rationale, reference, subject"
        ),
        "{issues:?}"
    );
}

/// The "ask, then read the answer" example in FORMAT.md, with its `return`
/// steps added and the real fingerprint filled in, is a valid playbook.
#[test]
fn the_format_example_validates() {
    let format = include_str!("../FORMAT.md");
    let start = format.find("    \"ask\":").expect("example in FORMAT.md");
    let end = start + format[start..].find("\n\n").unwrap();
    let fingerprint = registry().commands["judgment.request"]
        .fingerprint()
        .unwrap();
    let block = format[start..end].replace("sha256:…", &fingerprint);
    let mut steps: BTreeMap<String, Step> =
        serde_json::from_str(&format!("{{{block}}}")).expect("example steps parse");
    for outcome in ["approved", "declined", "unanswered", "late", "error"] {
        steps.insert(outcome.into(), returns_null(outcome));
    }
    let mut c = content(
        r("j", &[]),
        returns_null("chosen"),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::Null,
    );
    c.playbook.steps = steps;
    c.playbook.entries.get_mut("main").unwrap().outcomes =
        ["approved", "declined", "unanswered", "late", "error"]
            .into_iter()
            .map(|o| (o.to_string(), Schema::Null))
            .collect();
    assert_eq!(c.validate_structure(), vec![]);
    let Operation::Join { members, .. } = &c.playbook.steps["read"].op else {
        panic!("read is a join")
    };
    assert_eq!(members, &vec![r("j", &[])]);
}

#[test]
fn a_field_only_some_alternatives_have_names_the_common_fields() {
    let mut c = content(
        r("j", &[]),
        ret("chosen", r("input", &["choice"])),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::string(512),
    );
    let object = |fields: &[&str]| {
        Schema::object(
            fields
                .iter()
                .map(|f| (f.to_string(), Schema::string(512)))
                .collect(),
        )
    };
    c.playbook.entries.get_mut("main").unwrap().input = Schema::Union {
        variants: vec![
            object(&["choice", "reference"]),
            object(&["reason", "reference"]),
        ],
    };
    let issues = c.validate_structure();
    assert!(
        has_issue(
            &issues,
            "expression_type",
            "steps.chosen",
            "`input.choice` cannot be read: `input` has a different shape per outcome and only some of them have `choice`; every outcome has fields reference."
        ),
        "{issues:?}"
    );
}
