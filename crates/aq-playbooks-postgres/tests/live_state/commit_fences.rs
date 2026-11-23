use super::*;
fn commit_formats() -> [(&'static str, PostgresStorage); 3] {
    [
        ("f2_protected", support::protected()),
        ("f2_plain", support::store()),
        ("f3_cas", support::records()),
    ]
}
struct CommitPath<'a> {
    store: &'a PostgresStorage,
    label: &'a str,
    operation: &'static str,
}
impl CommitPath<'_> {
    fn seed(&self, state: &State) -> (String, u64) {
        let ns = support::namespace(self.label);
        self.store.acquire(&ns, "w").unwrap();
        let revision = self.store.compare_and_swap(&ns, "w", 0, state).unwrap();
        (ns, revision)
    }
    fn transition(
        &self,
        ns: &str,
        f: &dyn Fn(&mut State),
    ) -> std::result::Result<TransitionOutcome, StorageError> {
        let mut plan = |s: &mut State| {
            f(s);
            Ok(true)
        };
        storage_error(self.store.transition_v1(
            ns,
            "w",
            &mut TransitionRequest {
                operation: self.operation,
                plan: &mut plan,
            },
        ))
    }
}
/// The refusals every commit path must make, each as a change of
/// `fence_state()`.
fn refusal_cases() -> Vec<Case> {
    let mut cases = live_run_cases().into_iter().collect::<Vec<_>>();
    cases.extend(window_cases());
    cases.extend(receipt_cases());
    cases.extend(expiry_cases());
    cases.extend(rebase_refusal_cases());
    cases
}
fn live_run_cases() -> [Case; 2] {
    [
        ("run", retention::REFUSE_RUN_REMOVAL, |n| {
            n.runs.remove("run-a");
        }),
        (
            "running settled run",
            "unresolved run obligations cannot be disposed",
            |n| {
                let running = n.runs.remove("run-b").unwrap();
                n.receipts.retain(|r| r.sequence != 2);
                let mut c = cohort_of(running);
                c.receipts.push(audit(2, "run-b"));
                n.settled.push(c);
            },
        ),
    ]
}
fn window_cases() -> [Case; 4] {
    let early = retention::REFUSE_EARLY_REMOVAL;
    [
        ("keyed admission", early, |n| {
            n.retention.clock_ms = Some(2_999);
            n.keyed_admissions.remove("k");
            n.retention.admission_expires_at_ms.remove("k");
        }),
        ("keyed admission, policy window", early, |n| {
            n.retention.clock_ms = Some(1_999);
            n.keyed_admissions.remove("fallback");
        }),
        ("event", early, |n| {
            n.retention.clock_ms = Some(3_999);
            n.events.remove("ev");
            n.retention.event_expires_at_ms.remove("ev");
        }),
        ("forged legacy tombstone", early, |n| {
            n.events.remove("ev");
            n.retention.event_expires_at_ms.remove("ev");
            n.retention
                .event_tombstones
                .insert("ev".into(), tombstone("digest-ev"));
        }),
    ]
}
fn receipt_cases() -> [Case; 4] {
    [
        (
            "changed receipt",
            "immutable keyed admission receipt removed or changed",
            |n| {
                n.keyed_admissions.get_mut("k").unwrap().run_id = "other".into();
            },
        ),
        (
            "receipt of a live run",
            retention::REFUSE_RUN_REMOVAL,
            |n| {
                n.receipts.retain(|r| r.sequence != 2);
            },
        ),
        ("receipt overwritten", retention::REFUSE_RUN_REMOVAL, |n| {
            *n.receipts.get_mut(1).unwrap() = audit(3, "run-b");
        }),
        (
            "receipt of a settled run without its history",
            retention::REFUSE_RUN_REMOVAL,
            |n| {
                let run = n.runs.remove("run-a").unwrap();
                n.receipts.retain(|r| r.sequence != 1);
                n.settled.push(cohort_of(run));
            },
        ),
    ]
}
fn expiry_cases() -> [Case; 5] {
    let shortened = retention::REFUSE_EXPIRY;
    [
        ("lowered expiry", shortened, |n| {
            n.retention
                .admission_expires_at_ms
                .insert("k".into(), 2_500);
        }),
        ("expiry deleted, item kept", shortened, |n| {
            n.retention.event_expires_at_ms.remove("ev");
        }),
        (
            "new expiry below the fallback, stored item",
            shortened,
            |n| {
                n.retention
                    .admission_expires_at_ms
                    .insert("fallback".into(), 1_999);
            },
        ),
        ("new expiry below the fallback, new item", shortened, |n| {
            n.keyed_admissions.insert("new".into(), receipt("new", 100));
            n.retention
                .admission_expires_at_ms
                .insert("new".into(), 2_099);
        }),
        ("clock", retention::REFUSE_CLOCK, |n| {
            n.retention.clock_ms = Some(499);
        }),
    ]
}
/// A licensed rebase commits on every full path (protected and plain
/// format 2, format 3 compare-and-swap) and through a format 3 transition
/// (on the complete path); after it the fences are closed again.
#[test]
fn a_clock_rebase_is_licensed_on_every_commit_path() {
    for (label, store) in commit_formats() {
        verify_full_rebase(&CommitPath {
            store: &store,
            label,
            operation: "rebase",
        });
    }
    verify_transition_rebase();
}
fn rebase_full(path: &CommitPath) -> (String, u64) {
    let (ns, base) = path.seed(&fence_state());
    let mut next = fence_state();
    rebased(&mut next);
    let revision = path.store.compare_and_swap(&ns, "w", base, &next).unwrap();
    (ns, revision)
}
fn verify_full_rebase(path: &CommitPath) {
    let (store, label) = (path.store, path.label);
    for (case, text, f) in after_rebase_refusal_cases() {
        let (ns, r) = rebase_full(path);
        let s = store.load(&ns).unwrap().state;
        assert_eq!(
            (
                s.retention.clock_ms,
                s.retention.stale_floor_ms,
                s.retention.admission_expires_at_ms.get("k").copied()
            ),
            (Some(100), Some(50), Some(2_100)),
            "{label}"
        );
        assert_eq!(
            s.retention.event_expires_at_ms.get("ev-long").copied(),
            Some(5_100),
            "{label}"
        );
        assert_eq!(history_rows(&ns), 1, "{label}: the rebase receipt");
        let mut later = s.clone();
        f(&mut later);
        let result = store.compare_and_swap(&ns, "w", r, &later);
        assert!(
            refused_with(result.clone(), text),
            "{label} {case}: {result:?}"
        );
        assert_eq!(store.load(&ns).unwrap().revision, r, "{label} {case}");
    }
    let (ns, r) = rebase_full(path);
    let mut later = store.load(&ns).unwrap().state;
    floor_lowered(&mut later);
    store.compare_and_swap(&ns, "w", r, &later).unwrap();
    assert_eq!(
        store.load(&ns).unwrap().state.retention.stale_floor_ms,
        Some(40),
        "{label}"
    );
}
fn verify_transition_rebase() {
    let store = support::records();
    let path = CommitPath {
        store: &store,
        label: "f3_rebase",
        operation: "rebase",
    };
    let mut s = fence_state();
    s.indexes.initialized = true;
    let (ns, _) = path.seed(&s);
    let transition = |f: &dyn Fn(&mut State)| path.transition(&ns, f);
    store.reset_transition_stats();
    transition(&rebased).unwrap();
    assert_eq!(store.transition_stats().heavy, 1);
    let r = store.load(&ns).unwrap().state.retention;
    assert_eq!(
        (
            r.clock_ms,
            r.stale_floor_ms,
            r.event_expires_at_ms.get("ev").copied()
        ),
        (Some(100), Some(50), Some(3_100))
    );
    for (case, text, f) in after_rebase_refusal_cases() {
        let result = transition(&f);
        assert!(refused_with(result.clone(), text), "{case}: {result:?}");
    }
    // The floor lowered with its own receipt, also on the complete path.
    transition(&floor_lowered).unwrap();
    assert_eq!(store.transition_stats().heavy, 2);
    assert_eq!(
        store.load(&ns).unwrap().state.retention.stale_floor_ms,
        Some(40)
    );
    // A rebase that lowers only the clock also takes the complete path.
    let path = CommitPath {
        label: "f3_rebase_clock_only",
        ..path
    };
    let (ns, _) = path.seed(&s);
    path.transition(&ns, &|s| {
        s.retention.clock_ms = Some(100);
        license(s, retention::CLOCK_REBASE, 500, 100);
    })
    .unwrap();
    assert_eq!(store.load(&ns).unwrap().state.retention.clock_ms, Some(100));
}
/// No removal without a history record or an expired window entry, and no
/// expiry moved earlier, on every full commit path. A refusal writes
/// nothing and writes no tombstone.
#[test]
fn removals_without_history_or_expiry_are_refused_on_every_full_commit_path() {
    for (label, store) in commit_formats() {
        let s = fence_state();
        let path = CommitPath {
            store: &store,
            label,
            operation: "fence",
        };
        let fresh = || path.seed(&s);
        let mut failures = vec![];
        for (case, text, f) in refusal_cases() {
            let (ns, base) = fresh();
            let mut next = s.clone();
            f(&mut next);
            let result = store.compare_and_swap(&ns, "w", base, &next);
            if !refused_with(result.clone(), text) {
                failures.push(format!("{case}: {result:?}"));
            } else {
                assert_eq!(store.load(&ns).unwrap().revision, base, "{label} {case}");
                assert_eq!(
                    (history_rows(&ns), tombstone_rows(&ns)),
                    (0, 0),
                    "{label} {case}"
                );
            }
        }
        assert!(failures.is_empty(), "{label}: {failures:#?}");
        // Allowed: both receipts and the event expired with their records,
        // the run with its history and its receipt. No tombstone row.
        let (ns, base) = fresh();
        let mut next = s.clone();
        next.retention.clock_ms = Some(4_000);
        next.keyed_admissions.clear();
        next.retention.admission_expires_at_ms.remove("k");
        next.events.remove("ev");
        next.retention.event_expires_at_ms.remove("ev");
        let run = next.runs.remove("run-a").unwrap();
        next.receipts.retain(|r| r.sequence != 1);
        let mut c = cohort_of(run);
        c.receipts.push(audit(1, "run-a"));
        next.settled.push(c);
        store.compare_and_swap(&ns, "w", base, &next).unwrap();
        assert_eq!(history_rows(&ns), 2, "{label}");
        assert_eq!(tombstone_rows(&ns), 0, "{label}");
    }
}
/// Format 3 bounded transitions refuse the same commits, incrementally or
/// on the complete path; nothing is written and no commit is counted. An
/// expired window entry leaves incrementally; an event tombstone's row
/// leaves with it.
#[test]
fn format_3_transitions_hold_the_removal_fences() {
    let store = support::records();
    let mut s = fence_state();
    s.indexes.initialized = true;
    s.retention
        .event_tombstones
        .insert("old".into(), tombstone("d"));
    let path = CommitPath {
        store: &store,
        label: "f3_fence",
        operation: "fence",
    };
    verify_fenced_transitions(&path, &s);
    verify_allowed_transitions(&path, &s);
}
fn verify_fenced_transitions(path: &CommitPath, s: &State) {
    let store = path.store;
    store.reset_transition_stats();
    let mut cases = refusal_cases();
    // Also when the commit is heavy for another reason.
    cases.push((
        "early removal, heavy",
        retention::REFUSE_EARLY_REMOVAL,
        |n| {
            n.execution_denied.insert("x".into());
            n.retention.clock_ms = Some(2_999);
            n.keyed_admissions.remove("k");
            n.retention.admission_expires_at_ms.remove("k");
        },
    ));
    // Refused by the bounded commit itself, before any complete path.
    let bounded = [
        "lowered expiry",
        "new expiry below the fallback, new item",
        "changed receipt",
        "clock",
    ];
    let mut failures = vec![];
    for (case, text, f) in &cases {
        let (ns, base) = path.seed(s);
        store.set_profiling(true);
        let result = path.transition(&ns, f);
        let complete_path = store
            .profiling_snapshot()
            .unwrap()
            .operations
            .contains_key("cas");
        store.set_profiling(false);
        if !refused_with(result.clone(), text) {
            failures.push(format!("{case}: {result:?}"));
        } else if bounded.contains(case) && complete_path {
            failures.push(format!("{case}: refused only on the complete path"));
        } else {
            assert_eq!(store.load(&ns).unwrap().revision, base, "{case}");
            assert_eq!(history_rows(&ns), 0, "{case}");
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    let stats = store.transition_stats();
    assert_eq!((stats.incremental, stats.heavy), (0, 0));
}
fn verify_allowed_transitions(path: &CommitPath, s: &State) {
    let store = path.store;
    let (ns, _) = path.seed(s);
    // The in-State tombstone "old" has its row; removing it is bounded and
    // takes the row with it.
    let mut c = support::client();
    let mut tx = c.transaction().unwrap();
    tx.query_one(
        "SELECT set_config('aq_playbooks.writer_namespace',$1,true),set_config('aq_playbooks.writer_format','3',true)",
        &[&ns],
    )
    .unwrap();
    tx.execute(
        "INSERT INTO aq_playbooks.disposal_tombstones(namespace,kind,identity,content_digest,disposed_at_ms) VALUES($1,'event','old','d',5)",
        &[&ns],
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(tombstone_rows(&ns), 1);
    path.transition(&ns, &|s| {
        s.retention.event_tombstones.remove("old");
    })
    .unwrap();
    assert_eq!(store.transition_stats().incremental, 1);
    assert_eq!(tombstone_rows(&ns), 0);
    // Expired: a receipt and an event leave in one bounded commit, and an
    // expiry moves later; no tombstone row is written.
    path.transition(&ns, &|s| {
        s.retention.clock_ms = Some(4_000);
        s.keyed_admissions.remove("k");
        s.retention.admission_expires_at_ms.remove("k");
        s.events.remove("ev");
        s.retention.event_expires_at_ms.remove("ev");
        s.keyed_admissions
            .insert("new".into(), receipt("new", 4_000));
        s.retention
            .admission_expires_at_ms
            .insert("new".into(), 6_000);
    })
    .unwrap();
    assert_eq!(store.transition_stats().incremental, 2);
    path.transition(&ns, &|s| {
        s.retention
            .admission_expires_at_ms
            .insert("new".into(), 7_000);
    })
    .unwrap();
    assert_eq!(store.transition_stats().incremental, 3);
    let state = store.load(&ns).unwrap().state;
    assert!(!state.keyed_admissions.contains_key("k") && !state.events.contains_key("ev"));
    assert_eq!(tombstone_rows(&ns), 0);
    verify_complete_removals(path, &ns);
}
fn verify_complete_removals(path: &CommitPath, ns: &str) {
    let store = path.store;
    // Without its expiry record in the patch, an expired receipt leaves
    // through the complete path.
    path.transition(ns, &|s| {
        s.keyed_admissions.remove("fallback");
    })
    .unwrap();
    assert_eq!(store.transition_stats().heavy, 1);
    // The run leaves with its history and its receipt, through the complete
    // path.
    path.transition(ns, &|s| {
        let run = s.runs.remove("run-a").unwrap();
        s.receipts.retain(|r| r.sequence != 1);
        let mut c = cohort_of(run);
        c.receipts.push(audit(1, "run-a"));
        s.settled.push(c);
    })
    .unwrap();
    assert_eq!(store.transition_stats().heavy, 2);
    assert!(!store.load(ns).unwrap().state.runs.contains_key("run-a"));
    assert_eq!(history_rows(ns), 2);
}
