//! Type checking for the operations reachable from one playbook entry.
use super::*;
mod flow;

type Roots = BTreeMap<String, Schema>;
type DeepExpressions = BTreeSet<(String, String)>;

pub(super) struct EntryTyping<'a> {
    artifact: &'a ArtifactContent,
    entry_name: &'a str,
    entry: &'a EntryPoint,
    too_deep: &'a DeepExpressions,
}

impl<'a> EntryTyping<'a> {
    pub(super) fn new(
        artifact: &'a ArtifactContent,
        entry_name: &'a str,
        entry: &'a EntryPoint,
        too_deep: &'a DeepExpressions,
    ) -> Self {
        Self {
            artifact,
            entry_name,
            entry,
            too_deep,
        }
    }

    pub(super) fn validate(&self, reachable: &BTreeSet<String>, issues: &mut Vec<ValidationIssue>) {
        let roots = self.initial_roots(reachable);
        let contexts = self.artifact.outcome_contexts(self.entry, &roots);
        for (name, step) in self.steps(reachable) {
            let local = contexts.get(name).map(|c| &c.roots).unwrap_or(&roots);
            let mut check = StepTyping {
                entry: self,
                roots: with_vars(local),
                path: format!("steps.{name}"),
                issues,
            };
            check.operation(&step.op);
        }
    }

    fn steps<'b>(
        &'b self,
        reachable: &'b BTreeSet<String>,
    ) -> impl Iterator<Item = (&'b String, &'b Step)> {
        reachable.iter().filter_map(|name| {
            self.artifact
                .playbook
                .steps
                .get(name)
                .map(|step| (name, step))
        })
    }

    fn initial_roots(&self, reachable: &BTreeSet<String>) -> Roots {
        let mut roots = BTreeMap::from([("input".into(), self.entry.input.clone())]);
        let definitions = self
            .artifact
            .definitions
            .iter()
            .map(|(k, d)| (k.clone(), d.value.clone()))
            .collect();
        if let Ok(schema) = literal_schema(&Value::Object(definitions)) {
            roots.insert("definitions".into(), schema);
        }
        for (_, step) in self.steps(reachable) {
            if let Some((binding, schema)) = self.binding(step) {
                roots.insert(binding.to_owned(), schema);
            }
        }
        self.iteration_roots(reachable, &mut roots);
        roots
    }

    fn iteration_roots(&self, reachable: &BTreeSet<String>, roots: &mut Roots) {
        // Nested iteration collections can depend on the enclosing item.
        // Resolve lexical items to a fixed point within the nesting limit.
        for _ in 0..=self.artifact.playbook.limits.max_iteration_depth {
            let old_count = roots.len();
            for (_, step) in self.steps(reachable) {
                if let Some((binding, schema)) = iteration_item(&step.op, roots) {
                    roots.entry(binding.to_owned()).or_insert(schema);
                }
            }
            if roots.len() == old_count {
                break;
            }
        }
    }

    fn binding<'b>(&self, step: &'b Step) -> Option<(&'b str, Schema)> {
        match &step.op {
            Operation::Invoke { command, bind, .. } => {
                let contract = self.artifact.registry.commands.get(&command.name)?;
                Some((bind, union_schemas(contract.outcomes.values())?))
            }
            Operation::Wait {
                event: Some(event),
                bind,
                ..
            } => Some((
                bind,
                self.artifact
                    .registry
                    .events
                    .get(&event.name)?
                    .payload
                    .clone(),
            )),
            Operation::Call {
                dependency,
                entry,
                mode: CallMode::Wait,
                bind,
                ..
            } => {
                let interface = self.artifact.interface(dependency, entry)?;
                Some((bind, union_schemas(interface.outcomes.values())?))
            }
            Operation::Call { bind, .. } => Some((
                bind,
                Schema::object(BTreeMap::from([("child".into(), Schema::string(128))])),
            )),
            Operation::Join { bind, .. } => {
                Some((bind, self.artifact.join_schema(step, false).ok()??))
            }
            _ => None,
        }
    }
}

