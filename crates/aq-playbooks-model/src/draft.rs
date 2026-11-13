//! Checks a new draft is held to that `Artifact::verify` does not run, so an
//! artifact stored before them keeps loading:
//! - a join member that is an operation handle is named by its binding alone;
//! - a read through a field that some way of reaching the step lacks (an
//!   outcome without it, a join member that has not answered, a wait that
//!   timed out) is refused, except inside `exists`;
//! - so is a read of a binding on some way to the step that never bound it:
//!   around the step that binds it, or through that step's `on_error`.
//!
//! Ordinary typing merges the outcomes a binding can hold into one object, in
//! which a field only one outcome has is merely optional, and an optional
//! field types as present. Here each binding keeps its outcomes apart.
use super::*;

/// One shape a binding can have on one way of reaching a step.
#[derive(Clone, PartialEq)]
enum Variant {
    /// Reads follow this schema; a field optional in it is readable.
    Shape(Schema),
    /// Not there at all on this way.
    Absent,
    /// Bound, in a shape this check does not follow.
    Bound,
    /// Missing, for the reason given (an alias of a field some way lacks).
    Lacks(String),
    /// A named join's result: each key's own variants.
    Keys(BTreeMap<String, Variants>),
}
/// Variants with the way each one arises, for diagnostics.
type Variants = Vec<(Variant, String)>;
type Context = BTreeMap<String, Variants>;

fn add_all(into: &mut Variants, from: &Variants) -> bool {
    let mut changed = false;
    for v in from {
        if !into.contains(v) {
            into.push(v.clone());
            changed = true;
        }
    }
    changed
}
fn merge(into: &mut Context, from: &Context) -> bool {
    let mut changed = false;
    for (root, variants) in from {
        changed |= add_all(into.entry(root.clone()).or_default(), variants);
    }
    changed
}

