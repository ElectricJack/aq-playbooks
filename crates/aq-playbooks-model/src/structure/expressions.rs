//! Lexical binding checks and evaluation bounds for portable expressions.
use super::*;

pub(super) struct ExpressionCheck<'a> {
    step_roots: &'a BTreeSet<String>,
    lexical: Vec<String>,
    path: &'a str,
    issues: &'a mut Vec<ValidationIssue>,
}
impl<'a> ExpressionCheck<'a> {
    pub(super) fn new(
        step_roots: &'a BTreeSet<String>,
        path: &'a str,
        issues: &'a mut Vec<ValidationIssue>,
    ) -> Self {
        Self {
            step_roots,
            lexical: Vec::new(),
            path,
            issues,
        }
    }
    pub(super) fn validate(mut self, expr: &Expr, at: &str) -> bool {
        self.visit(expr, at);
        check_eval_bounds(expr, at, self.path, self.issues)
    }
    fn visit(&mut self, expr: &Expr, at: &str) {
        let loc = |field: &str| operand(at, expr, field);
        match expr {
            Expr::Literal { .. } | Expr::Ref { .. } => {}
            Expr::Object { fields } => {
                for (k, e) in fields {
                    self.visit(e, &loc(&format!(".{k}")));
                }
            }
            Expr::Array { items } => {
                for (i, e) in items.iter().enumerate() {
                    self.visit(e, &loc(&format!("[{i}]")));
                }
            }
            Expr::Exists { value } | Expr::Not { value } => self.visit(value, &loc(".value")),
            Expr::And { values } | Expr::Or { values } => {
                for (i, e) in values.iter().enumerate() {
                    self.visit(e, &loc(&format!(".values[{i}]")));
                }
            }
            Expr::Eq { left, right }
            | Expr::Lt { left, right }
            | Expr::Difference { left, right, .. } => {
                self.visit(left, &loc(".left"));
                self.visit(right, &loc(".right"));
            }
            Expr::Contains { collection, value } => {
                self.visit(collection, &loc(".collection"));
                self.visit(value, &loc(".value"));
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
                self.visit(collection, &loc(".collection"));
                self.lexical.push(item.clone());
                let field = if matches!(expr, Expr::Map { .. }) {
                    ".value"
                } else {
                    ".predicate"
                };
                self.visit(value, &loc(field));
                self.lexical.pop();
            }
            Expr::Lookup {
                key,
                cases,
                default,
            } => {
                self.check_cases(at, cases);
                self.visit(key, &loc(".key"));
                for (i, c) in cases.iter().enumerate() {
                    self.visit(&c.then, &loc(&format!(".cases[{i}].then")));
                }
                self.visit(default, &loc(".default"));
            }
            Expr::Let { bindings, body } => self.check_let(at, bindings, body),
        }
    }
    fn check_cases(&mut self, at: &str, cases: &[LookupCase]) {
        let path = self.path;
        if cases.len() > MAX_LOOKUP_CASES {
            issue(
                self.issues,
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
            let bytes = match canonical_bytes(&c.when) {
                Ok(bytes) => bytes,
                Err(error) => {
                    issue(
                        self.issues,
                        "lookup_case",
                        path,
                        format!(
                            "{when}: is not canonical JSON ({error}); use integers, strings, booleans, null, arrays or objects"
                        ),
                    );
                    continue;
                }
            };
            if bytes.len() > MAX_LOOKUP_KEY_BYTES {
                issue(
                    self.issues,
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
                    self.issues,
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
    fn check_let(&mut self, at: &str, bindings: &[LetBinding], body: &Expr) {
        let path = self.path;
        if bindings.len() > MAX_LET_BINDINGS {
            issue(
                self.issues,
                "expression_limit",
                path,
                format!(
                    "{path}.{at}.let: has {} bindings; a let has at most {MAX_LET_BINDINGS}. Nest a second let in the body for the rest.",
                    bindings.len()
                ),
            );
        }
        let outer = self.lexical.len();
        for (i, b) in bindings.iter().enumerate() {
            self.visit(&b.value, &format!("{at}.let.bindings[{i}].value"));
            let name = &b.name;
            if let Some(refusal) = self.binding_refusal(name) {
                issue(
                    self.issues,
                    "binding",
                    path,
                    format!(
                        "{path}.{at}.let.bindings[{i}].name: `{name}` {refusal}; choose another name"
                    ),
                );
            }
            self.lexical.push(name.clone());
        }
        self.visit(body, &format!("{at}.let.body"));
        self.lexical.truncate(outer);
    }
    fn binding_refusal(&self, name: &String) -> Option<&'static str> {
        if !valid_name(name) {
            Some("is not a valid name (1-128 of A-Z a-z 0-9 _ - .)")
        } else if RESERVED_ROOTS.contains(&name.as_str()) {
            Some("is a reserved root (input, event, definitions, vars)")
        } else if self.step_roots.contains(name) {
            Some("is already the name of a step binding or foreach item in this playbook")
        } else if self.lexical.contains(name) {
            Some("is already bound by an enclosing let, map or filter, or earlier in this let")
        } else {
            None
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
    let mut cost = EvalCost {
        depth: 0,
        elements: 1,
        lookup_or_let: matches!(expr, Expr::Lookup { .. } | Expr::Let { .. }),
    };
    cost.elements = cost.children(expr).saturating_add(1);
    cost
}
impl EvalCost {
    fn visit(&mut self, expr: &Expr) -> usize {
        let child = eval_cost(expr);
        self.depth = self.depth.max(child.depth + 1);
        self.lookup_or_let |= child.lookup_or_let;
        child.elements
    }
    fn sum<'a>(&mut self, exprs: impl Iterator<Item = &'a Expr>) -> usize {
        exprs.fold(0usize, |n, e| n.saturating_add(self.visit(e)))
    }
    fn children(&mut self, expr: &Expr) -> usize {
        match expr {
            Expr::Literal { .. } | Expr::Ref { .. } => 0,
            Expr::Object { fields } => self.sum(fields.values()),
            Expr::Array { items: values } | Expr::And { values } | Expr::Or { values } => {
                self.sum(values.iter())
            }
            Expr::Exists { value } | Expr::Not { value } => self.visit(value),
            Expr::Eq { left, right }
            | Expr::Lt { left, right }
            | Expr::Contains {
                collection: left,
                value: right,
            } => self.visit(left).saturating_add(self.visit(right)),
            Expr::Difference {
                left,
                right,
                max_items,
                ..
            } => self
                .visit(left)
                .saturating_add(self.visit(right))
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
            } => self
                .visit(collection)
                .saturating_add(max_items.saturating_mul(self.visit(value))),
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
                    .map(|e| self.visit(e))
                    .max()
                    .unwrap_or(0);
                cases
                    .len()
                    .saturating_add(self.visit(key))
                    .saturating_add(branch)
            }
            Expr::Let { bindings, body } => self
                .sum(bindings.iter().map(|b| &b.value))
                .saturating_add(self.visit(body)),
        }
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
