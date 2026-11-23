//! Host catalog 2.5.0 conformance and generator (sandbox environments, spec
//! 0017). It is the delivery lineage of 2.3.0 plus the 2.4.0 changes (decision
//! notes and `work.activate`), so that both the routing record (2.2.0) and the
//! delivery record (2.3.0) can pin it. It adds the environment commands, their
//! completion event, the base-change event, an `environment` on `work.route`
//! and the environment facts of `work.read`.
#[allow(dead_code)]
#[path = "support/schemas.rs"]
mod schemas;
use aq_playbooks_cli::catalog;
use aq_playbooks_compiler::{
    docs,
    draft::{SourceRecord, Workspace, check, retain_registry, seal},
};
use aq_playbooks_model::*;
use schemas::{enumeration, object, strings, subject, text};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

const ADDED_COMMANDS: [&str; 3] = ["environment.admit", "environment.build", "work.activate"];
const CHANGED_COMMANDS: [&str; 4] = [
    "decision.request",
    "operation.get",
    "work.read",
    "work.route",
];
const ADDED_EVENTS: [&str; 2] = ["aq.environment.base_changed", "aq.environment.completed"];
const CHANGED_EVENTS: [&str; 1] = ["aq.decision.completed"];
/// The contracts that 2.4.0 changed or added: 2.5.0 takes them as 2.4.0 has them.
const FROM_2_4: [&str; 3] = ["decision.request", "operation.get", "work.activate"];
const ENVIRONMENT_COMMANDS: [&str; 2] = ["environment.admit", "environment.build"];
const COMPLETION: &str = "aq.environment.completed";
const BASE_CHANGED: &str = "aq.environment.base_changed";
const ADMIT_REFUSALS: [&str; 4] = [
    "unknown_build",
    "unknown_project",
    "unknown_task",
    "unreachable",
];
const EXAMPLE: &str = "examples/deliver-with-environments";

fn fixtures_file() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/aq-host-2.5.0/fixtures.json"
    ))
    .unwrap()
}
fn contracts_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/aq-host-2.5.0")
}

// ---- The generator: 2.3.0, the 2.4.0 changes, and the environment contracts. ----

fn string(max_length: usize) -> Schema {
    Schema::string(max_length)
}
fn array(items: Schema, max_items: usize) -> Schema {
    Schema::Array {
        items: Box::new(items),
        max_items,
    }
}
fn bare() -> Schema {
    object(&[])
}
fn optional(schema: Schema, names: &[&str]) -> Schema {
    let Schema::Object {
        properties,
        mut required,
    } = schema
    else {
        unreachable!("an object")
    };
    for name in names {
        required.remove(*name);
    }
    Schema::Object {
        properties,
        required,
    }
}
/// A capability or environment name.
fn label() -> Schema {
    string(128)
}
fn labels() -> Schema {
    array(label(), 64)
}

/// The three facts 2.5.0 adds to `work.read`, always present: what the task
/// needs, the project's default environment (`""` for none), and the project's
/// allowed environments with what each provides.
fn environment_facts() -> Vec<(&'static str, Schema)> {
    vec![
        ("needs", labels()),
        ("default_environment", label()),
        (
            "environments",
            array(object(&[("id", label()), ("provides", labels())]), 64),
        ),
    ]
}
fn work_read(v23: &Registry) -> CommandContract {
    let mut read = v23.commands["work.read"].clone();
    read.version = catalog::VERSION_2_5.into();
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
    for (name, schema) in environment_facts() {
        facts.insert(name.into(), schema);
        required.insert(name.into());
    }
    read
}
fn work_route(v23: &Registry) -> CommandContract {
    let mut route = v23.commands["work.route"].clone();
    route.version = catalog::VERSION_2_5.into();
    let Schema::Object { properties, .. } = &mut route.input else {
        unreachable!("the input is an object")
    };
    properties.insert("environment".into(), label());
    route
}

