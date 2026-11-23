//! Cases: a situation in plain words and in data, and what must follow.
//!
//! A case says how a run starts (an event, or an entry with its input), what
//! each command the playbook calls returns, in call order, and what must
//! follow: the outcome the run ends with and the calls it makes, with their
//! inputs. [`run_case`] runs a sealed artifact on a fresh in-memory engine
//! with a host that answers from the case, records every call with its
//! input, and reads the run's outcome through public inspection (a finished
//! run has left the engine's state; spec 0003). Nothing is read from the
//! network, a clock or a file.
//!
//! - An answer to a synchronous command is its result.
//! - An answer to an asynchronous command (such as `judgment.assess`) is
//!   returned as `accepted` with the handle `case-<n>`, and its completion
//!   event is then admitted with `{outcome, output}` as its payload.
//! - `{"pending": true}` accepts an asynchronous command and never answers
//!   it: the clock moves on to each deadline, so the playbook's timeout path
//!   runs. A run that has returned and waits only for such an operation to
//!   settle ends, for the case, with the outcome it returned.
//! - A call the case gives no return for fails the case, naming the step.
//! - A call the case does not list fails the case, and so does a listed
//!   call the run does not make: `expect.calls` is every call the run makes,
//!   in order, and each `input` must equal what the playbook sent. A return
//!   the run never uses fails the case too.
//! - A case starts from any event a trigger of the playbook names, with its
//!   payload and the values of the event's correlation fields
//!   (`start.correlation`), or from an entry with its input.
//! - Events are built from their contract: its first trusted producer, ids
//!   `case-<i>`, the playbook's scope, and the subject and revision read
//!   from the payload at the contract's subject and revision fields.
//! - [`run_cases`] runs a save's cases within one total transition budget
//!   ([`CASES_TRANSITION_BUDGET`] by default) as well as the per-case cap.
//!
//! [`Outcome`] is what an agent's tools write. Every field of it is the
//! agent's claim: a reader re-derives each one (seals the draft again from
//! its own inputs, re-runs the cases, re-quotes each sentence from its own
//! copy of the document with [`quote`], recomputes the digests) and never
//! stores the agent's text or digests as fact.
use aq_playbooks_model::{Artifact, ContractRef, EventEnvelope, Operation, Registry, digest};
use aq_playbooks_runner::{
    Authority, DispatchResult, Engine, Host, InspectionKind, InspectionQuery, Intent, MemoryStorage,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// A save carries at least this many cases, and at most [`MAX_CASES`].
pub const MIN_CASES: usize = 5;
pub const MAX_CASES: usize = 40;
pub const MAX_CASE_NAME_BYTES: usize = 128;
pub const MAX_SITUATION_BYTES: usize = 1024;
/// Returns given for one command in one case.
pub const MAX_RETURNS_PER_COMMAND: usize = 64;
/// Engine transitions one case may take.
pub const MAX_CASE_TRANSITIONS: usize = 10_000;
/// Engine transitions all the cases of one save may take together.
pub const CASES_TRANSITION_BUDGET: usize = 40_000;
/// Times the clock may move on to a deadline in one case.
const MAX_CASE_ROUNDS: usize = 256;

mod report;
use report::{expected_calls, inspect_outcome};
mod validation;
use validation::check_case;
mod outcome;
pub use outcome::*;

mod format;
pub use format::*;

fn names<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    let items: Vec<&str> = items.into_iter().collect();
    if items.is_empty() {
        "none".into()
    } else {
        items.join(", ")
    }
}
fn field<'v>(value: &'v Value, dotted: &str) -> Option<&'v Value> {
    dotted.split('.').try_fold(value, |v, key| v.get(key))
}
/// An envelope for `event`, built from its contract.
struct CaseEvent {
    id: String,
    correlation: BTreeMap<String, String>,
    payload: Value,
}
impl CaseEvent {
    fn envelope(
        self,
        registry: &Registry,
        scope: &str,
        event: &str,
    ) -> Result<EventEnvelope, String> {
        let Self {
            id,
            correlation,
            payload,
        } = self;
        let contract = registry.events.get(event).ok_or_else(|| {
            format!(
                "the playbook knows no event {event}; it knows: {}",
                names(registry.events.keys().map(String::as_str))
            )
        })?;
        let producer = contract
            .trusted_producers
            .iter()
            .next()
            .ok_or_else(|| format!("{event} names no producer"))?
            .clone();
        let subject = match &contract.subject_field {
            Some(at) => Some(
                field(&payload, at)
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        format!(
                            "payload.{at}: {event} carries its subject here, so it must be a string"
                        )
                    })?
                    .to_string(),
            ),
            None => None,
        };
        let revision = match &contract.revision_field {
            Some(at) => Some(field(&payload, at).and_then(Value::as_u64).ok_or_else(|| {
                format!(
                    "payload.{at}: {event} carries its revision here, so it must be a whole number"
                )
            })?),
            None => None,
        };
        Ok(EventEnvelope {
            producer,
            event_id: id,
            contract: ContractRef {
                name: event.into(),
                fingerprint: contract.fingerprint().map_err(|e| e.to_string())?,
            },
            scope: scope.into(),
            correlation,
            subject,
            revision,
            causation: None,
            occurred_at_ms: None,
            observed_at_ms: None,
            payload,
        })
    }
}

