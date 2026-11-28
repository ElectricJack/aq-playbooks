//! Retain referenced artifacts and prune their catalog history a bounded page at a time.
use super::*;

/// What one `collect_garbage` call did. The call is one ordinary commit
/// (the same bounded settle, drain, window drop and eviction every commit
/// does), plus a bounded page of the artifact phase; nothing requires
/// calling it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GcReport {
    /// Finished runs this commit moved to history (cohorts that settled,
    /// or that the drain of an older engine's namespace moved). A run never
    /// leaves without its history.
    pub runs: usize,
    /// Events forgotten by the window drop (their window passed).
    pub events: usize,
    /// Artifacts disposed of by the artifact phase.
    pub artifacts: usize,
    /// Receipts moved to history.
    pub receipts: usize,
    /// Artifacts the artifact phase kept because something still references
    /// them. (It counted kept runs and events too before spec 0003; those
    /// are no longer examined, so only artifacts count.)
    pub preserved: usize,
    /// More bounded work is due than this call's bounds allowed: settled
    /// cohorts, expired window entries, entries past `replay_keep`, the
    /// drain of an older engine's namespace, or the artifact phase. Call
    /// again (or let later commits do it).
    pub more: bool,
    /// Keyed admission receipts forgotten by the window drop.
    #[serde(default)]
    pub admissions: usize,
    /// Window entries evicted past `policy.replay_keep`.
    #[serde(default)]
    pub evicted: usize,
}
/// Whether anything keeps artifact `id` installed: a binding activation, a
/// deactivated binding (its last activation), a live run (its artifact and
/// dependency manifest), an accepted event's destination, the catalog
/// revision the current catalog, a live run or an accepted event pins, or the
/// rollback window ([`rollback_window`]); and, transitively, a dependency
/// of a kept artifact. Operations, waits and recovery requests belong to a
/// live run, so they keep its artifact. Catalog history kept only for audit
/// keeps no artifact.
pub fn artifact_referenced(s: &State, id: &str) -> bool {
    let mut pending = referenced_roots(s);
    let mut seen = BTreeSet::new();
    while let Some(hash) = pending.pop() {
        if hash == id {
            return true;
        }
        if !seen.insert(hash.clone()) {
            continue;
        }
        if let Some(artifact) = s.artifacts.get(&hash) {
            add_dependencies(s, artifact, &mut pending);
        }
    }
    false
}
/// Every artifact [`artifact_referenced`] keeps.
pub fn referenced_artifacts(s: &State) -> BTreeSet<String> {
    closure(s, referenced_roots(s))
}
fn closure(s: &State, mut pending: Vec<String>) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    while let Some(hash) = pending.pop() {
        if !seen.insert(hash.clone()) {
            continue;
        }
        if let Some(artifact) = s.artifacts.get(&hash) {
            add_dependencies(s, artifact, &mut pending);
        }
    }
    seen
}
fn referenced_roots(s: &State) -> Vec<String> {
    let mut pending = catalog_roots(s);
    let mut revisions = BTreeSet::new();
    for run in s.runs.values() {
        pending.push(run.artifact.clone());
        pending.extend(run.dependencies.values().cloned());
        revisions.insert(run.catalog_revision);
    }
    for event in s.events.values() {
        revisions.insert(event.catalog_revision);
        for destination in &event.destinations {
            pending.push(destination.artifact.clone());
            pending.extend(destination.dependencies.values().cloned());
        }
    }
    for revision in revisions {
        if let Some(bindings) = s.catalog_history.get(&revision) {
            pending.extend(bindings.values().map(|a| a.artifact.clone()));
        }
    }
    pending
}
/// The roots that need no scan of runs or events: the bindings, the current
/// catalog revision, the deactivated bindings and the rollback window.
fn catalog_roots(s: &State) -> Vec<String> {
    let mut pending = s
        .bindings
        .values()
        .map(|a| a.artifact.clone())
        .collect::<Vec<_>>();
    if let Some(bindings) = s.catalog_history.get(&s.catalog_revision) {
        pending.extend(bindings.values().map(|a| a.artifact.clone()));
    }
    pending.extend(
        last_activations(s)
            .into_values()
            .map(|(_, activation)| activation.artifact.clone()),
    );
    pending.extend(rollback_window(s));
    pending
}
/// For each deactivated binding, its last activation: the newest catalog
/// revision that still has it bound, with that activation.
fn last_activations(s: &State) -> BTreeMap<&str, (u64, &crate::Activation)> {
    s.deactivated_bindings
        .keys()
        .filter_map(|binding| {
            s.catalog_history
                .iter()
                .rev()
                .find_map(|(revision, bindings)| Some((*revision, bindings.get(binding)?)))
                .map(|found| (binding.as_str(), found))
        })
        .collect()
}
fn deactivated_revisions(s: &State) -> BTreeSet<u64> {
    last_activations(s)
        .into_values()
        .map(|(revision, _)| revision)
        .collect()
}
/// The rollback window: the newest `policy.artifact_keep` activated
/// artifacts of each source (`content.source.header` scope and id), by
/// their last activation (`retention.artifact_activations`). An artifact
/// activated before that order was recorded ranks below every recorded one,
/// by the newest catalog revision that bound it. An artifact never
/// activated is no rollback target.
fn rollback_window(s: &State) -> Vec<String> {
    newest_per_source(s, |rank| rank.activated)
}
/// The installs no activation has used yet that automatic disposal leaves
/// in place (an install is usually followed by its activation in a later
/// commit): of the never activated artifacts of each source, the newest
/// `policy.artifact_keep` by install order (`retention.artifact_installs`;
/// unrecorded ones rank lowest). `collect_garbage` does not keep them.
fn pending_installs(s: &State) -> Vec<String> {
    newest_per_source(s, |rank| match rank.activated {
        Some(_) => None,
        None => Some(rank.installed),
    })
}
struct Rank {
    activated: Option<(bool, u64)>,
    installed: (bool, u64),
}
fn newest_per_source(s: &State, rank: impl Fn(&Rank) -> Option<(bool, u64)>) -> Vec<String> {
    let keep = s.retention.policy.artifact_keep;
    let installs = &s.retention.artifact_installs;
    let activations = &s.retention.artifact_activations;
    let mut last_bound = BTreeMap::<&str, u64>::new();
    if s.artifacts.keys().any(|id| !activations.contains_key(id)) {
        for (revision, bindings) in &s.catalog_history {
            for activation in bindings.values() {
                last_bound.insert(&activation.artifact, *revision);
            }
        }
        for activation in s.bindings.values() {
            last_bound
                .entry(&activation.artifact)
                .or_insert(s.catalog_revision);
        }
    }
    let mut sources = BTreeMap::<(&str, &str), Vec<((bool, u64), &String)>>::new();
    for (id, artifact) in &s.artifacts {
        let ranked = Rank {
            activated: match activations.get(id) {
                Some(order) => Some((true, *order)),
                None => last_bound
                    .get(id.as_str())
                    .map(|revision| (false, *revision)),
            },
            installed: installs.get(id).map_or((false, 0), |order| (true, *order)),
        };
        let Some(key) = rank(&ranked) else {
            continue;
        };
        let header = &artifact.content.source.header;
        sources
            .entry((&header.scope, &header.id))
            .or_default()
            .push((key, id));
    }
    let mut kept = vec![];
    for mut ranked in sources.into_values() {
        ranked.sort_unstable_by(|a, b| b.cmp(a));
        kept.extend(ranked.into_iter().take(keep).map(|(_, id)| id.clone()));
    }
    kept
}
fn next_order(s: &State) -> u64 {
    let r = &s.retention;
    r.artifact_installs
        .values()
        .chain(r.artifact_activations.values())
        .max()
        .map_or(1, |n| n.saturating_add(1))
}
/// Record the install of artifact `id`, new in the State: the newest
/// install of its source.
pub(crate) fn record_install(s: &mut State, id: &str) {
    let next = next_order(s);
    s.retention.artifact_installs.insert(id.into(), next);
}
/// Record that `id` is now (again) a binding's artifact: the newest of its
/// source in the rollback window.
pub(crate) fn record_activation(s: &mut State, id: &str) {
    let next = next_order(s);
    s.retention.artifact_activations.insert(id.into(), next);
}
/// Dispose of the installed artifacts among `candidates` (every installed
/// artifact when `None`) that nothing references ([`artifact_referenced`]),
/// nor the automatic-disposal installs ([`pending_installs`]), at most
/// `policy.gc_page_limit` of them, recording each disposal at
/// `at_ms`; then prune the disposals past `policy.audit_ms` before `at_ms`.
/// A candidate the bindings, the current catalog or the rollback window
/// keep is decided without a scan of runs and events; only the others are
/// checked against the whole State. Returns how many were disposed of.
pub(crate) fn dispose_unreferenced(
    s: &mut State,
    candidates: Option<&BTreeSet<String>>,
    at_ms: i64,
) -> usize {
    if let Some(candidates) = candidates
        && candidates.iter().all(|id| {
            !s.artifacts.contains_key(id) || s.bindings.values().any(|a| a.artifact == *id)
        })
    {
        // The usual commit: its runs used a bound artifact.
        return 0;
    }
    let pending = pending_installs(s);
    let mut roots = catalog_roots(s);
    roots.extend(pending.iter().cloned());
    let kept = closure(s, roots);
    let open = match candidates {
        None => s
            .artifacts
            .keys()
            .filter(|id| !kept.contains(*id))
            .cloned()
            .collect::<Vec<_>>(),
        Some(candidates) => candidates
            .iter()
            .filter(|id| s.artifacts.contains_key(*id) && !kept.contains(*id))
            .cloned()
            .collect(),
    };
    if open.is_empty() {
        return 0;
    }
    let mut roots = referenced_roots(s);
    roots.extend(pending);
    let referenced = closure(s, roots);
    let most = s.retention.policy.gc_page_limit;
    let mut disposed = 0;
    for id in open {
        if disposed == most {
            break;
        }
        if referenced.contains(&id) {
            continue;
        }
        s.artifacts.remove(&id);
        s.retention.artifact_installs.remove(&id);
        s.retention.artifact_activations.remove(&id);
        s.retention.artifact_disposals.insert(id, at_ms);
        disposed += 1;
    }
    if disposed > 0 {
        let audit_ms = i64::try_from(s.retention.policy.audit_ms).unwrap_or(i64::MAX);
        let cutoff = at_ms.saturating_sub(audit_ms);
        s.retention.artifact_disposals.retain(|_, at| *at >= cutoff);
    }
    disposed
}
/// The artifacts the runs this commit settled, and the events it removed,
/// referenced: the candidates [`dispose_unreferenced`] judges at the end of
/// every commit. Empty (no scan) when nothing left.
pub(crate) fn released(s: &mut State) -> BTreeSet<String> {
    let mut released = std::mem::take(&mut s.released_artifacts);
    for run in s.settled.iter().flat_map(|cohort| &cohort.runs) {
        released.insert(run.artifact.clone());
        released.extend(run.dependencies.values().cloned());
    }
    released
}
fn add_dependencies(s: &State, artifact: &aq_playbooks_model::Artifact, pending: &mut Vec<String>) {
    for dependency in artifact.content.dependencies.values() {
        pending.push(dependency.artifact.clone());
        if let aq_playbooks_model::DependencySelection::Catalog { allowed_artifacts } =
            &dependency.selection
        {
            pending.extend(allowed_artifacts.iter().cloned());
            if let Some(binding) = s.bindings.get(&dependency.binding) {
                pending.push(binding.artifact.clone());
            }
        }
    }
}
pub fn verify_artifact_removals(
    previous: &State,
    next: &State,
) -> std::result::Result<(), crate::StorageError> {
    for (id, old) in &previous.artifacts {
        match next.artifacts.get(id) {
            Some(new) if new != old => {
                return Err(crate::StorageError::Backend(
                    "immutable artifact replaced".into(),
                ));
            }
            None if !next.retention.artifact_disposals.contains_key(id)
                || artifact_referenced(next, id) =>
            {
                return Err(crate::StorageError::Backend(
                    "artifact removal lacks safe disposal".into(),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}
/// The artifact phase of `collect_garbage`, run in its commit before the
/// commit's ordinary bounded work ([`finish_commit`]): dispose of up to
/// `limit` unreferenced artifacts (from the stored cursor on), drop catalog
/// history no live run, event or audit window needs, prune artifact
/// disposals past `audit_ms`, and move the GC clock to `now` (never back).
/// It removes no run, operation, receipt or event, and writes no tombstone,
/// correlation fence or disposal record.
pub(crate) fn gc(s: &mut State, now: i64, limit: usize) -> Result<GcReport> {
    let most = s.retention.policy.gc_page_limit;
    if limit == 0 || limit > most {
        return Err(Error::Refused(format!("GC page limit must be 1..{most}")));
    }
    if s.retention
        .last_gc_at_ms
        .is_some_and(|previous| now < previous)
    {
        return Err(Error::Refused("GC clock cannot move backwards".into()));
    }
    s.retention.last_gc_at_ms = Some(now);
    let audit_ms = s.retention.policy.audit_ms;
    let cutoff = now.saturating_sub(i64::try_from(audit_ms).unwrap_or(i64::MAX));
    let report = dispose_page(s, now, limit)?;
    prune_history(s, cutoff);
    Ok(report)
}
fn dispose_page(s: &mut State, now: i64, limit: usize) -> Result<GcReport> {
    let mut report = GcReport::default();
    // The cursor of an older engine's GC (its run and event phases come
    // before the artifact phase, 2) starts the artifact phase afresh.
    let mut cursor = artifact_cursor(s);
    let mut examined = 0;
    let mut finished = false;
    while examined < limit {
        let key = s
            .artifacts
            .range((
                std::ops::Bound::Excluded(cursor.clone()),
                std::ops::Bound::Unbounded,
            ))
            .next()
            .map(|(key, _)| key.clone());
        let Some(id) = key else {
            finished = true;
            break;
        };
        examined += 1;
        cursor = id.clone();
        if artifact_referenced(s, &id) {
            report.preserved += 1;
            continue;
        }
        s.artifacts.remove(&id);
        s.retention.artifact_installs.remove(&id);
        s.retention.artifact_activations.remove(&id);
        s.retention.artifact_disposals.insert(id, now);
        report.artifacts += 1;
    }
    report.more = !finished;
    s.retention.gc_cursor = if report.more {
        Some(
            serde_json::to_string(&(GC_ARTIFACTS, cursor, false))
                .map_err(|e| Error::Refused(e.to_string()))?,
        )
    } else {
        None
    };
    Ok(report)
}
fn artifact_cursor(s: &State) -> String {
    s.retention
        .gc_cursor
        .as_deref()
        .and_then(|v| serde_json::from_str::<(u8, String, bool)>(v).ok())
        .filter(|(phase, ..)| *phase == GC_ARTIFACTS)
        .map(|(_, cursor, _)| cursor)
        .unwrap_or_default()
}
fn prune_history(s: &mut State, cutoff: i64) {
    let mut revisions = s
        .runs
        .values()
        .map(|r| r.catalog_revision)
        .chain(s.events.values().map(|e| e.catalog_revision))
        .collect::<BTreeSet<_>>();
    revisions.insert(s.catalog_revision);
    revisions.extend(deactivated_revisions(s));
    revisions.extend(
        s.catalog_history_at_ms
            .iter()
            .filter(|(_, at)| **at >= cutoff)
            .map(|(revision, _)| *revision),
    );
    s.catalog_history
        .retain(|revision, _| revisions.contains(revision));
    s.catalog_history_at_ms
        .retain(|revision, _| revisions.contains(revision));
    s.retention.artifact_disposals.retain(|_, at| *at >= cutoff);
}
/// The artifact phase's number in the stored GC cursor (an older engine's
/// phases 0 and 1 were its run and event removals).
const GC_ARTIFACTS: u8 = 2;
