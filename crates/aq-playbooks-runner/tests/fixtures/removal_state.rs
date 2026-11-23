//! History and removal-fence fixtures exercised against memory, records and PostgreSQL.
use super::{accepted_event, audit, long_artifact, long_contract, receipt};
use aq_playbooks_runner::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// One completed root run with the supplied receipt sequences.
pub(super) fn synthetic_cohort(id: &str, finished_at_ms: i64, receipts: &[u64]) -> SettledCohort {
    let run = Run {
        catalog_revision: 1,
        id: id.into(),
        root: id.into(),
        parent: None,
        origin: None,
        artifact: "artifact".into(),
        entry: "main".into(),
        dependencies: BTreeMap::new(),
        authority: Authority {
            actor: "alice".into(),
            scope: "project".into(),
            capabilities: ["execute".to_string()].into(),
        },
        step: "ok".into(),
        status: RunStatus::Completed,
        input: Value::Null,
        variables: BTreeMap::new(),
        outcome: Some("ok".into()),
        output: Some(Value::Null),
        reason: None,
        admitted_at_ms: 1,
        visits: 1,
        depth: 0,
        children: vec![],
        pending: None,
        loops: vec![],
        attempts: BTreeMap::new(),
        pending_return: None,
        pending_failure: false,
        terminal_sequence: receipts.last().copied(),
    };
    SettledCohort {
        root: id.into(),
        finished_at_ms,
        runs: vec![run],
        intents: vec![],
        receipts: receipts
            .iter()
            .map(|&sequence| Receipt {
                sequence,
                run: id.into(),
                step: "ok".into(),
                at_ms: finished_at_ms,
                kind: "terminal".into(),
                detail: json!({"status": "completed", "outcome": "ok"}),
            })
            .collect(),
    }
}

/// A terminal root run `id` with no children.
pub(super) fn completed_run(id: &str) -> Run {
    synthetic_cohort(id, 1, &[]).runs.remove(0)
}

pub(super) fn tombstone(digest: &str) -> retention::EventTombstone {
    retention::EventTombstone {
        producer: "host".into(),
        epoch: 0,
        digest: digest.into(),
        disposed_at_ms: 5,
    }
}

pub(super) fn refused_with<T>(result: std::result::Result<T, StorageError>, text: &str) -> bool {
    matches!(result, Err(StorageError::Backend(m)) if m == text)
}

pub(super) fn removal_fences_hold(store: &impl Storage, ns: &str) {
    store.acquire(ns, "w").unwrap();
    let mut s = State::default();
    // Round 1 (I2): a recorded expiry is at least acceptance plus the policy
    // window, so the brief's 1_000 needs a 1_000 window.
    s.retention.policy.admission_replay_ms = 1_000;
    s.retention.clock_ms = Some(500);
    s.keyed_admissions.insert("k".into(), receipt("k", 0));
    s.retention
        .admission_expires_at_ms
        .insert("k".into(), 1_000);
    s.runs.insert("run-a".into(), completed_run("run-a"));
    let r = store.compare_and_swap(ns, "w", 0, &s).unwrap();
    let mut next = s.clone();
    next.keyed_admissions.remove("k");
    next.retention.clock_ms = Some(999);
    assert!(
        matches!(store.compare_and_swap(ns, "w", r, &next), Err(StorageError::Backend(m)) if m == retention::REFUSE_EARLY_REMOVAL)
    );
    next.retention.clock_ms = Some(1_000);
    let r = store.compare_and_swap(ns, "w", r, &next).unwrap();
    let mut gone = next.clone();
    gone.runs.remove("run-a");
    assert!(
        matches!(store.compare_and_swap(ns, "w", r, &gone), Err(StorageError::Backend(m)) if m == retention::REFUSE_RUN_REMOVAL)
    );
    gone.settled.push(SettledCohort {
        root: "run-a".into(),
        finished_at_ms: 1,
        runs: vec![completed_run("run-a")],
        intents: vec![],
        receipts: vec![],
    });
    let r = store.compare_and_swap(ns, "w", r, &gone).unwrap();
    let mut back = gone.clone();
    back.settled.clear();
    back.retention.clock_ms = Some(10);
    assert!(
        matches!(store.compare_and_swap(ns, "w", r, &back), Err(StorageError::Backend(m)) if m == retention::REFUSE_CLOCK)
    );
}

/// Policy windows: receipts 2_000, events 3_000; recorded expiries "k"
/// 3_000 and "ev" 4_000; "fallback" has none. Receipt 1 belongs to the
/// finished run-a, receipt 2 to the running run-b.
pub(super) fn base_fence_state() -> State {
    let mut s = State::default();
    s.retention.clock_ms = Some(500);
    s.retention.policy.admission_replay_ms = 2_000;
    s.retention.policy.event_replay_ms = 3_000;
    s.runs.insert("run-a".into(), completed_run("run-a"));
    s.runs.insert("run-b".into(), {
        let mut run = completed_run("run-b");
        run.status = RunStatus::Running;
        run
    });
    s.receipts.push(audit(1, "run-a"));
    s.receipts.push(audit(2, "run-b"));
    s.sequence = 2;
    s.keyed_admissions.insert("k".into(), receipt("k", 0));
    s.keyed_admissions
        .insert("fallback".into(), receipt("fallback", 0));
    s.retention
        .admission_expires_at_ms
        .insert("k".into(), 3_000);
    s.events.insert("ev".into(), accepted_event("ev", 0));
    s.retention.event_expires_at_ms.insert("ev".into(), 4_000);
    // Round 2 (C1): an installed contract whose window (5_000) is longer
    // than the policy's, and an event of it expiring at 9_000.
    let long = long_artifact();
    s.artifacts.insert(long.digest.clone(), long);
    s.events.insert("ev-long".into(), {
        let mut e = accepted_event("ev-long", 0);
        e.envelope.contract = aq_playbooks_model::ContractRef {
            name: long_contract().name,
            fingerprint: long_contract().fingerprint().unwrap(),
        };
        e
    });
    s.retention
        .event_expires_at_ms
        .insert("ev-long".into(), 9_000);
    s
}
