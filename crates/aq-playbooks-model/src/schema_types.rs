//! Schema inference for literal values, merged alternatives and field access.
use super::{Schema, canonical_bytes};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The item type of an empty array: no value has it, so it merges into any
/// type and is assignable to any (an empty union). Only inference produces it;
/// it never appears in a contract or an artifact.
pub(super) fn no_type() -> Schema {
    Schema::Union { variants: vec![] }
}
pub(super) fn is_no_type(schema: &Schema) -> bool {
    matches!(schema, Schema::Union { variants } if variants.is_empty())
}
/// A boolean position also accepts a value of no type (an item of `[]`).
pub(super) fn is_boolean(schema: &Schema) -> bool {
    *schema == Schema::Boolean || is_no_type(schema)
}
pub(super) fn literal_schema(value: &Value) -> Result<Schema, String> {
    match value {
        Value::Null => Ok(Schema::Null),
        Value::Bool(_) => Ok(Schema::Boolean),
        Value::Number(n) => n
            .as_i64()
            .map(|n| Schema::Integer {
                minimum: n,
                maximum: n,
            })
            .ok_or_else(|| "integer outside i64 domain".into()),
        Value::String(s) => Ok(Schema::String {
            max_length: s.len().max(1),
            values: BTreeSet::from([s.clone()]),
        }),
        Value::Array(a) => {
            let mut item = None;
            for v in a {
                let s = literal_schema(v)?;
                item = Some(match item {
                    None => s,
                    Some(old) => {
                        merge_schema(&old, &s).ok_or("heterogeneous arrays unsupported")?
                    }
                });
            }
            Ok(Schema::Array {
                items: Box::new(item.unwrap_or_else(no_type)),
                max_items: a.len().max(1),
            })
        }
        Value::Object(o) => Ok(Schema::object(
            o.iter()
                .map(|(k, v)| Ok((k.clone(), literal_schema(v)?)))
                .collect::<Result<_, String>>()?,
        )),
    }
}
pub(super) fn merge_schema(a: &Schema, b: &Schema) -> Option<Schema> {
    if a == b || is_no_type(b) {
        return Some(a.clone());
    }
    if is_no_type(a) {
        return Some(b.clone());
    }
    match (a, b) {
        (Schema::Null, b @ Schema::Nullable { .. })
        | (b @ Schema::Nullable { .. }, Schema::Null) => Some(b.clone()),
        (Schema::Null, other) | (other, Schema::Null) => Some(Schema::Nullable {
            inner: Box::new(other.clone()),
        }),
        (Schema::Nullable { inner: a }, Schema::Nullable { inner: b }) => Some(Schema::Nullable {
            inner: Box::new(merge_schema(a, b)?),
        }),
        (Schema::Nullable { inner: a }, b) | (b, Schema::Nullable { inner: a }) => {
            Some(Schema::Nullable {
                inner: Box::new(merge_schema(a, b)?),
            })
        }
        _ => merge_non_nullable(a, b),
    }
}

fn merge_non_nullable(a: &Schema, b: &Schema) -> Option<Schema> {
    match (a, b) {
        (
            Schema::Integer {
                minimum: a,
                maximum: b,
            },
            Schema::Integer {
                minimum: c,
                maximum: d,
            },
        ) => Some(Schema::Integer {
            minimum: (*a).min(*c),
            maximum: (*b).max(*d),
        }),
        (
            Schema::String {
                max_length: a,
                values: x,
            },
            Schema::String {
                max_length: b,
                values: y,
            },
        ) => Some(Schema::String {
            max_length: (*a).max(*b),
            values: if x.is_empty() || y.is_empty() {
                BTreeSet::new()
            } else {
                x.union(y).cloned().collect()
            },
        }),
        (
            Schema::Array {
                items: a,
                max_items: x,
            },
            Schema::Array {
                items: b,
                max_items: y,
            },
        ) => Some(Schema::Array {
            items: Box::new(merge_schema(a, b)?),
            max_items: (*x).max(*y),
        }),
        (
            Schema::Object {
                properties: a,
                required: x,
            },
            Schema::Object {
                properties: b,
                required: y,
            },
        ) => Some(Schema::Object {
            properties: merge_properties(a, b)?,
            required: x.intersection(y).cloned().collect(),
        }),
        _ => bounded_union(a, b),
    }
}
pub(super) fn union_schemas<'a>(schemas: impl Iterator<Item = &'a Schema>) -> Option<Schema> {
    let mut result = None;
    for s in schemas {
        result = Some(match result {
            None => s.clone(),
            Some(old) => merge_schema(&old, s)?,
        });
    }
    result
}
pub(super) fn schema_path(schema: &Schema, path: &[String]) -> Result<Schema, String> {
    let Some((key, rest)) = path.split_first() else {
        return Ok(schema.clone());
    };
    match schema {
        Schema::Object { properties, .. } => schema_path(
            properties
                .get(key)
                .ok_or_else(|| format!("unknown field {key}"))?,
            rest,
        ),
        Schema::Array { items, .. } => {
            key.parse::<usize>()
                .map_err(|_| "array index must be nonnegative integer".to_string())?;
            schema_path(items, rest)
        }
        Schema::Union { variants } if variants.is_empty() => Ok(no_type()),
        Schema::Union { variants } => {
            let schemas = variants
                .iter()
                .map(|schema| schema_path(schema, path))
                .collect::<Result<Vec<_>, _>>()?;
            union_schemas(schemas.iter())
                .ok_or_else(|| "union projection exceeds alternative limit".into())
        }
        _ => Err(format!(
            "cannot traverse scalar/nullable reference at {key}"
        )),
    }
}

fn merge_properties(
    a: &BTreeMap<String, Schema>,
    b: &BTreeMap<String, Schema>,
) -> Option<BTreeMap<String, Schema>> {
    let mut properties = a.clone();
    for (k, v) in b {
        if let Some(old) = properties.get(k) {
            properties.insert(k.clone(), merge_schema(old, v)?);
        } else {
            properties.insert(k.clone(), v.clone());
        }
    }
    Some(properties)
}

fn bounded_union(a: &Schema, b: &Schema) -> Option<Schema> {
    let mut variants = vec![];
    for schema in [a, b] {
        match schema {
            Schema::Union { variants: inner } => variants.extend(inner.iter().cloned()),
            other => variants.push(other.clone()),
        }
    }
    variants.sort_by_key(|schema| canonical_bytes(schema).unwrap_or_default());
    variants.dedup();
    if variants.len() > 16 {
        None
    } else {
        Some(Schema::Union { variants })
    }
}
