use super::*;

impl FakeHost {
    /// The responder is an independently authenticated principal, never an
    /// approval flag supplied by the requesting workflow.
    pub fn complete_decision(
        &mut self,
        handle: &str,
        responder: &str,
        outcome: &str,
    ) -> Result<EventEnvelope, String> {
        let decision = self.decisions.get(handle).ok_or("unknown decision")?;
        if !decision.responders.contains(responder) || !matches!(outcome, "approved" | "rejected") {
            return Err("decision responder or outcome refused".into());
        }
        if decision.completion.is_none() {
            self.check_current(&decision.subject)?;
            if self.now_ms >= decision.expires_at_ms {
                return Err("decision deadline elapsed".into());
            }
        }
        self.settle_decision(handle, outcome)
    }

    pub(super) fn settle_decision(
        &mut self,
        handle: &str,
        outcome: &str,
    ) -> Result<EventEnvelope, String> {
        let decision = self.decisions.get_mut(handle).ok_or("unknown decision")?;
        let output = result(format!("decision:{handle}"), &decision.subject);
        if let Some((old, _)) = &decision.completion {
            if old != outcome {
                return Err("decision completion is immutable".into());
            }
        } else {
            decision.completion = Some((outcome.into(), output.clone()));
            decision.completed_at_ms = Some(self.now_ms);
        }
        if let Some(receipt) = self.operations.get_mut(handle) {
            receipt.result = completed(outcome, output.clone());
        }
        let contract = &catalog::catalog().events["aq.decision.completed"];
        Ok(EventEnvelope {
            producer: "aq.host.bridge@1".into(),
            event_id: format!("decision-completion:{handle}"),
            contract: ContractRef {
                name: contract.name.clone(),
                fingerprint: contract.fingerprint().map_err(|e| e.to_string())?,
            },
            scope: decision.scope.clone(),
            correlation: BTreeMap::from([("operation".into(), handle.into())]),
            subject: Some(decision.subject.id.clone()),
            revision: Some(decision.subject.revision),
            causation: Some(handle.into()),
            occurred_at_ms: decision.completed_at_ms.map(|at| at as i64),
            observed_at_ms: decision.completed_at_ms.map(|at| at as i64),
            payload: json!({"outcome":outcome,"output":output}),
        })
    }

    /// Host notification service accepts each identity exactly once. A lost
    /// acknowledgment leaves the outbox pending and safely retries that identity.
    pub fn deliver_notifications(
        &mut self,
        limit: usize,
        acknowledge: bool,
    ) -> Result<usize, String> {
        if limit > 1000 {
            return Err("delivery bound exceeded".into());
        }
        let mut count = 0;
        for notification in self
            .outbox
            .values_mut()
            .filter(|n| !n.acknowledged)
            .take(limit)
        {
            if let Some(old) = self.recipient_receipts.get(&notification.identity) {
                if old != &notification.body {
                    return Err("notification identity content conflict".into());
                }
            } else {
                self.recipient_receipts
                    .insert(notification.identity.clone(), notification.body.clone());
            }
            notification.acknowledged = acknowledge;
            count += 1;
        }
        Ok(count)
    }

    pub(super) fn send_message(&mut self, intent: &Intent) -> Result<DispatchResult, String> {
        let input = &intent.input;
        let actor = &intent.authority.actor;
        let visibility = field(input, "visibility")?;
        if !self.visible(actor, visibility) || !self.visible(field(input, "recipient")?, visibility)
        {
            return Err("message sender or recipient visibility refused".into());
        }
        let identity = field(input, "identity")?;
        if let Some(old) = self.messages.get(identity)
            && old != input
        {
            return Err("message identity content conflict".into());
        }
        self.messages.insert(identity.into(), input.clone());
        Ok(completed(
            "queued",
            result(
                identity,
                &Subject {
                    id: identity.into(),
                    revision: 1,
                },
            ),
        ))
    }
    pub(super) fn request_decision(&mut self, intent: &Intent) -> Result<DispatchResult, String> {
        let input = &intent.input;
        let actor = &intent.authority.actor;
        let subject = subject_field(input, "subject")?;
        self.check_current(&subject)?;
        let expires_at_ms = input["expires_at_ms"].as_u64().ok_or("invalid expiry")?;
        let responders: BTreeSet<String> = serde_json::from_value(input["responders"].clone())
            .map_err(|_| "invalid responders")?;
        let eligible = !responders.is_empty() && expires_at_ms > self.now_ms;
        if !eligible || input["choices"].as_array().is_none_or(Vec::is_empty) {
            return Err("decision needs eligible responders, choices and a future expiry".into());
        }
        self.decisions.insert(
            intent.id.clone(),
            Decision {
                subject,
                responders,
                expires_at_ms,
                actor: actor.clone(),
                scope: intent.authority.scope.clone(),
                completion: None,
                completed_at_ms: None,
            },
        );
        Ok(DispatchResult::Accepted {
            handle: intent.id.clone(),
        })
    }
    pub(super) fn request_judgment(&mut self, intent: &Intent) -> Result<DispatchResult, String> {
        let input = &intent.input;
        let subject = subject_field(input, "subject")?;
        self.check_current(&subject)?;
        if let Some(outcome) = self.configured_outcomes.get(&intent.id)
            && intent.command.outcomes.contains_key(outcome)
        {
            let mut output = result(format!("answer:{}", intent.id), &subject);
            if intent.command.name == "judgment.request" && outcome == "assessed" {
                let choice = self
                    .configured_choices
                    .get(&intent.id)
                    .ok_or("missing configured judgment choice")?;
                if !input["choices"]
                    .as_array()
                    .is_some_and(|choices| choices.iter().any(|v| v.as_str() == Some(choice)))
                {
                    return Err("judgment choice outside authorized options".into());
                }
                output["choice"] = choice.clone().into();
            }
            return Ok(completed(outcome, output));
        }
        Ok(DispatchResult::Unknown {
            reason: "missing fake authorized judgment/decision".into(),
        })
    }
}
