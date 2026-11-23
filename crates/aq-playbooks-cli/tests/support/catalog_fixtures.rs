use aq_playbooks_model::{Hash, Registry};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(super) fn pretty(value: &impl serde::Serialize) -> String {
    // A Value orders keys, as every published bundle does.
    let value = serde_json::to_value(value).unwrap();
    serde_json::to_string_pretty(&value).unwrap() + "\n"
}

pub(super) fn fingerprints<'a, T: 'a>(
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

fn validate_fixture(registry: &Registry, fixture: &Value) -> Option<String> {
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
    valid.then(|| match kind {
        "output" => format!("{contract}.{}", fixture["outcome"].as_str().unwrap()),
        "input" => format!("{contract}.input"),
        _ => contract.to_string(),
    })
}

pub(super) fn validate_fixtures(registry: &Registry, fixtures: &Value) -> BTreeSet<String> {
    fixtures["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|case| validate_fixture(registry, case))
        .collect()
}
