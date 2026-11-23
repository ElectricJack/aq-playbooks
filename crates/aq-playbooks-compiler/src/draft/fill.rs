//! Fill the host-pinned playbook fields and source locations.
use super::contracts::{capability_faults, resolve_registry};
use super::*;
/// Builds the compile input: identities from the catalog, the header and
/// definitions from the record and facts.
pub fn fill(ws: &Workspace) -> Result<CompileInput, Vec<Diagnostic>> {
    let draft = parse_draft(ws)?;
    let record = &ws.record;
    let (registry, capabilities) = resolve_registry(ws, &draft)?;
    let mut faults = capability_faults(&draft, &registry, record);
    faults.extend(identity_faults(&draft, record, &capabilities));
    let (source, header_lines) = source_text(
        record,
        &draft.entries.keys().cloned().collect(),
        &ws.facts.keys().cloned().collect(),
        &ws.markdown,
    );
    let source_map = source_locations(
        &draft,
        ws.markdown.lines().count(),
        header_lines,
        &mut faults,
    );
    let definitions = capture_definitions(ws, &mut faults);
    let steps = resolve_steps(draft.steps, &registry, &mut faults);
    let triggers = resolve_triggers(draft.triggers, &registry, &mut faults);
    if !faults.is_empty() {
        return Err(faults);
    }
    Ok(CompileInput {
        source,
        proposal: Proposal {
            playbook: Playbook {
                id: record.binding.clone(),
                purpose: draft.purpose.unwrap_or_else(|| record.purpose.clone()),
                scope: record.scope.clone(),
                entries: draft.entries,
                steps,
                triggers,
                limits: draft.limits.unwrap_or_default(),
                required_capabilities: capabilities,
            },
            source_map,
        },
        registry,
        dependencies: BTreeMap::new(),
        definitions,
        compiler: COMPILER_VERSION.into(),
    })
}

fn parse_draft(ws: &Workspace) -> Result<Draft, Vec<Diagnostic>> {
    // Bounded first: everything after this grows faster than the draft.
    let steps = ws
        .draft
        .get("steps")
        .and_then(Value::as_object)
        .map_or(0, |m| m.len());
    let bytes = serde_json::to_string(&ws.draft).map_or(usize::MAX, |t| t.len());
    let limits = draft_limits(steps, bytes);
    if !limits.is_empty() {
        return Err(limits);
    }
    let names = contract_positions(&ws.draft);
    if !names.is_empty() {
        return Err(names);
    }
    let pretty = serde_json::to_string_pretty(&ws.draft).unwrap_or_default();
    parse(DRAFT_FILE, &pretty, false, Some(&ws.draft)).map_err(|d| vec![d])
}

fn identity_faults(
    draft: &Draft,
    record: &SourceRecord,
    capabilities: &BTreeSet<String>,
) -> Vec<Diagnostic> {
    let mut faults = vec![];
    let identity = |field: &str, found: String, expected: String, from: &str| {
        Diagnostic::error(
            "draft_identity",
            &at(DRAFT_FILE, field),
            format!(
                "{field}: the draft says {found}, but {from} is {expected}. Leave {field} out: the tool writes it."
            ),
        )
    };
    if let Some(id) = draft.id.as_ref().filter(|id| **id != record.binding) {
        faults.push(identity(
            "id",
            format!("{id:?}"),
            format!("{:?}", record.binding),
            "this source's playbook id (record.json binding)",
        ));
    }
    if let Some(scope) = draft.scope.as_ref().filter(|s| **s != record.scope) {
        faults.push(identity(
            "scope",
            format!("{scope:?}"),
            format!("{:?}", record.scope),
            "this source's scope",
        ));
    }
    if let Some(c) = draft
        .required_capabilities
        .as_ref()
        .filter(|c| *c != capabilities)
    {
        faults.push(identity(
            "required_capabilities",
            list(c),
            list(capabilities),
            "what the commands it uses require",
        ));
    }
    faults
}

fn source_locations(
    draft: &Draft,
    markdown_lines: usize,
    header_lines: u32,
    faults: &mut Vec<Diagnostic>,
) -> BTreeMap<String, SourceLocation> {
    let mut source_map = BTreeMap::new();
    for (step, line) in &draft.source_map {
        let path = format!("source_map.{step}");
        if !draft.steps.contains_key(step) {
            faults.push(Diagnostic::error(
                "source_map",
                &at(DRAFT_FILE, &path),
                format!(
                    "{path}: there is no step {step:?}; map a step name (one of: {}) to a line of source.md.",
                    list(&draft.steps.keys().cloned().collect())
                ),
            ));
        } else if *line == 0 || *line as usize > markdown_lines {
            faults.push(Diagnostic::error(
                "source_map",
                &at(DRAFT_FILE, &path),
                format!(
                    "{path}: line {line} is not a line of source.md, which has lines 1 to {markdown_lines}."
                ),
            ));
        } else {
            source_map.insert(
                format!("steps.{step}"),
                SourceLocation {
                    line: line + header_lines,
                    column: 1,
                },
            );
        }
    }
    source_map
}

fn capture_definitions(
    ws: &Workspace,
    faults: &mut Vec<Diagnostic>,
) -> BTreeMap<String, CapturedDefinition> {
    let mut definitions = BTreeMap::new();
    for (name, value) in &ws.facts {
        match digest(value) {
            Ok(digest) => {
                definitions.insert(
                    name.clone(),
                    CapturedDefinition {
                        version: ws.record.facts_version.clone(),
                        digest,
                        value: value.clone(),
                    },
                );
            }
            Err(e) => faults.push(Diagnostic::error(
                "facts_format",
                &at(FACTS_FILE, name),
                format!("{FACTS_FILE} {name}: {e}."),
            )),
        }
    }
    definitions
}

fn reference(
    name: &str,
    fingerprint: Result<Hash, ModelError>,
    faults: &mut Vec<Diagnostic>,
) -> ContractRef {
    match fingerprint {
        Ok(fingerprint) => ContractRef {
            name: name.into(),
            fingerprint,
        },
        Err(e) => {
            faults.push(Diagnostic::error(
                "catalog",
                CATALOG_FILE,
                format!("{CATALOG_FILE}: contract {name} has no identity: {e}."),
            ));
            ContractRef {
                name: name.into(),
                fingerprint: String::new(),
            }
        }
    }
}

fn resolve_steps(
    draft_steps: BTreeMap<String, DraftStep>,
    registry: &Registry,
    faults: &mut Vec<Diagnostic>,
) -> BTreeMap<String, Step> {
    let mut steps = BTreeMap::new();
    for (name, step) in draft_steps {
        let mut op = step.op;
        match &mut op {
            Operation::Invoke { command, .. } => {
                *command = reference(
                    &command.name,
                    registry.commands[&command.name].fingerprint(),
                    faults,
                );
            }
            Operation::Wait {
                event: Some(event), ..
            } => {
                *event = reference(
                    &event.name,
                    registry.events[&event.name].fingerprint(),
                    faults,
                );
            }
            _ => {}
        }
        steps.insert(
            name,
            Step {
                op,
                edges: step.edges,
                on_error: step.on_error,
            },
        );
    }
    steps
}
fn resolve_triggers(
    draft_triggers: Vec<DraftTrigger>,
    registry: &Registry,
    faults: &mut Vec<Diagnostic>,
) -> Vec<Trigger> {
    draft_triggers
        .into_iter()
        .map(|t| Trigger {
            event: reference(&t.event, registry.events[&t.event].fingerprint(), faults),
            id: t.id,
            entry: t.entry,
            input: t.input,
        })
        .collect()
}