fn tool() -> Schema {
    object(&[("name", label()), ("version", label())])
}
/// One build per recipe.
fn build() -> Schema {
    object(&[
        ("name", label()),
        ("digest", string(128)),
        ("recipe_hash", string(128)),
        ("toolchains", array(tool(), 32)),
    ])
}
fn builds() -> Schema {
    array(build(), 32)
}
/// The task whose result head to build (or admit), or the project whose
/// admitted recipes to rebuild on the current base. Exactly one is given.
fn target() -> Vec<(&'static str, Schema)> {
    vec![("subject", subject()), ("project", text())]
}
fn async_command(
    accepted: &Schema,
    name: &str,
    input: Schema,
    outcomes: &[(&str, Schema)],
) -> CommandContract {
    let mut all = vec![("accepted".to_string(), accepted.clone())];
    all.extend(outcomes.iter().map(|(n, s)| ((*n).to_string(), s.clone())));
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
    CommandContract {
        name: name.into(),
        version: "1.0.0".into(),
        input,
        outcomes: all.into_iter().collect(),
        capabilities: strings(&[name]),
        scopes: strings(&["project"]),
        effect: Effect::Mutation,
        recovery: Recovery::KeyedIdempotent,
        asynchronous: Some(AsyncContract {
            completion_event: COMPLETION.into(),
            completion,
            correlation_field: "operation".into(),
            reconciliation_command: "operation.get".into(),
        }),
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 65_536,
        deadline_ms: 60_000,
    }
}
fn environment_commands(accepted: &Schema) -> Vec<CommandContract> {
    vec![
        async_command(
            accepted,
            "environment.build",
            optional(object(&target()), &["subject", "project"]),
            &[
                ("unchanged", bare()),
                ("built", object(&[("builds", builds())])),
                (
                    "failed",
                    object(&[
                        ("recipe", string(256)),
                        ("step", string(256)),
                        ("log", string(8192)),
                    ]),
                ),
            ],
        ),
        async_command(
            accepted,
            "environment.admit",
            optional(
                object(&[
                    target()[0].clone(),
                    target()[1].clone(),
                    ("builds", builds()),
                ]),
                &["subject", "project"],
            ),
            &[
                ("admitted", bare()),
                (
                    "refused",
                    object(&[
                        ("reason", enumeration(&ADMIT_REFUSALS)),
                        ("step", string(256)),
                    ]),
                ),
            ],
        ),
    ]
}
/// The completion event of both asynchronous environment commands.
fn completed(commands: &[CommandContract]) -> EventContract {
    let mut outcomes = BTreeSet::new();
    let mut variants: Vec<Schema> = vec![];
    for (name, schema) in commands
        .iter()
        .flat_map(|command| &command.outcomes)
        .filter(|(name, _)| *name != "accepted")
    {
        outcomes.insert(name.clone());
        if !variants.contains(schema) {
            variants.push(schema.clone());
        }
    }
    event(
        COMPLETION,
        object(&[
            (
                "outcome",
                enumeration(&outcomes.iter().map(String::as_str).collect::<Vec<_>>()),
            ),
            ("output", Schema::Union { variants }),
        ]),
        &["operation"],
    )
}
fn event(name: &str, payload: Schema, correlation: &[&str]) -> EventContract {
    EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: name.into(),
        version: "1.0.0".into(),
        payload,
        trusted_producers: strings(&["aq.host.bridge@1"]),
        scopes: strings(&["project"]),
        correlation_fields: strings(correlation),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 65_536,
        retention_ms: 86_400_000,
    }
}

fn build_catalog() -> Registry {
    let v23 = catalog::catalog_v2_3();
    let v24 = catalog::catalog_v2_4();
    let mut registry = v23.clone();
    for name in FROM_2_4 {
        registry
            .commands
            .insert(name.into(), v24.commands[name].clone());
    }
    for name in CHANGED_EVENTS {
        registry
            .events
            .insert(name.into(), v24.events[name].clone());
    }
    for read in [work_read(&v23), work_route(&v23)] {
        registry.commands.insert(read.name.clone(), read);
    }
    let accepted = v23.commands["judgment.assess"].outcomes["accepted"].clone();
    let commands = environment_commands(&accepted);
    let completion = completed(&commands);
    for command in commands {
        registry.commands.insert(command.name.clone(), command);
    }
    registry.events.insert(completion.name.clone(), completion);
    let base_changed = event(BASE_CHANGED, object(&[("project", text())]), &[]);
    registry
        .events
        .insert(base_changed.name.clone(), base_changed);
    registry
}

fn manifest(registry: &Registry) -> Value {
    let v23: Value = serde_json::from_str(catalog::MANIFEST_2_3_JSON).unwrap();
    let v24: Value = serde_json::from_str(catalog::MANIFEST_2_4_JSON).unwrap();
    json!({
        "version": catalog::VERSION_2_5,
        "fingerprint": digest(&(catalog::VERSION_2_5, registry)).unwrap(),
        "supersedes": {"version": v23["version"], "fingerprint": v23["fingerprint"]},
        "includes": {"version": v24["version"], "fingerprint": v24["fingerprint"]},
        "commands_added": ADDED_COMMANDS,
        "commands_changed": CHANGED_COMMANDS,
        "events_added": ADDED_EVENTS,
        "events_changed": CHANGED_EVENTS,
        "context_views": v23["context_views"],
        "work_kinds": v23["work_kinds"],
        "work_change_variants": v23["work_change_variants"],
    })
}

