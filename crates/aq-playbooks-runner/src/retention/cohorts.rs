//! Move settled runs, intents and receipts from live state into cohort history.
use super::*;

/// The roots this commit touched: the roots of its dirty runs and operations.
/// Call before `indexes::sync`, which clears the dirty sets. O(dirty).
pub(crate) fn candidate_roots(s: &State) -> BTreeSet<String> {
    let run_root = |run: &str| {
        s.runs
            .get(run)
            .map(|r| r.root.clone())
            .or_else(|| s.indexes.run_roots.get(run).cloned())
    };
    let mut roots = BTreeSet::new();
    for id in &s.indexes.dirty_runs {
        roots.extend(run_root(id));
    }
    for id in &s.indexes.dirty_intents {
        let root = s
            .intents
            .get(id)
            .and_then(|i| run_root(&i.run))
            .or_else(|| s.indexes.intent_roots.get(id).cloned());
        roots.extend(root);
    }
    roots
}
/// Whether the cohort of `root` is settled: it has members, none of them is
/// active and none of its operations is unsettled (the root indexes after
/// `indexes::sync`). O(1) index lookups.
pub(super) fn cohort_settled(s: &State, root: &str) -> bool {
    let nonempty = |ids: Option<&BTreeSet<String>>| ids.is_some_and(|ids| !ids.is_empty());
    nonempty(s.indexes.root_members.get(root))
        && !nonempty(s.indexes.root_active.get(root))
        && !nonempty(s.indexes.root_effects.get(root))
}
/// Remove every settled cohort among `candidates` (at most `limit`) from the
/// State and hand each to storage as history (`State::settled`). A settled
/// candidate beyond `limit` sets `report.more` and re-arms the drain's
/// cohort scan ([`drain_step`]), which finds it in a later commit, so no
/// settled cohort is stranded.
pub(crate) fn settle(
    s: &mut State,
    candidates: BTreeSet<String>,
    limit: usize,
    report: &mut CommitReport,
) -> Result<()> {
    let mut roots = vec![];
    for root in candidates {
        if !cohort_settled(s, &root) {
            continue;
        }
        if roots.len() >= limit {
            report.more = true;
            let drain = &mut s.retention.drain;
            drain.done = false;
            drain.roots_done = false;
            drain.root_cursor = None;
            break;
        }
        roots.push(root);
    }
    take_cohorts(s, &roots, report)
}
/// [`remove_cohorts`] for `roots`, handing each to storage as history and
/// counting it in `report`.
pub(super) fn take_cohorts(
    s: &mut State,
    roots: &[String],
    report: &mut CommitReport,
) -> Result<()> {
    for cohort in remove_cohorts(s, roots)? {
        report.cohorts += 1;
        report.runs += cohort.runs.len();
        report.receipts += cohort.receipts.len();
        s.settled.push(cohort);
    }
    Ok(())
}
/// Take the receipts at `offsets` (positions in `s.receipts`) out of the
/// State, compact the positional sequence and rebuild the receipt offset
/// indexes. O(live receipts).
pub(super) fn take_receipts(s: &mut State, offsets: &BTreeSet<usize>) -> Vec<Receipt> {
    if offsets.is_empty() {
        return vec![];
    }
    let taken: Vec<Receipt> = offsets
        .iter()
        .filter_map(|&offset| s.receipts.get(offset).cloned())
        .collect();
    let mut position = 0;
    s.receipts.retain(|_| {
        let keep = !offsets.contains(&position);
        position += 1;
        keep
    });
    s.indexes.receipts.clear();
    s.indexes.all_receipts.clear();
    let live: Vec<(String, u64)> = s
        .receipts
        .iter()
        .map(|r| (r.run.clone(), r.sequence))
        .collect();
    for (offset, (run, sequence)) in live.into_iter().enumerate() {
        s.indexes
            .receipts
            .entry(run)
            .or_default()
            .insert(sequence, offset);
        s.indexes.all_receipts.insert(sequence, offset);
    }
    taken
}
/// Offsets of the receipts owned by `run`, checked against the receipt.
pub(super) fn receipt_offsets(s: &State, run: &str) -> Vec<usize> {
    s.indexes
        .receipts
        .get(run)
        .into_iter()
        .flat_map(|offsets| offsets.values().copied())
        .filter(|&offset| s.receipts.get(offset).is_some_and(|r| r.run == run))
        .collect()
}
/// Remove the cohorts of `roots` from the State: their runs, operations,
/// their recovery requests, waits, receipts and budgets. Their runs and
/// operations are marked dirty, so the next `indexes::sync` drops their
/// index entries. O(cohorts), plus one compaction of the live receipts and
/// one pass over the live waits for all of them together.
pub(crate) fn remove_cohorts(s: &mut State, roots: &[String]) -> Result<Vec<SettledCohort>> {
    if roots.is_empty() {
        return Ok(vec![]);
    }
    let selection = CohortSelection::collect(s, roots);
    selection.mark_subscriptions(s);
    let mut cohorts = selection.take_members(s);
    selection.attach_receipts(s, &mut cohorts);
    Ok(cohorts)
}

struct Taken {
    root: String,
    members: Vec<String>,
    intents: Vec<String>,
    finished_at_ms: Option<i64>,
}

#[derive(Default)]
struct CohortSelection {
    taken: Vec<Taken>,
    offsets: BTreeSet<usize>,
    owner: BTreeMap<String, usize>,
}

