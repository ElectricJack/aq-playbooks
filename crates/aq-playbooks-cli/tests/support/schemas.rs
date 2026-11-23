//! Schema builders and the 1.0.0 vocabularies, for tests that write or
//! check host contracts by hand. The product reads its catalogs from the
//! published JSON (`aq_playbooks_cli::catalog`); nothing in it builds one.
use aq_playbooks_model::{Registry, Schema};
use serde_json::Value;
use std::collections::BTreeSet;

pub fn strings(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).into()).collect()
}
pub fn object(fields: &[(&str, Schema)]) -> Schema {
    Schema::object(fields.iter().cloned().map(|(k, v)| (k.into(), v)).collect())
}
pub fn enumeration(values: &[&str]) -> Schema {
    Schema::String {
        max_length: 128,
        values: strings(values),
    }
}
pub fn text() -> Schema {
    Schema::string(4096)
}
pub fn revision() -> Schema {
    Schema::Integer {
        minimum: 0,
        maximum: i64::MAX,
    }
}
pub fn subject() -> Schema {
    object(&[("id", text()), ("revision", revision())])
}
pub fn result() -> Schema {
    object(&[("reference", text()), ("subject", subject())])
}
pub const CONTEXT_VIEWS: &[&str] = &[
    "work",
    "project",
    "execution_options",
    "evidence",
    "health_observations",
];
pub const WORK_KINDS: &[&str] = &[
    "implementation",
    "investigation",
    "planning",
    "review",
    "repair",
    "supervisor_authoring",
    "spec_ingestion",
    "fidelity_review",
    "onboarding",
];
pub const CHANGE_VARIANTS: &[&str] = &[
    "requirements",
    "priority",
    "dependencies",
    "execution_constraints",
    "hold",
    "release_hold",
];

pub fn assert_fixture_cases(registry: &Registry, fixtures: &Value) {
    for fixture in fixtures["cases"].as_array().unwrap() {
        let name = fixture["name"].as_str().unwrap();
        let contract = fixture["contract"].as_str().unwrap();
        let schema = match fixture["kind"].as_str().unwrap() {
            "input" => &registry.commands[contract].input,
            "output" => &registry.commands[contract].outcomes[fixture["outcome"].as_str().unwrap()],
            "event" => &registry.events[contract].payload,
            other => panic!("unknown fixture type {other}"),
        };
        assert_eq!(
            schema.validate(&fixture["value"]).is_ok(),
            fixture["valid"].as_bool().unwrap(),
            "{name}"
        );
    }
}
