//! Helpers the `#[playbook]` and `#[vars]` macros expand to (vision 10.3).

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::{Error, Result};

/// JSON Schema for a vars struct, as `--describe` prints it.
pub fn schema_for<T: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}

/// For playbooks without vars.
pub fn no_schema() -> Value {
    Value::Null
}

/// Deserialize the merged vars sent in the `Start` frame. Missing-field
/// errors are reworded so they read as "missing required var".
pub fn from_value<T: DeserializeOwned>(raw: Value) -> Result<T> {
    let raw = if raw.is_null() {
        Value::Object(Default::default())
    } else {
        raw
    };
    serde_json::from_value(raw).map_err(|e| {
        let m = e.to_string();
        let m = m
            .strip_prefix("missing field ")
            .map(|f| format!("missing required var {f}"))
            .unwrap_or(m);
        Error::msg(format!("vars: {m}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize, schemars::JsonSchema, Debug, PartialEq)]
    struct V {
        user: String,
        port: Option<u16>,
    }

    #[test]
    fn missing_required_reads_well() {
        let e = from_value::<V>(serde_json::json!({})).unwrap_err();
        assert_eq!(e.chain(), "vars: missing required var `user`");
    }

    #[test]
    fn null_means_no_vars() {
        let v: V = from_value(serde_json::json!({"user": "cadu"})).unwrap();
        assert_eq!(
            v,
            V {
                user: "cadu".into(),
                port: None
            }
        );
    }

    #[test]
    fn schema_marks_required() {
        let s = schema_for::<V>();
        let req = s["required"].as_array().unwrap();
        assert_eq!(req, &[serde_json::json!("user")]);
    }
}
