//! Fairly alternate durable run transitions and external effect dispatch.
use super::*;

impl<S: Storage> Engine<S> {
    pub fn drive(
        &self,
        host: &mut impl Host,
        now_ms: i64,
        max_transitions: usize,
    ) -> Result<usize> {
        let mut total = 0;
        while total < max_transitions {
            self.wake_due(now_ms)?;
            let Some(mut turn) = self.next_turn()? else {
                break;
            };
            match self.transaction_at(Some(now_ms), |state| turn.advance(state, now_ms)) {
                Ok(Progress::Effect) => {
                    self.dispatch_next(host, now_ms)?;
                }
                Ok(Progress::Run(Some(elapsed))) => self.metrics.record_queue(elapsed),
                Ok(Progress::Run(None)) => {}
                Err(error) => {
                    turn.requeue_after_failure(self, &error, now_ms);
                    return Err(error);
                }
            }
            total += 1;
        }
        Ok(total)
    }

    fn wake_due(&self, now_ms: i64) -> Result<()> {
        let due = self.query(&IndexQuery::DueTimers { now_ms, limit: 1 })?;
        let completions = self.has_effect(EffectQueueKind::Completion)?;
        if due.items.is_empty() && !completions {
            return Ok(());
        }
        self.transaction_at(Some(now_ms), |state| {
            process_completions(state)?;
            process_operation_deadlines(state, now_ms);
            wake_timers(state, now_ms);
            Ok(())
        })
    }

    fn has_effect(&self, kind: EffectQueueKind) -> Result<bool> {
        let page = self.query(&IndexQuery::Effects {
            kind,
            scope_after: None,
            limit: 1,
        })?;
        Ok(!page.items.is_empty())
    }

    fn next_turn(&self) -> Result<Option<Turn>> {
        let ready = self
            .query(&IndexQuery::Ready {
                scope_after: None,
                limit: 1,
            })?
            .items
            .into_iter()
            .next();
        let dispatch = self.has_effect(EffectQueueKind::Dispatch)?;
        let cancel = self.has_effect(EffectQueueKind::Cancel)?;
        let has_effect = dispatch || cancel;
        if ready.is_none() && !has_effect {
            return Ok(None);
        }
        Ok(Some(Turn {
            ready,
            has_effect,
            advancing: false,
        }))
    }
}

fn wake_timers(state: &mut State, now_ms: i64) {
    let timers = state
        .indexes
        .query(state, &IndexQuery::DueTimers { now_ms, limit: 64 })
        .items;
    for item in timers {
        if let Some(timer) = state.indexes.timers.get(&item.key).cloned() {
            indexes::remove_timer(state, &item.key);
            if timer.intent.is_none() {
                indexes::wake(state, &timer.run);
            }
        }
    }
}

struct Turn {
    ready: Option<IndexItem>,
    has_effect: bool,
    advancing: bool,
}
enum Progress {
    Effect,
    Run(Option<u64>),
}

impl Turn {
    fn advance(&mut self, state: &mut State, now_ms: i64) -> Result<Progress> {
        let effect_turn = state.indexes.dispatch_turn || self.ready.is_none();
        if self.has_effect && effect_turn {
            state.indexes.dispatch_turn = false;
            return Ok(Progress::Effect);
        }
        let Some(item) = &self.ready else {
            return Ok(Progress::Run(None));
        };
        let queued = if state.indexes.ready_tokens.contains_key(&item.key) {
            self.advancing = true;
            Self::advance_run(state, item, now_ms)?
        } else {
            None
        };
        state.indexes.dispatch_turn = true;
        Ok(Progress::Run(queued))
    }

    fn advance_run(state: &mut State, item: &IndexItem, now_ms: i64) -> Result<Option<u64>> {
        state.indexes.scope_cursor = Some(item.scope.clone());
        indexes::unqueue(state, &item.key);
        let elapsed = state
            .runs
            .get(&item.key)
            .filter(|run| run.visits == 0)
            .map(|run| now_ms.saturating_sub(run.admitted_at_ms).max(0) as u64);
        Ok(if advance(state, &item.key, now_ms)? {
            elapsed
        } else {
            None
        })
    }

    fn requeue_after_failure<S: Storage>(&self, engine: &Engine<S>, error: &Error, now_ms: i64) {
        // Move a failed step behind other ready runs. A write race or lost lease
        // belongs to the storage owner, so the run keeps its place.
        if !self.advancing
            || matches!(
                error,
                Error::Storage(StorageError::Conflict | StorageError::Ownership)
            )
        {
            return;
        }
        let Some(item) = &self.ready else {
            return;
        };
        let _ = engine.transaction_at(Some(now_ms), |state| {
            if state.indexes.ready_tokens.contains_key(&item.key) {
                indexes::unqueue(state, &item.key);
                indexes::wake(state, &item.key);
                state.indexes.scope_cursor = Some(item.scope.clone());
            }
            Ok(())
        });
    }
}
