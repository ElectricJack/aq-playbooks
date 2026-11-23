use super::*;

/// The serialized snapshot and the Runs, Effects and Receipts pages (items
/// and cursor; the page revision is the backend's own) of an engine.
type ParityView = (Value, Vec<(Vec<Value>, Option<String>)>);
fn parity_view<S: Storage>(e: &Engine<S>) -> ParityView {
    let pages = [
        InspectionKind::Runs,
        InspectionKind::Effects,
        InspectionKind::Receipts,
    ]
    .into_iter()
    .map(|kind| {
        let page = e
            .inspect(&InspectionQuery {
                kind,
                ..runs_query()
            })
            .unwrap();
        (page.items, page.next)
    })
    .collect();
    (serde_json::to_value(e.snapshot().unwrap()).unwrap(), pages)
}
/// Run `$call` with `$e` bound to the reference engine, then to the format 3
/// engine; its results (`Debug`) and both engines' views must be equal.
/// Returns the format 3 result and the format 3 store's work counters for
/// the call alone.
macro_rules! on_both {
    ($reference:expr, $records:expr, $label:expr, |$e:ident| $call:expr) => {{
        $records.store.reset_transition_stats();
        let expected = {
            let $e = &$reference;
            $call
        };
        let actual = {
            let $e = &$records;
            $call
        };
        let stats = $records.store.transition_stats();
        assert_eq!(
            format!("{actual:?}"),
            format!("{expected:?}"),
            "{}: result",
            $label
        );
        let (reference, records) = (parity_view(&$reference), parity_view(&$records));
        assert_eq!(records.0, reference.0, "{}: snapshot", $label);
        assert_eq!(records.1, reference.1, "{}: inspection pages", $label);
        (actual, stats)
    }};
}
/// The same script on `Engine<MemoryStorage>` (the reference) and on
/// `Engine<PostgresStorage>` writing format 3: after every call the two
/// agree (snapshot, Runs/Effects/Receipts pages, call results), and format 3
/// takes the commit path SC-13 names: a commit that settles a cohort is
/// heavy; an admission and the commit that drops expired window entries are
/// bounded; no commit that settles nothing takes a full-container fallback.
#[test]
fn format_3_matches_the_reference_for_settle_drop_stale_and_drain() {
    let ns = support::namespace("parity");
    let reference = Engine::new(MemoryStorage::default(), &ns, "w").unwrap();
    let records = Engine::new(support::records(), &ns, "w").unwrap();
    let a = support::artifact("a");
    let digest = a.digest.clone();
    let setup = [
        on_both!(reference, records, "install", |e| e.install(a.clone())),
        on_both!(reference, records, "activate", |e| e.activate_at(
            "a",
            &digest,
            pg_authority(),
            0
        )),
        on_both!(reference, records, "policy", |e| e.set_retention_policy(
            RetentionPolicy {
                admission_replay_ms: 1_000,
                ..RetentionPolicy::default()
            }
        )),
    ];
    for (result, stats) in setup {
        result.unwrap();
        assert!(stats.fallbacks.is_empty(), "setup: {:?}", stats.fallbacks);
    }
    for i in 0..3_i64 {
        let label = format!("admit k-{i}");
        let (_, stats) = on_both!(reference, records, label, |e| e
            .admit_once(pg_keyed(e, &format!("k-{i}")), i));
        assert_eq!((stats.incremental, stats.heavy), (1, 0), "{label}");
        assert!(stats.fallbacks.is_empty(), "{label}: {:?}", stats.fallbacks);
        let label = format!("settle k-{i}");
        let (_, stats) = on_both!(reference, records, label, |e| e.drive(
            &mut FakeHost::default(),
            i,
            20
        ));
        assert_eq!(stats.heavy, 1, "{label}: {stats:?}");
        assert!(records.snapshot().unwrap().runs.is_empty(), "{label}");
    }
    // k-0..k-2 expire at 1_000..1_002: the admission at 2_000 drops all three.
    let (_, stats) = on_both!(reference, records, "drop", |e| e
        .admit_once(pg_keyed(e, "k-3"), 2_000));
    assert_eq!((stats.incremental, stats.heavy), (1, 0), "drop: {stats:?}");
    assert!(stats.fallbacks.is_empty(), "drop: {:?}", stats.fallbacks);
    let s = records.snapshot().unwrap();
    assert_eq!(
        s.keyed_admissions.keys().cloned().collect::<Vec<_>>(),
        vec!["k-3".to_string()]
    );
    // A forgotten key whose request time is older than the window.
    let (stale, stats) = on_both!(reference, records, "stale", |e| e.admit_once_at(
        pg_keyed(e, "k-0"),
        Some(500),
        2_001
    ));
    assert!(
        matches!(stale, Err(Error::Stale { .. })),
        "stale: {stale:?}"
    );
    assert_eq!((stats.incremental, stats.heavy), (0, 0), "stale: {stats:?}");
    verify_legacy_drain(reference, records, &ns);
}

fn verify_legacy_drain(
    reference: Engine<MemoryStorage>,
    records: Engine<PostgresStorage>,
    ns: &str,
) {
    // 60 legacy cohorts seeded by a direct CAS on each backend, then opened
    // again: the drain takes 50 at open and the rest in the next commit.
    let reference_store = reference.store.clone();
    let revision = reference_store.load(ns).unwrap().revision;
    let legacy = with_legacy_cohorts(&reference.snapshot().unwrap(), 60);
    reference_store
        .compare_and_swap(ns, "w", revision, &legacy)
        .unwrap();
    let revision = records.store.load(ns).unwrap().revision;
    records
        .store
        .compare_and_swap(ns, "w", revision, &legacy)
        .unwrap();
    reference.release().unwrap();
    records.release().unwrap();
    let reference = Engine::new(reference_store, ns, "w2").unwrap();
    let records = Engine::new(support::records(), ns, "w2").unwrap();
    let (r, p) = (parity_view(&reference), parity_view(&records));
    assert_eq!(p.0, r.0, "open: snapshot");
    assert_eq!(p.1, r.1, "open: inspection pages");
    let left = |e: &Engine<PostgresStorage>| {
        e.snapshot()
            .unwrap()
            .runs
            .keys()
            .filter(|id| id.starts_with("run-0-legacy-"))
            .count()
    };
    assert_eq!(left(&records), 10, "open drains 50");
    let (_, stats) = on_both!(reference, records, "drain", |e| e.drive(
        &mut FakeHost::default(),
        2_002,
        20
    ));
    assert_eq!(stats.heavy, 1, "drain: {stats:?}");
    assert_eq!(left(&records), 0, "drain: {stats:?}");
    let s = records.snapshot().unwrap();
    assert!(s.retention.drain.done && s.runs.is_empty(), "drained");
    assert_eq!(history_runs(ns), 64, "60 legacy runs and k-0..k-3");
}
