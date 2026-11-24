use crate::core::{AppError, Res};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

pub fn registry() -> &'static Value {
    static REGISTRY: OnceLock<Value> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        serde_json::from_str(include_str!("../spec/operations.json"))
            .expect("operation catalog is valid")
    })
}
pub fn operation(name: &str) -> Option<&'static Value> {
    registry()["operations"]
        .as_array()?
        .iter()
        .find(|op| op["name"] == name)
}
fn ref_name(v: &Value) -> Option<&str> {
    v.get("$ref")?.as_str()?.strip_prefix("#/$defs/")
}
fn walk_refs(v: &Value, seen: &mut BTreeSet<String>) {
    if let Some(k) = ref_name(v) {
        if seen.insert(k.into()) {
            walk_refs(&registry()["$defs"][k], seen)
        }
    } else if let Some(fields) = v.as_object() {
        for field in fields.values() {
            walk_refs(field, seen)
        }
    } else if let Some(items) = v.as_array() {
        for item in items {
            walk_refs(item, seen)
        }
    }
}
pub fn tool_schema(op: &Value, field: &str) -> Value {
    let raw = &op[field];
    let mut root = ref_name(raw)
        .map(|k| registry()["$defs"][k].clone())
        .unwrap_or_else(|| raw.clone());
    let mut refs = BTreeSet::new();
    walk_refs(&root, &mut refs);
    if !refs.is_empty() {
        root["$defs"] = json!(
            refs.into_iter()
                .map(|s| (s.clone(), registry()["$defs"][s].clone()))
                .collect::<BTreeMap<_, _>>()
        );
    }
    root
}
/// Validators cover the vocabulary present in the checked-in contract. A schema test
/// rejects new keywords that have no implementation here.
pub fn validate(schema: &Value, data: &Value, path: &str) -> Res<()> {
    let bad = |msg: &str| AppError::invalid(format!("{path}: {msg}"));
    if let Some(k) = ref_name(schema) {
        return validate(&registry()["$defs"][k], data, path);
    }
    if let Some(c) = schema.get("const")
        && c != data
    {
        return Err(bad("unexpected value"));
    }
    if let Some(opts) = schema.get("enum").and_then(Value::as_array)
        && !opts.contains(data)
    {
        return Err(bad("unknown option"));
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let obj = data.as_object().ok_or_else(|| bad("object required"))?;
            for k in schema["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !obj.contains_key(k) {
                    return Err(bad(&format!("missing {k}")));
                }
            }
            for (k, v) in obj {
                let child = schema["properties"]
                    .get(k)
                    .ok_or_else(|| bad(&format!("unknown field {k}")))?;
                validate(child, v, &format!("{path}.{k}"))?;
            }
        }
        Some("array") => {
            let arr = data.as_array().ok_or_else(|| bad("array required"))?;
            if arr.len() as u64 > schema["maxItems"].as_u64().unwrap_or(u64::MAX) {
                return Err(bad("too many items"));
            }
            for (i, x) in arr.iter().enumerate() {
                validate(&schema["items"], x, &format!("{path}[{i}]"))?
            }
        }
        Some("string") => {
            let s = data.as_str().ok_or_else(|| bad("string required"))?;
            let n = s.chars().count() as u64;
            if n < schema["minLength"].as_u64().unwrap_or(0)
                || n > schema["maxLength"].as_u64().unwrap_or(u64::MAX)
            {
                return Err(bad("length outside allowed range"));
            }
            if let Some(p) = schema["pattern"].as_str()
                && !regex::Regex::new(p)
                    .expect("tested schema pattern")
                    .is_match(s)
            {
                return Err(bad("invalid format"));
            }
            match schema["format"].as_str() {
                Some("date-time") if chrono::DateTime::parse_from_rfc3339(s).is_err() => {
                    return Err(bad("RFC 3339 timestamp required"));
                }
                Some("email") if crate::audience::email_key(s).is_none() => {
                    return Err(bad("valid email address required"));
                }
                Some("uri") if url::Url::parse(s).is_err() => {
                    return Err(bad("absolute URI required"));
                }
                _ => {}
            }
        }
        Some("number" | "integer") => {
            let v = data.as_f64().ok_or_else(|| bad("number required"))?;
            if schema["type"] == "integer" && v.fract() != 0.0 {
                return Err(bad("integer required"));
            }
            if v < schema["minimum"].as_f64().unwrap_or(f64::MIN)
                || v > schema["maximum"].as_f64().unwrap_or(f64::MAX)
            {
                return Err(bad("value outside allowed range"));
            }
        }
        Some("boolean") if !data.is_boolean() => return Err(bad("boolean required")),
        _ => {}
    }
    Ok(())
}
