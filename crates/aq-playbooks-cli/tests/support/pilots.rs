//! Reusable, compiled pilot workflows with deterministic fake-host scenarios,
//! for tests only. Their artifacts' bytes are pinned by
//! `tests/host_pilots.rs` (`the_coding_pilot_keeps_its_pinned_bytes`).
#[allow(dead_code)]
#[path = "schemas.rs"]
pub mod schemas;
use aq_playbooks_cli::{
    catalog::*,
    simulation::{Action, Scenario, Start},
};
use aq_playbooks_compiler::{COMPILER_VERSION, CompileInput, Proposal, compile};
use aq_playbooks_model::*;
use aq_playbooks_runner::{Authority, DispatchResult};
use schemas::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
pub const NAMES: &[&str] = &["coding", "watcher", "report"];
pub fn lit(v: Value) -> Expr {
    Expr::literal(v)
}
pub fn reference(root: &str, path: &[&str]) -> Expr {
    Expr::Ref {
        root: root.into(),
        path: path.iter().map(|s| (*s).into()).collect(),
    }
}
pub fn fields(items: &[(&str, Expr)]) -> Expr {
    Expr::Object {
        fields: items.iter().cloned().map(|(k, v)| (k.into(), v)).collect(),
    }
}
pub fn edge(op: Operation, edges: &[(&str, &str)]) -> Step {
    Step {
        op,
        edges: edges
            .iter()
            .map(|(a, b)| ((*a).into(), (*b).into()))
            .collect(),
        on_error: Some("failure".into()),
    }
}
pub fn ret(outcome: &str, output: Expr) -> Step {
    Step {
        op: Operation::Return {
            outcome: outcome.into(),
            output,
        },
        edges: BTreeMap::new(),
        on_error: None,
    }
}
pub fn invoke(c: &CommandContract, input: Expr, bind: &str, success: &[(&str, &str)]) -> Step {
    let mut s = edge(
        Operation::Invoke {
            command: ContractRef {
                name: c.name.clone(),
                fingerprint: c.fingerprint().unwrap(),
            },
            input,
            bind: bind.into(),
            retry: None,
        },
        &[],
    );
    for outcome in c.outcomes.keys() {
        s.edges.insert(outcome.clone(), "failure".into());
    }
    for (a, b) in success {
        s.edges.insert((*a).into(), (*b).into());
    }
    s
}
fn child_call(name: &str, input: Expr, mode: CallMode, bind: &str) -> Operation {
    Operation::Call {
        dependency: name.into(),
        entry: "main".into(),
        input,
        mode,
        bind: bind.into(),
    }
}
#[path = "pilot_compiler.rs"]
mod pilot_compiler;
pub use pilot_compiler::{PilotSpec, build};
pub fn subject_value() -> Value {
    json!({"id":"candidate-1","revision":1})
}
fn output() -> Value {
    json!({"reference":"evidence-1","subject":subject_value()})
}
fn done(outcome: &str) -> DispatchResult {
    let mut value = output();
    if outcome == "completed" {
        value["disposition"] = json!("accepted");
    } else if outcome == "assessed" {
        value["choice"] = json!("permitted-default");
    }
    DispatchResult::Completed {
        outcome: outcome.into(),
        output: value,
    }
}