#[test]
#[ignore = "writes contracts/aq-host-2.5.0; run by hand after changing the generator"]
fn write_catalog_v2_5() {
    let registry = build_catalog();
    let dir = contracts_dir();
    std::fs::write(dir.join("registry.json"), pretty(&registry)).unwrap();
    std::fs::write(dir.join("manifest.json"), pretty(&manifest(&registry))).unwrap();
    std::fs::write(dir.join("fixtures.json"), pretty(&fixtures(&registry))).unwrap();
    std::fs::write(dir.join("notes.json"), pretty(&notes())).unwrap();
}

// ---- Conformance. ----

#[test]
fn generated_catalog_files_are_pinned() {
    let registry = catalog::catalog_v2_5();
    assert_eq!(registry, build_catalog());
    for (name, shipped, generated) in [
        ("registry", catalog::REGISTRY_2_5_JSON, pretty(&registry)),
        (
            "manifest",
            catalog::MANIFEST_2_5_JSON,
            pretty(&manifest(&registry)),
        ),
        (
            "fixtures",
            include_str!("../../../contracts/aq-host-2.5.0/fixtures.json"),
            pretty(&fixtures(&registry)),
        ),
        ("notes", docs::CATALOG_2_5_NOTES, pretty(&notes())),
    ] {
        assert_eq!(shipped, generated, "{name}");
    }
}

#[test]
fn catalog_2_5_pins_its_version_and_lineage() {
    let registry = catalog::catalog_v2_5();
    assert_eq!(catalog::VERSION_2_5, "2.5.0");
    let manifest: Value = serde_json::from_str(catalog::MANIFEST_2_5_JSON).unwrap();
    assert_eq!(
        manifest["fingerprint"],
        digest(&(catalog::VERSION_2_5, &registry)).unwrap()
    );
    assert_eq!(
        (
            &manifest["supersedes"]["version"],
            &manifest["includes"]["version"]
        ),
        (&json!("2.3.0"), &json!("2.4.0"))
    );
}

#[test]
fn catalog_2_5_schema_definitions_and_new_fingerprints_validate() {
    let registry = catalog::catalog_v2_5();
    assert_eq!(registry.commands.len(), 16 + 10 + 2 + 1);
    assert_eq!(registry.events.len(), 11 + 2 + 2);
    for (name, command) in &registry.commands {
        assert_eq!(name, &command.name);
        command.input.validate_definition().unwrap();
        for output in command.outcomes.values() {
            output.validate_definition().unwrap();
        }
    }
    for (name, event) in &registry.events {
        assert_eq!(name, &event.name);
        event.payload.validate_definition().unwrap();
    }
    let compat = &fixtures_file()["compatibility"];
    for name in ADDED_COMMANDS {
        assert_eq!(
            compat["added_commands"][name],
            json!(registry.commands[name].fingerprint().unwrap())
        );
    }
    for name in ADDED_EVENTS {
        assert_eq!(
            compat["added_events"][name],
            json!(registry.events[name].fingerprint().unwrap())
        );
    }
}

#[test]
fn every_command_the_routing_and_delivery_records_use_is_present_and_unchanged() {
    let compat = &fixtures_file()["compatibility"];
    let v22 = catalog::catalog_v2_2();
    let v23 = catalog::catalog_v2_3();
    let v25 = catalog::catalog_v2_5();
    for (lineage, old) in [("2.2.0", &v22), ("2.3.0", &v23)] {
        verify_lineage(lineage, old, &v25, compat);
    }
    // The routing record's 2.2.0 commands that stay byte-identical are listed.
    assert!(
        compat["routing_commands_unchanged"]
            .as_array()
            .unwrap()
            .contains(&json!("judgment.assess"))
    );
}

#[test]
fn previous_decision_contracts_match_catalog_2_4() {
    let v24 = catalog::catalog_v2_4();
    let v25 = catalog::catalog_v2_5();
    // The commands and events 2.4.0 changed are exactly 2.4.0's.
    for name in FROM_2_4 {
        assert_eq!(v25.commands[name], v24.commands[name], "{name}");
    }
    for name in CHANGED_EVENTS {
        assert_eq!(v25.events[name], v24.events[name], "{name}");
    }
}