/// Answers each call from the case and records it.
struct CaseHost<'a> {
    registry: &'a Registry,
    scope: String,
    returns: BTreeMap<String, VecDeque<CaseReturn>>,
    calls: Vec<Call>,
    problems: Vec<String>,
    /// Completion events of answered asynchronous calls, admitted once the
    /// engine has stopped.
    completions: Vec<EventEnvelope>,
    handles: usize,
}
impl CaseHost<'_> {
    fn handle(&mut self) -> String {
        self.handles += 1;
        format!("case-{}", self.handles)
    }
}
impl Host for CaseHost<'_> {
    fn authorize(&self, a: &Authority, c: &aq_playbooks_model::CommandContract) -> bool {
        c.capabilities.is_subset(&a.capabilities) && c.scopes.contains(&a.scope)
    }
    fn dispatch(&mut self, intent: &Intent) -> DispatchResult {
        let command = intent.command.name.clone();
        let mut call = Call {
            step: intent.step.clone(),
            command: command.clone(),
            input: intent.input.clone(),
            outcome: None,
        };
        let given = self.returns.get_mut(&command).and_then(VecDeque::pop_front);
        let result = match given {
            None => {
                self.problems.push(format!(
                    "step {} called {command} but the case gives no return for it. Add its answer to returns.{command:?} (one per call, in call order), or {{\"pending\": true}} for an asynchronous command that never answers.",
                    intent.step
                ));
                DispatchResult::Rejected {
                    reason: "the case gives no return for this call".into(),
                }
            }
            Some(CaseReturn::Answer { outcome, output }) => match &intent.command.asynchronous {
                None => {
                    call.outcome = Some(outcome.clone());
                    DispatchResult::Completed { outcome, output }
                }
                Some(asynchronous) => {
                    let handle = self.handle();
                    let id = format!("case-{}", self.handles);
                    let completion = CaseEvent {
                        id,
                        correlation: BTreeMap::from([(
                            asynchronous.correlation_field.clone(),
                            handle.clone(),
                        )]),
                        payload: json!({"outcome":outcome,"output":output}),
                    }
                    .envelope(
                        self.registry,
                        &self.scope,
                        &asynchronous.completion_event,
                    );
                    match completion {
                        Ok(event) => self.completions.push(event),
                        Err(e) => self.problems.push(format!(
                            "step {}: the answer for {command} cannot be delivered: {e}.",
                            intent.step
                        )),
                    }
                    call.outcome = Some(outcome);
                    DispatchResult::Accepted { handle }
                }
            },
            Some(CaseReturn::AtOnce {
                outcome, output, ..
            }) => {
                call.outcome = Some(outcome.clone());
                DispatchResult::Completed { outcome, output }
            }
            Some(CaseReturn::Pending { .. }) => DispatchResult::Accepted {
                handle: self.handle(),
            },
        };
        self.calls.push(call);
        result
    }
    fn reconcile(&mut self, _intent: &Intent) -> DispatchResult {
        DispatchResult::Unknown {
            reason: "a case answers each call once; it does not reconcile".into(),
        }
    }
}

