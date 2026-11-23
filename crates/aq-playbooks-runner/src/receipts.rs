//! Persist receipt positions and both indexes in the same state update.
use super::*;
pub(super) struct ReceiptSite<'a> {
    run: &'a str,
    step: &'a str,
    at_ms: i64,
}
impl<'a> ReceiptSite<'a> {
    pub(super) fn new(run: &'a str, step: &'a str, at_ms: i64) -> Self {
        Self { run, step, at_ms }
    }
    pub(super) fn admin(step: &'a str, at_ms: i64) -> Self {
        Self::new("admin", step, at_ms)
    }
    pub(super) fn record(self, s: &mut State, kind: &str, detail: Value) {
        let Self {
            run,
            step,
            at_ms: now,
        } = self;
        s.sequence += 1;
        let offset = s.receipts.len();
        s.indexes
            .receipts
            .entry(run.into())
            .or_default()
            .insert(s.sequence, offset);
        s.indexes.all_receipts.insert(s.sequence, offset);
        s.receipts.push(Receipt {
            sequence: s.sequence,
            run: run.into(),
            step: step.into(),
            at_ms: now,
            kind: kind.into(),
            detail,
        });
    }
}
impl Run {
    pub(super) fn receipt_at(&self, at_ms: i64) -> ReceiptSite<'_> {
        ReceiptSite::new(&self.id, &self.step, at_ms)
    }
}
impl Intent {
    pub(super) fn receipt_at(&self, at_ms: i64) -> ReceiptSite<'_> {
        ReceiptSite::new(&self.run, &self.step, at_ms)
    }
}
