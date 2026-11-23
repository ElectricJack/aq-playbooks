//! Persist each effect handoff and retain its queue cursor before calling the host.
use super::*;
mod recovery;

impl EffectQueueKind {
    fn query_after(self, state: &State, limit: usize) -> IndexQuery {
        IndexQuery::Effects {
            kind: self,
            scope_after: state.indexes.effect_cursor.get(self.key()).cloned(),
            limit,
        }
    }

    fn advance_cursor(self, state: &mut State, item: &IndexItem) {
        let name = self.key();
        state
            .indexes
            .effect_item_cursor
            .insert(format!("{name}:{}", item.scope), item.key.clone());
        state
            .indexes
            .effect_cursor
            .insert(name.into(), item.scope.clone());
    }

    fn next(self, state: &mut State) -> Option<String> {
        let item = state
            .indexes
            .query(state, &self.query_after(state, 1))
            .items
            .into_iter()
            .next()?;
        self.advance_cursor(state, &item);
        indexes::mark_intent(state, &item.key);
        Some(item.key)
    }
}

impl Intent {
    fn permitted(&self, state: &State, host: &impl Host) -> Result<bool> {
        Ok(host.authorize(&self.authority, &self.command)
            && execution_allowed(
                state,
                &state.runs[&self.run].artifact,
                &self.authority,
                &self.command.capabilities,
            )?)
    }
}

impl<S: Storage> Engine<S> {
    /// Persist dispatch state before handing anything to a host. Crashes here leave
    /// a recoverable ambiguity rather than an apparently safe fresh invocation.
    pub fn dispatch_next(&self, host: &mut impl Host, now_ms: i64) -> Result<Option<String>> {
        let cancellation =
            self.transaction_at(Some(now_ms), |s| prepare_cancel(s, host, now_ms))?;
        if let Some(intent) = cancellation {
            let disposition = self.measure_host(|| host.cancel(&intent));
            self.transaction_at(Some(now_ms), |s| {
                record_cancellation(s, &intent, disposition, now_ms);
                Ok(())
            })?;
            return Ok(Some(intent.id));
        }
        let selected = self.transaction_at(Some(now_ms), |s| prepare_dispatch(s, host, now_ms))?;
        let Some(intent) = selected else {
            return Ok(None);
        };
        if intent.status != IntentStatus::Rejected {
            let result = self.measure_host(|| host.dispatch(&intent));
            self.record_result(&intent.id, result, now_ms)?;
        }
        Ok(Some(intent.id))
    }
}

fn prepare_cancel(s: &mut State, host: &impl Host, now_ms: i64) -> Result<Option<Intent>> {
    let Some(id) = EffectQueueKind::Cancel.next(s) else {
        return Ok(None);
    };
    let intent = s.intents.get_mut(&id).unwrap();
    let allowed = intent.command.cancellation && host.authorize(&intent.authority, &intent.command);
    intent.cancel_dispatched = true;
    if !allowed {
        return Ok(None);
    }
    let intent = intent.clone();
    intent
        .receipt_at(now_ms)
        .record(s, "cancel_dispatch", json!({"operation":id}));
    Ok(Some(intent))
}

fn record_cancellation(
    s: &mut State,
    intent: &Intent,
    disposition: CancellationResult,
    now_ms: i64,
) {
    indexes::mark_intent(s, &intent.id);
    if matches!(disposition, CancellationResult::Confirmed) {
        s.intents.get_mut(&intent.id).unwrap().status = IntentStatus::Cancelled;
    }
    intent.receipt_at(now_ms).record(
        s,
        "cancel_response",
        json!({"operation":intent.id,"disposition":disposition}),
    );
}

fn prepare_dispatch(s: &mut State, host: &impl Host, now_ms: i64) -> Result<Option<Intent>> {
    let Some(id) = EffectQueueKind::Dispatch.next(s) else {
        return Ok(None);
    };
    let intent = s.intents[&id].clone();
    if now_ms >= intent.deadline_at_ms {
        expire(s, &id, &intent, now_ms);
        return Ok(Some(s.intents[&id].clone()));
    }
    if !intent.permitted(s, host)? {
        block(s, &id, &intent, now_ms);
        return Ok(None);
    }
    s.intents.get_mut(&id).unwrap().status = IntentStatus::Dispatched;
    intent
        .receipt_at(now_ms)
        .record(s, "dispatch", json!({"operation":id}));
    // The first dispatch counts: a re-dispatch after recovery is the
    // same idempotent effect, whose completion may answer the first.
    let sequence = s.sequence;
    s.intents
        .get_mut(&id)
        .unwrap()
        .dispatch_sequence
        .get_or_insert(sequence);
    Ok(Some(s.intents[&id].clone()))
}

fn expire(s: &mut State, id: &str, intent: &Intent, now_ms: i64) {
    let expired = s.intents.get_mut(id).unwrap();
    expired.status = IntentStatus::Rejected;
    expired.reason = Some("command deadline expired before dispatch".into());
    intent.receipt_at(now_ms).record(
        s,
        "command_deadline_expired",
        json!({"operation":id,"issued":false}),
    );
}

fn block(s: &mut State, id: &str, intent: &Intent, now_ms: i64) {
    let i = s.intents.get_mut(id).unwrap();
    i.status = IntentStatus::Unknown;
    i.reason = Some("authority revoked".into());
    let run = s.runs.get_mut(&intent.run).unwrap();
    run.status = RunStatus::Blocked;
    run.reason = Some("authority revoked".into());
    intent
        .receipt_at(now_ms)
        .record(s, "authority_blocked", Value::Null);
}

pub(super) fn process_operation_deadlines(s: &mut State, now: i64) {
    let timers = s
        .indexes
        .query(
            s,
            &IndexQuery::DueTimers {
                now_ms: now,
                limit: 64,
            },
        )
        .items;
    let expired = timers
        .iter()
        .filter_map(|item| s.indexes.timers.get(&item.key))
        .filter_map(|timer| timer.intent.as_ref())
        .filter_map(|id| s.intents.get(id))
        .filter(|intent| {
            matches!(
                intent.status,
                IntentStatus::Dispatched | IntentStatus::Accepted
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    for intent in expired {
        indexes::remove_timer(s, &format!("effect:{}", intent.id));
        indexes::mark_intent(s, &intent.id);
        let current = s.intents.get_mut(&intent.id).unwrap();
        current.status = IntentStatus::Unknown;
        current.reason = Some("operation deadline elapsed with unresolved external outcome".into());
        intent.receipt_at(now).record(
            s,
            "command_deadline_expired",
            json!({"operation":intent.id,"issued":true}),
        );
    }
}
