//! Validate simulated calls, returns, and expected outcomes before running a case.
use super::*;
pub(super) fn check_case(artifact: &Artifact, case: &Case) -> Vec<String> {
    let mut problems = vec![];
    bounded("name", &case.name, MAX_CASE_NAME_BYTES, &mut problems);
    bounded(
        "situation",
        &case.situation,
        MAX_SITUATION_BYTES,
        &mut problems,
    );
    let called = invoked(artifact);
    let entries = start_entries(&case.start, artifact, &mut problems);
    check_returns(case, &artifact.content.registry, &called, &mut problems);
    let outcomes = entry_outcomes(&artifact.content.playbook, &entries);
    check_expectation(case, &called, &outcomes, &mut problems);
    problems
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

fn start_entries<'a>(
    start: &'a CaseStart,
    artifact: &'a Artifact,
    p: &mut Vec<String>,
) -> BTreeSet<&'a str> {
    let playbook = &artifact.content.playbook;
    let registry = &artifact.content.registry;
    match start {
        CaseStart::Entry { entry, .. } => {
            if !playbook.entries.contains_key(entry) {
                p.push(format!(
                    "start.entry: the playbook has no entry {entry:?}; its entries are: {}.",
                    names(playbook.entries.keys().map(String::as_str))
                ));
            }
            BTreeSet::from([entry.as_str()])
        }
        CaseStart::Event {
            event, correlation, ..
        } => {
            if let Some(contract) = registry.events.get(event) {
                let fields = &contract.correlation_fields;
                check_correlation(event, correlation, fields, p);
            }
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
    }
}
fn check_correlation(
    event: &str,
    correlation: &BTreeMap<String, String>,
    fields: &BTreeSet<String>,
    p: &mut Vec<String>,
) {
    let missing: Vec<&str> = fields
        .iter()
        .filter(|f| correlation.get(*f).is_none_or(|v| v.is_empty()))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        p.push(format!(
                        "start.correlation: {event} is correlated by {}; give each a value, such as \"correlation\": {{{}}}.",
                        names(missing.iter().copied()),
                        missing
                            .iter()
                            .map(|f| format!("\"{f}\": \"op-1\""))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
    }
    for extra in correlation.keys().filter(|k| !fields.contains(*k)) {
        p.push(format!(
                        "start.correlation.{extra}: {event} has no correlation field {extra}; its correlation fields are: {}.",
                        names(fields.iter().map(String::as_str))
                    ));
    }
}
fn check_returns(case: &Case, registry: &Registry, called: &BTreeSet<&str>, p: &mut Vec<String>) {
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
        for (i, given) in returns.iter().enumerate() {
            let mut check = ReturnCheck {
                contract,
                command,
                at: format!("{at}[{i}]"),
                problems: p,
            };
            match given {
                CaseReturn::Pending { pending } => check.pending(*pending),
                CaseReturn::AtOnce {
                    outcome,
                    output,
                    at_once,
                } => check.immediate(outcome, output, *at_once),
                CaseReturn::Answer { outcome, output } => check.answer(outcome, output),
            }
        }
    }
}

struct ReturnCheck<'a> {
    contract: &'a aq_playbooks_model::CommandContract,
    command: &'a str,
    at: String,
    problems: &'a mut Vec<String>,
}
impl ReturnCheck<'_> {
    fn answers(&self) -> String {
        names(
            self.contract
                .outcomes
                .keys()
                .map(String::as_str)
                .filter(|o| *o != "accepted"),
        )
    }
    fn pending(&mut self, pending: bool) {
        let (contract, command, at) = (self.contract, self.command, &self.at);
        if !pending {
            self.problems.push(format!(
                            "{at}.pending: write {{\"pending\": true}} for a call that never gets an answer, or give its answer."
                        ));
        }
        if contract.asynchronous.is_none() {
            self.problems.push(format!(
                            "{at}: {command} answers at once, so it cannot be left pending; give its outcome and output (one of: {}).",
                            self.answers()
                        ));
        }
    }
    fn immediate(&mut self, outcome: &str, output: &Value, at_once: bool) {
        let (contract, command, at) = (self.contract, self.command, &self.at);
        if !at_once {
            self.problems.push(format!(
                            "{at}.at_once: write \"at_once\": true, or leave it out for an answer that arrives later."
                        ));
        }
        match (&contract.asynchronous, contract.outcomes.get(outcome)) {
                        (None, _) => self.problems.push(format!(
                            "{at}.at_once: {command} always answers at once; leave at_once out."
                        )),
                        (Some(_), None) if outcome != "accepted" => self.problems.push(format!(
                            "{at}.outcome: {outcome:?} is not an outcome of {command}; it can answer at once with: {}.",
                            self.answers()
                        )),
                        (Some(_), _) if outcome == "accepted" => self.problems.push(format!(
                            "{at}.outcome: an answer at once is one of {command}'s own outcomes other than accepted: {}. For an answer that arrives later, leave at_once out.",
                            self.answers()
                        )),
                        (Some(_), Some(schema)) => {
                            if let Err(e) = schema.validate(output) {
                                self.problems.push(format!(
                                    "{at}.output: does not fit {command}'s {outcome:?} output: {e}. `aq-playbooks commands describe {command}` shows each outcome's output."
                                ));
                            }
                        }
                        (Some(_), None) => {}
                    }
    }
    fn answer(&mut self, outcome: &str, output: &Value) {
        let (contract, command, at) = (self.contract, self.command, &self.at);
        match &contract.asynchronous {
                    None => match contract.outcomes.get(outcome) {
                        None => self.problems.push(format!(
                            "{at}.outcome: {outcome:?} is not an outcome of {command}; its outcomes are: {}.",
                            self.answers()
                        )),
                        Some(schema) => {
                            if let Err(e) = schema.validate(output) {
                                self.problems.push(format!(
                                    "{at}.output: does not fit {command}'s {outcome:?} output: {e}. `aq-playbooks commands describe {command}` shows each outcome's output."
                                ));
                            }
                        }
                    },
                    Some(asynchronous) => {
                        let payload = json!({"outcome": outcome, "output": output});
                        if let Err(e) = asynchronous.completion.validate(&payload) {
                            self.problems.push(format!(
                                "{at}: is not an answer {command} can give: {e}. Give the answer itself, one of: {} (the tool returns `accepted` with a handle first, by itself). `aq-playbooks commands describe {command}` shows each outcome's output.",
                                self.answers()
                            ));
                        }
                    }
                }
    }
}
fn check_expectation(
    case: &Case,
    called: &BTreeSet<&str>,
    outcomes: &BTreeSet<&str>,
    p: &mut Vec<String>,
) {
    for (i, call) in case.expect.calls.iter().enumerate() {
        if !called.contains(call.command.as_str()) {
            p.push(format!(
                "expect.calls[{i}].command: the playbook never calls {:?}; it calls: {}.",
                call.command,
                names(called.iter().copied())
            ));
        }
    }
    if !outcomes.is_empty() && !outcomes.contains(case.expect.outcome.as_str()) {
        p.push(format!(
            "expect.outcome: the run cannot end {:?}; it ends with one of: {}.",
            case.expect.outcome,
            names(outcomes.iter().copied())
        ));
    }
}

fn entry_outcomes<'a>(
    playbook: &'a aq_playbooks_model::Playbook,
    entries: &BTreeSet<&str>,
) -> BTreeSet<&'a str> {
    playbook
        .entries
        .iter()
        .filter(|(name, _)| entries.contains(name.as_str()))
        .flat_map(|(_, entry)| entry.outcomes.keys().map(String::as_str))
        .collect()
}
