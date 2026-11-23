//! Resolve the contracts and capabilities requested by a draft.
use super::*;
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let here = row[j + 1];
            row[j + 1] = (previous + usize::from(ca != *cb))
                .min(row[j] + 1)
                .min(here + 1);
            previous = here;
        }
    }
    row[b.len()]
}

/// Refuses a name the source may not use, listing the ones it may, closest first.
fn allowed_name(
    kind: &str,
    name: &str,
    allowed: &BTreeSet<&String>,
    path: &str,
) -> Option<Diagnostic> {
    if allowed.contains(&name.to_string()) {
        return None;
    }
    let mut names: Vec<&&String> = allowed.iter().collect();
    names.sort_by_key(|n| (distance(name, n), (**n).clone()));
    let choice = if names.is_empty() {
        format!("This source may use no {kind}s.")
    } else {
        format!(
            "Use one of the {kind}s this source may use (closest first): {}.",
            names
                .iter()
                .map(|n| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Some(Diagnostic::error(
        &format!("unknown_{kind}"),
        &at(DRAFT_FILE, path),
        format!(
            "{path}: {name:?} is not {} {kind} this source may use. {choice}",
            if kind.starts_with('e') { "an" } else { "a" }
        ),
    ))
}

pub(super) fn resolve_registry(
    ws: &Workspace,
    draft: &Draft,
) -> Result<(Registry, BTreeSet<String>), Vec<Diagnostic>> {
    let record = &ws.record;
    let mut faults = host_catalog_faults(ws);
    let commands_allowed: BTreeSet<&String> = record
        .commands
        .iter()
        .filter(|n| ws.catalog.commands.contains_key(*n))
        .collect();
    let events_allowed: BTreeSet<&String> = record
        .events
        .iter()
        .filter(|n| ws.catalog.events.contains_key(*n))
        .collect();
    let mut commands = BTreeSet::new();
    let mut events = BTreeSet::new();
    for (name, step) in &draft.steps {
        match &step.op {
            Operation::Invoke { command, .. } => {
                let command = &command.name;
                let path = format!("steps.{name}.op.command");
                faults.extend(allowed_name("command", command, &commands_allowed, &path));
                commands.insert(command.clone());
            }
            Operation::Wait {
                event: Some(event), ..
            } => {
                let event = &event.name;
                let path = format!("steps.{name}.op.event");
                faults.extend(allowed_name("event", event, &events_allowed, &path));
                events.insert(event.clone());
            }
            Operation::Call { dependency, .. } => {
                let path = format!("steps.{name}.op");
                faults.push(Diagnostic::error(
                    "call",
                    &at(DRAFT_FILE, &path),
                    format!(
                        "{path}: calls playbook {dependency:?}, but this source is offered no callable playbooks. Use the commands it may use instead: {}.",
                        list(&commands_allowed.iter().map(|n| (*n).clone()).collect())
                    ),
                ));
            }
            _ => {}
        }
    }
    for (i, t) in draft.triggers.iter().enumerate() {
        let path = format!("triggers[{i}].event");
        faults.extend(allowed_name("event", &t.event, &events_allowed, &path));
        events.insert(t.event.clone());
    }
    if !faults.is_empty() {
        return Err(faults);
    }
    retain_registry(&ws.catalog, &commands, &events)
        .map_err(|e| vec![Diagnostic::error("catalog", CATALOG_FILE, format!("{e}."))])
}

fn host_catalog_faults(ws: &Workspace) -> Vec<Diagnostic> {
    let record = &ws.record;
    let mut faults = Vec::new();
    // A name the record allows that the catalog lacks: the host's own files
    // disagree, which no draft can fix.
    for (kind, missing) in [
        (
            "command",
            record
                .commands
                .iter()
                .filter(|n| !ws.catalog.commands.contains_key(*n))
                .collect::<Vec<_>>(),
        ),
        (
            "event",
            record
                .events
                .iter()
                .filter(|n| !ws.catalog.events.contains_key(*n))
                .collect::<Vec<_>>(),
        ),
    ] {
        for name in missing {
            faults.push(Diagnostic::error(
                "catalog",
                CATALOG_FILE,
                format!(
                    "{RECORD_FILE} allows the {kind} {name}, but {CATALOG_FILE} has no contract for it: the host's files disagree, and no draft can fix that. Give up with `aq-playbooks unable --file {CATALOG_FILE} \"<this fault>\"`."
                ),
            ));
        }
    }
    faults
}
fn command_capabilities(registry: &Registry, command: &str) -> BTreeSet<String> {
    let c = &registry.commands[command];
    let mut needed = c.capabilities.clone();
    if let Some(a) = &c.asynchronous {
        needed.extend(
            registry.commands[&a.reconciliation_command]
                .capabilities
                .clone(),
        );
    }
    needed
}
pub(super) fn capability_faults(
    draft: &Draft,
    registry: &Registry,
    record: &SourceRecord,
) -> Vec<Diagnostic> {
    let mut faults = vec![];
    // Each invoked command, with what its asynchronous completion adds, may
    // need only capabilities the record allows.
    for (name, step) in &draft.steps {
        if let Operation::Invoke { command, .. } = &step.op {
            let command = &command.name;
            let needed = command_capabilities(registry, command);
            let missing: BTreeSet<String> =
                needed.difference(&record.capabilities).cloned().collect();
            if !missing.is_empty() {
                let path = format!("steps.{name}.op.command");
                faults.push(Diagnostic::error(
                    "capability",
                    &at(DRAFT_FILE, &path),
                    format!(
                        "{path}: {command} needs the capabilities {}, which this source may not use (it may use: {}). Use another command.",
                        list(&missing),
                        list(&record.capabilities)
                    ),
                ));
            }
        }
    }
    faults
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_offered_closest_first() {
        assert_eq!(distance("work.reed", "work.read"), 1);
        assert!(distance("work.reed", "work.route") > 1);
        let allowed = ["work.route".to_string(), "work.read".to_string()];
        let fault = allowed_name(
            "command",
            "work.reed",
            &allowed.iter().collect(),
            "steps.a.op.command",
        )
        .unwrap();
        assert!(fault.message.ends_with("work.read, work.route."));
        let fault = allowed_name("event", "x", &BTreeSet::new(), "triggers[0].event").unwrap();
        assert!(fault.message.ends_with("This source may use no events."));
    }
}
