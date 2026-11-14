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
//! - Events are built from their contract: its first trusted producer, ids
//!   `case-<i>`, the playbook's scope, and the subject and revision read
//!   from the payload at the contract's subject and revision fields. A
//!   trigger event's correlation fields are read from the payload's
//!   top-level fields of the same names.
use aq_playbooks_model::{Artifact, ContractRef, EventEnvelope, Operation, Registry, digest};
use aq_playbooks_runner::{
    Authority, DispatchResult, Engine, Host, InspectionKind, InspectionQuery, Intent, MemoryStorage,
};
use serde::{Deserialize, Deserializer, Serialize};
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
/// Times the clock may move on to a deadline in one case.
const MAX_CASE_ROUNDS: usize = 256;

/// The outcome file the authoring agent's `save`, `ask` and `unable` write,
/// relative to the workspace, and the most bytes it may have.
pub const OUTCOME_FILE: &str = "out/outcome.json";
pub const MAX_OUTCOME_BYTES: usize = 1_048_576;
pub const MAX_QUESTION_BYTES: usize = 4096;
pub const MAX_REASON_BYTES: usize = 4096;
/// One question names at most this many sentences.
pub const MAX_SENTENCES: usize = 16;
pub const MAX_SENTENCE_BYTES: usize = 8192;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// Short and unique among the cases saved together.
    pub name: String,
    /// The situation in the owner's words, such as "a bug fix that failed once".
    pub situation: String,
    pub start: CaseStart,
    /// Per command name, what each call returns, in call order.
    #[serde(default)]
    pub returns: BTreeMap<String, Vec<CaseReturn>>,
    pub expect: Expectation,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CaseStart {
    /// The run is started by this event, through the playbook's trigger on it.
    Event { event: String, payload: Value },
    /// The run is started at this entry with this input.
    Entry { entry: String, input: Value },
}
/// `{"outcome": …, "output": …}`, or `{"pending": true}` for no answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum CaseReturn {
    Answer { outcome: String, output: Value },
    Pending { pending: bool },
}
// Written by hand so a wrong return says which shape is accepted.
impl<'de> Deserialize<'de> for CaseReturn {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let shape = "a return is an answer, {\"outcome\": \"<outcome>\", \"output\": {…}}, or no answer, {\"pending\": true}";
        let map = serde_json::Map::<String, Value>::deserialize(d)?;
        let keys: Vec<&str> = map.keys().map(String::as_str).collect();
        match keys.as_slice() {
            ["outcome", "output"] => match &map["outcome"] {
                Value::String(outcome) => Ok(Self::Answer {
                    outcome: outcome.clone(),
                    output: map["output"].clone(),
                }),
                other => Err(D::Error::custom(format!(
                    "outcome must be a string, found {other}; {shape}"
                ))),
            },
            ["pending"] => match &map["pending"] {
                Value::Bool(pending) => Ok(Self::Pending { pending: *pending }),
                other => Err(D::Error::custom(format!(
                    "pending must be true, found {other}; {shape}"
                ))),
            },
            _ => Err(D::Error::custom(format!(
                "found the fields {keys:?}; {shape}"
            ))),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expectation {
    /// The entry outcome the run must end with.
    pub outcome: String,
    /// Calls that must be made, in this order (other calls may come between).
    #[serde(default)]
    pub calls: Vec<ExpectedCall>,
}
/// A call matches when its command is `command` and its input contains
/// `input`: an object matches an object holding at least its fields (each
/// matched the same way); arrays and scalars must be equal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedCall {
    pub command: String,
    #[serde(default = "any_input")]
    pub input: Value,
}
fn any_input() -> Value {
    json!({})
}
/// A command the run called: its step, its input, and the outcome the case
/// answered with (`None` when the case gave no answer).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Call {
    pub step: String,
    pub command: String,
    pub input: Value,
    pub outcome: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseReport {
    pub name: String,
    pub situation: String,
    pub passed: bool,
    /// The outcome the run ended with, read through inspection.
    pub outcome: Option<String>,
    pub calls: Vec<Call>,
    /// Each way the run differs from the case, or the case is malformed.
    pub problems: Vec<String>,
}

fn names<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    let items: Vec<&str> = items.into_iter().collect();
    if items.is_empty() {
        "none".into()
    } else {
        items.join(", ")
    }
}
fn clip(value: &Value) -> String {
    let text = value.to_string();
    if text.len() <= 240 {
        return text;
    }
    let mut end = 240;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}
