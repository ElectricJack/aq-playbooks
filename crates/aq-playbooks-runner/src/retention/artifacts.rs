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
fn referenced_roots(s: &State) -> Vec<String> {
    let mut pending = s
        .bindings
        .values()
        .map(|a| a.artifact.clone())
        .collect::<Vec<_>>();
    for bindings in s.catalog_history.values() {
        pending.extend(bindings.values().map(|a| a.artifact.clone()));
    }
    for run in s.runs.values() {
        pending.push(run.artifact.clone());
        pending.extend(run.dependencies.values().cloned());
    }
    for destination in s.events.values().flat_map(|event| &event.destinations) {
        pending.push(destination.artifact.clone());
        pending.extend(destination.dependencies.values().cloned());
    }
    pending
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
