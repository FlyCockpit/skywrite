//! Agent-directed "focus" answers for `describe frames` and `describe video`:
//! normalized instructions, an optional JSON Schema from a documented subset,
//! validation of the model's answer (with normalized coordinates), and the
//! prompt sections that ask for it.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Part of every focus cache key; bump when the focus prompt changes.
pub const FOCUS_PROMPT_VERSION: &str = "focus-v1";
pub const MAX_FOCUS_ITEMS: usize = 16;
pub const MAX_FOCUS_BYTES: usize = 4000;
pub const MAX_SCHEMA_BYTES: usize = 16 * 1024;
pub const MAX_SCHEMA_DEPTH: usize = 8;
pub const MAX_SCHEMA_NODES: usize = 256;
/// Coordinates this far outside 0..1 are clamped (model rounding); further is an error.
pub const COORDINATE_TOLERANCE: f64 = 0.02;
const MAX_REPORTED_ERRORS: usize = 20;
const PREVIEW_CHARS: usize = 80;

/// Keywords accepted in a focus schema. Anything else is refused by name.
pub const SCHEMA_KEYWORDS: &[&str] = &[
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minItems",
    "maxItems",
    "minLength",
    "maxLength",
    "description",
    "title",
    "$schema",
    "x-skywrite",
];
const TYPES: &[&str] = &[
    "object", "array", "string", "number", "integer", "boolean", "null",
];

pub const COORDINATES: &str = "Coordinates: give positions as normalized numbers from 0 to 1 relative to the full image as shown (upright): divide pixel positions by the image width (x) and height (y). Never use pixels or a 0-1000 scale. Origin at the top-left corner, x increasing to the right and y increasing downward. A box is [x1, y1, x2, y2] = [left, top, right, bottom], with x1 < x2 and y1 < y2. A point is [x, y]. Make boxes as tight as you can. If something asked for is not visible, return an empty list or null instead of guessing.";

