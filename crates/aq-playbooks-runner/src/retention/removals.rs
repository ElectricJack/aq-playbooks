//! Validate replay records and the settled history required for removals.
use super::*;

/// The one removal check every backend runs on a commit from `previous` to
/// `next` (see the module docs). Each record of `previous` missing from
/// `next` must be allowed:
///
/// - a run: its history is in `next.settled`, it is terminal, its children
///   are terminal and its operations settled;
/// - an operation: its own history and its run's are in `next.settled` and it
///   is settled;
/// - an audit receipt: its history is in `next.settled`;
/// - an event: its expiry has been reached, or it is evicted as
///   [`check_evictions`] licenses;
/// - a keyed admission receipt: its expiry has been reached, or it is
///   evicted as [`check_evictions`] licenses (a changed receipt is always
///   refused).
///
/// The clock never decreases, the stale floor never changes, and expiry
/// records follow [`check_expiry_change`], except in a commit that carries
/// an admin licence ([`licence`]), which must be exactly that licence and do
/// nothing else (see [`check_licence`]): a clock rebase lowers the clock and
/// caps expiries to [`rebase_cap`]; a floor lowering lowers the floor.
/// Legacy event tombstones, correlation fences and disposal records (which
/// no engine writes any more; the drain removes them) may always be removed
/// and license nothing. Artifact removals are checked by
/// [`verify_artifact_removals`].
pub fn verify_removals(previous: &State, next: &State) -> std::result::Result<(), StorageError> {
    if licence(next)?.is_some() {
        let added = next.runs.keys().any(|k| !previous.runs.contains_key(k))
            || next
                .intents
                .keys()
                .any(|k| !previous.intents.contains_key(k))
            || next.events.keys().any(|k| !previous.events.contains_key(k))
            || next
                .keyed_admissions
                .keys()
                .any(|k| !previous.keyed_admissions.contains_key(k));
        if added {
            return Err(refuse(REFUSE_LICENCE));
        }
    }
    let kept: BTreeSet<u64> = next.receipts.iter().map(|r| r.sequence).collect();
    let stored = &previous.retention.replay_order;
    let evicted = check_evictions(
        previous,
        next,
        &mut next
            .retention
            .replay_order
            .keys()
            .filter(|key| !stored.contains_key(*key))
            .cloned(),
        &Stored {
            ordered: &|key| Ok(stored.contains_key(key)),
            memberships: &|key| Ok(previous.indexes.event_memberships.get(key).cloned()),
            waiters: &|key| Ok(previous.indexes.waiters.get(key).cloned()),
        },
    )?;
    check_removals(
        previous,
        next,
        &|sequence| kept.contains(&sequence),
        &|window, key| evicted.contains(&(window, key.to_string())),
    )?;
    check_expiry_records(previous, next)
}
/// [`verify_removals`] without the expiry records, with the receipts `next`
/// keeps given by `receipt_kept`. `previous` need only hold the removed records, the
/// operations of removed runs and the children of removed runs (and its
/// header fields), so a record backend can check a patch.
pub(crate) fn check_removals(
    previous: &State,
    next: &State,
    receipt_kept: &dyn Fn(u64) -> bool,
    evicted: &dyn Fn(Window, &str) -> bool,
) -> std::result::Result<(), StorageError> {
    verify_artifact_removals(previous, next)?;
    let licence = licence(next)?;
    check_licence(previous, next, licence)?;
    let records = RemovedRecords {
        previous,
        next,
        licence,
    };
    records.admissions(evicted)?;
    records.events(evicted)?;
    records.receipts(receipt_kept)?;
    records.cohorts()
}

