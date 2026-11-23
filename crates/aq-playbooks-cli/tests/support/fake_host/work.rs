use super::*;

impl FakeHost {
    pub fn complete_author_work(
        &mut self,
        key: &str,
        body: String,
        visibility: &str,
    ) -> Result<(), String> {
        let work = self.work.get_mut(key).ok_or("unknown author work")?;
        if work.kind != "supervisor_authoring" || work.visibility != visibility {
            return Err("author result visibility mismatch".into());
        }
        if let Some(old) = &work.authored_result
            && old != &body
        {
            return Err("author result immutable".into());
        }
        work.authored_result = Some(body);
        Ok(())
    }

    /// Explicit trusted domain completion. Creating work never supplies a review
    /// disposition. Tests must attest the exact result and business disposition.
    pub fn complete_work(
        &mut self,
        reference: &str,
        disposition: &str,
        result_reference: &str,
    ) -> Result<EventEnvelope, String> {
        if !matches!(disposition, "accepted" | "changes_requested" | "failed") {
            return Err("unsupported work disposition".into());
        }
        let work = self
            .work
            .values_mut()
            .find(|work| work.reference == reference)
            .ok_or("unknown host work handle")?;
        let output =
            json!({"reference":result_reference,"subject":work.subject,"disposition":disposition});
        if let Some(old) = &work.completion
            && old != &output
        {
            return Err("work completion is immutable".into());
        }
        work.completion = Some(output.clone());
        let subject = work.subject.clone();
        let completion = completed("completed", output.clone());
        for receipt in self.operations.values_mut() {
            if matches!(&receipt.result,DispatchResult::Accepted{handle} if handle==reference) {
                receipt.result = completion.clone();
            }
        }
        self.completed_work_event(reference, subject, output)
    }

    fn completed_work_event(
        &self,
        reference: &str,
        subject: Subject,
        output: Value,
    ) -> Result<EventEnvelope, String> {
        let contract = catalog::catalog().events["aq.operation.updated"].clone();
        Ok(EventEnvelope {
            producer: "aq.host.bridge@1".into(),
            event_id: format!("completion:{reference}"),
            contract: ContractRef {
                name: contract.name.clone(),
                fingerprint: contract.fingerprint().map_err(|e| e.to_string())?,
            },
            scope: "project".into(),
            correlation: BTreeMap::from([("operation".into(), reference.into())]),
            subject: Some(subject.id),
            revision: Some(subject.revision),
            causation: None,
            occurred_at_ms: Some(self.now_ms as i64),
            observed_at_ms: Some(self.now_ms as i64),
            payload: json!({"outcome":"completed","output":output}),
        })
    }

    pub(super) fn ensure_work(&mut self, intent: &Intent) -> Result<DispatchResult, String> {
        let input = &intent.input;
        let actor = &intent.authority.actor;
        let subject = subject_field(input, "subject")?;
        self.check_current(&subject)?;
        let visibility = field(input, "visibility")?;
        if !self.visible(actor, visibility) {
            return Err("work visibility refused".into());
        }
        let context = self
            .observations
            .get(field(input, "context")?)
            .ok_or("unknown frozen work context")?;
        if !context.available_to((&intent.authority.scope, visibility), &subject, self.now_ms) {
            return Err("work context scope, subject, freshness or visibility mismatch".into());
        }
        let key = format!(
            "{}/{}",
            intent.authority.scope,
            field(input, "logical_key")?
        );
        let request_digest = digest(input).map_err(|e| e.to_string())?;
        if let Some(old) = self.work.get(&key) {
            if old.request_digest != request_digest {
                return Err("logical work key content conflict".into());
            }
            return Ok(old.response());
        }
        let work = Work {
            reference: format!("work:{}", intent.id),
            subject: subject.clone(),
            kind: field(input, "kind")?.into(),
            visibility: visibility.into(),
            request_digest,
            authored_result: None,
            lifetime: field(input, "lifetime")?.into(),
            operation_id: intent.id.clone(),
            completion: None,
            cancelled: false,
        };
        let response = work.response();
        self.work.insert(key, work);
        Ok(response)
    }
    pub(super) fn change_work(&mut self, intent: &Intent) -> Result<DispatchResult, String> {
        let input = &intent.input;
        let subject = subject_field(input, "subject")?;
        if !self.current(&subject) {
            return Ok(completed("conflict", result("stale-subject", &subject)));
        }
        match field(input, "variant")? {
            "hold" => self.hold(intent, &subject)?,
            "release_hold" => self.release_hold(intent, &subject)?,
            _ => {
                self.subjects
                    .insert(subject.id.clone(), subject.revision + 1);
            }
        }
        Ok(completed(
            "changed",
            result(
                format!("change:{}", intent.id),
                &Subject {
                    id: subject.id.clone(),
                    revision: self.subjects[&subject.id],
                },
            ),
        ))
    }
}

impl FakeHost {
    fn hold(&mut self, intent: &Intent, subject: &Subject) -> Result<(), String> {
        let id = intent.input["hold_id"]
            .as_str()
            .ok_or("hold identity required")?;
        let hold = Hold {
            subject: subject.clone(),
            actor: intent.authority.actor.clone(),
            scope: intent.authority.scope.clone(),
        };
        if self.holds.get(id).is_some_and(|old| !old.same_owner(&hold)) {
            return Err("hold identity owned elsewhere".into());
        }
        self.holds.insert(id.into(), hold);
        Ok(())
    }
    fn release_hold(&mut self, intent: &Intent, subject: &Subject) -> Result<(), String> {
        let id = intent.input["hold_id"]
            .as_str()
            .ok_or("hold identity required")?;
        let hold = self.holds.get(id).ok_or("unknown hold")?;
        if !hold.can_release(&intent.authority, subject) {
            return Err("hold release authority refused".into());
        }
        self.holds.remove(id);
        Ok(())
    }
}
impl Work {
    fn response(&self) -> DispatchResult {
        if self.lifetime != "owned" {
            return completed("ensured", result(&self.reference, &self.subject));
        }
        match &self.completion {
            Some(output) => completed("completed", output.clone()),
            None => DispatchResult::Accepted {
                handle: self.reference.clone(),
            },
        }
    }
}
impl Hold {
    fn same_owner(&self, other: &Hold) -> bool {
        self.actor == other.actor && self.scope == other.scope && self.subject == other.subject
    }
    fn can_release(&self, authority: &Authority, subject: &Subject) -> bool {
        self.actor == authority.actor
            && self.scope == authority.scope
            && self.subject.id == subject.id
    }
}
impl Observation {
    fn available_to(&self, (scope, visibility): (&str, &str), subject: &Subject, now: u64) -> bool {
        self.visibility == visibility
            && self.scope == scope
            && &self.subject == subject
            && self.expires_at_ms > now
    }
}