fn bounded(field: &str, text: &str, max: usize, problems: &mut Vec<String>) {
    if text.trim().is_empty() {
        problems.push(format!("{field}: is empty; write it in the owner's words."));
    } else if text.len() > max {
        problems.push(format!(
            "{field}: is {} bytes; keep it to {max}.",
            text.len()
        ));
    }
}

/// The commands the playbook's steps invoke.
fn invoked(artifact: &Artifact) -> BTreeSet<&str> {
    artifact
        .content
        .playbook
        .steps
        .values()
        .filter_map(|s| match &s.op {
            Operation::Invoke { command, .. } => Some(command.name.as_str()),
            _ => None,
        })
        .collect()
}

/// What is wrong with the case itself, before it is run.
fn check_case(artifact: &Artifact, case: &Case) -> Vec<String> {
    let mut problems = vec![];
    let p = &mut problems;
    bounded("name", &case.name, MAX_CASE_NAME_BYTES, p);
    bounded("situation", &case.situation, MAX_SITUATION_BYTES, p);
    let playbook = &artifact.content.playbook;
    let registry = &artifact.content.registry;
    let called = invoked(artifact);
    let entries: BTreeSet<&str> = match &case.start {
        CaseStart::Entry { entry, .. } => {
            if !playbook.entries.contains_key(entry) {
                p.push(format!(
                    "start.entry: the playbook has no entry {entry:?}; its entries are: {}.",
                    names(playbook.entries.keys().map(String::as_str))
                ));
            }
            BTreeSet::from([entry.as_str()])
        }
        CaseStart::Event { event, .. } => {
            let started: BTreeSet<&str> = playbook
                .triggers
                .iter()
                .filter(|t| &t.event.name == event)
                .map(|t| t.entry.as_str())
                .collect();
            if started.is_empty() {
                p.push(format!(
                    "start.event: the playbook has no trigger on {event:?}; it starts on: {}. Start the case from an event a trigger names, or from an entry: {{\"kind\": \"entry\", \"entry\": \"main\", \"input\": {{…}}}}.",
                    names(playbook.triggers.iter().map(|t| t.event.name.as_str()))
                ));
            }
            started
        }
    };
    for (command, returns) in &case.returns {
        let at = format!("returns.{command:?}");
        let Some(contract) = registry
            .commands
            .get(command)
            .filter(|_| called.contains(command.as_str()))
        else {
            p.push(format!(
                "{at}: the playbook never calls {command}; it calls: {}. Give returns only for those.",
                names(called.iter().copied())
            ));
            continue;
        };
        if returns.len() > MAX_RETURNS_PER_COMMAND {
            p.push(format!(
                "{at}: gives {} returns; a case gives at most {MAX_RETURNS_PER_COMMAND} for one command.",
                returns.len()
            ));
        }
        let answers = || {
            names(
                contract
                    .outcomes
                    .keys()
                    .map(String::as_str)
                    .filter(|o| *o != "accepted"),
            )
        };
        for (i, given) in returns.iter().enumerate() {
            let at = format!("{at}[{i}]");
            match given {
                CaseReturn::Pending { pending } => {
                    if !pending {
                        p.push(format!(
                            "{at}.pending: write {{\"pending\": true}} for a call that never gets an answer, or give its answer."
                        ));
                    }
                    if contract.asynchronous.is_none() {
                        p.push(format!(
                            "{at}: {command} answers at once, so it cannot be left pending; give its outcome and output (one of: {}).",
                            answers()
                        ));
                    }
                }
                CaseReturn::Answer { outcome, output } => match &contract.asynchronous {
                    None => match contract.outcomes.get(outcome) {
                        None => p.push(format!(
                            "{at}.outcome: {outcome:?} is not an outcome of {command}; its outcomes are: {}.",
                            answers()
                        )),
                        Some(schema) => {
                            if let Err(e) = schema.validate(output) {
                                p.push(format!(
                                    "{at}.output: does not fit {command}'s {outcome:?} output: {e}. `aq-playbooks commands describe {command}` shows each outcome's output."
                                ));
                            }
                        }
                    },
                    Some(asynchronous) => {
                        let payload = json!({"outcome": outcome, "output": output});
                        if let Err(e) = asynchronous.completion.validate(&payload) {
                            p.push(format!(
                                "{at}: is not an answer {command} can give: {e}. Give the answer itself, one of: {} (the tool returns `accepted` with a handle first, by itself). `aq-playbooks commands describe {command}` shows each outcome's output.",
                                answers()
                            ));
                        }
                    }
                },
            }
        }
    }
    for (i, call) in case.expect.calls.iter().enumerate() {
        if !called.contains(call.command.as_str()) {
            p.push(format!(
                "expect.calls[{i}].command: the playbook never calls {:?}; it calls: {}.",
                call.command,
                names(called.iter().copied())
            ));
        }
    }
    let outcomes: BTreeSet<&str> = playbook
        .entries
        .iter()
        .filter(|(name, _)| entries.contains(name.as_str()))
        .flat_map(|(_, entry)| entry.outcomes.keys().map(String::as_str))
        .collect();
    if !outcomes.is_empty() && !outcomes.contains(case.expect.outcome.as_str()) {
        p.push(format!(
            "expect.outcome: the run cannot end {:?}; it ends with one of: {}.",
            case.expect.outcome,
            names(outcomes)
        ));
    }
    problems
}

