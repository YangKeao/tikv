// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::fmt;

use num_traits::identities::Zero;
use tidb_query_codegen::rpn_fn;
use tidb_query_common::{Result, error::EvaluateError};
pub use tidb_query_datatype::codec::mysql::decimal::NativeDecimalFastValue;
use tidb_query_datatype::{
    codec::{
        self, Error,
        data_type::*,
        div_i64, div_i64_with_u64, div_u64_with_i64,
        mysql::{
            Res,
            decimal::{
                NativeDecimalBinaryOp, NativeDecimalBinaryPolicy, native_decimal_fast_binary,
            },
        },
    },
    expr::EvalContext,
};

use crate::impl_math::{native_decimal_budget, native_decimal_failure};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryArithmeticOperation {
    Add,
    Subtract,
    Multiply,
    Modulo,
}

impl BinaryArithmeticOperation {
    fn sql_name(self) -> &'static str {
        match self {
            Self::Add => "ADD",
            Self::Subtract => "SUBTRACT",
            Self::Multiply => "MULTIPLY",
            Self::Modulo => "MOD",
        }
    }

    fn decimal(self) -> NativeDecimalBinaryOp {
        match self {
            Self::Add => NativeDecimalBinaryOp::Add,
            Self::Subtract => NativeDecimalBinaryOp::Subtract,
            Self::Multiply => NativeDecimalBinaryOp::Multiply,
            Self::Modulo => unreachable!("MOD requires its dedicated remainder recipe"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryArithmeticErrorKind {
    IntOverflow,
    FloatOverflow,
    DecimalOverflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeBinaryArithmeticError {
    pub operation: BinaryArithmeticOperation,
    pub kind: BinaryArithmeticErrorKind,
}

impl fmt::Display for NativeBinaryArithmeticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {:?}", self.operation.sql_name(), self.kind)
    }
}

impl std::error::Error for NativeBinaryArithmeticError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LegacyBinaryArithmeticError {
    pub operation: BinaryArithmeticOperation,
    pub unsigned: bool,
}

impl fmt::Display for LegacyBinaryArithmeticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let domain = if self.unsigned {
            "BIGINT UNSIGNED"
        } else {
            "BIGINT"
        };
        write!(
            formatter,
            "{domain} value is out of range in '{}'",
            self.operation.sql_name()
        )
    }
}

impl std::error::Error for LegacyBinaryArithmeticError {}

/// A computed fast result, not a SQL value masquerading as an unsupported flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeDecimalFastOutcome {
    Unsupported,
    Value(Option<NativeDecimalFastValue>),
}

/// Sole decoder for the factory-only fast report. None is exclusively SQL NULL.
pub(crate) fn decode_native_decimal_fast_outcome(
    value: Option<&[u8]>,
) -> Result<NativeDecimalFastOutcome> {
    let Some(bytes) = value else {
        return Ok(NativeDecimalFastOutcome::Value(None));
    };
    if bytes == [0] {
        return Ok(NativeDecimalFastOutcome::Unsupported);
    }
    if bytes.len() != 25 || bytes[0] != 1 {
        return Err(other_err!(
            "Invalid native decimal fast outcome tag or length"
        ));
    }
    let mut coefficient = [0; 16];
    coefficient.copy_from_slice(&bytes[1..17]);
    let mut storage_scale = [0; 4];
    storage_scale.copy_from_slice(&bytes[17..21]);
    let mut scale = [0; 4];
    scale.copy_from_slice(&bytes[21..25]);
    let value = NativeDecimalFastValue {
        coefficient: i128::from_le_bytes(coefficient),
        storage_scale: u32::from_le_bytes(storage_scale),
        scale: u32::from_le_bytes(scale),
    };
    if value.scale > value.storage_scale {
        return Err(other_err!(
            "Native decimal fast outcome has invalid scale shape"
        ));
    }
    Ok(NativeDecimalFastOutcome::Value(Some(value)))
}

fn encode_native_decimal_fast_outcome(value: Option<NativeDecimalFastValue>) -> Bytes {
    let Some(value) = value else {
        return vec![0];
    };
    let mut bytes = Vec::with_capacity(25);
    bytes.push(1);
    bytes.extend_from_slice(&value.coefficient.to_le_bytes());
    bytes.extend_from_slice(&value.storage_scale.to_le_bytes());
    bytes.extend_from_slice(&value.scale.to_le_bytes());
    bytes
}

fn native_binary_arithmetic_error(
    operation: BinaryArithmeticOperation,
    kind: BinaryArithmeticErrorKind,
) -> tidb_query_common::Error {
    EvaluateError::Caused(Box::new(NativeBinaryArithmeticError { operation, kind })).into()
}

fn checked_add_integer(lhs: Int, rhs: Int, lhs_unsigned: bool, rhs_unsigned: bool) -> Option<Int> {
    match (lhs_unsigned, rhs_unsigned) {
        (false, false) => lhs.checked_add(rhs),
        (true, true) => (lhs as u64)
            .checked_add(rhs as u64)
            .map(|value| value as Int),
        (true, false) => checked_add_integer(rhs, lhs, false, true),
        (false, true) => {
            let value = if lhs >= 0 {
                (lhs as u64).checked_add(rhs as u64)
            } else {
                (rhs as u64).checked_sub(lhs.unsigned_abs())
            };
            value.map(|value| value as Int)
        }
    }
}

fn checked_multiply_integer(lhs: Int, rhs: Int, unsigned: bool) -> Option<Int> {
    if unsigned {
        (lhs as u64)
            .checked_mul(rhs as u64)
            .map(|value| value as Int)
    } else {
        lhs.checked_mul(rhs)
    }
}

// Complete original Go minus_overflows policy, including signed zero minus MIN.
// Do not replace the branches with a mathematical i128 range comparison.
fn integer_minus_overflows(
    lhs_unsigned: bool,
    rhs_unsigned: bool,
    force_signed: bool,
    a: Int,
    b: Int,
) -> bool {
    let signed = force_signed || (!lhs_unsigned && !rhs_unsigned);
    let res = a.wrapping_sub(b);
    let (ua, ub) = (a as u64, b as u64);
    let mut res_unsigned = false;
    if lhs_unsigned {
        if rhs_unsigned {
            if ua < ub {
                if res >= 0 {
                    return true;
                }
            } else {
                res_unsigned = true;
            }
        } else if b >= 0 {
            if ua > ub {
                res_unsigned = true;
            }
        } else if ua > u64::MAX - b.unsigned_abs() {
            return true;
        } else {
            res_unsigned = true;
        }
    } else if rhs_unsigned {
        if (a.wrapping_sub(i64::MIN) as u64) < ub {
            return true;
        }
    } else if a > 0 && b < 0 {
        res_unsigned = true;
    } else if a < 0 && b > 0 && res >= 0 {
        return true;
    }
    (!signed && !res_unsigned && res < 0)
        || (signed && res_unsigned && (res as u64) > i64::MAX as u64)
}

#[derive(Clone, Copy)]
enum IntegerSubtractionPolicy {
    Wire,
    Native { force_signed: bool },
}

fn checked_subtract_integer(
    lhs: Int,
    rhs: Int,
    lhs_unsigned: bool,
    rhs_unsigned: bool,
    policy: IntegerSubtractionPolicy,
) -> Option<Int> {
    let force_signed = match policy {
        IntegerSubtractionPolicy::Wire => {
            // Original wire checked_sub rejects this one signed edge which the
            // original native Go predicate accepts. Keep both existing policies.
            if !lhs_unsigned && !rhs_unsigned && lhs == 0 && rhs == i64::MIN {
                return None;
            }
            false
        }
        IntegerSubtractionPolicy::Native { force_signed } => force_signed,
    };
    if integer_minus_overflows(lhs_unsigned, rhs_unsigned, force_signed, lhs, rhs) {
        None
    } else {
        Some(lhs.wrapping_sub(rhs))
    }
}

fn binary_arithmetic_value<T>(lhs: T, rhs: T, operation: BinaryArithmeticOperation) -> T
where
    T: std::ops::Add<Output = T> + std::ops::Sub<Output = T> + std::ops::Mul<Output = T>,
{
    // Real keeps its original NotNan operator policy; raw f64 keeps IEEE bits.
    match operation {
        BinaryArithmeticOperation::Add => lhs + rhs,
        BinaryArithmeticOperation::Subtract => lhs - rhs,
        BinaryArithmeticOperation::Multiply => lhs * rhs,
        BinaryArithmeticOperation::Modulo => {
            unreachable!("MOD requires its dedicated remainder recipe")
        }
    }
}

fn native_integer_binary(
    lhs: Int,
    rhs: Int,
    operation: BinaryArithmeticOperation,
    lhs_unsigned: bool,
    rhs_unsigned: bool,
    force_signed: bool,
) -> Result<Option<Int>> {
    let value = match operation {
        BinaryArithmeticOperation::Add => checked_add_integer(lhs, rhs, lhs_unsigned, rhs_unsigned),
        BinaryArithmeticOperation::Subtract => checked_subtract_integer(
            lhs,
            rhs,
            lhs_unsigned,
            rhs_unsigned,
            IntegerSubtractionPolicy::Native { force_signed },
        ),
        // Native unsigned multiply always reads BOTH original u64 bit patterns.
        BinaryArithmeticOperation::Multiply => {
            checked_multiply_integer(lhs, rhs, lhs_unsigned || rhs_unsigned)
        }
        BinaryArithmeticOperation::Modulo => {
            return Err(other_err!("MOD requires its dedicated remainder recipe"));
        }
    };
    value.map(Some).ok_or_else(|| {
        native_binary_arithmetic_error(operation, BinaryArithmeticErrorKind::IntOverflow)
    })
}

