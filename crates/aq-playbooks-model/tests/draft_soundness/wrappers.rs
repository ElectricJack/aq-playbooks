use super::*;

/// A combinator: wraps the expression built so far.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Wrap {
    // Give back the same value: a value read through them is the same read.
    LetAlias,
    AliasOfAlias,
    ObjectField,
    ArrayPosition,
    LookupBoth,
    MapResultIndex,
    FilterResultIndex,
    DifferenceResult,
    /// Read in a later step than the one control arrives at.
    LaterStep,
    /// The value is the item of a `for_each`, read in its body.
    ForEachItem,
    // Give back the value or null: only around the finished read.
    LookupCase,
    LookupDefault,
    // Compute something else from the value.
    Object,
    Array,
    MapBody,
    FilterPredicate,
    Contains,
    Eq,
    Lt,
    Not,
    And,
    Or,
    /// The value is a lookup key.
    LookupKey,
    Exists,
}
pub(super) const WRAPS: [Wrap; 24] = [
    Wrap::LetAlias,
    Wrap::AliasOfAlias,
    Wrap::ObjectField,
    Wrap::ArrayPosition,
    Wrap::LookupBoth,
    Wrap::MapResultIndex,
    Wrap::FilterResultIndex,
    Wrap::DifferenceResult,
    Wrap::LaterStep,
    Wrap::ForEachItem,
    Wrap::LookupCase,
    Wrap::LookupDefault,
    Wrap::Object,
    Wrap::Array,
    Wrap::MapBody,
    Wrap::FilterPredicate,
    Wrap::Contains,
    Wrap::Eq,
    Wrap::Lt,
    Wrap::Not,
    Wrap::And,
    Wrap::Or,
    Wrap::LookupKey,
    Wrap::Exists,
];
/// What a combinator does with a Missing value.
#[derive(PartialEq)]
pub(super) enum Missing {
    /// Its result is Missing too.
    Carries,
    /// It needs the value: the run stops, or a default is taken silently.
    Needs,
    /// `exists`: its result is `false`.
    Tests,
}
impl Wrap {
    pub(super) fn missing(self) -> Missing {
        match self {
            Wrap::LetAlias
            | Wrap::AliasOfAlias
            | Wrap::ObjectField
            | Wrap::ArrayPosition
            | Wrap::LookupBoth
            | Wrap::MapResultIndex
            | Wrap::FilterResultIndex
            | Wrap::DifferenceResult
            | Wrap::LaterStep
            | Wrap::LookupCase
            | Wrap::LookupDefault
            | Wrap::Object
            | Wrap::Array
            | Wrap::MapBody
            | Wrap::FilterPredicate
            | Wrap::Contains
            | Wrap::Eq
            | Wrap::Lt
            | Wrap::Not
            | Wrap::And
            | Wrap::Or => Missing::Carries,
            Wrap::ForEachItem | Wrap::LookupKey => Missing::Needs,
            Wrap::Exists => Missing::Tests,
        }
    }
    /// Gives back the very value it wraps, so a path can be read through it.
    pub(super) fn same_value(self) -> bool {
        matches!(
            self,
            Wrap::LetAlias
                | Wrap::AliasOfAlias
                | Wrap::ObjectField
                | Wrap::ArrayPosition
                | Wrap::LookupBoth
                | Wrap::MapResultIndex
                | Wrap::FilterResultIndex
                | Wrap::DifferenceResult
                | Wrap::LaterStep
                | Wrap::ForEachItem
        )
    }
}
fn alias_pair(value: Expr, outer: &str, inner: &str) -> Expr {
    let_(outer, value, let_(inner, r(outer, &[]), r(inner, &[])))
}

fn map_result(e: Expr, item: &str, name: &str) -> Expr {
    let mapped = Expr::Map {
        collection: b(array(vec![e])),
        item: item.into(),
        value: b(r(item, &[])),
        max_items: 1,
    };
    let_(name, mapped, r(name, &["0"]))
}
fn filter_result(e: Expr, item: &str, name: &str) -> Expr {
    let filtered = Expr::Filter {
        collection: b(array(vec![e])),
        item: item.into(),
        predicate: b(lit(json!(true))),
        max_items: 1,
    };
    let_(name, filtered, r(name, &["0"]))
}
fn difference_result(e: Expr, name: &str) -> Expr {
    let left = array(vec![Expr::Object {
        fields: BTreeMap::from([("id".to_string(), lit(json!("a"))), ("v".into(), e)]),
    }]);
    let kept = Expr::Difference {
        left: b(left),
        right: b(lit(json!([]))),
        identity: vec!["id".into()],
        max_items: 1,
    };
    let_(name, kept, r(name, &["0", "v"]))
}

