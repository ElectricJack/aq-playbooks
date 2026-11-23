//! Advance a run waiting for an effect, event, child, join, or retry deadline.
use super::*;

pub(crate) fn poll_pending(
    state: &mut State,
    run: &mut Run,
    pending: Pending,
    now: i64,
) -> Result<()> {
    let step = state.artifacts[&run.artifact].content.playbook.steps[&run.step].clone();
    let mut poll = Poll {
        state,
        run,
        now,
        step,
    };
    match pending {
        Pending::Retry { deadline } => {
            if now >= deadline {
                poll.run.pending = None;
                poll.run.status = RunStatus::Running;
            }
            Ok(())
        }
        Pending::Effect { intent } => poll.effect(&intent),
        Pending::Event { wait } => poll.event(wait),
        Pending::Child { child, bind } => poll.child(&child, bind),
        Pending::Join {
            members,
            member_keys,
            mode,
            accepted,
            deadline,
            remaining,
            bind,
        } => poll.join(
            Join {
                members,
                member_keys,
                mode,
                accepted,
                deadline,
                remaining,
            },
            bind,
        ),
    }
}

struct Poll<'a> {
    state: &'a mut State,
    run: &'a mut Run,
    now: i64,
    step: Step,
}
impl Poll<'_> {
    fn follow(&mut self, outcome: &str) -> Result<()> {
        self.run
            .transition(self.state, self.now)
            .follow(&self.step.edges, outcome)
    }

    fn effect(&mut self, id: &str) -> Result<()> {
        let intent = self.state.intents[id].clone();
        match intent.status {
            IntentStatus::Completed => self.completed(intent.result.unwrap()),
            IntentStatus::Rejected => Err(Error::Refused("command admission rejected".into())),
            IntentStatus::Unknown => {
                self.run.status = RunStatus::Blocked;
                self.run.reason = intent.reason;
                Ok(())
            }
            IntentStatus::Accepted if self.step.edges.contains_key("accepted") => {
                self.accepted(intent)
            }
            _ => Ok(()),
        }
    }

    fn completed(&mut self, (outcome, output): (String, Value)) -> Result<()> {
        if let Operation::Invoke { bind, retry, .. } = &self.step.op {
            if let Some(policy) = retry.as_ref().filter(|p| p.outcomes.contains(&outcome))
                && schedule_retry(self.state, self.run, self.now, policy)?
            {
                return Ok(());
            }
            self.run.variables.insert(bind.clone(), output);
        }
        self.follow(&outcome)
    }

    fn accepted(&mut self, intent: Intent) -> Result<()> {
        if let Operation::Invoke { bind, .. } = &self.step.op {
            let value = json!({"handle":intent.handle});
            intent
                .command
                .outcomes
                .get("accepted")
                .ok_or_else(|| {
                    Error::Refused("accepted edge missing declared admission schema".into())
                })?
                .validate(&value)?;
            self.run.variables.insert(bind.clone(), value);
        }
        self.follow("accepted")
    }

    fn event(&mut self, id: String) -> Result<()> {
        let mut wait = self.state.waits[&id].clone();
        if let Some(event) = winning_event(self.state, &wait)? {
            wait.winner = Some(event.key.clone());
            self.run
                .variables
                .insert(wait.bind.clone(), event.envelope.payload);
            self.state.waits.insert(id, wait);
            self.follow("received")?;
        } else if self.now >= wait.deadline {
            wait.expired = true;
            self.state.waits.insert(id, wait);
            self.follow("timeout")?;
        }
        Ok(())
    }

    fn child(&mut self, id: &str, bind: String) -> Result<()> {
        let child = self
            .state
            .runs
            .get(id)
            .ok_or_else(|| invariant(format!("child {id} of run {} is missing", self.run.id)))?
            .clone();
        if child.status.terminal() {
            let outcome = child.outcome.unwrap_or_else(|| {
                match child.status {
                    RunStatus::Cancelled => "cancelled",
                    _ => "failed",
                }
                .into()
            });
            self.run
                .variables
                .insert(bind, child.output.unwrap_or(Value::Null));
            self.follow(&outcome)?;
        }
        Ok(())
    }

    fn join(&mut self, join: Join, bind: String) -> Result<()> {
        let completed = join.completed(self.state);
        if let Some(outcome) = join
            .outcome(&completed)
            .or_else(|| (self.now >= join.deadline).then_some("timeout"))
        {
            self.run.variables.insert(bind, join.output(&completed));
            join.finish(self.state, self.now)?;
            self.follow(outcome)?;
        }
        Ok(())
    }
}

fn schedule_retry(
    state: &mut State,
    run: &mut Run,
    now: i64,
    policy: &RetryPolicy,
) -> Result<bool> {
    let key = attempt_key(run)?;
    let attempts = run.attempts.entry(key).or_insert(1);
    if *attempts >= policy.max_attempts {
        return Ok(false);
    }
    *attempts += 1;
    let attempt = *attempts;
    run.pending = Some(Pending::Retry {
        deadline: deadline(now, policy.backoff_ms)?,
    });
    run.receipt_at(now)
        .record(state, "business_retry", json!({"attempt":attempt}));
    Ok(true)
}

fn winning_event(state: &State, wait: &WaitRecord) -> Result<Option<AcceptedEvent>> {
    let Some(reference) = &wait.event else {
        return Ok(None);
    };
    let key = indexes::correlation_key(reference, &wait.scope, &wait.correlation)?;
    let Some(keys) = state.indexes.events.get(&key) else {
        return Ok(None);
    };
    Ok(keys
        .values()
        .filter_map(|id| state.events.get(id))
        .filter(|event| event_matches(wait, reference, event))
        .min_by_key(|event| event.sequence)
        .cloned())
}

