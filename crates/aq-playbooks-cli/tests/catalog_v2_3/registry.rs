use super::*;

fn string(max_length: usize) -> Schema {
    Schema::string(max_length)
}
fn code(values: &[&str]) -> Schema {
    Schema::String {
        max_length: 64,
        values: strings(values),
    }
}
fn array(items: Schema, max_items: usize) -> Schema {
    Schema::Array {
        items: Box::new(items),
        max_items,
    }
}
pub(super) fn range(minimum: i64, maximum: i64) -> Schema {
    Schema::Integer { minimum, maximum }
}
fn without_required(schema: Schema, optional: &[&str]) -> Schema {
    let Schema::Object {
        properties,
        mut required,
    } = schema
    else {
        unreachable!("an object")
    };
    for name in optional {
        required.remove(*name);
    }
    Schema::Object {
        properties,
        required,
    }
}
/// An outcome that says only that it happened.
fn bare() -> Schema {
    object(&[])
}
fn refused(reasons: &[&str]) -> Schema {
    object(&[("reason", enumeration(reasons))])
}
fn pull_request_fact() -> Schema {
    Schema::Nullable {
        inner: Box::new(object(&[
            ("number", range(1, 1_000_000_000)),
            ("url", string(2048)),
            ("state", enumeration(&["closed", "merged", "open"])),
            (
                "mergeable",
                enumeration(&["conflicting", "mergeable", "unknown"]),
            ),
            (
                "checks",
                object(&[
                    (
                        "state",
                        enumeration(&["failed", "none", "passed", "pending"]),
                    ),
                    ("failed", array(string(256), 64)),
                ]),
            ),
        ])),
    }
}
/// The facts 2.3.0 adds to `work.read`: those of the task's latest result.
pub(super) fn result_facts() -> Vec<(&'static str, Schema)> {
    vec![
        ("outcome", string(64)),
        ("changed", Schema::Boolean),
        ("branch", string(256)),
        ("head", string(128)),
        ("base", string(128)),
        ("main_head", string(128)),
        ("returns", range(0, 1000)),
        ("pull_request", pull_request_fact()),
    ]
}
fn work_read(v22: &Registry) -> CommandContract {
    let mut read = v22.commands["work.read"].clone();
    read.version = "2.3.0".into();
    let Some(Schema::Object { properties, .. }) = read.outcomes.get_mut("observed") else {
        unreachable!("observed is an object")
    };
    let Some(Schema::Object {
        properties: facts,
        required,
    }) = properties.get_mut("facts")
    else {
        unreachable!("facts is an object")
    };
    for (name, schema) in result_facts() {
        facts.insert(name.into(), schema);
        required.insert(name.into());
    }
    read
}

struct Shape {
    deadline_ms: u64,
    asynchronous: Option<AsyncContract>,
}
fn command(
    name: &str,
    input: Schema,
    outcomes: &[(&str, Schema)],
    shape: Shape,
) -> CommandContract {
    CommandContract {
        name: name.into(),
        version: "1.0.0".into(),
        input,
        outcomes: outcomes
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect(),
        capabilities: strings(&[name]),
        scopes: strings(&["project"]),
        effect: Effect::Mutation,
        recovery: Recovery::KeyedIdempotent,
        asynchronous: shape.asynchronous,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 65_536,
        deadline_ms: shape.deadline_ms,
    }
}
fn event(
    name: &str,
    payload: Schema,
    correlation: &[&str],
    subject: Option<&str>,
) -> EventContract {
    EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: name.into(),
        version: "1.0.0".into(),
        payload,
        trusted_producers: strings(&["aq.host.bridge@1"]),
        scopes: strings(&["project"]),
        correlation_fields: strings(correlation),
        subject_field: subject.map(str::to_string),
        revision_field: subject.map(|_| "version".to_string()),
        max_payload_bytes: 65_536,
        retention_ms: 86_400_000,
    }
}

const SYNC: Shape = Shape {
    asynchronous: None,
    deadline_ms: 10_000,
};

fn task() -> (&'static str, Schema) {
    ("task", text())
}
fn stale_reasons() -> [&'static str; 3] {
    ["stale_result", "unknown_result", "unknown_task"]
}
fn work_complete() -> CommandContract {
    command(
        "work.complete",
        object(&[task(), ("result_id", text())]),
        &[
            ("completed", bare()),
            ("refused", refused(&stale_reasons())),
        ],
        SYNC,
    )
}
fn work_return() -> CommandContract {
    command(
        "work.return",
        object(&[
            task(),
            ("result_id", text()),
            ("reason", code(&DELIVERY_REASONS)),
            ("detail", string(1024)),
            ("update_from_main", Schema::Boolean),
        ]),
        &[("returned", bare()), ("refused", refused(&stale_reasons()))],
        SYNC,
    )
    .with_optional(&["detail"])
}
fn work_hold() -> CommandContract {
    command(
        "work.hold",
        object(&[
            task(),
            ("result_id", text()),
            ("reason", code(&DELIVERY_REASONS)),
        ]),
        &[("held", bare())],
        SYNC,
    )
}
fn supervisor_notify() -> CommandContract {
    command(
        "supervisor.notify",
        object(&[
            ("project", text()),
            ("identity", text()),
            ("text", string(2048)),
        ]),
        &[("sent", bare()), ("duplicate", bare())],
        SYNC,
    )
}
trait WithOptional {
    fn with_optional(self, optional: &[&str]) -> Self;
}
impl WithOptional for CommandContract {
    fn with_optional(mut self, optional: &[&str]) -> Self {
        self.input = without_required(self.input, optional);
        self
    }
}

