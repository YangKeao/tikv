// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    cell::RefCell,
    convert::TryFrom,
    num::{IntErrorKind, ParseIntError},
};

use num::traits::Pow;
use tidb_query_codegen::rpn_fn;
use tidb_query_common::{Result, error::EvaluateError};
use tidb_query_datatype::{
    codec::{
        self, Error,
        convert::ConvertTo,
        data_type::*,
        mysql::{
            DEFAULT_FSP, RoundMode,
            decimal::{NativeDecimalError, NativeDecimalOp},
        },
    },
    expr::EvalContext,
};
use tikv_util::time::get_time;

mod native_go_exp_log;
pub(crate) mod native_go_trig;

const MAX_RAND_VALUE: u32 = 0x3FFFFFFF;

#[rpn_fn]
#[inline]
pub fn pi() -> Result<Option<Real>> {
    Ok(Some(Real::new(std::f64::consts::PI).unwrap()))
}

#[rpn_fn]
#[inline]
fn pi_raw() -> Result<Option<Bytes>> {
    Ok(pi()?.map(|value| encode_raw_f64(value.into_inner())))
}

#[rpn_fn]
#[inline]
pub fn crc32(arg: BytesRef) -> Result<Option<Int>> {
    Ok(Some(i64::from(file_system::calc_crc32_bytes(arg))))
}

#[inline]
fn ln_f64(arg: f64) -> f64 {
    arg.ln()
}

#[inline]
#[rpn_fn]
pub fn log_1_arg(arg: &Real) -> Result<Option<Real>> {
    Ok(f64_to_real(ln_f64(**arg)))
}

#[inline]
#[rpn_fn(nullable)]
fn ln_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(arg)?.map(ln_f64).map(encode_raw_f64))
}

#[inline]
fn log_f64(base: f64, value: f64) -> f64 {
    value.log(base)
}

#[inline]
#[rpn_fn]
#[allow(clippy::float_cmp)]
pub fn log_2_arg(arg0: &Real, arg1: &Real) -> Result<Option<Real>> {
    Ok({
        if **arg0 <= 0f64 || **arg0 == 1f64 || **arg1 <= 0f64 {
            None
        } else {
            f64_to_real(log_f64(**arg0, **arg1))
        }
    })
}

#[inline]
#[rpn_fn(nullable)]
fn log_native(base: Option<BytesRef>, value: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(base)?
        .zip(decode_raw_f64(value)?)
        .map(|(base, value)| log_f64(base, value))
        .map(encode_raw_f64))
}

#[inline]
fn log2_f64(arg: f64) -> f64 {
    arg.log2()
}

#[inline]
#[rpn_fn]
pub fn log2(arg: &Real) -> Result<Option<Real>> {
    Ok(f64_to_real(log2_f64(**arg)))
}

#[inline]
#[rpn_fn(nullable)]
fn log2_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(arg)?.map(log2_f64).map(encode_raw_f64))
}

#[inline]
#[rpn_fn]
pub fn log10(arg: &Real) -> Result<Option<Real>> {
    Ok(f64_to_real(arg.log10()))
}

// If the given f64 is finite, returns `Some(Real)`. Otherwise returns None.
fn f64_to_real(n: f64) -> Option<Real> {
    if n.is_finite() {
        Some(Real::new(n).unwrap())
    } else {
        None
    }
}

#[inline]
fn abs_int_value(value: Int) -> Option<Int> {
    value.checked_abs()
}

#[inline]
fn abs_f64(value: f64) -> f64 {
    value.abs()
}

#[inline]
fn ceil_floor_f64(value: f64, ceiling: bool) -> f64 {
    if ceiling { value.ceil() } else { value.floor() }
}

#[inline]
#[rpn_fn(capture = [ctx])]
pub fn ceil<C: Ceil>(ctx: &mut EvalContext, arg: &C::Input) -> Result<Option<C::Output>> {
    C::ceil(ctx, arg)
}

pub trait Ceil {
    type Input: Evaluable + EvaluableRet;
    type Output: EvaluableRet;

    fn ceil(_ctx: &mut EvalContext, arg: &Self::Input) -> Result<Option<Self::Output>>;
}

pub struct CeilReal;

impl Ceil for CeilReal {
    type Input = Real;
    type Output = Real;

    #[inline]
    fn ceil(_ctx: &mut EvalContext, arg: &Self::Input) -> Result<Option<Self::Output>> {
        Ok(Some(Real::new(ceil_floor_f64(**arg, true)).unwrap()))
    }
}

pub struct CeilDecToDec;

impl Ceil for CeilDecToDec {
    type Input = Decimal;
    type Output = Decimal;

    #[inline]
    fn ceil(ctx: &mut EvalContext, arg: &Self::Input) -> Result<Option<Self::Output>> {
        Ok(arg.ceil().into_result(ctx).map(Some)?)
    }
}

pub struct CeilIntToDec;

impl Ceil for CeilIntToDec {
    type Input = Int;
    type Output = Decimal;

    #[inline]
    fn ceil(_ctx: &mut EvalContext, arg: &Self::Input) -> Result<Option<Self::Output>> {
        Ok(Some(Decimal::from(*arg)))
    }
}

pub struct CeilDecToInt;

impl Ceil for CeilDecToInt {
    type Input = Decimal;
    type Output = Int;

    #[inline]
    fn ceil(ctx: &mut EvalContext, arg: &Self::Input) -> Result<Option<Self::Output>> {
        Ok(arg
            .ceil()
            .into_result(ctx)
            .and_then(|decimal| decimal.as_i64_with_ctx(ctx))
            .map(Some)?)
    }
}

pub struct CeilIntToInt;

impl Ceil for CeilIntToInt {
    type Input = Int;
    type Output = Int;

    #[inline]
    fn ceil(_ctx: &mut EvalContext, arg: &Self::Input) -> Result<Option<Self::Output>> {
        Ok(Some(*arg))
    }
}

#[rpn_fn(capture = [ctx])]
pub fn floor<T: Floor>(ctx: &mut EvalContext, arg: &T::Input) -> Result<Option<T::Output>> {
    T::floor(ctx, arg)
}

pub trait Floor {
    type Input: Evaluable + EvaluableRet;
    type Output: EvaluableRet;
    fn floor(_ctx: &mut EvalContext, arg: &Self::Input) -> Result<Option<Self::Output>>;
}

pub struct FloorReal;

impl Floor for FloorReal {
    type Input = Real;
    type Output = Real;

    #[inline]
    fn floor(_ctx: &mut EvalContext, arg: &Self::Input) -> Result<Option<Self::Output>> {
        Ok(Some(Real::new(ceil_floor_f64(**arg, false)).unwrap()))
    }
}

pub struct FloorIntToDec;

impl Floor for FloorIntToDec {
    type Input = Int;
    type Output = Decimal;

    fn floor(_ctx: &mut EvalContext, arg: &Self::Input) -> Result<Option<Self::Output>> {
        Ok(Some(Decimal::from(*arg)))
    }
}

pub struct FloorDecToInt;

impl Floor for FloorDecToInt {
    type Input = Decimal;
    type Output = Int;

    #[inline]
    fn floor(ctx: &mut EvalContext, arg: &Self::Input) -> Result<Option<Self::Output>> {
        Ok(arg
            .floor()
            .into_result(ctx)
            .and_then(|decimal| decimal.as_i64_with_ctx(ctx))
            .map(Some)?)
    }
}

pub struct FloorDecToDec;

impl Floor for FloorDecToDec {
    type Input = Decimal;
    type Output = Decimal;

    #[inline]
    fn floor(ctx: &mut EvalContext, arg: &Self::Input) -> Result<Option<Self::Output>> {
        Ok(arg.floor().into_result(ctx).map(Some)?)
    }
}

pub struct FloorIntToInt;

impl Floor for FloorIntToInt {
    type Input = Int;
    type Output = Int;

    #[inline]
    fn floor(_ctx: &mut EvalContext, arg: &Self::Input) -> Result<Option<Self::Output>> {
        Ok(Some(*arg))
    }
}

#[rpn_fn]
#[inline]
fn abs_int(arg: &Int) -> Result<Option<Int>> {
    match abs_int_value(*arg) {
        None => Err(Error::overflow("BIGINT", format!("abs({})", *arg)).into()),
        Some(arg_abs) => Ok(Some(arg_abs)),
    }
}

#[rpn_fn]
#[inline]
fn abs_uint(arg: &Int) -> Result<Option<Int>> {
    Ok(Some(arg.to_owned()))
}

#[rpn_fn]
#[inline]
fn abs_real(arg: &Real) -> Result<Option<Real>> {
    Ok(Some(Real::new(abs_f64(**arg)).unwrap()))
}

#[rpn_fn]
#[inline]
fn abs_decimal(arg: &Decimal) -> Result<Option<Decimal>> {
    let res: codec::Result<Decimal> = arg.to_owned().abs().into();
    Ok(Some(res?))
}

// Factory-only transport: these bytes are IEEE-754 bits, not SQL strings.
#[inline]
fn decode_raw_f64(arg: Option<BytesRef>) -> Result<Option<f64>> {
    let bytes = match arg {
        Some(bytes) => bytes,
        None => return Ok(None),
    };
    let bits = <[u8; 8]>::try_from(bytes).map_err(|_| {
        other_err!(
            "Internal raw f64 transport requires exactly 8 bytes, received {}",
            bytes.len()
        )
    })?;
    Ok(Some(f64::from_bits(u64::from_le_bytes(bits))))
}

#[inline]
fn encode_raw_f64(value: f64) -> Bytes {
    value.to_bits().to_le_bytes().to_vec()
}

#[inline]
fn sign_f64(arg: f64) -> i64 {
    if arg > 0f64 {
        1
    } else if arg < 0f64 {
        -1
    } else {
        0
    }
}

#[inline]
#[rpn_fn]
fn sign(arg: &Real) -> Result<Option<Int>> {
    Ok(Some(sign_f64(**arg)))
}

#[inline]
#[rpn_fn(nullable)]
fn sign_raw(arg: Option<BytesRef>) -> Result<Option<Int>> {
    Ok(decode_raw_f64(arg)?.map(sign_f64))
}

#[inline]
fn sqrt_f64(arg: f64) -> Option<f64> {
    if arg < 0f64 { None } else { Some(arg.sqrt()) }
}

#[inline]
#[rpn_fn]
fn sqrt(arg: &Real) -> Result<Option<Real>> {
    Ok(sqrt_f64(**arg).and_then(|value| Real::new(value).ok()))
}

#[inline]
#[rpn_fn(nullable)]
fn sqrt_raw(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(arg)?.and_then(sqrt_f64).map(encode_raw_f64))
}

#[inline]
fn radians_f64(arg: f64) -> f64 {
    arg * (std::f64::consts::PI / 180_f64)
}

#[inline]
#[rpn_fn]
fn radians(arg: &Real) -> Result<Option<Real>> {
    Ok(Real::new(radians_f64(**arg)).ok())
}

#[inline]
#[rpn_fn(nullable)]
fn radians_raw(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(arg)?.map(radians_f64).map(encode_raw_f64))
}

// Private native paths encode computed IEEE bits; frontend domain/diagnostic
// policies and the existing wire math remain independent.
#[rpn_fn(nullable)]
fn exp_go_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(arg)?
        .map(native_go_exp_log::go_exp)
        .map(encode_raw_f64))
}

#[rpn_fn(nullable)]
fn log10_go_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(arg)?
        .map(native_go_exp_log::go_log10)
        .map(encode_raw_f64))
}

#[inline]
#[rpn_fn]
pub fn exp(arg: &Real) -> Result<Option<Real>> {
    let ret = arg.exp();
    if ret.is_infinite() {
        Err(Error::overflow("DOUBLE", format!("exp({})", arg)).into())
    } else {
        Ok(Real::new(ret).ok())
    }
}

// Wire and legacy share libm arithmetic, not native Go's approximations. Their
// result policies remain separate: wire uses Real/overflow, legacy keeps raw
// computed IEEE values, including NaN and infinity.
#[inline]
fn sin_libm(value: f64) -> f64 {
    value.sin()
}

#[inline]
fn cos_libm(value: f64) -> f64 {
    value.cos()
}

#[inline]
fn tan_libm(value: f64) -> f64 {
    value.tan()
}

#[inline]
fn cot_libm(value: f64) -> f64 {
    tan_libm(value).recip()
}

#[inline]
fn atan_libm(value: f64) -> f64 {
    value.atan()
}

#[inline]
fn atan2_libm(y: f64, x: f64) -> f64 {
    y.atan2(x)
}

