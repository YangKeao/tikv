// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native serde-value JSON predicates and paths. These deliberately do not use
//! the binary JSON comparator's mixed-number epsilon or its lossless objects.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashSet},
};

use serde_json::{Number, Value as Json};

/// Original native parser's rune position, without a frontend SQL error type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeJsonPathError {
    pub position: usize,
}

/// Parsed native path; existing mutation/search callers share these same legs.
#[derive(Debug)]
pub struct NativeJsonPath {
    pub legs: Vec<NativeJsonPathLeg>,
    pub could_match_multiple: bool,
}

#[derive(Debug)]
pub enum NativeJsonPathLeg {
    Key(String),
    KeyWildcard,
    Array(NativeJsonArraySelection),
    Recursive,
}

#[derive(Debug)]
pub enum NativeJsonArraySelection {
    All,
    Index(i64),
    Range(i64, i64),
}

use NativeJsonArraySelection as ArraySelection;
use NativeJsonPath as JsonPath;
use NativeJsonPathLeg as PathLeg;

/// Parses the original native grammar, including its rune-indexed errors.
pub fn parse_native_json_path(input: &str) -> Result<JsonPath, NativeJsonPathError> {
    let chars: Vec<char> = input.chars().collect();
    let mut cursor = 0;
    skip_space(&chars, &mut cursor);
    if chars.get(cursor) != Some(&'$') {
        return Err(path_error(1));
    }
    cursor += 1;
    skip_space(&chars, &mut cursor);
    let mut legs = Vec::new();
    let mut could_match_multiple = false;
    while cursor < chars.len() {
        match chars[cursor] {
            '.' => {
                cursor += 1;
                skip_space(&chars, &mut cursor);
                if chars.get(cursor) == Some(&'*') {
                    cursor += 1;
                    legs.push(PathLeg::KeyWildcard);
                    could_match_multiple = true;
                } else {
                    let key = parse_member(&chars, &mut cursor)?;
                    legs.push(PathLeg::Key(key));
                }
            }
            '[' => {
                cursor += 1;
                skip_space(&chars, &mut cursor);
                let selection = if chars.get(cursor) == Some(&'*') {
                    cursor += 1;
                    could_match_multiple = true;
                    ArraySelection::All
                } else {
                    let start = parse_index(&chars, &mut cursor)?;
                    let after_start = cursor;
                    skip_space(&chars, &mut cursor);
                    if after_start != cursor && read_word(&chars, &mut cursor, "to") {
                        if cursor >= chars.len() || !chars[cursor].is_whitespace() {
                            return Err(path_error(cursor + 1));
                        }
                        skip_space(&chars, &mut cursor);
                        let end = parse_index(&chars, &mut cursor)?;
                        if (start >= 0 && end >= 0 || start < 0 && end < 0) && start > end {
                            return Err(path_error(cursor + 1));
                        }
                        could_match_multiple = true;
                        ArraySelection::Range(start, end)
                    } else {
                        cursor = after_start;
                        ArraySelection::Index(start)
                    }
                };
                skip_space(&chars, &mut cursor);
                if chars.get(cursor) != Some(&']') {
                    return Err(path_error(cursor + 1));
                }
                cursor += 1;
                legs.push(PathLeg::Array(selection));
            }
            '*' => {
                if chars.get(cursor + 1) != Some(&'*') || chars.get(cursor + 2) == Some(&'*') {
                    return Err(path_error(cursor + 1));
                }
                cursor += 2;
                legs.push(PathLeg::Recursive);
                could_match_multiple = true;
            }
            _ => return Err(path_error(cursor)),
        }
        skip_space(&chars, &mut cursor);
    }
    if matches!(legs.last(), Some(PathLeg::Recursive)) {
        return Err(path_error(cursor + 1));
    }
    Ok(JsonPath {
        legs,
        could_match_multiple,
    })
}

fn path_error(position: usize) -> NativeJsonPathError {
    NativeJsonPathError { position }
}

fn skip_space(chars: &[char], cursor: &mut usize) {
    while chars.get(*cursor).is_some_and(|ch| ch.is_whitespace()) {
        *cursor += 1;
    }
}

fn read_word(chars: &[char], cursor: &mut usize, expected: &str) -> bool {
    let saved = *cursor;
    for expected_char in expected.chars() {
        if chars.get(*cursor) != Some(&expected_char) {
            *cursor = saved;
            return false;
        }
        *cursor += 1;
    }
    true
}

