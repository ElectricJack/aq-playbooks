//! Checks a new draft is held to that `Artifact::verify` does not run, so an
//! artifact stored before them keeps loading (FORMAT.md, "What a draft is
//! held to"):
//! - a join member that is an operation handle is named by its binding alone;
//! - strict reads: a read through a field that some outcome lacks is refused;
//! - bound reads: so is a read of a binding some way to the step never bound;
//! - aliasing does not launder: so is a read through any expression that
//!   carries such a value;
//! - bounded cost: the types an expression reads, and the types in scope
//!   over the steps, are bounded in size.
//!
//! All of it rests on ONE derivation, `Expression::derive`: what every expression
//! carries (`Carried`: the forms its value can have on each way of reaching
//! the step, the ways it is Missing, and its expanded size). It is one match
//! over every `Expr` variant; `StepBindings::parts` is one match over every
//! `Operation` and says how each step puts a value in scope (`Held`); and
//! `Pass::reserved` is one match over the reserved roots. None has a wildcard
//! arm, and the lints below refuse one, so a new variant does not compile
//! until it is decided here. A read is refused where a Missing value is
//! consumed (`Pass::sink`): there is no second walk over expressions.
//!
//! Ordinary typing merges the outcomes a binding can hold into one object, in
//! which a field only one outcome has is merely optional, and an optional
//! field types as present. Here each binding keeps its outcomes apart.
#![deny(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]
mod expressions;
mod flow;
mod reads;
mod reporting;
use expressions::Site;
pub use reporting::DraftSize;
mod shapes;
mod step;
use super::*;
use shapes::Binding;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

/// The most nodes one expression of a new draft may count, and all of its
/// expressions together. An expression counts its own nodes, a literal the
/// nodes of its value, and every reference the nodes of the type it reads
/// (whatever it refers to: a root, a binding, a `let` name, an item). What a
/// name stands for is counted once, where it is written; a name whose type
/// doubles at every level still counts double at every level.
pub const MAX_DRAFT_EXPANSION: u64 = 200_000;
pub const MAX_DRAFT_EXPANSION_TOTAL: u64 = 1_000_000;
/// The most type nodes all the steps of a draft may have in scope together:
/// for each step, the types of its entry's `input`, of the definitions and of
/// every binding and item of the playbook. Ordinary typing copies them for
/// every step, several times, when a draft is checked and on every load.
pub const MAX_DRAFT_SCOPE: u64 = 250_000;
/// The most times the flow over the steps may revisit them before the draft
/// is refused as too entangled to check.
const MAX_FLOW_VISITS_PER_STEP: usize = 64;
/// A value with more shapes than this counts toward the work of following
/// values of many shapes, which is bounded over the whole draft.
const MANY_SHAPES: u64 = 16;
const MAX_SHAPE_WORK: u64 = 1_000_000;
/// The most reasons one diagnostic lists.
const MAX_REASONS: usize = 12;
/// The deepest a `for_each` item's derived value may nest.
const MAX_ITEM_DEPTH: u32 = 256;

// ---------------------------------------------------------------------------
// What a value is.
// ---------------------------------------------------------------------------