fn trig_raw_unary(arg: Option<BytesRef>, operation: fn(f64) -> f64) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(arg)?.map(operation).map(encode_raw_f64))
}

fn trig_raw_binary(
    y: Option<BytesRef>,
    x: Option<BytesRef>,
    operation: fn(f64, f64) -> f64,
) -> Result<Option<Bytes>> {
    let y = decode_raw_f64(y)?;
    let x = decode_raw_f64(x)?;
    Ok(y.zip(x).map(|(y, x)| operation(y, x)).map(encode_raw_f64))
}

// These private Go entries return computed raw bits, not a Real projection.
// Native finite_float and its existing GoError/AST rendering run afterward.
#[rpn_fn(nullable)]
fn sin_go_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    trig_raw_unary(arg, native_go_trig::go_sin)
}

#[rpn_fn(nullable)]
fn cos_go_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    trig_raw_unary(arg, native_go_trig::go_cos)
}

#[rpn_fn(nullable)]
fn tan_go_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    trig_raw_unary(arg, native_go_trig::go_tan)
}

#[rpn_fn(nullable)]
fn cot_go_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    trig_raw_unary(arg, |value| 1.0 / native_go_trig::go_tan(value))
}

#[rpn_fn(nullable)]
fn atan_go_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    trig_raw_unary(arg, native_go_trig::go_atan)
}

#[rpn_fn(nullable)]
fn atan2_go_native(y: Option<BytesRef>, x: Option<BytesRef>) -> Result<Option<Bytes>> {
    trig_raw_binary(y, x, native_go_trig::go_atan2)
}

#[rpn_fn(nullable)]
fn sin_libm_legacy(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    trig_raw_unary(arg, sin_libm)
}

#[rpn_fn(nullable)]
fn cos_libm_legacy(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    trig_raw_unary(arg, cos_libm)
}

#[rpn_fn(nullable)]
fn cot_libm_legacy(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    trig_raw_unary(arg, cot_libm)
}

#[rpn_fn(nullable)]
fn atan_libm_legacy(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    trig_raw_unary(arg, atan_libm)
}

#[rpn_fn(nullable)]
fn atan2_libm_legacy(y: Option<BytesRef>, x: Option<BytesRef>) -> Result<Option<Bytes>> {
    trig_raw_binary(y, x, atan2_libm)
}

#[inline]
#[rpn_fn]
fn sin(arg: &Real) -> Result<Option<Real>> {
    Ok(Real::new(sin_libm(**arg)).ok())
}

#[inline]
#[rpn_fn]
fn cos(arg: &Real) -> Result<Option<Real>> {
    Ok(Real::new(cos_libm(**arg)).ok())
}

#[inline]
#[rpn_fn]
fn tan(arg: &Real) -> Result<Option<Real>> {
    Ok(Real::new(tan_libm(**arg)).ok())
}

#[inline]
#[rpn_fn]
fn cot(arg: &Real) -> Result<Option<Real>> {
    let cot = cot_libm(**arg);
    if cot.is_infinite() {
        Err(Error::overflow("DOUBLE", format!("cot({})", arg)).into())
    } else {
        Ok(Real::new(cot).ok())
    }
}

#[inline]
fn pow_f64(base: f64, exponent: f64) -> f64 {
    base.pow(exponent)
}

#[inline]
#[rpn_fn]
fn pow(lhs: &Real, rhs: &Real) -> Result<Option<Real>> {
    let pow = pow_f64(lhs.into_inner(), rhs.into_inner());
    if pow.is_infinite() {
        Err(Error::overflow("DOUBLE", format!("pow({}, {})", lhs, rhs)).into())
    } else {
        Ok(Real::new(pow).ok())
    }
}

#[inline]
#[rpn_fn(nullable)]
fn pow_native(base: Option<BytesRef>, exponent: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(base)?
        .zip(decode_raw_f64(exponent)?)
        .map(|(base, exponent)| pow_f64(base, exponent))
        .map(encode_raw_f64))
}

#[inline]
#[rpn_fn]
fn rand() -> Result<Option<Real>> {
    let res = MYSQL_RNG.with(|mysql_rng| mysql_rng.borrow_mut().gen());
    Ok(Real::new(res).ok())
}

#[inline]
#[rpn_fn(nullable)]
fn rand_with_seed_first_gen(seed: Option<&i64>) -> Result<Option<Real>> {
    let mut rng = MySqlRng::new_with_seed(seed.cloned().unwrap_or(0));
    let res = rng.gen();
    Ok(Real::new(res).ok())
}

#[inline]
fn degrees_f64(arg: f64) -> f64 {
    arg.to_degrees()
}

#[inline]
#[rpn_fn]
fn degrees(arg: &Real) -> Result<Option<Real>> {
    let ret = degrees_f64(**arg);
    if ret.is_infinite() {
        Err(Error::overflow("DOUBLE", format!("degrees({})", arg)).into())
    } else {
        Ok(Real::new(ret).ok())
    }
}

#[inline]
#[rpn_fn(nullable)]
fn degrees_raw(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(arg)?.map(degrees_f64).map(encode_raw_f64))
}

#[inline]
fn asin_f64(arg: f64) -> f64 {
    arg.asin()
}

#[inline]
#[rpn_fn]
pub fn asin(arg: &Real) -> Result<Option<Real>> {
    Ok(Real::new(asin_f64(**arg)).ok())
}

#[inline]
#[rpn_fn(nullable)]
fn asin_raw(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(arg)?.map(asin_f64).map(encode_raw_f64))
}

#[inline]
fn acos_f64(arg: f64) -> f64 {
    arg.acos()
}

#[inline]
#[rpn_fn]
pub fn acos(arg: &Real) -> Result<Option<Real>> {
    Ok(Real::new(acos_f64(**arg)).ok())
}

#[inline]
#[rpn_fn(nullable)]
fn acos_raw(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(arg)?.map(acos_f64).map(encode_raw_f64))
}

#[inline]
#[rpn_fn]
pub fn atan_1_arg(arg: &Real) -> Result<Option<Real>> {
    Ok(Real::new(atan_libm(**arg)).ok())
}

#[inline]
#[rpn_fn]
pub fn atan_2_args(arg0: &Real, arg1: &Real) -> Result<Option<Real>> {
    Ok(Real::new(atan2_libm(**arg0, **arg1)).ok())
}

#[derive(Clone, Copy)]
enum ConvMode {
    Wire,
    Native,
    Legacy,
}

#[derive(Clone, Copy)]
enum ConvSignPolicy {
    WireOriginalSign,
    NativeWrappedSign,
}

fn conv_bases(from: Int, to: Int, mode: ConvMode) -> Option<(IntWithSign, IntWithSign)> {
    let (from, to) = match mode {
        ConvMode::Wire => (IntWithSign::from_int(from), IntWithSign::from_int(to)),
        ConvMode::Native | ConvMode::Legacy => {
            let (mut from, mut to) = (from, to);
            let (signed, ignore_sign) = (from < 0, to < 0);
            // Preserve the source unchecked operations and their order. Even an
            // invalid from radix does not skip negating to before range checks.
            // Actual overflow-check settings decide MIN's panic/wrap behavior.
            if signed {
                from = -from;
            }
            if ignore_sign {
                to = -to;
            }
            (
                IntWithSign::from_signed_uint(from as u64, signed),
                IntWithSign::from_signed_uint(to as u64, ignore_sign),
            )
        }
    };
    (is_valid_base(from) && is_valid_base(to)).then_some((from, to))
}

fn conv_text_with_bases(
    text: &str,
    from: IntWithSign,
    to: IntWithSign,
    mode: ConvMode,
) -> Result<Option<String>> {
    let Some((digits, negative)) = extract_num_str(text.trim(), from) else {
        return Ok(Some("0".to_owned()));
    };
    let value = match extract_num(&digits, negative, from) {
        Ok(value) => value,
        Err(source) => {
            return match mode {
                // Keep the existing wire error and its conv(...) expression.
                ConvMode::Wire => {
                    Err(Error::overflow("BIGINT UNSIGNED", format!("conv({})", digits)).into())
                }
                ConvMode::Legacy => Ok(None),
                ConvMode::Native => {
                    let overflow = matches!(source.kind(), IntErrorKind::PosOverflow);
                    let source = EvaluateError::Caused(Box::new(source));
                    if overflow {
                        Err(EvaluateError::ConvUnsignedOverflow {
                            digits,
                            source: Box::new(source),
                        }
                        .into())
                    } else {
                        Err(source.into())
                    }
                }
            };
        }
    };
    let policy = match mode {
        ConvMode::Wire => ConvSignPolicy::WireOriginalSign,
        ConvMode::Native | ConvMode::Legacy => ConvSignPolicy::NativeWrappedSign,
    };
    Ok(Some(value.format_to_base(to, policy)))
}

fn conv_text(text: &str, from: Int, to: Int, mode: ConvMode) -> Result<Option<String>> {
    let Some((from, to)) = conv_bases(from, to, mode) else {
        return Ok(None);
    };
    conv_text_with_bases(text, from, to, mode)
}

#[inline]
#[rpn_fn]
pub fn conv(n: BytesRef, from_base: &Int, to_base: &Int) -> Result<Option<Bytes>> {
    let text = String::from_utf8_lossy(n);
    Ok(conv_text(&text, *from_base, *to_base, ConvMode::Wire)?.map(String::into_bytes))
}

#[rpn_fn(nullable)]
fn conv_native(
    n: Option<BytesRef>,
    from_base: Option<&Int>,
    to_base: Option<&Int>,
) -> Result<Option<Bytes>> {
    let (Some(from), Some(to)) = (from_base, to_base) else {
        return Ok(None);
    };
    let Some(n) = n else {
        return Ok(None);
    };
    let text = std::str::from_utf8(n).map_err(|source| EvaluateError::Caused(Box::new(source)))?;
    Ok(conv_text(text, *from, *to, ConvMode::Native)?.map(String::into_bytes))
}

#[rpn_fn(nullable)]
fn conv_binary_literal_native(
    n: Option<BytesRef>,
    from_base: Option<&Int>,
    to_base: Option<&Int>,
) -> Result<Option<Bytes>> {
    use std::fmt::Write;

    let (Some(from), Some(to)) = (from_base, to_base) else {
        return Ok(None);
    };
    let Some(n) = n else {
        return Ok(None);
    };
    // Materialize the full payload's bits, never its truncated u64 reading.
    // The source b'...' wrapper contributes no valid digits after the closing
    // quote, so only its trim-leading-zero digit substring is needed here.
    let capacity = n
        .len()
        .checked_mul(8)
        .ok_or_else(|| other_err!("CONV binary literal bit length overflow"))?;
    let mut bits = String::new();
    bits.try_reserve_exact(capacity)
        .map_err(|source| EvaluateError::Caused(Box::new(source)))?;
    for byte in n {
        write!(bits, "{byte:08b}").expect("writing to String cannot fail");
    }
    let digits = bits.trim_start_matches('0');
    let digits = if !bits.is_empty() && digits.is_empty() {
        "0"
    } else {
        digits
    };
    // Do not validate the final target before this first conversion: NULL or
    // overflow from 2 -> from terminates before from -> to can run.
    let Some(first) = conv_text(digits, 2, *from, ConvMode::Native)? else {
        return Ok(None);
    };
    Ok(conv_text(&first, *from, *to, ConvMode::Native)?.map(String::into_bytes))
}

#[rpn_fn(nullable)]
fn conv_legacy(
    n: Option<BytesRef>,
    from_base: Option<BytesRef>,
    to_base: Option<BytesRef>,
) -> Result<Option<Bytes>> {
    let Some(n) = n else {
        return Ok(None);
    };
    let Some(from) = from_base else {
        return Ok(None);
    };
    let Ok(from) = i64::try_from(decode_raw_i128(from)?) else {
        return Ok(None);
    };
    let Some(to) = to_base else {
        return Ok(None);
    };
    let Ok(to) = i64::try_from(decode_raw_i128(to)?) else {
        return Ok(None);
    };
    // A merely invalid from radix did not skip reading/converting to. Legacy
    // normalizes both bases before its lossy text conversion, unlike wire.
    let Some((from, to)) = conv_bases(from, to, ConvMode::Legacy) else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(n);
    Ok(conv_text_with_bases(&text, from, to, ConvMode::Legacy)?.map(String::into_bytes))
}

/// Resolves the native decimal scale policy before ready-argument construction.
/// The wire signatures retain their own bounded/i8 policies. This cap is not a
/// kernel memory budget; a finite logical byte allowance travels separately.
#[inline]
pub fn native_decimal_target_scale(requested: i64, result_decimal: Option<i64>) -> i32 {
    let target = requested.clamp(i64::from(i32::MIN), 30);
    result_decimal
        .filter(|scale| *scale >= 0)
        .map_or(target, |scale| target.min(scale)) as i32
}