fn parse_member(chars: &[char], cursor: &mut usize) -> Result<String, NativeJsonPathError> {
    if chars.get(*cursor) == Some(&'"') {
        let start = *cursor;
        *cursor += 1;
        let mut escaped = false;
        while let Some(ch) = chars.get(*cursor) {
            *cursor += 1;
            if escaped {
                escaped = false;
            } else if *ch == '\\' {
                escaped = true;
            } else if *ch == '"' {
                let encoded: String = chars[start..*cursor].iter().collect();
                return serde_json::from_str(&encoded).map_err(|_| path_error(*cursor));
            }
        }
        return Err(path_error(*cursor));
    }
    let start = *cursor;
    while chars
        .get(*cursor)
        .is_some_and(|ch| !ch.is_whitespace() && *ch != '.' && *ch != '[' && *ch != '*')
    {
        *cursor += 1;
    }
    let key: String = chars[start..*cursor].iter().collect();
    if !native_json_is_ecmascript_identifier(&key) {
        return Err(path_error(*cursor));
    }
    Ok(key)
}

/// The original native capability boundary: unquoted ASCII identifiers only.
/// Non-ASCII keys must remain quoted; do not broaden this to Unicode letters.
pub fn native_json_is_ecmascript_identifier(value: &str) -> bool {
    if !value.is_ascii() {
        return false;
    }
    let bytes = value.as_bytes();
    let Some(&first) = bytes.first() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'$' || first == b'_') {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|&byte| byte.is_ascii_alphanumeric() || byte == b'$' || byte == b'_')
}

fn parse_index(chars: &[char], cursor: &mut usize) -> Result<i64, NativeJsonPathError> {
    skip_space(chars, cursor);
    if read_word(chars, cursor, "last") {
        skip_space(chars, cursor);
        if chars.get(*cursor) != Some(&'-') {
            return Ok(-1);
        }
        *cursor += 1;
        skip_space(chars, cursor);
        let amount = parse_u32(chars, cursor)?;
        return Ok(-1 - i64::from(amount));
    }
    Ok(i64::from(parse_u32(chars, cursor)?))
}

fn parse_u32(chars: &[char], cursor: &mut usize) -> Result<u32, NativeJsonPathError> {
    let start = *cursor;
    while chars.get(*cursor).is_some_and(char::is_ascii_digit) {
        *cursor += 1;
    }
    if start == *cursor {
        return Err(path_error(*cursor));
    }
    chars[start..*cursor]
        .iter()
        .collect::<String>()
        .parse()
        .map_err(|_| path_error(*cursor))
}

/// Extracts using the source's per-path pointer-identity deduplication and
/// single-path/single-value autowrap rule. JSON output encoding stays external.
pub fn native_json_extract(document: &Json, paths: &[JsonPath]) -> Option<Json> {
    let mut matches = Vec::new();
    for path in paths {
        let mut seen = HashSet::new();
        collect(document, &path.legs, &mut matches, &mut seen);
    }
    if matches.is_empty() {
        return None;
    }
    if paths.len() == 1 && matches.len() == 1 && !paths[0].could_match_multiple {
        return Some(matches.remove(0).clone());
    }
    Some(Json::Array(matches.into_iter().cloned().collect()))
}

fn collect<'a>(
    value: &'a Json,
    legs: &[PathLeg],
    output: &mut Vec<&'a Json>,
    seen: &mut HashSet<usize>,
) {
    if legs.is_empty() {
        let identity = value as *const Json as usize;
        if seen.insert(identity) {
            output.push(value);
        }
        return;
    }
    match &legs[0] {
        PathLeg::Key(key) => {
            if let Json::Object(object) = value {
                if let Some(child) = object.get(key) {
                    collect(child, &legs[1..], output, seen);
                }
            }
        }
        PathLeg::KeyWildcard => {
            if let Json::Object(object) = value {
                for child in object.values() {
                    collect(child, &legs[1..], output, seen);
                }
            }
        }
        PathLeg::Array(selection) => match value {
            Json::Array(values) => {
                let (start, end) = native_json_array_range(selection, values.len());
                if start <= end {
                    for child in &values[start..=end] {
                        collect(child, &legs[1..], output, seen);
                    }
                }
            }
            _ if select_non_array(selection) => collect(value, &legs[1..], output, seen),
            _ => {}
        },
        PathLeg::Recursive => {
            collect(value, &legs[1..], output, seen);
            match value {
                Json::Array(values) => {
                    for child in values {
                        collect(child, legs, output, seen);
                    }
                }
                Json::Object(object) => {
                    for child in object.values() {
                        collect(child, legs, output, seen);
                    }
                }
                Json::Null | Json::Bool(_) | Json::Number(_) | Json::String(_) => {}
            }
        }
    }
}

