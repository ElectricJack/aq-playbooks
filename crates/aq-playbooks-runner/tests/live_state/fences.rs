use super::*;

pub(super) type Case = (&'static str, &'static str, fn(&mut State));

pub(super) fn seed_state(store: &dyn Storage, namespace: &str, initial: &State) -> u64 {
    store.acquire(namespace, "w").unwrap();
    store.compare_and_swap(namespace, "w", 0, initial).unwrap()
}

pub(super) fn check_refusal(
    store: &dyn Storage,
    ns: &str,
    (revision, state): (u64, &State),
    (label, expected, change): Case,
) -> Option<String> {
    let mut next = state.clone();
    change(&mut next);
    let result = store.compare_and_swap(ns, "w", revision, &next);
    if result.is_err() {
        assert_eq!(store.load(ns).unwrap().revision, revision, "{label}");
    }
    if refused_with(result.clone(), expected) {
        None
    } else {
        Some(format!("{label}: {result:?}"))
    }
}
