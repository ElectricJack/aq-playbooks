//! Evaluate playbook expressions against the inputs, variables and definitions of the current run.
use super::*;

pub(super) fn context(s: &State, r: &Run) -> EvalContext {
    let mut c = EvalContext::new();
    c.insert("input".into(), EvalValue::Known(r.input.clone()));
    c.insert("vars".into(), EvalValue::Known(json!(r.variables)));
    for (k, v) in &r.variables {
        c.insert(k.clone(), EvalValue::Known(v.clone()));
    }
    let definitions = &s.artifacts[&r.artifact].content.definitions;
    c.insert(
        "definitions".into(),
        EvalValue::Known(Value::Object(
            definitions
                .iter()
                .map(|(k, v)| (k.clone(), v.value.clone()))
                .collect(),
        )),
    );
    c
}

pub(super) fn known_expr(expr: &Expr, ctx: &EvalContext) -> Result<Value> {
    match eval(expr, ctx, &EvalLimits::default())? {
        EvalValue::Known(v) => Ok(v),
        EvalValue::Missing => Err(Error::Refused("expression input missing".into())),
        EvalValue::Unknown(r) => Err(Error::Refused(format!("expression unknown: {r}"))),
    }
}

pub(super) fn check_bytes(value: &Value, limit: usize) -> Result<()> {
    if canonical_bytes(value)?.len() > limit {
        return Err(Error::Refused("payload byte limit".into()));
    }
    Ok(())
}
