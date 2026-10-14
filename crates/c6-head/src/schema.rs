//! Strict JSON Schema checking for records (docs/os.md §4.1): the subset
//! collections use — types, properties, required, enum and const, items,
//! anyOf/oneOf, `$ref` into `$defs`/`definitions`, ranges, lengths and
//! patterns. Unlike plain JSON Schema, an object accepts no fields beyond
//! its `properties` unless `additionalProperties` says so: agents cannot
//! drift the data.

use serde_json::{Map, Value};

/// Why `value` does not fit `schema`, as "path: problem" lines.
pub fn validate(schema: &Value, value: &Value) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    check(schema, schema, value, "", &mut errors, 0);
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

/// Whether `schema` is one a collection can have: an object schema whose
/// references resolve, and whose patterns compile.
pub fn check_schema(schema: &Value) -> Result<(), String> {
    if schema.get("type").and_then(Value::as_str) != Some("object") || !schema.get("properties").is_some_and(Value::is_object) {
        return Err("a collection's schema is an object schema with properties".into());
    }
    let mut problems = Vec::new();
    walk(schema, schema, &mut problems, 0);
    match problems.first() {
        Some(p) => Err(p.clone()),
        None => Ok(()),
    }
}

fn walk(root: &Value, s: &Value, problems: &mut Vec<String>, depth: usize) {
    if depth > 64 {
        problems.push("the schema nests too deeply".into());
        return;
    }
    let Some(o) = s.as_object() else { return };
    if let Some(r) = o.get("$ref").and_then(Value::as_str)
        && resolve(root, r).is_none()
    {
        problems.push(format!("reference {r} does not resolve"));
    }
    if let Some(p) = o.get("pattern").and_then(Value::as_str)
        && regex::Regex::new(p).is_err()
    {
        problems.push(format!("pattern {p:?} is not a valid regular expression"));
    }
    for key in ["properties", "$defs", "definitions"] {
        if let Some(m) = o.get(key).and_then(Value::as_object) {
            for v in m.values() {
                walk(root, v, problems, depth + 1);
            }
        }
    }
    for key in ["items", "additionalProperties"] {
        if let Some(v) = o.get(key) {
            walk(root, v, problems, depth + 1);
        }
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        for v in o.get(key).and_then(Value::as_array).into_iter().flatten() {
            walk(root, v, problems, depth + 1);
        }
    }
}

fn resolve<'a>(root: &'a Value, r: &str) -> Option<&'a Value> {
    let name = r.strip_prefix("#/$defs/").map(|n| ("$defs", n)).or_else(|| r.strip_prefix("#/definitions/").map(|n| ("definitions", n)))?;
    root.get(name.0)?.get(name.1)
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn is_type(v: &Value, t: &str) -> bool {
    let actual = type_name(v);
    actual == t || (t == "number" && actual == "integer") || (t == "integer" && v.as_f64().is_some_and(|f| f.fract() == 0.0))
}

fn at(path: &str) -> &str {
    if path.is_empty() { "(the record)" } else { path }
}

