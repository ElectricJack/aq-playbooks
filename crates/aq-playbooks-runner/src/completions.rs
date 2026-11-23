//! Match retained events to pinned asynchronous commands and persist their results.
use super::*;

pub(super) fn process_completions(s: &mut State) -> Result<()> {
    let ids = s
        .indexes
        .completion_ready
        .iter()
        .take(64)
        .cloned()
        .collect::<Vec<_>>();
    let pending = ids
        .iter()
        .filter_map(|id| s.intents.get(id))
        .filter(|intent| intent.completion_event.is_none() && intent.handle.is_some())
        .cloned()
        .collect::<Vec<_>>();
    for id in ids {
        s.indexes.completion_ready.remove(&id);
    }
    for intent in pending {
        if let Some(contract) = &intent.command.asynchronous
            && let Some(event) = completion_event(s, &intent, contract)?
        {
            apply(s, &intent, contract, event)?;
        }
    }
    Ok(())
}

fn completion_event(
    s: &State,
    intent: &Intent,
    contract: &AsyncContract,
) -> Result<Option<AcceptedEvent>> {
    let artifact = &s.artifacts[&s.runs[&intent.run].artifact];
    let expected = artifact
        .content
        .registry
        .events
        .get(&contract.completion_event)
        .ok_or_else(|| Error::Refused("missing pinned completion contract".into()))?;
    let reference = ContractRef {
        name: expected.name.clone(),
        fingerprint: expected.fingerprint()?,
    };
    let correlation = indexes::field_key(
        &reference,
        &intent.authority.scope,
        &contract.correlation_field,
        intent.handle.as_deref().unwrap_or_default(),
    )?;
    let keys = s
        .indexes
        .events
        .get(&correlation)
        .cloned()
        .unwrap_or_default();
    let matches = |event: &&AcceptedEvent| {
        let envelope = &event.envelope;
        let trusted = expected.trusted_producers.contains(&envelope.producer);
        let handle =
            envelope.correlation.get(&contract.correlation_field) == intent.handle.as_ref();
        envelope.contract == reference
            && envelope.scope == intent.authority.scope
            && trusted
            && handle
    };
    Ok(keys
        .values()
        .filter_map(|id| s.events.get(id))
        .filter(matches)
        .min_by_key(|event| event.sequence)
        .cloned())
}

fn apply(s: &mut State, i: &Intent, contract: &AsyncContract, event: AcceptedEvent) -> Result<()> {
    if contract
        .completion
        .validate(&event.envelope.payload)
        .is_err()
    {
        invalid(s, i, event);
        return Ok(());
    }
    let result = decode(&event.envelope.payload)?;
    record_result(s, &i.id, result, event.accepted_at_ms)?;
    s.intents.get_mut(&i.id).unwrap().completion_event = Some(event.key);
    Ok(())
}

fn decode(payload: &Value) -> Result<DispatchResult> {
    let outcome = payload
        .get("outcome")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Refused("async completion missing outcome".into()))?;
    let output = payload
        .get("output")
        .cloned()
        .ok_or_else(|| Error::Refused("async completion missing output".into()))?;
    Ok(DispatchResult::Completed {
        outcome: outcome.into(),
        output,
    })
}

fn invalid(s: &mut State, i: &Intent, event: AcceptedEvent) {
    const REASON: &str = "invalid asynchronous completion requires reconciliation";
    let current = s.intents.get_mut(&i.id).unwrap();
    if current.status != IntentStatus::Cancelled {
        current.status = IntentStatus::Unknown;
    }
    current.reason = Some(REASON.into());
    current.completion_event = Some(event.key);
    indexes::mark_intent(s, &i.id);
    let run = s.runs.get_mut(&i.run).unwrap();
    if !run.status.terminal() && !matches!(run.status, RunStatus::Finishing | RunStatus::Cancelling)
    {
        run.status = RunStatus::Blocked;
    }
    if !run.status.terminal() {
        run.reason = Some(REASON.into());
    }
    i.receipt_at(event.accepted_at_ms)
        .record(s, "invalid_output", json!({"operation":i.id}));
}
