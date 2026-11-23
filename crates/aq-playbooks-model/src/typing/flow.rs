//! Propagate narrowed outcome schemas along reachable control-flow edges.
use super::*;
use std::collections::{VecDeque, btree_map::Entry};

#[derive(Clone, PartialEq)]
pub(super) struct TypeState {
    pub(super) roots: Roots,
    untyped: BTreeSet<String>,
}

impl TypeState {
    fn merge(&mut self, other: &Self) -> bool {
        let before = self.clone();
        self.untyped.extend(other.untyped.iter().cloned());
        let keys: BTreeSet<_> = self
            .roots
            .keys()
            .chain(other.roots.keys())
            .cloned()
            .collect();
        for key in keys {
            let merged = self
                .roots
                .get(&key)
                .zip(other.roots.get(&key))
                .filter(|_| !self.untyped.contains(&key))
                .and_then(|(a, b)| merge_schema(a, b));
            match merged {
                Some(schema) => {
                    self.roots.insert(key, schema);
                }
                None => {
                    self.roots.remove(&key);
                    self.untyped.insert(key);
                }
            }
        }
        *self != before
    }

    fn narrow(&mut self, root: &str, schema: Schema) {
        self.untyped.remove(root);
        self.roots.insert(root.into(), schema);
    }
}

impl ArtifactContent {
    pub(super) fn outcome_contexts(
        &self,
        entry: &EntryPoint,
        roots: &Roots,
    ) -> BTreeMap<String, TypeState> {
        let start = &entry.start;
        let initial = TypeState {
            roots: roots.clone(),
            untyped: BTreeSet::new(),
        };
        let mut contexts = BTreeMap::from([(start.clone(), initial)]);
        let mut pending = VecDeque::from([start.clone()]);
        let mut queued = BTreeSet::from([start.clone()]);
        while let Some(name) = pending.pop_front() {
            queued.remove(&name);
            let Some(step) = self.playbook.steps.get(&name) else {
                continue;
            };
            let current = contexts[&name].clone();
            for (target, next) in self.next_contexts(step, &current) {
                let changed = match contexts.entry(target.clone()) {
                    Entry::Occupied(mut entry) => entry.get_mut().merge(&next),
                    Entry::Vacant(entry) => {
                        entry.insert(next);
                        true
                    }
                };
                if changed && queued.insert(target.clone()) {
                    pending.push_back(target);
                }
            }
        }
        contexts
    }

    fn next_contexts(&self, step: &Step, current: &TypeState) -> Vec<(String, TypeState)> {
        let mut targets = Vec::new();
        for (outcome, target) in &step.edges {
            let mut next = current.clone();
            if let Some((bind, schema)) = self.edge_binding(step, outcome) {
                next.narrow(bind, schema);
            }
            targets.push((target.clone(), next));
        }
        if let Some(target) = &step.on_error {
            targets.push((target.clone(), current.clone()));
        }
        if let Operation::ForEach { body, next, .. } = &step.op {
            targets.push((body.clone(), current.clone()));
            targets.push((next.clone(), current.clone()));
        }
        targets
    }

    fn edge_binding<'a>(&self, step: &'a Step, outcome: &str) -> Option<(&'a str, Schema)> {
        match &step.op {
            Operation::Invoke { command, bind, .. } => {
                let contract = self.registry.commands.get(&command.name)?;
                Some((bind, contract.outcomes.get(outcome)?.clone()))
            }
            Operation::Call {
                dependency,
                entry,
                mode: CallMode::Wait,
                bind,
                ..
            } => {
                let interface = self.interface(dependency, entry)?;
                Some((bind, interface.outcomes.get(outcome)?.clone()))
            }
            Operation::Join { bind, .. } if outcome == "completed" => {
                Some((bind, self.join_schema(step, true).ok()??))
            }
            _ => None,
        }
    }
}
