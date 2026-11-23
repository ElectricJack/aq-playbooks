//! Binding-owned intervals share the activation, event and run transaction.
//! A failed commit leaves its schedule cursor eligible for another collection.

use super::*;
use aq_playbooks_model::schedules::{
    BindingSchedule, FixedIntervalSchedule, ScheduleCursor, ScheduleOccurrence,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingSchedules {
    pub(super) configuration: ScheduleConfiguration,
    registrations: BTreeMap<String, Registration>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct ScheduleConfiguration {
    pub(super) event: ContractRef,
    declarations: BTreeMap<String, BindingSchedule>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeactivatedBinding {
    revision: u64,
    pub(super) configuration: Option<ScheduleConfiguration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Registration {
    definition: FixedIntervalSchedule,
    cursor: ScheduleCursor,
}

pub(super) struct BindingSite<'a> {
    pub(super) namespace: &'a str,
    pub(super) binding: &'a str,
    pub(super) now_ms: i64,
}

impl ScheduleConfiguration {
    pub(super) fn install(
        self,
        state: &mut State,
        site: BindingSite<'_>,
        previous: Option<BindingSchedules>,
    ) -> Result<()> {
        let activation = &state.bindings[site.binding];
        check_event(
            &state.artifacts[&activation.artifact],
            &activation.authority.scope,
            &self.event,
        )?;
        let mut registrations = BTreeMap::new();
        for (key, declaration) in &self.declarations {
            let existing = previous
                .as_ref()
                .filter(|old| {
                    old.configuration.event == self.event
                        && old.configuration.declarations.get(key) == Some(declaration)
                })
                .and_then(|old| old.registrations.get(key));
            let registration = if let Some(existing) = existing {
                existing.clone()
            } else {
                let id = digest(&("aq.binding.interval", site.namespace, site.binding, key))?;
                let definition = declaration.register(
                    id,
                    activation.authority.scope.clone(),
                    activation.revision.to_string(),
                    site.now_ms,
                )?;
                let cursor = definition.cursor()?;
                Registration { definition, cursor }
            };
            registrations.insert(key.clone(), registration);
        }
        state.binding_schedules.insert(
            site.binding.into(),
            BindingSchedules {
                configuration: self,
                registrations,
            },
        );
        Ok(())
    }
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
        let configuration = ScheduleConfiguration::from_declarations(declarations, event)?;
        let target = ConfiguredBinding {
            site: BindingSite {
                namespace: &self.namespace,
                binding,
                now_ms,
            },
            artifact,
            authority,
            may_activate,
        };
        self.transaction_choice_at(Some(now_ms), |state| target.reconcile(state, configuration))
    }

    /// Stop new calls and event starts. A retry at the same revision is safe.
    /// Already accepted events and admitted runs keep their pinned snapshots.
    pub fn deactivate_binding(&self, binding: &str, revision: u64, now_ms: i64) -> Result<bool> {
        self.transaction_choice_at(Some(now_ms), |s| {
            if s.deactivated_bindings
                .get(binding)
                .is_some_and(|d| d.revision == revision)
            {
                return Ok((true, false));
            }
            if s.bindings.get(binding).map(|a| a.revision) != Some(revision) {
                return Ok((false, false));
            }
            s.bindings.remove(binding);
            let configuration = s
                .binding_schedules
                .remove(binding)
                .map(|old| old.configuration);
            s.deactivated_bindings.insert(
                binding.into(),
                DeactivatedBinding {
                    revision,
                    configuration,
                },
            );
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
            let due = due_intervals(s, now_ms);
            let mut count = 0;
            for (_, binding, key) in due.into_iter().take(limit) {
                let schedules = &s.binding_schedules[&binding];
                let r = &schedules.registrations[&key];
                let plan = r.definition.plan(&r.cursor, now_ms, 1)?;
                let Some(occurrence) = plan.occurrences.into_iter().next() else {
                    continue;
                };
                let event = event_for(
                    &schedules.configuration.declarations[&key],
                    schedules.configuration.event.clone(),
                    occurrence,
                );
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
impl ScheduleConfiguration {
    fn from_declarations(
        declarations: &[BindingSchedule],
        event: Option<ContractRef>,
    ) -> Result<Option<Self>> {
        BindingSchedule::validate_all(declarations)?;
        Ok(if declarations.is_empty() {
            None
        } else {
            let event = event.ok_or_else(|| Error::Refused("interval event unavailable".into()))?;
            Some(ScheduleConfiguration {
                event,
                declarations: declarations
                    .iter()
                    .map(|d| (d.key.clone(), d.clone()))
                    .collect(),
            })
        })
    }
}
struct ConfiguredBinding<'a> {
    site: BindingSite<'a>,
    artifact: &'a str,
    authority: Authority,
    may_activate: bool,
}
impl ConfiguredBinding<'_> {
    fn reconcile(
        self,
        s: &mut State,
        configuration: Option<ScheduleConfiguration>,
    ) -> Result<(bool, bool)> {
        let binding = self.site.binding;
        if let Some(disabled) = s.deactivated_bindings.get_mut(binding) {
            let changed = disabled.configuration != configuration;
            disabled.configuration = configuration;
            return Ok((false, changed));
        }
        if !self.requires_update(s, configuration.as_ref())? {
            return Ok((true, false));
        }
        let Self {
            site,
            artifact,
            authority,
            ..
        } = self;
        site.activate(s, artifact, authority)?;
        let previous = s.binding_schedules.remove(binding);
        if let Some(configuration) = configuration {
            configuration.install(s, site, previous)?;
        }
        Ok((true, true))
    }
    fn requires_update(
        &self,
        s: &State,
        configuration: Option<&ScheduleConfiguration>,
    ) -> Result<bool> {
        let binding = self.site.binding;
        let active = s.bindings.get(binding);
        if active.is_some_and(|a| a.artifact != self.artifact || a.authority != self.authority) {
            return Err(Error::Conflict("configured activation differs".into()));
        }
        let old_configuration = s
            .binding_schedules
            .get(binding)
            .map(|old| &old.configuration);
        if active.is_some() && old_configuration == configuration {
            return Ok(false);
        }
        let needs_grant = active.is_none() || configuration.is_some();
        if !self.may_activate && needs_grant {
            return Err(Error::Refused("binding requires a live grant".into()));
        }
        Ok(true)
    }
}
fn due_intervals(s: &State, now_ms: i64) -> Vec<(i64, String, String)> {
    let mut due = vec![];
    for (binding, schedules) in &s.binding_schedules {
        if s.bindings
            .get(binding)
            .is_some_and(|a| a.controls.event_starts_enabled)
        {
            due.extend(schedules.registrations.iter().filter_map(|(key, r)| {
                r.due_at(now_ms)
                    .map(|at| (at, binding.clone(), key.clone()))
            }));
        }
    }
    due.sort_unstable();
    due
}
impl Registration {
    fn due_at(&self, now_ms: i64) -> Option<i64> {
        let at = self
            .definition
            .scheduled_at_ms(self.cursor.next_index)
            .ok()?;
        let ordered = self
            .cursor
            .last_observed_ms
            .is_none_or(|last| last <= now_ms);
        (at <= now_ms && ordered).then_some(at)
    }
}
impl BindingSite<'_> {
    fn activate(&self, s: &mut State, artifact: &str, authority: Authority) -> Result<()> {
        let Self {
            binding, now_ms, ..
        } = *self;
        if let Some(a) = s.bindings.get_mut(binding) {
            s.catalog_revision += 1;
            a.revision = s.catalog_revision;
            record_binding_change(s, binding, now_ms)?;
        } else {
            activate_binding(s, binding, artifact, authority, now_ms)?;
        }
        Ok(())
    }
}