// Go's math.Pow10 table policy. powi and a reciprocal positive power can have
// different low bits; retain the original table and multiplication/division
// order, including the asymmetric positive/negative exponent limits.
fn go_pow10(n: i64) -> f64 {
    const POW10_TAB: [f64; 32] = [
        1e00, 1e01, 1e02, 1e03, 1e04, 1e05, 1e06, 1e07, 1e08, 1e09, 1e10, 1e11, 1e12, 1e13, 1e14,
        1e15, 1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22, 1e23, 1e24, 1e25, 1e26, 1e27, 1e28, 1e29,
        1e30, 1e31,
    ];
    const POW10_POSTAB32: [f64; 10] = [
        1e00, 1e32, 1e64, 1e96, 1e128, 1e160, 1e192, 1e224, 1e256, 1e288,
    ];
    const POW10_NEGTAB32: [f64; 11] = [
        1e-00, 1e-32, 1e-64, 1e-96, 1e-128, 1e-160, 1e-192, 1e-224, 1e-256, 1e-288, 1e-320,
    ];
    if (0..=308).contains(&n) {
        let n = n as usize;
        POW10_POSTAB32[n / 32] * POW10_TAB[n % 32]
    } else if (-323..=0).contains(&n) {
        let n = (-n) as usize;
        POW10_NEGTAB32[n / 32] / POW10_TAB[n % 32]
    } else if n > 0 {
        f64::INFINITY
    } else {
        0.0
    }
}

// Native ROUND uses Go's multiply, ties-even, divide policy even for two-arg
// integer signatures. Keep its NaN-to-zero guard distinct from the wire policy.
fn go_round_float(value: f64, scale: i64) -> f64 {
    let shift = go_pow10(scale);
    let tmp = value * shift;
    if tmp.is_infinite() {
        return value;
    }
    let result = tmp.round_ties_even() / shift;
    if result.is_nan() { 0.0 } else { result }
}

fn go_truncate_float(value: f64, scale: i64) -> f64 {
    let shift = go_pow10(scale);
    let tmp = value * shift;
    if tmp.is_infinite() || tmp.is_nan() {
        return value;
    }
    if shift == 0.0 {
        return if value.is_nan() { value } else { 0.0 };
    }
    tmp.trunc() / shift
}

// These exact integer operations also implement the existing wire signed-scale
// signatures: a power outside the signed/unsigned integer range yields zero.
fn go_truncate_int(value: i64, scale: i64) -> i64 {
    if scale >= 0 {
        return value;
    }
    let shift = scale
        .checked_neg()
        .and_then(|n| u32::try_from(n).ok())
        .and_then(|n| 10i64.checked_pow(n));
    match shift {
        Some(shift) => value / shift * shift,
        None => 0,
    }
}

fn go_truncate_uint(value: u64, scale: i64) -> u64 {
    if scale >= 0 {
        return value;
    }
    let shift = scale
        .checked_neg()
        .and_then(|n| u32::try_from(n).ok())
        .and_then(|n| 10u64.checked_pow(n));
    match shift {
        Some(shift) => value / shift * shift,
        None => 0,
    }
}

fn native_decimal_failure(source: NativeDecimalError) -> tidb_query_common::Error {
    // The blanket boxed-error conversion stringifies its source. This explicit
    // carrier retains the actual native bridge/core/resource error instead.
    EvaluateError::Caused(Box::new(source)).into()
}

fn native_decimal_math(
    value: &Decimal,
    operation: NativeDecimalOp,
    raw_budget: &Int,
) -> Result<Decimal> {
    let budget = usize::try_from(*raw_budget as u64).map_err(|_| {
        native_decimal_failure(NativeDecimalError::Resource(
            "kernel budget exceeds indexing width",
        ))
    })?;
    if budget == usize::MAX {
        return Err(native_decimal_failure(NativeDecimalError::Resource(
            "native math requires a finite kernel budget",
        )));
    }
    value
        .try_native_math(operation, budget)
        .map_err(native_decimal_failure)
}

fn native_decimal_resolved_scale(scale: &Int) -> Result<i32> {
    i32::try_from(*scale).map_err(|_| {
        native_decimal_failure(NativeDecimalError::InvalidInput(
            "resolved native scale exceeds i32",
        ))
    })
}

// Factory-only native policies. These wrappers do not change any wire signature
// or add a PB dispatch surface. NULL still runs the RPN wrapper; no value-layer
// answer or invented argument stands in for that invocation.
#[rpn_fn]
fn abs_int_native(arg: &Int) -> Result<Option<Int>> {
    match abs_int_value(*arg) {
        Some(value) => Ok(Some(value)),
        None => {
            let source: EvaluateError = Error::overflow("BIGINT", format!("abs({})", *arg)).into();
            Err(EvaluateError::AbsSignedOverflow {
                source: Box::new(source),
            }
            .into())
        }
    }
}

#[rpn_fn]
fn abs_uint_native(arg: &Int) -> Result<Option<Int>> {
    abs_uint(arg)
}

#[rpn_fn(nullable)]
fn abs_real_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(arg)?.map(abs_f64).map(encode_raw_f64))
}

#[rpn_fn]
fn abs_decimal_native(arg: &Decimal, budget: &Int) -> Result<Option<Decimal>> {
    native_decimal_math(arg, NativeDecimalOp::Abs, budget).map(Some)
}

#[rpn_fn]
fn ceil_int_native(arg: &Int) -> Result<Option<Int>> {
    Ok(Some(*arg))
}

#[rpn_fn]
fn floor_int_native(arg: &Int) -> Result<Option<Int>> {
    Ok(Some(*arg))
}

#[rpn_fn(nullable)]
fn ceil_real_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(arg)?
        .map(|value| ceil_floor_f64(value, true))
        .map(encode_raw_f64))
}

#[rpn_fn(nullable)]
fn floor_real_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(arg)?
        .map(|value| ceil_floor_f64(value, false))
        .map(encode_raw_f64))
}

#[rpn_fn]
fn ceil_decimal_native(arg: &Decimal, budget: &Int) -> Result<Option<Decimal>> {
    native_decimal_math(arg, NativeDecimalOp::Ceil, budget).map(Some)
}

#[rpn_fn]
fn floor_decimal_native(arg: &Decimal, budget: &Int) -> Result<Option<Decimal>> {
    native_decimal_math(arg, NativeDecimalOp::Floor, budget).map(Some)
}

#[rpn_fn]
fn round_int_native(arg: &Int) -> Result<Option<Int>> {
    round_int(arg)
}

#[rpn_fn]
fn round_int_with_scale_native(arg: &Int, scale: &Int) -> Result<Option<Int>> {
    // UInt uses these same signed bits; the native owner restores its unsigned
    // result tag only after this signed f64 round-trip, including scale >= 0.
    Ok(Some(go_round_float(*arg as f64, *scale) as Int))
}

#[rpn_fn]
fn round_real_native(arg: BytesRef, scale: &Int) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(Some(arg))?
        .map(|value| go_round_float(value, *scale))
        .map(encode_raw_f64))
}

#[rpn_fn]
fn round_decimal_native(arg: &Decimal, scale: &Int, budget: &Int) -> Result<Option<Decimal>> {
    native_decimal_math(
        arg,
        NativeDecimalOp::Round(native_decimal_resolved_scale(scale)?),
        budget,
    )
    .map(Some)
}

#[rpn_fn]
fn truncate_int_native(arg: &Int, scale: &Int) -> Result<Option<Int>> {
    truncate_int_with_int(arg, scale)
}

#[rpn_fn]
fn truncate_uint_native(arg: &Int, scale: &Int) -> Result<Option<Int>> {
    truncate_uint_with_int(arg, scale)
}

#[rpn_fn]
fn truncate_int_unsigned_scale_native(arg: &Int, scale: &Int) -> Result<Option<Int>> {
    // This identity works for either value signedness. The real unsigned scale
    // remains an input: in particular the RPN NULL wrapper still observes it.
    truncate_int_with_uint(arg, scale)
}

#[rpn_fn]
fn truncate_real_native(arg: BytesRef, scale: &Int) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(Some(arg))?
        .map(|value| go_truncate_float(value, *scale))
        .map(encode_raw_f64))
}

#[rpn_fn]
fn truncate_decimal_native(arg: &Decimal, scale: &Int, budget: &Int) -> Result<Option<Decimal>> {
    native_decimal_math(
        arg,
        NativeDecimalOp::Truncate(native_decimal_resolved_scale(scale)?),
        budget,
    )
    .map(Some)
}

fn decode_raw_i128(arg: BytesRef) -> Result<i128> {
    let bytes = <[u8; 16]>::try_from(arg).map_err(|_| {
        other_err!(
            "Internal raw i128 transport requires exactly 16 bytes, received {}",
            arg.len()
        )
    })?;
    Ok(i128::from_le_bytes(bytes))
}

#[rpn_fn]
fn round_int128_legacy(arg: BytesRef) -> Result<Option<Bytes>> {
    Ok(Some(decode_raw_i128(arg)?.to_le_bytes().to_vec()))
}

#[rpn_fn(nullable)]
fn round_real_legacy(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(decode_raw_f64(arg)?.map(f64::round).map(encode_raw_f64))
}

#[rpn_fn(capture = [ctx])]
fn round_decimal_legacy(
    ctx: &mut EvalContext,
    arg: &Decimal,
    budget: &Int,
) -> Result<Option<Bytes>> {
    let rounded = native_decimal_math(arg, NativeDecimalOp::Round(0), budget)?;
    // At scale zero the exact result has matching storage/result scales. Use
    // the existing Rust storage-value conversion, not a native display string.
    let value: f64 = rounded
        .convert(ctx)
        .map_err(|source| native_decimal_failure(NativeDecimalError::Core(source)))?;
    Ok(Some(encode_raw_f64(value)))
}

#[rpn_fn(nullable)]
fn math_null_witness_native(arg: Option<&Int>) -> Result<Option<Int>> {
    match arg {
        None => Ok(None),
        Some(_) => Err(other_err!(
            "Native math NULL witness must be an actual NULL"
        )),
    }
}

#[inline]
#[rpn_fn]
pub fn round_real(arg: &Real) -> Result<Option<Real>> {
    Ok(Real::new(arg.round_ties_even()).ok())
}

#[inline]
#[rpn_fn]
pub fn round_int(arg: &Int) -> Result<Option<Int>> {
    Ok(Some(arg.to_owned()))
}

#[inline]
#[rpn_fn]
pub fn round_dec(arg: &Decimal) -> Result<Option<Decimal>> {
    let res: codec::Result<Decimal> = arg
        .to_owned()
        .round(DEFAULT_FSP, RoundMode::HalfEven)
        .into();
    Ok(Some(res?))
}

#[inline]
#[rpn_fn]
pub fn truncate_int_with_int(arg0: &Int, arg1: &Int) -> Result<Option<Int>> {
    Ok(Some(go_truncate_int(*arg0, *arg1)))
}

#[inline]
#[rpn_fn]
pub fn truncate_int_with_uint(arg0: &Int, _arg1: &Int) -> Result<Option<Int>> {
    Ok(Some(*arg0))
}

#[inline]
#[rpn_fn]
pub fn truncate_uint_with_int(arg0: &Int, arg1: &Int) -> Result<Option<Int>> {
    Ok(Some(go_truncate_uint(*arg0 as u64, *arg1) as Int))
}

#[inline]
#[rpn_fn]
pub fn truncate_uint_with_uint(arg0: &Int, _arg1: &Int) -> Result<Option<Int>> {
    Ok(Some(*arg0))
}

#[inline]
#[rpn_fn]
pub fn truncate_real_with_int(arg0: &Real, arg1: &Int) -> Result<Option<Real>> {
    let d = if *arg1 >= 0 {
        (*arg1).min(i64::from(i32::MAX)) as i32
    } else {
        (*arg1).max(i64::from(i32::MIN)) as i32
    };
    Ok(Some(truncate_real(*arg0, d)))
}

#[inline]
#[rpn_fn]
pub fn truncate_real_with_uint(arg0: &Real, arg1: &Int) -> Result<Option<Real>> {
    let d = (*arg1 as u64).min(i32::MAX as u64) as i32;
    Ok(Some(truncate_real(*arg0, d)))
}

fn truncate_real(x: Real, d: i32) -> Real {
    let shift = 10_f64.powi(d);
    let tmp = x * shift;
    if *tmp == 0_f64 {
        Real::new(0_f64).unwrap()
    } else if tmp.is_infinite() {
        x
    } else {
        Real::new(tmp.trunc() / shift).unwrap()
    }
}

