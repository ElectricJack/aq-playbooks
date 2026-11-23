use super::*;

/// `tests/fixtures/legacy_state_0173673.json` is a namespace written by the
/// engine aq-server vendors (0173673) on its in-memory store, by the
/// generator kept in `.local/task9/generator/` (see the Task 9 report): a
/// GC (short windows) that removed 5 finished runs and 3 expired events
/// (tombstones, correlation fences, disposals); 33 finished cohorts still
/// in the record (two with operations, one keyed); live runs: 3 admitted,
/// one waiting for a 24 h event, a terminal parent with a waiting
/// independent child, a parent waiting on its waiting child, a live keyed
/// admission, and an asynchronous operation dispatched without a handle;
/// an admin receipt; events of the completion contract before and after
/// that dispatch; ten events inside their 24 h window.
fn vendored_fixture() -> State {
    serde_json::from_str(include_str!("../fixtures/legacy_state_0173673.json")).unwrap()
}
struct VendoredRuns {
    finished: BTreeSet<String>,
    all: BTreeSet<String>,
    day: ContractRef,
    completion: ContractRef,
    revision: u64,
}
impl VendoredRuns {
    fn from_state(legacy: &State) -> Self {
        let finished: BTreeSet<String> = legacy
            .runs
            .values()
            .filter(|r| r.status.terminal())
            .map(|r| r.id.clone())
            .collect();
        let all_legacy: BTreeSet<String> = legacy.runs.keys().cloned().collect();
        let day = legacy.waits.values().find_map(|w| w.event.clone()).unwrap();
        let completion = legacy
            .events
            .values()
            .find(|e| e.envelope.event_id.starts_with("async-before"))
            .unwrap()
            .envelope
            .contract
            .clone();
        Self {
            finished,
            all: all_legacy,
            day,
            completion,
            revision: legacy.bindings["done"].revision,
        }
    }
    fn day_event(&self, id: &str, invocation: &str) -> EventEnvelope {
        EventEnvelope {
            contract: self.day.clone(),
            correlation: BTreeMap::from([("invocation".into(), invocation.into())]),
            subject: None,
            revision: None,
            ..event(id, 1)
        }
    }
    fn keyed(&self, key: &str) -> KeyedAdmissionRequest {
        KeyedAdmissionRequest {
            key: key.into(),
            binding: "done".into(),
            expected_activation_revision: self.revision,
            entry: "main".into(),
            input: Value::Null,
            authority: authority(),
        }
    }
    fn listed_once(&self, e: &Engine<impl Storage>, step: &str) {
        let ids: Vec<String> = inspect_all(e, InspectionKind::Runs, None)
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .filter(|id| self.all.contains(id))
            .collect();
        let unique: BTreeSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "{step}: listed twice");
        assert_eq!(unique.len(), self.all.len(), "{step}: missing");
    }
    fn drain(&self, e: &Engine<impl Storage>, host: &mut FakeHost) -> (i64, Vec<String>) {
        let mut t = 30_000;
        let mut new_runs = vec![];
        let mut commits = 0;
        while !e.snapshot().unwrap().retention.drain.done {
            t += 1;
            // New work at every step, while the drain goes on.
            let id = e
                .admit("done", "main", Value::Null, authority(), t)
                .unwrap();
            e.drive(host, t, 20).unwrap();
            new_runs.push(id);
            // Legacy duplicates are still recognised.
            assert!(e.admit_once(self.keyed("k-done"), t).unwrap().duplicate);
            assert!(e.admit_once(self.keyed("k-live"), t).unwrap().duplicate);
            assert!(
                e.admit_event("host", self.day_event("day-3", "other-3"), t)
                    .unwrap()
                    .duplicate
            );
            self.listed_once(e, &format!("commit {commits}"));
            commits += 1;
            assert!(commits < 200);
        }
        (t, new_runs)
    }
    fn settle(&self, e: &Engine<impl Storage>, host: &mut FakeHost, t: i64) {
        // The live legacy runs settle normally: the waiting run and both
        // children receive their event, the operation's handle is found and its
        // completion arrives, the keyed and admitted runs advance.
        e.admit_event("host", self.day_event("wake", "exact-invocation"), t + 1)
            .unwrap();
        e.recover(host, t + 1).unwrap();
        e.admit_event(
            "host",
            EventEnvelope {
                contract: self.completion.clone(),
                correlation: BTreeMap::from([("operation".into(), "server-operation".into())]),
                ..async_event(&async_artifact().1, "completion")
            },
            t + 2,
        )
        .unwrap();
        for t in t + 2..t + 10 {
            e.drive(host, t, 50).unwrap();
        }
    }
    fn verify_history(&self, e: &Engine<impl Storage>, new_runs: &[String]) {
        let s = e.snapshot().unwrap();
        assert!(s.runs.is_empty(), "{:?}", s.runs.keys().collect::<Vec<_>>());
        let r = &s.retention;
        assert!(r.event_tombstones.is_empty() && r.expired_correlations.is_empty());
        assert!(r.disposals.is_empty());
        let runs = inspect_all(e, InspectionKind::Runs, None);
        for id in self.all.iter().chain(new_runs) {
            let found: Vec<&Value> = runs.iter().filter(|r| r["id"] == id.as_str()).collect();
            assert_eq!(found.len(), 1, "{id}");
            if !self.finished.contains(id) {
                assert_eq!(found[0]["outcome"], "ok", "{id}: {:?}", found[0]);
            }
        }
    }
}
fn open_vendored<S: Storage>(store: S, legacy: State) -> Engine<S> {
    let mut stored = legacy;
    stored.retention.policy.drain_cohorts_per_commit = 4;
    stored.retention.policy.drain_entries_per_commit = 3;
    let mut value = serde_json::to_value(&stored).unwrap();
    value["retention"].as_object_mut().unwrap().remove("drain");
    let stored: State = serde_json::from_value(value).unwrap();
    store.acquire("test", "vendored").unwrap();
    store
        .compare_and_swap("test", "vendored", 0, &stored)
        .unwrap();
    store.release("test", "vendored").unwrap();
    let e = Engine::new(store, "test", "upgraded").unwrap();
    assert!(
        !e.snapshot().unwrap().retention.drain.done,
        "the drain spans more than the open commit"
    );
    e
}
/// The vendored engine's namespace opens and drains to the end while new
/// runs are admitted and served at every commit (bounds 4 cohorts and 3
/// entries per commit, from its stored policy, so the drain spans many
/// commits); no commit fails. Legacy duplicates are recognised until their
/// entries leave by the normal rules, every legacy live run settles
/// normally, and inspection lists every legacy run exactly once at every
/// step.
fn vendored_namespace_drains_while_new_runs_are_served<S: Storage + Clone>(store: S) {
    let legacy = vendored_fixture();
    let runs = VendoredRuns::from_state(&legacy);
    let e = open_vendored(store, legacy);
    runs.listed_once(&e, "open");
    let mut host = FakeHost {
        responses: BTreeMap::from([(
            "reconcile:test.command".into(),
            vec![DispatchResult::Accepted {
                handle: "server-operation".into(),
            }],
        )]),
        ..Default::default()
    };
    let (t, new_runs) = runs.drain(&e, &mut host);
    runs.settle(&e, &mut host, t);
    runs.verify_history(&e, &new_runs);
    // After the drain a re-delivery of an event the old GC disposed of is
    // judged by the rules of this engine: its tombstone has left, so it is
    // accepted as new.
    assert!(
        !e.admit_event("host", event("gc-old-0", 2), t + 20)
            .unwrap()
            .duplicate
    );
}
#[test]
fn memory_storage_vendored_namespace_drains_while_new_runs_are_served() {
    vendored_namespace_drains_while_new_runs_are_served(MemoryStorage::retaining_settled());
}
#[test]
fn record_storage_vendored_namespace_drains_while_new_runs_are_served() {
    vendored_namespace_drains_while_new_runs_are_served(RecordStorage::new());
}