struct Ran {
    report: CaseReport,
    used: usize,
    stopped: bool,
}

/// Runs one case on `artifact`, within [`MAX_CASE_TRANSITIONS`]. An `Err` is
/// a fault of the artifact or the engine; everything about the case is in
/// the report's `problems`.
pub fn run_case(artifact: &Artifact, case: &Case) -> anyhow::Result<CaseReport> {
    artifact.verify()?;
    let mut ran = run_within(artifact, case, MAX_CASE_TRANSITIONS)?;
    if ran.stopped {
        stopped_at_case_cap(&mut ran.report);
    }
    Ok(ran.report)
}
fn stopped_at_case_cap(report: &mut CaseReport) {
    report.problems.push(format!(
        "the run took more than {MAX_CASE_TRANSITIONS} transitions; look for a loop that never ends."
    ));
    report.passed = false;
}

fn run_within(artifact: &Artifact, case: &Case, limit: usize) -> anyhow::Result<Ran> {
    let mut report = CaseReport {
        name: case.name.clone(),
        situation: case.situation.clone(),
        passed: false,
        outcome: None,
        calls: vec![],
        problems: check_case(artifact, case),
    };
    let done = |report: CaseReport| Ran {
        report,
        used: 0,
        stopped: false,
    };
    if !report.problems.is_empty() {
        return Ok(done(report));
    }
    let (engine, authority) = case_engine(artifact)?;
    let mut host = CaseHost::new(artifact, case);
    if let Err(problem) = start_case(&engine, artifact, &case.start, authority) {
        report.problems.push(problem);
        return Ok(done(report));
    }
    let (used, stopped) = drive_case(&engine, &mut host, limit)?;
    report.calls = std::mem::take(&mut host.calls);
    report.problems = std::mem::take(&mut host.problems);
    inspect_outcome(&engine, artifact, &case.expect.outcome, &mut report)?;
    expected_calls(&case.expect.calls, &report.calls, &mut report.problems);
    host.check_unused(case, &mut report.problems);
    report.passed = report.problems.is_empty();
    Ok(Ran {
        report,
        used,
        stopped,
    })
}

type CaseEngine = Engine<MemoryStorage>;
fn case_engine(artifact: &Artifact) -> anyhow::Result<(CaseEngine, Authority)> {
    let playbook = &artifact.content.playbook;
    let engine = Engine::new(MemoryStorage::default(), "case", "case-runner")?;
    engine.install(artifact.clone())?;
    let authority = Authority {
        actor: "case".into(),
        scope: playbook.scope.clone(),
        capabilities: playbook.required_capabilities.clone(),
    };
    engine.activate(&playbook.id, &artifact.digest, authority.clone())?;
    Ok((engine, authority))
}

impl<'a> CaseHost<'a> {
    fn new(artifact: &'a Artifact, case: &Case) -> Self {
        Self {
            registry: &artifact.content.registry,
            scope: artifact.content.playbook.scope.clone(),
            returns: case
                .returns
                .iter()
                .map(|(k, v)| (k.clone(), v.iter().cloned().collect()))
                .collect(),
            calls: vec![],
            problems: vec![],
            completions: vec![],
            handles: 0,
        }
    }
    fn deliver(&mut self, engine: &CaseEngine, clock: i64) -> bool {
        let completions = std::mem::take(&mut self.completions);
        if completions.is_empty() {
            return false;
        }
        for event in completions {
            let producer = event.producer.clone();
            if let Err(e) = engine.admit_event(&producer, event, clock) {
                self.problems
                    .push(format!("an answer's completion event was refused: {e}."));
            }
        }

        true
    }
    fn check_unused(&self, case: &Case, problems: &mut Vec<String>) {
        // Every return the case gives is used by a call.
        for (command, left) in &self.returns {
            let given = case.returns.get(command).map_or(0, Vec::len);
            for i in given - left.len()..given {
                problems.push(format!(
                "returns.{command:?}[{i}]: never used: the run called {command} {} time(s). Give one return per call the run makes.",
                given - left.len()
            ));
            }
        }
    }
}

