use super::*;

pub(super) fn string(max_length: usize) -> Schema {
    Schema::string(max_length)
}
pub(super) fn code(values: &[&str]) -> Schema {
    Schema::String {
        max_length: 64,
        values: strings(values),
    }
}
pub(super) fn array(items: Schema, max_items: usize) -> Schema {
    Schema::Array {
        items: Box::new(items),
        max_items,
    }
}
fn range(minimum: i64, maximum: i64) -> Schema {
    Schema::Integer { minimum, maximum }
}
/// `reference` and `subject`, then `extra`.
fn outcome(extra: &[(&str, Schema)]) -> Schema {
    let Schema::Object {
        mut properties,
        mut required,
    } = result()
    else {
        unreachable!("result() is an object")
    };
    for (name, schema) in extra {
        properties.insert((*name).into(), schema.clone());
        required.insert((*name).into());
    }
    Schema::Object {
        properties,
        required,
    }
}
pub(super) fn level(extra: &[&str]) -> Schema {
    Schema::String {
        max_length: 64,
        values: LEVELS.iter().chain(extra).map(|s| (*s).into()).collect(),
    }
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
struct Shape {
    effect: Effect,
    recovery: Recovery,
    deadline_ms: u64,
    public: &'static [&'static str],
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
        effect: shape.effect,
        recovery: shape.recovery,
        asynchronous: shape.asynchronous,
        cancellation: false,
        public_result_fields: strings(shape.public),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 65_536,
        deadline_ms: shape.deadline_ms,
    }
}
fn event(name: &str, payload: Schema, correlation: &[&str], subject_path: &str) -> EventContract {
    EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: name.into(),
        version: "1.0.0".into(),
        payload,
        trusted_producers: strings(&["aq.host.bridge@1"]),
        scopes: strings(&["project"]),
        correlation_fields: strings(correlation),
        subject_field: Some(format!("{subject_path}.id")),
        revision_field: Some(format!("{subject_path}.revision")),
        max_payload_bytes: 65_536,
        retention_ms: 86_400_000,
    }
}

