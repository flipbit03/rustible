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
    let raw = coerce_scalars_to_lists(&schema, raw);
    validate(&schema, &raw)?;
    serde_json::from_value(raw).map_err(|e| Error::msg(format!("vars: {e}")))
}

/// An inventory `vars` block cannot spell a one-element list (a single
/// positional value is a scalar), so where the schema wants a list and the
/// bag holds a scalar, wrap it. Applied on both sides (orchestrator pre-check
/// and `Start`) so they agree. Objects and existing arrays pass through.
pub fn coerce_scalars_to_lists(schema: &Value, raw: Value) -> Value {
    let Value::Object(mut obj) = raw else {
        return raw;
    };
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        for (name, p) in props {
            let wants_list = resolve(schema, p).get("type").is_some_and(|t| {
                t == "array" || t.as_array().is_some_and(|a| a.iter().any(|x| x == "array"))
            });
            if wants_list
                && let Some(v) = obj.get_mut(name)
                && !v.is_array()
                && !v.is_null()
            {
                let scalar = v.take();
                *v = Value::Array(vec![scalar]);
            }
        }
    }
    Value::Object(obj)
}

/// Check a vars object against a playbook's schema: every required key
/// present, and the schema itself flat. All problems in one message.
pub fn validate(schema: &Value, raw: &Value) -> Result<()> {
    let mut problems = flatness_violations(schema);
    problems.extend(
        missing_required(schema, raw)
            .iter()
            .map(|r| format!("missing required var `{r}`")),
    );
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
    non_flat_vars(schema)
        .into_iter()
        .map(|name| format!("var `{name}` is an object; vars are flat scalars, lists, or enums"))
        .collect()
}

/// The names of the properties `flatness_violations` complains about.
pub fn non_flat_vars(schema: &Value) -> Vec<String> {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return vec![];
    };
    props
        .iter()
        .filter(|(_, p)| is_object_schema(schema, resolve(schema, p)))
        .map(|(name, _)| name.clone())
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
    unknown_keys(schema, raw)
        .iter()
        .map(ToString::to_string)
        .collect()
}

/// A key the playbook does not declare, with the nearest declared name if
/// one is close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownKey {
    pub key: String,
    pub suggestion: Option<String>,
}

impl std::fmt::Display for UnknownKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.suggestion {
            Some(near) => write!(
                f,
                "var `{}` is not declared by this playbook; did you mean `{near}`?",
                self.key
            ),
            None => write!(
                f,
                "var `{}` is not declared by this playbook (ignored)",
                self.key
            ),
        }
    }
}

/// Keys in `raw` that the schema does not declare, structured.
pub fn unknown_keys(schema: &Value, raw: &Value) -> Vec<UnknownKey> {
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
        .map(|k| UnknownKey {
            key: k.clone(),
            suggestion: did_you_mean(k, declared.iter().copied()).map(str::to_string),
        })
        .collect()
}

/// Names the schema marks `required` that `raw` does not carry. `raw` that
/// is not an object counts as empty.
pub fn missing_required(schema: &Value, raw: &Value) -> Vec<String> {
    let obj = raw.as_object();
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|r| obj.is_none_or(|o| !o.contains_key(*r)))
        .map(str::to_string)
        .collect()
}

/// A var whose value does not fit the type its schema declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeMismatch {
    pub var: String,
    /// What the schema asks for, in words: `integer`, `list of string`,
    /// `one of "sudo" | "doas"`, `string or null`.
    pub expected: String,
    /// What the inventory gave, rendered as JSON.
    pub got: String,
}

impl std::fmt::Display for TypeMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "var `{}` must be {}, got {}",
            self.var, self.expected, self.got
        )
    }
}

/// Every declared var present in `raw` whose value does not match its
/// property schema (schemars 1.x output: `type`, `enum`, `const`, `items`,
/// `anyOf`/`oneOf`, `$ref` into `$defs`, integer `format` and bounds).
/// Undeclared keys and missing keys are not reported here.
pub fn type_mismatches(schema: &Value, raw: &Value) -> Vec<TypeMismatch> {
    let (Some(props), Some(obj)) = (
        schema.get("properties").and_then(Value::as_object),
        raw.as_object(),
    ) else {
        return vec![];
    };
    props
        .iter()
        .filter_map(|(name, p)| {
            let v = obj.get(name)?;
            (!fits(schema, p, v)).then(|| TypeMismatch {
                var: name.clone(),
                expected: describe(schema, p),
                got: v.to_string(),
            })
        })
        .collect()
}

fn fits(root: &Value, p: &Value, v: &Value) -> bool {
    let p = resolve(root, p);
    if let Some(alts) = p
        .get("anyOf")
        .or_else(|| p.get("oneOf"))
        .and_then(Value::as_array)
    {
        return alts.iter().any(|a| fits(root, a, v));
    }
    if let Some(c) = p.get("const") {
        return c == v;
    }
    if let Some(allowed) = p.get("enum").and_then(Value::as_array) {
        return allowed.contains(v);
    }
    let types: Vec<&str> = match p.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => return true,
    };
    types.iter().any(|t| match (*t, v) {
        ("null", Value::Null) => true,
        ("boolean", Value::Bool(_)) => true,
        ("string", Value::String(_)) => true,
        ("number", Value::Number(_)) => true,
        ("integer", Value::Number(n)) => match n.as_i64() {
            Some(i) => {
                let ok_min = p
                    .get("minimum")
                    .and_then(Value::as_i64)
                    .is_none_or(|m| i >= m);
                let ok_max = p
                    .get("maximum")
                    .and_then(Value::as_i64)
                    .is_none_or(|m| i <= m);
                ok_min && ok_max && int_format_fits(p, i)
            }
            // Beyond i64: only an unsigned 64-bit slot can hold it.
            None => {
                n.as_u64().is_some()
                    && p.get("format")
                        .and_then(Value::as_str)
                        .is_none_or(|f| matches!(f, "uint" | "uint64"))
            }
        },
        ("array", Value::Array(items)) => p
            .get("items")
            .is_none_or(|item| items.iter().all(|x| fits(root, item, x))),
        ("object", Value::Object(_)) => true,
        _ => false,
    })
}