macro_rules! native_integer_binary_recipe {
    ($name:ident, $operation:ident, $left:expr, $right:expr, $forced:expr) => {
        #[rpn_fn]
        fn $name(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
            native_integer_binary(
                *lhs,
                *rhs,
                BinaryArithmeticOperation::$operation,
                $left,
                $right,
                $forced,
            )
        }
    };
}

native_integer_binary_recipe!(add_int_ss_native, Add, false, false, false);
native_integer_binary_recipe!(add_int_su_native, Add, false, true, false);
native_integer_binary_recipe!(add_int_us_native, Add, true, false, false);
native_integer_binary_recipe!(add_int_uu_native, Add, true, true, false);
native_integer_binary_recipe!(sub_int_ss_native, Subtract, false, false, false);
native_integer_binary_recipe!(sub_int_su_native, Subtract, false, true, false);
native_integer_binary_recipe!(sub_int_us_native, Subtract, true, false, false);
native_integer_binary_recipe!(sub_int_uu_native, Subtract, true, true, false);
native_integer_binary_recipe!(sub_int_su_forced_native, Subtract, false, true, true);
native_integer_binary_recipe!(sub_int_us_forced_native, Subtract, true, false, true);
native_integer_binary_recipe!(sub_int_uu_forced_native, Subtract, true, true, true);
native_integer_binary_recipe!(mul_int_signed_native, Multiply, false, false, false);
native_integer_binary_recipe!(mul_int_unsigned_native, Multiply, true, true, false);

fn binary_raw_f64(value: Option<BytesRef>) -> Result<Option<f64>> {
    let Some(bytes) = value else {
        return Ok(None);
    };
    let bytes = <[u8; 8]>::try_from(bytes)
        .map_err(|_| other_err!("Binary f64 transport requires exactly 8 bytes"))?;
    Ok(Some(f64::from_bits(u64::from_le_bytes(bytes))))
}

fn real_binary(
    lhs: Option<BytesRef>,
    rhs: Option<BytesRef>,
    operation: BinaryArithmeticOperation,
    native: bool,
) -> Result<Option<Bytes>> {
    let lhs = binary_raw_f64(lhs)?;
    let rhs = binary_raw_f64(rhs)?;
    let Some((lhs, rhs)) = lhs.zip(rhs) else {
        return Ok(None);
    };
    let value = binary_arithmetic_value(lhs, rhs, operation);
    if native && !value.is_finite() {
        return Err(native_binary_arithmetic_error(
            operation,
            BinaryArithmeticErrorKind::FloatOverflow,
        ));
    }
    Ok(Some(value.to_bits().to_le_bytes().to_vec()))
}

macro_rules! real_binary_recipe {
    ($name:ident, $operation:ident, $native:expr) => {
        #[rpn_fn(nullable)]
        fn $name(lhs: Option<BytesRef>, rhs: Option<BytesRef>) -> Result<Option<Bytes>> {
            real_binary(lhs, rhs, BinaryArithmeticOperation::$operation, $native)
        }
    };
}

real_binary_recipe!(add_real_native, Add, true);
real_binary_recipe!(sub_real_native, Subtract, true);
real_binary_recipe!(mul_real_native, Multiply, true);
real_binary_recipe!(add_real_legacy, Add, false);
real_binary_recipe!(sub_real_legacy, Subtract, false);
real_binary_recipe!(mul_real_legacy, Multiply, false);

fn decimal_binary(
    lhs: &Decimal,
    rhs: &Decimal,
    budget: &Int,
    operation: BinaryArithmeticOperation,
    native: bool,
) -> Result<Option<Decimal>> {
    let value = lhs
        .try_native_binary(
            rhs,
            operation.decimal(),
            NativeDecimalBinaryPolicy::MySql,
            native_decimal_budget(budget)?,
        )
        .map_err(native_decimal_failure)?;
    match value {
        Res::Overflow(_) if native => Err(native_binary_arithmetic_error(
            operation,
            BinaryArithmeticErrorKind::DecimalOverflow,
        )),
        Res::Ok(value) | Res::Truncated(value) | Res::Overflow(value) => Ok(Some(value)),
    }
}

macro_rules! decimal_binary_recipe {
    ($name:ident, $operation:ident, $native:expr) => {
        #[rpn_fn]
        fn $name(lhs: &Decimal, rhs: &Decimal, budget: &Int) -> Result<Option<Decimal>> {
            decimal_binary(
                lhs,
                rhs,
                budget,
                BinaryArithmeticOperation::$operation,
                $native,
            )
        }
    };
}

decimal_binary_recipe!(add_decimal_native, Add, true);
decimal_binary_recipe!(sub_decimal_native, Subtract, true);
decimal_binary_recipe!(mul_decimal_native, Multiply, true);
decimal_binary_recipe!(add_decimal_legacy, Add, false);
decimal_binary_recipe!(sub_decimal_legacy, Subtract, false);
decimal_binary_recipe!(mul_decimal_legacy, Multiply, false);

fn decimal_fast_binary(
    lhs: &Decimal,
    rhs: &Decimal,
    budget: &Int,
    operation: BinaryArithmeticOperation,
) -> Result<Option<Bytes>> {
    let budget = native_decimal_budget(budget)?;
    let left = lhs
        .try_native_fast_value(budget)
        .map_err(native_decimal_failure)?;
    let Some(left) = left else {
        return Ok(Some(encode_native_decimal_fast_outcome(None)));
    };
    let right = rhs
        .try_native_fast_value(budget)
        .map_err(native_decimal_failure)?;
    let value =
        right.and_then(|right| native_decimal_fast_binary(left, right, operation.decimal()));
    Ok(Some(encode_native_decimal_fast_outcome(value)))
}

macro_rules! decimal_fast_binary_recipe {
    ($name:ident, $operation:ident) => {
        #[rpn_fn]
        fn $name(lhs: &Decimal, rhs: &Decimal, budget: &Int) -> Result<Option<Bytes>> {
            decimal_fast_binary(lhs, rhs, budget, BinaryArithmeticOperation::$operation)
        }
    };
}

decimal_fast_binary_recipe!(add_decimal_fast_native, Add);
decimal_fast_binary_recipe!(sub_decimal_fast_native, Subtract);
decimal_fast_binary_recipe!(mul_decimal_fast_native, Multiply);

fn binary_raw_i128(bytes: BytesRef) -> Result<i128> {
    let bytes = <[u8; 16]>::try_from(bytes)
        .map_err(|_| other_err!("Binary i128 transport requires exactly 16 bytes"))?;
    Ok(i128::from_le_bytes(bytes))
}

#[derive(Clone, Copy)]
enum LegacyNegativeOperand {
    Neither,
    Left,
    Right,
}

fn legacy_integer_binary(
    lhs: BytesRef,
    rhs: BytesRef,
    operation: BinaryArithmeticOperation,
    unsigned: bool,
    reject: LegacyNegativeOperand,
) -> Result<Option<Bytes>> {
    let lhs = binary_raw_i128(lhs)?;
    let rhs = binary_raw_i128(rhs)?;
    // Inputs remain full i128 through checked arithmetic, before the output
    // range or the independently locked legacy negative-operand policy is used.
    let raw = match operation {
        BinaryArithmeticOperation::Add => lhs.checked_add(rhs),
        BinaryArithmeticOperation::Subtract => lhs.checked_sub(rhs),
        BinaryArithmeticOperation::Multiply => lhs.checked_mul(rhs),
        BinaryArithmeticOperation::Modulo => {
            return Err(other_err!("MOD requires its dedicated remainder recipe"));
        }
    };
    let negative = match reject {
        LegacyNegativeOperand::Neither => false,
        LegacyNegativeOperand::Left => lhs < 0,
        LegacyNegativeOperand::Right => rhs < 0,
    };
    let (low, high) = if unsigned {
        (0, u64::MAX as i128)
    } else {
        (i64::MIN as i128, i64::MAX as i128)
    };
    let value = raw
        .filter(|value| !negative && (low..=high).contains(value))
        .ok_or_else(|| {
            tidb_query_common::Error::from(EvaluateError::Caused(Box::new(
                LegacyBinaryArithmeticError {
                    operation,
                    unsigned,
                },
            )))
        })?;
    Ok(Some(value.to_le_bytes().to_vec()))
}

macro_rules! legacy_integer_binary_recipe {
    ($name:ident, $operation:ident, $unsigned:expr, $reject:ident) => {
        #[rpn_fn]
        fn $name(lhs: BytesRef, rhs: BytesRef) -> Result<Option<Bytes>> {
            legacy_integer_binary(
                lhs,
                rhs,
                BinaryArithmeticOperation::$operation,
                $unsigned,
                LegacyNegativeOperand::$reject,
            )
        }
    };
}

legacy_integer_binary_recipe!(add_int128_signed_legacy, Add, false, Neither);
legacy_integer_binary_recipe!(add_int128_unsigned_legacy, Add, true, Neither);
legacy_integer_binary_recipe!(add_int128_reject_left_legacy, Add, true, Left);
legacy_integer_binary_recipe!(add_int128_reject_right_legacy, Add, true, Right);
legacy_integer_binary_recipe!(sub_int128_signed_legacy, Subtract, false, Neither);
legacy_integer_binary_recipe!(sub_int128_unsigned_legacy, Subtract, true, Neither);
legacy_integer_binary_recipe!(sub_int128_reject_left_legacy, Subtract, true, Left);
legacy_integer_binary_recipe!(sub_int128_reject_right_legacy, Subtract, true, Right);
legacy_integer_binary_recipe!(mul_int128_signed_legacy, Multiply, false, Neither);
legacy_integer_binary_recipe!(mul_int128_unsigned_legacy, Multiply, true, Neither);

// Closed MOD value recipes accept non-NULL operands. The frontend owns NULL
// witnesses, result signedness, and warnings after a successful zero divisor.
#[rpn_fn]
fn mod_int_ss_native(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
    if *rhs == 0 {
        return Ok(None);
    }
    Ok(Some(lhs.wrapping_rem(*rhs)))
}