fn work(kind: &str) -> Expr {
    lit(
        json!({"kind":kind,"subject":subject_value(),"logical_key":format!("pilot-{kind}"),"lifetime":"owned","context":"snapshot-1","visibility":"project"}),
    )
}
fn context(view: &str) -> Expr {
    lit(json!({"view":view,"subject":subject_value(),"snapshot":"snapshot-1","page":0}))
}
fn observed() -> DispatchResult {
    DispatchResult::Completed {
        outcome: "observed".into(),
        output: json!({"reference":"observation-1","subject":subject_value(),"snapshot":"snapshot-1","complete":true,"next_page":null,"visibility":"project","observed_at_ms":0,"expires_at_ms":60000}),
    }
}
fn base_observation(revision: u64) -> DispatchResult {
    let mut value = match observed() {
        DispatchResult::Completed { output, .. } => output,
        _ => unreachable!(),
    };
    value["subject"] = json!({"id":"main","revision":revision});
    value["snapshot"] = json!(format!("base-{revision}"));
    DispatchResult::Completed {
        outcome: "observed".into(),
        output: value,
    }
}
fn base(
    artifacts: Vec<Artifact>,
    name: &str,
    mocks: BTreeMap<String, Vec<DispatchResult>>,
    actions: Vec<Action>,
) -> Scenario {
    let a = artifacts.last().unwrap();
    let authority = Authority {
        actor: "pilot-user".into(),
        scope: "project".into(),
        capabilities: a.content.playbook.required_capabilities.clone(),
    };
    Scenario {
        bindings: BTreeMap::from([(name.into(), a.digest.clone())]),
        authority: authority.clone(),
        artifacts,
        starts: vec![Start {
            binding: name.into(),
            entry: "main".into(),
            input: Value::Null,
            authority,
        }],
        mocks,
        actions,
        max_transitions: 1000,
    }
}
#[derive(Clone, Copy)]
enum WorkResult {
    Immediate,
    Completion,
}
impl WorkResult {
    fn field(self, name: &str) -> Expr {
        match self {
            Self::Immediate => reference("work", &[name]),
            Self::Completion => reference("completion", &["output", name]),
        }
    }
    fn projection(self) -> Expr {
        fields(&[
            ("reference", self.field("reference")),
            ("subject", self.field("subject")),
        ])
    }
    fn accepted(self) -> Expr {
        Expr::Eq {
            left: Box::new(self.field("disposition")),
            right: Box::new(lit(json!("accepted"))),
        }
    }
}
#[derive(Clone, Copy)]
enum WorkPilot {
    Implementation,
    Review,
    Repair,
    ReportAuthor,
}
impl WorkPilot {
    fn kind(self) -> &'static str {
        match self {
            Self::Implementation => "implementation",
            Self::Review => "review",
            Self::Repair => "repair",
            Self::ReportAuthor => "supervisor_authoring",
        }
    }
    fn id(self) -> &'static str {
        match self {
            Self::ReportAuthor => "report-author",
            _ => self.kind(),
        }
    }
}
fn work_child(pilot: WorkPilot) -> anyhow::Result<Artifact> {
    let r = catalog();
    let (input_schema, request) = work_request(pilot);
    build(
        PilotSpec {
            id: pilot.id(),
            input: input_schema,
            output: result(),
            start: "request",
        },
        r.clone(),
        BTreeMap::from([
            (
                "request".into(),
                invoke(
                    &r.commands["work.ensure"],
                    request,
                    "work",
                    &[("completed", "assess"), ("accepted", "await_result")],
                ),
            ),
            (
                "assess".into(),
                assessment(WorkResult::Immediate.accepted(), "done"),
            ),
            (
                "done".into(),
                ret("success", WorkResult::Immediate.projection()),
            ),
            ("await_result".into(), await_completion(&r)?),
            (
                "assess_event".into(),
                assessment(
                    Expr::And {
                        values: vec![
                            Expr::Eq {
                                left: Box::new(reference("completion", &["outcome"])),
                                right: Box::new(lit(json!("completed"))),
                            },
                            WorkResult::Completion.accepted(),
                        ],
                    },
                    "done_event",
                ),
            ),
            (
                "done_event".into(),
                ret("success", WorkResult::Completion.projection()),
            ),
        ]),
        &[],
    )
}

fn await_completion(r: &Registry) -> anyhow::Result<Step> {
    let event = ContractRef {
        name: "aq.operation.updated".into(),
        fingerprint: r.events["aq.operation.updated"].fingerprint()?,
    };
    Ok(edge(
        Operation::Wait {
            event: Some(event),
            correlation: BTreeMap::from([("operation".into(), reference("work", &["handle"]))]),
            minimum_revision: Some(lit(json!(1))),
            deadline_ms: 10000,
            bind: "completion".into(),
        },
        &[("received", "assess_event"), ("timeout", "failure")],
    ))
}

