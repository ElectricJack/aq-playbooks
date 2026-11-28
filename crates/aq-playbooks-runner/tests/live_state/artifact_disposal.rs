use super::*;

/// Revision `k` of one document (one source): the waiter playbook with a
/// different purpose, so a different digest.
fn revision(k: usize) -> Artifact {
    let mut content = wait_artifact().content;
    content.playbook.purpose = format!("saved revision {k}");
    Artifact::seal(content).unwrap()
}

/// The owner saves one document 24 times (install and activate a new
/// revision for one binding each time), and every sixth revision a run
/// starts on it and waits. The stored artifacts stay at the rollback bound
/// (`artifact_keep`) plus the older revisions a live run still uses; an
/// artifact a live run or the rollback window references is never
/// disposed of, and once those runs finish their artifacts leave in the
/// commit that settles them, back to the bound. The same on the snapshot
/// and the record backend.
fn saves_stay_at_the_rollback_bound<S: Storage>(store: S) {
    const KEEP: usize = 3;
    let e = Engine::new(store, "test", "i").unwrap();
    e.set_retention_policy(RetentionPolicy {
        artifact_keep: KEEP,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let mut saved = vec![];
    let mut live = vec![];
    for k in 0..24 {
        let a = revision(k);
        let digest = a.digest.clone();
        e.install(a).unwrap();
        e.activate("waiter", &digest, authority()).unwrap();
        saved.push(digest.clone());
        if k % 6 == 0 {
            let run = e
                .admit("waiter", "main", Value::Null, authority(), 0)
                .unwrap();
            e.advance(&run, 0).unwrap();
            live.push((run, digest));
        }
        let s = e.snapshot().unwrap();
        let window = &saved[saved.len().saturating_sub(KEEP)..];
        for digest in window {
            assert!(
                s.artifacts.contains_key(digest),
                "rollback window, save {k}"
            );
        }
        for (run, digest) in &live {
            assert!(!s.runs[run].status.terminal());
            assert!(s.artifacts.contains_key(digest), "live run, save {k}");
        }
        let held = live.iter().filter(|(_, d)| !window.contains(d)).count();
        assert_eq!(s.artifacts.len(), window.len() + held, "save {k}");
    }
    // The waits time out: each run finishes, and the commit that settles it
    // disposes of its revision (outside the rollback window).
    for (n, (run, digest)) in live.iter().enumerate() {
        let at = 10_000 + 10 * n as i64;
        // The timeout, then the error return.
        e.advance(run, at).unwrap();
        assert!(e.snapshot().unwrap().artifacts.contains_key(digest));
        e.advance(run, at + 1).unwrap();
        let s = e.snapshot().unwrap();
        assert!(!s.runs.contains_key(run), "the run left with its history");
        assert!(!s.artifacts.contains_key(digest));
        assert!(s.retention.artifact_disposals.contains_key(digest));
        assert_eq!(s.artifacts.len(), KEEP + live.len() - n - 1);
    }
    let s = e.snapshot().unwrap();
    assert_eq!(s.artifacts.len(), KEEP);
    assert_eq!(
        s.artifacts.keys().cloned().collect::<BTreeSet<_>>(),
        saved[saved.len() - KEEP..].iter().cloned().collect()
    );
    // A rollback to the oldest revision of the window works, and keeps the
    // count at the bound.
    let back = &saved[saved.len() - KEEP];
    e.activate("waiter", back, authority()).unwrap();
    let run = e
        .admit("waiter", "main", Value::Null, authority(), 20_000)
        .unwrap();
    e.advance(&run, 20_000).unwrap();
    let s = e.snapshot().unwrap();
    assert_eq!(s.runs[&run].artifact, *back);
    assert_eq!(s.artifacts.len(), KEEP);
}

#[test]
fn saving_many_revisions_keeps_the_stored_artifacts_at_the_rollback_bound() {
    saves_stay_at_the_rollback_bound(MemoryStorage::default());
    saves_stay_at_the_rollback_bound(RecordStorage::new());
}

/// A deactivated binding keeps its last artifact, and an installed
/// artifact no activation has used yet is not disposed of automatically
/// (an install is followed by its activation in a later commit); only the
/// rollback window and references decide. `artifact_keep` must be positive.
#[test]
fn deactivated_bindings_and_pending_installs_keep_their_artifacts() {
    let e = engine();
    e.set_retention_policy(RetentionPolicy {
        artifact_keep: 1,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let first = revision(0);
    let parked = first.digest.clone();
    install(&e, first, "parked");
    let parked_revision = e.snapshot().unwrap().bindings["parked"].revision;
    assert!(e.deactivate_binding("parked", parked_revision, 1).unwrap());
    // A newer revision of the same source is active elsewhere: the window
    // (1) holds it, not the parked one.
    let active = install(&e, revision(1), "live");
    // A newer one still, installed but not activated.
    let pending = revision(2);
    let pending_digest = pending.digest.clone();
    e.install(pending).unwrap();
    // Another binding's saves trigger disposal passes.
    for k in 0..3 {
        let mut content = done_artifact().content;
        content.playbook.purpose = format!("other {k}");
        install(&e, Artifact::seal(content).unwrap(), "other");
    }
    let s = e.snapshot().unwrap();
    assert!(s.artifacts.contains_key(&parked), "deactivated binding");
    assert!(s.artifacts.contains_key(&active), "active binding");
    assert!(s.artifacts.contains_key(&pending_digest), "pending install");
    // `other` keeps only its active revision.
    let others = s
        .artifacts
        .values()
        .filter(|a| a.content.source.header.id == "a")
        .count();
    assert_eq!(others, 1);
    // Once the pending revision is activated, the parked binding's
    // artifact is still kept for it.
    e.activate("live", &pending_digest, authority()).unwrap();
    let s = e.snapshot().unwrap();
    assert!(s.artifacts.contains_key(&parked));
    assert!(!s.artifacts.contains_key(&active), "left the window");
    assert!(matches!(
        e.set_retention_policy(RetentionPolicy {
            artifact_keep: 0,
            ..RetentionPolicy::default()
        }),
        Err(Error::Refused(m)) if m == "artifact_keep must be positive"
    ));
}
