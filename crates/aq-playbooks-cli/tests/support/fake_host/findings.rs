use super::*;
impl FakeHost {
    pub(super) fn reconcile_findings(&mut self, intent: &Intent) -> Result<DispatchResult, String> {
        let partition = field(&intent.input, "partition")?;
        let observation = self.scan_for(intent, partition)?;
        self.check_evaluation(intent, &observation, partition)?;
        let findings = intent.input["findings"]
            .as_array()
            .ok_or("findings list required")?;
        if findings.len() > 100 {
            return Err("findings bound exceeded".into());
        }
        let reminder = intent.input["reminder_ms"]
            .as_u64()
            .ok_or("reminder required")?;
        let observed = observation.observed(findings)?;
        self.check_baselines(&observation, &observed)?;
        let prefix = format!("{partition}/");
        let mut keys: BTreeSet<_> = self
            .episodes
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .cloned()
            .collect();
        keys.extend(observed.keys().cloned());
        for key in keys {
            self.update_episode(
                key.clone(),
                observed.get(&key),
                (reminder, observation.complete),
            );
        }
        Ok(completed(
            "reconciled",
            result(
                format!("evaluation:{}", field(&intent.input, "evaluation")?),
                &observation.subject,
            ),
        ))
    }

    fn scan_for(&self, intent: &Intent, partition: &str) -> Result<Observation, String> {
        let observation = self
            .observations
            .get(field(&intent.input, "observation")?)
            .ok_or("unknown observation")?
            .clone();
        if !self.applicable(&observation, partition, &intent.authority) {
            return Err("observation expired, inapplicable or invisible".into());
        }
        Ok(observation)
    }
    fn applicable(
        &self,
        observation: &Observation,
        partition: &str,
        authority: &Authority,
    ) -> bool {
        observation.partition == partition
            && observation.scope == authority.scope
            && self.visible(&authority.actor, &observation.visibility)
            && observation.expires_at_ms > self.now_ms
            && self.current(&observation.subject)
    }
    fn check_evaluation(
        &self,
        intent: &Intent,
        observation: &Observation,
        partition: &str,
    ) -> Result<(), String> {
        let evaluation = self
            .evaluations
            .get(field(&intent.input, "evaluation")?)
            .ok_or("unknown host evaluation")?;
        let matches = evaluation.observation == observation.reference
            && evaluation.partition == partition
            && evaluation.scope == intent.authority.scope;
        if !matches
            || evaluation.findings_digest
                != digest(&intent.input["findings"]).map_err(|e| e.to_string())?
        {
            return Err("evaluation does not attest these observation findings".into());
        }
        Ok(())
    }
    fn check_baselines(
        &self,
        observation: &Observation,
        observed: &BTreeMap<String, String>,
    ) -> Result<(), String> {
        let prefix = format!("{}/", observation.partition);
        // Check every applicable baseline before mutating any episode, including
        // those absent from the findings (the ones a complete scan may clear).
        let stale = self
            .episodes
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .any(|(key, episode)| observation.revision(key) != episode.revision);
        if stale {
            return Err("stale pre-scan episode revision".into());
        }
        let missing = observed.keys().any(|key| {
            !self.episodes.contains_key(key) && observation.episode_revisions.contains_key(key)
        });
        if missing {
            return Err("missing retained episode".into());
        }
        Ok(())
    }
    fn update_episode(
        &mut self,
        key: String,
        next_evidence: Option<&String>,
        (reminder, complete): (u64, bool),
    ) {
        let old = self.episodes.get(&key).cloned();
        let kind = change(
            old.as_ref(),
            next_evidence,
            self.now_ms,
            (reminder, complete),
        );
        if let Some(kind) = kind {
            let revision = old.as_ref().map_or(1, |e| e.revision + 1);
            let evidence = next_evidence
                .cloned()
                .or_else(|| old.as_ref().map(|e| e.evidence.clone()))
                .unwrap_or_default();
            self.episodes.insert(
                key.clone(),
                Episode {
                    revision,
                    active: next_evidence.is_some(),
                    evidence: evidence.clone(),
                    last_notification_ms: self.now_ms,
                },
            );
            let identity = format!("finding/{key}/{revision}/{kind}");
            self.outbox.insert(
                identity.clone(),
                Notification {
                    identity,
                    body: format!("{kind}: {key}: {evidence}"),
                    acknowledged: false,
                },
            );
        }
    }
}

impl Observation {
    fn revision(&self, key: &str) -> u64 {
        self.episode_revisions.get(key).copied().unwrap_or(0)
    }
    fn observed(&self, findings: &[Value]) -> Result<BTreeMap<String, String>, String> {
        let mut observed = BTreeMap::new();
        for finding in findings {
            let finding_key = field(finding, "key")?;
            if finding_key.is_empty() || finding_key.contains('/') {
                return Err("invalid finding key".into());
            }
            let key = episode_key(&self.partition, finding_key);
            if observed
                .insert(key.clone(), field(finding, "evidence")?.to_string())
                .is_some()
            {
                return Err("duplicate finding identity".into());
            }
            let expected = finding["expected_revision"]
                .as_u64()
                .ok_or("expected episode revision required")?;
            if self.revision(&key) != expected {
                return Err("finding revision is not the pre-scan revision".into());
            }
        }
        Ok(observed)
    }
}

fn change(
    old: Option<&Episode>,
    next_evidence: Option<&String>,
    now_ms: u64,
    (reminder, complete): (u64, bool),
) -> Option<&'static str> {
    match (old, next_evidence) {
        (None, Some(_)) => Some("opened"),
        (Some(e), Some(v)) if !e.active || &e.evidence != v => Some("changed"),
        (Some(e), Some(_)) if e.reminder_due(now_ms, reminder) => Some("reminder"),
        (Some(e), None) if e.active && complete => Some("cleared"),
        _ => None,
    }
}
impl Episode {
    fn reminder_due(&self, now_ms: u64, interval: u64) -> bool {
        interval > 0 && now_ms.saturating_sub(self.last_notification_ms) >= interval
    }
}
