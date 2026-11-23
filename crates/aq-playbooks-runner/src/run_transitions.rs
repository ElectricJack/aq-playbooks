//! Branch and settle runs, preserving receipt and dependency ordering.
use super::*;

pub(super) enum Terminal {
    Failed,
    Cancelled,
    Completed { outcome: String, output: Value },
}
pub(super) struct RunTransition<'a> {
    state: &'a mut State,
    run: &'a mut Run,
    now: i64,
}
impl Run {
    pub(super) fn transition<'a>(
        &'a mut self,
        state: &'a mut State,
        now: i64,
    ) -> RunTransition<'a> {
        RunTransition {
            state,
            run: self,
            now,
        }
    }
}
impl RunTransition<'_> {
    pub(super) fn follow(self, edges: &BTreeMap<String, String>, outcome: &str) -> Result<()> {
        let Self {
            state: s,
            run: r,
            now,
        } = self;

        let target = edges
            .get(outcome)
            .ok_or_else(|| Error::Refused(format!("unhandled outcome {outcome}")))?;
        r.receipt_at(now)
            .record(s, "branch", json!({"outcome":outcome,"next":target}));
        r.step = target.clone();
        r.pending = None;
        r.status = RunStatus::Running;
        Ok(())
    }
    pub(super) fn finish(self, terminal: Terminal) {
        let Self {
            state: s,
            run: r,
            now,
        } = self;
        let (status, outcome, output) = match terminal {
            Terminal::Failed => (RunStatus::Failed, None, None),
            Terminal::Cancelled => (RunStatus::Cancelled, None, None),
            Terminal::Completed { outcome, output } => {
                (RunStatus::Completed, Some(outcome), Some(output))
            }
        };

        r.status = status;
        r.outcome = outcome;
        r.output = output;
        r.pending = None;
        r.receipt_at(now).record(
            s,
            "terminal",
            json!({"status":r.status,"outcome":r.outcome}),
        );
        r.terminal_sequence = Some(s.sequence);
        indexes::wake_dependents(s, &r.id);
    }
}
pub(super) fn settled(s: &State, r: &Run) -> Result<bool> {
    // A cohort leaves only as a whole: a live run's children are live.
    for id in &r.children {
        let child = s
            .runs
            .get(id)
            .ok_or_else(|| invariant(format!("child {id} of run {} is missing", r.id)))?;
        if !child.status.terminal() {
            return Ok(false);
        }
    }
    Ok(match indexes::root_open_intents(s, &r.root) {
        Some(open) => !open.iter().any(|id| s.intents[id].run == r.id),
        None => s.intents.values().filter(|i| i.run == r.id).all(|i| {
            matches!(
                i.status,
                IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
            )
        }),
    })
}
pub(super) fn request_cancel(s: &mut State, id: &str, now: i64) -> Result<()> {
    let mut r = s
        .runs
        .get(id)
        .ok_or_else(|| Error::NotFound(id.into()))?
        .clone();
    if r.status.terminal() {
        return Ok(());
    }
    r.status = RunStatus::Cancelling;
    ReceiptSite::new(id, &r.step, now).record(s, "cancel_requested", Value::Null);
    for child in r.children.clone() {
        request_cancel(s, &child, now)?;
    }
    for i in s.intents.values_mut().filter(|i| i.run == id) {
        if i.status == IntentStatus::Prepared {
            i.status = IntentStatus::Cancelled;
        } else if !matches!(
            i.status,
            IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
        ) {
            i.cancel_requested = true;
        }
    }
    s.runs.insert(id.into(), r);
    indexes::mark_run(s, id);
    let intents = s
        .intents
        .values()
        .filter(|i| i.run == id)
        .map(|i| i.id.clone())
        .collect::<Vec<_>>();
    for intent in intents {
        indexes::mark_intent(s, &intent);
    }
    Ok(())
}
pub(super) fn settle_cancel(s: &mut State, r: &mut Run, now: i64) -> Result<()> {
    if settled(s, r)? {
        r.transition(s, now).finish(Terminal::Cancelled);
    } else if s.intents.values().any(|i| {
        i.run == r.id
            && matches!(
                i.status,
                IntentStatus::Dispatched | IntentStatus::Unknown | IntentStatus::Accepted
            )
    }) {
        r.reason = Some("cancellation awaits external operation settlement".into());
    }
    Ok(())
}

pub(super) fn fail_run(s: &mut State, r: &mut Run, reason: String, now: i64) -> Result<()> {
    r.pending = None;
    r.reason = Some(reason.clone());
    r.pending_failure = true;
    for child in r.children.clone() {
        request_cancel(s, &child, now)?;
    }
    for intent in s.intents.values_mut().filter(|i| i.run == r.id) {
        if intent.status == IntentStatus::Prepared {
            intent.status = IntentStatus::Cancelled;
        } else if !matches!(
            intent.status,
            IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
        ) {
            intent.cancel_requested = true;
        }
    }
    let intents = s
        .intents
        .values()
        .filter(|i| i.run == r.id)
        .map(|i| i.id.clone())
        .collect::<Vec<_>>();
    for intent in intents {
        indexes::mark_intent(s, &intent);
    }
    r.receipt_at(now)
        .record(s, "engine_failure", json!({"reason":reason}));
    if settled(s, r)? {
        r.transition(s, now).finish(Terminal::Failed);
    } else {
        r.status = RunStatus::Finishing;
    }
    Ok(())
}
