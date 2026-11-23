use super::*;

fn rebase_fences_hold_on_cas(store: &dyn Storage) {
    let s = fence_state();
    let count = std::cell::Cell::new(0);
    let fresh = |state: &State| {
        count.set(count.get() + 1);
        let ns = format!("rebase-{}", count.get());
        let base = seed_state(store, &ns, state);
        (ns, base)
    };
    let mut next = s.clone();
    rebased(&mut next);
    let before = rebase_refusal_cases().into_iter().map(|case| {
        let (ns, revision) = fresh(&s);
        check_refusal(store, &ns, (revision, &s), case)
    });
    let after = after_rebase_refusal_cases().into_iter().map(|case| {
        let (ns, base) = fresh(&s);
        let revision = store.compare_and_swap(&ns, "w", base, &next).unwrap();
        let stored = store.load(&ns).unwrap().state;
        check_refusal(store, &ns, (revision, &stored), case)
    });
    let failures: Vec<_> = before.chain(after).flatten().collect();
    assert!(failures.is_empty(), "{failures:#?}");
    // A rebase, then a floor lowering with its own receipt.
    let (ns, base) = fresh(&s);
    let r = store.compare_and_swap(&ns, "w", base, &next).unwrap();
    let mut later = store.load(&ns).unwrap().state;
    floor_lowered(&mut later);
    store.compare_and_swap(&ns, "w", r, &later).unwrap();
    let stored = store.load(&ns).unwrap().state.retention;
    assert_eq!(
        (stored.clock_ms, stored.stale_floor_ms),
        (Some(100), Some(40))
    );
}
#[test]
fn memory_storage_opens_the_clock_fences_only_for_a_rebase() {
    rebase_fences_hold_on_cas(&MemoryStorage::default());
    let store = MemoryStorage::default();
    store.acquire("ok", "w").unwrap();
    let s = fence_state();
    let base = store.compare_and_swap("ok", "w", 0, &s).unwrap();
    let mut next = s.clone();
    rebased(&mut next);
    store.compare_and_swap("ok", "w", base, &next).unwrap();
    let r = store.load("ok").unwrap().state.retention;
    assert_eq!(
        (
            r.clock_ms,
            r.stale_floor_ms,
            r.admission_expires_at_ms.get("k").copied()
        ),
        (Some(100), Some(50), Some(2_100))
    );
}
#[test]
fn record_storage_opens_the_clock_fences_only_for_a_rebase() {
    rebase_fences_hold_on_cas(&RecordStorage::new());
    let store = RecordStorage::new();
    store.acquire("ok", "w").unwrap();
    let s = fence_state();
    let base = store.compare_and_swap("ok", "w", 0, &s).unwrap();
    let mut next = s.clone();
    rebased(&mut next);
    store.compare_and_swap("ok", "w", base, &next).unwrap();
    assert_eq!(
        store.load("ok").unwrap().state.retention.clock_ms,
        Some(100)
    );
}
