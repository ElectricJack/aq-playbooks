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
const EXAMPLE: &str = "../aq-host-2.3.0/examples/deliver-by-playbook";

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
    for command in commands {
        for (name, schema) in &command.outcomes {
            if name != "accepted" {
                outcomes.insert(name.clone());
                if !variants.contains(schema) {
                    variants.push(schema.clone());
                }
            }
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

fn pretty(value: &impl serde::Serialize) -> String {
    // A Value orders keys, as every published bundle does.
    let value = serde_json::to_value(value).unwrap();
    serde_json::to_string_pretty(&value).unwrap() + "\n"
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

fn fingerprints<'a, T: 'a>(
    items: impl Iterator<Item = (&'a String, &'a T)>,
    keep: impl Fn(&str) -> bool,
    fingerprint: impl Fn(&T) -> Hash,
) -> Value {
    items
        .filter(|(n, _)| keep(n))
        .map(|(n, c)| (n.clone(), json!(fingerprint(c))))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

fn name_cases(cases: &mut Vec<Value>, prefix: &str, from: Value) {
    for case in from["cases"].as_array().unwrap() {
        let mut case = case.clone();
        if case.get("name").is_none() {
            let place = case.get("outcome").and_then(Value::as_str).unwrap_or("");
            let validity = if case["valid"].as_bool().unwrap() {
                "valid"
            } else {
                "invalid"
            };
            case["name"] = json!(format!(
                "{prefix}: {validity} {} {} {place}",
                case["contract"].as_str().unwrap(),
                case["kind"].as_str().unwrap()
            ));
        }
        cases.push(case);
    }
}
fn case(contract: &str, kind: &str, outcome: Option<&str>, valid: bool, value: Value) -> Value {
    let mut case = json!({
        "contract": contract, "kind": kind, "valid": valid, "value": value,
        "name": format!(
            "{} {contract} {kind} {}",
            if valid { "valid" } else { "invalid" },
            outcome.unwrap_or("")
        ).trim().to_string()
    });
    if let Some(outcome) = outcome {
        case["outcome"] = json!(outcome);
    }
    case
}
fn sample_build() -> Value {
    json!({"name": "rust", "digest": "sha256:aaaa", "recipe_hash": "sha256:bbbb",
        "toolchains": [{"name": "rustc", "version": "1.90.0"}]})
}
fn sample_facts() -> Value {
    json!({"base": "2222222222222222222222222222222222222222", "branch": "aq/task-1",
    "changed": true, "charged_failures": 0, "description": "Add a parser.",
    "ended_attempts": 0, "head": "1111111111111111111111111111111111111111",
    "kind": "feature", "level_hint": "", "main_head": "3333333333333333333333333333333333333333",
    "origin": "owner", "outcome": "completed", "pull_request": null, "returns": 0,
    "route": "none", "title": "Add a parser",
    "needs": ["rust", "postgres"], "default_environment": "rust",
    "environments": [
        {"id": "rust", "provides": ["rust"]},
        {"id": "rust-postgres", "provides": ["rust", "postgres"]}
    ]})
}
fn fixtures(registry: &Registry) -> Value {
    let v23: Value = serde_json::from_str(include_str!(
        "../../../contracts/aq-host-2.3.0/fixtures.json"
    ))
    .unwrap();
    let v24: Value = serde_json::from_str(include_str!(
        "../../../contracts/aq-host-2.4.0/fixtures.json"
    ))
    .unwrap();
    let mut cases: Vec<Value> = vec![];
    // The 2.3.0 cases; a valid `work.read` answer also carries the new facts.
    for case in v23["cases"].as_array().unwrap() {
        let mut case = case.clone();
        if case["contract"] == "work.read" && case["outcome"] == "observed" {
            if let Some(facts) = case["value"]["facts"].as_object_mut() {
                for (name, value) in sample_facts().as_object().unwrap() {
                    if ["needs", "default_environment", "environments"].contains(&name.as_str()) {
                        facts.insert(name.clone(), value.clone());
                    }
                }
            }
        }
        cases.push(case);
    }
    name_cases(&mut cases, "2.4.0", v24.clone());
    let sub = json!({"id": "task-1", "revision": 3});
    let observed = |facts: Value| json!({"reference": "task-1@3", "subject": sub, "facts": facts});
    let mut no_needs = sample_facts();
    no_needs.as_object_mut().unwrap().remove("needs");
    let mut bad_environment = sample_facts();
    bad_environment["environments"] = json!([{"id": "rust"}]);
    cases.extend([
        case(
            "work.read",
            "output",
            Some("observed"),
            true,
            observed(sample_facts()),
        ),
        case(
            "work.read",
            "output",
            Some("observed"),
            false,
            observed(no_needs),
        ),
        case(
            "work.read",
            "output",
            Some("observed"),
            false,
            observed(bad_environment),
        ),
    ]);
    let route = |environment: Option<Value>| {
        let mut value = json!({"classification": null, "harnesses": [], "hold": false,
            "level": "standard-high", "prefer": [], "providers": [],
            "reasons": [{"code": "level_from_kind", "detail": "Features are Standard."}],
            "subject": sub});
        if let Some(environment) = environment {
            value["environment"] = environment;
        }
        value
    };
    cases.extend([
        case(
            "work.route",
            "input",
            None,
            true,
            route(Some(json!("rust-postgres"))),
        ),
        case("work.route", "input", None, true, route(None)),
        case("work.route", "input", None, false, route(Some(json!(7)))),
    ]);
    let accepted = json!({"handle": "op-1"});
    let tasks = json!({"subject": sub});
    let project = json!({"project": "project-1"});
    let with_builds = |target: &Value| {
        let mut value = target.clone();
        value["builds"] = json!([sample_build()]);
        value
    };
    cases.extend([
        case("environment.build", "input", None, true, tasks.clone()),
        case("environment.build", "input", None, true, project.clone()),
        case(
            "environment.build",
            "input",
            None,
            false,
            json!({"subject": {"id": "task-1"}}),
        ),
        case(
            "environment.build",
            "output",
            Some("accepted"),
            true,
            accepted.clone(),
        ),
        case(
            "environment.build",
            "output",
            Some("accepted"),
            false,
            json!({}),
        ),
        case(
            "environment.build",
            "output",
            Some("unchanged"),
            true,
            json!({}),
        ),
        case(
            "environment.build",
            "output",
            Some("built"),
            true,
            json!({"builds": [sample_build()]}),
        ),
        case(
            "environment.build",
            "output",
            Some("built"),
            false,
            json!({}),
        ),
        case(
            "environment.build",
            "output",
            Some("failed"),
            true,
            json!({"recipe": "rust", "step": "check: cargo test", "log": "error: test failed"}),
        ),
        case(
            "environment.build",
            "output",
            Some("failed"),
            false,
            json!({"recipe": "rust", "log": "error: test failed"}),
        ),
        case(
            "environment.admit",
            "input",
            None,
            true,
            with_builds(&tasks),
        ),
        case(
            "environment.admit",
            "input",
            None,
            true,
            with_builds(&project),
        ),
        case("environment.admit", "input", None, false, tasks.clone()),
        case(
            "environment.admit",
            "output",
            Some("accepted"),
            true,
            accepted,
        ),
        case(
            "environment.admit",
            "output",
            Some("admitted"),
            true,
            json!({}),
        ),
        case(
            "environment.admit",
            "output",
            Some("refused"),
            true,
            json!({"reason": "unknown_build", "step": "registry"}),
        ),
        case(
            "environment.admit",
            "output",
            Some("refused"),
            false,
            json!({"reason": "because", "step": "registry"}),
        ),
        case(
            COMPLETION,
            "event",
            None,
            true,
            json!({"outcome": "built", "output": {"builds": [sample_build()]}}),
        ),
        case(
            COMPLETION,
            "event",
            None,
            true,
            json!({"outcome": "admitted", "output": {}}),
        ),
        case(
            COMPLETION,
            "event",
            None,
            false,
            json!({"outcome": "accepted", "output": {}}),
        ),
        case(BASE_CHANGED, "event", None, true, project.clone()),
        case(BASE_CHANGED, "event", None, false, json!({})),
    ]);
    let v22 = catalog::catalog_v2_2();
    let v23_registry = catalog::catalog_v2_3();
    let added = |n: &str| ADDED_COMMANDS.contains(&n) || ADDED_EVENTS.contains(&n);
    let changed = |n: &str| CHANGED_COMMANDS.contains(&n) || CHANGED_EVENTS.contains(&n);
    let unchanged = |n: &str| !added(n) && !changed(n);
    let command_fp = |c: &CommandContract| c.fingerprint().unwrap();
    let event_fp = |e: &EventContract| e.fingerprint().unwrap();
    let m22: Value = serde_json::from_str(catalog::MANIFEST_2_2_JSON).unwrap();
    let m23: Value = serde_json::from_str(catalog::MANIFEST_2_3_JSON).unwrap();
    let m24: Value = serde_json::from_str(catalog::MANIFEST_2_4_JSON).unwrap();
    let changes = |old: &Registry| -> Value {
        CHANGED_COMMANDS
            .iter()
            .map(|n| {
                (
                    n.to_string(),
                    json!({
                        "capabilities": registry.commands[*n].capabilities,
                        "from": old.commands[*n].version,
                        "to": registry.commands[*n].version,
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>()
            .into()
    };
    json!({
        "assessed_guarantee": v23["assessed_guarantee"],
        "cases": cases,
        "compatibility": {
            "base": {
                "version": m23["version"], "fingerprint": m23["fingerprint"],
                "registry_sha256": hash_bytes(catalog::REGISTRY_2_3_JSON.as_bytes()),
            },
            "includes": {
                "version": m24["version"], "fingerprint": m24["fingerprint"],
                "registry_sha256": hash_bytes(catalog::REGISTRY_2_4_JSON.as_bytes()),
            },
            "routing_base": {
                "version": m22["version"], "fingerprint": m22["fingerprint"],
                "registry_sha256": hash_bytes(catalog::REGISTRY_2_2_JSON.as_bytes()),
            },
            "unchanged_commands": fingerprints(registry.commands.iter(), unchanged, command_fp),
            "unchanged_events": fingerprints(registry.events.iter(), unchanged, event_fp),
            "changed_commands": changes(&v23_registry),
            "changed_events": fingerprints(registry.events.iter(), |n| changed(n), event_fp),
            "added_commands": fingerprints(registry.commands.iter(), added, command_fp),
            "added_events": fingerprints(registry.events.iter(), added, event_fp),
            "routing_commands_unchanged": v22.commands.keys()
                .filter(|n| unchanged(n)).collect::<Vec<_>>(),
        }
    })
}

fn notes() -> Value {
    let mut notes: Value = serde_json::from_str(docs::CATALOG_2_3_NOTES).unwrap();
    let v24: Value = serde_json::from_str(docs::CATALOG_2_4_NOTES).unwrap();
    notes["about"] = json!(
        "What the host says about each contract of catalog 2.5.0, for an authoring agent. Rendered into CATALOG.md for the commands and events a source may use (aq_playbooks_compiler::docs::catalog). An empty string means the catalog does not say. Kept beside registry.json so that the registry, and every fingerprint, stays unchanged. 2.5.0 is catalog 2.3.0 with the changes of 2.4.0 (decision notes and `work.activate`), the sandbox environment commands and events, an optional `environment` on `work.route` and the environment facts of `work.read`; every other note is the one of 2.3.0."
    );
    for name in FROM_2_4 {
        if let Some(note) = v24["commands"].get(name) {
            notes["commands"][name] = note.clone();
        }
    }
    for name in CHANGED_EVENTS {
        notes["events"][name] = v24["events"][name].clone();
    }
    notes["commands"]["work.read"]["purpose"] = json!(
        "Reads the facts of a task as it is now, with those of its latest result (whether it changed anything, its branch and pull request, and how many times delivery has sent it back) and its sandbox environment facts: what it needs and which environments its project allows."
    );
    notes["commands"]["work.read"]["notes"]
        .as_array_mut()
        .unwrap()
        .push(json!(
            "The environment facts, always present: `needs` (at most 64 capability names the task asks for, from its `aq:needs` metadata; empty when it asks for none); `default_environment` (the id of the project's default environment, or `\"\"` when it has none); `environments` (the project's allowed environments, at most 64 `{id, provides}`, where `provides` lists at most 64 capability names the environment provides). To choose an environment, pick one whose `provides` holds every name in `needs`; the document says how to choose among them."
        ));
    notes["commands"]["work.route"]["input"]["environment"] = json!(
        "The id of the environment the task runs in, one of the `id` values of `environments` from `work.read`. Leave it out to keep the task's current environment, or the project's default when it has none. The host refuses an environment the project does not allow or one that lacks a capability in `needs`, and offers the task only to a node that can run it."
    );
    notes["commands"]["work.route"]["outcomes"]["refused"] = json!(
        "Nothing was written; end. `not_offered`: nothing at that level (or in a non-empty list) is offered, and the host raises the task itself; never try a route the document does not give. `owner_route`, `not_on_router`, `unknown_task`: end."
    );
    notes["commands"]["environment.build"] = json!({
        "purpose": "Builds the environment recipes under `.aq/environments/` whose bytes differ from the project's admitted version, for a task's result head or for the project's admitted recipes on the current base. It answers later, on `aq.environment.completed`.",
        "input": {
            "subject": "The task whose result head to build, at the revision of the trigger. Give `subject` or `project`, not both.",
            "project": "The project whose admitted recipes to rebuild on the current base (after `aq.environment.base_changed`). Give `project` or `subject`, not both."
        },
        "outcomes": {
            "accepted": "The build is queued on the builder; the edge binds `{handle}`. Join the handle under a key.",
            "unchanged": "No recipe differs from the admitted version; there is nothing to admit.",
            "built": "Every changed recipe was built, checked, scanned and published by digest. `builds` has one entry per recipe: its `name`, image `digest`, `recipe_hash` and `toolchains` (`{name, version}`). Pass `builds` to `environment.admit` unchanged.",
            "failed": "A recipe did not build. `recipe` names it, `step` the step that failed and `log` is the redacted log. The previous environment version stays in place; nothing was registered."
        },
        "notes": [
            "The build runs on the builder, outside attempt capacity, and compares recipe bytes itself: no fact about changed files is needed.",
            "Build before the merge, so that a broken recipe never merges: on `failed`, send the task back with `work.return` and the failure as its detail."
        ]
    });
    notes["commands"]["environment.admit"] = json!({
        "purpose": "Records each build as the project's next environment version, adds it to the project's environments and makes it available to nodes. It answers later, on `aq.environment.completed`.",
        "input": {
            "subject": "The task whose builds are admitted. Give `subject` or `project`, not both.",
            "project": "The project whose rebuilt environments are admitted. Give `project` or `subject`, not both.",
            "builds": "The `builds` of the `built` answer of `environment.build`, unchanged."
        },
        "outcomes": {
            "accepted": "The admission is queued; the edge binds `{handle}`. Join the handle under a key.",
            "admitted": "Every build is now the project's next environment version and is available to nodes. An attempt already placed keeps its environment.",
            "refused": "Nothing was admitted and the previous version stays. `reason` is `unknown_build` (no such build was published), `unknown_task`, `unknown_project` or `unreachable` (the registry did not answer); `step` names the failing step."
        },
        "notes": [
            "Admit after the merge: when `pull_request.merge` completes with `merged`. A squash merge makes a new commit with the same recipe bytes, so the builds still match."
        ]
    });
    notes["events"][COMPLETION] = json!({
        "purpose": "The answer to a sandbox environment command that answers later: `environment.build` or `environment.admit`.",
        "sent": "Only as the completion of an operation a playbook started with one of those commands, once per operation. Join the handle, which types the answer per outcome; do not wait on this event or start a playbook on it.",
        "payload": {
            "outcome": "The outcome of the command that was called: one of its business outcomes, never `accepted`.",
            "output": "That outcome's output: `builds` for `built`, `recipe`, `step` and `log` for `failed`, `reason` and `step` for `refused`."
        },
        "notes": ["It carries no task or revision: the join correlates it by the operation."]
    });
    notes["events"][BASE_CHANGED] = json!({
        "purpose": "A new base template was admitted, so a project's environments must be rebuilt on it.",
        "sent": "Today the host sends it once per project when it admits a new base template (a new OS or runtime image in the builder's configuration).",
        "payload": {
            "project": "The project's id. Pass it as `project` to `environment.build` and then `environment.admit`."
        },
        "notes": [
            "A project whose rebuild fails stays on its previous environment; report the failure to the project's supervisor. Other projects are not affected."
        ]
    });
    notes
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
    assert_eq!(catalog::REGISTRY_2_5_JSON, pretty(&registry));
    assert_eq!(catalog::MANIFEST_2_5_JSON, pretty(&manifest(&registry)));
    assert_eq!(
        include_str!("../../../contracts/aq-host-2.5.0/fixtures.json"),
        pretty(&fixtures(&registry))
    );
    assert_eq!(docs::CATALOG_2_5_NOTES, pretty(&notes()));
    assert_eq!(catalog::VERSION_2_5, "2.5.0");

    let manifest: Value = serde_json::from_str(catalog::MANIFEST_2_5_JSON).unwrap();
    assert_eq!(
        manifest["fingerprint"],
        digest(&(catalog::VERSION_2_5, &registry)).unwrap()
    );
    assert_eq!(manifest["supersedes"]["version"], "2.3.0");
    assert_eq!(manifest["includes"]["version"], "2.4.0");
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
    let v24 = catalog::catalog_v2_4();
    let v25 = catalog::catalog_v2_5();
    for (lineage, old) in [("2.2.0", &v22), ("2.3.0", &v23)] {
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
        for (name, old_event) in &old.events {
            if CHANGED_EVENTS.contains(&name.as_str()) {
                continue;
            }
            assert_eq!(&v25.events[name], old_event, "{lineage}: {name}");
        }
    }
    // The routing record's 2.2.0 commands that stay byte-identical are listed.
    assert!(
        compat["routing_commands_unchanged"]
            .as_array()
            .unwrap()
            .contains(&json!("judgment.assess"))
    );
    // The commands and events 2.4.0 changed are exactly 2.4.0's.
    for name in FROM_2_4 {
        assert_eq!(v25.commands[name], v24.commands[name], "{name}");
    }
    for name in CHANGED_EVENTS {
        assert_eq!(v25.events[name], v24.events[name], "{name}");
    }
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
        assert_eq!(c.version, "1.0.0");
        assert_eq!(c.capabilities, strings(&[name]));
        assert_eq!(c.scopes, strings(&["project"]));
        assert_eq!(c.deadline_ms, 60_000);
        assert_eq!(
            (&c.effect, &c.recovery),
            (&Effect::Mutation, &Recovery::KeyedIdempotent)
        );
        assert_eq!(a.completion_event, COMPLETION);
        assert_eq!(a.correlation_field, "operation");
        assert_eq!(a.reconciliation_command, "operation.get");
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
    let mut covered = BTreeSet::new();
    for fixture in fixtures_file()["cases"].as_array().unwrap() {
        let name = fixture["name"].as_str().unwrap();
        let contract = fixture["contract"].as_str().unwrap();
        let kind = fixture["kind"].as_str().unwrap();
        let schema = match kind {
            "input" => &registry.commands[contract].input,
            "output" => &registry.commands[contract].outcomes[fixture["outcome"].as_str().unwrap()],
            "event" => &registry.events[contract].payload,
            other => panic!("unknown fixture kind {other}"),
        };
        let valid = fixture["valid"].as_bool().unwrap();
        assert_eq!(schema.validate(&fixture["value"]).is_ok(), valid, "{name}");
        if valid {
            covered.insert(match kind {
                "output" => format!("{contract}.{}", fixture["outcome"].as_str().unwrap()),
                "input" => format!("{contract}.input"),
                _ => contract.to_string(),
            });
        }
    }
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
fn the_example_delivery_playbook_checks_and_seals_against_catalog_2_5_0() {
    let mut record: SourceRecord = serde_json::from_value(example_json("record.json")).unwrap();
    // The record may also use the environment contracts.
    for name in ENVIRONMENT_COMMANDS {
        record.commands.insert(name.into());
        record.capabilities.insert(name.into());
    }
    record.events.insert(BASE_CHANGED.into());
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
}