#[test]
fn environment_additions_match_the_manifest() {
    let v23 = catalog::catalog_v2_3();
    let v25 = catalog::catalog_v2_5();
    // The added contracts are exactly the declared ones.
    let added: BTreeSet<&str> = v25
        .commands
        .keys()
        .filter(|n| !v23.commands.contains_key(*n))
        .map(String::as_str)
        .collect();
    assert_eq!(added, BTreeSet::from(ADDED_COMMANDS));
    let added: BTreeSet<&str> = v25
        .events
        .keys()
        .filter(|n| !v23.events.contains_key(*n))
        .map(String::as_str)
        .collect();
    assert_eq!(added, BTreeSet::from(ADDED_EVENTS));
}

#[test]
fn published_unchanged_commands_retain_their_old_text() {
    let v23 = catalog::catalog_v2_3();
    // Each 2.3.0 entry other than the changed ones appears unchanged in the file.
    for name in v23
        .commands
        .keys()
        .filter(|n| !CHANGED_COMMANDS.contains(&n.as_str()))
    {
        let entry =
            serde_json::to_string_pretty(&serde_json::to_value(&v23.commands[name]).unwrap())
                .unwrap()
                .replace('\n', "\n    ");
        assert!(
            catalog::REGISTRY_2_5_JSON.contains(&format!("\"{name}\": {entry}")),
            "{name}"
        );
    }
}

#[test]
fn work_read_and_work_route_only_gain_fields() {
    let v23 = catalog::catalog_v2_3();
    let v25 = catalog::catalog_v2_5();

    // `work.route`: one optional input field; outcomes and the rest are as before.
    let (old, new) = (&v23.commands["work.route"], &v25.commands["work.route"]);
    assert_eq!(new.version, "2.5.0");
    assert_eq!(new.capabilities, old.capabilities);
    assert_eq!(new.outcomes, old.outcomes);
    let (
        Schema::Object {
            properties: op,
            required: or,
        },
        Schema::Object {
            properties: np,
            required: nr,
        },
    ) = (&old.input, &new.input)
    else {
        panic!("the input is an object")
    };
    assert_eq!(or, nr);
    assert!(!nr.contains("environment"));
    for (field, schema) in op {
        assert_eq!(&np[field], schema, "{field}");
    }
    assert_eq!(
        np.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        op.keys()
            .map(String::as_str)
            .chain(["environment"])
            .collect::<BTreeSet<_>>()
    );
}

#[test]
fn work_read_preserves_existing_facts() {
    let v23 = catalog::catalog_v2_3();
    let v25 = catalog::catalog_v2_5();
    // `work.read`: the facts of 2.3.0, and three more, always present.
    let (old, new) = (&v23.commands["work.read"], &v25.commands["work.read"]);
    assert_eq!(new.version, "2.5.0");
    assert_eq!(new.input, old.input);
    assert_eq!(new.outcomes["refused"], old.outcomes["refused"]);
    let facts = |c: &CommandContract| match &c.outcomes["observed"] {
        Schema::Object { properties, .. } => match &properties["facts"] {
            Schema::Object {
                properties,
                required,
            } => (properties.clone(), required.clone()),
            _ => panic!("facts is an object"),
        },
        _ => panic!("observed is an object"),
    };
    let ((of, ofr), (nf, nfr)) = (facts(old), facts(new));
    for (field, schema) in &of {
        assert_eq!(&nf[field], schema, "facts.{field}");
    }
    let added: BTreeSet<&str> = nf
        .keys()
        .filter(|k| !of.contains_key(*k))
        .map(String::as_str)
        .collect();
    assert_eq!(
        added,
        environment_facts()
            .iter()
            .map(|(n, _)| *n)
            .collect::<BTreeSet<_>>()
    );
    assert_eq!(
        nfr.difference(&ofr)
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        added
    );
    assert!(ofr.is_subset(&nfr));
}

