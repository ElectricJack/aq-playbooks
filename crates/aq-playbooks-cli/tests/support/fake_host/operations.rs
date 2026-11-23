use super::*;

impl FakeHost {
    pub(super) fn operation(&mut self, intent: &Intent) -> Result<DispatchResult, String> {
        let input = &intent.input;
        let actor = &intent.authority.actor;
        let handle = field(input, "handle")?;
        let subject = Subject {
            id: handle.into(),
            revision: 1,
        };
        let operation_id = self
            .work
            .values()
            .find(|work| work.reference == handle)
            .map(|work| work.operation_id.clone())
            .unwrap_or_else(|| handle.into());
        let operation = self
            .operations
            .get(&operation_id)
            .ok_or("unknown host operation")?;
        if operation.actor != *actor || operation.scope != intent.authority.scope {
            return Err("operation authority refused".into());
        }
        let cancelling = intent.command.name == "operation.cancel";
        if let Some(outcome) = self.decision_operation(handle, cancelling)? {
            return Ok(completed(outcome, result(handle, &subject)));
        }
        if let Some(outcome) = self.work_operation(handle, cancelling) {
            return Ok(completed(outcome, result(handle, &subject)));
        }
        Ok(match &self.operations[&operation_id].result {
            DispatchResult::Accepted { .. } => completed("running", result(handle, &subject)),
            DispatchResult::Completed { outcome, output } => {
                completed("completed", json!({"outcome":outcome,"output":output}))
            }
            DispatchResult::Unknown { .. } => completed("indeterminate", result(handle, &subject)),
            DispatchResult::Rejected { .. } => completed("refused", result(handle, &subject)),
        })
    }
    fn decision_operation(
        &mut self,
        handle: &str,
        cancelling: bool,
    ) -> Result<Option<&'static str>, String> {
        let Some(decision) = self.decisions.get(handle) else {
            return Ok(None);
        };
        if decision.completion.is_none() && self.now_ms >= decision.expires_at_ms {
            self.settle_decision(handle, "expired")?;
        }
        if !cancelling {
            return Ok(None);
        }
        if self.decisions[handle].completion.is_some() {
            return Ok(Some("too_late"));
        }
        self.settle_decision(handle, "cancelled")?;
        Ok(Some("cancelled"))
    }

    fn work_operation(&mut self, handle: &str, cancelling: bool) -> Option<&'static str> {
        let work = self.work.values_mut().find(|work| work.reference == handle);
        if cancelling {
            let outcome = match work {
                Some(work) if work.cancelled => "cancelled",
                Some(work) if work.completion.is_none() => {
                    work.cancelled = true;
                    "cancelled"
                }
                _ => "too_late",
            };
            return Some(outcome);
        }
        if work.is_some_and(|work| work.cancelled && work.completion.is_none()) {
            return Some("cancelled");
        }
        None
    }
}
