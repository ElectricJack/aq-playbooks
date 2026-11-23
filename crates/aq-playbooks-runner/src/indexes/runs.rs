use super::*;
use crate::Run;

pub(super) fn refresh_run(s: &mut State, id: &str) -> Result<()> {
    let was_queued = s.indexes.ready_tokens.contains_key(id);
    unqueue(s, id);
    unsubscribe(s, id);
    if let Some(ids) = s.indexes.run_dependencies.remove(id) {
        for dep in ids {
            remove_lazy_member(&mut s.indexes.dependents, &dep, id);
        }
    }
    let timerkey = format!("run:{id}");
    remove_timer(s, &timerkey);

    let Some(r) = s.runs.get(id).cloned() else {
        remove_run(&mut s.indexes, id);
        return Ok(());
    };
    index_root(&mut s.indexes, id, &r);
    if r.status.terminal() {
        wake_dependents(s, id);
        return Ok(());
    }
    let at = schedule(s, id, &r)?;
    if was_queued
        && matches!(
            r.status,
            RunStatus::Waiting | RunStatus::Finishing | RunStatus::Cancelling
        )
    {
        wake(s, id);
    }
    if let Some(at_ms) = at {
        timer(
            s,
            TimerIndex {
                key: timerkey,
                run: id.into(),
                at_ms,
                intent: None,
            },
        );
    }
    Ok(())
}

fn remove_run(indexes: &mut RuntimeIndexes, id: &str) {
    indexes.runs.remove(id);
    if let Some(root) = indexes.run_roots.remove(id) {
        if let Some(ids) = indexes.root_members.get_mut(&root) {
            ids.remove(id);
            if ids.is_empty() {
                indexes.root_members.remove(&root);
                indexes.root_active.remove(&root);
                indexes.root_effects.remove(&root);
                indexes.root_intents.remove(&root);
                indexes.root_last_activity.remove(&root);
            }
        }
        if let Some(ids) = indexes.root_active.get_mut(&root) {
            ids.remove(id);
        }
    }
}

fn index_root(indexes: &mut RuntimeIndexes, id: &str, r: &Run) {
    indexes.runs.insert(id.into());
    indexes.run_roots.insert(id.into(), r.root.clone());
    indexes
        .root_members
        .entry(r.root.clone())
        .or_default()
        .insert(id.into());
    if r.status.terminal() {
        if let Some(ids) = indexes.root_active.get_mut(&r.root) {
            ids.remove(id);
        }
    } else {
        indexes
            .root_active
            .entry(r.root.clone())
            .or_default()
            .insert(id.into());
    }
}

fn schedule(s: &mut State, id: &str, r: &Run) -> Result<Option<i64>> {
    let mut at = None;
    match r.status {
        RunStatus::Admitted | RunStatus::Running => wake(s, id),
        RunStatus::Waiting => at = waiting(s, id, r)?,
        RunStatus::Finishing | RunStatus::Cancelling => finishing(s, id, r)?,
        _ => {}
    }
    if matches!(
        r.status,
        RunStatus::Finishing | RunStatus::Cancelling | RunStatus::Blocked
    ) {
        return Ok(at);
    }
    let end = duration_deadline(s, id, r)?;
    Ok(Some(at.map_or(end, |a| a.min(end))))
}

fn duration_deadline(s: &State, id: &str, r: &Run) -> Result<i64> {
    // A live run's root is live: a cohort leaves only as a whole.
    let root = s
        .runs
        .get(&r.root)
        .ok_or_else(|| crate::invariant(format!("root {} of run {id} is missing", r.root)))?;
    let end = crate::deadline(
        root.admitted_at_ms,
        s.artifacts[&root.artifact]
            .content
            .playbook
            .limits
            .max_duration_ms,
    )?;
    let own = crate::deadline(
        r.admitted_at_ms,
        s.artifacts[&r.artifact]
            .content
            .playbook
            .limits
            .max_duration_ms,
    )?;
    Ok(end.min(own))
}

fn finishing(s: &mut State, id: &str, r: &Run) -> Result<()> {
    if crate::settled(s, r)? {
        wake(s, id)
    } else {
        for child in &r.children {
            depend(s, id, child.clone());
        }
        let ids = s
            .intents
            .values()
            .filter(|i| i.run == id)
            .map(|i| i.id.clone())
            .collect::<Vec<_>>();
        for intent in ids {
            depend(s, id, intent);
        }
    }
    Ok(())
}

fn waiting(s: &mut State, id: &str, r: &Run) -> Result<Option<i64>> {
    match &r.pending {
        Some(Pending::Effect { intent }) => {
            depend(s, id, intent.clone());
            if s.intents
                .get(intent)
                .is_some_and(|i| effect_ready(s, r, &i.status))
            {
                wake(s, id);
            }
        }
        Some(Pending::Event { wait }) => {
            let w = &s.waits[wait];
            let deadline = w.deadline;
            if let Some(contract) = &w.event {
                subscribe(s, id, correlation_key(contract, &w.scope, &w.correlation)?);
            }
            return Ok(Some(deadline));
        }
        Some(Pending::Child { child, .. }) => {
            depend(s, id, child.clone());
            if s.runs
                .get(child)
                .is_some_and(|child| child.status.terminal())
            {
                wake(s, id);
            }
        }
        Some(Pending::Join {
            members, deadline, ..
        }) => {
            for member in members {
                depend(s, id, member.clone());
            }
            return Ok(Some(*deadline));
        }
        Some(Pending::Retry { deadline }) => return Ok(Some(*deadline)),
        None => {}
    }
    Ok(None)
}

fn effect_ready(s: &State, r: &Run, status: &IntentStatus) -> bool {
    match status {
        IntentStatus::Completed
        | IntentStatus::Rejected
        | IntentStatus::Cancelled
        | IntentStatus::Unknown => true,
        IntentStatus::Accepted => s.artifacts[&r.artifact].content.playbook.steps[&r.step]
            .edges
            .contains_key("accepted"),
        _ => false,
    }
}
