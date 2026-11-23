use super::*;

fn completion_contract(s: &State, i: &Intent, name: &str) -> Result<ContractRef> {
    let event = &s.artifacts[&s.runs[&i.run].artifact]
        .content
        .registry
        .events[name];
    Ok(ContractRef {
        name: event.name.clone(),
        fingerprint: event.fingerprint()?,
    })
}
impl IntentEntry<'_> {
    pub(super) fn index_completion(&self, s: &mut State) -> Result<()> {
        let Self { id, intent: i } = *self;
        let Some(asynchronous) = &i.command.asynchronous else {
            return Ok(());
        };
        if uncertain(i) && !hold_lapsed(s, i) {
            let contract = completion_contract(s, i, &asynchronous.completion_event)?;
            let key = trigger_key(&contract, &i.authority.scope)?;
            s.indexes
                .uncertain_completion_contracts
                .entry(key.clone())
                .or_default()
                .insert(id.into());
            s.indexes
                .uncertain_contract_membership
                .insert(id.into(), key);
        }

        if let Some(handle) = &i.handle
            && i.completion_event.is_none()
        {
            let contract = completion_contract(s, i, &asynchronous.completion_event)?;
            let key = field_key(
                &contract,
                &i.authority.scope,
                &asynchronous.correlation_field,
                handle,
            )?;
            self.subscribe_completion(s, key);
        }
        Ok(())
    }

    fn subscribe_completion(&self, s: &mut State, key: String) {
        let id = self.id;
        subscribe(s, &format!("intent:{id}"), key.clone());
        if s.indexes.events.get(&key).is_some_and(|e| !e.is_empty()) {
            s.indexes.completion_ready.insert(id.into());
        }
    }
}
