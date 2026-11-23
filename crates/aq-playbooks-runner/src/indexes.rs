//! Durable bounded discovery indexes. These are maintained in the same CAS as
//! authoritative state; adapters may mirror them into indexed native tables.
use crate::lazy::{LazyMap, LazySet};
use crate::{IntentStatus, Pending, Result, RunStatus, State};
use aq_playbooks_model::{ContractRef, digest};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectQueueKind {
    Dispatch,
    Cancel,
    Recovery,
    Completion,
}
impl EffectQueueKind {
    pub fn key(self) -> &'static str {
        match self {
            Self::Dispatch => "dispatch",
            Self::Cancel => "cancel",
            Self::Recovery => "recovery",
            Self::Completion => "completion",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "query", rename_all = "snake_case")]
pub enum IndexQuery {
    Ready {
        scope_after: Option<String>,
        limit: usize,
    },
    DueTimers {
        now_ms: i64,
        limit: usize,
    },
    Effects {
        kind: EffectQueueKind,
        scope_after: Option<String>,
        limit: usize,
    },
    CorrelatedEvents {
        key: String,
        after_sequence: u64,
        limit: usize,
    },
    Waiters {
        key: String,
        limit: usize,
    },
    Runs {
        after: Option<String>,
        limit: usize,
    },
    /// Receipts after `after_sequence`, of one run or of all. Each item's
    /// `key` is the receipt's sequence as 20 zero-padded digits.
    Receipts {
        run: Option<String>,
        after_sequence: u64,
        limit: usize,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexItem {
    pub key: String,
    pub scope: String,
    pub sequence: u64,
    pub at_ms: Option<i64>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IndexPage {
    pub items: Vec<IndexItem>,
    pub next: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimerIndex {
    pub key: String,
    pub run: String,
    pub at_ms: i64,
    pub intent: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeIndexes {
    pub ready: BTreeMap<String, BTreeMap<u64, String>>,
    pub ready_tokens: BTreeMap<String, (String, u64)>,
    pub scope_cursor: Option<String>,
    pub dispatch_turn: bool,
    pub effect_cursor: BTreeMap<String, String>,
    pub effect_item_cursor: BTreeMap<String, String>,
    pub root_members: LazyMap<String, BTreeSet<String>>,
    pub root_active: LazyMap<String, BTreeSet<String>>,
    pub root_effects: LazyMap<String, BTreeSet<String>>,
    /// Every intent (terminal included) per root, for bounded history checks.
    #[serde(default)]
    pub root_intents: LazyMap<String, BTreeSet<String>>,
    /// Deprecated: the latest receipt time of each root, read only by the
    /// collector spec 0003 retired (its age test). No longer written; an
    /// entry an older engine wrote leaves with its cohort.
    pub root_last_activity: LazyMap<String, i64>,
    pub run_roots: LazyMap<String, String>,
    pub intent_roots: LazyMap<String, String>,
    pub effects: LazyMap<String, BTreeMap<String, BTreeSet<String>>>,
    pub effect_memberships: LazyMap<String, Vec<(String, String)>>,
    pub timers: LazyMap<String, TimerIndex>,
    pub due: LazyMap<i64, BTreeSet<String>>,
    pub run_timer: LazyMap<String, String>,
    pub events: LazyMap<String, BTreeMap<u64, String>>,
    pub event_memberships: LazyMap<String, Vec<String>>,
    /// The events of each contract and scope (`trigger_key`) by acceptance
    /// sequence, so a change of that contract's uncertain completions
    /// re-judges the holds of the events accepted after the operation's
    /// dispatch, and only those.
    #[serde(default)]
    pub contract_events: LazyMap<String, BTreeMap<u64, String>>,
    pub waiters: LazyMap<String, BTreeSet<String>>,
    pub run_subscriptions: LazyMap<String, Vec<String>>,
    pub dependents: LazyMap<String, BTreeSet<String>>,
    pub run_dependencies: LazyMap<String, Vec<String>>,
    pub completion_ready: BTreeSet<String>,
    pub uncertain_completion_contracts: BTreeMap<String, BTreeSet<String>>,
    pub uncertain_contract_membership: BTreeMap<String, String>,
    pub dirty_runs: BTreeSet<String>,
    pub dirty_intents: BTreeSet<String>,
    pub runs: LazySet<String>,
    pub receipts: LazyMap<String, BTreeMap<u64, usize>>,
    pub all_receipts: LazyMap<u64, usize>,
    pub triggers: BTreeMap<String, BTreeSet<String>>,
    pub activation_triggers: BTreeMap<String, Vec<String>>,
    pub initialized: bool,
    /// `root_intents` holds every live operation. False in a State written
    /// before the index existed (pre-D10); `Engine::new` then backfills it
    /// once. Cohort removal takes a cohort's operations from it.
    #[serde(default)]
    pub root_intents_complete: bool,
    /// `contract_events` holds every event. False in a State written before
    /// the index existed; the drain then fills it, a bounded number of
    /// events per commit (`retention::drain_step`).
    #[serde(default)]
    pub contract_events_complete: bool,
    /// Every dispatched live operation carries its dispatch sequence. False
    /// in a State written before `Intent::dispatch_sequence` existed;
    /// `Engine::new` then fills it once from each operation's own first
    /// `dispatch` receipt.
    #[serde(default)]
    pub dispatch_sequences_complete: bool,
}
impl IndexQuery {
    /// Maximum number of items a storage adapter may return for this query.
    pub fn page_limit(&self) -> usize {
        match self {
            IndexQuery::Ready { limit, .. }
            | IndexQuery::DueTimers { limit, .. }
            | IndexQuery::Effects { limit, .. }
            | IndexQuery::CorrelatedEvents { limit, .. }
            | IndexQuery::Waiters { limit, .. }
            | IndexQuery::Runs { limit, .. }
            | IndexQuery::Receipts { limit, .. } => *limit,
        }
        .min(1000)
    }

    /// Attach the continuation cursor specified for this query.
    pub fn page(&self, items: Vec<IndexItem>) -> IndexPage {
        let next = items.last().map(|i| match self {
            IndexQuery::Ready { .. } | IndexQuery::Effects { .. } => i.scope.clone(),
            IndexQuery::Receipts { .. } | IndexQuery::CorrelatedEvents { .. } => {
                i.sequence.to_string()
            }
            _ => i.key.clone(),
        });
        IndexPage { items, next }
    }
}

pub fn correlation_key(
    contract: &ContractRef,
    scope: &str,
    correlation: &BTreeMap<String, String>,
) -> Result<String> {
    Ok(digest(&(contract, scope, correlation))?)
}
pub fn field_key(contract: &ContractRef, scope: &str, field: &str, value: &str) -> Result<String> {
    Ok(digest(&(contract, scope, field, value))?)
}
pub fn trigger_key(contract: &ContractRef, scope: &str) -> Result<String> {
    Ok(digest(&(contract, scope))?)
}
mod query;

pub(crate) fn mark_run(s: &mut State, id: &str) {
    s.indexes.dirty_runs.insert(id.into());
}
pub(crate) fn mark_intent(s: &mut State, id: &str) {
    s.indexes.dirty_intents.insert(id.into());
}
pub(crate) fn wake(s: &mut State, id: &str) {
    if !s.runs.contains_key(id) {
        return;
    }
    if s.runs[id].status.terminal() {
        return;
    }
    if !s.indexes.ready_tokens.contains_key(id) {
        s.sequence += 1;
        let scope = s.runs[id].authority.scope.clone();
        s.indexes
            .ready
            .entry(scope.clone())
            .or_default()
            .insert(s.sequence, id.into());
        s.indexes
            .ready_tokens
            .insert(id.into(), (scope, s.sequence));
    }
}
pub(crate) fn unqueue(s: &mut State, id: &str) {
    if let Some((scope, seq)) = s.indexes.ready_tokens.remove(id)
        && let Some(q) = s.indexes.ready.get_mut(&scope)
    {
        q.remove(&seq);
        if q.is_empty() {
            s.indexes.ready.remove(&scope);
        }
    }
}
pub(crate) fn wake_dependents(s: &mut State, id: &str) {
    let ids = s.indexes.dependents.get(id).cloned().unwrap_or_default();
    for id in ids {
        wake(s, &id);
    }
}
pub(crate) fn remove_timer(s: &mut State, key: &str) {
    if let Some(t) = s.indexes.timers.remove(key)
        && let Some(ids) = s.indexes.due.get_mut(&t.at_ms)
    {
        ids.remove(key);
        if ids.is_empty() {
            s.indexes.due.remove(&t.at_ms);
        }
    }
}
pub(crate) fn timer(s: &mut State, t: TimerIndex) {
    remove_timer(s, &t.key);
    s.indexes
        .due
        .entry(t.at_ms)
        .or_default()
        .insert(t.key.clone());
    s.indexes.timers.insert(t.key.clone(), t);
}
fn unsubscribe(s: &mut State, id: &str) {
    if let Some(keys) = s.indexes.run_subscriptions.remove(id) {
        for key in keys {
            remove_lazy_member(&mut s.indexes.waiters, &key, id);
            s.replay_touched.wait_keys.insert(key);
        }
    }
}
fn subscribe(s: &mut State, run: &str, key: String) {
    s.replay_touched.wait_keys.insert(key.clone());
    s.indexes
        .waiters
        .entry(key.clone())
        .or_default()
        .insert(run.into());
    s.indexes
        .run_subscriptions
        .entry(run.into())
        .or_default()
        .push(key);
}
fn depend(s: &mut State, run: &str, id: String) {
    s.indexes
        .dependents
        .entry(id.clone())
        .or_default()
        .insert(run.into());
    s.indexes
        .run_dependencies
        .entry(run.into())
        .or_default()
        .push(id);
}
pub(crate) fn index_event(s: &mut State, id: &str) -> Result<()> {
    let e = s.events[id].clone();
    let mut keys = vec![correlation_key(
        &e.envelope.contract,
        &e.envelope.scope,
        &e.envelope.correlation,
    )?];
    for (field, value) in &e.envelope.correlation {
        keys.push(field_key(
            &e.envelope.contract,
            &e.envelope.scope,
            field,
            value,
        )?);
    }
    for key in &keys {
        s.indexes
            .events
            .entry(key.clone())
            .or_default()
            .insert(e.sequence, id.into());
        let targets = s.indexes.waiters.get(key).cloned().unwrap_or_default();
        for target in targets {
            if let Some(intent) = target.strip_prefix("intent:") {
                s.indexes.completion_ready.insert(intent.into());
            } else {
                wake(s, &target);
            }
        }
    }
    s.indexes.event_memberships.insert(id.into(), keys);
    let contract = trigger_key(&e.envelope.contract, &e.envelope.scope)?;
    s.indexes
        .contract_events
        .entry(contract)
        .or_default()
        .insert(e.sequence, id.into());
    Ok(())
}
/// Take event `e` out of `contract_events`.
pub(crate) fn unindex_contract_event(s: &mut State, e: &crate::AcceptedEvent) -> Result<()> {
    let contract = trigger_key(&e.envelope.contract, &e.envelope.scope)?;
    if let Some(events) = s.indexes.contract_events.get_mut(&contract) {
        events.remove(&e.sequence);
        if events.is_empty() {
            s.indexes.contract_events.remove(&contract);
        }
    }
    Ok(())
}
pub(crate) fn index_activation(s: &mut State, binding: &str) -> Result<()> {
    if let Some(keys) = s.indexes.activation_triggers.remove(binding) {
        for key in keys {
            remove_member(&mut s.indexes.triggers, &key, binding);
        }
    }
    let Some(a) = s.bindings.get(binding) else {
        return Ok(());
    };
    if !a.controls.event_starts_enabled {
        return Ok(());
    }
    let artifact = &s.artifacts[&a.artifact];
    let scope = a.authority.scope.clone();
    let keys = artifact
        .content
        .playbook
        .triggers
        .iter()
        .map(|t| trigger_key(&t.event, &scope))
        .collect::<Result<Vec<_>>>()?;
    for key in &keys {
        s.indexes
            .triggers
            .entry(key.clone())
            .or_default()
            .insert(binding.into());
    }
    s.indexes.activation_triggers.insert(binding.into(), keys);
    Ok(())
}
mod runs;
use runs::refresh_run;

mod intents;
use intents::refresh_intent;

/// Whether `i` (asynchronous) may still be completed by an event its handle
/// does not name: it was dispatched, has no handle and is not settled. A
/// recovery that prepares it again does not end that: the first issue may
/// still answer. An operation never dispatched cannot be completed.
pub(crate) fn uncertain(i: &crate::Intent) -> bool {
    i.dispatch_sequence.is_some()
        && i.handle.is_none()
        && matches!(
            i.status,
            IntentStatus::Prepared | IntentStatus::Dispatched | IntentStatus::Unknown
        )
}
/// The window of the completion event of asynchronous operation `i`:
/// `max(policy.event_replay_ms, contract.retention_ms)` (the policy's alone
/// if its artifact or contract is not found).
pub(crate) fn completion_window(s: &State, i: &crate::Intent) -> u64 {
    let contract_ms = i
        .command
        .asynchronous
        .as_ref()
        .and_then(|a| {
            let run = s.runs.get(&i.run)?;
            let artifact = s.artifacts.get(&run.artifact)?;
            artifact.content.registry.events.get(&a.completion_event)
        })
        .map_or(0, |event| event.retention_ms);
    s.retention.policy.event_replay_ms.max(contract_ms)
}
/// Ruling C (Task 10): an uncertain operation holds the events of its
/// completion contract only until the retention clock is past its command
/// deadline plus the completion event's window. From then on those events
/// are ordinary unheld entries (evicted by `replay_keep`, dropped by time),
/// so a lost handle no operator recovers does not hold them for ever.
pub(crate) fn hold_lapsed(s: &State, i: &crate::Intent) -> bool {
    let window = i64::try_from(completion_window(s, i)).unwrap_or(i64::MAX);
    s.retention
        .clock_ms
        .is_some_and(|clock| clock >= i.deadline_at_ms.saturating_add(window))
}
pub(crate) fn sync(s: &mut State) -> Result<()> {
    let intents = std::mem::take(&mut s.indexes.dirty_intents);
    for id in intents {
        refresh_intent(s, &id)?;
    }
    let runs = std::mem::take(&mut s.indexes.dirty_runs);
    for id in runs {
        refresh_run(s, &id)?;
    }
    Ok(())
}
pub(crate) fn rebuild(s: &mut State) -> Result<()> {
    s.catalog_history
        .entry(s.catalog_revision)
        .or_insert_with(|| s.bindings.clone());
    s.indexes = RuntimeIndexes::default();
    let events = s.events.keys().cloned().collect::<Vec<_>>();
    for id in events {
        index_event(s, &id)?;
    }
    let bindings = s.bindings.keys().cloned().collect::<Vec<_>>();
    for id in bindings {
        index_activation(s, &id)?;
    }
    s.indexes.dirty_runs = s.runs.keys().cloned().collect();
    s.indexes.dirty_intents = s.intents.keys().cloned().collect();
    let receipts = s
        .receipts
        .iter()
        .enumerate()
        .map(|(i, r)| (i, r.run.clone(), r.sequence))
        .collect::<Vec<_>>();
    for (i, run, seq) in receipts {
        s.indexes.receipts.entry(run).or_default().insert(seq, i);
        s.indexes.all_receipts.insert(seq, i);
    }
    sync(s)?;
    s.indexes.initialized = true;
    s.indexes.root_intents_complete = true;
    s.indexes.contract_events_complete = true;
    // A rebuild drops the dirty sets, so cohorts settled before it are
    // seen by no settle: the drain scans for them again.
    s.retention.drain = crate::retention::DrainState::default();
    Ok(())
}
/// Once per namespace written before `root_intents` existed (pre-D10, whose
/// indexes are otherwise initialized): put every live operation into
/// `root_intents` under its root and set `root_intents_complete`. One pass
/// over live operations; operations of a run without a known root are left
/// out (they cannot belong to a cohort). Returns whether it ran.
pub(crate) fn backfill_root_intents(s: &mut State) -> bool {
    if !s.indexes.initialized || s.indexes.root_intents_complete {
        return false;
    }
    let roots: Vec<(String, String)> = s
        .intents
        .values()
        .filter_map(|i| {
            s.runs
                .get(&i.run)
                .map(|r| r.root.clone())
                .or_else(|| s.indexes.run_roots.get(&i.run).cloned())
                .or_else(|| s.indexes.intent_roots.get(&i.id).cloned())
                .map(|root| (i.id.clone(), root))
        })
        .collect();
    for (id, root) in roots {
        if !s
            .indexes
            .root_intents
            .get(&root)
            .is_some_and(|ids| ids.contains(&id))
        {
            s.indexes.root_intents.entry(root).or_default().insert(id);
        }
    }
    s.indexes.root_intents_complete = true;
    true
}
/// Once per namespace written before `Intent::dispatch_sequence` existed:
/// give every live asynchronous operation without a handle that an older
/// engine dispatched the sequence of its own first `dispatch` receipt (its
/// run is live, so its receipts are in the record), so it holds only the
/// events accepted after it. One without such a receipt was never
/// dispatched and holds nothing. Each is marked, so the next sync records
/// its uncertain membership by the current rule. One pass over live
/// operations, plus each candidate's run receipts. Returns whether it ran.
pub(crate) fn backfill_dispatch_sequences(s: &mut State) -> bool {
    if !s.indexes.initialized || s.indexes.dispatch_sequences_complete {
        return false;
    }
    let candidates: Vec<LegacyDispatch> = s
        .intents
        .values()
        .filter(|i| {
            i.command.asynchronous.is_some() && i.handle.is_none() && i.dispatch_sequence.is_none()
        })
        .map(|i| LegacyDispatch {
            id: i.id.clone(),
            run: i.run.clone(),
        })
        .collect();
    for candidate in candidates {
        let first = candidate.first_sequence(s);
        if let Some(intent) = s.intents.get_mut(&candidate.id) {
            intent.dispatch_sequence = first;
        }
        mark_intent(s, &candidate.id);
    }
    s.indexes.dispatch_sequences_complete = true;
    true
}

struct LegacyDispatch {
    id: String,
    run: String,
}
impl LegacyDispatch {
    fn first_sequence(&self, s: &State) -> Option<u64> {
        s.indexes
            .receipts
            .get(&self.run)
            .into_iter()
            .flat_map(|offsets| offsets.values().copied())
            .filter_map(|offset| s.receipts.get(offset))
            .filter(|r| {
                r.run == self.run
                    && r.kind == "dispatch"
                    && r.detail.get("operation").and_then(|v| v.as_str()) == Some(self.id.as_str())
            })
            .map(|r| r.sequence)
            .min()
    }
}

fn remove_lazy_member(map: &mut LazyMap<String, BTreeSet<String>>, key: &str, member: &str) {
    if let Some(values) = map.get_mut(key) {
        values.remove(member);
        if values.is_empty() {
            map.remove(key);
        }
    }
}
fn remove_member(map: &mut BTreeMap<String, BTreeSet<String>>, key: &str, member: &str) {
    if let Some(values) = map.get_mut(key) {
        values.remove(member);
        if values.is_empty() {
            map.remove(key);
        }
    }
}

/// Runs, intents and receipts of `root`, in the order of a full-state scan
/// (runs and intents by id, receipts by position), using root-scoped indexes
/// plus this transaction's dirty sets. Returns `None` before initialization.
#[allow(clippy::type_complexity)]
pub(crate) fn root_history(
    s: &State,
    root: &str,
) -> Option<(Vec<usize>, Vec<String>, Vec<String>)> {
    if !s.indexes.initialized {
        return None;
    }
    let in_root = |run: &str| s.runs.get(run).is_some_and(|r| r.root == root);
    let mut runs: BTreeSet<String> = s
        .indexes
        .root_members
        .get(root)
        .cloned()
        .unwrap_or_default();
    runs.extend(s.indexes.dirty_runs.iter().cloned());
    runs.retain(|id| in_root(id));
    let mut intents: BTreeSet<String> = s
        .indexes
        .root_intents
        .get(root)
        .cloned()
        .unwrap_or_default();
    intents.extend(s.indexes.dirty_intents.iter().cloned());
    intents.retain(|id| s.intents.get(id).is_some_and(|i| in_root(&i.run)));
    let mut receipts = BTreeSet::new();
    for run in &runs {
        if let Some(offsets) = s.indexes.receipts.get(run) {
            receipts.extend(
                offsets
                    .values()
                    .copied()
                    .filter(|i| s.receipts.get(*i).is_some_and(|r| &r.run == run)),
            );
        }
    }
    Some((
        receipts.into_iter().collect(),
        intents.into_iter().collect(),
        runs.into_iter().collect(),
    ))
}
/// Non-terminal intents of `root` (indexed plus dirty), filtered by current state.
pub(crate) fn root_open_intents(s: &State, root: &str) -> Option<Vec<String>> {
    if !s.indexes.initialized {
        return None;
    }
    let mut ids: BTreeSet<String> = s
        .indexes
        .root_effects
        .get(root)
        .cloned()
        .unwrap_or_default();
    ids.extend(s.indexes.dirty_intents.iter().cloned());
    Some(
        ids.into_iter()
            .filter(|id| {
                s.intents.get(id).is_some_and(|i| {
                    s.runs.get(&i.run).is_some_and(|x| x.root == root)
                        && !matches!(
                            i.status,
                            IntentStatus::Completed
                                | IntentStatus::Rejected
                                | IntentStatus::Cancelled
                        )
                })
            })
            .collect(),
    )
}
