//! Bounded JSON schema definitions, value checks and assignability.
use crate::ModelError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Schema {
    Null,
    Boolean,
    Integer {
        minimum: i64,
        maximum: i64,
    },
    String {
        max_length: usize,
        #[serde(default)]
        values: BTreeSet<String>,
    },
    Array {
        items: Box<Schema>,
        max_items: usize,
    },
    Object {
        properties: BTreeMap<String, Schema>,
        #[serde(default)]
        required: BTreeSet<String>,
    },
    Nullable {
        inner: Box<Schema>,
    },
    /// A bounded explicit union; a value must satisfy at least one alternative.
    Union {
        variants: Vec<Schema>,
    },
}
impl Schema {
    pub fn string(max_length: usize) -> Self {
        Self::String {
            max_length,
            values: BTreeSet::new(),
        }
    }
    pub fn object(properties: BTreeMap<String, Schema>) -> Self {
        let required = properties.keys().cloned().collect();
        Self::Object {
            properties,
            required,
        }
    }
    pub fn validate(&self, value: &Value) -> Result<(), ModelError> {
        self.check_value(value, "$", 0)
    }
    fn check_value(&self, v: &Value, path: &str, depth: usize) -> Result<(), ModelError> {
        let at = ValueAt {
            value: v,
            path,
            depth,
        };
        if depth > 32 {
            return Err(ModelError::Invalid(format!(
                "{path}: schema nesting exceeds 32"
            )));
        }
        let valid = match self {
            Self::Null => v.is_null(),
            Self::Boolean => v.is_boolean(),
            Self::Integer { minimum, maximum } => v
                .as_i64()
                .is_some_and(|n| (*minimum..=*maximum).contains(&n)),
            Self::String { max_length, values } => v
                .as_str()
                .is_some_and(|s| bounded_string(s, *max_length, values)),
            Self::Array { items, max_items } => at.check_array(items, *max_items)?,
            Self::Object {
                properties,
                required,
            } => at.check_object(properties, required)?,
            Self::Union { variants } => variants
                .iter()
                .any(|schema| at.check_nested(schema).is_ok()),
            Self::Nullable { inner } => {
                if !v.is_null() {
                    at.check_nested(inner)?;
                }
                true
            }
        };
        if valid {
            Ok(())
        } else {
            Err(ModelError::Invalid(format!(
                "{path}: value violates bounded schema"
            )))
        }
    }
    pub fn validate_definition(&self) -> Result<(), ModelError> {
        self.check_definition(0)
    }
    fn check_definition(&self, depth: usize) -> Result<(), ModelError> {
        if depth > 32 {
            return Err(ModelError::Invalid("schema nesting exceeds 32".into()));
        }
        match self {
            Self::Integer { minimum, maximum } if minimum > maximum => {
                return Err(ModelError::Invalid("inverted integer bounds".into()));
            }
            Self::String { max_length, values } => {
                if !valid_string_definition(*max_length, values) {
                    return Err(ModelError::Invalid("invalid string bound".into()));
                }
            }
            Self::Array { items, max_items } => {
                if !(1..=100_000).contains(max_items) {
                    return Err(ModelError::Invalid("invalid array bound".into()));
                }
                items.check_definition(depth + 1)?;
            }
            Self::Object {
                properties,
                required,
            } => {
                check_object_definition(properties, required, depth)?;
            }
            Self::Union { variants } => {
                if !(2..=16).contains(&variants.len()) {
                    return Err(ModelError::Invalid(
                        "union requires 2..16 alternatives".into(),
                    ));
                }
                for schema in variants {
                    schema.check_definition(depth + 1)?;
                }
            }
            Self::Nullable { inner } => inner.check_definition(depth + 1)?,
            _ => {}
        }
        Ok(())
    }
    /// Every value admitted by `self` must be admitted by `target`.
    pub fn assignable_to(&self, target: &Schema) -> bool {
        match (self, target) {
            (Self::Nullable { inner }, target @ Self::Union { .. }) => {
                Self::Null.assignable_to(target) && inner.assignable_to(target)
            }
            (Self::Union { variants }, target) => {
                variants.iter().all(|schema| schema.assignable_to(target))
            }
            (source, Self::Union { variants }) => {
                variants.iter().any(|schema| source.assignable_to(schema))
            }
            (Self::Null, Self::Nullable { .. }) => true,
            (Self::Nullable { inner: a }, Self::Nullable { inner: b }) => a.assignable_to(b),
            (a, Self::Nullable { inner: b }) => a.assignable_to(b),
            (
                Self::Integer {
                    minimum: a,
                    maximum: b,
                },
                Self::Integer {
                    minimum: c,
                    maximum: d,
                },
            ) => a >= c && b <= d,
            (
                Self::String {
                    max_length: a,
                    values: x,
                },
                Self::String {
                    max_length: b,
                    values: y,
                },
            ) => a <= b && allowed_strings_assignable(x, y),
            (
                Self::Array {
                    items: a,
                    max_items: x,
                },
                Self::Array {
                    items: b,
                    max_items: y,
                },
            ) => x <= y && a.assignable_to(b),
            (
                Self::Object {
                    properties: a,
                    required: ar,
                },
                Self::Object {
                    properties: b,
                    required: br,
                },
            ) => {
                br.is_subset(ar)
                    && a.iter()
                        .all(|(k, v)| b.get(k).is_some_and(|s| v.assignable_to(s)))
            }
            _ => self == target,
        }
    }
}