type Checked = std::result::Result<(), StorageError>;
struct RemovedRecords<'a> {
    previous: &'a State,
    next: &'a State,
    licence: Option<Licence>,
}
impl RemovedRecords<'_> {
    fn allow_removal(&self) -> Checked {
        if self.licence.is_some() {
            return Err(refuse(REFUSE_LICENCE));
        }
        Ok(())
    }
    fn admissions(&self, evicted: &dyn Fn(Window, &str) -> bool) -> Checked {
        let (previous, next) = (self.previous, self.next);
        let clock = next.retention.clock_ms;
        for (key, receipt) in &previous.keyed_admissions {
            match next.keyed_admissions.get(key) {
                Some(kept) if kept == receipt => {}
                Some(_) => return Err(refuse(REFUSE_RECEIPT_CHANGE)),
                None if admission_expiry(previous, key).is_some_and(|at| expired(at, clock))
                    || evicted(Window::Admission, key) =>
                {
                    self.allow_removal()?
                }
                None => return Err(refuse(REFUSE_EARLY_REMOVAL)),
            }
        }
        Ok(())
    }
    fn events(&self, evicted: &dyn Fn(Window, &str) -> bool) -> Checked {
        let (previous, next) = (self.previous, self.next);
        let clock = next.retention.clock_ms;
        for (key, event) in &previous.events {
            if let Some(kept) = next.events.get(key) {
                check_event_change(event, kept)?;
                continue;
            }
            self.allow_removal()?;
            if !event_expiry(previous, key).is_some_and(|at| expired(at, clock))
                && !evicted(Window::Event, key)
            {
                return Err(refuse(REFUSE_EARLY_REMOVAL));
            }
        }
        Ok(())
    }
    fn receipts(&self, receipt_kept: &dyn Fn(u64) -> bool) -> Checked {
        let (previous, next) = (self.previous, self.next);
        let removed_receipts: Vec<&crate::Receipt> = previous
            .receipts
            .iter()
            .filter(|r| !receipt_kept(r.sequence))
            .collect();
        if !removed_receipts.is_empty() {
            self.allow_removal()?;
            let settled: BTreeSet<u64> = next
                .settled
                .iter()
                .flat_map(|c| &c.receipts)
                .map(|r| r.sequence)
                .collect();
            for receipt in removed_receipts {
                if !settled.contains(&receipt.sequence) {
                    return Err(refuse(REFUSE_RUN_REMOVAL));
                }
            }
        }
        Ok(())
    }
    fn cohorts(&self) -> Checked {
        let (previous, next) = (self.previous, self.next);
        let removed_runs: Vec<(&String, &Run)> = previous
            .runs
            .iter()
            .filter(|(id, _)| !next.runs.contains_key(*id))
            .collect();
        let removed_intents: Vec<(&String, &Intent)> = previous
            .intents
            .iter()
            .filter(|(id, _)| !next.intents.contains_key(*id))
            .collect();
        if removed_runs.is_empty() && removed_intents.is_empty() {
            return Ok(());
        }
        self.allow_removal()?;
        let history = SettledHistory::load(previous, next, !removed_runs.is_empty())?;
        history.check_runs(removed_runs)?;
        history.check_intents(removed_intents)
    }
}

struct SettledHistory<'a> {
    next: &'a State,
    runs: BTreeMap<&'a str, &'a Run>,
    intents: BTreeMap<&'a str, &'a Intent>,
    intents_of: BTreeMap<&'a str, Vec<&'a Intent>>,
}
impl<'a> SettledHistory<'a> {
    fn load(
        previous: &'a State,
        next: &'a State,
        include_run_intents: bool,
    ) -> std::result::Result<Self, StorageError> {
        let settled_runs = Self::runs_in_history(next)?;
        let settled_intents: BTreeMap<&str, &Intent> = next
            .settled
            .iter()
            .flat_map(|c| &c.intents)
            .map(|i| (i.id.as_str(), i))
            .collect();
        let mut intents_of: BTreeMap<&str, Vec<&Intent>> = BTreeMap::new();
        if include_run_intents {
            for intent in previous
                .intents
                .values()
                .chain(settled_intents.values().copied())
            {
                intents_of
                    .entry(intent.run.as_str())
                    .or_default()
                    .push(intent);
            }
        }
        Ok(Self {
            next,
            runs: settled_runs,
            intents: settled_intents,
            intents_of,
        })
    }
    fn runs_in_history(
        next: &'a State,
    ) -> std::result::Result<BTreeMap<&'a str, &'a Run>, StorageError> {
        // A cohort's runs belong to its root.
        let mut settled_runs: BTreeMap<&str, &Run> = BTreeMap::new();
        for cohort in &next.settled {
            for run in &cohort.runs {
                if run.root != cohort.root {
                    return Err(refuse(REFUSE_RUN_OBLIGATIONS));
                }
                settled_runs.insert(run.id.as_str(), run);
            }
        }
        Ok(settled_runs)
    }
    fn check_runs(&self, removed: Vec<(&String, &Run)>) -> Checked {
        for (id, _) in removed {
            let Some(settled) = self.runs.get(id.as_str()) else {
                return Err(refuse(REFUSE_RUN_REMOVAL));
            };
            if self.unresolved(id, settled) {
                return Err(refuse(REFUSE_RUN_OBLIGATIONS));
            }
        }
        Ok(())
    }
    fn unresolved(&self, id: &str, settled: &Run) -> bool {
        // Each run and operation as this commit leaves it.
        let terminal = |child: &String| {
            self.runs
                .get(child.as_str())
                .copied()
                .or_else(|| self.next.runs.get(child))
                .is_none_or(|r| r.status.terminal())
        };
        // An operation of a run that leaves goes with it: one still
        // live would be stranded (its run's history is written).
        let settled_intent = |intent: &&Intent| {
            !self.next.intents.contains_key(&intent.id)
                && intent_settled(
                    &self
                        .intents
                        .get(intent.id.as_str())
                        .copied()
                        .unwrap_or(intent)
                        .status,
                )
        };
        !settled.status.terminal()
            || !settled.children.iter().all(terminal)
            || !self
                .intents_of
                .get(id)
                .into_iter()
                .flatten()
                .all(settled_intent)
    }
    fn check_intents(&self, removed: Vec<(&String, &Intent)>) -> Checked {
        for (id, intent) in removed {
            let Some(settled) = self
                .intents
                .get(id.as_str())
                .filter(|_| self.runs.contains_key(intent.run.as_str()))
            else {
                return Err(refuse(REFUSE_RUN_REMOVAL));
            };
            if !intent_settled(&settled.status) {
                return Err(refuse(REFUSE_EFFECT));
            }
        }
        Ok(())
    }
}
