//! A generated soundness test of the draft read check.
//!
//! Base reads that must be refused (a value some way of reaching the step
//! lacks) and base reads that must be accepted are wrapped in every
//! composition, up to three deep, of combinators that cover every `Expr`
//! variant and every construct that puts a value in scope. An oracle that
//! knows only what each combinator does to a Missing value (carries it,
//! needs it, or tests it with `exists`) says whether the draft must be
//! refused. Every wrapped unsound read must be refused with the reasons of
//! the direct read; no wrapped sound read may be refused.
use aq_playbooks_model::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn lit(v: Value) -> Expr {
    Expr::literal(v)
}
fn r(root: &str, path: &[&str]) -> Expr {
    Expr::Ref {
        root: root.into(),
        path: path.iter().map(|s| s.to_string()).collect(),
    }
}
fn b(e: Expr) -> Box<Expr> {
    Box::new(e)
}
fn edges(xs: &[(&str, &str)]) -> BTreeMap<String, String> {
    xs.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
fn strings(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}
fn let_(name: &str, value: Expr, body: Expr) -> Expr {
    Expr::Let {
        bindings: vec![LetBinding {
            name: name.into(),
            value,
        }],
        body: b(body),
    }
}
fn eq(left: Expr, right: Expr) -> Expr {
    Expr::Eq {
        left: b(left),
        right: b(right),
    }
}
fn array(items: Vec<Expr>) -> Expr {
    Expr::Array { items }
}
fn object(key: &str, value: Expr) -> Expr {
    Expr::Object {
        fields: BTreeMap::from([(key.to_string(), value)]),
    }
}
fn lookup(key: Expr, cases: Vec<(Value, Expr)>, default: Expr) -> Expr {
    Expr::Lookup {
        key: b(key),
        cases: cases
            .into_iter()
            .map(|(when, then)| LookupCase { when, then })
            .collect(),
        default: b(default),
    }
}

fn fields(names: &[&str]) -> Schema {
    Schema::object(
        names
            .iter()
            .map(|name| (name.to_string(), Schema::string(64)))
            .collect(),
    )
}
fn command(name: &str, outcomes: &[(&str, Schema)]) -> CommandContract {
    CommandContract {
        name: name.into(),
        version: "1.0.0".into(),
        input: Schema::object(BTreeMap::new()),
        outcomes: outcomes
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect(),
        capabilities: strings(&[name]),
        scopes: strings(&["project"]),
        effect: Effect::Observation,
        recovery: Recovery::Observation,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 4096,
        deadline_ms: 1000,
    }
}
fn event(name: &str, payload: Schema, correlation: &[&str]) -> EventContract {
    EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: name.into(),
        version: "1.0.0".into(),
        payload,
        trusted_producers: strings(&["host"]),
        scopes: strings(&["project"]),
        correlation_fields: strings(correlation),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 4096,
        retention_ms: 86_400_000,
    }
}
/// `ask.judge`, asynchronous and joined by its handle, whose outcomes differ
/// in shape as the host's judgment does (`assessed` alone has `choice`);
/// `work.look`, synchronous, whose outcomes differ too (`found` alone has
/// `kind`, and its `hint` is optional); and an event to wait for.
fn registry() -> Registry {
    let assessed = fields(&["choice", "reference"]);
    let declined = fields(&["reason", "reference"]);
    let completion = Schema::object(BTreeMap::from([
        ("operation".to_string(), Schema::string(64)),
        (
            "outcome".to_string(),
            Schema::String {
                max_length: 16,
                values: strings(&["assessed", "failed", "refused"]),
            },
        ),
        (
            "output".to_string(),
            Schema::Union {
                variants: vec![assessed.clone(), declined.clone()],
            },
        ),
    ]));
    let mut judge = command(
        "ask.judge",
        &[
            ("accepted", fields(&["handle"])),
            ("assessed", assessed),
            ("failed", declined.clone()),
            ("refused", declined),
        ],
    );
    judge.asynchronous = Some(AsyncContract {
        completion_event: "ask.judged".into(),
        completion: completion.clone(),
        correlation_field: "operation".into(),
        reconciliation_command: "ask.get".into(),
    });
    let found = Schema::Object {
        properties: BTreeMap::from([
            ("id".into(), Schema::string(64)),
            ("kind".into(), Schema::string(32)),
            ("hint".into(), Schema::string(32)),
        ]),
        required: strings(&["id", "kind"]),
    };
    let look = command("work.look", &[("found", found), ("none", fields(&["id"]))]);
    let get = command("ask.get", &[("known", fields(&["status"]))]);
    // A command whose input a generated read can be written into.
    let mut flag = command("work.flag", &[("done", fields(&[]))]);
    flag.input = Schema::object(BTreeMap::from([("flag".into(), Schema::Boolean)]));
    Registry {
        commands: [judge, look, get, flag]
            .into_iter()
            .map(|c| (c.name.clone(), c))
            .collect(),
        events: [
            event("ask.judged", completion, &["operation"]),
            event("aq.work.seen", fields(&["note"]), &[]),
        ]
        .into_iter()
        .map(|e| (e.name.clone(), e))
        .collect(),
    }
}
fn decide(predicate: Expr, next: &str) -> Step {
    Step {
        op: Operation::Decide { predicate },
        edges: edges(&[("true", next), ("false", next), ("unknown", next)]),
        on_error: None,
    }
}