#[rpn_fn]
fn mod_int_su_native(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
    if *rhs == 0 {
        return Ok(None);
    }
    let value = if *lhs < 0 {
        // Preserve the original native negation, including its overflow quirk.
        -((lhs.unsigned_abs() % (*rhs as u64)) as i64)
    } else {
        ((*lhs as u64) % (*rhs as u64)) as i64
    };
    Ok(Some(value))
}

#[rpn_fn]
fn mod_int_us_native(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
    if *rhs == 0 {
        return Ok(None);
    }
    Ok(Some(((*lhs as u64) % rhs.unsigned_abs()) as i64))
}

#[rpn_fn]
fn mod_int_uu_native(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
    if *rhs == 0 {
        return Ok(None);
    }
    Ok(Some(((*lhs as u64) % (*rhs as u64)) as i64))
}

#[rpn_fn]
fn mod_int128_legacy(lhs: BytesRef, rhs: BytesRef) -> Result<Option<Bytes>> {
    let lhs = binary_raw_i128(lhs)?;
    let rhs = binary_raw_i128(rhs)?;
    if rhs == 0 {
        return Ok(None);
    }
    // Deliberately retain full-width legacy %, including MIN % -1 panic.
    Ok(Some((lhs % rhs).to_le_bytes().to_vec()))
}

fn real_modulo(lhs: BytesRef, rhs: BytesRef, native: bool) -> Result<Option<Bytes>> {
    let lhs = binary_raw_f64(Some(lhs))?.expect("non-NULL MOD operand");
    let rhs = binary_raw_f64(Some(rhs))?.expect("non-NULL MOD operand");
    if rhs == 0.0 {
        return Ok(None);
    }
    let value = lhs % rhs;
    if native && !value.is_finite() {
        return Err(native_binary_arithmetic_error(
            BinaryArithmeticOperation::Modulo,
            BinaryArithmeticErrorKind::FloatOverflow,
        ));
    }
    Ok(Some(value.to_bits().to_le_bytes().to_vec()))
}

#[rpn_fn]
fn mod_real_native(lhs: BytesRef, rhs: BytesRef) -> Result<Option<Bytes>> {
    real_modulo(lhs, rhs, true)
}

#[rpn_fn]
fn mod_real_legacy(lhs: BytesRef, rhs: BytesRef) -> Result<Option<Bytes>> {
    real_modulo(lhs, rhs, false)
}

#[rpn_fn]
fn mod_decimal_native(lhs: &Decimal, rhs: &Decimal, budget: &Int) -> Result<Option<Decimal>> {
    lhs.try_native_rem(rhs, native_decimal_budget(budget)?)
        .map_err(native_decimal_failure)
}

#[rpn_fn(nullable)]
fn binary_arithmetic_null_native(witness: Option<&Int>) -> Result<Option<Int>> {
    match witness {
        None => Ok(None),
        Some(_) => Err(other_err!(
            "Binary arithmetic NULL witness must be an actual NULL"
        )),
    }
}

#[rpn_fn]
fn binary_arithmetic_missing_legacy() -> Result<Option<Int>> {
    Ok(None)
}

#[rpn_fn]
#[inline]
pub fn arithmetic<A: ArithmeticOp>(lhs: &A::T, rhs: &A::T) -> Result<Option<A::T>> {
    A::calc(lhs, rhs)
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn arithmetic_with_ctx<A: ArithmeticOpWithCtx>(
    ctx: &mut EvalContext,
    lhs: &A::T,
    rhs: &A::T,
) -> Result<Option<A::T>> {
    A::calc(ctx, lhs, rhs)
}

pub trait ArithmeticOp {
    type T: Evaluable + EvaluableRet;

    fn calc(lhs: &Self::T, rhs: &Self::T) -> Result<Option<Self::T>>;
}

pub trait ArithmeticOpWithCtx {
    type T: Evaluable + EvaluableRet;

    fn calc(ctx: &mut EvalContext, lhs: &Self::T, rhs: &Self::T) -> Result<Option<Self::T>>;
}

#[derive(Debug)]
pub struct IntIntPlus;

impl ArithmeticOp for IntIntPlus {
    type T = Int;

    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        checked_add_integer(*lhs, *rhs, false, false)
            .ok_or_else(|| Error::overflow("BIGINT", format!("({} + {})", lhs, rhs)).into())
            .map(Some)
    }
}

#[derive(Debug)]
pub struct IntUintPlus;

impl ArithmeticOp for IntUintPlus {
    type T = Int;

    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        checked_add_integer(*lhs, *rhs, false, true)
            .ok_or_else(|| {
                Error::overflow("BIGINT UNSIGNED", format!("({} + {})", lhs, rhs)).into()
            })
            .map(Some)
    }
}

#[derive(Debug)]
pub struct UintIntPlus;

impl ArithmeticOp for UintIntPlus {
    type T = Int;

    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        IntUintPlus::calc(rhs, lhs)
    }
}

#[derive(Debug)]
pub struct UintUintPlus;

impl ArithmeticOp for UintUintPlus {
    type T = Int;

    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        checked_add_integer(*lhs, *rhs, true, true)
            .ok_or_else(|| {
                Error::overflow("BIGINT UNSIGNED", format!("({} + {})", lhs, rhs)).into()
            })
            .map(Some)
    }
}

#[derive(Debug)]
pub struct RealPlus;

impl ArithmeticOp for RealPlus {
    type T = Real;

    fn calc(lhs: &Real, rhs: &Real) -> Result<Option<Real>> {
        let res = binary_arithmetic_value(*lhs, *rhs, BinaryArithmeticOperation::Add);
        if !res.is_finite() {
            return Err(Error::overflow("DOUBLE", format!("({} + {})", lhs, rhs)).into());
        }
        Ok(Some(res))
    }
}

#[derive(Debug)]
pub struct DecimalPlus;

impl ArithmeticOp for DecimalPlus {
    type T = Decimal;

    fn calc(lhs: &Decimal, rhs: &Decimal) -> Result<Option<Decimal>> {
        let res: codec::Result<Decimal> = (lhs + rhs).into();
        Ok(Some(res?))
    }
}

#[derive(Debug)]
pub struct IntIntMinus;

impl ArithmeticOp for IntIntMinus {
    type T = Int;

    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        checked_subtract_integer(*lhs, *rhs, false, false, IntegerSubtractionPolicy::Wire)
            .ok_or_else(|| Error::overflow("BIGINT", format!("({} - {})", lhs, rhs)).into())
            .map(Some)
    }
}

#[derive(Debug)]
pub struct IntUintMinus;

impl ArithmeticOp for IntUintMinus {
    type T = Int;

    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        checked_subtract_integer(*lhs, *rhs, false, true, IntegerSubtractionPolicy::Wire)
            .ok_or_else(|| Error::overflow("BIGINT", format!("({} - {})", lhs, rhs)).into())
            .map(Some)
    }
}

#[derive(Debug)]
pub struct UintIntMinus;

impl ArithmeticOp for UintIntMinus {
    type T = Int;

    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        checked_subtract_integer(*lhs, *rhs, true, false, IntegerSubtractionPolicy::Wire)
            .ok_or_else(|| Error::overflow("BIGINT", format!("({} - {})", lhs, rhs)).into())
            .map(Some)
    }
}

#[derive(Debug)]
pub struct UintUintMinus;

impl ArithmeticOp for UintUintMinus {
    type T = Int;

    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        checked_subtract_integer(*lhs, *rhs, true, true, IntegerSubtractionPolicy::Wire)
            .ok_or_else(|| {
                Error::overflow("BIGINT UNSIGNED", format!("({} - {})", lhs, rhs)).into()
            })
            .map(Some)
    }
}

#[derive(Debug)]
pub struct RealMinus;

impl ArithmeticOp for RealMinus {
    type T = Real;

    fn calc(lhs: &Real, rhs: &Real) -> Result<Option<Real>> {
        let res = binary_arithmetic_value(*lhs, *rhs, BinaryArithmeticOperation::Subtract);
        if !res.is_finite() {
            return Err(Error::overflow("DOUBLE", format!("({} - {})", lhs, rhs)).into());
        }
        Ok(Some(res))
    }
}

#[derive(Debug)]
pub struct DecimalMinus;

impl ArithmeticOp for DecimalMinus {
    type T = Decimal;

    fn calc(lhs: &Decimal, rhs: &Decimal) -> Result<Option<Decimal>> {
        let res: codec::Result<Decimal> = (lhs - rhs).into();
        Ok(Some(res?))
    }
}

#[derive(Debug)]
pub struct IntIntMod;

impl ArithmeticOp for IntIntMod {
    type T = Int;

    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        if *rhs == 0i64 {
            return Ok(None);
        }
        Ok(Some(lhs % rhs))
    }
}

#[derive(Debug)]
pub struct IntUintMod;

impl ArithmeticOp for IntUintMod {
    type T = Int;

    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        if *rhs == 0i64 {
            return Ok(None);
        }

        if *lhs > 0 {
            Ok(Some(((*lhs as u64) % (*rhs as u64)) as i64))
        } else {
            Ok(Some(
                0i64.overflowing_sub(((lhs.overflowing_abs().0 as u64) % (*rhs as u64)) as i64)
                    .0,
            ))
        }
    }
}

#[derive(Debug)]
pub struct UintIntMod;

impl ArithmeticOp for UintIntMod {
    type T = Int;

    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        if *rhs == 0i64 {
            return Ok(None);
        }
        Ok(Some(
            ((*lhs as u64) % (rhs.overflowing_abs().0 as u64)) as i64,
        ))
    }
}

#[derive(Debug)]
pub struct UintUintMod;
impl ArithmeticOp for UintUintMod {
    type T = Int;

    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        if *rhs == 0i64 {
            return Ok(None);
        }
        Ok(Some(((*lhs as u64) % (*rhs as u64)) as i64))
    }
}

