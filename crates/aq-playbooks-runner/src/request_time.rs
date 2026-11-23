//! Producer request time and retained replay-clock diagnostics.
use super::*;
pub(super) struct ReplayWindow {
    clock_ms: Option<i64>,
    now_ms: i64,
    window_ms: u64,
    floor_ms: Option<i64>,
}
impl ReplayWindow {
    pub(super) fn new(
        clock_ms: Option<i64>,
        now_ms: i64,
        window_ms: u64,
        floor_ms: Option<i64>,
    ) -> Self {
        Self {
            clock_ms,
            now_ms,
            window_ms,
            floor_ms,
        }
    }
    fn start(&self) -> i64 {
        self.clock_ms
            .map_or(self.now_ms, |clock| clock.max(self.now_ms))
    }
    fn check(self, requested_at_ms: i64) -> Result<()> {
        let cutoff_ms = self
            .start()
            .saturating_sub(i64::try_from(self.window_ms).unwrap_or(i64::MAX))
            .max(self.floor_ms.unwrap_or(i64::MIN));
        if requested_at_ms > cutoff_ms {
            return Ok(());
        }
        Err(Error::Stale {
            requested_at_ms,
            cutoff_ms,
            clock_ms: self.clock_ms,
            now_ms: self.now_ms,
            window_ms: self.window_ms,
            floor_ms: self.floor_ms,
        })
    }
    pub(super) fn stale_message(self, requested_at_ms: i64, cutoff_ms: i64) -> String {
        let start = self.start();
        let Self {
            now_ms,
            window_ms,
            floor_ms,
            ..
        } = self;
        let mut message = format!(
            "{STALE_REQUEST}: request time {requested_at_ms} ms is at or before the cut-off \
         {cutoff_ms} ms (retention time {start} ms minus the {window_ms} ms window; host \
         time {now_ms} ms"
        );
        if let Some(floor) = floor_ms {
            message.push_str(&format!("; stale floor {floor} ms from a clock rebase"));
        }
        message.push(')');
        if start > now_ms {
            message.push_str(&format!(
                "; the retention clock ({start} ms) is ahead of host time by {} ms, and while \
             it is, every request that carries a real request time is refused as stale \
             (a clock pinned by a far-future host time stays ahead until it is rebased)",
                start.saturating_sub(now_ms)
            ));
        }
        message
    }
}

/// Refuse a request for a key the engine does not remember (absent, or
/// expired and held by nothing) when its request time is at or before the
/// cut-off: the window before `max(now, clock)`, the time expiry is judged
/// against, or the stale floor if that is later. A request without a
/// request time is not checked, and the window is not computed. The request
/// time is only compared: it never moves the clock and is never stored.
pub(super) fn refuse_stale(
    s: &State,
    requested_at_ms: Option<i64>,
    window_ms: impl FnOnce() -> u64,
    now_ms: i64,
) -> Result<()> {
    let Some(requested_at_ms) = requested_at_ms else {
        return Ok(());
    };
    let window = ReplayWindow::new(
        s.retention.clock_ms,
        now_ms,
        window_ms(),
        s.retention.stale_floor_ms,
    );
    window.check(requested_at_ms)
}
