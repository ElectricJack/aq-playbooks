//! Store host replies and reconcile retries of the same durable intent.
use super::*;

pub(super) fn record_result(
    state: &mut State,
    id: &str,
    result: DispatchResult,
    now_ms: i64,
) -> Result<()> {
    indexes::mark_intent(state, id);
    let intent = state
        .intents
        .get(id)
        .cloned()
        .ok_or_else(|| Error::NotFound(id.into()))?;
    let mut operation = Recording {
        state,
        id,
        intent,
        now_ms,
    };
    match operation.intent.status {
        IntentStatus::Cancelled => operation.late(result),
        IntentStatus::Completed => operation.check_replay(result),
        _ => {
            if operation.apply(result)? {
                operation.resume_run()?;
                operation.save();
            }
            Ok(())
        }
    }
}

struct Recording<'a> {
    state: &'a mut State,
    id: &'a str,
    intent: Intent,
    now_ms: i64,
}

impl Recording<'_> {
    fn save(self) {
        self.state.intents.insert(self.id.into(), self.intent);
    }

    fn audit(&mut self, kind: &str) {
        self.intent.receipt_at(self.now_ms).record(
            self.state,
            kind,
            json!({"operation":self.intent.id}),
        );
    }

    fn late(mut self, result: DispatchResult) -> Result<()> {
        let DispatchResult::Completed { outcome, output } = result else {
            return Ok(());
        };
        let response = (outcome, output);
        if self.intent.late_results.contains(&response) {
            return Ok(());
        }
        self.intent
            .command
            .outcomes
            .get(&response.0)
            .ok_or_else(|| Error::Refused("undeclared late command outcome".into()))?
            .validate(&response.1)?;
        check_bytes(&response.1, self.intent.command.max_payload_bytes)?;
        self.intent.late_results.push(response);
        self.audit("late_result");
        self.save();
        Ok(())
    }

    fn check_replay(&self, result: DispatchResult) -> Result<()> {
        if let DispatchResult::Completed { outcome, output } = result
            && self.intent.result.as_ref() != Some(&(outcome, output))
        {
            return Err(Error::Conflict("operation result changed".into()));
        }
        Ok(())
    }

    fn apply(&mut self, result: DispatchResult) -> Result<bool> {
        match result {
            DispatchResult::Completed { outcome, output } => self.complete(outcome, output)?,
            DispatchResult::Accepted { handle } => return self.accept(handle),
            DispatchResult::Unknown { reason } => {
                self.set_status(IntentStatus::Unknown, reason, "operation_unknown")
            }
            DispatchResult::Rejected { reason } => {
                self.set_status(IntentStatus::Rejected, reason, "operation_rejected")
            }
        }
        Ok(true)
    }

    fn set_status(&mut self, status: IntentStatus, reason: String, kind: &str) {
        self.intent.status = status;
        self.intent.reason = Some(reason);
        self.audit(kind);
    }

    fn accept(&mut self, handle: String) -> Result<bool> {
        if let Some(previous) = &self.intent.handle {
            if previous != &handle {
                return Err(Error::Conflict("operation admission handle changed".into()));
            }
            if self.intent.status == IntentStatus::Accepted {
                return Ok(false);
            }
        }
        if self.intent.command.asynchronous.is_none() {
            return Err(Error::Refused(
                "synchronous contract returned async admission".into(),
            ));
        }
        self.intent.status = IntentStatus::Accepted;
        self.intent.handle = Some(handle);
        self.intent.reason = None;
        self.audit("operation_accepted");
        Ok(true)
    }

    fn complete(&mut self, outcome: String, output: Value) -> Result<()> {
        if self.validate_output(&outcome, &output).is_err() {
            return self.block_invalid_output();
        }
        let public = output
            .as_object()
            .map(|fields| {
                Value::Object(
                    fields
                        .iter()
                        .filter(|(k, _)| self.intent.command.public_result_fields.contains(*k))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                )
            })
            .unwrap_or(Value::Null);
        self.intent.status = IntentStatus::Completed;
        self.intent.receipt_at(self.now_ms).record(
            self.state,
            "command_result",
            json!({"operation":self.intent.id,"outcome":outcome,"public":public}),
        );
        self.intent.result = Some((outcome, output));
        Ok(())
    }

    fn validate_output(&self, outcome: &str, output: &Value) -> Result<()> {
        let command = &self.intent.command;
        command
            .outcomes
            .get(outcome)
            .ok_or_else(|| Error::Refused("undeclared command outcome".into()))?
            .validate(output)?;
        let run = &self.state.runs[&self.intent.run];
        let root = &self.state.runs[&run.root];
        let limit = |run: &Run| {
            self.state.artifacts[&run.artifact]
                .content
                .playbook
                .limits
                .max_payload_bytes
        };
        check_bytes(
            output,
            command.max_payload_bytes.min(limit(run)).min(limit(root)),
        )
    }

    fn block_invalid_output(&mut self) -> Result<()> {
        self.intent.status = IntentStatus::Unknown;
        self.intent.reason = Some(
            "command returned invalid outcome/output; external effect requires reconciliation"
                .into(),
        );
        let run = self
            .state
            .runs
            .get_mut(&self.intent.run)
            .ok_or_else(|| Error::NotFound(self.intent.run.clone()))?;
        if !matches!(run.status, RunStatus::Cancelling | RunStatus::Finishing) {
            run.status = RunStatus::Blocked;
        }
        run.reason = self.intent.reason.clone();
        self.audit("invalid_output");
        Ok(())
    }

    fn resume_run(&mut self) -> Result<()> {
        let run = self
            .state
            .runs
            .get_mut(&self.intent.run)
            .ok_or_else(|| Error::NotFound(self.intent.run.clone()))?;
        // A confirmed admission resumes the same pending invoke before final completion.
        // A reply cannot revive cancellation, a terminal run, or a different operation.
        if run.status != RunStatus::Blocked {
            return Ok(());
        }
        let awaiting =
            matches!(&run.pending, Some(Pending::Effect { intent }) if intent == self.id);
        let recovered = self.intent.status == IntentStatus::Accepted && awaiting;
        let complete = self.intent.status == IntentStatus::Completed;
        if complete || recovered {
            run.status = RunStatus::Waiting;
            run.reason = None;
        }
        Ok(())
    }
}