#[derive(Debug)]
pub struct RealMod;

impl ArithmeticOp for RealMod {
    type T = Real;

    fn calc(lhs: &Real, rhs: &Real) -> Result<Option<Real>> {
        if rhs.into_inner() == 0f64 {
            return Ok(None);
        }
        Ok(Some(*lhs % *rhs))
    }
}

#[derive(Debug)]
pub struct DecimalMod;

impl ArithmeticOpWithCtx for DecimalMod {
    type T = Decimal;

    fn calc(ctx: &mut EvalContext, lhs: &Decimal, rhs: &Decimal) -> Result<Option<Decimal>> {
        Ok(if let Some(value) = lhs % rhs {
            value
                .into_result_with_overflow_err_lazy(ctx, || {
                    Error::overflow("DECIMAL", format!("({} % {})", lhs, rhs))
                })
                .map(Some)
        } else {
            ctx.handle_division_by_zero().map(|_| None)
        }?)
    }
}

#[derive(Debug)]
pub struct DecimalMultiply;

impl ArithmeticOp for DecimalMultiply {
    type T = Decimal;

    fn calc(lhs: &Decimal, rhs: &Decimal) -> Result<Option<Decimal>> {
        let res: codec::Result<Decimal> = match lhs * rhs {
            codec::mysql::Res::Ok(t) => Ok(t),
            codec::mysql::Res::Truncated(t) => Ok(t),
            other => other.into(),
        };

        Ok(Some(res?))
    }
}

#[derive(Debug)]
pub struct RealMultiply;

impl ArithmeticOp for RealMultiply {
    type T = Real;
    fn calc(lhs: &Real, rhs: &Real) -> Result<Option<Real>> {
        let res = binary_arithmetic_value(*lhs, *rhs, BinaryArithmeticOperation::Multiply);
        if res.is_infinite() {
            Err(Error::overflow("REAL", format!("({} * {})", lhs, rhs)).into())
        } else {
            Ok(Some(res))
        }
    }
}

#[derive(Debug)]
pub struct IntIntMultiply;

impl ArithmeticOp for IntIntMultiply {
    type T = Int;
    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        checked_multiply_integer(*lhs, *rhs, false)
            .ok_or_else(|| Error::overflow("BIGINT", format!("({} * {})", lhs, rhs)).into())
            .map(Some)
    }
}

#[derive(Debug)]
pub struct IntUintMultiply;

impl ArithmeticOp for IntUintMultiply {
    type T = Int;
    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        // Wire mixed multiply rejects a negative signed operand, unlike the
        // native unsigned recipe which intentionally reads both raw u64 values.
        if *lhs >= 0 {
            checked_multiply_integer(*lhs, *rhs, true)
        } else {
            None
        }
        .ok_or_else(|| Error::overflow("BIGINT UNSIGNED", format!("({} * {})", lhs, rhs)).into())
        .map(Some)
    }
}

#[derive(Debug)]
pub struct UintIntMultiply;

impl ArithmeticOp for UintIntMultiply {
    type T = Int;
    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        IntUintMultiply::calc(rhs, lhs)
    }
}

#[derive(Debug)]
pub struct UintUintMultiply;

impl ArithmeticOp for UintUintMultiply {
    type T = Int;
    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        checked_multiply_integer(*lhs, *rhs, true)
            .ok_or_else(|| {
                Error::overflow("BIGINT UNSIGNED", format!("({} * {})", lhs, rhs)).into()
            })
            .map(Some)
    }
}

#[derive(Debug)]
pub struct IntDivideInt;

impl ArithmeticOp for IntDivideInt {
    type T = Int;

    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        if *rhs == 0 {
            return Ok(None);
        }
        Ok(Some(div_i64(*lhs, *rhs)?))
    }
}

#[derive(Debug)]
pub struct IntDivideUint;

impl ArithmeticOp for IntDivideUint {
    type T = Int;

    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        if *rhs == 0 {
            return Ok(None);
        }
        Ok(Some(div_i64_with_u64(*lhs, *rhs as u64).map(|r| r as i64)?))
    }
}

#[derive(Debug)]
pub struct UintDivideUint;

impl ArithmeticOp for UintDivideUint {
    type T = Int;

    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        if *rhs == 0 {
            return Ok(None);
        }
        Ok(Some(((*lhs as u64) / (*rhs as u64)) as i64))
    }
}

#[derive(Debug)]
pub struct UintDivideInt;

impl ArithmeticOp for UintDivideInt {
    type T = Int;

    fn calc(lhs: &Int, rhs: &Int) -> Result<Option<Int>> {
        if *rhs == 0 {
            return Ok(None);
        }
        Ok(Some(div_u64_with_i64(*lhs as u64, *rhs).map(|r| r as i64)?))
    }
}

#[rpn_fn(capture = [ctx])]
#[inline]
fn int_divide_decimal(ctx: &mut EvalContext, lhs: &Decimal, rhs: &Decimal) -> Result<Option<Int>> {
    let result = arithmetic_with_ctx::<DecimalDivide>(ctx, lhs, rhs)?;
    if let Some(result) = result {
        let result = result.as_i64();
        match result {
            Res::Ok(i) => Ok(Some(i)),
            Res::Truncated(i) => Ok(Some(i)),
            _ => Err(Error::overflow("BIGINT", format!("({} / {})", lhs, rhs)).into()),
        }
    } else {
        Ok(None)
    }
}

#[rpn_fn(capture = [ctx])]
#[inline]
fn int_divide_decimal_unsigned(
    ctx: &mut EvalContext,
    lhs: &Decimal,
    rhs: &Decimal,
) -> Result<Option<Int>> {
    let result = arithmetic_with_ctx::<DecimalDivide>(ctx, lhs, rhs)?;
    if let Some(result) = result {
        let unsigned_result = result.as_u64();
        if unsigned_result.is_overflow() {
            let signed_result = result.as_i64();
            return if signed_result.unwrap() == 0 && signed_result.is_truncated() {
                Ok(Some(0))
            } else {
                Err(Error::overflow("BIGINT UNSIGNED", format!("({} / {})", lhs, rhs)).into())
            };
        }
        return Ok(Some(unsigned_result.unwrap() as i64));
    }
    Ok(None)
}

pub struct DecimalDivide;

impl ArithmeticOpWithCtx for DecimalDivide {
    type T = Decimal;

    fn calc(ctx: &mut EvalContext, lhs: &Decimal, rhs: &Decimal) -> Result<Option<Decimal>> {
        Ok(
            if let Some(value) = lhs.div(rhs, ctx.cfg.div_precision_increment) {
                value
                    .into_result_with_overflow_err_lazy(ctx, || {
                        Error::overflow("DECIMAL", format!("({} / {})", lhs, rhs))
                    })
                    .map(Some)
            } else {
                // TODO: handle RpnFuncExtra's field_type, round the result if is needed.
                ctx.handle_division_by_zero().map(|_| None)
            }?,
        )
    }
}

pub struct RealDivide;

impl ArithmeticOpWithCtx for RealDivide {
    type T = Real;

    fn calc(ctx: &mut EvalContext, lhs: &Real, rhs: &Real) -> Result<Option<Real>> {
        Ok(if rhs.is_zero() {
            ctx.handle_division_by_zero().map(|_| None)?
        } else {
            let result = *lhs / *rhs;
            if result.is_infinite() {
                ctx.handle_overflow_err(Error::overflow("DOUBLE", format!("{} / {}", lhs, rhs)))
                    .map(|_| None)?
            } else {
                Some(result)
            }
        })
    }
}

#[cfg(test)]
mod native_modulo_tests {
    use tidb_query_common::error::ErrorInner;

    use super::*;

    #[test]
    fn modulo_integer_profiles_zero_and_full_width() {
        assert_eq!(mod_int_ss_native(&i64::MIN, &-1).unwrap(), Some(0));
        assert_eq!(mod_int_ss_native(&-13, &5).unwrap(), Some(-3));
        assert_eq!(mod_int_su_native(&-13, &5).unwrap(), Some(-3));
        assert_eq!(mod_int_su_native(&i64::MIN, &3).unwrap(), Some(-2));
        assert_eq!(mod_int_us_native(&-1, &i64::MIN).unwrap(), Some(i64::MAX));
        assert_eq!(mod_int_uu_native(&-2, &-1).unwrap(), Some(-2));
        for recipe in [
            mod_int_ss_native,
            mod_int_su_native,
            mod_int_us_native,
            mod_int_uu_native,
        ] {
            assert_eq!(recipe(&i64::MIN, &0).unwrap(), None);
        }
        let lhs = (1_i128 << 100) + 7;
        let rhs = (1_i128 << 99) + 1;
        let result = mod_int128_legacy(&lhs.to_le_bytes(), &rhs.to_le_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(binary_raw_i128(&result).unwrap(), 5);
        assert_eq!(
            mod_int128_legacy(&lhs.to_le_bytes(), &0_i128.to_le_bytes()).unwrap(),
            None
        );
        assert!(mod_int128_legacy(b"short", &0_i128.to_le_bytes()).is_err());
        assert!(
            std::panic::catch_unwind(|| {
                mod_int128_legacy(&i128::MIN.to_le_bytes(), &(-1_i128).to_le_bytes())
            })
            .is_err()
        );
    }

    #[test]
    fn modulo_real_raw_bits_errors_and_decimal_zero() {
        let bits = |value: f64| value.to_bits().to_le_bytes();
        let negative_zero = bits(-0.0);
        let two = bits(2.0);
        for recipe in [mod_real_native, mod_real_legacy] {
            assert_eq!(
                recipe(&negative_zero, &two).unwrap(),
                Some(negative_zero.to_vec())
            );
            assert_eq!(recipe(&two, &negative_zero).unwrap(), None);
            assert!(recipe(b"short", &negative_zero).is_err());
            assert!(recipe(&two, b"short").is_err());
        }
        for value in [f64::INFINITY, f64::from_bits(0x7ff8_0000_0000_0042)] {
            let error = mod_real_native(&bits(value), &two).unwrap_err();
            match error.0.as_ref() {
                ErrorInner::Evaluate(EvaluateError::Caused(cause)) => assert_eq!(
                    cause.downcast_ref::<NativeBinaryArithmeticError>(),
                    Some(&NativeBinaryArithmeticError {
                        operation: BinaryArithmeticOperation::Modulo,
                        kind: BinaryArithmeticErrorKind::FloatOverflow,
                    })
                ),
                _ => panic!("lost typed MOD error: {error:?}"),
            }
            let legacy = mod_real_legacy(&bits(value), &two).unwrap().unwrap();
            assert!(binary_raw_f64(Some(&legacy)).unwrap().unwrap().is_nan());
        }
        assert_eq!(
            mod_real_native(&two, &bits(f64::INFINITY)).unwrap(),
            Some(two.to_vec())
        );
        let lhs = Decimal::from(-13_i64);
        let rhs = Decimal::from(5_i64);
        assert_eq!(
            mod_decimal_native(&lhs, &rhs, &4096).unwrap(),
            Some(Decimal::from(-3_i64))
        );
        assert_eq!(
            mod_decimal_native(&lhs, &Decimal::from(0_i64), &4096).unwrap(),
            None
        );
        assert!(mod_decimal_native(&lhs, &rhs, &-1).is_err());
    }
}

#[cfg(test)]
mod native_binary_tests {
    use tidb_query_common::error::ErrorInner;
    use tidb_query_datatype::codec::{convert::ToStringValue, mysql::decimal::NativeDecimalError};