impl ArtifactContent {
    /// Issues only a new draft is held to. `reported` are the issues
    /// `validate_structure` already found; a read it already refused is not
    /// reported again.
    pub fn draft_issues(&self, reported: &[ValidationIssue]) -> Vec<ValidationIssue> {
        let mut issues = vec![];
        for (name, step) in &self.playbook.steps {
            let Operation::Join {
                members,
                member_keys,
                ..
            } = &step.op
            else {
                continue;
            };
            for (i, member) in members.iter().enumerate() {
                if let Some(error) = self.handle_member_path(member) {
                    let key = member_keys
                        .get(i)
                        .map(|k| format!(" (key {k})"))
                        .unwrap_or_default();
                    issue(
                        &mut issues,
                        "join_member_type",
                        &format!("steps.{name}"),
                        format!("members[{i}]{key}: {error}"),
                    );
                }
            }
        }
        for (entry_name, entry) in &self.playbook.entries {
            for (name, context) in self.variant_contexts(entry_name, entry) {
                let Some(step) = self.playbook.steps.get(&name) else {
                    continue;
                };
                let path = format!("steps.{name}");
                for (at, expr) in step_exprs(&step.op) {
                    let mut reads = vec![];
                    strict_reads(expr, &at, false, false, &context, &Scope::new(), &mut reads);
                    for (place, message) in reads {
                        let refused = reported.iter().chain(&issues).any(|i| {
                            i.path == path && i.message.starts_with(&format!("{path}.{place}:"))
                        });
                        if !refused {
                            issue(
                                &mut issues,
                                "expression_type",
                                &path,
                                format!("{path}.{place}: {message}"),
                            );
                        }
                    }
                }
            }
        }
        issues
    }
    /// The variants of every binding at every step reachable from `entry`,
    /// along the same edges as `outcome_contexts`. Every step binding is
    /// present in every context: `Absent` on a way that never bound it.
    fn variant_contexts(&self, entry_name: &str, entry: &EntryPoint) -> BTreeMap<String, Context> {
        let start: Context = self
            .playbook
            .steps
            .iter()
            .filter_map(|(name, step)| {
                let way = format!(
                    "the way from the start of entry `{entry_name}` that does not pass step `{name}`"
                );
                Some((binding_of(step)?.to_string(), vec![(Variant::Absent, way)]))
            })
            .collect();
        let mut contexts = BTreeMap::from([(entry.start.clone(), start)]);
        let mut pending = std::collections::VecDeque::from([entry.start.clone()]);
        let mut queued = BTreeSet::from([entry.start.clone()]);
        while let Some(name) = pending.pop_front() {
            queued.remove(&name);
            let Some(step) = self.playbook.steps.get(&name) else {
                continue;
            };
            let current = contexts[&name].clone();
            let mut targets = vec![];
            for (edge, target) in &step.edges {
                let mut next = current.clone();
                if let Some((bind, variants)) = self.bound_on(&name, step, edge) {
                    next.insert(bind, variants);
                } else if let Some(bind) = binding_of(step) {
                    let way = format!("edge `{edge}` of step `{name}`");
                    next.insert(bind.to_string(), vec![(Variant::Bound, way)]);
                }
                targets.push((target.clone(), next));
            }
            if let Some(target) = &step.on_error {
                let mut next = current.clone();
                if let Some(bind) = binding_of(step) {
                    let way = format!("the `on_error` exit of step `{name}`");
                    next.insert(bind.to_string(), vec![(Variant::Absent, way)]);
                }
                targets.push((target.clone(), next));
            }
            if let Operation::ForEach { body, next, .. } = &step.op {
                targets.push((body.clone(), current.clone()));
                targets.push((next.clone(), current.clone()));
            }
            for (target, next) in targets {
                let changed = match contexts.get_mut(&target) {
                    Some(existing) => merge(existing, &next),
                    None => {
                        contexts.insert(target.clone(), next);
                        true
                    }
                };
                if changed && queued.insert(target.clone()) {
                    pending.push_back(target);
                }
            }
        }
        contexts
    }
    /// What the step at `name` binds when it leaves by `edge`.
    fn bound_on(&self, name: &str, step: &Step, edge: &str) -> Option<(String, Variants)> {
        let outcome = |schema: &Schema| {
            vec![(
                Variant::Shape(schema.clone()),
                format!("outcome `{edge}` of step `{name}`"),
            )]
        };
        match &step.op {
            Operation::Invoke { command, bind, .. } => {
                let schema = self
                    .registry
                    .commands
                    .get(&command.name)?
                    .outcomes
                    .get(edge)?;
                Some((bind.clone(), outcome(schema)))
            }
            Operation::Call {
                dependency,
                entry,
                mode: CallMode::Wait,
                bind,
                ..
            } => {
                let schema = self
                    .dependencies
                    .get(dependency)?
                    .interfaces
                    .get(entry)?
                    .outcomes
                    .get(edge)?;
                Some((bind.clone(), outcome(schema)))
            }
            Operation::Wait {
                event: Some(event),
                bind,
                ..
            } => {
                let variant = match edge {
                    "received" => {
                        Variant::Shape(self.registry.events.get(&event.name)?.payload.clone())
                    }
                    _ => Variant::Absent,
                };
                Some((
                    bind.clone(),
                    vec![(variant, format!("edge `{edge}` of wait `{name}`"))],
                ))
            }
            Operation::Join {
                member_keys, bind, ..
            } if !member_keys.is_empty() => Some((
                bind.clone(),
                vec![(
                    Variant::Keys(self.join_keys(name, step, edge)?),
                    format!("edge `{edge}` of join `{name}`"),
                )],
            )),
            _ => None,
        }
    }
    /// Each key of a named join's result on `edge`: one variant per outcome
    /// the member can have there, and `Absent` where the key can be missing.
    fn join_keys(&self, name: &str, step: &Step, edge: &str) -> Option<BTreeMap<String, Variants>> {
        let Operation::Join {
            members,
            member_keys,
            mode,
            accepted_outcomes,
            ..
        } = &step.op
        else {
            return None;
        };
        let Ok(Some(Schema::Object {
            properties: loose, ..
        })) = self.join_schema(step, false)
        else {
            return None;
        };
        let on_edge = format!("edge `{edge}` of join `{name}`");
        let mut keys = BTreeMap::new();
        for (key, member) in member_keys.iter().zip(members) {
            let outcomes = self.member_outcomes(member).ok()?;
            let absent = |why: &str| (Variant::Absent, format!("{on_edge}, {why}"));
            let mut variants = vec![];
            match (edge, mode) {
                ("completed", JoinMode::All | JoinMode::AnyAccepted) => {
                    for (o, schema) in outcomes.iter().filter(|(o, _)| {
                        accepted_outcomes.is_empty() || accepted_outcomes.contains(*o)
                    }) {
                        let answer = Schema::object(BTreeMap::from([
                            ("participant".into(), Schema::string(128)),
                            (
                                "outcome".into(),
                                Schema::String {
                                    max_length: o.len().max(1),
                                    values: BTreeSet::from([o.clone()]),
                                },
                            ),
                            ("output".into(), schema.clone()),
                        ]));
                        variants.push((
                            Variant::Shape(answer),
                            format!("outcome `{o}` of member `{key}` on {on_edge}"),
                        ));
                    }
                    if *mode == JoinMode::AnyAccepted {
                        variants.push(absent("when another member qualified first"));
                    }
                }
                ("no_match", JoinMode::All) => {
                    variants.push((Variant::Shape(loose.get(key)?.clone()), on_edge.clone()));
                }
                ("completed", JoinMode::AnyCompletion) | ("timeout", _) => {
                    variants.push((Variant::Shape(loose.get(key)?.clone()), on_edge.clone()));
                    variants.push(absent(if edge == "timeout" {
                        "when that member has not answered"
                    } else {
                        "when another member completed first"
                    }));
                }
                _ => variants.push(absent("where no member is kept")),
            }
            keys.insert(key.clone(), variants);
        }
        Some(keys)
    }
}

