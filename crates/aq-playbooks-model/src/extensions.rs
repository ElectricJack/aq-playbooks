use super::{ContractRef, EventEnvelope, ModelError, Registry, digest, validate_event};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

/// The contracts and authoring notes an installed plugin contributes. Package
/// versions are transport provenance and do not belong in this view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "ExtensionFields")]
pub struct CatalogExtension {
    name: String,
    registry: Registry,
    notes: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtensionFields {
    name: String,
    registry: Registry,
    notes: Value,
}

impl TryFrom<ExtensionFields> for CatalogExtension {
    type Error = ModelError;

    fn try_from(fields: ExtensionFields) -> Result<Self, Self::Error> {
        Self::new(fields.name, fields.registry, fields.notes)
    }
}

impl CatalogExtension {
    pub fn new(
        name: impl Into<String>,
        registry: Registry,
        notes: Value,
    ) -> Result<Self, ModelError> {
        let name = name.into();
        validation::check(&name, &registry, &notes)?;
        Ok(Self {
            name,
            registry,
            notes,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn notes(&self) -> &Value {
        &self.notes
    }

    /// Derive an F5 event from the host's intent identity and the plugin's
    /// zero-based response index. The plugin supplies only its event name
    /// and payload. The host supplies the remaining envelope fields.
    pub fn event_envelope(
        &self,
        intent_id: &str,
        index: usize,
        name: String,
        payload: Value,
        now_ms: i64,
    ) -> Result<EventEnvelope, ModelError> {
        let invalid =
            || ModelError::Invalid("invalid plugin event subject, revision or correlation".into());
        let contract = self
            .registry
            .events
            .get(&name)
            .ok_or_else(|| ModelError::Invalid("unknown plugin event".into()))?;
        let field = |path: &str| {
            path.split('.')
                .try_fold(&payload, |value, key| value.get(key))
        };
        let subject = contract
            .subject_field
            .as_deref()
            .and_then(field)
            .and_then(Value::as_str)
            .filter(|subject| !subject.is_empty())
            .ok_or_else(invalid)?;
        let revision = contract
            .revision_field
            .as_deref()
            .and_then(field)
            .and_then(Value::as_u64)
            .ok_or_else(invalid)?;
        let correlation = contract
            .correlation_fields
            .iter()
            .map(|path| {
                let value = field(path).and_then(Value::as_str).ok_or_else(invalid)?;
                Ok((path.clone(), value.to_owned()))
            })
            .collect::<Result<_, ModelError>>()?;
        let event = EventEnvelope {
            producer: format!("ext.{}", self.name),
            event_id: digest(&(intent_id, index))?,
            contract: ContractRef {
                name,
                fingerprint: contract.fingerprint()?,
            },
            scope: "project".into(),
            correlation,
            subject: Some(subject.to_owned()),
            revision: Some(revision),
            causation: Some(intent_id.to_owned()),
            occurred_at_ms: Some(now_ms),
            observed_at_ms: Some(now_ms),
            payload,
        };
        validate_event(&event, &self.registry)?;
        Ok(event)
    }
}

mod validation;

impl Registry {
    /// Compose an independent view without mutating the base or changing any
    /// contract. A command or event name must belong to one catalog only.
    pub fn with_extensions(&self, extensions: &[CatalogExtension]) -> Result<Self, ModelError> {
        let mut registry = self.clone();
        let mut names = BTreeSet::new();
        for extension in extensions {
            if !names.insert(extension.name()) {
                return Err(ModelError::Invalid(format!(
                    "duplicate extension {}",
                    extension.name()
                )));
            }
            registry.add_extension(extension)?;
        }
        Ok(registry)
    }

    fn add_extension(&mut self, extension: &CatalogExtension) -> Result<(), ModelError> {
        let incoming = extension.registry();
        if let Some(name) = incoming
            .commands
            .keys()
            .chain(incoming.events.keys())
            .find(|name| self.commands.contains_key(*name) || self.events.contains_key(*name))
        {
            return Err(ModelError::Invalid(format!(
                "extension {}: duplicate registration {name}",
                extension.name()
            )));
        }
        self.commands.extend(incoming.commands.clone());
        self.events.extend(incoming.events.clone());
        Ok(())
    }
}
