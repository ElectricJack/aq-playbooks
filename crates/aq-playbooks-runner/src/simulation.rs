//! Deterministic host responses and reports for an in-memory engine.
use super::*;

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
        next_response(&mut self.responses, &i.command.name).unwrap_or_else(|| {
            DispatchResult::Unknown {
                reason: format!("missing mock for {}", i.command.name),
            }
        })
    }
    fn reconcile(&mut self, i: &Intent) -> DispatchResult {
        self.reconciliations.push(i.id.clone());
        next_response(
            &mut self.responses,
            &format!("reconcile:{}", i.command.name),
        )
        .unwrap_or_else(|| DispatchResult::Unknown {
            reason: "missing reconciliation mock".into(),
        })
    }
    fn cancel(&mut self, i: &Intent) -> CancellationResult {
        self.cancelled.push(i.id.clone());
        next_response(&mut self.cancellation_responses, &i.command.name)
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

fn next_response<T>(responses: &mut BTreeMap<String, Vec<T>>, command: &str) -> Option<T> {
    responses
        .get_mut(command)
        .filter(|queue| !queue.is_empty())
        .map(|queue| queue.remove(0))
}
