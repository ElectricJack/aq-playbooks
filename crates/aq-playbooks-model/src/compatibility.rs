//! Check each expression against the schema expected at its location.
use super::*;

pub(super) struct Compatibility<'a> {
    roots: &'a BTreeMap<String, Schema>,
    path: &'a str,
    issues: &'a mut Vec<ValidationIssue>,
}

impl<'a> Compatibility<'a> {
    pub(super) fn new(
        roots: &'a BTreeMap<String, Schema>,
        path: &'a str,
        issues: &'a mut Vec<ValidationIssue>,
    ) -> Self {
        Self {
            roots,
            path,
            issues,
        }
    }

    fn mismatch(&mut self, at: &str, message: String) {
        issue(
            self.issues,
            "schema_compatibility",
            self.path,
            format!("{}.{at}: {message}", self.path),
        );
    }

    pub(super) fn check(&mut self, expr: &Expr, target: &Schema, at: &str) {
        match (expr, target) {
            (Expr::Literal { value }, _) => self.literal(value, target, at),
            (
                Expr::Object { fields },
                Schema::Object {
                    properties,
                    required,
                },
            ) => {
                if let Some(message) = invalid_keys(fields, properties, required, target) {
                    self.mismatch(at, message);
                }
                self.fields(fields, properties, at);
            }
            (
                Expr::Array { items },
                Schema::Array {
                    items: expected,
                    max_items,
                },
            ) => {
                self.array_bound(items.len(), *max_items, at);
                for (i, item) in items.iter().enumerate() {
                    self.check(item, expected, &format!("{at}[{i}]"));
                }
            }
            (
                Expr::Lookup {
                    key,
                    cases,
                    default,
                },
                _,
            ) => {
                if let Err(error) = validate_lookup_key(key, cases, self.roots, at) {
                    type_issue(self.issues, self.path, error);
                    return;
                }
                for (i, case) in cases.iter().enumerate() {
                    self.check(&case.then, target, &format!("{at}.lookup.cases[{i}].then"));
                }
                self.check(default, target, &format!("{at}.lookup.default"));
            }
            (Expr::Let { bindings, body }, _) => match let_roots(bindings, self.roots, 0, at) {
                Ok(local) => Compatibility::new(&local, self.path, self.issues).check(
                    body,
                    target,
                    &format!("{at}.let.body"),
                ),
                Err(error) => type_issue(self.issues, self.path, error),
            },
            _ => self.inferred(expr, target, at),
        }
    }

    // Empty literals have no inherent element type. Validate the value directly.
    fn literal(&mut self, value: &Value, target: &Schema, at: &str) {
        if target.validate(value).is_err() {
            self.mismatch(
                at,
                format!(
                    "literal {} is not a value of the target type {}",
                    shown(value),
                    describe(target)
                ),
            );
        }
    }

    fn fields(
        &mut self,
        fields: &BTreeMap<String, Expr>,
        properties: &BTreeMap<String, Schema>,
        at: &str,
    ) {
        for (key, expr) in fields {
            if let Some(target) = properties.get(key) {
                self.check(expr, target, &format!("{at}.{key}"));
            }
        }
    }

    fn array_bound(&mut self, length: usize, max_items: usize, at: &str) {
        if length > max_items {
            self.mismatch(
                at,
                format!(
                    "expression array exceeds target bound: {} items, at most {max_items}",
                    length
                ),
            );
        }
    }

    fn inferred(&mut self, expr: &Expr, target: &Schema, at: &str) {
        match infer_expr(expr, self.roots, 0, at) {
            Ok(source) if !source.assignable_to(target) => self.mismatch(at, format!(
                "expression type/bounds not assignable to target schema: it has type {}, the target is {}",
                describe(&source), describe(target))),
            Ok(_) => (),
            Err(error) => type_issue(self.issues, self.path, error),
        }
    }
}

fn invalid_keys(
    fields: &BTreeMap<String, Expr>,
    properties: &BTreeMap<String, Schema>,
    required: &BTreeSet<String>,
    target: &Schema,
) -> Option<String> {
    let missing: Vec<_> = required
        .iter()
        .filter(|k| !fields.contains_key(*k))
        .collect();
    let unknown: Vec<_> = fields
        .keys()
        .filter(|k| !properties.contains_key(*k))
        .collect();
    if missing.is_empty() && unknown.is_empty() {
        return None;
    }
    Some(format!(
        "expression object keys incompatible with closed target schema: missing required {missing:?}, not allowed {unknown:?}; the target is {}",
        describe(target)
    ))
}
