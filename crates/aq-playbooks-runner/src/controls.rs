//! Apply binding and authority changes and validate administrative control requests.
use super::*;

impl ActivationControls {
    pub(super) fn apply(self, s: &mut State, binding: &str, now_ms: i64) -> Result<()> {
        let activation = s
            .bindings
            .get_mut(binding)
            .ok_or_else(|| Error::NotFound(binding.into()))?;
        activation.controls = self.clone();
        activation.revision += 1;
        let artifact = activation.artifact.clone();
        if self.execution_enabled {
            s.execution_denied.remove(&artifact);
        } else {
            s.execution_denied.insert(artifact.clone());
        }
        s.catalog_revision += 1;
        record_binding_change(s, binding, now_ms)?;
        update_runs(s, |r| r.artifact == artifact, self.execution_enabled);
        ReceiptSite::admin(binding, now_ms).record(
            s,
            "activation_controls",
            json!({"artifact":artifact,"controls":self}),
        );
        Ok(())
    }
}

pub(super) struct Subject<'a> {
    pub(super) actor: &'a str,
    pub(super) scope: &'a str,
}
impl Subject<'_> {
    fn matches(&self, authority: &Authority) -> bool {
        authority.actor == self.actor && authority.scope == self.scope
    }
    pub(super) fn revoke(
        &self,
        s: &mut State,
        revoked: BTreeSet<String>,
        now_ms: i64,
    ) -> Result<()> {
        let Subject { actor, scope } = *self;
        s.authority_revocations
            .insert(digest(&(actor, scope))?, revoked.clone());
        update_runs(s, |r| self.matches(&r.authority), true);
        let intents = s
            .intents
            .values()
            .filter(|i| self.matches(&i.authority))
            .map(|i| i.id.clone())
            .collect::<Vec<_>>();
        for id in intents {
            indexes::mark_intent(s, &id);
        }
        ReceiptSite::admin("authority", now_ms).record(
            s,
            "authority_changed",
            json!({"actor":actor,"scope":scope,"revoked_capabilities":revoked}),
        );
        Ok(())
    }
}

fn update_runs(s: &mut State, matches: impl Fn(&Run) -> bool, resume: bool) {
    let ids = s
        .runs
        .values()
        .filter(|r| matches(r) && !r.status.terminal())
        .map(|r| r.id.clone())
        .collect::<Vec<_>>();
    for id in ids {
        if resume {
            s.runs.get_mut(&id).unwrap().resume_revoked();
        }
        indexes::mark_run(s, &id);
    }
}

impl Run {
    fn resume_revoked(&mut self) {
        if self.status == RunStatus::Blocked
            && self.reason.as_deref() == Some("execution authority revoked")
        {
            self.status = if self.pending.is_some() {
                RunStatus::Waiting
            } else {
                RunStatus::Running
            };
            self.reason = None;
        }
    }
}
impl RecoveryRequest {
    pub(super) fn validate_evidence(
        actor: &str,
        reason: &str,
        references: &[String],
    ) -> Result<()> {
        let identified = !actor.is_empty() && (1..=4096).contains(&reason.len());
        let bounded = (1..=64).contains(&references.len())
            && references.iter().all(|r| (1..=1024).contains(&r.len()));
        if !identified || !bounded {
            return Err(Error::Refused(
                "recovery requires bounded actor, reason and evidence references".into(),
            ));
        }
        Ok(())
    }
}

impl RetentionPolicy {
    pub(super) fn validate_bounds(&self) -> Result<()> {
        if [
            self.event_replay_ms,
            self.admission_replay_ms,
            self.audit_ms,
        ]
        .contains(&0)
        {
            return Err(Error::Refused("retention windows must be positive".into()));
        }
        if self.artifact_keep == 0 {
            return Err(Error::Refused("artifact_keep must be positive".into()));
        }
        if self.replay_keep == 0 {
            return Err(Error::Refused("replay_keep must be positive".into()));
        }
        if [self.drain_cohorts_per_commit, self.drain_entries_per_commit].contains(&0) {
            return Err(Error::Refused("drain bounds must be positive".into()));
        }
        let per_commit = [
            self.window_drops_per_commit,
            self.replay_evictions_per_commit,
            self.history_trim_per_commit,
            self.gc_page_limit,
            self.settle_cohorts_per_commit,
        ];
        if per_commit.contains(&0) {
            return Err(Error::Refused(
                "per-commit retention bounds must be positive".into(),
            ));
        }
        Ok(())
    }
}

pub(super) fn check_authority(artifact: &Artifact, authority: &Authority) -> Result<()> {
    if artifact.content.playbook.scope != authority.scope
        || !artifact
            .content
            .playbook
            .required_capabilities
            .is_subset(&authority.capabilities)
    {
        return Err(Error::Refused("scope or capability ceiling".into()));
    }
    Ok(())
}

pub(super) fn execution_allowed(
    s: &State,
    artifact: &str,
    authority: &Authority,
    capabilities: &BTreeSet<String>,
) -> Result<bool> {
    Ok(!s.execution_denied.contains(artifact)
        && s.authority_revocations
            .get(&digest(&(&authority.actor, &authority.scope))?)
            .is_none_or(|revoked| revoked.is_disjoint(capabilities)))
}