fn iteration_item<'a>(op: &'a Operation, roots: &Roots) -> Option<(&'a str, Schema)> {
    let Operation::ForEach {
        collection, item, ..
    } = op
    else {
        return None;
    };
    let Schema::Array { items, .. } = infer_expr(collection, roots, 0, "collection").ok()? else {
        return None;
    };
    Some((item, *items))
}

fn with_vars(roots: &Roots) -> Roots {
    let properties = roots
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "input" | "definitions" | "vars"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut roots = roots.clone();
    roots.insert(
        "vars".into(),
        Schema::Object {
            properties,
            required: BTreeSet::new(),
        },
    );
    roots
}

impl ArtifactContent {
    fn interface(&self, dependency: &str, entry: &str) -> Option<&EntryPoint> {
        self.dependencies.get(dependency)?.interfaces.get(entry)
    }
}

struct StepTyping<'a, 'b> {
    entry: &'a EntryTyping<'b>,
    roots: Roots,
    path: String,
    issues: &'a mut Vec<ValidationIssue>,
}

impl StepTyping<'_, '_> {
    fn typed(&self, at: &str) -> bool {
        !self
            .entry
            .too_deep
            .contains(&(self.path.clone(), at.to_owned()))
    }
    fn check(&mut self, expr: &Expr, at: &str, target: &Schema) {
        if self.typed(at) {
            Compatibility::new(&self.roots, &self.path, self.issues).check(expr, target, at);
        }
    }
    fn operation(&mut self, op: &Operation) {
        let artifact = self.entry.artifact;
        match op {
            Operation::Invoke { command, input, .. } => {
                if let Some(contract) = artifact.registry.commands.get(&command.name) {
                    self.check(input, "input", &contract.input);
                }
            }
            Operation::Decide { predicate } => self.check(predicate, "predicate", &Schema::Boolean),
            Operation::Wait {
                correlation,
                minimum_revision,
                ..
            } => self.wait(correlation, minimum_revision.as_ref()),
            Operation::ForEach {
                collection,
                max_items,
                ..
            } => self.collection(collection, *max_items),
            Operation::Call {
                dependency,
                entry,
                input,
                ..
            } => {
                if let Some(interface) = artifact.interface(dependency, entry) {
                    self.check(input, "input", &interface.input);
                }
            }
            Operation::Join { members, .. } => self.join(members),
            Operation::Return { outcome, output } => self.return_value(outcome, output),
        }
    }
    fn wait(&mut self, correlation: &BTreeMap<String, Expr>, revision: Option<&Expr>) {
        for (key, expr) in correlation {
            self.check(expr, &format!("correlation.{key}"), &Schema::string(1024));
        }
        if let Some(expr) = revision {
            self.check(
                expr,
                "minimum_revision",
                &Schema::Integer {
                    minimum: 0,
                    maximum: i64::MAX,
                },
            );
        }
    }
    fn collection(&mut self, collection: &Expr, max_items: usize) {
        if !self.typed("collection") {
            return;
        }
        match infer_expr(collection, &self.roots, 0, "collection") {
            Ok(Schema::Array {
                max_items: bound, ..
            }) => {
                if bound > max_items {
                    issue(
                        self.issues,
                        "iteration_limit",
                        &self.path,
                        "foreach item bound does not cover collection schema",
                    );
                }
            }
            Ok(_) => issue(
                self.issues,
                "expression_type",
                &self.path,
                "foreach collection must be bounded array",
            ),
            Err(e) => type_issue(self.issues, &self.path, e),
        }
    }
    fn join(&mut self, members: &[Expr]) {
        for (i, member) in members.iter().enumerate() {
            let at = format!("members[{i}]");
            if self.typed(&at)
                && let Err(e) = infer_expr(member, &self.roots, 0, &at)
            {
                type_issue(self.issues, &self.path, e);
            }
        }
    }
    fn return_value(&mut self, outcome: &str, output: &Expr) {
        if let Some(target) = self.entry.entry.outcomes.get(outcome) {
            self.check(output, "output", target);
        } else {
            issue(
                self.issues,
                "return_outcome",
                &self.path,
                format!(
                    "outcome {outcome} not declared by entry {}",
                    self.entry.entry_name
                ),
            );
        }
    }
}
