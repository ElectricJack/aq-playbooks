//! Infer a bounded expression schema in its lexical environment.
use super::*;

/// A typing error at the location (inside the step) of the innermost
/// expression that caused it.
#[derive(Debug)]
pub(super) struct TypeError {
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
    pub(super) fn render(&self, path: &str) -> String {
        format!(
            "{path}.{}: {}",
            self.at.as_deref().unwrap_or_default(),
            self.message
        )
    }
}

type Inferred = Result<Schema, TypeError>;
struct Node<'a> {
    expr: &'a Expr,
    roots: &'a BTreeMap<String, Schema>,
    depth: usize,
    at: &'a str,
}
pub(super) fn infer_expr(
    expr: &Expr,
    roots: &BTreeMap<String, Schema>,
    depth: usize,
    at: &str,
) -> Inferred {
    Node {
        expr,
        roots,
        depth,
        at,
    }
    .infer()
    .map_err(|mut error| {
        error.at.get_or_insert_with(|| at.to_owned());
        error
    })
}
impl Node<'_> {
    fn child(&self, expr: &Expr, field: &str) -> Inferred {
        infer_expr(
            expr,
            self.roots,
            self.depth + 1,
            &operand(self.at, self.expr, field),
        )
    }
    fn infer(&self) -> Inferred {
        if self.depth > 32 {
            return Err("expression nesting exceeds 32".into());
        }
        match self.expr {
            Expr::Literal { value } => Ok(literal_schema(value)?),
            Expr::Ref { root, path } => self.reference(root, path),
            Expr::Object { fields } => Ok(Schema::object(
                fields
                    .iter()
                    .map(|(k, e)| Ok((k.clone(), self.child(e, &format!(".{k}"))?)))
                    .collect::<Result<_, TypeError>>()?,
            )),
            Expr::Array { items } => self.array(items),
            Expr::Exists { value } => {
                self.child(value, ".value")?;
                Ok(Schema::Boolean)
            }
            Expr::Not { value } => Self::boolean_operand(
                self.child(value, ".value")?,
                "not requires boolean expression",
            ),
            Expr::And { values } | Expr::Or { values } => self.boolean_values(values),
            Expr::Eq { left, right } | Expr::Lt { left, right } => self.compare(left, right),
            Expr::Contains { collection, value } => self.membership(collection, value),
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
                let array = self.collection_input(collection, *max_items)?;
                self.transform(item, value, array)
            }
            Expr::Difference {
                left,
                right,
                identity,
                max_items,
            } => self.difference([left, right], identity, *max_items),
            Expr::Lookup {
                key,
                cases,
                default,
            } => self.lookup(key, cases, default),
            Expr::Let { bindings, body } => {
                let local = let_roots(bindings, self.roots, self.depth, self.at)?;
                infer_expr(
                    body,
                    &local,
                    self.depth + 1,
                    &format!("{}.let.body", self.at),
                )
            }
        }
    }
    fn reference(&self, root: &str, path: &[String]) -> Inferred {
        let current = self
            .roots
            .get(root)
            .ok_or_else(|| format!("unknown or unsupported typed root {root}"))?;
        schema_path(current, path)
            .map_err(|error| unreadable_ref(root, path, current, error).into())
    }
    fn array(&self, items: &[Expr]) -> Inferred {
        let mut element = None;
        for (i, e) in items.iter().enumerate() {
            let x = self.child(e, &format!("[{i}]"))?;
            element = Some(match element {
                None => x,
                Some(old) => {
                    merge_schema(&old, &x).ok_or("heterogeneous expression array unsupported")?
                }
            });
        }
        Ok(Schema::Array {
            items: Box::new(element.unwrap_or_else(no_type)),
            max_items: items.len().max(1),
        })
    }
    fn boolean_operand(schema: Schema, error: &str) -> Inferred {
        if !is_boolean(&schema) {
            return Err(error.into());
        }
        Ok(Schema::Boolean)
    }
    fn boolean_values(&self, values: &[Expr]) -> Inferred {
        for (i, e) in values.iter().enumerate() {
            Self::boolean_operand(
                self.child(e, &format!(".values[{i}]"))?,
                "boolean logic requires booleans",
            )?;
        }
        Ok(Schema::Boolean)
    }
    fn compare(&self, left: &Expr, right: &Expr) -> Inferred {
        let left = self.child(left, ".left")?;
        let right = self.child(right, ".right")?;
        if matches!(self.expr, Expr::Eq { .. }) {
            if merge_schema(&left, &right).is_none() {
                return Err("equality compares incompatible types".into());
            }
        } else if !matches!(
            (left, right),
            (Schema::Integer { .. }, Schema::Integer { .. })
                | (Schema::String { .. }, Schema::String { .. })
        ) {
            return Err("ordering requires two integers or strings".into());
        }
        Ok(Schema::Boolean)
    }
    fn membership(&self, collection: &Expr, value: &Expr) -> Inferred {
        let Schema::Array { items, .. } = self.child(collection, ".collection")? else {
            return Err("membership requires bounded array".into());
        };
        if merge_schema(&items, &self.child(value, ".value")?).is_none() {
            return Err("membership value has incompatible type".into());
        }
        Ok(Schema::Boolean)
    }
    fn collection_input(
        &self,
        collection: &Expr,
        max_items: usize,
    ) -> Result<(Box<Schema>, usize), TypeError> {
        let Schema::Array {
            items,
            max_items: bound,
        } = self.child(collection, ".collection")?
        else {
            return Err("collection expression requires bounded array".into());
        };
        if !(1..=100_000).contains(&max_items) || bound > max_items {
            return Err("collection expression limit cannot cover input schema".into());
        }
        Ok((items, bound))
    }
    fn transform(&self, item: &str, value: &Expr, array: (Box<Schema>, usize)) -> Inferred {
        let (items, bound) = array;
        if self.roots.contains_key(item) || !valid_name(item) {
            return Err("invalid/shadowing collection lexical root".into());
        }
        let mut local = self.roots.clone();
        local.insert(item.to_owned(), *items.clone());
        let filter = matches!(self.expr, Expr::Filter { .. });
        let field = if filter { ".predicate" } else { ".value" };
        let result = infer_expr(
            value,
            &local,
            self.depth + 1,
            &operand(self.at, self.expr, field),
        )?;
        let items = if filter {
            Self::boolean_operand(result, "filter predicate requires boolean")?;
            items
        } else {
            Box::new(result)
        };
        Ok(Schema::Array {
            items,
            max_items: bound,
        })
    }
    fn difference(&self, pair: [&Expr; 2], identity: &[String], max_items: usize) -> Inferred {
        let Schema::Array {
            items: a,
            max_items: x,
        } = self.child(pair[0], ".left")?
        else {
            return Err("difference requires bounded arrays".into());
        };
        let Schema::Array {
            items: b,
            max_items: y,
        } = self.child(pair[1], ".right")?
        else {
            return Err("difference requires bounded arrays".into());
        };
        let bounded = max_items > 0 && x.max(y) <= max_items;
        if !bounded || identity.is_empty() {
            return Err("difference requires explicit identity and covering element bound".into());
        }
        let roots = BTreeMap::from([("left".into(), *a.clone()), ("right".into(), *b)]);
        let identity_at = operand(self.at, self.expr, ".identity");
        let key = |name: &str| {
            infer_expr(
                &Expr::Ref {
                    root: name.to_owned(),
                    path: identity.to_vec(),
                },
                &roots,
                self.depth + 1,
                &identity_at,
            )
        };
        let a_key = key("left")?;
        let b_key = key("right")?;
        if merge_schema(&a_key, &b_key).is_none() {
            return Err("difference identities incompatible".into());
        }
        Ok(Schema::Array {
            items: a,
            max_items: x,
        })
    }
    fn lookup(&self, key: &Expr, cases: &[LookupCase], default: &Expr) -> Inferred {
        self.check_key(key, cases)?;
        let at = self.at;
        let mut results = vec![];
        for (i, c) in cases.iter().enumerate() {
            results.push((
                format!("cases[{i}].then"),
                self.child(&c.then, &format!(".cases[{i}].then"))?,
            ));
        }
        results.push(("default".into(), self.child(default, ".default")?));
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
    fn check_key(&self, key: &Expr, cases: &[LookupCase]) -> Result<(), TypeError> {
        let at = self.at;
        let key_at = format!("{at}.lookup.key");
        let key_type = infer_expr(key, self.roots, self.depth + 1, &key_at)?;
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
/// The roots visible to a let body: each binding typed with the bindings before it.
pub(super) fn let_roots(
    bindings: &[LetBinding],
    roots: &BTreeMap<String, Schema>,
    depth: usize,
    at: &str,
) -> Result<BTreeMap<String, Schema>, TypeError> {
    let mut local = roots.clone();
    for (i, b) in bindings.iter().enumerate() {
        if local.contains_key(&b.name) || !valid_name(&b.name) {
            // structural validation refuses every such name, with its reason.
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
pub(super) fn type_issue(issues: &mut Vec<ValidationIssue>, path: &str, e: TypeError) {
    if !e.reported {
        issue(issues, "expression_type", path, e.render(path));
    }
}
pub(super) fn validate_lookup_key(
    key: &Expr,
    cases: &[LookupCase],
    roots: &BTreeMap<String, Schema>,
    at: &str,
) -> Result<(), TypeError> {
    Node {
        expr: key,
        roots,
        depth: 0,
        at,
    }
    .check_key(key, cases)
}
