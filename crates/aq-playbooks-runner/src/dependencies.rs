//! Resolve executable dependency manifests against the accepting catalog revision.
use super::*;

type Manifest = BTreeMap<String, String>;

pub(super) fn resolve(state: &State, artifact: &str, authority: &Authority) -> Result<Manifest> {
    resolve_at(state, artifact, authority, state.catalog_revision)
}

pub(super) fn resolve_at(
    state: &State,
    artifact: &str,
    authority: &Authority,
    revision: u64,
) -> Result<Manifest> {
    let mut traversal = Resolver {
        state,
        authority,
        bindings: bindings_at(state, revision),
        stack: BTreeSet::new(),
        manifest: Manifest::new(),
    };
    traversal.visit(artifact)?;
    Ok(traversal.manifest)
}

fn bindings_at(state: &State, revision: u64) -> &BTreeMap<String, Activation> {
    state
        .catalog_history
        .get(&revision)
        .unwrap_or(&state.bindings)
}

struct Resolver<'a> {
    state: &'a State,
    authority: &'a Authority,
    bindings: &'a BTreeMap<String, Activation>,
    stack: BTreeSet<String>,
    manifest: Manifest,
}

impl Resolver<'_> {
    fn visit(&mut self, id: &str) -> Result<()> {
        if !self.stack.insert(id.into()) {
            return Err(Error::Refused("recursive dependency".into()));
        }
        let artifact = self
            .state
            .artifacts
            .get(id)
            .ok_or_else(|| Error::NotFound(id.into()))?;
        artifact.verify()?;
        check_authority(artifact, self.authority)?;
        for (name, dependency) in &artifact.content.dependencies {
            let selected = self.select(dependency)?;
            self.check_target(&selected, dependency)?;
            self.manifest
                .insert(format!("{id}/{name}"), selected.clone());
            self.visit(&selected)?;
        }
        self.stack.remove(id);
        Ok(())
    }

    fn select(&self, dependency: &CallableDependency) -> Result<String> {
        let DependencySelection::Catalog { allowed_artifacts } = &dependency.selection else {
            return Ok(dependency.artifact.clone());
        };
        let binding = self
            .bindings
            .get(&dependency.binding)
            .ok_or_else(|| Error::NotFound(dependency.binding.clone()))?;
        if !binding.controls.calls_enabled {
            return Err(Error::Refused("dependency new calls disabled".into()));
        }
        if !allowed_artifacts.is_empty() && !allowed_artifacts.contains(&binding.artifact) {
            return Err(Error::Refused(
                "catalog target outside allowed artifacts".into(),
            ));
        }
        Ok(binding.artifact.clone())
    }

    fn check_target(&self, selected: &str, dependency: &CallableDependency) -> Result<()> {
        let child = self
            .state
            .artifacts
            .get(selected)
            .ok_or_else(|| Error::NotFound(selected.into()))?;
        let playbook = &child.content.playbook;
        if !execution_allowed(
            self.state,
            selected,
            self.authority,
            &playbook.required_capabilities,
        )? {
            return Err(Error::Refused(
                "dependency execution authority revoked".into(),
            ));
        }
        if dependency.scope != self.authority.scope
            || !dependency
                .capabilities
                .is_subset(&self.authority.capabilities)
        {
            return Err(Error::Refused("dependency authority".into()));
        }
        for (entry, interface) in &dependency.interfaces {
            let actual = playbook
                .entries
                .get(entry)
                .ok_or_else(|| Error::Refused("missing child interface".into()))?;
            if !interface_compatible(actual, interface) {
                return Err(Error::Refused("incompatible child interface".into()));
            }
        }
        Ok(())
    }
}

/// Determine protection from the exact pinned execution closure before any input
/// is persisted. Catalog provenance that is not selected does not grant or deny
/// execution; known exact/transitive selections are followed even for blocked
/// event destinations whose immutable bytes are only partially installed.
pub(super) fn ensure_admission_protection(
    state: &State,
    artifact: &str,
    manifest: &Manifest,
    revision: u64,
) -> Result<()> {
    if state.protection_available {
        return Ok(());
    }
    let bindings = bindings_at(state, revision);
    let mut pending = vec![artifact.to_owned()];
    pending.extend(manifest.values().cloned());
    let mut visited = BTreeSet::new();
    while let Some(hash) = pending.pop() {
        if !visited.insert(hash.clone()) {
            continue;
        }
        let Some(artifact) = state.artifacts.get(&hash) else {
            continue;
        };
        if artifact
            .content
            .registry
            .commands
            .values()
            .any(sensitive_command)
        {
            return Err(Error::Refused(
                "protected-value storage unavailable for admitted workflow inputs/results".into(),
            ));
        }
        for (name, dependency) in &artifact.content.dependencies {
            let selected = manifest
                .get(&format!("{hash}/{name}"))
                .cloned()
                .unwrap_or_else(|| fallback(bindings, dependency).into());
            pending.push(selected);
        }
    }
    Ok(())
}

fn sensitive_command(command: &CommandContract) -> bool {
    !command.sensitive_input_fields.is_empty() || !command.sensitive_output_fields.is_empty()
}

fn fallback<'a>(
    bindings: &'a BTreeMap<String, Activation>,
    dependency: &'a CallableDependency,
) -> &'a str {
    match dependency.selection {
        DependencySelection::Exact => &dependency.artifact,
        DependencySelection::Catalog { .. } => bindings
            .get(&dependency.binding)
            .map_or(&dependency.artifact, |binding| &binding.artifact),
    }
}
