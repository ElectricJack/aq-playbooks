//! Admission, replay and fanout of authenticated event envelopes.
use super::*;
mod destinations;
use destinations::Destinations;
pub(super) use destinations::fanout_event;

pub(super) struct EventIngress<'a> {
    pub(super) producer: &'a str,
    pub(super) epoch: u64,
    pub(super) requested_at_ms: Option<i64>,
    pub(super) now_ms: i64,
}
struct Identity {
    key: String,
    content: String,
}

enum Replay {
    New,
    Expired,
    Duplicate(EventDisposition),
}

struct NewEvent {
    identity: Identity,
    envelope: EventEnvelope,
    destinations: Destinations,
    window_ms: u64,
}

impl EventIngress<'_> {
    pub(super) fn accept(
        &self,
        s: &mut State,
        event: EventEnvelope,
        target: Option<&str>,
    ) -> Result<Option<EventDisposition>> {
        let identity = self.identify(s, &event)?;
        match self.replay(s, &identity, &event.contract)? {
            Replay::New => {}
            Replay::Expired => return Ok(None),
            Replay::Duplicate(disposition) => return Ok(Some(disposition)),
        }
        let window_ms = self.window(s, &event)?;
        let destinations = Destinations::collect(s, &identity.key, &event, target)?;
        self.store(
            s,
            NewEvent {
                identity,
                envelope: event,
                destinations,
                window_ms,
            },
        )
        .map(Some)
    }

    fn identify(&self, s: &State, event: &EventEnvelope) -> Result<Identity> {
        if s.retention
            .producer_epochs
            .get(self.producer)
            .copied()
            .unwrap_or(0)
            != self.epoch
        {
            return Err(Error::Refused(
                "producer replay epoch retired; explicit new replay identity required".into(),
            ));
        }
        let key = digest(&(self.producer, self.epoch, &event.event_id))?;
        if let Some(tombstone) = s.retention.event_tombstones.get(&key) {
            if tombstone.digest != digest(event)? {
                return Err(Error::Conflict(
                    "expired event identity reused with changed content".into(),
                ));
            }
            return Err(Error::Refused(
                "event replay horizon expired; explicit new replay identity required".into(),
            ));
        }
        let content = digest(event)?;
        if event.producer != self.producer {
            return Err(Error::Refused("forged producer identity".into()));
        }
        Ok(Identity { key, content })
    }
    fn replay(&self, s: &mut State, identity: &Identity, contract: &ContractRef) -> Result<Replay> {
        let key = &identity.key;
        let Some(old) = s.events.get(key) else {
            return Ok(Replay::New);
        };
        // Past its window and held by nothing: it leaves first and
        // is then new. A held one is still remembered, so it is
        // answered as inside its window (SC-9).
        let start = retention::window_start(s, self.now_ms);
        if retention::event_droppable(s, key, start)? {
            // Not remembered any more: a stale request is refused
            // before this commit forgets the expired event.
            refuse_stale(
                s,
                self.requested_at_ms,
                || retention::contract_event_window(s, contract),
                self.now_ms,
            )?;
            retention::forget_event(s, key)?;
            return Ok(Replay::Expired);
        }
        if old.digest != identity.content {
            return Err(Error::Conflict(
                "event identity reused with changed content".into(),
            ));
        }
        Ok(Replay::Duplicate(EventDisposition {
            key: key.into(),
            duplicate: true,
            destinations: old.destinations.len(),
        }))
    }
    fn window(&self, s: &State, event: &EventEnvelope) -> Result<u64> {
        let contract = find_event(s, &event.contract)?;
        if !contract.trusted_producers.contains(self.producer)
            || !contract.scopes.contains(&event.scope)
        {
            return Err(Error::Refused("event producer or scope".into()));
        }
        let window_ms = s
            .retention
            .policy
            .event_replay_ms
            .max(contract.retention_ms);
        refuse_stale(s, self.requested_at_ms, || window_ms, self.now_ms)?;
        validate_event(
            event,
            &Registry {
                commands: BTreeMap::new(),
                events: BTreeMap::from([(contract.name.clone(), contract.clone())]),
            },
        )?;
        if !s.protection_available && requires_protection(s, &contract) {
            return Err(Error::Refused(
                "protected-value storage unavailable for event payload".into(),
            ));
        }
        Ok(window_ms)
    }
    fn store(&self, s: &mut State, event: NewEvent) -> Result<EventDisposition> {
        let NewEvent {
            identity,
            envelope,
            destinations,
            window_ms,
        } = event;
        let key = identity.key;
        let destinations = destinations.record_blockers(s, &key, self.now_ms);
        let count = destinations.len();
        s.sequence += 1;
        let sequence = s.sequence;
        s.replay_touched.events.insert(key.clone());
        s.events.insert(
            key.clone(),
            AcceptedEvent {
                key: key.clone(),
                envelope,
                digest: identity.content,
                accepted_at_ms: self.now_ms,
                sequence,
                catalog_revision: s.catalog_revision,
                destinations,
            },
        );
        s.retention.event_epochs.insert(key.clone(), self.epoch);
        // From the later of host time and clock; a clock already near the
        // end of time saturates rather than refusing the event.
        let expires_at_ms = deadline(self.now_ms, window_ms)?.max(
            retention::window_start(s, self.now_ms)
                .saturating_add(i64::try_from(window_ms).unwrap_or(i64::MAX)),
        );
        s.retention
            .event_expires_at_ms
            .insert(key.clone(), expires_at_ms);
        retention::window_insert(
            s,
            expires_at_ms,
            format!("{}{key}", retention::WINDOW_EVENT),
        );
        indexes::index_event(s, &key)?;
        Ok(EventDisposition {
            key,
            duplicate: false,
            destinations: count,
        })
    }
}

fn requires_protection(state: &State, contract: &EventContract) -> bool {
    !contract.sensitive_payload_fields.is_empty()
        || state.artifacts.values().any(|artifact| {
            let registry = &artifact.content.registry;
            registry.events.get(&contract.name) == Some(contract)
                && registry.commands.values().any(|command| {
                    !command.sensitive_output_fields.is_empty()
                        && command
                            .asynchronous
                            .as_ref()
                            .is_some_and(|a| a.completion_event == contract.name)
                })
        })
}

pub(super) fn find_event(s: &State, reference: &ContractRef) -> Result<EventContract> {
    for a in s.artifacts.values() {
        if let Some(c) = a.content.registry.events.get(&reference.name)
            && c.fingerprint()? == reference.fingerprint
        {
            return Ok(c.clone());
        }
    }
    Err(Error::Refused("unknown event contract".into()))
}
