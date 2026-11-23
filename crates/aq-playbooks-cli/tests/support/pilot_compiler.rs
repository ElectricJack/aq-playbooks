use super::*;
pub struct PilotSpec<'a> {
    pub id: &'a str,
    pub input: Schema,
    pub output: Schema,
    pub start: &'a str,
}
pub fn build(
    spec: PilotSpec<'_>,
    registry: Registry,
    mut steps: BTreeMap<String, Step>,
    children: &[(&str, &Artifact)],
) -> anyhow::Result<Artifact> {
    let PilotSpec {
        id,
        input,
        output,
        start,
    } = spec;
    let (registry, mut caps) = step_registry(&registry, &steps)?;
    steps
        .entry("failure".into())
        .or_insert_with(|| ret("escalated", lit(Value::Null)));
    caps.extend(
        children
            .iter()
            .flat_map(|(_, a)| a.content.playbook.required_capabilities.iter().cloned()),
    );
    let playbook = Playbook {
        id: id.into(),
        purpose: format!("Synthetic {id} pilot"),
        scope: "project".into(),
        entries: BTreeMap::from([(
            "main".into(),
            EntryPoint {
                input,
                outcomes: BTreeMap::from([
                    ("success".into(), output),
                    ("escalated".into(), Schema::Null),
                ]),
                start: start.into(),
            },
        )]),
        steps,
        triggers: vec![],
        limits: Limits::default(),
        required_capabilities: caps,
    };
    let header = SourceHeader {
        format_version: 1,
        id: id.into(),
        scope: "project".into(),
        interfaces: strings(&["main"]),
        definitions: BTreeSet::new(),
    };
    let input = CompileInput {
        source: format!(
            "---aq-playbook\n{}\n---\n\n# {id}\n\nA synthetic bounded pilot. Domain services retain authority.\n",
            serde_json::to_string(&header)?
        ),
        proposal: Proposal {
            playbook,
            source_map: BTreeMap::new(),
        },
        registry,
        dependencies: child_dependencies(children),
        definitions: BTreeMap::new(),
        compiler: COMPILER_VERSION.into(),
    };
    compile(&input)
        .map_err(|d| anyhow::anyhow!("{id}: {}", serde_json::to_string_pretty(&d).unwrap()))
}
fn step_registry(
    registry: &Registry,
    steps: &BTreeMap<String, Step>,
) -> anyhow::Result<(Registry, BTreeSet<String>)> {
    let mut commands = BTreeSet::new();
    let mut events = BTreeSet::new();
    for step in steps.values() {
        match &step.op {
            Operation::Invoke { command, .. } => {
                commands.insert(command.name.clone());
            }
            Operation::Wait {
                event: Some(event), ..
            } => {
                events.insert(event.name.clone());
            }
            _ => {}
        }
    }
    aq_playbooks_compiler::draft::retain_registry(registry, &commands, &events)
        .map_err(anyhow::Error::msg)
}
fn child_dependencies(children: &[(&str, &Artifact)]) -> BTreeMap<String, CallableDependency> {
    children
        .iter()
        .map(|(name, a)| {
            (
                (*name).into(),
                CallableDependency {
                    selection: DependencySelection::Exact,
                    binding: (*name).into(),
                    artifact: a.digest.clone(),
                    interfaces: a.content.playbook.entries.clone(),
                    capabilities: a.content.playbook.required_capabilities.clone(),
                    scope: "project".into(),
                    dependencies: a
                        .content
                        .dependencies
                        .values()
                        .flat_map(|d| {
                            std::iter::once(d.artifact.clone()).chain(d.dependencies.clone())
                        })
                        .collect(),
                },
            )
        })
        .collect()
}
