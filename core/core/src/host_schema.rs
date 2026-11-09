//! Engine-independent JSON schema helper for the host boundary.
//!
//! This module carries no guest identity, paths, or user values in its error
//! type: [`SchemaError`] exposes only a static reason, so callers may reject or
//! log without leaking sensitive data. A schema is a *value constraint* only;
//! it can never authorize a guest identity or privilege.

use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Recursive traversal budgets applied while validating a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaLimits {
    pub max_depth: usize,
    pub max_nodes: usize,
    pub max_string_bytes: usize,
    pub max_properties: usize,
    pub max_array_items: usize,
}

impl Default for SchemaLimits {
    fn default() -> Self {
        Self {
            max_depth: 32,
            max_nodes: 16_384,
            max_string_bytes: 1024 * 1024,
            max_properties: 1024,
            max_array_items: 4096,
        }
    }
}

/// Validation failure. Only a static reason; never user data or raw paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaError {
    reason: &'static str,
}

impl SchemaError {
    pub const fn new(reason: &'static str) -> Self {
        Self { reason }
    }
    pub const fn reason(&self) -> &'static str {
        self.reason
    }
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason)
    }
}

impl std::error::Error for SchemaError {}

/// Minimal JSON schema used to validate host-provided values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonSchema {
    Any,
    Null,
    Boolean,
    Integer,
    Number,
    String {
        max_bytes: usize,
    },
    Array {
        items: Box<JsonSchema>,
        max_items: usize,
    },
    Object {
        properties: BTreeMap<String, JsonSchema>,
        required: BTreeSet<String>,
        additional_properties: bool,
    },
}

struct Budget {
    nodes: usize,
    limits: SchemaLimits,
}

impl Budget {
    fn charge(&mut self) -> Result<(), SchemaError> {
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or(SchemaError::new("node budget exhausted"))?;
        if self.nodes > self.limits.max_nodes {
            return Err(SchemaError::new("node budget exhausted"));
        }
        Ok(())
    }
}

impl JsonSchema {
    /// Validate `value` against this schema under `limits`. Depth and node
    /// budgets apply to every variant, including [`JsonSchema::Any`].
    pub fn validate(&self, value: &Value, limits: SchemaLimits) -> Result<(), SchemaError> {
        self.check(
            value,
            0,
            &mut Budget {
                nodes: 0,
                limits: SchemaLimits {
                    max_depth: limits.max_depth.min(128),
                    ..limits
                },
            },
        )
    }

