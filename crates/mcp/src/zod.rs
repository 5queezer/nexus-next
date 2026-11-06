//! Validates tool arguments as old Nexus's zod v3 schemas did, walking the frozen JSON Schema. It
//! accepts what zod accepted (unknown keys are stripped, not rejected), applies defaults, and
//! reports zod's issues with their key order, because clients see the text.
//!
//! Rules JSON Schema can't express are special-cased by tool: `.trim()` on the note of
//! `append_application_note`, and `datetime({ offset: true })` on the archive tools.

use serde_json::{Map, Value, json};
use url::Url;

use crate::js::{is_zod_datetime, utf16_len};

fn received(value: Option<&Value>) -> &'static str {
    match value {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

/// An issue's fields in zod's order. `message` first means zod's check had a `message` key.
macro_rules! issue {
    ($($key:literal: $value:expr),* $(,)?) => {{
        let mut map = Map::new();
        $(map.insert($key.to_owned(), json!($value));)*
        map
    }};
}

struct Walker<'a> {
    tool: &'a str,
    path: Vec<Value>,
    issues: Vec<Value>,
}

impl Walker<'_> {
    /// Adds `path` and, for a type error, `message` after it, as zod does.
    fn push(&mut self, mut issue: Map<String, Value>, message: Option<String>) {
        issue.insert("path".to_owned(), Value::Array(self.path.clone()));
        if let Some(message) = message {
            issue.insert("message".to_owned(), message.into());
        }
        self.issues.push(Value::Object(issue));
    }

    fn invalid_type(&mut self, expected: &str, value: Option<&Value>) {
        let got = received(value);
        let message = if value.is_none() {
            "Required".to_owned()
        } else {
            format!("Expected {expected}, received {got}")
        };
        self.push(
            issue! {"code": "invalid_type", "expected": expected, "received": got},
            Some(message),
        );
    }

    fn walk(&mut self, schema: &Map<String, Value>, value: Option<&Value>) -> Option<Value> {
        if let Some(Value::Array(options)) = schema.get("anyOf") {
            if value == Some(&Value::Null) {
                return Some(Value::Null);
            }
            let inner = options.iter().find(|o| o["type"] != "null")?.as_object()?;
            return self.walk(inner, value);
        }
        if let Some(Value::Array(options)) = schema.get("enum") {
            return self.enumeration(options, value);
        }
        // Simple nullables are written `"type": ["string", "null"]`.
        let kind = match schema.get("type") {
            Some(Value::Array(kinds)) => {
                if value == Some(&Value::Null) {
                    return Some(Value::Null);
                }
                kinds
                    .iter()
                    .filter_map(Value::as_str)
                    .find(|kind| *kind != "null")
            }
            other => other.and_then(Value::as_str),
        };
        match kind {
            Some("string") => self.string(schema, value),
            Some("number" | "integer") => self.number(schema, value),
            Some("boolean") => match value {
                Some(Value::Bool(_)) => value.cloned(),
                _ => {
                    self.invalid_type("boolean", value);
                    None
                }
            },
            Some("array") => self.array(schema, value),
            Some("object") if schema.contains_key("properties") => self.object(schema, value),
            Some("object") => match value {
                Some(Value::Object(_)) => value.cloned(),
                _ => {
                    self.invalid_type("object", value);
                    None
                }
            },
            // z.unknown()
            _ => value.cloned(),
        }
    }

    fn enumeration(&mut self, options: &[Value], value: Option<&Value>) -> Option<Value> {
        let quoted: Vec<String> = options
            .iter()
            .filter_map(Value::as_str)
            .map(|option| format!("'{option}'"))
            .collect();
        let expected = quoted.join(" | ");
        match value {
            Some(Value::String(text)) if options.iter().any(|o| o == text.as_str()) => {
                value.cloned()
            }
            Some(Value::String(text)) => {
                self.push(
                    issue! {"received": text, "code": "invalid_enum_value", "options": options},
                    Some(format!(
                        "Invalid enum value. Expected {expected}, received '{text}'"
                    )),
                );
                None
            }
            _ => {
                let got = received(value);
                let message = if value.is_none() {
                    "Required".to_owned()
                } else {
                    format!("Expected {expected}, received {got}")
                };
                self.push(
                    issue! {"expected": expected, "received": got, "code": "invalid_type"},
                    Some(message),
                );
                None
            }
        }
    }

    fn too_small(&mut self, kind: &str, minimum: &Value, exact: bool, message: String) {
        self.push(
            issue! {"code": "too_small", "minimum": minimum, "type": kind, "inclusive": true, "exact": exact, "message": message},
            None,
        );
    }

    fn too_big(&mut self, kind: &str, maximum: &Value, exact: bool, message: String) {
        self.push(
            issue! {"code": "too_big", "maximum": maximum, "type": kind, "inclusive": true, "exact": exact, "message": message},
            None,
        );
    }

    fn string(&mut self, schema: &Map<String, Value>, value: Option<&Value>) -> Option<Value> {
        let Some(Value::String(text)) = value else {
            self.invalid_type("string", value);
            return None;
        };
        let trimmed;
        let is_trimmed_note = self.tool == "append_application_note"
            && self.path.first().and_then(Value::as_str) == Some("note");
        let text = if is_trimmed_note {
            trimmed = crate::js::trim(text).to_owned();
            &trimmed
        } else {
            text
        };
        let length = utf16_len(text) as u64;
        let exact =
            schema.get("minLength").is_some() && schema.get("minLength") == schema.get("maxLength");
        // zod-to-json-schema writes checks in declaration order, and zod runs them in that order.
        for (keyword, bound) in schema {
            match keyword.as_str() {
                "minLength" if exact => {
                    let n = bound.as_u64()?;
                    if length < n {
                        self.too_small(
                            "string",
                            bound,
                            true,
                            format!("String must contain exactly {n} character(s)"),
                        );
                    }
                }
                "maxLength" if exact => {
                    let n = bound.as_u64()?;
                    if length > n {
                        self.too_big(
                            "string",
                            bound,
                            true,
                            format!("String must contain exactly {n} character(s)"),
                        );
                    }
                }
                "minLength" => {
                    let n = bound.as_u64()?;
                    if length < n {
                        self.too_small(
                            "string",
                            bound,
                            false,
                            format!("String must contain at least {n} character(s)"),
                        );
                    }
                }
                "maxLength" => {
                    let n = bound.as_u64()?;
                    if length > n {
                        self.too_big(
                            "string",
                            bound,
                            false,
                            format!("String must contain at most {n} character(s)"),
                        );
                    }
                }
                "format" if bound == "date-time" => {
                    let offset =
                        matches!(self.tool, "archive_application" | "unarchive_application");
                    if !is_zod_datetime(text, offset) {
                        self.push(
                            issue! {"code": "invalid_string", "validation": "datetime", "message": "Invalid datetime"},
                            None,
                        );
                    }
                }
                "format" if bound == "uri" && Url::parse(text).is_err() => {
                    self.push(
                        issue! {"validation": "url", "code": "invalid_string", "message": "Invalid url"},
                        None,
                    );
                }
                // The only pattern in the contract is contentHash's, /^[a-f0-9]{64}$/i.
                "pattern" if !(text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())) => {
                    self.push(
                        issue! {"validation": "regex", "code": "invalid_string", "message": "Invalid"},
                        None,
                    );
                }
                _ => {}
            }
        }
        Some(Value::String(text.clone()))
    }

    fn number(&mut self, schema: &Map<String, Value>, value: Option<&Value>) -> Option<Value> {
        let Some(Value::Number(number)) = value else {
            self.invalid_type("number", value);
            return None;
        };
        let n = number.as_f64()?;
        for (keyword, bound) in schema {
            match keyword.as_str() {
                "type" if bound == "integer" && n.fract() != 0.0 => {
                    self.push(
                        issue! {"code": "invalid_type", "expected": "integer", "received": "float", "message": "Expected integer, received float"},
                        None,
                    );
                }
                "minimum" if n < bound.as_f64()? => {
                    self.too_small(
                        "number",
                        bound,
                        false,
                        format!("Number must be greater than or equal to {bound}"),
                    );
                }
                "maximum" if n > bound.as_f64()? => {
                    self.too_big(
                        "number",
                        bound,
                        false,
                        format!("Number must be less than or equal to {bound}"),
                    );
                }
                _ => {}
            }
        }
        value.cloned()
    }

    fn array(&mut self, schema: &Map<String, Value>, value: Option<&Value>) -> Option<Value> {
        let Some(Value::Array(items)) = value else {
            self.invalid_type("array", value);
            return None;
        };
        if let Some(bound) = schema.get("minItems")
            && (items.len() as u64) < bound.as_u64()?
        {
            self.too_small(
                "array",
                bound,
                false,
                format!("Array must contain at least {bound} element(s)"),
            );
        }
        if let Some(bound) = schema.get("maxItems")
            && items.len() as u64 > bound.as_u64()?
        {
            self.too_big(
                "array",
                bound,
                false,
                format!("Array must contain at most {bound} element(s)"),
            );
        }
        let item_schema = schema.get("items").and_then(Value::as_object);
        let mut out = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            self.path.push(json!(index));
            let parsed = match item_schema {
                Some(inner) => self.walk(inner, Some(item)),
                None => Some(item.clone()),
            };
            self.path.pop();
            out.extend(parsed);
        }
        Some(Value::Array(out))
    }

    fn object(&mut self, schema: &Map<String, Value>, value: Option<&Value>) -> Option<Value> {
        let Some(Value::Object(input)) = value else {
            self.invalid_type("object", value);
            return None;
        };
        let required = schema.get("required").and_then(Value::as_array);
        let mut out = Map::new();
        for (name, property) in schema.get("properties")?.as_object()? {
            let property = property.as_object()?;
            let given = input.get(name);
            if given.is_none() {
                if let Some(default) = property.get("default") {
                    out.insert(name.clone(), default.clone());
                    continue;
                }
                if !required.is_some_and(|names| names.iter().any(|n| n == name.as_str())) {
                    continue;
                }
            }
            self.path.push(name.as_str().into());
            let parsed = self.walk(property, given);
            self.path.pop();
            if let Some(parsed) = parsed {
                out.insert(name.clone(), parsed);
            }
        }
        Some(Value::Object(out))
    }
}

