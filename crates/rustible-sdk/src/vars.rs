//! Helpers the `#[playbook]` and `#[vars]` macros expand to (vision 10.3),
//! plus the shared `--var` parser.

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

/// Deserialize the merged vars sent in the `Start` frame. The schema is
/// checked first so *every* missing required var is reported at once (serde
/// alone stops at the first); serde then does the coercion.
pub fn from_value<T: DeserializeOwned + schemars::JsonSchema>(raw: Value) -> Result<T> {
    let raw = if raw.is_null() {
        Value::Object(Default::default())
    } else {
        raw
    };
    let schema = schema_for::<T>();
    validate(&schema, &raw)?;
    serde_json::from_value(raw).map_err(|e| Error::msg(format!("vars: {e}")))
}

/// Check a vars object against a playbook's schema: every required key
/// present, and the schema itself flat. All problems in one message.
pub fn validate(schema: &Value, raw: &Value) -> Result<()> {
    let mut problems = flatness_violations(schema);
    let obj = raw.as_object();
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for r in required.iter().filter_map(Value::as_str) {
            if obj.is_none_or(|o| !o.contains_key(r)) {
                problems.push(format!("missing required var `{r}`"));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Error::msg(format!("vars: {}", problems.join("; "))))
    }
}

/// Vars are one level deep (vision 10.3): a property whose schema is an
/// object cannot be filled from an inventory `vars` block. Returns one
/// message per offending field.
pub fn flatness_violations(schema: &Value) -> Vec<String> {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return vec![];
    };
    props
        .iter()
        .filter(|(_, p)| is_object_schema(schema, resolve(schema, p)))
        .map(|(name, _)| {
            format!("var `{name}` is an object; vars are flat scalars, lists, or enums")
        })
        .collect()
}

/// Follow a local `$ref` (`#/$defs/Name`) to its definition in the root schema.
fn resolve<'a>(root: &'a Value, p: &'a Value) -> &'a Value {
    let Some(r) = p.get("$ref").and_then(Value::as_str) else {
        return p;
    };
    r.strip_prefix("#/$defs/")
        .and_then(|name| root.get("$defs").and_then(|d| d.get(name)))
        .or_else(|| {
            r.strip_prefix("#/definitions/")
                .and_then(|name| root.get("definitions").and_then(|d| d.get(name)))
        })
        .unwrap_or(p)
}

fn is_object_schema(root: &Value, p: &Value) -> bool {
    let p = resolve(root, p);
    let ty_is_object = |v: &Value| match v {
        Value::String(s) => s == "object",
        Value::Array(a) => a.iter().any(|t| t == "object"),
        _ => false,
    };
    p.get("type").is_some_and(ty_is_object)
        || p.get("additionalProperties").is_some()
        || p.get("properties").is_some()
        || ["anyOf", "oneOf"].iter().any(|k| {
            p.get(k)
                .and_then(Value::as_array)
                .is_some_and(|alts| alts.iter().any(|a| is_object_schema(root, a)))
        })
}

/// Keys in `raw` that the schema does not declare. Not an error (the bag is
/// shared across playbooks), but worth a warning, especially when one is
/// within an edit or two of a declared name.
pub fn unknown_key_warnings(schema: &Value, raw: &Value) -> Vec<String> {
    let declared: Vec<&str> = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let Some(obj) = raw.as_object() else {
        return vec![];
    };
    obj.keys()
        .filter(|k| !declared.contains(&k.as_str()))
        .map(
            |k| match declared.iter().find(|d| edit_distance(k, d) <= 2) {
                Some(near) => {
                    format!("var `{k}` is not declared by this playbook; did you mean `{near}`?")
                }
                None => format!("var `{k}` is not declared by this playbook (ignored)"),
            },
        )
        .collect()
}

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Parse one `--var key=value`. A value that parses as JSON (number, bool,
/// list) is taken as such; anything else is a string. Shared by the local
/// runner and the CLI so both sides coerce identically.
pub fn parse_var(kv: &str) -> Result<(String, Value)> {
    let Some((k, v)) = kv.split_once('=') else {
        return Err(Error::msg(format!("--var needs key=value, got `{kv}`")));
    };
    let value = serde_json::from_str::<Value>(v).unwrap_or_else(|_| Value::String(v.to_string()));
    Ok((k.to_string(), value))
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

    #[derive(serde::Deserialize, schemars::JsonSchema, Debug)]
    struct Two {
        #[allow(dead_code)]
        user: String,
        #[allow(dead_code)]
        fruit: String,
    }

    #[test]
    fn all_missing_vars_reported_at_once() {
        let e = from_value::<Two>(serde_json::json!({})).unwrap_err();
        assert_eq!(
            e.chain(),
            "vars: missing required var `user`; missing required var `fruit`"
        );
    }

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct Nested {
        #[allow(dead_code)]
        inner: Inner,
    }
    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct Inner {
        #[allow(dead_code)]
        x: u8,
    }

    #[test]
    fn nested_struct_is_a_flatness_violation() {
        let v = flatness_violations(&schema_for::<Nested>());
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains("`inner` is an object"), "{v:?}");
    }

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    #[allow(dead_code)]
    enum Env {
        Staging,
        Production,
    }
    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct WithEnum {
        #[allow(dead_code)]
        env: Env,
        #[allow(dead_code)]
        ports: Vec<u16>,
    }

    #[test]
    fn enums_and_lists_are_flat() {
        assert!(flatness_violations(&schema_for::<WithEnum>()).is_empty());
    }

    #[test]
    fn unknown_keys_warn_with_suggestion() {
        let w = unknown_key_warnings(
            &schema_for::<V>(),
            &serde_json::json!({"user": "x", "usr": "y", "zzz": 1}),
        );
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].contains("did you mean `user`"), "{w:?}");
        assert!(w[1].contains("ignored"), "{w:?}");
    }

    #[test]
    fn parse_var_coerces_json_else_string() {
        assert_eq!(
            parse_var("port=22").unwrap(),
            ("port".into(), serde_json::json!(22))
        );
        assert_eq!(
            parse_var("name=mc").unwrap(),
            ("name".into(), serde_json::json!("mc"))
        );
        assert_eq!(
            parse_var("tags=[1,2]").unwrap(),
            ("tags".into(), serde_json::json!([1, 2]))
        );
        assert!(parse_var("novalue").is_err());
    }
}