/// The name a step binds, if it binds one.
fn binding_of(step: &Step) -> Option<&str> {
    match &step.op {
        Operation::Invoke { bind, .. }
        | Operation::Wait { bind, .. }
        | Operation::Call { bind, .. }
        | Operation::Join { bind, .. } => Some(bind),
        _ => None,
    }
}

/// A lexical name (`let` alias, `map` or `filter` item): the variants of what
/// it was derived from; the name reasons use for it (an alias reads as its
/// origin, so its reasons are worded as the direct read's); and a note for
/// the message.
#[derive(Clone)]
struct Lexical {
    variants: Variants,
    shown: Option<String>,
    note: Option<String>,
}
type Scope = BTreeMap<String, Lexical>;

/// A root as a reference names it: the binding, and the path inside it.
fn split_ref<'a>(root: &'a str, path: &'a [String]) -> (&'a str, &'a [String]) {
    match (root, path.split_first()) {
        ("vars", Some((binding, rest))) => (binding.as_str(), rest),
        _ => (root, path),
    }
}
fn push_unique(into: &mut Variants, items: Variants) {
    for v in items {
        if !into.contains(&v) {
            into.push(v);
        }
    }
}
/// What a reference to a tracked binding or alias holds on each way, with the
/// origin it reads, as written; `None` for an untracked root.
fn alias(value: &Expr, context: &Context, scope: &Scope) -> Option<(Variants, String)> {
    let Expr::Ref { root, path } = value else {
        return None;
    };
    let (binding, rest) = split_ref(root, path);
    let (variants, origin) = match scope.get(binding) {
        Some(name) => (
            &name.variants,
            name.shown.clone().unwrap_or_else(|| binding.to_string()),
        ),
        None => (context.get(binding)?, binding.to_string()),
    };
    let mut projected = vec![];
    for (variant, way) in variants {
        push_unique(&mut projected, project(variant, way, &origin, rest, 0));
    }
    Some((projected, dotted(&origin, rest)))
}
/// `variant` (arising `way`) followed along `path[depth..]`. Wherever the
/// value is missing it becomes `Lacks`, worded as a direct read says it.
fn project(variant: &Variant, way: &str, binding: &str, path: &[String], depth: usize) -> Variants {
    let lacks = |text: String| vec![(Variant::Lacks(text), way.to_string())];
    match variant {
        Variant::Absent => lacks(format!("on {way}, `{binding}` is not bound")),
        Variant::Lacks(_) | Variant::Bound => vec![(variant.clone(), way.to_string())],
        Variant::Keys(keys) => {
            let Some(key) = path.get(depth) else {
                return vec![(variant.clone(), way.to_string())];
            };
            let Some(alternatives) = keys.get(key) else {
                return vec![(Variant::Bound, way.to_string())];
            };
            let mut out = vec![];
            for (v, w) in alternatives {
                let items = if *v == Variant::Absent {
                    vec![(
                        Variant::Lacks(format!(
                            "on {w}, `{}` is absent",
                            dotted(binding, &path[..=depth])
                        )),
                        w.clone(),
                    )]
                } else {
                    project(v, w, binding, path, depth + 1)
                };
                push_unique(&mut out, items);
            }
            out
        }
        Variant::Shape(schema) => project_schema(schema, way, binding, path, depth),
    }
}
fn project_schema(
    schema: &Schema,
    way: &str,
    binding: &str,
    path: &[String],
    depth: usize,
) -> Variants {
    let Some(key) = path.get(depth) else {
        return vec![(Variant::Shape(schema.clone()), way.to_string())];
    };
    match schema {
        Schema::Union { variants } => {
            let mut out = vec![];
            for v in variants {
                push_unique(&mut out, project_schema(v, way, binding, path, depth));
            }
            out
        }
        Schema::Object { properties, .. } => match properties.get(key) {
            Some(next) => project_schema(next, way, binding, path, depth + 1),
            None => vec![(
                Variant::Lacks(format!(
                    "{way} has no `{key}` in `{}` (it has {})",
                    dotted(binding, &path[..depth]),
                    properties.keys().cloned().collect::<Vec<_>>().join(", ")
                )),
                way.to_string(),
            )],
        },
        Schema::Array { items, .. } => project_schema(items, way, binding, path, depth + 1),
        _ => vec![(Variant::Bound, way.to_string())],
    }
}
/// The variants of an item of `collection`: the items of each array it can
/// be. Where the collection itself is missing its own read is refused, so
/// the item is taken as bound there.
fn item_variants(collection: &Expr, context: &Context, scope: &Scope) -> Lexical {
    let mut out = vec![];
    let mut add = |variants: Variants, of_array: bool| {
        for (variant, way) in variants {
            let items = match (&variant, of_array) {
                (Variant::Shape(Schema::Array { items, .. }), true) => {
                    vec![(Variant::Shape((**items).clone()), way)]
                }
                (Variant::Shape(Schema::Union { variants }), true) => variants
                    .iter()
                    .map(|v| match v {
                        Schema::Array { items, .. } => {
                            (Variant::Shape((**items).clone()), way.clone())
                        }
                        _ => (Variant::Bound, way.clone()),
                    })
                    .collect(),
                (Variant::Shape(_) | Variant::Keys(_), false) => vec![(variant, way)],
                _ => vec![(Variant::Bound, way)],
            };
            push_unique(&mut out, items);
        }
    };
    match collection {
        Expr::Array { items } => {
            for item in items {
                match alias(item, context, scope) {
                    Some((variants, _)) => add(variants, false),
                    None => add(vec![(Variant::Bound, String::new())], false),
                }
            }
            Lexical {
                variants: out,
                shown: None,
                note: Some("is an item of its collection".into()),
            }
        }
        _ => match alias(collection, context, scope) {
            Some((variants, origin)) => {
                add(variants, true);
                Lexical {
                    variants: out,
                    shown: None,
                    note: Some(format!("is an item of `{origin}`")),
                }
            }
            None => bound_name(),
        },
    }
}
fn bound_name() -> Lexical {
    Lexical {
        variants: vec![(Variant::Bound, String::new())],
        shown: None,
        note: None,
    }
}

