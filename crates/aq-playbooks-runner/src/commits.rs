//! Execute the same transition once on either a record backend or snapshot CAS.
use super::*;
use std::time::Instant;

impl Finish {
    fn complete(self, state: &mut State, sequence: u64) -> Result<retention::CommitReport> {
        let report = match self {
            Self::Host(now_ms) => {
                let candidates = retention::candidate_roots(state);
                indexes::sync(state)?;
                retention::finish_commit(state, now_ms, candidates, sequence)?
            }
            Self::Licensed => {
                indexes::sync(state)?;
                retention::finish_licensed_commit(state, sequence)?
            }
        };
        indexes::sync(state)?;
        Ok(report)
    }
}

struct Plan<'a, F, T> {
    transition: Option<F>,
    mode: Finish,
    protection: bool,
    metrics: &'a metrics::Metrics,
    value: Option<T>,
    report: retention::CommitReport,
}

impl<F, T> Plan<'_, F, T>
where
    F: FnOnce(&mut State) -> Result<(T, bool)>,
{
    fn apply(&mut self, state: &mut State) -> Result<bool> {
        let transition = self
            .transition
            .take()
            .ok_or_else(|| Error::Conflict("transition plan ran twice".into()))?;
        state.protection_available = self.protection;
        let sequence = state.sequence;
        let started = Instant::now();
        let evaluated = transition(state);
        self.metrics
            .record_time(metrics::Timing::Transition, started.elapsed());
        let (value, changed) = evaluated?;
        self.value = Some(value);
        if changed {
            self.report = self.mode.complete(state, sequence)?;
        }
        Ok(changed)
    }

    fn result(self) -> Result<(T, retention::CommitReport)> {
        let value = self
            .value
            .ok_or_else(|| Error::Conflict("transition plan did not run".into()))?;
        Ok((value, self.report))
    }
}

impl<S: Storage> Engine<S> {
    /// One commit, and what its retention work moved out of the State. When
    /// the plan changes the State, both paths run `candidate_roots`,
    /// `indexes::sync`, `finish_commit` (clock, settled cohorts, admin
    /// receipts) and `indexes::sync` again, so a cohort leaves in the commit
    /// that settles it.
    pub(super) fn transaction_report_at<T>(
        &self,
        mode: Finish,
        f: impl FnOnce(&mut State) -> Result<(T, bool)>,
    ) -> Result<(T, retention::CommitReport)> {
        let mut plan = Plan {
            transition: Some(f),
            mode,
            protection: self.store.protection_available(),
            metrics: &self.metrics,
            value: None,
            report: retention::CommitReport::default(),
        };
        let started = Instant::now();
        let outcome = self.store.transition_v1(
            &self.namespace,
            &self.owner,
            &mut TransitionRequest {
                operation: "transaction",
                plan: &mut |state| plan.apply(state),
            },
        )?;
        if outcome != TransitionOutcome::Unsupported {
            self.metrics
                .record_time(metrics::Timing::Commit, started.elapsed());
            return plan.result();
        }
        if plan.transition.is_none() {
            return Err(Error::Conflict(
                "unsupported transition ran the plan".into(),
            ));
        }
        let mut loaded = self.load_transition_state()?;
        if plan.apply(&mut loaded.state)? {
            let started = Instant::now();
            let committed = self.store.compare_and_swap(
                &self.namespace,
                &self.owner,
                loaded.revision,
                &loaded.state,
            );
            self.metrics
                .record_time(metrics::Timing::Commit, started.elapsed());
            committed?;
        }
        plan.result()
    }

    fn load_transition_state(&self) -> Result<VersionedState> {
        let started = Instant::now();
        let loaded = self.store.load(&self.namespace);
        self.metrics
            .record_time(metrics::Timing::Load, started.elapsed());
        let mut loaded = loaded?;
        if !loaded.state.indexes.initialized {
            indexes::rebuild(&mut loaded.state)?;
        }
        Ok(loaded)
    }
}