fn work_request(pilot: WorkPilot) -> (Schema, Expr) {
    let input_schema = if matches!(pilot, WorkPilot::Repair) {
        Schema::string(128)
    } else {
        Schema::Null
    };
    let request = if matches!(pilot, WorkPilot::Repair) {
        fields(&[
            ("kind", lit(json!(pilot.kind()))),
            ("subject", lit(subject_value())),
            ("logical_key", reference("input", &[])),
            ("lifetime", lit(json!("owned"))),
            ("context", lit(json!("snapshot-1"))),
            ("visibility", lit(json!("project"))),
        ])
    } else {
        work(pilot.kind())
    };
    (input_schema, request)
}
fn assessment(predicate: Expr, pass: &str) -> Step {
    edge(
        Operation::Decide { predicate },
        &[("true", pass), ("false", "failure"), ("unknown", "failure")],
    )
}
pub fn scenario(name: &str) -> anyhow::Result<Scenario> {
    match name {
        "coding" => coding(),
        "watcher" => watcher(),
        "report" => report(),
        _ => anyhow::bail!("unknown pilot {name}; choose {}", NAMES.join(", ")),
    }
}
#[path = "coding_pilot.rs"]
mod coding_pilot;
use coding_pilot::coding;
fn watcher() -> anyhow::Result<Scenario> {
    let r = catalog();
    let steps = BTreeMap::from([
        (
            "observe".into(),
            invoke(
                &r.commands["context.read"],
                context("health_observations"),
                "observation",
                &[("observed", "reconcile"), ("incomplete", "reconcile")],
            ),
        ),
        (
            "reconcile".into(),
            invoke(
                &r.commands["findings.reconcile"],
                fields(&[
                    ("partition", lit(json!("project-health"))),
                    ("observation", reference("observation", &["snapshot"])),
                    ("evaluation", lit(json!("evaluation-1"))),
                    (
                        "findings",
                        lit(
                            json!([{"key":"stuck-work","evidence":"work-1","expected_revision":0}]),
                        ),
                    ),
                    ("reminder_ms", lit(json!(3600000))),
                ]),
                "findings",
                &[("reconciled", "done")],
            ),
        ),
        ("done".into(), ret("success", lit(Value::Null))),
    ]);
    let a = build(
        PilotSpec {
            id: "watcher",
            input: Schema::Null,
            output: Schema::Null,
            start: "observe",
        },
        r,
        steps,
        &[],
    )?;
    Ok(base(
        vec![a],
        "watcher",
        BTreeMap::from([
            ("context.read".into(), vec![observed()]),
            ("findings.reconcile".into(), vec![done("reconciled")]),
        ]),
        vec![],
    ))
}
fn report() -> anyhow::Result<Scenario> {
    let r = catalog();
    let author = work_child(WorkPilot::ReportAuthor)?;
    let steps = report_steps(&r);
    let root = build(
        PilotSpec {
            id: "report",
            input: Schema::Null,
            output: Schema::Null,
            start: "context",
        },
        r.clone(),
        steps,
        &[("author", &author)],
    )?;
    let event = author_completion(&r)?;
    Ok(base(
        vec![author, root],
        "report",
        BTreeMap::from([
            ("context.read".into(), vec![observed()]),
            (
                "work.ensure".into(),
                vec![DispatchResult::Accepted {
                    handle: "author-operation".into(),
                }],
            ),
            ("message.send".into(), vec![done("queued")]),
        ]),
        vec![
            Action::Tick { at_ms: 1001 },
            Action::Event {
                at_ms: 2000,
                producer: "aq.host.bridge@1".into(),
                event: Box::new(event),
            },
        ],
    ))
}

fn report_steps(r: &Registry) -> BTreeMap<String, Step> {
    let mut steps = BTreeMap::from([
        (
            "context".into(),
            invoke(
                &r.commands["context.read"],
                context("project"),
                "context",
                &[("observed", "author")],
            ),
        ),
        (
            "author".into(),
            edge(
                child_call(
                    "author",
                    lit(Value::Null),
                    CallMode::OwnedAsync,
                    "author_handle",
                ),
                &[("started", "wait")],
            ),
        ),
        (
            "wait".into(),
            edge(
                Operation::Join {
                    member_keys: vec![],
                    members: vec![reference("author_handle", &[])],
                    mode: JoinMode::All,
                    accepted_outcomes: strings(&["success"]),
                    deadline_ms: 1000,
                    remaining: RemainingPolicy::Cancel,
                    bind: "author_result".into(),
                },
                &[
                    ("completed", "authored"),
                    ("timeout", "fallback"),
                    ("no_match", "fallback"),
                ],
            ),
        ),
        ("done".into(), ret("success", lit(Value::Null))),
    ]);
    for (name, body) in [
        (
            "authored",
            "Supervisor report is available as retained author work",
        ),
        (
            "fallback",
            "Supervisor report deadline elapsed; frozen context snapshot-1 remains available",
        ),
    ] {
        steps.insert(name.into(),invoke(&r.commands["message.send"],lit(json!({"identity":"report-window-1","recipient":"supervisor","visibility":"project","body":body})),&format!("{name}_message"),&[("queued","done")]));
    }
    steps
}
fn author_completion(r: &Registry) -> anyhow::Result<EventEnvelope> {
    Ok(EventEnvelope {
        producer: "aq.host.bridge@1".into(),
        event_id: "author-complete".into(),
        contract: ContractRef {
            name: "aq.operation.updated".into(),
            fingerprint: r.events["aq.operation.updated"].fingerprint()?,
        },
        scope: "project".into(),
        correlation: BTreeMap::from([("operation".into(), "author-operation".into())]),
        subject: Some("candidate-1".into()),
        revision: Some(1),
        causation: None,
        occurred_at_ms: Some(2000),
        observed_at_ms: Some(2000),
        payload: json!({"outcome":"completed","output":{"reference":"evidence-1","subject":subject_value(),"disposition":"accepted"}}),
    })
}