/// What the agent asked for, normalized and hashed for caching.
#[derive(Debug, Clone)]
pub struct Focus {
    /// Normalized instructions: each item trimmed, CRLF as LF, trailing spaces removed, joined by newlines.
    pub text: String,
    pub instructions_sha256: String,
    /// The validated schema as written, `x-skywrite` annotations included.
    pub schema: Option<Value>,
    pub schema_sha256: Option<String>,
    /// Identity of instructions plus schema: the part of the focus cache key this run adds.
    pub sha256: String,
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn normalize_item(raw: &str, what: &str) -> Result<String> {
    let text = raw.replace("\r\n", "\n");
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let text = lines.join("\n").trim().to_owned();
    ensure!(!text.is_empty(), "{what} is empty");
    ensure!(
        !text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t'),
        "{what} contains control characters (only newlines and tabs are allowed)"
    );
    Ok(text)
}

/// Normalized instruction text from `--focus` items and `--focus-file`, in that order.
pub fn normalize_text(items: &[String], file: Option<&Path>) -> Result<Option<String>> {
    ensure!(
        items.len() <= MAX_FOCUS_ITEMS,
        "at most {MAX_FOCUS_ITEMS} --focus items"
    );
    let mut parts = Vec::new();
    for item in items {
        parts.push(normalize_item(item, "a --focus item")?);
    }
    if let Some(path) = file {
        let bytes = read_bounded(path, MAX_FOCUS_BYTES, "--focus-file")?;
        let text = String::from_utf8(bytes).context("--focus-file must be UTF-8 text")?;
        parts.push(normalize_item(&text, "--focus-file")?);
    }
    if parts.is_empty() {
        return Ok(None);
    }
    let text = parts.join("\n");
    ensure!(
        text.len() <= MAX_FOCUS_BYTES,
        "focus instructions are {} bytes; the limit is {MAX_FOCUS_BYTES} (all --focus items and --focus-file together)",
        text.len()
    );
    Ok(Some(text))
}

/// Reads one regular file through a single descriptor, at most `limit`
/// bytes: a file that grows or is swapped after the check cannot exceed it.
fn read_bounded(path: &Path, limit: usize, what: &str) -> Result<Vec<u8>> {
    crate::io::read_regular_bounded(path, limit as u64, false)
        .with_context(|| format!("{what} must be a regular file of at most {limit} bytes"))
}

pub fn instructions_sha256(text: &str) -> String {
    sha256_hex(text.as_bytes())
}

pub fn load(items: &[String], file: Option<&Path>, schema: Option<&Path>) -> Result<Option<Focus>> {
    let Some(text) = normalize_text(items, file)? else {
        ensure!(
            schema.is_none(),
            "--focus-schema needs --focus or --focus-file saying what to answer"
        );
        return Ok(None);
    };
    let schema = schema.map(load_schema).transpose()?;
    let schema_sha256 = schema.as_ref().map(|schema| sha256_hex(&canonical(schema)));
    let sha256 = crate::digest::cache_key(&[
        b"focus-identity-v1",
        text.as_bytes(),
        schema_sha256.as_deref().unwrap_or("").as_bytes(),
    ]);
    Ok(Some(Focus {
        instructions_sha256: instructions_sha256(&text),
        text,
        schema,
        schema_sha256,
        sha256,
    }))
}

pub fn load_schema(path: &Path) -> Result<Value> {
    let bytes = read_bounded(path, MAX_SCHEMA_BYTES, "--focus-schema")?;
    if let Some(literal) = inexact_number(&String::from_utf8_lossy(&bytes)) {
        bail!(
            "--focus-schema number {literal} cannot be represented exactly (it would be rounded when parsed); use integers up to 64 bits or floats between 2^-1022 and 2^53 in magnitude"
        );
    }
    let schema: Value =
        serde_json::from_slice(&bytes).context("--focus-schema is not valid JSON")?;
    check_schema(&schema).context(
        "--focus-schema is outside the supported JSON Schema subset (see describe frames --help)",
    )?;
    Ok(schema)
}

/// Sorted-key JSON, so the same schema always hashes the same.
pub fn canonical(value: &Value) -> Vec<u8> {
    fn sort(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut sorted = Map::new();
                for key in keys {
                    sorted.insert(key.clone(), sort(&map[key]));
                }
                Value::Object(sorted)
            }
            Value::Array(items) => Value::Array(items.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_vec(&sort(value)).expect("JSON value serializes")
}

/// For schema files: the first number literal in raw JSON text that parsing
/// would change in a way that matters (answers are not scanned: numbers
/// compare as IEEE-754 doubles, as RFC 8259 recommends for interoperability): an integer outside 64 bits, a float that underflows to
/// zero or overflows, or a float of 2^53 or more in magnitude (no longer
/// exact for integers). Strings are skipped.
pub fn inexact_number(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let (mut i, mut in_string) = (0, false);
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            match c {
                b'\\' => i += 1,
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_string = true;
            i += 1;
            continue;
        }
        if c == b'-' || c.is_ascii_digit() {
            let start = i;
            while i < bytes.len()
                && matches!(bytes[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
            {
                i += 1;
            }
            let literal = &text[start..i];
            // Only JSON number tokens; prose such as "3-4" or "left-right" is not one.
            if !json_number(literal) {
                continue;
            }
            let integer = !literal.contains(['.', 'e', 'E']);
            let exact = if integer {
                literal.parse::<i64>().is_ok() || literal.parse::<u64>().is_ok()
            } else {
                let mantissa = literal.split(['e', 'E']).next().unwrap_or("");
                let nonzero = mantissa.bytes().any(|b| matches!(b, b'1'..=b'9'));
                literal.parse::<f64>().is_ok_and(|f| {
                    f.is_finite()
                        && (f != 0.0 || !nonzero)
                        && f.abs() < 9_007_199_254_740_992.0
                        && (f == 0.0 || f.abs() >= f64::MIN_POSITIVE)
                })
            };
            if !exact {
                return Some(literal.to_owned());
            }
            continue;
        }
        i += 1;
    }
    None
}

/// The JSON number grammar: -?(0|[1-9][0-9]*)(.[0-9]+)?([eE][+-]?[0-9]+)?
fn json_number(token: &str) -> bool {
    let b = token.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let digits = |i: &mut usize| {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        *i - start
    };
    let int_start = i;
    let n = digits(&mut i);
    if n == 0 || (n > 1 && b[int_start] == b'0') {
        return false;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        if digits(&mut i) == 0 {
            return false;
        }
    }
    if i < b.len() && matches!(b[i], b'e' | b'E') {
        i += 1;
        if i < b.len() && matches!(b[i], b'+' | b'-') {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return false;
        }
    }
    i == b.len()
}

/// Numbers in a schema must be exact after parsing: integers (64-bit) or
/// floats below 2^53 in magnitude. A larger float literal may already have
/// been rounded by the JSON parser, so a bound could silently move.
fn exact_number(value: &Value, path: &str) -> Result<()> {
    match value {
        Value::Number(n) if n.is_f64() => {
            let f = n.as_f64().unwrap_or(f64::INFINITY);
            ensure!(
                f.abs() < 9_007_199_254_740_992.0,
                "{path}: {value} cannot be represented exactly; write large bounds as integers (up to 64 bits) or use smaller numbers"
            );
            Ok(())
        }
        Value::Array(items) => items.iter().try_for_each(|item| exact_number(item, path)),
        Value::Object(map) => map.values().try_for_each(|item| exact_number(item, path)),
        _ => Ok(()),
    }
}

/// Refuses anything outside the documented subset, naming the path.
pub fn check_schema(schema: &Value) -> Result<()> {
    let mut nodes = 0;
    check_node(schema, "$", 0, &mut nodes)
}

fn types_of(node: &Map<String, Value>, path: &str) -> Result<Vec<String>> {
    let types = match node.get("type") {
        None => Vec::new(),
        Some(Value::String(t)) => vec![t.clone()],
        Some(Value::Array(list)) => list
            .iter()
            .map(|t| t.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .with_context(|| format!("{path}.type must be a type name or a list of them"))?,
        Some(_) => bail!("{path}.type must be a type name or a list of them"),
    };
    for t in &types {
        ensure!(
            TYPES.contains(&t.as_str()),
            "{path}.type {t:?} is not one of {}",
            TYPES.join(", ")
        );
    }
    Ok(types)
}

fn check_node(schema: &Value, path: &str, depth: usize, nodes: &mut usize) -> Result<()> {
    *nodes += 1;
    ensure!(
        *nodes <= MAX_SCHEMA_NODES,
        "--focus-schema has more than {MAX_SCHEMA_NODES} schema nodes"
    );
    ensure!(
        depth <= MAX_SCHEMA_DEPTH,
        "--focus-schema nests deeper than {MAX_SCHEMA_DEPTH} levels at {path}"
    );
    let node = schema
        .as_object()
        .with_context(|| format!("{path} must be a schema object"))?;
    for key in node.keys() {
        ensure!(
            SCHEMA_KEYWORDS.contains(&key.as_str()),
            "{path} uses unsupported schema keyword {key:?}; supported: {}",
            SCHEMA_KEYWORDS.join(", ")
        );
    }
    ensure!(
        path == "$" || !node.contains_key("$schema"),
        "{path}: $schema is allowed only at the top level"
    );
    let types = types_of(node, path)?;
    let numeric = |key: &str| -> Result<()> {
        if let Some(value) = node.get(key) {
            ensure!(value.is_number(), "{path}.{key} must be a number");
            exact_number(value, &format!("{path}.{key}"))?;
        }
        Ok(())
    };
    for key in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
        numeric(key)?;
    }
    for key in ["minItems", "maxItems", "minLength", "maxLength"] {
        if let Some(value) = node.get(key) {
            ensure!(
                value.as_u64().is_some(),
                "{path}.{key} must be a non-negative integer"
            );
        }
    }
    for key in ["description", "title", "$schema"] {
        if let Some(value) = node.get(key) {
            ensure!(value.is_string(), "{path}.{key} must be a string");
        }
    }
    if let Some(value) = node.get("const") {
        exact_number(value, &format!("{path}.const"))?;
    }
    if let Some(values) = node.get("enum") {
        let values = values
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= 256)
            .with_context(|| format!("{path}.enum must list 1..256 values"))?;
        ensure!(
            values.iter().all(|v| !v.is_object() && !v.is_array()),
            "{path}.enum values must be strings, numbers, booleans or null"
        );
        for value in values {
            exact_number(value, &format!("{path}.enum"))?;
        }
    }
    if let Some(value) = node.get("additionalProperties") {
        ensure!(
            value.is_boolean(),
            "{path}.additionalProperties must be true or false (schemas are not supported there)"
        );
    }
    let properties = match node.get("properties") {
        None => None,
        Some(Value::Object(properties)) => Some(properties),
        Some(_) => bail!("{path}.properties must be an object of schemas"),
    };
    if let Some(properties) = properties {
        ensure!(
            properties.len() <= 64,
            "{path}.properties lists more than 64 properties"
        );
        for (name, child) in properties {
            check_node(child, &format!("{path}.{name}"), depth + 1, nodes)?;
        }
    }
    if let Some(required) = node.get("required") {
        let required = required
            .as_array()
            .with_context(|| format!("{path}.required must be a list of property names"))?;
        for name in required {
            let name = name
                .as_str()
                .with_context(|| format!("{path}.required must be a list of property names"))?;
            ensure!(
                properties.is_some_and(|p| p.contains_key(name)),
                "{path}.required names {name:?}, which is not in {path}.properties"
            );
        }
    }
    if let Some(items) = node.get("items") {
        check_node(items, &format!("{path}[]"), depth + 1, nodes)?;
    }
    if let Some(kind) = node.get("x-skywrite") {
        let kind = kind.as_str().unwrap_or_default();
        ensure!(
            matches!(kind, "box" | "point"),
            "{path}.x-skywrite must be \"box\" or \"point\""
        );
        ensure!(
            types.is_empty() || types == ["array"],
            "{path}: an x-skywrite {kind} is an array; give \"type\": \"array\" or leave type out"
        );
        let extra: Vec<&str> = node
            .keys()
            .map(String::as_str)
            .filter(|k| !matches!(*k, "x-skywrite" | "type" | "description" | "title"))
            .collect();
        ensure!(
            extra.is_empty(),
            "{path}: an x-skywrite {kind} defines its own shape and range; remove {}",
            extra.join(", ")
        );
    }
    Ok(())
}

/// The schema sent to the model: `x-skywrite` annotations become plain
/// arrays of numbers with a description of the convention.
pub fn wire_schema(schema: &Value) -> Value {
    let mut node = schema.clone();
    let Some(object) = node.as_object_mut() else {
        return node;
    };
    object.remove("$schema");
    if let Some(kind) = object.remove("x-skywrite") {
        let (count, text) = if kind == "box" {
            (
                4,
                "box [x1, y1, x2, y2] = [left, top, right, bottom], each a fraction 0..1 of the image width (x) or height (y) from the top-left; not pixels, not 0-1000",
            )
        } else {
            (
                2,
                "point [x, y], each a fraction 0..1 of the image width (x) or height (y) from the top-left; not pixels, not 0-1000",
            )
        };
        object.insert("type".into(), json!("array"));
        object.insert(
            "items".into(),
            json!({"type":"number","minimum":0,"maximum":1}),
        );
        object.insert("minItems".into(), json!(count));
        object.insert("maxItems".into(), json!(count));
        object.entry("description").or_insert_with(|| json!(text));
        return node;
    }
    if let Some(Value::Object(properties)) = object.get_mut("properties") {
        for child in properties.values_mut() {
            *child = wire_schema(child);
        }
    }
    if let Some(items) = object.get_mut("items") {
        *items = wire_schema(items);
    }
    node
}

/// Strict structured output needs every object closed and fully required.
pub fn strict_compatible(schema: &Value) -> bool {
    let Some(node) = schema.as_object() else {
        return false;
    };
    let properties = node.get("properties").and_then(Value::as_object);
    let types = types_of(node, "$").unwrap_or_default();
    let is_object =
        types.iter().any(|t| t == "object") || (node.get("type").is_none() && properties.is_some());
    if is_object {
        let required: Vec<&str> = node
            .get("required")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let closed = node.get("additionalProperties") == Some(&Value::Bool(false));
        let all = properties.is_some_and(|p| p.keys().all(|k| required.contains(&k.as_str())));
        if !closed || !all {
            return false;
        }
    }
    if node.get("type").is_none() && !node.contains_key("enum") && !node.contains_key("const") {
        return false;
    }
    // Strict providers need an item schema for every array.
    let is_array = types.iter().any(|t| t == "array");
    if is_array && !node.contains_key("items") {
        return false;
    }
    properties.is_none_or(|p| p.values().all(strict_compatible))
        && node.get("items").is_none_or(strict_compatible)
}

/// Validation outcome: the (possibly clamped) value or the problems found.
pub struct Checked {
    pub value: Value,
    pub errors: Vec<String>,
    pub clamped: usize,
}

/// Checks an answer against the schema. Numbers compare as IEEE-754 doubles
/// (RFC 8259's interoperable precision), exactly for whole numbers: digits a
/// double cannot hold are not distinguished.
pub fn validate(schema: Option<&Value>, value: &Value) -> Checked {
    let mut checked = Checked {
        value: value.clone(),
        errors: Vec::new(),
        clamped: 0,
    };
    if let Some(schema) = schema {
        // Pass 1 clamps coordinates; pass 2 checks every constraint, parents'
        // const/enum included, against the final value that is returned and cached.
        let mut value = value.clone();
        let mut first = Checked {
            value: Value::Null,
            errors: Vec::new(),
            clamped: 0,
        };
        check_value(schema, &mut value, "focus", &mut first);
        check_value(schema, &mut value, "focus", &mut checked);
        checked.clamped = first.clamped;
        checked.value = value;
    }
    checked
}

fn error(checked: &mut Checked, message: String) {
    if checked.errors.len() < MAX_REPORTED_ERRORS {
        checked.errors.push(message);
    }
}

fn type_matches(t: &str, value: &Value) -> bool {
    match t {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0)
        }
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn coordinates(kind: &str, value: &mut Value, path: &str, checked: &mut Checked) {
    let count = if kind == "box" { 4 } else { 2 };
    let Some(items) = value.as_array_mut().filter(|items| items.len() == count) else {
        return error(
            checked,
            format!(
                "{path}: a {kind} is an array of exactly {count} numbers, got {}",
                compact(value)
            ),
        );
    };
    let mut numbers = Vec::with_capacity(count);
    for item in items.iter_mut() {
        let Some(n) = item.as_f64().filter(|n| n.is_finite()) else {
            return error(checked, format!("{path}: {kind} values must be numbers"));
        };
        if !(-COORDINATE_TOLERANCE..=1.0 + COORDINATE_TOLERANCE).contains(&n) {
            return error(
                checked,
                format!(
                    "{path}: {n} is outside 0..1; coordinates are normalized to the image size, not pixels"
                ),
            );
        }
        let clamped = n.clamp(0.0, 1.0);
        if clamped != n {
            checked.clamped += 1;
            *item = json!(clamped);
        }
        numbers.push(clamped);
    }
    if kind == "box" && !(numbers[0] < numbers[2] && numbers[1] < numbers[3]) {
        error(
            checked,
            format!(
                "{path}: a box is [x1, y1, x2, y2] with x1 < x2 and y1 < y2 (left, top, right, bottom), got {}",
                compact(value)
            ),
        );
    }
}

/// Numeric order that is exact for integers of any representation: an
/// integer and a whole-number float compare as integers, never through a
/// rounded f64.
fn compare_numbers(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    fn whole(value: &Value) -> Option<i128> {
        if let Some(n) = value.as_i64() {
            return Some(n as i128);
        }
        if let Some(n) = value.as_u64() {
            return Some(n as i128);
        }
        let f = value.as_f64()?;
        // Every finite f64 of at least 2^53 is whole; i128 holds up to 1.7e38.
        (f.fract() == 0.0 && f.abs() < 1e38).then_some(f as i128)
    }
    match (whole(a), whole(b)) {
        (Some(x), Some(y)) => Some(x.cmp(&y)),
        // One side has a fraction, so it is below 2^53 in magnitude, where f64
        // is exact for the integer side's order relative to it.
        _ => a.as_f64()?.partial_cmp(&b.as_f64()?),
    }
}

/// JSON Schema equality: numbers compare by value (1 equals 1.0).
fn json_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(_), Value::Number(_)) => {
            compare_numbers(a, b) == Some(std::cmp::Ordering::Equal)
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| json_equal(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_equal(v, w)))
        }
        _ => a == b,
    }
}

