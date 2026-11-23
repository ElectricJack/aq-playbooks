//! Roll failed steps back to their checkpoint before selecting the error branch.
use super::*;

pub(crate) fn advance(s: &mut State, id: &str, now: i64) -> Result<bool> {
    process_completions(s)?;
    process_operation_deadlines(s, now);
    indexes::unqueue(s, id);
    let mut run = s
        .runs
        .get(id)
        .cloned()
        .ok_or_else(|| Error::NotFound(id.into()))?;
    if run.status.terminal() {
        return Ok(false);
    }
    let before = snapshot(&run)?;
    let checkpoint = s.clone();
    let original = run.clone();
    if let Err(error) = advance_inner(s, &mut run, now) {
        if is_invariant(&error) {
            return Err(error);
        }
        *s = checkpoint;
        run = original;
        if matches!(error, Error::Exhausted(_)) {
            fail_run(s, &mut run, error.to_string(), now)?;
        } else {
            route_error(s, &mut run, error.to_string(), now);
        }
    }
    let changed = before != snapshot(&run)?;
    s.runs.insert(id.into(), run);
    indexes::mark_run(s, id);
    Ok(changed)
}

fn snapshot(run: &Run) -> Result<Value> {
    serde_json::to_value(run).map_err(|e| Error::Refused(e.to_string()))
}

fn route_error(s: &mut State, run: &mut Run, message: String, now: i64) {
    let step_target = s.artifacts[&run.artifact]
        .content
        .playbook
        .steps
        .get(&run.step)
        .and_then(|step| step.on_error.clone());
    run.receipt_at(now)
        .record(s, "error", json!({"reason":message}));
    let target = match run.loops.last().cloned() {
        None => step_target,
        Some(frame) if frame.failure == IterationFailure::Continue => Some(frame.site),
        Some(frame) => {
            run.loops.pop();
            if let Some(previous) = frame.previous {
                run.variables.insert(frame.item, previous);
            } else {
                run.variables.remove(&frame.item);
            }
            s.artifacts[&run.artifact].content.playbook.steps[&frame.site]
                .on_error
                .clone()
        }
    };
    if let Some(target) = target {
        run.step = target;
        run.pending = None;
        run.status = RunStatus::Running;
    } else {
        run.status = RunStatus::Blocked;
        run.reason = Some(message);
    }
}
