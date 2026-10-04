// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native JSON_SEARCH selection and path-producing walk. Unlike JSON_EXTRACT,
//! an array leg never auto-wraps a non-array. Results are deduplicated across
//! the whole ordered walk, and `one` stops traversal at the first match.

use std::collections::HashSet;

use serde_json::Value as Json;
use tidb_query_datatype::codec::collation::pattern::{MatchOptions, TrailingEscape, matches_runes};

use crate::{
    NativeJsonPath, NativeJsonPathLeg as PathLeg, native_json_array_range, native_json_format,
    native_json_is_ecmascript_identifier,
};

/// Actual SQL mode semantics: ASCII case folding only, without trimming.
pub fn parse_native_json_search_mode(mode: &str) -> Option<bool> {
    if mode.eq_ignore_ascii_case("one") {
        Some(true)
    } else if mode.eq_ignore_ascii_case("all") {
        Some(false)
    } else {
        None
    }
}

/// Frame actual SQL mode, escape scalar and pattern. A failed allocation or
/// checked size is an infrastructure failure for the caller, never SQL NULL.
pub(crate) fn encode_native_json_search_spec(
    one: bool,
    pattern: &str,
    escape: char,
) -> Option<Vec<u8>> {
    let len = 5usize.checked_add(pattern.len())?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).ok()?;
    bytes.push(u8::from(one));
    bytes.extend_from_slice(&(escape as u32).to_le_bytes());
    bytes.extend_from_slice(pattern.as_bytes());
    Some(bytes)
}

/// Validate only the exact header, Unicode scalar and remaining UTF-8 payload.
pub(crate) fn decode_native_json_search_spec(bytes: &[u8]) -> Option<(bool, &str, char)> {
    let header = bytes.get(..5)?;
    let one = match header[0] {
        0 => false,
        1 => true,
        _ => return None,
    };
    let escape = char::from_u32(u32::from_le_bytes(header[1..5].try_into().ok()?))?;
    let pattern = std::str::from_utf8(&bytes[5..]).ok()?;
    Some((one, pattern, escape))
}

pub(crate) fn native_json_search(
    document: &Json,
    one: bool,
    pattern: &str,
    escape: char,
    paths: &[NativeJsonPath],
) -> Option<Vec<u8>> {
    let mut matches = Vec::new();
    if paths.is_empty() {
        walk_search(document, "$", pattern, escape, &mut matches, one);
    } else {
        for path in paths {
            select_search(
                document,
                &path.legs,
                "$".to_string(),
                pattern,
                escape,
                &mut matches,
                one,
            );
            if one && !matches.is_empty() {
                break;
            }
        }
    }
    // Recursive and overlapping selections can revisit nonadjacent leaves.
    // Retaining first occurrences preserves the original global pathSet order.
    let mut seen = HashSet::new();
    matches.retain(|path| seen.insert(path.clone()));
    if matches.is_empty() {
        return None;
    }
    let result = if matches.len() == 1 {
        Json::String(matches.remove(0))
    } else {
        Json::Array(matches.into_iter().map(Json::String).collect())
    };
    Some(native_json_format(&result).into_bytes())
}

fn select_search(
    value: &Json,
    legs: &[PathLeg],
    path: String,
    pattern: &str,
    escape: char,
    output: &mut Vec<String>,
    stop_after_one: bool,
) {
    if stop_after_one && !output.is_empty() {
        return;
    }
    if legs.is_empty() {
        walk_search(value, &path, pattern, escape, output, stop_after_one);
        return;
    }
    match &legs[0] {
        PathLeg::Key(key) => {
            if let Json::Object(object) = value {
                if let Some(child) = object.get(key) {
                    select_search(
                        child,
                        &legs[1..],
                        append_object_path(&path, key),
                        pattern,
                        escape,
                        output,
                        stop_after_one,
                    );
                }
            }
        }
        PathLeg::KeyWildcard => {
            if let Json::Object(object) = value {
                for (key, child) in object {
                    select_search(
                        child,
                        &legs[1..],
                        append_object_path(&path, key),
                        pattern,
                        escape,
                        output,
                        stop_after_one,
                    );
                    if stop_after_one && !output.is_empty() {
                        return;
                    }
                }
            }
        }
        PathLeg::Array(selection) => {
            if let Json::Array(values) = value {
                let (start, end) = native_json_array_range(selection, values.len());
                if start <= end {
                    for (index, child) in
                        values.iter().enumerate().skip(start).take(end - start + 1)
                    {
                        select_search(
                            child,
                            &legs[1..],
                            format!("{path}[{index}]"),
                            pattern,
                            escape,
                            output,
                            stop_after_one,
                        );
                        if stop_after_one && !output.is_empty() {
                            return;
                        }
                    }
                }
            }
            // Deliberately no JSON_EXTRACT-style scalar/object auto-wrap.
        }
        PathLeg::Recursive => {
            select_search(
                value,
                &legs[1..],
                path.clone(),
                pattern,
                escape,
                output,
                stop_after_one,
            );
            if stop_after_one && !output.is_empty() {
                return;
            }
            match value {
                Json::Array(values) => {
                    for (index, child) in values.iter().enumerate() {
                        select_search(
                            child,
                            legs,
                            format!("{path}[{index}]"),
                            pattern,
                            escape,
                            output,
                            stop_after_one,
                        );
                        if stop_after_one && !output.is_empty() {
                            return;
                        }
                    }
                }
                Json::Object(object) => {
                    for (key, child) in object {
                        select_search(
                            child,
                            legs,
                            append_object_path(&path, key),
                            pattern,
                            escape,
                            output,
                            stop_after_one,
                        );
                        if stop_after_one && !output.is_empty() {
                            return;
                        }
                    }
                }
                Json::Null | Json::Bool(_) | Json::Number(_) | Json::String(_) => {}
            }
        }
    }
}

