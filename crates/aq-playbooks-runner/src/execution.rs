//! Execute one deterministic run transition under its root and playbook budgets.
use super::*;
mod advance;
pub(super) use advance::advance;
mod calls;
mod effects;
mod iteration;
mod waits;
use calls::ChildEntry;
use iteration::ForEachStep;
use waits::{EventSelector, JoinStep};

fn advance_inner(s: &mut State, r: &mut Run, now: i64) -> Result<()> {
    match r.status {
        RunStatus::Cancelling => return settle_cancel(s, r, now),
        RunStatus::Finishing => return settle_finishing(s, r, now),
        _ => {}
    }
    if !can_advance(s, r)? {
        return Ok(());
    }
    check_run_limits(s, r, now)?;
    if let Some(pending) = r.pending.clone() {
        return poll_pending(s, r, pending, now);
    }
    execute_step(s, r, now)
}

fn can_advance(s: &State, r: &mut Run) -> Result<bool> {
    if !execution_allowed(
        s,
        &r.artifact,
        &r.authority,
        &s.artifacts[&r.artifact]
            .content
            .playbook
            .required_capabilities,
    )? {
        r.status = RunStatus::Blocked;
        r.reason = Some("execution authority revoked".into());
        return Ok(false);
    }
    Ok(r.status != RunStatus::Blocked)
}

fn settle_finishing(s: &mut State, r: &mut Run, now: i64) -> Result<()> {
    if settled(s, r)? {
        if r.pending_failure {
            finish(s, r, RunStatus::Failed, None, None, now);
        } else {
            let (outcome, output) = r.pending_return.take().unwrap();
            finish(s, r, RunStatus::Completed, Some(outcome), Some(output), now);
        }
    }
    Ok(())
}

fn check_run_limits(s: &State, r: &Run, now: i64) -> Result<()> {
    let own_limits = &s.artifacts[&r.artifact].content.playbook.limits;
    let root_run = s
        .runs
        .get(&r.root)
        .ok_or_else(|| invariant(format!("root {} of run {} is missing", r.root, r.id)))?;
    let root_limits = &s.artifacts[&root_run.artifact].content.playbook.limits;
    if now.saturating_sub(root_run.admitted_at_ms) >= root_limits.max_duration_ms as i64
        || now.saturating_sub(r.admitted_at_ms) > own_limits.max_duration_ms as i64
    {
        return Err(Error::Exhausted("duration".into()));
    }
    if retained_bytes(s, r)? >= root_limits.max_history_bytes {
        return Err(Error::Exhausted("retained history".into()));
    }
    Ok(())
}

fn retained_bytes(s: &State, r: &Run) -> Result<usize> {
    let size = if let Some((receipts, intents, runs)) = indexes::root_history(s, &r.root) {
        canonical_bytes(&(
            receipts.iter().map(|i| &s.receipts[*i]).collect::<Vec<_>>(),
            intents.iter().map(|id| &s.intents[id]).collect::<Vec<_>>(),
            runs.iter().map(|id| &s.runs[id]).collect::<Vec<_>>(),
        ))?
        .len()
    } else {
        canonical_bytes(&(
            s.receipts
                .iter()
                .filter(|receipt| {
                    s.runs
                        .get(&receipt.run)
                        .is_some_and(|run| run.root == r.root)
                })
                .collect::<Vec<_>>(),
            s.intents
                .values()
                .filter(|intent| {
                    s.runs
                        .get(&intent.run)
                        .is_some_and(|run| run.root == r.root)
                })
                .collect::<Vec<_>>(),
            s.runs
                .values()
                .filter(|run| run.root == r.root)
                .collect::<Vec<_>>(),
        ))?
        .len()
    };
    Ok(size)
}

