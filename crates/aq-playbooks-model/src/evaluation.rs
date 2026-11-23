//! Bounded evaluation of expressions in a lexical environment.
use super::{
    EvalContext, EvalLimits, EvalValue, Expr, LetBinding, LookupCase, MAX_LET_BINDINGS,
    MAX_LOOKUP_CASES, ModelError, canonical_bytes,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

type Evaluation = Result<EvalValue, ModelError>;
pub fn eval(
    expr: &Expr,
    context: &EvalContext,
    limits: &EvalLimits,
) -> Result<EvalValue, ModelError> {
    let mut budget = limits.max_elements;
    let result = Frame {
        context,
        limits,
        depth: 0,
        remaining: &mut budget,
    }
    .evaluate(expr)?;
    if let EvalValue::Known(v) = &result
        && canonical_bytes(v)?.len() > limits.max_bytes
    {
        return Err(ModelError::Expression("expression byte limit".into()));
    }
    Ok(result)
}
impl Expr {
    pub fn eval(
        &self,
        context: &EvalContext,
        limits: &EvalLimits,
    ) -> Result<EvalValue, ModelError> {
        eval(self, context, limits)
    }
}
fn unresolved(values: &[EvalValue]) -> Option<EvalValue> {
    values
        .iter()
        .find(|x| matches!(x, EvalValue::Unknown(_)))
        .or_else(|| values.iter().find(|x| matches!(x, EvalValue::Missing)))
        .cloned()
}
fn path_value(value: &Value, path: &[String]) -> EvalValue {
    let mut v = value;
    for k in path {
        let n = match v {
            Value::Object(o) => o.get(k),
            Value::Array(a) => k.parse::<usize>().ok().and_then(|i| a.get(i)),
            _ => None,
        };
        let Some(n) = n else {
            return EvalValue::Missing;
        };
        v = n;
    }
    EvalValue::Known(v.clone())
}

struct Frame<'a> {
    context: &'a EvalContext,
    limits: &'a EvalLimits,
    depth: usize,
    remaining: &'a mut usize,
}
impl Frame<'_> {
    fn child(&mut self, expr: &Expr) -> Evaluation {
        self.child_in(expr, self.context)
    }
    fn child_in(&mut self, expr: &Expr, context: &EvalContext) -> Evaluation {
        Frame {
            context,
            limits: self.limits,
            depth: self.depth + 1,
            remaining: self.remaining,
        }
        .evaluate(expr)
    }
    fn values(&mut self, exprs: &[Expr]) -> Result<Vec<EvalValue>, ModelError> {
        exprs.iter().map(|expr| self.child(expr)).collect()
    }
    fn reference(&self, root: &str, path: &[String]) -> EvalValue {
        match self.context.get(root) {
            Some(EvalValue::Known(value)) => path_value(value, path),
            Some(value) => value.clone(),
            None => EvalValue::Missing,
        }
    }
    fn charge_node(&mut self) -> Result<(), ModelError> {
        if self.depth > self.limits.max_depth || *self.remaining == 0 {
            return Err(ModelError::Expression(
                "expression depth/element limit".into(),
            ));
        }
        *self.remaining -= 1;
        Ok(())
    }
    fn evaluate(&mut self, expr: &Expr) -> Evaluation {
        self.charge_node()?;
        match expr {
            Expr::Literal { value } => {
                canonical_bytes(value)?;
                known(value.clone())
            }
            Expr::Ref { root, path } => Ok(self.reference(root, path)),
            Expr::Object { fields } => self.object(fields),
            Expr::Array { items } => self.array(items),
            Expr::Exists { value } => Ok(exists(self.child(value)?)),
            Expr::Not { value } => negate(self.child(value)?),
            Expr::And { values } => self.boolean(values, true),
            Expr::Or { values } => self.boolean(values, false),
            Expr::Eq { left, right } => self.compare(left, right, |a, b| Ok(a == b)),
            Expr::Lt { left, right } => self.compare(left, right, less_than),
            Expr::Contains { collection, value } => self.compare(collection, value, contains),
            Expr::Map {
                collection,
                item,
                value,
                max_items,
            } => self.transform(
                collection,
                Transform::Map(value),
                CollectionBinding {
                    item,
                    max_items: *max_items,
                },
            ),
            Expr::Filter {
                collection,
                item,
                predicate,
                max_items,
            } => self.transform(
                collection,
                Transform::Filter(predicate),
                CollectionBinding {
                    item,
                    max_items: *max_items,
                },
            ),
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
            Expr::Let { bindings, body } => self.let_bindings(bindings, body),
        }
    }

    fn object(&mut self, fields: &BTreeMap<String, Expr>) -> Evaluation {
        let mut o = serde_json::Map::new();
        let mut pending = vec![];
        for (k, e) in fields {
            match self.child(e)? {
                EvalValue::Known(v) => {
                    o.insert(k.clone(), v);
                }
                v => pending.push(v),
            }
        }
        if let Some(v) = unresolved(&pending) {
            Ok(v)
        } else {
            known(Value::Object(o))
        }
    }
    fn array(&mut self, items: &[Expr]) -> Evaluation {
        let values = self.values(items)?;
        if let Some(v) = unresolved(&values) {
            Ok(v)
        } else {
            known(Value::Array(
                values
                    .into_iter()
                    .map(|x| match x {
                        EvalValue::Known(v) => v,
                        _ => unreachable!(),
                    })
                    .collect(),
            ))
        }
    }
    fn boolean(&mut self, values: &[Expr], and: bool) -> Evaluation {
        let values = self.values(values)?;
        // Conservative evidence semantics: unresolved required evidence never disappears
        // through short circuiting (false && unknown remains unknown).
        if let Some(v) = unresolved(&values) {
            return Ok(v);
        }
        let mut result = and;
        for v in values {
            let EvalValue::Known(Value::Bool(b)) = v else {
                return Err(ModelError::Expression(
                    "boolean expression requires boolean".into(),
                ));
            };
            if and { result &= b } else { result |= b }
        }
        known(Value::Bool(result))
    }
    fn compare(
        &mut self,
        left: &Expr,
        right: &Expr,
        test: impl FnOnce(&Value, &Value) -> Result<bool, ModelError>,
    ) -> Evaluation {
        let values = [self.child(left)?, self.child(right)?];
        if let Some(value) = unresolved(&values) {
            return Ok(value);
        }
        let [EvalValue::Known(a), EvalValue::Known(b)] = &values else {
            unreachable!()
        };
        known(Value::Bool(test(a, b)?))
    }

    fn transform(
        &mut self,
        collection: &Expr,
        transform: Transform<'_>,
        binding: CollectionBinding<'_>,
    ) -> Evaluation {
        let source = self.child(collection)?;
        let EvalValue::Known(source) = source else {
            return Ok(source);
        };
        let source = source
            .as_array()
            .ok_or_else(|| ModelError::Expression("collection operation requires array".into()))?;
        if !binding.accepts(source.len(), self.limits.max_elements) {
            return Err(ModelError::Expression("collection bound exceeded".into()));
        }
        if self.context.contains_key(binding.item) {
            return Err(ModelError::Expression(
                "lexical binding shadows existing root".into(),
            ));
        }
        let mut result = Vec::new();
        let mut missing = false;
        for original in source {
            let mut local = self.context.clone();
            local.insert(binding.item.to_owned(), EvalValue::Known(original.clone()));
            let mapped = self.child_in(transform.expression(), &local)?;
            match mapped {
                EvalValue::Known(mapped) => transform.append(&mut result, original, mapped)?,
                EvalValue::Missing => missing = true,
                unknown @ EvalValue::Unknown(_) => return Ok(unknown),
            }
        }
        if missing {
            return Ok(EvalValue::Missing);
        }
        known(Value::Array(result))
    }
    fn difference(
        &mut self,
        pair: [&Expr; 2],
        identity: &[String],
        max_items: usize,
    ) -> Evaluation {
        let values = [self.child(pair[0])?, self.child(pair[1])?];
        if let Some(value) = unresolved(&values) {
            return Ok(value);
        }
        let [EvalValue::Known(a), EvalValue::Known(b)] = &values else {
            unreachable!()
        };
        let a = a
            .as_array()
            .ok_or_else(|| ModelError::Expression("difference requires arrays".into()))?;
        let b = b
            .as_array()
            .ok_or_else(|| ModelError::Expression("difference requires arrays".into()))?;
        let bounded = max_items > 0 && a.len().max(b.len()) <= max_items;
        let within_budget = a.len() + b.len() <= *self.remaining;
        let valid_identity = !identity.is_empty() && bounded;
        if !(valid_identity && within_budget) {
            return Err(ModelError::Expression(
                "difference identity/element bound".into(),
            ));
        }
        *self.remaining -= a.len() + b.len();
        difference_values(a, b, identity)
    }

    fn lookup(&mut self, key: &Expr, cases: &[LookupCase], default: &Expr) -> Evaluation {
        // Every case costs one element, evaluated or not.
        if cases.len() > MAX_LOOKUP_CASES || cases.len() > *self.remaining {
            return Err(ModelError::Expression("lookup case/element bound".into()));
        }
        *self.remaining -= cases.len();
        // An Unknown key is the result (conservative evidence, as for eq). A
        // Missing key, such as an absent optional fact, takes the default.
        let chosen = match self.child(key)? {
            EvalValue::Known(key) => cases
                .iter()
                .find(|c| c.when == key)
                .map_or(default, |c| &c.then),
            EvalValue::Missing => default,
            unknown @ EvalValue::Unknown(_) => return Ok(unknown),
        };
        self.child(chosen)
    }
    fn let_bindings(&mut self, bindings: &[LetBinding], body: &Expr) -> Evaluation {
        if bindings.len() > MAX_LET_BINDINGS {
            return Err(ModelError::Expression("let binding bound".into()));
        }
        let mut local = self.context.clone();
        let mut unknown = None;
        for b in bindings {
            if local.contains_key(&b.name) {
                return Err(ModelError::Expression(
                    "lexical binding shadows existing root".into(),
                ));
            }
            let value = self.child_in(&b.value, &local)?;
            // Unknown is never dropped, used or not (as and(false, unknown)).
            // Missing is bound as an observation that `exists` can test.
            if unknown.is_none() && matches!(value, EvalValue::Unknown(_)) {
                unknown = Some(value.clone());
            }
            local.insert(b.name.clone(), value);
        }
        let result = self.child_in(body, &local)?;
        Ok(unknown.unwrap_or(result))
    }
}