impl CohortSelection {
    fn collect(s: &State, roots: &[String]) -> Self {
        let mut selection = Self::default();
        for root in roots {
            selection.include(s, root);
        }
        selection
    }
    fn include(&mut self, s: &State, root: &str) {
        let members: Vec<String> = s
            .indexes
            .root_members
            .get(root)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default();
        let intents: Vec<String> = s
            .indexes
            .root_intents
            .get(root)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default();
        let mut finished_at_ms = None;
        for id in &members {
            self.offsets.extend(receipt_offsets(s, id));
            self.owner.insert(id.clone(), self.taken.len());
            let terminal = s
                .runs
                .get(id)
                .and_then(|r| r.terminal_sequence)
                .and_then(|sequence| s.indexes.all_receipts.get(&sequence).copied())
                .and_then(|offset| s.receipts.get(offset));
            if let Some(receipt) = terminal {
                finished_at_ms = finished_at_ms.max(Some(receipt.at_ms));
            }
        }
        self.taken.push(Taken {
            root: root.to_owned(),
            members,
            intents,
            finished_at_ms,
        });
    }

    fn mark_subscriptions(&self, s: &mut State) {
        // Their waits end with them: the events those waits held are judged
        // again (`rejoin`) before the next index sync drops the subscriptions.
        for t in &self.taken {
            for token in t
                .members
                .iter()
                .cloned()
                .chain(t.intents.iter().map(|id| format!("intent:{id}")))
            {
                if let Some(keys) = s.indexes.run_subscriptions.get(&token) {
                    let keys = keys.clone();
                    s.replay_touched.wait_keys.extend(keys);
                }
            }
        }
    }
    fn take_members(&self, s: &mut State) -> Vec<SettledCohort> {
        let mut all_intents = BTreeSet::new();
        let mut all_members = BTreeSet::new();
        let cohorts = self
            .taken
            .iter()
            .map(|taken| taken.detach(s, &mut all_members, &mut all_intents))
            .collect();
        if !all_intents.is_empty() && !s.recovery_requests.is_empty() {
            s.recovery_requests
                .retain(|_, request| !all_intents.contains(&request.intent));
        }
        if !s.waits.is_empty() {
            s.waits.retain(|_, wait| !all_members.contains(&wait.run));
        }
        cohorts
    }
    fn attach_receipts(&self, s: &mut State, cohorts: &mut [SettledCohort]) {
        for receipt in take_receipts(s, &self.offsets) {
            if let Some(&i) = self.owner.get(&receipt.run) {
                cohorts[i].receipts.push(receipt);
            }
        }
        for (cohort, t) in cohorts.iter_mut().zip(&self.taken) {
            cohort.finished_at_ms = t
                .finished_at_ms
                .or_else(|| cohort.receipts.iter().map(|r| r.at_ms).max())
                .or(s.retention.clock_ms)
                .unwrap_or(0);
        }
    }
}
impl Taken {
    fn detach(
        &self,
        s: &mut State,
        all_members: &mut BTreeSet<String>,
        all_intents: &mut BTreeSet<String>,
    ) -> SettledCohort {
        let mut cohort = SettledCohort {
            root: self.root.clone(),
            finished_at_ms: 0,
            runs: vec![],
            intents: vec![],
            receipts: vec![],
        };
        for id in &self.members {
            cohort.runs.extend(s.runs.remove(id));
            crate::indexes::mark_run(s, id);
            all_members.insert(id.clone());
        }
        for id in &self.intents {
            cohort.intents.extend(s.intents.remove(id));
            crate::indexes::mark_intent(s, id);
            all_intents.insert(id.clone());
        }
        s.budgets.remove(&self.root);
        cohort
    }
}
/// Move the receipts owned by `owner`, which belongs to no cohort (`"admin"`,
/// `"event:<key>"`), out of the State as a cohort of root `owner` with no
/// runs. `None` when it owns none. O(its receipts) plus compaction.
pub(crate) fn move_unowned_receipts(s: &mut State, owner: &str) -> Result<Option<SettledCohort>> {
    let offsets: BTreeSet<usize> = receipt_offsets(s, owner).into_iter().collect();
    if offsets.is_empty() {
        return Ok(None);
    }
    let receipts = take_receipts(s, &offsets);
    let finished_at_ms = receipts
        .iter()
        .map(|r| r.at_ms)
        .max()
        .or(s.retention.clock_ms)
        .unwrap_or(0);
    Ok(Some(SettledCohort {
        root: owner.into(),
        finished_at_ms,
        runs: vec![],
        intents: vec![],
        receipts,
    }))
}
/// Move the `"admin"` receipts this commit wrote (sequence after `started`,
/// the sequence it began at) to history. Older ones were written by an
/// older engine and leave through the drain, a bounded number per commit.
pub(super) fn move_admin_receipts(
    s: &mut State,
    started: u64,
    report: &mut CommitReport,
) -> Result<()> {
    let offsets: BTreeSet<usize> = s
        .indexes
        .receipts
        .get("admin")
        .into_iter()
        .flat_map(|offsets| {
            offsets
                .range((
                    std::ops::Bound::Excluded(started),
                    std::ops::Bound::Unbounded,
                ))
                .map(|(_, offset)| *offset)
        })
        .filter(|&offset| s.receipts.get(offset).is_some_and(|r| r.run == "admin"))
        .collect();
    if offsets.is_empty() {
        return Ok(());
    }
    let receipts = take_receipts(s, &offsets);
    report.receipts += receipts.len();
    let finished_at_ms = receipts
        .iter()
        .map(|r| r.at_ms)
        .max()
        .or(s.retention.clock_ms)
        .unwrap_or(0);
    s.settled.push(SettledCohort {
        root: "admin".into(),
        finished_at_ms,
        runs: vec![],
        intents: vec![],
        receipts,
    });
    Ok(())
}