fn int_format_fits(p: &Value, i: i64) -> bool {
    match p.get("format").and_then(Value::as_str) {
        Some("uint8") => u8::try_from(i).is_ok(),
        Some("uint16") => u16::try_from(i).is_ok(),
        Some("uint32") => u32::try_from(i).is_ok(),
        Some("uint" | "uint64") => i >= 0,
        Some("int8") => i8::try_from(i).is_ok(),
        Some("int16") => i16::try_from(i).is_ok(),
        Some("int32") => i32::try_from(i).is_ok(),
        _ => true,
    }
}

fn describe(root: &Value, p: &Value) -> String {
    let p = resolve(root, p);
    if let Some(alts) = p
        .get("anyOf")
        .or_else(|| p.get("oneOf"))
        .and_then(Value::as_array)
    {
        let parts: Vec<String> = alts.iter().map(|a| describe(root, a)).collect();
        return parts.join(" or ");
    }
    if let Some(c) = p.get("const") {
        return c.to_string();
    }
    if let Some(allowed) = p.get("enum").and_then(Value::as_array) {
        let parts: Vec<String> = allowed.iter().map(Value::to_string).collect();
        return format!("one of {}", parts.join(" | "));
    }
    let one = |t: &str| match t {
        "array" => match p.get("items") {
            Some(item) => format!("list of {}", describe(root, item)),
            None => "list".to_string(),
        },
        "integer" => match p.get("format").and_then(Value::as_str) {
            Some(f) => format!("integer ({f})"),
            None => "integer".to_string(),
        },
        t => t.to_string(),
    };
    match p.get("type") {
        Some(Value::String(t)) => one(t),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(one)
            .collect::<Vec<_>>()
            .join(" or "),
        _ => "any".to_string(),
    }
}

/// The candidate within two edits of `word`, if any. Shared by every
/// "unknown name" message so suggestions behave the same everywhere.
pub fn did_you_mean<'a>(
    word: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    candidates
        .into_iter()
        .map(|c| (edit_distance(word, c), c))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
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
    fn missing_required_names_every_gap() {
        let s = schema_for::<Two>();
        assert_eq!(
            missing_required(&s, &serde_json::json!({})),
            ["user", "fruit"]
        );
        assert_eq!(
            missing_required(&s, &serde_json::json!({"user": "x"})),
            ["fruit"]
        );
        assert!(missing_required(&s, &serde_json::json!({"user": "x", "fruit": "y"})).is_empty());
        assert_eq!(missing_required(&s, &Value::Null), ["user", "fruit"]);
    }

    #[test]
    fn type_mismatches_follow_the_schema() {
        let s = schema_for::<WithEnum>();
        let bad = serde_json::json!({"env": "Prod", "ports": [22, "80"]});
        let m = type_mismatches(&s, &bad);
        assert_eq!(m.len(), 2, "{m:?}");
        assert_eq!(m[0].var, "env");
        assert_eq!(m[0].expected, "one of \"Staging\" | \"Production\"");
        assert_eq!(m[0].got, "\"Prod\"");
        assert_eq!(
            m[1].to_string(),
            "var `ports` must be list of integer (uint16), got [22,\"80\"]"
        );
        let good = serde_json::json!({"env": "Staging", "ports": [22, 80]});
        assert!(type_mismatches(&s, &good).is_empty());
        // Option<u16>: null or an integer in range.
        let s = schema_for::<V>();
        assert!(type_mismatches(&s, &serde_json::json!({"user": "a", "port": null})).is_empty());
        assert!(type_mismatches(&s, &serde_json::json!({"user": "a", "port": 22})).is_empty());
        let m = type_mismatches(&s, &serde_json::json!({"user": 1, "port": 70000}));
        assert_eq!(m.len(), 2, "{m:?}");
        assert_eq!(m[0].expected, "integer (uint16) or null");
        assert_eq!(m[1].expected, "string");
    }

    #[test]
    fn did_you_mean_picks_the_closest_within_two_edits() {
        assert_eq!(
            did_you_mean("sshuser", ["addr", "ssh_user", "port"]),
            Some("ssh_user")
        );
        assert_eq!(did_you_mean("prot", ["port", "post"]), Some("port"));
        assert_eq!(did_you_mean("zzzzz", ["port"]), None);
    }

    #[derive(serde::Deserialize, schemars::JsonSchema, Debug, PartialEq)]
    struct Lists {
        packages: Vec<String>,
        ports: Option<Vec<u16>>,
    }

    #[test]
    fn scalar_for_a_list_field_becomes_a_one_element_list() {
        let v: Lists = from_value(serde_json::json!({"packages": "nginx", "ports": 22})).unwrap();
        assert_eq!(
            v,
            Lists {
                packages: vec!["nginx".into()],
                ports: Some(vec![22])
            }
        );
        let v: Lists = from_value(serde_json::json!({"packages": []})).unwrap();
        assert_eq!(v.packages, Vec::<String>::new());
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