/// A step in front of the one that reads.
pub(super) enum Layer {
    Later,
    ForEach { collection: Expr, item: String },
}
/// The expression built so far and the steps in front of it.
pub(super) struct Built {
    pub(super) expr: Expr,
    /// The expression is a string (`lt` compares two of them).
    string: bool,
    pub(super) layers: Vec<Layer>,
    /// A string every way has, for the key of a table: `input.task` in a
    /// step, `event.note` in a trigger.
    key: Expr,
}
impl Built {
    pub(super) fn new(expr: Expr, string: bool, site: Site) -> Self {
        let key = match site {
            Site::Decide | Site::Input | Site::Output => r("input", &["task"]),
            Site::Trigger => r("event", &["note"]),
        };
        Self {
            expr,
            string,
            layers: vec![],
            key,
        }
    }
}
impl Built {
    pub(super) fn wrap(&mut self, wrap: Wrap, n: usize) -> bool {
        let e = self.expr.clone();
        let name = |prefix: &str| format!("{prefix}{n}");
        let item = name("x");
        let both = |e: &Expr| eq(e.clone(), e.clone());
        self.expr = match wrap {
            Wrap::LetAlias => let_(&name("a"), e, r(&name("a"), &[])),
            Wrap::AliasOfAlias => alias_pair(e, &name("a"), &name("b")),
            Wrap::ObjectField => let_(&name("o"), object("k", e), r(&name("o"), &["k"])),
            Wrap::ArrayPosition => let_(&name("r"), array(vec![e]), r(&name("r"), &["0"])),
            Wrap::LookupBoth => lookup(self.key.clone(), vec![(json!("a"), e.clone())], e),
            Wrap::MapResultIndex => map_result(e, &item, &name("m")),
            Wrap::FilterResultIndex => filter_result(e, &item, &name("f")),
            Wrap::DifferenceResult => difference_result(e, &name("d")),
            Wrap::LaterStep => {
                self.layers.push(Layer::Later);
                e
            }
            Wrap::ForEachItem => self.loop_item(e, name("t")),
            Wrap::LookupCase => lookup(self.key.clone(), vec![(json!("a"), e)], lit(Value::Null)),
            Wrap::LookupDefault => {
                lookup(self.key.clone(), vec![(json!("a"), lit(Value::Null))], e)
            }
            Wrap::Object => object("k", e),
            Wrap::Array => array(vec![e]),
            Wrap::MapBody => Expr::Map {
                collection: b(lit(json!([1]))),
                item,
                value: b(e),
                max_items: 1,
            },
            Wrap::FilterPredicate => Expr::Filter {
                collection: b(lit(json!([1]))),
                item,
                predicate: b(both(&e)),
                max_items: 1,
            },
            Wrap::Contains => Expr::Contains {
                collection: b(array(vec![e.clone()])),
                value: b(e),
            },
            Wrap::Eq => both(&e),
            Wrap::Lt => {
                if !self.string {
                    return false;
                }
                Expr::Lt {
                    left: b(e.clone()),
                    right: b(e),
                }
            }
            Wrap::Not => Expr::Not { value: b(both(&e)) },
            Wrap::And => Expr::And {
                values: vec![both(&e), lit(json!(true))],
            },
            Wrap::Or => Expr::Or {
                values: vec![lit(json!(false)), both(&e)],
            },
            Wrap::LookupKey => lookup(e, vec![], lit(json!(true))),
            Wrap::Exists => Expr::Exists { value: b(e) },
        };
        self.string &= wrap.same_value();
        true
    }
    fn loop_item(&mut self, value: Expr, item: String) -> Expr {
        let reference = r(&item, &[]);
        self.layers.push(Layer::ForEach {
            collection: array(vec![value]),
            item,
        });
        reference
    }
    /// Reads `path` in the expression built so far.
    pub(super) fn project(&mut self, path: &[&str]) {
        // The expression is a reference (to the carrier, a name or an item),
        // or a lookup of two references: read the path in each.
        fn read(e: &mut Expr, path: &[&str]) {
            match e {
                Expr::Ref { path: p, .. } => p.extend(path.iter().map(|s| s.to_string())),
                Expr::Let { body, .. } => read(body, path),
                Expr::Lookup { cases, default, .. } => {
                    for c in cases {
                        read(&mut c.then, path);
                    }
                    read(default, path);
                }
                other => panic!("not a value to read through: {other:?}"),
            }
        }
        if !path.is_empty() {
            read(&mut self.expr, path);
            self.string = true;
        }
    }
    /// Puts the generated read into `content`: the layers as steps from
    /// `SLOT` on, then the expression where `site` needs its value.
    pub(super) fn install(self, site: Site, content: &mut ArtifactContent) {
        let steps = &mut content.playbook.steps;
        steps.retain(|name, _| {
            name != SLOT && !(name.starts_with('g') && name[1..].parse::<usize>().is_ok())
        });
        content.playbook.triggers.clear();
        let names: Vec<String> = (0..=self.layers.len())
            .map(|i| if i == 0 { SLOT.into() } else { format!("g{i}") })
            .collect();
        let after = install_layers(steps, &names, self.layers);
        // A test of any value that is Missing where the value is.
        let reads = let_("v", self.expr, eq(r("v", &[]), r("v", &[])));
        let last = names.last().unwrap().clone();
        let reader = site.reader(reads, &after, content);
        content.playbook.steps.insert(last, reader);
    }
}

