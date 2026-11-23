use super::*;

impl FakeHost {
    /// Freeze observations and episode revisions before scanning. Pages never
    /// acquire authority merely because the workflow later sees another page.
    pub fn capture_observation(&mut self, observation: Observation) -> Result<(), String> {
        // This fake registers only the catalog's one project scope. A production
        // multi-scope host must namespace all domain keys by authenticated scope.
        if !observation.supported_partition() {
            return Err("unsupported observation scope or partition".into());
        }
        if !observation.bounded() {
            return Err("invalid bounded observation".into());
        }
        if self.observations.contains_key(&observation.reference) {
            return Err("observation references are immutable".into());
        }
        let mut observation = observation;
        observation.episode_revisions = self.episode_revisions(&observation.partition);
        self.observations
            .insert(observation.reference.clone(), observation);
        Ok(())
    }

    fn episode_revisions(&self, partition: &str) -> BTreeMap<String, u64> {
        let prefix = format!("{partition}/");
        self.episodes
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(key, e)| (key.clone(), e.revision))
            .collect()
    }
    /// Record a trusted evaluator's exact bounded findings before a workflow
    /// asks for reconciliation. A caller-supplied evaluation string is not proof.
    pub fn record_evaluation(
        &mut self,
        reference: &str,
        observation: &str,
        findings: &Value,
    ) -> Result<(), String> {
        let scan = self
            .observations
            .get(observation)
            .ok_or("unknown evaluation observation")?;
        let items = findings
            .as_array()
            .ok_or("evaluation findings must be an array")?;
        if items.len() > 100 || self.evaluations.contains_key(reference) {
            return Err("evaluation bound or immutable identity conflict".into());
        }
        let evaluation = Evaluation {
            reference: reference.into(),
            observation: observation.into(),
            partition: scan.partition.clone(),
            scope: scan.scope.clone(),
            findings_digest: digest(findings).map_err(|e| e.to_string())?,
        };
        self.evaluations.insert(reference.into(), evaluation);
        Ok(())
    }

    pub(super) fn read_context(&mut self, intent: &Intent) -> Result<DispatchResult, String> {
        let input = &intent.input;
        let mut subject = subject_field(input, "subject")?;
        let requested_snapshot = field(input, "snapshot")?;
        let reference = if requested_snapshot.is_empty() {
            self.fresh_snapshot(intent, &mut subject)?
        } else {
            requested_snapshot.into()
        };
        let observation = self
            .observations
            .get(&reference)
            .ok_or("unknown frozen snapshot")?;
        let same_subject =
            subject == observation.subject && observation.scope == intent.authority.scope;
        if !same_subject || observation.view != field(input, "view")? {
            return Err("snapshot subject, view, scope or visibility mismatch".into());
        }
        if !self.visible(&intent.authority.actor, &observation.visibility) {
            return Err("snapshot subject, view, scope or visibility mismatch".into());
        }
        observation.page(input, subject, self.now_ms)
    }
    fn fresh_snapshot(&mut self, intent: &Intent, subject: &mut Subject) -> Result<String, String> {
        let input = &intent.input;
        let eligible = field(input, "view")? == "project"
            && input["page"].as_u64() == Some(0)
            && subject.id == "main"
            && intent.authority.scope == "project"
            && self.visible(&intent.authority.actor, "project");
        if !eligible {
            return Err("fresh snapshots support only authorized project/main page zero".into());
        }
        subject.revision = *self.subjects.get("main").ok_or("unknown base subject")?;
        let reference = format!("base-snapshot:{}", intent.id);
        let expires_at_ms = self
            .now_ms
            .checked_add(60_000)
            .ok_or("observation expiry overflow")?;
        self.capture_observation(Observation {
            reference: reference.clone(),
            partition: "base-main".into(),
            scope: "project".into(),
            visibility: "project".into(),
            view: "project".into(),
            subject: subject.clone(),
            pages: vec![json!({"subject":subject})],
            complete: true,
            observed_at_ms: self.now_ms,
            expires_at_ms,
            episode_revisions: BTreeMap::new(),
        })?;
        Ok(reference)
    }
}

impl Observation {
    fn supported_partition(&self) -> bool {
        self.scope == "project" && !self.partition.is_empty() && !self.partition.contains('/')
    }
    fn bounded(&self) -> bool {
        (1..=100).contains(&self.pages.len()) && self.expires_at_ms > self.observed_at_ms
    }
    fn page(&self, input: &Value, subject: Subject, now_ms: u64) -> Result<DispatchResult, String> {
        let page = input["page"].as_u64().ok_or("page required")? as usize;
        if page >= self.pages.len() {
            return Err("page outside frozen snapshot".into());
        }
        let next = if page + 1 < self.pages.len() {
            Some(page + 1)
        } else {
            None
        };
        let complete = self.complete && next.is_none();
        Ok(completed(
            self.status(now_ms, complete),
            json!({"reference":format!("{}#{page}",self.reference),"subject":subject,"snapshot":self.reference,"complete":complete,"next_page":next,"visibility":self.visibility,"observed_at_ms":self.observed_at_ms,"expires_at_ms":self.expires_at_ms}),
        ))
    }
    fn status(&self, now_ms: u64, complete: bool) -> &'static str {
        if now_ms >= self.expires_at_ms {
            "expired"
        } else if complete {
            "observed"
        } else {
            "incomplete"
        }
    }
}
