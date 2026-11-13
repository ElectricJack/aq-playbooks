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
    /// Window entries by expiry: `WINDOW_EVENT` or `WINDOW_ADMISSION` plus
    /// the entry's key.
    #[serde(default)]
    pub expiry: LazyMap<i64, BTreeSet<String>>,
    /// Expiry of each keyed admission receipt that has one recorded.
    #[serde(default)]
    pub admission_expires_at_ms: LazyMap<String, i64>,
}
/// `RetentionState::expiry` member prefix of an admitted event.
pub const WINDOW_EVENT: &str = "e:";
/// `RetentionState::expiry` member prefix of a keyed admission receipt.
pub const WINDOW_ADMISSION: &str = "k:";
pub const REFUSE_RUN_REMOVAL: &str = "run removal requires a history record in the same commit";
pub const REFUSE_EARLY_REMOVAL: &str = "window entry removed before its expiry";
pub const REFUSE_CLOCK: &str = "retention clock cannot decrease";
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
/// [`check_expiry_change`] for record `key` of `window`, against `next`.
pub(crate) fn check_expiry_record(
    next: &State,
    window: Window,
    key: &str,
    old: Option<i64>,
    new: Option<i64>,
) -> std::result::Result<(), StorageError> {
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
/// The clock never decreases, and expiry records follow
/// [`check_expiry_change`]. Event tombstones and correlation fences may
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
    if previous
        .retention
        .clock_ms
        .is_some_and(|old| clock.is_none_or(|new| new < old))
    {
        return Err(refuse(REFUSE_CLOCK));
    }
    for (key, receipt) in &previous.keyed_admissions {
        match next.keyed_admissions.get(key) {
            Some(kept) if kept == receipt => {}
            Some(_) => return Err(refuse(REFUSE_RECEIPT_CHANGE)),
            None if admission_expiry(previous, key).is_some_and(|at| expired(at, clock)) => {}
            None => return Err(refuse(REFUSE_EARLY_REMOVAL)),
        }
    }
    let mut removals = Removals::default();
    for (key, event) in &previous.events {
        if next.events.contains_key(key) {
            continue;
        }
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
/// The retention work of every commit that changes the State, after its
/// indexes are synchronized: move the clock to the commit's host time, remove
/// the settled cohorts among `candidates` (see [`candidate_roots`]), and move
/// `"admin"` receipts to history.
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
