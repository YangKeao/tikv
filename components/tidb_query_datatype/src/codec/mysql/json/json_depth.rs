// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use serde_json::Value;

use super::{super::Result, JsonRef, JsonType};

impl JsonRef<'_> {
    /// Returns maximum depth of JSON document
    pub fn depth(&self) -> Result<i64> {
        native_json_depth_from_children(&DepthNode::Binary(*self), DepthNode::visit_children)
    }
}

/// Computes depth without encoding a native document into binary JSON.
/// In particular, object keys are not narrowed to binary JSON's u16 lengths.
pub fn native_json_depth(value: &Value) -> Result<i64> {
    native_json_depth_from_children(&DepthNode::Native(value), DepthNode::visit_children)
}

// Representation adapters only enumerate children. The recursive algorithm
// below is the sole depth owner for both wire and native documents.
enum DepthNode<'a> {
    Binary(JsonRef<'a>),
    Native(&'a Value),
}

impl<'a> DepthNode<'a> {
    fn visit_children(&self, visitor: &mut dyn FnMut(&Self) -> Result<()>) -> Result<()> {
        match self {
            Self::Binary(json) => {
                let kind = json.get_type();
                if matches!(kind, JsonType::Object | JsonType::Array) {
                    let length = json.get_elem_count();
                    for i in 0..length {
                        let child = if kind == JsonType::Object {
                            json.object_get_val(i)?
                        } else {
                            json.array_get_elem(i)?
                        };
                        visitor(&Self::Binary(child))?;
                    }
                }
            }
            Self::Native(Value::Array(values)) => {
                for value in values {
                    visitor(&Self::Native(value))?;
                }
            }
            Self::Native(Value::Object(values)) => {
                for value in values.values() {
                    visitor(&Self::Native(value))?;
                }
            }
            Self::Native(_) => {}
        }
        Ok(())
    }
}

/// Computes JSON depth through a representation-only child visitor.
/// The visitor must enumerate each immediate child in order, without computing
/// depth or converting the document to another representation. Callers retain
/// their original validation boundary before entering this shared recursion.
pub fn native_json_depth_from_children<N>(
    node: &N,
    visitor: impl Fn(&N, &mut dyn FnMut(&N) -> Result<()>) -> Result<()>,
) -> Result<i64> {
    depth_json(node, &visitor)
}

// See `GetElemDepth()` in TiDB `json/binary_function.go`.
fn depth_json<N, F>(node: &N, visitor: &F) -> Result<i64>
where
    F: Fn(&N, &mut dyn FnMut(&N) -> Result<()>) -> Result<()>,
{
    let mut max_depth = 0;
    visitor(node, &mut |child| {
        let depth = depth_json(child, visitor)?;
        if depth > max_depth {
            max_depth = depth;
        }
        Ok(())
    })?;
    Ok(max_depth + 1)
}

#[cfg(test)]
mod tests {
    use super::super::Json;

    #[test]
    fn test_json_depth() {
        let mut test_cases = vec![
            ("null", 1),
            ("[true, 2017]", 2),
            (r#"{"a": {"a1": [3]}, "b": {"b1": {"c": {"d": [5]}}}}"#, 6),
            ("{}", 1),
            ("[]", 1),
            ("true", 1),
            ("1", 1),
            ("-1", 1),
            (r#""a""#, 1),
            (r#"[10, 20]"#, 2),
            (r#"[[], {}]"#, 2),
            (r#"[10, {"a": 20}]"#, 3),
            (r#"[[2], 3, [[[4]]]]"#, 5),
            (r#"{"Name": "Homer"}"#, 2),
            (r#"[10, {"a": 20}]"#, 3),
            (
                r#"{"Person": {"Name": "Homer", "Age": 39, "Hobbies": ["Eating", "Sleeping"]} }"#,
                4,
            ),
            (r#"{"a":1}"#, 2),
            (r#"{"a":[1]}"#, 3),
            (r#"{"b":2, "c":3}"#, 2),
            (r#"[1]"#, 2),
            (r#"[1,2]"#, 2),
            (r#"[1,2,[1,3]]"#, 3),
            (r#"[1,2,[1,[5,[3]]]]"#, 5),
            (r#"[1,2,[1,[5,{"a":[2,3]}]]]"#, 6),
            (r#"[{"a":1}]"#, 3),
            (r#"[{"a":1,"b":2}]"#, 3),
            (r#"[{"a":{"a":1},"b":2}]"#, 4),
        ];
        for (i, (js, expected)) in test_cases.drain(..).enumerate() {
            let j = js.parse();
            assert!(j.is_ok(), "#{} expect parse ok but got {:?}", i, j);
            let j: Json = j.unwrap();
            let got = j.as_ref().depth().unwrap();
            assert_eq!(
                got, expected,
                "#{} expect {:?}, but got {:?}",
                i, expected, got
            );
        }
    }
}
