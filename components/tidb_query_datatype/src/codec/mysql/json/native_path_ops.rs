// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Lossless native SDK path operations. These are distinct from both wire JSON
//! paths and the native expression serde-value policy. Codecs stay with
//! callers.

use std::collections::HashSet;

use super::native_policy::NativeJsonNode;

/// Selection inside one native SDK JSON array path leg.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeBinaryJsonArraySelection {
    /// Select every element.
    Asterisk,
    /// Select one zero-based index; negative values count from the end.
    Index(i64),
    /// Select an inclusive range.
    Range {
        /// Inclusive first index.
        start: i64,
        /// Inclusive last index.
        end: i64,
    },
}

/// Native SDK path leg. A key named `*` retains the source's wildcard
/// conflation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeBinaryJsonPathLeg {
    Key(String),
    Array(NativeBinaryJsonArraySelection),
    DoubleAsterisk,
}

/// The original native SDK modifier modes and variant order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeBinaryJsonModifyType {
    Insert,
    Replace,
    Set,
}

use NativeBinaryJsonArraySelection as Selection;
use NativeBinaryJsonModifyType as ModifyType;
use NativeBinaryJsonPathLeg as Leg;
use NativeJsonNode as Node;

/// Extracts with one identity set across all paths. The per-path boolean is the
/// ORIGINAL parsed multiple-selection flag, not a flag reconstructed from legs:
/// quoted `"*"` and an asterisk can have identical legs but different flags.
pub fn extract_native_json_node<T: Clone>(
    root: &Node<T>,
    paths: &[(&[Leg], bool)],
) -> Option<Node<T>> {
    let mut matches = Vec::new();
    let mut seen = HashSet::new();
    for (legs, _) in paths {
        extract_value(root, legs, &mut matches, &mut seen);
    }
    if matches.is_empty() {
        return None;
    }
    if paths.len() == 1 && !paths[0].1 && matches.len() == 1 {
        return Some(matches.remove(0).clone());
    }
    Some(Node::Array(matches.into_iter().cloned().collect()))
}

fn extract_value<'a, T>(
    value: &'a Node<T>,
    legs: &[Leg],
    output: &mut Vec<&'a Node<T>>,
    seen: &mut HashSet<*const Node<T>>,
) {
    let Some((leg, remain)) = legs.split_first() else {
        if seen.insert(value) {
            output.push(value);
        }
        return;
    };
    match leg {
        Leg::Key(key) if key == "*" => {
            if let Node::Object(values) = value {
                let mut entries = values.iter().collect::<Vec<_>>();
                entries.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
                for (_, value) in entries {
                    extract_value(value, remain, output, seen);
                }
            }
        }
        Leg::Key(key) => {
            if let Node::Object(values) = value {
                if let Some((_, value)) = values.iter().find(|(name, _)| name == key) {
                    extract_value(value, remain, output, seen);
                }
            }
        }
        Leg::Array(selection) => {
            if let Node::Array(values) = value {
                for index in selected_indices(selection, values.len()) {
                    extract_value(&values[index], remain, output, seen);
                }
            } else if autowraps_non_array(selection) {
                extract_value(value, remain, output, seen);
            }
        }
        Leg::DoubleAsterisk => {
            extract_value(value, remain, output, seen);
            match value {
                Node::Array(values) => {
                    for value in values {
                        extract_descendants(value, remain, output, seen);
                    }
                }
                Node::Object(values) => {
                    let mut entries = values.iter().collect::<Vec<_>>();
                    entries
                        .sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
                    for (_, value) in entries {
                        extract_descendants(value, remain, output, seen);
                    }
                }
                _ => {}
            }
        }
    }
}

fn extract_descendants<'a, T>(
    value: &'a Node<T>,
    remain: &[Leg],
    output: &mut Vec<&'a Node<T>>,
    seen: &mut HashSet<*const Node<T>>,
) {
    extract_value(value, remain, output, seen);
    match value {
        Node::Array(values) => {
            for value in values {
                extract_descendants(value, remain, output, seen);
            }
        }
        Node::Object(values) => {
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
            for (_, value) in entries {
                extract_descendants(value, remain, output, seen);
            }
        }
        _ => {}
    }
}

