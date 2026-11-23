//! Drain the replay history and indexes left by older engine versions.
use super::*;

/// Drain phase: legacy event tombstones are dropped.
pub const DRAIN_TOMBSTONES: u8 = 0;
/// Drain phase: legacy correlation fences are dropped.
pub const DRAIN_FENCES: u8 = 1;
/// Drain phase: legacy disposal records are dropped.
pub const DRAIN_DISPOSALS: u8 = 2;
/// Drain phase: legacy `"admin"` receipts go to history.
pub const DRAIN_ADMIN: u8 = 3;
/// Drain phase: legacy keyed receipts join the window and eviction order.
pub const DRAIN_KEYED: u8 = 4;
/// Drain phase: legacy events join the window and eviction order.
pub const DRAIN_EVENTS: u8 = 5;
/// Every entry phase is complete.
pub const DRAIN_DONE: u8 = 6;
impl DrainState {
    /// Whether event `key` (accepted at `sequence`) was accepted by an older
    /// engine and the drain has not judged it yet.
    pub fn event_pending(&self, key: &str, sequence: u64) -> bool {
        !self.done
            && self.phase <= DRAIN_EVENTS
            && self
                .legacy_sequence
                .is_some_and(|legacy| sequence <= legacy)
            && (self.phase < DRAIN_EVENTS || self.cursor.as_deref().is_none_or(|c| key > c))
    }
}
#[cfg(test)]
thread_local! {
    /// Calls of `drain_step` that did any work beyond the `done` check.
    pub(super) static DRAIN_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
/// Move what an older engine left in the record out of it, a bounded amount
/// per commit, in every commit that changes the State (not a licensed
/// one), timed or not; `started` is the sequence the commit began at. Two
/// parts, each with its own bound from the policy:
///
/// - **Cohorts.** The roots after `drain.root_cursor` are examined, at most
///   `policy.drain_cohorts_per_commit`; the settled ones leave as history
///   ([`remove_cohorts`]), so that with those [`settle`] moved the commit
///   moves at most that many. Live ones stay and leave when they settle.
/// - **Entries**, at most `policy.drain_entries_per_commit`, in phases:
///   event tombstones and correlation fences are dropped, disposal records
///   cleared, `"admin"` receipts written before this commit moved to history
///   (each dated by its own time); then each keyed receipt and event gets
///   its expiry record and window key and joins the eviction order unless
///   live work holds it (a keyed receipt of a live run then waits in
///   `replay_held`), and each event joins `indexes.contract_events`.
///
/// When both parts are complete, `drain.done` is set and every later commit
/// costs one check. A [`settle`] beyond its bound re-arms the cohort part;
/// an index rebuild re-arms all of it.
pub(crate) fn drain_step(s: &mut State, started: u64, report: &mut CommitReport) -> Result<()> {
    if s.retention.drain.done {
        return Ok(());
    }
    #[cfg(test)]
    DRAIN_STEPS.with(|steps| steps.set(steps.get() + 1));
    // The cohorts this commit removed leave the root index first, so the
    // scan's bound is not spent on them.
    crate::indexes::sync(s)?;
    if s.retention.drain.legacy_sequence.is_none() {
        s.retention.drain.legacy_sequence = Some(started);
    }
    let cohorts = s.retention.policy.drain_cohorts_per_commit;
    let entries = s.retention.policy.drain_entries_per_commit;
    drain_cohorts(s, cohorts, report)?;
    drain_entries(s, started, entries, report)?;
    let drain = &mut s.retention.drain;
    if drain.roots_done && drain.phase >= DRAIN_DONE {
        drain.done = true;
        drain.root_cursor = None;
        drain.cursor = None;
    }
    Ok(())
}
fn drain_cohorts(s: &mut State, limit: usize, report: &mut CommitReport) -> Result<()> {
    if s.retention.drain.roots_done {
        return Ok(());
    }
    let mut removable = limit.saturating_sub(report.cohorts);
    let mut cursor = s.retention.drain.root_cursor.clone();
    let mut roots = vec![];
    let mut end = false;
    for _ in 0..limit {
        let Some(root) = s.indexes.root_members.next_key(cursor.as_ref()) else {
            end = true;
            break;
        };
        if cohort_settled(s, &root) {
            if removable == 0 {
                break;
            }
            removable -= 1;
            roots.push(root.clone());
        }
        cursor = Some(root);
    }
    take_cohorts(s, &roots, report)?;
    let drain = &mut s.retention.drain;
    drain.root_cursor = cursor;
    drain.roots_done = end;
    Ok(())
}
fn drain_entries(
    s: &mut State,
    started: u64,
    limit: usize,
    report: &mut CommitReport,
) -> Result<()> {
    let mut budget = limit;
    while budget > 0 && s.retention.drain.phase < DRAIN_DONE {
        let used = drain_phase(s, started, budget, report)?;
        report.drained += used;
        if used < budget {
            // The phase is exhausted.
            if s.retention.drain.phase == DRAIN_EVENTS {
                s.indexes.contract_events_complete = true;
            }
            let drain = &mut s.retention.drain;
            drain.phase += 1;
            drain.cursor = None;
        }
        budget -= used;
    }
    Ok(())
}
fn drain_phase(
    s: &mut State,
    started: u64,
    budget: usize,
    report: &mut CommitReport,
) -> Result<usize> {
    let used = match s.retention.drain.phase {
        DRAIN_TOMBSTONES => remove_legacy_keys(&mut s.retention.event_tombstones, budget),
        DRAIN_FENCES => remove_legacy_keys(&mut s.retention.expired_correlations, budget),
        DRAIN_DISPOSALS => {
            let len = s.retention.disposals.len();
            let n = len.min(budget);
            if n > 0 {
                s.retention.disposals.truncate(len - n);
            }
            n
        }
        DRAIN_ADMIN => drain_admin_receipts(s, started, budget, report),
        DRAIN_KEYED => {
            let keys = s
                .keyed_admissions
                .keys_after(s.retention.drain.cursor.as_ref(), budget);
            index_page(s, keys, |state, key| {
                index_legacy_admission(state, key);
                Ok(())
            })?
        }
        _ => {
            let keys = s
                .events
                .keys_after(s.retention.drain.cursor.as_ref(), budget);
            index_page(s, keys, index_legacy_event)?
        }
    };
    Ok(used)
}
fn remove_legacy_keys<V: crate::lazy::Record>(
    records: &mut LazyMap<String, V>,
    budget: usize,
) -> usize {
    let keys = records.keys_after::<String>(None, budget);
    for key in &keys {
        records.remove(key);
    }
    keys.len()
}

fn index_page(
    s: &mut State,
    keys: Vec<String>,
    index: impl Fn(&mut State, &str) -> Result<()>,
) -> Result<usize> {
    for key in &keys {
        index(s, key)?;
    }
    if let Some(last) = keys.last() {
        s.retention.drain.cursor = Some(last.clone());
    }
    Ok(keys.len())
}

/// Up to `budget` of the `"admin"` receipts written before this commit
/// (sequence at or before `started`), oldest first, each to history as a
/// cohort of its own dated by its own time. Returns how many.
fn drain_admin_receipts(
    s: &mut State,
    started: u64,
    budget: usize,
    report: &mut CommitReport,
) -> usize {
    let offsets: BTreeSet<usize> = s
        .indexes
        .receipts
        .get("admin")
        .into_iter()
        .flat_map(|offsets| offsets.range(..=started).map(|(_, offset)| *offset))
        .filter(|&offset| s.receipts.get(offset).is_some_and(|r| r.run == "admin"))
        .take(budget)
        .collect();
    let receipts = take_receipts(s, &offsets);
    report.receipts += receipts.len();
    let n = receipts.len();
    for receipt in receipts {
        s.settled.push(SettledCohort {
            root: "admin".into(),
            finished_at_ms: receipt.at_ms,
            runs: vec![],
            intents: vec![],
            receipts: vec![receipt],
        });
    }
    n
}
/// A keyed admission receipt an older engine wrote: its expiry record
/// (acceptance plus the admission window) and window key, and its place in
/// the eviction order, or in `replay_held` while its run is live. Each is
/// added only if missing.
fn index_legacy_admission(s: &mut State, key: &str) {
    let Some(receipt) = s.keyed_admissions.get(key).cloned() else {
        return;
    };
    let at = match s.retention.admission_expires_at_ms.get(key) {
        Some(at) => *at,
        None => {
            let at = receipt
                .accepted_at_ms
                .saturating_add(window(s.retention.policy.admission_replay_ms));
            s.retention
                .admission_expires_at_ms
                .insert(key.to_string(), at);
            at
        }
    };
    ensure_window_member(s, at, format!("{WINDOW_ADMISSION}{key}"));
    if s.runs.contains_key(&receipt.run_id) {
        if !s.retention.replay_held.contains_key(&receipt.run_id) {
            s.retention
                .replay_held
                .insert(receipt.run_id.clone(), key.to_string());
        }
    } else {
        let order = admission_order_key(&receipt);
        if !s.retention.replay_order.contains_key(&order) {
            s.retention.replay_order.insert(order, ());
        }
    }
}
/// An event an older engine accepted: its expiry record and window key, its
/// `indexes.contract_events` entry, and its place in or out of the eviction
/// order as live work holds it. Each is added only if missing.
fn index_legacy_event(s: &mut State, key: &str) -> Result<()> {
    let Some((sequence, order, contract)) = s.events.get(key).map(|e| {
        (
            e.sequence,
            event_order_key(e),
            crate::indexes::trigger_key(&e.envelope.contract, &e.envelope.scope),
        )
    }) else {
        return Ok(());
    };
    let contract = contract?;
    let Some(at) = event_expiry(s, key) else {
        return Ok(());
    };
    if !s.retention.event_expires_at_ms.contains_key(key) {
        s.retention.event_expires_at_ms.insert(key.to_string(), at);
    }
    ensure_window_member(s, at, format!("{WINDOW_EVENT}{key}"));
    if !s
        .indexes
        .contract_events
        .get(&contract)
        .is_some_and(|ids| ids.contains_key(&sequence))
    {
        s.indexes
            .contract_events
            .entry(contract)
            .or_default()
            .insert(sequence, key.to_string());
    }
    sync_event_order(s, key, order)
}

fn ensure_window_member(s: &mut State, at: i64, member: String) {
    let member = window_key(at, &member);
    if !s.retention.expiry.contains_key(&member) {
        s.retention.expiry.insert(member, ());
    }
}
