// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Original native serde JSON mutations, separate from wire binary JSON policy.
//! Callers own SQL argument preparation and path validation. Mutations apply in
//! source order directly to the document, without intermediate encoding.

use serde_json::Value as Json;

use crate::native_json::{
    NativeJsonArraySelection as ArraySelection, NativeJsonPath, NativeJsonPathLeg as PathLeg,
};

/// Native `JSON_SET`, `JSON_INSERT`, and `JSON_REPLACE` traversal policy.
#[derive(Clone, Copy)]
pub enum NativeJsonModifyMode {
    Set,
    Insert,
    Replace,
}

/// Applies prepared path/value pairs in order, zipped to the shorter list.
pub fn native_json_modify(
    mut document: Json,
    paths: &[NativeJsonPath],
    values: Vec<Json>,
    mode: NativeJsonModifyMode,
) -> Json {
    for (path, value) in paths.iter().zip(values) {
        let exists = descend_exact_mut(&mut document, &path.legs).is_some();
        match mode {
            NativeJsonModifyMode::Set => {
                if let Some(target) = descend_exact_mut(&mut document, &path.legs) {
                    *target = value;
                } else {
                    insert_missing_path(&mut document, &path.legs, &value);
                }
            }
            NativeJsonModifyMode::Insert if !exists => {
                insert_missing_path(&mut document, &path.legs, &value);
            }
            NativeJsonModifyMode::Replace if exists => {
                if let Some(target) = descend_exact_mut(&mut document, &path.legs) {
                    *target = value;
                }
            }
            NativeJsonModifyMode::Insert | NativeJsonModifyMode::Replace => {}
        }
    }
    document
}

/// Removes prepared exact paths in order, without scalar array autowrapping.
pub fn native_json_remove(mut document: Json, paths: &[NativeJsonPath]) -> Json {
    for path in paths {
        remove_path(&mut document, &path.legs);
    }
    document
}

/// Appends prepared values at exact paths, wrapping existing non-array targets.
pub fn native_json_array_append(
    mut document: Json,
    paths: &[NativeJsonPath],
    values: Vec<Json>,
) -> Json {
    for (path, value) in paths.iter().zip(values) {
        append_at_path(&mut document, &path.legs, &value);
    }
    document
}

/// Inserts prepared values before array cells, clamping each insertion index.
pub fn native_json_array_insert(
    mut document: Json,
    paths: &[NativeJsonPath],
    values: Vec<Json>,
) -> Json {
    for (path, value) in paths.iter().zip(values) {
        if let Some(PathLeg::Array(ArraySelection::Index(index))) = path.legs.last() {
            insert_at_path(&mut document, &path.legs, *index, &value);
        }
    }
    document
}

fn insert_missing_path(document: &mut Json, legs: &[PathLeg], value: &Json) {
    let Some((last, parent_legs)) = legs.split_last() else {
        *document = value.clone();
        return;
    };
    let Some(parent) = descend_exact_mut(document, parent_legs) else {
        return;
    };
    match last {
        PathLeg::Key(key) => {
            if let Json::Object(object) = parent {
                object.insert(key.clone(), value.clone());
            }
        }
        PathLeg::Array(ArraySelection::Index(_)) => match parent {
            Json::Array(values) => values.push(value.clone()),
            _ => {
                let original = std::mem::replace(parent, Json::Null);
                *parent = Json::Array(vec![original, value.clone()]);
            }
        },
        _ => {}
    }
}

fn append_at_path(value: &mut Json, legs: &[PathLeg], appended: &Json) -> bool {
    let Some((first, rest)) = legs.split_first() else {
        append_json_value(value, appended);
        return true;
    };
    match first {
        PathLeg::Key(key) => match value {
            Json::Object(object) => object
                .get_mut(key)
                .is_some_and(|child| append_at_path(child, rest, appended)),
            _ => false,
        },
        PathLeg::Array(ArraySelection::Index(index)) => match value {
            Json::Array(values) => {
                let Some(index) = resolve_array_index(*index, values.len()) else {
                    return false;
                };
                append_at_path(&mut values[index], rest, appended)
            }
            // BinaryJSON.Extract treats [0] and [last] as selecting a scalar
            // itself.  Preserve that source behavior for nested append paths.
            _ if *index == 0 || *index == -1 => append_at_path(value, rest, appended),
            _ => false,
        },
        _ => false,
    }
}

fn append_json_value(target: &mut Json, appended: &Json) {
    if let Json::Array(values) = target {
        values.push(appended.clone());
        return;
    }
    let original = std::mem::replace(target, Json::Null);
    *target = Json::Array(vec![original, appended.clone()]);
}

fn insert_at_path(document: &mut Json, legs: &[PathLeg], index: i64, value: &Json) -> bool {
    let Some((PathLeg::Array(ArraySelection::Index(_)), parent_legs)) = legs.split_last() else {
        return false;
    };
    let Some(parent) = descend_exact_mut(document, parent_legs) else {
        return false;
    };
    let Json::Array(values) = parent else {
        return false;
    };
    let len = i64::try_from(values.len()).unwrap_or(i64::MAX);
    let index = if index < 0 {
        len.saturating_add(index).max(0)
    } else {
        index
    }
    .min(len) as usize;
    values.insert(index, value.clone());
    true
}