    fn check(&self, value: &Value, depth: usize, budget: &mut Budget) -> Result<(), SchemaError> {
        if depth > budget.limits.max_depth {
            return Err(SchemaError::new("depth budget exhausted"));
        }
        budget.charge()?;
        if let Value::String(text) = value {
            if text.len() > budget.limits.max_string_bytes {
                return Err(SchemaError::new("string budget exhausted"));
            }
        }
        if let Value::Object(map) = value {
            if map
                .keys()
                .any(|key| key.len() > budget.limits.max_string_bytes)
            {
                return Err(SchemaError::new("property name budget exhausted"));
            }
        }

        match self {
            JsonSchema::Any => match value {
                Value::Array(items) => {
                    if items.len() > budget.limits.max_array_items {
                        return Err(SchemaError::new("array budget exhausted"));
                    }
                    for item in items {
                        JsonSchema::Any.check(item, depth + 1, budget)?;
                    }
                }
                Value::Object(map) => {
                    if map.len() > budget.limits.max_properties {
                        return Err(SchemaError::new("property budget exhausted"));
                    }
                    for v in map.values() {
                        JsonSchema::Any.check(v, depth + 1, budget)?;
                    }
                }
                _ => {}
            },
            JsonSchema::Null => {
                if !value.is_null() {
                    return Err(SchemaError::new("expected null"));
                }
            }
            JsonSchema::Boolean => {
                if !value.is_boolean() {
                    return Err(SchemaError::new("expected boolean"));
                }
            }
            JsonSchema::Integer => {
                let ok = matches!(value, Value::Number(n) if n.is_i64() || n.is_u64());
                if !ok {
                    return Err(SchemaError::new("expected integer"));
                }
            }
            JsonSchema::Number => {
                if !value.is_number() {
                    return Err(SchemaError::new("expected number"));
                }
            }
            JsonSchema::String { max_bytes } => match value {
                Value::String(s) => {
                    if s.len() > *max_bytes {
                        return Err(SchemaError::new("string exceeds max_bytes"));
                    }
                    if s.len() > budget.limits.max_string_bytes {
                        return Err(SchemaError::new("string budget exhausted"));
                    }
                }
                _ => return Err(SchemaError::new("expected string")),
            },
            JsonSchema::Array { items, max_items } => match value {
                Value::Array(arr) => {
                    if arr.len() > *max_items || arr.len() > budget.limits.max_array_items {
                        return Err(SchemaError::new("array budget exhausted"));
                    }
                    for item in arr {
                        items.check(item, depth + 1, budget)?;
                    }
                }
                _ => return Err(SchemaError::new("expected array")),
            },
            JsonSchema::Object {
                properties,
                required,
                additional_properties,
            } => match value {
                Value::Object(map) => {
                    if map.len() > budget.limits.max_properties {
                        return Err(SchemaError::new("property budget exhausted"));
                    }
                    for key in required {
                        if !map.contains_key(key) {
                            return Err(SchemaError::new("missing required property"));
                        }
                    }
                    for (key, val) in map {
                        match properties.get(key) {
                            Some(schema) => schema.check(val, depth + 1, budget)?,
                            None if !*additional_properties => {
                                return Err(SchemaError::new("additional property not allowed"));
                            }
                            None => JsonSchema::Any.check(val, depth + 1, budget)?,
                        }
                    }
                }
                _ => return Err(SchemaError::new("expected object")),
            },
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(props: Vec<(&str, JsonSchema)>, required: &[&str], additional: bool) -> JsonSchema {
        JsonSchema::Object {
            properties: props.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            required: required.iter().map(|k| k.to_string()).collect(),
            additional_properties: additional,
        }
    }

    fn lim() -> SchemaLimits {
        SchemaLimits::default()
    }
    fn reason(r: Result<(), SchemaError>) -> &'static str {
        r.unwrap_err().reason()
    }

    #[test]
    fn required_extra_and_wrong_type() {
        let s = obj(vec![("a", JsonSchema::Integer)], &["a"], false);
        assert!(s.validate(&json!({"a": 1}), lim()).is_ok());
        assert_eq!(
            reason(s.validate(&json!({}), lim())),
            "missing required property"
        );
        assert_eq!(
            reason(s.validate(&json!({"a": 1, "b": 2}), lim())),
            "additional property not allowed"
        );
        assert_eq!(
            reason(s.validate(&json!({"a": "x"}), lim())),
            "expected integer"
        );
    }

    #[test]
    fn null_and_object_variants() {
        assert!(JsonSchema::Null.validate(&json!(null), lim()).is_ok());
        assert_eq!(
            reason(JsonSchema::Null.validate(&json!(1), lim())),
            "expected null"
        );
        assert!(JsonSchema::Boolean.validate(&json!(true), lim()).is_ok());
        assert_eq!(
            reason(JsonSchema::Boolean.validate(&json!(1), lim())),
            "expected boolean"
        );
        assert!(obj(vec![], &[], true)
            .validate(&json!({"any": [1, 2]}), lim())
            .is_ok());
    }

    #[test]
    fn integer_accepts_i64_and_u64_only() {
        assert!(JsonSchema::Integer.validate(&json!(-5), lim()).is_ok());
        assert!(JsonSchema::Integer
            .validate(&json!(u64::MAX), lim())
            .is_ok());
        assert!(JsonSchema::Integer.validate(&json!(1.5), lim()).is_err());
        assert!(JsonSchema::Integer.validate(&json!("1"), lim()).is_err());
        assert!(JsonSchema::Number.validate(&json!(1.5), lim()).is_ok());
    }

    #[test]
    fn bounded_string_bytes() {
        let s = JsonSchema::String { max_bytes: 4 };
        assert!(s.validate(&json!("abcd"), lim()).is_ok());
        assert_eq!(
            reason(s.validate(&json!("abcde"), lim())),
            "string exceeds max_bytes"
        );
        let mut l = lim();
        l.max_string_bytes = 2;
        assert_eq!(
            reason(JsonSchema::String { max_bytes: 10 }.validate(&json!("abc"), l)),
            "string budget exhausted"
        );
    }

    #[test]
    fn bounded_nodes() {
        let mut l = lim();
        l.max_nodes = 3;
        let s = JsonSchema::Array {
            items: Box::new(JsonSchema::Any),
            max_items: 100,
        };
        assert!(s.validate(&json!([1, 2]), l).is_ok());
        assert_eq!(
            reason(s.validate(&json!([1, 2, 3]), l)),
            "node budget exhausted"
        );
    }

    #[test]
    fn bounded_depth() {
        let mut l = lim();
        l.max_depth = 2;
        let s = JsonSchema::Array {
            items: Box::new(JsonSchema::Array {
                items: Box::new(JsonSchema::Any),
                max_items: 8,
            }),
            max_items: 8,
        };
        assert!(s.validate(&json!([[null]]), l).is_ok());
        assert_eq!(
            reason(s.validate(&json!([[[null]]]), l)),
            "depth budget exhausted"
        );
    }

    #[test]
    fn bounded_array_and_any_recursive_budget() {
        let s = JsonSchema::Array {
            items: Box::new(JsonSchema::Integer),
            max_items: 2,
        };
        assert_eq!(
            reason(s.validate(&json!([1, 2, 3]), lim())),
            "array budget exhausted"
        );
        let mut l = lim();
        l.max_nodes = 4;
        assert!(JsonSchema::Any
            .validate(&json!({"a": {"b": {"c": {"d": 1}}}}), l)
            .is_err());
        assert!(JsonSchema::Any.validate(&json!({"a": [1, 2]}), l).is_ok());
        l.max_depth = 1;
        assert_eq!(
            reason(JsonSchema::Any.validate(&json!({"a": {"b": 1}}), l)),
            "depth budget exhausted"
        );
    }

    #[test]
    fn valid_nested_object() {
        let s = obj(
            vec![
                ("name", JsonSchema::String { max_bytes: 32 }),
                (
                    "tags",
                    JsonSchema::Array {
                        items: Box::new(JsonSchema::String { max_bytes: 8 }),
                        max_items: 4,
                    },
                ),
                (
                    "meta",
                    obj(vec![("version", JsonSchema::Integer)], &["version"], false),
                ),
            ],
            &["name", "meta"],
            false,
        );
        let v = json!({"name": "svc", "tags": ["a", "b"], "meta": {"version": 3}});
        assert!(s.validate(&v, lim()).is_ok());
    }
    #[test]
    fn permissive_schemas_still_reject_unbounded_additional_values() {
        let limits = SchemaLimits {
            max_nodes: 3,
            max_string_bytes: 4,
            ..Default::default()
        };
        assert!(JsonSchema::Any.validate(&json!("12345"), limits).is_err());
        let additional = obj(vec![], &[], true);
        assert!(additional.validate(&json!({"a":[1,2,3]}), limits).is_err());
        assert!(additional.validate(&json!({"long-key":1}), limits).is_err());
    }
}
