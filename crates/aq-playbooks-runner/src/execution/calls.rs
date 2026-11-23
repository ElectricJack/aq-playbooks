use super::*;
pub(super) struct ChildEntry {
    dependency: String,
    entry: String,
    input: Expr,
}

impl ChildEntry {
    pub(super) fn new(dependency: String, entry: String, input: Expr) -> Self {
        Self {
            dependency,
            entry,
            input,
        }
    }
}

impl StepExecution<'_> {
    pub(super) fn call(&mut self, spec: ChildEntry, mode: CallMode, bind: String) -> Result<()> {
        let ChildEntry {
            dependency,
            entry,
            input,
        } = spec;
        let child_artifact = self.child_artifact(&dependency)?;
        self.check_child_budget()?;
        let input = known_expr(&input, &self.context)?;
        check_bytes(
            &input,
            self.limits
                .max_payload_bytes
                .min(self.root_limits.max_payload_bytes),
        )?;
        let id = digest(&(
            &self.run.id,
            &self.run.step,
            self.run.visits,
            self.run
                .loops
                .iter()
                .map(|f| (&f.site, f.index))
                .collect::<Vec<_>>(),
        ))?;
        self.state.budgets.get_mut(&self.run.root).unwrap().children += 1;
        Invocation {
            artifact: &child_artifact,
            entry: &entry,
            input,
            authority: self.run.authority.clone(),
            dependencies: self.run.dependencies.clone(),
        }
        .admit(self.state, Lineage::child(self.run, &id, &mode), self.now)?;
        if mode != CallMode::Independent {
            self.run.children.push(id.clone());
        }
        self.run.receipt_at(self.now).record(
            self.state,
            "child_admitted",
            json!({"child":id,"mode":mode}),
        );
        if mode == CallMode::Wait {
            self.run.pending = Some(Pending::Child { child: id, bind });
            self.run.status = RunStatus::Waiting;
        } else {
            self.run.variables.insert(bind, json!({"child":id}));
            self.run
                .transition(self.state, self.now)
                .follow(&self.edges, "started")?;
        }
        Ok(())
    }

    fn child_artifact(&self, dependency: &str) -> Result<String> {
        if let Some(dependency_contract) = self.artifact.content.dependencies.get(dependency)
            && self
                .state
                .bindings
                .get(&dependency_contract.binding)
                .is_some_and(|binding| !binding.controls.calls_enabled)
        {
            return Err(Error::Refused("new child calls disabled".into()));
        }
        let child_artifact = self
            .run
            .dependencies
            .get(&format!("{}/{}", self.run.artifact, dependency))
            .ok_or_else(|| Error::Refused("dependency absent from frozen manifest".into()))?
            .clone();
        let child = &self.state.artifacts[&child_artifact];
        check_authority(child, &self.run.authority)?;
        Ok(child_artifact)
    }

    fn check_child_budget(&self) -> Result<()> {
        if self.run.depth + 1 > self.root_limits.max_depth
            || self.run.depth + 1 > self.limits.max_depth
        {
            return Err(Error::Exhausted("child depth".into()));
        }
        let budget = self
            .state
            .budgets
            .get(&self.run.root)
            .ok_or_else(|| invariant(format!("budget of root {} is missing", self.run.root)))?;
        if budget.children >= self.root_limits.max_children
            || self
                .state
                .runs
                .values()
                .filter(|child| child.origin.as_deref() == Some(&self.run.id))
                .count()
                >= self.limits.max_children as usize
        {
            return Err(Error::Exhausted("child admissions".into()));
        }
        self.check_concurrent_children()
    }

    fn check_concurrent_children(&self) -> Result<()> {
        if self
            .state
            .runs
            .values()
            .filter(|child| {
                child.origin.as_deref() == Some(&self.run.id) && !child.status.terminal()
            })
            .count()
            >= self.limits.max_concurrent_children as usize
            || self
                .state
                .runs
                .values()
                .filter(|child| {
                    child.root == self.run.root
                        && child.id != self.run.root
                        && !child.status.terminal()
                })
                .count()
                >= self.root_limits.max_concurrent_children as usize
        {
            return Err(Error::Refused("concurrent child limit".into()));
        }
        Ok(())
    }
}