/// What a value can be, over every way of reaching the step that reads it.
#[derive(Clone)]
struct Carried<'a> {
    /// Ways on which the value is not there, not yet read by any reference:
    /// only the values steps put in scope have these.
    lacks: Vec<Rc<str>>,
    /// Ways on which the value is Missing because a reference inside the
    /// expression that computed it read something that was not there.
    gaps: Gaps,
    /// What it is where it is there: one form per way.
    forms: Rc<[Form<'a>]>,
    /// Nodes of its type, as ordinary typing would build it.
    nodes: u64,
    /// How deep its built forms nest.
    depth: u32,
}
/// One form a present value has.
#[derive(Clone)]
enum Form<'a> {
    /// A value of this schema, arising on `way`; `name` is how a direct
    /// reference writes it. A field optional in the schema is readable.
    Shape {
        schema: &'a Schema,
        way: Rc<str>,
        name: Rc<str>,
    },
    /// A present value whose inside this check does not follow (a literal, a
    /// boolean, a scalar, a result ordinary typing gives no shape); the
    /// number is the node count of its type.
    Opaque(u64),
    /// An object or array an expression builds, a named join's result, or
    /// `vars`: each key or position with what it carries.
    Built(Rc<Built<'a>>),
    /// An array whose every item is this (the result of `map`, `filter` or
    /// `difference`).
    Items(Rc<Carried<'a>>),
}
struct Built<'a> {
    /// The field names of an object, sorted; none for an array.
    fields: Vec<String>,
    /// What each field, or each position, carries.
    values: Vec<Carried<'a>>,
    array: bool,
    nodes: u64,
    depth: u32,
}
impl<'a> Built<'a> {
    fn get(&self, key: &str) -> Option<&Carried<'a>> {
        let at = if self.array {
            key.parse::<usize>().ok()?
        } else {
            self.fields.binary_search_by(|f| f.as_str().cmp(key)).ok()?
        };
        self.values.get(at)
    }
}
/// A set of gaps, shared: joining two sets never copies either.
type Gaps = Option<Rc<GapNode>>;
enum GapNode {
    /// `read` found the value missing for `reason`.
    Leaf {
        reason: Rc<str>,
        read: Rc<Read>,
    },
    All(Vec<Rc<GapNode>>),
    /// The gaps of the value a lexical name holds, used through that name.
    Via(Rc<str>, Rc<GapNode>),
}
/// A reference, as written, that read a value some way lacks.
struct Read {
    at: String,
    written: String,
    note: Option<String>,
    /// Read as the identity of a `difference` item: `exists` cannot test it.
    identity: bool,
}
fn join_gaps(parts: impl IntoIterator<Item = Gaps>) -> Gaps {
    let mut nodes: Vec<Rc<GapNode>> = parts.into_iter().flatten().collect();
    if nodes.len() > 1 {
        Some(Rc::new(GapNode::All(nodes)))
    } else {
        nodes.pop()
    }
}