/// An asynchronous delivery command: `accepted {handle}` and its business
/// outcomes. The answer arrives on `aq.delivery.completed`.
fn async_command(
    v22: &Registry,
    name: &str,
    input: Schema,
    outcomes: &[(&str, Schema)],
) -> CommandContract {
    let accepted = v22.commands["judgment.assess"].outcomes["accepted"].clone();
    let mut all = vec![("accepted", accepted)];
    all.extend(outcomes.iter().cloned());
    let completion = object(&[
        (
            "outcome",
            enumeration(&outcomes.iter().map(|(n, _)| *n).collect::<Vec<_>>()),
        ),
        (
            "output",
            Schema::Union {
                variants: outcomes.iter().map(|(_, s)| s.clone()).collect(),
            },
        ),
    ]);
    command(
        name,
        input,
        &all,
        Shape {
            deadline_ms: 60_000,
            asynchronous: Some(AsyncContract {
                completion_event: "aq.delivery.completed".into(),
                completion,
                correlation_field: "operation".into(),
                reconciliation_command: "operation.get".into(),
            }),
        },
    )
}
fn delivery_commands(v22: &Registry) -> Vec<CommandContract> {
    let mut commands = vec![
        work_complete(),
        work_return(),
        work_hold(),
        supervisor_notify(),
    ];
    commands.push(async_command(
        v22,
        "main.sync",
        object(&[("project", text())]),
        &[
            ("synced", object(&[("head", string(128))])),
            ("refused", refused(&["unknown_project", "unreachable"])),
        ],
    ));
    commands.push(async_command(
        v22,
        "branch.publish",
        object(&[task()]),
        &[
            ("published", object(&[("head", string(128))])),
            ("conflict", bare()),
            (
                "refused",
                refused(&["no_branch", "unknown_task", "unreachable"]),
            ),
        ],
    ));
    commands.extend(pull_request_commands(v22));
    commands
}
/// The one completion event of every asynchronous delivery command: its
/// payload accepts the completion of each of them.
fn delivery_completed(commands: &[CommandContract]) -> EventContract {
    let mut outcomes = BTreeSet::new();
    let mut variants: Vec<Schema> = vec![];
    let completion_outcomes = commands
        .iter()
        .filter(|c| c.asynchronous.is_some())
        .flat_map(|c| &c.outcomes)
        .filter(|(name, _)| *name != "accepted");
    for (name, schema) in completion_outcomes {
        outcomes.insert(name.clone());
        if !variants.contains(schema) {
            variants.push(schema.clone());
        }
    }
    event(
        "aq.delivery.completed",
        object(&[
            (
                "outcome",
                enumeration(&outcomes.iter().map(String::as_str).collect::<Vec<_>>()),
            ),
            ("output", Schema::Union { variants }),
        ]),
        &["operation"],
        None,
    )
}
fn result_ready() -> EventContract {
    event(
        "aq.work.result_ready",
        object(&[
            ("task_id", text()),
            ("project_id", text()),
            ("attempt_id", text()),
            ("result_id", text()),
            ("version", range(0, i64::MAX)),
        ]),
        &[],
        Some("task_id"),
    )
}
/// 2.2.0, unchanged, except `work.read`, plus the delivery contracts.
pub(super) fn build_v2_3() -> Registry {
    let v22 = catalog::catalog_v2_2();
    let mut registry = v22.clone();
    let read = work_read(&v22);
    registry.commands.insert(read.name.clone(), read);
    let commands = delivery_commands(&v22);
    let completed = delivery_completed(&commands);
    for c in commands {
        registry.commands.insert(c.name.clone(), c);
    }
    for e in [completed, result_ready()] {
        registry.events.insert(e.name.clone(), e);
    }
    registry
}
fn pull_request_commands(v22: &Registry) -> Vec<CommandContract> {
    let mut commands = Vec::new();
    commands.push(async_command(
        v22,
        "pull_request.open",
        object(&[task()]),
        &[
            (
                "opened",
                object(&[("number", range(1, 1_000_000_000)), ("url", string(2048))]),
            ),
            (
                "refused",
                refused(&["not_published", "unknown_task", "unreachable"]),
            ),
        ],
    ));
    commands.push(
        async_command(
            v22,
            "pull_request.await_checks",
            object(&[
                task(),
                ("required", array(string(256), 64)),
                ("deadline_ms", range(1000, 604_800_000)),
            ]),
            &[
                ("passed", bare()),
                ("failed", object(&[("names", array(string(256), 64))])),
                ("conflict", bare()),
                ("closed", bare()),
                ("timeout", bare()),
            ],
        )
        .with_optional(&["required"]),
    );
    commands.push(async_command(
        v22,
        "pull_request.merge",
        object(&[
            task(),
            ("method", code(&MERGE_METHODS)),
            ("expected_head_sha", string(128)),
        ]),
        &[
            ("merged", bare()),
            ("conflict", bare()),
            ("not_mergeable", bare()),
            (
                "refused",
                refused(&[
                    "head_changed",
                    "no_pull_request",
                    "unknown_task",
                    "unreachable",
                ]),
            ),
        ],
    ));
    commands.push(async_command(
        v22,
        "pull_request.close",
        object(&[task()]),
        &[("closed", bare())],
    ));
    commands
}
