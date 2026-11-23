//! An in-memory backend that can interrupt one commit or synchronize competing readers.
use aq_playbooks_runner::*;
use std::sync::{Arc, Barrier, Mutex};
type Result<T> = std::result::Result<T, StorageError>;

#[derive(Clone, Copy, Default)]
pub enum CommitOutcome {
    #[default]
    Success,
    Conflict,
    LostResponse,
}
#[derive(Default)]
struct Interruptions {
    next_commit: CommitOutcome,
    racing_reads: Option<(usize, Arc<Barrier>)>,
}
#[derive(Clone, Default)]
pub struct InterruptedStorage {
    store: MemoryStorage,
    controls: Arc<Mutex<Interruptions>>,
}
impl InterruptedStorage {
    pub fn new(store: MemoryStorage) -> Self {
        Self {
            store,
            ..Default::default()
        }
    }
    pub fn next_commit(&self, outcome: CommitOutcome) {
        self.controls.lock().unwrap().next_commit = outcome;
    }
    pub fn synchronize_reads(&self, count: usize) {
        self.controls.lock().unwrap().racing_reads = Some((count, Arc::new(Barrier::new(count))));
    }
    fn read_barrier(&self) -> Option<Arc<Barrier>> {
        let mut controls = self.controls.lock().unwrap();
        let (remaining, barrier) = controls.racing_reads.as_mut()?;
        let barrier = Arc::clone(barrier);
        *remaining -= 1;
        if *remaining == 0 {
            controls.racing_reads = None;
        }
        Some(barrier)
    }
}
impl Storage for InterruptedStorage {
    fn persists_history(&self) -> bool {
        self.store.persists_history()
    }
    fn history_page(&self, namespace: &str, query: &InspectionQuery) -> Result<Vec<HistoryItem>> {
        self.store.history_page(namespace, query)
    }
    fn delete_history(&self, namespace: &str, owner: &str, before_ms: i64) -> Result<u64> {
        self.store.delete_history(namespace, owner, before_ms)
    }
    fn acquire(&self, namespace: &str, owner: &str) -> Result<()> {
        self.store.acquire(namespace, owner)
    }
    fn release(&self, namespace: &str, owner: &str) -> Result<()> {
        self.store.release(namespace, owner)
    }
    fn load(&self, namespace: &str) -> Result<VersionedState> {
        let loaded = self.store.load(namespace)?;
        if let Some(barrier) = self.read_barrier() {
            barrier.wait();
        }
        Ok(loaded)
    }
    fn compare_and_swap(
        &self,
        namespace: &str,
        owner: &str,
        revision: u64,
        state: &State,
    ) -> Result<u64> {
        let outcome = std::mem::take(&mut self.controls.lock().unwrap().next_commit);
        if matches!(outcome, CommitOutcome::Conflict) {
            return Err(StorageError::Conflict);
        }
        let committed = self
            .store
            .compare_and_swap(namespace, owner, revision, state)?;
        match outcome {
            CommitOutcome::LostResponse => Err(StorageError::Backend(
                "synthetic lost commit response".into(),
            )),
            _ => Ok(committed),
        }
    }
}
