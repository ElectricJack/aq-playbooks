use crate::{CommandContract, EventContract, ModelError, Recovery, Registry, Schema, valid_name};
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) fn check(name: &str, registry: &Registry, notes: &Value) -> Result<(), ModelError> {
    let namespace = Namespace::new(name)?;
    for (key, command) in &registry.commands {
        namespace.command(key, command)?;
    }
    for (key, event) in &registry.events {
        if registry.commands.contains_key(key) {
            return Err(ModelError::Invalid(format!(
                "extension {name}: duplicate registration {key}"
            )));
        }
        namespace.event(key, event)?;
    }
    if !valid_notes(notes, registry) {
        return Err(ModelError::Invalid(format!(
            "extension {name}: catalog notes must describe its own registered commands and events"
        )));
    }
    Ok(())
}

fn valid_notes(notes: &Value, registry: &Registry) -> bool {
    notes.is_object()
        && known_notes(notes.get("commands"), &registry.commands)
        && known_notes(notes.get("events"), &registry.events)
}
fn known_notes<T>(notes: Option<&Value>, contracts: &BTreeMap<String, T>) -> bool {
    notes.is_none_or(|map| {
        map.as_object().is_some_and(|entries| {
            entries
                .iter()
                .all(|(key, note)| note.is_object() && contracts.contains_key(key))
        })
    })
}

fn valid_extension_name(name: &str) -> bool {
    let identifier = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_';
    (2..=32).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name.bytes().all(identifier)
}

struct Namespace<'a> {
    name: &'a str,
    capability: String,
    prefix: String,
}
impl<'a> Namespace<'a> {
    fn new(name: &'a str) -> Result<Self, ModelError> {
        if !valid_extension_name(name) {
            return Err(ModelError::Invalid(format!(
                "invalid extension name {name:?}"
            )));
        }
        let capability = format!("ext.{name}");
        let prefix = format!("{capability}.");
        Ok(Self {
            name,
            capability,
            prefix,
        })
    }

    fn registration(&self, kind: &str, key: &str, name: &str) -> Result<(), ModelError> {
        let owned = name
            .strip_prefix(&self.prefix)
            .is_some_and(|suffix| !suffix.is_empty())
            && valid_name(name);
        if name != key || !owned {
            return Err(ModelError::Invalid(format!(
                "extension {}: {kind} {key:?} must have a matching name under {}",
                self.name, self.prefix
            )));
        }
        Ok(())
    }

    fn command(&self, key: &str, command: &CommandContract) -> Result<(), ModelError> {
        self.registration("command", key, &command.name)?;
        let error = |message: &str| {
            ModelError::Invalid(format!(
                "extension {}: command {key:?} {message}",
                self.name
            ))
        };
        if !command.outcomes.get("refused").is_some_and(host_refusal) {
            return Err(error(
                "requires refused {reason: string} with open values that admit host refusals",
            ));
        }
        if !self.command_access(command) {
            return Err(error(&format!(
                "requires keyed_idempotent recovery, project scope and capability {}",
                self.capability
            )));
        }
        if command.asynchronous.is_some() || command.cancellation {
            return Err(error("must be synchronous and cannot be cancelled"));
        }
        if !valid_command_bounds(command) {
            return Err(error("has invalid version or resource bounds"));
        }
        command.input.validate_definition()?;
        command
            .outcomes
            .values()
            .try_for_each(Schema::validate_definition)
    }

    fn command_access(&self, command: &CommandContract) -> bool {
        command.recovery == Recovery::KeyedIdempotent
            && only(&command.scopes, "project")
            && only(&command.capabilities, &self.capability)
    }

    fn event(&self, key: &str, event: &EventContract) -> Result<(), ModelError> {
        self.registration("event", key, &event.name)?;
        let error = |message: &str| {
            ModelError::Invalid(format!("extension {}: event {key:?} {message}", self.name))
        };
        if !only(&event.trusted_producers, &self.capability) || !only(&event.scopes, "project") {
            return Err(error(&format!(
                "requires project scope and trusted producer {}",
                self.capability
            )));
        }
        if !valid_event_bounds(event) {
            return Err(error("has invalid version or resource bounds"));
        }
        event.payload.validate_definition()?;
        if !valid_event_fields(event) {
            return Err(error(
                "requires a string subject, non-negative integer revision and string correlation fields in its payload",
            ));
        }
        Ok(())
    }
}

mod constraints;
use constraints::{
    host_refusal, only, valid_command_bounds, valid_event_bounds, valid_event_fields,
};
