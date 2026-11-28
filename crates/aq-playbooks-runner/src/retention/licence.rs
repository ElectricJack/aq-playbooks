//! Validate the scope and exact clock change of administrative retention receipts.
use super::*;

/// What an admin licence receipt in a commit's own history allows: exactly
/// one clock rebase, or exactly one lowering of the stale floor. Each
/// carries the receipt's sequence, which must be new in this commit (after
/// the stored sequence), so a receipt cannot be replayed, even after its
/// history row was deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Licence {
    /// [`CLOCK_REBASE`]: the clock from `from_ms` down to `to_ms`.
    Rebase {
        from_ms: i64,
        to_ms: i64,
        sequence: u64,
    },
    /// [`STALE_FLOOR_LOWERED`]: the stale floor from `from_ms` down to
    /// `to_ms`.
    FloorLowered {
        from_ms: i64,
        to_ms: i64,
        sequence: u64,
    },
}
/// The licence `next`'s commit carries: a [`CLOCK_REBASE`] or
/// [`STALE_FLOOR_LOWERED`] admin receipt in `next.settled`, the history this
/// commit writes. A receipt left in the live State licenses nothing. More
/// than one, or one without its clocks, is refused.
pub fn licence(next: &State) -> std::result::Result<Option<Licence>, StorageError> {
    let mut found = None;
    for r in next
        .settled
        .iter()
        .filter(|cohort| cohort.root == "admin")
        .flat_map(|cohort| &cohort.receipts)
        .filter(|r| r.run == "admin")
    {
        let clocks = || Some((r.detail["from_ms"].as_i64()?, r.detail["to_ms"].as_i64()?));
        let licence = match r.kind.as_str() {
            CLOCK_REBASE => clocks().map(|(from_ms, to_ms)| Licence::Rebase {
                from_ms,
                to_ms,
                sequence: r.sequence,
            }),
            STALE_FLOOR_LOWERED => clocks().map(|(from_ms, to_ms)| Licence::FloorLowered {
                from_ms,
                to_ms,
                sequence: r.sequence,
            }),
            _ => continue,
        }
        .ok_or_else(|| refuse(REFUSE_LICENCE))?;
        if found.replace(licence).is_some() {
            return Err(refuse(REFUSE_LICENCE));
        }
    }
    Ok(found)
}
/// Whether `next`'s commit carries an admin licence receipt at all (valid
/// or not): a format 3 store sends such a commit to its complete path,
/// where [`verify_removals`] judges it.
pub fn carries_licence(next: &State) -> bool {
    next.settled
        .iter()
        .filter(|cohort| cohort.root == "admin")
        .flat_map(|cohort| &cohort.receipts)
        .any(|r| r.run == "admin" && (r.kind == CLOCK_REBASE || r.kind == STALE_FLOOR_LOWERED))
}
/// The expiry a clock rebase to `to_ms` gives entry `key` of `window`:
/// `to_ms` plus the entry's own window (the admission window; for an event
/// `max(policy.event_replay_ms, contract.retention_ms)`).
pub fn rebase_cap(s: &State, window: Window, key: &str, to_ms: i64) -> i64 {
    let window_ms = match window {
        Window::Admission => super::window(s.retention.policy.admission_replay_ms),
        Window::Event => event_window(s, key),
    };
    to_ms.saturating_add(window_ms)
}
/// The clock, the stale floor and the licence of a commit from `previous`
/// to `next`. Without a licence the clock never decreases and the floor
/// never changes. With one, the commit must be exactly that licence: its
/// receipt's sequence is new in this commit, and
/// - a rebase lowers the clock from exactly `from_ms` to exactly `to_ms <
///   from_ms`, and may only raise the floor, to at most `to_ms`;
/// - a floor lowering leaves the clock alone and lowers the floor from
///   exactly `from_ms` to exactly `to_ms < from_ms`.
///
/// What else such a commit may not do (add or remove a run, operation,
/// event or keyed receipt, remove an audit receipt, change an expiry other
/// than by a rebase cap) is checked where those records are.
pub(super) fn check_licence(
    previous: &State,
    next: &State,
    licence: Option<Licence>,
) -> std::result::Result<(), StorageError> {
    let clocks = ClockChange {
        before: previous.retention.clock_ms,
        after: next.retention.clock_ms,
    };
    let floors = ClockChange {
        before: previous.retention.stale_floor_ms,
        after: next.retention.stale_floor_ms,
    };
    let Some(licence) = licence else {
        if clocks.decreased() {
            return Err(refuse(REFUSE_CLOCK));
        }
        return check_floor_unchanged(previous, next);
    };
    check_licensed_header(previous, next)?;
    let (Licence::Rebase { sequence, .. } | Licence::FloorLowered { sequence, .. }) = licence;
    if sequence <= previous.sequence || sequence > next.sequence {
        return Err(refuse(REFUSE_LICENCE));
    }
    match licence {
        Licence::Rebase { from_ms, to_ms, .. } => {
            clocks.expect_lowering(from_ms, to_ms)?;
            if !floors.raised_within(to_ms) {
                return Err(refuse(REFUSE_STALE_FLOOR));
            }
        }
        Licence::FloorLowered { from_ms, to_ms, .. } => {
            if clocks.before != clocks.after {
                return Err(refuse(REFUSE_LICENCE));
            }
            floors.expect_lowering(from_ms, to_ms)?;
        }
    }
    Ok(())
}
struct ClockChange {
    before: Option<i64>,
    after: Option<i64>,
}
impl ClockChange {
    fn decreased(&self) -> bool {
        self.before
            .is_some_and(|old| self.after.is_none_or(|new| new < old))
    }
    fn expect_lowering(&self, from: i64, to: i64) -> std::result::Result<(), StorageError> {
        if to < from && (self.before, self.after) == (Some(from), Some(to)) {
            Ok(())
        } else {
            Err(refuse(REFUSE_LICENCE))
        }
    }
    fn raised_within(&self, cap: i64) -> bool {
        match self.after {
            Some(new) => new <= cap && self.before.is_none_or(|old| new >= old),
            None => self.before.is_none(),
        }
    }
}
/// In a licensed commit every header field the licence does not name is
/// unchanged: the retention policy, producer epochs, GC state and artifact
/// disposals, the artifacts, bindings, catalog revision and history,
/// execution denials, authority revocations and recovery requests. (The
/// clock, the floor and the sequence are judged by [`check_licence`].)
pub(crate) fn check_licensed_header(
    previous: &State,
    next: &State,
) -> std::result::Result<(), StorageError> {
    // Every field is named (no `..`): a field added to `State` or
    // `RetentionState` does not compile here until someone decides whether a
    // licence may change it. `_` marks a field this comparison does not
    // judge, with the reason.
    let State {
        // Containers: removals are judged per key by `check_removals`, which
        // a licence does not relax; their content (and waits and budgets,
        // live sub-records) any commit may change, so a licence adds nothing.
        keyed_admissions: _,
        runs: _,
        receipts: _,
        intents: _,
        waits: _,
        events: _,
        budgets: _,
        // Derived from the State and rebuilt by `indexes::sync`.
        indexes: _,
        // Judged by `check_licence` (it must increase).
        sequence: _,
        // Not stored.
        protection_available: _,
        settled: _,
        replay_touched: _,
        released_artifacts: _,
        // Compared in same_catalog_and_recovery.
        catalog_history: _,
        catalog_history_at_ms,
        retention,
        execution_denied,
        authority_revocations,
        recovery_requests: _,
        artifacts: _,
        bindings: _,
        binding_schedules: _,
        deactivated_bindings: _,
        catalog_revision,
    } = previous;
    let n = next;
    let unchanged = same_retention_header(retention, &next.retention)
        && (catalog_revision, catalog_history_at_ms)
            == (&n.catalog_revision, &n.catalog_history_at_ms)
        && (execution_denied, authority_revocations)
            == (&n.execution_denied, &n.authority_revocations)
        && same_catalog_and_recovery(previous, next);
    if unchanged {
        Ok(())
    } else {
        Err(refuse(REFUSE_LICENCE))
    }
}
fn same_catalog_and_recovery(previous: &State, next: &State) -> bool {
    same_serialized(&previous.catalog_history, &next.catalog_history)
        && same_serialized(&previous.bindings, &next.bindings)
        && same_serialized(&previous.binding_schedules, &next.binding_schedules)
        && previous.deactivated_bindings == next.deactivated_bindings
        && same_serialized(&previous.recovery_requests, &next.recovery_requests)
        && same_serialized(&previous.artifacts, &next.artifacts)
}
fn same_serialized<T: Serialize>(a: &T, b: &T) -> bool {
    matches!((serde_json::to_vec(a), serde_json::to_vec(b)), (Ok(a), Ok(b)) if a == b)
}