#[inline]
#[rpn_fn]
pub fn truncate_decimal_with_int(arg0: &Decimal, arg1: &Int) -> Result<Option<Decimal>> {
    let d = if *arg1 >= 0 {
        *arg1.min(&127) as i8
    } else {
        *arg1.max(&-128) as i8
    };

    let res: codec::Result<Decimal> = arg0.clone().round(d, RoundMode::Truncate).into();
    Ok(Some(res?))
}

#[inline]
#[rpn_fn]
pub fn truncate_decimal_with_uint(arg0: &Decimal, arg1: &Int) -> Result<Option<Decimal>> {
    let d = (*arg1 as u64).min(127) as i8;

    let res: codec::Result<Decimal> = arg0.clone().round(d, RoundMode::Truncate).into();
    Ok(Some(res?))
}

#[inline]
#[rpn_fn]
pub fn round_with_frac_int(arg0: &Int, arg1: &Int) -> Result<Option<Int>> {
    let number = arg0;
    let digits = arg1;
    if *digits >= 0 {
        Ok(Some(*number))
    } else {
        let power = 10.0_f64.powi(-digits as i32);
        let frac = *number as f64 / power;
        Ok(Some((frac.round() * power) as i64))
    }
}

#[rpn_fn]
#[inline]
fn round_with_frac_dec(arg0: &Decimal, arg1: &Int) -> Result<Option<Decimal>> {
    let number = arg0;
    let digits = arg1;
    let res: codec::Result<Decimal> = number
        .to_owned()
        .round(*digits as i8, RoundMode::HalfEven)
        .into();
    Ok(Some(res?))
}

#[inline]
#[rpn_fn]
pub fn round_with_frac_real(arg0: &Real, arg1: &Int) -> Result<Option<Real>> {
    let number = arg0;
    let digits = arg1;
    let power = 10.0_f64.powi(*digits as i32);
    let frac = *number * power;
    if frac.is_infinite() {
        return Ok(Some(*number));
    }
    Ok(Some(Real::new(frac.round_ties_even() / power).unwrap()))
}

thread_local! {
   static MYSQL_RNG: RefCell<MySqlRng> = RefCell::new(MySqlRng::new())
}

#[derive(Copy, Clone)]
struct IntWithSign(u64, bool);

impl IntWithSign {
    fn from_int(num: Int) -> IntWithSign {
        IntWithSign(num.wrapping_abs() as u64, num < 0)
    }

    fn from_signed_uint(num: u64, is_neg: bool) -> IntWithSign {
        IntWithSign(num, is_neg)
    }

    // Shrink num to fit the boundary of i64.
    fn shrink_from_signed_uint(num: u64, is_neg: bool) -> IntWithSign {
        let value = if is_neg {
            // Avoid int64 overflow error.
            // -int64_min = int64_max + 1
            num.min(Int::MAX as u64 + 1)
        } else {
            num.min(Int::MAX as u64)
        };
        IntWithSign::from_signed_uint(value, is_neg)
    }

    fn format_radix(mut x: u64, radix: u32) -> String {
        let mut r = vec![];
        loop {
            let m = x % u64::from(radix);
            x /= u64::from(radix);
            r.push(
                std::char::from_digit(m as u32, radix)
                    .unwrap()
                    .to_ascii_uppercase(),
            );
            if x == 0 {
                break;
            }
        }
        r.iter().rev().collect::<String>()
    }

    fn format_to_base(self, to_base: IntWithSign, policy: ConvSignPolicy) -> String {
        let IntWithSign(value, is_neg) = self;
        let IntWithSign(to_base, should_ignore_sign) = to_base;
        let (magnitude, negative) = match policy {
            ConvSignPolicy::WireOriginalSign => {
                let mut real_val = value as i64;
                // Preserve the wire guard and original sign, including -0 and
                // magnitudes above i64::MAX. This is not native wrapped sign.
                if is_neg && !should_ignore_sign && real_val > 0 {
                    real_val = -real_val;
                }
                (real_val as u64, is_neg && should_ignore_sign)
            }
            ConvSignPolicy::NativeWrappedSign => {
                let mut bits = if is_neg { value.wrapping_neg() } else { value };
                let negative = (bits as i64) < 0;
                if should_ignore_sign && negative {
                    bits = bits.wrapping_neg();
                }
                (bits, negative && should_ignore_sign)
            }
        };
        let mut ret = IntWithSign::format_radix(magnitude, to_base as u32);
        if negative {
            ret.insert(0, '-');
        }
        ret
    }
}

fn is_valid_base(base: IntWithSign) -> bool {
    let IntWithSign(num, _) = base;
    (2..=36).contains(&num)
}

fn extract_num_str(s: &str, from_base: IntWithSign) -> Option<(String, bool)> {
    let mut iter = s.chars().peekable();
    let head = *iter.peek()?;
    let mut is_neg = false;
    if head == '+' || head == '-' {
        is_neg = head == '-';
        iter.next();
    }
    let IntWithSign(base, _) = from_base;
    let s = iter
        .take_while(|x| x.is_digit(base as u32))
        .collect::<String>();
    if s.is_empty() {
        None
    } else {
        Some((s, is_neg))
    }
}

/// The native prefix-only compatibility surface. Deliberately does not trim;
/// top-level CONV owns whitespace normalization. All callers share this
/// scanner.
pub fn conv_valid_prefix_native(s: &str, base: u32) -> String {
    match extract_num_str(s, IntWithSign::from_signed_uint(u64::from(base), false)) {
        Some((mut digits, negative)) => {
            if negative {
                digits.insert(0, '-');
            }
            digits
        }
        None => String::new(),
    }
}

fn extract_num(
    num_s: &str,
    is_neg: bool,
    from_base: IntWithSign,
) -> std::result::Result<IntWithSign, ParseIntError> {
    let IntWithSign(from_base, signed) = from_base;
    let value = u64::from_str_radix(num_s, from_base as u32)?;
    Ok(if signed {
        IntWithSign::shrink_from_signed_uint(value, is_neg)
    } else {
        IntWithSign::from_signed_uint(value, is_neg)
    })
}

// Returns (isize, is_positive): convert an i64 to usize, and whether the input
// is positive
//
// # Examples
// ```
// assert_eq!(i64_to_usize(1_i64, false), (1_usize, true));
// assert_eq!(i64_to_usize(1_i64, false), (1_usize, true));
// assert_eq!(i64_to_usize(-1_i64, false), (1_usize, false));
// assert_eq!(
//     i64_to_usize(u64::MAX as i64, true),
//     (u64::MAX as usize, true)
// );
// assert_eq!(i64_to_usize(u64::MAX as i64, false), (1_usize, false));
// ```
#[inline]
pub fn i64_to_usize(i: i64, is_unsigned: bool) -> (usize, bool) {
    if is_unsigned {
        (i as u64 as usize, true)
    } else if i >= 0 {
        (i as usize, true)
    } else {
        let i = if i == i64::MIN {
            i64::MAX as usize + 1
        } else {
            -i as usize
        };
        (i, false)
    }
}

pub struct MySqlRng {
    seed1: u32,
    seed2: u32,
}

impl MySqlRng {
    fn new() -> Self {
        let current_time = get_time();
        let nsec = i64::from(current_time.nsec);
        Self::new_with_seed(nsec)
    }

    fn new_with_seed(seed: i64) -> Self {
        let seed1 = (seed.wrapping_mul(0x10001).wrapping_add(55555555)) as u32 % MAX_RAND_VALUE;
        let seed2 = (seed.wrapping_mul(0x10000001)) as u32 % MAX_RAND_VALUE;
        MySqlRng { seed1, seed2 }
    }

    fn gen(&mut self) -> f64 {
        self.seed1 = (self.seed1 * 3 + self.seed2) % MAX_RAND_VALUE;
        self.seed2 = (self.seed1 + self.seed2 + 33) % MAX_RAND_VALUE;
        f64::from(self.seed1) / f64::from(MAX_RAND_VALUE)
    }
}

impl Default for MySqlRng {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::{f64, str::FromStr};

    use tidb_query_datatype::{FieldTypeFlag, FieldTypeTp, builder::FieldTypeBuilder};
    use tipb::ScalarFuncSig;

    use super::*;
    use crate::types::test_util::RpnFnScalarEvaluator;

    #[test]
    fn test_exp_go_native_raw_golden_and_specials() {
        let arg = encode_raw_f64(1.5);
        assert_eq!(
            exp_go_native(Some(&arg)).unwrap(),
            Some(encode_raw_f64(4.481689070338065_f64))
        );
        assert_eq!(exp_go_native(None).unwrap(), None);
        for (value, expected) in [(f64::INFINITY, f64::INFINITY), (f64::NEG_INFINITY, 0.0)] {
            let arg = encode_raw_f64(value);
            assert_eq!(
                exp_go_native(Some(&arg)).unwrap(),
                Some(encode_raw_f64(expected))
            );
        }
        let arg = encode_raw_f64(f64::NAN);
        let result = exp_go_native(Some(&arg)).unwrap().unwrap();
        assert!(decode_raw_f64(Some(&result)).unwrap().unwrap().is_nan());
    }

    #[test]
    fn test_log10_go_native_raw_golden_and_specials() {
        let arg = encode_raw_f64(100.0);
        assert_eq!(
            log10_go_native(Some(&arg)).unwrap(),
            Some(encode_raw_f64(2.0))
        );
        assert_eq!(log10_go_native(None).unwrap(), None);
        let arg = encode_raw_f64(f64::INFINITY);
        assert_eq!(
            log10_go_native(Some(&arg)).unwrap(),
            Some(encode_raw_f64(f64::INFINITY))
        );
        for value in [f64::NAN, f64::NEG_INFINITY] {
            let arg = encode_raw_f64(value);
            let result = log10_go_native(Some(&arg)).unwrap().unwrap();
            assert!(decode_raw_f64(Some(&result)).unwrap().unwrap().is_nan());
        }
    }

    #[test]
    fn test_trig_native_go_and_libm_keep_distinct_computed_bits() {
        let one = encode_raw_f64(1.0);
        let go = cot_go_native(Some(&one)).unwrap().unwrap();
        let libm = cot_libm_legacy(Some(&one)).unwrap().unwrap();
        assert_eq!(go, encode_raw_f64(0.6420926159343308_f64));
        assert_eq!(libm, encode_raw_f64(1.0 / 1.0_f64.tan()));
        assert_ne!(go, libm);
        let negative_zero = encode_raw_f64(-0.0);
        assert_eq!(
            atan_go_native(Some(&negative_zero)).unwrap(),
            Some(negative_zero)
        );
        // Go COS returns its computed canonical NaN, not the original payload.
        let payload_nan = encode_raw_f64(f64::from_bits(0x7ff8_0000_0000_0042));
        assert_eq!(
            cos_go_native(Some(&payload_nan)).unwrap(),
            Some(encode_raw_f64(f64::NAN))
        );
        assert_eq!(sin_go_native(None).unwrap(), None);
    }

    #[test]
    fn test_trig_legacy_raw_classes_and_atan2_order() {
        for zero in [0.0, -0.0] {
            let arg = encode_raw_f64(zero);
            let expected = Some(encode_raw_f64(1.0 / zero));
            assert_eq!(cot_libm_legacy(Some(&arg)).unwrap(), expected);
            assert_eq!(cot_go_native(Some(&arg)).unwrap(), expected);
            // Wire retains its existing infinity error, unlike either raw ABI.
            assert!(cot(&Real::new(zero).unwrap()).is_err());
        }
        let nan = f64::from_bits(0x7ff8_0000_0000_0042);
        let arg = encode_raw_f64(nan);
        assert_eq!(
            sin_libm_legacy(Some(&arg)).unwrap(),
            Some(encode_raw_f64(nan.sin()))
        );
        let infinity = encode_raw_f64(f64::INFINITY);
        let legacy = cos_libm_legacy(Some(&infinity)).unwrap().unwrap();
        assert!(decode_raw_f64(Some(&legacy)).unwrap().unwrap().is_nan());
        assert_eq!(cos(&Real::new(f64::INFINITY).unwrap()).unwrap(), None);
        let y = encode_raw_f64(1.0);
        let x = encode_raw_f64(2.0);
        assert_eq!(
            atan2_libm_legacy(Some(&y), Some(&x)).unwrap(),
            Some(encode_raw_f64(1.0_f64.atan2(2.0)))
        );
        assert_eq!(
            atan2_go_native(Some(&y), Some(&x)).unwrap(),
            Some(encode_raw_f64(native_go_trig::go_atan2(1.0, 2.0)))
        );
        assert_eq!(atan2_libm_legacy(None, Some(&x)).unwrap(), None);
    }

