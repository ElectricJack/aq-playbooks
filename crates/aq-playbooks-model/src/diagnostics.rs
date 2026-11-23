//! Render schema and reference diagnostics for playbook authors.
use super::{Schema, is_no_type, schema_path};
use std::collections::{BTreeMap, BTreeSet};

/// A schema in words, for diagnostics an author fixes a draft from.
pub(super) fn describe(schema: &Schema) -> String {
    let text = match schema {
        Schema::Null => "null".to_string(),
        Schema::Boolean => "boolean".to_string(),
        Schema::Integer { minimum, maximum } if minimum == maximum => {
            format!("the integer {minimum}")
        }
        Schema::Integer {
            minimum: i64::MIN,
            maximum: i64::MAX,
        } => "integer".to_string(),
        Schema::Integer { minimum, maximum } => format!("integer {minimum}..{maximum}"),
        Schema::String { max_length, values } if values.is_empty() => {
            format!("string of at most {max_length} bytes")
        }
        Schema::String { values, .. } => describe_values(values),
        Schema::Array { items, .. } if is_no_type(items) => "empty array".to_string(),
        Schema::Array { items, max_items } => {
            format!("array of at most {max_items} ({})", describe(items))
        }
        Schema::Object {
            properties,
            required,
        } => format!(
            "object {{{}}}",
            properties
                .iter()
                .map(|(k, v)| {
                    let optional = if required.contains(k) { "" } else { "?" };
                    format!("{k}{optional}: {}", describe(v))
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Schema::Nullable { inner } => format!("null or {}", describe(inner)),
        Schema::Union { variants } if variants.is_empty() => "no value".to_string(),
        Schema::Union { variants } => format!(
            "one of ({})",
            variants
                .iter()
                .map(describe)
                .collect::<Vec<_>>()
                .join(" | ")
        ),
    };
    if text.len() <= 400 {
        return text;
    }
    let mut end = 397;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}
pub(super) fn dotted(root: &str, path: &[String]) -> String {
    std::iter::once(root)
        .chain(path.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(".")
}
/// Why `root.path` cannot be read, said at the deepest part that can: what is
/// there, and where the missing part would be readable.
pub(super) fn unreadable_ref(
    root: &str,
    path: &[String],
    schema: &Schema,
    error: String,
) -> String {
    let (at, parent, depth) = deepest_projection(schema, path);
    let Some(key) = path.get(depth) else {
        return format!("`{}` cannot be read: {error}", dotted(root, path));
    };
    let prefix = dotted(root, &path[..depth]);
    let fields = |properties: &BTreeMap<String, Schema>| {
        properties.keys().cloned().collect::<Vec<_>>().join(", ")
    };
    // A named join's answer for one member: {participant, outcome, output}.
    let join_answer = matches!(&parent, Some(Schema::Object { properties, .. })
        if properties.keys().map(String::as_str).eq(["outcome", "output", "participant"]));
    let why = match &at {
        Schema::Object { properties, .. } => {
            format!("{error}: `{prefix}` has fields {}", fields(properties))
        }
        Schema::Nullable { .. } if join_answer => format!(
            "`{prefix}` may be null at this step, so `{key}` cannot be read. A join's \
             answer (`outcome`, `output`) is present only on the join's `completed` edge, \
             in mode `all` or `any_accepted`: read it in a step reached through that edge"
        ),
        Schema::Nullable { .. } => format!(
            "`{prefix}` may be null at this step, so `{key}` cannot be read. Read it after \
             an edge on which it is not null, or test it first with `exists`"
        ),
        Schema::Union { variants } => union_read_error(variants, &prefix, key),
        Schema::Array { .. } => {
            format!("{error} (`{prefix}` is an array; `{key}` is not an index)")
        }
        other => format!(
            "`{prefix}` is {}, which has no field `{key}`",
            describe(other)
        ),
    };
    format!("`{}` cannot be read: {why}", dotted(root, path))
}

fn describe_values(values: &BTreeSet<String>) -> String {
    let mut shown: Vec<_> = values.iter().take(12).map(|v| format!("{v:?}")).collect();
    if values.len() > 12 {
        shown.push(format!("and {} more", values.len() - 12));
    }
    format!("string, one of {}", shown.join(", "))
}

fn union_read_error(variants: &[Schema], prefix: &str, key: &str) -> String {
    let objects: Vec<_> = variants
        .iter()
        .filter_map(|v| match v {
            Schema::Object { properties, .. } => Some(properties),
            _ => None,
        })
        .collect();
    let common: Vec<_> = objects
        .first()
        .map(|first| {
            first
                .keys()
                .filter(|k| objects.iter().all(|o| o.contains_key(*k)))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let some = if objects.iter().any(|o| o.contains_key(key)) {
        "only some"
    } else {
        "none"
    };
    format!(
        "`{prefix}` has a different shape per alternative and {some} of them have \
                 `{key}`; every alternative has {}. Read `{key}` after an edge that selects \
                 an alternative that has it, or test it first with `exists`",
        if common.is_empty() {
            "no common field".to_string()
        } else {
            format!("fields {}", common.join(", "))
        }
    )
}

fn deepest_projection(schema: &Schema, path: &[String]) -> (Schema, Option<Schema>, usize) {
    let mut at = schema.clone();
    let mut parent = None;
    let mut depth = 0;
    while let Some(key) = path.get(depth) {
        match schema_path(&at, std::slice::from_ref(key)) {
            Ok(next) => {
                parent = Some(std::mem::replace(&mut at, next));
                depth += 1;
            }
            Err(_) => break,
        }
    }
    (at, parent, depth)
}