fn compact(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() > 120 {
        format!("{}…", text.chars().take(120).collect::<String>())
    } else {
        text
    }
}

fn check_value(schema: &Value, value: &mut Value, path: &str, checked: &mut Checked) {
    let Some(node) = schema.as_object() else {
        return;
    };
    if let Some(kind) = node.get("x-skywrite").and_then(Value::as_str) {
        return coordinates(kind, value, path, checked);
    }
    let types = types_of(node, path).unwrap_or_default();
    if !types.is_empty() && !types.iter().any(|t| type_matches(t, value)) {
        return error(
            checked,
            format!(
                "{path}: expected {}, got {}",
                types.join(" or "),
                compact(value)
            ),
        );
    }
    if let Some(values) = node.get("enum").and_then(Value::as_array)
        && !values.iter().any(|v| json_equal(v, value))
    {
        error(
            checked,
            format!(
                "{path}: {} is not one of {}",
                compact(value),
                compact(&Value::Array(values.clone()))
            ),
        );
    }
    if let Some(expected) = node.get("const")
        && !json_equal(expected, value)
    {
        error(
            checked,
            format!(
                "{path}: expected {}, got {}",
                compact(expected),
                compact(value)
            ),
        );
    }
    if value.is_number() {
        let violated = [
            (
                "minimum",
                std::cmp::Ordering::is_lt as fn(std::cmp::Ordering) -> bool,
            ),
            ("maximum", std::cmp::Ordering::is_gt),
            ("exclusiveMinimum", std::cmp::Ordering::is_le),
            ("exclusiveMaximum", std::cmp::Ordering::is_ge),
        ];
        for (key, broken) in violated {
            if let Some(bound) = node.get(key)
                && compare_numbers(value, bound).is_some_and(broken)
            {
                error(checked, format!("{path}: {value} violates {key} {bound}"));
            }
        }
    }
    if let Some(text) = value.as_str() {
        let length = text.chars().count() as u64;
        if node
            .get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|min| length < min)
            || node
                .get("maxLength")
                .and_then(Value::as_u64)
                .is_some_and(|max| length > max)
        {
            error(
                checked,
                format!("{path}: string length {length} is out of bounds"),
            );
        }
    }
    if let Some(items) = value.as_array_mut() {
        let length = items.len() as u64;
        if node
            .get("minItems")
            .and_then(Value::as_u64)
            .is_some_and(|min| length < min)
            || node
                .get("maxItems")
                .and_then(Value::as_u64)
                .is_some_and(|max| length > max)
        {
            error(checked, format!("{path}: {length} items is out of bounds"));
        }
        if let Some(item_schema) = node.get("items") {
            for (index, item) in items.iter_mut().enumerate() {
                check_value(item_schema, item, &format!("{path}[{index}]"), checked);
            }
        }
    }
    if let Some(object) = value.as_object_mut() {
        let properties = node.get("properties").and_then(Value::as_object);
        for name in node
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !object.contains_key(name) {
                error(checked, format!("{path}: missing required key {name:?}"));
            }
        }
        if node.get("additionalProperties") == Some(&Value::Bool(false)) {
            for key in object.keys() {
                if !properties.is_some_and(|p| p.contains_key(key)) {
                    error(checked, format!("{path}: unexpected key {key:?}"));
                }
            }
        }
        if let Some(properties) = properties {
            for (name, child) in properties {
                if let Some(item) = object.get_mut(name) {
                    check_value(child, item, &format!("{path}.{name}"), checked);
                }
            }
        }
    }
}

