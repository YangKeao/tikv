// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    any::Any,
    cmp::{Ordering, max, min},
    str,
};

use tidb_query_codegen::rpn_fn;
use tidb_query_common::Result;
use tidb_query_datatype::{
    codec::{Error, collation::Collator, data_type::*, mysql::Time},
    expr::EvalContext,
};

use crate::{
    LazyChildren, RpnFnCallExtra,
    lazy_util::{BytesElem, GenericElem, LazyValue, null_output},
};

#[rpn_fn(nullable, borrowed)]
#[inline]
pub fn compare<C: Comparer>(lhs: Option<&C::T>, rhs: Option<&C::T>) -> Result<Option<i64>> {
    C::compare(lhs, rhs)
}

#[rpn_fn(nullable)]
#[inline]
pub fn compare_json<F: CmpOp>(lhs: Option<JsonRef>, rhs: Option<JsonRef>) -> Result<Option<i64>> {
    Ok(match (lhs, rhs) {
        (None, None) => F::compare_null(),
        (None, _) | (_, None) => F::compare_partial_null(),
        (Some(lhs), Some(rhs)) => Some(F::compare_order(lhs.cmp(&rhs)) as i64),
    })
}

#[rpn_fn(nullable)]
#[inline]
pub fn compare_vector_float32<F: CmpOp>(
    lhs: Option<VectorFloat32Ref>,
    rhs: Option<VectorFloat32Ref>,
) -> Result<Option<i64>> {
    Ok(match (lhs, rhs) {
        (None, None) => F::compare_null(),
        (None, _) | (_, None) => F::compare_partial_null(),
        (Some(lhs), Some(rhs)) => Some(F::compare_order(lhs.cmp(&rhs)) as i64),
    })
}

#[rpn_fn(nullable)]
#[inline]
pub fn compare_bytes<C: Collator, F: CmpOp>(
    lhs: Option<BytesRef>,
    rhs: Option<BytesRef>,
) -> Result<Option<i64>> {
    Ok(match (lhs, rhs) {
        (None, None) => F::compare_null(),
        (None, _) | (_, None) => F::compare_partial_null(),
        (Some(lhs), Some(rhs)) => {
            let ord = C::sort_compare(lhs, rhs, false)?;
            Some(F::compare_order(ord) as i64)
        }
    })
}

pub trait Comparer {
    type T: Evaluable + EvaluableRet;

    fn compare(lhs: Option<&Self::T>, rhs: Option<&Self::T>) -> Result<Option<i64>>;
}

pub struct BasicComparer<T: Evaluable + Ord, F: CmpOp> {
    _phantom: std::marker::PhantomData<(T, F)>,
}

impl<T: Evaluable + EvaluableRet + Ord, F: CmpOp> Comparer for BasicComparer<T, F> {
    type T = T;

    #[inline]
    fn compare(lhs: Option<&T>, rhs: Option<&T>) -> Result<Option<i64>> {
        Ok(match (lhs, rhs) {
            (None, None) => F::compare_null(),
            (None, _) | (_, None) => F::compare_partial_null(),
            (Some(lhs), Some(rhs)) => Some(F::compare_order(lhs.cmp(rhs)) as i64),
        })
    }
}

pub struct UintUintComparer<F: CmpOp> {
    _phantom: std::marker::PhantomData<F>,
}

impl<F: CmpOp> Comparer for UintUintComparer<F> {
    type T = Int;

    #[inline]
    fn compare(lhs: Option<&Int>, rhs: Option<&Int>) -> Result<Option<i64>> {
        Ok(match (lhs, rhs) {
            (None, None) => F::compare_null(),
            (None, _) | (_, None) => F::compare_partial_null(),
            (Some(lhs), Some(rhs)) => {
                let lhs = *lhs as u64;
                let rhs = *rhs as u64;
                Some(F::compare_order(lhs.cmp(&rhs)) as i64)
            }
        })
    }
}

pub struct UintIntComparer<F: CmpOp> {
    _phantom: std::marker::PhantomData<F>,
}

impl<F: CmpOp> Comparer for UintIntComparer<F> {
    type T = Int;

    #[inline]
    fn compare(lhs: Option<&Int>, rhs: Option<&Int>) -> Result<Option<i64>> {
        Ok(match (lhs, rhs) {
            (None, None) => F::compare_null(),
            (None, _) | (_, None) => F::compare_partial_null(),
            (Some(lhs), Some(rhs)) => {
                let ordering = if *rhs < 0 || *lhs as u64 > i64::MAX as u64 {
                    Ordering::Greater
                } else {
                    lhs.cmp(rhs)
                };
                Some(F::compare_order(ordering) as i64)
            }
        })
    }
}

pub struct IntUintComparer<F: CmpOp> {
    _phantom: std::marker::PhantomData<F>,
}

impl<F: CmpOp> Comparer for IntUintComparer<F> {
    type T = Int;

    #[inline]
    fn compare(lhs: Option<&Int>, rhs: Option<&Int>) -> Result<Option<i64>> {
        Ok(match (lhs, rhs) {
            (None, None) => F::compare_null(),
            (None, _) | (_, None) => F::compare_partial_null(),
            (Some(lhs), Some(rhs)) => {
                let ordering = if *lhs < 0 || *rhs as u64 > i64::MAX as u64 {
                    Ordering::Less
                } else {
                    lhs.cmp(rhs)
                };
                Some(F::compare_order(ordering) as i64)
            }
        })
    }
}

pub trait CmpOp {
    #[inline]
    fn compare_null() -> Option<i64> {
        None
    }

    #[inline]
    fn compare_partial_null() -> Option<i64> {
        None
    }

    fn compare_order(ordering: std::cmp::Ordering) -> bool;
}

pub struct CmpOpLt;

impl CmpOp for CmpOpLt {
    #[inline]
    fn compare_order(ordering: Ordering) -> bool {
        ordering == Ordering::Less
    }
}

pub struct CmpOpLe;

impl CmpOp for CmpOpLe {
    #[inline]
    fn compare_order(ordering: Ordering) -> bool {
        ordering != Ordering::Greater
    }
}

pub struct CmpOpGt;

impl CmpOp for CmpOpGt {
    #[inline]
    fn compare_order(ordering: Ordering) -> bool {
        ordering == Ordering::Greater
    }
}

pub struct CmpOpGe;

impl CmpOp for CmpOpGe {
    #[inline]
    fn compare_order(ordering: Ordering) -> bool {
        ordering != Ordering::Less
    }
}

pub struct CmpOpNe;

impl CmpOp for CmpOpNe {
    #[inline]
    fn compare_order(ordering: Ordering) -> bool {
        ordering != Ordering::Equal
    }
}

pub struct CmpOpEq;

impl CmpOp for CmpOpEq {
    #[inline]
    fn compare_order(ordering: Ordering) -> bool {
        ordering == Ordering::Equal
    }
}

pub struct CmpOpNullEq;

impl CmpOp for CmpOpNullEq {
    #[inline]
    fn compare_null() -> Option<i64> {
        Some(1)
    }

    #[inline]
    fn compare_partial_null() -> Option<i64> {
        Some(0)
    }

    #[inline]
    fn compare_order(ordering: Ordering) -> bool {
        ordering == Ordering::Equal
    }
}

#[rpn_fn(nullable, varg)]
#[inline]
pub fn coalesce<T: Evaluable + EvaluableRet>(args: &[Option<&T>]) -> Result<Option<T>> {
    for arg in args {
        if arg.is_some() {
            return Ok(arg.cloned());
        }
    }
    Ok(None)
}

#[rpn_fn(nullable, varg)]
#[inline]
pub fn coalesce_bytes(args: &[Option<BytesRef>]) -> Result<Option<Bytes>> {
    for arg in args {
        if arg.is_some() {
            return Ok(arg.map(|x| x.to_vec()));
        }
    }
    Ok(None)
}

#[rpn_fn(nullable, varg)]
#[inline]
pub fn coalesce_json(args: &[Option<JsonRef>]) -> Result<Option<Json>> {
    for arg in args {
        if arg.is_some() {
            return Ok(arg.map(|x| x.to_owned()));
        }
    }
    Ok(None)
}

#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn greatest_int(args: &[Option<&Int>]) -> Result<Option<Int>> {
    do_get_extremum(args, max)
}

#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn least_int(args: &[Option<&Int>]) -> Result<Option<Int>> {
    do_get_extremum(args, min)
}

#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn interval_int(args: &[Option<&Int>]) -> Result<Option<Int>> {
    let target = match args[0] {
        None => return Ok(Some(-1)),
        Some(v) => Some(v),
    };

    let arr = &args[1..];

    match arr.binary_search(&target) {
        Ok(pos) => Ok(Some(pos as Int + 1)),
        Err(pos) => Ok(Some(pos as Int)),
    }
}

