use super::*;

impl Changes {
    pub(super) fn expiries(&mut self, rules: &Retention) -> Result<(), StorageError> {
        // Expiry records (`retention::check_expiry_change`): judged here when the
        // patch shows the item, otherwise on the complete path.
        let window_ms = |item: &str| match item {
            "events" => rules.policy.event_replay_ms,
            _ => rules.policy.admission_replay_ms,
        };
        for (expiry, key, before, after) in std::mem::take(&mut self.expiry_changes) {
            let (item, _) = WINDOWS
                .iter()
                .find(|(_, e)| *e == expiry)
                .expect("an expiry field");
            let at = |bytes: &Option<Vec<u8>>| -> Result<Option<i64>, StorageError> {
                bytes
                    .as_deref()
                    .map(|b| serde_json::from_slice(b).map_err(backend))
                    .transpose()
            };
            let (old, new) = (at(&before)?, at(&after)?);
            match (old, new) {
                (Some(before), Some(after)) if rules.rebase && after < before => {
                    self.staged.history_removed = true
                }
                (Some(_), Some(_)) => {
                    aq_playbooks_runner::retention::check_expiry_change(old, new, true, None)?
                }
                (Some(_), None) if self.deleted.contains_key(&(*item, key.clone())) => {}
                (None, Some(_)) if self.written.contains_key(&(*item, key.clone())) => {
                    let value: Value = serde_json::from_slice(&self.written[&(*item, key.clone())])
                        .map_err(backend)?;
                    let fallback = value["accepted_at_ms"].as_i64().map(|accepted| {
                        accepted.saturating_add(i64::try_from(window_ms(item)).unwrap_or(i64::MAX))
                    });
                    aq_playbooks_runner::retention::check_expiry_change(old, new, true, fallback)?
                }
                _ => self.staged.history_removed = true,
            }
        }
        Ok(())
    }
    pub(super) fn deletions(&mut self, rules: &Retention) {
        let unlicensed = WINDOWS.iter().any(|(item, expiry)| {
            self.deleted
                .keys()
                .filter(|(field, _)| field == item)
                .any(|(_, key)| !self.licensed((item, expiry), key, rules))
        });
        self.staged.history_removed |= unlicensed;
    }

    fn licensed(
        &self,
        (item, expiry): (&'static str, &'static str),
        key: &Vec<u8>,
        rules: &Retention,
    ) -> bool {
        let at: Option<i64> = self
            .deleted
            .get(&(expiry, key.clone()))
            .and_then(|bytes| serde_json::from_slice(bytes).ok());
        let expired =
            at.is_some_and(|at| aq_playbooks_runner::retention::expired(at, rules.clock_ms));
        // An eviction the bounded fence licensed (`licensed_evictions`)
        // stays bounded too.
        let window = if item == "events" {
            Window::Event
        } else {
            Window::Admission
        };
        let licensed = std::str::from_utf8(key)
            .is_ok_and(|key| rules.evicted.contains(&(window, key.to_string())));
        (expired || licensed) && (item != "events" || !self.tombstoned.contains(key))
    }
}
