//! Deterministic durable workflow transitions with explicit effect handoffs.
//! All methods take trusted host/storage time; producer timestamps never win races.
pub mod admission;
pub use admission::{KeyedAdmissionDisposition, KeyedAdmissionReceipt, KeyedAdmissionRequest};
pub mod catalog_inspection;
pub mod history;
pub mod inspection;
pub use catalog_inspection::{CatalogError, CatalogKind, CatalogPage, CatalogQuery, CatalogStamp};
pub use history::{HistoryItem, SettledCohort, history_items};
pub mod lazy;
pub mod metrics;
pub mod storage;
pub mod transition;
pub use inspection::{InspectionKind, InspectionPage, InspectionQuery};
mod commits;
mod completions;
use completions::process_completions;
mod clock_rebase;
use clock_rebase::rebase_plan;
pub use clock_rebase::{ClockRebaseReport, DeadlineAdjustment, REBASE_ADJUSTMENTS_LISTED};
mod activation;
mod controls;
use activation::{BindingUpdate, record_binding_change};
use controls::{check_authority, execution_allowed};
mod dependencies;
use dependencies::{ensure_admission_protection, resolve, resolve_at};
mod state;
pub use state::{
    AcceptedEvent, Activation, ActivationControls, Authority, Budget, Destination,
    EventDisposition, Intent, IntentStatus, LoopFrame, Pending, Receipt, RecoveryRequest, Run,
    RunStatus, State, WaitRecord,
};
mod starts;
use starts::{Invocation, Lineage};
mod driver;
mod effects;
use effects::process_operation_deadlines;
mod events;
use events::{EventIngress, fanout_event, find_event};
mod execution;
mod expressions;
use expressions::{check_bytes, context, known_expr};
pub mod indexes;
use execution::advance;
mod pending;
pub mod retention;
use pending::poll_pending;
mod request_time;
use request_time::{ReplayWindow, refuse_stale};
mod receipts;
use receipts::ReceiptSite;
mod run_transitions;
use run_transitions::{Terminal, fail_run, request_cancel, settle_cancel, settled};
mod results;
use results::record_result;
mod schedules;
mod simulation;
use aq_playbooks_model::*;
pub use indexes::{EffectQueueKind, IndexItem, IndexPage, IndexQuery, RuntimeIndexes};
pub use lazy::{LazyMap, LazySeq, LazySet};
pub use retention::{GcReport, RetentionPolicy, RetentionState};
pub use schedules::{BindingSchedules, DeactivatedBinding};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
pub use simulation::{FakeHost, Simulation, SimulationReport};
use std::collections::{BTreeMap, BTreeSet};
pub use storage::{MemoryStorage, Storage, StorageError, VersionedState};
pub use transition::{RecordStorage, TransitionOutcome, TransitionRequest};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("refused: {0}")]
    Refused(String),
    #[error("integrity conflict: {0}")]
    Conflict(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("execution budget exhausted: {0}")]
    Exhausted(String),
    /// A request for a key the engine no longer remembers whose request time
    /// is at or before `cutoff_ms`: the later of `max(now, clock) - window`
    /// and the stale floor a clock rebase recorded. Its text starts with
    /// [`STALE_REQUEST`]. Nothing was written.
    #[error("{}", ReplayWindow::new(*.clock_ms, *.now_ms, *.window_ms, *.floor_ms).stale_message(*.requested_at_ms, *.cutoff_ms))]
    Stale {
        requested_at_ms: i64,
        cutoff_ms: i64,
        /// The retention clock when the request was judged.
        clock_ms: Option<i64>,
        /// The host time of the request.
        now_ms: i64,
        window_ms: u64,
        /// The stale floor, when a clock rebase recorded one.
        floor_ms: Option<i64>,
    },
}
pub type Result<T> = std::result::Result<T, Error>;
const INVARIANT: &str = "engine invariant violated: ";
/// An engine fault (a record the State's own invariants guarantee is
/// missing). It fails the commit; `advance` never turns it into a playbook
/// error branch.
pub(crate) fn invariant(what: String) -> Error {
    Error::Conflict(format!("{INVARIANT}{what}"))
}
fn is_invariant(error: &Error) -> bool {
    matches!(error, Error::Conflict(m) if m.starts_with(INVARIANT))
}
/// How a commit ends: with a host time (`None` for commits without one), or
/// as a licensed admin commit.
#[derive(Clone, Copy)]
enum Finish {
    Host(Option<i64>),
    Licensed,
}
/// The start of the text of every [`Error::Stale`]: a request whose producer
/// (request) time is older than its window.
pub const STALE_REQUEST: &str = "stale request: producer time is older than the replay window";
/// An admin operation's `actor` and `reason`: present and bounded.
fn admin_text(actor: &str, reason: &str) -> Result<()> {
    if !(1..=512).contains(&actor.len()) || !(1..=4096).contains(&reason.len()) {
        return Err(Error::Refused(
            "admin operation requires bounded actor and reason".into(),
        ));
    }
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DispatchResult {
    Completed { outcome: String, output: Value },
    Accepted { handle: String },
    Unknown { reason: String },
    Rejected { reason: String },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CancellationResult {
    Confirmed,
    Accepted,
    Refused,
    TooLate,
    Unknown,
}
pub trait Host {
    fn authorize(&self, authority: &Authority, contract: &CommandContract) -> bool;
    fn dispatch(&mut self, intent: &Intent) -> DispatchResult;
    fn reconcile(&mut self, intent: &Intent) -> DispatchResult;
    /// A supported cancellation is a keyed idempotent request. The stable key is
    /// `cancellation_key(intent)`, independent of the original business effect.
    /// Lost responses are reconciled before delivery retry; acceptance is never
    /// interpreted as confirmed stopping.
    fn cancel(&mut self, _intent: &Intent) -> CancellationResult {
        CancellationResult::Unknown
    }
}
pub fn cancellation_key(intent: &Intent) -> Result<String> {
    Ok(digest(&(&intent.id, "cancel"))?)
}
/// A runner incarnation must be unique. Do not share its identifier between processes.
///
/// Deprecated: a namespace has no limits (spec 0003, "no limits"). These
/// fields are accepted, so existing hosts compile and configurations decode,
/// and ignored: nothing is ever refused because a namespace holds too much.
/// The record is bounded by deletion alone (finished work at completion,
/// `replay_keep` remembered keys, `history_keep` finished runs).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    pub max_runs: usize,
    pub max_events: usize,
    pub max_queued_destinations: usize,
    pub max_state_bytes: usize,
    pub max_receipts: usize,
}
impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_runs: 10_000,
            max_events: 100_000,
            max_queued_destinations: 10_000,
            max_state_bytes: 67_108_864,
            max_receipts: 1_000_000,
        }
    }
}
pub struct Engine<S: Storage> {
    pub store: S,
    pub namespace: String,
    pub owner: String,
    pub config: EngineConfig,
    pub metrics: metrics::Metrics,
}
impl<S: Storage> Engine<S> {
    pub fn new(store: S, namespace: impl Into<String>, owner: impl Into<String>) -> Result<Self> {
        Self::open(store, namespace, owner, EngineConfig::default())
    }
    /// Acquire the namespace and run the upgrade's bookkeeping commit (index
    /// rebuild, `root_intents` and dispatch sequence fills). Like every
    /// commit, it also takes the drain's first step on a namespace an older
    /// engine wrote (`retention::drain_step`).
    fn open(
        store: S,
        namespace: impl Into<String>,
        owner: impl Into<String>,
        config: EngineConfig,
    ) -> Result<Self> {
        // A backend (or wrapper) that would drop history fails closed.
        if !store.persists_history() {
            return Err(Error::Refused(
                "storage backend does not persist run history".into(),
            ));
        }
        let e = Self {
            store,
            namespace: namespace.into(),
            owner: owner.into(),
            config,
            metrics: metrics::Metrics::default(),
        };
        e.store.acquire(&e.namespace, &e.owner)?;
        e.transaction_at(None, |s| {
            if !s.indexes.initialized {
                indexes::rebuild(s)?;
            }
            // A State from before `root_intents` (pre-D10): cohort removal
            // needs every operation there, so fill it once.
            indexes::backfill_root_intents(s);
            // `contract_events` of a State written before it (Task 8b) is
            // filled by the drain, a bounded number of events per commit.
            // Before `Intent::dispatch_sequence` (Task 8b): fill it once.
            indexes::backfill_dispatch_sequences(s);
            Ok(())
        })?;
        Ok(e)
    }
    /// [`Self::new`] keeping `config`, whose (deprecated) limits are ignored.
    pub fn with_config(
        store: S,
        namespace: impl Into<String>,
        owner: impl Into<String>,
        config: EngineConfig,
    ) -> Result<Self> {
        Self::open(store, namespace, owner, config)
    }
    fn measure_host<T>(&self, f: impl FnOnce() -> T) -> T {
        let started = std::time::Instant::now();
        let result = f();
        self.metrics
            .record_time(metrics::Timing::Host, started.elapsed());
        result
    }
    pub fn release(&self) -> Result<()> {
        Ok(self.store.release(&self.namespace, &self.owner)?)
    }
    pub fn snapshot(&self) -> Result<State> {
        Ok(self.store.load(&self.namespace)?.state)
    }
    /// Bounded current metadata through this Engine's actual owning storage
    /// context. Backends without the explicit contract return Unsupported.
    pub fn inspect_catalog(
        &self,
        query: &CatalogQuery,
    ) -> std::result::Result<CatalogPage, CatalogError> {
        self.store
            .inspect_catalog(&self.namespace, &self.owner, query)
    }
    /// Public inspection over this namespace's live state and run history.
    pub fn inspect(&self, query: &InspectionQuery) -> Result<InspectionPage> {
        Ok(self.store.inspect(&self.namespace, query)?)
    }
    /// Delete the history of runs finished before `before_ms`; returns the
    /// number of history items deleted. Host-callable and optional: history
    /// is already a rolling log of `policy.history_keep` per set. Commits
    /// no State and moves no clock; on PostgreSQL it needs this engine's
    /// lease, which the engine holds.
    pub fn delete_history_before(&self, before_ms: i64) -> Result<u64> {
        Ok(self
            .store
            .delete_history(&self.namespace, &self.owner, before_ms)?)
    }
    /// One commit. `now_ms` is the host time the commit carries: when the plan
    /// changes the State, the retention clock moves to it (never back). A
    /// commit without host time leaves the clock where it is.
    fn transaction_at<T>(
        &self,
        now_ms: Option<i64>,
        f: impl FnOnce(&mut State) -> Result<T>,
    ) -> Result<T> {
        self.transaction_choice_at(now_ms, |state| f(state).map(|value| (value, true)))
    }
    fn transaction_choice_at<T>(
        &self,
        now_ms: Option<i64>,
        f: impl FnOnce(&mut State) -> Result<(T, bool)>,
    ) -> Result<T> {
        self.transaction_report_at(Finish::Host(now_ms), f)
            .map(|(value, _)| value)
    }
    /// One licensed admin commit (a clock rebase or a floor lowering): it
    /// carries no host time, settles and drops nothing, and moves only its
    /// own admin receipt to history, so it is exactly its licence.
    fn transaction_licensed<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        self.transaction_report_at(Finish::Licensed, |state| {
            f(state).map(|value| (value, true))
        })
        .map(|(value, _)| value)
    }
    pub fn install(&self, artifact: Artifact) -> Result<()> {
        artifact.verify()?;
        self.transaction_at(None, |s| {
            if let Some(old) = s.artifacts.get(&artifact.digest)
                && old != &artifact
            {
                return Err(Error::Conflict("artifact replacement".into()));
            }
            let digest = artifact.digest.clone();
            if s.artifacts.insert(digest.clone(), artifact).is_none() {
                // The newest of its source: an older one may leave the
                // rollback window.
                retention::record_install(s, &digest);
                let at = retention::disposal_time(s, None);
                retention::dispose_unreferenced(s, None, at);
            }
            Ok(())
        })
    }
    pub fn activate(&self, binding: &str, artifact: &str, authority: Authority) -> Result<()> {
        BindingUpdate {
            binding,
            artifact,
            authority,
        }
        .commit(self, None)
    }
    pub fn activate_at(
        &self,
        binding: &str,
        artifact: &str,
        authority: Authority,
        now_ms: i64,
    ) -> Result<()> {
        BindingUpdate {
            binding,
            artifact,
            authority,
        }
        .commit(self, Some(now_ms))
    }
    pub fn admit(
        &self,
        binding: &str,
        entry: &str,
        input: Value,
        authority: Authority,
        now_ms: i64,
    ) -> Result<String> {
        let result = self.transaction_at(Some(now_ms), |s| {
            let activation = s
                .bindings
                .get(binding)
                .ok_or_else(|| Error::NotFound(binding.into()))?
                .clone();
            if !activation.controls.calls_enabled {
                return Err(Error::Refused("new calls disabled".into()));
            }
            let dependencies = resolve(s, &activation.artifact, &authority)?;
            let id = s.next_id("run");
            Invocation {
                artifact: &activation.artifact,
                entry,
                input,
                authority,
                dependencies,
            }
            .admit_root(s, &id, now_ms)?;
            Ok(id)
        });
        self.metrics.admission(&result, false);
        result
    }
    pub fn advance(&self, run_id: &str, now_ms: i64) -> Result<bool> {
        let (progress, queued_ms) = self.transaction_at(Some(now_ms), |s| {
            let queued_ms = s
                .runs
                .get(run_id)
                .filter(|r| r.visits == 0)
                .map(|r| now_ms.saturating_sub(r.admitted_at_ms).max(0) as u64);
            Ok((advance(s, run_id, now_ms)?, queued_ms))
        })?;
        if progress && let Some(elapsed) = queued_ms {
            self.metrics.record_queue(elapsed);
        }
        Ok(progress)
    }
    pub fn cancel(&self, run_id: &str, now_ms: i64) -> Result<()> {
        self.transaction_at(Some(now_ms), |s| {
            request_cancel(s, run_id, now_ms)?;
            Ok(())
        })
    }
    pub fn admit_event(
        &self,
        authenticated_producer: &str,
        event: EventEnvelope,
        now_ms: i64,
    ) -> Result<EventDisposition> {
        self.admit_event_in_epoch(authenticated_producer, event, 0, now_ms)
    }
    pub fn admit_event_in_epoch(
        &self,
        authenticated_producer: &str,
        event: EventEnvelope,
        epoch: u64,
        now_ms: i64,
    ) -> Result<EventDisposition> {
        self.admit_event_at(authenticated_producer, event, epoch, None, now_ms)
    }
    /// [`Self::admit_event_in_epoch`] with the producer's request time (not
    /// the event's `occurred_at_ms`, and not part of any digest). An event
    /// the engine still remembers answers as before, whatever its request
    /// time. Otherwise a request time older than
    /// `max(now, clock) - max(policy.event_replay_ms, contract.retention_ms)`
    /// is refused with [`STALE_REQUEST`] and nothing is written. `None` is
    /// not checked: an expired event is then accepted as new.
    pub fn admit_event_at(
        &self,
        authenticated_producer: &str,
        event: EventEnvelope,
        epoch: u64,
        requested_at_ms: Option<i64>,
        now_ms: i64,
    ) -> Result<EventDisposition> {
        // An expired event that nothing holds is accepted as new; it leaves in
        // a commit of its own first (see `admit_once`).
        let ingress = EventIngress {
            producer: authenticated_producer,
            epoch,
            requested_at_ms,
            now_ms,
        };
        let mut result = Err(Error::Conflict("event changed during admission".into()));
        for _ in 0..2 {
            match self.transaction_at(Some(now_ms), |s| ingress.accept(s, event.clone(), None)) {
                Ok(None) => continue,
                Ok(Some(disposition)) => result = Ok(disposition),
                Err(error) => result = Err(error),
            }
            break;
        }
        self.metrics.admission(
            &result,
            result
                .as_ref()
                .is_ok_and(|disposition| disposition.duplicate),
        );
        result
    }
    pub fn fanout(&self, event_key: &str, limit: usize, now_ms: i64) -> Result<usize> {
        self.transaction_at(Some(now_ms), |s| fanout_event(s, event_key, limit, now_ms))
    }
    pub fn record_result(
        &self,
        intent_id: &str,
        result: DispatchResult,
        now_ms: i64,
    ) -> Result<()> {
        self.transaction_at(Some(now_ms), |s| {
            record_result(s, intent_id, result, now_ms)
        })
    }
}
fn deadline(now: i64, duration: u64) -> Result<i64> {
    let duration =
        i64::try_from(duration).map_err(|_| Error::Refused("deadline overflow".into()))?;
    now.checked_add(duration)
        .ok_or_else(|| Error::Refused("deadline overflow".into()))
}
fn attempt_key(run: &Run) -> Result<String> {
    Ok(digest(&(
        &run.step,
        run.loops
            .iter()
            .map(|frame| (&frame.site, frame.index))
            .collect::<Vec<_>>(),
    ))?)
}

