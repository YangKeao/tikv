// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use tidb_query_datatype::{codec::data_type::VectorValue, expr::EvalContext};
use tipb::FieldType;

use super::{InputRow, LocalError, LocalResult, registry};

static NEXT_CATALOG_KEY: AtomicU64 = AtomicU64::new(1);

/// Process-local catalog identity, not a digest of its signatures.
/// Cloning a catalog preserves this identity; constructing another does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostCatalogKey(u64);

fn allocate_catalog_key(next: &AtomicU64) -> LocalResult<HostCatalogKey> {
    next.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        value.checked_add(1)
    })
    .map(HostCatalogKey)
    .map_err(|_| LocalError::ResourceLimit("host catalog identity space exhausted".into()))
}

/// A typed host boundary. The current admission domain is signed LongLong for
/// every argument and the result; all FieldType details remain significant.
#[derive(Clone, Debug)]
pub struct HostSignature {
    pub arg_types: Box<[FieldType]>,
    pub return_type: FieldType,
}

/// Immutable signatures shared by compiled calls. There are no callbacks or
/// native expression objects in this catalog.
#[derive(Clone, Debug)]
pub struct HostCatalog {
    key: HostCatalogKey,
    signatures: Arc<[HostSignature]>,
}

impl HostCatalog {
    pub fn new(signatures: Vec<HostSignature>) -> LocalResult<Self> {
        for (index, signature) in signatures.iter().enumerate() {
            for (argument, field_type) in signature.arg_types.iter().enumerate() {
                registry::check_signed_int_type(field_type).map_err(|error| {
                    LocalError::InvalidSpec(format!(
                        "host slot {} argument {}: {}",
                        index, argument, error
                    ))
                })?;
            }
            registry::check_signed_int_type(&signature.return_type).map_err(|error| {
                LocalError::InvalidSpec(format!("host slot {} result: {}", index, error))
            })?;
        }
        Ok(Self {
            key: allocate_catalog_key(&NEXT_CATALOG_KEY)?,
            signatures: signatures.into(),
        })
    }

    pub fn key(&self) -> &HostCatalogKey {
        &self.key
    }

    /// Resolves a catalog-owned slot without allowing callers to forge one.
    pub fn slot(&self, index: usize) -> LocalResult<HostSlot> {
        if index >= self.signatures.len() {
            return Err(LocalError::InvalidSpec(format!(
                "host slot {} is outside its catalog",
                index
            )));
        }
        Ok(HostSlot {
            key: self.key,
            index,
        })
    }
}

/// Opaque catalog-bound identity. The index alone is only an adapter dispatch
/// coordinate and must not be used to establish catalog compatibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostSlot {
    key: HostCatalogKey,
    index: usize,
}

impl HostSlot {
    pub fn index(self) -> usize {
        self.index
    }
}

/// A checked call retaining its immutable catalog, with no caller-supplied
/// metadata or native evaluator. Only local compilation can prepare this value.
#[derive(Clone, Debug)]
pub struct PreparedHostCall {
    catalog: HostCatalog,
    slot: HostSlot,
}

impl PreparedHostCall {
    pub(crate) fn prepare(
        catalog: &HostCatalog,
        slot: HostSlot,
        arg_types: &[FieldType],
        return_type: &FieldType,
    ) -> LocalResult<Self> {
        if slot.key != catalog.key {
            return Err(LocalError::InvalidSpec(
                "host slot belongs to a different catalog".into(),
            ));
        }
        let signature = catalog
            .signatures
            .get(slot.index)
            .ok_or_else(|| LocalError::InvalidSpec("host slot is outside its catalog".into()))?;
        if signature.arg_types.as_ref() != arg_types {
            return Err(LocalError::InvalidSpec(
                "host argument field types differ from its catalog signature".into(),
            ));
        }
        if &signature.return_type != return_type {
            return Err(LocalError::InvalidSpec(
                "host result field type differs from its catalog signature".into(),
            ));
        }
        Ok(Self {
            catalog: catalog.clone(),
            slot,
        })
    }

    pub fn slot(&self) -> HostSlot {
        self.slot
    }

    pub fn catalog_key(&self) -> &HostCatalogKey {
        self.catalog.key()
    }

    pub fn arg_types(&self) -> &[FieldType] {
        &self.catalog.signatures[self.slot.index].arg_types
    }

    pub fn return_type(&self) -> &FieldType {
        &self.catalog.signatures[self.slot.index].return_type
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgMode {
    /// Evaluate the requested argument for this occurrence again.
    Fresh,
    /// Reuse the last successful value for this argument and invocation; a
    /// cache miss evaluates the child once and retains that successful result.
    Reuse,
}

/// Borrows are valid only during `start`; tasks retain owned state and IDs, not
/// the caller's program, FieldTypes, or EvalContext.
#[derive(Clone, Copy, Debug)]
pub struct HostInvocation<'a> {
    pub slot: HostSlot,
    pub row: InputRow,
    pub arg_types: &'a [FieldType],
    pub return_type: &'a FieldType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostArgRequest {
    pub index: usize,
    pub mode: ArgMode,
}

/// A checked, width-one argument value borrowed only for the `resume` call.
#[derive(Clone, Copy, Debug)]
pub struct HostArgReply<'a> {
    pub index: usize,
    pub field_type: &'a FieldType,
    pub values: &'a VectorValue,
}

