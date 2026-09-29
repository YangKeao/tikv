// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use std::{fmt, mem};

use tidb_query_datatype::codec::data_type::ScalarValue;
use tipb::FieldType;

use crate::{CallMetadata, FunctionRef, LiteralKind, local::HostSlot};

/// Immutable typed construction input. This contains no session or input-row
/// references and can be shared; compiled metadata is deliberately not shared.
///
/// Destruction is iterative even when construction limits reject the tree.
/// Derived `Clone` and `Debug` still recurse and are not deep-safe operations;
/// share a deeply nested specification with `Arc::clone` instead of cloning it.
#[derive(Clone, Debug)]
pub enum LocalExpr {
    Constant {
        value: ScalarValue,
        field_type: FieldType,
        literal_kind: LiteralKind,
    },
    InputSlot {
        slot: usize,
        field_type: FieldType,
    },
    Call {
        function: FunctionRef,
        args: Box<[LocalExpr]>,
        return_type: FieldType,
        metadata: CallMetadata,
    },
    HostCall {
        slot: HostSlot,
        args: Box<[LocalExpr]>,
        return_type: FieldType,
    },
}

impl Drop for LocalExpr {
    fn drop(&mut self) {
        let args = match self {
            Self::Call { args, .. } | Self::HostCall { args, .. } => args,
            _ => return,
        };
        let mut pending = mem::take(args).into_vec();
        while let Some(mut expr) = pending.pop() {
            if let Self::Call { args, .. } | Self::HostCall { args, .. } = &mut expr {
                // Detach descendants before dropping their owning expression.
                // This also covers trees rejected before compilation finishes.
                pending.extend(mem::take(args).into_vec());
            }
        }
    }
}

impl LocalExpr {
    pub fn field_type(&self) -> &FieldType {
        match self {
            Self::Constant { field_type, .. } | Self::InputSlot { field_type, .. } => field_type,
            Self::Call { return_type, .. } | Self::HostCall { return_type, .. } => return_type,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CompileLimits {
    pub max_nodes: usize,
    pub max_depth: usize,
}

impl Default for CompileLimits {
    fn default() -> Self {
        Self {
            max_nodes: 16_384,
            max_depth: 256,
        }
    }
}

/// Construction is side-effect free in this seed domain; it does not need a
/// default EvalContext or a borrowed statement context.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalCompileContext {
    pub limits: CompileLimits,
}

#[derive(Debug)]
pub enum LocalError {
    InvalidSpec(String),
    InvalidBatch(String),
    /// A demanded input service returned an incompatible value representation.
    BindingContract(String),
    /// A registered host violated its staged invocation or result contract.
    HostContract(String),
    ResourceLimit(String),
    Evaluation(tidb_query_common::Error),
}

pub type LocalResult<T> = std::result::Result<T, LocalError>;

impl fmt::Display for LocalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSpec(message) => write!(f, "Invalid local expression: {}", message),
            Self::InvalidBatch(message) => write!(f, "Invalid local batch: {}", message),
            Self::BindingContract(message) => {
                write!(f, "Local input binding contract violation: {}", message)
            }
            Self::HostContract(message) => {
                write!(f, "Local host contract violation: {}", message)
            }
            Self::ResourceLimit(message) => {
                write!(f, "Local expression resource limit: {}", message)
            }
            Self::Evaluation(error) => write!(f, "{}", error),
        }
    }
}

impl std::error::Error for LocalError {}

#[cfg(test)]
mod tests {
    use std::thread;

    use tidb_query_datatype::FieldTypeTp;
    use tikv_util::sys::thread::StdThreadBuildWrapper;
    use tipb::ScalarFuncSig;

    use super::*;
    use crate::local::{HostCatalog, HostSignature, compile_local};

    fn nested_calls(depth: usize) -> LocalExpr {
        let mut expression = LocalExpr::Constant {
            value: ScalarValue::Int(Some(1)),
            field_type: FieldTypeTp::LongLong.into(),
            literal_kind: LiteralKind::Typed,
        };
        for _ in 0..depth {
            expression = LocalExpr::Call {
                function: FunctionRef::TiPb(ScalarFuncSig::AbsInt),
                args: vec![expression].into_boxed_slice(),
                return_type: FieldTypeTp::LongLong.into(),
                metadata: CallMetadata::None,
            };
        }
        expression
    }

    fn nested_host_calls(depth: usize, slot: HostSlot) -> LocalExpr {
        let mut expression = nested_calls(0);
        for level in 0..depth {
            expression = if level % 2 == 0 {
                LocalExpr::HostCall {
                    slot,
                    args: vec![expression].into_boxed_slice(),
                    return_type: FieldTypeTp::LongLong.into(),
                }
            } else {
                LocalExpr::Call {
                    function: FunctionRef::TiPb(ScalarFuncSig::AbsInt),
                    args: vec![expression].into_boxed_slice(),
                    return_type: FieldTypeTp::LongLong.into(),
                    metadata: CallMetadata::None,
                }
            };
        }
        expression
    }

    #[test]
    fn test_deep_host_spec_drop_on_small_stack() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LocalExpr>();

        thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn_wrapper(|| {
                let field_type: FieldType = FieldTypeTp::LongLong.into();
                let catalog = HostCatalog::new(vec![HostSignature {
                    arg_types: vec![field_type.clone()].into_boxed_slice(),
                    return_type: field_type.clone(),
                }])
                .unwrap();
                let slot = catalog.slot(0).unwrap();
                for depth in [33, 64, 256, 16_384] {
                    let expression = nested_host_calls(depth, slot);
                    assert_eq!(expression.field_type(), &field_type);
                    // Teardown is independent of admission or compilation, and
                    // must traverse both ordinary and host-call children.
                    drop(expression);
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn test_deep_local_spec_drop_after_successful_compile() {
        thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn_wrapper(|| {
                for depth in [33, 64, 256, 4_096] {
                    let expression = nested_calls(depth);
                    let result = compile_local(
                        &expression,
                        &[],
                        LocalCompileContext {
                            limits: CompileLimits {
                                max_nodes: depth + 1,
                                max_depth: depth + 1,
                            },
                        },
                    );
                    // Do not format or clone a deeply nested specification.
                    assert!(result.is_ok());
                    drop(result);
                    drop(expression);
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn test_deep_local_spec_drop_after_compile_rejection() {
        thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn_wrapper(|| {
                let expression = nested_calls(16_384);
                let result = compile_local(
                    &expression,
                    &[],
                    LocalCompileContext {
                        limits: CompileLimits {
                            max_nodes: 16_385,
                            max_depth: 32,
                        },
                    },
                );
                assert!(matches!(result, Err(LocalError::ResourceLimit(_))));
                drop(expression);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn test_host_contract_error_display() {
        let error = LocalError::HostContract("unexpected argument index".into());
        assert_eq!(
            error.to_string(),
            "Local host contract violation: unexpected argument index"
        );
    }

    #[test]
    fn test_binding_contract_error_display() {
        let error = LocalError::BindingContract("expected one Int value".into());
        assert_eq!(
            error.to_string(),
            "Local input binding contract violation: expected one Int value"
        );
    }
}