fn known(value: Value) -> Evaluation {
    Ok(EvalValue::Known(value))
}
struct CollectionBinding<'a> {
    item: &'a str,
    max_items: usize,
}
impl CollectionBinding<'_> {
    fn accepts(&self, count: usize, element_limit: usize) -> bool {
        self.max_items > 0 && count <= self.max_items.min(element_limit)
    }
}
enum Transform<'a> {
    Map(&'a Expr),
    Filter(&'a Expr),
}
impl<'a> Transform<'a> {
    fn expression(&self) -> &'a Expr {
        match self {
            Self::Map(expr) | Self::Filter(expr) => expr,
        }
    }
    fn append(
        &self,
        result: &mut Vec<Value>,
        original: &Value,
        mapped: Value,
    ) -> Result<(), ModelError> {
        match self {
            Self::Map(_) => result.push(mapped),
            Self::Filter(_) => {
                let keep = mapped.as_bool().ok_or_else(|| {
                    ModelError::Expression("filter requires boolean predicate".into())
                })?;
                if keep {
                    result.push(original.clone());
                }
            }
        }
        Ok(())
    }
}
fn difference_values(left: &[Value], right: &[Value], identity: &[String]) -> Evaluation {
    let mut excluded = BTreeSet::new();
    for value in right {
        unique_key(value, identity, &mut excluded)?;
    }
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for value in left {
        if !excluded.contains(&unique_key(value, identity, &mut seen)?) {
            result.push(value.clone());
        }
    }
    known(Value::Array(result))
}
fn unique_key(
    value: &Value,
    identity: &[String],
    seen: &mut BTreeSet<Vec<u8>>,
) -> Result<Vec<u8>, ModelError> {
    let EvalValue::Known(key) = path_value(value, identity) else {
        return Err(ModelError::Expression("missing difference identity".into()));
    };
    let key = canonical_bytes(&key)?;
    if !seen.insert(key.clone()) {
        return Err(ModelError::Expression(
            "duplicate difference identity".into(),
        ));
    }
    Ok(key)
}
fn less_than(a: &Value, b: &Value) -> Result<bool, ModelError> {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => Ok(number_i128(a)? < number_i128(b)?),
        (Value::String(a), Value::String(b)) => Ok(a < b),
        _ => Err(ModelError::Expression(
            "lt requires matching integers or strings".into(),
        )),
    }
}
fn contains(a: &Value, b: &Value) -> Result<bool, ModelError> {
    Ok(a.as_array()
        .ok_or_else(|| ModelError::Expression("contains requires array".into()))?
        .contains(b))
}
fn number_i128(n: &serde_json::Number) -> Result<i128, ModelError> {
    n.as_i64()
        .map(i128::from)
        .or_else(|| n.as_u64().map(i128::from))
        .ok_or_else(|| ModelError::Expression("noninteger number".into()))
}

fn exists(value: EvalValue) -> EvalValue {
    match value {
        EvalValue::Missing => EvalValue::Known(Value::Bool(false)),
        EvalValue::Known(_) => EvalValue::Known(Value::Bool(true)),
        unresolved => unresolved,
    }
}
fn negate(value: EvalValue) -> Evaluation {
    match value {
        EvalValue::Known(Value::Bool(b)) => known(Value::Bool(!b)),
        EvalValue::Known(_) => Err(ModelError::Expression("not requires boolean".into())),
        unresolved => Ok(unresolved),
    }
}
