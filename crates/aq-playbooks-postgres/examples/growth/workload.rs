use super::*;

fn set(xs: &[&str]) -> BTreeSet<String> {
    xs.iter().map(|s| s.to_string()).collect()
}
pub(super) fn authority() -> Authority {
    Authority {
        actor: "bench".into(),
        scope: "project".into(),
        capabilities: set(&["execute"]),
    }
}
fn r(root: &str, path: &[&str]) -> Expr {
    Expr::Ref {
        root: root.into(),
        path: path.iter().map(|s| s.to_string()).collect(),
    }
}
fn ticket_object() -> Expr {
    Expr::Object {
        fields: BTreeMap::from([("ticket".into(), r("input", &["ticket"]))]),
    }
}
fn command(name: &str) -> CommandContract {
    CommandContract {
        name: name.into(),
        version: "1".into(),
        input: Schema::object(BTreeMap::from([("ticket".into(), Schema::string(64))])),
        outcomes: BTreeMap::from([(
            "ok".into(),
            Schema::object(BTreeMap::from([("summary".into(), Schema::string(2048))])),
        )]),
        capabilities: set(&["execute"]),
        scopes: set(&["project"]),
        effect: Effect::Mutation,
        recovery: Recovery::KeyedIdempotent,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 4096,
        deadline_ms: 60_000,
    }
}
fn signal_contract() -> EventContract {
    EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: "bench.signal".into(),
        version: "1".into(),
        payload: Schema::object(BTreeMap::from([("detail".into(), Schema::string(1024))])),
        trusted_producers: set(&["host"]),
        scopes: set(&["project"]),
        correlation_fields: set(&["ticket"]),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 2048,
        retention_ms: 60_000,
    }
}
fn cref(c: &CommandContract) -> ContractRef {
    ContractRef {
        name: c.name.clone(),
        fingerprint: c.fingerprint().unwrap(),
    }
}
fn edges(xs: &[(&str, &str)]) -> BTreeMap<String, String> {
    xs.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}
fn invoke(command: &CommandContract, bind: &str, next: &str) -> Step {
    Step {
        op: Operation::Invoke {
            command: cref(command),
            input: ticket_object(),
            bind: bind.into(),
            retry: None,
        },
        edges: edges(&[("ok", next)]),
        on_error: Some("failed".into()),
    }
}

fn finish_run(outcome: &str, output: Expr) -> Step {
    Step {
        op: Operation::Return {
            outcome: outcome.into(),
            output,
        },
        edges: BTreeMap::new(),
        on_error: None,
    }
}

fn steps(
    prepare: &CommandContract,
    finish: &CommandContract,
    signal: &EventContract,
) -> BTreeMap<String, Step> {
    BTreeMap::from([
        ("prepare".into(), invoke(prepare, "prepared", "await")),
        (
            "await".into(),
            Step {
                op: Operation::Wait {
                    event: Some(ContractRef {
                        name: signal.name.clone(),
                        fingerprint: signal.fingerprint().unwrap(),
                    }),
                    correlation: BTreeMap::from([("ticket".into(), r("input", &["ticket"]))]),
                    minimum_revision: None,
                    deadline_ms: 60_000,
                    bind: "signal".into(),
                },
                edges: edges(&[("received", "check"), ("timeout", "failed")]),
                on_error: Some("failed".into()),
            },
        ),
        (
            "check".into(),
            Step {
                op: Operation::Decide {
                    predicate: Expr::Exists {
                        value: Box::new(r("prepared", &[])),
                    },
                },
                edges: edges(&[
                    ("true", "finish"),
                    ("false", "failed"),
                    ("unknown", "failed"),
                ]),
                on_error: Some("failed".into()),
            },
        ),
        ("finish".into(), invoke(finish, "finished", "done")),
        ("done".into(), finish_run("ok", ticket_object())),
        (
            "failed".into(),
            finish_run("error", Expr::literal(Value::Null)),
        ),
    ])
}