/// Shares the native array-index/range arithmetic with existing path callers.
pub fn native_json_array_range(selection: &ArraySelection, len: usize) -> (usize, usize) {
    let len = len as i64;
    let index = |value: i64| if value < 0 { len + value } else { value };
    let clamp_end = |value: i64| value.min(len - 1);
    let (start, end) = match *selection {
        ArraySelection::All => (0, len - 1),
        ArraySelection::Index(index_value) => (index(index_value), clamp_end(index(index_value))),
        ArraySelection::Range(start, end) => (index(start), clamp_end(index(end))),
    };
    if start < 0 || end < 0 {
        (1, 0)
    } else {
        (start as usize, end as usize)
    }
}

// SEARCH deliberately does not share this extraction-only non-array rule.
fn select_non_array(selection: &ArraySelection) -> bool {
    match *selection {
        ArraySelection::Index(index) => index == 0 || index == -1,
        ArraySelection::Range(start, end) => start == 0 && end >= -1,
        ArraySelection::All => false,
    }
}

/// Constructs a native JSON array from actual, already-coerced arguments.
pub fn native_json_array(values: Vec<Json>) -> Json {
    Json::Array(values)
}

/// Constructs a native object in input order; repeated keys keep the last
/// value.
pub fn native_json_object(pairs: Vec<(String, Json)>) -> Json {
    let mut object = serde_json::Map::new();
    for (key, value) in pairs {
        object.insert(key, value);
    }
    Json::Object(object)
}

/// Native expression KEYS returns SQL NULL for a selected non-object value.
pub fn native_json_keys(value: &Json) -> Option<Json> {
    let Json::Object(object) = value else {
        return None;
    };
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    Some(Json::Array(
        keys.into_iter()
            .map(|key| Json::String(key.to_owned()))
            .collect(),
    ))
}

/// Native JSON_PRETTY preserves the original sorted-key, two-space layout and
/// delegates scalar spelling to the same formatter as JSON result codecs.
pub fn native_json_pretty(value: &Json) -> String {
    format_json_pretty(value, 0)
}

fn format_json_pretty(value: &Json, level: usize) -> String {
    let indent = |level: usize| "  ".repeat(level);
    match value {
        Json::Null | Json::Bool(_) | Json::Number(_) | Json::String(_) => native_json_format(value),
        Json::Array(values) => {
            if values.is_empty() {
                return "[]".to_string();
            }
            let children = values
                .iter()
                .map(|value| {
                    format!(
                        "{}{}",
                        indent(level + 1),
                        format_json_pretty(value, level + 1)
                    )
                })
                .collect::<Vec<_>>();
            format!("[\n{}\n{}]", children.join(",\n"), indent(level))
        }
        Json::Object(object) => {
            if object.is_empty() {
                return "{}".to_string();
            }
            let sorted: BTreeMap<&str, &Json> = object
                .iter()
                .map(|(key, value)| (key.as_str(), value))
                .collect();
            let children = sorted
                .into_iter()
                .map(|(key, value)| {
                    let key = serde_json::to_string(key).expect("string serialization cannot fail");
                    format!(
                        "{}{}: {}",
                        indent(level + 1),
                        key,
                        format_json_pretty(value, level + 1)
                    )
                })
                .collect::<Vec<_>>();
            format!("{{\n{}\n{}}}", children.join(",\n"), indent(level))
        }
    }
}

/// Original native JSON result text: spaces after separators, byte-sorted
/// object keys, serde string escaping, and TiDB's floating-number spelling.
pub fn native_json_format(value: &Json) -> String {
    match value {
        Json::Null => "null".to_string(),
        Json::Bool(boolean) => boolean.to_string(),
        Json::Number(number) => format_json_number(number),
        Json::String(string) => {
            serde_json::to_string(string).expect("string serialization cannot fail")
        }
        Json::Array(values) => {
            let values = values.iter().map(native_json_format).collect::<Vec<_>>();
            format!("[{}]", values.join(", "))
        }
        Json::Object(object) => {
            let sorted: BTreeMap<&str, &Json> = object
                .iter()
                .map(|(key, value)| (key.as_str(), value))
                .collect();
            let values = sorted
                .into_iter()
                .map(|(key, value)| {
                    let key = serde_json::to_string(key).expect("string serialization cannot fail");
                    format!("{key}: {}", native_json_format(value))
                })
                .collect::<Vec<_>>();
            format!("{{{}}}", values.join(", "))
        }
    }
}