#[test]
fn environment_commands_complete_on_one_event() {
    let v25 = catalog::catalog_v2_5();
    let event = &v25.events[COMPLETION];
    assert!(event.correlation_fields.contains("operation"));
    assert_eq!(event.trusted_producers, strings(&["aq.host.bridge@1"]));
    for name in ENVIRONMENT_COMMANDS {
        let c = &v25.commands[name];
        let a = c.asynchronous.as_ref().unwrap();
        assert_eq!(
            (c.version.as_str(), &c.capabilities, &c.scopes),
            ("1.0.0", &strings(&[name]), &strings(&["project"]))
        );
        assert_eq!(
            (&c.effect, &c.recovery, c.deadline_ms),
            (&Effect::Mutation, &Recovery::KeyedIdempotent, 60_000)
        );
        assert_eq!(
            (
                a.completion_event.as_str(),
                a.correlation_field.as_str(),
                a.reconciliation_command.as_str()
            ),
            (COMPLETION, "operation", "operation.get")
        );
        assert!(a.completion.assignable_to(&event.payload), "{name}");
        let Schema::Object { properties, .. } = &a.completion else {
            panic!("completion is an object")
        };
        let Schema::String { values, .. } = &properties["outcome"] else {
            panic!("outcome is an enumeration")
        };
        let business: Vec<&String> = c.outcomes.keys().filter(|n| *n != "accepted").collect();
        assert_eq!(values.iter().collect::<Vec<_>>(), business, "{name}");
    }
    let outcomes = |name: &str| -> BTreeSet<String> {
        v25.commands[name]
            .outcomes
            .keys()
            .filter(|n| *n != "accepted")
            .cloned()
            .collect()
    };
    assert_eq!(
        outcomes("environment.build"),
        strings(&["built", "failed", "unchanged"])
    );
    assert_eq!(
        outcomes("environment.admit"),
        strings(&["admitted", "refused"])
    );
    // Both commands take a task `subject` or a `project`, and neither is required.
    for name in ENVIRONMENT_COMMANDS {
        let Schema::Object {
            properties,
            required,
        } = &v25.commands[name].input
        else {
            panic!("the input is an object")
        };
        assert!(properties.contains_key("subject") && properties.contains_key("project"));
        assert!(!required.contains("subject") && !required.contains("project"));
    }
    // The base-change event names the project, once per project.
    assert_eq!(
        v25.events[BASE_CHANGED].payload,
        object(&[("project", text())])
    );
    assert!(v25.events[BASE_CHANGED].correlation_fields.is_empty());
}

#[test]
fn valid_and_invalid_fixture_cases_validate_as_declared() {
    let registry = catalog::catalog_v2_5();
    let covered = schemas::assert_fixture_cases(&registry, &fixtures_file());
    // A valid case for every input and outcome of the environment commands
    // and for both environment events.
    for name in ENVIRONMENT_COMMANDS {
        assert!(covered.contains(&format!("{name}.input")), "{name}");
        for outcome in registry.commands[name].outcomes.keys() {
            assert!(
                covered.contains(&format!("{name}.{outcome}")),
                "{name}.{outcome}"
            );
        }
    }
    for name in [COMPLETION, BASE_CHANGED] {
        assert!(covered.contains(name), "{name}");
    }
    assert!(covered.contains("work.read.observed"));
    assert!(covered.contains("work.route.input"));
}

