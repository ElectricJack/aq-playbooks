//! Binding-owned intervals share the activation, event and run transaction.
//! A failed commit leaves its schedule cursor eligible for another collection.

use super::*;
use aq_playbooks_model::schedules::{
    BindingSchedule, FixedIntervalSchedule, ScheduleCursor, ScheduleOccurrence,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingSchedules {
    pub event: ContractRef,
    registrations: BTreeMap<String, Registration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Registration {
    declaration: BindingSchedule,
    definition: FixedIntervalSchedule,
    cursor: ScheduleCursor,
}

pub(super) fn check_event(artifact: &Artifact, scope: &str, event: &ContractRef) -> Result<()> {
    let contract = artifact.content.registry.events.get(&event.name);
    if !contract.is_some_and(|c| {
        c.scopes.contains(scope) && c.fingerprint().is_ok_and(|hash| hash == event.fingerprint)
    }) || !artifact
        .content
        .playbook
        .triggers
        .iter()
        .any(|trigger| trigger.event == *event)
    {
        return Err(Error::Refused(
            "a scheduled binding requires its interval event trigger".into(),
        ));
    }
    Ok(())
}

impl<S: Storage> Engine<S> {
    /// Reconcile a configured binding without renewing its grant or replaying
    /// its schedule. A disabled binding stays disabled across startup.
    #[allow(clippy::too_many_arguments)]
    pub fn configure_binding(
        &self,
        binding: &str,
        artifact: &str,
        authority: Authority,
        may_activate: bool,
        declarations: &[BindingSchedule],
        event: Option<ContractRef>,
        now_ms: i64,
    ) -> Result<bool> {
        BindingSchedule::validate_all(declarations)?;
        self.transaction_choice_at(Some(now_ms), |s| {
            if s.deactivated_bindings.contains_key(binding) {
                return Ok((false, false));
            }
            let active = s.bindings.get(binding);
            if active.is_some_and(|a| a.artifact != artifact || a.authority != authority) {
                return Err(Error::Conflict("configured activation differs".into()));
            }
            let unchanged = match s.binding_schedules.get(binding) {
                Some(old) => {
                    event.as_ref() == Some(&old.event)
                        && old.registrations.len() == declarations.len()
                        && declarations.iter().all(|d| {
                            old.registrations
                                .get(&d.key)
                                .is_some_and(|r| r.declaration == *d)
                        })
                }
                None => declarations.is_empty(),
            };
            if active.is_some() && unchanged {
                return Ok((true, false));
            }
            if !may_activate && (active.is_none() || !declarations.is_empty()) {
                return Err(Error::Refused("binding requires a live grant".into()));
            }
            if let Some(a) = s.bindings.get_mut(binding) {
                s.catalog_revision += 1;
                a.revision = s.catalog_revision;
                record_binding_change(s, binding, now_ms)?;
            } else {
                activate_binding(s, binding, artifact, authority.clone(), now_ms)?;
            }
            let previous = s.binding_schedules.remove(binding);
            if declarations.is_empty() {
                return Ok((true, true));
            }
            let event = event.ok_or_else(|| Error::Refused("interval event unavailable".into()))?;
            check_event(&s.artifacts[artifact], &authority.scope, &event)?;
            let mut registrations = BTreeMap::new();
            for declaration in declarations {
                let existing = previous
                    .as_ref()
                    .filter(|old| old.event == event)
                    .and_then(|old| old.registrations.get(&declaration.key))
                    .filter(|r| r.declaration == *declaration);
                let registration = if let Some(existing) = existing {
                    existing.clone()
                } else {
                    let id = digest(&(
                        "aq.binding.interval",
                        &self.namespace,
                        binding,
                        &declaration.key,
                    ))?;
                    let definition = declaration.register(
                        id,
                        authority.scope.clone(),
                        s.catalog_revision.to_string(),
                        now_ms,
                    )?;
                    let cursor = definition.cursor()?;
                    Registration {
                        declaration: declaration.clone(),
                        definition,
                        cursor,
                    }
                };
                registrations.insert(declaration.key.clone(), registration);
            }
            s.binding_schedules.insert(
                binding.into(),
                BindingSchedules {
                    event,
                    registrations,
                },
            );
            Ok((true, true))
        })
    }

    /// Stop new calls and event starts. A retry at the same revision is safe.
    /// Already accepted events and admitted runs keep their pinned snapshots.
    pub fn deactivate_binding(&self, binding: &str, revision: u64, now_ms: i64) -> Result<bool> {
        self.transaction_choice_at(Some(now_ms), |s| {
            if s.deactivated_bindings.get(binding) == Some(&revision) {
                return Ok((true, false));
            }
            if s.bindings.get(binding).map(|a| a.revision) != Some(revision) {
                return Ok((false, false));
            }
            s.bindings.remove(binding);
            s.binding_schedules.remove(binding);
            s.deactivated_bindings.insert(binding.into(), revision);
            s.catalog_revision += 1;
            record_binding_change(s, binding, now_ms)?;
            Ok((true, true))
        })
    }

    /// Admit only the latest missed occurrence for each due schedule. Each
    /// interval starts triggers only on its owning binding.
    pub fn collect_interval_events(
        &self,
        now_ms: i64,
        limit: usize,
        mut event_for: impl FnMut(&BindingSchedule, ContractRef, ScheduleOccurrence) -> EventEnvelope,
    ) -> Result<usize> {
        if !(1..=2).contains(&limit) {
            return Err(Error::Refused("invalid interval collection limit".into()));
        }
        self.transaction_choice_at(Some(now_ms), |s| {
            let mut due = vec![];
            for (binding, schedules) in &s.binding_schedules {
                if !s
                    .bindings
                    .get(binding)
                    .is_some_and(|a| a.controls.event_starts_enabled)
                {
                    continue;
                }
                for (key, r) in &schedules.registrations {
                    if let Ok(at) = r.definition.scheduled_at_ms(r.cursor.next_index)
                        && at <= now_ms
                        && r.cursor.last_observed_ms.is_none_or(|last| last <= now_ms)
                    {
                        due.push((at, binding.clone(), key.clone()));
                    }
                }
            }
            due.sort_unstable();
            let mut count = 0;
            for (_, binding, key) in due.into_iter().take(limit) {
                let schedules = &s.binding_schedules[&binding];
                let r = &schedules.registrations[&key];
                let plan = r.definition.plan(&r.cursor, now_ms, 1)?;
                let Some(occurrence) = plan.occurrences.into_iter().next() else {
                    continue;
                };
                let event = event_for(&r.declaration, schedules.event.clone(), occurrence);
                let producer = event.producer.clone();
                let epoch = s
                    .retention
                    .producer_epochs
                    .get(&producer)
                    .copied()
                    .unwrap_or(0);
                let accepted =
                    accept_event(s, &producer, event, epoch, None, now_ms, Some(&binding))?
                        .ok_or_else(|| invariant("new interval event has expired".into()))?;
                fanout_event(s, &accepted.key, 128, now_ms)?;
                s.binding_schedules
                    .get_mut(&binding)
                    .unwrap()
                    .registrations
                    .get_mut(&key)
                    .unwrap()
                    .cursor = plan.cursor;
                count += 1;
            }
            Ok((count, count > 0))
        })
    }
}