fn format_json_number(number: &Number) -> String {
    if let Some(integer) = number.as_i64() {
        return integer.to_string();
    }
    if let Some(integer) = number.as_u64() {
        return integer.to_string();
    }
    let float = number
        .as_f64()
        .expect("serde JSON numbers are finite f64 here");
    format_binary_json_float(float)
}

fn format_binary_json_float(value: f64) -> String {
    let abs = value.abs();
    if abs != 0.0 && !(1e-15..1e15).contains(&abs) {
        let mut rendered = format!("{value:e}");
        if let Some(exponent) = rendered.find('e') {
            let exponent_part = &rendered[exponent + 1..];
            let cleaned = exponent_part
                .strip_prefix('+')
                .unwrap_or(exponent_part)
                .strip_prefix("-0")
                .map_or_else(
                    || exponent_part.trim_start_matches('+').to_string(),
                    |rest| format!("-{rest}"),
                );
            rendered.truncate(exponent + 1);
            rendered.push_str(&cleaned);
        }
        return rendered;
    }
    let mut rendered = value.to_string();
    if !rendered.contains('.') {
        rendered.push_str(".0");
    }
    rendered
}

/// Native structural containment on the already-coerced serde value domain.
pub fn native_json_contains(document: &Json, candidate: &Json) -> bool {
    match document {
        Json::Object(object) => match candidate {
            Json::Object(candidate) => candidate.iter().all(|(key, value)| {
                object
                    .get(key)
                    .is_some_and(|document| native_json_contains(document, value))
            }),
            _ => false,
        },
        Json::Array(values) => match candidate {
            Json::Array(candidate) => candidate
                .iter()
                .all(|value| native_json_contains(document, value)),
            _ => values
                .iter()
                .any(|value| native_json_contains(value, candidate)),
        },
        _ => native_json_equal(document, candidate),
    }
}

/// Overlap compares whole values one level down, not recursive containment.
pub fn native_json_overlaps(left: &Json, right: &Json) -> bool {
    if !matches!(left, Json::Array(_)) && matches!(right, Json::Array(_)) {
        return native_json_overlaps(right, left);
    }
    match left {
        Json::Object(object) => match right {
            Json::Object(right) => right.iter().any(|(key, value)| {
                object
                    .get(key)
                    .is_some_and(|left| native_json_equal(left, value))
            }),
            _ => false,
        },
        Json::Array(values) => match right {
            Json::Array(right) => values
                .iter()
                .any(|left| right.iter().any(|right| native_json_equal(left, right))),
            _ => values.iter().any(|left| native_json_equal(left, right)),
        },
        _ => native_json_equal(left, right),
    }
}

/// Candidate-first MEMBER OF, after the caller's asymmetric argument coercion.
pub fn native_json_member_of(candidate: &Json, document: &Json) -> bool {
    match document {
        Json::Array(values) => values
            .iter()
            .any(|value| native_json_equal(value, candidate)),
        value => native_json_equal(value, candidate),
    }
}

/// Length of the selected serde value; a JSON null is a scalar of length one.
pub fn native_json_length(value: &Json) -> i64 {
    let len = match value {
        Json::Array(values) => values.len(),
        Json::Object(values) => values.len(),
        Json::Null | Json::Bool(_) | Json::Number(_) | Json::String(_) => 1,
    };
    len as i64
}

/// Native serde equality: exact integer comparisons and plain floating-point
/// partial_cmp on mixed numbers, with no raw-BinaryJSON epsilon admission.
pub fn native_json_equal(left: &Json, right: &Json) -> bool {
    match (left, right) {
        (Json::Null, Json::Null) => true,
        (Json::Bool(left), Json::Bool(right)) => left == right,
        (Json::Number(left), Json::Number(right)) => {
            compare_json_numbers(left, right) == Ordering::Equal
        }
        (Json::String(left), Json::String(right)) => left.as_bytes() == right.as_bytes(),
        (Json::Array(left), Json::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| native_json_equal(left, right))
        }
        (Json::Object(left), Json::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| native_json_equal(left, right))
                })
        }
        _ => false,
    }
}