/// Every reference in `expr` (at location `at`) that some variant of its
/// binding, or of the binding an alias or item was derived from, cannot
/// satisfy, as (location, message). `exempt` holds inside `exists`; `key`
/// inside a lookup key, which is never exempt.
fn strict_reads(
    expr: &Expr,
    at: &str,
    exempt: bool,
    key: bool,
    context: &Context,
    scope: &Scope,
    out: &mut Vec<(String, String)>,
) {
    let mut sub = |e: &Expr, field: &str, exempt: bool, key: bool, scope: &Scope| {
        strict_reads(
            e,
            &operand(at, expr, field),
            exempt,
            key,
            context,
            scope,
            out,
        )
    };
    match expr {
        Expr::Literal { .. } => {}
        Expr::Ref { root, path } => {
            if exempt {
                return;
            }
            let (binding, rest) = split_ref(root, path);
            let (variants, shown, note) = match scope.get(binding) {
                Some(name) => (
                    &name.variants,
                    name.shown.clone().unwrap_or_else(|| binding.to_string()),
                    name.note.clone(),
                ),
                None => match context.get(binding) {
                    Some(variants) => (variants, binding.to_string(), None),
                    None => return,
                },
            };
            let mut lacks = vec![];
            for (variant, way) in variants {
                lacking(variant, way, &shown, rest, 0, &mut lacks);
            }
            if lacks.is_empty() {
                return;
            }
            let note = note
                .map(|n| format!(" (`{binding}` {n})"))
                .unwrap_or_default();
            let mut message = format!(
                "`{}`{note} is not there on every way to this step: {}. Read it only where it is \
                 always present: after the outcome edge of the step that returns it, or on the \
                 `completed` edge of a join whose `accepted_outcomes` lists only outcomes that \
                 have it (a `decide` on `outcome` does not narrow it). Or test it first with \
                 `exists`",
                dotted(root, path),
                lacks.join("; ")
            );
            if key {
                message.push_str(
                    ". A lookup whose key is missing takes its default without a trace, so a \
                     lookup key is held to this too",
                );
            }
            out.push((at.to_string(), message));
        }
        Expr::Object { fields } => {
            for (k, e) in fields {
                sub(e, &format!(".{k}"), exempt, key, scope);
            }
        }
        Expr::Array { items } => {
            for (i, e) in items.iter().enumerate() {
                sub(e, &format!("[{i}]"), exempt, key, scope);
            }
        }
        // Missing becomes `false` here; everything else inside propagates it
        // to here, except a lookup key, which is checked regardless.
        Expr::Exists { value } => sub(value, ".value", true, key, scope),
        Expr::Not { value } => sub(value, ".value", exempt, key, scope),
        Expr::And { values } | Expr::Or { values } => {
            for (i, e) in values.iter().enumerate() {
                sub(e, &format!(".values[{i}]"), exempt, key, scope);
            }
        }
        Expr::Eq { left, right }
        | Expr::Lt { left, right }
        | Expr::Difference { left, right, .. } => {
            sub(left, ".left", exempt, key, scope);
            sub(right, ".right", exempt, key, scope);
        }
        Expr::Contains { collection, value } => {
            sub(collection, ".collection", exempt, key, scope);
            sub(value, ".value", exempt, key, scope);
        }
        Expr::Map {
            collection,
            item,
            value: body,
            ..
        }
        | Expr::Filter {
            collection,
            item,
            predicate: body,
            ..
        } => {
            sub(collection, ".collection", exempt, key, scope);
            // The item shadows any binding of the same name.
            let mut inner = scope.clone();
            inner.insert(item.clone(), item_variants(collection, context, scope));
            let field = if matches!(expr, Expr::Map { .. }) {
                ".value"
            } else {
                ".predicate"
            };
            sub(body, field, exempt, key, &inner);
        }
        Expr::Lookup {
            key: k,
            cases,
            default,
        } => {
            sub(k, ".key", false, true, scope);
            for (i, c) in cases.iter().enumerate() {
                sub(&c.then, &format!(".cases[{i}].then"), exempt, key, scope);
            }
            sub(default, ".default", exempt, key, scope);
        }
        Expr::Let { bindings, body } => {
            let mut inner = scope.clone();
            for (i, b) in bindings.iter().enumerate() {
                // An alias of a binding is checked where it is used, as the
                // binding would be: binding a missing value has no effect
                // until it is read.
                match alias(&b.value, context, &inner) {
                    Some((variants, origin)) => {
                        let note = Some(format!("is `{origin}`"));
                        let shown = Some(origin);
                        inner.insert(
                            b.name.clone(),
                            Lexical {
                                variants,
                                shown,
                                note,
                            },
                        );
                    }
                    None => {
                        sub(
                            &b.value,
                            &format!(".bindings[{i}].value"),
                            exempt,
                            key,
                            &inner,
                        );
                        inner.insert(b.name.clone(), bound_name());
                    }
                }
            }
            sub(body, ".body", exempt, key, &inner);
        }
    }
}
/// Pushes, for `variant` (arising `way`), why `binding.path[depth..]` is
/// absent in it. Nullable and scalar stops are left to ordinary typing.
fn lacking(
    variant: &Variant,
    way: &str,
    binding: &str,
    path: &[String],
    depth: usize,
    lacks: &mut Vec<String>,
) {
    let push = |lacks: &mut Vec<String>, text: String| {
        if !lacks.contains(&text) {
            lacks.push(text);
        }
    };
    match variant {
        Variant::Absent => push(lacks, format!("on {way}, `{binding}` is not bound")),
        Variant::Lacks(text) => push(lacks, text.clone()),
        Variant::Keys(keys) => {
            let Some(alternatives) = path.get(depth).and_then(|k| keys.get(k)) else {
                return;
            };
            for (v, w) in alternatives {
                if *v == Variant::Absent {
                    push(
                        lacks,
                        format!("on {w}, `{}` is absent", dotted(binding, &path[..=depth])),
                    );
                } else {
                    lacking(v, w, binding, path, depth + 1, lacks);
                }
            }
        }
        Variant::Shape(schema) => lacking_in(schema, way, binding, path, depth, lacks),
        Variant::Bound => {}
    }
}
fn lacking_in(
    schema: &Schema,
    way: &str,
    binding: &str,
    path: &[String],
    depth: usize,
    lacks: &mut Vec<String>,
) {
    let Some(key) = path.get(depth) else {
        return;
    };
    match schema {
        Schema::Union { variants } => {
            for v in variants {
                lacking_in(v, way, binding, path, depth, lacks);
            }
        }
        Schema::Object { properties, .. } => match properties.get(key) {
            Some(next) => lacking_in(next, way, binding, path, depth + 1, lacks),
            None => {
                let text = format!(
                    "{way} has no `{key}` in `{}` (it has {})",
                    dotted(binding, &path[..depth]),
                    properties.keys().cloned().collect::<Vec<_>>().join(", ")
                );
                if !lacks.contains(&text) {
                    lacks.push(text);
                }
            }
        },
        Schema::Array { items, .. } => lacking_in(items, way, binding, path, depth + 1, lacks),
        _ => {}
    }
}