/// `RuntimeIndexes` fields of the engine that aq-server vendors
/// (`vendor/aq-playbooks-0173673`, pre-D10): no `root_intents` and no
/// backfill flag.
const VENDORED_INDEX_FIELDS: &[&str] = &[
    "ready",
    "ready_tokens",
    "scope_cursor",
    "dispatch_turn",
    "effect_cursor",
    "effect_item_cursor",
    "root_members",
    "root_active",
    "root_effects",
    "root_last_activity",
    "run_roots",
    "intent_roots",
    "effects",
    "effect_memberships",
    "timers",
    "due",
    "run_timer",
    "events",
    "event_memberships",
    "waiters",
    "run_subscriptions",
    "dependents",
    "run_dependencies",
    "completion_ready",
    "uncertain_completion_contracts",
    "uncertain_contract_membership",
    "dirty_runs",
    "dirty_intents",
    "runs",
    "receipts",
    "all_receipts",
    "triggers",
    "activation_triggers",
    "initialized",
];
/// `state` as a pre-D10 engine stored it: the index fields D10 and this task
/// added are absent, so they decode with their defaults.
pub(super) fn vendored_shape(state: &State) -> State {
    let mut value = serde_json::to_value(state).unwrap();
    let indexes = value["indexes"].as_object_mut().unwrap();
    indexes.retain(|field, _| VENDORED_INDEX_FIELDS.contains(&field.as_str()));
    let fields: BTreeSet<&str> = indexes.keys().map(String::as_str).collect();
    assert_eq!(fields, VENDORED_INDEX_FIELDS.iter().copied().collect());
    serde_json::from_value(value).unwrap()
}

