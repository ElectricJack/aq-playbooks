//! Check that a route through the draft fits within the run's duration limit.
use super::*;

fn step_wait(step: &Step) -> u64 {
    match &step.op {
        Operation::Wait { deadline_ms, .. } | Operation::Join { deadline_ms, .. } => *deadline_ms,
        _ => 0,
    }
}

fn longest(
    playbook: &Playbook,
    name: &str,
    memo: &mut BTreeMap<String, (u64, Vec<String>)>,
    stack: &mut BTreeSet<String>,
) -> (u64, Vec<String>) {
    if let Some(found) = memo.get(name) {
        return found.clone();
    }
    let Some(step) = playbook.steps.get(name) else {
        return (0, vec![]);
    };
    if !stack.insert(name.to_string()) {
        return (0, vec![]);
    }
    let own = step_wait(step);
    let mut next: Vec<&String> = step.edges.values().chain(step.on_error.iter()).collect();
    if let Operation::ForEach { next: after, .. } = &step.op {
        next.push(after);
    }
    let mut best = (0, vec![]);
    for target in next {
        let found = longest(playbook, target, memo, stack);
        if found.0 > best.0 {
            best = found;
        }
    }
    stack.remove(name);
    let mut way = if own > 0 {
        vec![name.to_string()]
    } else {
        vec![]
    };
    way.extend(best.1);
    let found = (own.saturating_add(best.0), way);
    memo.insert(name.to_string(), found.clone());
    found
}
/// The longest wait on one way from each entry, adding the `deadline_ms` of
/// every wait and join on it (a `for_each` body is left out: it may run any
/// number of times). The run's own `limits.max_duration_ms` bounds the whole
/// run from its start, so a way whose waits reach it stops the run there.
pub(super) fn duration_faults(playbook: &Playbook) -> Vec<Diagnostic> {
    let limit = playbook.limits.max_duration_ms;
    let mut faults = vec![];
    for (entry, point) in &playbook.entries {
        let (total, way) = longest(
            playbook,
            &point.start,
            &mut BTreeMap::new(),
            &mut BTreeSet::new(),
        );
        if total >= limit && !way.is_empty() {
            let steps: Vec<String> = way
                .iter()
                .map(|s| {
                    let ms = step_wait(&playbook.steps[s]);
                    format!("`{s}` ({ms} ms)")
                })
                .collect();
            let default = if playbook.limits == Limits::default() {
                " (the default)"
            } else {
                ""
            };
            faults.push(Diagnostic::error(
                "deadline",
                "playbook.limits",
                format!(
                    "limits.max_duration_ms: a run from entry `{entry}` may wait {total} ms on one way, at {}, which reaches the playbook's duration limit of {limit} ms{default}. The limit covers the whole run from its start, so a run that takes that way stops there. Set `max_duration_ms` from the document, above {total} ms, or shorten the waits if the document asks for less.",
                    steps.join(", ")
                ),
            ));
        }
    }
    faults
}
