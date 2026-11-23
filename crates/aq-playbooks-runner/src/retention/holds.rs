//! Decide whether live obligations hold an accepted event past its replay window.
use super::*;

/// Whether an event is still needed past its window (SC-9): a live waiter on
/// one of its correlation keys, a queued destination that its input does not
/// block, or an uncertain completion of its contract and scope.
pub(crate) fn event_held(s: &State, key: &str) -> Result<bool> {
    let Some(event) = s.events.get(key) else {
        return Ok(false);
    };
    let memberships = s.indexes.event_memberships.get(key);
    EventHolds {
        state: s,
        stored: None,
    }
    .held(key, event, memberships.map(Vec::as_slice))
}
/// The waiters and uncertain operations a stored State recorded, which a
/// removal check adds to those of the State it judges: a hold is judged from
/// the live records (wait records, operations), and a commit cannot release
/// one by rewriting a derived index.
pub struct StoredHolds<'a> {
    /// The stored waiters of a correlation key (`indexes.waiters`).
    pub waiters: StoredWaiters<'a>,
    /// The stored uncertain completions by contract and scope.
    pub uncertain: &'a BTreeMap<String, BTreeSet<String>>,
}
/// [`event_held`] for `event` (key `key`, correlation keys `memberships`),
/// judged by the live work of `s`, which need not hold the event itself (a
/// commit that removes it): its `event:<key>` receipts are read from `s` and
/// from the history `s` writes.
pub(super) struct EventHolds<'a, 'b> {
    pub(super) state: &'a State,
    pub(super) stored: Option<&'a StoredHolds<'b>>,
}
impl EventHolds<'_, '_> {
    pub(super) fn held(
        &self,
        key: &str,
        event: &crate::AcceptedEvent,
        memberships: Option<&[String]>,
    ) -> Result<bool> {
        if self.unblocked_destination(key, event) {
            return Ok(true);
        }
        for correlation in memberships.into_iter().flatten() {
            if self.has_waiter(correlation)? {
                return Ok(true);
            }
        }
        self.uncertain_completion(event)
    }
    fn unblocked_destination(&self, key: &str, event: &crate::AcceptedEvent) -> bool {
        let s = self.state;
        if event.destinations.iter().any(|d| d.run.is_none()) {
            let owner = format!("event:{key}");
            let blocked: BTreeSet<String> = receipt_offsets(s, &owner)
                .into_iter()
                .filter_map(|offset| s.receipts.get(offset).cloned())
                .chain(
                    s.settled
                        .iter()
                        .filter(|cohort| cohort.root == owner)
                        .flat_map(|cohort| cohort.receipts.iter().cloned()),
                )
                .filter(|r| {
                    r.kind == "destination_blocked"
                        && r.detail.get("blocked_kind").and_then(|v| v.as_str()) == Some("input")
                })
                .filter_map(|r| {
                    r.detail
                        .get("destination")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                })
                .collect();
            if event
                .destinations
                .iter()
                .any(|d| d.run.is_none() && !blocked.contains(&d.key))
            {
                return true;
            }
        }
        false
    }
    // A waiter holds the event when its live record still waits on one of
    // the event's correlation keys: a run waiting on a wait record whose key
    // it is, or an unsettled operation waiting for its completion. The
    // candidates come from the waiter index (and the stored one).
    fn waiting(&self, target: &str, k: &str) -> Result<bool> {
        let s = self.state;
        if let Some(intent) = target.strip_prefix("intent:") {
            return Ok(s
                .intents
                .get(intent)
                .is_some_and(|i| !intent_settled(&i.status)));
        }
        let Some(run) = s.runs.get(target) else {
            return Ok(false);
        };
        let wait = match (&run.status, &run.pending) {
            (crate::RunStatus::Waiting, Some(crate::Pending::Event { wait })) => wait,
            _ => return Ok(false),
        };
        let Some(record) = s.waits.get(wait) else {
            return Ok(false);
        };
        let Some(contract) = &record.event else {
            return Ok(false);
        };
        Ok(crate::indexes::correlation_key(contract, &record.scope, &record.correlation)? == k)
    }

    fn has_waiter(&self, k: &str) -> Result<bool> {
        let mut targets = self
            .state
            .indexes
            .waiters
            .get(k)
            .cloned()
            .unwrap_or_default();
        if let Some(stored) = self.stored {
            targets.extend((stored.waiters)(k)?.unwrap_or_default());
        }
        for target in &targets {
            if self.waiting(target, k)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    // An operation dispatched without a handle (yet) may complete through
    // any event of its completion contract and scope accepted after its
    // first dispatch.
    fn uncertain_completion(&self, event: &crate::AcceptedEvent) -> Result<bool> {
        let s = self.state;
        let contract =
            crate::indexes::trigger_key(&event.envelope.contract, &event.envelope.scope)?;
        let mut uncertain: BTreeSet<String> = s
            .indexes
            .uncertain_completion_contracts
            .get(&contract)
            .cloned()
            .unwrap_or_default();
        if let Some(stored) = self.stored {
            uncertain.extend(stored.uncertain.get(&contract).cloned().unwrap_or_default());
        }
        Ok(uncertain.iter().any(|id| {
            s.intents.get(id).is_some_and(|i| {
                i.command.asynchronous.is_some()
                && crate::indexes::uncertain(i)
                // Its completion is accepted after its (first) dispatch.
                && i.dispatch_sequence.is_some_and(|d| d < event.sequence)
                // Until its command deadline plus the window (ruling C).
                && !crate::indexes::hold_lapsed(s, i)
            })
        }))
    }
}
/// An accepted event's identity (key, sequence, digest, acceptance time
/// and envelope) never changes; only its destinations do (fan-out).
pub fn check_event_change(
    old: &crate::AcceptedEvent,
    new: &crate::AcceptedEvent,
) -> std::result::Result<(), StorageError> {
    let old_identity = (
        &old.key,
        old.sequence,
        &old.digest,
        old.accepted_at_ms,
        &old.envelope,
    );
    let new_identity = (
        &new.key,
        new.sequence,
        &new.digest,
        new.accepted_at_ms,
        &new.envelope,
    );
    if old_identity != new_identity {
        return Err(refuse(REFUSE_EVENT_CHANGE));
    }
    Ok(())
}