fn compare_json_numbers(left: &Number, right: &Number) -> Ordering {
    match (left.as_i64(), left.as_u64(), right.as_i64(), right.as_u64()) {
        (Some(left), _, Some(right), _) => left.cmp(&right),
        (Some(left), _, _, Some(right)) => compare_signed_unsigned(left, right),
        (_, Some(left), Some(right), _) => compare_signed_unsigned(right, left).reverse(),
        (_, Some(left), _, Some(right)) => left.cmp(&right),
        _ => left
            .as_f64()
            .zip(right.as_f64())
            .and_then(|(left, right)| left.partial_cmp(&right))
            .unwrap_or(Ordering::Equal),
    }
}

fn compare_signed_unsigned(left: i64, right: u64) -> Ordering {
    if left < 0 {
        Ordering::Less
    } else {
        (left as u64).cmp(&right)
    }
}

#[cfg(test)]
mod output_tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn constructors_keys_and_formatting_preserve_native_output_policy() {
        assert_eq!(native_json_format(&native_json_array(vec![])), "[]");
        assert_eq!(native_json_pretty(&native_json_object(vec![])), "{}");
        let object = native_json_object(vec![
            ("z".to_owned(), json!([])),
            (
                "a".to_owned(),
                native_json_array(vec![Json::Null, json!(-0.0), json!(1.0)]),
            ),
            ("z".to_owned(), json!(true)),
        ]);
        assert_eq!(native_json_keys(&object), Some(json!(["a", "z"])));
        assert_eq!(native_json_keys(&json!({})), Some(json!([])));
        assert_eq!(native_json_keys(&Json::Null), None);
        assert_eq!(native_json_keys(&json!([])), None);
        assert_eq!(
            native_json_format(&object),
            "{\"a\": [null, -0.0, 1.0], \"z\": true}"
        );
        assert_eq!(
            native_json_pretty(&object),
            "{\n  \"a\": [\n    null,\n    -0.0,\n    1.0\n  ],\n  \"z\": true\n}"
        );
        assert_eq!(
            native_json_format(&json!([1e-16, 1e-15, 1e14, 1e15])),
            "[1e-16, 0.000000000000001, 100000000000000.0, 1e15]"
        );
        let separators = format!("<>&{}{}", '\u{2028}', '\u{2029}');
        assert_eq!(
            native_json_format(&Json::String(separators.clone())),
            format!("\"{separators}\"")
        );
        assert_eq!(native_json_pretty(&json!([[], {}])), "[\n  [],\n  {}\n]");
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn native_serde_predicates_and_paths_keep_their_original_policy() {
        assert!(native_json_equal(&json!(1), &json!(1.0)));
        assert!(!native_json_equal(&json!(1), &json!(1.000000005)));
        assert!(!native_json_equal(&json!(true), &json!(1)));
        assert!(native_json_contains(
            &json!([{"a": [1, 2]}]),
            &json!({"a": [2]})
        ));
        assert!(!native_json_overlaps(
            &json!({"a": [1, 2]}),
            &json!({"a": [2]})
        ));
        assert!(native_json_member_of(&json!(1.0), &json!([1])));
        assert!(!native_json_member_of(&json!("1"), &json!([1])));
        assert_eq!(native_json_length(&Json::Null), 1);
        let duplicate: Json = serde_json::from_str(r#"{"a":1,"a":2}"#).unwrap();
        assert_eq!(native_json_length(&duplicate), 1);
        assert!(native_json_contains(&duplicate, &json!({"a":2})));
        assert!(!native_json_is_ecmascript_identifier("宽"));
        assert_eq!(parse_native_json_path("$.宽").unwrap_err().position, 3);
        let path = parse_native_json_path("$.\"宽\"[last]").unwrap();
        assert_eq!(
            native_json_extract(&json!({"宽":[1,2]}), &[path]),
            Some(json!(2))
        );
        let recursive = parse_native_json_path("$**.a").unwrap();
        assert!(recursive.could_match_multiple);
        assert_eq!(
            native_json_extract(&json!({"a":1,"b":{"a":2}}), &[recursive]),
            Some(json!([1, 2]))
        );
        assert_eq!(
            native_json_extract(&json!(4), &[parse_native_json_path("$[last]").unwrap()]),
            Some(json!(4))
        );
        assert_eq!(
            native_json_extract(&json!([]), &[parse_native_json_path("$[0]").unwrap()]),
            None
        );
    }
}
