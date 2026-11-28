//! Install bindings, restore their schedules and record their catalog revision.
use super::*;
pub(super) struct BindingUpdate<'a> {
    pub(super) binding: &'a str,
    pub(super) artifact: &'a str,
    pub(super) authority: Authority,
}
impl BindingUpdate<'_> {
    pub(super) fn commit(self, engine: &Engine<impl Storage>, host_ms: Option<i64>) -> Result<()> {
        let now_ms = match host_ms {
            Some(time) => time,
            None => derived_time(engine)?,
        };
        let binding = self.binding;
        engine.transaction_at(host_ms, |s| {
            self.install(s, now_ms)?;
            if let Some(configuration) = s
                .deactivated_bindings
                .remove(binding)
                .and_then(|d| d.configuration)
            {
                configuration.install(
                    s,
                    schedules::BindingSite {
                        namespace: &engine.namespace,
                        binding,
                        now_ms,
                    },
                    None,
                )?;
            }
            Ok(())
        })
    }
    pub(super) fn install(self, s: &mut State, now_ms: i64) -> Result<()> {
        let Self {
            binding,
            artifact,
            authority,
        } = self;
        let a = s
            .artifacts
            .get(artifact)
            .ok_or_else(|| Error::NotFound(artifact.into()))?;
        check_authority(a, &authority)?;
        if let Some(schedules) = s.binding_schedules.get(binding) {
            schedules::check_event(a, &authority.scope, &schedules.configuration.event)?;
        }
        s.catalog_revision += 1;
        let controls = s
            .bindings
            .get(binding)
            .map(|activation| activation.controls.clone())
            .unwrap_or_default();
        if !controls.execution_enabled {
            s.execution_denied.insert(artifact.into());
        }
        s.bindings.insert(
            binding.into(),
            Activation {
                artifact: artifact.into(),
                authority,
                revision: s.catalog_revision,
                controls,
            },
        );
        record_binding_change(s, binding, now_ms)
    }
}

fn derived_time(engine: &Engine<impl Storage>) -> Result<i64> {
    let state = engine.store.load(&engine.namespace)?.state;
    let derived = state
        .receipts
        .last()
        .map(|receipt| receipt.at_ms)
        .unwrap_or(0);
    // Finished work leaves with its receipts; the clock still dates it.
    Ok(retention::window_start(&state, derived))
}

pub(super) fn record_binding_change(s: &mut State, binding: &str, now_ms: i64) -> Result<()> {
    s.catalog_history
        .insert(s.catalog_revision, s.bindings.clone());
    s.catalog_history_at_ms.insert(s.catalog_revision, now_ms);
    indexes::index_activation(s, binding)?;
    if let Some(artifact) = s.bindings.get(binding).map(|a| a.artifact.clone()) {
        retention::record_activation(s, &artifact);
    }
    // The binding's previous artifact may no longer be referenced.
    let at = retention::window_start(s, now_ms);
    retention::dispose_unreferenced(s, None, at);
    Ok(())
}