fn install_layers(
    steps: &mut BTreeMap<String, Step>,
    names: &[String],
    layers: Vec<Layer>,
) -> String {
    // Where a step leads when it is done: the innermost loop around it.
    let mut after = "end".to_string();
    for (i, layer) in layers.into_iter().enumerate() {
        let next = &names[i + 1];
        match layer {
            Layer::Later => {
                steps.insert(names[i].clone(), decide(lit(json!(true)), next));
            }
            Layer::ForEach { collection, item } => {
                steps.insert(
                    names[i].clone(),
                    Step {
                        op: Operation::ForEach {
                            collection,
                            item,
                            body: next.clone(),
                            next: after.clone(),
                            max_items: 1,
                            concurrency: 1,
                            on_failure: IterationFailure::Stop,
                        },
                        edges: BTreeMap::new(),
                        on_error: Some("end".into()),
                    },
                );
                after = names[i].clone();
            }
        }
    }
    after
}
impl Site {
    fn reader(self, reads: Expr, after: &str, content: &mut ArtifactContent) -> Step {
        let flag = |reads: Expr| Expr::Object {
            fields: BTreeMap::from([("flag".to_string(), reads)]),
        };
        match self {
            Site::Decide => decide(reads, after),
            Site::Input => {
                let work = &content.registry.commands["work.flag"];
                Step {
                    op: Operation::Invoke {
                        command: ContractRef {
                            name: work.name.clone(),
                            fingerprint: work.fingerprint().unwrap(),
                        },
                        input: flag(reads),
                        bind: "flagged".into(),
                        retry: None,
                    },
                    edges: edges(&[("done", after)]),
                    on_error: Some("end".into()),
                }
            }
            // A step that returns it, beside the way on to `end`.
            Site::Output => {
                content.playbook.steps.insert(
                    "g99".into(),
                    Step {
                        op: Operation::Return {
                            outcome: "told".into(),
                            output: reads,
                        },
                        edges: BTreeMap::new(),
                        on_error: None,
                    },
                );
                Step {
                    op: Operation::Decide {
                        predicate: lit(json!(true)),
                    },
                    edges: edges(&[("true", "g99"), ("false", after), ("unknown", after)]),
                    on_error: None,
                }
            }
            Site::Trigger => {
                let seen = &content.registry.events["aq.work.seen"];
                let mut input = flag(reads);
                let Expr::Object { fields } = &mut input else {
                    unreachable!()
                };
                fields.insert("task".into(), r("event", &["note"]));
                content.playbook.triggers.push(Trigger {
                    id: "t".into(),
                    event: ContractRef {
                        name: seen.name.clone(),
                        fingerprint: seen.fingerprint().unwrap(),
                    },
                    entry: "main".into(),
                    input,
                });
                decide(lit(json!(true)), "end")
            }
        }
    }
}
