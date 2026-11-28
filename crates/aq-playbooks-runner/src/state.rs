//! Persisted runner state and its execution records.
use super::*;

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
    /// Artifacts something this commit removed from the State referenced
    /// (an accepted event that left): the commit judges them for disposal
    /// ([`retention::dispose_unreferenced`]). Never stored.
    #[serde(skip)]
    pub released_artifacts: BTreeSet<String>,
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

impl State {
    pub(crate) fn next_id(&mut self, prefix: &str) -> String {
        self.sequence += 1;
        format!("{prefix}-{:016x}", self.sequence)
    }
}