    #[test]
    fn test_native_math_scalar_policies_and_overflow_cause() {
        let wide = 9_007_199_254_740_993;
        assert_eq!(round_int_native(&wide).unwrap(), Some(wide));
        assert_eq!(round_with_frac_int(&wide, &0).unwrap(), Some(wide));
        assert_eq!(
            round_int_with_scale_native(&wide, &0).unwrap(),
            Some(wide - 1)
        );
        assert_eq!(round_int_with_scale_native(&-6, &-1).unwrap(), Some(-10));
        assert_eq!(go_pow10(23).to_bits(), 1e23_f64.to_bits());
        assert_eq!(go_round_float(2.5, 0).to_bits(), 2.0_f64.to_bits());
        assert_eq!(go_round_float(0.0, 309).to_bits(), 0.0_f64.to_bits());
        let nan = f64::from_bits(0x7ff8_0000_0000_0042);
        assert_eq!(go_truncate_float(nan, i64::MIN).to_bits(), nan.to_bits());
        assert_eq!(
            go_truncate_float(1.0, i64::MIN).to_bits(),
            0.0_f64.to_bits()
        );
        assert_eq!(truncate_int_native(&i64::MIN, &-19).unwrap(), Some(0));
        assert_eq!(
            truncate_uint_native(&-1, &-19).unwrap(),
            Some(10_000_000_000_000_000_000_u64 as i64)
        );
        assert_eq!(
            truncate_int_unsigned_scale_native(&-1, &-1).unwrap(),
            Some(-1)
        );
        let bits = encode_raw_f64(2.5);
        assert_eq!(
            round_real_legacy(Some(bits.as_slice())).unwrap(),
            Some(encode_raw_f64(3.0))
        );
        let wide_bits = i128::MIN.to_le_bytes();
        assert_eq!(
            round_int128_legacy(&wide_bits).unwrap(),
            Some(wide_bits.to_vec())
        );
        assert!(round_int128_legacy(&wide_bits[..15]).is_err());
        assert_eq!(math_null_witness_native(None).unwrap(), None);
        assert!(math_null_witness_native(Some(&0)).is_err());
        let native_error = abs_int_native(&i64::MIN).unwrap_err();
        let tidb_query_common::error::ErrorInner::Evaluate(EvaluateError::AbsSignedOverflow {
            source,
        }) = native_error.0.as_ref()
        else {
            panic!("native ABS must retain its typed overflow source");
        };
        assert_eq!(source.code(), 1690);
        assert!(matches!(
            abs_int(&i64::MIN).unwrap_err().0.as_ref(),
            tidb_query_common::error::ErrorInner::Evaluate(EvaluateError::Custom {
                code: 1690,
                ..
            })
        ));
    }

    #[test]
    fn test_native_decimal_scale_policy_and_finite_budget_cause() {
        assert_eq!(native_decimal_target_scale(i64::MIN, None), i32::MIN);
        assert_eq!(native_decimal_target_scale(i64::MAX, Some(-1)), 30);
        assert_eq!(native_decimal_target_scale(20, Some(2)), 2);
        assert_eq!(native_decimal_target_scale(-129, Some(0)), -129);
        // This is the actual TiKV Decimal constructor, not the native value
        // type's distinct from_literal API or a wide transport through Display.
        let value = Decimal::from_str("-2.5").unwrap();
        assert_eq!(
            round_decimal_native(&value, &0, &64).unwrap(),
            Some(Decimal::from(-3))
        );
        assert!(round_decimal_native(&value, &i64::MAX, &64).is_err());
        assert!(abs_decimal_native(&value, &-1).is_err());
        let failure = abs_decimal_native(&value, &1).unwrap_err();
        let tidb_query_common::error::ErrorInner::Evaluate(EvaluateError::Caused(source)) =
            failure.0.as_ref()
        else {
            panic!("native decimal refusal must retain its owned cause");
        };
        assert!(matches!(
            source.downcast_ref::<NativeDecimalError>(),
            Some(NativeDecimalError::Resource(_))
        ));
    }

    #[test]
    fn test_pi() {
        let output = RpnFnScalarEvaluator::new()
            .evaluate(ScalarFuncSig::Pi)
            .unwrap();
        assert_eq!(output, Some(Real::new(std::f64::consts::PI).unwrap()));
    }

    #[test]
    fn test_crc32() {
        let cases = vec![
            (Some(""), Some(0)),
            (Some("-1"), Some(808273962)),
            (Some("mysql"), Some(2501908538)),
            (Some("MySQL"), Some(3259397556)),
            (Some("hello"), Some(907060870)),
            (Some("❤️"), Some(4067711813)),
            (None, None),
        ];

        for (input, expect) in cases {
            let input = input.map(|s| s.as_bytes().to_vec());
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate(ScalarFuncSig::Crc32)
                .unwrap();
            assert_eq!(output, expect);
        }
    }