/// Identity of a form, for keeping each once.
#[derive(PartialEq, Eq, Hash)]
enum FormKey {
    Shape(*const Schema, Rc<str>),
    Opaque(u64),
    Built(*const ()),
    Items(*const ()),
}
impl Form<'_> {
    fn key(&self) -> FormKey {
        match self {
            Form::Shape { schema, way, .. } => {
                FormKey::Shape(std::ptr::from_ref(*schema), way.clone())
            }
            Form::Opaque(nodes) => FormKey::Opaque(*nodes),
            Form::Built(built) => FormKey::Built(Rc::as_ptr(built).cast()),
            Form::Items(items) => FormKey::Items(Rc::as_ptr(items).cast()),
        }
    }
}
/// Identity of a built value: equal ones are built once and shared, so that
/// deriving the same value again gives the very same form.
#[derive(PartialEq, Eq, Hash)]
enum BuiltKey {
    Parts(bool, Vec<(String, PartKey)>),
    Items(PartKey),
}
type PartKey = (Vec<Rc<str>>, Vec<FormKey>);
/// `None` where the part has gaps of its own: such a value is not shared.
fn part_key(part: &Carried<'_>) -> Option<PartKey> {
    part.gaps.is_none().then(|| {
        (
            part.lacks.clone(),
            part.forms.iter().map(Form::key).collect(),
        )
    })
}
/// Collects what a value carries, each form and lack once.
#[derive(Default)]
struct Build<'a> {
    lacks: Vec<Rc<str>>,
    lacking: HashSet<Rc<str>>,
    gaps: Vec<Gaps>,
    forms: Vec<Form<'a>>,
    seen: HashSet<FormKey>,
}
impl<'a> Build<'a> {
    fn form(&mut self, form: Form<'a>) {
        if self.seen.insert(form.key()) {
            self.forms.push(form);
        }
    }
    fn lack(&mut self, text: &str) {
        if !self.lacking.contains(text) {
            let text: Rc<str> = text.into();
            self.lacking.insert(text.clone());
            self.lacks.push(text);
        }
    }
    /// Everything `other` carries.
    fn all(&mut self, other: &Carried<'a>) {
        for lack in &other.lacks {
            self.lack(lack);
        }
        self.gaps.push(other.gaps.clone());
        for form in other.forms.iter() {
            self.form(form.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// How a value enters scope.
// ---------------------------------------------------------------------------

/// What a step leaves in its binding (or a `for_each` in its item) on one
/// exit. Every kind of step binding is one of these.
enum Held<'a> {
    /// A value of this schema: an invoke or call outcome, a received event.
    Shape(&'a Schema),
    /// Nothing: the name is not bound on this exit.
    Unbound,
    /// A value ordinary typing gives no shape, so no read of it types.
    Unfollowed,
    /// A named join's result: what each key holds.
    Members(BTreeMap<&'a str, Vec<Member<'a>>>),
    /// An item of this collection, evaluated where the step runs.
    ItemOf(&'a Expr),
}
/// What one key of a named join's result holds on one edge.
enum Member<'a> {
    /// The member answered with this outcome and output.
    Answer(&'a str, &'a Schema),
    /// The member's answer, any outcome, perhaps without outcome and output.
    Loose(&'a Schema),
    /// The key is not in the result, for this reason.
    Absent(&'static str),
}
/// One way out of a step: where it leads, and what it leaves bound.
struct Exit<'a> {
    to: &'a str,
    /// Names the way, for a diagnostic.
    way: String,
    holds: Option<(&'a str, Held<'a>)>,
}
/// Everything about one step this check uses: the expressions it evaluates
/// and its exits.
struct Parts<'a> {
    exprs: Vec<(String, &'a Expr)>,
    exits: Vec<Exit<'a>>,
    /// A `for_each`'s own item: not bound while the step's own expression is
    /// evaluated, which is when the loop is entered.
    own_item: Option<&'a str>,
}
/// A reserved root.
#[derive(Clone, Copy)]
enum Reserved {
    Input,
    Event,
    Definitions,
    Vars,
}
impl Reserved {
    /// In the order of `RESERVED_ROOTS`.
    const ALL: [Reserved; 4] = [
        Reserved::Input,
        Reserved::Event,
        Reserved::Definitions,
        Reserved::Vars,
    ];
    fn name(self) -> &'static str {
        match self {
            Reserved::Input => "input",
            Reserved::Event => "event",
            Reserved::Definitions => "definitions",
            Reserved::Vars => "vars",
        }
    }
    fn named(root: &str) -> Option<Reserved> {
        Self::ALL.into_iter().find(|r| r.name() == root)
    }
}
/// Where an expression is evaluated.
#[derive(Clone, Copy)]
enum Place<'a, 'c> {
    /// In a step of `entry`, with these bindings.
    Step {
        entry_name: &'a str,
        entry: &'a EntryPoint,
        context: &'c Context<'a>,
    },
    /// In a trigger's `input`, with this event payload.
    Trigger { payload: Option<&'a Schema> },
}
/// Every binding and `for_each` item of the playbook at one step.
type Context<'a> = BTreeMap<&'a str, Rc<Carried<'a>>>;

/// Schemas this check reads that the artifact does not hold as such.
struct Sources {
    definitions: Option<Schema>,
    child: Schema,
    /// Per named join step: each key's answer of any outcome.
    loose: BTreeMap<String, BTreeMap<String, Schema>>,
    /// Per named join step: each member's outcomes.
    outcomes: BTreeMap<String, Vec<BTreeMap<String, Schema>>>,
}
impl Sources {
    fn new(content: &ArtifactContent) -> Self {
        let definitions = literal_schema(&Value::Object(
            content
                .definitions
                .iter()
                .map(|(k, d)| (k.clone(), d.value.clone()))
                .collect(),
        ))
        .ok();
        let mut loose = BTreeMap::new();
        let mut outcomes = BTreeMap::new();
        for (name, step) in &content.playbook.steps {
            let Operation::Join {
                members,
                member_keys,
                ..
            } = &step.op
            else {
                continue;
            };
            if member_keys.is_empty() || member_keys.len() != members.len() {
                continue;
            }
            let Ok(Some(Schema::Object { properties, .. })) = content.join_schema(step, false)
            else {
                continue;
            };
            let Ok(each) = members
                .iter()
                .map(|m| content.member_outcomes(m))
                .collect::<Result<Vec<_>, _>>()
            else {
                continue;
            };
            loose.insert(name.clone(), properties);
            outcomes.insert(name.clone(), each);
        }
        Self {
            definitions,
            child: Schema::object(BTreeMap::from([("child".into(), Schema::string(128))])),
            loose,
            outcomes,
        }
    }
}

// ---------------------------------------------------------------------------
// The derivation.
// ---------------------------------------------------------------------------

/// A lexical name: a `let` name or a `map` or `filter` item.
#[derive(Clone)]
struct Named<'a> {
    carried: Carried<'a>,
    /// The reference it stands for, as a direct read writes it.
    origin: Option<Rc<str>>,
    /// What it is, for a diagnostic: "is `work`", "is an item of …".
    note: Rc<str>,
}
type Scope<'a> = BTreeMap<&'a str, Named<'a>>;
/// What `derive` says of one expression.
struct Out<'a> {
    carried: Carried<'a>,
    /// Its size with every name replaced by what it stands for. Saturating.
    size: u64,
    /// Set for a reference: what it reads, as a direct read writes it.
    origin: Option<Rc<str>>,
}
/// A read some way cannot satisfy, consumed where its value is needed.
struct Refusal {
    read: Rc<Read>,
    reasons: Vec<Rc<str>>,
    /// Reasons beyond the ones listed.
    more: usize,
    /// Consumed as a lookup key.
    key: bool,
    /// The lexical names its value was used through.
    through: Vec<Rc<str>>,
}
/// What one name contributes to an expression's expanded size.
#[derive(Clone, Default)]
struct Charge {
    each: u64,
    times: u64,
    total: u64,
}
/// The size of one expression of the draft.
struct ExprSize {
    path: String,
    at: String,
    size: u64,
    /// The name that contributes most.
    most: Option<(String, Charge)>,
}

/// What stays the same over the whole draft.
struct Check<'a> {
    content: &'a ArtifactContent,
    sources: &'a Sources,
    /// Nodes of each schema, by address.
    schema_nodes: RefCell<HashMap<*const Schema, u64>>,
    /// Every built value, by what it is made of.
    built: RefCell<HashMap<BuiltKey, Form<'a>>>,
    /// What each literal is, by where it is written.
    literals: RefCell<HashMap<*const Value, Carried<'a>>>,
    /// A present scalar.
    scalar: Carried<'a>,
    /// Shapes followed so far, of values with many.
    work: Cell<u64>,
    /// What each `for_each` item is, for a diagnostic.
    item_notes: BTreeMap<&'a str, Rc<str>>,
}
struct ReadUse {
    key: bool,
    through: Option<Rc<str>>,
}
/// Evaluates expressions, collecting the reads it refuses.
struct Pass<'a, 'k> {
    check: &'k Check<'a>,
    refusals: Vec<Refusal>,
    by_read: HashMap<*const Read, usize>,
    /// Gap sets already consumed, kept alive so an address is never reused.
    sunk: HashSet<(*const GapNode, bool)>,
    kept: Vec<Rc<GapNode>>,
    charges: BTreeMap<String, Charge>,
}

fn value_nodes(value: &Value) -> u64 {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => 1,
        Value::Array(items) => items
            .iter()
            .fold(1u64, |n, v| n.saturating_add(value_nodes(v))),
        Value::Object(map) => map
            .values()
            .fold(1u64, |n, v| n.saturating_add(value_nodes(v))),
    }
}
/// "<way> has no `key` in `at` (it has a, b)", or "(it has no fields)".
fn no_field(way: &str, key: &str, at: &str, properties: &BTreeMap<String, Schema>) -> String {
    let has = if properties.is_empty() {
        "no fields".to_string()
    } else {
        properties.keys().cloned().collect::<Vec<_>>().join(", ")
    };
    format!("{way} has no `{key}` in `{at}` (it has {has})")
}

impl<'a> Check<'a> {
    fn nodes(&self, schema: &Schema) -> u64 {
        if let Some(n) = self.schema_nodes.borrow().get(&std::ptr::from_ref(schema)) {
            return *n;
        }
        let sum = |parts: &mut dyn Iterator<Item = &Schema>| {
            parts.fold(1u64, |n, s| n.saturating_add(self.nodes(s)))
        };
        let n = match schema {
            Schema::Null | Schema::Boolean | Schema::Integer { .. } | Schema::String { .. } => 1,
            Schema::Array {
                items,
                max_items: _,
            } => sum(&mut [&**items].into_iter()),
            Schema::Object {
                properties,
                required: _,
            } => sum(&mut properties.values()),
            Schema::Nullable { inner } => sum(&mut [&**inner].into_iter()),
            Schema::Union { variants } => sum(&mut variants.iter()),
        };
        self.schema_nodes
            .borrow_mut()
            .insert(std::ptr::from_ref(schema), n);
        n
    }
    fn finish(&self, build: Build<'a>) -> Carried<'a> {
        let mut nodes = 0u64;
        let mut depth = 0;
        for form in &build.forms {
            let (n, d) = match form {
                Form::Shape { schema, .. } => (self.nodes(schema), 0),
                Form::Opaque(n) => (*n, 0),
                Form::Built(built) => (built.nodes, built.depth),
                Form::Items(items) => (items.nodes.saturating_add(1), items.depth + 1),
            };
            nodes = nodes.saturating_add(n);
            depth = depth.max(d);
        }
        Carried {
            lacks: build.lacks,
            gaps: join_gaps(build.gaps),
            forms: build.forms.into(),
            nodes,
            depth,
        }
    }
    fn of(&self, form: Form<'a>) -> Carried<'a> {
        let mut build = Build::default();
        build.form(form);
        self.finish(build)
    }
    fn opaque(&self, nodes: u64) -> Carried<'a> {
        self.of(Form::Opaque(nodes.max(1)))
    }
    fn shape(&self, schema: &'a Schema, way: &str, name: &str) -> Carried<'a> {
        self.of(Form::Shape {
            schema,
            way: way.into(),
            name: name.into(),
        })
    }
    fn lacking(&self, text: &str) -> Carried<'a> {
        let mut build = Build::default();
        build.lack(text);
        self.finish(build)
    }
    /// An object of `parts` by field, or an array of them in their order.
    fn built(&self, parts: Vec<(String, Carried<'a>)>, array: bool) -> Carried<'a> {
        let key = parts
            .iter()
            .map(|(k, part)| Some((k.clone(), part_key(part)?)))
            .collect::<Option<Vec<_>>>()
            .map(|parts| BuiltKey::Parts(array, parts));
        match key {
            Some(key) => self.shared(Some(key), || self.assembled_form(parts, array)),
            None => self.assembled(parts, array),
        }
    }
    /// An object (or array) of `parts`, not looked up among those built
    /// before (a literal is found again by where it is written).
    fn assembled(&self, parts: Vec<(String, Carried<'a>)>, array: bool) -> Carried<'a> {
        self.of(self.assembled_form(parts, array))
    }
    fn assembled_form(&self, mut parts: Vec<(String, Carried<'a>)>, array: bool) -> Form<'a> {
        if !array {
            parts.sort_by(|a, b| a.0.cmp(&b.0));
        }
        let nodes = parts
            .iter()
            .fold(1u64, |n, (_, c)| n.saturating_add(c.nodes.max(1)));
        let depth = parts.iter().map(|(_, c)| c.depth).max().unwrap_or(0) + 1;
        let (fields, values): (Vec<String>, Vec<Carried>) = parts.into_iter().unzip();
        Form::Built(Rc::new(Built {
            fields: if array { vec![] } else { fields },
            values,
            array,
            nodes,
            depth,
        }))
    }
    /// An array whose every item is `item`.
    fn items(&self, item: Carried<'a>) -> Carried<'a> {
        let key = part_key(&item).map(BuiltKey::Items);
        self.shared(key, || Form::Items(Rc::new(item)))
    }
    /// The form `make` builds, or the equal one built before.
    fn shared(&self, key: Option<BuiltKey>, make: impl FnOnce() -> Form<'a>) -> Carried<'a> {
        let Some(key) = key else {
            return self.of(make());
        };
        if let Some(form) = self.built.borrow().get(&key) {
            return self.of(form.clone());
        }
        let form = make();
        self.built.borrow_mut().insert(key, form.clone());
        self.of(form)
    }
    /// What a literal value is: an object or array as it is written, a
    /// scalar as a present value. The same literal gives the same forms
    /// every time it is derived.
    fn literal(&self, value: &'a Value) -> Carried<'a> {
        if let Some(known) = self.literals.borrow().get(&std::ptr::from_ref(value)) {
            return known.clone();
        }
        let carried = self.literal_value(value);
        self.literals
            .borrow_mut()
            .insert(std::ptr::from_ref(value), carried.clone());
        carried
    }
    fn literal_value(&self, value: &Value) -> Carried<'a> {
        match value {
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                self.scalar.clone()
            }
            Value::Array(items) => self.assembled(
                items
                    .iter()
                    .map(|v| (String::new(), self.literal_value(v)))
                    .collect(),
                true,
            ),
            Value::Object(map) => self.assembled(
                map.iter()
                    .map(|(k, v)| (k.clone(), self.literal_value(v)))
                    .collect(),
                false,
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// The flow over the steps, and the whole draft.
// ---------------------------------------------------------------------------

/// Per step inside a `for_each` body, where a failure there leads: the target,
/// the items that are unbound there, and the name of the way.
type LoopFailures<'a> = BTreeMap<&'a str, Vec<(&'a str, Vec<&'a str>, String)>>;
/// What the derivation says of the whole draft.
#[derive(Default)]
struct Derived {
    /// Refused reads: (step or trigger path, location, message).
    reads: Vec<(String, String, String)>,
    sizes: Vec<ExprSize>,
    /// Per entry: the steps it reaches and the type nodes in scope at each.
    scope: Vec<(String, u64, u64)>,
    /// The expanded size of every expression, once for each entry that
    /// reaches its step: ordinary typing types it once for each.
    total: u64,
    /// Faults that stop the check itself.
    limits: Vec<ValidationIssue>,
}

impl<'a> Check<'a> {
    fn new(content: &'a ArtifactContent, sources: &'a Sources) -> Self {
        let item_notes = content
            .playbook
            .steps
            .iter()
            .filter_map(|(name, step)| match &step.op {
                Operation::ForEach { item, .. } => Some((
                    item.as_str(),
                    format!("is an item of the collection of for_each `{name}`").into(),
                )),
                Operation::Invoke { .. }
                | Operation::Decide { .. }
                | Operation::Wait { .. }
                | Operation::Call { .. }
                | Operation::Join { .. }
                | Operation::Return { .. } => None,
            })
            .collect();
        Self {
            content,
            sources,
            schema_nodes: RefCell::new(HashMap::new()),
            built: RefCell::new(HashMap::new()),
            literals: RefCell::new(HashMap::new()),
            work: Cell::new(0),
            scalar: Carried {
                lacks: vec![],
                gaps: None,
                forms: vec![Form::Opaque(1)].into(),
                nodes: 1,
                depth: 0,
            },
            item_notes,
        }
    }
    /// A `for_each`'s own item where the loop is entered: its collection is
    /// evaluated then, before any item is bound.
    fn entering(&self, site: &str, item: &str) -> Carried<'a> {
        self.lacking(&format!(
            "where for_each `{site}` is entered, `{item}` is not bound yet"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reserved_root_is_decided() {
        let named: Vec<_> = Reserved::ALL.iter().map(|r| r.name()).collect();
        assert_eq!(named, RESERVED_ROOTS);
        for root in RESERVED_ROOTS {
            assert!(Reserved::named(root).is_some(), "{root}");
        }
    }
}
