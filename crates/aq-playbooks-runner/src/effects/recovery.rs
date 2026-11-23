use super::*;

impl<S: Storage> Engine<S> {
    pub fn recover(&self, host: &mut impl Host, now_ms: i64) -> Result<usize> {
        let state = self.snapshot()?;
        let candidates = self.query(&EffectQueueKind::Recovery.query_after(&state, 64))?;
        let mut count = 0;
        for item in candidates.items {
            let Some(intent) = state.intents.get(&item.key) else {
                continue;
            };
            if !intent.permitted(&state, host)? {
                continue;
            }
            self.transaction_at(Some(now_ms), |s| {
                EffectQueueKind::Recovery.advance_cursor(s, &item);
                intent.receipt_at(now_ms).record(
                    s,
                    "recovery_attempt",
                    json!({"operation":intent.id}),
                );
                Ok(())
            })?;
            let request = state
                .recovery_requests
                .values()
                .find(|r| r.intent == intent.id && !r.consumed);
            RecoveryAttempt {
                intent,
                request,
                now_ms,
            }
            .perform(self, host)?;
            count += 1;
        }
        Ok(count)
    }
}

struct RecoveryAttempt<'a> {
    intent: &'a Intent,
    request: Option<&'a RecoveryRequest>,
    now_ms: i64,
}

impl Intent {
    fn cancellation_in_flight(&self) -> bool {
        self.cancel_requested && self.cancel_dispatched && self.command.cancellation
    }
}

impl RecoveryAttempt<'_> {
    fn perform<S: Storage>(&self, engine: &Engine<S>, host: &mut impl Host) -> Result<()> {
        let intent = self.intent;
        if intent.cancellation_in_flight() {
            return self.reconcile_cancel(engine, host);
        }
        if self.replay_stopped() {
            return engine.record_result(&intent.id, DispatchResult::Unknown {
                reason: "operation expired or cancellation requested; replay cannot submit another effect".into(),
            }, self.now_ms);
        }
        let result = match intent.command.recovery {
            Recovery::Observation | Recovery::NaturallyIdempotent | Recovery::KeyedIdempotent => {
                return engine.transaction_at(Some(self.now_ms), |s| {
                    s.intents.get_mut(&intent.id).unwrap().status = IntentStatus::Prepared;
                    indexes::mark_intent(s, &intent.id);
                    Ok(())
                });
            }
            Recovery::Reconcilable => {
                if let Some(request) = self.request {
                    self.consume(engine, request, false)?;
                }
                engine.measure_host(|| host.reconcile(intent))
            }
            Recovery::Unsafe => self.recover_unsafe(engine, host)?,
        };
        engine.record_result(&intent.id, result, self.now_ms)
    }

    fn replay_stopped(&self) -> bool {
        let intent = self.intent;
        let stopped = self.now_ms >= intent.deadline_at_ms || intent.cancel_requested;
        let can_reconcile =
            intent.command.recovery == Recovery::Reconcilable || self.request.is_some();
        stopped && !can_reconcile
    }

    fn reconcile_cancel<S: Storage>(&self, engine: &Engine<S>, host: &mut impl Host) -> Result<()> {
        let intent = self.intent;
        let result = engine.measure_host(|| host.reconcile(intent));
        let running = matches!(result, DispatchResult::Accepted { .. });
        engine.record_result(&intent.id, result, self.now_ms)?;
        if running {
            engine.transaction_at(Some(self.now_ms), |s| {
                s.intents.get_mut(&intent.id).unwrap().cancel_dispatched = false;
                indexes::mark_intent(s, &intent.id);
                intent.receipt_at(self.now_ms).record(
                    s,
                    "cancel_retry_authorized_by_status",
                    json!({"operation":intent.id}),
                );
                Ok(())
            })?;
        }
        Ok(())
    }

    fn recover_unsafe<S: Storage>(
        &self,
        engine: &Engine<S>,
        host: &mut impl Host,
    ) -> Result<DispatchResult> {
        let Some(request) = self.request else {
            return Ok(DispatchResult::Unknown {
                reason: "unsafe external outcome requires authorized reconciliation".into(),
            });
        };
        self.consume(engine, request, true)?;
        Ok(engine.measure_host(|| host.reconcile(self.intent)))
    }

    fn consume<S: Storage>(
        &self,
        engine: &Engine<S>,
        request: &RecoveryRequest,
        audit: bool,
    ) -> Result<()> {
        engine.transaction_at(Some(self.now_ms), |s| {
            s.recovery_requests.get_mut(&request.id).unwrap().consumed = true;
            if audit {
                self.intent.receipt_at(self.now_ms).record(
                    s,
                    "authorized_recovery",
                    json!({"request":request.id,"actor":request.actor}),
                );
            }
            Ok(())
        })
    }
}