/// The parsed arguments, or the text of zod's error: `JSON.stringify(issues, null, 2)`.
pub fn parse(
    tool: &str,
    schema: &Value,
    args: Option<&Value>,
) -> Result<Map<String, Value>, String> {
    let mut walker = Walker {
        tool,
        path: Vec::new(),
        issues: Vec::new(),
    };
    let parsed = schema
        .as_object()
        .and_then(|schema| walker.walk(schema, args));
    if !walker.issues.is_empty() {
        return Err(serde_json::to_string_pretty(&walker.issues).expect("issues serialize"));
    }
    match parsed {
        Some(Value::Object(map)) => Ok(map),
        _ => Err("[]".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "minLength": 1},
                "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 50},
                "code": {"anyOf": [{"type": "string", "minLength": 2, "maxLength": 2}, {"type": "null"}]},
                "kind": {"type": "string", "enum": ["a", "b"]},
                "meta": {"type": "object", "additionalProperties": {}},
                "any": {"description": "z.unknown"},
            },
            "required": ["id"],
            "additionalProperties": false,
        })
    }

    #[test]
    fn strips_unknown_keys_and_applies_defaults() {
        let parsed = parse(
            "t",
            &schema(),
            Some(&json!({"id": "x", "extra": 1, "any": null})),
        )
        .unwrap();
        assert_eq!(
            Value::Object(parsed),
            json!({"id": "x", "limit": 50, "any": null})
        );
    }

    #[test]
    fn missing_arguments_and_fields_read_required() {
        let err = parse("t", &schema(), None).unwrap_err();
        assert!(err.contains("\"message\": \"Required\""), "{err}");
        assert!(err.contains("\"expected\": \"object\""), "{err}");
        let err = parse("t", &schema(), Some(&json!({}))).unwrap_err();
        let expected = "[\n  {\n    \"code\": \"invalid_type\",\n    \"expected\": \"string\",\n    \"received\": \"undefined\",\n    \"path\": [\n      \"id\"\n    ],\n    \"message\": \"Required\"\n  }\n]";
        assert_eq!(err, expected);
    }

    #[test]
    fn numbers_report_float_and_bounds_in_zod_order() {
        let err = parse("t", &schema(), Some(&json!({"id": "x", "limit": 0.5}))).unwrap_err();
        let issues: Value = serde_json::from_str(&err).unwrap();
        assert_eq!(issues[0]["received"], "float");
        assert_eq!(issues[1]["code"], "too_small");
    }

    #[test]
    fn exact_length_and_enums_use_zod_messages() {
        let err = parse(
            "t",
            &schema(),
            Some(&json!({"id": "x", "code": "abc", "kind": "c"})),
        )
        .unwrap_err();
        assert!(
            err.contains("String must contain exactly 2 character(s)"),
            "{err}"
        );
        assert!(
            err.contains("Invalid enum value. Expected 'a' | 'b', received 'c'"),
            "{err}"
        );
    }

    #[test]
    fn type_lists_are_nullables_of_the_other_type() {
        let schema = json!({
            "type": "object",
            "properties": {"email": {"type": ["string", "null"]}, "pay": {"type": ["number", "null"]}},
        });
        let parsed = parse("t", &schema, Some(&json!({"email": null, "pay": 5}))).unwrap();
        assert_eq!(Value::Object(parsed), json!({"email": null, "pay": 5}));
        let err = parse("t", &schema, Some(&json!({"email": 5, "pay": "x"}))).unwrap_err();
        assert!(err.contains("Expected string, received number"), "{err}");
        assert!(err.contains("Expected number, received string"), "{err}");
    }

    #[test]
    fn records_reject_arrays() {
        let err = parse("t", &schema(), Some(&json!({"id": "x", "meta": []}))).unwrap_err();
        assert!(err.contains("Expected object, received array"), "{err}");
    }
}