fn field<'v>(value: &'v Value, dotted: &str) -> Option<&'v Value> {
    dotted.split('.').try_fold(value, |v, key| v.get(key))
}
/// An envelope for `event`, built from its contract.
fn envelope(
    registry: &Registry,
    scope: &str,
    event: &str,
    id: String,
    correlation: BTreeMap<String, String>,
    payload: Value,
) -> Result<EventEnvelope, String> {
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
            format!("payload.{at}: {event} carries its revision here, so it must be a whole number")
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
                    let completion = envelope(
                        self.registry,
                        &self.scope,
                        &asynchronous.completion_event,
                        id,
                        BTreeMap::from([(asynchronous.correlation_field.clone(), handle.clone())]),
                        json!({"outcome": outcome, "output": output}),
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

fn inspected<S: aq_playbooks_runner::Storage>(
    engine: &Engine<S>,
    kind: InspectionKind,
) -> anyhow::Result<Vec<Value>> {
    let mut items = vec![];
    let mut after = None;
    loop {
        let page = engine.inspect(&InspectionQuery {
            kind,
            run: None,
            after,
            limit: 1000,
        })?;
        items.extend(page.items);
        match page.next {
            Some(next) => after = Some(next),
            None => return Ok(items),
        }
    }
}

/// The outcome a live run has returned while it finishes.
fn returned<S: aq_playbooks_runner::Storage>(
    engine: &Engine<S>,
    run: &Value,
) -> anyhow::Result<Option<String>> {
    let id = run["id"].as_str().unwrap_or_default();
    Ok(engine
        .snapshot()?
        .runs
        .get(id)
        .and_then(|r| r.pending_return.as_ref())
        .map(|(outcome, _)| outcome.clone()))
}

/// The first place `got` does not contain `want`, or `None`.
fn mismatch(want: &Value, got: &Value, path: &str) -> Option<String> {
    match (want, got) {
        (Value::Object(want), Value::Object(got)) => want.iter().find_map(|(key, w)| {
            let at = format!("{path}.{key}");
            match got.get(key) {
                None => Some(format!("{at} is missing")),
                Some(g) => mismatch(w, g, &at),
            }
        }),
        _ if want == got => None,
        _ => Some(format!(
            "{path}: expected {}, found {}",
            clip(want),
            clip(got)
        )),
    }
}
fn expected_calls(expect: &[ExpectedCall], calls: &[Call], problems: &mut Vec<String>) {
    let made = names(calls.iter().map(|c| c.command.as_str()));
    let mut next = 0;
    for (i, want) in expect.iter().enumerate() {
        let found = calls[next..].iter().position(|c| {
            c.command == want.command && mismatch(&want.input, &c.input, "input").is_none()
        });
        if let Some(at) = found {
            next += at + 1;
            continue;
        }
        let later: Vec<&Call> = calls[next..]
            .iter()
            .filter(|c| c.command == want.command)
            .collect();
        let why = if let Some(first) = later.first() {
            format!(
                "the call at step {} differs: {}",
                first.step,
                mismatch(&want.input, &first.input, "input").unwrap_or_default()
            )
        } else if calls.iter().any(|c| c.command == want.command) {
            format!(
                "{} was called only before the calls expected ahead of it (expected calls are matched in order)",
                want.command
            )
        } else {
            format!("the playbook never called {}", want.command)
        };
        problems.push(format!(
            "expect.calls[{i}]: no call to {} with an input containing {}: {why}. The calls made, in order: {made}.",
            want.command,
            clip(&want.input)
        ));
    }
}

/// Runs one case on `artifact`. An `Err` is a fault of the artifact or the
/// engine; everything about the case is in the report's `problems`.
pub fn run_case(artifact: &Artifact, case: &Case) -> anyhow::Result<CaseReport> {
    artifact.verify()?;
    let mut report = CaseReport {
        name: case.name.clone(),
        situation: case.situation.clone(),
        passed: false,
        outcome: None,
        calls: vec![],
        problems: check_case(artifact, case),
    };
    if !report.problems.is_empty() {
        return Ok(report);
    }
    let playbook = &artifact.content.playbook;
    let registry = &artifact.content.registry;
    let engine = Engine::new(MemoryStorage::default(), "case", "case-runner")?;
    engine.install(artifact.clone())?;
    let authority = Authority {
        actor: "case".into(),
        scope: playbook.scope.clone(),
        capabilities: playbook.required_capabilities.clone(),
    };
    engine.activate(&playbook.id, &artifact.digest, authority.clone())?;
    let mut host = CaseHost {
        registry,
        scope: playbook.scope.clone(),
        returns: case
            .returns
            .iter()
            .map(|(k, v)| (k.clone(), v.iter().cloned().collect()))
            .collect(),
        calls: vec![],
        problems: vec![],
        completions: vec![],
        handles: 0,
    };
    let started = match &case.start {
        CaseStart::Entry { entry, input } => engine
            .admit(&playbook.id, entry, input.clone(), authority, 0)
            .map(|_| ())
            .map_err(|e| format!("start.input: the entry {entry} refused it: {e}.")),
        CaseStart::Event { event, payload } => (|| {
            let contract = &registry.events[event];
            let mut correlation = BTreeMap::new();
            for name in &contract.correlation_fields {
                let value = payload.get(name).and_then(Value::as_str).ok_or_else(|| {
                    format!("start.payload.{name}: {event} is correlated by {name}, so the payload must give it as a string")
                })?;
                correlation.insert(name.clone(), value.to_string());
            }
            let event = envelope(
                registry,
                &playbook.scope,
                event,
                "case-0".into(),
                correlation,
                payload.clone(),
            )
            .map_err(|e| format!("start.{e}"))?;
            let producer = event.producer.clone();
            let accepted = engine
                .admit_event(&producer, event, 0)
                .map_err(|e| format!("start.payload: the event was refused: {e}."))?;
            engine
                .fanout(&accepted.key, 100, 0)
                .map(|_| ())
                .map_err(|e| format!("start: the event started no run: {e}."))
        })(),
    };
    if let Err(problem) = started {
        report.problems.push(problem);
        return Ok(report);
    }
    let mut clock = 0;
    let mut used = 0;
    for _ in 0..MAX_CASE_ROUNDS {
        used += engine.drive(&mut host, clock, MAX_CASE_TRANSITIONS - used)?;
        let completions = std::mem::take(&mut host.completions);
        if !completions.is_empty() {
            for event in completions {
                let producer = event.producer.clone();
                if let Err(e) = engine.admit_event(&producer, event, clock) {
                    host.problems
                        .push(format!("an answer's completion event was refused: {e}."));
                }
            }
            continue;
        }
        if used >= MAX_CASE_TRANSITIONS {
            host.problems.push(format!(
                "the run took more than {MAX_CASE_TRANSITIONS} steps; look for a loop that never ends."
            ));
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
    report.calls = std::mem::take(&mut host.calls);
    report.problems = std::mem::take(&mut host.problems);
    // Finished runs have left the state: read them through inspection.
    let runs = inspected(&engine, InspectionKind::Runs)?;
    let roots: Vec<&Value> = runs.iter().filter(|r| r["parent"].is_null()).collect();
    let at = |run: &Value| run["step"].as_str().unwrap_or_default().to_string();
    match roots.as_slice() {
        [] => report
            .problems
            .push("start: no run started.".to_string()),
        [run] => match run["status"].as_str() {
            Some("completed") => report.outcome = run["outcome"].as_str().map(String::from),
            Some("failed") => report.problems.push(format!(
                "the run failed at step {} without an outcome; give each step's command a return that fits it, and an `on_error` edge where a call may fail.",
                at(run)
            )),
            Some("cancelled") => report
                .problems
                .push(format!("the run was cancelled at step {}.", at(run))),
            // It has returned, and waits only for an operation it started to
            // settle (an answer the case leaves pending): it is still live,
            // so the outcome it returned is read from the state.
            Some("finishing") if returned(&engine, run)?.is_some() => {
                report.outcome = returned(&engine, run)?
            }
            status => report.problems.push(format!(
                "the run did not finish: it is {} at step {}. A case runs until nothing more can happen; give the returns this step needs.",
                status.unwrap_or("unknown"),
                at(run)
            )),
        },
        many => report.problems.push(format!(
            "start: the event started {} runs; a case follows one run, so start it from an event one trigger names, or from an entry.",
            many.len()
        )),
    }
    if let Some(outcome) = &report.outcome
        && *outcome != case.expect.outcome
    {
        let step = roots.first().map(|r| at(r)).unwrap_or_default();
        report.problems.push(format!(
            "expect.outcome: the case expects the run to end {:?}, but it ended {outcome:?} (at step {step}).",
            case.expect.outcome
        ));
    }
    expected_calls(&case.expect.calls, &report.calls, &mut report.problems);
    report.passed = report.problems.is_empty();
    Ok(report)
}

/// Runs the cases a playbook is saved with: at least [`MIN_CASES`], at most
/// [`MAX_CASES`], each with its own name.
pub fn run_cases(artifact: &Artifact, cases: &[Case]) -> anyhow::Result<Vec<CaseReport>> {
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
    cases.iter().map(|case| run_case(artifact, case)).collect()
}

/// The digest of the facts a playbook was authored with (`facts.json`, as
/// the map of fact name to value), recorded on a save.
pub fn facts_digest(facts: &BTreeMap<String, Value>) -> anyhow::Result<String> {
    Ok(digest(facts)?)
}

/// A line of `source.md` and its text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sentence {
    pub line: u32,
    pub text: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    Saved,
    Question,
    Unable,
}
/// `out/outcome.json`: how an authoring run ended. Data for the host to
/// check again, never an instruction: a host re-seals the draft from its own
/// record, catalog, facts and document, and re-runs the cases.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    /// The draft and its cases; the digest of the artifact the agent's tools
    /// sealed and of the facts it was authored with.
    Saved {
        draft: Value,
        cases: Vec<Case>,
        artifact: String,
        facts_digest: String,
    },
    /// A question for the owner, about these sentences of the document.
    Question {
        question: String,
        sentences: Vec<Sentence>,
    },
    /// Why the document could not be turned into a playbook, and the
    /// sentence it is about.
    Unable {
        reason: String,
        line: u32,
        sentence: String,
    },
}
impl Outcome {
    pub fn kind(&self) -> OutcomeKind {
        match self {
            Self::Saved { .. } => OutcomeKind::Saved,
            Self::Question { .. } => OutcomeKind::Question,
            Self::Unable { .. } => OutcomeKind::Unable,
        }
    }
    /// Every bound the outcome must meet, except its total size.
    pub fn check(&self) -> Result<(), String> {
        let text = |field: &str, value: &str, max: usize| {
            if value.trim().is_empty() {
                Err(format!("{field} is empty"))
            } else if value.len() > max {
                Err(format!("{field} is {} bytes; at most {max}", value.len()))
            } else {
                Ok(())
            }
        };
        let digest = |field: &str, value: &str| {
            if value.starts_with("sha256:") && value.len() == 71 {
                Ok(())
            } else {
                Err(format!("{field} is not a sha256: digest"))
            }
        };
        match self {
            Self::Saved {
                cases,
                artifact,
                facts_digest,
                ..
            } => {
                if !(MIN_CASES..=MAX_CASES).contains(&cases.len()) {
                    return Err(format!(
                        "a save has {} cases; at least {MIN_CASES} and at most {MAX_CASES}",
                        cases.len()
                    ));
                }
                digest("artifact", artifact)?;
                digest("facts_digest", facts_digest)
            }
            Self::Question {
                question,
                sentences,
            } => {
                text("question", question, MAX_QUESTION_BYTES)?;
                if sentences.is_empty() || sentences.len() > MAX_SENTENCES {
                    return Err(format!(
                        "a question names 1 to {MAX_SENTENCES} sentences; it names {}",
                        sentences.len()
                    ));
                }
                let mut lines = BTreeSet::new();
                for s in sentences {
                    if s.line == 0 || !lines.insert(s.line) {
                        return Err(format!("sentence line {} is zero or named twice", s.line));
                    }
                    text("sentence text", &s.text, MAX_SENTENCE_BYTES)?;
                }
                Ok(())
            }
            Self::Unable {
                reason,
                line,
                sentence,
            } => {
                text("reason", reason, MAX_REASON_BYTES)?;
                if *line == 0 {
                    return Err("line is zero".into());
                }
                text("sentence", sentence, MAX_SENTENCE_BYTES)
            }
        }
    }
    /// The file's bytes: checked, and at most [`MAX_OUTCOME_BYTES`].
    pub fn to_bytes(&self) -> anyhow::Result<Vec<u8>> {
        self.check().map_err(anyhow::Error::msg)?;
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        anyhow::ensure!(
            bytes.len() <= MAX_OUTCOME_BYTES,
            "the outcome is {} bytes; {OUTCOME_FILE} may be at most {MAX_OUTCOME_BYTES}. Shorten the draft or the cases.",
            bytes.len()
        );
        Ok(bytes)
    }
    /// Reads an outcome file: at most [`MAX_OUTCOME_BYTES`], strict, checked.
    pub fn from_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            bytes.len() <= MAX_OUTCOME_BYTES,
            "the outcome is {} bytes; at most {MAX_OUTCOME_BYTES}",
            bytes.len()
        );
        let outcome: Self = serde_json::from_slice(bytes)?;
        outcome.check().map_err(anyhow::Error::msg)?;
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aq_playbooks_compiler::draft::{SourceRecord, Workspace, seal};
    use aq_playbooks_model::Registry;
    use serde_json::json;

    const REGISTRY_2_2: &str = include_str!("../../../contracts/aq-host-2.2.0/registry.json");
    const ROUTING_DRAFT: &str =
        include_str!("../../aq-playbooks-cli/tests/data/routing_draft.json");

    fn strings(xs: &[&str]) -> std::collections::BTreeSet<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }
    /// The owner's routing document as a draft (the A4 feasibility draft),
    /// sealed against catalog 2.2.0 with the owner's Settings as facts.
    fn routing() -> Artifact {
        let everyone = json!({"harnesses": ["claude", "codex", "gemini", "opencode"],
                              "providers": ["anthropic", "google", "ollama", "openai"]});
        let ws = Workspace {
            markdown: "# Routing\n\nThe owner's routing document.\n".into(),
            record: SourceRecord {
                id: "routing".into(),
                binding: "aq.routing".into(),
                scope: "project".into(),
                purpose: "Route each task".into(),
                commands: strings(&["work.read", "work.route", "judgment.assess"]),
                events: strings(&["aq.work.route_needed"]),
                capabilities: strings(&[
                    "work.read",
                    "work.route",
                    "judgment.assess",
                    "operation.get",
                ]),
                facts_version: "settings-1".into(),
            },
            catalog: serde_json::from_str::<Registry>(REGISTRY_2_2).unwrap(),
            facts: BTreeMap::from([
                (
                    "levels".into(),
                    json!(["simple-high", "standard-high", "complex-high", "deep-high"]),
                ),
                (
                    "offers".into(),
                    json!({"simple-high": everyone, "standard-high": everyone,
                           "complex-high": everyone, "deep-high": everyone}),
                ),
                (
                    "harnesses".into(),
                    json!(["claude", "codex", "gemini", "opencode"]),
                ),
                (
                    "providers".into(),
                    json!(["anthropic", "google", "ollama", "openai"]),
                ),
            ]),
            draft: serde_json::from_str(ROUTING_DRAFT).unwrap(),
        };
        seal(&ws).unwrap_or_else(|d| panic!("{d:#?}"))
    }
    fn subject() -> Value {
        json!({"id": "task-1", "revision": 3})
    }
    fn observed(filed_kind: &str) -> CaseReturn {
        CaseReturn::Answer {
            outcome: "observed".into(),
            output: json!({"reference": "task-1@3", "subject": subject(), "facts": {
                "kind": filed_kind, "title": "A task", "description": "", "origin": "owner",
                "level_hint": "", "charged_failures": 0, "ended_attempts": 0, "route": "none"}}),
        }
    }
    fn assessed(kind: &str, risk: &str, flags: &[&str]) -> CaseReturn {
        CaseReturn::Answer {
            outcome: "assessed".into(),
            output: json!({"reference": "judgment:op-1", "subject": subject(),
                "labels": [kind, risk], "flags": flags, "rationale": "Because.",
                "provenance": {"context_digest": "sha256:a", "credential_profile": "profile/a",
                    "model": "model-a", "output_digest": null, "prompt_digest": "sha256:c",
                    "provider": "provider-a",
                    "usage": {"cost_microunits": 1, "input_tokens": 1, "output_tokens": 1}}}),
        }
    }
    fn routed() -> CaseReturn {
        CaseReturn::Answer {
            outcome: "routed".into(),
            output: json!({"reference": "route-1", "subject": subject(),
                "harnesses": [], "providers": [], "prefer": []}),
        }
    }
    fn case(returns: Vec<(&str, Vec<CaseReturn>)>, outcome: &str) -> Case {
        Case {
            name: "a task".into(),
            situation: "A task needs a route".into(),
            start: CaseStart::Entry {
                entry: "main".into(),
                input: json!({"task": subject()}),
            },
            returns: returns
                .into_iter()
                .map(|(c, r)| (c.to_string(), r))
                .collect(),
            expect: Expectation {
                outcome: outcome.into(),
                calls: vec![],
            },
        }
    }
    fn classified(filed: &str, kind: &str, risk: &str, flags: &[&str]) -> Case {
        case(
            vec![
                ("work.read", vec![observed(filed)]),
                ("judgment.assess", vec![assessed(kind, risk, flags)]),
                ("work.route", vec![routed()]),
            ],
            "routed",
        )
    }
    fn route_call(report: &CaseReport) -> &Call {
        let routes: Vec<&Call> = report
            .calls
            .iter()
            .filter(|c| c.command == "work.route")
            .collect();
        assert_eq!(routes.len(), 1, "{report:#?}");
        routes[0]
    }

    #[test]
    fn a_case_records_each_call_with_its_input() {
        let artifact = routing();
        let report = run_case(&artifact, &classified("feature", "feature", "low", &[])).unwrap();
        assert!(report.passed, "{report:#?}");
        assert_eq!(report.outcome.as_deref(), Some("routed"));
        let seen: Vec<(&str, &str, Option<&str>)> = report
            .calls
            .iter()
            .map(|c| (c.step.as_str(), c.command.as_str(), c.outcome.as_deref()))
            .collect();
        assert_eq!(
            seen,
            [
                ("read", "work.read", Some("observed")),
                ("ask", "judgment.assess", Some("assessed")),
                ("route", "work.route", Some("routed")),
            ]
        );
        assert_eq!(report.calls[0].input, json!({"subject": subject()}));
        assert_eq!(report.calls[1].input["subject"], subject());
        assert_eq!(
            report.calls[1].input["context"]["items"][0]["text"],
            "A task"
        );
        let route = &report.calls[2].input;
        assert_eq!(route["subject"], subject());
        assert_eq!(route["level"], "standard-high");
        assert_eq!(route["classification"]["labels"], json!(["feature", "low"]));
        assert!(report.problems.is_empty(), "{report:#?}");
    }

    /// The A2 runtime proof: the route's level comes from the judgment's
    /// first label, read through the named join, not from the kind the
    /// filer gave (always "feature" here).
    #[test]
    fn an_async_answer_reaches_the_playbook_through_the_join() {
        let artifact = routing();
        for (kind, flags, level) in [
            ("chore", &[][..], "simple-high"),
            ("feature", &[][..], "standard-high"),
            ("research", &["clearly_harder"][..], "complex-high"),
            ("spec", &["needs_deep_reasoning"][..], "deep-high"),
        ] {
            let report = run_case(&artifact, &classified("feature", kind, "low", flags)).unwrap();
            assert!(report.passed, "{kind}: {report:#?}");
            let route = route_call(&report);
            assert_eq!(route.step, "route", "{kind}");
            assert_eq!(route.input["level"], level, "{kind}");
            assert_eq!(route.input["classification"]["labels"][0], kind);
        }
    }

    #[test]
    fn a_pending_answer_takes_the_timeout_path() {
        let artifact = routing();
        let mut pending = case(
            vec![
                ("work.read", vec![observed("bugfix")]),
                (
                    "judgment.assess",
                    vec![CaseReturn::Pending { pending: true }],
                ),
                ("work.route", vec![routed()]),
            ],
            "routed",
        );
        pending.expect.calls = vec![ExpectedCall {
            command: "work.route".into(),
            input: json!({"level": "standard-high", "harnesses": ["claude", "codex"],
                          "classification": null}),
        }];
        let report = run_case(&artifact, &pending).unwrap();
        assert!(report.passed, "{report:#?}");
        let ask = report
            .calls
            .iter()
            .find(|c| c.command == "judgment.assess")
            .unwrap();
        assert_eq!(ask.outcome, None, "no answer came");
        let route = route_call(&report);
        assert_eq!(route.step, "assume");
        let codes: Vec<&Value> = route.input["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| &r["code"])
            .collect();
        assert!(codes.contains(&&json!("risk_assumed")), "{codes:?}");
    }

    #[test]
    fn a_call_without_a_return_fails_the_case_and_names_the_step() {
        let artifact = routing();
        let missing = case(
            vec![
                ("work.read", vec![observed("feature")]),
                ("judgment.assess", vec![assessed("feature", "low", &[])]),
            ],
            "routed",
        );
        let report = run_case(&artifact, &missing).unwrap();
        assert!(!report.passed, "{report:#?}");
        assert!(
            report.problems.iter().any(|p| p
                .starts_with("step route called work.route but the case gives no return for it")),
            "{report:#?}"
        );
        let route = route_call(&report);
        assert_eq!(route.outcome, None);
        assert_eq!(
            route.input["level"], "standard-high",
            "the input is still recorded"
        );
    }

    #[test]
    fn an_expected_call_matches_by_subset_and_order() {
        let artifact = routing();
        let read = ExpectedCall {
            command: "work.read".into(),
            input: json!({"subject": {"id": "task-1"}}),
        };
        let route = ExpectedCall {
            command: "work.route".into(),
            input: json!({"level": "complex-high", "harnesses": ["claude", "codex"]}),
        };
        let mut c = classified("feature", "design", "low", &[]);
        // An object is matched by subset, an array exactly, and in order.
        c.expect.calls = vec![read.clone(), route.clone()];
        let report = run_case(&artifact, &c).unwrap();
        assert!(report.passed, "{report:#?}");
        // Out of order.
        c.expect.calls = vec![route.clone(), read.clone()];
        let report = run_case(&artifact, &c).unwrap();
        assert!(!report.passed);
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.starts_with("expect.calls[1]")),
            "{report:#?}"
        );
        // An array is not matched by subset.
        let mut fewer = route.clone();
        fewer.input["harnesses"] = json!(["claude"]);
        c.expect.calls = vec![fewer];
        let report = run_case(&artifact, &c).unwrap();
        assert!(!report.passed);
        let problem = report
            .problems
            .iter()
            .find(|p| p.starts_with("expect.calls[0]"))
            .unwrap_or_else(|| panic!("{report:#?}"));
        assert!(problem.contains("input.harnesses"), "{problem}");
        // A scalar is matched exactly.
        let mut other = route;
        other.input["level"] = json!("deep-high");
        c.expect.calls = vec![other];
        let report = run_case(&artifact, &c).unwrap();
        assert!(!report.passed);
        assert!(
            report.problems.iter().any(|p| p.contains("input.level")),
            "{report:#?}"
        );
    }

    #[test]
    fn a_triggered_case_starts_from_the_event() {
        let artifact = routing();
        let mut c = classified("feature", "chore", "low", &[]);
        c.start = CaseStart::Event {
            event: "aq.work.route_needed".into(),
            payload: json!({"subject": {"id": "task-1", "revision": 2}}),
        };
        let report = run_case(&artifact, &c).unwrap();
        assert!(report.passed, "{report:#?}");
        // The trigger's input is the event's subject.
        assert_eq!(
            report.calls[0].input,
            json!({"subject": {"id": "task-1", "revision": 2}})
        );
        // An event the playbook has no trigger for starts nothing.
        c.start = CaseStart::Event {
            event: "aq.judgment.assessed".into(),
            payload: json!({}),
        };
        let report = run_case(&artifact, &c).unwrap();
        assert!(!report.passed);
        assert!(!report.problems.is_empty());
    }

    #[test]
    fn fewer_than_five_or_more_than_forty_cases_are_refused() {
        let artifact = routing();
        let one = classified("feature", "feature", "low", &[]);
        let named = |n: usize| {
            (0..n)
                .map(|i| Case {
                    name: format!("case {i}"),
                    ..one.clone()
                })
                .collect::<Vec<_>>()
        };
        for n in [0, 4, 41] {
            let error = run_cases(&artifact, &named(n)).unwrap_err().to_string();
            assert!(
                error.contains("at least 5") && error.contains("at most 40"),
                "{error}"
            );
        }
        for n in [MIN_CASES, MAX_CASES] {
            let reports = run_cases(&artifact, &named(n)).unwrap();
            assert_eq!(reports.len(), n);
            assert!(reports.iter().all(|r| r.passed));
        }
        // Each case has its own name.
        let mut twins = named(5);
        twins[1].name = twins[0].name.clone();
        let error = run_cases(&artifact, &twins).unwrap_err().to_string();
        assert!(error.contains("case 0"), "{error}");
    }
}