#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn greatest_decimal(args: &[Option<&Decimal>]) -> Result<Option<Decimal>> {
    do_get_extremum(args, max)
}

#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn least_decimal(args: &[Option<&Decimal>]) -> Result<Option<Decimal>> {
    do_get_extremum(args, min)
}

#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn greatest_string(args: &[Option<BytesRef>]) -> Result<Option<Bytes>> {
    do_get_extremum(args, max)
}

#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn least_string(args: &[Option<BytesRef>]) -> Result<Option<Bytes>> {
    do_get_extremum(args, min)
}

#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn greatest_real(args: &[Option<&Real>]) -> Result<Option<Real>> {
    do_get_extremum(args, max)
}

#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn least_real(args: &[Option<&Real>]) -> Result<Option<Real>> {
    do_get_extremum(args, min)
}

#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn interval_real(args: &[Option<&Real>]) -> Result<Option<Int>> {
    let target = match args[0] {
        None => return Ok(Some(-1)),
        Some(v) => Some(v),
    };

    let arr = &args[1..];

    match arr.binary_search(&target) {
        Ok(pos) => Ok(Some(pos as Int + 1)),
        Err(pos) => Ok(Some(pos as Int)),
    }
}

#[rpn_fn(nullable, varg, min_args = 2, capture = [ctx])]
#[inline]
pub fn greatest_cmp_string_as_time(
    ctx: &mut EvalContext,
    args: &[Option<BytesRef>],
) -> Result<Option<Bytes>> {
    let mut greatest = None;
    for arg in args {
        match arg {
            Some(arg_val) => {
                let s = match str::from_utf8(arg_val) {
                    Ok(s) => s,
                    Err(err) => {
                        return ctx
                            .handle_invalid_time_error(Error::Encoding(err))
                            .map(|_| Ok(None))?;
                    }
                };
                match Time::parse_datetime(ctx, s, Time::parse_fsp(s), true) {
                    Ok(t) => greatest = max(greatest, Some(t)),
                    Err(_) => {
                        return ctx
                            .handle_invalid_time_error(Error::invalid_time_format(s))
                            .map(|_| Ok(None))?;
                    }
                }
            }
            None => {
                return Ok(None);
            }
        }
    }

    Ok(greatest.map(|time| time.to_string().into_bytes()))
}

#[rpn_fn(nullable, varg, min_args = 2, capture = [ctx])]
#[inline]
pub fn least_cmp_string_as_time(
    ctx: &mut EvalContext,
    args: &[Option<BytesRef>],
) -> Result<Option<Bytes>> {
    // Max datetime range defined at https://dev.mysql.com/doc/refman/8.0/en/datetime.html
    let mut least = Some(Time::parse_datetime(ctx, "9999-12-31 23:59:59", 0, true)?);
    for arg in args {
        match arg {
            Some(arg_val) => {
                let s = match str::from_utf8(arg_val) {
                    Ok(s) => s,
                    Err(err) => {
                        return ctx
                            .handle_invalid_time_error(Error::Encoding(err))
                            .map(|_| Ok(None))?;
                    }
                };
                match Time::parse_datetime(ctx, s, Time::parse_fsp(s), true) {
                    Ok(t) => least = min(least, Some(t)),
                    Err(_) => {
                        return ctx
                            .handle_invalid_time_error(Error::invalid_time_format(s))
                            .map(|_| Ok(None))?;
                    }
                }
            }
            None => {
                return Ok(None);
            }
        }
    }

    Ok(least.map(|time| time.to_string().into_bytes()))
}

#[rpn_fn(nullable, varg, min_args = 2, capture = [ctx])]
#[inline]
pub fn greatest_cmp_string_as_date(
    ctx: &mut EvalContext,
    args: &[Option<BytesRef>],
) -> Result<Option<Bytes>> {
    let mut greatest = None;
    for arg in args {
        match arg {
            Some(arg_val) => {
                let s = match str::from_utf8(arg_val) {
                    Ok(s) => s,
                    Err(err) => {
                        return ctx
                            .handle_invalid_time_error(Error::Encoding(err))
                            .map(|_| Ok(None))?;
                    }
                };
                match Time::parse_date(ctx, s) {
                    Ok(t) => greatest = max(greatest, Some(t)),
                    Err(_) => {
                        return ctx
                            .handle_invalid_time_error(Error::invalid_time_format(s))
                            .map(|_| Ok(None))?;
                    }
                }
            }
            None => {
                return Ok(None);
            }
        }
    }

    Ok(greatest.map(|time| time.to_string().into_bytes()))
}

#[rpn_fn(nullable, varg, min_args = 2, capture = [ctx])]
#[inline]
pub fn least_cmp_string_as_date(
    ctx: &mut EvalContext,
    args: &[Option<BytesRef>],
) -> Result<Option<Bytes>> {
    // Max date range defined at https://dev.mysql.com/doc/refman/8.0/en/datetime.html
    let mut least = Some(Time::parse_date(ctx, "9999-12-31")?);
    for arg in args {
        match arg {
            Some(arg_val) => {
                let s = match str::from_utf8(arg_val) {
                    Ok(s) => s,
                    Err(err) => {
                        return ctx
                            .handle_invalid_time_error(Error::Encoding(err))
                            .map(|_| Ok(None))?;
                    }
                };
                match Time::parse_date(ctx, s) {
                    Ok(t) => least = min(least, Some(t)),
                    Err(_) => {
                        return ctx
                            .handle_invalid_time_error(Error::invalid_time_format(s))
                            .map(|_| Ok(None))?;
                    }
                }
            }
            None => {
                return Ok(None);
            }
        }
    }

    Ok(least.map(|time| time.to_string().into_bytes()))
}

#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn greatest_datetime(args: &[Option<&DateTime>]) -> Result<Option<DateTime>> {
    do_get_extremum(args, max)
}

#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn least_datetime(args: &[Option<&DateTime>]) -> Result<Option<DateTime>> {
    do_get_extremum(args, min)
}

#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn greatest_duration(args: &[Option<&Duration>]) -> Result<Option<Duration>> {
    do_get_extremum(args, max)
}

#[rpn_fn(nullable, varg, min_args = 2)]
#[inline]
pub fn least_duration(args: &[Option<&Duration>]) -> Result<Option<Duration>> {
    do_get_extremum(args, min)
}

#[inline]
fn do_get_extremum<'a, T>(
    args: &[Option<&'a T>],
    chooser: fn(&'a T, &'a T) -> &'a T,
) -> Result<Option<T::Owned>>
where
    T: Ord + ToOwned + ?Sized,
{
    let first = args[0];
    match first {
        None => Ok(None),
        Some(first_val) => {
            let mut res = first_val;
            for arg in &args[1..] {
                match arg {
                    None => {
                        return Ok(None);
                    }
                    Some(v) => {
                        res = chooser(res, *v);
                    }
                }
            }
            Ok(Some(res.to_owned()))
        }
    }
}

/// Lazy `GREATEST`/`LEAST` for the `Ord` element types (`Int`, `Real`,
/// `Decimal`, `DateTime`, `Duration`, `Bytes`).
///
/// Child 0 is evaluated for every row; each later child is requested only for
/// the rows that have not seen a NULL yet, so a row stops at its first NULL
/// argument and never enters a later child's subtree. Go's
/// `builtinGreatest*Sig.eval*` / `builtinLeast*Sig.eval*`
/// (`pkg/expression/builtin_compare.go`) return as soon as an argument is NULL.
fn lazy_extremum_impl<T: LazyValue>(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    greatest: bool,
) -> Result<VectorValue>
where
    T::Value: Ord,
{
    let child_count = children.len();
    let all_rows: Vec<usize> = (0..output_rows).collect();
    let first = children.eval(ctx, 0, &all_rows)?;

    let mut output = null_output::<T::Value>(output_rows);
    // `extremes[row]` is the running extremum. A row whose entry is `None`
    // either has not been initialized (greatest) or has already seen a NULL
    // argument; only rows that saw a non-NULL argument stay `active`.
    let mut extremes: Vec<Option<T::Value>> =
        (0..output_rows).map(|row| T::read(&first, row)).collect();
    let mut active: Vec<usize> = (0..output_rows)
        .filter(|&row| extremes[row].is_some())
        .collect();

    let mut arg = 1;
    while arg < child_count && !active.is_empty() {
        let values = children.eval(ctx, arg, &active)?;
        let mut still = Vec::new();
        for (position, &row) in active.iter().enumerate() {
            match T::read(&values, position) {
                None => extremes[row] = None,
                Some(value) => {
                    match extremes[row].take() {
                        None => extremes[row] = Some(value),
                        Some(current) => {
                            let take = if greatest {
                                value > current
                            } else {
                                value < current
                            };
                            extremes[row] = Some(if take { value } else { current });
                        }
                    }
                    still.push(row);
                }
            }
        }
        active = still;
        arg += 1;
    }

    for &row in &active {
        output[row] = extremes[row].take();
    }
    Ok(T::build(output))
}