fn check(root: &Value, s: &Value, v: &Value, path: &str, errors: &mut Vec<String>, depth: usize) {
    if depth > 64 {
        errors.push(format!("{}: nests too deeply", at(path)));
        return;
    }
    let s = match s {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            errors.push(format!("{}: not allowed", at(path)));
            return;
        }
        Value::Object(o) => o,
        _ => return,
    };
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        match resolve(root, r) {
            Some(target) => check(root, target, v, path, errors, depth + 1),
            None => errors.push(format!("{}: reference {r} does not resolve", at(path))),
        }
    }
    if let Some(t) = s.get("type") {
        let types: Vec<&str> = match t {
            Value::String(t) => vec![t.as_str()],
            Value::Array(ts) => ts.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if !types.is_empty() && !types.iter().any(|t| is_type(v, t)) {
            errors.push(format!("{}: expected {}, got {}", at(path), types.join(" or "), type_name(v)));
            return;
        }
    }
    if let Some(options) = s.get("enum").and_then(Value::as_array)
        && !options.contains(v)
    {
        let shown: Vec<String> = options.iter().map(|o| o.to_string()).collect();
        errors.push(format!("{}: must be one of {}", at(path), shown.join(", ")));
    }
    if let Some(c) = s.get("const")
        && c != v
    {
        errors.push(format!("{}: must be {c}", at(path)));
    }
    for (key, all) in [("anyOf", false), ("oneOf", true)] {
        if let Some(options) = s.get(key).and_then(Value::as_array) {
            let fitting = options
                .iter()
                .filter(|o| {
                    let mut e = Vec::new();
                    check(root, o, v, path, &mut e, depth + 1);
                    e.is_empty()
                })
                .count();
            if fitting == 0 || (all && fitting > 1) {
                errors.push(format!(
                    "{}: does not match {}",
                    at(path),
                    if all { "exactly one allowed shape" } else { "any allowed shape" }
                ));
            }
        }
    }
    for o in s.get("allOf").and_then(Value::as_array).into_iter().flatten() {
        check(root, o, v, path, errors, depth + 1);
    }
    match v {
        Value::Object(fields) => object(root, s, fields, path, errors, depth),
        Value::Array(items) => {
            if let Some(n) = s.get("minItems").and_then(Value::as_u64)
                && (items.len() as u64) < n
            {
                errors.push(format!("{}: needs at least {n} items", at(path)));
            }
            if let Some(n) = s.get("maxItems").and_then(Value::as_u64)
                && (items.len() as u64) > n
            {
                errors.push(format!("{}: at most {n} items", at(path)));
            }
            if let Some(item) = s.get("items") {
                for (i, x) in items.iter().enumerate() {
                    check(root, item, x, &format!("{path}[{i}]"), errors, depth + 1);
                }
            }
        }
        Value::String(text) => {
            let n = text.chars().count() as u64;
            if let Some(min) = s.get("minLength").and_then(Value::as_u64)
                && n < min
            {
                errors.push(format!("{}: at least {min} characters", at(path)));
            }
            if let Some(max) = s.get("maxLength").and_then(Value::as_u64)
                && n > max
            {
                errors.push(format!("{}: at most {max} characters", at(path)));
            }
            if let Some(p) = s.get("pattern").and_then(Value::as_str) {
                match regex::Regex::new(p) {
                    Ok(re) if !re.is_match(text) => errors.push(format!("{}: does not match {p:?}", at(path))),
                    Err(_) => errors.push(format!("{}: the schema's pattern {p:?} is invalid", at(path))),
                    _ => {}
                }
            }
        }
        Value::Number(n) => {
            let x = n.as_f64().unwrap_or_default();
            for key in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
                if let Some(limit) = s.get(key).and_then(Value::as_f64) {
                    let fails = match key {
                        "minimum" => x < limit,
                        "maximum" => x > limit,
                        "exclusiveMinimum" => x <= limit,
                        _ => x >= limit,
                    };
                    if fails {
                        errors.push(format!("{}: {x} is outside the allowed range ({key} {limit})", at(path)));
                    }
                }
            }
        }
        _ => {}
    }
}