    use super::*;

    fn actual_cause<T: std::error::Error + 'static>(error: &tidb_query_common::Error) -> &T {
        match error.0.as_ref() {
            ErrorInner::Evaluate(EvaluateError::Caused(cause)) => {
                cause.downcast_ref().expect("actual arithmetic cause")
            }
            _ => panic!("lost arithmetic cause: {error:?}"),
        }
    }

    #[test]
    fn native_integer_branches_and_wire_policy_are_distinct() {
        assert_eq!(add_int_su_native(&-1, &1).unwrap(), Some(0));
        assert_eq!(add_int_us_native(&1, &-1).unwrap(), Some(0));
        assert_eq!(add_int_uu_native(&-2, &1).unwrap(), Some(-1));
        let overflow = add_int_ss_native(&i64::MAX, &1).unwrap_err();
        assert_eq!(
            *actual_cause::<NativeBinaryArithmeticError>(&overflow),
            NativeBinaryArithmeticError {
                operation: BinaryArithmeticOperation::Add,
                kind: BinaryArithmeticErrorKind::IntOverflow,
            }
        );
        // Preserve the actual native Go edge; don't repair it with wire policy.
        assert_eq!(sub_int_ss_native(&0, &i64::MIN).unwrap(), Some(i64::MIN));
        assert!(IntIntMinus::calc(&0, &i64::MIN).is_err());
        assert!(sub_int_uu_native(&1, &2).is_err());
        assert_eq!(sub_int_uu_forced_native(&1, &2).unwrap(), Some(-1));
        assert_eq!(sub_int_uu_native(&-1, &0).unwrap(), Some(-1));
        assert!(sub_int_uu_forced_native(&-1, &0).is_err());
        assert_eq!(sub_int_su_forced_native(&-1, &1).unwrap(), Some(-2));
        assert_eq!(sub_int_us_forced_native(&0, &1).unwrap(), Some(-1));
        assert_eq!(mul_int_unsigned_native(&-1, &1).unwrap(), Some(-1));
        assert_eq!(mul_int_unsigned_native(&-1, &0).unwrap(), Some(0));
        assert!(IntUintMultiply::calc(&-1, &0).is_err());
        assert!(mul_int_unsigned_native(&-1, &2).is_err());
        assert_eq!(binary_arithmetic_null_native(None).unwrap(), None);
        assert!(binary_arithmetic_null_native(Some(&0)).is_err());
        assert_eq!(binary_arithmetic_missing_legacy().unwrap(), None);
    }