/// C1: a namespace written by the pre-D10 engine has no `root_intents`.
/// Opening it backfills the index once, so a pre-upgrade run with an earlier
/// completed operation finishes, leaves with that operation in its history,
/// and a run admitted afterwards progresses.
fn pre_d10_namespace_is_backfilled_on_open<S: Storage + Clone>(store: S) {
    let e = Engine::new(store.clone(), "test", "incarnation-1").unwrap();
    install_on(
        &e,
        invoke_artifact(command(Recovery::KeyedIdempotent), None),
        "invoke",
    );
    let mut host = completed_host();
    let (old, intent) = run_with_completed_operation(&e, &mut host);
    let legacy = vendored_shape(&e.snapshot().unwrap());
    assert!(legacy.indexes.initialized && legacy.indexes.root_intents.is_empty());
    assert!(!legacy.indexes.root_intents_complete);
    overwrite(&e.store, "incarnation-1", &legacy);
    e.release().unwrap();
    let e = Engine::new(store, "test", "incarnation-2").unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.indexes.root_intents_complete);
    assert!(s.indexes.root_intents[&old].contains(&intent));
    let new = e
        .admit("invoke", "main", Value::Null, authority(), 1)
        .unwrap();
    for _ in 0..10 {
        e.drive(&mut host, 1, 20).unwrap();
    }
    let s = e.snapshot().unwrap();
    assert!(
        s.runs.is_empty() && s.intents.is_empty(),
        "{:?}",
        s.runs.keys().collect::<Vec<_>>()
    );
    let effects = inspect_all(&e, InspectionKind::Effects, Some(&old));
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0]["id"], intent.as_str());
    assert_eq!(effects[0]["status"], "completed");
    for id in [&old, &new] {
        let runs = inspect_all(&e, InspectionKind::Runs, Some(id));
        assert_eq!(runs[0]["outcome"], "ok", "{id}");
    }
}

#[test]
fn memory_storage_pre_d10_namespace_is_backfilled_on_open() {
    pre_d10_namespace_is_backfilled_on_open(MemoryStorage::retaining_settled());
}

#[test]
fn record_storage_pre_d10_namespace_is_backfilled_on_open() {
    pre_d10_namespace_is_backfilled_on_open(RecordStorage::new());
}
