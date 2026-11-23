//! Step binding, edge, and operation validation during artifact installation.
use super::*;

pub(super) struct StepBindings {
    pub(super) roots: BTreeSet<String>,
    pub(super) too_deep: BTreeSet<(String, String)>,
    command_bindings: BTreeSet<String>,
    owners: BTreeMap<String, String>,
    iteration_items: BTreeSet<String>,
}

fn binding_name(op: &Operation) -> Option<&str> {
    match op {
        Operation::Invoke { bind, .. }
        | Operation::Wait { bind, .. }
        | Operation::Call { bind, .. }
        | Operation::Join { bind, .. } => Some(bind),
        _ => None,
    }
}

impl StepBindings {
    fn new(playbook: &Playbook) -> Self {
        let command_bindings: BTreeSet<String> = playbook
            .steps
            .values()
            .filter_map(|step| binding_name(&step.op))
            .map(str::to_owned)
            .collect();
        // Let bindings may not shadow any step binding or iteration item in the playbook.
        let roots = command_bindings
            .iter()
            .cloned()
            .chain(playbook.steps.values().filter_map(|step| match &step.op {
                Operation::ForEach { item, .. } => Some(item.clone()),
                _ => None,
            }))
            .collect();
        Self {
            roots,
            command_bindings,
            too_deep: BTreeSet::new(),
            owners: BTreeMap::new(),
            iteration_items: BTreeSet::new(),
        }
    }
    fn record_binding(&mut self, name: &str, bind: &str) -> Option<String> {
        if let Some(other) = self.owners.get(bind) {
            return Some(format!(
                "bindings must be valid distinct roots, excluding reserved roots: step `{name}` binds `{bind}`, which step `{other}` already binds"
            ));
        }
        self.owners.insert(bind.to_owned(), name.to_owned());
        (!valid_binding_name(bind))
            .then(|| "bindings must be valid distinct roots, excluding reserved roots".to_owned())
    }
    fn record_item(&mut self, name: &str) -> bool {
        let allowed = valid_binding_name(name) && !self.command_bindings.contains(name);
        allowed && self.iteration_items.insert(name.to_owned())
    }
}
fn valid_binding_name(name: &str) -> bool {
    valid_name(name) && !RESERVED_ROOTS.contains(&name)
}

impl ArtifactContent {
    pub(super) fn validate_steps(&self, issues: &mut Vec<ValidationIssue>) -> StepBindings {
        let mut bindings = StepBindings::new(&self.playbook);
        for (name, step) in &self.playbook.steps {
            let mut check = StepCheck {
                step,
                artifact: self,
                path: format!("steps.{name}"),
                issues,
            };
            check.validate_name_and_edges(name, &mut bindings);
            check.validate_operation(&mut bindings);
        }
        bindings
    }
}

struct WaitFilter<'a> {
    correlation: &'a BTreeMap<String, Expr>,
    minimum_revision: bool,
}