struct ValueAt<'a> {
    value: &'a Value,
    path: &'a str,
    depth: usize,
}

impl ValueAt<'_> {
    fn check_nested(&self, schema: &Schema) -> Result<(), ModelError> {
        schema.check_value(self.value, self.path, self.depth + 1)
    }
    fn check_array(&self, items: &Schema, max_items: usize) -> Result<bool, ModelError> {
        let Some(values) = self.value.as_array().filter(|a| a.len() <= max_items) else {
            return Ok(false);
        };
        for (index, value) in values.iter().enumerate() {
            items.check_value(value, &format!("{}/{index}", self.path), self.depth + 1)?;
        }
        Ok(true)
    }
    fn check_object(
        &self,
        properties: &BTreeMap<String, Schema>,
        required: &BTreeSet<String>,
    ) -> Result<bool, ModelError> {
        let Some(object) = self.value.as_object() else {
            return Ok(false);
        };
        let has_required = required.iter().all(|k| object.contains_key(k));
        let known_keys = object.keys().all(|k| properties.contains_key(k));
        if !has_required || !known_keys {
            return Ok(false);
        }
        for (key, value) in object {
            properties[key].check_value(value, &format!("{}/{key}", self.path), self.depth + 1)?;
        }
        Ok(true)
    }
}

fn bounded_string(value: &str, length: usize, allowed: &BTreeSet<String>) -> bool {
    let allowed_value = allowed.is_empty() || allowed.contains(value);
    value.len() <= length && allowed_value
}

fn valid_string_definition(length: usize, values: &BTreeSet<String>) -> bool {
    let bounded = (1..=1_048_576).contains(&length) && values.len() <= 1024;
    bounded && values.iter().all(|s| s.len() <= length)
}

fn allowed_strings_assignable(source: &BTreeSet<String>, target: &BTreeSet<String>) -> bool {
    target.is_empty() || (!source.is_empty() && source.is_subset(target))
}

fn check_object_definition(
    properties: &BTreeMap<String, Schema>,
    required: &BTreeSet<String>,
    depth: usize,
) -> Result<(), ModelError> {
    if properties.len() > 1024 || required.iter().any(|k| !properties.contains_key(k)) {
        return Err(ModelError::Invalid(
            "invalid object properties/required".into(),
        ));
    }
    for s in properties.values() {
        s.check_definition(depth + 1)?;
    }
    Ok(())
}
