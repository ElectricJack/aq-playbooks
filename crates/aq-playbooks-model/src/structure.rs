//! Structural validation of captured playbooks and their registered contracts.
use super::*;
mod contracts;
mod expressions;
use expressions::ExpressionCheck;
mod steps;

struct ChecksAt<'a> {
    path: String,
    issues: &'a mut Vec<ValidationIssue>,
}

impl ChecksAt<'_> {
    fn report(&mut self, code: &str, message: &str) {
        issue(self.issues, code, &self.path, message);
    }
    fn schema(&mut self, schema: &Schema) {
        schema_issue(schema, &self.path, self.issues);
    }
    fn require(&mut self, code: &str, allowed: bool, message: &str) {
        if !allowed {
            self.report(code, message);
        }
    }
}

impl ArtifactContent {
    /// Structural and typed validation shared by compiler and runner installation.
    /// Exact dependency hashes are format-1's supported callable selection policy.
    pub fn validate_structure(&self) -> Vec<ValidationIssue> {
        let mut issues = vec![];
        self.validate_header(&mut issues);
        self.validate_limits(&mut issues);
        self.validate_entries(&mut issues);
        contracts::validate(&self.registry, &mut issues);
        self.validate_definitions(&mut issues);
        self.validate_dependencies(&mut issues);
        let bindings = self.validate_steps(&mut issues);
        self.validate_triggers(&mut issues, &bindings.roots);
        self.validate_step_graph(&mut issues, &bindings.too_deep);
        self.validate_source_map(&mut issues);
        self.validate_size(&mut issues);
        issues
    }
    fn validate_header(&self, issues: &mut Vec<ValidationIssue>) {
        if self.format_version != FORMAT_VERSION
            || self.source.header.format_version != FORMAT_VERSION
        {
            issue(
                issues,
                "unsupported_format",
                "format_version",
                "unsupported artifact/source format",
            );
        }
        if !(1..=256).contains(&self.compiler.len()) {
            issue(
                issues,
                "compiler_build",
                "compiler",
                "bounded compiler build identifier required",
            );
        }
        if !valid_hash(&self.source.digest) {
            issue(
                issues,
                "source_digest",
                "source.digest",
                "invalid source SHA-256 address",
            );
        }
        self.validate_source_identity(issues);
    }

    fn validate_source_identity(&self, issues: &mut Vec<ValidationIssue>) {
        let p = &self.playbook;
        let valid_names = valid_name(&p.id) && valid_name(&p.scope);
        let matches_header = p.id == self.source.header.id && p.scope == self.source.header.scope;
        if !valid_names || !matches_header {
            issue(
                issues,
                "source_identity",
                "playbook",
                "source and proposal identity/scope must match",
            );
        }
        if self.source.header.interfaces != p.entries.keys().cloned().collect() {
            issue(
                issues,
                "source_interfaces",
                "source.header.interfaces",
                "header must enumerate exact callable entry points",
            );
        }
        if self.source.header.definitions != self.definitions.keys().cloned().collect() {
            issue(
                issues,
                "source_definitions",
                "source.header.definitions",
                "header must enumerate exact captured definitions",
            );
        }
    }

    fn validate_limits(&self, issues: &mut Vec<ValidationIssue>) {
        let p = &self.playbook;
        let sizes = [
            (p.entries.len(), 1..=128),
            (p.steps.len(), 1..=4096),
            (p.triggers.len(), 0..=128),
            (self.dependencies.len(), 0..=128),
            (self.registry.commands.len(), 0..=256),
            (self.registry.events.len(), 0..=256),
            (self.definitions.len(), 0..=256),
        ];
        if sizes
            .into_iter()
            .any(|(count, range)| !range.contains(&count))
        {
            issue(
                issues,
                "model_limits",
                "playbook",
                "entry/step/trigger/registry/dependency/definition count bound exceeded",
            );
        }
        if !execution_limits_supported(&p.limits) {
            issue(
                issues,
                "limits",
                "playbook.limits",
                "invalid or unsupported execution limits",
            );
        }
    }