fn start_case(
    engine: &CaseEngine,
    artifact: &Artifact,
    start: &CaseStart,
    authority: Authority,
) -> Result<(), String> {
    let playbook = &artifact.content.playbook;
    let registry = &artifact.content.registry;
    match start {
        CaseStart::Entry { entry, input } => engine
            .admit(&playbook.id, entry, input.clone(), authority, 0)
            .map(|_| ())
            .map_err(|e| format!("start.input: the entry {entry} refused it: {e}.")),
        CaseStart::Event {
            event,
            payload,
            correlation,
        } => {
            let event = CaseEvent {
                id: "case-0".into(),
                correlation: correlation.clone(),
                payload: payload.clone(),
            }
            .envelope(registry, &playbook.scope, event)
            .map_err(|e| format!("start.{e}"))?;
            let producer = event.producer.clone();
            let accepted = engine
                .admit_event(&producer, event, 0)
                .map_err(|e| format!("start.payload: the event was refused: {e}."))?;
            engine
                .fanout(&accepted.key, 100, 0)
                .map(|_| ())
                .map_err(|e| format!("start: the event started no run: {e}."))
        }
    }
}

fn drive_case(
    engine: &CaseEngine,
    host: &mut CaseHost<'_>,
    limit: usize,
) -> anyhow::Result<(usize, bool)> {
    let mut clock = 0;
    let mut used = 0;
    let mut stopped = false;
    for _ in 0..MAX_CASE_ROUNDS {
        used += engine.drive(host, clock, limit - used)?;
        if host.deliver(engine, clock) {
            continue;
        }
        if used >= limit {
            stopped = true;
            break;
        }
        // Nothing more happens now: move the clock to the next deadline.
        match engine
            .snapshot()?
            .indexes
            .due
            .keys()
            .find(|at| **at > clock)
        {
            Some(at) => clock = *at,
            None => break,
        }
    }
    Ok((used, stopped))
}

/// Runs the cases a playbook is saved with: at least [`MIN_CASES`], at most
/// [`MAX_CASES`], each with its own name. Together they may take at most
/// `total_transitions` engine transitions ([`CASES_TRANSITION_BUDGET`] unless
/// the caller sets another), and each at most [`MAX_CASE_TRANSITIONS`].
/// Running out of the total is an `Err` naming the case it stopped at, never
/// a case failure and never a hang.
pub fn run_cases(
    artifact: &Artifact,
    cases: &[Case],
    total_transitions: usize,
) -> anyhow::Result<Vec<CaseReport>> {
    anyhow::ensure!(
        (MIN_CASES..=MAX_CASES).contains(&cases.len()),
        "a playbook is saved with at least {MIN_CASES} cases and at most {MAX_CASES}; {} were given. Write a case for each situation the document distinguishes.",
        cases.len()
    );
    let mut seen = BTreeSet::new();
    for case in cases {
        anyhow::ensure!(
            seen.insert(case.name.as_str()),
            "two cases are named {:?}; give each case its own name.",
            case.name
        );
    }
    artifact.verify()?;
    let mut left = total_transitions;
    let mut reports = vec![];
    for (k, case) in cases.iter().enumerate() {
        let limit = left.min(MAX_CASE_TRANSITIONS);
        let exceeded = || {
            anyhow::anyhow!(
                "the cases exceeded the total budget of {total_transitions} transitions at case {} ({:?}); together the cases may take at most that many. Make the cases or the playbook smaller.",
                k + 1,
                case.name
            )
        };
        if limit == 0 {
            return Err(exceeded());
        }
        let mut ran = run_within(artifact, case, limit)?;
        if ran.stopped {
            if limit < MAX_CASE_TRANSITIONS {
                return Err(exceeded());
            }
            stopped_at_case_cap(&mut ran.report);
        }
        left -= ran.used;
        reports.push(ran.report);
    }
    Ok(reports)
}

/// The digest of the facts a playbook was authored with (`facts.json`, as
/// the map of fact name to value), recorded on a save.
pub fn facts_digest(facts: &BTreeMap<String, Value>) -> anyhow::Result<String> {
    Ok(digest(facts)?)
}

#[cfg(test)]
mod tests;
