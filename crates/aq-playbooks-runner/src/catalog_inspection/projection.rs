use super::*;
use crate::Activation;
use aq_playbooks_model::{Artifact, CallableDependency, DependencySelection};

impl From<&CallableDependency> for CatalogDependency {
    fn from(dep: &CallableDependency) -> Self {
        Self {
            binding: dep.binding.clone(),
            reference_artifact: dep.artifact.clone(),
            selection: match dep.selection {
                DependencySelection::Exact => CatalogSelection::Exact,
                DependencySelection::Catalog { .. } => CatalogSelection::Catalog,
            },
        }
    }
}

impl CatalogItem {
    fn definition(key: &str, artifact: &Artifact) -> Self {
        let content = &artifact.content;
        let purpose = &content.playbook.purpose;
        let mut end = purpose.len().min(4096);
        while !purpose.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            key: key.to_owned(),
            record: CatalogRecord::Definition(DefinitionMetadata {
                artifact: artifact.digest.clone(),
                executable_fingerprint: artifact.executable_fingerprint.clone(),
                source_digest: content.source.digest.clone(),
                name: content.playbook.id.clone(),
                scope: content.playbook.scope.clone(),
                compiler: content.compiler.clone(),
                purpose: purpose[..end].into(),
                purpose_truncated: end < purpose.len(),
                entries: content.playbook.entries.keys().cloned().collect(),
                dependencies: content
                    .dependencies
                    .iter()
                    .map(|(key, dep)| (key.clone(), dep.into()))
                    .collect(),
            }),
        }
    }

    fn activation(binding: &str, activation: &Activation, denied: bool) -> Self {
        Self {
            key: activation_key(binding),
            record: CatalogRecord::Activation(ActivationMetadata {
                binding: binding.to_owned(),
                artifact: activation.artifact.clone(),
                revision: activation.revision,
                controls: CatalogControls {
                    calls_enabled: activation.controls.calls_enabled,
                    event_starts_enabled: activation.controls.event_starts_enabled,
                    execution_enabled: activation.controls.execution_enabled,
                },
                artifact_execution_denied: denied,
            }),
        }
    }
}

pub fn project(state: &State) -> Result<CatalogProjection, CatalogError> {
    let definitions = state
        .artifacts
        .iter()
        .map(|(key, artifact)| CatalogItem::definition(key, artifact));
    let activations = state.bindings.iter().map(|(binding, activation)| {
        CatalogItem::activation(
            binding,
            activation,
            state.execution_denied.contains(&activation.artifact),
        )
    });
    let mut rows = CatalogProjection::new();
    for item in definitions.chain(activations) {
        item.validate()?;
        if rows.insert((item.kind(), item.key.clone()), item).is_some() {
            return Err(CatalogError::Unsupported);
        }
    }
    Ok(rows)
}