    fn validate_entries(&self, issues: &mut Vec<ValidationIssue>) {
        let p = &self.playbook;
        for (name, entry) in &p.entries {
            let path = format!("entries.{name}");
            if !valid_name(name) {
                issue(issues, "identifier", &path, "invalid entry name");
            }
            schema_issue(&entry.input, &path, issues);
            if entry.outcomes.is_empty() || entry.outcomes.len() > 64 {
                issue(
                    issues,
                    "outcomes",
                    &path,
                    "one to 64 business outcomes required",
                );
            }
            for s in entry.outcomes.values() {
                schema_issue(s, &path, issues);
            }
            if !p.steps.contains_key(&entry.start) {
                issue(
                    issues,
                    "step_reference",
                    &path,
                    "entry start does not exist",
                );
            }
        }
    }

    fn validate_definitions(&self, issues: &mut Vec<ValidationIssue>) {
        for (name, definition) in &self.definitions {
            if !definition_valid(name, definition) {
                issue(
                    issues,
                    "definition",
                    &format!("definitions.{name}"),
                    "captured definition identity/version/hash invalid",
                );
            }
        }
    }

    fn validate_dependencies(&self, issues: &mut Vec<ValidationIssue>) {
        let p = &self.playbook;
        let mut bindings = BTreeSet::new();
        for (name, d) in &self.dependencies {
            let mut check = ChecksAt::member("dependencies", name, issues);
            check.require(
"dependency",
dependency_matches(name,d,p),"dependency requires a reference artifact, same scope, declared interfaces and covered capabilities");
            check.require(
                "dependency_selection",
                valid_selection(&d.selection),
                "catalog artifact allowlist must contain bounded SHA-256 addresses",
            );
            check.require(
                "recursive_call",
                !d.dependencies.contains(&d.artifact),
                "recursive dependency closure",
            );
            check.require(
                "dependency",
                bindings.insert(d.binding.clone()),
                "duplicate callable binding",
            );
            check.interfaces(d);
        }
        // A root's own artifact address is unavailable until packaging, so bind the
        // static logical name too. Runtime resolves and verifies every exact closure.
        for _ in self.dependencies.values().filter(|d| d.binding == p.id) {
            issue(
                issues,
                "recursive_call",
                "dependencies",
                "self-call binding forbidden",
            );
        }
    }

    fn validate_triggers(&self, issues: &mut Vec<ValidationIssue>, step_roots: &BTreeSet<String>) {
        let p = &self.playbook;
        let mut trigger_ids = BTreeSet::new();
        for t in &p.triggers {
            let path = format!("triggers.{}", t.id);
            let trigger_too_deep =
                ExpressionCheck::new(step_roots, &path, issues).validate(&t.input, "input");
            if !valid_name(&t.id) || !trigger_ids.insert(t.id.clone()) {
                issue(
                    issues,
                    "trigger_identity",
                    &path,
                    "invalid or duplicate trigger identity",
                );
            }
            if !p.entries.contains_key(&t.entry) {
                issue(
                    issues,
                    "unknown_interface",
                    &path,
                    "trigger entry does not exist",
                );
            }
            self.validate_trigger_event(t, trigger_too_deep, issues);
        }
    }

    fn validate_trigger_event(
        &self,
        t: &Trigger,
        trigger_too_deep: bool,
        issues: &mut Vec<ValidationIssue>,
    ) {
        let p = &self.playbook;
        let path = format!("triggers.{}", t.id);
        match self.registry.events.get(&t.event.name) {
            Some(e) => {
                if e.fingerprint().ok().as_ref() != Some(&t.event.fingerprint) {
                    issue(
                        issues,
                        "contract_fingerprint",
                        &path,
                        "trigger event fingerprint mismatch",
                    );
                }
                if !e.scopes.contains(&p.scope) {
                    issue(
                        issues,
                        "authority",
                        &path,
                        "event scope incompatible with playbook",
                    );
                }
                if let Some(entry) = p.entries.get(&t.entry)
                    && !trigger_too_deep
                {
                    let roots = BTreeMap::from([("event".into(), e.payload.clone())]);
                    Compatibility::new(&roots, &path, issues).check(
                        &t.input,
                        &entry.input,
                        "input",
                    );
                }
            }
            None => issue(issues, "unknown_event", &path, "unknown trigger event"),
        }
    }

