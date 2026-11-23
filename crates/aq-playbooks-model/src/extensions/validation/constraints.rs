use crate::{CommandContract, EventContract, MAX_ARTIFACT_BYTES, Schema};
use std::collections::BTreeSet;

pub(super) fn host_refusal(schema: &Schema) -> bool {
    let Schema::Object {
        properties,
        required,
    } = schema
    else {
        return false;
    };
    let valid_reason = matches!(properties.get("reason"), Some(Schema::String { max_length, values })
        if *max_length >= "invalid_plugin_response".len() && values.is_empty());
    properties.len() == 1 && only(required, "reason") && valid_reason
}

pub(super) fn valid_command_bounds(command: &CommandContract) -> bool {
    (1..=64).contains(&command.version.len())
        && command.outcomes.len() <= 64
        && (1..=MAX_ARTIFACT_BYTES).contains(&command.max_payload_bytes)
        && (2..=31_536_000_000).contains(&command.deadline_ms)
}

pub(super) fn valid_event_bounds(event: &EventContract) -> bool {
    !event.version.is_empty()
        && (1..=MAX_ARTIFACT_BYTES).contains(&event.max_payload_bytes)
        && event.retention_ms > 0
}

pub(super) fn valid_event_fields(event: &EventContract) -> bool {
    let field = |path: &str| required_field(&event.payload, path);
    let string = |path: &str| matches!(field(path), Some(Schema::String { .. }));
    let revision = event.revision_field.as_deref().and_then(field);
    event.subject_field.as_deref().is_some_and(string)
        && matches!(revision, Some(Schema::Integer { minimum,.. }) if *minimum>=0)
        && event.correlation_fields.iter().all(|path| string(path))
}

pub(super) fn only(values: &BTreeSet<String>, value: &str) -> bool {
    values.len() == 1 && values.contains(value)
}

fn required_field<'a>(schema: &'a Schema, path: &str) -> Option<&'a Schema> {
    path.split('.')
        .try_fold(schema, |schema, field| match schema {
            Schema::Object {
                properties,
                required,
            } if required.contains(field) => properties.get(field),
            _ => None,
        })
}