/// The step every generated chain starts at.
const SLOT: &str = "slot";

/// A read: the expression that carries the value, and the path read in it.
struct Base {
    name: &'static str,
    carrier: Expr,
    path: &'static [&'static str],
    /// The read is there on every way to `SLOT`.
    sound: bool,
    /// The carrier itself is Missing on some way (rule R-B, or a read given
    /// whole that is Missing); otherwise the carrier is there and the path
    /// leads through a field some way lacks.
    carrier_missing: bool,
    /// The value is needed inside the carrier itself (as a lookup key), so
    /// nothing around it, not even `exists`, makes the draft sound.
    needed_inside: bool,
    /// Ordinary typing gives a `for_each` item the type its collection has
    /// anywhere in the playbook, not at the loop, and for this carrier that
    /// type cannot be read: typing itself refuses the read through an item.
    loose_item: bool,
    /// The carrier may be Missing and, where it is there, may lack the
    /// field: a wrapped read is refused for some of the direct read's
    /// reasons, not always all (`exists` can absorb one kind).
    two_causes: bool,
    /// How deep the combinators are composed around it.
    depth: usize,
    /// Where the read can be consumed.
    sites: &'static [Site],
    content: ArtifactContent,
}
/// Where the value of the generated expression is needed.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Site {
    /// A `decide` predicate.
    Decide,
    /// A command's input.
    Input,
    /// A `return` output.
    Output,
    /// A trigger's `input`, where only `event` is in scope.
    Trigger,
}
const IN_STEPS: &[Site] = &[Site::Decide, Site::Input, Site::Output];
const ANYWHERE: &[Site] = &[Site::Decide, Site::Input, Site::Output, Site::Trigger];

fn playbook(registry: &Registry, start: &str, steps: Vec<(&str, Step)>) -> ArtifactContent {
    let mut steps: BTreeMap<String, Step> =
        steps.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    steps.insert(
        "end".into(),
        Step {
            op: Operation::Return {
                outcome: "done".into(),
                output: lit(Value::Null),
            },
            edges: BTreeMap::new(),
            on_error: None,
        },
    );
    ArtifactContent {
        format_version: FORMAT_VERSION,
        compiler: "test".into(),
        source: SourceBinding {
            digest: hash_bytes(b"source"),
            header: SourceHeader {
                format_version: FORMAT_VERSION,
                id: "generated".into(),
                scope: "project".into(),
                interfaces: strings(&["main"]),
                definitions: strings(&["levels"]),
            },
        },
        playbook: Playbook {
            id: "generated".into(),
            purpose: "a generated read".into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Object {
                        properties: BTreeMap::from([
                            ("task".into(), Schema::string(64)),
                            ("flag".into(), Schema::Boolean),
                        ]),
                        required: strings(&["task"]),
                    },
                    outcomes: BTreeMap::from([
                        ("done".into(), Schema::Null),
                        ("told".into(), Schema::Boolean),
                    ]),
                    start: start.into(),
                },
            )]),
            steps,
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: strings(&["ask.judge", "work.look", "work.flag"]),
        },
        registry: registry.clone(),
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::from([(
            "levels".to_string(),
            CapturedDefinition {
                version: "1".into(),
                digest: digest(&json!(["low", "high"])).unwrap(),
                value: json!(["low", "high"]),
            },
        )]),
        source_map: BTreeMap::new(),
    }
}
#[path = "draft_soundness/bases.rs"]
mod bases;
use bases::bases;

#[path = "draft_soundness/wrappers.rs"]
mod wrappers;
use wrappers::{Built, Layer, Missing, WRAPS, Wrap};

