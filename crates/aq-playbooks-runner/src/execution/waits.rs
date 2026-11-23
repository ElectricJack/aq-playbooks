use super::*;
pub(super) struct EventSelector {
    event: Option<ContractRef>,
    correlation: BTreeMap<String, Expr>,
    minimum_revision: Option<Expr>,
}

impl EventSelector {
    pub(super) fn new(
        event: Option<ContractRef>,
        correlation: BTreeMap<String, Expr>,
        minimum_revision: Option<Expr>,
    ) -> Self {
        Self {
            event,
            correlation,
            minimum_revision,
        }
    }
}

impl StepExecution<'_> {
    pub(super) fn wait(
        &mut self,
        spec: EventSelector,
        deadline_ms: u64,
        bind: String,
    ) -> Result<()> {
        let EventSelector {
            event,
            correlation,
            minimum_revision,
        } = spec;
        self.check_event_horizon(event.as_ref(), deadline_ms)?;
        let corr = self.correlations(correlation, event.is_some())?;
        let minimum = minimum_revision
            .map(|e| {
                known_expr(&e, &self.context).and_then(|v| {
                    v.as_u64().ok_or_else(|| {
                        Error::Refused("minimum revision must be nonnegative integer".into())
                    })
                })
            })
            .transpose()?;
        self.check_event_history(event.as_ref(), &corr)?;
        let id = digest(&(&self.run.id, &self.run.step, self.run.visits))?;
        self.state.waits.insert(
            id.clone(),
            WaitRecord {
                id: id.clone(),
                run: self.run.id.clone(),
                step: self.run.step.clone(),
                event,
                scope: self.run.authority.scope.clone(),
                correlation: corr,
                minimum_revision: minimum,
                deadline: deadline(self.now, deadline_ms)?,
                bind,
                winner: None,
                expired: false,
            },
        );
        self.run.pending = Some(Pending::Event { wait: id });
        self.run.status = RunStatus::Waiting;
        poll_pending(
            self.state,
            self.run,
            self.run.pending.clone().unwrap(),
            self.now,
        )?;
        Ok(())
    }

    fn check_event_horizon(&self, event: Option<&ContractRef>, deadline_ms: u64) -> Result<()> {
        if let Some(e) = event {
            let c = find_event(self.state, e)?;
            if c.retention_ms < deadline_ms {
                return Err(Error::Refused(
                    "wait exceeds supported event retention horizon".into(),
                ));
            }
        }
        Ok(())
    }

    fn correlations(
        &self,
        correlation: BTreeMap<String, Expr>,
        event: bool,
    ) -> Result<BTreeMap<String, String>> {
        let mut corr = BTreeMap::new();
        for (k, v) in correlation {
            let v = known_expr(&v, &self.context)?;
            let v = v
                .as_str()
                .ok_or_else(|| Error::Refused("correlation must be string".into()))?;
            corr.insert(k, v.into());
        }
        if event && corr.is_empty() {
            return Err(Error::Refused(
                "event wait requires exact invocation correlation".into(),
            ));
        }
        Ok(corr)
    }

    fn check_event_history(
        &self,
        event: Option<&ContractRef>,
        corr: &BTreeMap<String, String>,
    ) -> Result<()> {
        if let Some(reference) = event {
            let key = indexes::correlation_key(reference, &self.run.authority.scope, corr)?;
            if self.state.retention.expired_correlations.contains_key(&key) {
                return Err(Error::Refused(
                    "required correlation history expired; explicit new invocation required".into(),
                ));
            }
        }
        Ok(())
    }
}

pub(super) struct JoinStep {
    pub(super) members: Vec<Expr>,
    pub(super) member_keys: Vec<String>,
    pub(super) mode: JoinMode,
    pub(super) accepted_outcomes: BTreeSet<String>,
    pub(super) deadline_ms: u64,
    pub(super) remaining: RemainingPolicy,
    pub(super) bind: String,
}

impl StepExecution<'_> {
    pub(super) fn join(&mut self, spec: JoinStep) -> Result<()> {
        let JoinStep {
            members,
            member_keys,
            mode,
            accepted_outcomes,
            deadline_ms,
            remaining,
            bind,
        } = spec;
        let mut ids = vec![];
        for m in members {
            let value = known_expr(&m, &self.context)?;
            let member = if let Some(child) = value
                .get("child")
                .and_then(Value::as_str)
                .or_else(|| value.as_str())
            {
                if !self.run.children.iter().any(|x| x == child) {
                    return Err(Error::Refused("join requires owned child handle".into()));
                }
                child.to_string()
            } else if let Some(handle) = value.get("handle").and_then(Value::as_str) {
                self.state
                    .intents
                    .values()
                    .find(|i| i.run == self.run.id && i.handle.as_deref() == Some(handle))
                    .ok_or_else(|| Error::Refused("join requires owned operation handle".into()))?
                    .id
                    .clone()
            } else {
                return Err(Error::Refused(
                    "join member must be owned child or operation handle".into(),
                ));
            };
            if ids.contains(&member) {
                return Err(Error::Refused("duplicate join member".into()));
            }
            ids.push(member);
        }
        if ids.len() > self.limits.max_children as usize {
            return Err(Error::Refused("join member bound".into()));
        }
        self.run.pending = Some(Pending::Join {
            members: ids,
            member_keys,
            mode,
            accepted: accepted_outcomes,
            deadline: deadline(self.now, deadline_ms)?,
            remaining,
            bind,
        });
        self.run.status = RunStatus::Waiting;
        poll_pending(
            self.state,
            self.run,
            self.run.pending.clone().unwrap(),
            self.now,
        )?;
        Ok(())
    }
}
