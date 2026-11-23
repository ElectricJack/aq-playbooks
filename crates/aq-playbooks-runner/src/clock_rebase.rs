//! Correction of retention clocks pinned ahead of host time.
use super::*;
mod deadlines;
use deadlines::Deadlines;

/// What [`Engine::rebase_retention_clock`] changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockRebaseReport {
    pub from_ms: i64,
    pub to_ms: i64,
    pub stale_floor_ms: i64,
    pub capped_events: usize,
    pub capped_admissions: usize,
    /// Wait, join, retry and operation deadlines and run duration bases
    /// capped at `to_ms` plus their declared duration.
    pub capped_deadlines: usize,
    /// The live runs whose timers were capped (an operation's run included).
    pub adjusted_runs: Vec<String>,
    /// Live runs with a deadline after `to_ms` whose declared duration
    /// could not be found (an inconsistent State): left as they are, for the
    /// operator to cancel.
    pub needs_operator_action: Vec<String>,
    /// Each capped timer with its original value, for audit: at most
    /// [`REBASE_ADJUSTMENTS_LISTED`]; the rest are only counted.
    pub adjustments: Vec<DeadlineAdjustment>,
    pub adjustments_not_listed: usize,
}
/// At most this many [`DeadlineAdjustment`]s are listed in a rebase report
/// and its receipt.
pub const REBASE_ADJUSTMENTS_LISTED: usize = 1_000;
/// One timer a clock rebase capped: of `run`, its `kind`
/// (`duration_basis`, `wait`, `join`, `retry`, `operation`) on `item` (the
/// run, wait or operation id), from `original_ms` to `capped_ms`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadlineAdjustment {
    pub run: String,
    pub kind: String,
    pub item: String,
    pub original_ms: i64,
    pub capped_ms: i64,
}
/// Of the windows in `policy`, the smallest (at least 1 ms).
fn smallest_window(policy: &RetentionPolicy) -> i64 {
    let ms = policy
        .admission_replay_ms
        .min(policy.event_replay_ms)
        .max(1);
    i64::try_from(ms).unwrap_or(i64::MAX)
}
/// How far past the host time a rebase target may be: a minute, or a tenth
/// of the smallest window if that is less. Enough for clock skew between
/// hosts; small enough that the rebased clock leaves nearly every window
/// behind real time.
fn rebase_slack(policy: &RetentionPolicy) -> i64 {
    (smallest_window(policy) / 10).min(60_000)
}
/// The rebase of `s` to `to_ms` at host time `now_ms` (see
/// [`Engine::rebase_retention_clock`]), without its receipt.
pub(super) fn rebase_plan(
    s: &mut State,
    to_ms: i64,
    stale_floor_ms: i64,
    now_ms: i64,
) -> Result<ClockRebaseReport> {
    let Some(from_ms) = s.retention.clock_ms else {
        return Err(Error::Refused("no retention clock to rebase".into()));
    };
    let smallest = smallest_window(&s.retention.policy);
    let ahead = from_ms.saturating_sub(now_ms);
    if ahead <= smallest {
        return Err(Error::Refused(format!(
            "retention clock ({from_ms} ms) is not pinned: it is {ahead} ms ahead of host time \
             ({now_ms} ms), not more than the smallest window ({smallest} ms)"
        )));
    }
    let slack = rebase_slack(&s.retention.policy);
    if to_ms < now_ms || to_ms > now_ms.saturating_add(slack) {
        return Err(Error::Refused(format!(
            "clock rebase target ({to_ms} ms) must be from host time ({now_ms} ms) to {slack} ms \
             after it"
        )));
    }
    if stale_floor_ms > now_ms {
        return Err(Error::Refused(format!(
            "stale floor ({stale_floor_ms} ms) must be in the past (host time {now_ms} ms)"
        )));
    }
    let floor_ms = s
        .retention
        .stale_floor_ms
        .map_or(stale_floor_ms, |old| old.max(stale_floor_ms));
    if floor_ms > to_ms {
        return Err(Error::Refused(format!(
            "stale floor ({floor_ms} ms) is later than the rebase target ({to_ms} ms)"
        )));
    }
    let (capped_events, capped_admissions) = retention::rebase_clock(s, to_ms);
    let deadlines = Deadlines::rebase(s, to_ms);
    s.retention.stale_floor_ms = Some(floor_ms);
    Ok(ClockRebaseReport {
        from_ms,
        to_ms,
        stale_floor_ms: floor_ms,
        capped_events,
        capped_admissions,
        capped_deadlines: deadlines.capped,
        adjusted_runs: deadlines.adjusted.into_iter().collect(),
        needs_operator_action: deadlines.unknown.into_iter().collect(),
        adjustments_not_listed: deadlines.capped - deadlines.adjustments.len(),
        adjustments: deadlines.adjustments,
    })
}
