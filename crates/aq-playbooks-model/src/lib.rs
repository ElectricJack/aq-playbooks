//! Versioned, bounded portable workflow format. See FORMAT.md for canonical rules.
mod draft;
mod evaluation;
mod extensions;
pub use evaluation::eval;
mod compatibility;
use compatibility::Compatibility;
mod diagnostics;
mod inference;
mod joins;
pub mod schedules;
mod schema;
use inference::{infer_expr, let_roots, type_issue, validate_lookup_key};
mod schema_types;
use diagnostics::{describe, dotted, unreadable_ref};
pub use schema::Schema;
use schema_types::{
    is_boolean, is_no_type, literal_schema, merge_schema, no_type, schema_path, union_schemas,
};
mod structure;
mod typing;
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
    validate_event_identity(event, c)?;
    if !event.correlation.keys().eq(c.correlation_fields.iter()) {
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

fn validate_event_identity(
    event: &EventEnvelope,
    contract: &EventContract,
) -> Result<(), ModelError> {
    let trusted = contract.trusted_producers.contains(&event.producer)
        && contract.scopes.contains(&event.scope);
    if event.event_id.is_empty() || !trusted {
        return Err(ModelError::Invalid(
            "event identity/producer/scope refused".into(),
        ));
    }
    Ok(())
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
impl Operation {
    /// Every expression of an operation, with its location inside the step.
    pub fn expressions(&self) -> Vec<(String, &Expr)> {
        match self {
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