pub fn preview(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > PREVIEW_CHARS {
        format!("{}…", flat.chars().take(PREVIEW_CHARS).collect::<String>())
    } else {
        flat
    }
}

/// The prompt section that states the task, coordinates and answer shape.
/// `subject` is "frame" or "clip".
pub fn section(focus: &Focus, subject: &str) -> String {
    let mut text = format!(
        "FOCUS TASK. The user needs this from the {subject}; answer it under the key \"focus\". Report only what is visible.\nInstructions:\n<<<\n{}\n>>>\n{COORDINATES}\n",
        focus.text
    );
    if subject == "clip" {
        text.push_str("Times are seconds from the start of this clip.\n");
    }
    match &focus.schema {
        Some(schema) => text.push_str(&format!(
            "The value of \"focus\" must match this JSON Schema exactly:\n{}\n",
            serde_json::to_string(&wire_schema(schema)).expect("JSON value serializes")
        )),
        None => text.push_str(
            "The value of \"focus\" is JSON shaped as the instructions ask (an object or a list).\n",
        ),
    }
    text
}

/// A focus-only request: nothing about the general description.
pub fn only_prompt(focus: &Focus, subject: &str, context: &str) -> String {
    let intro = if subject == "clip" {
        "You look at a short video clip for a video editor."
    } else {
        "You look at ONE exact video frame (a single still image) for a video editor."
    };
    format!(
        "{intro} Do not guess names or identities, and do not invent anything that is not visible.\n\n{context}{}\nReturn only a JSON object of the form {{\"focus\": ...}} and nothing else.",
        section(focus, subject)
    )
}