    #[test]
    fn test_log_1_arg() {
        let test_cases = vec![
            (Some(std::f64::consts::E), Some(Real::new(1.0_f64).unwrap())),
            (Some(100.0), Some(Real::new(4.605170185988092_f64).unwrap())),
            (Some(-1.0), None),
            (Some(0.0), None),
            (None, None),
        ];
        for (input, expect) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate(ScalarFuncSig::Log1Arg)
                .unwrap();
            assert_eq!(output, expect, "{:?}", input);
        }
    }

    #[test]
    fn test_log_2_arg() {
        let test_cases = vec![
            (
                Some(10.0_f64),
                Some(100.0_f64),
                Some(Real::new(2.0_f64).unwrap()),
            ),
            (
                Some(2.0_f64),
                Some(1.0_f64),
                Some(Real::new(0.0_f64).unwrap()),
            ),
            (
                Some(0.5_f64),
                Some(0.25_f64),
                Some(Real::new(2.0_f64).unwrap()),
            ),
            (Some(-0.23323_f64), Some(2.0_f64), None),
            (Some(0_f64), Some(123_f64), None),
            (Some(1_f64), Some(123_f64), None),
            (Some(1123_f64), Some(0_f64), None),
            (None, None, None),
            (Some(2.0_f64), None, None),
            (None, Some(2.0_f64), None),
        ];
        for (a1, a2, expect) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(a1)
                .push_param(a2)
                .evaluate(ScalarFuncSig::Log2Args)
                .unwrap();
            assert_eq!(output, expect, "arg1 {:?}, arg2 {:?}", a1, a2);
        }
    }

    #[test]
    fn test_log2() {
        let test_cases = vec![
            (Some(16_f64), Some(Real::new(4_f64).unwrap())),
            (Some(5_f64), Some(Real::new(2.321928094887362_f64).unwrap())),
            (Some(-1.234_f64), None),
            (Some(0_f64), None),
            (None, None),
        ];
        for (input, expect) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate(ScalarFuncSig::Log2)
                .unwrap();
            assert_eq!(output, expect, "{:?}", input);
        }
    }

    #[test]
    fn test_log10() {
        let test_cases = vec![
            (Some(100_f64), Some(Real::new(2_f64).unwrap())),
            (
                Some(101_f64),
                Some(Real::new(2.0043213737826426_f64).unwrap()),
            ),
            (Some(-1.234_f64), None),
            (Some(0_f64), None),
            (None, None),
        ];
        for (input, expect) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate(ScalarFuncSig::Log10)
                .unwrap();
            assert_eq!(output, expect, "{:?}", input);
        }
    }

    #[test]
    fn test_abs_int() {
        let test_cases = vec![
            (ScalarFuncSig::AbsInt, -3, Some(3), false),
            (ScalarFuncSig::AbsInt, i64::MAX, Some(i64::MAX), false),
            (
                ScalarFuncSig::AbsUInt,
                u64::MAX as i64,
                Some(u64::MAX as i64),
                false,
            ),
            (ScalarFuncSig::AbsInt, i64::MIN, Some(0), true),
        ];

        for (sig, arg, expect_output, is_err) in test_cases {
            let output = RpnFnScalarEvaluator::new().push_param(arg).evaluate(sig);

            if is_err {
                assert!(output.is_err());
            } else {
                let output = output.unwrap();
                assert_eq!(output, expect_output, "{:?}", arg);
            }
        }
    }

    #[test]
    fn test_abs_real() {
        let test_cases: Vec<(Real, Option<Real>)> = vec![
            (Real::new(3.5).unwrap(), Real::new(3.5).ok()),
            (Real::new(-3.5).unwrap(), Real::new(3.5).ok()),
        ];

        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate(ScalarFuncSig::AbsReal)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", arg);
        }
    }

    #[test]
    fn test_abs_decimal() {
        let test_cases = vec![("1.1", "1.1"), ("-1.1", "1.1")];

        for (arg, expect_output) in test_cases {
            let arg = arg.parse::<Decimal>().ok();
            let expect_output = expect_output.parse::<Decimal>().ok();
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg.clone())
                .evaluate(ScalarFuncSig::AbsDecimal)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}", arg);
        }
    }

    #[test]
    fn test_ceil_real() {
        let cases = vec![
            (4.0, 3.5),
            (4.0, 3.45),
            (4.0, 3.1),
            (-3.0, -3.45),
            (0.0, -0.1),
            (f64::MAX, f64::MAX),
            (f64::MIN, f64::MIN),
        ];
        for (expected, input) in cases {
            let arg = Real::new(input).unwrap();
            let expected = Real::new(expected).ok();
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Real>(ScalarFuncSig::CeilReal)
                .unwrap();
            assert_eq!(expected, output);
        }
    }

    #[test]
    fn test_ceil_dec_to_dec() {
        let cases = vec![
            ("9223372036854775808", "9223372036854775808"),
            ("124", "123.456"),
            ("-123", "-123.456"),
        ];

        for (expected, input) in cases {
            let arg = input.parse::<Decimal>().ok();
            let expected = expected.parse::<Decimal>().ok();
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Decimal>(ScalarFuncSig::CeilDecToDec)
                .unwrap();
            assert_eq!(expected, output);
        }
    }

    #[test]
    fn test_ceil_int_to_dec() {
        let cases = vec![
            ("-9223372036854775808", i64::MIN),
            ("9223372036854775807", i64::MAX),
            ("123", 123),
            ("-123", -123),
        ];
        for (expected, input) in cases {
            let expected = expected.parse::<Decimal>().ok();
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate::<Decimal>(ScalarFuncSig::CeilIntToDec)
                .unwrap();
            assert_eq!(expected, output);
        }
    }

    #[test]
    fn test_ceil_dec_to_int() {
        let cases = vec![
            (124, "123.456"),
            (2, "1.23"),
            (-1, "-1.23"),
            (i64::MIN, "-9223372036854775808"),
        ];
        for (expected, input) in cases {
            let arg = input.parse::<Decimal>().ok();
            let expected = Some(expected);
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Int>(ScalarFuncSig::CeilDecToInt)
                .unwrap();
            assert_eq!(expected, output);
        }
    }

    #[test]
    fn test_ceil_int_to_int() {
        let cases = vec![
            (1, 1),
            (2, 2),
            (666, 666),
            (-3, -3),
            (-233, -233),
            (i64::MAX, i64::MAX),
            (i64::MIN, i64::MIN),
        ];

        for (expected, input) in cases {
            let expected = Some(expected);
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate::<Int>(ScalarFuncSig::CeilIntToInt)
                .unwrap();
            assert_eq!(expected, output);
        }
    }

    fn test_unary_func_ok_none<I, O>(sig: ScalarFuncSig)
    where
        I: Evaluable,
        O: EvaluableRet + PartialEq,
        Option<I>: Into<ScalarValue>,
        Option<O>: From<ScalarValue>,
    {
        assert_eq!(
            None,
            RpnFnScalarEvaluator::new()
                .push_param(Option::<I>::None)
                .evaluate::<O>(sig)
                .unwrap()
        );
    }

    #[test]
    fn test_floor_real() {
        let cases = vec![
            (3.5, 3.0),
            (3.7, 3.0),
            (3.45, 3.0),
            (3.1, 3.0),
            (-3.45, -4.0),
            (-0.1, -1.0),
            (16140901064495871255.0, 16140901064495871255.0),
            (f64::MAX, f64::MAX),
            (f64::MIN, f64::MIN),
        ];
        for (input, expected) in cases {
            let arg = Real::new(input).unwrap();
            let expected = Real::new(expected).ok();
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Real>(ScalarFuncSig::FloorReal)
                .unwrap();
            assert_eq!(expected, output);
        }

        test_unary_func_ok_none::<Real, Real>(ScalarFuncSig::FloorReal);
    }

    #[test]
    fn test_floor_int_to_dec() {
        let tests_cases = vec![
            (i64::MIN, "-9223372036854775808"),
            (i64::MAX, "9223372036854775807"),
            (123, "123"),
            (-123, "-123"),
        ];

        for (input, expected) in tests_cases {
            let expected = expected.parse::<Decimal>().ok();
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate::<Decimal>(ScalarFuncSig::FloorIntToDec)
                .unwrap();
            assert_eq!(output, expected);
        }

        test_unary_func_ok_none::<Int, Decimal>(ScalarFuncSig::FloorIntToDec);
    }

    #[test]
    fn test_floor_dec_to_dec() {
        let cases = vec![
            ("9223372036854775808", "9223372036854775808"),
            ("123.456", "123"),
            ("-123.456", "-124"),
        ];

        for (input, expected) in cases {
            let arg = input.parse::<Decimal>().ok();
            let expected = expected.parse::<Decimal>().ok();
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Decimal>(ScalarFuncSig::FloorDecToDec)
                .unwrap();
            assert_eq!(expected, output);
        }

        test_unary_func_ok_none::<Decimal, Decimal>(ScalarFuncSig::FloorDecToDec);
    }

    #[test]
    fn test_floor_dec_to_int() {
        let cases = vec![
            ("123.456", 123),
            ("1.23", 1),
            ("-1.23", -2),
            ("-9223372036854775808", i64::MIN),
        ];
        for (input, expected) in cases {
            let arg = input.parse::<Decimal>().ok();
            let expected = Some(expected);
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Int>(ScalarFuncSig::FloorDecToInt)
                .unwrap();
            assert_eq!(expected, output);
        }

        test_unary_func_ok_none::<Decimal, Int>(ScalarFuncSig::FloorDecToInt);
    }

    #[test]
    fn test_floor_int_to_int() {
        let cases = vec![
            (1, 1),
            (2, 2),
            (-3, -3),
            (i64::MAX, i64::MAX),
            (i64::MIN, i64::MIN),
        ];

        for (expected, input) in cases {
            let expected = Some(expected);
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate::<Int>(ScalarFuncSig::FloorIntToInt)
                .unwrap();
            assert_eq!(expected, output);
        }

        test_unary_func_ok_none::<Int, Int>(ScalarFuncSig::FloorIntToInt);
    }

    #[test]
    fn test_sign() {
        let test_cases = vec![
            (None, None),
            (Some(42f64), Some(1)),
            (Some(0f64), Some(0)),
            (Some(-47f64), Some(-1)),
        ];
        for (input, expect) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate(ScalarFuncSig::Sign)
                .unwrap();
            assert_eq!(expect, output, "{:?}", input);
        }
    }

    #[test]
    fn test_sqrt() {
        let test_cases = vec![
            (None, None),
            (Some(64f64), Some(Real::new(8f64).unwrap())),
            (Some(2f64), Some(Real::new(f64::consts::SQRT_2).unwrap())),
            (Some(-16f64), None),
            (Some(f64::NAN), None),
        ];
        for (input, expect) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate(ScalarFuncSig::Sqrt)
                .unwrap();
            assert_eq!(expect, output, "{:?}", input);
        }
    }

    #[test]
    fn test_radians() {
        let test_cases = vec![
            (None, None),
            (Some(0_f64), Some(Real::new(0_f64).unwrap())),
            (
                Some(180_f64),
                Some(Real::new(std::f64::consts::PI).unwrap()),
            ),
            (
                Some(-360_f64),
                Some(Real::new(-2_f64 * std::f64::consts::PI).unwrap()),
            ),
            (Some(f64::NAN), None),
            (Some(f64::INFINITY), Some(Real::new(f64::INFINITY).unwrap())),
            (
                Some(1.0E308),
                Some(Real::new(1.0E308 * (std::f64::consts::PI / 180_f64)).unwrap()),
            ),
        ];
        for (input, expect) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate(ScalarFuncSig::Radians)
                .unwrap();
            assert_eq!(expect, output, "{:?}", input);
        }
    }

    #[test]
    fn test_exp() {
        let tests = vec![
            (1_f64, std::f64::consts::E),
            (1.23_f64, 3.4212295362896734),
            (-1.23_f64, 0.2922925776808594),
            (0_f64, 1_f64),
        ];
        for (x, expected) in tests {
            let output = RpnFnScalarEvaluator::new()
                .push_param(Some(Real::new(x).unwrap()))
                .evaluate(ScalarFuncSig::Exp)
                .unwrap();
            assert_eq!(output, Some(Real::new(expected).unwrap()));
        }
        test_unary_func_ok_none::<Real, Real>(ScalarFuncSig::Exp);

        let overflow_tests = vec![100000_f64];
        for x in overflow_tests {
            let output: Result<Option<Real>> = RpnFnScalarEvaluator::new()
                .push_param(Some(Real::new(x).unwrap()))
                .evaluate(ScalarFuncSig::Exp);
            output.unwrap_err();
        }
    }

    #[test]
    fn test_degrees() {
        let tests_cases = vec![
            (None, None, false),
            (Some(f64::NAN), None, false),
            (Some(0f64), Some(Real::new(0f64).unwrap()), false),
            (
                Some(1f64),
                Some(Real::new(57.29577951308232_f64).unwrap()),
                false,
            ),
            (
                Some(std::f64::consts::PI),
                Some(Real::new(180.0_f64).unwrap()),
                false,
            ),
            (
                Some(-std::f64::consts::PI / 2.0_f64),
                Some(Real::new(-90.0_f64).unwrap()),
                false,
            ),
            (Some(1.0E307), None, true),
        ];
        for (input, expect, is_err) in tests_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate(ScalarFuncSig::Degrees);
            assert_eq!(is_err, output.is_err());
            if let Ok(out) = output {
                assert_eq!(expect, out, "{:?}", input);
            }
        }
    }

    #[test]
    fn test_sin() {
        let valid_test_cases = vec![
            (0.0_f64, 0.0_f64),
            (std::f64::consts::PI / 4.0_f64, f64::consts::FRAC_1_SQRT_2),
            (std::f64::consts::PI / 2.0_f64, 1.0_f64),
            (std::f64::consts::PI, 0.0_f64),
        ];
        for (input, expect) in valid_test_cases {
            let output: Option<Real> = RpnFnScalarEvaluator::new()
                .push_param(Some(Real::new(input).unwrap()))
                .evaluate(ScalarFuncSig::Sin)
                .unwrap();
            assert!((output.unwrap().into_inner() - expect).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn test_cos() {
        let test_cases = vec![
            (0f64, 1f64),
            (std::f64::consts::PI / 2f64, 0f64),
            (std::f64::consts::PI, -1f64),
            (-std::f64::consts::PI, -1f64),
        ];
        for (input, expect) in test_cases {
            let output: Option<Real> = RpnFnScalarEvaluator::new()
                .push_param(Some(Real::new(input).unwrap()))
                .evaluate(ScalarFuncSig::Cos)
                .unwrap();
            assert!((output.unwrap().into_inner() - expect).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn test_tan() {
        let test_cases = vec![
            (0.0_f64, 0.0_f64),
            (std::f64::consts::PI / 4.0_f64, 1.0_f64),
            (-std::f64::consts::PI / 4.0_f64, -1.0_f64),
            (std::f64::consts::PI, 0.0_f64),
            (
                (std::f64::consts::PI * 3.0) / 4.0,
                f64::tan((std::f64::consts::PI * 3.0) / 4.0), /* in mysql and rust, it equals
                                                               * -1.0000000000000002, not -1 */
            ),
        ];
        for (input, expect) in test_cases {
            let output: Option<Real> = RpnFnScalarEvaluator::new()
                .push_param(Some(Real::new(input).unwrap()))
                .evaluate(ScalarFuncSig::Tan)
                .unwrap();
            assert!((output.unwrap().into_inner() - expect).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn test_cot() {
        let test_cases = vec![
            (-1.0_f64, -0.6420926159343308_f64),
            (1.0_f64, 0.6420926159343308_f64),
            (
                std::f64::consts::PI / 4.0_f64,
                1.0_f64 / f64::tan(std::f64::consts::PI / 4.0_f64),
            ),
            (
                std::f64::consts::PI / 2.0_f64,
                1.0_f64 / f64::tan(std::f64::consts::PI / 2.0_f64),
            ),
            (
                std::f64::consts::PI,
                1.0_f64 / f64::tan(std::f64::consts::PI),
            ),
        ];
        for (input, expect) in test_cases {
            let output: Option<Real> = RpnFnScalarEvaluator::new()
                .push_param(Some(Real::new(input).unwrap()))
                .evaluate(ScalarFuncSig::Cot)
                .unwrap();
            assert!((output.unwrap().into_inner() - expect).abs() < f64::EPSILON);
        }
        RpnFnScalarEvaluator::new()
            .push_param(Some(Real::new(0.0_f64).unwrap()))
            .evaluate::<Real>(ScalarFuncSig::Cot)
            .unwrap_err();
    }

    #[test]
    fn test_pow() {
        let cases = vec![
            (
                Some(Real::new(1.0f64).unwrap()),
                Some(Real::new(3.0f64).unwrap()),
                Some(Real::new(1.0f64).unwrap()),
            ),
            (
                Some(Real::new(3.0f64).unwrap()),
                Some(Real::new(0.0f64).unwrap()),
                Some(Real::new(1.0f64).unwrap()),
            ),
            (
                Some(Real::new(2.0f64).unwrap()),
                Some(Real::new(4.0f64).unwrap()),
                Some(Real::new(16.0f64).unwrap()),
            ),
            (
                Some(Real::new(f64::INFINITY).unwrap()),
                Some(Real::new(0.0f64).unwrap()),
                Some(Real::new(1.0f64).unwrap()),
            ),
            (Some(Real::new(4.0f64).unwrap()), None, None),
            (None, Some(Real::new(4.0f64).unwrap()), None),
            (None, None, None),
        ];

        for (lhs, rhs, expect) in cases {
            let output: Option<Real> = RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate(ScalarFuncSig::Pow)
                .unwrap();
            assert_eq!(output, expect);
        }

        let invalid_cases = vec![
            (
                Some(Real::new(f64::INFINITY).unwrap()),
                Some(Real::new(f64::INFINITY).unwrap()),
            ),
            (
                Some(Real::new(0.0f64).unwrap()),
                Some(Real::new(-9999999.0f64).unwrap()),
            ),
        ];

        for (lhs, rhs) in invalid_cases {
            RpnFnScalarEvaluator::new()
                .push_param(lhs)
                .push_param(rhs)
                .evaluate::<Real>(ScalarFuncSig::Pow)
                .unwrap_err();
        }
    }

    #[test]
    fn test_rand() {
        let got1 = RpnFnScalarEvaluator::new()
            .evaluate::<Real>(ScalarFuncSig::Rand)
            .unwrap()
            .unwrap();
        let got2 = RpnFnScalarEvaluator::new()
            .evaluate::<Real>(ScalarFuncSig::Rand)
            .unwrap()
            .unwrap();

        assert!(got1 < Real::new(1.0).unwrap());
        assert!(got1 >= Real::new(0.0).unwrap());
        assert!(got2 < Real::new(1.0).unwrap());
        assert!(got2 >= Real::new(0.0).unwrap());
        assert_ne!(got1, got2);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn test_rand_with_seed_first_gen() {
        let tests: Vec<(i64, f64)> = vec![
            (0, 0.15522042769493574),
            (1, 0.40540353712197724),
            (-1, 0.9050373219931845),
            (622337, 0.3608469249315997),
            (10000000009, 0.3472714008272359),
            (-1845798578934, 0.5058874688166077),
            (922337203685, 0.40536338501178043),
            (922337203685477580, 0.5550739490939993),
            (9223372036854775807, 0.9050373219931845),
        ];

        for (seed, exp) in tests {
            let got = RpnFnScalarEvaluator::new()
                .push_param(Some(seed))
                .evaluate::<Real>(ScalarFuncSig::RandWithSeedFirstGen)
                .unwrap()
                .unwrap();
            assert_eq!(got, Real::new(exp).unwrap());
        }

        let none_case_got = RpnFnScalarEvaluator::new()
            .push_param(ScalarValue::Int(None))
            .evaluate::<Real>(ScalarFuncSig::RandWithSeedFirstGen)
            .unwrap()
            .unwrap();
        assert_eq!(none_case_got, Real::new(0.15522042769493574).unwrap());
    }

    #[test]
    fn test_asin() {
        let test_cases = vec![
            (
                Some(Real::new(0.0_f64).unwrap()),
                Some(Real::new(0.0_f64).unwrap()),
            ),
            (
                Some(Real::new(1.0_f64).unwrap()),
                Some(Real::new(std::f64::consts::PI / 2.0_f64).unwrap()),
            ),
            (
                Some(Real::new(-1.0_f64).unwrap()),
                Some(Real::new(-std::f64::consts::PI / 2.0_f64).unwrap()),
            ),
            (
                Some(Real::new(f64::consts::SQRT_2 / 2.0_f64).unwrap()),
                Some(Real::new(std::f64::consts::PI / 4.0_f64).unwrap()),
            ),
        ];
        for (input, expect) in test_cases {
            let output: Option<Real> = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate(ScalarFuncSig::Asin)
                .unwrap();
            assert!((output.unwrap() - expect.unwrap()).abs() < f64::EPSILON);
        }
        let invalid_test_cases = vec![
            (Some(Real::new(f64::INFINITY).unwrap()), None),
            (Some(Real::new(2.0_f64).unwrap()), None),
            (Some(Real::new(-2.0_f64).unwrap()), None),
        ];
        for (input, expect) in invalid_test_cases {
            let output: Option<Real> = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate(ScalarFuncSig::Asin)
                .unwrap();
            assert_eq!(expect, output);
        }
    }

    #[test]
    fn test_acos() {
        let test_cases = vec![
            (
                Some(Real::new(0.0_f64).unwrap()),
                Some(Real::new(std::f64::consts::PI / 2.0_f64).unwrap()),
            ),
            (
                Some(Real::new(1.0_f64).unwrap()),
                Some(Real::new(0.0_f64).unwrap()),
            ),
            (
                Some(Real::new(-1.0_f64).unwrap()),
                Some(Real::new(std::f64::consts::PI).unwrap()),
            ),
            (
                Some(Real::new(f64::consts::SQRT_2 / 2.0_f64).unwrap()),
                Some(Real::new(std::f64::consts::PI / 4.0_f64).unwrap()),
            ),
        ];
        for (input, expect) in test_cases {
            let output: Option<Real> = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate(ScalarFuncSig::Acos)
                .unwrap();
            assert!((output.unwrap() - expect.unwrap()).abs() < f64::EPSILON);
        }
        let invalid_test_cases = vec![
            (Some(Real::new(f64::INFINITY).unwrap()), None),
            (Some(Real::new(2.0_f64).unwrap()), None),
            (Some(Real::new(-2.0_f64).unwrap()), None),
        ];
        for (input, expect) in invalid_test_cases {
            let output: Option<Real> = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate(ScalarFuncSig::Acos)
                .unwrap();
            assert_eq!(expect, output);
        }
    }

    #[test]
    fn test_atan_1_arg() {
        let test_cases = vec![
            (
                Some(Real::new(1.0_f64).unwrap()),
                Some(Real::new(std::f64::consts::PI / 4.0_f64).unwrap()),
            ),
            (
                Some(Real::new(-1.0_f64).unwrap()),
                Some(Real::new(-std::f64::consts::PI / 4.0_f64).unwrap()),
            ),
            (
                Some(Real::new(f64::MAX).unwrap()),
                Some(Real::new(std::f64::consts::PI / 2.0_f64).unwrap()),
            ),
            (
                Some(Real::new(f64::MIN).unwrap()),
                Some(Real::new(-std::f64::consts::PI / 2.0_f64).unwrap()),
            ),
            (
                Some(Real::new(0.0_f64).unwrap()),
                Some(Real::new(0.0_f64).unwrap()),
            ),
        ];
        for (input, expect) in test_cases {
            let output: Option<Real> = RpnFnScalarEvaluator::new()
                .push_param(input)
                .evaluate(ScalarFuncSig::Atan1Arg)
                .unwrap();
            assert!((output.unwrap() - expect.unwrap()).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn test_atan_2_args() {
        let test_cases = vec![
            (
                Some(Real::new(0.0_f64).unwrap()),
                Some(Real::new(0.0_f64).unwrap()),
                Some(Real::new(0.0_f64).unwrap()),
            ),
            (
                Some(Real::new(0.0_f64).unwrap()),
                Some(Real::new(-1.0_f64).unwrap()),
                Some(Real::new(std::f64::consts::PI).unwrap()),
            ),
            (
                Some(Real::new(1.0_f64).unwrap()),
                Some(Real::new(-1.0_f64).unwrap()),
                Some(Real::new(3.0_f64 * std::f64::consts::PI / 4.0_f64).unwrap()),
            ),
            (
                Some(Real::new(-1.0_f64).unwrap()),
                Some(Real::new(1.0_f64).unwrap()),
                Some(Real::new(-std::f64::consts::PI / 4.0_f64).unwrap()),
            ),
            (
                Some(Real::new(1.0_f64).unwrap()),
                Some(Real::new(0.0_f64).unwrap()),
                Some(Real::new(std::f64::consts::PI / 2.0_f64).unwrap()),
            ),
        ];
        for (arg0, arg1, expect) in test_cases {
            let output: Option<Real> = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .push_param(arg1)
                .evaluate(ScalarFuncSig::Atan2Args)
                .unwrap();
            assert!((output.unwrap() - expect.unwrap()).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn test_conv_native_sign_binary_stages_and_overflow_cause() {
        let maximum = b"18446744073709551615";
        assert_eq!(
            conv_native(Some(maximum), Some(&10), Some(&-10)).unwrap(),
            Some(b"-1".to_vec())
        );
        assert_eq!(conv(maximum, &10, &-10).unwrap(), Some(maximum.to_vec()));
        assert_eq!(
            conv_native(Some(b"-0"), Some(&10), Some(&-10)).unwrap(),
            Some(b"0".to_vec())
        );
        assert_eq!(conv(b"-0", &10, &-10).unwrap(), Some(b"-0".to_vec()));
        assert_eq!(conv_valid_prefix_native(" +12azD", 16), "");
        assert_eq!(conv_valid_prefix_native("+12azD", 16), "12a");
        // A negative intermediate base is observable: a single signed-input
        // clamp of u64::MAX would incorrectly return i64::MAX here.
        assert_eq!(
            conv_binary_literal_native(Some(&[0xff; 8]), Some(&-10), Some(&10)).unwrap(),
            Some(maximum.to_vec())
        );
        assert_eq!(
            conv_binary_literal_native(Some(b""), Some(&2), Some(&10)).unwrap(),
            Some(b"0".to_vec())
        );
        let wide = [1, 0, 0, 0, 0, 0, 0, 0, 0];
        // First-stage NULL skips the final MIN negation, but first-stage
        // overflow precedes even an invalid final target.
        assert_eq!(
            conv_binary_literal_native(Some(&wide), Some(&37), Some(&i64::MIN)).unwrap(),
            None
        );
        let binary_error =
            conv_binary_literal_native(Some(&wide), Some(&2), Some(&37)).unwrap_err();
        let digits = format!("00{}f", "fF".repeat(9));
        let input = format!(" -{digits}z");
        let text_error = conv_native(Some(input.as_bytes()), Some(&16), Some(&10)).unwrap_err();
        for (failure, expected_digits) in [
            (text_error, digits),
            (binary_error, format!("1{}", "0".repeat(64))),
        ] {
            let tidb_query_common::error::ErrorInner::Evaluate(error) = failure.0.as_ref() else {
                panic!("CONV overflow must be an evaluation failure");
            };
            assert_eq!(error.code(), 1690);
            let EvaluateError::ConvUnsignedOverflow { digits, source } = error else {
                panic!("only actual native parse overflow gets the semantic marker");
            };
            assert_eq!(digits, &expected_digits);
            let EvaluateError::Caused(original) = source.as_ref() else {
                panic!("CONV must retain the actual parser error");
            };
            assert_eq!(
                original.downcast_ref::<ParseIntError>().unwrap().kind(),
                &IntErrorKind::PosOverflow
            );
        }
    }

    #[test]
    fn test_conv_legacy_full_i128_demand_and_base_overflow_profile() {
        use std::{hint::black_box, panic::catch_unwind};

        let ten = 10_i128.to_le_bytes();
        let negative_ten = (-10_i128).to_le_bytes();
        let outside_i64 = ((1_i128 << 64) + 10).to_le_bytes();
        assert_eq!(
            conv_legacy(
                Some(b"18446744073709551615"),
                Some(&ten),
                Some(&negative_ten)
            )
            .unwrap(),
            Some(b"-1".to_vec())
        );
        assert_eq!(
            conv_legacy(Some(b"18446744073709551616"), Some(&ten), Some(&ten)).unwrap(),
            None
        );
        assert_eq!(
            conv_legacy(Some(b"10"), Some(&outside_i64), Some(&ten)).unwrap(),
            None
        );
        assert_eq!(
            conv_legacy(Some(b"10"), Some(&ten), Some(&outside_i64)).unwrap(),
            None
        );
        // Direct-kernel malformed-width probes pin decoding order; these are
        // not assertions that the closed factory admits malformed columns.
        assert_eq!(conv_legacy(None, Some(b"bad"), Some(b"bad")).unwrap(), None);
        assert_eq!(
            conv_legacy(Some(b"1"), Some(&outside_i64), Some(b"bad")).unwrap(),
            None
        );
        let invalid_radix = 37_i128.to_le_bytes();
        assert!(conv_legacy(Some(b"1"), Some(&invalid_radix), Some(b"bad")).is_err());
        let base = black_box(i64::MIN);
        assert_eq!(conv(b"1", &base, &10).unwrap(), None);
        assert_eq!(conv_native(None, Some(&base), Some(&10)).unwrap(), None);
        let source = catch_unwind(|| {
            let mut from = base;
            if from < 0 {
                from = -from;
            }
            (2..=36).contains(&from)
        });
        let native = catch_unwind(|| conv_native(Some(b"1"), Some(&base), Some(&10)));
        match (source, native) {
            (Ok(false), Ok(Ok(None))) | (Err(_), Err(_)) => {}
            _ => panic!("native base negation must follow the actual source overflow profile"),
        }
    }

    #[test]
    fn test_conv() {
        let tests = vec![
            ("a", 16, 2, "1010"),
            ("6E", 18, 8, "172"),
            ("-17", 10, -18, "-H"),
            ("  -17", 10, -18, "-H"),
            ("-17", 10, 18, "2D3FGB0B9CG4BD1H"),
            ("+18aZ", 7, 36, "1"),
            ("  +18aZ", 7, 36, "1"),
            ("18446744073709551615", -10, 16, "7FFFFFFFFFFFFFFF"),
            ("12F", -10, 16, "C"),
            ("  FF ", 16, 10, "255"),
            ("TIDB", 10, 8, "0"),
            ("aa", 10, 2, "0"),
            (" A", -10, 16, "0"),
            ("a6a", 10, 8, "0"),
            ("16九a", 10, 8, "20"),
            ("+", 10, 8, "0"),
            ("-", 10, 8, "0"),
            ("", 2, 16, "0"),
            (
                "18446744073709551615",
                10,
                2,
                "1111111111111111111111111111111111111111111111111111111111111111",
            ),
            (
                "-18446744073709551615",
                -10,
                2,
                "1000000000000000000000000000000000000000000000000000000000000000",
            ),
        ];
        for (n, f, t, e) in tests {
            let n = Some(n.as_bytes().to_vec());
            let f = Some(f);
            let t = Some(t);
            let e = Some(e.as_bytes().to_vec());
            let got = RpnFnScalarEvaluator::new()
                .push_param(n)
                .push_param(f)
                .push_param(t)
                .evaluate(ScalarFuncSig::Conv)
                .unwrap();
            assert_eq!(got, e);
        }

        let invalid_tests = vec![
            (None, Some(10), Some(10)),
            (Some(b"111".to_vec()), None, Some(7)),
            (Some(b"112".to_vec()), Some(10), None),
            (None, None, None),
            (Some(b"222".to_vec()), Some(2), Some(100)),
            (Some(b"333".to_vec()), Some(37), Some(2)),
            (Some(b"a6a".to_vec()), Some(1), Some(8)),
        ];
        for (n, f, t) in invalid_tests {
            let got = RpnFnScalarEvaluator::new()
                .push_param(n)
                .push_param(f)
                .push_param(t)
                .evaluate::<Bytes>(ScalarFuncSig::Conv)
                .unwrap();
            assert_eq!(got, None);
        }

        let error_tests = vec![
            ("18446744073709551616", Some(10), Some(10)),
            ("100000000000000000001", Some(10), Some(8)),
            ("-18446744073709551616", Some(-10), Some(4)),
        ];
        for (n, f, t) in error_tests {
            let n = Some(n.as_bytes().to_vec());
            let got = RpnFnScalarEvaluator::new()
                .push_param(n)
                .push_param(f)
                .push_param(t)
                .evaluate::<Bytes>(ScalarFuncSig::Conv);
            got.unwrap_err();
        }
    }

    #[test]
    fn test_round_real() {
        let test_cases = vec![
            (
                Some(Real::new(-3.12_f64).unwrap()),
                Some(Real::new(-3f64).unwrap()),
            ),
            (
                Some(Real::new(-3.5_f64).unwrap()),
                Some(Real::new(-4f64).unwrap()),
            ),
            (
                Some(Real::new(-4.5_f64).unwrap()),
                Some(Real::new(-4f64).unwrap()),
            ),
            (
                Some(Real::new(f64::MAX).unwrap()),
                Some(Real::new(f64::MAX).unwrap()),
            ),
            (
                Some(Real::new(f64::MIN).unwrap()),
                Some(Real::new(f64::MIN).unwrap()),
            ),
            (None, None),
        ];

        for (arg, exp) in test_cases {
            let got = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Real>(ScalarFuncSig::RoundReal)
                .unwrap();
            assert_eq!(got, exp);
        }
    }

    #[test]
    fn test_round_int() {
        let test_cases = vec![
            (Some(1), Some(1)),
            (Some(i64::MAX), Some(i64::MAX)),
            (Some(i64::MIN), Some(i64::MIN)),
            (None, None),
        ];

        for (arg, exp) in test_cases {
            let got = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Int>(ScalarFuncSig::RoundInt)
                .unwrap();
            assert_eq!(got, exp);
        }
    }

    #[test]
    fn test_round_dec() {
        let test_cases = vec![
            (
                Some(Decimal::from_str("123.1").unwrap()),
                Some(Decimal::from_str("123.0").unwrap()),
            ),
            (
                Some(Decimal::from_str("-1111.1").unwrap()),
                Some(Decimal::from_str("-1111.0").unwrap()),
            ),
            (None, None),
        ];

        for (arg, expect_output) in test_cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate::<Decimal>(ScalarFuncSig::RoundDec)
                .unwrap();
            assert_eq!(output, expect_output);
        }
    }

    #[test]
    fn test_truncate_int() {
        let tests = vec![
            (1028_i64, 0_i64, false, 1028_i64),
            (1028, 5, false, 1028),
            (1028, -2, false, 1000),
            (1028, 309, false, 1028),
            (1028, i64::MIN, false, 0),
            (1028, u64::MAX as i64, true, 1028),
        ];
        for (lhs, rhs, rhs_is_unsigned, expected) in tests {
            let rhs_field_type = FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .flag(if rhs_is_unsigned {
                    FieldTypeFlag::UNSIGNED
                } else {
                    FieldTypeFlag::empty()
                })
                .build();

            let output = RpnFnScalarEvaluator::new()
                .push_param(Some(lhs))
                .push_param_with_field_type(Some(rhs), rhs_field_type)
                .evaluate::<Int>(ScalarFuncSig::TruncateInt)
                .unwrap();

            assert_eq!(output, Some(expected));
        }
    }

    #[test]
    fn test_truncate_uint() {
        let tests = vec![
            (
                18446744073709551615_u64,
                u64::MAX as i64,
                true,
                18446744073709551615_u64,
            ),
            (
                18446744073709551615_u64,
                -2,
                false,
                18446744073709551600_u64,
            ),
            (18446744073709551615_u64, -20, false, 0),
            (18446744073709551615_u64, 2, false, 18446744073709551615_u64),
        ];
        for (lhs, rhs, rhs_is_unsigned, expected) in tests {
            let rhs_field_type = FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .flag(if rhs_is_unsigned {
                    FieldTypeFlag::UNSIGNED
                } else {
                    FieldTypeFlag::empty()
                })
                .build();

            let output = RpnFnScalarEvaluator::new()
                .push_param(Some(lhs as Int))
                .push_param_with_field_type(Some(rhs), rhs_field_type)
                .evaluate::<Int>(ScalarFuncSig::TruncateUint)
                .unwrap();

            assert_eq!(output, Some(expected as Int));
        }
    }

    #[test]
    #[allow(clippy::excessive_precision)]
    fn test_truncate_real() {
        let test_cases = vec![
            (-1.23, 0, false, -1.0),
            (1.58, 0, false, 1.0),
            (1.298, 1, false, 1.2),
            (123.2, -1, false, 120.0),
            (123.2, 100, false, 123.2),
            (123.2, -100, false, 0.0),
            (123.2, i64::MAX, false, 123.2),
            (123.2, i64::MIN, false, 0.0),
            (123.2, u64::MAX as i64, true, 123.2),
            (-1.23, 0, false, -1.0),
            (
                1.797693134862315708145274237317043567981e+308,
                2,
                false,
                1.797693134862315708145274237317043567981e+308,
            ),
        ];
        for (lhs, rhs, rhs_is_unsigned, expected) in test_cases {
            let rhs_field_type = FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .flag(if rhs_is_unsigned {
                    FieldTypeFlag::UNSIGNED
                } else {
                    FieldTypeFlag::empty()
                })
                .build();

            let output = RpnFnScalarEvaluator::new()
                .push_param(Some(Real::new(lhs).unwrap()))
                .push_param_with_field_type(Some(rhs), rhs_field_type)
                .evaluate::<Real>(ScalarFuncSig::TruncateReal)
                .unwrap();

            assert_eq!(output, Some(Real::new(expected).unwrap()));
        }
    }

    #[test]
    fn test_truncate_decimal() {
        let tests = vec![
            (
                Decimal::from_str("-1.23").unwrap(),
                0,
                false,
                Decimal::from_str("-1").unwrap(),
            ),
            (
                Decimal::from_str("-1.23").unwrap(),
                1,
                false,
                Decimal::from_str("-1.2").unwrap(),
            ),
            (
                Decimal::from_str("-11.23").unwrap(),
                -1,
                false,
                Decimal::from_str("-10").unwrap(),
            ),
            (
                Decimal::from_str("1.58").unwrap(),
                0,
                false,
                Decimal::from_str("1").unwrap(),
            ),
            (
                Decimal::from_str("1.58").unwrap(),
                1,
                false,
                Decimal::from_str("1.5").unwrap(),
            ),
            (
                Decimal::from_str("23.298").unwrap(),
                -1,
                false,
                Decimal::from_str("20").unwrap(),
            ),
            (
                Decimal::from_str("23.298").unwrap(),
                -100,
                false,
                Decimal::from_str("0").unwrap(),
            ),
            (
                Decimal::from_str("23.298").unwrap(),
                100,
                false,
                Decimal::from_str("23.298").unwrap(),
            ),
            (
                Decimal::from_str("23.298").unwrap(),
                200,
                false,
                Decimal::from_str("23.298").unwrap(),
            ),
            (
                Decimal::from_str("23.298").unwrap(),
                -200,
                false,
                Decimal::from_str("0").unwrap(),
            ),
            (
                Decimal::from_str("23.298").unwrap(),
                u64::MAX as i64,
                true,
                Decimal::from_str("23.298").unwrap(),
            ),
            (
                Decimal::from_str("1.999999999999999999999999999999").unwrap(),
                31,
                false,
                Decimal::from_str("1.999999999999999999999999999999").unwrap(),
            ),
            (
                Decimal::from_str(
                    "99999999999999999999999999999999999999999999999999999999999999999",
                )
                .unwrap(),
                -66,
                false,
                Decimal::from_str("0").unwrap(),
            ),
            (
                Decimal::from_str(
                    "99999999999999999999999999999999999.999999999999999999999999999999",
                )
                .unwrap(),
                31,
                false,
                Decimal::from_str(
                    "99999999999999999999999999999999999.999999999999999999999999999999",
                )
                .unwrap(),
            ),
            (
                Decimal::from_str(
                    "99999999999999999999999999999999999.999999999999999999999999999999",
                )
                .unwrap(),
                -36,
                false,
                Decimal::from_str("0").unwrap(),
            ),
        ];

        for (lhs, rhs, rhs_is_unsigned, expected) in tests {
            let rhs_field_type = FieldTypeBuilder::new()
                .tp(FieldTypeTp::LongLong)
                .flag(if rhs_is_unsigned {
                    FieldTypeFlag::UNSIGNED
                } else {
                    FieldTypeFlag::empty()
                })
                .build();

            let output = RpnFnScalarEvaluator::new()
                .push_param(Some(lhs))
                .push_param_with_field_type(Some(rhs), rhs_field_type)
                .evaluate::<Decimal>(ScalarFuncSig::TruncateDecimal)
                .unwrap();

            assert_eq!(output, Some(expected));
        }
    }

    #[test]
    fn test_round_frac() {
        let int_cases = vec![
            (Some(23), Some(2), Some(23)),
            (Some(23), Some(-1), Some(20)),
            (Some(-27), Some(-1), Some(-30)),
            (Some(-27), Some(-2), Some(0)),
            (Some(-27), Some(-2), Some(0)),
            (None, Some(-27), None),
            (Some(-27), None, None),
            (None, None, None),
        ];

        for (arg0, arg1, exp) in int_cases {
            let got = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .push_param(arg1)
                .evaluate(ScalarFuncSig::RoundWithFracInt)
                .unwrap();
            assert_eq!(got, exp);
        }

        let dec_cases = vec![
            (
                Some(Decimal::from_str("150.000").unwrap()),
                Some(2),
                Some(Decimal::from_str("150.000").unwrap()),
            ),
            (
                Some(Decimal::from_str("150.257").unwrap()),
                Some(1),
                Some(Decimal::from_str("150.3").unwrap()),
            ),
            (
                Some(Decimal::from_str("153.257").unwrap()),
                Some(-1),
                Some(Decimal::from_str("150").unwrap()),
            ),
            (Some(Decimal::from_str("153.257").unwrap()), None, None),
            (None, Some(-27), None),
            (None, None, None),
        ];

        for (arg0, arg1, exp) in dec_cases {
            let got = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .push_param(arg1)
                .evaluate(ScalarFuncSig::RoundWithFracDec)
                .unwrap();
            assert_eq!(got, exp);
        }

        let real_cases = vec![
            (
                Some(Real::new(-1.298_f64).unwrap()),
                Some(1),
                Some(Real::new(-1.3_f64).unwrap()),
            ),
            (
                Some(Real::new(-1.298_f64).unwrap()),
                Some(0),
                Some(Real::new(-1.0_f64).unwrap()),
            ),
            (
                Some(Real::new(23.298_f64).unwrap()),
                Some(2),
                Some(Real::new(23.30_f64).unwrap()),
            ),
            (
                Some(Real::new(23.298_f64).unwrap()),
                Some(-1),
                Some(Real::new(20.0_f64).unwrap()),
            ),
            (
                Some(Real::new(0.95_f64).unwrap()),
                Some(1),
                Some(Real::new(1.0_f64).unwrap()),
            ),
            (
                Some(Real::new(1.05_f64).unwrap()),
                Some(1),
                Some(Real::new(1.0_f64).unwrap()),
            ),
            (
                Some(Real::new(1.05_f64).unwrap()),
                Some(1000000),
                Some(Real::new(1.05_f64).unwrap()),
            ),
            (Some(Real::new(23.298_f64).unwrap()), None, None),
            (None, Some(2), None),
            (None, None, None),
        ];

        for (arg0, arg1, exp) in real_cases {
            let got = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .push_param(arg1)
                .evaluate(ScalarFuncSig::RoundWithFracReal)
                .unwrap();
            assert_eq!(got, exp);
        }
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn test_rand_new() {
        let mut rng1 = MySqlRng::new();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut rng2 = MySqlRng::new();
        let got1 = rng1.gen();
        let got2 = rng2.gen();
        assert!(got1 < 1.0);
        assert!(got1 >= 0.0);
        assert_ne!(got1, rng1.gen());
        assert!(got2 < 1.0);
        assert!(got2 >= 0.0);
        assert_ne!(got2, rng2.gen());
        assert_ne!(got1, got2);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn test_rand_new_with_seed() {
        let tests = vec![
            (0, 0.15522042769493574, 0.620881741513388),
            (1, 0.40540353712197724, 0.8716141803857071),
            (-1, 0.9050373219931845, 0.37014932126752037),
            (9223372036854775807, 0.9050373219931845, 0.37014932126752037),
        ];
        for (seed, exp1, exp2) in tests {
            let mut rand = MySqlRng::new_with_seed(seed);
            let res1 = rand.gen();
            assert_eq!(res1, exp1);
            let res2 = rand.gen();
            assert_eq!(res2, exp2);
        }
    }
}