fn same_retention_header(previous: &RetentionState, r: &RetentionState) -> bool {
    let RetentionState {
        // Retained-history containers and the window: fenced per key
        // (`check_removals`, `check_expiry_record`), not here.
        event_epochs: _,
        event_expires_at_ms: _,
        expired_correlations: _,
        event_tombstones: _,
        disposals: _,
        expiry: _,
        admission_expires_at_ms: _,
        replay_order: _,
        replay_held: _,
        // What a licence names: judged by `check_licence`.
        clock_ms: _,
        stale_floor_ms: _,
        last_gc_at_ms,
        policy,
        producer_epochs,
        artifact_disposals,
        artifact_installs,
        artifact_activations,
        gc_cursor,
        // A licensed commit drains nothing and judges no event again.
        drain,
        rejudge,
    } = previous;

    same_serialized(policy, &r.policy)
        && (drain, rejudge) == (&r.drain, &r.rejudge)
        && producer_epochs == &r.producer_epochs
        && (last_gc_at_ms, gc_cursor, artifact_disposals)
            == (&r.last_gc_at_ms, &r.gc_cursor, &r.artifact_disposals)
        && (artifact_installs, artifact_activations)
            == (&r.artifact_installs, &r.artifact_activations)
}
/// Without a licence the stale floor never changes (a format 3 bounded
/// commit never carries one).
pub fn check_floor_unchanged(
    previous: &State,
    next: &State,
) -> std::result::Result<(), StorageError> {
    if previous.retention.stale_floor_ms != next.retention.stale_floor_ms {
        return Err(refuse(REFUSE_STALE_FLOOR));
    }
    Ok(())
}
/// [`check_expiry_change`] for record `key` of `window`, against `next`. In
/// a commit with a licence the only allowed change is a rebase cap: a
/// recorded expiry lowered to exactly [`rebase_cap`] computed from
/// `previous` (its policy, artifacts and the stored event), with the clock
/// at the rebase's `to_ms`. `previous` needs its header and, for an event,
/// that event's stored record.
pub(crate) struct ExpiryChange<'a> {
    pub(crate) window: Window,
    pub(crate) key: &'a str,
    pub(crate) old: Option<i64>,
    pub(crate) new: Option<i64>,
}
impl ExpiryChange<'_> {
    pub(crate) fn check(
        self,
        previous: &State,
        next: &State,
    ) -> std::result::Result<(), StorageError> {
        let Self {
            old,
            new,
            window,
            key,
        } = self;
        match licence(next)? {
            Some(Licence::Rebase { to_ms, .. }) => {
                if next.retention.clock_ms != Some(to_ms) {
                    return Err(refuse(REFUSE_LICENCE));
                }
                let capped = matches!((old, new), (Some(old), Some(new))
                if new < old && new == rebase_cap(previous, window, key, to_ms));
                return if capped {
                    Ok(())
                } else {
                    Err(refuse(REFUSE_EXPIRY))
                };
            }
            Some(Licence::FloorLowered { .. }) => return Err(refuse(REFUSE_EXPIRY)),
            None => {}
        }
        let item_kept = match window {
            Window::Event => next.events.contains_key(key),
            Window::Admission => next.keyed_admissions.contains_key(key),
        };
        let fallback = match (old, new) {
            (None, Some(_)) => fallback_expiry(next, window, key),
            _ => None,
        };
        check_expiry_change(old, new, item_kept, fallback)
    }
}