/// Appended to the general prompt when one request answers both.
pub fn combined_suffix(focus: &Focus, subject: &str) -> String {
    format!(
        "\n\nThis request has two parts. Put the object described above under the key \"description\", and the focus answer under the key \"focus\": return only {{\"description\": {{...}}, \"focus\": ...}}.\n\n{}",
        section(focus, subject)
    )
}

pub fn correction(errors: &[String]) -> String {
    format!(
        "Your \"focus\" value did not match the required shape:\n- {}\nReturn only the corrected JSON object {{\"focus\": ...}}, following the schema and the coordinate convention exactly.",
        errors.join("\n- ")
    )
}

/// The `response_format` for a request, and its label for the report.
pub fn response_format(
    focus: &Focus,
    combined: bool,
    structured: Option<bool>,
) -> (Value, &'static str) {
    let Some(schema) = focus.schema.as_ref().filter(|_| structured == Some(true)) else {
        return (json!({"type":"json_object"}), "json_object");
    };
    let wire = wire_schema(schema);
    let wrapper = if combined {
        // The general description has no strict schema; the wrapper stays non-strict.
        json!({"type":"object","properties":{"description":{"type":"object"},"focus":wire},
            "required":["description","focus"],"additionalProperties":false})
    } else {
        json!({"type":"object","properties":{"focus":wire},"required":["focus"],"additionalProperties":false})
    };
    let strict = !combined && strict_compatible(&wire);
    (
        json!({"type":"json_schema","json_schema":{"name":"skywrite_focus","strict":strict,"schema":wrapper}}),
        if strict {
            "json_schema_strict"
        } else {
            "json_schema"
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn faces() -> Value {
        json!({"type":"object","additionalProperties":false,"required":["faces"],"properties":{
            "faces":{"type":"array","items":{"type":"object","additionalProperties":false,"required":["box","label"],
                "properties":{"box":{"x-skywrite":"box"},"label":{"type":"string"}}}}}})
    }

    #[test]
    fn instructions_normalize_and_bound() {
        let a = normalize_text(&[" faces  \r\nplease \n".into()], None).unwrap();
        assert_eq!(a.as_deref(), Some("faces\nplease"));
        assert!(normalize_text(&["  ".into()], None).is_err());
        assert!(normalize_text(&["a\u{7}".into()], None).is_err());
        assert!(normalize_text(&["x".repeat(MAX_FOCUS_BYTES + 1)], None).is_err());
        assert!(normalize_text(&vec!["x".into(); MAX_FOCUS_ITEMS + 1], None).is_err());
        assert_eq!(normalize_text(&[], None).unwrap(), None);
    }

    #[test]
    fn schemas_outside_the_subset_are_refused_by_name() {
        check_schema(&faces()).unwrap();
        for (schema, message) in [
            (json!({"type":"object","oneOf":[]}), "\"oneOf\""),
            (json!({"type":"thing"}), "not one of"),
            (
                json!({"type":"object","required":["x"]}),
                "not in $.properties",
            ),
            (
                json!({"additionalProperties":{"type":"string"}}),
                "true or false",
            ),
            (json!({"x-skywrite":"circle"}), "\"box\" or \"point\""),
            (json!({"x-skywrite":"box","type":"string"}), "is an array"),
            (json!([1]), "schema object"),
        ] {
            let error = check_schema(&schema).unwrap_err().to_string();
            assert!(error.contains(message), "{error}");
        }
        let mut deep = json!({"type":"string"});
        for _ in 0..=MAX_SCHEMA_DEPTH {
            deep = json!({"type":"array","items":deep});
        }
        assert!(
            check_schema(&deep)
                .unwrap_err()
                .to_string()
                .contains("deeper")
        );
    }

    #[test]
    fn boxes_are_validated_and_clamped_within_tolerance() {
        let schema = faces();
        let ok = validate(
            Some(&schema),
            &json!({"faces":[{"box":[-0.01,0.1,0.5,1.01],"label":"left"}]}),
        );
        assert!(ok.errors.is_empty(), "{:?}", ok.errors);
        assert_eq!(ok.clamped, 2);
        assert_eq!(ok.value["faces"][0]["box"], json!([0.0, 0.1, 0.5, 1.0]));
        for (answer, message) in [
            (
                json!({"faces":[{"box":[0.5,0.1,0.2,0.9],"label":"x"}]}),
                "x1 < x2",
            ),
            (
                json!({"faces":[{"box":[0.1,0.9,0.2,0.3],"label":"x"}]}),
                "x1 < x2 and y1 < y2",
            ),
            (
                json!({"faces":[{"box":[10,20,300,400],"label":"x"}]}),
                "not pixels",
            ),
            (
                json!({"faces":[{"box":[0.1,0.2,0.3],"label":"x"}]}),
                "exactly 4",
            ),
            (
                json!({"faces":[{"label":"x"}]}),
                "missing required key \"box\"",
            ),
            (json!({"faces":[],"extra":1}), "unexpected key \"extra\""),
            (json!({"faces":"none"}), "expected array"),
        ] {
            let checked = validate(Some(&schema), &answer);
            assert!(
                checked.errors.iter().any(|e| e.contains(message)),
                "{answer}: {:?}",
                checked.errors
            );
        }
    }

    #[test]
    fn numbers_compare_by_value_and_keep_integer_precision() {
        let one = json!({"enum":[1]});
        assert!(validate(Some(&one), &json!(1.0)).errors.is_empty());
        assert!(
            validate(Some(&json!({"const":2})), &json!(2.0))
                .errors
                .is_empty()
        );
        assert!(!validate(Some(&one), &json!(2)).errors.is_empty());
        let big = json!({"minimum": 9007199254740993_u64});
        assert!(
            !validate(Some(&big), &json!(9007199254740992_u64))
                .errors
                .is_empty()
        );
        assert!(
            validate(Some(&big), &json!(9007199254740993_u64))
                .errors
                .is_empty()
        );
        assert!(
            !validate(Some(&json!({"exclusiveMaximum":1})), &json!(1.0))
                .errors
                .is_empty()
        );
        // Mixed representations: a whole float against a large integer bound.
        assert!(
            !validate(Some(&big), &json!(9007199254740992.0))
                .errors
                .is_empty()
        );
        assert!(
            validate(
                Some(&json!({"const": 9007199254740993_u64})),
                &json!(9007199254740992.0)
            )
            .errors
            .len()
                == 1
        );
        let max = json!({"maximum": 9007199254740992.0});
        assert!(
            !validate(Some(&max), &json!(9007199254740993_u64))
                .errors
                .is_empty()
        );
        assert!(
            validate(Some(&json!({"maximum":1.5})), &json!(1))
                .errors
                .is_empty()
        );
        // A float literal that the parser may have rounded is refused, from raw JSON text.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("s.json");
        std::fs::write(&file, r#"{"type":"integer","minimum":9007199254740993.0}"#).unwrap();
        assert!(format!("{:#}", load_schema(&file).unwrap_err()).contains("exactly"));
        std::fs::write(&file, r#"{"type":"integer","minimum":9007199254740993}"#).unwrap();
        let schema = load_schema(&file).unwrap();
        let answer: Value = serde_json::from_str("9007199254740992").unwrap();
        assert!(!validate(Some(&schema), &answer).errors.is_empty());
        std::fs::write(&file, r#"{"enum":[1e300]}"#).unwrap();
        assert!(load_schema(&file).is_err());
        // Raw literals are checked before parsing can round them.
        std::fs::write(&file, r#"{"type":"number","minimum":1e-400}"#).unwrap();
        assert!(format!("{:#}", load_schema(&file).unwrap_err()).contains("1e-400"));
        std::fs::write(
            &file,
            r#"{"type":"integer","maximum":100000000000000000000}"#,
        )
        .unwrap();
        assert!(load_schema(&file).is_err());
        assert_eq!(
            inexact_number(r#"{"x":9007199254740993.0}"#).as_deref(),
            Some("9007199254740993.0")
        );
        assert_eq!(
            inexact_number(r#"{"a":"1e-400","b":[0.5,-3,0.0,1e10]}"#),
            None
        );
        assert_eq!(inexact_number(r#"{"a":"x\"1e-400","b":2}"#), None);
        // Prose around an answer (a thinking block) is not mistaken for numbers.
        assert_eq!(
            inexact_number("<think>left-right, 3-4 faces, v2.5-</think>{\"focus\":1}"),
            None
        );
    }

    #[test]
    fn coordinate_annotations_refuse_other_constraints() {
        let error = check_schema(&json!({"x-skywrite":"point","const":[0,0]}))
            .unwrap_err()
            .to_string();
        assert!(error.contains("remove const"), "{error}");
        check_schema(&json!({"x-skywrite":"box","type":"array","description":"face"})).unwrap();
        // A parent const is checked against the clamped value that is returned.
        let parent = json!({"const":{"p":[-0.01,0.5]},"properties":{"p":{"x-skywrite":"point"}}});
        check_schema(&parent).unwrap();
        let checked = validate(Some(&parent), &json!({"p":[-0.01,0.5]}));
        assert!(!checked.errors.is_empty(), "{:?}", checked.value);
        let ok = validate(
            Some(&json!({"properties":{"p":{"x-skywrite":"point"}}})),
            &json!({"p":[-0.01,0.5]}),
        );
        assert!(ok.errors.is_empty());
        assert_eq!(ok.clamped, 1);
        assert_eq!(ok.value, json!({"p":[0.0,0.5]}));
    }

    #[test]
    fn oversized_files_are_refused_through_one_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.txt");
        std::fs::write(&big, "x".repeat(MAX_FOCUS_BYTES + 1)).unwrap();
        assert!(
            normalize_text(&[], Some(&big))
                .unwrap_err()
                .to_string()
                .contains("at most")
        );
        assert!(normalize_text(&[], Some(dir.path())).is_err());
        // A FIFO without a writer is refused instead of blocking the open.
        #[cfg(unix)]
        {
            let fifo = dir.path().join("fifo");
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&fifo)
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(normalize_text(&[], Some(&fifo)).is_err());
            assert!(load_schema(&fifo).is_err());
        }
    }

    #[test]
    fn wire_schema_and_strictness() {
        let wire = wire_schema(&faces());
        let item = &wire["properties"]["faces"]["items"]["properties"]["box"];
        assert_eq!(item["minItems"], 4);
        assert_eq!(
            item["items"],
            json!({"type":"number","minimum":0,"maximum":1})
        );
        assert!(item.get("x-skywrite").is_none());
        assert!(strict_compatible(&wire));
        assert!(!strict_compatible(
            &json!({"type":"object","properties":{"a":{"type":"string"}}})
        ));
        // Strict providers need items on every array.
        assert!(!strict_compatible(&json!({"type":"array"})));
        assert!(strict_compatible(
            &json!({"type":"array","items":{"type":"number"}})
        ));
        // A nullable object is still an object: it must be closed and fully required.
        assert!(!strict_compatible(
            &json!({"type":["object","null"],"properties":{"a":{"type":"string"}}})
        ));
        assert!(strict_compatible(
            &json!({"type":["object","null"],"additionalProperties":false,
            "required":["a"],"properties":{"a":{"type":"string"}}})
        ));
        let focus = Focus {
            text: "faces".into(),
            instructions_sha256: instructions_sha256("faces"),
            schema: Some(faces()),
            schema_sha256: None,
            sha256: String::new(),
        };
        assert_eq!(
            response_format(&focus, false, Some(true)).1,
            "json_schema_strict"
        );
        assert_eq!(response_format(&focus, true, Some(true)).1, "json_schema");
        assert_eq!(response_format(&focus, false, Some(false)).1, "json_object");
        assert_eq!(response_format(&focus, false, None).1, "json_object");
        let plain = Focus {
            schema: None,
            ..focus
        };
        assert_eq!(response_format(&plain, false, Some(true)).1, "json_object");
        assert!(!only_prompt(&plain, "frame", "").contains("shot_size"));
    }

    #[test]
    fn identity_changes_with_text_and_schema_only() {
        let dir = tempfile::tempdir().unwrap();
        let schema = dir.path().join("s.json");
        std::fs::write(&schema, serde_json::to_vec(&faces()).unwrap()).unwrap();
        let a = load(&["faces".into()], None, None).unwrap().unwrap();
        let b = load(&[" faces ".into()], None, None).unwrap().unwrap();
        let c = load(&["faces".into()], None, Some(&schema))
            .unwrap()
            .unwrap();
        let d = load(&["hands".into()], None, None).unwrap().unwrap();
        assert_eq!(a.sha256, b.sha256);
        assert_ne!(a.sha256, c.sha256);
        assert_eq!(a.instructions_sha256, c.instructions_sha256);
        assert_ne!(a.sha256, d.sha256);
        assert!(load(&[], None, Some(&schema)).is_err());
    }
}