/// Callback-style selection used by extract_matches and walk/search roots.
/// Unlike extraction, this never autowraps a scalar and does not deduplicate.
/// Returned paths contain the actual selected keys and absolute array indices.
pub fn select_native_json_nodes<'a, T>(
    root: &'a Node<T>,
    legs: &[Leg],
) -> Vec<(Vec<Leg>, &'a Node<T>)> {
    let mut output = Vec::new();
    select_walk_roots(root, legs, Vec::new(), &mut output);
    output
}

fn append_leg(path: &[Leg], leg: Leg) -> Vec<Leg> {
    let mut path = path.to_vec();
    path.push(leg);
    path
}

fn select_walk_roots<'a, T>(
    value: &'a Node<T>,
    legs: &[Leg],
    path: Vec<Leg>,
    output: &mut Vec<(Vec<Leg>, &'a Node<T>)>,
) {
    let Some((leg, remain)) = legs.split_first() else {
        output.push((path, value));
        return;
    };
    match leg {
        Leg::Key(key) if key == "*" => {
            if let Node::Object(values) = value {
                let mut values = values.iter().collect::<Vec<_>>();
                values.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
                for (key, value) in values {
                    select_walk_roots(
                        value,
                        remain,
                        append_leg(&path, Leg::Key(key.clone())),
                        output,
                    );
                }
            }
        }
        Leg::Key(key) => {
            if let Node::Object(values) = value {
                if let Some((_, value)) = values.iter().find(|(name, _)| name == key) {
                    select_walk_roots(
                        value,
                        remain,
                        append_leg(&path, Leg::Key(key.clone())),
                        output,
                    );
                }
            }
        }
        Leg::Array(selection) => {
            if let Node::Array(values) = value {
                for index in selected_indices(selection, values.len()) {
                    select_walk_roots(
                        &values[index],
                        remain,
                        append_leg(&path, Leg::Array(Selection::Index(index as i64))),
                        output,
                    );
                }
            }
        }
        Leg::DoubleAsterisk => {
            select_walk_roots(value, remain, path.clone(), output);
            match value {
                Node::Array(values) => {
                    for (index, value) in values.iter().enumerate() {
                        select_walk_roots(
                            value,
                            legs,
                            append_leg(&path, Leg::Array(Selection::Index(index as i64))),
                            output,
                        );
                    }
                }
                Node::Object(values) => {
                    let mut values = values.iter().collect::<Vec<_>>();
                    values
                        .sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
                    for (key, value) in values {
                        select_walk_roots(
                            value,
                            legs,
                            append_leg(&path, Leg::Key(key.clone())),
                            output,
                        );
                    }
                }
                _ => {}
            }
        }
    }
}

fn selected_indices(selection: &Selection, length: usize) -> Vec<usize> {
    match selection {
        Selection::Asterisk => (0..length).collect(),
        Selection::Index(index) => normalize_index(*index, length).into_iter().collect(),
        Selection::Range { start, end } => {
            if length == 0 {
                return Vec::new();
            }
            let Some(start) = normalize_range_start(*start, length) else {
                return Vec::new();
            };
            let end = normalize_range_end(*end, length);
            if start > end {
                Vec::new()
            } else {
                (start..=end).collect()
            }
        }
    }
}

fn normalize_range_start(index: i64, length: usize) -> Option<usize> {
    if index >= 0 {
        return usize::try_from(index).ok().filter(|index| *index < length);
    }
    Some(i64::try_from(length).ok()?.saturating_add(index).max(0) as usize)
}

fn normalize_range_end(index: i64, length: usize) -> usize {
    if index >= 0 {
        return usize::try_from(index).unwrap_or(usize::MAX).min(length - 1);
    }
    i64::try_from(length)
        .unwrap_or(i64::MAX)
        .saturating_add(index)
        .max(0) as usize
}

fn autowraps_non_array(selection: &Selection) -> bool {
    match selection {
        Selection::Asterisk => false,
        Selection::Index(index) => *index == 0 || *index == -1,
        Selection::Range { start, end } => *start == 0 && *end >= -1,
    }
}

fn normalize_index(index: i64, length: usize) -> Option<usize> {
    let index = if index < 0 {
        i64::try_from(length).ok()?.checked_add(index)?
    } else {
        index
    };
    usize::try_from(index).ok().filter(|index| *index < length)
}