impl<S: Storage> Engine<S> {
    pub fn set_activation_controls(
        &self,
        binding: &str,
        controls: ActivationControls,
        now_ms: i64,
    ) -> Result<()> {
        self.transaction_at(Some(now_ms), |s| controls.apply(s, binding, now_ms))
    }
    pub fn set_execution_authority(
        &self,
        actor: &str,
        scope: &str,
        revoked_capabilities: BTreeSet<String>,
        now_ms: i64,
    ) -> Result<()> {
        let subject = controls::Subject { actor, scope };
        self.transaction_at(Some(now_ms), |s| {
            subject.revoke(s, revoked_capabilities, now_ms)
        })
    }
    /// Store `policy`. Its windows must be positive. The deprecated
    /// `max_tombstones` and `terminal_run_ms` are stored as given and never
    /// read: there are no namespace limits, finished runs leave the State as
    /// history, and no tombstone is written for them.
    pub fn set_retention_policy(&self, policy: RetentionPolicy) -> Result<()> {
        policy.validate_bounds()?;
        self.transaction_at(None, |s| {
            s.retention.policy = policy;
            let at = retention::disposal_time(s, None);
            retention::dispose_unreferenced(s, None, at);
            Ok(())
        })
    }
    /// Recover from a retention clock that a far-future host time pinned
    /// (see `docs/implementation/0003-live-state-only.md`, "If the clock was
    /// pinned"). One licensed commit, refused unless the clock is ahead of
    /// `now_ms` by more than the smallest window (it is pinned), `now_ms <=
    /// to_ms <= now_ms + slack` (a minute, or a tenth of the smallest window
    /// if less) and `stale_floor_ms <= now_ms`:
    ///
    /// - the clock becomes `to_ms` (the corrected real time);
    /// - every recorded expiry later than `to_ms` plus its own window is
    ///   capped there, so what the pin wrote expires in real time again and
    ///   no entry is remembered for less than its window from `to_ms`;
    /// - every live run's timer beyond `to_ms` plus its declared duration is
    ///   capped there (a wait, a join, a retry, an operation deadline, the
    ///   run's duration basis); the runs are listed in the report;
    /// - the stale floor becomes the later of the old floor and
    ///   `stale_floor_ms`: a request for a key the engine does not remember
    ///   whose request time is at or before it is stale. Pass the last
    ///   correct host time before the pin;
    /// - an admin receipt (history) records all of it with `actor` and
    ///   `reason`. Every backend lowers the clock or an expiry, or changes
    ///   the floor, only in a commit that is exactly such a receipt's.
    pub fn rebase_retention_clock(
        &self,
        to_ms: i64,
        stale_floor_ms: i64,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<ClockRebaseReport> {
        admin_text(actor, reason)?;
        self.transaction_licensed(|s| {
            let report = rebase_plan(s, to_ms, stale_floor_ms, now_ms)?;
            ReceiptSite::admin("retention", now_ms).record(
                s,
                retention::CLOCK_REBASE,
                report.receipt_detail(actor, reason),
            );
            Ok(report)
        })
    }
    /// What [`Self::rebase_retention_clock`] would do, on a snapshot, with
    /// the same checks; nothing is committed.
    pub fn rebase_retention_clock_dry_run(
        &self,
        to_ms: i64,
        stale_floor_ms: i64,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<ClockRebaseReport> {
        admin_text(actor, reason)?;
        let mut s = self.snapshot()?;
        rebase_plan(&mut s, to_ms, stale_floor_ms, now_ms)
    }
    /// Undo a stale floor set too high by an earlier rebase: lower it to
    /// `to_ms`, accepting that keys the engine forgot after `to_ms` may be
    /// re-admitted as new by a retry with a request time in between. One
    /// licensed commit with its own admin receipt; nothing else changes.
    /// Returns the old and new floor.
    pub fn lower_stale_floor(
        &self,
        to_ms: i64,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<(i64, i64)> {
        admin_text(actor, reason)?;
        self.transaction_licensed(|s| {
            let Some(from_ms) = s.retention.stale_floor_ms else {
                return Err(Error::Refused("no stale floor to lower".into()));
            };
            if to_ms >= from_ms {
                return Err(Error::Refused(format!(
                    "stale floor can only be lowered: {to_ms} ms is not below {from_ms} ms"
                )));
            }
            s.retention.stale_floor_ms = Some(to_ms);
            ReceiptSite::admin("retention", now_ms).record(
                s,
                retention::STALE_FLOOR_LOWERED,
                json!({"from_ms":from_ms,"to_ms":to_ms,"actor":actor,"reason":reason}),
            );
            Ok((from_ms, to_ms))
        })
    }
    /// Not needed: every commit already does this work, a bounded amount
    /// each (see `docs/implementation/0003-live-state-only.md`). Kept so
    /// existing callers compile. One ordinary commit at host time `now_ms`
    /// (the bounded settle, drain of an older engine's namespace, window
    /// drop and eviction of every commit), plus a page of at most `limit`
    /// artifacts examined for disposal (`1..=policy.gc_page_limit`); refused
    /// if `now_ms` is before the previous call's. It removes no run without
    /// its history and writes no tombstone, correlation fence or disposal
    /// record. `more` says more bounded work is due.
    pub fn collect_garbage(&self, now_ms: i64, limit: usize) -> Result<GcReport> {
        let (mut gc, commit) = self.transaction_report_at(Finish::Host(Some(now_ms)), |s| {
            retention::gc(s, now_ms, limit).map(|gc| (gc, true))
        })?;
        gc.runs = commit.runs;
        gc.events = commit.events;
        gc.admissions = commit.admissions;
        gc.receipts = commit.receipts;
        gc.evicted = commit.evicted;
        gc.more = gc.more || commit.more;
        Ok(gc)
    }
    pub fn advance_producer_epoch(
        &self,
        producer: &str,
        new_epoch: u64,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<()> {
        if actor.is_empty() || reason.is_empty() {
            return Err(Error::Refused(
                "epoch retirement requires actor and reason".into(),
            ));
        }
        self.transaction_at(Some(now_ms), |s| {
            let old = s.retention.producer_epochs.get(producer).copied().unwrap_or(0);
            if new_epoch <= old {
                return Err(Error::Refused("producer epoch must increase".into()));
            }
            s.retention.producer_epochs.insert(producer.into(), new_epoch);
            s.retention.event_tombstones.retain(|_, t| t.producer != producer || t.epoch >= new_epoch);
            if s.indexes.root_active.values().all(BTreeSet::is_empty) {
                s.retention.expired_correlations.retain(|_, (p, epoch)| p != producer || *epoch >= new_epoch);
            }
            ReceiptSite::admin("replay",now_ms).record(s, "producer_epoch_retired", json!({
                "producer": producer, "old_epoch": old, "new_epoch": new_epoch, "actor": actor, "reason": reason,
            }));
            Ok(())
        })
    }
    pub fn request_recovery(
        &self,
        intent_id: &str,
        actor: &str,
        reason: &str,
        evidence_refs: Vec<String>,
        now_ms: i64,
    ) -> Result<String> {
        RecoveryRequest::validate_evidence(actor, reason, &evidence_refs)?;
        self.transaction_at(Some(now_ms), |s| {
            let i = s
                .intents
                .get(intent_id)
                .ok_or_else(|| Error::NotFound(intent_id.into()))?
                .clone();
            let id = digest(&(intent_id, actor, reason, &evidence_refs))?;
            if !s.recovery_requests.contains_key(&id) {
                s.recovery_requests.insert(
                    id.clone(),
                    RecoveryRequest {
                        id: id.clone(),
                        intent: intent_id.into(),
                        actor: actor.into(),
                        reason: reason.into(),
                        evidence_refs,
                        requested_at_ms: now_ms,
                        consumed: false,
                    },
                );
                i.receipt_at(now_ms).record(
                    s,
                    "recovery_requested",
                    json!({"request":id,"operation":intent_id,"actor":actor}),
                );
            }
            Ok(id)
        })
    }
    pub fn query(&self, query: &IndexQuery) -> Result<IndexPage> {
        Ok(self.store.query(&self.namespace, query)?)
    }
}
