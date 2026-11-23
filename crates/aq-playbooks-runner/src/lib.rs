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
mod dependencies;
use dependencies::{ensure_admission_protection, resolve, resolve_at};
mod starts;
use starts::{Invocation, Lineage};
mod driver;
mod effects;
mod events;
use events::{EventIngress, fanout_event, find_event};
mod execution;
pub mod indexes;
use execution::advance;
mod pending;
pub mod retention;
use pending::poll_pending;
mod results;
use results::record_result;
mod schedules;
use aq_playbooks_model::*;
pub use indexes::{EffectQueueKind, IndexItem, IndexPage, IndexQuery, RuntimeIndexes};
pub use lazy::{LazyMap, LazySeq, LazySet};
pub use retention::{GcReport, RetentionPolicy, RetentionState};
pub use schedules::{BindingSchedules, DeactivatedBinding};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
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
    #[error("{}", stale_message(*.requested_at_ms, *.cutoff_ms, *.clock_ms, *.now_ms, *.window_ms, *.floor_ms))]
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
    if actor.is_empty() || reason.is_empty() || actor.len() > 512 || reason.len() > 4096 {
        return Err(Error::Refused(
            "admin operation requires bounded actor and reason".into(),
        ));
    }
    Ok(())
}
fn stale_message(
    requested_at_ms: i64,
    cutoff_ms: i64,
    clock_ms: Option<i64>,
    now_ms: i64,
    window_ms: u64,
    floor_ms: Option<i64>,
) -> String {
    let start = clock_ms.map_or(now_ms, |clock| clock.max(now_ms));
    let mut message = format!(
        "{STALE_REQUEST}: request time {requested_at_ms} ms is at or before the cut-off \
         {cutoff_ms} ms (retention time {start} ms minus the {window_ms} ms window; host \
         time {now_ms} ms"
    );
    if let Some(floor) = floor_ms {
        message.push_str(&format!("; stale floor {floor} ms from a clock rebase"));
    }
    message.push(')');
    if start > now_ms {
        message.push_str(&format!(
            "; the retention clock ({start} ms) is ahead of host time by {} ms, and while \
             it is, every request that carries a real request time is refused as stale \
             (a clock pinned by a far-future host time stays ahead until it is rebased)",
            start.saturating_sub(now_ms)
        ));
    }
    message
}
/// Refuse a request for a key the engine does not remember (absent, or
/// expired and held by nothing) when its request time is at or before the
/// cut-off: the window before `max(now, clock)`, the time expiry is judged
/// against, or the stale floor if that is later. A request without a
/// request time is not checked, and the window is not computed. The request
/// time is only compared: it never moves the clock and is never stored.
fn refuse_stale(
    s: &State,
    requested_at_ms: Option<i64>,
    window_ms: impl FnOnce() -> u64,
    now_ms: i64,
) -> Result<()> {
    let Some(requested_at_ms) = requested_at_ms else {
        return Ok(());
    };
    let window_ms = window_ms();
    let start = retention::window_start(s, now_ms);
    let floor_ms = s.retention.stale_floor_ms;
    let cutoff_ms = start
        .saturating_sub(i64::try_from(window_ms).unwrap_or(i64::MAX))
        .max(floor_ms.unwrap_or(i64::MIN));
    if requested_at_ms > cutoff_ms {
        return Ok(());
    }
    Err(Error::Stale {
        requested_at_ms,
        cutoff_ms,
        clock_ms: s.retention.clock_ms,
        now_ms,
        window_ms,
        floor_ms,
    })
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authority {
    pub actor: String,
    pub scope: String,
    pub capabilities: BTreeSet<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activation {
    pub artifact: String,
    pub authority: Authority,
    pub revision: u64,
    #[serde(default)]
    pub controls: ActivationControls,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationControls {
    pub calls_enabled: bool,
    pub event_starts_enabled: bool,
    pub execution_enabled: bool,
}
impl Default for ActivationControls {
    fn default() -> Self {
        Self {
            calls_enabled: true,
            event_starts_enabled: true,
            execution_enabled: true,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Admitted,
    Running,
    Waiting,
    Finishing,
    Cancelling,
    Blocked,
    Completed,
    Failed,
    Cancelled,
}
impl RunStatus {
    pub fn terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    #[serde(default)]
    pub catalog_revision: u64,
    pub id: String,
    pub root: String,
    pub parent: Option<String>,
    pub origin: Option<String>,
    pub artifact: String,
    pub entry: String,
    pub dependencies: BTreeMap<String, String>,
    pub authority: Authority,
    pub step: String,
    pub status: RunStatus,
    pub input: Value,
    pub variables: BTreeMap<String, Value>,
    pub outcome: Option<String>,
    pub output: Option<Value>,
    pub reason: Option<String>,
    pub admitted_at_ms: i64,
    pub visits: u64,
    pub depth: u32,
    pub children: Vec<String>,
    pub pending: Option<Pending>,
    pub loops: Vec<LoopFrame>,
    pub attempts: BTreeMap<String, u32>,
    pub pending_return: Option<(String, Value)>,
    #[serde(default)]
    pub pending_failure: bool,
    pub terminal_sequence: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopFrame {
    pub site: String,
    pub item: String,
    pub items: Vec<Value>,
    pub index: usize,
    pub body: String,
    pub next: String,
    pub failure: IterationFailure,
    pub previous: Option<Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pending {
    Effect {
        intent: String,
    },
    Event {
        wait: String,
    },
    Child {
        child: String,
        bind: String,
    },
    Join {
        members: Vec<String>,
        #[serde(default)]
        member_keys: Vec<String>,
        mode: JoinMode,
        accepted: BTreeSet<String>,
        deadline: i64,
        remaining: RemainingPolicy,
        bind: String,
    },
    Retry {
        deadline: i64,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Budget {
    pub visits: u64,
    pub children: u64,
    pub iterations: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub sequence: u64,
    pub run: String,
    pub step: String,
    pub at_ms: i64,
    pub kind: String,
    pub detail: Value,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentStatus {
    Prepared,
    Dispatched,
    Accepted,
    Completed,
    Unknown,
    Rejected,
    Cancelled,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    pub id: String,
    pub run: String,
    pub step: String,
    pub invocation: u64,
    #[serde(default)]
    pub deadline_at_ms: i64,
    pub command: CommandContract,
    pub input: Value,
    pub input_digest: String,
    pub authority: Authority,
    pub status: IntentStatus,
    #[serde(default)]
    pub cancel_requested: bool,
    #[serde(default)]
    pub cancel_dispatched: bool,
    pub handle: Option<String>,
    pub result: Option<(String, Value)>,
    pub reason: Option<String>,
    #[serde(default)]
    pub late_results: Vec<(String, Value)>,
    #[serde(default)]
    pub completion_event: Option<String>,
    /// The namespace sequence of the commit that first dispatched the
    /// operation (its first `dispatch` receipt; a re-dispatch after recovery
    /// keeps it). Its completion can only be an event accepted later, so an
    /// operation whose admission handle is lost holds only those
    /// (`retention::event_held`). Absent until dispatch: an operation never
    /// dispatched holds nothing. Operations dispatched before Task 8b get it
    /// on open from their own first `dispatch` receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_sequence: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitRecord {
    pub id: String,
    pub run: String,
    pub step: String,
    pub event: Option<ContractRef>,
    pub scope: String,
    pub correlation: BTreeMap<String, String>,
    pub minimum_revision: Option<u64>,
    pub deadline: i64,
    pub bind: String,
    pub winner: Option<String>,
    pub expired: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Destination {
    pub key: String,
    pub artifact: String,
    pub entry: String,
    pub input: Value,
    pub authority: Authority,
    pub dependencies: BTreeMap<String, String>,
    pub run: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedEvent {
    pub key: String,
    pub envelope: EventEnvelope,
    pub digest: String,
    pub accepted_at_ms: i64,
    pub sequence: u64,
    pub catalog_revision: u64,
    pub destinations: Vec<Destination>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventDisposition {
    pub key: String,
    pub duplicate: bool,
    pub destinations: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    #[serde(default)]
    pub keyed_admissions: LazyMap<String, KeyedAdmissionReceipt>,
    #[serde(default)]
    pub catalog_history: BTreeMap<u64, BTreeMap<String, Activation>>,
    #[serde(default)]
    pub catalog_history_at_ms: BTreeMap<u64, i64>,
    #[serde(default)]
    pub retention: RetentionState,
    #[serde(default)]
    pub execution_denied: BTreeSet<String>,
    #[serde(default)]
    pub authority_revocations: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    pub recovery_requests: BTreeMap<String, RecoveryRequest>,
    #[serde(skip)]
    pub protection_available: bool,
    /// Cohorts this commit removes from the State, handed to storage as
    /// history in the same commit. Never stored with the State.
    #[serde(skip)]
    pub settled: Vec<SettledCohort>,
    /// What this commit may have changed the hold of, for the eviction
    /// order (`retention::rejoin`). Never stored.
    #[serde(skip)]
    pub replay_touched: retention::ReplayTouched,
    #[serde(default)]
    pub indexes: RuntimeIndexes,
    pub artifacts: BTreeMap<String, Artifact>,
    pub bindings: BTreeMap<String, Activation>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub binding_schedules: BTreeMap<String, BindingSchedules>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deactivated_bindings: BTreeMap<String, DeactivatedBinding>,
    pub catalog_revision: u64,
    pub runs: LazyMap<String, Run>,
    pub receipts: LazySeq<Receipt>,
    pub intents: LazyMap<String, Intent>,
    pub waits: LazyMap<String, WaitRecord>,
    pub events: LazyMap<String, AcceptedEvent>,
    pub budgets: LazyMap<String, Budget>,
    pub sequence: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryRequest {
    pub id: String,
    pub intent: String,
    pub actor: String,
    pub reason: String,
    pub evidence_refs: Vec<String>,
    pub requested_at_ms: i64,
    pub consumed: bool,
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
            s.artifacts.insert(artifact.digest.clone(), artifact);
            Ok(())
        })
    }
    pub fn activate(&self, binding: &str, artifact: &str, authority: Authority) -> Result<()> {
        let state = self.store.load(&self.namespace)?.state;
        let derived = state
            .receipts
            .last()
            .map(|receipt| receipt.at_ms)
            .unwrap_or(0);
        // Finished work leaves with its receipts; the clock still dates it.
        let now_ms = state
            .retention
            .clock_ms
            .map_or(derived, |clock| clock.max(derived));
        // A derived time, not host time: the clock does not move.
        self.activate_with(binding, artifact, authority, now_ms, None)
    }
    pub fn activate_at(
        &self,
        binding: &str,
        artifact: &str,
        authority: Authority,
        now_ms: i64,
    ) -> Result<()> {
        self.activate_with(binding, artifact, authority, now_ms, Some(now_ms))
    }
    fn activate_with(
        &self,
        binding: &str,
        artifact: &str,
        authority: Authority,
        now_ms: i64,
        host_ms: Option<i64>,
    ) -> Result<()> {
        self.transaction_at(host_ms, |s| {
            activate_binding(s, binding, artifact, authority, now_ms)?;
            if let Some(configuration) = s
                .deactivated_bindings
                .remove(binding)
                .and_then(|d| d.configuration)
            {
                configuration.install(
                    s,
                    schedules::BindingSite {
                        namespace: &self.namespace,
                        binding,
                        now_ms,
                    },
                    None,
                )?;
            }
            Ok(())
        })
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
            let id = next_id(s, "run");
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
fn check_bytes(value: &Value, limit: usize) -> Result<()> {
    if canonical_bytes(value)?.len() > limit {
        return Err(Error::Refused("payload byte limit".into()));
    }
    Ok(())
}
fn check_authority(artifact: &Artifact, authority: &Authority) -> Result<()> {
    if artifact.content.playbook.scope != authority.scope
        || !artifact
            .content
            .playbook
            .required_capabilities
            .is_subset(&authority.capabilities)
    {
        return Err(Error::Refused("scope or capability ceiling".into()));
    }
    Ok(())
}
fn next_id(s: &mut State, prefix: &str) -> String {
    s.sequence += 1;
    format!("{prefix}-{:016x}", s.sequence)
}
fn receipt(s: &mut State, run: &str, step: &str, now: i64, kind: &str, detail: Value) {
    s.sequence += 1;
    let offset = s.receipts.len();
    s.indexes
        .receipts
        .entry(run.into())
        .or_default()
        .insert(s.sequence, offset);
    s.indexes.all_receipts.insert(s.sequence, offset);
    s.receipts.push(Receipt {
        sequence: s.sequence,
        run: run.into(),
        step: step.into(),
        at_ms: now,
        kind: kind.into(),
        detail,
    });
}
fn context(s: &State, r: &Run) -> EvalContext {
    let mut c = EvalContext::new();
    c.insert("input".into(), EvalValue::Known(r.input.clone()));
    c.insert("vars".into(), EvalValue::Known(json!(r.variables)));
    for (k, v) in &r.variables {
        c.insert(k.clone(), EvalValue::Known(v.clone()));
    }
    let definitions = &s.artifacts[&r.artifact].content.definitions;
    c.insert(
        "definitions".into(),
        EvalValue::Known(Value::Object(
            definitions
                .iter()
                .map(|(k, v)| (k.clone(), v.value.clone()))
                .collect(),
        )),
    );
    c
}
fn known_expr(expr: &Expr, ctx: &EvalContext) -> Result<Value> {
    match eval(expr, ctx, &EvalLimits::default())? {
        EvalValue::Known(v) => Ok(v),
        EvalValue::Missing => Err(Error::Refused("expression input missing".into())),
        EvalValue::Unknown(r) => Err(Error::Refused(format!("expression unknown: {r}"))),
    }
}
fn activate_binding(
    s: &mut State,
    binding: &str,
    artifact: &str,
    authority: Authority,
    now_ms: i64,
) -> Result<()> {
    let a = s
        .artifacts
        .get(artifact)
        .ok_or_else(|| Error::NotFound(artifact.into()))?;
    check_authority(a, &authority)?;
    if let Some(schedules) = s.binding_schedules.get(binding) {
        schedules::check_event(a, &authority.scope, &schedules.configuration.event)?;
    }
    s.catalog_revision += 1;
    let controls = s
        .bindings
        .get(binding)
        .map(|activation| activation.controls.clone())
        .unwrap_or_default();
    if !controls.execution_enabled {
        s.execution_denied.insert(artifact.into());
    }
    s.bindings.insert(
        binding.into(),
        Activation {
            artifact: artifact.into(),
            authority,
            revision: s.catalog_revision,
            controls,
        },
    );
    record_binding_change(s, binding, now_ms)
}

fn record_binding_change(s: &mut State, binding: &str, now_ms: i64) -> Result<()> {
    s.catalog_history
        .insert(s.catalog_revision, s.bindings.clone());
    s.catalog_history_at_ms.insert(s.catalog_revision, now_ms);
    indexes::index_activation(s, binding)
}

fn deadline(now: i64, duration: u64) -> Result<i64> {
    let duration =
        i64::try_from(duration).map_err(|_| Error::Refused("deadline overflow".into()))?;
    now.checked_add(duration)
        .ok_or_else(|| Error::Refused("deadline overflow".into()))
}
fn reserve_iteration(s: &mut State, r: &Run, limits: &Limits) -> Result<()> {
    let root = s
        .runs
        .get(&r.root)
        .ok_or_else(|| invariant(format!("root {} of run {} is missing", r.root, r.id)))?;
    let bound = s.artifacts[&root.artifact]
        .content
        .playbook
        .limits
        .max_iterations
        .min(limits.max_iterations);
    let b = s
        .budgets
        .get_mut(&r.root)
        .ok_or_else(|| invariant(format!("budget of root {} is missing", r.root)))?;
    if b.iterations >= bound {
        return Err(Error::Exhausted("iterations".into()));
    }
    b.iterations += 1;
    Ok(())
}
fn follow(
    s: &mut State,
    r: &mut Run,
    edges: &BTreeMap<String, String>,
    outcome: &str,
    now: i64,
) -> Result<()> {
    let target = edges
        .get(outcome)
        .ok_or_else(|| Error::Refused(format!("unhandled outcome {outcome}")))?;
    receipt(
        s,
        &r.id,
        &r.step,
        now,
        "branch",
        json!({"outcome":outcome,"next":target}),
    );
    r.step = target.clone();
    r.pending = None;
    r.status = RunStatus::Running;
    Ok(())
}
fn finish(
    s: &mut State,
    r: &mut Run,
    status: RunStatus,
    outcome: Option<String>,
    output: Option<Value>,
    now: i64,
) {
    r.status = status;
    r.outcome = outcome;
    r.output = output;
    r.pending = None;
    receipt(
        s,
        &r.id,
        &r.step,
        now,
        "terminal",
        json!({"status":r.status,"outcome":r.outcome}),
    );
    r.terminal_sequence = Some(s.sequence);
    indexes::wake_dependents(s, &r.id);
}
fn settled(s: &State, r: &Run) -> Result<bool> {
    // A cohort leaves only as a whole: a live run's children are live.
    for id in &r.children {
        let child = s
            .runs
            .get(id)
            .ok_or_else(|| invariant(format!("child {id} of run {} is missing", r.id)))?;
        if !child.status.terminal() {
            return Ok(false);
        }
    }
    Ok(match indexes::root_open_intents(s, &r.root) {
        Some(open) => !open.iter().any(|id| s.intents[id].run == r.id),
        None => s.intents.values().filter(|i| i.run == r.id).all(|i| {
            matches!(
                i.status,
                IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
            )
        }),
    })
}
fn request_cancel(s: &mut State, id: &str, now: i64) -> Result<()> {
    let mut r = s
        .runs
        .get(id)
        .ok_or_else(|| Error::NotFound(id.into()))?
        .clone();
    if r.status.terminal() {
        return Ok(());
    }
    r.status = RunStatus::Cancelling;
    receipt(s, id, &r.step, now, "cancel_requested", Value::Null);
    for child in r.children.clone() {
        request_cancel(s, &child, now)?;
    }
    for i in s.intents.values_mut().filter(|i| i.run == id) {
        if i.status == IntentStatus::Prepared {
            i.status = IntentStatus::Cancelled;
        } else if !matches!(
            i.status,
            IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
        ) {
            i.cancel_requested = true;
        }
    }
    s.runs.insert(id.into(), r);
    indexes::mark_run(s, id);
    let intents = s
        .intents
        .values()
        .filter(|i| i.run == id)
        .map(|i| i.id.clone())
        .collect::<Vec<_>>();
    for intent in intents {
        indexes::mark_intent(s, &intent);
    }
    Ok(())
}
fn settle_cancel(s: &mut State, r: &mut Run, now: i64) -> Result<()> {
    if settled(s, r)? {
        finish(s, r, RunStatus::Cancelled, None, None, now);
    } else if s.intents.values().any(|i| {
        i.run == r.id
            && matches!(
                i.status,
                IntentStatus::Dispatched | IntentStatus::Unknown | IntentStatus::Accepted
            )
    }) {
        r.reason = Some("cancellation awaits external operation settlement".into());
    }
    Ok(())
}

/// Fake dispatch data is keyed by exact command name. Responses are consumed in
/// order and recorded by the same Engine used by embedded production hosts.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FakeHost {
    pub responses: BTreeMap<String, Vec<DispatchResult>>,
    pub dispatched: Vec<String>,
    pub reconciliations: Vec<String>,
    pub revoked: BTreeSet<String>,
    pub cancellation_responses: BTreeMap<String, Vec<CancellationResult>>,
    pub cancelled: Vec<String>,
}
impl Host for FakeHost {
    fn authorize(&self, a: &Authority, c: &CommandContract) -> bool {
        c.capabilities.is_subset(&a.capabilities)
            && c.scopes.contains(&a.scope)
            && c.capabilities.is_disjoint(&self.revoked)
    }
    fn dispatch(&mut self, i: &Intent) -> DispatchResult {
        self.dispatched.push(i.id.clone());
        self.responses
            .get_mut(&i.command.name)
            .filter(|v| !v.is_empty())
            .map(|v| v.remove(0))
            .unwrap_or_else(|| DispatchResult::Unknown {
                reason: format!("missing mock for {}", i.command.name),
            })
    }
    fn reconcile(&mut self, i: &Intent) -> DispatchResult {
        self.reconciliations.push(i.id.clone());
        self.responses
            .get_mut(&format!("reconcile:{}", i.command.name))
            .filter(|v| !v.is_empty())
            .map(|v| v.remove(0))
            .unwrap_or_else(|| DispatchResult::Unknown {
                reason: "missing reconciliation mock".into(),
            })
    }
    fn cancel(&mut self, i: &Intent) -> CancellationResult {
        self.cancelled.push(i.id.clone());
        self.cancellation_responses
            .get_mut(&i.command.name)
            .filter(|v| !v.is_empty())
            .map(|v| v.remove(0))
            .unwrap_or(CancellationResult::Unknown)
    }
}
pub struct Simulation {
    pub engine: Engine<MemoryStorage>,
    pub host: FakeHost,
    pub now_ms: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationReport {
    pub state: State,
    pub transitions: usize,
    pub incomplete: bool,
    pub unknown_operations: Vec<String>,
}
impl Simulation {
    pub fn new(host: FakeHost) -> Result<Self> {
        Ok(Self {
            engine: Engine::new(MemoryStorage::default(), "simulation", "fake-runtime")?,
            host,
            now_ms: 0,
        })
    }
    pub fn advance_clock(&mut self, now_ms: i64) -> Result<()> {
        if now_ms < self.now_ms {
            return Err(Error::Refused("virtual clock cannot move backwards".into()));
        }
        self.now_ms = now_ms;
        Ok(())
    }
    pub fn run(&mut self, max_transitions: usize) -> Result<SimulationReport> {
        let transitions = self
            .engine
            .drive(&mut self.host, self.now_ms, max_transitions)?;
        let state = self.engine.snapshot()?;
        let unknown_operations = state
            .intents
            .values()
            .filter(|i| i.status == IntentStatus::Unknown)
            .map(|i| i.id.clone())
            .collect();
        let incomplete = state.runs.values().any(|r| !r.status.terminal());
        Ok(SimulationReport {
            state,
            transitions,
            incomplete,
            unknown_operations,
        })
    }
}
struct Participant {
    id: String,
    sequence: u64,
    accepted_at_ms: i64,
    success: bool,
    outcome: Option<String>,
    output: Option<Value>,
}
fn participant(s: &State, id: &str) -> Option<Participant> {
    if let Some(c) = s.runs.get(id)
        && c.status.terminal()
    {
        return Some(Participant {
            id: id.into(),
            sequence: c.terminal_sequence.unwrap_or(u64::MAX),
            accepted_at_ms: s
                .receipts
                .iter()
                .find(|r| Some(r.sequence) == c.terminal_sequence)
                .map(|r| r.at_ms)
                .unwrap_or(i64::MAX),
            success: c.status == RunStatus::Completed,
            outcome: c.outcome.clone(),
            output: c.output.clone(),
        });
    }
    if let Some(i) = s.intents.get(id)
        && matches!(
            i.status,
            IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
        )
    {
        let sequence = s
            .receipts
            .iter()
            .rev()
            .find(|r| {
                r.detail.get("operation").and_then(Value::as_str) == Some(id)
                    && matches!(
                        r.kind.as_str(),
                        "command_result" | "operation_rejected" | "cancel_response"
                    )
            })
            .map(|r| r.sequence)
            .unwrap_or(u64::MAX);
        return Some(Participant {
            id: id.into(),
            sequence,
            accepted_at_ms: s
                .receipts
                .iter()
                .find(|r| r.sequence == sequence)
                .map(|r| r.at_ms)
                .unwrap_or(i64::MAX),
            success: i.status == IntentStatus::Completed,
            outcome: i.result.as_ref().map(|r| r.0.clone()),
            output: i.result.as_ref().map(|r| r.1.clone()),
        });
    }
    None
}
fn fail_run(s: &mut State, r: &mut Run, reason: String, now: i64) -> Result<()> {
    r.pending = None;
    r.reason = Some(reason.clone());
    r.pending_failure = true;
    for child in r.children.clone() {
        request_cancel(s, &child, now)?;
    }
    for intent in s.intents.values_mut().filter(|i| i.run == r.id) {
        if intent.status == IntentStatus::Prepared {
            intent.status = IntentStatus::Cancelled;
        } else if !matches!(
            intent.status,
            IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
        ) {
            intent.cancel_requested = true;
        }
    }
    let intents = s
        .intents
        .values()
        .filter(|i| i.run == r.id)
        .map(|i| i.id.clone())
        .collect::<Vec<_>>();
    for intent in intents {
        indexes::mark_intent(s, &intent);
    }
    receipt(
        s,
        &r.id,
        &r.step,
        now,
        "engine_failure",
        json!({"reason":reason}),
    );
    if settled(s, r)? {
        finish(s, r, RunStatus::Failed, None, None, now);
    } else {
        r.status = RunStatus::Finishing;
    }
    Ok(())
}

fn process_operation_deadlines(s: &mut State, now: i64) {
    let timers = s
        .indexes
        .query(
            s,
            &IndexQuery::DueTimers {
                now_ms: now,
                limit: 64,
            },
        )
        .items;
    let expired = timers
        .iter()
        .filter_map(|item| s.indexes.timers.get(&item.key))
        .filter_map(|timer| timer.intent.as_ref())
        .filter_map(|id| s.intents.get(id))
        .filter(|intent| {
            matches!(
                intent.status,
                IntentStatus::Dispatched | IntentStatus::Accepted
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    for intent in expired {
        indexes::remove_timer(s, &format!("effect:{}", intent.id));
        indexes::mark_intent(s, &intent.id);
        let current = s.intents.get_mut(&intent.id).unwrap();
        current.status = IntentStatus::Unknown;
        current.reason = Some("operation deadline elapsed with unresolved external outcome".into());
        receipt(
            s,
            &intent.run,
            &intent.step,
            now,
            "command_deadline_expired",
            json!({"operation":intent.id,"issued":true}),
        );
    }
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
        self.transaction_at(Some(now_ms), |s| {
            let a = s
                .bindings
                .get_mut(binding)
                .ok_or_else(|| Error::NotFound(binding.into()))?;
            a.controls = controls.clone();
            a.revision += 1;
            let artifact = a.artifact.clone();
            if controls.execution_enabled {
                s.execution_denied.remove(&artifact);
            } else {
                s.execution_denied.insert(artifact.clone());
            }
            s.catalog_revision += 1;
            s.catalog_history
                .insert(s.catalog_revision, s.bindings.clone());
            s.catalog_history_at_ms.insert(s.catalog_revision, now_ms);
            indexes::index_activation(s, binding)?;
            let runs = s
                .runs
                .values()
                .filter(|r| r.artifact == artifact && !r.status.terminal())
                .map(|r| r.id.clone())
                .collect::<Vec<_>>();
            for id in runs {
                if controls.execution_enabled {
                    let r = s.runs.get_mut(&id).unwrap();
                    if r.status == RunStatus::Blocked
                        && r.reason.as_deref() == Some("execution authority revoked")
                    {
                        r.status = if r.pending.is_some() {
                            RunStatus::Waiting
                        } else {
                            RunStatus::Running
                        };
                        r.reason = None;
                    }
                }
                indexes::mark_run(s, &id);
            }
            receipt(
                s,
                "admin",
                binding,
                now_ms,
                "activation_controls",
                json!({"artifact":artifact,"controls":controls}),
            );
            Ok(())
        })
    }
    pub fn set_execution_authority(
        &self,
        actor: &str,
        scope: &str,
        revoked_capabilities: BTreeSet<String>,
        now_ms: i64,
    ) -> Result<()> {
        self.transaction_at(Some(now_ms), |s| {
            s.authority_revocations
                .insert(digest(&(actor, scope))?, revoked_capabilities.clone());
            let ids = s
                .runs
                .values()
                .filter(|r| {
                    r.authority.actor == actor && r.authority.scope == scope && !r.status.terminal()
                })
                .map(|r| r.id.clone())
                .collect::<Vec<_>>();
            for id in ids {
                let r = s.runs.get_mut(&id).unwrap();
                if r.status == RunStatus::Blocked
                    && r.reason.as_deref() == Some("execution authority revoked")
                {
                    r.status = if r.pending.is_some() {
                        RunStatus::Waiting
                    } else {
                        RunStatus::Running
                    };
                    r.reason = None;
                }
                indexes::mark_run(s, &id);
            }
            let intents = s
                .intents
                .values()
                .filter(|i| i.authority.actor == actor && i.authority.scope == scope)
                .map(|i| i.id.clone())
                .collect::<Vec<_>>();
            for id in intents {
                indexes::mark_intent(s, &id);
            }
            receipt(
                s,
                "admin",
                "authority",
                now_ms,
                "authority_changed",
                json!({"actor":actor,"scope":scope,"revoked_capabilities":revoked_capabilities}),
            );
            Ok(())
        })
    }
    /// Store `policy`. Its windows must be positive. The deprecated
    /// `max_tombstones` and `terminal_run_ms` are stored as given and never
    /// read: there are no namespace limits, finished runs leave the State as
    /// history, and no tombstone is written for them.
    pub fn set_retention_policy(&self, policy: RetentionPolicy) -> Result<()> {
        if policy.event_replay_ms == 0 || policy.admission_replay_ms == 0 || policy.audit_ms == 0 {
            return Err(Error::Refused("retention windows must be positive".into()));
        }
        if policy.replay_keep == 0 {
            return Err(Error::Refused("replay_keep must be positive".into()));
        }
        if policy.drain_cohorts_per_commit == 0 || policy.drain_entries_per_commit == 0 {
            return Err(Error::Refused("drain bounds must be positive".into()));
        }
        if policy.window_drops_per_commit == 0
            || policy.replay_evictions_per_commit == 0
            || policy.history_trim_per_commit == 0
            || policy.gc_page_limit == 0
            || policy.settle_cohorts_per_commit == 0
        {
            return Err(Error::Refused(
                "per-commit retention bounds must be positive".into(),
            ));
        }
        self.transaction_at(None, |s| {
            s.retention.policy = policy;
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
            receipt(
                s,
                "admin",
                "retention",
                now_ms,
                retention::CLOCK_REBASE,
                json!({"from_ms":report.from_ms,"to_ms":report.to_ms,"stale_floor_ms":report.stale_floor_ms,"capped_events":report.capped_events,"capped_admissions":report.capped_admissions,"capped_deadlines":report.capped_deadlines,"adjusted_runs":report.adjusted_runs,"needs_operator_action":report.needs_operator_action,"adjustments":report.adjustments,"adjustments_not_listed":report.adjustments_not_listed,"actor":actor,"reason":reason}),
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
            receipt(
                s,
                "admin",
                "retention",
                now_ms,
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
        self.transaction_at(Some(now_ms), |s|{let old=s.retention.producer_epochs.get(producer).copied().unwrap_or(0);if new_epoch<=old{return Err(Error::Refused("producer epoch must increase".into()))}s.retention.producer_epochs.insert(producer.into(),new_epoch);s.retention.event_tombstones.retain(|_,t|t.producer!=producer||t.epoch>=new_epoch);if s.indexes.root_active.values().all(BTreeSet::is_empty){s.retention.expired_correlations.retain(|_,(p,epoch)|p!=producer||*epoch>=new_epoch);}receipt(s,"admin","replay",now_ms,"producer_epoch_retired",json!({"producer":producer,"old_epoch":old,"new_epoch":new_epoch,"actor":actor,"reason":reason}));Ok(())})
    }
    pub fn request_recovery(
        &self,
        intent_id: &str,
        actor: &str,
        reason: &str,
        evidence_refs: Vec<String>,
        now_ms: i64,
    ) -> Result<String> {
        if actor.is_empty()
            || reason.is_empty()
            || reason.len() > 4096
            || evidence_refs.is_empty()
            || evidence_refs.len() > 64
            || evidence_refs.iter().any(|e| e.is_empty() || e.len() > 1024)
        {
            return Err(Error::Refused(
                "recovery requires bounded actor, reason and evidence references".into(),
            ));
        }
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
                receipt(
                    s,
                    &i.run,
                    &i.step,
                    now_ms,
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
fn execution_allowed(
    s: &State,
    artifact: &str,
    authority: &Authority,
    capabilities: &BTreeSet<String>,
) -> Result<bool> {
    Ok(!s.execution_denied.contains(artifact)
        && s.authority_revocations
            .get(&digest(&(&authority.actor, &authority.scope))?)
            .is_none_or(|revoked| revoked.is_disjoint(capabilities)))
}
