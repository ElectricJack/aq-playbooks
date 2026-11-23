use super::*;
type PgEngine = Engine<PostgresStorage>;
struct SignalNamespace {
    engine: PgEngine,
    format: &'static str,
}
impl SignalNamespace {
    fn open(store: PostgresStorage, ns: &str, format: &'static str) -> Self {
        let e = Engine::new(store, ns, "w").unwrap();
        let mut content = support::artifact("a").content;
        content
            .registry
            .events
            .insert("test.signal".into(), pg_event_contract());
        let a = aq_playbooks_model::Artifact::seal(content).unwrap();
        let digest = a.digest.clone();
        e.install(a).unwrap();
        e.activate_at("a", &digest, pg_authority(), 0).unwrap();
        Self { engine: e, format }
    }
}

fn set_replay_windows(e: &PgEngine) {
    e.set_retention_policy(RetentionPolicy {
        admission_replay_ms: 1_000,
        event_replay_ms: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
}
/// The 64-drop script on f2 protected and f3: 65 finished keyed runs with a
/// 1 s window and one event with a 1 s window, then one admission past them.
/// The dropping commit passes the removal fence on f2 and stays on the
/// bounded path on f3; an expired key admitted again leaves first and is
/// new; no tombstone row is written.
#[test]
fn window_drop_on_postgres_is_incremental_on_f3_and_fenced_on_f2() {
    for (format, store) in both() {
        let ns = support::namespace("window");
        let fixture = SignalNamespace::open(store, &ns, format);
        let e = &fixture.engine;
        set_replay_windows(e);
        e.admit_event("host", pg_event("e1"), 0).unwrap();
        for i in 0..65 {
            let d = e.admit_once(pg_keyed(e, &format!("k-{i:03}")), i).unwrap();
            e.advance(&d.receipt.run_id, i).unwrap();
        }
        let s = e.snapshot().unwrap();
        assert_eq!(
            (s.keyed_admissions.len(), s.events.len()),
            (65, 1),
            "{format}"
        );
        e.store.reset_transition_stats();
        e.admit_once(pg_keyed(e, "k-new"), 2_000).unwrap();
        fixture.assert_oldest_dropped();
        // An expired key admitted again: its receipt leaves in a commit of
        // its own, then the key is new (and the drop takes k-064's sibling).
        let again = e.admit_once(pg_keyed(e, "k-064"), 2_001).unwrap();
        assert!(!again.duplicate, "{format}");
        let s = e.snapshot().unwrap();
        assert_eq!(
            s.keyed_admissions.keys().cloned().collect::<Vec<_>>(),
            vec!["k-064".to_string(), "k-new".to_string()],
            "{format}"
        );
        assert_eq!(s.keyed_admissions["k-064"], again.receipt, "{format}");
        if format == "f3" {
            let stats = e.store.transition_stats();
            assert_eq!((stats.incremental, stats.heavy), (3, 0), "{format}");
        }
        // A redelivered event after its window is new.
        assert!(
            !e.admit_event("host", pg_event("e1"), 2_002)
                .unwrap()
                .duplicate
        );
        assert_eq!(e.snapshot().unwrap().events.len(), 1, "{format}");
        assert_eq!(tombstone_rows(&ns), 0, "{format}");
        if format == "f3" {
            check_burst_records(e);
        }
    }
}

fn check_burst_records(e: &PgEngine) {
    // A burst in one millisecond: one small record per entry, and
    // each admission stays on the bounded path.
    e.store.reset_transition_stats();
    for i in 0..20 {
        e.admit_once(pg_keyed(e, &format!("burst-{i:02}")), 2_003)
            .unwrap();
    }
    assert_eq!(e.store.transition_stats().heavy, 0);
    let row = support::client()
                .query_one(
                    "SELECT count(*), max(octet_length(value)) FROM aq_playbooks.state_records WHERE namespace=$1 AND field='retention.expiry'",
                    &[&e.namespace],
                )
                .unwrap();
    let (records, largest): (i64, i32) = (row.get(0), row.get(1));
    assert_eq!(records, 23, "k-064, k-new, the event and 20 burst entries");
    assert!(largest <= 8, "{largest} bytes");
}

/// Ruling 3, the full scenario on PostgreSQL protected format 2 and format
/// 3: normal operation, one far-future host time, stale refusals that write
/// nothing and entries that do not expire, the rebase, then normal
/// operation with held events, live runs and duplicates intact and the
/// record no longer growing.
#[test]
fn a_pinned_clock_is_rebased_on_postgres() {
    for (format, store) in both() {
        let ns = support::namespace("rebase");
        let fixture = SignalNamespace::open(store, &ns, format);
        let e = &fixture.engine;
        let w = pg_waiter_artifact();
        let waiter_digest = w.digest.clone();
        e.install(w).unwrap();
        e.activate_at("waiter", &waiter_digest, pg_authority(), 0)
            .unwrap();
        set_replay_windows(e);
        let held = fixture.pin_clock();
        fixture.assert_pinned_requests();
        fixture.rebase_clock(&held);
        let during = fixture.resume(&held);
        fixture.assert_rolling_admissions(&held, &during);
    }
}
const T0: i64 = 10_000;
const T1: i64 = T0 + 200;
const T2: i64 = T0 + 500;
const PINNED: i64 = T0 + 1_000_000_000;
struct Held {
    live: String,
    waiter: String,
    event: String,
}

/// The engine on PostgreSQL with `replay_keep` 3: events past it evict the
/// oldest, on f3 incrementally (no heavy commit), on f2 protected through
/// the complete fence; an evicted event delivered again is new, a kept one
/// a duplicate. Finished keyed runs then evict the oldest too.
#[test]
fn evictions_on_postgres_are_incremental_on_f3_and_fenced_on_f2() {
    for (format, store) in both() {
        let ns = support::namespace("evict");
        let fixture = SignalNamespace::open(store, &ns, format);
        let e = &fixture.engine;
        e.set_retention_policy(RetentionPolicy {
            replay_keep: 3,
            ..RetentionPolicy::default()
        })
        .unwrap();
        e.store.reset_transition_stats();
        for i in 0..6 {
            e.admit_event("host", pg_event(&format!("e{i}")), i)
                .unwrap();
        }
        let s = e.snapshot().unwrap();
        assert_eq!(
            (s.events.len(), s.retention.replay_order.len()),
            (3, 3),
            "{format}"
        );
        if format == "f3" {
            let stats = e.store.transition_stats();
            assert_eq!((stats.incremental, stats.heavy), (6, 0), "{format}");
        }
        assert!(
            e.admit_event("host", pg_event("e5"), 10).unwrap().duplicate,
            "{format}"
        );
        assert!(
            !e.admit_event("host", pg_event("e0"), 11).unwrap().duplicate,
            "{format}"
        );
        for i in 0..4 {
            let d = e.admit_once(pg_keyed(e, &format!("k{i}")), 20 + i).unwrap();
            e.advance(&d.receipt.run_id, 20 + i).unwrap();
        }
        let s = e.snapshot().unwrap();
        assert_eq!(
            (
                s.retention.replay_order.len(),
                s.keyed_admissions.len() + s.events.len()
            ),
            (3, 3),
            "{format}"
        );
        assert!(s.events.is_empty(), "{format}");
        assert!(!s.keyed_admissions.contains_key("k0"), "{format}");
    }
}

/// On protected format 2 and format 3, `collect_garbage` over an expired
/// event writes no `disposal_tombstones` row and no tombstone, fence or
/// disposal record: the event leaves by the commit's window drop, and a
/// redelivery is new.
#[test]
fn collect_garbage_writes_no_disposal_tombstone_row_on_postgres() {
    for (format, store) in both() {
        let ns = support::namespace("gc_rows");
        let fixture = SignalNamespace::open(store, &ns, format);
        let e = &fixture.engine;
        e.set_retention_policy(RetentionPolicy {
            event_replay_ms: 1,
            audit_ms: 1,
            ..RetentionPolicy::default()
        })
        .unwrap();
        let first = e.admit_event("host", pg_event("gone"), 0).unwrap();
        let report = e.collect_garbage(10_000, 100).unwrap();
        assert_eq!(report.events, 1, "{format}");
        let s = e.snapshot().unwrap();
        assert!(!s.events.contains_key(&first.key), "{format}");
        assert_eq!(
            (
                s.retention.event_tombstones.len(),
                s.retention.expired_correlations.len(),
                s.retention.disposals.len()
            ),
            (0, 0, 0),
            "{format}: legacy retention records"
        );
        let rows = tombstone_rows(&ns);
        assert_eq!(rows, 0, "{format}");
        assert!(
            !e.admit_event("host", pg_event("gone"), 10_001)
                .unwrap()
                .duplicate,
            "{format}"
        );
    }
}

impl SignalNamespace {
    fn assert_oldest_dropped(&self) {
        let (e, format) = (&self.engine, self.format);
        let s = e.snapshot().unwrap();
        // The event (1_000) and k-000..k-062 (1_000..1_062): 64, oldest first.
        assert!(s.events.is_empty(), "{format}");
        assert_eq!(
            s.keyed_admissions.keys().cloned().collect::<Vec<_>>(),
            vec![
                "k-063".to_string(),
                "k-064".to_string(),
                "k-new".to_string()
            ],
            "{format}"
        );
        assert!(
            s.retention.event_epochs.is_empty()
                && s.retention.event_expires_at_ms.is_empty()
                && s.indexes.events.is_empty()
                && s.indexes.event_memberships.is_empty(),
            "{format}"
        );
        assert_eq!(
            s.retention
                .expiry
                .keys()
                .map(|k| retention::window_entry(k).unwrap().0)
                .collect::<Vec<_>>(),
            vec![1_063, 1_064, 3_000],
            "{format}"
        );
        assert!(
            s.retention.event_tombstones.is_empty() && s.retention.expired_correlations.is_empty(),
            "{format}"
        );
        let stats = e.store.transition_stats();
        if format == "f3" {
            assert_eq!((stats.incremental, stats.heavy), (1, 0), "{format}");
        }
    }

    fn pin_clock(&self) -> Held {
        let (e, format) = (&self.engine, self.format);
        let done = e.admit_once_at(pg_keyed(e, "done"), Some(T0), T0).unwrap();
        e.advance(&done.receipt.run_id, T0).unwrap();
        let live = e.admit_once_at(pg_keyed(e, "live"), Some(T0), T0).unwrap();
        let waiter = e
            .admit("waiter", "main", Value::Null, pg_authority(), T0)
            .unwrap();
        e.advance(&waiter, T0).unwrap();
        let held = e
            .admit_event_at("host", pg_event("held"), 0, Some(T0), T0)
            .unwrap();
        e.admit("a", "main", Value::Null, pg_authority(), PINNED)
            .unwrap();
        let s = e.snapshot().unwrap();
        assert!(!s.keyed_admissions.contains_key("done"), "{format}");
        assert!(s.events.contains_key(&held.key), "{format}");
        Held {
            live: live.receipt.run_id,
            waiter,
            event: held.key,
        }
    }

    fn assert_pinned_requests(&self) {
        let (e, format) = (&self.engine, self.format);
        // Real time again: stale, and nothing written (M5).
        let revision = e.store.load(&e.namespace).unwrap().revision;
        assert!(matches!(
            e.admit_once_at(pg_keyed(e, "during"), Some(T1), T1),
            Err(Error::Stale { .. })
        ));
        assert!(matches!(
            e.admit_event_at("host", pg_event("during-e"), 0, Some(T1), T1),
            Err(Error::Stale { .. })
        ));
        assert_eq!(
            e.store.load(&e.namespace).unwrap().revision,
            revision,
            "{format}"
        );
        for i in 0..3 {
            let d = e
                .admit_once(pg_keyed(e, &format!("pin-{i}")), T1 + i)
                .unwrap();
            e.advance(&d.receipt.run_id, T1 + i).unwrap();
        }
        e.admit("a", "main", Value::Null, pg_authority(), T1 + 5_000)
            .unwrap();
        assert_eq!(e.snapshot().unwrap().keyed_admissions.len(), 4, "{format}");
    }

    fn rebase_clock(&self, held: &Held) {
        let (e, format) = (&self.engine, self.format);
        // The rebase.
        let report = e
            .rebase_retention_clock(T2, T0, "ops", "pinned clock", T2)
            .unwrap();
        assert_eq!(
            (
                report.from_ms,
                report.capped_admissions,
                report.capped_events
            ),
            (PINNED, 4, 1),
            "{format}"
        );
        let s = e.snapshot().unwrap();
        assert_eq!(
            (s.retention.clock_ms, s.retention.stale_floor_ms),
            (Some(T2), Some(T0)),
            "{format}"
        );
        assert_eq!(
            s.retention.event_expires_at_ms.get(&held.event).copied(),
            Some(T2 + 1_000),
            "{format}"
        );
        let rows: i64 = support::client()
            .query_one(
                "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1 AND kind='receipts' AND public_value->>'kind'='retention_clock_rebased'",
                &[&e.namespace],
            )
            .unwrap()
            .get(0);
        assert_eq!(rows, 1, "{format}: the rebase receipt is history");
    }

    fn resume(&self, held: &Held) -> String {
        let (e, format) = (&self.engine, self.format);
        // Normal operation resumes.
        let during = e
            .admit_once_at(pg_keyed(e, "during"), Some(T1), T2)
            .unwrap();
        assert!(!during.duplicate, "{format}");
        for (key, request_time) in [("during", T1), ("live", T0)] {
            assert!(
                e.admit_once_at(pg_keyed(e, key), Some(request_time), T2 + 10)
                    .unwrap()
                    .duplicate
            );
        }
        assert!(
            e.admit_event_at("host", pg_event("held"), 0, Some(T0), T2 + 10)
                .unwrap()
                .duplicate
        );
        let s = e.snapshot().unwrap();
        assert!(s.runs.contains_key(&held.waiter), "{format}");
        assert!(s.runs.contains_key(&held.live), "{format}");
        // The floor refuses a retry of the early-forgotten key.
        assert!(matches!(
            e.admit_once_at(pg_keyed(e, "done"), Some(T0), T2 + 10),
            Err(Error::Stale { floor_ms: Some(f), .. }) if f == T0
        ));
        during.receipt.run_id
    }

    fn assert_rolling_admissions(&self, held: &Held, during: &str) {
        let (e, format) = (&self.engine, self.format);
        // The record stops growing.
        for run in [held.live.as_str(), during] {
            e.advance(run, T2 + 20).unwrap();
        }
        for i in 0..10 {
            let at = T2 + 1_001 + i * 200;
            let d = e
                .admit_once_at(pg_keyed(e, &format!("after-{i}")), Some(at), at)
                .unwrap();
            e.advance(&d.receipt.run_id, at).unwrap();
        }
        let s = e.snapshot().unwrap();
        let mut keys: Vec<String> = s.keyed_admissions.keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            (5..10).map(|i| format!("after-{i}")).collect::<Vec<_>>(),
            "{format}"
        );
    }
}