fn object(root: &Value, s: &Map<String, Value>, fields: &Map<String, Value>, path: &str, errors: &mut Vec<String>, depth: usize) {
    let props = s.get("properties").and_then(Value::as_object);
    for r in s.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        if !fields.contains_key(r) {
            errors.push(format!("{}: {r} is required", at(path)));
        }
    }
    for (k, v) in fields {
        let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
        match props.and_then(|ps| ps.get(k)) {
            Some(ps) => check(root, ps, v, &p, errors, depth + 1),
            None => match s.get("additionalProperties") {
                Some(Value::Bool(true)) => {}
                Some(extra @ Value::Object(_)) => check(root, extra, v, &p, errors, depth + 1),
                // Closed unless it says otherwise (and schemas without
                // properties at all, like a free-form map, stay open).
                None if props.is_none() => {}
                _ => errors.push(format!("{p}: unknown field")),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn errs(schema: Value, v: Value) -> Vec<String> {
        validate(&schema, &v).err().unwrap_or_default()
    }

    #[test]
    fn records_match_their_schema_strictly() {
        let s = json!({"type": "object", "required": ["name", "stage"], "properties": {
            "name": {"type": "string", "minLength": 1, "maxLength": 20},
            "stage": {"enum": ["found", "approved", "build"]},
            "score": {"type": "number", "minimum": 0, "maximum": 10},
            "tags": {"type": "array", "items": {"type": "string", "pattern": "^[a-z-]+$"}, "maxItems": 3},
            "owner": {"$ref": "#/$defs/Person"},
            "note": {"type": ["string", "null"]},
            "meta": {"type": "object"}
        }, "$defs": {"Person": {"type": "object", "required": ["email"], "properties": {"email": {"type": "string"}}}}});
        assert!(check_schema(&s).is_ok());
        assert!(errs(s.clone(), json!({"name": "gap", "stage": "found", "score": 7.5, "tags": ["shopify"], "owner": {"email": "a@b"}, "note": null, "meta": {"anything": 1}})).is_empty());
        assert_eq!(errs(s.clone(), json!({"name": "gap"})), vec!["(the record): stage is required"]);
        assert_eq!(errs(s.clone(), json!({"name": "gap", "stage": "done"})), vec![r#"stage: must be one of "found", "approved", "build""#]);
        assert_eq!(errs(s.clone(), json!({"name": "gap", "stage": "found", "extra": 1})), vec!["extra: unknown field"]);
        assert_eq!(
            errs(s.clone(), json!({"name": "", "stage": "found", "score": 11})),
            vec!["name: at least 1 characters", "score: 11 is outside the allowed range (maximum 10)"]
        );
        assert_eq!(errs(s.clone(), json!({"name": 3, "stage": "found"})), vec!["name: expected string, got integer"]);
        assert_eq!(
            errs(s.clone(), json!({"name": "g", "stage": "found", "tags": ["Bad Tag"]})),
            vec![r#"tags[0]: does not match "^[a-z-]+$""#]
        );
        assert_eq!(errs(s.clone(), json!({"name": "g", "stage": "found", "owner": {}})), vec!["owner: email is required"]);
        assert_eq!(
            errs(s, json!({"name": "g", "stage": "found", "owner": {"email": "x", "phone": "1"}})),
            vec!["owner.phone: unknown field"]
        );
    }

    #[test]
    fn schemas_are_checked() {
        assert!(check_schema(&json!({"type": "array"})).is_err());
        assert!(
            check_schema(&json!({"type": "object", "properties": {"x": {"$ref": "#/$defs/Nope"}}}))
                .unwrap_err()
                .contains("does not resolve")
        );
        assert!(
            check_schema(&json!({"type": "object", "properties": {"x": {"type": "string", "pattern": "("}}}))
                .unwrap_err()
                .contains("pattern")
        );
    }

    #[test]
    fn any_of_and_integers() {
        let s =
            json!({"type": "object", "properties": {"n": {"type": "integer"}, "v": {"anyOf": [{"type": "string"}, {"type": "integer"}]}}});
        assert!(errs(s.clone(), json!({"n": 3, "v": "x"})).is_empty());
        assert!(errs(s.clone(), json!({"n": 3.0, "v": 2})).is_empty());
        assert_eq!(errs(s.clone(), json!({"n": 3.5})), vec!["n: expected integer, got number"]);
        assert_eq!(errs(s, json!({"v": true})), vec!["v: does not match any allowed shape"]);
    }
}