fn execute_step(s: &mut State, r: &mut Run, now: i64) -> Result<()> {
    let a = s.artifacts[&r.artifact].clone();
    let limits = &a.content.playbook.limits;
    let root = s
        .runs
        .get(&r.root)
        .ok_or_else(|| invariant(format!("root {} of run {} is missing", r.root, r.id)))?;
    let root_limits = s.artifacts[&root.artifact].content.playbook.limits.clone();
    let root_admitted_at = root.admitted_at_ms;
    let budget = s
        .budgets
        .get_mut(&r.root)
        .ok_or_else(|| invariant(format!("budget of root {} is missing", r.root)))?;
    if budget.visits >= root_limits.max_step_visits || r.visits >= limits.max_step_visits {
        return Err(Error::Exhausted("step visits".into()));
    }
    if now.saturating_sub(r.admitted_at_ms) > limits.max_duration_ms as i64 {
        return Err(Error::Exhausted("duration".into()));
    }
    budget.visits += 1;
    r.visits += 1;
    r.status = RunStatus::Running;
    let step = a
        .content
        .playbook
        .steps
        .get(&r.step)
        .ok_or_else(|| Error::NotFound(r.step.clone()))?
        .clone();
    let ctx = context(s, r);
    StepExecution {
        state: s,
        run: r,
        now,
        context: ctx,
        artifact: &a,
        limits,
        root_limits,
        root_admitted_at,
        edges: step.edges,
    }
    .execute(step.op)
}

struct StepExecution<'a> {
    state: &'a mut State,
    run: &'a mut Run,
    now: i64,
    context: EvalContext,
    artifact: &'a Artifact,
    limits: &'a Limits,
    root_limits: Limits,
    root_admitted_at: i64,
    edges: BTreeMap<String, String>,
}

impl StepExecution<'_> {
    fn execute(&mut self, op: Operation) -> Result<()> {
        match op {
            Operation::Decide { predicate } => self.decide(predicate),
            Operation::Invoke { command, input, .. } => self.invoke(command, input),
            Operation::Wait {
                event,
                correlation,
                minimum_revision,
                deadline_ms,
                bind,
            } => self.wait(
                EventSelector::new(event, correlation, minimum_revision),
                deadline_ms,
                bind,
            ),
            Operation::ForEach {
                collection,
                item,
                body,
                next,
                max_items,
                concurrency,
                on_failure,
            } => self.for_each(ForEachStep {
                collection,
                item,
                body,
                next,
                max_items,
                concurrency,
                on_failure,
            }),
            Operation::Call {
                dependency,
                entry,
                input,
                mode,
                bind,
            } => self.call(ChildEntry::new(dependency, entry, input), mode, bind),
            Operation::Join {
                members,
                member_keys,
                mode,
                accepted_outcomes,
                deadline_ms,
                remaining,
                bind,
            } => self.join(JoinStep {
                members,
                member_keys,
                mode,
                accepted_outcomes,
                deadline_ms,
                remaining,
                bind,
            }),
            Operation::Return { outcome, output } => self.return_outcome(outcome, output),
        }
    }

    fn decide(&mut self, predicate: Expr) -> Result<()> {
        let outcome = match eval(&predicate, &self.context, &EvalLimits::default())? {
            EvalValue::Known(Value::Bool(true)) => "true",
            EvalValue::Known(Value::Bool(false)) => "false",
            EvalValue::Missing | EvalValue::Unknown(_) => "unknown",
            _ => return Err(Error::Refused("predicate must be boolean".into())),
        };
        follow(self.state, self.run, &self.edges, outcome, self.now)?;
        Ok(())
    }

    fn return_outcome(&mut self, outcome: String, output: Expr) -> Result<()> {
        let output = known_expr(&output, &self.context)?;
        let entry = &self.artifact.content.playbook.entries[&self.run.entry];
        entry
            .outcomes
            .get(&outcome)
            .ok_or_else(|| Error::Refused("undeclared return outcome".into()))?
            .validate(&output)?;
        check_bytes(
            &output,
            self.limits
                .max_payload_bytes
                .min(self.root_limits.max_payload_bytes),
        )?;
        if settled(self.state, self.run)? {
            finish(
                self.state,
                self.run,
                RunStatus::Completed,
                Some(outcome),
                Some(output),
                self.now,
            );
        } else {
            self.run.pending_return = Some((outcome, output));
            self.run.status = RunStatus::Finishing;
            receipt(
                self.state,
                &self.run.id,
                &self.run.step,
                self.now,
                "finishing",
                Value::Null,
            );
        }
        Ok(())
    }
}
