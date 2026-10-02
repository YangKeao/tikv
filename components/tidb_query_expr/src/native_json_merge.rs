// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native serde merge operands use the shared lossless-node merge traversals.
//! Only representation conversion and the native SQL-NULL/reset policy live
//! here. Callers prepare all operands and format the actual final value.

use serde_json::Value as Json;
use tidb_query_datatype::codec::mysql::json::{
    NativeJsonNode, merge_native_json_nodes, merge_patch_native_json_node,
};

/// Merges prepared native documents with the shared consecutive-object-run and
/// one-level array-concatenation policy. An empty list produces an empty array.
pub fn native_json_merge_preserve(values: Vec<Json>) -> Json {
    let values = values.into_iter().map(serde_to_node).collect::<Vec<_>>();
    node_to_serde(merge_native_json_nodes(&values))
}

/// Applies native merge-patch after every input has been prepared. SQL NULL is
/// distinct from JSON null and only a later non-object document can reset it.
/// The original empty-list indexing panic is intentionally retained.
pub fn native_json_merge_patch(values: Vec<Option<Json>>) -> Option<Json> {
    let mut start = 0;
    for index in (0..values.len()).rev() {
        if values[index]
            .as_ref()
            .is_none_or(|value| !matches!(value, Json::Object(_)))
        {
            start = index;
            break;
        }
    }
    let mut target = values[start].clone().map(serde_to_node);
    for patch in values.into_iter().skip(start + 1) {
        target = match (target, patch) {
            (_, None) => None,
            (Some(target), Some(patch)) => Some(merge_patch_native_json_node(
                target,
                serde_to_node(patch),
                Json::is_null,
                || Json::Null,
            )),
            (None, Some(Json::Object(_))) => None,
            (None, Some(patch)) => Some(serde_to_node(patch)),
        };
    }
    target.map(node_to_serde)
}

// These are representation bridges, not merge algorithms. Primitive leaves
// retain their actual serde type/value; containers never hide in Scalar.
fn serde_to_node(value: Json) -> NativeJsonNode<Json> {
    match value {
        Json::Array(values) => {
            NativeJsonNode::Array(values.into_iter().map(serde_to_node).collect())
        }
        Json::Object(values) => NativeJsonNode::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, serde_to_node(value)))
                .collect(),
        ),
        value => NativeJsonNode::Scalar(value),
    }
}

fn node_to_serde(value: NativeJsonNode<Json>) -> Json {
    match value {
        NativeJsonNode::Scalar(value) => value,
        NativeJsonNode::Array(values) => {
            Json::Array(values.into_iter().map(node_to_serde).collect())
        }
        NativeJsonNode::Object(values) => Json::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, node_to_serde(value)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn native_merge_preserve_uses_shared_grouping_without_scalar_reencoding() {
        assert_eq!(native_json_merge_preserve(vec![]), json!([]));
        assert_eq!(native_json_merge_preserve(vec![json!([[1]])]), json!([[1]]));
        assert_eq!(
            native_json_merge_preserve(vec![
                json!({"a": 1}),
                json!([2, [3]]),
                json!({"a": {"x": 1}, "b": [4]}),
                json!({"a": {"x": 2}, "b": 5}),
            ]),
            json!([{"a": 1}, 2, [3], {"a": {"x": [1, 2]}, "b": [4, 5]}]),
        );
        let output = native_json_merge_preserve(vec![
            json!(u64::MAX),
            json!(-0.0),
            Json::Null,
            json!("[1]"),
        ]);
        let Json::Array(values) = output else {
            panic!("nonobject merge must produce an array");
        };
        assert_eq!(values[0].as_u64(), Some(u64::MAX));
        assert_eq!(values[1].as_f64().unwrap().to_bits(), (-0.0_f64).to_bits());
        assert_eq!(values[2], Json::Null);
        assert_eq!(values[3], json!("[1]"));
    }

    #[test]
    fn native_merge_patch_keeps_nullable_reset_and_empty_panic() {
        assert_eq!(
            native_json_merge_patch(vec![
                Some(json!({"a": {"b": 1, "c": 2}, "keep": null})),
                Some(json!({"a": {"b": null, "d": 3}, "new": {"gone": null}})),
            ]),
            Some(json!({"a": {"c": 2, "d": 3}, "keep": null, "new": {}})),
        );
        assert_eq!(
            native_json_merge_patch(vec![None, Some(json!({"a": 1}))]),
            None,
        );
        assert_eq!(
            native_json_merge_patch(vec![None, Some(Json::Null), Some(json!({"a": 1}))]),
            Some(json!({"a": 1})),
        );
        assert_eq!(
            native_json_merge_patch(vec![Some(json!({"old": 1})), Some(json!([2])), None]),
            None,
        );
        assert_eq!(
            native_json_merge_patch(vec![Some(json!({"old": 1})), None, Some(json!([2]))]),
            Some(json!([2])),
        );
        assert_eq!(
            native_json_merge_patch(vec![Some(Json::Null)]),
            Some(Json::Null)
        );
        assert!(std::panic::catch_unwind(|| native_json_merge_patch(vec![])).is_err());
    }
}