fn event_matches(wait: &WaitRecord, reference: &ContractRef, event: &AcceptedEvent) -> bool {
    let envelope = &event.envelope;
    envelope.contract == *reference
        && envelope.scope == wait.scope
        && event.accepted_at_ms <= wait.deadline
        && wait
            .correlation
            .iter()
            .all(|(k, v)| envelope.correlation.get(k) == Some(v))
        && wait
            .minimum_revision
            .is_none_or(|rev| envelope.revision.is_some_and(|r| r >= rev))
}

struct Join {
    members: Vec<String>,
    member_keys: Vec<String>,
    mode: JoinMode,
    accepted: BTreeSet<String>,
    deadline: i64,
    remaining: RemainingPolicy,
}

impl Join {
    fn completed(&self, state: &State) -> Vec<Participant> {
        let mut completed: Vec<_> = self
            .members
            .iter()
            .filter_map(|id| Participant::find(state, id))
            .filter(|p| p.accepted_at_ms <= self.deadline)
            .collect();
        completed.sort_by_key(|p| p.sequence);
        completed
    }
    fn qualifies(&self, p: &Participant) -> bool {
        p.success
            && (self.accepted.is_empty()
                || p.outcome
                    .as_ref()
                    .is_some_and(|o| self.accepted.contains(o)))
    }
    fn outcome(&self, completed: &[Participant]) -> Option<&'static str> {
        let all = completed.len() == self.members.len();
        match self.mode {
            JoinMode::All if all => Some(if completed.iter().all(|p| self.qualifies(p)) {
                "completed"
            } else {
                "no_match"
            }),
            JoinMode::AnyCompletion if !completed.is_empty() => Some("completed"),
            JoinMode::AnyAccepted if completed.iter().any(|p| self.qualifies(p)) => {
                Some("completed")
            }
            JoinMode::AnyAccepted if all => Some("no_match"),
            _ => None,
        }
    }
    fn output(&self, completed: &[Participant]) -> Value {
        let selected: Vec<_> = match self.mode {
            JoinMode::AnyAccepted => completed
                .iter()
                .filter(|p| self.qualifies(p))
                .take(1)
                .collect(),
            JoinMode::AnyCompletion => completed.iter().take(1).collect(),
            JoinMode::All => completed.iter().collect(),
        };
        Value::Object(selected.into_iter().map(|p| self.entry(p)).collect())
    }
    fn entry(&self, p: &Participant) -> (String, Value) {
        let key = self
            .members
            .iter()
            .position(|id| id == &p.id)
            .and_then(|i| self.member_keys.get(i))
            .cloned()
            .unwrap_or_else(|| p.id.clone());
        let value = if self.member_keys.is_empty() {
            json!({"outcome":p.outcome,"output":p.output})
        } else {
            json!({"participant":p.id,"outcome":p.outcome,"output":p.output})
        };
        (key, value)
    }
    fn finish(self, state: &mut State, now: i64) -> Result<()> {
        let unfinished: Vec<_> = self
            .members
            .into_iter()
            .filter(|id| Participant::find(state, id).is_none())
            .collect();
        if self.remaining == RemainingPolicy::Cancel {
            for id in unfinished {
                if state.runs.contains_key(&id) {
                    request_cancel(state, &id, now)?;
                } else {
                    state.intents.get_mut(&id).unwrap().cancel_requested = true;
                    indexes::mark_intent(state, &id);
                }
            }
        }
        Ok(())
    }
}

struct Participant {
    id: String,
    sequence: u64,
    accepted_at_ms: i64,
    success: bool,
    outcome: Option<String>,
    output: Option<Value>,
}
impl Participant {
    fn find(s: &State, id: &str) -> Option<Self> {
        if let Some(c) = s.runs.get(id)
            && c.status.terminal()
        {
            return Some(Participant {
                id: id.into(),
                sequence: c.terminal_sequence.unwrap_or(u64::MAX),
                accepted_at_ms: Self::receipt_time(s, c.terminal_sequence),
                success: c.status == RunStatus::Completed,
                outcome: c.outcome.clone(),
                output: c.output.clone(),
            });
        }
        if let Some(i) = s.intents.get(id)
            && matches!(
                i.status,
                IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
            )
        {
            let sequence = s
                .receipts
                .iter()
                .rev()
                .find(|r| {
                    r.detail.get("operation").and_then(Value::as_str) == Some(id)
                        && matches!(
                            r.kind.as_str(),
                            "command_result" | "operation_rejected" | "cancel_response"
                        )
                })
                .map(|r| r.sequence)
                .unwrap_or(u64::MAX);
            return Some(Participant {
                id: id.into(),
                sequence,
                accepted_at_ms: Self::receipt_time(s, Some(sequence)),
                success: i.status == IntentStatus::Completed,
                outcome: i.result.as_ref().map(|r| r.0.clone()),
                output: i.result.as_ref().map(|r| r.1.clone()),
            });
        }
        None
    }
    fn receipt_time(s: &State, seq: Option<u64>) -> i64 {
        s.receipts
            .iter()
            .find(|r| Some(r.sequence) == seq)
            .map(|r| r.at_ms)
            .unwrap_or(i64::MAX)
    }
}