    #[test]
    fn legacy_full_i128_and_raw_ieee_are_not_native_domains() {
        let large = 1_i128 << 100;
        let result = add_int128_signed_legacy(&large.to_le_bytes(), &(-large).to_le_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(binary_raw_i128(&result).unwrap(), 0);
        let result = sub_int128_unsigned_legacy(&(-1_i128).to_le_bytes(), &(-2_i128).to_le_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(binary_raw_i128(&result).unwrap(), 1);
        let rejected =
            sub_int128_reject_left_legacy(&(-1_i128).to_le_bytes(), &(-2_i128).to_le_bytes())
                .unwrap_err();
        assert_eq!(
            *actual_cause::<LegacyBinaryArithmeticError>(&rejected),
            LegacyBinaryArithmeticError {
                operation: BinaryArithmeticOperation::Subtract,
                unsigned: true,
            }
        );
        assert!(
            add_int128_reject_right_legacy(&2_i128.to_le_bytes(), &(-1_i128).to_le_bytes())
                .is_err()
        );
        let result = mul_int128_unsigned_legacy(&(-1_i128).to_le_bytes(), &(-1_i128).to_le_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(binary_raw_i128(&result).unwrap(), 1);
        assert!(add_int128_signed_legacy(&i128::MAX.to_le_bytes(), &1_i128.to_le_bytes()).is_err());
        assert!(add_int128_signed_legacy(b"short", &0_i128.to_le_bytes()).is_err());
        let max = f64::MAX.to_bits().to_le_bytes();
        let overflow = add_real_native(Some(&max), Some(&max)).unwrap_err();
        assert_eq!(
            *actual_cause::<NativeBinaryArithmeticError>(&overflow),
            NativeBinaryArithmeticError {
                operation: BinaryArithmeticOperation::Add,
                kind: BinaryArithmeticErrorKind::FloatOverflow,
            }
        );
        let legacy = add_real_legacy(Some(&max), Some(&max)).unwrap().unwrap();
        assert_eq!(binary_raw_f64(Some(&legacy)).unwrap(), Some(f64::INFINITY));
        let negative_zero = (-0.0_f64).to_bits().to_le_bytes();
        let two = 2.0_f64.to_bits().to_le_bytes();
        assert_eq!(
            mul_real_native(Some(&negative_zero), Some(&two)).unwrap(),
            Some(negative_zero.to_vec())
        );
        assert_eq!(sub_real_legacy(None, Some(&two)).unwrap(), None);
    }

    #[test]
    fn decimal_status_fast_value_and_unsupported_are_separate() {
        let maximum = "9".repeat(81);
        let wide = Decimal::try_from_native_digits(false, maximum.as_bytes(), 0, 0, 4096).unwrap();
        let one = Decimal::from(1_i64);
        let overflow = add_decimal_native(&wide, &one, &4096).unwrap_err();
        assert_eq!(
            *actual_cause::<NativeBinaryArithmeticError>(&overflow),
            NativeBinaryArithmeticError {
                operation: BinaryArithmeticOperation::Add,
                kind: BinaryArithmeticErrorKind::DecimalOverflow,
            }
        );
        assert_eq!(
            add_decimal_legacy(&wide, &one, &4096)
                .unwrap()
                .unwrap()
                .to_string_value(),
            maximum
        );
        let left = Decimal::try_from_native_fast(
            NativeDecimalFastValue {
                coefficient: 123,
                storage_scale: 2,
                scale: 1,
            },
            4096,
        )
        .unwrap();
        let right = Decimal::try_from_native_fast(
            NativeDecimalFastValue {
                coefficient: 20,
                storage_scale: 2,
                scale: 1,
            },
            4096,
        )
        .unwrap();
        let fast = add_decimal_fast_native(&left, &right, &4096).unwrap();
        assert_eq!(
            decode_native_decimal_fast_outcome(fast.as_deref()).unwrap(),
            NativeDecimalFastOutcome::Value(Some(NativeDecimalFastValue {
                coefficient: 143,
                storage_scale: 2,
                scale: 1,
            }))
        );
        let minimum = Decimal::try_from_native_fast(
            NativeDecimalFastValue {
                coefficient: i128::MIN,
                storage_scale: 0,
                scale: 0,
            },
            4096,
        )
        .unwrap();
        let fast = sub_decimal_fast_native(&minimum, &minimum, &4096).unwrap();
        assert_eq!(fast, Some(vec![0])); // rhs.checked_neg fails before subtraction
        assert_eq!(
            decode_native_decimal_fast_outcome(fast.as_deref()).unwrap(),
            NativeDecimalFastOutcome::Unsupported
        );
        assert!(
            sub_decimal_native(&minimum, &minimum, &4096)
                .unwrap()
                .unwrap()
                .is_zero()
        );
        assert_eq!(
            decode_native_decimal_fast_outcome(None).unwrap(),
            NativeDecimalFastOutcome::Value(None)
        );
        for bad in [&b""[..], &b"\0\0"[..], &b"\x01"[..], &b"\x02"[..]] {
            assert!(decode_native_decimal_fast_outcome(Some(bad)).is_err());
        }
        let bad_shape = encode_native_decimal_fast_outcome(Some(NativeDecimalFastValue {
            coefficient: 0,
            storage_scale: 1,
            scale: 2,
        }));
        assert!(decode_native_decimal_fast_outcome(Some(&bad_shape)).is_err());
        let resource = mul_decimal_fast_native(&left, &right, &1).unwrap_err();
        assert!(matches!(
            actual_cause::<NativeDecimalError>(&resource),
            NativeDecimalError::Resource(_)
        ));
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use tidb_query_datatype::{
        FieldTypeFlag, FieldTypeTp,
        builder::FieldTypeBuilder,
        codec::error::ERR_DIVISION_BY_ZERO,
        expr::{EvalConfig, Flag, SqlMode},
    };
    use tipb::ScalarFuncSig;

    use super::*;
    use crate::test_util::RpnFnScalarEvaluator;

    #[test]
    fn test_decimal_mod_div_lazy_bounded_compatibility() {
        use std::sync::Arc;

        use tidb_query_datatype::codec::mysql::DecimalDecoder;

        // The previous eager adapter is a bounded compatibility reference,
        // not a second arithmetic engine or an oversized-text policy.
        fn eager(
            ctx: &mut EvalContext,
            lhs: &Decimal,
            rhs: &Decimal,
            divide: bool,
        ) -> Result<Option<Decimal>> {
            let result = if divide {
                lhs.div(rhs, ctx.cfg.div_precision_increment)
            } else {
                lhs % rhs
            };
            Ok(if let Some(value) = result {
                let error = if divide {
                    Error::overflow("DECIMAL", format!("({} / {})", lhs, rhs))
                } else {
                    Error::overflow("DECIMAL", format!("({} % {})", lhs, rhs))
                };
                value.into_result_with_overflow_err(ctx, error).map(Some)
            } else {
                ctx.handle_division_by_zero().map(|_| None)
            }?)
        }
        let mut pairs = vec![
            (Decimal::from_str("12.345").unwrap(), Decimal::from(2)),
            (Decimal::from_str("-12.345").unwrap(), Decimal::from(-2)),
            (Decimal::from(1), Decimal::from(3)),
            (Decimal::from(1), Decimal::zero()),
            (
                Decimal::from_str(&format!("1{}", "0".repeat(80))).unwrap(),
                Decimal::from_str("0.01").unwrap(),
            ),
        ];
        for visible in [0, 30, 81, 127, 128, 255] {
            let mut cell = [0; 40];
            cell[..4].copy_from_slice(&[2, 1, visible, 1]);
            cell[4..8].copy_from_slice(&12_u32.to_ne_bytes());
            cell[8..12].copy_from_slice(&300_000_000_u32.to_ne_bytes());
            pairs.push((
                cell.as_slice().read_decimal_from_chunk().unwrap(),
                Decimal::from(2),
            ));
        }
        for (lhs, rhs) in pairs {
            for flags in [
                Flag::empty(),
                Flag::TRUNCATE_AS_WARNING | Flag::OVERFLOW_AS_WARNING,
                Flag::IN_INSERT_STMT,
            ] {
                for divide in [false, true] {
                    let mut config = EvalConfig::from_flag(flags);
                    config.set_max_warning_cnt(1);
                    config.sql_mode =
                        SqlMode::ERROR_FOR_DIVISION_BY_ZERO | SqlMode::STRICT_ALL_TABLES;
                    let config = Arc::new(config);
                    let mut before = EvalContext::new(config.clone());
                    let mut after = EvalContext::new(config);
                    for ctx in [&mut before, &mut after] {
                        ctx.warnings
                            .append_warning(Error::truncated_wrong_val("prefix", "retained"));
                    }
                    let expected = eager(&mut before, &lhs, &rhs, divide);
                    let actual = if divide {
                        DecimalDivide::calc(&mut after, &lhs, &rhs)
                    } else {
                        DecimalMod::calc(&mut after, &lhs, &rhs)
                    };
                    assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
                    assert_eq!(after.warnings.warning_cnt, before.warnings.warning_cnt);
                    assert_eq!(after.warnings.warnings, before.warnings.warnings);
                }
            }
        }
    }

    #[test]
    fn test_plus_int() {
        let test_cases = vec![
            (None, false, Some(1), false, None),
            (Some(1), false, None, false, None),
            (Some(17), false, Some(25), false, Some(42)),
            (
                Some(i64::MIN),
                false,
                Some((i64::MAX as u64 + 1) as i64),
                true,
                Some(0),
            ),
        ];
        for (lhs, lhs_is_unsigned, rhs, rhs_is_unsigned, expected) in test_cases {
            let lhs_field_type = FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .flag(if lhs_is_unsigned {
                    FieldTypeFlag::UNSIGNED
                } else {
                    FieldTypeFlag::empty()
                })
                .build();
            let rhs_field_type = FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .flag(if rhs_is_unsigned {
                    FieldTypeFlag::UNSIGNED
                } else {
                    FieldTypeFlag::empty()
                })
                .build();
            let output = RpnFnScalarEvaluator::new()
                .push_param_with_field_type(lhs, lhs_field_type)
                .push_param_with_field_type(rhs, rhs_field_type)
                .evaluate(ScalarFuncSig::PlusInt)
                .unwrap();
            assert_eq!(output, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
        }
    }

    #[test]
    fn test_plus_real() {
        let test_cases = vec![
            (
                Real::new(1.01001).ok(),
                Real::new(-0.01).ok(),
                Real::new(1.00001).ok(),
                false,
            ),
            (Real::new(1e308).ok(), Real::new(1e308).ok(), None, true),
            (
                Real::new(f64::MAX - 1f64).ok(),
                Real::new(2f64).ok(),
                Real::new(f64::MAX).ok(),
                false,
            ),
        ];
        for (lhs, rhs, expected, is_err) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::PlusReal);
            if is_err {
                assert!(output.is_err())
            } else {
                let output = output.unwrap();
                assert_eq!(output, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
            }
        }
    }

    #[test]
    fn test_plus_decimal() {
        let test_cases = vec![("1.1", "2.2", "3.3")];
        for (lhs, rhs, expected) in test_cases {
            let expected: Option<Decimal> = expected.parse().ok();
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs.parse::<Decimal>().ok())
                .push_param(rhs.parse::<Decimal>().ok())
                .evaluate(ScalarFuncSig::PlusDecimal)
                .unwrap();
            assert_eq!(output, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
        }
    }

    #[test]
    fn test_minus_int() {
        let test_cases = vec![
            (None, false, Some(1), false, None, false),
            (Some(1), false, None, false, None, false),
            (Some(12), false, Some(1), false, Some(11), false),
            (
                Some(0),
                true,
                Some(i64::MIN),
                false,
                Some((i64::MAX as u64 + 1) as i64),
                false,
            ),
            (Some(i64::MIN), false, Some(i64::MAX), false, None, true),
            (Some(i64::MAX), false, Some(i64::MIN), false, None, true),
            (Some(-1), false, Some(2), true, None, true),
            (Some(1), true, Some(2), false, None, true),
        ];
        for (lhs, lhs_is_unsigned, rhs, rhs_is_unsigned, expected, is_err) in test_cases {
            let lhs_field_type = FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .flag(if lhs_is_unsigned {
                    FieldTypeFlag::UNSIGNED
                } else {
                    FieldTypeFlag::empty()
                })
                .build();
            let rhs_field_type = FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .flag(if rhs_is_unsigned {
                    FieldTypeFlag::UNSIGNED
                } else {
                    FieldTypeFlag::empty()
                })
                .build();
            let output = RpnFnScalarEvaluator::new()
                .push_param_with_field_type(lhs, lhs_field_type)
                .push_param_with_field_type(rhs, rhs_field_type)
                .evaluate(ScalarFuncSig::MinusInt);
            if is_err {
                assert!(output.is_err())
            } else {
                let output = output.unwrap();
                assert_eq!(output, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
            }
        }
    }

    #[test]
    fn test_minus_real() {
        let test_cases = vec![
            (
                Real::new(1.01001).ok(),
                Real::new(-0.01).ok(),
                Real::new(1.02001).ok(),
                false,
            ),
            (
                Real::new(f64::MIN).ok(),
                Real::new(f64::MAX).ok(),
                None,
                true,
            ),
            (
                Real::new(f64::MIN).ok(),
                Real::new(1f64).ok(),
                Real::new(f64::MIN).ok(),
                false,
            ),
        ];
        for (lhs, rhs, expected, is_err) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::MinusReal);
            if is_err {
                assert!(output.is_err())
            } else {
                let output = output.unwrap();
                assert_eq!(output, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
            }
        }
    }

    #[test]
    fn test_minus_decimal() {
        let test_cases = vec![("1.1", "2.2", "-1.1")];
        for (lhs, rhs, expected) in test_cases {
            let expected: Option<Decimal> = expected.parse().ok();
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs.parse::<Decimal>().ok())
                .push_param(rhs.parse::<Decimal>().ok())
                .evaluate(ScalarFuncSig::MinusDecimal)
                .unwrap();
            assert_eq!(output, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
        }
    }

    #[test]
    fn test_mod_int() {
        let tests = vec![
            (Some(13), Some(11), Some(2)),
            (Some(-13), Some(11), Some(-2)),
            (Some(13), Some(-11), Some(2)),
            (Some(-13), Some(-11), Some(-2)),
            (Some(33), Some(11), Some(0)),
            (Some(33), Some(-11), Some(0)),
            (Some(-33), Some(-11), Some(0)),
            (Some(-11), None, None),
            (None, Some(-11), None),
            (Some(11), Some(0), None),
            (Some(-11), Some(0), None),
            (Some(i64::MAX), Some(i64::MIN), Some(i64::MAX)),
            (Some(i64::MIN), Some(i64::MAX), Some(-1)),
        ];

        for (lhs, rhs, expected) in tests {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::ModInt)
                .unwrap();
            assert_eq!(output, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
        }
    }

    #[test]
    fn test_mod_int_unsigned() {
        let tests = vec![
            (
                Some(u64::MAX as i64),
                true,
                Some(i64::MIN),
                false,
                Some(i64::MAX),
            ),
            (
                Some(i64::MIN),
                false,
                Some(u64::MAX as i64),
                true,
                Some(i64::MIN),
            ),
        ];

        for (lhs, lhs_is_unsigned, rhs, rhs_is_unsigned, expected) in tests {
            let lhs_field_type = FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .flag(if lhs_is_unsigned {
                    FieldTypeFlag::UNSIGNED
                } else {
                    FieldTypeFlag::empty()
                })
                .build();
            let rhs_field_type = FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .flag(if rhs_is_unsigned {
                    FieldTypeFlag::UNSIGNED
                } else {
                    FieldTypeFlag::empty()
                })
                .build();
            let output = RpnFnScalarEvaluator::new()
                .push_param_with_field_type(lhs, lhs_field_type)
                .push_param_with_field_type(rhs, rhs_field_type)
                .evaluate(ScalarFuncSig::ModInt)
                .unwrap();
            assert_eq!(output, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
        }
    }

    #[test]
    fn test_mod_real() {
        let tests = vec![
            (Real::new(1.0).ok(), None, None),
            (None, Real::new(1.0).ok(), None),
            (
                Real::new(1.0).ok(),
                Real::new(1.1).ok(),
                Real::new(1.0).ok(),
            ),
            (
                Real::new(-1.0).ok(),
                Real::new(1.1).ok(),
                Real::new(-1.0).ok(),
            ),
            (
                Real::new(1.0).ok(),
                Real::new(-1.1).ok(),
                Real::new(1.0).ok(),
            ),
            (
                Real::new(-1.0).ok(),
                Real::new(-1.1).ok(),
                Real::new(-1.0).ok(),
            ),
            (Real::new(1.0).ok(), Real::new(0.0).ok(), None),
        ];

        for (lhs, rhs, expected) in tests {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::ModReal)
                .unwrap();
            assert_eq!(output, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
        }
    }

    #[test]
    fn test_mod_decimal() {
        let tests = vec![
            ("13", "11", "2"),
            ("-13", "11", "-2"),
            ("13", "-11", "2"),
            ("-13", "-11", "-2"),
            ("33", "11", "0"),
            ("-33", "11", "0"),
            ("33", "-11", "0"),
            ("-33", "-11", "0"),
            ("0.0000000001", "1.0", "0.0000000001"),
            ("1", "1.1", "1"),
            ("-1", "1.1", "-1"),
            ("1", "-1.1", "1"),
            ("-1", "-1.1", "-1"),
            ("3", "0", ""),
            ("-3", "0", ""),
            ("0", "0", ""),
            ("-3", "", ""),
            ("", ("-3"), ""),
            ("", "", ""),
        ];

        for (lhs, rhs, expected) in tests {
            let expected = expected.parse::<Decimal>().ok();
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs.parse::<Decimal>().ok())
                .push_param(rhs.parse::<Decimal>().ok())
                .evaluate(ScalarFuncSig::ModDecimal)
                .unwrap();
            assert_eq!(output, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
        }
    }

    #[test]
    fn test_multiply_decimal() {
        let test_cases = vec![
            ("1.1", "2.2", "2.42"),
            (
                "999999999999999999999999999999999.9999",
                "766507373740683764182618847769240.9770",
                "766507373740683764182618847769239999923349262625931623581738115223.07600000",
            ),
        ];
        for (lhs, rhs, expected) in test_cases {
            let expected: Option<Decimal> = expected.parse().ok();
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs.parse::<Decimal>().ok())
                .push_param(rhs.parse::<Decimal>().ok())
                .evaluate(ScalarFuncSig::MultiplyDecimal)
                .unwrap();
            assert_eq!(output, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
        }
    }

    #[test]
    fn test_int_divide_int() {
        let test_cases = vec![
            (13, false, 11, false, Some(1)),
            (13, false, -11, false, Some(-1)),
            (-13, false, 11, false, Some(-1)),
            (-13, false, -11, false, Some(1)),
            (33, false, 11, false, Some(3)),
            (33, false, -11, false, Some(-3)),
            (-33, false, 11, false, Some(-3)),
            (-33, false, -11, false, Some(3)),
            (11, false, 0, false, None),
            (-11, false, 0, false, None),
            (-3, false, 5, true, Some(0)),
            (3, false, -5, false, Some(0)),
            (i64::MIN + 1, false, -1, false, Some(i64::MAX)),
            (i64::MIN, false, 1, false, Some(i64::MIN)),
            (i64::MAX, false, 1, false, Some(i64::MAX)),
            (u64::MAX as i64, true, 1, false, Some(u64::MAX as i64)),
        ];

        for (lhs, lhs_is_unsigned, rhs, rhs_is_unsigned, expected) in test_cases {
            let lhs_field_type = FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .flag(if lhs_is_unsigned {
                    FieldTypeFlag::UNSIGNED
                } else {
                    FieldTypeFlag::empty()
                })
                .build();
            let rhs_field_type = FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .flag(if rhs_is_unsigned {
                    FieldTypeFlag::UNSIGNED
                } else {
                    FieldTypeFlag::empty()
                })
                .build();

            let output = RpnFnScalarEvaluator::new()
                .push_param_with_field_type(lhs, lhs_field_type)
                .push_param_with_field_type(rhs, rhs_field_type)
                .evaluate(ScalarFuncSig::IntDivideInt)
                .unwrap();

            assert_eq!(output, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
        }
    }

    #[test]
    fn test_int_divide_int_overflow() {
        let test_cases = vec![
            (i64::MIN, false, -1, false),
            (-1, false, 1, true),
            (-2, false, 1, true),
            (1, true, -1, false),
            (2, true, -1, false),
        ];
        for (lhs, lhs_is_unsigned, rhs, rhs_is_unsigned) in test_cases {
            let lhs_field_type = FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .flag(if lhs_is_unsigned {
                    FieldTypeFlag::UNSIGNED
                } else {
                    FieldTypeFlag::empty()
                })
                .build();
            let rhs_field_type = FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .flag(if rhs_is_unsigned {
                    FieldTypeFlag::UNSIGNED
                } else {
                    FieldTypeFlag::empty()
                })
                .build();

            let output: Result<Option<Int>> = RpnFnScalarEvaluator::new()
                .push_param_with_field_type(lhs, lhs_field_type)
                .push_param_with_field_type(rhs, rhs_field_type)
                .evaluate(ScalarFuncSig::IntDivideInt);
            assert!(output.is_err(), "lhs={:?}, rhs={:?}", lhs, rhs);
        }
    }

    #[test]
    fn test_int_divide_decimal() {
        let test_cases = vec![
            (Some("11.01"), Some("1.1"), Some(10)),
            (Some("-11.01"), Some("1.1"), Some(-10)),
            (Some("11.01"), Some("-1.1"), Some(-10)),
            (Some("-11.01"), Some("-1.1"), Some(10)),
            (Some("123.0"), None, None),
            (None, Some("123.0"), None),
            // divide by zero
            (Some("0.0"), Some("0.0"), None),
            (None, None, None),
            (Some("0"), Some("45584"), Some(0)),
        ];

        for (lhs, rhs, expected) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(lhs.map(|f| Decimal::from_bytes(f.as_bytes()).unwrap().unwrap()))
                .push_param(rhs.map(|f| Decimal::from_bytes(f.as_bytes()).unwrap().unwrap()))
                .evaluate(ScalarFuncSig::IntDivideDecimal)
                .unwrap();

            assert_eq!(output, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
        }
    }

    #[test]
    fn test_int_divide_decimal_overflow() {
        let test_cases = vec![
            (Decimal::from(i64::MIN), Decimal::from(-1)),
            (
                Decimal::from(i64::MAX),
                Decimal::from_bytes(b"0.1").unwrap().unwrap(),
            ),
        ];

        for (lhs, rhs) in test_cases {
            let output: Result<Option<Int>> = RpnFnScalarEvaluator::new()
                .push_param(lhs.clone())
                .push_param(rhs.clone())
                .evaluate(ScalarFuncSig::IntDivideDecimal);

            assert!(output.is_err(), "lhs={:?}, rhs={:?}", lhs, rhs);
        }
    }

    #[test]
    fn test_int_divide_decimal_unsigned_overflow() {
        let lft = FieldTypeBuilder::new()
            .tp(FieldTypeTp::NewDecimal)
            .flag(FieldTypeFlag::UNSIGNED)
            .build();
        let rft = FieldTypeBuilder::new()
            .tp(FieldTypeTp::NewDecimal)
            .flag(FieldTypeFlag::UNSIGNED)
            .build();
        let output: Option<Int> = RpnFnScalarEvaluator::new()
            .push_param_with_field_type(Decimal::from(1), lft)
            .push_param_with_field_type(Decimal::from_f64(-2_f64).unwrap(), rft)
            .evaluate(ScalarFuncSig::IntDivideDecimal)
            .unwrap();
        assert_eq!(output, Some(0));

        let lft = FieldTypeBuilder::new()
            .tp(FieldTypeTp::NewDecimal)
            .flag(FieldTypeFlag::UNSIGNED)
            .build();
        let rft = FieldTypeBuilder::new()
            .tp(FieldTypeTp::NewDecimal)
            .flag(FieldTypeFlag::UNSIGNED)
            .build();
        let output: Result<Option<Int>> = RpnFnScalarEvaluator::new()
            .push_param_with_field_type(Decimal::from(1), lft)
            .push_param_with_field_type(Decimal::from_f64(-1_f64).unwrap(), rft)
            .evaluate(ScalarFuncSig::IntDivideDecimal);
        assert!(output.is_err(), "should be error");
    }

    #[test]
    fn test_real_multiply() {
        let should_pass = vec![(1.01001, -0.01, Real::new(-0.0101001).ok())];

        for (lhs, rhs, expected) in should_pass {
            assert_eq!(
                expected,
                RpnFnScalarEvaluator::new()
                    .push_param(lhs)
                    .push_param(rhs)
                    .evaluate(ScalarFuncSig::MultiplyReal)
                    .unwrap()
            );
        }

        let should_fail = vec![(f64::MAX, f64::MAX), (f64::MAX, f64::MIN)];

        for (lhs, rhs) in should_fail {
            assert!(
                RpnFnScalarEvaluator::new()
                    .push_param(lhs)
                    .push_param(rhs)
                    .evaluate::<Real>(ScalarFuncSig::MultiplyReal)
                    .is_err(),
                "{} * {} should fail",
                lhs,
                rhs
            );
        }
    }

    #[test]
    fn test_int_multiply() {
        let should_pass = vec![
            (11, 17, Some(187)),
            (-1, -3, Some(3)),
            (1, i64::MIN, Some(i64::MIN)),
        ];
        for (lhs, rhs, expected) in should_pass {
            assert_eq!(
                expected,
                RpnFnScalarEvaluator::new()
                    .push_param_with_field_type(lhs, FieldTypeTp::LongLong)
                    .push_param_with_field_type(rhs, FieldTypeTp::LongLong)
                    .evaluate(ScalarFuncSig::MultiplyInt)
                    .unwrap()
            );
        }

        let should_fail = vec![(i64::MAX, 2), (i64::MIN, -1)];
        for (lhs, rhs) in should_fail {
            assert!(
                RpnFnScalarEvaluator::new()
                    .push_param_with_field_type(lhs, FieldTypeTp::LongLong)
                    .push_param_with_field_type(rhs, FieldTypeTp::LongLong)
                    .evaluate::<Int>(ScalarFuncSig::MultiplyInt)
                    .is_err(),
                "{} * {} should fail",
                lhs,
                rhs
            );
        }
    }

    #[test]
    fn test_int_uint_multiply() {
        let should_pass = vec![(i64::MAX, 1, Some(i64::MAX)), (3, 7, Some(21))];

        for (lhs, rhs, expected) in should_pass {
            assert_eq!(
                expected,
                RpnFnScalarEvaluator::new()
                    .push_param_with_field_type(lhs, FieldTypeTp::LongLong)
                    .push_param_with_field_type(
                        rhs,
                        FieldTypeBuilder::new()
                            .tp(FieldTypeTp::LongLong)
                            .flag(FieldTypeFlag::UNSIGNED)
                    )
                    .evaluate(ScalarFuncSig::MultiplyInt)
                    .unwrap()
            );
        }

        let should_fail = vec![(-2, 1), (i64::MIN, 2)];
        for (lhs, rhs) in should_fail {
            assert!(
                RpnFnScalarEvaluator::new()
                    .push_param_with_field_type(lhs, FieldTypeTp::LongLong)
                    .push_param_with_field_type(
                        rhs,
                        FieldTypeBuilder::new()
                            .tp(FieldTypeTp::LongLong)
                            .flag(FieldTypeFlag::UNSIGNED)
                    )
                    .evaluate::<Int>(ScalarFuncSig::MultiplyInt)
                    .is_err(),
                "{} * {} should fail",
                lhs,
                rhs
            );
        }
    }

    #[test]
    fn test_uint_uint_multiply() {
        let should_pass = vec![
            (7, 11, Some(77)),
            (1, 2, Some(2)),
            (u64::MAX as i64, 1, Some(u64::MAX as i64)),
        ];

        for (lhs, rhs, expected) in should_pass {
            assert_eq!(
                expected,
                RpnFnScalarEvaluator::new()
                    .push_param_with_field_type(
                        lhs,
                        FieldTypeBuilder::new()
                            .tp(FieldTypeTp::LongLong)
                            .flag(FieldTypeFlag::UNSIGNED)
                    )
                    .push_param_with_field_type(
                        rhs,
                        FieldTypeBuilder::new()
                            .tp(FieldTypeTp::LongLong)
                            .flag(FieldTypeFlag::UNSIGNED)
                    )
                    .evaluate(ScalarFuncSig::MultiplyIntUnsigned)
                    .unwrap()
            );
        }

        let should_fail = vec![(u64::MAX as i64, 2)];
        for (lhs, rhs) in should_fail {
            assert!(
                RpnFnScalarEvaluator::new()
                    .push_param_with_field_type(
                        lhs,
                        FieldTypeBuilder::new()
                            .tp(FieldTypeTp::LongLong)
                            .flag(FieldTypeFlag::UNSIGNED)
                    )
                    .push_param_with_field_type(
                        rhs,
                        FieldTypeBuilder::new()
                            .tp(FieldTypeTp::LongLong)
                            .flag(FieldTypeFlag::UNSIGNED)
                    )
                    .evaluate::<Int>(ScalarFuncSig::MultiplyIntUnsigned)
                    .is_err(),
                "{} * {} should fail",
                lhs,
                rhs
            );
        }
    }

    #[test]
    fn test_decimal_divide() {
        let cases = vec![
            (Some("2.2"), Some("1.1"), Some("2.0")),
            (Some("2.33"), Some("-0.01"), Some("-233")),
            (Some("2.33"), Some("0.01"), Some("233")),
            (None, Some("2"), None),
            (Some("123"), None, None),
        ];

        for (lhs, rhs, expected) in cases {
            let actual = RpnFnScalarEvaluator::new()
                .push_param(lhs.map(|s| Decimal::from_str(s).unwrap()))
                .push_param(rhs.map(|s| Decimal::from_str(s).unwrap()))
                .evaluate(ScalarFuncSig::DivideDecimal)
                .unwrap();

            let expected = expected.map(|s| Decimal::from_str(s).unwrap());

            assert_eq!(actual, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
        }

        let cases2 = vec![
            (Some("2.2"), Some("1.3"), Some("1.692"), 2),
            (Some("2.2"), Some("1.3"), Some("1.6923"), 3),
            (Some("2.2"), Some("1.3"), Some("1.69231"), 4),
            (None, Some("2"), None, 4),
            (Some("123"), None, None, 4),
        ];
        for (lhs, rhs, expected, frac_incr) in cases2 {
            let mut cfg = EvalConfig::new();
            cfg.set_div_precision_incr(frac_incr);
            let ctx = EvalContext::new(cfg.into());
            let actual: Option<Decimal> = RpnFnScalarEvaluator::new_for_test(ctx)
                .push_param(lhs.map(|s| Decimal::from_str(s).unwrap()))
                .push_param(rhs.map(|s| Decimal::from_str(s).unwrap()))
                .evaluate(ScalarFuncSig::DivideDecimal)
                .unwrap();

            let expected = expected.map(|s| Decimal::from_str(s).unwrap());
            if let (Some(lhs_), Some(rhs_)) = (expected, actual) {
                assert_eq!(format!("{lhs_}"), format!("{rhs_}"));
            }
        }
    }

    #[test]
    fn test_real_divide() {
        let normal = vec![
            (Some(2.2), Some(1.1), Real::new(2.0).ok()),
            (Some(2.33), Some(-0.01), Real::new(-233.0).ok()),
            (Some(2.33), Some(0.01), Real::new(233.0).ok()),
            (None, Some(2.0), None),
            (Some(123.0), None, None),
        ];

        for (lhs, rhs, expected) in normal {
            let actual = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::DivideReal)
                .unwrap();

            assert_eq!(actual, expected, "lhs={:?}, rhs={:?}", lhs, rhs);
        }

        let overflow = vec![(f64::MAX, 0.0001)];
        for (lhs, rhs) in overflow {
            RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate::<Real>(ScalarFuncSig::DivideReal)
                .unwrap_err();
        }
    }

    #[test]
    fn test_divide_by_zero() {
        let cases: Vec<(ScalarFuncSig, FieldTypeTp, ScalarValue, ScalarValue)> = vec![
            (
                ScalarFuncSig::DivideDecimal,
                FieldTypeTp::NewDecimal,
                Decimal::from_str("2.33").unwrap().into(),
                Decimal::from_str("0.0").unwrap().into(),
            ),
            (
                ScalarFuncSig::DivideDecimal,
                FieldTypeTp::NewDecimal,
                Decimal::from_str("2.33").unwrap().into(),
                Decimal::from_str("-0.0").unwrap().into(),
            ),
            (
                ScalarFuncSig::DivideReal,
                FieldTypeTp::Double,
                2.33.into(),
                0.0.into(),
            ),
        ];

        // Vec<[(Flag, SqlMode, is_ok(bool), has_warning(bool))]>
        let modes = vec![
            // Warning
            (Flag::empty(), SqlMode::empty(), true, true),
            // Error
            (
                Flag::IN_UPDATE_OR_DELETE_STMT,
                SqlMode::ERROR_FOR_DIVISION_BY_ZERO | SqlMode::STRICT_ALL_TABLES,
                false,
                false,
            ),
            // Ok
            (
                Flag::IN_UPDATE_OR_DELETE_STMT,
                SqlMode::STRICT_ALL_TABLES,
                true,
                false,
            ),
            // Warning
            (
                Flag::IN_UPDATE_OR_DELETE_STMT | Flag::DIVIDED_BY_ZERO_AS_WARNING,
                SqlMode::ERROR_FOR_DIVISION_BY_ZERO | SqlMode::STRICT_ALL_TABLES,
                true,
                true,
            ),
        ];

        for (sig, ret_field_type, lhs, rhs) in &cases {
            for &(flag, sql_mode, is_ok, has_warning) in &modes {
                // Construct an `EvalContext`
                let mut config = EvalConfig::new();
                config.set_flag(flag).set_sql_mode(sql_mode);

                let (result, mut ctx) = RpnFnScalarEvaluator::new()
                    .context(EvalContext::new(std::sync::Arc::new(config)))
                    .push_param(lhs.to_owned())
                    .push_param(rhs.to_owned())
                    .evaluate_raw(*ret_field_type, *sig);

                if is_ok {
                    assert!(result.unwrap().is_none());
                } else {
                    result.unwrap_err();
                }

                if has_warning {
                    assert_eq!(
                        ctx.take_warnings().warnings[0].get_code(),
                        ERR_DIVISION_BY_ZERO
                    );
                } else {
                    assert!(ctx.take_warnings().warnings.is_empty());
                }
            }
        }
    }
}
