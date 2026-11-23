use super::*;
impl StepExecution<'_> {
    pub(super) fn invoke(&mut self, command: ContractRef, input: Expr) -> Result<()> {
        let contract = self.command(&command)?;
        let input = known_expr(&input, &self.context)?;
        contract.input.validate(&input)?;
        check_bytes(
            &input,
            contract
                .max_payload_bytes
                .min(self.limits.max_payload_bytes)
                .min(self.root_limits.max_payload_bytes),
        )?;
        self.check_effect_budget()?;
        let id = digest(&(
            &self.run.id,
            &self.run.step,
            self.run.visits,
            &self
                .run
                .loops
                .iter()
                .map(|f| (&f.site, f.index))
                .collect::<Vec<_>>(),
            self.run.attempts.get(&attempt_key(self.run)?),
        ))?;
        let deadline_at_ms = deadline(self.now, contract.deadline_ms)?
            .min(deadline(
                self.run.admitted_at_ms,
                self.limits.max_duration_ms,
            )?)
            .min(deadline(
                self.root_admitted_at,
                self.root_limits.max_duration_ms,
            )?);
        self.state.intents.insert(
            id.clone(),
            Intent {
                id: id.clone(),
                run: self.run.id.clone(),
                step: self.run.step.clone(),
                invocation: self.run.visits,
                deadline_at_ms,
                input_digest: digest(&input)?,
                input,
                command: contract,
                authority: self.run.authority.clone(),
                status: IntentStatus::Prepared,
                cancel_requested: false,
                cancel_dispatched: false,
                handle: None,
                result: None,
                reason: None,
                late_results: vec![],
                completion_event: None,
                dispatch_sequence: None,
            },
        );
        indexes::mark_intent(self.state, &id);
        self.run.pending = Some(Pending::Effect { intent: id.clone() });
        self.run.status = RunStatus::Waiting;
        self.run.receipt_at(self.now).record(
            self.state,
            "intent",
            json!({"operation":id,"input_digest":self.state.intents[&id].input_digest}),
        );
        Ok(())
    }

    fn command(&self, command: &ContractRef) -> Result<CommandContract> {
        let contract = self
            .artifact
            .content
            .registry
            .commands
            .get(&command.name)
            .ok_or_else(|| Error::Refused("unknown command".into()))?
            .clone();
        if contract.fingerprint()? != command.fingerprint {
            return Err(Error::Conflict("command fingerprint".into()));
        }
        if !contract
            .capabilities
            .is_subset(&self.run.authority.capabilities)
            || !contract.scopes.contains(&self.run.authority.scope)
        {
            return Err(Error::Refused("command authority ceiling".into()));
        }
        // Confidential persistence requires a separately configured protection provider.
        let protected = !contract.sensitive_input_fields.is_empty()
            || !contract.sensitive_output_fields.is_empty();
        if protected && !self.state.protection_available {
            return Err(Error::Refused("protected-value storage unavailable".into()));
        }
        Ok(contract)
    }

    fn check_effect_budget(&self) -> Result<()> {
        let (root_open, run_open) = match indexes::root_open_intents(self.state, &self.run.root) {
            Some(ids) => {
                let run_open = ids
                    .iter()
                    .filter(|id| self.state.intents[*id].run == self.run.id)
                    .count();
                (ids.len(), run_open)
            }
            None => {
                let open = |i: &&Intent| {
                    !matches!(
                        i.status,
                        IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
                    )
                };
                (
                    self.state
                        .intents
                        .values()
                        .filter(|i| {
                            self.state
                                .runs
                                .get(&i.run)
                                .is_some_and(|x| x.root == self.run.root)
                        })
                        .filter(open)
                        .count(),
                    self.state
                        .intents
                        .values()
                        .filter(|i| i.run == self.run.id)
                        .filter(open)
                        .count(),
                )
            }
        };
        if root_open >= self.root_limits.max_concurrent_effects as usize
            || run_open >= self.limits.max_concurrent_effects as usize
        {
            return Err(Error::Refused("outstanding effect limit".into()));
        }
        Ok(())
    }
}