#[test]
fn the_notes_cover_every_contract_and_render() {
    let registry = catalog::catalog_v2_5();
    let all: Value = serde_json::from_str(docs::CATALOG_2_5_NOTES).unwrap();
    let keys = |kind: &str| {
        all[kind]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(
        keys("commands"),
        registry.commands.keys().cloned().collect::<BTreeSet<_>>()
    );
    assert_eq!(
        keys("events"),
        registry.events.keys().cloned().collect::<BTreeSet<_>>()
    );
    let commands: BTreeSet<String> = registry.commands.keys().cloned().collect();
    let events: BTreeSet<String> = registry.events.keys().cloned().collect();
    let rendered =
        docs::catalog_with_extensions(&registry, docs::CATALOG_2_5_NOTES, &commands, &events, &[])
            .unwrap_or_else(|e| panic!("{e}"));
    for name in ADDED_COMMANDS.iter().chain(&CHANGED_COMMANDS) {
        assert!(rendered.contains(&format!("### `{name}`")), "{name}");
    }
    for name in ADDED_EVENTS.iter().chain(&CHANGED_EVENTS) {
        assert!(rendered.contains(&format!("### `{name}`")), "{name}");
    }
    for fact in environment_facts().iter().map(|(n, _)| *n) {
        assert!(rendered.contains(&format!("`{fact}`")), "{fact}");
    }
    assert!(rendered.contains("responder's note"));
    assert!(rendered.contains("Activates draft descendants"));
    for name in ENVIRONMENT_COMMANDS {
        let contract = serde_json::to_value(&registry.commands[name]).unwrap();
        assert!(docs::published_notes("commands", name, &contract).is_some());
    }
}

// ---- Feasibility: the delivery example pins 2.5.0. ----

fn example_file(name: &str) -> String {
    std::fs::read_to_string(contracts_dir().join(EXAMPLE).join(name))
        .unwrap_or_else(|e| panic!("{EXAMPLE}/{name}: {e}"))
}
fn example_json(name: &str) -> Value {
    serde_json::from_str(&example_file(name)).unwrap_or_else(|e| panic!("{EXAMPLE}/{name}: {e}"))
}

#[test]
fn the_environment_delivery_example_checks_and_seals_against_catalog_2_5_0() {
    let record: SourceRecord = serde_json::from_value(example_json("record.json")).unwrap();
    let (catalog, _) =
        retain_registry(&catalog::catalog_v2_5(), &record.commands, &record.events).unwrap();
    assert!(catalog.events.contains_key(COMPLETION));
    let facts: BTreeMap<String, Value> =
        serde_json::from_value(example_json("facts.json")).unwrap();
    let ws = Workspace {
        markdown: example_file("source.md"),
        record,
        catalog,
        facts,
        draft: example_json("draft.json"),
    };
    let diagnostics = check(&ws);
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    let sealed = seal(&ws).unwrap_or_else(|d| panic!("{d:#?}"));
    assert!(sealed.verify().is_ok());
    assert_eq!(sealed.content.validate_structure(), vec![]);
    // The catalog file the example ships is the one the host places.
    assert_eq!(
        example_json("catalog.json"),
        serde_json::to_value(&ws.catalog).unwrap()
    );
    // It builds before the merge, admits after it, and rebuilds on a new base.
    let steps = ws.draft["steps"].as_object().unwrap();
    for (step, expected) in [
        ("publish", "branch.publish"),
        ("build", "environment.build"),
        ("merge", "pull_request.merge"),
        ("admit", "environment.admit"),
        ("rb_build", "environment.build"),
        ("rb_admit", "environment.admit"),
        ("rb_notify", "supervisor.notify"),
        ("send_back_build", "work.return"),
    ] {
        assert_eq!(steps[step]["op"]["command"], expected, "{step}");
    }
    for (step, target) in [
        ("await_publish", "build"),
        ("await_merge", "admit"),
        ("await_failure", "may_return_build"),
    ] {
        assert_eq!(steps[step]["edges"]["completed"], target, "{step}");
    }
    assert_eq!(
        steps["build"]["op"]["input"]["fields"]["subject"]["op"],
        "object"
    );
    assert!(
        steps["rb_build"]["op"]["input"]["fields"]
            .get("project")
            .is_some()
    );
    let triggers: BTreeSet<&str> = ws.draft["triggers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        triggers,
        BTreeSet::from(["aq.work.result_ready", BASE_CHANGED])
    );
    // The strict rules still bite: the builds of a join that may be
    // `unchanged` or `failed` cannot be read without the `built` edge.
    let mut wrong = ws;
    wrong.draft["steps"]["await_build"]["op"]["accepted_outcomes"] = json!(["built", "unchanged"]);
    assert!(!check(&wrong).is_empty());
}

#[path = "support/catalog_fixtures.rs"]
mod catalog_fixtures;
use catalog_fixtures::{fingerprints, pretty};

#[path = "catalog_v2_5/cases.rs"]
mod cases;
use cases::fixtures;
#[path = "catalog_v2_5/notes.rs"]
mod catalog_notes;
use catalog_notes::notes;

fn verify_lineage(lineage: &str, old: &Registry, v25: &Registry, compat: &Value) {
    for (name, old_command) in &old.commands {
        let new = v25
            .commands
            .get(name)
            .unwrap_or_else(|| panic!("{lineage}: {name} is missing from 2.5.0"));
        if CHANGED_COMMANDS.contains(&name.as_str()) {
            continue;
        }
        assert_eq!(new, old_command, "{lineage}: {name}");
        assert_eq!(
            canonical_bytes(new).unwrap(),
            canonical_bytes(old_command).unwrap(),
            "{lineage}: {name}"
        );
        assert_eq!(
            compat["unchanged_commands"][name],
            json!(old_command.fingerprint().unwrap()),
            "{lineage}: {name}"
        );
    }
    for (name, old_event) in old
        .events
        .iter()
        .filter(|(name, _)| !CHANGED_EVENTS.contains(&name.as_str()))
    {
        assert_eq!(&v25.events[name], old_event, "{lineage}: {name}");
    }
}
