//! Exercise each backend's replay fixtures through the full and bounded commit paths.
use super::{
    ReplayMember, bulk_eviction_state, evicted_correctly, eviction_refusal_cases, eviction_state,
};
use aq_playbooks_runner::*;
type Mutation = dyn Fn(&mut State);
type AllowedEviction = (&'static str, State, Box<Mutation>);
type Result<T> = std::result::Result<T, StorageError>;

fn allowed_evictions() -> [AllowedEviction; 2] {
    [
        (
            "the correct eviction",
            eviction_state(),
            Box::new(evicted_correctly),
        ),
        (
            "64 at once",
            bulk_eviction_state(),
            Box::new(|s| {
                for i in 0..64 {
                    ReplayMember::Keyed(&format!("b{i:03}")).evict(s);
                }
            }),
        ),
    ]
}
pub(super) fn transition(
    store: &dyn Storage,
    namespace: &str,
    mutate: &Mutation,
) -> Result<TransitionOutcome> {
    let mut plan = |s: &mut State| {
        mutate(s);
        Ok(true)
    };
    store
        .transition_v1(
            namespace,
            "w",
            &mut TransitionRequest {
                operation: "evict",
                plan: &mut plan,
            },
        )
        .map_err(|e| match e {
            Error::Storage(e) => e,
            other => StorageError::Backend(other.to_string()),
        })
}
type Seed<'a> = dyn Fn(&State) -> (String, u64) + 'a;
/// Each refusal leaves the revision unchanged. Bounded probes additionally report
/// whether the refusal fell back to a full commit.
pub(super) fn refusals<T: std::fmt::Debug>(
    store: &dyn Storage,
    seed: &Seed<'_>,
    apply: impl Fn(&str, u64, &State, &Mutation) -> (Result<T>, bool),
) -> Vec<String> {
    let mut failures = vec![];
    for (label, expected, previous, mutate) in eviction_refusal_cases() {
        let (ns, base) = seed(&previous);
        let (result, full) = apply(&ns, base, &previous, mutate.as_ref());
        if !matches!(&result,Err(StorageError::Backend(error)) if error==expected) {
            failures.push(format!("{label}: {result:?}"));
            continue;
        }
        if full {
            failures.push(format!("{label}: refused only on the complete path"));
            continue;
        }
        assert_eq!(store.load(&ns).unwrap().revision, base, "{label}");
    }
    failures
}
pub(super) fn allowances<T>(
    seed: &Seed<'_>,
    apply: impl Fn(&str, u64, &State, &Mutation) -> Result<T>,
) -> Vec<String> {
    allowed_evictions()
        .into_iter()
        .filter_map(|(label, previous, mutate)| {
            let (ns, base) = seed(&previous);
            apply(&ns, base, &previous, mutate.as_ref())
                .err()
                .map(|err| format!("{label} refused: {err}"))
        })
        .collect()
}
pub(super) fn cas(store: &dyn Storage, seed: &Seed<'_>) -> Vec<String> {
    let commit = |ns: &str, base: u64, previous: &State, mutate: &Mutation| {
        let mut next = previous.clone();
        mutate(&mut next);
        store.compare_and_swap(ns, "w", base, &next)
    };
    let mut failures = refusals(store, seed, |ns, base, previous, mutate| {
        (commit(ns, base, previous, mutate), false)
    });
    failures.extend(allowances(seed, commit));
    failures
}
