//! Replay windows, rolling bounds, history and epoch fences. No unresolved
//! obligation is ever evicted.
//!
//! The retention clock (`RetentionState::clock_ms`) is the latest host time a
//! commit carried. A removal from the State is allowed only when the commit
//! writes its history (a settled cohort), or when it is a window entry whose
//! expiry the clock has reached or that the rolling bound evicts. Nothing
//! writes event tombstones, correlation fences or disposal records any more;
//! those an older engine left are drained ([`drain_step`]). [`verify_removals`] is that one check, shared by
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
    /// Deprecated and ignored: a finished run leaves the record when it
    /// finishes. Accepted (and defaulted when absent) so stored policies
    /// decode.
    #[serde(default = "default_terminal_run_ms")]
    pub terminal_run_ms: u64,
    pub audit_ms: u64,
    /// Deprecated and ignored: there are no namespace limits. Accepted (and
    /// defaulted when absent) so stored policies decode.
    #[serde(default = "default_max_tombstones")]
    pub max_tombstones: usize,
    /// Window of a keyed admission receipt, from its acceptance.
    #[serde(default = "default_admission_replay_ms")]
    pub admission_replay_ms: u64,
    /// How many window entries (events and keyed admission receipts
    /// together) that no live work holds the engine remembers: past it the
    /// oldest are evicted, whatever their window ([`evict`]). Positive.
    #[serde(default = "default_replay_keep")]
    pub replay_keep: usize,
    /// How many finished runs history keeps: each commit that writes
    /// history deletes the oldest beyond it (a bounded number per commit,
    /// see [`crate::history::HISTORY_TRIM_RUNS_PER_COMMIT`]). `0` writes no
    /// history at all (inspection then finds no finished run).
    #[serde(default = "default_history_keep")]
    pub history_keep: u64,
    /// The drain of a namespace an older engine wrote ([`drain_step`]):
    /// at most this many roots examined, and settled cohorts moved to
    /// history, per commit (together with those [`settle`] moves).
    /// Positive.
    #[serde(default = "default_drain_cohorts")]
    pub drain_cohorts_per_commit: usize,
    /// The drain's other work per commit: at most this many legacy entries
    /// (tombstones, correlation fences, disposals, admin receipts, keyed
    /// receipts and events examined), and at most this many PostgreSQL
    /// legacy tombstone rows deleted. Positive.
    #[serde(default = "default_drain_entries")]
    pub drain_entries_per_commit: usize,
    /// At most this many expired window entries examined (dropped or
    /// deferred) per commit ([`drop_expired`]). Positive.
    #[serde(default = "default_window_drops")]
    pub window_drops_per_commit: usize,
    /// At most this many window entries evicted past `replay_keep` per
    /// commit ([`evict`]); the removal fences allow no more. Positive.
    #[serde(default = "default_replay_evictions")]
    pub replay_evictions_per_commit: usize,
    /// A commit that writes history trims, of each rolling set, at most
    /// this many more than it writes ([`crate::history::trim_budget`]).
    /// Positive.
    #[serde(default = "default_history_trim")]
    pub history_trim_per_commit: usize,
    /// The largest page `collect_garbage` accepts (its `limit`), which also
    /// bounds the PostgreSQL artifact catalog delete of a GC commit.
    /// Positive.
    #[serde(default = "default_gc_page_limit")]
    pub gc_page_limit: usize,
    /// At most this many cohorts that settle in a commit leave the State in
    /// it ([`settle`]); the rest re-arm the drain, which takes them in later
    /// commits. Positive.
    #[serde(default = "default_settle_cohorts")]
    pub settle_cohorts_per_commit: usize,
}
fn default_settle_cohorts() -> usize {
    COHORTS_PER_COMMIT
}
fn default_window_drops() -> usize {
    WINDOW_DROPS_PER_COMMIT
}
fn default_replay_evictions() -> usize {
    REPLAY_EVICTIONS_PER_COMMIT
}
fn default_history_trim() -> usize {
    crate::history::HISTORY_TRIM_RUNS_PER_COMMIT
}
fn default_gc_page_limit() -> usize {
    GC_PAGE_LIMIT
}
/// The default of `RetentionPolicy::gc_page_limit`.
pub const GC_PAGE_LIMIT: usize = 1_000;
fn default_drain_cohorts() -> usize {
    50
}
fn default_drain_entries() -> usize {
    500
}
fn default_replay_keep() -> usize {
    256
}
fn default_history_keep() -> u64 {
    10_000
}
fn default_admission_replay_ms() -> u64 {
    604_800_000
}
fn default_terminal_run_ms() -> u64 {
    604_800_000
}
fn default_max_tombstones() -> usize {
    100_000
}
impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            event_replay_ms: 86_400_000,
            terminal_run_ms: default_terminal_run_ms(),
            audit_ms: 604_800_000,
            max_tombstones: default_max_tombstones(),
            admission_replay_ms: default_admission_replay_ms(),
            replay_keep: default_replay_keep(),
            history_keep: default_history_keep(),
            drain_cohorts_per_commit: default_drain_cohorts(),
            drain_entries_per_commit: default_drain_entries(),
            window_drops_per_commit: default_window_drops(),
            replay_evictions_per_commit: default_replay_evictions(),
            history_trim_per_commit: default_history_trim(),
            gc_page_limit: default_gc_page_limit(),
            settle_cohorts_per_commit: default_settle_cohorts(),
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
    /// The window entries no live work holds, in acceptance order: one
    /// record each, keyed [`replay_key`]`(acceptance sequence, member)`.
    /// Past `policy.replay_keep` the oldest are evicted.
    #[serde(default)]
    pub replay_order: LazyMap<String, ()>,
    /// Keyed admission receipts whose run is live, by run id: each joins
    /// `replay_order` when its run leaves the record.
    #[serde(default)]
    pub replay_held: LazyMap<String, String>,
    /// Progress of the drain of what an older engine left in the record
    /// ([`drain_step`]). Absent in a State written before it: the drain
    /// then runs.
    #[serde(default)]
    pub drain: DrainState,
    /// Events still to be judged again because the uncertain operations of
    /// their contract and scope changed (a hold through an uncertain
    /// completion began, ended or lapsed): by contract key, the acceptance
    /// sequence after which [`rejoin`] continues. It judges at most
    /// `policy.window_drops_per_commit` of them per commit. Empty (and not
    /// written) when nothing is pending.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rejudge: BTreeMap<String, u64>,
}
/// Progress of the drain ([`drain_step`]), stored with the State so a
/// restart continues where it stopped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainState {
    /// Nothing is left to drain: every commit's drain work is one check.
    pub done: bool,
    /// The last root the cohort scan examined (`None`: from the first).
    pub root_cursor: Option<String>,
    /// The cohort scan has passed the last root since it was (re)armed.
    #[serde(default)]
    pub roots_done: bool,
    /// The entry phase: one of the `DRAIN_*` phases.
    pub phase: u8,
    /// The last key the entry phase examined (`None`: from the first).
    pub cursor: Option<String>,
    /// The namespace sequence when the drain began: events at or before it
    /// were accepted by the older engine, and the event phase judges them.
    #[serde(default)]
    pub legacy_sequence: Option<u64>,
}
/// The [`RetentionState::replay_order`] key of `member` accepted at
/// `sequence`: the sequence as 16 lowercase hex digits, then the member.
/// Byte order of keys is acceptance order.
pub fn replay_key(sequence: u64, member: &str) -> String {
    format!("{sequence:016x}{member}")
}
/// The acceptance sequence and member of a [`replay_key`].
pub fn replay_entry(key: &str) -> Option<(u64, &str)> {
    Some((
        u64::from_str_radix(key.get(..16)?, 16).ok()?,
        key.get(16..)?,
    ))
}
/// The acceptance sequence of a keyed admission receipt: that of its run
/// id (`run-<16 hex>`, assigned by the admitting commit), else 0.
pub fn admission_sequence(receipt: &crate::KeyedAdmissionReceipt) -> u64 {
    receipt
        .run_id
        .strip_prefix("run-")
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .unwrap_or(0)
}
/// The [`RetentionState::replay_order`] key of a keyed admission receipt.
pub fn admission_order_key(receipt: &crate::KeyedAdmissionReceipt) -> String {
    replay_key(
        admission_sequence(receipt),
        &format!("{WINDOW_ADMISSION}{}", receipt.key),
    )
}
/// The [`RetentionState::replay_order`] key of an event.
pub fn event_order_key(event: &crate::AcceptedEvent) -> String {
    replay_key(event.sequence, &format!("{WINDOW_EVENT}{}", event.key))
}
/// The default of `RetentionPolicy::replay_evictions_per_commit`: entries
/// at the head of the eviction order examined (evicted, or taken out of the
/// order because live work holds them) per commit. Counted apart from the
/// window drop's own bound.
pub const REPLAY_EVICTIONS_PER_COMMIT: usize = 64;
/// Refusal of a commit that evicts a window entry other than as
/// [`evict`] does (see [`verify_removals`]).
pub const REFUSE_EVICTION: &str = "window entry evicted without its licence";
/// Refusal of a commit that changes an accepted event's identity (its key,
/// sequence, digest, acceptance time or envelope); only its destinations
/// change (fan-out).
pub const REFUSE_EVENT_CHANGE: &str = "immutable accepted event changed";
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Window {
    Event,
    Admission,
}
impl Window {
    fn member(value: &str) -> Option<(Self, &str)> {
        if let Some(key) = value.strip_prefix(WINDOW_ADMISSION) {
            return Some((Self::Admission, key));
        }
        value
            .strip_prefix(WINDOW_EVENT)
            .map(|key| (Self::Event, key))
    }
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
mod licence;
use licence::check_licence;
pub(crate) use licence::{ExpiryChange, check_licensed_header};
pub use licence::{Licence, carries_licence, check_floor_unchanged, licence, rebase_cap};

/// Every expiry record change between two complete States.
fn check_expiry_records(previous: &State, next: &State) -> std::result::Result<(), StorageError> {
    Window::Event.check_records(previous, next)?;
    Window::Admission.check_records(previous, next)
}
impl Window {
    fn expires(self, state: &State) -> &LazyMap<String, i64> {
        match self {
            Window::Admission => &state.retention.admission_expires_at_ms,
            Window::Event => &state.retention.event_expires_at_ms,
        }
    }
    fn check_records(
        self,
        previous: &State,
        next: &State,
    ) -> std::result::Result<(), StorageError> {
        let (old, new) = (self.expires(previous), self.expires(next));
        for (key, at) in old {
            let after = new.get(key).copied();
            if after != Some(*at) {
                ExpiryChange {
                    window: self,
                    key,
                    old: Some(*at),
                    new: after,
                }
                .check(previous, next)?;
            }
        }
        for (key, at) in new.iter().filter(|(key, _)| !old.contains_key(*key)) {
            ExpiryChange {
                window: self,
                key,
                old: None,
                new: Some(*at),
            }
            .check(previous, next)?;
        }
        Ok(())
    }
}
/// Move the clock to `now_ms` unless it is already later.
pub(crate) fn advance_clock(s: &mut State, now_ms: i64) {
    let clock = s
        .retention
        .clock_ms
        .map_or(now_ms, |clock| clock.max(now_ms));
    s.retention.clock_ms = Some(clock);
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
mod removals;
pub(crate) use removals::check_removals;
pub use removals::verify_removals;
/// The default of `RetentionPolicy::settle_cohorts_per_commit`: settled
/// cohorts that leave the State in one commit.
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
    /// Window entries evicted by count ([`evict`]).
    pub evicted: usize,
    pub drained: usize,
    /// More bounded work is due than this commit's bounds allowed: settled
    /// cohorts past the settle bound, the drain of an older engine's
    /// namespace not done, expired window entries past the drop bound, or
    /// entries past `replay_keep` past the eviction bound.
    pub more: bool,
}
mod cohorts;
pub(crate) use cohorts::{candidate_roots, move_unowned_receipts, settle};
use cohorts::{cohort_settled, move_admin_receipts, receipt_offsets, take_cohorts, take_receipts};
/// The default of `RetentionPolicy::window_drops_per_commit`: window
/// entries examined (dropped or deferred) per commit, oldest expiry first.
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
/// of entries above it (at most the remembered window). Returns (events,
/// admissions) capped. The caller writes the [`CLOCK_REBASE`] receipt that
/// licenses the commit.
pub(crate) fn rebase_clock(s: &mut State, to_ms: i64) -> (usize, usize) {
    let policy = &s.retention.policy;
    let lowest = to_ms.saturating_add(
        window(policy.admission_replay_ms).min(window(policy.event_replay_ms).max(1)),
    );
    let keys = expiry_keys_after(s, lowest);
    let (mut events, mut admissions) = (0, 0);
    for key in keys {
        let Some((_, member)) = window_entry(&key) else {
            continue;
        };
        let member = member.to_string();
        let Some((window, item)) = Window::member(&member) else {
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
fn expiry_keys_after(s: &State, lowest: i64) -> Vec<String> {
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
    keys
}
/// The end of a licensed admin commit (a clock rebase or a floor
/// lowering): only its own `"admin"` receipt (written after `started`)
/// moves to history. The clock is not advanced, nothing settles, nothing is
/// dropped or drained, so the commit is exactly its licence.
pub(crate) fn finish_licensed_commit(s: &mut State, started: u64) -> Result<CommitReport> {
    let mut report = CommitReport::default();
    move_admin_receipts(s, started, &mut report)?;
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
    if let Some(receipt) = s.keyed_admissions.remove(key) {
        s.retention
            .replay_order
            .remove(&admission_order_key(&receipt));
        if s.retention.replay_held.get(&receipt.run_id) == Some(&receipt.key) {
            s.retention.replay_held.remove(&receipt.run_id);
        }
    }
    s.retention.admission_expires_at_ms.remove(key);
}
mod holds;
use holds::EventHolds;
pub(crate) use holds::event_held;
pub use holds::{StoredHolds, check_event_change};

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
    s.retention.replay_order.remove(&event_order_key(&event));
    crate::indexes::unindex_contract_event(s, &event)?;
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
        let Some(key) = head.clone() else {
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
        drop_member(s, &member, clock, report)?;
    }
    // Stopped at the bound with an expired entry still at the head.
    if head
        .as_deref()
        .and_then(window_entry)
        .is_some_and(|(at, _)| at <= clock)
    {
        report.more = true;
    }
    Ok(())
}
fn drop_member(s: &mut State, member: &str, clock: i64, report: &mut CommitReport) -> Result<()> {
    match Window::member(member) {
        Some((Window::Admission, key)) => drop_admission(s, key, clock, report),
        Some((Window::Event, key)) => drop_event(s, key, clock, report)?,
        None => {}
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
/// Ruling C (Task 10): the uncertain operations whose hold has lapsed (the
/// clock is past their command deadline plus their completion event's
/// window, [`crate::indexes::hold_lapsed`]) leave the uncertain index, so
/// [`rejoin`] judges the events they held again: unheld now, those join
/// the eviction order at their acceptance positions. O(uncertain
/// operations): nothing when no handle is lost.
fn lapse_holds(s: &mut State) -> Result<()> {
    if s.indexes.uncertain_contract_membership.is_empty() {
        return Ok(());
    }
    let lapsed: Vec<String> = s
        .indexes
        .uncertain_contract_membership
        .keys()
        .filter(|id| {
            s.intents
                .get(*id)
                .is_some_and(|i| crate::indexes::hold_lapsed(s, i))
        })
        .cloned()
        .collect();
    if lapsed.is_empty() {
        return Ok(());
    }
    for id in &lapsed {
        crate::indexes::mark_intent(s, id);
    }
    crate::indexes::sync(s)
}
mod replay;
pub use replay::ReplayTouched;
pub(crate) use replay::rejoin;
use replay::sync_event_order;
mod evictions;
pub(crate) use evictions::evict;
pub use evictions::{Memberships, Stored, StoredWaiters, check_evictions};
/// The retention work of every commit that changes the State, after its
/// indexes are synchronized: move the clock to the commit's host time, remove
/// the settled cohorts among `candidates` (see [`candidate_roots`]), drain a
/// bounded amount of what an older engine left ([`drain_step`]), put the
/// window entries whose hold this commit may have changed in or out of the
/// eviction order ([`rejoin`]), drop expired window entries
/// ([`drop_expired`]), evict the oldest entries past `policy.replay_keep`
/// ([`evict`]) and move the `"admin"` receipts this commit wrote (after
/// `started`, the sequence it began at) to history. The clock step and the
/// drop need host time; eviction and the drain do not.
pub(crate) fn finish_commit(
    s: &mut State,
    now_ms: Option<i64>,
    candidates: BTreeSet<String>,
    started: u64,
) -> Result<CommitReport> {
    let mut report = CommitReport::default();
    if let Some(now_ms) = now_ms {
        advance_clock(s, now_ms);
    }
    let settle_bound = s.retention.policy.settle_cohorts_per_commit;
    settle(s, candidates, settle_bound, &mut report)?;
    drain_step(s, started, &mut report)?;
    if !s.retention.drain.done {
        report.more = true;
    }
    lapse_holds(s)?;
    rejoin(s, &mut report)?;
    if now_ms.is_some() {
        let drops = s.retention.policy.window_drops_per_commit;
        drop_expired(s, drops, &mut report)?;
    }
    let evictions = s.retention.policy.replay_evictions_per_commit;
    evict(s, evictions, &mut report)?;
    move_admin_receipts(s, started, &mut report)?;
    Ok(report)
}
mod drain;
#[cfg(test)]
use drain::DRAIN_STEPS;
pub(crate) use drain::drain_step;
pub use drain::{
    DRAIN_ADMIN, DRAIN_DISPOSALS, DRAIN_DONE, DRAIN_EVENTS, DRAIN_FENCES, DRAIN_KEYED,
    DRAIN_TOMBSTONES,
};

mod artifacts;
pub(crate) use artifacts::gc;
pub use artifacts::{GcReport, artifact_referenced, verify_artifact_removals};
#[cfg(test)]
mod window_tests;
