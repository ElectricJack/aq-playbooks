//! Evict entries past the replay bound and verify the exact order of their removals.
use super::*;
/// Evict the oldest window entries past `policy.replay_keep`: while the
/// eviction order holds more, take its head (at most `limit` examined). An
/// entry live work holds leaves the order without being evicted (a keyed
/// receipt waits for its run in `replay_held`; an event rejoins when
/// [`rejoin`] judges it unheld). Otherwise the entry is forgotten: its item,
/// expiry record, expiry order key and (for an event) indexes, as when its
/// window passes. Never refuses, never needs host time. Nothing to evict
/// costs one count; k examined cost O(k).
pub(crate) fn evict(s: &mut State, limit: usize, report: &mut CommitReport) -> Result<()> {
    let keep = s.retention.policy.replay_keep;
    if s.retention.replay_order.len() <= keep {
        return Ok(());
    }
    window_step();
    let mut head = s.retention.replay_order.next_key_by_one::<String>(None);
    for _ in 0..limit {
        if s.retention.replay_order.len() <= keep {
            break;
        }
        let Some(key) = head else {
            break;
        };
        s.retention.replay_order.remove(&key);
        window_step();
        head = s.retention.replay_order.next_key_by_one(Some(&key));
        window_step();
        evict_member(s, &key, report)?;
    }
    if s.retention.replay_order.len() > keep {
        report.more = true;
    }
    Ok(())
}
fn evict_member(s: &mut State, order: &str, report: &mut CommitReport) -> Result<()> {
    match replay_entry(order).and_then(|(_, member)| Window::member(member)) {
        Some((Window::Admission, item)) => evict_admission(s, item, order, report),
        Some((Window::Event, item)) => evict_event(s, item, order, report)?,
        None => {}
    }
    Ok(())
}
fn evict_admission(s: &mut State, item: &str, order: &str, report: &mut CommitReport) {
    let Some(receipt) = s.keyed_admissions.get(item).cloned() else {
        return;
    };
    if admission_order_key(&receipt) != order {
        return;
    }
    if s.runs.contains_key(&receipt.run_id) {
        s.retention.replay_held.insert(receipt.run_id, item.into());
        return;
    }
    forget_admission(s, item);
    report.evicted += 1;
}
fn evict_event(s: &mut State, item: &str, order: &str, report: &mut CommitReport) -> Result<()> {
    if !s
        .events
        .get(item)
        .is_some_and(|event| event_order_key(event) == order)
        || event_held(s, item)?
    {
        return Ok(());
    }
    let at = event_expiry(s, item);
    let mut removed = CommitReport::default();
    remove_event(s, item, &mut removed)?;
    report.receipts += removed.receipts;
    if let Some(at) = at {
        window_remove(s, at, &format!("{WINDOW_EVENT}{item}"));
    }
    report.evicted += 1;
    Ok(())
}
/// The window entries `next` holds in its eviction order that `previous`
/// did not must each be the order key of an item `next` holds and no live
/// work in `next` holds ([`REFUSE_EVICTION`] otherwise), so the order's
/// count, which licenses evictions, counts only real unheld entries.
fn check_order_entry(next: &State, key: &str) -> std::result::Result<(), StorageError> {
    let member = replay_entry(key).and_then(|(_, raw)| Window::member(raw));
    let genuine = match member {
        Some((Window::Admission, item)) => next
            .keyed_admissions
            .get(item)
            .is_some_and(|r| admission_order_key(r) == key && !next.runs.contains_key(&r.run_id)),
        Some((Window::Event, item))
            if next
                .events
                .get(item)
                .is_some_and(|event| event_order_key(event) == key) =>
        {
            !event_held(next, item).map_err(|e| refuse(&e.to_string()))?
        }
        _ => false,
    };
    if genuine {
        Ok(())
    } else {
        Err(refuse(REFUSE_EVICTION))
    }
}
/// What [`check_evictions`] reads of the stored State.
pub struct Stored<'a> {
    /// Whether the stored eviction order holds `key` (it decides only the
    /// refusal's text).
    pub ordered: &'a dyn Fn(&str) -> std::result::Result<bool, StorageError>,
    /// The stored correlation keys of event `key`.
    pub memberships: &'a dyn Fn(&str) -> std::result::Result<Option<Memberships>, StorageError>,
    /// The stored waiters of a correlation key (`indexes.waiters`).
    pub waiters: StoredWaiters<'a>,
}
/// An event's correlation keys (`indexes.event_memberships`).
pub type Memberships = Vec<String>;
/// The stored waiters of a correlation key (`indexes.waiters`).
pub type StoredWaiters<'a> =
    &'a dyn Fn(&str) -> std::result::Result<Option<BTreeSet<String>>, StorageError>;