fn descend_exact_mut<'a>(value: &'a mut Json, legs: &[PathLeg]) -> Option<&'a mut Json> {
    let Some((first, rest)) = legs.split_first() else {
        return Some(value);
    };
    // `BinaryJSON.Extract`: `[0]` and `[last]` select a NON-ARRAY value
    // itself. The array check has to come first -- reading it as a self
    // selection for an array too would make `$[0]` on `[1, 2]` name the
    // whole array instead of its first element.
    if !matches!(value, Json::Array(_))
        && matches!(first, PathLeg::Array(ArraySelection::Index(index)) if *index == 0 || *index == -1)
    {
        return descend_exact_mut(value, rest);
    }
    match first {
        PathLeg::Key(key) => {
            let Json::Object(object) = value else {
                return None;
            };
            object
                .get_mut(key)
                .and_then(|child| descend_exact_mut(child, rest))
        }
        PathLeg::Array(ArraySelection::Index(index)) => {
            let Json::Array(values) = value else {
                return None;
            };
            let index = resolve_array_index(*index, values.len())?;
            descend_exact_mut(&mut values[index], rest)
        }
        _ => None,
    }
}

fn remove_path(value: &mut Json, legs: &[PathLeg]) -> bool {
    let Some((first, rest)) = legs.split_first() else {
        return false;
    };
    if rest.is_empty() {
        return match (value, first) {
            (Json::Object(object), PathLeg::Key(key)) => object.remove(key).is_some(),
            (Json::Array(values), PathLeg::Array(ArraySelection::Index(index))) => {
                let Some(index) = resolve_array_index(*index, values.len()) else {
                    return false;
                };
                values.remove(index);
                true
            }
            _ => false,
        };
    }

    match (value, first) {
        (Json::Object(object), PathLeg::Key(key)) => object
            .get_mut(key)
            .is_some_and(|child| remove_path(child, rest)),
        (Json::Array(values), PathLeg::Array(ArraySelection::Index(index))) => {
            let Some(index) = resolve_array_index(*index, values.len()) else {
                return false;
            };
            remove_path(&mut values[index], rest)
        }
        _ => false,
    }
}

fn resolve_array_index(index: i64, len: usize) -> Option<usize> {
    let len = i64::try_from(len).ok()?;
    let index = if index < 0 {
        len.checked_add(index)?
    } else {
        index
    };
    (index >= 0 && index < len).then_some(index as usize)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::native_json::parse_native_json_path;

    fn paths(inputs: &[&str]) -> Vec<NativeJsonPath> {
        inputs
            .iter()
            .map(|input| parse_native_json_path(input).unwrap())
            .collect()
    }

    #[test]
    fn test_native_modify_root_missing_parent_and_zip() {
        use NativeJsonModifyMode::{Insert, Replace, Set};

        assert_eq!(
            native_json_modify(
                json!({"a": 1}),
                &paths(&["$", "$.a", "$.new"]),
                vec![json!(9), json!(2), Json::Null],
                Insert,
            ),
            json!({"a": 1, "new": null}),
        );
        assert_eq!(
            native_json_modify(
                json!({"a": 1}),
                &paths(&["$.missing.x", "$.a[9]", "$.ignored"]),
                vec![json!(2), Json::Null],
                Set,
            ),
            json!({"a": [1, null]}),
        );
        assert_eq!(
            native_json_modify(
                json!({"a": 1}),
                &paths(&["$.missing", "$.a[last]", "$.a[9]"]),
                vec![json!(2), Json::Null, json!(3)],
                Replace,
            ),
            json!({"a": null}),
        );
        assert_eq!(
            native_json_modify(
                json!({"a": 1}),
                &paths(&["$", "$[0]"]),
                vec![json!([1, 2]), Json::Null, json!(99)],
                Set,
            ),
            json!([null, 2]),
        );
    }

    #[test]
    fn test_native_array_ordered_shifts_autowrap_and_negative_insert() {
        assert_eq!(
            native_json_remove(json!([0, 1, 2, 3]), &paths(&["$[0]", "$[1]"])),
            json!([1, 3]),
        );
        assert_eq!(
            native_json_array_append(
                json!({"a": 1}),
                &paths(&["$.a[0]", "$.missing", "$.a"]),
                vec![Json::Null, json!(9), json!(2)],
            ),
            json!({"a": [1, null, 2]}),
        );
        assert_eq!(
            native_json_array_append(json!(1), &paths(&["$[last]"]), vec![Json::Null]),
            json!([1, null]),
        );
        assert_eq!(
            native_json_remove(json!({"a": 1}), &paths(&["$[0].a", "$[last].a"])),
            json!({"a": 1}),
        );
        assert_eq!(
            native_json_array_insert(
                json!([1, 2]),
                &paths(&["$[last-9]", "$[1]", "$[99]"]),
                vec![json!(0), Json::Null, json!(3)],
            ),
            json!([0, null, 1, 2, 3]),
        );
        assert_eq!(
            native_json_array_insert(
                json!({"a": 1}),
                &paths(&["$.a[0]", "$.missing[0]"]),
                vec![Json::Null, json!(2)],
            ),
            json!({"a": 1}),
        );
    }
}
