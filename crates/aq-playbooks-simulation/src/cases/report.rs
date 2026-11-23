//! Compare calls and inspect the final outcome of a simulated case.
use super::*;
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

/// Why a failed run stopped, when it stopped on one of the playbook's own
/// limits: the engine records an `engine_failure` for the run, and its time
/// and the run's visits tell which limit it was.
fn own_limit<S: aq_playbooks_runner::Storage>(
    engine: &Engine<S>,
    artifact: &Artifact,
    run: &Value,
) -> anyhow::Result<Option<String>> {
    let receipts = inspected(engine, InspectionKind::Receipts)?;
    let Some(failure) = receipts
        .iter()
        .find(|r| r["run"] == run["id"] && r["kind"] == "engine_failure")
    else {
        return Ok(None);
    };
    let limits = &artifact.content.playbook.limits;
    let step = failure["step"].as_str().unwrap_or_default();
    let elapsed =
        failure["at_ms"].as_i64().unwrap_or(0) - run["admitted_at_ms"].as_i64().unwrap_or(0);
    let visits = run["visits"].as_u64().unwrap_or(0);
    Ok(Some(if elapsed >= limits.max_duration_ms as i64 {
        format!(
            "the run stopped at step {step} on the playbook's own duration limit: it had run {elapsed} ms, and `limits.max_duration_ms` is {}. The limit covers the whole run from its start, so it must be longer than every wait and join deadline on the longest way through the run. Set it from what the document says (a wait of a day needs a limit above a day).",
            limits.max_duration_ms
        )
    } else if visits >= limits.max_step_visits {
        format!(
            "the run stopped at step {step} on the playbook's own limit of {} step visits (`limits.max_step_visits`). Look for a loop that runs more often than the document allows, or set the limit from the document.",
            limits.max_step_visits
        )
    } else {
        format!(
            "the run stopped at step {step} on one of the playbook's own `limits` (iterations, child runs, history or payload size). Compare the run with the limits set in the draft, and set them from the document."
        )
    }))
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

struct Differences<'a> {
    found: &'a mut Vec<String>,
    limit: usize,
}
impl Differences<'_> {
    fn compare(&mut self, want: &Value, got: &Value, path: &str) {
        if self.found.len() >= self.limit || want == got {
            return;
        }
        match (want, got) {
            (Value::Object(w), Value::Object(g)) => {
                self.fields(w, g, path);
            }
            (Value::Array(w), Value::Array(g)) if w.len() == g.len() => {
                for (i, (w, g)) in w.iter().zip(g).enumerate() {
                    self.compare(w, g, &format!("{path}[{i}]"));
                }
            }
            _ => self.found.push(format!(
                "{path}: the case expects {}, the call has {}",
                clip(want),
                clip(got)
            )),
        }
    }
    fn fields(
        &mut self,
        w: &serde_json::Map<String, Value>,
        g: &serde_json::Map<String, Value>,
        path: &str,
    ) {
        let keys: BTreeSet<&String> = w.keys().chain(g.keys()).collect();
        for key in keys {
            let at = format!("{path}.{key}");
            match (w.get(key), g.get(key)) {
                (Some(w), Some(g)) => self.compare(w, g, &at),
                (Some(w), None) => self.found.push(format!(
                    "{at}: the case expects {}; the call has nothing there",
                    clip(w)
                )),
                (None, Some(g)) => self.found.push(format!(
                    "{at}: the case does not list it; the call has {}",
                    clip(g)
                )),
                (None, None) => {}
            }
            if self.found.len() >= self.limit {
                return;
            }
        }
    }
}
/// The calls made must be exactly the calls the case lists, in order, each
/// with exactly its input.
pub(super) fn expected_calls(expect: &[ExpectedCall], calls: &[Call], problems: &mut Vec<String>) {
    let made = || {
        names(
            calls
                .iter()
                .map(|c| format!("{} at step {}", c.command, c.step))
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str),
        )
    };
    for (i, pair) in expect.iter().zip(calls).enumerate() {
        let (want, call) = pair;
        if want.command != call.command {
            problems.push(format!(
                "expect.calls[{i}]: the case expects a call to {} here, but the run called {} at step {}. List every call the run makes, in order; the calls made were: {}.",
                want.command,
                call.command,
                call.step,
                made()
            ));
            // The rest no longer line up; one problem says it.
            return;
        }
        let mut found = vec![];
        Differences {
            found: &mut found,
            limit: 8,
        }
        .compare(&want.input, &call.input, "input");
        if !found.is_empty() {
            problems.push(format!(
                "expect.calls[{i}].input: the call to {} at step {} differs from the case: {}.",
                call.command,
                call.step,
                found.join("; ")
            ));
        }
    }
    if expect.len() > calls.len() {
        problems.push(format!(
            "expect.calls[{}]: the case expects a call to {} here, but the run made {} calls: {}.",
            calls.len(),
            expect[calls.len()].command,
            calls.len(),
            made()
        ));
    } else if calls.len() > expect.len() {
        let extra = &calls[expect.len()];
        problems.push(format!(
            "expect.calls: the run called {} at step {} after the {} calls the case lists (it lists {}). List every call the run makes, in order, each with its whole input; the calls made were: {}.",
            extra.command,
            extra.step,
            expect.len(),
            expect.len(),
            made()
        ));
    }
}