#[path = "draft_soundness/oracle.rs"]
mod oracle;
use oracle::{ACCEPTED, OTHER_REASONS, SOUND_REFUSED, TYPING_REFUSES, Tally, check_base};

type Task<'a> = (&'a Base, Option<Wrap>, Site, usize);

fn tasks(bases: &[Base]) -> Vec<Task<'_>> {
    // One task per base, site and first combinator, shared out over the
    // cores. A `decide` consumes the read at the base's full depth; the
    // other sites at depth two at most.
    let mut tasks: Vec<Task> = vec![];
    for base in bases {
        for site in base.sites {
            let depth = match site {
                Site::Decide => base.depth,
                Site::Input | Site::Output | Site::Trigger => base.depth.min(2),
            };
            tasks.push((base, None, *site, depth));
            tasks.extend(WRAPS.into_iter().map(|w| (base, Some(w), *site, depth)));
        }
    }
    tasks
}

fn run(tasks: &[Task<'_>]) -> Tally {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    let results: Vec<Tally> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut tally = Tally::default();
                    while let Some((base, first, site, depth)) =
                        tasks.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
                    {
                        tally.add(check_base(base, *depth, *first, *site));
                    }
                    tally
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut tally = Tally::default();
    for result in results {
        tally.add(result);
    }
    tally
}

#[test]
fn every_wrapped_read_is_refused_or_accepted_as_the_direct_read_is() {
    let bases = bases();
    let tally = run(&tasks(&bases));
    println!(
        "soundness: {} bases, {} combinators, 4 sites, {} compositions ({} must be refused, {} \
         must be accepted; {} also checked against ordinary typing, which refused {} itself), {} \
         failures",
        bases.len(),
        WRAPS.len(),
        tally.cases,
        tally.must_refuse,
        tally.must_accept,
        tally.typed,
        tally.typing_stricter,
        tally.failures.len()
    );
    let count = |what: &str| tally.failures.iter().filter(|f| f.contains(what)).count();
    let shown: Vec<_> = tally.failures.iter().take(12).collect();
    assert!(
        tally.failures.is_empty(),
        "{} of {} generated compositions fail ({} unsound reads accepted, {} refused for other \
         reasons than the direct read, {} sound reads refused, {} that ordinary typing refuses); \
         the first: {shown:#?}",
        tally.failures.len(),
        tally.cases,
        count(ACCEPTED),
        count(OTHER_REASONS),
        count(SOUND_REFUSED),
        count(TYPING_REFUSES),
    );
}

/// The combinators cover every `Expr` variant: adding a variant fails to
/// compile here until a combinator uses it.
#[test]
fn the_combinators_cover_every_expression() {
    fn seen(e: &Expr, used: &mut BTreeSet<&'static str>) {
        let mut all = |es: &mut dyn Iterator<Item = &Expr>| {
            for e in es {
                seen(e, used);
            }
        };
        match e {
            Expr::Literal { .. } => {}
            Expr::Ref { .. } => {}
            Expr::Object { fields } => all(&mut fields.values()),
            Expr::Array { items } => all(&mut items.iter()),
            Expr::Exists { value } | Expr::Not { value } => all(&mut [&**value].into_iter()),
            Expr::And { values } | Expr::Or { values } => all(&mut values.iter()),
            Expr::Eq { left, right }
            | Expr::Lt { left, right }
            | Expr::Difference { left, right, .. } => all(&mut [&**left, &**right].into_iter()),
            Expr::Contains { collection, value } => all(&mut [&**collection, &**value].into_iter()),
            Expr::Map {
                collection, value, ..
            } => all(&mut [&**collection, &**value].into_iter()),
            Expr::Filter {
                collection,
                predicate,
                ..
            } => all(&mut [&**collection, &**predicate].into_iter()),
            Expr::Lookup {
                key,
                cases,
                default,
            } => all(&mut [&**key, &**default]
                .into_iter()
                .chain(cases.iter().map(|c| &c.then))),
            Expr::Let { bindings, body } => {
                all(&mut bindings.iter().map(|b| &b.value).chain([&**body]))
            }
        }
        used.insert(match e {
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
        });
    }
    let mut used = BTreeSet::new();
    for (i, wrap) in WRAPS.into_iter().enumerate() {
        let mut built = Built::new(r("input", &["task"]), true, Site::Decide);
        assert!(built.wrap(wrap, i), "{wrap:?}");
        for layer in &built.layers {
            if let Layer::ForEach { collection, .. } = layer {
                seen(collection, &mut used);
            }
        }
        seen(&built.expr, &mut used);
    }
    assert_eq!(used.len(), 16, "{used:?}");
}
