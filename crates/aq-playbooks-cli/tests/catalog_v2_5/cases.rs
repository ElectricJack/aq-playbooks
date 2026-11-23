use super::*;

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
enum Place<'a> {
    Input,
    Output(&'a str),
    Event,
}

fn case(contract: &str, place: Place<'_>, valid: bool, value: Value) -> Value {
    let (kind, outcome) = match place {
        Place::Input => ("input", None),
        Place::Output(name) => ("output", Some(name)),
        Place::Event => ("event", None),
    };
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
pub(super) fn fixtures(registry: &Registry) -> Value {
    let v23: Value = serde_json::from_str(include_str!(
        "../../../../contracts/aq-host-2.3.0/fixtures.json"
    ))
    .unwrap();
    let v24: Value = serde_json::from_str(include_str!(
        "../../../../contracts/aq-host-2.4.0/fixtures.json"
    ))
    .unwrap();
    let mut cases: Vec<_> = v23["cases"]
        .as_array()
        .unwrap()
        .iter()
        .cloned()
        .map(with_environment_facts)
        .collect();
    name_cases(&mut cases, "2.4.0", v24);
    cases.extend(work_read_cases());
    cases.extend(work_route_cases());
    cases.extend(environment_build_cases());
    cases.extend(environment_admit_cases());
    cases.extend(environment_event_cases());
    json!({"assessed_guarantee": v23["assessed_guarantee"], "cases": cases, "compatibility": compatibility(registry)})
}

fn compatibility(registry: &Registry) -> Value {
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
    })
}

fn with_environment_facts(mut case: Value) -> Value {
    let is_observation = case["contract"] == "work.read" && case["outcome"] == "observed";
    if !is_observation {
        return case;
    }
    if let Some(facts) = case["value"]["facts"].as_object_mut() {
        let sample = sample_facts();
        for field in ["needs", "default_environment", "environments"] {
            facts.insert(field.into(), sample[field].clone());
        }
    }
    case
}

fn work_read_cases() -> Vec<Value> {
    let sub = json!({"id": "task-1", "revision": 3});
    let observed = |facts: Value| json!({"reference": "task-1@3", "subject": sub, "facts": facts});
    let mut no_needs = sample_facts();
    no_needs.as_object_mut().unwrap().remove("needs");
    let mut bad_environment = sample_facts();
    bad_environment["environments"] = json!([{"id": "rust"}]);
    vec![
        case(
            "work.read",
            Place::Output("observed"),
            true,
            observed(sample_facts()),
        ),
        case(
            "work.read",
            Place::Output("observed"),
            false,
            observed(no_needs),
        ),
        case(
            "work.read",
            Place::Output("observed"),
            false,
            observed(bad_environment),
        ),
    ]
}

fn work_route_cases() -> Vec<Value> {
    let sub = json!({"id": "task-1", "revision": 3});
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
    vec![
        case(
            "work.route",
            Place::Input,
            true,
            route(Some(json!("rust-postgres"))),
        ),
        case("work.route", Place::Input, true, route(None)),
        case("work.route", Place::Input, false, route(Some(json!(7)))),
    ]
}

fn environment_build_cases() -> Vec<Value> {
    let tasks = json!({"subject": {"id": "task-1", "revision": 3}});
    let project = json!({"project": "project-1"});
    let accepted = json!({"handle": "op-1"});
    vec![
        case("environment.build", Place::Input, true, tasks.clone()),
        case("environment.build", Place::Input, true, project.clone()),
        case(
            "environment.build",
            Place::Input,
            false,
            json!({"subject": {"id": "task-1"}}),
        ),
        case(
            "environment.build",
            Place::Output("accepted"),
            true,
            accepted.clone(),
        ),
        case(
            "environment.build",
            Place::Output("accepted"),
            false,
            json!({}),
        ),
        case(
            "environment.build",
            Place::Output("unchanged"),
            true,
            json!({}),
        ),
        case(
            "environment.build",
            Place::Output("built"),
            true,
            json!({"builds": [sample_build()]}),
        ),
        case(
            "environment.build",
            Place::Output("built"),
            false,
            json!({}),
        ),
        case(
            "environment.build",
            Place::Output("failed"),
            true,
            json!({"recipe": "rust", "step": "check: cargo test", "log": "error: test failed"}),
        ),
        case(
            "environment.build",
            Place::Output("failed"),
            false,
            json!({"recipe": "rust", "log": "error: test failed"}),
        ),
    ]
}

fn environment_admit_cases() -> Vec<Value> {
    let tasks = json!({"subject": {"id": "task-1", "revision": 3}});
    let project = json!({"project": "project-1"});
    let accepted = json!({"handle": "op-1"});
    let with_builds = |target: &Value| {
        let mut value = target.clone();
        value["builds"] = json!([sample_build()]);
        value
    };
    vec![
        case("environment.admit", Place::Input, true, with_builds(&tasks)),
        case(
            "environment.admit",
            Place::Input,
            true,
            with_builds(&project),
        ),
        case("environment.admit", Place::Input, false, tasks.clone()),
        case(
            "environment.admit",
            Place::Output("accepted"),
            true,
            accepted,
        ),
        case(
            "environment.admit",
            Place::Output("admitted"),
            true,
            json!({}),
        ),
        case(
            "environment.admit",
            Place::Output("refused"),
            true,
            json!({"reason": "unknown_build", "step": "registry"}),
        ),
        case(
            "environment.admit",
            Place::Output("refused"),
            false,
            json!({"reason": "because", "step": "registry"}),
        ),
    ]
}

fn environment_event_cases() -> Vec<Value> {
    let project = json!({"project": "project-1"});
    vec![
        case(
            COMPLETION,
            Place::Event,
            true,
            json!({"outcome": "built", "output": {"builds": [sample_build()]}}),
        ),
        case(
            COMPLETION,
            Place::Event,
            true,
            json!({"outcome": "admitted", "output": {}}),
        ),
        case(
            COMPLETION,
            Place::Event,
            false,
            json!({"outcome": "accepted", "output": {}}),
        ),
        case(BASE_CHANGED, Place::Event, true, project.clone()),
        case(BASE_CHANGED, Place::Event, false, json!({})),
    ]
}
