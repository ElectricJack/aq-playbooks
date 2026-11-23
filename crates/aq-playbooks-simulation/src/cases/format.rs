//! Serializable case input and report types.
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

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
    Event {
        event: String,
        payload: Value,
        #[serde(default)]
        correlation: BTreeMap<String, String>,
    },
    /// The run is started at this entry with this input.
    Entry { entry: String, input: Value },
}
/// `{"outcome": …, "output": …}`, or `{"pending": true}` for no answer.
/// For an asynchronous command, `{"outcome": …, "output": …, "at_once":
/// true}` answers the call itself with one of the command's own outcomes
/// other than `accepted` (such as `refused` at admission), instead of
/// accepting it and answering through the completion event.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum CaseReturn {
    Answer {
        outcome: String,
        output: Value,
    },
    AtOnce {
        outcome: String,
        output: Value,
        at_once: bool,
    },
    Pending {
        pending: bool,
    },
}
// Written by hand so a wrong return says which shape is accepted.
impl<'de> Deserialize<'de> for CaseReturn {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let shape = "a return is an answer, {\"outcome\": \"<outcome>\", \"output\": {…}}, an answer an asynchronous command gives at once, {\"outcome\": \"<outcome>\", \"output\": {…}, \"at_once\": true}, or no answer, {\"pending\": true}";
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
            ["at_once", "outcome", "output"] => match (&map["outcome"], &map["at_once"]) {
                (Value::String(outcome), Value::Bool(at_once)) => Ok(Self::AtOnce {
                    outcome: outcome.clone(),
                    output: map["output"].clone(),
                    at_once: *at_once,
                }),
                _ => Err(D::Error::custom(format!(
                    "outcome must be a string and at_once true; {shape}"
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
    /// Every call the run makes, in order, each with its whole input.
    /// Required: a case that expects no call writes `[]`.
    pub calls: Vec<ExpectedCall>,
}
/// One call the run must make: its command, and its whole input, which
/// must equal what the playbook sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedCall {
    pub command: String,
    pub input: Value,
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