/// One case run within `limit` transitions: the report, the transitions it
/// took, and whether it was stopped at `limit`.
fn step_name(run: &Value) -> &str {
    run["step"].as_str().unwrap_or_default()
}

pub(super) fn inspect_outcome(
    engine: &CaseEngine,
    artifact: &Artifact,
    expected: &str,
    report: &mut CaseReport,
) -> anyhow::Result<()> {
    // Finished runs have left the state: read them through inspection.
    let runs = inspected(engine, InspectionKind::Runs)?;
    let roots: Vec<&Value> = runs.iter().filter(|r| r["parent"].is_null()).collect();
    match roots.as_slice() {
        [] => report.problems.push("start: no run started.".to_string()),
        [run] => inspect_run(engine, artifact, run, report)?,
        many => report.problems.push(format!(
            "start: the event started {} runs; a case follows one run, so start it from an event one trigger names, or from an entry.",
            many.len()
        )),
    }
    if let Some(outcome) = &report.outcome
        && outcome != expected
    {
        let step = roots.first().map(|r| step_name(r)).unwrap_or_default();
        report.problems.push(format!(
            "expect.outcome: the case expects the run to end {:?}, but it ended {outcome:?} (at step {step}).",
            expected
        ));
    }
    Ok(())
}

fn inspect_run(
    engine: &CaseEngine,
    artifact: &Artifact,
    run: &Value,
    report: &mut CaseReport,
) -> anyhow::Result<()> {
    match run["status"].as_str() {
        Some("completed") => report.outcome = run["outcome"].as_str().map(String::from),
        Some("failed") => {
            let problem = match own_limit(engine, artifact, run)? {
                Some(problem) => problem,
                None => format!(
                    "the run failed at step {} without an outcome; give each step's command a return that fits it, and an `on_error` edge where a call may fail.",
                    step_name(run)
                ),
            };
            report.problems.push(problem);
        }
        Some("cancelled") => report
            .problems
            .push(format!("the run was cancelled at step {}.", step_name(run))),
        status => {
            // A run that has returned and waits only for an operation it
            // started to settle (an answer the case leaves pending) is
            // still live, so the outcome it returned is read from the state.
            let returned = match status {
                Some("finishing") => returned(engine, run)?,
                _ => None,
            };
            match returned {
                    Some(outcome) => report.outcome = Some(outcome),
                    None => report.problems.push(format!(
                        "the run did not finish: it is {} at step {}. A case runs until nothing more can happen; give the returns this step needs.",
                        status.unwrap_or("unknown"),
                        step_name(run)
                    )),
                }
        }
    }
    Ok(())
}