/// Probes the original ARRAY_INSERT shape/index admission before the caller
/// decodes the replacement. None is a no-op, including too-negative indices.
pub fn native_json_array_insert_index<T>(parent: &Node<T>, index: i64) -> Option<usize> {
    let Node::Array(array) = parent else {
        return None;
    };
    let index = if index < 0 {
        i64::try_from(array.len())
            .ok()
            .and_then(|length| length.checked_add(index))
            .and_then(|index| usize::try_from(index).ok())?
    } else {
        usize::try_from(index).unwrap_or(usize::MAX)
    }
    .min(array.len());
    Some(index)
}

/// Performs ARRAY_INSERT after the same probe; no codec or value coercion runs
/// here. Callers retain the original intermediate encode/decode boundaries.
pub fn insert_native_json_array_node<T>(
    mut parent: Node<T>,
    index: i64,
    replacement: Node<T>,
) -> Node<T> {
    if let Some(index) = native_json_array_insert_index(&parent, index) {
        if let Node::Array(array) = &mut parent {
            array.insert(index, replacement);
        }
    }
    parent
}

/// One native SDK modification; callers validate original path flags and decode
/// each replacement in source order. No scalar payload is interpreted here.
pub fn modify_native_json_node<T>(
    mut document: Node<T>,
    legs: &[Leg],
    replacement: Node<T>,
    mode: ModifyType,
) -> Node<T> {
    let Some((leg, remain)) = legs.split_first() else {
        return match mode {
            ModifyType::Insert => document,
            ModifyType::Replace | ModifyType::Set => replacement,
        };
    };
    match leg {
        Leg::Key(key) => {
            let Node::Object(values) = &mut document else {
                return document;
            };
            let position = values.iter().position(|(name, _)| name == key);
            if remain.is_empty() {
                match (position.is_some(), mode) {
                    (true, ModifyType::Insert) | (false, ModifyType::Replace) => {}
                    _ => {
                        if let Some(position) = position {
                            values[position].1 = replacement;
                        } else {
                            values.push((key.clone(), replacement));
                        }
                    }
                }
            } else if let Some(position) = position {
                let (_, value) = values.remove(position);
                values.push((
                    key.clone(),
                    modify_native_json_node(value, remain, replacement, mode),
                ));
            }
            document
        }
        Leg::Array(Selection::Index(index)) => {
            if let Node::Array(values) = &mut document {
                if let Some(index) = normalize_index(*index, values.len()) {
                    if remain.is_empty() && mode == ModifyType::Insert {
                        return document;
                    }
                    let value = values.remove(index);
                    values.insert(
                        index,
                        modify_native_json_node(value, remain, replacement, mode),
                    );
                } else if remain.is_empty() && mode != ModifyType::Replace {
                    values.push(replacement);
                }
                return document;
            }
            if normalize_index(*index, 1) == Some(0) {
                return modify_native_json_node(document, remain, replacement, mode);
            }
            if remain.is_empty() && mode != ModifyType::Replace {
                return Node::Array(vec![document, replacement]);
            }
            document
        }
        Leg::Array(_) | Leg::DoubleAsterisk => document,
    }
}

/// Removes the first matching duplicate object key, or one exact array cell.
pub fn remove_native_json_node<T>(document: &mut Node<T>, legs: &[Leg]) {
    let Some((leg, remain)) = legs.split_first() else {
        return;
    };
    match leg {
        Leg::Key(key) => {
            let Node::Object(values) = document else {
                return;
            };
            if remain.is_empty() {
                if let Some(position) = values.iter().position(|(name, _)| name == key) {
                    values.remove(position);
                }
            } else if let Some((_, value)) = values.iter_mut().find(|(name, _)| name == key) {
                remove_native_json_node(value, remain);
            }
        }
        Leg::Array(Selection::Index(index)) => {
            let Node::Array(values) = document else {
                return;
            };
            let Some(index) = normalize_index(*index, values.len()) else {
                return;
            };
            if remain.is_empty() {
                values.remove(index);
            } else {
                remove_native_json_node(&mut values[index], remain);
            }
        }
        Leg::Array(_) | Leg::DoubleAsterisk => {}
    }
}