/// Adapter task identity. Generations distinguish reuse of a task slot and must
/// never be zero. A driver receiving generation zero must reject it and attempt
/// cancellation, rather than treating it as a ready or absent task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostTaskId {
    pub slot: usize,
    pub generation: u64,
}

impl HostTaskId {
    /// Validation does not perform cleanup; the driver must still best-effort
    /// cancel a malformed ID returned by a service.
    pub(crate) fn validate_generation(&self) -> LocalResult<()> {
        if self.generation == 0 {
            return Err(LocalError::HostContract(
                "host task generation must be nonzero".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum HostStart {
    Ready(VectorValue),
    Pending {
        task: HostTaskId,
        request: HostArgRequest,
    },
}

#[derive(Debug)]
pub enum HostStep {
    NeedArg(HostArgRequest),
    Ready(VectorValue),
}

/// Demand protocol for catalog-bound hosts, not native Expr evaluation or an
/// arbitrary evaluation callback. Argument expressions are evaluated only by
/// the official driver in response to explicit requests.
///
/// Implementations must keep their catalog identity and task namespace stable
/// across service reborrows. Invocation/reply references and EvalContext may
/// not be retained across calls; suspended tasks contain only owned state.
/// `start` must release partial state before returning Err or immediate Ready.
/// `resume` must finish its task before Ready; the driver retains the token
/// until that result is validated and may still issue idempotent cancellation.
/// Start/resume bodies must be bounded, and adapters must meter their own
/// opaque task allocations: RPN limits are not a total host-heap or
/// external-stop hook. Cleanup guarantees require a stable, contract-compliant
/// provider; they cannot recover resources after the provider disappears,
/// changes identity or panics.
pub trait LocalHostServices {
    fn catalog_key(&self) -> &HostCatalogKey;

    fn start(
        &mut self,
        ctx: &mut EvalContext,
        invocation: HostInvocation<'_>,
    ) -> LocalResult<HostStart>;

    fn resume(
        &mut self,
        ctx: &mut EvalContext,
        task: &HostTaskId,
        reply: HostArgReply<'_>,
    ) -> LocalResult<HostStep>;

    /// Infallible, nonpanicking, idempotent cleanup, including malformed,
    /// stale, or already completed IDs. Do not evaluate expressions, append
    /// diagnostics, or replace the error that caused cancellation. There is
    /// no default: every host implementation must explicitly provide its
    /// cleanup contract.
    fn cancel(&mut self, task: &HostTaskId);
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use tidb_query_datatype::{FieldTypeAccessor, FieldTypeFlag, FieldTypeTp};

    use super::*;

    fn int_type() -> FieldType {
        FieldTypeTp::LongLong.into()
    }

    fn signature() -> HostSignature {
        HostSignature {
            arg_types: vec![int_type()].into_boxed_slice(),
            return_type: int_type(),
        }
    }

    #[test]
    fn catalog_identity_is_not_signature_identity() {
        let catalog = HostCatalog::new(vec![signature()]).unwrap();
        let cloned = catalog.clone();
        let separate = HostCatalog::new(vec![signature()]).unwrap();
        assert_eq!(catalog.key(), cloned.key());
        assert!(Arc::ptr_eq(&catalog.signatures, &cloned.signatures));
        assert_ne!(catalog.key(), separate.key());
        assert_eq!(catalog.slot(0).unwrap(), cloned.slot(0).unwrap());
        assert_ne!(catalog.slot(0).unwrap(), separate.slot(0).unwrap());
        let keys: HashSet<_> = [*catalog.key(), *cloned.key(), *separate.key()]
            .into_iter()
            .collect();
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn catalog_keys_are_unique_across_threads() {
        let workers: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| *HostCatalog::new(vec![signature()]).unwrap().key()))
            .collect();
        let keys: HashSet<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(keys.len(), 8);
    }

    #[test]
    fn catalog_key_exhaustion_does_not_wrap() {
        let next = AtomicU64::new(u64::MAX - 1);
        assert_eq!(
            allocate_catalog_key(&next).unwrap(),
            HostCatalogKey(u64::MAX - 1)
        );
        assert!(matches!(
            allocate_catalog_key(&next),
            Err(LocalError::ResourceLimit(_))
        ));
        assert_eq!(next.load(Ordering::Relaxed), u64::MAX);
        assert!(allocate_catalog_key(&next).is_err());
    }

    #[test]
    fn catalog_rejects_non_signed_longlong_signatures() {
        let mut unsigned = int_type();
        unsigned.as_mut_accessor().set_flag(FieldTypeFlag::UNSIGNED);
        for invalid in [
            FieldTypeTp::Double.into(),
            FieldTypeTp::String.into(),
            unsigned,
        ] {
            let bad_argument = HostSignature {
                arg_types: vec![invalid.clone()].into_boxed_slice(),
                return_type: int_type(),
            };
            assert!(matches!(
                HostCatalog::new(vec![bad_argument]),
                Err(LocalError::InvalidSpec(_))
            ));
            let bad_result = HostSignature {
                arg_types: Box::new([]),
                return_type: invalid,
            };
            assert!(matches!(
                HostCatalog::new(vec![bad_result]),
                Err(LocalError::InvalidSpec(_))
            ));
        }
    }

    #[test]
    fn catalog_slot_bounds_are_checked() {
        assert!(HostCatalog::new(vec![]).unwrap().slot(0).is_err());
        let catalog = HostCatalog::new(vec![signature()]).unwrap();
        assert_eq!(catalog.slot(0).unwrap().index(), 0);
        assert!(catalog.slot(1).is_err());
        assert!(catalog.slot(usize::MAX).is_err());
    }

    #[test]
    fn preparation_checks_catalog_and_complete_signature() {
        let catalog = HostCatalog::new(vec![signature()]).unwrap();
        let separate = HostCatalog::new(vec![signature()]).unwrap();
        let slot = catalog.slot(0).unwrap();
        let prepared =
            PreparedHostCall::prepare(&catalog, slot, &[int_type()], &int_type()).unwrap();
        assert_eq!(prepared.slot(), slot);
        assert_eq!(prepared.catalog_key(), catalog.key());
        assert_eq!(prepared.arg_types(), &[int_type()]);
        assert_eq!(prepared.return_type(), &int_type());
        assert!(
            PreparedHostCall::prepare(&catalog.clone(), slot, &[int_type()], &int_type()).is_ok()
        );
        assert!(PreparedHostCall::prepare(&separate, slot, &[int_type()], &int_type()).is_err());
        assert!(PreparedHostCall::prepare(&catalog, slot, &[], &int_type()).is_err());
        let mut different = int_type();
        different.set_flen(7);
        assert!(
            PreparedHostCall::prepare(&catalog, slot, &[different.clone()], &int_type()).is_err()
        );
        assert!(PreparedHostCall::prepare(&catalog, slot, &[int_type()], &different).is_err());
        let invalid_slot = HostSlot {
            key: *catalog.key(),
            index: usize::MAX,
        };
        assert!(
            PreparedHostCall::prepare(&catalog, invalid_slot, &[int_type()], &int_type()).is_err()
        );
    }

    #[test]
    fn prepared_call_keeps_its_catalog_alive() {
        let prepared = {
            let catalog = HostCatalog::new(vec![signature()]).unwrap();
            PreparedHostCall::prepare(
                &catalog,
                catalog.slot(0).unwrap(),
                &[int_type()],
                &int_type(),
            )
            .unwrap()
        };
        assert_eq!(prepared.arg_types(), &[int_type()]);
        assert_eq!(prepared.return_type(), &int_type());
    }

    #[test]
    fn existing_input_services_need_no_host_implementation() {
        struct Inputs;
        impl super::super::LocalRuntimeServices for Inputs {
            fn binding_schema(&self) -> &[FieldType] {
                &[]
            }
            fn read_input(
                &mut self,
                _ctx: &mut EvalContext,
                _slot: usize,
                _row: InputRow,
                _expected: &FieldType,
            ) -> LocalResult<VectorValue> {
                Err(LocalError::InvalidBatch("no input slots".into()))
            }
        }
        let mut inputs = Inputs;
        assert!(super::super::LocalRuntimeServices::host_services(&mut inputs).is_none());
    }

    #[test]
    fn task_budget_reserves_before_start() {
        use super::super::runtime::{EvalBudget, ExecutionLimits};
        let budget = EvalBudget::local(
            ExecutionLimits {
                max_active_tasks: 1,
                ..ExecutionLimits::default()
            },
            0,
        )
        .unwrap();
        assert!(budget.task_count(0).is_ok());
        assert!(budget.task_count(1).is_ok());
        assert!(matches!(
            budget.task_count(2),
            Err(LocalError::ResourceLimit(_))
        ));
        assert!(EvalBudget::legacy().task_count(usize::MAX).is_ok());
    }

    #[test]
    fn task_generation_zero_is_a_contract_error() {
        let invalid = HostTaskId {
            slot: 0,
            generation: 0,
        };
        assert!(matches!(
            invalid.validate_generation(),
            Err(LocalError::HostContract(_))
        ));
        let valid = HostTaskId {
            slot: 0,
            generation: 1,
        };
        assert!(valid.validate_generation().is_ok());
        let maximum = HostTaskId {
            slot: 0,
            generation: u64::MAX,
        };
        assert!(maximum.validate_generation().is_ok());
        let identities: HashSet<_> = [invalid, valid, maximum].into_iter().collect();
        assert_eq!(identities.len(), 3);
    }
}