pub fn lazy_greatest<T>(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue>
where
    T: Evaluable + EvaluableRet + Ord,
{
    lazy_extremum_impl::<GenericElem<T>>(ctx, output_rows, children, true)
}

pub fn lazy_least<T>(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue>
where
    T: Evaluable + EvaluableRet + Ord,
{
    lazy_extremum_impl::<GenericElem<T>>(ctx, output_rows, children, false)
}

pub fn lazy_greatest_bytes(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    lazy_extremum_impl::<BytesElem>(ctx, output_rows, children, true)
}

pub fn lazy_least_bytes(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    lazy_extremum_impl::<BytesElem>(ctx, output_rows, children, false)
}

/// Converts one `Bytes` element for the `CmpStringAsDate`/`CmpStringAsTime`
/// extremum, mirroring the eager kernels' `str::from_utf8` + `parse_date` /
/// `parse_datetime` steps. A failure goes through
/// `EvalContext::handle_invalid_time_error`, so a non-strict context records a
/// warning and yields `None` (NULL) while a strict one aborts the batch.
fn parse_cmp_string_as_time(
    ctx: &mut EvalContext,
    value: &[u8],
    as_date: bool,
) -> Result<Option<Time>> {
    let text = match str::from_utf8(value) {
        Ok(text) => text,
        Err(err) => {
            ctx.handle_invalid_time_error(Error::Encoding(err))?;
            return Ok(None);
        }
    };
    let parsed = if as_date {
        Time::parse_date(ctx, text)
    } else {
        Time::parse_datetime(ctx, text, Time::parse_fsp(text), true)
    };
    match parsed {
        Ok(time) => Ok(Some(time)),
        Err(_) => {
            ctx.handle_invalid_time_error(Error::invalid_time_format(text))?;
            Ok(None)
        }
    }
}

/// Lazy `GREATEST`/`LEAST` for the `CmpStringAsDate`/`CmpStringAsTime`
/// signatures. As in the eager kernels, every element is parsed to a `Time`,
/// compared as a `Time`, and the winner is rendered with `Time::to_string`.
fn lazy_cmp_string_extremum_impl(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    greatest: bool,
    as_date: bool,
) -> Result<VectorValue> {
    let child_count = children.len();
    // Go initializes the running LEAST value to the greatest representable
    // time so the first parsed argument always replaces it; GREATEST starts
    // uninitialized. TiKV's eager `least_cmp_string_as_*` does the same.
    let initial = if greatest {
        None
    } else if as_date {
        Some(Time::parse_date(ctx, "9999-12-31")?)
    } else {
        Some(Time::parse_datetime(ctx, "9999-12-31 23:59:59", 0, true)?)
    };

    let mut output = null_output::<Bytes>(output_rows);
    let mut extremes: Vec<Option<Time>> = vec![initial; output_rows];
    let mut active: Vec<usize> = (0..output_rows).collect();

    let mut arg = 0;
    while arg < child_count && !active.is_empty() {
        let values = children.eval(ctx, arg, &active)?;
        let mut still = Vec::new();
        for (position, &row) in active.iter().enumerate() {
            let parsed = match BytesElem::read(&values, position) {
                None => None,
                Some(bytes) => parse_cmp_string_as_time(ctx, &bytes, as_date)?,
            };
            match parsed {
                // A NULL or unparsable argument yields NULL and drops the row
                // before any later argument is requested for it.
                None => extremes[row] = None,
                Some(time) => {
                    match extremes[row].take() {
                        None => extremes[row] = Some(time),
                        Some(current) => {
                            let take = if greatest {
                                time > current
                            } else {
                                time < current
                            };
                            extremes[row] = Some(if take { time } else { current });
                        }
                    }
                    still.push(row);
                }
            }
        }
        active = still;
        arg += 1;
    }

    for &row in &active {
        output[row] = extremes[row]
            .take()
            .map(|time| time.to_string().into_bytes());
    }
    Ok(BytesElem::build(output))
}

pub fn lazy_greatest_cmp_string_as_time(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    lazy_cmp_string_extremum_impl(ctx, output_rows, children, true, false)
}

pub fn lazy_least_cmp_string_as_time(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    lazy_cmp_string_extremum_impl(ctx, output_rows, children, false, false)
}

pub fn lazy_greatest_cmp_string_as_date(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    lazy_cmp_string_extremum_impl(ctx, output_rows, children, true, true)
}

pub fn lazy_least_cmp_string_as_date(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    lazy_cmp_string_extremum_impl(ctx, output_rows, children, false, true)
}

/// Lazy `INTERVAL(target, boundary0, ...)`.
///
/// Go's `builtinInterval*Sig.evalInt` (`pkg/expression/builtin_compare.go`)
/// returns `-1` for a NULL target without touching `args[1..]`. The lazy
/// kernel evaluates child 0 for every row, defaults each NULL target to `-1`,
/// and requests the boundary children only over the rows that actually have a
/// target; a batch whose targets are all NULL never enters them. For rows with
/// a target it reproduces the eager binary search over `args[1..]` exactly
/// (including NULL boundaries, which order below `Some`).
fn lazy_interval_impl<T: LazyValue>(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
) -> Result<VectorValue>
where
    T::Value: Ord,
{
    let child_count = children.len();
    let all_rows: Vec<usize> = (0..output_rows).collect();
    let target = children.eval(ctx, 0, &all_rows)?;
    let targets: Vec<Option<T::Value>> =
        (0..output_rows).map(|row| T::read(&target, row)).collect();

    let mut output: Vec<Option<Int>> = vec![Some(-1); output_rows];
    let active: Vec<usize> = (0..output_rows)
        .filter(|&row| targets[row].is_some())
        .collect();

    if !active.is_empty() {
        let mut columns: Vec<VectorValue> = Vec::with_capacity(child_count.saturating_sub(1));
        for arg in 1..child_count {
            columns.push(children.eval(ctx, arg, &active)?);
        }
        for (position, &row) in active.iter().enumerate() {
            let boundaries: Vec<Option<T::Value>> = columns
                .iter()
                .map(|column| T::read(column, position))
                .collect();
            output[row] = Some(match boundaries.binary_search(&targets[row]) {
                Ok(found) => found as i64 + 1,
                Err(insert) => insert as i64,
            });
        }
    }

    Ok(GenericElem::<Int>::build(output))
}

pub fn lazy_interval_int(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    lazy_interval_impl::<GenericElem<Int>>(ctx, output_rows, children)
}

pub fn lazy_interval_real(
    ctx: &mut EvalContext,
    output_rows: usize,
    children: &mut dyn LazyChildren<'_>,
    _extra: &mut RpnFnCallExtra<'_>,
    _metadata: &(dyn Any + Send + Sync),
) -> Result<VectorValue> {
    lazy_interval_impl::<GenericElem<Real>>(ctx, output_rows, children)
}

#[cfg(test)]
mod tests {
    use tidb_query_datatype::{Collation, FieldTypeFlag, FieldTypeTp, builder::FieldTypeBuilder};
    use tipb::ScalarFuncSig;

    use super::*;
    use crate::test_util::RpnFnScalarEvaluator;

    #[derive(Clone, Copy, PartialEq)]
    enum TestCaseCmpOp {
        Gt,
        Ge,
        Lt,
        Le,
        Eq,
        Ne,
        NullEq,
    }

    #[allow(clippy::type_complexity)]
    fn generate_numeric_compare_cases()
    -> Vec<(Option<Real>, Option<Real>, TestCaseCmpOp, Option<i64>)> {
        vec![
            (None, None, TestCaseCmpOp::Gt, None),
            (Real::new(3.5).ok(), None, TestCaseCmpOp::Gt, None),
            (Real::new(-2.1).ok(), None, TestCaseCmpOp::Gt, None),
            (None, Real::new(3.5).ok(), TestCaseCmpOp::Gt, None),
            (None, Real::new(-2.1).ok(), TestCaseCmpOp::Gt, None),
            (
                Real::new(3.5).ok(),
                Real::new(-2.1).ok(),
                TestCaseCmpOp::Gt,
                Some(1),
            ),
            (
                Real::new(-2.1).ok(),
                Real::new(3.5).ok(),
                TestCaseCmpOp::Gt,
                Some(0),
            ),
            (
                Real::new(3.5).ok(),
                Real::new(3.5).ok(),
                TestCaseCmpOp::Gt,
                Some(0),
            ),
            (
                Real::new(-2.1).ok(),
                Real::new(-2.1).ok(),
                TestCaseCmpOp::Gt,
                Some(0),
            ),
            (None, None, TestCaseCmpOp::Ge, None),
            (Real::new(3.5).ok(), None, TestCaseCmpOp::Ge, None),
            (Real::new(-2.1).ok(), None, TestCaseCmpOp::Ge, None),
            (None, Real::new(3.5).ok(), TestCaseCmpOp::Ge, None),
            (None, Real::new(-2.1).ok(), TestCaseCmpOp::Ge, None),
            (
                Real::new(3.5).ok(),
                Real::new(-2.1).ok(),
                TestCaseCmpOp::Ge,
                Some(1),
            ),
            (
                Real::new(-2.1).ok(),
                Real::new(3.5).ok(),
                TestCaseCmpOp::Ge,
                Some(0),
            ),
            (
                Real::new(3.5).ok(),
                Real::new(3.5).ok(),
                TestCaseCmpOp::Ge,
                Some(1),
            ),
            (
                Real::new(-2.1).ok(),
                Real::new(-2.1).ok(),
                TestCaseCmpOp::Ge,
                Some(1),
            ),
            (None, None, TestCaseCmpOp::Lt, None),
            (Real::new(3.5).ok(), None, TestCaseCmpOp::Lt, None),
            (Real::new(-2.1).ok(), None, TestCaseCmpOp::Lt, None),
            (None, Real::new(3.5).ok(), TestCaseCmpOp::Lt, None),
            (None, Real::new(-2.1).ok(), TestCaseCmpOp::Lt, None),
            (
                Real::new(3.5).ok(),
                Real::new(-2.1).ok(),
                TestCaseCmpOp::Lt,
                Some(0),
            ),
            (
                Real::new(-2.1).ok(),
                Real::new(3.5).ok(),
                TestCaseCmpOp::Lt,
                Some(1),
            ),
            (
                Real::new(3.5).ok(),
                Real::new(3.5).ok(),
                TestCaseCmpOp::Lt,
                Some(0),
            ),
            (
                Real::new(-2.1).ok(),
                Real::new(-2.1).ok(),
                TestCaseCmpOp::Lt,
                Some(0),
            ),
            (None, None, TestCaseCmpOp::Le, None),
            (Real::new(3.5).ok(), None, TestCaseCmpOp::Le, None),
            (Real::new(-2.1).ok(), None, TestCaseCmpOp::Le, None),
            (None, Real::new(3.5).ok(), TestCaseCmpOp::Le, None),
            (None, Real::new(-2.1).ok(), TestCaseCmpOp::Le, None),
            (
                Real::new(3.5).ok(),
                Real::new(-2.1).ok(),
                TestCaseCmpOp::Le,
                Some(0),
            ),
            (
                Real::new(-2.1).ok(),
                Real::new(3.5).ok(),
                TestCaseCmpOp::Le,
                Some(1),
            ),
            (
                Real::new(3.5).ok(),
                Real::new(3.5).ok(),
                TestCaseCmpOp::Le,
                Some(1),
            ),
            (
                Real::new(-2.1).ok(),
                Real::new(-2.1).ok(),
                TestCaseCmpOp::Le,
                Some(1),
            ),
            (None, None, TestCaseCmpOp::Eq, None),
            (Real::new(3.5).ok(), None, TestCaseCmpOp::Eq, None),
            (Real::new(-2.1).ok(), None, TestCaseCmpOp::Eq, None),
            (None, Real::new(3.5).ok(), TestCaseCmpOp::Eq, None),
            (None, Real::new(-2.1).ok(), TestCaseCmpOp::Eq, None),
            (
                Real::new(3.5).ok(),
                Real::new(-2.1).ok(),
                TestCaseCmpOp::Eq,
                Some(0),
            ),
            (
                Real::new(-2.1).ok(),
                Real::new(3.5).ok(),
                TestCaseCmpOp::Eq,
                Some(0),
            ),
            (
                Real::new(3.5).ok(),
                Real::new(3.5).ok(),
                TestCaseCmpOp::Eq,
                Some(1),
            ),
            (
                Real::new(-2.1).ok(),
                Real::new(-2.1).ok(),
                TestCaseCmpOp::Eq,
                Some(1),
            ),
            (None, None, TestCaseCmpOp::Ne, None),
            (Real::new(3.5).ok(), None, TestCaseCmpOp::Ne, None),
            (Real::new(-2.1).ok(), None, TestCaseCmpOp::Ne, None),
            (None, Real::new(3.5).ok(), TestCaseCmpOp::Ne, None),
            (None, Real::new(-2.1).ok(), TestCaseCmpOp::Ne, None),
            (
                Real::new(3.5).ok(),
                Real::new(-2.1).ok(),
                TestCaseCmpOp::Ne,
                Some(1),
            ),
            (
                Real::new(-2.1).ok(),
                Real::new(3.5).ok(),
                TestCaseCmpOp::Ne,
                Some(1),
            ),
            (
                Real::new(3.5).ok(),
                Real::new(3.5).ok(),
                TestCaseCmpOp::Ne,
                Some(0),
            ),
            (
                Real::new(-2.1).ok(),
                Real::new(-2.1).ok(),
                TestCaseCmpOp::Ne,
                Some(0),
            ),
            (None, None, TestCaseCmpOp::NullEq, Some(1)),
            (Real::new(3.5).ok(), None, TestCaseCmpOp::NullEq, Some(0)),
            (Real::new(-2.1).ok(), None, TestCaseCmpOp::NullEq, Some(0)),
            (None, Real::new(3.5).ok(), TestCaseCmpOp::NullEq, Some(0)),
            (None, Real::new(-2.1).ok(), TestCaseCmpOp::NullEq, Some(0)),
            (
                Real::new(3.5).ok(),
                Real::new(-2.1).ok(),
                TestCaseCmpOp::NullEq,
                Some(0),
            ),
            (
                Real::new(-2.1).ok(),
                Real::new(3.5).ok(),
                TestCaseCmpOp::NullEq,
                Some(0),
            ),
            (
                Real::new(3.5).ok(),
                Real::new(3.5).ok(),
                TestCaseCmpOp::NullEq,
                Some(1),
            ),
            (
                Real::new(-2.1).ok(),
                Real::new(-2.1).ok(),
                TestCaseCmpOp::NullEq,
                Some(1),
            ),
        ]
    }

    #[test]
    fn test_compare_real() {
        for (arg0, arg1, cmp_op, expect_output) in generate_numeric_compare_cases() {
            let sig = match cmp_op {
                TestCaseCmpOp::Gt => ScalarFuncSig::GtReal,
                TestCaseCmpOp::Ge => ScalarFuncSig::GeReal,
                TestCaseCmpOp::Lt => ScalarFuncSig::LtReal,
                TestCaseCmpOp::Le => ScalarFuncSig::LeReal,
                TestCaseCmpOp::Eq => ScalarFuncSig::EqReal,
                TestCaseCmpOp::Ne => ScalarFuncSig::NeReal,
                TestCaseCmpOp::NullEq => ScalarFuncSig::NullEqReal,
            };
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .push_param(arg1)
                .evaluate(sig)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}, {:?}, {:?}", arg0, arg1, sig);
        }
    }

    #[test]
    fn test_compare_duration() {
        fn map_double_to_duration(v: Real) -> Duration {
            Duration::from_millis((v.into_inner() * 1000.0) as i64, 4).unwrap()
        }

        for (arg0, arg1, cmp_op, expect_output) in generate_numeric_compare_cases() {
            let sig = match cmp_op {
                TestCaseCmpOp::Gt => ScalarFuncSig::GtDuration,
                TestCaseCmpOp::Ge => ScalarFuncSig::GeDuration,
                TestCaseCmpOp::Lt => ScalarFuncSig::LtDuration,
                TestCaseCmpOp::Le => ScalarFuncSig::LeDuration,
                TestCaseCmpOp::Eq => ScalarFuncSig::EqDuration,
                TestCaseCmpOp::Ne => ScalarFuncSig::NeDuration,
                TestCaseCmpOp::NullEq => ScalarFuncSig::NullEqDuration,
            };
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg0.map(map_double_to_duration))
                .push_param(arg1.map(map_double_to_duration))
                .evaluate(sig)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}, {:?}, {:?}", arg0, arg1, sig);
        }
    }

    #[test]
    fn test_compare_decimal() {
        use tidb_query_datatype::{codec::convert::ConvertTo, expr::EvalContext};
        fn f64_to_decimal(ctx: &mut EvalContext, f: f64) -> Result<Decimal> {
            let val = f.convert(ctx)?;
            Ok(val)
        }
        let mut ctx = EvalContext::default();
        for (arg0, arg1, cmp_op, expect_output) in generate_numeric_compare_cases() {
            let sig = match cmp_op {
                TestCaseCmpOp::Gt => ScalarFuncSig::GtDecimal,
                TestCaseCmpOp::Ge => ScalarFuncSig::GeDecimal,
                TestCaseCmpOp::Lt => ScalarFuncSig::LtDecimal,
                TestCaseCmpOp::Le => ScalarFuncSig::LeDecimal,
                TestCaseCmpOp::Eq => ScalarFuncSig::EqDecimal,
                TestCaseCmpOp::Ne => ScalarFuncSig::NeDecimal,
                TestCaseCmpOp::NullEq => ScalarFuncSig::NullEqDecimal,
            };
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg0.map(|v| f64_to_decimal(&mut ctx, v.into_inner()).unwrap()))
                .push_param(arg1.map(|v| f64_to_decimal(&mut ctx, v.into_inner()).unwrap()))
                .evaluate(sig)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}, {:?}, {:?}", arg0, arg1, sig);
        }
    }

    #[test]
    fn test_compare_signed_int() {
        for (arg0, arg1, cmp_op, expect_output) in generate_numeric_compare_cases() {
            let sig = match cmp_op {
                TestCaseCmpOp::Gt => ScalarFuncSig::GtInt,
                TestCaseCmpOp::Ge => ScalarFuncSig::GeInt,
                TestCaseCmpOp::Lt => ScalarFuncSig::LtInt,
                TestCaseCmpOp::Le => ScalarFuncSig::LeInt,
                TestCaseCmpOp::Eq => ScalarFuncSig::EqInt,
                TestCaseCmpOp::Ne => ScalarFuncSig::NeInt,
                TestCaseCmpOp::NullEq => ScalarFuncSig::NullEqInt,
            };
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg0.map(|v| v.into_inner() as i64))
                .push_param(arg1.map(|v| v.into_inner() as i64))
                .evaluate(sig)
                .unwrap();
            assert_eq!(output, expect_output, "{:?}, {:?}, {:?}", arg0, arg1, sig);
        }
    }

    #[test]
    fn test_compare_int_2() {
        let test_cases = vec![
            (Some(5), false, Some(3), false, Ordering::Greater),
            (Some(u64::MAX as i64), false, Some(5), false, Ordering::Less),
            (
                Some(u64::MAX as i64),
                true,
                Some((u64::MAX - 1) as i64),
                true,
                Ordering::Greater,
            ),
            (
                Some(u64::MAX as i64),
                true,
                Some(5),
                true,
                Ordering::Greater,
            ),
            (Some(5), true, Some(i64::MIN), false, Ordering::Greater),
            (
                Some(u64::MAX as i64),
                true,
                Some(i64::MIN),
                false,
                Ordering::Greater,
            ),
            (Some(5), true, Some(3), false, Ordering::Greater),
            (Some(i64::MIN), false, Some(3), true, Ordering::Less),
            (Some(5), false, Some(u64::MAX as i64), true, Ordering::Less),
            (Some(5), false, Some(3), true, Ordering::Greater),
        ];
        for (lhs, lhs_is_unsigned, rhs, rhs_is_unsigned, ordering) in test_cases {
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

            for (sig, accept_orderings) in &[
                (ScalarFuncSig::EqInt, vec![Ordering::Equal]),
                (
                    ScalarFuncSig::NeInt,
                    vec![Ordering::Greater, Ordering::Less],
                ),
                (ScalarFuncSig::GtInt, vec![Ordering::Greater]),
                (
                    ScalarFuncSig::GeInt,
                    vec![Ordering::Greater, Ordering::Equal],
                ),
                (ScalarFuncSig::LtInt, vec![Ordering::Less]),
                (ScalarFuncSig::LeInt, vec![Ordering::Less, Ordering::Equal]),
            ] {
                let output = RpnFnScalarEvaluator::new()
                    .push_param_with_field_type(lhs, lhs_field_type.clone())
                    .push_param_with_field_type(rhs, rhs_field_type.clone())
                    .evaluate(*sig)
                    .unwrap();
                if accept_orderings.contains(&ordering) {
                    assert_eq!(output, Some(1));
                } else {
                    assert_eq!(output, Some(0));
                }
            }
        }
    }

    #[test]
    fn test_compare_string() {
        fn should_match(ord: Ordering, sig: ScalarFuncSig) -> bool {
            match ord {
                Ordering::Less => {
                    sig == ScalarFuncSig::LtString
                        || sig == ScalarFuncSig::LeString
                        || sig == ScalarFuncSig::NeString
                }
                Ordering::Equal => {
                    sig == ScalarFuncSig::EqString
                        || sig == ScalarFuncSig::LeString
                        || sig == ScalarFuncSig::GeString
                }
                Ordering::Greater => {
                    sig == ScalarFuncSig::GtString
                        || sig == ScalarFuncSig::GeString
                        || sig == ScalarFuncSig::NeString
                }
            }
        }

        let signatures = vec![
            ScalarFuncSig::LtString,
            ScalarFuncSig::LeString,
            ScalarFuncSig::GtString,
            ScalarFuncSig::GeString,
            ScalarFuncSig::EqString,
            ScalarFuncSig::NeString,
        ];
        let cases = vec![
            // strA, strB, [binOrd, utfbin_no_padding, utf8bin, ciOrd]
            (
                "",
                " ",
                [
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Less,
                    Ordering::Less,
                ],
            ),
            (
                "a",
                "b",
                [
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                ],
            ),
            (
                "a",
                "A",
                [
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Greater,
                ],
            ),
            (
                "a",
                "A ",
                [
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Less,
                    Ordering::Greater,
                ],
            ),
            (
                "a",
                "a ",
                [
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Less,
                    Ordering::Less,
                ],
            ),
            (
                "À",
                "A",
                [
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Greater,
                ],
            ),
            (
                "À\t",
                "A",
                [
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                ],
            ),
            (
                "abc",
                "ab",
                [
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                ],
            ),
            (
                "a bc",
                "ab ",
                [
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                ],
            ),
            (
                "Abc",
                "abC",
                [
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Less,
                ],
            ),
            (
                "filé-110",
                "file-12",
                [
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Less,
                    Ordering::Greater,
                ],
            ),
            (
                "😜",
                "😃",
                [
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Greater,
                    Ordering::Greater,
                ],
            ),
            (
                "aa",
                "AA۝۝۝۝۝۝۝۝۝",
                [
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Greater,
                    Ordering::Less,
                    Ordering::Equal,
                    Ordering::Equal,
                    Ordering::Greater,
                ],
            ),
        ];
        let collations = [
            (Collation::Binary, 0),
            (Collation::Utf8Mb4BinNoPadding, 1),
            (Collation::Utf8Mb4Bin, 2),
            (Collation::Utf8Mb4GeneralCi, 3),
            (Collation::Utf8Mb4UnicodeCi, 4),
            (Collation::Utf8Mb40900AiCi, 5),
            (Collation::Utf8Mb40900Bin, 6),
        ];

        for (str_a, str_b, ordering_in_collations) in cases {
            for &sig in &signatures {
                for &(collation, index) in &collations {
                    let result: i64 = RpnFnScalarEvaluator::new()
                        .push_param(str_a.as_bytes().to_vec())
                        .push_param(str_b.as_bytes().to_vec())
                        .return_field_type(
                            FieldTypeBuilder::new()
                                .tp(FieldTypeTp::Long)
                                .collation(collation),
                        )
                        .evaluate(sig)
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        should_match(ordering_in_collations[index], sig) as i64,
                        result,
                        "Unexpected {:?}({}, {}) == {} in {}",
                        sig,
                        str_a,
                        str_b,
                        result,
                        collation
                    );
                }
            }
        }
    }

    #[test]
    fn test_coalesce() {
        let cases = vec![
            (vec![], None),
            (vec![None], None),
            (vec![None, None], None),
            (vec![None, None, None], None),
            (vec![None, Some(0), None], Some(0)),
        ];
        for (args, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(args)
                .evaluate(ScalarFuncSig::CoalesceInt)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_greatest_int() {
        let cases = vec![
            (vec![None, None], None),
            (vec![Some(1), Some(1)], Some(1)),
            (vec![Some(1), Some(-1), None], None),
            (vec![Some(-2), Some(-1), Some(1), Some(2)], Some(2)),
            (
                vec![Some(i64::MIN), Some(0), Some(-1), Some(i64::MAX)],
                Some(i64::MAX),
            ),
            (vec![Some(0), Some(4), Some(8), Some(8)], Some(8)),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::GreatestInt)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_least_int() {
        let cases = vec![
            (vec![None, None], None),
            (vec![Some(1), Some(1)], Some(1)),
            (vec![Some(1), Some(-1), None], None),
            (vec![Some(-2), Some(-1), Some(1), Some(2)], Some(-2)),
            (
                vec![Some(i64::MIN), Some(0), Some(-1), Some(i64::MAX)],
                Some(i64::MIN),
            ),
            (vec![Some(0), Some(4), Some(8), Some(8)], Some(0)),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::LeastInt)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_interval_int() {
        let cases = vec![
            (vec![Some(1), None], Some(1)),
            (vec![Some(1), Some(-10)], Some(1)),
            (vec![Some(1), Some(2)], Some(0)),
            (vec![Some(1), Some(1), Some(2)], Some(1)),
            (vec![Some(1), Some(0), Some(1), Some(2)], Some(2)),
            (vec![Some(1), Some(0), Some(1), Some(2), Some(5)], Some(2)),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::IntervalInt)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_interval_real() {
        let cases = vec![
            (vec![Some(1f64), None], Some(1)),
            (vec![Some(1f64), Some(-10f64)], Some(1)),
            (vec![Some(1f64), Some(2f64)], Some(0)),
            (vec![Some(1f64), Some(1f64), Some(2f64)], Some(1)),
            (
                vec![Some(1f64), Some(0f64), Some(1f64), Some(2f64)],
                Some(2),
            ),
            (
                vec![Some(1f64), Some(0f64), Some(1f64), Some(2f64), Some(5f64)],
                Some(2),
            ),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::IntervalReal)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_greatest_real() {
        let cases = vec![
            (vec![None, None], None),
            (vec![Real::new(1.0).ok(), Real::new(-1.0).ok(), None], None),
            (
                vec![
                    Real::new(1.0).ok(),
                    Real::new(-1.0).ok(),
                    Real::new(-2.0).ok(),
                    Real::new(0f64).ok(),
                ],
                Real::new(1.0).ok(),
            ),
            (
                vec![
                    Real::new(f64::MAX).ok(),
                    Real::new(f64::MIN).ok(),
                    Real::new(0f64).ok(),
                ],
                Real::new(f64::MAX).ok(),
            ),
            (vec![Real::new(f64::NAN).ok(), Real::new(0f64).ok()], None),
            (
                vec![
                    Real::new(f64::INFINITY).ok(),
                    Real::new(f64::NEG_INFINITY).ok(),
                    Real::new(f64::MAX).ok(),
                    Real::new(f64::MIN).ok(),
                ],
                Real::new(f64::INFINITY).ok(),
            ),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::GreatestReal)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_greatest_string() {
        let cases = vec![
            (vec![None, None], None),
            (
                vec![
                    Some(b"aaa".to_owned().to_vec()),
                    Some(b"bbb".to_owned().to_vec()),
                ],
                Some(b"bbb".to_owned().to_vec()),
            ),
            (vec![Some(b"aaa".to_owned().to_vec()), None], None),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::GreatestString)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_least_string() {
        let cases = vec![
            (vec![None, None], None),
            (
                vec![
                    Some(b"aaa".to_owned().to_vec()),
                    Some(b"bbb".to_owned().to_vec()),
                ],
                Some(b"aaa".to_owned().to_vec()),
            ),
            (vec![Some(b"aaa".to_owned().to_vec()), None], None),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::LeastString)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_least_real() {
        let cases = vec![
            (vec![None, None], None),
            (vec![Real::new(1.0).ok(), Real::new(-1.0).ok(), None], None),
            (
                vec![
                    Real::new(1.0).ok(),
                    Real::new(-1.0).ok(),
                    Real::new(-2.0).ok(),
                    Real::new(0f64).ok(),
                ],
                Real::new(-2.0).ok(),
            ),
            (
                vec![
                    Real::new(f64::MAX).ok(),
                    Real::new(f64::MIN).ok(),
                    Real::new(0f64).ok(),
                ],
                Real::new(f64::MIN).ok(),
            ),
            (vec![Real::new(f64::NAN).ok(), Real::new(0f64).ok()], None),
            (
                vec![
                    Real::new(f64::INFINITY).ok(),
                    Real::new(f64::NEG_INFINITY).ok(),
                    Real::new(f64::MAX).ok(),
                    Real::new(f64::MIN).ok(),
                ],
                Real::new(f64::NEG_INFINITY).ok(),
            ),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::LeastReal)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_greatest_time() {
        let mut ctx = EvalContext::default();
        let cases = vec![
            (vec![None, None], None),
            (
                vec![
                    DateTime::parse_date(&mut ctx, "2012-12-12 12:00:39").ok(),
                    DateTime::parse_date(&mut ctx, "2012-12-24 12:00:39").ok(),
                    None,
                ],
                None,
            ),
            (
                vec![
                    DateTime::parse_date(&mut ctx, "2012-12-12 12:00:39").ok(),
                    DateTime::parse_date(&mut ctx, "2012-12-24 12:00:39").ok(),
                    DateTime::parse_date(&mut ctx, "2012-12-31 12:00:39").ok(),
                ],
                DateTime::parse_date(&mut ctx, "2012-12-31 12:00:39").ok(),
            ),
            (
                vec![
                    DateTime::parse_date(&mut ctx, "2012-12-12 12:00:39").ok(),
                    DateTime::parse_date(&mut ctx, "2013-12-24 12:00:39").ok(),
                    DateTime::parse_date(&mut ctx, "2014-12-31 12:00:39").ok(),
                ],
                DateTime::parse_date(&mut ctx, "2014-12-31 12:00:39").ok(),
            ),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::GreatestTime)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_least_time() {
        let mut ctx = EvalContext::default();
        let cases = vec![
            (vec![None, None], None),
            (
                vec![
                    DateTime::parse_date(&mut ctx, "2012-12-12 12:00:39").ok(),
                    DateTime::parse_date(&mut ctx, "2012-12-24 12:00:39").ok(),
                    None,
                ],
                None,
            ),
            (
                vec![
                    DateTime::parse_date(&mut ctx, "2012-12-12 12:00:39").ok(),
                    DateTime::parse_date(&mut ctx, "2012-12-24 12:00:39").ok(),
                    DateTime::parse_date(&mut ctx, "2012-12-31 12:00:39").ok(),
                ],
                DateTime::parse_date(&mut ctx, "2012-12-12 12:00:39").ok(),
            ),
            (
                vec![
                    DateTime::parse_date(&mut ctx, "2012-12-12 12:00:39").ok(),
                    DateTime::parse_date(&mut ctx, "2013-12-24 12:00:39").ok(),
                    DateTime::parse_date(&mut ctx, "2014-12-31 12:00:39").ok(),
                ],
                DateTime::parse_date(&mut ctx, "2012-12-12 12:00:39").ok(),
            ),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::LeastTime)
                .unwrap();

            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_greatest_date() {
        let mut ctx = EvalContext::default();
        let cases = vec![
            (vec![None, None], None),
            (
                vec![
                    DateTime::parse_date(&mut ctx, "2012-12-12").ok(),
                    DateTime::parse_date(&mut ctx, "2012-12-24").ok(),
                    None,
                ],
                None,
            ),
            (
                vec![
                    DateTime::parse_date(&mut ctx, "2012-12-12").ok(),
                    DateTime::parse_date(&mut ctx, "2012-12-24").ok(),
                    DateTime::parse_date(&mut ctx, "2012-12-31").ok(),
                ],
                DateTime::parse_date(&mut ctx, "2012-12-31").ok(),
            ),
            (
                vec![
                    DateTime::parse_date(&mut ctx, "2012-12-12").ok(),
                    DateTime::parse_date(&mut ctx, "2013-12-24").ok(),
                    DateTime::parse_date(&mut ctx, "2014-12-31").ok(),
                ],
                DateTime::parse_date(&mut ctx, "2014-12-31").ok(),
            ),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::GreatestDate)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_least_date() {
        let mut ctx = EvalContext::default();
        let cases = vec![
            (vec![None, None], None),
            (
                vec![
                    DateTime::parse_date(&mut ctx, "2012-12-12").ok(),
                    DateTime::parse_date(&mut ctx, "2012-12-24").ok(),
                    None,
                ],
                None,
            ),
            (
                vec![
                    DateTime::parse_date(&mut ctx, "2012-12-12").ok(),
                    DateTime::parse_date(&mut ctx, "2012-12-24").ok(),
                    DateTime::parse_date(&mut ctx, "2012-12-31").ok(),
                ],
                DateTime::parse_date(&mut ctx, "2012-12-12").ok(),
            ),
            (
                vec![
                    DateTime::parse_date(&mut ctx, "2012-12-12").ok(),
                    DateTime::parse_date(&mut ctx, "2013-12-24").ok(),
                    DateTime::parse_date(&mut ctx, "2014-12-31").ok(),
                ],
                DateTime::parse_date(&mut ctx, "2012-12-12").ok(),
            ),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::LeastDate)
                .unwrap();

            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_greatest_duration() {
        let mut ctx = EvalContext::default();

        let cases = vec![
            (vec![None, None], None),
            (
                vec![
                    Duration::parse(&mut ctx, "123:12:12", 0).ok(),
                    Duration::parse(&mut ctx, "123:22:12", 0).ok(),
                    None,
                ],
                None,
            ),
            (
                vec![
                    Duration::parse(&mut ctx, "123:12:12", 0).ok(),
                    Duration::parse(&mut ctx, "123:22:12", 0).ok(),
                    Duration::parse(&mut ctx, "123:32:12", 0).ok(),
                ],
                Duration::parse(&mut ctx, "123:32:12", 0).ok(),
            ),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::GreatestDuration)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_least_duration() {
        let mut ctx = EvalContext::default();

        let cases = vec![
            (vec![None, None], None),
            (
                vec![
                    Duration::parse(&mut ctx, "123:12:12", 0).ok(),
                    Duration::parse(&mut ctx, "123:22:12", 0).ok(),
                    None,
                ],
                None,
            ),
            (
                vec![
                    Duration::parse(&mut ctx, "123:12:12", 0).ok(),
                    Duration::parse(&mut ctx, "123:22:12", 0).ok(),
                    Duration::parse(&mut ctx, "123:32:12", 0).ok(),
                ],
                Duration::parse(&mut ctx, "123:12:12", 0).ok(),
            ),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::LeastDuration)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_greatest_cmp_string_as_time() {
        let cases = vec![
            (vec![None, None], None),
            (
                vec![
                    Some(b"2012-12-12 12:00:39".to_owned().to_vec()),
                    Some(b"2012-12-24 12:00:39".to_owned().to_vec()),
                    None,
                ],
                None,
            ),
            (
                vec![
                    Some(b"2012-12-12 12:00:39".to_owned().to_vec()),
                    Some(b"2012-12-24 12:00:39".to_owned().to_vec()),
                    Some(b"2012-12-31 12:00:39".to_owned().to_vec()),
                ],
                Some(b"2012-12-31 12:00:39".to_owned().to_vec()),
            ),
            (
                vec![
                    Some(b"2012-12-12 12:00:39".to_owned().to_vec()),
                    Some(b"2012-12-24 12:00:39".to_owned().to_vec()),
                    Some(b"2012-12-31 12:00:39".to_owned().to_vec()),
                    Some(b"invalid_time".to_owned().to_vec()),
                ],
                None,
            ),
            (
                vec![
                    Some(b"2012-12-12 12:00:39".to_owned().to_vec()),
                    Some(b"2012-12-24 12:00:39".to_owned().to_vec()),
                    Some(b"2012-12-31 12:00:39".to_owned().to_vec()),
                    Some(b"2012-12-12 12:00:38.12003800000".to_owned().to_vec()),
                    Some(b"2012-12-31 12:00:39.120050".to_owned().to_vec()),
                    Some(b"2018-04-03 00:00:00.000000".to_owned().to_vec()),
                ],
                Some(b"2018-04-03 00:00:00.000000".to_owned().to_vec()),
            ),
            (
                vec![
                    Some(b"2012-12-12 12:00:39".to_owned().to_vec()),
                    Some(vec![0, 159, 146, 150]), // Invalid utf-8 bytes
                ],
                None,
            ),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::GreatestCmpStringAsTime)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_least_cmp_string_as_time() {
        let cases = vec![
            (vec![None, None], None),
            (
                vec![
                    Some(b"2012-12-12 12:00:39".to_owned().to_vec()),
                    Some(b"2012-12-24 12:00:39".to_owned().to_vec()),
                    None,
                ],
                None,
            ),
            (
                vec![
                    Some(b"2012-12-12 12:00:39".to_owned().to_vec()),
                    Some(b"2012-12-24 12:00:39".to_owned().to_vec()),
                    Some(b"2012-12-31 12:00:39".to_owned().to_vec()),
                ],
                Some(b"2012-12-12 12:00:39".to_owned().to_vec()),
            ),
            (
                vec![
                    Some(b"2012-12-12 12:00:39".to_owned().to_vec()),
                    Some(b"2012-12-24 12:00:39".to_owned().to_vec()),
                    Some(b"2012-12-31 12:00:39".to_owned().to_vec()),
                    Some(b"invalid_time".to_owned().to_vec()),
                ],
                None,
            ),
            (
                vec![
                    Some(b"2012-12-12 12:00:39".to_owned().to_vec()),
                    Some(b"2012-12-24 12:00:39".to_owned().to_vec()),
                    Some(b"2012-12-31 12:00:39".to_owned().to_vec()),
                    Some(b"2012-12-12 12:00:38.12003800000".to_owned().to_vec()),
                    Some(b"2012-12-31 12:00:39.120050".to_owned().to_vec()),
                    Some(b"2018-04-03 00:00:00.000000".to_owned().to_vec()),
                ],
                Some(b"2012-12-12 12:00:38.120038".to_owned().to_vec()),
            ),
            (
                vec![
                    Some(b"2012-12-12 12:00:39".to_owned().to_vec()),
                    Some(vec![0, 159, 146, 150]), // Invalid utf-8 bytes
                ],
                None,
            ),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::LeastCmpStringAsTime)
                .unwrap();

            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_greatest_cmp_string_as_date() {
        let cases = vec![
            (vec![None, None], None),
            (
                vec![
                    Some(b"2012-12-12".to_owned().to_vec()),
                    Some(b"2012-12-24".to_owned().to_vec()),
                    None,
                ],
                None,
            ),
            (
                vec![
                    Some(b"2012-12-12".to_owned().to_vec()),
                    Some(b"2012-12-24".to_owned().to_vec()),
                    Some(b"2012-12-31".to_owned().to_vec()),
                ],
                Some(b"2012-12-31".to_owned().to_vec()),
            ),
            (
                vec![
                    Some(b"2012-12-12".to_owned().to_vec()),
                    Some(b"2012-12-24".to_owned().to_vec()),
                    Some(b"2012-12-31".to_owned().to_vec()),
                    Some(b"invalid_time".to_owned().to_vec()),
                ],
                None,
            ),
            (
                vec![
                    Some(b"2012-12-12".to_owned().to_vec()),
                    Some(b"2012-12-24".to_owned().to_vec()),
                    Some(b"2012-12-31".to_owned().to_vec()),
                    Some(b"2012-12-12".to_owned().to_vec()),
                    Some(b"2012-12-31".to_owned().to_vec()),
                    Some(b"2018-04-03".to_owned().to_vec()),
                ],
                Some(b"2018-04-03".to_owned().to_vec()),
            ),
            (
                vec![
                    Some(b"2012-12-12".to_owned().to_vec()),
                    Some(vec![0, 159, 146, 150]), // Invalid utf-8 bytes
                ],
                None,
            ),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::GreatestCmpStringAsDate)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_least_cmp_string_as_date() {
        let cases = vec![
            (vec![None, None], None),
            (
                vec![
                    Some(b"2012-12-12".to_owned().to_vec()),
                    Some(b"2012-12-24".to_owned().to_vec()),
                    None,
                ],
                None,
            ),
            (
                vec![
                    Some(b"2012-12-12".to_owned().to_vec()),
                    Some(b"2012-12-24".to_owned().to_vec()),
                    Some(b"2012-12-31".to_owned().to_vec()),
                ],
                Some(b"2012-12-12".to_owned().to_vec()),
            ),
            (
                vec![
                    Some(b"2012-12-12".to_owned().to_vec()),
                    Some(b"2012-12-24".to_owned().to_vec()),
                    Some(b"2012-12-31".to_owned().to_vec()),
                    Some(b"invalid_time".to_owned().to_vec()),
                ],
                None,
            ),
            (
                vec![
                    Some(b"2012-12-12".to_owned().to_vec()),
                    Some(b"2012-12-24".to_owned().to_vec()),
                    Some(b"2012-12-31".to_owned().to_vec()),
                    Some(b"2012-12-12".to_owned().to_vec()),
                    Some(b"2012-12-31".to_owned().to_vec()),
                    Some(b"2018-04-03".to_owned().to_vec()),
                ],
                Some(b"2012-12-12".to_owned().to_vec()),
            ),
            (
                vec![
                    Some(b"2012-12-12".to_owned().to_vec()),
                    Some(vec![0, 159, 146, 150]), // Invalid utf-8 bytes
                ],
                None,
            ),
        ];

        for (row, expected) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_params(row)
                .evaluate(ScalarFuncSig::LeastCmpStringAsDate)
                .unwrap();

            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_do_get_extrenum() {
        let ints = [Some(1), Some(2), Some(3)];
        let ints_ref = ints.iter().map(|it| it.as_ref()).collect::<Vec<_>>();

        let ints_max = do_get_extremum(&ints_ref, max);
        let ints_min = do_get_extremum(&ints_ref, min);
        assert_eq!(ints_max.unwrap(), Some(3));
        assert_eq!(ints_min.unwrap(), Some(1));

        // If any item in the array is None, result should be none
        let ints_with_none = [Some(1), None, Some(3)];
        let ints_ref = ints_with_none
            .iter()
            .map(|it| it.as_ref())
            .collect::<Vec<_>>();

        let ints_max = do_get_extremum(&ints_ref, max);
        let ints_min = do_get_extremum(&ints_ref, min);
        assert_eq!(ints_max.unwrap(), None);
        assert_eq!(ints_min.unwrap(), None);
    }

    use tidb_query_datatype::{
        EvalType,
        codec::batch::{LazyBatchColumn, LazyBatchColumnVec},
    };
    use tipb_helper::ExprDefBuilder;

    use crate::{RpnExpression, RpnExpressionBuilder};

    /// `-i64::MIN`, which overflows only when it is actually entered.
    fn overflowing_int_child() -> ExprDefBuilder {
        ExprDefBuilder::scalar_func(ScalarFuncSig::UnaryMinusInt, FieldTypeTp::LongLong)
            .push_child(ExprDefBuilder::constant_int(i64::MIN))
    }

    /// A `Bytes`-typed child that fails only when entered: `JSON_UNQUOTE` of
    /// invalid UTF-8 returns an error.
    fn failing_bytes_child() -> ExprDefBuilder {
        ExprDefBuilder::scalar_func(ScalarFuncSig::JsonUnquoteSig, FieldTypeTp::VarChar)
            .push_child(ExprDefBuilder::constant_bytes(vec![0xff, 0xfe]))
    }

    fn build_expr(node: ExprDefBuilder, max_columns: usize) -> RpnExpression {
        RpnExpressionBuilder::build_from_expr_tree(
            node.build(),
            &mut EvalContext::default(),
            max_columns,
        )
        .unwrap()
    }

    fn decoded_int_column(values: impl IntoIterator<Item = Option<i64>>) -> LazyBatchColumn {
        let values: Vec<Option<i64>> = values.into_iter().collect();
        let mut column = LazyBatchColumn::decoded_with_capacity_and_tp(values.len(), EvalType::Int);
        for value in values {
            column.mut_decoded().push_int(value);
        }
        column
    }

    /// `GREATEST(Col0, -i64::MIN)`: a NULL first argument stops the row before
    /// the overflowing second child is entered.
    #[test]
    fn test_lazy_greatest_skips_after_null() {
        let expr = build_expr(
            ExprDefBuilder::scalar_func(ScalarFuncSig::GreatestInt, FieldTypeTp::LongLong)
                .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
                .push_child(overflowing_int_child()),
            1,
        );
        let schema = [FieldTypeTp::LongLong.into()];
        let mut columns = LazyBatchColumnVec::from(vec![decoded_int_column([None, None])]);
        let mut ctx = EvalContext::default();
        let result = expr
            .eval(&mut ctx, &schema, &mut columns, &[0, 1], 2)
            .unwrap();
        assert_eq!(
            result.vector_value().unwrap().as_ref().to_int_vec(),
            [None, None]
        );

        // A non-NULL first argument needs the second child and overflows.
        let mut columns = LazyBatchColumnVec::from(vec![decoded_int_column([Some(1)])]);
        let mut ctx = EvalContext::default();
        assert!(expr.eval(&mut ctx, &schema, &mut columns, &[0], 1).is_err());
    }

    /// `LEAST(Col0, Col1, -i64::MIN)`: once an argument is NULL the row is
    /// finished, so the overflowing third child is never entered.
    #[test]
    fn test_lazy_least_skips_after_null() {
        let expr = build_expr(
            ExprDefBuilder::scalar_func(ScalarFuncSig::LeastInt, FieldTypeTp::LongLong)
                .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
                .push_child(ExprDefBuilder::column_ref(1, FieldTypeTp::LongLong))
                .push_child(overflowing_int_child()),
            2,
        );
        let schema = [FieldTypeTp::LongLong.into(), FieldTypeTp::LongLong.into()];
        let mut columns = LazyBatchColumnVec::from(vec![
            decoded_int_column([Some(1), Some(1)]),
            decoded_int_column([None, None]),
        ]);
        let mut ctx = EvalContext::default();
        let result = expr
            .eval(&mut ctx, &schema, &mut columns, &[0, 1], 2)
            .unwrap();
        assert_eq!(
            result.vector_value().unwrap().as_ref().to_int_vec(),
            [None, None]
        );

        let mut columns = LazyBatchColumnVec::from(vec![
            decoded_int_column([Some(1)]),
            decoded_int_column([Some(2)]),
        ]);
        let mut ctx = EvalContext::default();
        assert!(expr.eval(&mut ctx, &schema, &mut columns, &[0], 1).is_err());
    }

    /// `GREATEST(CmpStringAsTime(NULL), <failing bytes>)`: a NULL argument
    /// stops the row before the failing later child.
    #[test]
    fn test_lazy_greatest_cmp_string_as_time_skips_after_null() {
        let expr = build_expr(
            ExprDefBuilder::scalar_func(
                ScalarFuncSig::GreatestCmpStringAsTime,
                FieldTypeTp::VarChar,
            )
            .push_child(ExprDefBuilder::constant_null(FieldTypeTp::VarChar))
            .push_child(failing_bytes_child()),
            0,
        );
        let mut columns = LazyBatchColumnVec::empty();
        let mut ctx = EvalContext::default();
        let result = expr.eval(&mut ctx, &[], &mut columns, &[0], 1).unwrap();
        assert_eq!(
            result.vector_value().unwrap().as_ref().to_bytes_vec(),
            [None]
        );

        // A non-NULL first argument enters the failing child.
        let expr = build_expr(
            ExprDefBuilder::scalar_func(
                ScalarFuncSig::GreatestCmpStringAsTime,
                FieldTypeTp::VarChar,
            )
            .push_child(ExprDefBuilder::constant_bytes(
                b"2012-12-12 12:00:39".to_vec(),
            ))
            .push_child(failing_bytes_child()),
            0,
        );
        let mut columns = LazyBatchColumnVec::empty();
        let mut ctx = EvalContext::default();
        assert!(expr.eval(&mut ctx, &[], &mut columns, &[0], 1).is_err());
    }

    /// `INTERVAL(Col0, -i64::MIN)`: a NULL target returns -1 without entering
    /// the boundary arguments; a non-NULL target does enter them.
    #[test]
    fn test_lazy_interval_skips_boundaries_for_null_target() {
        let expr = build_expr(
            ExprDefBuilder::scalar_func(ScalarFuncSig::IntervalInt, FieldTypeTp::LongLong)
                .push_child(ExprDefBuilder::column_ref(0, FieldTypeTp::LongLong))
                .push_child(overflowing_int_child()),
            1,
        );
        let schema = [FieldTypeTp::LongLong.into()];
        let mut columns = LazyBatchColumnVec::from(vec![decoded_int_column([None, None])]);
        let mut ctx = EvalContext::default();
        let result = expr
            .eval(&mut ctx, &schema, &mut columns, &[0, 1], 2)
            .unwrap();
        assert_eq!(
            result.vector_value().unwrap().as_ref().to_int_vec(),
            [Some(-1), Some(-1)]
        );

        let mut columns = LazyBatchColumnVec::from(vec![decoded_int_column([Some(1)])]);
        let mut ctx = EvalContext::default();
        assert!(expr.eval(&mut ctx, &schema, &mut columns, &[0], 1).is_err());
    }
}
