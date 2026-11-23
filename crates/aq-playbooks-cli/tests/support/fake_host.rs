//! Synthetic domain services for contract tests. This is deliberately not an AQ
//! adapter. Domain records and notification intents belong to this host, separate
//! from runner state; serializing `FakeHost` simulates a durable host restart.
//! Named `context.read` snapshots are immutable retained observations. The one
//! supported fresh-read mode uses `snapshot=""`, `view="project"`, `page=0`, and
//! subject id `main`; its input revision is a request hint, while the returned
//! subject revision comes from the current host record. Every fresh read records
//! its own bounded immutable snapshot and requires project visibility.
use aq_playbooks_cli::catalog;
use aq_playbooks_model::{CommandContract, ContractRef, EventEnvelope, Registry, digest};
use aq_playbooks_runner::{Authority, CancellationResult, DispatchResult, Host, Intent};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Subject {
    pub id: String,
    pub revision: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hold {
    pub subject: Subject,
    pub actor: String,
    pub scope: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub subject: Subject,
    pub suite: String,
    pub passed: bool,
    pub issuer: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub subject: Subject,
    pub base: Subject,
    pub sources: Vec<Subject>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Work {
    pub reference: String,
    pub subject: Subject,
    pub kind: String,
    pub visibility: String,
    pub request_digest: String,
    pub authored_result: Option<String>,
    pub lifetime: String,
    pub operation_id: String,
    pub completion: Option<Value>,
    pub cancelled: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub reference: String,
    pub partition: String,
    pub scope: String,
    pub visibility: String,
    pub view: String,
    pub subject: Subject,
    pub pages: Vec<Value>,
    pub complete: bool,
    pub observed_at_ms: u64,
    pub expires_at_ms: u64,
    pub episode_revisions: BTreeMap<String, u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub revision: u64,
    pub active: bool,
    pub evidence: String,
    pub last_notification_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evaluation {
    pub reference: String,
    pub observation: String,
    pub partition: String,
    pub scope: String,
    pub findings_digest: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub identity: String,
    pub body: String,
    pub acknowledged: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub subject: Subject,
    pub responders: BTreeSet<String>,
    pub expires_at_ms: u64,
    pub actor: String,
    pub scope: String,
    pub completion: Option<(String, Value)>,
    pub completed_at_ms: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationReceipt {
    pub request_digest: String,
    pub actor: String,
    pub scope: String,
    pub result: DispatchResult,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarOccurrence {
    pub identity: String,
    pub schedule: String,
    pub scheduled_at_ms: u64,
    pub timezone: String,
    pub misfire: String,
    pub configuration_revision: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalendarConfiguration {
    pub revision: u64,
    pub timezone: String,
    pub misfire: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FakeHost {
    pub now_ms: u64,
    pub subjects: BTreeMap<String, u64>,
    pub grants: BTreeMap<String, BTreeSet<String>>,
    pub visibility: BTreeMap<String, BTreeSet<String>>,
    pub holds: BTreeMap<String, Hold>,
    pub evidence: BTreeMap<String, Evidence>,
    pub candidates: BTreeMap<String, Candidate>,
    pub work: BTreeMap<String, Work>,
    pub observations: BTreeMap<String, Observation>,
    pub evaluations: BTreeMap<String, Evaluation>,
    pub episodes: BTreeMap<String, Episode>,
    pub outbox: BTreeMap<String, Notification>,
    pub recipient_receipts: BTreeMap<String, String>,
    pub operations: BTreeMap<String, OperationReceipt>,
    #[serde(default)]
    pub decisions: BTreeMap<String, Decision>,
    pub messages: BTreeMap<String, Value>,
    pub calendars: BTreeMap<String, CalendarConfiguration>,
    pub occurrences: BTreeMap<String, CalendarOccurrence>,
    /// Explicit fake outcomes, keyed by subject id/revision/suite or operation id.
    pub configured_outcomes: BTreeMap<String, String>,
    pub configured_choices: BTreeMap<String, String>,
    /// Emulates a lost transport response after the domain transaction committed.
    pub lose_next_response: bool,
}

fn field<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {key}"))
}
pub fn parse_subject(value: &Value) -> Result<Subject, String> {
    serde_json::from_value(value.clone()).map_err(|_| "invalid subject".into())
}
fn subject_field(input: &Value, key: &str) -> Result<Subject, String> {
    parse_subject(&input[key])
}
fn result(reference: impl Into<String>, subject: &Subject) -> Value {
    json!({"reference":reference.into(),"subject":subject})
}
fn completed(outcome: &str, output: Value) -> DispatchResult {
    DispatchResult::Completed {
        outcome: outcome.into(),
        output,
    }
}
fn episode_key(partition: &str, key: &str) -> String {
    format!("{partition}/{key}")
}

impl FakeHost {
    pub fn grant_all(&mut self, actor: &str, visibility: &[&str]) {
        self.grants.insert(
            actor.into(),
            catalog::catalog().commands.keys().cloned().collect(),
        );
        self.visibility.insert(
            actor.into(),
            visibility.iter().map(|s| (*s).into()).collect(),
        );
    }
    fn visible(&self, actor: &str, visibility: &str) -> bool {
        self.visibility
            .get(actor)
            .is_some_and(|v| v.contains(visibility))
    }
    fn current(&self, subject: &Subject) -> bool {
        self.subjects.get(&subject.id) == Some(&subject.revision)
    }
    fn check_current(&self, subject: &Subject) -> Result<(), String> {
        if self.current(subject) {
            Ok(())
        } else {
            Err("subject revision conflict".into())
        }
    }
    pub fn check_key(subject: &Subject, suite: &str) -> String {
        format!("{}@{}/{suite}", subject.id, subject.revision)
    }
    pub fn restart(&self) -> Self {
        serde_json::from_slice(&serde_json::to_vec(self).expect("fake host JSON"))
            .expect("fake host restart")
    }

    /// Calendar calculation is host-owned. The fake accepts explicit instants and
    /// pins timezone/misfire/configuration metadata rather than doing DST math.
    pub fn admit_occurrence(
        &mut self,
        identity: &str,
        (schedule, expected_revision): (&str, u64),
        at_ms: u64,
    ) -> Result<CalendarOccurrence, String> {
        if let Some(old) = self.occurrences.get(identity) {
            if old.schedule != schedule || old.scheduled_at_ms != at_ms {
                return Err("calendar occurrence identity conflict".into());
            }
            return Ok(old.clone());
        }
        let config = self.calendars.get(schedule).ok_or("unknown calendar")?;
        if !config.valid_at(expected_revision) {
            return Err("calendar configuration conflict".into());
        }
        let occurrence = CalendarOccurrence {
            identity: identity.into(),
            schedule: schedule.into(),
            scheduled_at_ms: at_ms,
            timezone: config.timezone.clone(),
            misfire: config.misfire.clone(),
            configuration_revision: config.revision,
        };
        self.occurrences.insert(identity.into(), occurrence.clone());
        Ok(occurrence)
    }

    fn execute(&mut self, intent: &Intent) -> Result<DispatchResult, String> {
        if intent.command.version == catalog::VERSION_2
            && matches!(
                intent.command.name.as_str(),
                "judgment.request" | "check.request"
            )
        {
            // 2.0.0 admission only: the durable handle completes by host event.
            self.check_current(&subject_field(&intent.input, "subject")?)?;
            return Ok(DispatchResult::Accepted {
                handle: intent.id.clone(),
            });
        }
        match intent.command.name.as_str() {
            "context.read" => self.read_context(intent),
            "work.ensure" => self.ensure_work(intent),
            "work.change" => self.change_work(intent),
            "check.request" => self.request_check(intent),
            "candidate.prepare" => self.prepare_candidate(intent),
            "candidate.publish" => self.publish_candidate(intent),
            "message.send" => self.send_message(intent),
            "findings.reconcile" => self.reconcile_findings(intent),
            "decision.request" => self.request_decision(intent),
            "judgment.request" => self.request_judgment(intent),
            "operation.get" | "operation.cancel" => self.operation(intent),
            _ => Err("unregistered synthetic command".into()),
        }
    }
}

impl FakeHost {
    fn commit_dispatch(&mut self, intent: &Intent, request_digest: String) -> DispatchResult {
        // Clone/commit models a host-owned transaction, independent of the runner.
        let mut staged = self.clone();
        let response = match staged.execute(intent) {
            Ok(response) => response,
            Err(reason) => return DispatchResult::Rejected { reason },
        };
        if matches!(response, DispatchResult::Unknown { .. }) {
            return response;
        }
        if let DispatchResult::Completed { outcome, output } = &response {
            let valid = intent
                .command
                .outcomes
                .get(outcome)
                .is_some_and(|schema| schema.validate(output).is_ok());
            if !valid {
                return DispatchResult::Rejected {
                    reason: "fake host output violates captured contract".into(),
                };
            }
        }
        staged.operations.insert(
            intent.id.clone(),
            OperationReceipt {
                request_digest,
                actor: intent.authority.actor.clone(),
                scope: intent.authority.scope.clone(),
                result: response.clone(),
            },
        );
        let lost = staged.lose_next_response;
        staged.lose_next_response = false;
        *self = staged;
        if lost {
            DispatchResult::Unknown {
                reason: "synthetic transport interrupted after host commit".into(),
            }
        } else {
            response
        }
    }
    fn cancel_work(&mut self, operation_id: &str) -> CancellationResult {
        let Some(work) = self
            .work
            .values_mut()
            .find(|work| work.operation_id == operation_id)
        else {
            return CancellationResult::Unknown;
        };
        if work.completion.is_some() {
            return CancellationResult::TooLate;
        }
        work.cancelled = true;
        CancellationResult::Confirmed
    }
}

impl Host for FakeHost {
    fn authorize(&self, authority: &Authority, contract: &CommandContract) -> bool {
        let published =
            |registry: Registry| registry.commands.get(&contract.name) == Some(contract);
        (published(catalog::catalog())
            || published(catalog::catalog_v2())
            || published(catalog::catalog_v2_1()))
            && contract.scopes.contains(&authority.scope)
            && contract.capabilities.is_subset(&authority.capabilities)
            && self
                .grants
                .get(&authority.actor)
                .is_some_and(|g| contract.capabilities.is_subset(g))
    }
    fn dispatch(&mut self, intent: &Intent) -> DispatchResult {
        if !self.authorize(&intent.authority, &intent.command) {
            return DispatchResult::Rejected {
                reason: "fake host authority refused".into(),
            };
        }
        if let Err(error) = intent.command.input.validate(&intent.input) {
            return DispatchResult::Rejected {
                reason: error.to_string(),
            };
        }
        let request_digest = match digest(&(&intent.command, &intent.input, &intent.authority)) {
            Ok(d) => d,
            Err(e) => {
                return DispatchResult::Rejected {
                    reason: e.to_string(),
                };
            }
        };
        if let Some(old) = self.operations.get(&intent.id) {
            return if old.request_digest == request_digest {
                old.result.clone()
            } else {
                DispatchResult::Rejected {
                    reason: "operation identity content conflict".into(),
                }
            };
        }
        self.commit_dispatch(intent, request_digest)
    }
    fn reconcile(&mut self, intent: &Intent) -> DispatchResult {
        self.dispatch(intent)
    }
    fn cancel(&mut self, intent: &Intent) -> CancellationResult {
        if !self.authorize(&intent.authority, &intent.command) {
            return CancellationResult::Refused;
        }
        if let Some(decision) = self.decisions.get(&intent.id) {
            if decision.completion.is_some() {
                return CancellationResult::TooLate;
            }
            let outcome = if self.now_ms >= decision.expires_at_ms {
                "expired"
            } else {
                "cancelled"
            };
            return match self.settle_decision(&intent.id, outcome) {
                Ok(_) if outcome == "cancelled" => CancellationResult::Confirmed,
                Ok(_) => CancellationResult::TooLate,
                Err(_) => CancellationResult::Unknown,
            };
        }
        self.cancel_work(&intent.id)
    }
}

#[path = "fake_host/communications.rs"]
mod communications;
#[path = "fake_host/observations.rs"]
mod observations;
#[path = "fake_host/operations.rs"]
mod operations;
#[path = "fake_host/publication.rs"]
mod publication;
#[path = "fake_host/work.rs"]
mod work;
impl CalendarConfiguration {
    fn valid_at(&self, expected_revision: u64) -> bool {
        self.revision == expected_revision
            && !self.timezone.is_empty()
            && matches!(
                self.misfire.as_str(),
                "skip" | "coalesce" | "catch_up_bounded"
            )
    }
}

#[path = "fake_host/findings.rs"]
mod findings;