    fn validate_step_graph(
        &self,
        issues: &mut Vec<ValidationIssue>,
        too_deep: &BTreeSet<(String, String)>,
    ) {
        let p = &self.playbook;
        let mut reachable = BTreeSet::new();
        for (name, e) in &p.entries {
            let mut local = BTreeSet::new();
            walk(&e.start, &p.steps, &mut local);
            reachable.extend(local.iter().cloned());
            typing::EntryTyping::new(self, name, e, too_deep).validate(&local, issues);
        }
        for name in p.steps.keys().filter(|name| !reachable.contains(*name)) {
            issue(
                issues,
                "unreachable_step",
                &format!("steps.{name}"),
                "step is unreachable from every entry",
            );
        }
        let mut done = BTreeSet::new();
        let mut active = BTreeSet::new();
        for name in p.steps.keys() {
            if unbounded_cycle(name, &p.steps, &mut done, &mut active) {
                issue(
                    issues,
                    "unbounded_cycle",
                    &format!("steps.{name}"),
                    "control-flow cycle does not cross a bounded foreach anchor",
                );
                break;
            }
        }
    }

    fn validate_source_map(&self, issues: &mut Vec<ValidationIssue>) {
        let p = &self.playbook;
        for (name, loc) in &self.source_map {
            let existing = name.starts_with("steps.")
                && p.steps.contains_key(name.trim_start_matches("steps."));
            let positioned = loc.line > 0 && loc.column > 0;
            if !existing || !positioned {
                issue(
                    issues,
                    "source_map",
                    &format!("source_map.{name}"),
                    "source mapping must identify an existing step with positive line/column",
                );
            }
        }
    }

    fn validate_size(&self, issues: &mut Vec<ValidationIssue>) {
        match canonical_bytes(self) {
            Err(e) => issue(issues, "canonical", "artifact", e.to_string()),
            Ok(bytes) if bytes.len() > MAX_ARTIFACT_BYTES - 512 => issue(
                issues,
                "artifact_limit",
                "artifact",
                "artifact content exceeds bounded 16 MiB envelope",
            ),
            _ => {}
        }
    }
}

fn execution_limits_supported(limits: &Limits) -> bool {
    let sizes = [
        (limits.max_step_visits, 1..=1_000_000),
        (limits.max_children, 0..=10_000),
        (limits.max_depth.into(), 1..=32),
        (limits.max_concurrent_effects.into(), 1..=1024),
        (limits.max_concurrent_children.into(), 1..=1024),
        (limits.max_duration_ms, 1..=31_536_000_000),
        (limits.max_iterations, 1..=1_000_000),
        (limits.max_iteration_depth.into(), 1..=16),
    ];
    let within_bounds = sizes
        .into_iter()
        .all(|(value, range)| range.contains(&value));
    let payload = (1..=16_777_216).contains(&limits.max_payload_bytes);
    let history = (limits.max_payload_bytes..=1_073_741_824).contains(&limits.max_history_bytes);
    within_bounds && payload && history
}

fn dependency_matches(name: &str, d: &CallableDependency, p: &Playbook) -> bool {
    let names_valid = valid_name(name) && valid_name(&d.binding);
    let authority = d.scope == p.scope && d.capabilities.is_subset(&p.required_capabilities);
    let captured = valid_hash(&d.artifact) && (1..=128).contains(&d.interfaces.len());
    let closure =
        d.dependencies.len() <= 1024 && d.dependencies.iter().all(|hash| valid_hash(hash));
    names_valid && authority && captured && closure
}

impl ChecksAt<'_> {
    fn interfaces(&mut self, d: &CallableDependency) {
        for interface in d.interfaces.values() {
            self.require(
                "outcomes",
                (1..=64).contains(&interface.outcomes.len()),
                "dependency interface requires one to 64 outcomes",
            );
            self.schema(&interface.input);
            for outcome in interface.outcomes.values() {
                self.schema(outcome);
            }
        }
    }
}

fn valid_selection(selection: &DependencySelection) -> bool {
    match selection {
        DependencySelection::Catalog { allowed_artifacts } => {
            allowed_artifacts.len() <= 1024 && allowed_artifacts.iter().all(|hash| valid_hash(hash))
        }
        _ => true,
    }
}
fn definition_valid(name: &str, d: &CapturedDefinition) -> bool {
    let valid_identity = valid_name(name) && !d.version.is_empty();
    valid_identity && digest(&d.value).ok().as_ref() == Some(&d.digest)
}
