//! Restore each timer to at most one declared duration beyond the rebased clock.
use super::*;

#[derive(Default)]
pub(super) struct Deadlines {
    pub(super) capped: usize,
    pub(super) adjusted: BTreeSet<String>,
    pub(super) unknown: BTreeSet<String>,
    pub(super) adjustments: Vec<DeadlineAdjustment>,
}

impl Deadlines {
    pub(super) fn rebase(state: &mut State, to_ms: i64) -> Self {
        let mut report = Self::default();
        let runs: Vec<_> = state.runs.keys().cloned().collect();
        for id in runs {
            report.adjust_run(state, &id, to_ms);
        }
        let intents: Vec<_> = state.intents.keys().cloned().collect();
        for id in intents {
            report.adjust_intent(state, &id, to_ms);
        }
        report
    }

    fn record(&mut self, change: DeadlineAdjustment) {
        self.adjusted.insert(change.run.clone());
        self.capped += 1;
        if self.adjustments.len() < REBASE_ADJUSTMENTS_LISTED {
            self.adjustments.push(change);
        }
    }

    fn adjust_run(&mut self, state: &mut State, id: &str, to_ms: i64) {
        let Some(mut run) = state.runs.get(id).filter(|r| !r.status.terminal()).cloned() else {
            return;
        };
        let playbook = state
            .artifacts
            .get(&run.artifact)
            .map(|a| &a.content.playbook);
        let mut timers = RunTimers {
            id,
            to_ms,
            report: self,
        };
        let before = timers.report.capped;
        timers.cap(
            Timer::DurationBasis(id),
            &mut run.admitted_at_ms,
            playbook.map(|_| 0),
        );
        let op = playbook
            .and_then(|p| p.steps.get(&run.step))
            .map(|step| &step.op);
        match &mut run.pending {
            Some(Pending::Event { wait }) => timers.wait(&mut state.waits, wait, playbook),
            Some(Pending::Join { deadline, .. }) => {
                let duration = match op {
                    Some(Operation::Join { deadline_ms, .. }) => Some(*deadline_ms),
                    _ => None,
                };
                timers.cap(Timer::Join(id), deadline, duration);
            }
            Some(Pending::Retry { deadline }) => {
                let duration = match op {
                    Some(Operation::Invoke {
                        retry: Some(policy),
                        ..
                    }) => Some(policy.backoff_ms),
                    _ => None,
                };
                timers.cap(Timer::Retry(id), deadline, duration);
            }
            _ => {}
        }
        if timers.report.capped != before {
            state.runs.insert(id.into(), run);
            indexes::mark_run(state, id);
        }
    }

    fn adjust_intent(&mut self, state: &mut State, id: &str, to_ms: i64) {
        let Some(mut intent) = state.intents.get(id).cloned() else {
            return;
        };
        let run = intent.run.clone();
        let mut timers = RunTimers {
            id: &run,
            to_ms,
            report: self,
        };
        if timers.cap(
            Timer::Operation(id),
            &mut intent.deadline_at_ms,
            Some(intent.command.deadline_ms),
        ) {
            state.intents.insert(id.into(), intent);
            indexes::mark_intent(state, id);
            indexes::mark_run(state, &run);
        }
    }
}

struct RunTimers<'a> {
    id: &'a str,
    to_ms: i64,
    report: &'a mut Deadlines,
}

enum Timer<'a> {
    DurationBasis(&'a str),
    Join(&'a str),
    Retry(&'a str),
    Wait(&'a str),
    Operation(&'a str),
}

impl<'a> Timer<'a> {
    fn audit_fields(self) -> (&'static str, &'a str) {
        match self {
            Self::DurationBasis(id) => ("duration_basis", id),
            Self::Join(id) => ("join", id),
            Self::Retry(id) => ("retry", id),
            Self::Wait(id) => ("wait", id),
            Self::Operation(id) => ("operation", id),
        }
    }
}

impl RunTimers<'_> {
    fn cap(&mut self, timer: Timer<'_>, value: &mut i64, duration: Option<u64>) -> bool {
        if *value <= self.to_ms {
            return false;
        }
        let Some(duration) = duration else {
            self.report.unknown.insert(self.id.into());
            return false;
        };
        let limit = self
            .to_ms
            .saturating_add(i64::try_from(duration).unwrap_or(i64::MAX));
        if *value <= limit {
            return false;
        }
        let (kind, item) = timer.audit_fields();
        self.report.record(DeadlineAdjustment {
            run: self.id.into(),
            kind: kind.into(),
            item: item.into(),
            original_ms: *value,
            capped_ms: limit,
        });
        *value = limit;
        true
    }

    fn wait(
        &mut self,
        waits: &mut LazyMap<String, WaitRecord>,
        id: &str,
        playbook: Option<&Playbook>,
    ) {
        let Some(mut wait) = waits.get(id).cloned() else {
            return;
        };
        let duration =
            playbook
                .and_then(|p| p.steps.get(&wait.step))
                .and_then(|step| match &step.op {
                    Operation::Wait { deadline_ms, .. } => Some(*deadline_ms),
                    _ => None,
                });
        if self.cap(Timer::Wait(id), &mut wait.deadline, duration) {
            waits.insert(id.into(), wait);
        }
    }
}