fn work_read() -> CommandContract {
    let facts = object(&[
        ("kind", string(64)),
        ("title", string(512)),
        ("description", string(8192)),
        (
            "origin",
            enumeration(&["owner", "agent", "supervisor", "playbook", "unknown"]),
        ),
        // `""` when none was asked for.
        ("level_hint", level(&[""])),
        ("charged_failures", range(0, 1000)),
        ("ended_attempts", range(0, 1000)),
        ("route", enumeration(&["none", "router", "owner"])),
    ]);
    command(
        "work.read",
        object(&[("subject", subject())]),
        &[
            ("observed", outcome(&[("facts", facts)])),
            (
                "refused",
                outcome(&[("reason", enumeration(&["unknown_task", "not_on_router"]))]),
            ),
        ],
        Shape {
            effect: Effect::Observation,
            recovery: Recovery::Observation,
            deadline_ms: 10_000,
            public: &["reference", "subject"],
            asynchronous: None,
        },
    )
}
fn classification() -> Schema {
    object(&[
        ("reference", text()),
        ("fields", array(string(64), 16)),
        ("labels", array(string(128), 16)),
        ("flags", array(string(64), 32)),
        ("rationale", string(4096)),
    ])
}
/// A reason: a fixed code, and optionally a literal sentence shown on the task.
fn reasons() -> Schema {
    let reason = without_required(
        object(&[("code", code(&ROUTE_REASONS)), ("detail", string(256))]),
        &["detail"],
    );
    array(reason, 16)
}
/// "The document allows nobody for this task": the host leaves the task
/// unrouted and visible, shows the reasons and raises it.
fn work_unroutable() -> CommandContract {
    command(
        "work.unroutable",
        object(&[("subject", subject()), ("reasons", reasons())]),
        &[
            ("recorded", result()),
            ("stale", result()),
            (
                "refused",
                outcome(&[(
                    "reason",
                    enumeration(&["no_reasons", "owner_route", "not_on_router", "unknown_task"]),
                )]),
            ),
        ],
        Shape {
            effect: Effect::Mutation,
            recovery: Recovery::KeyedIdempotent,
            deadline_ms: 10_000,
            public: &["reference", "subject"],
            asynchronous: None,
        },
    )
}
fn work_route() -> CommandContract {
    let names = || array(string(64), MAX_ROUTE_NAMES);
    command(
        "work.route",
        object(&[
            ("subject", subject()),
            ("level", level(&[])),
            ("harnesses", names()),
            ("providers", names()),
            ("prefer", names()),
            ("hold", Schema::Boolean),
            ("reasons", reasons()),
            (
                "classification",
                Schema::Nullable {
                    inner: Box::new(classification()),
                },
            ),
        ]),
        &[
            // What remained of each list after the host intersected it
            // with what the level offers.
            (
                "routed",
                outcome(&[
                    ("harnesses", names()),
                    ("providers", names()),
                    ("prefer", names()),
                ]),
            ),
            ("stale", result()),
            (
                "refused",
                outcome(&[(
                    "reason",
                    enumeration(&[
                        "not_offered",
                        "owner_route",
                        "not_on_router",
                        "unknown_task",
                    ]),
                )]),
            ),
        ],
        Shape {
            effect: Effect::Mutation,
            recovery: Recovery::KeyedIdempotent,
            deadline_ms: 10_000,
            public: &["reference", "subject"],
            asynchronous: None,
        },
    )
}
fn judgment_outcomes(v21: &Registry) -> [(&'static str, Schema); 3] {
    let Schema::Object { properties, .. } = &v21.commands["judgment.request"].outcomes["assessed"]
    else {
        unreachable!("judgment.request assessed is an object")
    };
    let provenance = properties["provenance"].clone();
    [
        (
            "assessed",
            outcome(&[
                ("labels", array(string(128), 16)),
                ("flags", array(string(64), 32)),
                ("rationale", string(4096)),
                ("provenance", provenance),
            ]),
        ),
        (
            "failed",
            outcome(&[(
                "reason",
                enumeration(&[
                    "indeterminate",
                    "provider_error",
                    "provider_unavailable",
                    "deadline_exceeded",
                ]),
            )]),
        ),
        (
            "refused",
            outcome(&[(
                "reason",
                enumeration(&["invalid_request", "budget_exceeded"]),
            )]),
        ),
    ]
}
fn judgment_completion(v21: &Registry) -> Schema {
    let outcomes = judgment_outcomes(v21);
    object(&[
        (
            "outcome",
            enumeration(&outcomes.iter().map(|(n, _)| *n).collect::<Vec<_>>()),
        ),
        (
            "output",
            Schema::Union {
                variants: outcomes.into_iter().map(|(_, s)| s).collect(),
            },
        ),
    ])
}
fn judgment_assess(v21: &Registry) -> CommandContract {
    let accepted = v21.commands["judgment.request"].outcomes["accepted"].clone();
    let mut outcomes = vec![("accepted", accepted)];
    outcomes.extend(judgment_outcomes(v21));
    command(
        "judgment.assess",
        object(&[
            ("subject", subject()),
            ("question", text()),
            (
                "context",
                object(&[(
                    "items",
                    array(
                        object(&[("label", string(128)), ("text", string(8192))]),
                        16,
                    ),
                )]),
            ),
            (
                "fields",
                array(
                    object(&[
                        ("id", string(64)),
                        ("description", string(1024)),
                        ("choices", array(string(128), 64)),
                    ]),
                    16,
                ),
            ),
            (
                "flags",
                array(
                    object(&[("id", string(64)), ("description", string(1024))]),
                    32,
                ),
            ),
            ("rationale_max_bytes", range(0, 4096)),
            (
                "budget",
                object(&[
                    ("deadline_ms", range(1000, 600_000)),
                    ("max_output_tokens", range(1, 65_536)),
                ]),
            ),
        ]),
        &outcomes,
        Shape {
            effect: Effect::Mutation,
            recovery: Recovery::KeyedIdempotent,
            deadline_ms: 60_000,
            public: &[],
            asynchronous: Some(AsyncContract {
                completion_event: "aq.judgment.assessed".into(),
                completion: judgment_completion(v21),
                correlation_field: "operation".into(),
                reconciliation_command: "operation.get".into(),
            }),
        },
    )
}
/// 2.1.0, unchanged, plus the five additive contracts.
pub(super) fn build_v2_2() -> Registry {
    let v21 = catalog::catalog_v2_1();
    let mut registry = v21.clone();
    for c in [
        work_read(),
        work_route(),
        work_unroutable(),
        judgment_assess(&v21),
    ] {
        registry.commands.insert(c.name.clone(), c);
    }
    for e in [
        event(
            "aq.judgment.assessed",
            judgment_completion(&v21),
            &["operation"],
            "output.subject",
        ),
        event(
            "aq.work.route_needed",
            object(&[("subject", subject())]),
            &[],
            "subject",
        ),
    ] {
        registry.events.insert(e.name.clone(), e);
    }
    registry
}