/// The evictions of a commit from `previous` to `next`, which a removal
/// check then allows: each keyed admission receipt or event of `previous`
/// that `next` no longer holds, whose expiry the clock has not reached, and
/// whose eviction order key `next` does not hold (it was in the order, or
/// joined and left it in this commit). Such an eviction is licensed only
/// as [`evict`] makes it, else it is refused ([`REFUSE_EVICTION`] when the
/// stored order held it, [`REFUSE_EARLY_REMOVAL`] when not):
///
/// - no live work in `next` holds the entry (a keyed receipt's run is not in
///   `next`; for an event, [`event_held`] judged by `next`);
/// - at most `policy.replay_evictions_per_commit` (of `next`) in the
///   commit, and `next`'s
///   order still holds at least `policy.replay_keep` entries (so the order
///   held more before them);
/// - oldest first: every evicted key is before every key `next`'s order
///   holds.
///
/// `added` are the order keys `next` holds that `previous` did not: each
/// must be genuine ([`check_order_entry`]), so the count is real. A
/// removal before expiry whose order key `next` still holds is no eviction:
/// the removal check refuses it as early. `previous` need hold only the
/// removed items, their expiry records and the stored header. Cost
/// O(evicted + added), plus one ordered lookup.
pub fn check_evictions(
    previous: &State,
    next: &State,
    added: &mut dyn Iterator<Item = String>,
    stored: &Stored<'_>,
) -> std::result::Result<BTreeSet<(Window, String)>, StorageError> {
    for key in added {
        check_order_entry(next, &key)?;
    }
    let mut check = EvictionCheck {
        previous,
        next,
        stored,
        evicted: Vec::new(),
    };
    for (key, receipt) in &previous.keyed_admissions {
        check.consider(key, RemovedItem::Admission(receipt))?;
    }
    for (key, event) in &previous.events {
        check.consider(key, RemovedItem::Event(event))?;
    }
    check.finish()
}

enum RemovedItem<'a> {
    Admission(&'a crate::KeyedAdmissionReceipt),
    Event(&'a crate::AcceptedEvent),
}

struct EvictionCheck<'a, 'b> {
    previous: &'a State,
    next: &'a State,
    stored: &'a Stored<'b>,
    evicted: Vec<(String, Window, String)>,
}
impl EvictionCheck<'_, '_> {
    fn refusal(&self, ordered: &str) -> StorageError {
        match (self.stored.ordered)(ordered) {
            Ok(true) => refuse(REFUSE_EVICTION),
            Ok(false) => refuse(REFUSE_EARLY_REMOVAL),
            Err(error) => error,
        }
    }
    fn consider(
        &mut self,
        key: &str,
        item: RemovedItem<'_>,
    ) -> std::result::Result<(), StorageError> {
        let window = match item {
            RemovedItem::Admission(_) => Window::Admission,
            RemovedItem::Event(_) => Window::Event,
        };
        let Some(ordered) = self.candidate_order(window, key, || match item {
            RemovedItem::Admission(receipt) => admission_order_key(receipt),
            RemovedItem::Event(event) => event_order_key(event),
        }) else {
            return Ok(());
        };
        let held = match item {
            RemovedItem::Admission(receipt) => self.next.runs.contains_key(&receipt.run_id),
            RemovedItem::Event(event) => self.event_held(event, key)?,
        };
        if held {
            return Err(self.refusal(&ordered));
        }
        self.evicted.push((ordered, window, key.to_owned()));
        Ok(())
    }

    fn candidate_order(
        &self,
        window: Window,
        key: &str,
        order: impl FnOnce() -> String,
    ) -> Option<String> {
        let expiry = match window {
            Window::Admission if !self.next.keyed_admissions.contains_key(key) => {
                admission_expiry(self.previous, key)
            }
            Window::Event if !self.next.events.contains_key(key) => {
                event_expiry(self.previous, key)
            }
            _ => return None,
        };
        if expiry.is_some_and(|at| expired(at, self.next.retention.clock_ms)) {
            return None;
        }
        let order = order();
        (!self.next.retention.replay_order.contains_key(&order)).then_some(order)
    }
    fn event_held(
        &self,
        event: &crate::AcceptedEvent,
        key: &str,
    ) -> std::result::Result<bool, StorageError> {
        let (previous, next, stored) = (self.previous, self.next, self.stored);
        let memberships = (stored.memberships)(key)?;
        let holds = StoredHolds {
            waiters: stored.waiters,
            uncertain: &previous.indexes.uncertain_completion_contracts,
        };
        (EventHolds {
            state: next,
            stored: Some(&holds),
        })
        .held(key, event, memberships.as_deref())
        .map_err(|e| refuse(&e.to_string()))
    }
    fn finish(self) -> std::result::Result<BTreeSet<(Window, String)>, StorageError> {
        let next = self.next;
        let order = &next.retention.replay_order;
        if self.evicted.is_empty() {
            return Ok(BTreeSet::new());
        }

        if self.evicted.len() > next.retention.policy.replay_evictions_per_commit
            || order.len() < next.retention.policy.replay_keep
        {
            return Err(self.refusal(&self.evicted[0].0));
        }
        if let Some(head) = order.next_key_by_one::<String>(None)
            && let Some(late) = self.evicted.iter().find(|(ordered, ..)| *ordered >= head)
        {
            return Err(self.refusal(&late.0));
        }
        Ok(self
            .evicted
            .into_iter()
            .map(|(_, window, key)| (window, key))
            .collect())
    }
}
