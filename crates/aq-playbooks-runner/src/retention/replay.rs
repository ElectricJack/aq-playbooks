//! Re-evaluate held replay entries when their referencing work changes.
use super::*;
/// The window entries a commit may have changed the hold of, for
/// [`rejoin`]: events it accepted or fanned out, and the correlation keys
/// whose waiters it changed. Never stored.
#[derive(Debug, Clone, Default)]
pub struct ReplayTouched {
    pub events: BTreeSet<String>,
    pub wait_keys: BTreeSet<String>,
    /// Contract and scope keys whose uncertain completions changed, each
    /// with the earliest dispatch sequence concerned: only events accepted
    /// after it are judged again.
    pub contract_keys: BTreeMap<String, u64>,
}
/// Put the window entries whose hold this commit may have ended or begun
/// in or out of the eviction order: a keyed admission receipt joins it when
/// its run leaves the record (in this commit's history); an event is judged
/// again ([`event_held`]) when the commit accepted it, fanned it out, or
/// changed the waiters of one of its correlation keys or the uncertain
/// operations of its contract and scope. O(touched).
///
/// A hold through an uncertain completion (an operation whose admission
/// handle is lost) begins or ends when the uncertain operations of a
/// contract and scope change. Such an operation holds only events accepted
/// after its dispatch, so only those are judged again (from
/// `indexes.contract_events` by sequence): a dispatch judges none, the
/// binding of its handle those that arrived during the host call.
pub(crate) fn rejoin(s: &mut State, report: &mut CommitReport) -> Result<()> {
    rejoin_admissions(s);
    let touched = std::mem::take(&mut s.replay_touched);
    let mut events = touched.events;
    for key in touched.wait_keys {
        if let Some(ids) = s.indexes.events.get(&key) {
            events.extend(ids.values().cloned());
        }
    }
    // A change of a contract's uncertain operations queues its events after
    // the earliest dispatch concerned; at most `window_drops_per_commit` of
    // the queued events are judged per commit, the rest in later commits
    // (an unjudged event that has become held is taken out of the order by
    // `evict` rather than evicted; one that has become unheld waits outside
    // it, or leaves by its window). Events an older engine accepted that the
    // drain has not judged yet are left to it.
    for (key, after) in touched.contract_keys {
        let entry = s.retention.rejudge.entry(key).or_insert(after);
        *entry = (*entry).min(after);
    }
    collect_contract_events(s, &mut events);
    if !s.retention.rejudge.is_empty() {
        report.more = true;
    }
    for key in events {
        let Some(order) = s.events.get(&key).map(event_order_key) else {
            continue;
        };
        sync_event_order(s, &key, order)?;
    }
    Ok(())
}
fn rejoin_admissions(s: &mut State) {
    let runs: Vec<String> = s
        .settled
        .iter()
        .flat_map(|cohort| cohort.runs.iter().map(|run| run.id.clone()))
        .collect();
    for run in runs {
        let Some(key) = s.retention.replay_held.remove(&run) else {
            continue;
        };
        if let Some(receipt) = s.keyed_admissions.get(&key)
            && receipt.run_id == run
        {
            let order = admission_order_key(receipt);
            s.retention.replay_order.insert(order, ());
        }
    }
}

fn collect_contract_events(s: &mut State, events: &mut BTreeSet<String>) {
    let mut budget = s.retention.policy.window_drops_per_commit;
    let drain = &s.retention.drain;
    let mut progress = vec![];
    for (key, after) in &s.retention.rejudge {
        if budget == 0 {
            break;
        }
        let batch: Vec<(u64, String)> = s
            .indexes
            .contract_events
            .get(key)
            .into_iter()
            .flat_map(|ids| {
                ids.range((
                    std::ops::Bound::Excluded(*after),
                    std::ops::Bound::Unbounded,
                ))
            })
            .filter(|(sequence, id)| !drain.event_pending(id, **sequence))
            .take(budget)
            .map(|(sequence, id)| (*sequence, id.clone()))
            .collect();
        budget -= batch.len();
        // A batch shorter than the budget reached the contract's last
        // event: done. A full one continues after its last event.
        let next = if budget > 0 {
            None
        } else {
            batch.last().map(|(sequence, _)| *sequence)
        };
        progress.push((key.clone(), next));
        events.extend(batch.into_iter().map(|(_, id)| id));
    }
    update_cursors(s, progress);
}
fn update_cursors(s: &mut State, progress: Vec<(String, Option<u64>)>) {
    for (key, next) in progress {
        match next {
            Some(cursor) => {
                s.retention.rejudge.insert(key, cursor);
            }
            None => {
                s.retention.rejudge.remove(&key);
            }
        }
    }
}

pub(super) fn sync_event_order(s: &mut State, key: &str, order: String) -> Result<()> {
    let held = event_held(s, key)?;
    let in_order = s.retention.replay_order.contains_key(&order);
    if held && in_order {
        s.retention.replay_order.remove(&order);
    } else if !held && !in_order {
        s.retention.replay_order.insert(order, ());
    }
    Ok(())
}
