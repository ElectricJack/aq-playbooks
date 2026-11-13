//! Explicit replay horizons, disposal records and epoch fences. No unresolved
//! obligation is evicted for quota pressure.
//!
//! The retention clock (`RetentionState::clock_ms`) is the latest host time a
//! commit carried. A removal from the State is allowed only when the commit
//! writes its history (a settled cohort), when it is a window entry whose
//! expiry the clock has reached, or (until the legacy drain) when it carries
//! a legacy disposal record. [`verify_removals`] is that one check, shared by
//! every storage backend. It covers runs, operations, audit receipts, events
//! and keyed admission receipts, and the expiry records the window rule
//! reads (which may only move later). Waits, budgets and recovery requests
//! are live sub-records that ordinary transitions consume (a wait wakes, a
//! request is taken); the history-or-expiry rule cannot judge them, so they
//! are not fenced here.
use crate::lazy::{LazyMap, LazySeq};
use crate::{
    Error, Intent, IntentStatus, Receipt, Result, Run, SettledCohort, State, StorageError,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionPolicy {
    pub event_replay_ms: u64,
    pub terminal_run_ms: u64,
    pub audit_ms: u64,
    pub max_tombstones: usize,
    /// Window of a keyed admission receipt, from its acceptance.
    #[serde(default = "default_admission_replay_ms")]
    pub admission_replay_ms: u64,
}
fn default_admission_replay_ms() -> u64 {
    604_800_000
}
impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            event_replay_ms: 86_400_000,
            terminal_run_ms: 604_800_000,
            audit_ms: 604_800_000,
            max_tombstones: 100_000,
            admission_replay_ms: default_admission_replay_ms(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventTombstone {
    pub producer: String,
    pub epoch: u64,
    pub digest: String,
    pub disposed_at_ms: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Disposal {
    pub kind: String,
    pub key: String,
    pub at_ms: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RetentionState {
    #[serde(default)]
    pub last_gc_at_ms: Option<i64>,
    pub policy: RetentionPolicy,
    pub producer_epochs: BTreeMap<String, u64>,
    pub event_epochs: LazyMap<String, u64>,
    #[serde(default)]
    pub event_expires_at_ms: LazyMap<String, i64>,
    #[serde(default)]
    pub expired_correlations: LazyMap<String, (String, u64)>,
    pub event_tombstones: LazyMap<String, EventTombstone>,
    pub artifact_disposals: BTreeMap<String, i64>,
    pub disposals: LazySeq<Disposal>,
    pub gc_cursor: Option<String>,
    /// The latest host time any commit carried; never decreases.
    #[serde(default)]
    pub clock_ms: Option<i64>,
    /// Window entries in expiry order, one record each, keyed
    /// [`window_key`]`(expiry, member)`; a member is `WINDOW_EVENT` or
    /// `WINDOW_ADMISSION` plus the entry's key.
    #[serde(default)]
    pub expiry: LazyMap<String, ()>,
    /// Expiry of each keyed admission receipt that has one recorded.
    #[serde(default)]
    pub admission_expires_at_ms: LazyMap<String, i64>,
    /// Set by a clock rebase: a request for a key the engine does not
    /// remember whose request time is at or before it is stale. Never
    /// decreases. Absent (and not written) until the first rebase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_floor_ms: Option<i64>,
}
/// The `RetentionState::expiry` key of `member` expiring at
/// `expires_at_ms`: the expiry as 16 lowercase hex digits of its
/// order-preserving unsigned form, then the member. Byte order of keys is
/// expiry order, then member order.
pub fn window_key(expires_at_ms: i64, member: &str) -> String {
    format!("{:016x}{member}", (expires_at_ms as u64) ^ (1 << 63))
}
/// The expiry and member of a [`window_key`].
pub fn window_entry(key: &str) -> Option<(i64, &str)> {
    let at = u64::from_str_radix(key.get(..16)?, 16).ok()?;
    Some(((at ^ (1 << 63)) as i64, key.get(16..)?))
}
/// `RetentionState::expiry` member prefix of an admitted event.
pub const WINDOW_EVENT: &str = "e:";
/// `RetentionState::expiry` member prefix of a keyed admission receipt.
pub const WINDOW_ADMISSION: &str = "k:";
pub const REFUSE_RUN_REMOVAL: &str = "run removal requires a history record in the same commit";
pub const REFUSE_EARLY_REMOVAL: &str = "window entry removed before its expiry";
pub const REFUSE_CLOCK: &str = "retention clock cannot decrease";
/// Refusal of a commit that changes the stale floor without a licence, or
/// beyond its licence.
pub const REFUSE_STALE_FLOOR: &str = "stale floor changed without its licence";
/// Refusal of a commit whose clock rebase or floor receipt is not exactly
/// that commit (another clock or floor, a replayed receipt, or other work).
pub const REFUSE_LICENCE: &str = "clock rebase or floor receipt does not match this commit";
/// Kind of the admin receipt that lowers the stale floor
/// ([`crate::Engine::lower_stale_floor`]).
pub const STALE_FLOOR_LOWERED: &str = "stale_floor_lowered";
/// Kind of the admin receipt a clock rebase writes
/// ([`crate::Engine::rebase_retention_clock`]).
pub const CLOCK_REBASE: &str = "retention_clock_rebased";
const REFUSE_RUN_OBLIGATIONS: &str = "unresolved run obligations cannot be disposed";
const REFUSE_EFFECT: &str = "unresolved effect cannot be disposed";
const REFUSE_RECEIPT_CHANGE: &str = "immutable keyed admission receipt removed or changed";
/// Refusal of a commit that lowers, orphans or under-sets a window expiry.
pub const REFUSE_EXPIRY: &str = "window expiry cannot be shortened";
fn window(ms: u64) -> i64 {
    i64::try_from(ms).unwrap_or(i64::MAX)
}
/// Expiry of event `key`: its recorded expiry, else acceptance plus the
/// policy's replay window.
pub fn event_expiry(s: &State, key: &str) -> Option<i64> {
    if let Some(at) = s.retention.event_expires_at_ms.get(key) {
        return Some(*at);
    }
    s.events.get(key).map(|e| {
        e.accepted_at_ms
            .saturating_add(window(s.retention.policy.event_replay_ms))
    })
}
/// Expiry of keyed admission receipt `key`: its recorded expiry, else
/// acceptance plus the policy's admission window.
pub fn admission_expiry(s: &State, key: &str) -> Option<i64> {
    if let Some(at) = s.retention.admission_expires_at_ms.get(key) {
        return Some(*at);
    }
    s.keyed_admissions.get(key).map(|r| {
        r.accepted_at_ms
            .saturating_add(window(s.retention.policy.admission_replay_ms))
    })
}
/// Whether an entry expiring at `expiry` has expired at `clock_ms`. Nothing
/// expires before the first host time.
pub fn expired(expiry: i64, clock_ms: Option<i64>) -> bool {
    clock_ms.is_some_and(|now| expiry <= now)
}
/// The window of a keyed admission receipt or an event in `s` without its
/// recorded expiry: acceptance plus the policy's window.
fn fallback_expiry(s: &State, window: Window, key: &str) -> Option<i64> {
    let policy = &s.retention.policy;
    match window {
        Window::Event => s.events.get(key).map(|e| {
            e.accepted_at_ms
                .saturating_add(self::window(policy.event_replay_ms))
        }),
        Window::Admission => s.keyed_admissions.get(key).map(|r| {
            r.accepted_at_ms
                .saturating_add(self::window(policy.admission_replay_ms))
        }),
    }
}
/// Which expiry record: `event_expires_at_ms` or `admission_expires_at_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Event,
    Admission,
}
/// One change of an expiry record, from `old` to `new`, judged against the
/// State the commit stores (`item_kept`: the item is still in it;
/// `fallback`: its acceptance plus the policy window there). A recorded
/// expiry may only move later, may be deleted only with its item, and a new
/// one is at least the fallback.
pub fn check_expiry_change(
    old: Option<i64>,
    new: Option<i64>,
    item_kept: bool,
    fallback: Option<i64>,
) -> std::result::Result<(), StorageError> {
    let allowed = match (old, new) {
        (Some(old), Some(new)) => new >= old,
        (Some(_), None) => !item_kept,
        (None, Some(new)) => fallback.is_some_and(|fallback| new >= fallback),
        (None, None) => true,
    };
    if allowed {
        Ok(())
    } else {
        Err(refuse(REFUSE_EXPIRY))
    }
}
/// What an admin licence receipt in a commit's own history allows: exactly
/// one clock rebase, or exactly one lowering of the stale floor. Each
/// carries the receipt's sequence, which must be new in this commit (after
/// the stored sequence), so a receipt cannot be replayed, even after its
/// history row was deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Licence {
    /// [`CLOCK_REBASE`]: the clock from `from_ms` down to `to_ms`.
    Rebase {
        from_ms: i64,
        to_ms: i64,
        sequence: u64,
    },
    /// [`STALE_FLOOR_LOWERED`]: the stale floor from `from_ms` down to
    /// `to_ms`.
    FloorLowered {
        from_ms: i64,
        to_ms: i64,
        sequence: u64,
    },
}
/// The licence `next`'s commit carries: a [`CLOCK_REBASE`] or
/// [`STALE_FLOOR_LOWERED`] admin receipt in `next.settled`, the history this
/// commit writes. A receipt left in the live State licenses nothing. More
/// than one, or one without its clocks, is refused.
pub fn licence(next: &State) -> std::result::Result<Option<Licence>, StorageError> {
    let mut found = None;
    for r in next
        .settled
        .iter()
        .filter(|cohort| cohort.root == "admin")
        .flat_map(|cohort| &cohort.receipts)
        .filter(|r| r.run == "admin")
    {
        let clocks = || Some((r.detail["from_ms"].as_i64()?, r.detail["to_ms"].as_i64()?));
        let licence = match r.kind.as_str() {
            CLOCK_REBASE => clocks().map(|(from_ms, to_ms)| Licence::Rebase {
                from_ms,
                to_ms,
                sequence: r.sequence,
            }),
            STALE_FLOOR_LOWERED => clocks().map(|(from_ms, to_ms)| Licence::FloorLowered {
                from_ms,
                to_ms,
                sequence: r.sequence,
            }),
            _ => continue,
        }
        .ok_or_else(|| refuse(REFUSE_LICENCE))?;
        if found.replace(licence).is_some() {
            return Err(refuse(REFUSE_LICENCE));
        }
    }
    Ok(found)
}
/// Whether `next`'s commit carries an admin licence receipt at all (valid
/// or not): a format 3 store sends such a commit to its complete path,
/// where [`verify_removals`] judges it.
pub fn carries_licence(next: &State) -> bool {
    next.settled
        .iter()
        .filter(|cohort| cohort.root == "admin")
        .flat_map(|cohort| &cohort.receipts)
        .any(|r| r.run == "admin" && (r.kind == CLOCK_REBASE || r.kind == STALE_FLOOR_LOWERED))
}
/// The expiry a clock rebase to `to_ms` gives entry `key` of `window`:
/// `to_ms` plus the entry's own window (the admission window; for an event
/// `max(policy.event_replay_ms, contract.retention_ms)`).
pub fn rebase_cap(s: &State, window: Window, key: &str, to_ms: i64) -> i64 {
    let window_ms = match window {
        Window::Admission => self::window(s.retention.policy.admission_replay_ms),
        Window::Event => event_window(s, key),
    };
    to_ms.saturating_add(window_ms)
}
/// The clock, the stale floor and the licence of a commit from `previous`
/// to `next`. Without a licence the clock never decreases and the floor
/// never changes. With one, the commit must be exactly that licence: its
/// receipt's sequence is new in this commit, and
/// - a rebase lowers the clock from exactly `from_ms` to exactly `to_ms <
///   from_ms`, and may only raise the floor, to at most `to_ms`;
/// - a floor lowering leaves the clock alone and lowers the floor from
///   exactly `from_ms` to exactly `to_ms < from_ms`.
///
/// What else such a commit may not do (add or remove a run, operation,
/// event or keyed receipt, remove an audit receipt, change an expiry other
/// than by a rebase cap) is checked where those records are.
fn check_licence(
    previous: &State,
    next: &State,
    licence: Option<Licence>,
) -> std::result::Result<(), StorageError> {
    let (old_clock, new_clock) = (previous.retention.clock_ms, next.retention.clock_ms);
    let (old_floor, new_floor) = (
        previous.retention.stale_floor_ms,
        next.retention.stale_floor_ms,
    );
    let Some(licence) = licence else {
        if old_clock.is_some_and(|old| new_clock.is_none_or(|new| new < old)) {
            return Err(refuse(REFUSE_CLOCK));
        }
        return check_floor_unchanged(previous, next);
    };
    let (Licence::Rebase { sequence, .. } | Licence::FloorLowered { sequence, .. }) = licence;
    if sequence <= previous.sequence || sequence > next.sequence {
        return Err(refuse(REFUSE_LICENCE));
    }
    match licence {
        Licence::Rebase { from_ms, to_ms, .. } => {
            if old_clock != Some(from_ms) || new_clock != Some(to_ms) || to_ms >= from_ms {
                return Err(refuse(REFUSE_LICENCE));
            }
            let raised = match (old_floor, new_floor) {
                (_, Some(new)) => new <= to_ms && old_floor.is_none_or(|old| new >= old),
                (None, None) => true,
                (Some(_), None) => false,
            };
            if !raised {
                return Err(refuse(REFUSE_STALE_FLOOR));
            }
        }
        Licence::FloorLowered { from_ms, to_ms, .. } => {
            if old_clock != new_clock
                || old_floor != Some(from_ms)
                || new_floor != Some(to_ms)
                || to_ms >= from_ms
            {
                return Err(refuse(REFUSE_LICENCE));
            }
        }
    }
    Ok(())
}
/// Without a licence the stale floor never changes (a format 3 bounded
/// commit never carries one).
pub fn check_floor_unchanged(
    previous: &State,
    next: &State,
) -> std::result::Result<(), StorageError> {
    if previous.retention.stale_floor_ms != next.retention.stale_floor_ms {
        return Err(refuse(REFUSE_STALE_FLOOR));
    }
    Ok(())
}
/// [`check_expiry_change`] for record `key` of `window`, against `next`. In
/// a commit with a licence the only allowed change is a rebase cap: a
/// recorded expiry lowered to exactly [`rebase_cap`], with the clock at the
/// rebase's `to_ms`.
pub(crate) fn check_expiry_record(
    next: &State,
    window: Window,
    key: &str,
    old: Option<i64>,
    new: Option<i64>,
) -> std::result::Result<(), StorageError> {
    match licence(next)? {
        Some(Licence::Rebase { to_ms, .. }) => {
            if next.retention.clock_ms != Some(to_ms) {
                return Err(refuse(REFUSE_LICENCE));
            }
            let capped = matches!((old, new), (Some(old), Some(new))
                if new < old && new == rebase_cap(next, window, key, to_ms));
            return if capped {
                Ok(())
            } else {
                Err(refuse(REFUSE_EXPIRY))
            };
        }
        Some(Licence::FloorLowered { .. }) => return Err(refuse(REFUSE_EXPIRY)),
        None => {}
    }
    let item_kept = match window {
        Window::Event => next.events.contains_key(key),
        Window::Admission => next.keyed_admissions.contains_key(key),
    };
    let fallback = match (old, new) {
        (None, Some(_)) => fallback_expiry(next, window, key),
        _ => None,
    };
    check_expiry_change(old, new, item_kept, fallback)
}
/// Every expiry record change between two complete States.
fn check_expiry_records(previous: &State, next: &State) -> std::result::Result<(), StorageError> {
    for (window, old, new) in [
        (
            Window::Event,
            &previous.retention.event_expires_at_ms,
            &next.retention.event_expires_at_ms,
        ),
        (
            Window::Admission,
            &previous.retention.admission_expires_at_ms,
            &next.retention.admission_expires_at_ms,
        ),
    ] {
        for (key, at) in old {
            let after = new.get(key).copied();
            if after != Some(*at) {
                check_expiry_record(next, window, key, Some(*at), after)?;
            }
        }
        for (key, at) in new {
            if !old.contains_key(key) {
                check_expiry_record(next, window, key, None, Some(*at))?;
            }
        }
    }
    Ok(())
}
/// Move the clock to `now_ms` unless it is already later.
pub(crate) fn advance_clock(s: &mut State, now_ms: i64) {
    let clock = s
        .retention
        .clock_ms
        .map_or(now_ms, |clock| clock.max(now_ms));
    s.retention.clock_ms = Some(clock);
}
/// Removals that carry a legacy disposal record (written by `gc` before the
/// retention clock). Backends record their durable tombstones as before.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Removals {
    pub legacy_runs: Vec<String>,
    pub legacy_intents: Vec<String>,
    pub legacy_events: Vec<String>,
}
fn refuse(text: &str) -> StorageError {
    StorageError::Backend(text.into())
}
fn intent_settled(status: &IntentStatus) -> bool {
    matches!(
        status,
        IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
    )
}
/// The one removal check every backend runs on a commit from `previous` to
/// `next` (see the module docs). Each record of `previous` missing from
/// `next` must be allowed:
///
/// - a run: its history is in `next.settled`, it is terminal, its children
///   are terminal and its operations settled; or (legacy) a run disposal;
/// - an operation: its own history and its run's are in `next.settled` and it
///   is settled; or (legacy) its run has a disposal;
/// - an audit receipt: its history is in `next.settled`; or (legacy) its run
///   has a disposal;
/// - an event: its expiry has been reached (with a legacy tombstone, the
///   tombstone's digest must match);
/// - a keyed admission receipt: its expiry has been reached (a changed
///   receipt is always refused).
///
/// The clock never decreases, the stale floor never changes, and expiry
/// records follow [`check_expiry_change`], except in a commit that carries
/// an admin licence ([`licence`]), which must be exactly that licence and do
/// nothing else (see [`check_licence`]): a clock rebase lowers the clock and
/// caps expiries to [`rebase_cap`]; a floor lowering lowers the floor. Event tombstones and correlation fences may
/// always be removed. Artifact removals are checked by
/// [`verify_artifact_removals`].
pub fn verify_removals(
    previous: &State,
    next: &State,
) -> std::result::Result<Removals, StorageError> {
    let disposed: std::cell::OnceCell<BTreeSet<String>> = std::cell::OnceCell::new();
    let disposed_run = |id: &str| {
        disposed
            .get_or_init(|| {
                next.retention
                    .disposals
                    .iter()
                    .filter(|d| d.kind == "run")
                    .map(|d| d.key.clone())
                    .collect()
            })
            .contains(id)
    };
    if licence(next)?.is_some() {
        let added = next.runs.keys().any(|k| !previous.runs.contains_key(k))
            || next
                .intents
                .keys()
                .any(|k| !previous.intents.contains_key(k))
            || next.events.keys().any(|k| !previous.events.contains_key(k))
            || next
                .keyed_admissions
                .keys()
                .any(|k| !previous.keyed_admissions.contains_key(k));
        if added {
            return Err(refuse(REFUSE_LICENCE));
        }
    }
    let kept: BTreeSet<u64> = next.receipts.iter().map(|r| r.sequence).collect();
    let removals = check_removals(previous, next, &disposed_run, &|sequence| {
        kept.contains(&sequence)
    })?;
    check_expiry_records(previous, next)?;
    Ok(removals)
}
/// [`verify_removals`] without the expiry records, with the legacy run
/// disposals of `next` given by `disposed_run` and the receipts `next` keeps
/// by `receipt_kept`. `previous` need only hold the removed records, the
/// operations of removed runs and the children of removed runs (and its
/// header fields), so a record backend can check a patch.
pub(crate) fn check_removals(
    previous: &State,
    next: &State,
    disposed_run: &dyn Fn(&str) -> bool,
    receipt_kept: &dyn Fn(u64) -> bool,
) -> std::result::Result<Removals, StorageError> {
    verify_artifact_removals(previous, next)?;
    let clock = next.retention.clock_ms;
    let licensed = licence(next)?;
    check_licence(previous, next, licensed)?;
    // A licensed commit removes nothing.
    let removal = || -> std::result::Result<(), StorageError> {
        if licensed.is_some() {
            return Err(refuse(REFUSE_LICENCE));
        }
        Ok(())
    };
    for (key, receipt) in &previous.keyed_admissions {
        match next.keyed_admissions.get(key) {
            Some(kept) if kept == receipt => {}
            Some(_) => return Err(refuse(REFUSE_RECEIPT_CHANGE)),
            None if admission_expiry(previous, key).is_some_and(|at| expired(at, clock)) => {
                removal()?
            }
            None => return Err(refuse(REFUSE_EARLY_REMOVAL)),
        }
    }
    let mut removals = Removals::default();
    for (key, event) in &previous.events {
        if next.events.contains_key(key) {
            continue;
        }
        removal()?;
        // A legacy tombstone records the removal; it never licenses one.
        if !event_expiry(previous, key).is_some_and(|at| expired(at, clock)) {
            return Err(refuse(REFUSE_EARLY_REMOVAL));
        }
        if let Some(tombstone) = next.retention.event_tombstones.get(key) {
            if tombstone.digest != event.digest {
                return Err(refuse("event disposal digest conflict"));
            }
            removals.legacy_events.push(key.clone());
        }
    }
    let removed_receipts: Vec<&crate::Receipt> = previous
        .receipts
        .iter()
        .filter(|r| !receipt_kept(r.sequence))
        .collect();
    if !removed_receipts.is_empty() {
        removal()?;
        let settled: BTreeSet<u64> = next
            .settled
            .iter()
            .flat_map(|c| &c.receipts)
            .map(|r| r.sequence)
            .collect();
        for receipt in removed_receipts {
            if !settled.contains(&receipt.sequence) && !disposed_run(&receipt.run) {
                return Err(refuse(REFUSE_RUN_REMOVAL));
            }
        }
    }
    let removed_runs: Vec<(&String, &Run)> = previous
        .runs
        .iter()
        .filter(|(id, _)| !next.runs.contains_key(*id))
        .collect();
    let removed_intents: Vec<(&String, &Intent)> = previous
        .intents
        .iter()
        .filter(|(id, _)| !next.intents.contains_key(*id))
        .collect();
    if removed_runs.is_empty() && removed_intents.is_empty() {
        return Ok(removals);
    }
    removal()?;
    // A cohort's runs belong to its root.
    let mut settled_runs: BTreeMap<&str, &Run> = BTreeMap::new();
    for cohort in &next.settled {
        for run in &cohort.runs {
            if run.root != cohort.root {
                return Err(refuse(REFUSE_RUN_OBLIGATIONS));
            }
            settled_runs.insert(run.id.as_str(), run);
        }
    }
    let settled_intents: BTreeMap<&str, &Intent> = next
        .settled
        .iter()
        .flat_map(|c| &c.intents)
        .map(|i| (i.id.as_str(), i))
        .collect();
    let mut intents_of: BTreeMap<&str, Vec<&Intent>> = BTreeMap::new();
    if !removed_runs.is_empty() {
        for intent in previous
            .intents
            .values()
            .chain(settled_intents.values().copied())
        {
            intents_of
                .entry(intent.run.as_str())
                .or_default()
                .push(intent);
        }
    }
    for (id, run) in removed_runs {
        let unresolved = if let Some(settled) = settled_runs.get(id.as_str()) {
            // Each run and operation as this commit leaves it.
            let terminal = |child: &String| {
                settled_runs
                    .get(child.as_str())
                    .copied()
                    .or_else(|| next.runs.get(child))
                    .is_none_or(|r| r.status.terminal())
            };
            // An operation of a run that leaves goes with it: one still
            // live would be stranded (its run's history is written).
            let settled_intent = |intent: &&Intent| {
                !next.intents.contains_key(&intent.id)
                    && intent_settled(
                        &settled_intents
                            .get(intent.id.as_str())
                            .copied()
                            .unwrap_or(intent)
                            .status,
                    )
            };
            !settled.status.terminal()
                || !settled.children.iter().all(terminal)
                || !intents_of
                    .get(id.as_str())
                    .into_iter()
                    .flatten()
                    .all(settled_intent)
        } else if disposed_run(id) {
            removals.legacy_runs.push(id.clone());
            !run.status.terminal()
                || run.children.iter().any(|child| {
                    previous
                        .runs
                        .get(child)
                        .is_some_and(|r| !r.status.terminal())
                })
                || intents_of
                    .get(id.as_str())
                    .into_iter()
                    .flatten()
                    .any(|i| !intent_settled(&i.status))
        } else {
            return Err(refuse(REFUSE_RUN_REMOVAL));
        };
        if unresolved {
            return Err(refuse(REFUSE_RUN_OBLIGATIONS));
        }
    }
    for (id, intent) in removed_intents {
        let status = if let Some(settled) = settled_intents
            .get(id.as_str())
            .filter(|_| settled_runs.contains_key(intent.run.as_str()))
        {
            &settled.status
        } else if disposed_run(&intent.run) {
            removals.legacy_intents.push(id.clone());
            &intent.status
        } else {
            return Err(refuse(REFUSE_RUN_REMOVAL));
        };
        if !intent_settled(status) {
            return Err(refuse(REFUSE_EFFECT));
        }
    }
    Ok(removals)
}
/// At most this many settled cohorts leave the State in one commit.
pub const COHORTS_PER_COMMIT: usize = 50;
/// What one commit moved out of the State.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CommitReport {
    pub cohorts: usize,
    pub runs: usize,
    pub receipts: usize,
    pub events: usize,
    pub admissions: usize,
    pub deferred: usize,
    pub drained: usize,
    pub more: bool,
}
/// The roots this commit touched: the roots of its dirty runs and operations.
/// Call before `indexes::sync`, which clears the dirty sets. O(dirty).
pub(crate) fn candidate_roots(s: &State) -> BTreeSet<String> {
    let run_root = |run: &str| {
        s.runs
            .get(run)
            .map(|r| r.root.clone())
            .or_else(|| s.indexes.run_roots.get(run).cloned())
    };
    let mut roots = BTreeSet::new();
    for id in &s.indexes.dirty_runs {
        roots.extend(run_root(id));
    }
    for id in &s.indexes.dirty_intents {
        let root = s
            .intents
            .get(id)
            .and_then(|i| run_root(&i.run))
            .or_else(|| s.indexes.intent_roots.get(id).cloned());
        roots.extend(root);
    }
    roots
}
/// Whether the cohort of `root` is settled: it has members, none of them is
/// active and none of its operations is unsettled (the root indexes after
/// `indexes::sync`). O(1) index lookups.
fn cohort_settled(s: &State, root: &str) -> bool {
    let nonempty = |ids: Option<&BTreeSet<String>>| ids.is_some_and(|ids| !ids.is_empty());
    nonempty(s.indexes.root_members.get(root))
        && !nonempty(s.indexes.root_active.get(root))
        && !nonempty(s.indexes.root_effects.get(root))
}
/// Remove every settled cohort among `candidates` (at most `limit`) from the
/// State and hand each to storage as history (`State::settled`). A settled
/// candidate beyond `limit` sets `report.more`.
pub(crate) fn settle(
    s: &mut State,
    candidates: BTreeSet<String>,
    limit: usize,
    report: &mut CommitReport,
) -> Result<()> {
    let mut removed = 0;
    for root in candidates {
        if !cohort_settled(s, &root) {
            continue;
        }
        if removed >= limit {
            report.more = true;
            break;
        }
        let cohort = remove_cohort(s, &root)?;
        removed += 1;
        report.cohorts += 1;
        report.runs += cohort.runs.len();
        report.receipts += cohort.receipts.len();
        s.settled.push(cohort);
    }
    Ok(())
}
/// Take the receipts at `offsets` (positions in `s.receipts`) out of the
/// State, compact the positional sequence and rebuild the receipt offset
/// indexes. O(live receipts).
fn take_receipts(s: &mut State, offsets: &BTreeSet<usize>) -> Vec<Receipt> {
    if offsets.is_empty() {
        return vec![];
    }
    let taken: Vec<Receipt> = offsets
        .iter()
        .filter_map(|&offset| s.receipts.get(offset).cloned())
        .collect();
    let mut position = 0;
    s.receipts.retain(|_| {
        let keep = !offsets.contains(&position);
        position += 1;
        keep
    });
    s.indexes.receipts.clear();
    s.indexes.all_receipts.clear();
    let live: Vec<(String, u64)> = s
        .receipts
        .iter()
        .map(|r| (r.run.clone(), r.sequence))
        .collect();
    for (offset, (run, sequence)) in live.into_iter().enumerate() {
        s.indexes
            .receipts
            .entry(run)
            .or_default()
            .insert(sequence, offset);
        s.indexes.all_receipts.insert(sequence, offset);
    }
    taken
}
/// Offsets of the receipts owned by `run`, checked against the receipt.
fn receipt_offsets(s: &State, run: &str) -> Vec<usize> {
    s.indexes
        .receipts
        .get(run)
        .into_iter()
        .flat_map(|offsets| offsets.values().copied())
        .filter(|&offset| s.receipts.get(offset).is_some_and(|r| r.run == run))
        .collect()
}
/// Remove the cohort of `root` from the State: its runs, operations, their
/// recovery requests, waits, receipts and budget. Its runs and operations
/// are marked dirty, so the next `indexes::sync` drops their index entries.
/// O(cohort) plus O(live receipts) for compaction and O(live waits).
pub(crate) fn remove_cohort(s: &mut State, root: &str) -> Result<SettledCohort> {
    let members: Vec<String> = s
        .indexes
        .root_members
        .get(root)
        .map(|ids| ids.iter().cloned().collect())
        .unwrap_or_default();
    let intent_ids: Vec<String> = s
        .indexes
        .root_intents
        .get(root)
        .map(|ids| ids.iter().cloned().collect())
        .unwrap_or_default();
    let mut offsets = BTreeSet::new();
    let mut finished_at_ms = None;
    for id in &members {
        offsets.extend(receipt_offsets(s, id));
        let terminal = s
            .runs
            .get(id)
            .and_then(|r| r.terminal_sequence)
            .and_then(|sequence| s.indexes.all_receipts.get(&sequence).copied())
            .and_then(|offset| s.receipts.get(offset));
        if let Some(receipt) = terminal {
            finished_at_ms = finished_at_ms.max(Some(receipt.at_ms));
        }
    }
    let mut runs = vec![];
    for id in &members {
        if let Some(run) = s.runs.remove(id) {
            runs.push(run);
        }
        crate::indexes::mark_run(s, id);
    }
    let mut intents = vec![];
    for id in &intent_ids {
        if let Some(intent) = s.intents.remove(id) {
            intents.push(intent);
        }
        crate::indexes::mark_intent(s, id);
    }
    if !intent_ids.is_empty() && !s.recovery_requests.is_empty() {
        let ids: BTreeSet<&String> = intent_ids.iter().collect();
        s.recovery_requests
            .retain(|_, request| !ids.contains(&request.intent));
    }
    if !s.waits.is_empty() {
        let ids: BTreeSet<&String> = members.iter().collect();
        s.waits.retain(|_, wait| !ids.contains(&wait.run));
    }
    let receipts = take_receipts(s, &offsets);
    s.budgets.remove(root);
    let finished_at_ms = finished_at_ms
        .or_else(|| receipts.iter().map(|r| r.at_ms).max())
        .or(s.retention.clock_ms)
        .unwrap_or(0);
    Ok(SettledCohort {
        root: root.into(),
        finished_at_ms,
        runs,
        intents,
        receipts,
    })
}
/// Move the receipts owned by `owner`, which belongs to no cohort (`"admin"`,
/// `"event:<key>"`), out of the State as a cohort of root `owner` with no
/// runs. `None` when it owns none. O(its receipts) plus compaction.
pub(crate) fn move_unowned_receipts(s: &mut State, owner: &str) -> Result<Option<SettledCohort>> {
    let offsets: BTreeSet<usize> = receipt_offsets(s, owner).into_iter().collect();
    if offsets.is_empty() {
        return Ok(None);
    }
    let receipts = take_receipts(s, &offsets);
    let finished_at_ms = receipts
        .iter()
        .map(|r| r.at_ms)
        .max()
        .or(s.retention.clock_ms)
        .unwrap_or(0);
    Ok(Some(SettledCohort {
        root: owner.into(),
        finished_at_ms,
        runs: vec![],
        intents: vec![],
        receipts,
    }))
}
/// At most this many window entries are examined (dropped or deferred) by
/// one commit, oldest expiry first.
pub const WINDOW_DROPS_PER_COMMIT: usize = 64;
/// The time a window entry written now counts from: the later of the
/// commit's host time and the clock, so a backwards host time never
/// shortens a window.
pub(crate) fn window_start(s: &State, now_ms: i64) -> i64 {
    s.retention
        .clock_ms
        .map_or(now_ms, |clock| clock.max(now_ms))
}
/// Keep `member` (`WINDOW_EVENT` or `WINDOW_ADMISSION` plus its key) in
/// expiry order under `expires_at_ms`.
pub(crate) fn window_insert(s: &mut State, expires_at_ms: i64, member: String) {
    s.retention
        .expiry
        .insert(window_key(expires_at_ms, &member), ());
}
fn window_remove(s: &mut State, expires_at_ms: i64, member: &str) {
    s.retention
        .expiry
        .remove(&window_key(expires_at_ms, member));
}
/// Whether the keyed admission receipt `key` may leave at `clock`: its
/// window has passed and its run is no longer live (SC-10).
pub(crate) fn admission_droppable(s: &State, key: &str, clock: i64) -> bool {
    s.keyed_admissions.get(key).is_some_and(|receipt| {
        admission_expiry(s, key).is_some_and(|at| at <= clock)
            && !s.runs.contains_key(&receipt.run_id)
    })
}
/// Lower the retention clock to `to_ms` (a clock rebase) and cap every
/// recorded expiry later than its [`rebase_cap`] at that: `to_ms` plus the
/// entry's own window. An expiry at or below its cap stays. Walks the expiry
/// order from the smallest possible cap upwards, so the cost is the number
/// of entries above it (bounded by the namespace quotas). Returns (events,
/// admissions) capped. The caller writes the [`CLOCK_REBASE`] receipt that
/// licenses the commit.
pub(crate) fn rebase_clock(s: &mut State, to_ms: i64) -> (usize, usize) {
    let policy = &s.retention.policy;
    let lowest = to_ms.saturating_add(
        window(policy.admission_replay_ms).min(window(policy.event_replay_ms).max(1)),
    );
    let mut keys = vec![];
    let mut after = window_key(lowest, "");
    loop {
        let page = s.retention.expiry.keys_after(Some(&after), 256);
        let Some(last) = page.last().cloned() else {
            break;
        };
        keys.extend(page);
        after = last;
    }
    let (mut events, mut admissions) = (0, 0);
    for key in keys {
        let Some((_, member)) = window_entry(&key) else {
            continue;
        };
        let member = member.to_string();
        let (window, item) = if let Some(item) = member.strip_prefix(WINDOW_ADMISSION) {
            (Window::Admission, item)
        } else if let Some(item) = member.strip_prefix(WINDOW_EVENT) {
            (Window::Event, item)
        } else {
            continue;
        };
        let cap = rebase_cap(s, window, item, to_ms);
        let records = match window {
            Window::Admission => &mut s.retention.admission_expires_at_ms,
            Window::Event => &mut s.retention.event_expires_at_ms,
        };
        if records.get(item).is_some_and(|at| *at > cap) {
            records.insert(item.to_string(), cap);
            s.retention.expiry.remove(&key);
            window_insert(s, cap, member.clone());
            match window {
                Window::Admission => admissions += 1,
                Window::Event => events += 1,
            }
        }
    }
    s.retention.clock_ms = Some(to_ms);
    (events, admissions)
}
/// The end of a licensed admin commit (a clock rebase or a floor
/// lowering): only its `"admin"` receipt moves to history. The clock is not
/// advanced, nothing settles and nothing is dropped, so the commit is
/// exactly its licence.
pub(crate) fn finish_licensed_commit(s: &mut State) -> Result<CommitReport> {
    let mut report = CommitReport::default();
    if let Some(cohort) = move_unowned_receipts(s, "admin")? {
        report.receipts += cohort.receipts.len();
        s.settled.push(cohort);
    }
    Ok(report)
}
/// Remove the keyed admission receipt `key` ahead of its drop, with its
/// expiry record and window member. A receipt is never replaced in place, so
/// an expired key admitted again leaves in a commit of its own first.
pub(crate) fn forget_admission(s: &mut State, key: &str) {
    let at = admission_expiry(s, key);
    remove_admission(s, key);
    if let Some(at) = at {
        window_remove(s, at, &format!("{WINDOW_ADMISSION}{key}"));
    }
}
fn remove_admission(s: &mut State, key: &str) {
    s.keyed_admissions.remove(key);
    s.retention.admission_expires_at_ms.remove(key);
}
/// Whether an event is still needed past its window (SC-9): a live waiter on
/// one of its correlation keys, a queued destination that its input does not
/// block, or an uncertain completion of its contract and scope.
pub(crate) fn event_held(s: &State, key: &str) -> Result<bool> {
    let Some(event) = s.events.get(key) else {
        return Ok(false);
    };
    if event.destinations.iter().any(|d| d.run.is_none()) {
        let owner = format!("event:{key}");
        let blocked: BTreeSet<String> = receipt_offsets(s, &owner)
            .into_iter()
            .filter_map(|offset| s.receipts.get(offset))
            .filter(|r| {
                r.kind == "destination_blocked"
                    && r.detail.get("blocked_kind").and_then(|v| v.as_str()) == Some("input")
            })
            .filter_map(|r| r.detail.get("destination").and_then(|v| v.as_str()))
            .map(String::from)
            .collect();
        if event
            .destinations
            .iter()
            .any(|d| d.run.is_none() && !blocked.contains(&d.key))
        {
            return Ok(true);
        }
    }
    let live = |target: &String| {
        if let Some(intent) = target.strip_prefix("intent:") {
            s.intents
                .get(intent)
                .is_some_and(|i| !intent_settled(&i.status))
        } else {
            s.runs.get(target).is_some_and(|r| !r.status.terminal())
        }
    };
    let waited = s.indexes.event_memberships.get(key).is_some_and(|keys| {
        keys.iter().any(|k| {
            s.indexes
                .waiters
                .get(k)
                .is_some_and(|targets| targets.iter().any(live))
        })
    });
    if waited {
        return Ok(true);
    }
    let contract = crate::indexes::trigger_key(&event.envelope.contract, &event.envelope.scope)?;
    Ok(s.indexes
        .uncertain_completion_contracts
        .get(&contract)
        .is_some_and(|intents| !intents.is_empty()))
}
/// Whether event `key` may leave at `clock`: its window has passed and
/// nothing holds it.
pub(crate) fn event_droppable(s: &State, key: &str, clock: i64) -> Result<bool> {
    Ok(event_expiry(s, key).is_some_and(|at| at <= clock) && !event_held(s, key)?)
}
/// Remove event `key` ahead of its drop, with its window member (see
/// [`forget_admission`]).
pub(crate) fn forget_event(s: &mut State, key: &str) -> Result<()> {
    let at = event_expiry(s, key);
    remove_event(s, key, &mut CommitReport::default())?;
    if let Some(at) = at {
        window_remove(s, at, &format!("{WINDOW_EVENT}{key}"));
    }
    Ok(())
}
/// Remove event `key`, its epoch, expiry, correlation memberships and index
/// entries; its `event:<key>` receipts go to history.
fn remove_event(s: &mut State, key: &str, report: &mut CommitReport) -> Result<()> {
    let Some(event) = s.events.remove(key) else {
        return Ok(());
    };
    s.retention.event_epochs.remove(key);
    s.retention.event_expires_at_ms.remove(key);
    if let Some(keys) = s.indexes.event_memberships.remove(key) {
        for k in keys {
            let Some(events) = s.indexes.events.get_mut(&k) else {
                continue;
            };
            if events.get(&event.sequence).is_some_and(|id| id == key) {
                events.remove(&event.sequence);
            } else {
                events.retain(|_, id| id != key);
            }
            if events.is_empty() {
                s.indexes.events.remove(&k);
            }
        }
    }
    if let Some(cohort) = move_unowned_receipts(s, &format!("event:{key}"))? {
        report.receipts += cohort.receipts.len();
        s.settled.push(cohort);
    }
    report.events += 1;
    Ok(())
}
/// Drop the window entries whose expiry the clock has reached, oldest first,
/// examining at most `limit` members. An entry still needed is deferred: a
/// keyed receipt of a live run by the admission window, an event (see
/// [`event_held`]) by its window ([`event_window`]), both from the clock, so it neither
/// leaves early nor blocks the entries behind it. Finding the oldest entry
/// is one ordered lookup; the work is O(members examined). A commit without
/// a clock drops nothing.
pub(crate) fn drop_expired(s: &mut State, limit: usize, report: &mut CommitReport) -> Result<()> {
    let Some(clock) = s.retention.clock_ms else {
        return Ok(());
    };
    window_step();
    let mut head = s.retention.expiry.next_key_by_one::<String>(None);
    for _ in 0..limit {
        let Some(key) = head else {
            break;
        };
        let Some((at, member)) = window_entry(&key) else {
            // Not a window key: it is no entry; take it out of the order.
            s.retention.expiry.remove(&key);
            window_step();
            head = s.retention.expiry.next_key_by_one(Some(&key));
            continue;
        };
        if at > clock {
            break;
        }
        let member = member.to_string();
        s.retention.expiry.remove(&key);
        // Deferred entries go after the clock, so nothing is added behind
        // this cursor.
        window_step();
        head = s.retention.expiry.next_key_by_one(Some(&key));
        window_step();
        if let Some(key) = member.strip_prefix(WINDOW_ADMISSION) {
            drop_admission(s, key, clock, report);
        } else if let Some(key) = member.strip_prefix(WINDOW_EVENT) {
            drop_event(s, key, clock, report)?;
        }
    }
    Ok(())
}
#[cfg(test)]
thread_local! {
    /// Ordered expiry lookups and members examined by `drop_expired`.
    static WINDOW_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
fn window_step() {
    #[cfg(test)]
    WINDOW_STEPS.with(|steps| steps.set(steps.get() + 1));
}
fn drop_admission(s: &mut State, key: &str, clock: i64, report: &mut CommitReport) {
    let member = || format!("{WINDOW_ADMISSION}{key}");
    let Some(run_id) = s.keyed_admissions.get(key).map(|r| r.run_id.clone()) else {
        return;
    };
    let Some(at) = admission_expiry(s, key) else {
        return;
    };
    if at > clock {
        // Its expiry moved later: keep it in order there.
        window_insert(s, at, member());
        return;
    }
    if s.runs.contains_key(&run_id) {
        let next = clock.saturating_add(window(s.retention.policy.admission_replay_ms));
        s.retention.admission_expires_at_ms.insert(key.into(), next);
        window_insert(s, next, member());
        report.deferred += 1;
        return;
    }
    remove_admission(s, key);
    report.admissions += 1;
}
/// The window of event `key`: `max(policy.event_replay_ms,
/// contract.retention_ms)`, as when it was accepted (the policy's alone if
/// its contract is no longer installed); at least 1 ms.
fn event_window(s: &State, key: &str) -> i64 {
    let window_ms = match s.events.get(key) {
        Some(e) => contract_event_window(s, &e.envelope.contract),
        None => s.retention.policy.event_replay_ms,
    };
    window(window_ms).max(1)
}
/// The window of an event of `contract`: `max(policy.event_replay_ms,
/// contract.retention_ms)`, the policy value alone when the contract is no
/// longer installed. One scan of the installed artifacts.
pub(crate) fn contract_event_window(s: &State, contract: &aq_playbooks_model::ContractRef) -> u64 {
    let contract_ms = crate::find_event(s, contract).map_or(0, |c| c.retention_ms);
    s.retention.policy.event_replay_ms.max(contract_ms)
}
fn drop_event(s: &mut State, key: &str, clock: i64, report: &mut CommitReport) -> Result<()> {
    let member = || format!("{WINDOW_EVENT}{key}");
    if !s.events.contains_key(key) {
        return Ok(());
    }
    let Some(at) = event_expiry(s, key) else {
        return Ok(());
    };
    if at > clock {
        window_insert(s, at, member());
        return Ok(());
    }
    if event_held(s, key)? {
        let next = clock.saturating_add(event_window(s, key));
        s.retention.event_expires_at_ms.insert(key.into(), next);
        window_insert(s, next, member());
        report.deferred += 1;
        return Ok(());
    }
    remove_event(s, key, report)
}
/// The retention work of every commit that changes the State, after its
/// indexes are synchronized: move the clock to the commit's host time, remove
/// the settled cohorts among `candidates` (see [`candidate_roots`]), drop
/// expired window entries ([`drop_expired`]) and move `"admin"` receipts to
/// history. The clock step and the drop need host time.
pub(crate) fn finish_commit(
    s: &mut State,
    now_ms: Option<i64>,
    candidates: BTreeSet<String>,
) -> Result<CommitReport> {
    let mut report = CommitReport::default();
    if let Some(now_ms) = now_ms {
        advance_clock(s, now_ms);
    }
    settle(s, candidates, COHORTS_PER_COMMIT, &mut report)?;
    if now_ms.is_some() {
        drop_expired(s, WINDOW_DROPS_PER_COMMIT, &mut report)?;
    }
    if let Some(cohort) = move_unowned_receipts(s, "admin")? {
        report.receipts += cohort.receipts.len();
        s.settled.push(cohort);
    }
    Ok(report)
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GcReport {
    pub runs: usize,
    pub events: usize,
    pub artifacts: usize,
    pub receipts: usize,
    pub preserved: usize,
    pub more: bool,
}
pub fn artifact_referenced(s: &State, id: &str) -> bool {
    let mut pending = s
        .bindings
        .values()
        .map(|a| a.artifact.clone())
        .collect::<Vec<_>>();
    for bindings in s.catalog_history.values() {
        pending.extend(bindings.values().map(|a| a.artifact.clone()));
    }
    for run in s.runs.values() {
        pending.push(run.artifact.clone());
        pending.extend(run.dependencies.values().cloned());
    }
    for event in s.events.values() {
        for destination in &event.destinations {
            pending.push(destination.artifact.clone());
            pending.extend(destination.dependencies.values().cloned());
        }
    }
    let mut seen = BTreeSet::new();
    while let Some(hash) = pending.pop() {
        if hash == id {
            return true;
        }
        if !seen.insert(hash.clone()) {
            continue;
        }
        if let Some(artifact) = s.artifacts.get(&hash) {
            for dependency in artifact.content.dependencies.values() {
                pending.push(dependency.artifact.clone());
                if let aq_playbooks_model::DependencySelection::Catalog { allowed_artifacts } =
                    &dependency.selection
                {
                    pending.extend(allowed_artifacts.iter().cloned());
                    if let Some(binding) = s.bindings.get(&dependency.binding) {
                        pending.push(binding.artifact.clone());
                    }
                }
            }
        }
    }
    false
}
pub fn verify_artifact_removals(
    previous: &State,
    next: &State,
) -> std::result::Result<(), crate::StorageError> {
    for (id, old) in &previous.artifacts {
        match next.artifacts.get(id) {
            Some(new) if new != old => {
                return Err(crate::StorageError::Backend(
                    "immutable artifact replaced".into(),
                ));
            }
            None if !next.retention.artifact_disposals.contains_key(id)
                || artifact_referenced(next, id) =>
            {
                return Err(crate::StorageError::Backend(
                    "artifact removal lacks safe disposal".into(),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}
pub(crate) fn gc(s: &mut State, now: i64, limit: usize) -> Result<GcReport> {
    if limit == 0 || limit > 1000 {
        return Err(Error::Refused("GC page limit must be 1..1000".into()));
    }
    if s.retention
        .last_gc_at_ms
        .is_some_and(|previous| now < previous)
    {
        return Err(Error::Refused("GC clock cannot move backwards".into()));
    }
    s.retention.last_gc_at_ms = Some(now);
    let p = s.retention.policy.clone();
    let cutoff = |duration: u64| now.saturating_sub(i64::try_from(duration).unwrap_or(i64::MAX));
    let mut report = GcReport::default();
    let (mut phase, mut cursor, mut inclusive): (u8, String, bool) = s
        .retention
        .gc_cursor
        .as_deref()
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or((0, String::new(), false));
    let mut examined = 0;
    while examined < limit {
        match phase {
            0 => {
                let key = if inclusive && s.indexes.root_members.contains_key(&cursor) {
                    Some(cursor.clone())
                } else if inclusive && cursor.is_empty() {
                    s.indexes.root_members.next_key::<String>(None)
                } else {
                    s.indexes.root_members.next_key(Some(&cursor))
                };
                let Some(root) = key else {
                    phase = 1;
                    cursor.clear();
                    inclusive = false;
                    continue;
                };
                examined += 1;
                cursor = root.clone();
                inclusive = false;
                let active = s
                    .indexes
                    .root_active
                    .get(&root)
                    .is_some_and(|ids| !ids.is_empty());
                let unsettled = s
                    .indexes
                    .root_effects
                    .get(&root)
                    .is_some_and(|ids| !ids.is_empty());
                let recent = s
                    .indexes
                    .root_last_activity
                    .get(&root)
                    .copied()
                    .unwrap_or(i64::MAX)
                    > cutoff(p.terminal_run_ms.max(p.audit_ms));
                if active || unsettled || recent {
                    report.preserved += 1;
                    continue;
                }
                let Some(id) = s.indexes.root_members[&root]
                    .iter()
                    .find(|id| *id != &root)
                    .cloned()
                    .or_else(|| s.indexes.root_members[&root].first().cloned())
                else {
                    continue;
                };
                let intents = s
                    .intents
                    .values()
                    .filter(|i| i.run == id)
                    .map(|i| i.id.clone())
                    .collect::<Vec<_>>();
                for intent in intents {
                    s.intents.remove(&intent);
                    s.recovery_requests
                        .retain(|_, request| request.intent != intent);
                    crate::indexes::mark_intent(s, &intent);
                }
                s.runs.remove(&id);
                s.waits.retain(|_, w| w.run != id);
                crate::indexes::mark_run(s, &id);
                let before = s.receipts.len();
                s.receipts.retain(|receipt| receipt.run != id);
                report.receipts += before - s.receipts.len();
                report.runs += 1;
                s.retention.disposals.push(Disposal {
                    kind: "run".into(),
                    key: id.clone(),
                    at_ms: now,
                });
                s.indexes.root_members.get_mut(&root).unwrap().remove(&id);
                if s.indexes.root_members[&root].is_empty() {
                    s.indexes.root_members.remove(&root);
                    s.indexes.root_active.remove(&root);
                    s.indexes.root_effects.remove(&root);
                    s.indexes.root_last_activity.remove(&root);
                    s.budgets.remove(&root);
                } else {
                    inclusive = true;
                }
            }
            1 => {
                let key = s
                    .events
                    .range((
                        std::ops::Bound::Excluded(cursor.clone()),
                        std::ops::Bound::Unbounded,
                    ))
                    .next()
                    .map(|(key, _)| key.clone());
                let Some(id) = key else {
                    phase = 2;
                    cursor.clear();
                    inclusive = false;
                    continue;
                };
                examined += 1;
                cursor = id.clone();
                let e = &s.events[&id];
                let expiry = s
                    .retention
                    .event_expires_at_ms
                    .get(&id)
                    .copied()
                    .unwrap_or_else(|| {
                        e.accepted_at_ms
                            .saturating_add(i64::try_from(p.event_replay_ms).unwrap_or(i64::MAX))
                    });
                let protected = s.indexes.event_memberships.get(&id).is_some_and(|keys| {
                    keys.iter().any(|key| {
                        s.indexes.waiters.get(key).is_some_and(|waiters| {
                            waiters.iter().any(|target| {
                                if let Some(intent) = target.strip_prefix("intent:") {
                                    s.intents.get(intent).is_some_and(|i| {
                                        !matches!(
                                            i.status,
                                            IntentStatus::Completed
                                                | IntentStatus::Rejected
                                                | IntentStatus::Cancelled
                                        )
                                    })
                                } else {
                                    s.runs.get(target).is_some_and(|r| !r.status.terminal())
                                }
                            })
                        })
                    })
                });
                let unknown_handle = s
                    .indexes
                    .uncertain_completion_contracts
                    .get(&crate::indexes::trigger_key(
                        &e.envelope.contract,
                        &e.envelope.scope,
                    )?)
                    .is_some_and(|intents| !intents.is_empty());
                if expiry > now
                    || e.destinations.iter().any(|d| d.run.is_none())
                    || protected
                    || unknown_handle
                {
                    report.preserved += 1;
                    continue;
                }
                let epoch = s.retention.event_epochs.get(&id).copied().unwrap_or(0);
                let current = s
                    .retention
                    .producer_epochs
                    .get(&e.envelope.producer)
                    .copied()
                    .unwrap_or(0);
                if epoch == current && s.retention.event_tombstones.len() >= p.max_tombstones {
                    return Err(Error::Refused(
                        "replay tombstone quota reached; retire producer epoch before GC".into(),
                    ));
                }
                let new_correlations = s
                    .indexes
                    .event_memberships
                    .get(&id)
                    .map(|keys| {
                        keys.iter()
                            .filter(|key| !s.retention.expired_correlations.contains_key(*key))
                            .count()
                    })
                    .unwrap_or(0);
                if epoch == current
                    && s.retention
                        .expired_correlations
                        .len()
                        .saturating_add(new_correlations)
                        > p.max_tombstones.saturating_mul(64)
                {
                    return Err(Error::Refused("correlation history fence quota reached; settle retained invocations before epoch compaction".into()));
                }
                let e = s.events.remove(&id).unwrap();
                s.retention.event_epochs.remove(&id);
                s.retention.event_expires_at_ms.remove(&id);
                if epoch == current {
                    s.retention.event_tombstones.insert(
                        id.clone(),
                        EventTombstone {
                            producer: e.envelope.producer.clone(),
                            epoch,
                            digest: e.digest,
                            disposed_at_ms: now,
                        },
                    );
                }
                if let Some(keys) = s.indexes.event_memberships.remove(&id) {
                    for key in keys {
                        if epoch == current {
                            s.retention
                                .expired_correlations
                                .insert(key.clone(), (e.envelope.producer.clone(), epoch));
                        }
                        if let Some(events) = s.indexes.events.get_mut(&key) {
                            events.retain(|_, value| value != &id);
                            if events.is_empty() {
                                s.indexes.events.remove(&key);
                            }
                        }
                    }
                }
                s.retention.disposals.push(Disposal {
                    kind: "event".into(),
                    key: id,
                    at_ms: now,
                });
                report.events += 1;
            }
            2 => {
                let key = s
                    .artifacts
                    .range((
                        std::ops::Bound::Excluded(cursor.clone()),
                        std::ops::Bound::Unbounded,
                    ))
                    .next()
                    .map(|(key, _)| key.clone());
                let Some(id) = key else {
                    phase = 3;
                    break;
                };
                examined += 1;
                cursor = id.clone();
                if artifact_referenced(s, &id) {
                    report.preserved += 1;
                    continue;
                }
                s.artifacts.remove(&id);
                s.retention.artifact_disposals.insert(id.clone(), now);
                s.retention.disposals.push(Disposal {
                    kind: "artifact".into(),
                    key: id,
                    at_ms: now,
                });
                report.artifacts += 1;
            }
            _ => break,
        }
    }
    report.more = phase < 3;
    s.retention.gc_cursor = if report.more {
        Some(
            serde_json::to_string(&(phase, cursor, inclusive))
                .map_err(|e| Error::Refused(e.to_string()))?,
        )
    } else {
        None
    };
    let mut revisions = s
        .runs
        .values()
        .map(|r| r.catalog_revision)
        .chain(s.events.values().map(|e| e.catalog_revision))
        .collect::<BTreeSet<_>>();
    revisions.insert(s.catalog_revision);
    for (revision, at_ms) in &s.catalog_history_at_ms {
        if *at_ms >= cutoff(p.audit_ms) {
            revisions.insert(*revision);
        }
    }
    s.catalog_history
        .retain(|revision, _| revisions.contains(revision));
    s.catalog_history_at_ms
        .retain(|revision, _| revisions.contains(revision));
    s.retention
        .disposals
        .retain(|d| d.at_ms >= cutoff(p.audit_ms));
    s.retention
        .artifact_disposals
        .retain(|_, at| *at >= cutoff(p.audit_ms));
    s.indexes.receipts.clear();
    s.indexes.all_receipts.clear();
    for (i, r) in s.receipts.iter().enumerate() {
        s.indexes
            .receipts
            .entry(r.run.clone())
            .or_default()
            .insert(r.sequence, i);
        s.indexes.all_receipts.insert(r.sequence, i);
    }
    Ok(report)
}
#[cfg(test)]
mod window_tests {
    use super::*;
    use crate::KeyedAdmissionReceipt;
    /// A plain (format 2) State at clock 1_000 with `due` expired keyed
    /// receipts (each its own expiry) and `kept` unexpired ones.
    fn state(kept: usize, due: usize) -> State {
        let mut s = State::default();
        s.retention.clock_ms = Some(1_000);
        let expiries = (0..due)
            .map(|i| 500 + i as i64)
            .chain((0..kept).map(|i| 2_000 + i as i64));
        for (i, at) in expiries.enumerate() {
            let key = format!("k-{i:05}");
            s.keyed_admissions.insert(
                key.clone(),
                KeyedAdmissionReceipt {
                    key: key.clone(),
                    request_digest: "digest".into(),
                    run_id: format!("run-{i}"),
                    activation_revision: 1,
                    catalog_revision: 1,
                    accepted_at_ms: 0,
                },
            );
            s.retention.admission_expires_at_ms.insert(key.clone(), at);
            window_insert(&mut s, at, format!("{WINDOW_ADMISSION}{key}"));
        }
        s
    }
    fn steps(kept: usize, due: usize) -> (usize, CommitReport) {
        let mut s = state(kept, due);
        let mut report = CommitReport::default();
        WINDOW_STEPS.with(|steps| steps.set(0));
        drop_expired(&mut s, WINDOW_DROPS_PER_COMMIT, &mut report).unwrap();
        assert_eq!(s.keyed_admissions.len(), kept + due - report.admissions);
        (WINDOW_STEPS.with(std::cell::Cell::get), report)
    }
    #[test]
    fn window_work_on_a_plain_state_depends_on_entries_dropped_not_on_entries_kept() {
        // Nothing expired: one ordered lookup, however many entries are kept.
        assert_eq!(steps(10, 0).0, 1);
        assert_eq!(steps(10_000, 0).0, 1);
        // k dropped: one lookup, then one member and one lookup per entry.
        for k in [1, 8, 64] {
            let (work, report) = steps(10_000, k);
            assert_eq!((work, report.admissions), (1 + 2 * k, k), "k = {k}");
        }
        // More than 64 due: still 64.
        let (work, report) = steps(10_000, 200);
        assert_eq!((work, report.admissions), (1 + 2 * 64, 64));
    }
    #[test]
    fn a_member_behind_its_items_expiry_moves_to_it() {
        let mut s = state(0, 1);
        // The record says 2_000; the member is still at 500.
        s.retention
            .admission_expires_at_ms
            .insert("k-00000".into(), 2_000);
        let mut report = CommitReport::default();
        drop_expired(&mut s, WINDOW_DROPS_PER_COMMIT, &mut report).unwrap();
        assert_eq!(report, CommitReport::default());
        assert!(s.keyed_admissions.contains_key("k-00000"));
        assert_eq!(
            s.retention.expiry.into_plain(),
            BTreeMap::from([(window_key(2_000, "k:k-00000"), ())])
        );
    }
    #[test]
    fn window_keys_sort_by_expiry_then_member() {
        let at = [i64::MIN, -1, 0, 1, 999, 1_000, i64::MAX];
        let keys: Vec<String> = at.iter().map(|at| window_key(*at, "k:x")).collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "{keys:?}");
        assert!(window_key(5, "e:z") < window_key(6, "e:a"));
        assert!(window_key(5, "e:a") < window_key(5, "k:a"));
        for at in at {
            assert_eq!(window_entry(&window_key(at, "k:x")), Some((at, "k:x")));
        }
    }
    /// The order is one record per entry: a burst in one millisecond adds
    /// records, it does not grow one.
    #[test]
    fn a_same_millisecond_burst_is_one_record_per_entry() {
        let mut s = State::default();
        for i in 0..100 {
            window_insert(&mut s, 7, format!("{WINDOW_EVENT}e-{i:03}"));
        }
        assert_eq!(s.retention.expiry.len(), 100);
        assert!(s.retention.expiry.keys().all(|k| k.len() == 16 + 2 + 5));
    }
    /// A State written by Task 6's first commit (`expiry` as expiry → set of
    /// members) is refused when decoded, not silently emptied.
    #[test]
    fn the_first_task_6_expiry_shape_is_refused() {
        let old = serde_json::json!({"1000": ["k:x"]});
        assert!(serde_json::from_value::<LazyMap<String, ()>>(old).is_err());
    }
}