struct StepCheck<'a> {
    artifact: &'a ArtifactContent,
    step: &'a Step,
    path: String,
    issues: &'a mut Vec<ValidationIssue>,
}
impl StepCheck<'_> {
    fn report(&mut self, code: &str, message: impl Into<String>) {
        issue(self.issues, code, &self.path, message);
    }
    fn required_edges(&mut self, names: impl IntoIterator<Item = String>) {
        required_edges(self.step, names, &self.path, self.issues);
    }
    fn check_expressions(&mut self, bindings: &mut StepBindings) {
        for (at, e) in step_exprs(&self.step.op) {
            check_expr_structure(
                e,
                &at,
                &bindings.roots,
                &mut vec![],
                &self.path,
                self.issues,
            );
            if check_eval_bounds(e, &at, &self.path, self.issues) {
                bindings.too_deep.insert((self.path.clone(), at));
            }
        }
    }
    fn validate_name_and_edges(&mut self, name: &str, bindings: &mut StepBindings) {
        if !valid_name(name) {
            self.report("identifier", "invalid step name");
        }
        self.check_expressions(bindings);
        for target in children(self.step) {
            if !self.artifact.playbook.steps.contains_key(target) {
                self.report("step_reference", format!("unknown target {target}"));
            }
        }
        if let Some(msg) =
            binding_name(&self.step.op).and_then(|bind| bindings.record_binding(name, bind))
        {
            self.report("binding", msg);
        }
        if !matches!(
            self.step.op,
            Operation::Return { .. } | Operation::Decide { .. }
        ) && self.step.on_error.is_none()
        {
            self.report(
                "unhandled_error",
                "operation requires explicit on_error continuation",
            );
        }
    }
    fn validate_operation(&mut self, bindings: &mut StepBindings) {
        match &self.step.op {
            Operation::Invoke { command, retry, .. } => self.invoke(command, retry.as_ref()),
            Operation::Decide { .. } => {
                self.required_edges(["true", "false", "unknown"].map(String::from))
            }
            Operation::Wait {
                event,
                correlation,
                minimum_revision,
                deadline_ms,
                ..
            } => self.wait(
                event.as_ref(),
                WaitFilter {
                    correlation,
                    minimum_revision: minimum_revision.is_some(),
                },
                *deadline_ms,
            ),
            Operation::ForEach {
                item,
                max_items,
                concurrency,
                ..
            } => {
                self.for_each(bindings.record_item(item), *max_items, *concurrency);
            }
            Operation::Call {
                dependency,
                entry,
                mode,
                ..
            } => self.call(dependency, entry, mode),
            Operation::Join {
                members,
                member_keys,
                mode,
                accepted_outcomes,
                deadline_ms,
                ..
            } => {
                self.join_types(members.len(), member_keys);
                let has_qualifier = self.join_qualifier(mode, accepted_outcomes);
                self.join_limits(members.len(), *deadline_ms, has_qualifier);
                self.required_edges(["completed", "timeout", "no_match"].map(String::from));
            }
            Operation::Return { .. } => {
                if !self.step.edges.is_empty() || self.step.on_error.is_some() {
                    self.report("return_edges", "return cannot declare transitions");
                }
            }
        }
    }
    fn invoke(&mut self, command: &ContractRef, retry: Option<&RetryPolicy>) {
        let Some(c) = self.artifact.registry.commands.get(&command.name) else {
            self.report(
                "unknown_command",
                format!("unknown command {}", command.name),
            );
            return;
        };
        if c.fingerprint().ok().as_ref() != Some(&command.fingerprint) {
            self.report(
                "contract_fingerprint",
                "command exact contract fingerprint mismatch",
            );
        }
        let p = &self.artifact.playbook;
        if !c.scopes.contains(&p.scope) || !c.capabilities.is_subset(&p.required_capabilities) {
            self.report(
                "authority",
                "command scope/capabilities not declared by playbook",
            );
        }
        self.required_edges(c.outcomes.keys().cloned());
        if retry.is_some_and(|r| !retry_allowed(r, c, p.limits.max_duration_ms)) {
            self.report("retry", "invalid bounded business retry policy");
        }
    }
    fn wait(&mut self, event: Option<&ContractRef>, filter: WaitFilter<'_>, deadline_ms: u64) {
        if !(1..=self.artifact.playbook.limits.max_duration_ms).contains(&deadline_ms) {
            self.report("deadline", "wait deadline must be bounded by root duration");
        }
        if let Some(reference) = event {
            self.wait_for_event(reference, filter, deadline_ms);
            self.required_edges(["received", "timeout"].map(String::from));
        } else {
            if !filter.correlation.is_empty() || filter.minimum_revision {
                self.report(
                    "timer_correlation",
                    "timer cannot carry event correlation/revision",
                );
            }
            self.required_edges(["timeout".into()]);
        }
    }
    fn wait_for_event(
        &mut self,
        reference: &ContractRef,
        filter: WaitFilter<'_>,
        deadline_ms: u64,
    ) {
        let Some(event) = self.artifact.registry.events.get(&reference.name) else {
            self.report("unknown_event", format!("unknown event {}", reference.name));
            return;
        };
        if event.fingerprint().ok().as_ref() != Some(&reference.fingerprint) {
            self.report(
                "contract_fingerprint",
                "event exact contract fingerprint mismatch",
            );
        }
        if !event.scopes.contains(&self.artifact.playbook.scope) || deadline_ms > event.retention_ms
        {
            self.report(
                "wait_horizon",
                "event scope or retained correlation horizon unsupported",
            );
        }
        if filter
            .correlation
            .keys()
            .ne(event.correlation_fields.iter())
        {
            self.report(
                "correlation",
                "wait must bind every exact event correlation field",
            );
        }
        if filter.minimum_revision && event.revision_field.is_none() {
            self.report(
                "revision",
                "minimum revision requires versioned event contract",
            );
        }
    }
    fn for_each(&mut self, valid_item: bool, max_items: usize, concurrency: u32) {
        if !valid_item {
            self.report("binding", "invalid/shadowing iteration item binding");
        }
        if !(1..=self.artifact.playbook.limits.max_iterations).contains(&(max_items as u64)) {
            self.report(
                "iteration_limit",
                "finite item bound must fit root iteration budget",
            );
        }
        if concurrency != 1 {
            self.report("unsupported_feature","format 1 runner supports serial foreach; use owned async calls and bounded joins for parallel work");
        }
    }
    fn call(&mut self, dependency: &str, entry: &str, mode: &CallMode) {
        let Some(dependency) = self.artifact.dependencies.get(dependency) else {
            self.report("unknown_dependency", "unknown statically named dependency");
            return;
        };
        let Some(interface) = dependency.interfaces.get(entry) else {
            self.report("unknown_interface", "unknown child entry point");
            return;
        };
        if *mode == CallMode::Wait {
            self.required_edges(interface.outcomes.keys().cloned());
        } else {
            self.required_edges(["started".into()]);
        }
    }
    fn join_types(&mut self, member_count: usize, keys: &[String]) {
        if !keys.is_empty() && !valid_join_keys(keys, member_count) {
            self.report(
                "join_member_keys",
                "join member keys must be unique valid identities with exact member cardinality",
            );
        }
        if let Err(error) = self.artifact.join_schema(self.step, false) {
            self.report("join_member_type", error);
        }
    }
    fn join_qualifier(&mut self, mode: &JoinMode, outcomes: &BTreeSet<String>) -> bool {
        if *mode == JoinMode::AnyCompletion && !outcomes.is_empty() {
            self.report("join_qualifier","any_completion cannot declare accepted_outcomes; use any_accepted to qualify outcomes");
        }
        *mode != JoinMode::AnyAccepted || !outcomes.is_empty()
    }
    fn join_limits(&mut self, member_count: usize, deadline_ms: u64, has_qualifier: bool) {
        let limits = &self.artifact.playbook.limits;
        let bounded = (1..=limits.max_children as usize).contains(&member_count)
            && (1..=limits.max_duration_ms).contains(&deadline_ms);
        if !bounded || !has_qualifier {
            self.report("join","join requires finite members, bounded deadline, qualifying outcomes for any_accepted");
        }
    }
}
fn valid_join_keys(keys: &[String], member_count: usize) -> bool {
    let unique = keys.iter().collect::<BTreeSet<_>>();
    let correct_count = keys.len() == member_count && unique.len() == member_count;
    correct_count && keys.iter().all(|key| valid_name(key))
}
fn retry_allowed(retry: &RetryPolicy, command: &CommandContract, max_duration_ms: u64) -> bool {
    let timing_allowed =
        (2..=16).contains(&retry.max_attempts) && (1..=max_duration_ms).contains(&retry.backoff_ms);
    let outcomes_allowed = !retry.outcomes.is_empty()
        && retry
            .outcomes
            .iter()
            .all(|o| command.outcomes.contains_key(o));
    timing_allowed && outcomes_allowed
}
