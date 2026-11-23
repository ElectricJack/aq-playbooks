use super::*;

impl FakeHost {
    pub(super) fn request_check(&mut self, intent: &Intent) -> Result<DispatchResult, String> {
        let input = &intent.input;
        let subject = subject_field(input, "subject")?;
        self.check_current(&subject)?;
        let suite = field(input, "suite")?;
        let Some(outcome) = self
            .configured_outcomes
            .get(&Self::check_key(&subject, suite))
            .cloned()
        else {
            return Ok(DispatchResult::Unknown {
                reason: "missing fake check outcome".into(),
            });
        };
        if !matches!(
            outcome.as_str(),
            "passed" | "failed" | "infrastructure_failure" | "refused"
        ) {
            return Err("invalid configured check outcome".into());
        }
        let reference = format!("check:{}", intent.id);
        self.evidence.insert(
            reference.clone(),
            Evidence {
                subject: subject.clone(),
                suite: suite.into(),
                passed: outcome == "passed",
                issuer: "synthetic-check-service".into(),
            },
        );
        Ok(completed(&outcome, result(reference, &subject)))
    }
    pub(super) fn prepare_candidate(&mut self, intent: &Intent) -> Result<DispatchResult, String> {
        let input = &intent.input;
        let base = subject_field(input, "base")?;
        let sources: Vec<Subject> = serde_json::from_value(input["sources"].clone())
            .map_err(|_| "invalid candidate sources")?;
        if !self.current(&base) || sources.iter().any(|s| !self.current(s)) {
            return Ok(completed(
                "conflict",
                result("stale-candidate-input", &base),
            ));
        }
        let subject = Subject {
            id: format!("candidate:{}", intent.id),
            revision: 1,
        };
        self.subjects.insert(subject.id.clone(), 1);
        self.candidates.insert(
            subject.id.clone(),
            Candidate {
                subject: subject.clone(),
                base,
                sources,
            },
        );
        let mut output = result(&subject.id, &subject);
        if intent.command.version == catalog::VERSION_2_1 {
            // 2.1.0 returns the immutable candidate OID that checks bind.
            let hex = aq_playbooks_model::hash_bytes(subject.id.as_bytes());
            output["source"] =
                json!({"format": "sha256", "hex": hex.trim_start_matches("sha256:")});
        }
        Ok(completed("prepared", output))
    }
    pub(super) fn publish_candidate(&mut self, intent: &Intent) -> Result<DispatchResult, String> {
        let input = &intent.input;
        let candidate = subject_field(input, "candidate")?;
        let destination = subject_field(input, "destination")?;
        let Some(record) = self.candidates.get(&candidate.id) else {
            return Err("unknown immutable candidate".into());
        };
        if !self.publishable(record, &candidate, &destination) {
            return Ok(completed(
                "conflict",
                result("publication-precondition", &candidate),
            ));
        }
        if self.holds.values().any(|h| {
            h.scope == intent.authority.scope
                && (h.subject.id == candidate.id || h.subject.id == destination.id)
        }) {
            return Err("publication blocked by an active hold".into());
        }
        let evidence: Vec<Subject> = serde_json::from_value(input["evidence"].clone())
            .map_err(|_| "invalid evidence subjects")?;
        if !self.passing_evidence(&candidate, &evidence) {
            return Err("passing evidence for this exact candidate revision required".into());
        }
        self.subjects
            .insert(destination.id.clone(), destination.revision + 1);
        Ok(completed(
            "published",
            result(format!("publication:{}", intent.id), &candidate),
        ))
    }
    fn publishable(&self, record: &Candidate, candidate: &Subject, destination: &Subject) -> bool {
        &record.subject == candidate
            && &record.base == destination
            && self.current(candidate)
            && self.current(destination)
    }
    fn passing_evidence(&self, candidate: &Subject, references: &[Subject]) -> bool {
        let passed = self
            .evidence
            .values()
            .any(|e| &e.subject == candidate && e.passed && e.issuer == "synthetic-check-service");
        !references.is_empty() && references.iter().all(|r| r == candidate) && passed
    }
}
