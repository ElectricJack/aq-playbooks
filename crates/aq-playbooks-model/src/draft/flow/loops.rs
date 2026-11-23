//! Identify the step and unbound items reached after a failure in a loop body.
use super::*;

fn body_steps<'a>(
    site: &str,
    body: &'a str,
    parts: &BTreeMap<&'a str, Parts<'a>>,
) -> BTreeSet<&'a str> {
    let mut inside = BTreeSet::new();
    let mut pending = vec![body];
    while let Some(name) = pending.pop() {
        if name == site || !inside.insert(name) {
            continue;
        }
        if let Some(p) = parts.get(name) {
            pending.extend(p.exits.iter().map(|e| e.to));
        }
    }
    inside
}
impl<'a> Check<'a> {
    /// For each step inside a `for_each` body, where a failure there leads:
    /// the runner hands it to the innermost loop, not to the step's own
    /// `on_error`. `(target, item unbound there, way)`.
    pub(super) fn loop_failures(&self, parts: &BTreeMap<&'a str, Parts<'a>>) -> LoopFailures<'a> {
        let mut failures: BTreeMap<&str, Vec<_>> = BTreeMap::new();
        for (site, step) in &self.content.playbook.steps {
            let Operation::ForEach {
                item,
                body,
                on_failure,
                ..
            } = &step.op
            else {
                continue;
            };
            let target = match on_failure {
                // The next item, or the end of the loop.
                IterationFailure::Continue => Some((site.as_str(), None)),
                IterationFailure::Stop => {
                    step.on_error.as_deref().map(|to| (to, Some(item.as_str())))
                }
            };
            let Some((to, own)) = target else {
                continue;
            };
            let inside = body_steps(site, body, parts);
            // The failure is this loop's when no loop inside it is running,
            // so the items of those loops are not bound either.
            let unbinds: Vec<&str> = inside
                .iter()
                .filter_map(|name| parts.get(name)?.own_item)
                .chain(own)
                .collect();
            for name in inside {
                failures.entry(name).or_default().push((
                    to,
                    unbinds.clone(),
                    format!("a failure in the body of for_each `{site}`"),
                ));
            }
        }
        failures
    }
}