fn walk_search(
    value: &Json,
    path: &str,
    pattern: &str,
    escape: char,
    output: &mut Vec<String>,
    stop_after_one: bool,
) {
    if stop_after_one && !output.is_empty() {
        return;
    }
    match value {
        Json::String(text) => {
            if matches_runes(
                text.as_bytes(),
                pattern.as_bytes(),
                MatchOptions {
                    escape: escape as u32,
                    trailing_escape: TrailingEscape::PrefixLiteral,
                },
            ) {
                output.push(path.to_string());
            }
        }
        Json::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                walk_search(
                    child,
                    &format!("{path}[{index}]"),
                    pattern,
                    escape,
                    output,
                    stop_after_one,
                );
                if stop_after_one && !output.is_empty() {
                    return;
                }
            }
        }
        Json::Object(object) => {
            for (key, child) in object {
                walk_search(
                    child,
                    &append_object_path(path, key),
                    pattern,
                    escape,
                    output,
                    stop_after_one,
                );
                if stop_after_one && !output.is_empty() {
                    return;
                }
            }
        }
        Json::Null | Json::Bool(_) | Json::Number(_) => {}
    }
}

fn append_object_path(path: &str, key: &str) -> String {
    if native_json_is_ecmascript_identifier(key) {
        format!("{path}.{key}")
    } else {
        let encoded = serde_json::to_string(key).expect("string serialization cannot fail");
        format!("{path}.{encoded}")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::parse_native_json_path;

    #[test]
    fn native_json_search_preserves_order_paths_rune_escapes_and_spec_frames() {
        let paths = |values: &[&str]| {
            values
                .iter()
                .map(|text| parse_native_json_path(text).unwrap())
                .collect::<Vec<_>>()
        };
        let document = json!({"a": {"b": "x", "a": "x"}});
        let output = native_json_search(
            &document,
            false,
            "x",
            '\\',
            &paths(&["$**.a", "$.a", "$.a.b"]),
        )
        .unwrap();
        assert_eq!(output, br#"["$.a.a", "$.a.b"]"#);
        let output =
            native_json_search(&document, true, "x", '\\', &paths(&["$.a.b", "$.a.a"])).unwrap();
        assert_eq!(output, br#""$.a.b""#);
        let output = native_json_search(
            &document,
            false,
            "x",
            '\\',
            &paths(&["$.a.b", "$.a.a", "$.a.b"]),
        )
        .unwrap();
        assert_eq!(output, br#"["$.a.b", "$.a.a"]"#);
        let object = json!({"*": "x", "a": "x", "宽": "x"});
        let output = native_json_search(&object, false, "x", '\\', &paths(&["$.\"*\""])).unwrap();
        assert_eq!(
            serde_json::from_slice::<Json>(&output).unwrap(),
            json!("$.\"*\"")
        );
        let output = native_json_search(&object, false, "x", '\\', &paths(&["$.*"])).unwrap();
        assert_eq!(
            serde_json::from_slice::<Json>(&output).unwrap(),
            json!(["$.\"*\"", "$.a", "$.\"宽\""])
        );
        for path in ["$[0].a", "$[*].a", "$[0 to 1].a"] {
            assert_eq!(
                native_json_search(&object, false, "x", '\\', &paths(&[path])),
                None
            );
        }
        let array = json!([{"a": "x"}, {"a": "y"}]);
        let output = native_json_search(&array, false, "%", '\\', &paths(&["$[last].a"])).unwrap();
        assert_eq!(output, br#""$[1].a""#);
        for (text, pattern, escape, hit) in [
            ("_%", "界_界%", '界', true),
            ("界suffix", "界", '界', true),
            ("a\\suffix", "a\\", '\\', true),
            ("a", "a\\", '\\', false),
            ("suffix", "界", '界', false),
            ("宽", "_", '\\', true),
            ("Ä", "ä", '\\', false),
            ("%", "%%", '%', true),
            ("_tail", "_", '_', true),
            ("", "", '\\', true),
            ("", "%", '\\', true),
        ] {
            let output = native_json_search(&json!(text), false, pattern, escape, &[]);
            assert_eq!(
                output,
                hit.then(|| br#""$""#.to_vec()),
                "{text:?} {pattern:?}"
            );
        }
        assert_eq!(
            native_json_search(&json!([1, null, true, {}, []]), false, "%", '\\', &[]),
            None
        );
        assert_eq!(
            native_json_search(&document, false, "no hit", '\\', &[]),
            None
        );
        for (mode, expected) in [
            ("one", Some(true)),
            ("OnE", Some(true)),
            ("ALL", Some(false)),
            (" all", None),
            ("", None),
        ] {
            assert_eq!(parse_native_json_search_mode(mode), expected);
        }
        for one in [false, true] {
            for (pattern, escape) in [("", '\0'), ("界_%\0", '界'), ("tail\\", '\\')] {
                let spec = encode_native_json_search_spec(one, pattern, escape).unwrap();
                assert_eq!(spec[0], u8::from(one));
                assert_eq!(&spec[1..5], &(escape as u32).to_le_bytes());
                assert_eq!(
                    decode_native_json_search_spec(&spec),
                    Some((one, pattern, escape))
                );
            }
        }
        for spec in [
            vec![],
            vec![0, 0, 0, 0],
            vec![2, 0, 0, 0, 0],
            vec![0, 0, 0xd8, 0, 0],
            vec![0, 0, 0, 0x11, 0],
            vec![1, 0, 0, 0, 0, 255],
        ] {
            assert_eq!(decode_native_json_search_spec(&spec), None);
        }
    }
}