/// Revision `n` of the playbook's document, as an authoring save installs
/// it: the same source and steps, another purpose (so another digest).
pub(crate) fn saved_revision(n: usize) -> Artifact {
    let mut content = playbook().content;
    content.playbook.purpose = format!("engine growth measurement, save {n}");
    Artifact::seal(content).unwrap()
}
pub(crate) fn playbook() -> Artifact {
    let prepare = command("bench.prepare");
    let finish = command("bench.finish");
    let signal = signal_contract();
    let steps = steps(&prepare, &finish, &signal);
    Artifact::seal(ArtifactContent {
        format_version: 1,
        compiler: "engine-growth-bench".into(),
        source: SourceBinding {
            digest: hash_bytes(b"engine-growth-bench"),
            header: SourceHeader {
                format_version: 1,
                id: "growth".into(),
                scope: "project".into(),
                interfaces: set(&["main"]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: "growth".into(),
            purpose: "engine growth measurement".into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::object(BTreeMap::from([("ticket".into(), Schema::string(64))])),
                    outcomes: BTreeMap::from([
                        (
                            "ok".into(),
                            Schema::object(BTreeMap::from([("ticket".into(), Schema::string(64))])),
                        ),
                        ("error".into(), Schema::Null),
                    ]),
                    start: "prepare".into(),
                },
            )]),
            steps,
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: set(&["execute"]),
        },
        registry: Registry {
            commands: BTreeMap::from([
                (prepare.name.clone(), prepare),
                (finish.name.clone(), finish),
            ]),
            events: BTreeMap::from([(signal.name.clone(), signal)]),
        },
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })
    .unwrap()
}

struct BenchHost;
impl Host for BenchHost {
    fn authorize(&self, a: &Authority, c: &CommandContract) -> bool {
        c.capabilities.is_subset(&a.capabilities) && c.scopes.contains(&a.scope)
    }
    fn dispatch(&mut self, i: &Intent) -> DispatchResult {
        DispatchResult::Completed {
            outcome: "ok".into(),
            output: json!({"summary": format!("{} handled {}", i.command.name, "s".repeat(400))}),
        }
    }
    fn reconcile(&mut self, i: &Intent) -> DispatchResult {
        self.dispatch(i)
    }
}

/// What serving a run needs of the namespace.
pub(crate) struct Bench<'a> {
    pub(crate) e: &'a Engine<Measured>,
    pub(crate) activation_revision: u64,
    /// Prefix of admission keys, tickets and event ids.
    pub(crate) prefix: String,
    /// Pass the producer's request time (`admit_once_at`, `admit_event_at`).
    pub(crate) timed: bool,
}

#[derive(Clone, Copy)]
pub(crate) struct RunSample {
    pub(crate) number: usize,
    pub(crate) at_ms: i64,
    pub(crate) step_ms: i64,
}

pub(crate) fn serve_run(b: &Bench, sample: RunSample) -> (String, i64) {
    let RunSample {
        number: k,
        at_ms: mut now,
        step_ms: step,
    } = sample;
    let (e, prefix) = (b.e, &b.prefix);
    let ticket = format!("{prefix}ticket-{k:06}");
    // Keyed admission, as aq-server's host uses (`admit_once`).
    let id = e
        .admit_once_at(
            KeyedAdmissionRequest {
                key: format!("{prefix}admission-{k:06}"),
                binding: "growth".into(),
                expected_activation_revision: b.activation_revision,
                entry: "main".into(),
                input: json!({"ticket": ticket}),
                authority: authority(),
            },
            b.timed.then_some(now),
            now,
        )
        .unwrap()
        .receipt
        .run_id;
    let mut guard = 0;
    let mut host = BenchHost;
    let mut drive = |now: &mut i64, limit| loop {
        *now += step;
        guard += 1;
        if e.drive(&mut host, *now, 1).unwrap() == 0 || guard > limit {
            break;
        }
    };
    drive(&mut now, 40);
    now += step;
    let c = signal_contract();
    let d = e
        .admit_event_at(
            "host",
            EventEnvelope {
                producer: "host".into(),
                event_id: format!("{prefix}signal-{k:06}"),
                contract: ContractRef {
                    name: c.name.clone(),
                    fingerprint: c.fingerprint().unwrap(),
                },
                scope: "project".into(),
                correlation: BTreeMap::from([("ticket".into(), ticket.clone())]),
                subject: None,
                revision: None,
                causation: None,
                occurred_at_ms: Some(now),
                observed_at_ms: None,
                payload: json!({"detail": "d".repeat(256)}),
            },
            0,
            b.timed.then_some(now),
            now,
        )
        .unwrap();
    assert!(!d.duplicate);
    drive(&mut now, 80);
    (id, now)
}
