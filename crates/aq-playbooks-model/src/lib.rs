//! Versioned, bounded portable workflow format. See FORMAT.md for canonical rules.
mod draft;
mod evaluation;
mod extensions;
pub use evaluation::eval;
pub mod schedules;
mod schema;
pub use schema::Schema;
mod structure;
pub use draft::{DraftSize, MAX_DRAFT_EXPANSION, MAX_DRAFT_EXPANSION_TOTAL, MAX_DRAFT_SCOPE};
pub use extensions::CatalogExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const FORMAT_VERSION: u32 = 1;
pub const MAX_ARTIFACT_BYTES: usize = 16_777_216;
pub type Json = Value;
pub type Hash = String;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Expr {
    Literal {
        value: Value,
    },
    Ref {
        root: String,
        #[serde(default)]
        path: Vec<String>,
    },
    Object {
        fields: BTreeMap<String, Expr>,
    },
    Array {
        items: Vec<Expr>,
    },
    Exists {
        value: Box<Expr>,
    },
    Not {
        value: Box<Expr>,
    },
    And {
        values: Vec<Expr>,
    },
    Or {
        values: Vec<Expr>,
    },
    Eq {
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Lt {
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Contains {
        collection: Box<Expr>,
        value: Box<Expr>,
    },
    Map {
        collection: Box<Expr>,
        item: String,
        value: Box<Expr>,
        max_items: usize,
    },
    Filter {
        collection: Box<Expr>,
        item: String,
        predicate: Box<Expr>,
        max_items: usize,
    },
    Difference {
        left: Box<Expr>,
        right: Box<Expr>,
        identity: Vec<String>,
        max_items: usize,
    },
    /// Chooses by a run-time key from a literal table; `default` when no case matches.
    Lookup {
        key: Box<Expr>,
        cases: Vec<LookupCase>,
        default: Box<Expr>,
    },
    /// Sequential, single-assignment lexical bindings visible to later bindings and `body`.
    Let {
        bindings: Vec<LetBinding>,
        body: Box<Expr>,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LookupCase {
    pub when: Value,
    pub then: Expr,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LetBinding {
    pub name: String,
    pub value: Expr,
}
pub const MAX_LOOKUP_CASES: usize = 256;
/// Canonical bytes of one lookup case's `when`.
pub const MAX_LOOKUP_KEY_BYTES: usize = 1024;
pub const MAX_LET_BINDINGS: usize = 16;
/// Roots an expression's lexical names may never take.
const RESERVED_ROOTS: [&str; 4] = ["input", "event", "definitions", "vars"];
impl Expr {
    pub fn literal(value: impl Into<Value>) -> Self {
        Self::Literal {
            value: value.into(),
        }
    }
    pub fn reference(root: impl Into<String>) -> Self {
        Self::Ref {
            root: root.into(),
            path: vec![],
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum EvalValue {
    Known(Value),
    Missing,
    Unknown(String),
}
pub type EvalContext = BTreeMap<String, EvalValue>;
#[derive(Debug, Clone, Copy)]
pub struct EvalLimits {
    pub max_depth: usize,
    pub max_elements: usize,
    pub max_bytes: usize,
}
impl Default for EvalLimits {
    fn default() -> Self {
        Self {
            max_depth: 32,
            max_elements: 10_000,
            max_bytes: 1_048_576,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Observation,
    Mutation,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recovery {
    Observation,
    NaturallyIdempotent,
    KeyedIdempotent,
    Reconcilable,
    Unsafe,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandContract {
    pub name: String,
    pub version: String,
    pub input: Schema,
    pub outcomes: BTreeMap<String, Schema>,
    pub capabilities: BTreeSet<String>,
    pub scopes: BTreeSet<String>,
    pub effect: Effect,
    pub recovery: Recovery,
    pub asynchronous: Option<AsyncContract>,
    pub cancellation: bool,
    pub public_result_fields: BTreeSet<String>,
    pub sensitive_input_fields: BTreeSet<String>,
    #[serde(default)]
    pub sensitive_output_fields: BTreeSet<String>,
    pub max_payload_bytes: usize,
    pub deadline_ms: u64,
}
impl CommandContract {
    pub fn fingerprint(&self) -> Result<Hash, ModelError> {
        digest(self)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AsyncContract {
    pub completion_event: String,
    pub completion: Schema,
    pub correlation_field: String,
    pub reconciliation_command: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventContract {
    #[serde(default)]
    pub sensitive_payload_fields: BTreeSet<String>,
    pub name: String,
    pub version: String,
    pub payload: Schema,
    pub trusted_producers: BTreeSet<String>,
    pub scopes: BTreeSet<String>,
    pub correlation_fields: BTreeSet<String>,
    pub subject_field: Option<String>,
    pub revision_field: Option<String>,
    pub max_payload_bytes: usize,
    pub retention_ms: u64,
}
impl EventContract {
    pub fn fingerprint(&self) -> Result<Hash, ModelError> {
        digest(self)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub commands: BTreeMap<String, CommandContract>,
    pub events: BTreeMap<String, EventContract>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractRef {
    pub name: String,
    pub fingerprint: Hash,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventEnvelope {
    pub producer: String,
    pub event_id: String,
    pub contract: ContractRef,
    pub scope: String,
    pub correlation: BTreeMap<String, String>,
    pub subject: Option<String>,
    pub revision: Option<u64>,
    pub causation: Option<String>,
    pub occurred_at_ms: Option<i64>,
    pub observed_at_ms: Option<i64>,
    pub payload: Value,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryPoint {
    pub input: Schema,
    pub outcomes: BTreeMap<String, Schema>,
    pub start: String,
}
/// A catalog selection resolves the named binding exactly once at root admission.
/// An empty digest allowlist permits any currently active compatible version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DependencySelection {
    #[default]
    Exact,
    Catalog {
        allowed_artifacts: BTreeSet<Hash>,
    },
}
/// Actual implementations may accept a wider input domain, return a narrower
/// output domain, and omit expected outcomes; they may not add unhandled outcomes.
pub fn interface_compatible(actual: &EntryPoint, expected: &EntryPoint) -> bool {
    expected.input.assignable_to(&actual.input)
        && actual.outcomes.iter().all(|(name, schema)| {
            expected
                .outcomes
                .get(name)
                .is_some_and(|target| schema.assignable_to(target))
        })
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallableDependency {
    pub binding: String,
    #[serde(default)]
    pub selection: DependencySelection,
    pub artifact: Hash,
    pub interfaces: BTreeMap<String, EntryPoint>,
    pub capabilities: BTreeSet<String>,
    pub scope: String,
    pub dependencies: BTreeSet<Hash>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    pub id: String,
    pub event: ContractRef,
    pub entry: String,
    pub input: Expr,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_step_visits: u64,
    pub max_children: u64,
    pub max_depth: u32,
    pub max_concurrent_effects: u32,
    pub max_concurrent_children: u32,
    pub max_payload_bytes: usize,
    pub max_history_bytes: usize,
    pub max_duration_ms: u64,
    pub max_iterations: u64,
    pub max_iteration_depth: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_step_visits: 10_000,
            max_children: 100,
            max_depth: 8,
            max_concurrent_effects: 16,
            max_concurrent_children: 16,
            max_payload_bytes: 1_048_576,
            max_history_bytes: 16_777_216,
            max_duration_ms: 86_400_000,
            max_iterations: 1000,
            max_iteration_depth: 4,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Playbook {
    pub id: String,
    pub purpose: String,
    pub scope: String,
    pub entries: BTreeMap<String, EntryPoint>,
    pub steps: BTreeMap<String, Step>,
    pub triggers: Vec<Trigger>,
    pub limits: Limits,
    pub required_capabilities: BTreeSet<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub op: Operation,
    pub edges: BTreeMap<String, String>,
    pub on_error: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Invoke {
        command: ContractRef,
        input: Expr,
        bind: String,
        retry: Option<RetryPolicy>,
    },
    Decide {
        predicate: Expr,
    },
    Wait {
        event: Option<ContractRef>,
        correlation: BTreeMap<String, Expr>,
        minimum_revision: Option<Expr>,
        deadline_ms: u64,
        bind: String,
    },
    ForEach {
        collection: Expr,
        item: String,
        body: String,
        next: String,
        max_items: usize,
        concurrency: u32,
        on_failure: IterationFailure,
    },
    Call {
        dependency: String,
        entry: String,
        input: Expr,
        mode: CallMode,
        bind: String,
    },
    Join {
        members: Vec<Expr>,
        #[serde(default)]
        member_keys: Vec<String>,
        mode: JoinMode,
        accepted_outcomes: BTreeSet<String>,
        deadline_ms: u64,
        remaining: RemainingPolicy,
        bind: String,
    },
    Return {
        outcome: String,
        output: Expr,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallMode {
    Wait,
    OwnedAsync,
    Independent,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinMode {
    All,
    AnyCompletion,
    AnyAccepted,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemainingPolicy {
    Track,
    Cancel,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IterationFailure {
    Stop,
    Continue,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff_ms: u64,
    pub outcomes: BTreeSet<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceHeader {
    pub format_version: u32,
    pub id: String,
    pub scope: String,
    pub interfaces: BTreeSet<String>,
    pub definitions: BTreeSet<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceBinding {
    pub digest: Hash,
    pub header: SourceHeader,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLocation {
    pub line: u32,
    pub column: u32,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedDefinition {
    pub version: String,
    pub digest: Hash,
    pub value: Value,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactContent {
    pub format_version: u32,
    pub compiler: String,
    pub source: SourceBinding,
    pub playbook: Playbook,
    pub registry: Registry,
    pub dependencies: BTreeMap<String, CallableDependency>,
    pub definitions: BTreeMap<String, CapturedDefinition>,
    pub source_map: BTreeMap<String, SourceLocation>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub digest: Hash,
    pub executable_fingerprint: Hash,
    pub content: ArtifactContent,
}
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("invalid model: {0}")]
    Invalid(String),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("integrity mismatch: {0}")]
    Integrity(String),
    #[error("expression: {0}")]
    Expression(String),
}

/// Canonical JSON: UTF-8, recursively sorted keys, no whitespace, integral i64/u64 numbers
/// only, serde_json string escaping, no omitted fields beyond the serialized model.
pub fn canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, ModelError> {
    let value = serde_json::to_value(value)?;
    let normalized = canonical_value(value)?;
    Ok(serde_json::to_vec(&normalized)?)
}
fn canonical_value(value: Value) -> Result<Value, ModelError> {
    match value {
        Value::Number(n) if !(n.is_i64() || n.is_u64()) => Err(ModelError::Invalid(
            "fractional/exponential numbers unsupported; use schema-defined decimal strings".into(),
        )),
        Value::Array(a) => Ok(Value::Array(
            a.into_iter()
                .map(canonical_value)
                .collect::<Result<_, _>>()?,
        )),
        Value::Object(o) => {
            let sorted: BTreeMap<_, _> = o.into_iter().collect();
            let mut m = serde_json::Map::new();
            for (k, v) in sorted {
                m.insert(k, canonical_value(v)?);
            }
            Ok(Value::Object(m))
        }
        v => Ok(v),
    }
}
pub fn digest<T: Serialize>(value: &T) -> Result<Hash, ModelError> {
    Ok(hash_bytes(&canonical_bytes(value)?))
}
pub fn hash_bytes(bytes: &[u8]) -> Hash {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
impl ArtifactContent {
    pub fn executable_fingerprint(&self) -> Result<Hash, ModelError> {
        // Source prose, mappings, and human-facing purpose are presentation metadata.
        let mut p = self.playbook.clone();
        p.purpose.clear();
        digest(&(
            self.format_version,
            &p,
            &self.registry,
            &self.dependencies,
            &self.definitions,
        ))
    }
}
impl Artifact {
    pub fn seal(content: ArtifactContent) -> Result<Self, ModelError> {
        Ok(Self {
            digest: digest(&content)?,
            executable_fingerprint: content.executable_fingerprint()?,
            content,
        })
    }
    pub fn verify(&self) -> Result<(), ModelError> {
        let issues = self.content.validate_structure();
        if !issues.is_empty() {
            return Err(ModelError::Invalid(
                issues
                    .iter()
                    .map(|i| format!("{}: {}", i.path, i.message))
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        if self.content.format_version != FORMAT_VERSION {
            return Err(ModelError::Invalid("unsupported artifact format".into()));
        }
        if digest(&self.content)? != self.digest {
            return Err(ModelError::Integrity("content digest".into()));
        }
        if self.content.executable_fingerprint()? != self.executable_fingerprint {
            return Err(ModelError::Integrity("executable fingerprint".into()));
        }
        for d in self.content.definitions.values() {
            if digest(&d.value)? != d.digest {
                return Err(ModelError::Integrity("captured definition".into()));
            }
        }
        Ok(())
    }
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, ModelError> {
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err(ModelError::Invalid("artifact exceeds 16 MiB".into()));
        }
        let a: Self = serde_json::from_slice(bytes)?;
        if canonical_bytes(&a)? != bytes {
            return Err(ModelError::Integrity(
                "noncanonical artifact encoding".into(),
            ));
        }
        a.verify()?;
        Ok(a)
    }
}

/// Validates a bounded event against its captured executable registration.
pub fn validate_event(event: &EventEnvelope, registry: &Registry) -> Result<(), ModelError> {
    let c = registry
        .events
        .get(&event.contract.name)
        .ok_or_else(|| ModelError::Invalid("unknown event".into()))?;
    if c.fingerprint()? != event.contract.fingerprint {
        return Err(ModelError::Integrity("event contract fingerprint".into()));
    }
    if event.event_id.is_empty()
        || !c.trusted_producers.contains(&event.producer)
        || !c.scopes.contains(&event.scope)
    {
        return Err(ModelError::Invalid(
            "event identity/producer/scope refused".into(),
        ));
    }
    if c.correlation_fields
        .iter()
        .any(|k| !event.correlation.contains_key(k))
        || event
            .correlation
            .keys()
            .any(|k| !c.correlation_fields.contains(k))
    {
        return Err(ModelError::Invalid("event correlation fields".into()));
    }
    if c.subject_field.is_some() != event.subject.is_some()
        || c.revision_field.is_some() != event.revision.is_some()
    {
        return Err(ModelError::Invalid(
            "event subject/revision required by contract".into(),
        ));
    }
    if canonical_bytes(&event.payload)?.len() > c.max_payload_bytes {
        return Err(ModelError::Invalid("event payload byte bound".into()));
    }
    c.payload.validate(&event.payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn bounded_union_and_callable_interface_variance() {
        let union = Schema::Union {
            variants: vec![Schema::Boolean, Schema::string(8)],
        };
        assert!(union.validate(&json!(true)).is_ok());
        assert!(union.validate(&json!("subject")).is_ok());
        assert!(union.validate(&json!(42)).is_err());
        assert!(
            Schema::Union {
                variants: vec![Schema::Null]
            }
            .validate_definition()
            .is_err()
        );
        let expected = EntryPoint {
            input: Schema::string(8),
            outcomes: BTreeMap::from([("ok".into(), Schema::string(16))]),
            start: "old".into(),
        };
        let actual = EntryPoint {
            input: Schema::string(16),
            outcomes: BTreeMap::from([("ok".into(), Schema::string(8))]),
            start: "new".into(),
        };
        assert!(interface_compatible(&actual, &expected));
        assert!(!interface_compatible(&expected, &actual));
        let mut expanded = actual.clone();
        expanded.outcomes.insert("new_outcome".into(), Schema::Null);
        assert!(!interface_compatible(&expanded, &expected));
    }

    #[test]
    fn canonical_golden_vectors() {
        assert_eq!(
            canonical_bytes(&json!({"z":null,"a":"line\n\u{0000}","n":-3})).unwrap(),
            b"{\"a\":\"line\\n\\u0000\",\"n\":-3,\"z\":null}"
        );
        assert!(canonical_bytes(&json!(1.5)).is_err());
        assert_ne!(
            digest(&json!({})).unwrap(),
            digest(&json!({"x":null})).unwrap()
        );
    }
    #[test]
    fn missing_null_and_unknown_are_distinct() {
        let context = BTreeMap::from([
            ("null".into(), EvalValue::Known(Value::Null)),
            ("stale".into(), EvalValue::Unknown("revision".into())),
        ]);
        let l = EvalLimits::default();
        assert_eq!(
            eval(
                &Expr::Exists {
                    value: Box::new(Expr::reference("null"))
                },
                &context,
                &l
            )
            .unwrap(),
            EvalValue::Known(json!(true))
        );
        assert_eq!(
            eval(
                &Expr::Exists {
                    value: Box::new(Expr::reference("absent"))
                },
                &context,
                &l
            )
            .unwrap(),
            EvalValue::Known(json!(false))
        );
        assert!(matches!(
            eval(
                &Expr::Not {
                    value: Box::new(Expr::reference("stale"))
                },
                &context,
                &l
            )
            .unwrap(),
            EvalValue::Unknown(_)
        ));
        assert!(matches!(
            eval(
                &Expr::And {
                    values: vec![Expr::literal(false), Expr::reference("stale")]
                },
                &context,
                &l
            )
            .unwrap(),
            EvalValue::Unknown(_)
        ));
    }
    #[test]
    fn collection_order_duplicates_and_budget() {
        let d = Expr::Difference {
            left: Box::new(Expr::literal(json!([{"id":"b"},{"id":"a"},{"id":"c"}]))),
            right: Box::new(Expr::literal(json!([{"id":"a"}]))),
            identity: vec!["id".into()],
            max_items: 4,
        };
        assert_eq!(
            eval(&d, &BTreeMap::new(), &EvalLimits::default()).unwrap(),
            EvalValue::Known(json!([{"id":"b"},{"id":"c"}]))
        );
        let duplicate = Expr::Difference {
            left: Box::new(Expr::literal(json!([{"id":1},{"id":1}]))),
            right: Box::new(Expr::literal(json!([]))),
            identity: vec!["id".into()],
            max_items: 4,
        };
        assert!(eval(&duplicate, &BTreeMap::new(), &EvalLimits::default()).is_err());
        assert!(
            eval(
                &d,
                &BTreeMap::new(),
                &EvalLimits {
                    max_elements: 2,
                    ..EvalLimits::default()
                }
            )
            .is_err()
        );
    }
    #[test]
    fn schemas_are_closed_and_bounded() {
        let schema = Schema::object(BTreeMap::from([("s".into(), Schema::string(3))]));
        assert!(schema.validate(&json!({"s":"abc"})).is_ok());
        assert!(schema.validate(&json!({"s":"abcd"})).is_err());
        assert!(schema.validate(&json!({"s":"ab","extra":true})).is_err());
        assert!(
            serde_json::from_value::<Schema>(
                json!({"type":"string","max_length":4,"pattern":".*"})
            )
            .is_err()
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub code: String,
    pub path: String,
    pub message: String,
}
fn issue(issues: &mut Vec<ValidationIssue>, code: &str, path: &str, message: impl Into<String>) {
    issues.push(ValidationIssue {
        code: code.into(),
        path: path.into(),
        message: message.into(),
    });
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}
fn valid_hash(hash: &str) -> bool {
    hash.strip_prefix("sha256:").is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
fn schema_has_field(schema: &Schema, field: &str) -> bool {
    match schema {
        Schema::Object { properties, .. } => properties.contains_key(field),
        Schema::Nullable { inner } => schema_has_field(inner, field),
        Schema::Union { variants } => variants
            .iter()
            .any(|schema| schema_has_field(schema, field)),
        _ => false,
    }
}
fn schema_issue(schema: &Schema, path: &str, issues: &mut Vec<ValidationIssue>) {
    if let Err(e) = schema.validate_definition() {
        issue(issues, "schema", path, e.to_string());
    }
}
fn required_edges(
    step: &Step,
    names: impl IntoIterator<Item = String>,
    path: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    for name in names {
        if !step.edges.contains_key(&name) {
            issue(
                issues,
                "unhandled_outcome",
                path,
                format!("outcome {name} has no edge"),
            );
        }
    }
}
fn children(step: &Step) -> Vec<&str> {
    let mut result: Vec<_> = step.edges.values().map(String::as_str).collect();
    if let Some(e) = &step.on_error {
        result.push(e);
    }
    if let Operation::ForEach { body, next, .. } = &step.op {
        result.push(body);
        result.push(next);
    }
    result
}
/// Every expression of an operation, with its location inside the step.
fn step_exprs(op: &Operation) -> Vec<(String, &Expr)> {
    match op {
        Operation::Invoke { input, .. } | Operation::Call { input, .. } => {
            vec![("input".into(), input)]
        }
        Operation::Decide { predicate } => vec![("predicate".into(), predicate)],
        Operation::Wait {
            correlation,
            minimum_revision,
            ..
        } => correlation
            .iter()
            .map(|(k, e)| (format!("correlation.{k}"), e))
            .chain(
                minimum_revision
                    .iter()
                    .map(|e| ("minimum_revision".into(), e)),
            )
            .collect(),
        Operation::ForEach { collection, .. } => vec![("collection".into(), collection)],
        Operation::Join { members, .. } => members
            .iter()
            .enumerate()
            .map(|(i, e)| (format!("members[{i}]"), e))
            .collect(),
        Operation::Return { output, .. } => vec![("output".into(), output)],
    }
}
fn op_name(expr: &Expr) -> &'static str {
    match expr {
        Expr::Literal { .. } => "literal",
        Expr::Ref { .. } => "ref",
        Expr::Object { .. } => "object",
        Expr::Array { .. } => "array",
        Expr::Exists { .. } => "exists",
        Expr::Not { .. } => "not",
        Expr::And { .. } => "and",
        Expr::Or { .. } => "or",
        Expr::Eq { .. } => "eq",
        Expr::Lt { .. } => "lt",
        Expr::Contains { .. } => "contains",
        Expr::Map { .. } => "map",
        Expr::Filter { .. } => "filter",
        Expr::Difference { .. } => "difference",
        Expr::Lookup { .. } => "lookup",
        Expr::Let { .. } => "let",
    }
}
/// The location of `expr`'s operand `field` (".left", ".cases[0].then") below
/// `at`. Object fields and array items need no operator name: `at.field`, `at[i]`.
fn operand(at: &str, expr: &Expr, field: &str) -> String {
    match expr {
        Expr::Object { .. } | Expr::Array { .. } => format!("{at}{field}"),
        _ => format!("{at}.{}{field}", op_name(expr)),
    }
}
/// A JSON value shortened for a diagnostic.
fn shown(value: &Value) -> String {
    let text = value.to_string();
    if text.len() <= 80 {
        return text;
    }
    let mut end = 77;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}
/// Bounds of lookup tables and let bindings, and the names a let may take.
/// `step_roots` are the playbook's binds and iteration items; `lexical` holds
/// the names of enclosing lets and collection items. `at` is the expression's
/// location inside the step at `path`.
fn check_expr_structure(
    expr: &Expr,
    at: &str,
    step_roots: &BTreeSet<String>,
    lexical: &mut Vec<String>,
    path: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    let sub = |e: &Expr, field: &str, lexical: &mut Vec<String>, issues: &mut Vec<_>| {
        check_expr_structure(
            e,
            &operand(at, expr, field),
            step_roots,
            lexical,
            path,
            issues,
        )
    };
    match expr {
        Expr::Literal { .. } | Expr::Ref { .. } => {}
        Expr::Object { fields } => {
            for (k, e) in fields {
                sub(e, &format!(".{k}"), lexical, issues);
            }
        }
        Expr::Array { items } => {
            for (i, e) in items.iter().enumerate() {
                sub(e, &format!("[{i}]"), lexical, issues);
            }
        }
        Expr::Exists { value } | Expr::Not { value } => sub(value, ".value", lexical, issues),
        Expr::And { values } | Expr::Or { values } => {
            for (i, e) in values.iter().enumerate() {
                sub(e, &format!(".values[{i}]"), lexical, issues);
            }
        }
        Expr::Eq { left, right }
        | Expr::Lt { left, right }
        | Expr::Difference { left, right, .. } => {
            sub(left, ".left", lexical, issues);
            sub(right, ".right", lexical, issues);
        }
        Expr::Contains { collection, value } => {
            sub(collection, ".collection", lexical, issues);
            sub(value, ".value", lexical, issues);
        }
        Expr::Map {
            collection,
            item,
            value,
            ..
        }
        | Expr::Filter {
            collection,
            item,
            predicate: value,
            ..
        } => {
            sub(collection, ".collection", lexical, issues);
            lexical.push(item.clone());
            let field = if matches!(expr, Expr::Map { .. }) {
                ".value"
            } else {
                ".predicate"
            };
            sub(value, field, lexical, issues);
            lexical.pop();
        }
        Expr::Lookup {
            key,
            cases,
            default,
        } => {
            if cases.len() > MAX_LOOKUP_CASES {
                issue(
                    issues,
                    "expression_limit",
                    path,
                    format!(
                        "{path}.{at}.lookup: has {} cases; a lookup has at most {MAX_LOOKUP_CASES}. Split the table, or compute part of the key first.",
                        cases.len()
                    ),
                );
            }
            let mut seen = BTreeMap::new();
            for (i, c) in cases.iter().enumerate() {
                let when = format!("{path}.{at}.lookup.cases[{i}].when");
                match canonical_bytes(&c.when) {
                    Err(e) => issue(
                        issues,
                        "lookup_case",
                        path,
                        format!(
                            "{when}: is not canonical JSON ({e}); use integers, strings, booleans, null, arrays or objects"
                        ),
                    ),
                    Ok(bytes) => {
                        if bytes.len() > MAX_LOOKUP_KEY_BYTES {
                            issue(
                                issues,
                                "expression_limit",
                                path,
                                format!(
                                    "{when}: is {} canonical bytes; a `when` has at most {MAX_LOOKUP_KEY_BYTES}",
                                    bytes.len()
                                ),
                            );
                        }
                        if let Some(first) = seen.insert(bytes, i) {
                            issue(
                                issues,
                                "lookup_case",
                                path,
                                format!(
                                    "{when}: {} equals cases[{first}].when; each `when` must be distinct, so remove or merge one of the two cases",
                                    shown(&c.when)
                                ),
                            );
                        }
                    }
                }
            }
            sub(key, ".key", lexical, issues);
            for (i, c) in cases.iter().enumerate() {
                sub(&c.then, &format!(".cases[{i}].then"), lexical, issues);
            }
            sub(default, ".default", lexical, issues);
        }
        Expr::Let { bindings, body } => {
            if bindings.len() > MAX_LET_BINDINGS {
                issue(
                    issues,
                    "expression_limit",
                    path,
                    format!(
                        "{path}.{at}.let: has {} bindings; a let has at most {MAX_LET_BINDINGS}. Nest a second let in the body for the rest.",
                        bindings.len()
                    ),
                );
            }
            let outer = lexical.len();
            for (i, b) in bindings.iter().enumerate() {
                sub(&b.value, &format!(".bindings[{i}].value"), lexical, issues);
                let name = &b.name;
                let refusal = if !valid_name(name) {
                    Some("is not a valid name (1-128 of A-Z a-z 0-9 _ - .)")
                } else if RESERVED_ROOTS.contains(&name.as_str()) {
                    Some("is a reserved root (input, event, definitions, vars)")
                } else if step_roots.contains(name) {
                    Some("is already the name of a step binding or foreach item in this playbook")
                } else if lexical.contains(name) {
                    Some(
                        "is already bound by an enclosing let, map or filter, or earlier in this let",
                    )
                } else {
                    None
                };
                if let Some(refusal) = refusal {
                    issue(
                        issues,
                        "binding",
                        path,
                        format!(
                            "{path}.{at}.let.bindings[{i}].name: `{name}` {refusal}; choose another name"
                        ),
                    );
                }
                lexical.push(name.clone());
            }
            sub(body, ".body", lexical, issues);
            lexical.truncate(outer);
        }
    }
}
/// Worst-case evaluation cost: nesting depth and elements charged to the budget.
struct EvalCost {
    depth: usize,
    elements: usize,
    lookup_or_let: bool,
}
fn eval_cost(expr: &Expr) -> EvalCost {
    let mut depth = 0;
    let mut lookup_or_let = matches!(expr, Expr::Lookup { .. } | Expr::Let { .. });
    let mut visit = |e: &Expr| {
        let c = eval_cost(e);
        depth = depth.max(c.depth + 1);
        lookup_or_let |= c.lookup_or_let;
        c.elements
    };
    let sum = |es: &mut dyn Iterator<Item = &Expr>, visit: &mut dyn FnMut(&Expr) -> usize| {
        es.fold(0usize, |n, e| n.saturating_add(visit(e)))
    };
    let below = match expr {
        Expr::Literal { .. } | Expr::Ref { .. } => 0,
        Expr::Object { fields } => sum(&mut fields.values(), &mut visit),
        Expr::Array { items: values } | Expr::And { values } | Expr::Or { values } => {
            sum(&mut values.iter(), &mut visit)
        }
        Expr::Exists { value } | Expr::Not { value } => visit(value),
        Expr::Eq { left, right }
        | Expr::Lt { left, right }
        | Expr::Contains {
            collection: left,
            value: right,
        } => visit(left).saturating_add(visit(right)),
        Expr::Difference {
            left,
            right,
            max_items,
            ..
        } => visit(left)
            .saturating_add(visit(right))
            .saturating_add(max_items.saturating_mul(2)),
        // Each item evaluates the body once; eval refuses more than max_items.
        Expr::Map {
            collection,
            value,
            max_items,
            ..
        }
        | Expr::Filter {
            collection,
            predicate: value,
            max_items,
            ..
        } => visit(collection).saturating_add(max_items.saturating_mul(visit(value))),
        // Every case is charged; one branch is evaluated.
        Expr::Lookup {
            key,
            cases,
            default,
        } => {
            let branch = cases
                .iter()
                .map(|c| &c.then)
                .chain([&**default])
                .map(&mut visit)
                .max()
                .unwrap_or(0);
            cases
                .len()
                .saturating_add(visit(key))
                .saturating_add(branch)
        }
        Expr::Let { bindings, body } => {
            sum(&mut bindings.iter().map(|b| &b.value), &mut visit).saturating_add(visit(body))
        }
    };
    EvalCost {
        depth,
        elements: below.saturating_add(1),
        lookup_or_let,
    }
}
/// An expression that uses lookup or let must fit the evaluator's limits on
/// every path, so nothing validation accepts fails on depth or element budget at
/// run time. Returns whether the depth was refused (typing then adds nothing).
fn check_eval_bounds(expr: &Expr, at: &str, path: &str, issues: &mut Vec<ValidationIssue>) -> bool {
    let cost = eval_cost(expr);
    if !cost.lookup_or_let {
        return false;
    }
    let limits = EvalLimits::default();
    if cost.depth > limits.max_depth {
        issue(
            issues,
            "expression_limit",
            path,
            format!(
                "{path}.{at}: nests {} levels deep; evaluation stops past depth {}. Flatten it: move part of the work to an earlier step, or turn a chain of lookups into one table.",
                cost.depth, limits.max_depth
            ),
        );
    }
    if cost.elements > limits.max_elements {
        issue(
            issues,
            "expression_limit",
            path,
            format!(
                "{path}.{at}: may charge up to {} elements; evaluation stops past {} (one per expression, one per lookup case, and a map or filter body once per item up to its max_items). Lower max_items or shrink the tables.",
                cost.elements, limits.max_elements
            ),
        );
    }
    cost.depth > limits.max_depth
}
fn walk(name: &str, steps: &BTreeMap<String, Step>, seen: &mut BTreeSet<String>) {
    let mut pending = vec![name.to_owned()];
    while let Some(name) = pending.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(s) = steps.get(&name) {
            pending.extend(children(s).into_iter().map(str::to_owned));
        }
    }
}
fn unbounded_cycle(
    name: &str,
    steps: &BTreeMap<String, Step>,
    done: &mut BTreeSet<String>,
    active: &mut BTreeSet<String>,
) -> bool {
    if done.contains(name) {
        return false;
    }
    if !active.insert(name.into()) {
        return true;
    }
    if let Some(s) = steps.get(name)
        && !matches!(s.op, Operation::ForEach { .. })
    {
        for n in children(s) {
            if unbounded_cycle(n, steps, done, active) {
                return true;
            }
        }
    }
    active.remove(name);
    done.insert(name.into());
    false
}

/// The item type of an empty array: no value has it, so it merges into any
/// type and is assignable to any (an empty union). Only inference produces it;
/// it never appears in a contract or an artifact.
fn no_type() -> Schema {
    Schema::Union { variants: vec![] }
}
fn is_no_type(schema: &Schema) -> bool {
    matches!(schema, Schema::Union { variants } if variants.is_empty())
}
/// A boolean position also accepts a value of no type (an item of `[]`).
fn is_boolean(schema: &Schema) -> bool {
    *schema == Schema::Boolean || is_no_type(schema)
}
fn literal_schema(value: &Value) -> Result<Schema, String> {
    match value {
        Value::Null => Ok(Schema::Null),
        Value::Bool(_) => Ok(Schema::Boolean),
        Value::Number(n) => n
            .as_i64()
            .map(|n| Schema::Integer {
                minimum: n,
                maximum: n,
            })
            .ok_or_else(|| "integer outside i64 domain".into()),
        Value::String(s) => Ok(Schema::String {
            max_length: s.len().max(1),
            values: BTreeSet::from([s.clone()]),
        }),
        Value::Array(a) => {
            let mut item = None;
            for v in a {
                let s = literal_schema(v)?;
                item = Some(match item {
                    None => s,
                    Some(old) => {
                        merge_schema(&old, &s).ok_or("heterogeneous arrays unsupported")?
                    }
                });
            }
            Ok(Schema::Array {
                items: Box::new(item.unwrap_or_else(no_type)),
                max_items: a.len().max(1),
            })
        }
        Value::Object(o) => Ok(Schema::object(
            o.iter()
                .map(|(k, v)| Ok((k.clone(), literal_schema(v)?)))
                .collect::<Result<_, String>>()?,
        )),
    }
}
fn merge_schema(a: &Schema, b: &Schema) -> Option<Schema> {
    if a == b || is_no_type(b) {
        return Some(a.clone());
    }
    if is_no_type(a) {
        return Some(b.clone());
    }
    match (a, b) {
        (Schema::Null, b @ Schema::Nullable { .. })
        | (b @ Schema::Nullable { .. }, Schema::Null) => Some(b.clone()),
        (Schema::Null, b) => Some(Schema::Nullable {
            inner: Box::new(b.clone()),
        }),
        (a, Schema::Null) => Some(Schema::Nullable {
            inner: Box::new(a.clone()),
        }),
        (Schema::Nullable { inner: a }, Schema::Nullable { inner: b }) => Some(Schema::Nullable {
            inner: Box::new(merge_schema(a, b)?),
        }),
        (Schema::Nullable { inner: a }, b) | (b, Schema::Nullable { inner: a }) => {
            Some(Schema::Nullable {
                inner: Box::new(merge_schema(a, b)?),
            })
        }
        (
            Schema::Integer {
                minimum: a,
                maximum: b,
            },
            Schema::Integer {
                minimum: c,
                maximum: d,
            },
        ) => Some(Schema::Integer {
            minimum: (*a).min(*c),
            maximum: (*b).max(*d),
        }),
        (
            Schema::String {
                max_length: a,
                values: x,
            },
            Schema::String {
                max_length: b,
                values: y,
            },
        ) => Some(Schema::String {
            max_length: (*a).max(*b),
            values: if x.is_empty() || y.is_empty() {
                BTreeSet::new()
            } else {
                x.union(y).cloned().collect()
            },
        }),
        (
            Schema::Array {
                items: a,
                max_items: x,
            },
            Schema::Array {
                items: b,
                max_items: y,
            },
        ) => Some(Schema::Array {
            items: Box::new(merge_schema(a, b)?),
            max_items: (*x).max(*y),
        }),
        (
            Schema::Object {
                properties: a,
                required: x,
            },
            Schema::Object {
                properties: b,
                required: y,
            },
        ) => {
            let mut properties = a.clone();
            for (k, v) in b {
                if let Some(old) = properties.get(k) {
                    properties.insert(k.clone(), merge_schema(old, v)?);
                } else {
                    properties.insert(k.clone(), v.clone());
                }
            }
            Some(Schema::Object {
                properties,
                required: x.intersection(y).cloned().collect(),
            })
        }
        _ => {
            let mut variants = vec![];
            for schema in [a, b] {
                match schema {
                    Schema::Union { variants: inner } => variants.extend(inner.iter().cloned()),
                    other => variants.push(other.clone()),
                }
            }
            variants.sort_by_key(|schema| canonical_bytes(schema).unwrap_or_default());
            variants.dedup();
            if variants.len() > 16 {
                None
            } else {
                Some(Schema::Union { variants })
            }
        }
    }
}
fn union_schemas<'a>(schemas: impl Iterator<Item = &'a Schema>) -> Option<Schema> {
    let mut result = None;
    for s in schemas {
        result = Some(match result {
            None => s.clone(),
            Some(old) => merge_schema(&old, s)?,
        });
    }
    result
}
/// A typing error at the location (inside the step) of the innermost
/// expression that caused it.
#[derive(Debug)]
struct TypeError {
    at: Option<String>,
    message: String,
    /// The structural checks already reported this fault; typing adds nothing.
    reported: bool,
}
impl From<String> for TypeError {
    fn from(message: String) -> Self {
        Self {
            at: None,
            message,
            reported: false,
        }
    }
}
impl From<&str> for TypeError {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}
impl TypeError {
    fn at(at: String, message: String) -> Self {
        Self {
            at: Some(at),
            message,
            reported: false,
        }
    }
    fn render(&self, path: &str) -> String {
        format!(
            "{path}.{}: {}",
            self.at.as_deref().unwrap_or_default(),
            self.message
        )
    }
}
/// A schema in words, for diagnostics an author fixes a draft from.
fn describe(schema: &Schema) -> String {
    let text = match schema {
        Schema::Null => "null".to_string(),
        Schema::Boolean => "boolean".to_string(),
        Schema::Integer { minimum, maximum } if minimum == maximum => {
            format!("the integer {minimum}")
        }
        Schema::Integer {
            minimum: i64::MIN,
            maximum: i64::MAX,
        } => "integer".to_string(),
        Schema::Integer { minimum, maximum } => format!("integer {minimum}..{maximum}"),
        Schema::String { max_length, values } if values.is_empty() => {
            format!("string of at most {max_length} bytes")
        }
        Schema::String { values, .. } => {
            let mut shown: Vec<_> = values.iter().take(12).map(|v| format!("{v:?}")).collect();
            if values.len() > 12 {
                shown.push(format!("and {} more", values.len() - 12));
            }
            format!("string, one of {}", shown.join(", "))
        }
        Schema::Array { items, .. } if is_no_type(items) => "empty array".to_string(),
        Schema::Array { items, max_items } => {
            format!("array of at most {max_items} ({})", describe(items))
        }
        Schema::Object {
            properties,
            required,
        } => format!(
            "object {{{}}}",
            properties
                .iter()
                .map(|(k, v)| {
                    let optional = if required.contains(k) { "" } else { "?" };
                    format!("{k}{optional}: {}", describe(v))
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Schema::Nullable { inner } => format!("null or {}", describe(inner)),
        Schema::Union { variants } if variants.is_empty() => "no value".to_string(),
        Schema::Union { variants } => format!(
            "one of ({})",
            variants
                .iter()
                .map(describe)
                .collect::<Vec<_>>()
                .join(" | ")
        ),
    };
    if text.len() <= 400 {
        return text;
    }
    let mut end = 397;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}
fn infer_expr(
    expr: &Expr,
    roots: &BTreeMap<String, Schema>,
    depth: usize,
    at: &str,
) -> Result<Schema, TypeError> {
    infer_node(expr, roots, depth, at).map_err(|mut e| {
        e.at.get_or_insert_with(|| at.to_string());
        e
    })
}
fn infer_node(
    expr: &Expr,
    roots: &BTreeMap<String, Schema>,
    depth: usize,
    at: &str,
) -> Result<Schema, TypeError> {
    if depth > 32 {
        return Err("expression nesting exceeds 32".into());
    }
    let sub = |e: &Expr, field: &str| infer_expr(e, roots, depth + 1, &operand(at, expr, field));
    match expr {
        Expr::Literal { value } => Ok(literal_schema(value)?),
        Expr::Ref { root, path } => {
            let current = roots
                .get(root)
                .ok_or_else(|| format!("unknown or unsupported typed root {root}"))?;
            schema_path(current, path)
                .map_err(|error| unreadable_ref(root, path, current, error).into())
        }
        Expr::Object { fields } => Ok(Schema::object(
            fields
                .iter()
                .map(|(k, e)| Ok((k.clone(), sub(e, &format!(".{k}"))?)))
                .collect::<Result<_, TypeError>>()?,
        )),
        Expr::Array { items } => {
            let mut s = None;
            for (i, e) in items.iter().enumerate() {
                let x = sub(e, &format!("[{i}]"))?;
                s = Some(match s {
                    None => x,
                    Some(old) => merge_schema(&old, &x)
                        .ok_or("heterogeneous expression array unsupported")?,
                });
            }
            Ok(Schema::Array {
                items: Box::new(s.unwrap_or_else(no_type)),
                max_items: items.len().max(1),
            })
        }
        Expr::Exists { value } => {
            sub(value, ".value")?;
            Ok(Schema::Boolean)
        }
        Expr::Not { value } => {
            if !is_boolean(&sub(value, ".value")?) {
                return Err("not requires boolean expression".into());
            }
            Ok(Schema::Boolean)
        }
        Expr::And { values } | Expr::Or { values } => {
            for (i, e) in values.iter().enumerate() {
                if !is_boolean(&sub(e, &format!(".values[{i}]"))?) {
                    return Err("boolean logic requires booleans".into());
                }
            }
            Ok(Schema::Boolean)
        }
        Expr::Eq { left, right } => {
            let a = sub(left, ".left")?;
            let b = sub(right, ".right")?;
            if merge_schema(&a, &b).is_none() {
                return Err("equality compares incompatible types".into());
            }
            Ok(Schema::Boolean)
        }
        Expr::Lt { left, right } => {
            let a = sub(left, ".left")?;
            let b = sub(right, ".right")?;
            if !matches!(
                (&a, &b),
                (Schema::Integer { .. }, Schema::Integer { .. })
                    | (Schema::String { .. }, Schema::String { .. })
            ) {
                return Err("ordering requires two integers or strings".into());
            }
            Ok(Schema::Boolean)
        }
        Expr::Contains { collection, value } => {
            let Schema::Array { items, .. } = sub(collection, ".collection")? else {
                return Err("membership requires bounded array".into());
            };
            if merge_schema(&items, &sub(value, ".value")?).is_none() {
                return Err("membership value has incompatible type".into());
            }
            Ok(Schema::Boolean)
        }
        Expr::Map {
            collection,
            item,
            value,
            max_items,
        }
        | Expr::Filter {
            collection,
            item,
            predicate: value,
            max_items,
        } => {
            let Schema::Array {
                items,
                max_items: bound,
            } = sub(collection, ".collection")?
            else {
                return Err("collection expression requires bounded array".into());
            };
            if *max_items == 0 || bound > *max_items || *max_items > 100_000 {
                return Err("collection expression limit cannot cover input schema".into());
            }
            if roots.contains_key(item) || !valid_name(item) {
                return Err("invalid/shadowing collection lexical root".into());
            }
            let mut local = roots.clone();
            local.insert(item.clone(), *items.clone());
            let filter = matches!(expr, Expr::Filter { .. });
            let field = if filter { ".predicate" } else { ".value" };
            let result = infer_expr(value, &local, depth + 1, &operand(at, expr, field))?;
            if filter {
                if !is_boolean(&result) {
                    return Err("filter predicate requires boolean".into());
                }
                Ok(Schema::Array {
                    items,
                    max_items: bound,
                })
            } else {
                Ok(Schema::Array {
                    items: Box::new(result),
                    max_items: bound,
                })
            }
        }
        Expr::Difference {
            left,
            right,
            identity,
            max_items,
        } => {
            let Schema::Array {
                items: a,
                max_items: x,
            } = sub(left, ".left")?
            else {
                return Err("difference requires bounded arrays".into());
            };
            let Schema::Array {
                items: b,
                max_items: y,
            } = sub(right, ".right")?
            else {
                return Err("difference requires bounded arrays".into());
            };
            if *max_items == 0 || x > *max_items || y > *max_items || identity.is_empty() {
                return Err(
                    "difference requires explicit identity and covering element bound".into(),
                );
            }
            let roots = BTreeMap::from([("left".into(), *a.clone()), ("right".into(), *b)]);
            let identity_at = operand(at, expr, ".identity");
            let a_key = infer_expr(
                &Expr::Ref {
                    root: "left".into(),
                    path: identity.clone(),
                },
                &roots,
                depth + 1,
                &identity_at,
            )?;
            let b_key = infer_expr(
                &Expr::Ref {
                    root: "right".into(),
                    path: identity.clone(),
                },
                &roots,
                depth + 1,
                &identity_at,
            )?;
            if merge_schema(&a_key, &b_key).is_none() {
                return Err("difference identities incompatible".into());
            }
            Ok(Schema::Array {
                items: a,
                max_items: x,
            })
        }
        Expr::Lookup {
            key,
            cases,
            default,
        } => {
            check_lookup_key(key, cases, roots, depth, at)?;
            let mut results = vec![];
            for (i, c) in cases.iter().enumerate() {
                results.push((
                    format!("cases[{i}].then"),
                    sub(&c.then, &format!(".cases[{i}].then"))?,
                ));
            }
            results.push(("default".into(), sub(default, ".default")?));
            union_schemas(results.iter().map(|(_, s)| s)).ok_or_else(|| {
                let branches: Vec<_> = results
                    .iter()
                    .map(|(branch, s)| format!("{branch}: {}", describe(s)))
                    .collect();
                TypeError::at(
                    format!("{at}.lookup"),
                    format!(
                        "the results have more than 16 distinct types, so the lookup has no type. Make the branches the same kind of value. Branch types: {}",
                        branches.join("; ")
                    ),
                )
            })
        }
        Expr::Let { bindings, body } => {
            let local = let_roots(bindings, roots, depth, at)?;
            infer_expr(body, &local, depth + 1, &format!("{at}.let.body"))
        }
    }
}
/// The key's type, loosened for matching: any integer and any string length
/// can be a key's value, but a string enum, a kind and a shape are kept.
fn matchable(schema: &Schema) -> Schema {
    match schema {
        Schema::Integer { .. } => Schema::Integer {
            minimum: i64::MIN,
            maximum: i64::MAX,
        },
        Schema::String { values, .. } => Schema::String {
            max_length: usize::MAX,
            values: values.clone(),
        },
        Schema::Array { items, max_items } => Schema::Array {
            items: Box::new(matchable(items)),
            max_items: *max_items,
        },
        Schema::Object {
            properties,
            required,
        } => Schema::Object {
            properties: properties
                .iter()
                .map(|(k, v)| (k.clone(), matchable(v)))
                .collect(),
            required: required.clone(),
        },
        Schema::Nullable { inner } => Schema::Nullable {
            inner: Box::new(matchable(inner)),
        },
        Schema::Union { variants } => Schema::Union {
            variants: variants.iter().map(matchable).collect(),
        },
        other => other.clone(),
    }
}
/// Every `when` must be a value the key can take, so each case can match.
fn check_lookup_key(
    key: &Expr,
    cases: &[LookupCase],
    roots: &BTreeMap<String, Schema>,
    depth: usize,
    at: &str,
) -> Result<(), TypeError> {
    let key_at = format!("{at}.lookup.key");
    let key_type = infer_expr(key, roots, depth + 1, &key_at)?;
    let can_match = matchable(&key_type);
    let never: Vec<_> = cases
        .iter()
        .enumerate()
        .filter(|(_, c)| can_match.validate(&c.when).is_err())
        .collect();
    let Some(((first, c), rest)) = never.split_first() else {
        return Ok(());
    };
    let mut message = format!(
        "`when` {} can never match: the key ({key_at}) has type {}",
        shown(&c.when),
        describe(&key_type)
    );
    if !rest.is_empty() {
        let others: Vec<_> = rest
            .iter()
            .map(|(i, c)| format!("cases[{i}] ({})", shown(&c.when)))
            .collect();
        message.push_str(&format!("; nor can {}", others.join(", ")));
    }
    message.push_str(". Change the `when` to a value of the key's type, or change the key.");
    Err(TypeError::at(
        format!("{at}.lookup.cases[{first}].when"),
        message,
    ))
}
/// The roots visible to a let body: each binding typed with the bindings before it.
fn let_roots(
    bindings: &[LetBinding],
    roots: &BTreeMap<String, Schema>,
    depth: usize,
    at: &str,
) -> Result<BTreeMap<String, Schema>, TypeError> {
    let mut local = roots.clone();
    for (i, b) in bindings.iter().enumerate() {
        if local.contains_key(&b.name) || !valid_name(&b.name) {
            // check_expr_structure refuses every such name, with its reason.
            return Err(TypeError {
                at: Some(format!("{at}.let.bindings[{i}].name")),
                message: format!("let binding `{}` shadows a root", b.name),
                reported: true,
            });
        }
        let schema = infer_expr(
            &b.value,
            &local,
            depth + 1,
            &format!("{at}.let.bindings[{i}].value"),
        )?;
        local.insert(b.name.clone(), schema);
    }
    Ok(local)
}
fn type_issue(issues: &mut Vec<ValidationIssue>, path: &str, e: TypeError) {
    if !e.reported {
        issue(issues, "expression_type", path, e.render(path));
    }
}
/// Checks `expr`, at location `at` inside the step at `path`, against `target`.
fn check_expr_compat(
    expr: &Expr,
    roots: &BTreeMap<String, Schema>,
    target: &Schema,
    path: &str,
    at: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    // Empty literals and arrays have no inherent element type; validate literal
    // values directly against the expected schema rather than inventing one.
    if let Expr::Literal { value } = expr {
        if target.validate(value).is_err() {
            issue(
                issues,
                "schema_compatibility",
                path,
                format!(
                    "{path}.{at}: literal {} is not a value of the target type {}",
                    shown(value),
                    describe(target)
                ),
            );
        }
        return;
    }
    if let (
        Expr::Object { fields },
        Schema::Object {
            properties,
            required,
        },
    ) = (expr, target)
    {
        let missing: Vec<_> = required
            .iter()
            .filter(|k| !fields.contains_key(*k))
            .collect();
        let unknown: Vec<_> = fields
            .keys()
            .filter(|k| !properties.contains_key(*k))
            .collect();
        if !missing.is_empty() || !unknown.is_empty() {
            issue(
                issues,
                "schema_compatibility",
                path,
                format!(
                    "{path}.{at}: expression object keys incompatible with closed target schema: missing required {missing:?}, not allowed {unknown:?}; the target is {}",
                    describe(target)
                ),
            );
        }
        for (k, e) in fields {
            if let Some(t) = properties.get(k) {
                check_expr_compat(e, roots, t, path, &format!("{at}.{k}"), issues);
            }
        }
        return;
    }
    if let (
        Expr::Array { items },
        Schema::Array {
            items: target,
            max_items,
        },
    ) = (expr, target)
    {
        if items.len() > *max_items {
            issue(
                issues,
                "schema_compatibility",
                path,
                format!(
                    "{path}.{at}: expression array exceeds target bound: {} items, at most {max_items}",
                    items.len()
                ),
            );
        }
        for (i, e) in items.iter().enumerate() {
            check_expr_compat(e, roots, target, path, &format!("{at}[{i}]"), issues);
        }
        return;
    }
    // Each result of a lookup, and a let's body, meets the target directly, so a
    // refused branch is named with its own type.
    if let Expr::Lookup {
        key,
        cases,
        default,
    } = expr
    {
        if let Err(e) = check_lookup_key(key, cases, roots, 0, at) {
            type_issue(issues, path, e);
            return;
        }
        for (i, c) in cases.iter().enumerate() {
            let at = format!("{at}.lookup.cases[{i}].then");
            check_expr_compat(&c.then, roots, target, path, &at, issues);
        }
        check_expr_compat(
            default,
            roots,
            target,
            path,
            &format!("{at}.lookup.default"),
            issues,
        );
        return;
    }
    if let Expr::Let { bindings, body } = expr {
        match let_roots(bindings, roots, 0, at) {
            Ok(local) => check_expr_compat(
                body,
                &local,
                target,
                path,
                &format!("{at}.let.body"),
                issues,
            ),
            Err(e) => type_issue(issues, path, e),
        }
        return;
    }
    match infer_expr(expr, roots, 0, at) {
        Ok(source) => {
            if !source.assignable_to(target) {
                issue(
                    issues,
                    "schema_compatibility",
                    path,
                    format!(
                        "{path}.{at}: expression type/bounds not assignable to target schema: it has type {}, the target is {}",
                        describe(&source),
                        describe(target)
                    ),
                );
            }
        }
        Err(e) => type_issue(issues, path, e),
    }
}
impl ArtifactContent {
    fn validate_types(
        &self,
        entry_name: &str,
        entry: &EntryPoint,
        reachable: &BTreeSet<String>,
        too_deep: &BTreeSet<(String, String)>,
        issues: &mut Vec<ValidationIssue>,
    ) {
        let p = &self.playbook;
        let mut roots = BTreeMap::from([("input".into(), entry.input.clone())]);
        if let Ok(definitions) = literal_schema(&Value::Object(
            self.definitions
                .iter()
                .map(|(k, d)| (k.clone(), d.value.clone()))
                .collect(),
        )) {
            roots.insert("definitions".into(), definitions);
        }
        for name in reachable {
            let Some(s) = p.steps.get(name) else {
                continue;
            };
            match &s.op {
                Operation::Invoke { command, bind, .. } => {
                    if let Some(c) = self.registry.commands.get(&command.name)
                        && let Some(s) = union_schemas(c.outcomes.values())
                    {
                        roots.insert(bind.clone(), s);
                    }
                }
                Operation::Wait {
                    event: Some(event),
                    bind,
                    ..
                } => {
                    if let Some(e) = self.registry.events.get(&event.name) {
                        roots.insert(bind.clone(), e.payload.clone());
                    }
                }
                Operation::Call {
                    dependency,
                    entry,
                    mode,
                    bind,
                    ..
                } => {
                    if *mode == CallMode::Wait {
                        if let Some(e) = self
                            .dependencies
                            .get(dependency)
                            .and_then(|d| d.interfaces.get(entry))
                            && let Some(s) = union_schemas(e.outcomes.values())
                        {
                            roots.insert(bind.clone(), s);
                        }
                    } else {
                        roots.insert(
                            bind.clone(),
                            Schema::object(BTreeMap::from([("child".into(), Schema::string(128))])),
                        );
                    }
                }
                Operation::Join { bind, .. } => {
                    if let Ok(Some(schema)) = self.join_schema(s, false) {
                        roots.insert(bind.clone(), schema);
                    }
                }
                _ => {}
            }
        }
        // Iteration collections may depend on an outer captured iteration item.
        // Resolve lexical item schemas to a fixed point bounded by step count.
        for _ in 0..=p.limits.max_iteration_depth {
            let old = roots.len();
            for name in reachable {
                if let Some(Step {
                    op:
                        Operation::ForEach {
                            collection, item, ..
                        },
                    ..
                }) = p.steps.get(name)
                    && let Ok(Schema::Array { items, .. }) =
                        infer_expr(collection, &roots, 0, "collection")
                {
                    roots.entry(item.clone()).or_insert(*items);
                }
            }
            if old == roots.len() {
                break;
            }
        }
        let contexts = self.outcome_contexts(entry, &roots);
        for name in reachable {
            let Some(s) = p.steps.get(name) else {
                continue;
            };
            let mut roots = contexts
                .get(name)
                .map(|c| c.roots.clone())
                .unwrap_or_else(|| roots.clone());
            let vars = Schema::Object {
                properties: roots
                    .iter()
                    .filter(|(k, _)| !matches!(k.as_str(), "input" | "definitions" | "vars"))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                required: BTreeSet::new(),
            };
            roots.insert("vars".into(), vars);
            let path = format!("steps.{name}");
            let typed = |at: &str| !too_deep.contains(&(path.clone(), at.to_string()));
            let check = |e: &Expr, at: &str, target: &Schema, issues: &mut Vec<_>| {
                if typed(at) {
                    check_expr_compat(e, &roots, target, &path, at, issues);
                }
            };
            match &s.op {
                Operation::Invoke { command, input, .. } => {
                    if let Some(c) = self.registry.commands.get(&command.name) {
                        check(input, "input", &c.input, issues);
                    }
                }
                Operation::Decide { predicate } => {
                    check(predicate, "predicate", &Schema::Boolean, issues)
                }
                Operation::Wait {
                    correlation,
                    minimum_revision,
                    ..
                } => {
                    for (k, e) in correlation {
                        check(
                            e,
                            &format!("correlation.{k}"),
                            &Schema::string(1024),
                            issues,
                        );
                    }
                    if let Some(e) = minimum_revision {
                        let revision = Schema::Integer {
                            minimum: 0,
                            maximum: i64::MAX,
                        };
                        check(e, "minimum_revision", &revision, issues);
                    }
                }
                Operation::ForEach {
                    collection,
                    max_items,
                    ..
                } if typed("collection") => match infer_expr(collection, &roots, 0, "collection") {
                    Ok(Schema::Array {
                        max_items: bound, ..
                    }) => {
                        if bound > *max_items {
                            issue(
                                issues,
                                "iteration_limit",
                                &path,
                                "foreach item bound does not cover collection schema",
                            );
                        }
                    }
                    Ok(_) => issue(
                        issues,
                        "expression_type",
                        &path,
                        "foreach collection must be bounded array",
                    ),
                    Err(e) => type_issue(issues, &path, e),
                },
                Operation::ForEach { .. } => {}
                Operation::Call {
                    dependency,
                    entry,
                    input,
                    ..
                } => {
                    if let Some(e) = self
                        .dependencies
                        .get(dependency)
                        .and_then(|d| d.interfaces.get(entry))
                    {
                        check(input, "input", &e.input, issues);
                    }
                }
                Operation::Join { members, .. } => {
                    for (i, e) in members.iter().enumerate() {
                        let at = format!("members[{i}]");
                        if typed(&at)
                            && let Err(e) = infer_expr(e, &roots, 0, &at)
                        {
                            type_issue(issues, &path, e);
                        }
                    }
                }
                Operation::Return { outcome, output } => {
                    if let Some(target) = entry.outcomes.get(outcome) {
                        check(output, "output", target, issues);
                    } else {
                        issue(
                            issues,
                            "return_outcome",
                            &path,
                            format!("outcome {outcome} not declared by entry {entry_name}"),
                        );
                    }
                }
            }
        }
    }
}

#[derive(Clone, PartialEq)]
struct TypeState {
    roots: BTreeMap<String, Schema>,
    untyped: BTreeSet<String>,
}
impl TypeState {
    fn merge(&mut self, other: &Self) -> bool {
        let before = self.clone();
        self.untyped.extend(other.untyped.iter().cloned());
        for key in self
            .roots
            .keys()
            .chain(other.roots.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
        {
            match (self.roots.get(&key), other.roots.get(&key)) {
                (Some(a), Some(b)) if !self.untyped.contains(&key) => {
                    if let Some(s) = merge_schema(a, b) {
                        self.roots.insert(key, s);
                    } else {
                        self.untyped.insert(key.clone());
                        self.roots.remove(&key);
                    }
                }
                _ => {
                    self.untyped.insert(key.clone());
                    self.roots.remove(&key);
                }
            }
        }
        *self != before
    }
    fn narrow(&mut self, root: &str, schema: &Schema) {
        self.untyped.remove(root);
        self.roots.insert(root.into(), schema.clone());
    }
}
impl ArtifactContent {
    fn outcome_contexts(
        &self,
        entry: &EntryPoint,
        roots: &BTreeMap<String, Schema>,
    ) -> BTreeMap<String, TypeState> {
        let mut contexts = BTreeMap::from([(
            entry.start.clone(),
            TypeState {
                roots: roots.clone(),
                untyped: BTreeSet::new(),
            },
        )]);
        let mut pending = std::collections::VecDeque::from([entry.start.clone()]);
        let mut queued = BTreeSet::from([entry.start.clone()]);
        while let Some(name) = pending.pop_front() {
            queued.remove(&name);
            let Some(step) = self.playbook.steps.get(&name) else {
                continue;
            };
            let current = contexts[&name].clone();
            let mut targets: Vec<(String, TypeState)> = vec![];
            for (outcome, target) in &step.edges {
                let mut next = current.clone();
                match &step.op {
                    Operation::Invoke { command, bind, .. } => {
                        if let Some(schema) = self
                            .registry
                            .commands
                            .get(&command.name)
                            .and_then(|c| c.outcomes.get(outcome))
                        {
                            next.narrow(bind, schema);
                        }
                    }
                    Operation::Call {
                        dependency,
                        entry,
                        mode: CallMode::Wait,
                        bind,
                        ..
                    } => {
                        if let Some(schema) = self
                            .dependencies
                            .get(dependency)
                            .and_then(|d| d.interfaces.get(entry))
                            .and_then(|e| e.outcomes.get(outcome))
                        {
                            next.narrow(bind, schema);
                        }
                    }
                    Operation::Join { bind, .. } if outcome == "completed" => {
                        if let Ok(Some(schema)) = self.join_schema(step, true) {
                            next.narrow(bind, &schema);
                        }
                    }
                    _ => {}
                }
                targets.push((target.clone(), next));
            }
            if let Some(target) = &step.on_error {
                targets.push((target.clone(), current.clone()));
            }
            if let Operation::ForEach { body, next, .. } = &step.op {
                targets.push((body.clone(), current.clone()));
                targets.push((next.clone(), current.clone()));
            }
            for (target, next) in targets {
                let changed = if let Some(existing) = contexts.get_mut(&target) {
                    existing.merge(&next)
                } else {
                    contexts.insert(target.clone(), next);
                    true
                };
                if changed && queued.insert(target.clone()) {
                    pending.push_back(target);
                }
            }
        }
        contexts
    }
}

fn schema_path(schema: &Schema, path: &[String]) -> Result<Schema, String> {
    let Some((key, rest)) = path.split_first() else {
        return Ok(schema.clone());
    };
    match schema {
        Schema::Object { properties, .. } => schema_path(
            properties
                .get(key)
                .ok_or_else(|| format!("unknown field {key}"))?,
            rest,
        ),
        Schema::Array { items, .. } => {
            key.parse::<usize>()
                .map_err(|_| "array index must be nonnegative integer".to_string())?;
            schema_path(items, rest)
        }
        Schema::Union { variants } if variants.is_empty() => Ok(no_type()),
        Schema::Union { variants } => {
            let schemas = variants
                .iter()
                .map(|schema| schema_path(schema, path))
                .collect::<Result<Vec<_>, _>>()?;
            union_schemas(schemas.iter())
                .ok_or_else(|| "union projection exceeds alternative limit".into())
        }
        _ => Err(format!(
            "cannot traverse scalar/nullable reference at {key}"
        )),
    }
}
fn dotted(root: &str, path: &[String]) -> String {
    std::iter::once(root)
        .chain(path.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(".")
}
/// Why `root.path` cannot be read, said at the deepest part that can: what is
/// there, and where the missing part would be readable.
fn unreadable_ref(root: &str, path: &[String], schema: &Schema, error: String) -> String {
    let mut at = schema.clone();
    let mut parent = None;
    let mut depth = 0;
    while let Some(key) = path.get(depth) {
        match schema_path(&at, std::slice::from_ref(key)) {
            Ok(next) => {
                parent = Some(std::mem::replace(&mut at, next));
                depth += 1;
            }
            Err(_) => break,
        }
    }
    let Some(key) = path.get(depth) else {
        return format!("`{}` cannot be read: {error}", dotted(root, path));
    };
    let prefix = dotted(root, &path[..depth]);
    let fields = |properties: &BTreeMap<String, Schema>| {
        properties.keys().cloned().collect::<Vec<_>>().join(", ")
    };
    // A named join's answer for one member: {participant, outcome, output}.
    let join_answer = matches!(&parent, Some(Schema::Object { properties, .. })
        if properties.keys().map(String::as_str).eq(["outcome", "output", "participant"]));
    let why = match &at {
        Schema::Object { properties, .. } => {
            format!("{error}: `{prefix}` has fields {}", fields(properties))
        }
        Schema::Nullable { .. } if join_answer => format!(
            "`{prefix}` may be null at this step, so `{key}` cannot be read. A join's \
             answer (`outcome`, `output`) is present only on the join's `completed` edge, \
             in mode `all` or `any_accepted`: read it in a step reached through that edge"
        ),
        Schema::Nullable { .. } => format!(
            "`{prefix}` may be null at this step, so `{key}` cannot be read. Read it after \
             an edge on which it is not null, or test it first with `exists`"
        ),
        Schema::Union { variants } => {
            let objects: Vec<_> = variants
                .iter()
                .filter_map(|v| match v {
                    Schema::Object { properties, .. } => Some(properties),
                    _ => None,
                })
                .collect();
            let common: Vec<_> = objects
                .first()
                .map(|first| {
                    first
                        .keys()
                        .filter(|k| objects.iter().all(|o| o.contains_key(*k)))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            let some = if objects.iter().any(|o| o.contains_key(key)) {
                "only some"
            } else {
                "none"
            };
            format!(
                "`{prefix}` has a different shape per alternative and {some} of them have \
                 `{key}`; every alternative has {}. Read `{key}` after an edge that selects \
                 an alternative that has it, or test it first with `exists`",
                if common.is_empty() {
                    "no common field".to_string()
                } else {
                    format!("fields {}", common.join(", "))
                }
            )
        }
        Schema::Array { .. } => {
            format!("{error} (`{prefix}` is an array; `{key}` is not an index)")
        }
        other => format!(
            "`{prefix}` is {}, which has no field `{key}`",
            describe(other)
        ),
    };
    format!("`{}` cannot be read: {why}", dotted(root, path))
}
impl ArtifactContent {
    /// A join names an operation handle by the binding of the invoke that
    /// admitted it, as the runner resolves members (`{"handle": h}`). Returns
    /// the fix when `member` reads a path inside such a binding instead.
    fn handle_member_path(&self, member: &Expr) -> Option<String> {
        let Expr::Ref { root, path } = member else {
            return None;
        };
        let (binding, rest) = match (root.as_str(), path.split_first()) {
            ("vars", Some((binding, rest))) => (binding, rest),
            ("vars", None) => return None,
            _ => (root, path.as_slice()),
        };
        if rest.is_empty() {
            return None;
        }
        let (step, command) = self
            .playbook
            .steps
            .iter()
            .find_map(|(name, s)| match &s.op {
                Operation::Invoke { command, bind, .. } if bind == binding => {
                    Some((name, &command.name))
                }
                _ => None,
            })?;
        let written = dotted(root, path);
        if self
            .registry
            .commands
            .get(command)
            .is_some_and(|c| c.asynchronous.is_none())
        {
            return Some(format!(
                "`{written}` is a result of step `{step}`, whose command {command} is \
                 synchronous; only the handle of an asynchronous command can be joined. Use \
                 the result directly on the step's outcome edges instead of joining it"
            ));
        }
        Some(format!(
            "`{written}` reads inside the operation handle bound by step `{step}` ({command}); \
             a join names the handle by its binding alone: write \
             {{\"op\":\"ref\",\"root\":\"{binding}\",\"path\":[]}}"
        ))
    }
    fn member_outcomes(&self, member: &Expr) -> Result<BTreeMap<String, Schema>, String> {
        let Expr::Ref { root, path } = member else {
            return Err("named join members must reference statically declared owned child or operation handles".into());
        };
        let (binding, path) = if root == "vars" {
            let Some((binding, path)) = path.split_first() else {
                return Err("join member vars reference needs a handle name".into());
            };
            (binding.as_str(), path)
        } else {
            (root.as_str(), path.as_slice())
        };
        for step in self.playbook.steps.values() {
            match &step.op {
                Operation::Call {
                    dependency,
                    entry,
                    mode: CallMode::OwnedAsync,
                    bind,
                    ..
                } if bind == binding && (path.is_empty() || path == ["child"]) => {
                    return self
                        .dependencies
                        .get(dependency)
                        .and_then(|d| d.interfaces.get(entry))
                        .map(|entry| entry.outcomes.clone())
                        .ok_or_else(|| "join references unknown child interface".into());
                }
                // `["handle"]` types as the handle itself so that stored
                // artifacts keep verifying; a new draft is refused it
                // (`draft_issues`), and the runner refuses it at the join.
                Operation::Invoke { command, bind, .. }
                    if bind == binding && (path.is_empty() || path == ["handle"]) =>
                {
                    let command = self
                        .registry
                        .commands
                        .get(&command.name)
                        .ok_or("join references unknown command")?;
                    let asynchronous = command
                        .asynchronous
                        .as_ref()
                        .ok_or("join operation must have asynchronous contract")?;
                    let Schema::Object { properties, .. } = &asynchronous.completion else {
                        return Err("async completion must be object".into());
                    };
                    let Some(Schema::String { values, .. }) = properties.get("outcome") else {
                        return Err("async completion requires outcome enum".into());
                    };
                    return Ok(command
                        .outcomes
                        .iter()
                        .filter(|(outcome, _)| values.contains(*outcome))
                        .map(|(name, schema)| (name.clone(), schema.clone()))
                        .collect());
                }
                _ => {}
            }
        }
        Err(format!(
            "join member {binding} is not a statically declared owned asynchronous handle"
        ))
    }
    fn join_schema(&self, step: &Step, completed: bool) -> Result<Option<Schema>, String> {
        let Operation::Join {
            members,
            member_keys,
            mode,
            accepted_outcomes,
            ..
        } = &step.op
        else {
            return Ok(None);
        };
        if member_keys.is_empty() {
            return Ok(None);
        }
        if member_keys.len() != members.len() {
            return Err("join key/member cardinality mismatch".into());
        }
        let guaranteed_business = completed && *mode != JoinMode::AnyCompletion;
        let mut properties = BTreeMap::new();
        for (i, (key, member)) in member_keys.iter().zip(members).enumerate() {
            let outcomes = self
                .member_outcomes(member)
                .map_err(|e| format!("members[{i}] (key {key}): {e}"))?;
            let selected: BTreeMap<_, _> = outcomes
                .iter()
                .filter(|(outcome, _)| {
                    !guaranteed_business
                        || accepted_outcomes.is_empty()
                        || accepted_outcomes.contains(*outcome)
                })
                .map(|(outcome, schema)| (outcome.clone(), schema.clone()))
                .collect();
            // A member whose vocabulary has no qualifying outcome cannot appear
            // in an AnyAccepted result; the optional key is absent in that case.
            if guaranteed_business && selected.is_empty() {
                continue;
            }
            let output = union_schemas(selected.values())
                .ok_or("join member result exceeds bounded union limit")?;
            let outcome = Schema::String {
                max_length: selected.keys().map(String::len).max().unwrap_or(1).max(1),
                values: selected.keys().cloned().collect(),
            };
            let (outcome, output) = if guaranteed_business {
                (outcome, output)
            } else {
                (
                    Schema::Nullable {
                        inner: Box::new(outcome),
                    },
                    Schema::Nullable {
                        inner: Box::new(output),
                    },
                )
            };
            properties.insert(
                key.clone(),
                Schema::object(BTreeMap::from([
                    ("participant".into(), Schema::string(128)),
                    ("outcome".into(), outcome),
                    ("output".into(), output),
                ])),
            );
        }
        let required = if guaranteed_business && *mode == JoinMode::All {
            properties.keys().cloned().collect()
        } else {
            BTreeSet::new()
        };
        Ok(Some(Schema::Object {
            properties,
            required,
        }))
    }
}
