// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::str::from_utf8;

use chrono::{self, DurationRound, Offset, TimeZone};
use tidb_query_codegen::rpn_fn;
use tidb_query_common::{Result, error::EvaluateError};
use tidb_query_datatype::{
    FieldTypeAccessor, FieldTypeFlag,
    codec::{
        Error, Result as CodecResult,
        convert::ConvertTo,
        data_type::*,
        mysql::{
            Duration, MAX_FSP, RoundMode, Time, TimeType, Tz, check_fsp,
            duration::{
                MAX_HOUR_PART, MAX_MINUTE_PART, MAX_NANOS, MAX_NANOS_PART, MAX_SECOND_PART,
                NANOS_PER_SEC,
            },
            time::{
                WeekdayExtension, extension::DateTimeExtension, interval::*, weekmode::WeekMode,
            },
        },
    },
    expr::{EvalContext, SqlMode},
};

use crate::RpnFnCallExtra;

/// Only the actual clock tuple's transport width is constrained. Raw nanos and
/// timestamp/offset domains retain the original pure clock implementation's
/// arithmetic and panic behavior rather than being normalized by admission.
pub fn native_clock_args_valid(bytes: &[u8]) -> bool {
    bytes.len() == 16
}

/// The second operand is the caller's actual parsed fractional precision.
pub fn native_clock_fsp_args_valid(bytes: &[u8], fsp: i64) -> bool {
    native_clock_args_valid(bytes) && (0..=6).contains(&fsp)
}

fn decode_native_clock(bytes: &[u8]) -> Result<crate::NativeClockInput> {
    if !native_clock_args_valid(bytes) {
        return Err(other_err!(
            "Native clock transport requires exactly 16 bytes"
        ));
    }
    Ok(crate::NativeClockInput {
        utc_secs: i64::from_le_bytes(bytes[..8].try_into().unwrap()),
        nanos: u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
        tz_offset: i32::from_le_bytes(bytes[12..16].try_into().unwrap()),
    })
}

fn decode_native_clock_fsp(bytes: &[u8], fsp: &Int) -> Result<(crate::NativeClockInput, u32)> {
    if !native_clock_fsp_args_valid(bytes, *fsp) {
        return Err(other_err!(
            "Native clock FSP transport requires 16 bytes and precision 0 through 6"
        ));
    }
    Ok((decode_native_clock(bytes)?, *fsp as u32))
}

#[rpn_fn]
fn now_native(bytes: BytesRef, fsp: &Int) -> Result<Option<Bytes>> {
    let (clock, fsp) = decode_native_clock_fsp(bytes, fsp)?;
    Ok(Some(crate::native_now(clock, fsp).into_bytes()))
}

#[rpn_fn]
fn current_date_native(bytes: BytesRef) -> Result<Option<Bytes>> {
    Ok(Some(
        crate::native_current_date(decode_native_clock(bytes)?).into_bytes(),
    ))
}

#[rpn_fn]
fn sysdate_native(bytes: BytesRef, fsp: &Int) -> Result<Option<Bytes>> {
    let (clock, fsp) = decode_native_clock_fsp(bytes, fsp)?;
    Ok(Some(crate::native_sysdate(clock, fsp).into_bytes()))
}

#[rpn_fn]
fn utc_date_native(bytes: BytesRef) -> Result<Option<Bytes>> {
    Ok(Some(
        crate::native_utc_date(decode_native_clock(bytes)?).into_bytes(),
    ))
}

#[rpn_fn]
fn utc_timestamp_native(bytes: BytesRef, fsp: &Int) -> Result<Option<Bytes>> {
    let (clock, fsp) = decode_native_clock_fsp(bytes, fsp)?;
    Ok(Some(crate::native_utc_timestamp(clock, fsp).into_bytes()))
}

#[rpn_fn]
fn current_time_without_fsp_native(bytes: BytesRef) -> Result<Option<Bytes>> {
    Ok(Some(
        crate::native_current_time_without_fsp(decode_native_clock(bytes)?).into_bytes(),
    ))
}

#[rpn_fn]
fn current_time_with_fsp_native(bytes: BytesRef, fsp: &Int) -> Result<Option<Bytes>> {
    let (clock, fsp) = decode_native_clock_fsp(bytes, fsp)?;
    Ok(Some(
        crate::native_current_time_with_fsp(clock, fsp).into_bytes(),
    ))
}

#[rpn_fn]
fn utc_time_without_fsp_native(bytes: BytesRef) -> Result<Option<Bytes>> {
    Ok(Some(
        crate::native_utc_time_without_fsp(decode_native_clock(bytes)?).into_bytes(),
    ))
}

#[rpn_fn]
fn utc_time_with_fsp_native(bytes: BytesRef, fsp: &Int) -> Result<Option<Bytes>> {
    let (clock, fsp) = decode_native_clock_fsp(bytes, fsp)?;
    Ok(Some(
        crate::native_utc_time_with_fsp(clock, fsp).into_bytes(),
    ))
}

#[rpn_fn(nullable)]
fn utc_time_null_native(witness: Option<&Int>) -> Result<Option<Bytes>> {
    if witness.is_some() {
        return Err(other_err!(
            "UTC_TIME NULL transport contains a non-NULL witness"
        ));
    }
    Ok(None)
}

// TimeCoreBits is a field-projection carrier, not a validated DateTime or a
// packed-Time conversion. Only transport width is checked here.
fn decode_time_core_native(bytes: BytesRef) -> Result<u64> {
    if bytes.len() != 8 {
        return Err(other_err!(
            "Native TimeCoreBits transport requires exactly 8 bytes"
        ));
    }
    let mut encoded = [0; 8];
    encoded.copy_from_slice(bytes);
    Ok(u64::from_le_bytes(encoded))
}

/// DATE consumes the original non-NULL core and actual SQL-mode flag bits.
/// The allow-invalid bit is retained metadata but does not affect DATE's
/// policy.
pub fn date_core_native_args_valid(core: Option<&[u8]>, modes: Option<i64>) -> bool {
    core.is_some_and(|bytes| bytes.len() == 8)
        && modes.is_some_and(|flags| (0..=7).contains(&flags))
}

#[rpn_fn]
fn date_core_native(core: BytesRef, modes: &Int) -> Result<Option<Int>> {
    if !date_core_native_args_valid(Some(core), Some(*modes)) {
        return Err(other_err!(
            "Native DATE requires an actual 8-byte core and mode flags 0 through 7"
        ));
    }
    let raw = decode_time_core_native(core)?;
    if Time::native_date_rejects_zero(raw, *modes & 1 != 0, *modes & 2 != 0) {
        return Ok(None);
    }
    // Preserve the full unsigned computed core in the existing signed owner.
    Ok(Some(Time::native_date_core(raw) as Int))
}

#[rpn_fn(nullable)]
fn date_core_predicate_legacy(core: Option<BytesRef>) -> Result<Option<Int>> {
    core.map_or(Ok(None), |bytes| {
        let raw = decode_time_core_native(bytes)?;
        Ok(Some(Int::from(Time::native_date_core(raw) != 0)))
    })
}

#[rpn_fn(nullable)]
fn year_core_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(Some(
            Time::year_from_core_bits(decode_time_core_native(bytes)?) as Int,
        ))
    })
}

#[rpn_fn(nullable)]
fn month_core_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(Some(
            Time::month_from_core_bits(decode_time_core_native(bytes)?) as Int,
        ))
    })
}

#[rpn_fn(nullable)]
fn day_of_month_core_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(Some(
            Time::day_from_core_bits(decode_time_core_native(bytes)?) as Int,
        ))
    })
}

#[rpn_fn(nullable)]
fn quarter_core_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(Some(
            Time::quarter_from_core_bits(decode_time_core_native(bytes)?) as Int,
        ))
    })
}

// Keep the existing HMS transport diagnostic while sharing strict UTF-8
// decoding.
fn decode_native_time_text(bytes: BytesRef) -> Result<&str> {
    from_utf8(bytes).map_err(|_| other_err!("Native HMS text transport requires UTF-8"))
}

// SQL's Display-derived text policy is distinct from legacy signed nanos.
// Invalid UTF-8 is a transport error; a valid string may parse to SQL NULL.
fn parse_native_hms_text(bytes: BytesRef) -> Result<Option<(u32, u32, u32)>> {
    Ok(Time::parse_native_hms(decode_native_time_text(bytes)?))
}

#[rpn_fn(nullable)]
fn hour_text_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(parse_native_hms_text(bytes)?.map(|(hour, ..)| hour as Int))
    })
}

#[rpn_fn(nullable)]
fn minute_text_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(parse_native_hms_text(bytes)?.map(|(_, minute, _)| minute as Int))
    })
}

#[rpn_fn(nullable)]
fn second_text_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(parse_native_hms_text(bytes)?.map(|(_, _, second)| second as Int))
    })
}

#[rpn_fn(nullable)]
fn hour_nanos_native(arg: Option<&Int>) -> Result<Option<Int>> {
    Ok(arg.map(|nanos| Duration::hours_from_nanos(*nanos) as Int))
}

#[rpn_fn(nullable)]
fn minute_nanos_native(arg: Option<&Int>) -> Result<Option<Int>> {
    Ok(arg.map(|nanos| Duration::minutes_from_nanos(*nanos) as Int))
}

#[rpn_fn(nullable)]
fn second_nanos_native(arg: Option<&Int>) -> Result<Option<Int>> {
    Ok(arg.map(|nanos| Duration::secs_from_nanos(*nanos) as Int))
}

#[rpn_fn(nullable)]
fn month_name_text_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(Time::parse_native_date_ymd(decode_native_time_text(bytes)?)
            .map(|(_, month, _)| Time::month_name_from_month(month).to_string().into_bytes()))
    })
}

#[rpn_fn(nullable)]
fn time_to_sec_text_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(
            Time::parse_native_duration_text(decode_native_time_text(bytes)?)
                .map(|(seconds, _)| seconds),
        )
    })
}

#[rpn_fn(nullable)]
fn period_add_native(period: Option<&Int>, months: Option<&Int>) -> Result<Option<Int>> {
    let (Some(period), Some(months)) = (period, months) else {
        return Ok(None);
    };
    if !Time::is_valid_native_period(*period) {
        return Err(EvaluateError::PeriodAddIncorrectArguments.into());
    }
    let sum = (Time::period_to_month_wrapping(*period as u64) as i64).wrapping_add(*months);
    Ok(Some(Time::month_to_period_wrapping(sum as u64) as Int))
}

#[rpn_fn(nullable)]
fn period_diff_native(left: Option<&Int>, right: Option<&Int>) -> Result<Option<Int>> {
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(None);
    };
    if !Time::is_valid_native_period(*left) || !Time::is_valid_native_period(*right) {
        return Err(EvaluateError::PeriodDiffIncorrectArguments.into());
    }
    Ok(Some(
        Time::period_to_month_wrapping(*left as u64)
            .wrapping_sub(Time::period_to_month_wrapping(*right as u64)) as Int,
    ))
}

#[rpn_fn(nullable)]
fn get_format_native(kind: Option<BytesRef>, locale: Option<BytesRef>) -> Result<Option<Bytes>> {
    Ok(kind.zip(locale).map(|(kind, locale)| {
        Time::get_format_native(kind, locale)
            .to_string()
            .into_bytes()
    }))
}

#[rpn_fn(nullable)]
fn get_format_null_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    match arg {
        None => Ok(None),
        Some(_) => Err(other_err!(
            "Native GET_FORMAT NULL witness must be an actual NULL"
        )),
    }
}

#[rpn_fn(nullable)]
fn day_of_week_text_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(
            Time::parse_native_date_ymd(decode_native_time_text(bytes)?).map(
                |(year, month, day)| {
                    (Time::native_weekday_sunday_index(year, month, day) + 1) as Int
                },
            ),
        )
    })
}

#[rpn_fn(nullable)]
fn weekday_text_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(
            Time::parse_native_date_ymd(decode_native_time_text(bytes)?).map(
                |(year, month, day)| {
                    ((Time::native_weekday_sunday_index(year, month, day) + 6) % 7) as Int
                },
            ),
        )
    })
}

#[rpn_fn(nullable)]
fn day_of_year_text_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(Time::parse_native_date_ymd(decode_native_time_text(bytes)?)
            .map(|(year, month, day)| Time::native_day_of_year(year, month, day)))
    })
}

#[rpn_fn(nullable)]
fn day_name_text_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(
            Time::parse_native_date_ymd(decode_native_time_text(bytes)?).map(
                |(year, month, day)| {
                    Time::weekday_name_from_sunday_index(Time::native_weekday_sunday_index(
                        year, month, day,
                    ))
                    .to_string()
                    .into_bytes()
                },
            ),
        )
    })
}

#[rpn_fn(nullable)]
fn date_diff_text_native(left: Option<BytesRef>, right: Option<BytesRef>) -> Result<Option<Int>> {
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(None);
    };
    let left = decode_native_time_text(left)?;
    let right = decode_native_time_text(right)?;
    let (Some((ly, lm, ld)), Some((ry, rm, rd))) = (
        Time::parse_native_date_ymd(left),
        Time::parse_native_date_ymd(right),
    ) else {
        return Ok(None);
    };
    Ok(Some(
        Time::native_days_from_civil(ly, lm, ld) - Time::native_days_from_civil(ry, rm, rd),
    ))
}

#[rpn_fn(nullable)]
fn date_diff_null_native(arg: Option<&Int>) -> Result<Option<Int>> {
    match arg {
        None => Ok(None),
        Some(_) => Err(other_err!(
            "Native DATEDIFF NULL witness must be an actual NULL"
        )),
    }
}

#[rpn_fn(nullable)]
fn date_diff_core_native(left: Option<BytesRef>, right: Option<BytesRef>) -> Result<Option<Int>> {
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(None);
    };
    Ok(Some(Time::native_core_date_diff(
        decode_time_core_native(left)?,
        decode_time_core_native(right)?,
    ) as Int))
}

// This demand probe returns the original owned text, not a date/validity
// marker.
#[rpn_fn(nullable)]
fn week_date_text_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(Time::parse_native_date_ymd(decode_native_time_text(bytes)?).map(|_| bytes.to_vec()))
    })
}

#[rpn_fn(nullable)]
fn week_text_native(arg: Option<BytesRef>, mode: Option<&Int>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(
            Time::parse_native_date_ymd(decode_native_time_text(bytes)?).map(
                |(year, month, day)| {
                    // Native explicit NULL mode is zero; wire WEEK keeps its own policy.
                    Time::native_week_of_year(year, month, day, mode.copied().unwrap_or(0), false).1
                },
            ),
        )
    })
}

#[rpn_fn(nullable)]
fn year_week_text_native(arg: Option<BytesRef>, mode: Option<&Int>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(
            Time::parse_native_date_ymd(decode_native_time_text(bytes)?).map(
                |(year, month, day)| {
                    let (year, number) = Time::native_week_of_year(
                        year,
                        month,
                        day,
                        mode.copied().unwrap_or(0),
                        true,
                    );
                    let result = year * 100 + number;
                    if result < 0 {
                        i64::from(u32::MAX)
                    } else {
                        result
                    }
                },
            ),
        )
    })
}

#[rpn_fn(nullable)]
fn week_of_year_text_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(Time::parse_native_date_ymd(decode_native_time_text(bytes)?)
            .map(|(year, month, day)| Time::native_week_of_year(year, month, day, 3, false).1))
    })
}

#[rpn_fn(nullable)]
fn week_null_native(arg: Option<&Int>) -> Result<Option<Int>> {
    match arg {
        None => Ok(None),
        Some(_) => Err(other_err!(
            "Native WEEK NULL witness must be an actual NULL"
        )),
    }
}

#[rpn_fn(nullable)]
fn week_core_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(Some(
            Time::native_core_week(decode_time_core_native(bytes)?, 0) as Int,
        ))
    })
}

#[rpn_fn(nullable)]
fn to_days_text_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(
            Time::parse_native_datetime_components(decode_native_time_text(bytes)?)
                .map(|(year, month, day, ..)| Time::native_time_diff_daynr(year, month, day)),
        )
    })
}

#[rpn_fn(nullable)]
fn to_seconds_text_native(arg: Option<BytesRef>) -> Result<Option<Int>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(
            Time::parse_native_datetime_components(decode_native_time_text(bytes)?).map(
                |(year, month, day, hour, minute, second, _)| {
                    Time::native_time_diff_daynr(year, month, day) * 86_400
                        + i64::from(hour) * 3_600
                        + i64::from(minute) * 60
                        + i64::from(second)
                },
            ),
        )
    })
}

#[rpn_fn(nullable)]
fn tso_logical_native(arg: Option<&Int>) -> Result<Option<Int>> {
    Ok(arg.and_then(|tso| {
        if *tso <= 0 {
            None
        } else {
            Some(*tso & ((1_i64 << 18) - 1))
        }
    }))
}

#[rpn_fn(nullable)]
fn date_format_text_native(
    date: Option<BytesRef>,
    layout: Option<BytesRef>,
) -> Result<Option<Bytes>> {
    let (Some(date), Some(layout)) = (date, layout) else {
        return Ok(None);
    };
    Ok(Time::native_text_date_format(
        decode_native_time_text(date)?,
        decode_native_time_text(layout)?,
    )
    .map(String::into_bytes))
}

#[rpn_fn(nullable)]
fn date_format_core_native(
    core: Option<BytesRef>,
    layout: Option<BytesRef>,
) -> Result<Option<Bytes>> {
    let Some(core) = core else {
        return Err(other_err!(
            "Native DATE_FORMAT raw transport requires a present value"
        ));
    };
    // Preserve the full raw word. No Time construction, field projection, or
    // low-bit normalization belongs to this transport boundary.
    let core = decode_time_core_native(core)?;
    let Some(layout) = layout else {
        return Ok(None);
    };
    Ok(
        Time::native_core_date_format(core, decode_native_time_text(layout)?)
            .map(String::into_bytes),
    )
}

#[rpn_fn(nullable)]
fn date_format_null_native(arg: Option<&Int>) -> Result<Option<Bytes>> {
    match arg {
        None => Ok(None),
        Some(_) => Err(other_err!(
            "Native DATE_FORMAT NULL witness must be an actual NULL"
        )),
    }
}

#[rpn_fn]
fn date_format_missing_native() -> Result<Option<Int>> {
    Ok(Some(0))
}

#[rpn_fn(nullable)]
fn duration_text_probe_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    arg.map_or(Ok(None), |bytes| {
        Ok(
            Time::parse_native_duration_text(decode_native_time_text(bytes)?)
                .map(|_| bytes.to_vec()),
        )
    })
}

#[rpn_fn(nullable)]
fn time_format_text_native(
    time: Option<BytesRef>,
    layout: Option<BytesRef>,
) -> Result<Option<Bytes>> {
    let (Some(time), Some(layout)) = (time, layout) else {
        return Ok(None);
    };
    Ok(Time::native_time_format(
        decode_native_time_text(time)?,
        decode_native_time_text(layout)?,
    )
    .map(String::into_bytes))
}

#[rpn_fn(nullable)]
fn last_day_text_native(arg: Option<BytesRef>) -> Result<Option<Bytes>> {
    arg.map_or(Ok(None), |bytes| {
        let value = decode_native_time_text(bytes)?.trim();
        let (date, time) = value
            .split_once(char::is_whitespace)
            .map_or((value, None), |(date, time)| (date, Some(time.trim())));
        let Some((y, m, _)) = Time::parse_native_date_ymd(date) else {
            return Ok(None);
        };
        if let Some(time) = time {
            if Time::parse_native_clock_with_fraction(time).is_none() {
                return Ok(None);
            }
        }
        let next_month = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        let (last_y, last_m, last_d) = Time::native_civil_from_days(
            Time::native_days_from_civil(next_month.0, next_month.1, 1) - 1,
        );
        Ok(Some(
            format!("{last_y:04}-{last_m:02}-{last_d:02}").into_bytes(),
        ))
    })
}

#[rpn_fn(nullable, capture = [ctx])]
#[inline]
pub fn date_format(
    ctx: &mut EvalContext,
    t: Option<&DateTime>,
    layout: Option<BytesRef>,
) -> Result<Option<Bytes>> {
    use std::str::from_utf8;

    if t.is_none() || layout.is_none() {
        return Ok(None);
    }
    let (t, layout) = (t.as_ref().unwrap(), layout.as_ref().unwrap());
    if t.invalid_zero() {
        return ctx
            .handle_invalid_time_error(Error::incorrect_datetime_value(t))
            .map(|_| Ok(None))?;
    }

    let t = t.date_format(from_utf8(layout).map_err(Error::Encoding)?);
    if let Err(err) = t {
        return ctx.handle_invalid_time_error(err).map(|_| Ok(None))?;
    }

    Ok(Some(t.unwrap().into_bytes()))
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn date(ctx: &mut EvalContext, t: &DateTime) -> Result<Option<DateTime>> {
    if t.invalid_zero() {
        return ctx
            .handle_invalid_time_error(Error::incorrect_datetime_value(t))
            .map(|_| Ok(None))?;
    }

    let mut res = *t;
    res.set_time_type(TimeType::Date)?;
    Ok(Some(res))
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn sysdate_with_fsp(ctx: &mut EvalContext, fsp: &Int) -> Result<Option<DateTime>> {
    DateTime::from_local_time(ctx, TimeType::DateTime, *fsp as i8)
        .map(Some)
        .or_else(|e| ctx.handle_invalid_time_error(e).map(|_| Ok(None))?)
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn sysdate_without_fsp(ctx: &mut EvalContext) -> Result<Option<DateTime>> {
    DateTime::from_local_time(ctx, TimeType::DateTime, 0)
        .map(Some)
        .or_else(|e| ctx.handle_invalid_time_error(e).map(|_| Ok(None))?)
}

#[rpn_fn(nullable, capture = [ctx])]
#[inline]
pub fn week_with_mode(
    ctx: &mut EvalContext,
    t: Option<&DateTime>,
    m: Option<&Int>,
) -> Result<Option<Int>> {
    if t.is_none() || m.is_none() {
        return Ok(None);
    }
    let (t, m) = (t.unwrap(), m.unwrap());
    if t.invalid_zero() {
        return ctx
            .handle_invalid_time_error(Error::incorrect_datetime_value(t))
            .map(|_| Ok(None))?;
    }
    let week = t.week(WeekMode::from_bits_truncate(*m as u32));
    Ok(Some(i64::from(week)))
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn week_without_mode(ctx: &mut EvalContext, t: &DateTime) -> Result<Option<Int>> {
    if t.invalid_zero() {
        return ctx
            .handle_invalid_time_error(Error::incorrect_datetime_value(t))
            .map(|_| Ok(None))?;
    }
    let week = t.week(WeekMode::from_bits_truncate(0u32));
    Ok(Some(i64::from(week)))
}

#[rpn_fn(nullable, capture = [ctx])]
#[inline]
pub fn week_day(ctx: &mut EvalContext, t: Option<&DateTime>) -> Result<Option<Int>> {
    if t.is_none() {
        return Ok(None);
    }
    let t = t.as_ref().unwrap();
    if t.invalid_zero() {
        return ctx
            .handle_invalid_time_error(Error::incorrect_datetime_value(t))
            .map(|_| Ok(None))?;
    }
    let day = t.weekday().num_days_from_monday();
    Ok(Some(i64::from(day)))
}

#[rpn_fn(nullable, capture = [ctx])]
#[inline]
pub fn day_of_week(ctx: &mut EvalContext, t: Option<&DateTime>) -> Result<Option<Int>> {
    if t.is_none() {
        return Ok(None);
    }
    let t = t.as_ref().unwrap();
    if t.invalid_zero() {
        return ctx
            .handle_invalid_time_error(Error::incorrect_datetime_value(t))
            .map(|_| Ok(None))?;
    }
    let day = t.weekday().number_from_sunday();
    Ok(Some(Int::from(day)))
}

#[rpn_fn(nullable, capture = [ctx])]
#[inline]
pub fn day_of_year(ctx: &mut EvalContext, t: Option<&DateTime>) -> Result<Option<Int>> {
    if t.is_none() {
        return Ok(None);
    }
    let t = t.as_ref().unwrap();
    if t.invalid_zero() {
        return ctx
            .handle_invalid_time_error(Error::incorrect_datetime_value(t))
            .map(|_| Ok(None))?;
    }
    let day = t.days();
    Ok(Some(Int::from(day)))
}

#[rpn_fn(nullable, capture = [ctx])]
#[inline]
pub fn week_of_year(ctx: &mut EvalContext, t: Option<&DateTime>) -> Result<Option<Int>> {
    if t.is_none() {
        return Ok(None);
    }
    let t = t.as_ref().unwrap();
    if t.invalid_zero() {
        return ctx
            .handle_invalid_time_error(Error::incorrect_datetime_value(t))
            .map(|_| Ok(None))?;
    }
    let week = t.week(WeekMode::from_bits_truncate(3));
    Ok(Some(Int::from(week)))
}

// year_week_with_mode implements `YEARWEEK` in MySQL.
// See also: https://dev.mysql.com/doc/refman/5.7/en/date-and-time-functions.html#function_yearweek
//
// e.g.: SELECT YEARWEEK('1987-01-01');  -- -> 198652, here the first 4 digits
// represents year, and the last 2 digits represents week.
#[rpn_fn(capture = [ctx])]
#[inline]
pub fn year_week_with_mode(ctx: &mut EvalContext, t: &DateTime, mode: &Int) -> Result<Option<Int>> {
    if t.invalid_zero() {
        return ctx
            .handle_invalid_time_error(Error::incorrect_datetime_value(t))
            .map(|_| Ok(None))?;
    }

    let (year, week) = t.year_week(WeekMode::from_bits_truncate(*mode as u32));
    let result = i64::from(week + year * 100);
    if result < 0 {
        return Ok(Some(i64::from(u32::MAX)));
    }
    Ok(Some(result))
}

// year_week_without_mode implements `YEARWEEK` in MySQL.
// See also: https://dev.mysql.com/doc/refman/5.7/en/date-and-time-functions.html#function_yearweek
#[rpn_fn(capture = [ctx])]
#[inline]
pub fn year_week_without_mode(ctx: &mut EvalContext, t: &DateTime) -> Result<Option<Int>> {
    if t.invalid_zero() {
        return ctx
            .handle_invalid_time_error(Error::incorrect_datetime_value(t))
            .map(|_| Ok(None))?;
    }

    let (year, week) = t.year_week(WeekMode::from_bits_truncate(0u32));
    let result = i64::from(week + year * 100);
    if result < 0 {
        return Ok(Some(i64::from(u32::MAX)));
    }
    Ok(Some(result))
}

#[rpn_fn(nullable, capture = [ctx])]
#[inline]
pub fn to_days(ctx: &mut EvalContext, t: Option<&DateTime>) -> Result<Option<Int>> {
    let t = match t {
        Some(v) => v,
        _ => return Ok(None),
    };
    if t.invalid_zero() {
        return ctx
            .handle_invalid_time_error(Error::incorrect_datetime_value(t))
            .map(|_| Ok(None))?;
    }
    Ok(Some(Int::from(t.day_number())))
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn to_seconds(ctx: &mut EvalContext, t: &DateTime) -> Result<Option<Int>> {
    if t.invalid_zero() {
        return ctx
            .handle_invalid_time_error(Error::incorrect_datetime_value(t))
            .map(|_| Ok(None))?;
    }
    Ok(Some(t.second_number()))
}

#[rpn_fn(writer, capture = [ctx])]
#[inline]
pub fn add_string_and_duration(
    ctx: &mut EvalContext,
    arg0: BytesRef,
    arg1: &Duration,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    let arg0 = std::str::from_utf8(arg0).map_err(Error::Encoding)?;
    if let Ok(arg0) = Duration::parse_exactly(ctx, arg0, MAX_FSP) {
        return match arg0.checked_add(*arg1) {
            Some(result) => Ok(writer.write(Some(duration_to_string(result).into_bytes()))),
            None => ctx
                .handle_overflow_err(Error::overflow("DURATION", format!("{} + {}", arg0, arg1)))
                .map(|_| Ok(writer.write(None)))?,
        };
    };
    if let Ok(arg0) = DateTime::parse_datetime(ctx, arg0, MAX_FSP, true) {
        return match arg0.checked_add(ctx, *arg1) {
            Some(result) => Ok(writer.write(Some(datetime_to_string(result).into_bytes()))),
            None => ctx
                .handle_overflow_err(Error::overflow("DATETIME", format!("{} + {}", arg0, arg1)))
                .map(|_| Ok(writer.write(None)))?,
        };
    };
    ctx.handle_invalid_time_error(Error::incorrect_datetime_value(arg0))?;

    Ok(writer.write(None))
}

#[rpn_fn(writer, capture = [ctx])]
#[inline]
pub fn sub_string_and_duration(
    ctx: &mut EvalContext,
    arg0: BytesRef,
    arg1: &Duration,
    writer: BytesWriter,
) -> Result<BytesGuard> {
    let arg0 = std::str::from_utf8(arg0).map_err(Error::Encoding)?;
    if let Ok(arg0) = Duration::parse_exactly(ctx, arg0, MAX_FSP) {
        return match arg0.checked_sub(*arg1) {
            Some(result) => Ok(writer.write(Some(duration_to_string(result).into_bytes()))),
            None => ctx
                .handle_overflow_err(Error::overflow("DURATION", format!("{} - {}", arg0, arg1)))
                .map(|_| Ok(writer.write(None)))?,
        };
    };
    if let Ok(arg0) = DateTime::parse_datetime(ctx, arg0, MAX_FSP, true) {
        return match arg0.checked_sub(ctx, *arg1) {
            Some(result) => Ok(writer.write(Some(datetime_to_string(result).into_bytes()))),
            None => ctx
                .handle_overflow_err(Error::overflow("DATETIME", format!("{} - {}", arg0, arg1)))
                .map(|_| Ok(writer.write(None)))?,
        };
    };
    ctx.handle_invalid_time_error(Error::incorrect_datetime_value(arg0))?;

    Ok(writer.write(None))
}

#[rpn_fn]
#[inline]
pub fn date_diff(from_time: &DateTime, to_time: &DateTime) -> Result<Option<Int>> {
    Ok(from_time.date_diff(*to_time))
}

#[rpn_fn]
#[inline]
pub fn null_time_diff() -> Result<Option<Duration>> {
    Ok(None)
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn add_datetime_and_duration(
    ctx: &mut EvalContext,
    datetime: &DateTime,
    duration: &Duration,
) -> Result<Option<DateTime>> {
    let mut res = match datetime.checked_add(ctx, *duration) {
        Some(res) => res,
        None => {
            return ctx
                .handle_invalid_time_error(Error::overflow(
                    "DATETIME",
                    format!("({} + {})", datetime, duration),
                ))
                .map(|_| Ok(None))?;
        }
    };
    if res.set_time_type(TimeType::DateTime).is_err() {
        return Ok(None);
    }
    Ok(Some(res))
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn add_datetime_and_string(
    ctx: &mut EvalContext,
    arg0: &DateTime,
    arg1: BytesRef,
) -> Result<Option<DateTime>> {
    let arg1 = std::str::from_utf8(arg1).map_err(Error::Encoding)?;
    let arg1 = match Duration::parse(ctx, arg1, MAX_FSP) {
        Ok(arg) => arg,
        Err(_) => return Ok(None),
    };

    let res = match arg0.checked_add(ctx, arg1) {
        Some(res) => res,
        None => {
            return ctx
                .handle_invalid_time_error(Error::overflow(
                    "DATETIME",
                    format!("({} + {})", arg0, arg1),
                ))
                .map(|_| Ok(None))?;
        }
    };
    Ok(Some(res))
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn add_date_and_string(
    ctx: &mut EvalContext,
    arg0: &DateTime,
    arg1: BytesRef,
) -> Result<Option<DateTime>> {
    let arg1 = std::str::from_utf8(arg1).map_err(Error::Encoding)?;
    let arg1 = match Duration::parse(ctx, arg1, MAX_FSP) {
        Ok(arg) => arg,
        Err(_) => return Ok(None),
    };

    let mut res = match arg0.checked_add(ctx, arg1) {
        Some(res) => res,
        None => {
            return ctx
                .handle_invalid_time_error(Error::overflow(
                    "DATE",
                    format!("({} + {})", arg0, arg1),
                ))
                .map(|_| Ok(None))?;
        }
    };
    res.set_time_type(TimeType::Date)?;
    Ok(Some(res))
}

#[rpn_fn()]
#[inline]
pub fn add_time_datetime_null(_arg0: &DateTime, _arg1: &DateTime) -> Result<Option<DateTime>> {
    Ok(None)
}

#[rpn_fn()]
#[inline]
pub fn add_time_duration_null(_arg0: &DateTime, _arg1: &DateTime) -> Result<Option<Duration>> {
    Ok(None)
}

#[rpn_fn()]
#[inline]
pub fn add_time_string_null(_arg0: &DateTime, _arg1: &DateTime) -> Result<Option<Bytes>> {
    Ok(None)
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn sub_duration_and_duration(
    ctx: &mut EvalContext,
    duration1: &Duration,
    duration2: &Duration,
) -> Result<Option<Duration>> {
    let result = match duration1.checked_sub(*duration2) {
        Some(result) => result,
        None => {
            return ctx
                .handle_invalid_time_error(Error::overflow(
                    "Duration",
                    format!("({} - {})", duration1, duration2),
                ))
                .map(|_| Ok(None))?;
        }
    };
    Ok(Some(result))
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn sub_datetime_and_duration(
    ctx: &mut EvalContext,
    datetime: &DateTime,
    duration: &Duration,
) -> Result<Option<DateTime>> {
    let mut res = match datetime.checked_sub(ctx, *duration) {
        Some(res) => res,
        None => {
            return ctx
                .handle_invalid_time_error(Error::overflow(
                    "DATETIME",
                    format!("({} - {})", datetime, duration),
                ))
                .map(|_| Ok(None))?;
        }
    };
    if res.set_time_type(TimeType::DateTime).is_err() {
        return Ok(None);
    }
    Ok(Some(res))
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn sub_datetime_and_string(
    ctx: &mut EvalContext,
    datetime: &DateTime,
    duration_str: BytesRef,
) -> Result<Option<DateTime>> {
    let duration_str = std::str::from_utf8(duration_str).map_err(Error::Encoding)?;
    let duration = match Duration::parse(ctx, duration_str, MAX_FSP) {
        Ok(duration) => duration,
        Err(_) => return Ok(None),
    };

    let res = match datetime.checked_sub(ctx, duration) {
        Some(res) => res,
        None => {
            return ctx
                .handle_invalid_time_error(Error::overflow(
                    "DATETIME",
                    format!("({} - {})", datetime, duration),
                ))
                .map(|_| Ok(None))?;
        }
    };
    Ok(Some(res))
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn sub_duration_and_string(
    ctx: &mut EvalContext,
    arg1: &Duration,
    arg2: BytesRef,
) -> Result<Option<Duration>> {
    let arg2 = std::str::from_utf8(arg2).map_err(Error::Encoding)?;
    let arg2 = match Duration::parse(ctx, arg2, MAX_FSP) {
        Ok(arg) => arg,
        Err(_) => return Ok(None),
    };

    let res = match arg1.checked_sub(arg2) {
        Some(res) => res,
        None => {
            return ctx
                .handle_invalid_time_error(Error::overflow(
                    "DURATION",
                    format!("({} - {})", arg1, arg2),
                ))
                .map(|_| Ok(None))?;
        }
    };
    Ok(Some(res))
}

#[rpn_fn(nullable)]
fn make_date_native(year: Option<&Int>, day: Option<&Int>) -> Result<Option<Bytes>> {
    let (Some(year), Some(day)) = (year, day) else {
        return Ok(None);
    };
    let mut year = *year;
    let day = *day;
    if day <= 0 || !(0..=9999).contains(&year) {
        return Ok(None);
    }
    if year < 70 {
        year += 2000;
    } else if year < 100 {
        year += 1900;
    }
    let (result_y, result_m, result_d) =
        Time::native_civil_from_days(Time::native_days_from_civil(year, 1, 1) + day - 1);
    if !(1..=9999).contains(&result_y) {
        return Ok(None);
    }
    Ok(Some(
        format!("{result_y:04}-{result_m:02}-{result_d:02}").into_bytes(),
    ))
}

#[rpn_fn(nullable)]
fn from_days_native(arg: Option<&Int>) -> Result<Option<Bytes>> {
    let Some(&n) = arg else {
        return Ok(None);
    };
    if (3_652_425..=3_652_499).contains(&n) {
        return Ok(None);
    }
    if !(366..=3_652_424).contains(&n) {
        // Actual canonical date text; the native result packer preserves its
        // original zero-Time datum kind without inspecting the input number.
        return Ok(Some(b"0000-00-00".to_vec()));
    }
    let (y, m, d) = Time::native_civil_from_days(n - 719_528);
    Ok(Some(format!("{y:04}-{m:02}-{d:02}").into_bytes()))
}

fn decode_duration_seconds_native(bytes: BytesRef) -> Result<f64> {
    if bytes.len() != 8 {
        return Err(other_err!(
            "Native duration seconds transport requires exactly 8 IEEE754 bytes"
        ));
    }
    let mut encoded = [0; 8];
    encoded.copy_from_slice(bytes);
    Ok(f64::from_bits(u64::from_le_bytes(encoded)))
}

#[rpn_fn(nullable)]
fn make_time_parts_native(
    hour: Option<BytesRef>,
    minute: Option<&Int>,
    second: Option<BytesRef>,
) -> Result<Option<Bytes>> {
    let (Some(hour), Some(minute), Some(second)) = (hour, minute, second) else {
        return Ok(None);
    };
    if hour.len() != 9 || hour[8] > 1 {
        return Err(other_err!(
            "Native MAKETIME hour transport requires LE i64 and a 0/1 unsigned flag"
        ));
    }
    let mut encoded_hour = [0; 8];
    encoded_hour.copy_from_slice(&hour[..8]);
    let hour_unsigned = hour[8] == 1;
    let mut hour = i64::from_le_bytes(encoded_hour);
    let minute = *minute;
    let second = decode_duration_seconds_native(second)?;
    if !(0..60).contains(&minute) || !(0.0..60.0).contains(&second) {
        return Ok(None);
    }
    let mut overflow = false;
    if hour < 0 && hour_unsigned {
        hour = 838;
        overflow = true;
    }
    let negative = hour < 0;
    let hour_abs = hour.unsigned_abs();
    if hour_abs > 838 || (hour_abs == 838 && minute == 59 && second > 59.0) {
        overflow = true;
    }
    let total = if overflow {
        838.0 * 3600.0 + 59.0 * 60.0 + 59.0
    } else {
        hour_abs as f64 * 3600.0 + minute as f64 * 60.0 + second
    };
    let seconds = if negative { -total } else { total };
    // This is the actual signed seconds value, never a validity marker. The
    // caller obtains FSP only after this complete worker call returns Some.
    Ok(Some(seconds.to_bits().to_le_bytes().to_vec()))
}

#[rpn_fn(nullable)]
fn sec_to_time_native(seconds: Option<BytesRef>, fsp: Option<&Int>) -> Result<Option<Bytes>> {
    match (seconds, fsp) {
        (None, None) => Ok(None),
        (Some(seconds), Some(fsp)) => {
            let seconds = decode_duration_seconds_native(seconds)?;
            let fsp = usize::try_from(*fsp).map_err(|_| {
                other_err!("Native duration FSP transport requires a nonnegative usize value")
            })?;
            Ok(Some(format_native_duration(seconds, fsp).into_bytes()))
        }
        _ => Err(other_err!(
            "Native duration seconds and demanded FSP must be jointly absent or present"
        )),
    }
}

fn format_native_duration(seconds: f64, fsp: usize) -> String {
    let sign = if seconds < 0.0 { "-" } else { "" };
    let max = 838.0 * 3600.0 + 59.0 * 60.0 + 59.0;
    let mut seconds = seconds.abs();
    if seconds > max {
        seconds = max;
    }
    let whole = seconds.trunc() as i64;
    let hour = whole / 3600;
    let minute = whole / 60 % 60;
    let mut second = whole % 60;
    if fsp == 0 {
        return format!("{sign}{hour:02}:{minute:02}:{second:02}");
    }
    // Go reaches this text through `fmt.Sprintf("%v", second)` followed by
    // `ParseDuration`, whose fraction rounding works on the DECIMAL DIGITS
    // and carries half-up at the requested precision
    // (`Duration.RoundFrac`: Go's time.Round rounds nearest values and sends
    // exact ties toward positive infinity).
    // Doing the arithmetic in f64 first re-derives 30.0000005 as
    // ...4999996µs and loses the digit -- so round off the SHORTEST decimal
    // rendering instead.
    // Go formats the REAL second value with %v (shortest repr): 30.1 stays
    // "30.1", 30.0000005 stays "30.0000005".
    let digits = format!("{seconds}");
    let mut digits_fraction = match digits.split_once('.') {
        Some((_, fraction)) => fraction.to_owned(),
        None => String::new(),
    };
    // Round half-up at the requested precision off the FIRST DISCARDED
    // digit, carrying into the whole part when the fraction overflows.
    let round_up = digits_fraction.len() > fsp && digits_fraction.as_bytes()[fsp] >= b'5';
    digits_fraction.truncate(fsp);
    while digits_fraction.len() < fsp {
        digits_fraction.push('0');
    }
    let mut fraction: i64 = digits_fraction.parse().unwrap_or(0);
    if round_up {
        fraction += 1;
        if fraction >= 10_i64.pow(fsp as u32) {
            fraction = 0;
            second += 1;
        }
    }
    format!("{sign}{hour:02}:{minute:02}:{second:02}.{fraction:0fsp$}")
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn from_days(ctx: &mut EvalContext, arg: &Int) -> Result<Option<DateTime>> {
    let time = DateTime::from_days(ctx, *arg as u32)?;
    Ok(Some(time))
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn make_date(ctx: &mut EvalContext, year: &Int, day: &Int) -> Result<Option<DateTime>> {
    let mut year = *year;
    let mut day = *day;
    if !(1..=366 * 9999).contains(&day) || !(0..=9999).contains(&year) {
        return Ok(None);
    }
    if year < 70 {
        year += 2000;
    } else if year < 100 {
        year += 1900;
    }
    year -= 1;
    let d4 = year / 4;
    let d100 = year / 100;
    let d400 = year / 400;
    let leap = d4 - d100 + d400;
    day = day + leap + year * 365 + 365;
    let days = day as u32;
    let ret = DateTime::from_days(ctx, days)?;
    if ret.year() > 9999 || ret.is_zero() {
        return Ok(None);
    }
    Ok(Some(ret))
}

#[rpn_fn(capture = [extra, args])]
#[inline]
pub fn make_time(
    extra: &RpnFnCallExtra,
    args: &[crate::RpnStackNode<'_>],
    hour: &Int,
    minute: &Int,
    second: &Real,
) -> Result<Option<Duration>> {
    let (is_negative, mut hour) = if args[0].field_type().is_unsigned() {
        (false, *hour as u64)
    } else {
        (hour.is_negative(), hour.wrapping_abs() as u64)
    };

    let nanosecond = second.fract().abs() * NANOS_PER_SEC as f64;
    let second = second.trunc();
    let minute = *minute;

    // Filter out the number that is negative or greater than `MAX_MINUTE_PART`.
    let mut minute = if 0 <= minute && minute <= MAX_MINUTE_PART.into() {
        minute as u32
    } else {
        return Ok(None);
    };

    // Filter out the number that is negative or greater than `MAX_SECOND_PART`.
    let mut second = if 0.0 <= second && second <= MAX_SECOND_PART.into() {
        second as u32
    } else {
        return Ok(None);
    };

    // Ensure that the nanosecond part is valid.
    debug_assert!(0.0 <= nanosecond && nanosecond.floor() <= MAX_NANOS_PART.into());
    let mut nanosecond = nanosecond as u32;

    let is_overflow = (hour, minute, second, nanosecond)
        > (MAX_HOUR_PART.into(), MAX_MINUTE_PART, MAX_SECOND_PART, 0);
    if is_overflow {
        hour = MAX_HOUR_PART.into();
        minute = MAX_MINUTE_PART;
        second = MAX_SECOND_PART;
        nanosecond = 0;
    }

    match Duration::new_from_parts(
        is_negative,
        hour as _,
        minute,
        second,
        nanosecond,
        extra.ret_field_type.get_decimal() as i8,
    ) {
        Ok(duration) => Ok(Some(duration)),

        // May encounter the fsp error
        Err(err) => Err(err.into()),
    }
}

#[rpn_fn(nullable)]
#[inline]
pub fn month(t: Option<&DateTime>) -> Result<Option<Int>> {
    t.map_or(Ok(None), |time| Ok(Some(Int::from(time.month()))))
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn month_name(ctx: &mut EvalContext, t: &DateTime) -> Result<Option<Bytes>> {
    if t.invalid_zero() {
        return ctx
            .handle_invalid_time_error(Error::incorrect_datetime_value(t))
            .map(|_| Ok(None))?;
    }
    let month = t.month();
    Ok(Some(
        Time::month_name_from_month(month).to_string().into_bytes(),
    ))
}

#[rpn_fn(nullable)]
#[inline]
pub fn hour(t: Option<&Duration>) -> Result<Option<Int>> {
    Ok(t.as_ref().map(|t| i64::from(t.hours())))
}

#[rpn_fn(nullable)]
#[inline]
pub fn minute(t: Option<&Duration>) -> Result<Option<Int>> {
    Ok(t.as_ref().map(|t| i64::from(t.minutes())))
}

#[rpn_fn(nullable)]
#[inline]
pub fn second(t: Option<&Duration>) -> Result<Option<Int>> {
    Ok(t.as_ref().map(|t| i64::from(t.secs())))
}

#[rpn_fn(nullable)]
#[inline]
pub fn time_to_sec(t: Option<&Duration>) -> Result<Option<Int>> {
    Ok(t.as_ref().map(|t| t.to_secs()))
}

#[rpn_fn(nullable)]
#[inline]
pub fn micro_second(t: Option<&Duration>) -> Result<Option<Int>> {
    Ok(t.as_ref().map(|t| i64::from(t.subsec_micros())))
}

#[rpn_fn(nullable, capture = [ctx])]
#[inline]
pub fn year(ctx: &mut EvalContext, t: Option<&DateTime>) -> Result<Option<Int>> {
    let t = match t {
        Some(v) => v,
        _ => return Ok(None),
    };

    if t.is_zero() {
        if ctx.cfg.sql_mode.contains(SqlMode::NO_ZERO_DATE) {
            return ctx
                .handle_invalid_time_error(Error::incorrect_datetime_value(t))
                .map(|_| Ok(None))?;
        }
        return Ok(Some(0));
    }
    Ok(Some(Int::from(t.year())))
}

#[rpn_fn(nullable, capture = [ctx])]
#[inline]
pub fn day_of_month(ctx: &mut EvalContext, t: Option<&DateTime>) -> Result<Option<Int>> {
    let t = match t {
        Some(v) => v,
        _ => return Ok(None),
    };

    if t.is_zero() {
        if ctx.cfg.sql_mode.contains(SqlMode::NO_ZERO_DATE) {
            return ctx
                .handle_invalid_time_error(Error::incorrect_datetime_value(t))
                .map(|_| Ok(None))?;
        }
        return Ok(Some(0));
    }
    Ok(Some(Int::from(t.day())))
}

#[rpn_fn(nullable, capture = [ctx])]
#[inline]
pub fn day_name(ctx: &mut EvalContext, t: Option<&DateTime>) -> Result<Option<Bytes>> {
    match t {
        Some(t) => {
            if t.invalid_zero() {
                return ctx
                    .handle_invalid_time_error(Error::incorrect_datetime_value(t))
                    .map(|_| Ok(None))?;
            }
            Ok(Some(t.weekday().name().to_string().into_bytes()))
        }
        None => Ok(None),
    }
}

#[rpn_fn(nullable)]
#[inline]
pub fn period_add(p: Option<&Int>, n: Option<&Int>) -> Result<Option<Int>> {
    Ok(match (p, n) {
        (Some(p), Some(n)) => {
            if *p == 0 {
                Some(0)
            } else {
                let (month, _) =
                    (i64::from(DateTime::period_to_month(*p as u64) as i32)).overflowing_add(*n);
                Some(DateTime::month_to_period(u64::from(month as u32)) as i64)
            }
        }
        _ => None,
    })
}

#[rpn_fn(nullable)]
#[inline]
pub fn period_diff(p1: Option<&Int>, p2: Option<&Int>) -> Result<Option<Int>> {
    match (p1, p2) {
        (Some(p1), Some(p2)) => Ok(Some(
            DateTime::period_to_month(*p1 as u64) as i64
                - DateTime::period_to_month(*p2 as u64) as i64,
        )),
        _ => Ok(None),
    }
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn last_day(ctx: &mut EvalContext, t: &DateTime) -> Result<Option<DateTime>> {
    if t.month() == 0 {
        return ctx
            .handle_invalid_time_error(Error::incorrect_datetime_value(t))
            .map(|_| Ok(None))?;
    }
    if t.day() == 0 {
        let one_day = Duration::parse(ctx, "1 00:00:00", 6).unwrap();
        let adjusted_t: DateTime = t.checked_add(ctx, one_day).unwrap();
        return Ok(adjusted_t.last_date_of_month());
    }
    Ok(t.last_date_of_month())
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn add_duration_and_duration(
    ctx: &mut EvalContext,
    duration1: &Duration,
    duration2: &Duration,
) -> Result<Option<Duration>> {
    let res = duration1.checked_add(*duration2);
    let res = match res {
        None => {
            return ctx
                .handle_invalid_time_error(Error::overflow(
                    "DURATION",
                    format!("({} + {})", duration1, duration2),
                ))
                .map(|_| Ok(None))?;
        }
        Some(res) => res,
    };
    Ok(Some(res))
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn add_duration_and_string(
    ctx: &mut EvalContext,
    arg1: &Duration,
    arg2: BytesRef,
) -> Result<Option<Duration>> {
    let arg2 = std::str::from_utf8(arg2).map_err(Error::Encoding)?;
    let arg2 = match Duration::parse(ctx, arg2, MAX_FSP) {
        Ok(arg) => arg,
        Err(_) => return Ok(None),
    };

    let res = match arg1.checked_add(arg2) {
        Some(res) => res,
        None => {
            return ctx
                .handle_invalid_time_error(Error::overflow(
                    "DURATION",
                    format!("({} + {})", arg1, arg2),
                ))
                .map(|_| Ok(None))?;
        }
    };
    Ok(Some(res))
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn duration_duration_time_diff(
    ctx: &mut EvalContext,
    arg1: &Duration,
    arg2: &Duration,
) -> Result<Option<Duration>> {
    let res = match arg1.checked_sub(*arg2) {
        Some(res) => res,
        // `check_sub` returns `None` if the sub operation overflow/underflow i64 bound or
        // mysql_time_value bound. and we need to treat these two case separately.
        // if `arg1 - arg2` is in (`MAX_NANOS`, `i64::MAX`], return max value of mysql `TIME` type.
        // if `arg1 - arg2` is in [`i64::MIN`, `-MAX_NANOS`), return min value of mysql `TIME` type.
        // if `arg1 - arg2` is overflow or underflow i64, return `None`.
        None if !arg1.is_neg() && arg1.to_nanos() - i64::MAX <= arg2.to_nanos() => {
            Duration::from_nanos(MAX_NANOS, arg1.fsp().max(arg2.fsp()) as i8)?
        }
        None if arg1.is_neg() && arg1.to_nanos() - i64::MIN >= arg2.to_nanos() => {
            Duration::from_nanos(-MAX_NANOS, arg1.fsp().max(arg2.fsp()) as i8)?
        }
        _ => {
            return ctx
                .handle_invalid_time_error(Error::overflow(
                    "DURATION",
                    format!("({} - {})", arg1, arg2),
                ))
                .map(|_| Ok(None))?;
        }
    };
    Ok(Some(res))
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn string_duration_time_diff(
    ctx: &mut EvalContext,
    arg1: BytesRef,
    arg2: &Duration,
) -> Result<Option<Duration>> {
    let arg1 = std::str::from_utf8(arg1).map_err(Error::Encoding)?;
    let arg1 = match Duration::parse(ctx, arg1, MAX_FSP) {
        Ok(arg) => arg,
        Err(_) => return Ok(None),
    };

    duration_duration_time_diff(ctx, &arg1, arg2)
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn string_string_time_diff(
    ctx: &mut EvalContext,
    arg1: BytesRef,
    arg2: BytesRef,
) -> Result<Option<Duration>> {
    let arg1 = std::str::from_utf8(arg1).map_err(Error::Encoding)?;
    let arg1 = match Duration::parse(ctx, arg1, MAX_FSP) {
        Ok(arg) => arg,
        Err(_) => return Ok(None),
    };

    let arg2 = std::str::from_utf8(arg2).map_err(Error::Encoding)?;
    let arg2 = match Duration::parse(ctx, arg2, MAX_FSP) {
        Ok(arg) => arg,
        Err(_) => return Ok(None),
    };

    duration_duration_time_diff(ctx, &arg1, &arg2)
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn duration_string_time_diff(
    ctx: &mut EvalContext,
    arg1: &Duration,
    arg2: BytesRef,
) -> Result<Option<Duration>> {
    let arg2 = std::str::from_utf8(arg2).map_err(Error::Encoding)?;
    let arg2 = match Duration::parse(ctx, arg2, MAX_FSP) {
        Ok(arg) => arg,
        Err(_) => return Ok(None),
    };

    duration_duration_time_diff(ctx, arg1, &arg2)
}

#[rpn_fn]
#[inline]
pub fn quarter(t: &DateTime) -> Result<Option<Int>> {
    Ok(Some(Int::from(t.quarter())))
}

/// Cast Duration into string representation and drop subsec if possible.
fn duration_to_string(duration: Duration) -> String {
    match duration.subsec_micros() {
        0 => duration.minimize_fsp().to_string(),
        _ => duration.maximize_fsp().to_string(),
    }
}

/// Cast DateTime into string representation and drop subsec if possible.
fn datetime_to_string(mut datetime: DateTime) -> String {
    match datetime.micro() {
        0 => datetime.minimize_fsp(),
        _ => datetime.maximize_fsp(),
    };
    datetime.to_string()
}

pub struct AddSubDateMeta {
    unit: IntervalUnit,
    is_clock_unit: bool,
    interval_unsigned: bool,
    interval_decimal: isize,
}

fn build_add_sub_date_meta(expr: &mut crate::types::function::CallBuild) -> Result<AddSubDateMeta> {
    let children = expr.args();
    if children.len() != 3 {
        return Err(box_err!("wrong add/sub_date expr size {}", children.len()));
    }
    let Some(unit_bytes) = children[2].constant_bytes() else {
        return Err(box_err!(
            "unknown unit type {}",
            children[2].source_type_name()
        ));
    };
    let unit_str = std::str::from_utf8(unit_bytes).map_err(Error::Encoding)?;
    let unit = IntervalUnit::from_str(unit_str)?;
    let is_clock_unit = unit.is_clock_unit();
    let interval_unsigned = children[1]
        .field_type()
        .as_accessor()
        .flag()
        .contains(FieldTypeFlag::UNSIGNED);
    let interval_decimal = children[1].field_type().decimal();

    Ok(AddSubDateMeta {
        unit,
        is_clock_unit,
        interval_unsigned,
        interval_decimal,
    })
}

pub trait AddSubDateConvertToTime {
    fn to_time(&self, ctx: &mut EvalContext, metadata: &AddSubDateMeta) -> CodecResult<Time>;
}

impl AddSubDateConvertToTime for BytesRef<'_> {
    #[inline]
    fn to_time(&self, ctx: &mut EvalContext, metadata: &AddSubDateMeta) -> CodecResult<Time> {
        let input = std::str::from_utf8(self).map_err(Error::Encoding)?;
        let mut t = Time::parse_without_type(ctx, input, MAX_FSP, true)?;
        if metadata.is_clock_unit {
            t.set_time_type(TimeType::DateTime)?;
        }
        Ok(t)
    }
}

impl AddSubDateConvertToTime for i64 {
    #[inline]
    fn to_time(&self, ctx: &mut EvalContext, metadata: &AddSubDateMeta) -> CodecResult<Time> {
        let mut t = Time::parse_from_i64_default(ctx, *self)?;
        if metadata.is_clock_unit {
            t.set_time_type(TimeType::DateTime)?;
        }
        Ok(t)
    }
}

impl AddSubDateConvertToTime for Real {
    #[inline]
    fn to_time(&self, ctx: &mut EvalContext, metadata: &AddSubDateMeta) -> CodecResult<Time> {
        let mut t = Time::parse_from_real_default(ctx, self)?;
        if metadata.is_clock_unit {
            t.set_time_type(TimeType::DateTime)?;
        }
        Ok(t)
    }
}

impl AddSubDateConvertToTime for Decimal {
    #[inline]
    fn to_time(&self, ctx: &mut EvalContext, metadata: &AddSubDateMeta) -> CodecResult<Time> {
        let mut t = Time::parse_from_decimal_default(ctx, self)?;
        if metadata.is_clock_unit {
            t.set_time_type(TimeType::DateTime)?;
        }
        Ok(t)
    }
}

impl AddSubDateConvertToTime for DateTime {
    #[inline]
    fn to_time(&self, _ctx: &mut EvalContext, metadata: &AddSubDateMeta) -> CodecResult<Time> {
        let mut t = *self;
        if metadata.is_clock_unit || self.get_time_type() == TimeType::Timestamp {
            t.set_time_type(TimeType::DateTime)?;
        }
        Ok(t)
    }
}

fn add_date(
    ctx: &mut EvalContext,
    mut datetime: DateTime,
    interval: &Interval,
    result_fsp: i8,
) -> CodecResult<DateTime> {
    let month = interval.month();
    let sec = interval.sec();
    let nano = interval.nano();

    datetime = datetime.add_sec_nanos(ctx, sec, nano)?;
    if month != 0 {
        datetime.add_months(month)?;
    }

    if let Ok(fsp) = check_fsp(result_fsp) {
        datetime.set_fsp(fsp);
    }

    Ok(datetime)
}

fn sub_date(
    ctx: &mut EvalContext,
    datetime: DateTime,
    interval: &Interval,
    result_fsp: i8,
) -> CodecResult<DateTime> {
    add_date(ctx, datetime, &interval.negate(), result_fsp)
}

#[inline]
fn add_sub_date_time_any_interval_any_as_string<
    T: AddSubDateConvertToTime,
    I: ConvertToIntervalStr,
    F: Fn(&mut EvalContext, DateTime, &Interval, i8) -> CodecResult<DateTime>,
>(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    time: &T,
    interval: &I,
    op: F,
) -> Result<Option<Bytes>> {
    let datetime = match time.to_time(ctx, metadata) {
        Ok(d) => d,
        Err(e) => return ctx.handle_invalid_time_error(e).map(|_| Ok(None))?,
    };
    let interval_str = interval.to_interval_string(
        ctx,
        metadata.unit,
        metadata.interval_unsigned,
        metadata.interval_decimal,
    )?;
    let interval = match Interval::parse_from_str(ctx, &metadata.unit, &interval_str)? {
        Some(i) => i,
        None => return Ok(None),
    };
    let mut date_res = match op(
        ctx,
        datetime,
        &interval,
        extra.ret_field_type.get_decimal() as i8,
    ) {
        Ok(d) => d,
        Err(e) => return ctx.handle_invalid_time_error(e).map(|_| Ok(None))?,
    };
    if date_res.micro() == 0 {
        date_res.minimize_fsp();
    } else {
        date_res.maximize_fsp();
    }

    Ok(Some(date_res.to_string().into_bytes()))
}

#[inline]
fn add_sub_date_time_datetime_interval_any_as_datetime<
    I: ConvertToIntervalStr,
    F: Fn(&mut EvalContext, DateTime, &Interval, i8) -> CodecResult<DateTime>,
>(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    time: &DateTime,
    interval: &I,
    op: F,
) -> Result<Option<DateTime>> {
    let datetime = match time.to_time(ctx, metadata) {
        Ok(d) => d,
        Err(e) => return ctx.handle_invalid_time_error(e).map(|_| Ok(None))?,
    };
    let interval_str = interval.to_interval_string(
        ctx,
        metadata.unit,
        metadata.interval_unsigned,
        metadata.interval_decimal,
    )?;
    let interval = match Interval::parse_from_str(ctx, &metadata.unit, &interval_str)? {
        Some(i) => i,
        None => return Ok(None),
    };
    let date_res = match op(
        ctx,
        datetime,
        &interval,
        extra.ret_field_type.get_decimal() as i8,
    ) {
        Ok(d) => d,
        Err(e) => return ctx.handle_invalid_time_error(e).map(|_| Ok(None))?,
    };

    Ok(Some(date_res))
}

#[inline]
fn add_sub_date_time_duration_interval_any_as_datetime<
    I: ConvertToIntervalStr,
    F: Fn(&mut EvalContext, DateTime, &Interval, i8) -> CodecResult<DateTime>,
>(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    dur: &Duration,
    interval: &I,
    op: F,
) -> Result<Option<DateTime>> {
    let datetime = DateTime::from_duration(ctx, *dur, TimeType::DateTime)?;
    let interval_str = interval.to_interval_string(
        ctx,
        metadata.unit,
        metadata.interval_unsigned,
        metadata.interval_decimal,
    )?;
    let interval = match Interval::parse_from_str(ctx, &metadata.unit, &interval_str)? {
        Some(i) => i,
        None => return Ok(None),
    };
    let date_res = match op(
        ctx,
        datetime,
        &interval,
        extra.ret_field_type.get_decimal() as i8,
    ) {
        Ok(d) => d,
        Err(e) => return ctx.handle_invalid_time_error(e).map(|_| Ok(None))?,
    };

    Ok(Some(date_res))
}

#[inline]
fn add_sub_date_time_duration_interval_any_as_duration<
    I: ConvertToIntervalStr,
    F: Fn(Duration, Duration) -> Option<Duration>,
>(
    ctx: &mut EvalContext,
    metadata: &AddSubDateMeta,
    dur: &Duration,
    interval: &I,
    op: F,
) -> Result<Option<Duration>> {
    let interval_str = interval.to_interval_string(
        ctx,
        metadata.unit,
        metadata.interval_unsigned,
        metadata.interval_decimal,
    )?;
    let interval = match Interval::extract_duration(ctx, &metadata.unit, &interval_str) {
        Ok(d) => d,
        Err(e) => return ctx.handle_invalid_time_error(e).map(|_| Ok(None))?,
    };
    let dur_res = match op(*dur, interval) {
        Some(d) => d,
        None => {
            return ctx
                .handle_invalid_time_error(Error::overflow(
                    "Duration",
                    format!("({} - {})", dur, interval),
                ))
                .map(|_| Ok(None))?;
        }
    };

    Ok(Some(dur_res))
}

#[rpn_fn(capture = [ctx, extra, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn add_date_time_any_interval_any_as_string<
    T: AddSubDateConvertToTime + Evaluable + EvaluableRet,
    I: ConvertToIntervalStr + Evaluable + EvaluableRet,
>(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    time: &T,
    interval: &I,
    _unit: BytesRef,
) -> Result<Option<Bytes>> {
    add_sub_date_time_any_interval_any_as_string(ctx, extra, metadata, time, interval, add_date)
}

#[rpn_fn(capture = [ctx, extra, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn sub_date_time_any_interval_any_as_string<
    T: AddSubDateConvertToTime + Evaluable + EvaluableRet,
    I: ConvertToIntervalStr + Evaluable + EvaluableRet,
>(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    time: &T,
    interval: &I,
    _unit: BytesRef,
) -> Result<Option<Bytes>> {
    add_sub_date_time_any_interval_any_as_string(ctx, extra, metadata, time, interval, sub_date)
}

#[rpn_fn(capture = [ctx, extra, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn add_date_time_string_interval_string_as_string(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    time: BytesRef,
    interval: BytesRef,
    _unit: BytesRef,
) -> Result<Option<Bytes>> {
    add_sub_date_time_any_interval_any_as_string(ctx, extra, metadata, &time, &interval, add_date)
}

#[rpn_fn(capture = [ctx, extra, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn sub_date_time_string_interval_string_as_string(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    time: BytesRef,
    interval: BytesRef,
    _unit: BytesRef,
) -> Result<Option<Bytes>> {
    add_sub_date_time_any_interval_any_as_string(ctx, extra, metadata, &time, &interval, sub_date)
}

#[rpn_fn(capture = [ctx, extra, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn add_date_time_string_interval_any_as_string<
    I: ConvertToIntervalStr + Evaluable + EvaluableRet,
>(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    time: BytesRef,
    interval: &I,
    _unit: BytesRef,
) -> Result<Option<Bytes>> {
    add_sub_date_time_any_interval_any_as_string(ctx, extra, metadata, &time, interval, add_date)
}

#[rpn_fn(capture = [ctx, extra, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn sub_date_time_string_interval_any_as_string<
    I: ConvertToIntervalStr + Evaluable + EvaluableRet,
>(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    time: BytesRef,
    interval: &I,
    _unit: BytesRef,
) -> Result<Option<Bytes>> {
    add_sub_date_time_any_interval_any_as_string(ctx, extra, metadata, &time, interval, sub_date)
}

#[rpn_fn(capture = [ctx, extra, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn add_date_time_any_interval_string_as_string<
    T: AddSubDateConvertToTime + Evaluable + EvaluableRet,
>(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    time: &T,
    interval: BytesRef,
    _unit: BytesRef,
) -> Result<Option<Bytes>> {
    add_sub_date_time_any_interval_any_as_string(ctx, extra, metadata, time, &interval, add_date)
}

#[rpn_fn(capture = [ctx, extra, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn sub_date_time_any_interval_string_as_string<
    T: AddSubDateConvertToTime + Evaluable + EvaluableRet,
>(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    time: &T,
    interval: BytesRef,
    _unit: BytesRef,
) -> Result<Option<Bytes>> {
    add_sub_date_time_any_interval_any_as_string(ctx, extra, metadata, time, &interval, sub_date)
}

#[rpn_fn(capture = [ctx, extra, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn add_date_time_datetime_interval_string_as_datetime(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    time: &DateTime,
    interval: BytesRef,
    _unit: BytesRef,
) -> Result<Option<DateTime>> {
    add_sub_date_time_datetime_interval_any_as_datetime(
        ctx, extra, metadata, time, &interval, add_date,
    )
}

#[rpn_fn(capture = [ctx, extra, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn sub_date_time_datetime_interval_string_as_datetime(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    time: &DateTime,
    interval: BytesRef,
    _unit: BytesRef,
) -> Result<Option<DateTime>> {
    add_sub_date_time_datetime_interval_any_as_datetime(
        ctx, extra, metadata, time, &interval, sub_date,
    )
}

#[rpn_fn(capture = [ctx, extra, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn add_date_time_datetime_interval_any_as_datetime<
    I: ConvertToIntervalStr + Evaluable + EvaluableRet,
>(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    time: &DateTime,
    interval: &I,
    _unit: BytesRef,
) -> Result<Option<DateTime>> {
    add_sub_date_time_datetime_interval_any_as_datetime(
        ctx, extra, metadata, time, interval, add_date,
    )
}

#[rpn_fn(capture = [ctx, extra, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn sub_date_time_datetime_interval_any_as_datetime<
    I: ConvertToIntervalStr + Evaluable + EvaluableRet,
>(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    time: &DateTime,
    interval: &I,
    _unit: BytesRef,
) -> Result<Option<DateTime>> {
    add_sub_date_time_datetime_interval_any_as_datetime(
        ctx, extra, metadata, time, interval, sub_date,
    )
}

#[rpn_fn(capture = [ctx, extra, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn add_date_time_duration_interval_string_as_datetime(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    dur: &Duration,
    interval: BytesRef,
    _unit: BytesRef,
) -> Result<Option<DateTime>> {
    add_sub_date_time_duration_interval_any_as_datetime(
        ctx, extra, metadata, dur, &interval, add_date,
    )
}

#[rpn_fn(capture = [ctx, extra, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn sub_date_time_duration_interval_string_as_datetime(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    dur: &Duration,
    interval: BytesRef,
    _unit: BytesRef,
) -> Result<Option<DateTime>> {
    add_sub_date_time_duration_interval_any_as_datetime(
        ctx, extra, metadata, dur, &interval, sub_date,
    )
}

#[rpn_fn(capture = [ctx, extra, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn add_date_time_duration_interval_any_as_datetime<
    I: ConvertToIntervalStr + Evaluable + EvaluableRet,
>(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    dur: &Duration,
    interval: &I,
    _unit: BytesRef,
) -> Result<Option<DateTime>> {
    add_sub_date_time_duration_interval_any_as_datetime(
        ctx, extra, metadata, dur, interval, add_date,
    )
}

#[rpn_fn(capture = [ctx, extra, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn sub_date_time_duration_interval_any_as_datetime<
    I: ConvertToIntervalStr + Evaluable + EvaluableRet,
>(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    metadata: &AddSubDateMeta,
    dur: &Duration,
    interval: &I,
    _unit: BytesRef,
) -> Result<Option<DateTime>> {
    add_sub_date_time_duration_interval_any_as_datetime(
        ctx, extra, metadata, dur, interval, sub_date,
    )
}

#[rpn_fn(capture = [ctx, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn add_date_time_duration_interval_string_as_duration(
    ctx: &mut EvalContext,
    metadata: &AddSubDateMeta,
    dur: &Duration,
    interval: BytesRef,
    _unit: BytesRef,
) -> Result<Option<Duration>> {
    add_sub_date_time_duration_interval_any_as_duration(
        ctx,
        metadata,
        dur,
        &interval,
        Duration::checked_add,
    )
}

#[rpn_fn(capture = [ctx, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn sub_date_time_duration_interval_string_as_duration(
    ctx: &mut EvalContext,
    metadata: &AddSubDateMeta,
    dur: &Duration,
    interval: BytesRef,
    _unit: BytesRef,
) -> Result<Option<Duration>> {
    add_sub_date_time_duration_interval_any_as_duration(
        ctx,
        metadata,
        dur,
        &interval,
        Duration::checked_sub,
    )
}

#[rpn_fn(capture = [ctx, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn add_date_time_duration_interval_any_as_duration<
    I: ConvertToIntervalStr + Evaluable + EvaluableRet,
>(
    ctx: &mut EvalContext,
    metadata: &AddSubDateMeta,
    dur: &Duration,
    interval: &I,
    _unit: BytesRef,
) -> Result<Option<Duration>> {
    add_sub_date_time_duration_interval_any_as_duration(
        ctx,
        metadata,
        dur,
        interval,
        Duration::checked_add,
    )
}

#[rpn_fn(capture = [ctx, metadata], metadata_mapper = build_add_sub_date_meta)]
#[inline]
pub fn sub_date_time_duration_interval_any_as_duration<
    I: ConvertToIntervalStr + Evaluable + EvaluableRet,
>(
    ctx: &mut EvalContext,
    metadata: &AddSubDateMeta,
    dur: &Duration,
    interval: &I,
    _unit: BytesRef,
) -> Result<Option<Duration>> {
    add_sub_date_time_duration_interval_any_as_duration(
        ctx,
        metadata,
        dur,
        interval,
        Duration::checked_sub,
    )
}

#[rpn_fn(capture = [ctx, extra])]
#[inline]
pub fn from_unixtime_1_arg(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    arg0: &Decimal,
) -> Result<Option<DateTime>> {
    eval_from_unixtime(ctx, extra.ret_field_type.get_decimal() as i8, arg0.clone())
}

#[rpn_fn(capture = [ctx, extra])]
#[inline]
pub fn from_unixtime_2_arg(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    arg0: &Decimal,
    arg1: BytesRef,
) -> Result<Option<Bytes>> {
    let t = eval_from_unixtime(ctx, extra.ret_field_type.get_decimal() as i8, arg0.clone())?;
    match t {
        Some(t) => {
            let res = t.date_format(std::str::from_utf8(arg1).map_err(Error::Encoding)?)?;
            Ok(Some(res.into()))
        }
        None => Ok(None),
    }
}

// Port from TiDB's evalFromUnixTime
pub fn eval_from_unixtime(
    ctx: &mut EvalContext,
    mut fsp: i8,
    unix_timestamp: Decimal,
) -> Result<Option<DateTime>> {
    // 0 <= unixTimeStamp <= 32536771199.999999
    if unix_timestamp.is_negative() {
        return Ok(None);
    }
    let integral_part = unix_timestamp.as_i64().unwrap(); // Ignore Truncated error and Overflow error
    // The max integralPart should not be larger than 32536771199.
    // Refer to https://dev.mysql.com/doc/relnotes/mysql/8.0/en/news-8-0-28.html
    if integral_part > 32536771199 {
        return Ok(None);
    }
    // Split the integral part and fractional part of a decimal timestamp.
    // e.g. for timestamp 12345.678,
    // first get the integral part 12345,
    // then (12345.678 - 12345) * (10^9) to get the decimal part and convert it to
    // nanosecond precision.
    let integer_decimal_tp = Decimal::from(integral_part);
    let frac_decimal_tp = &unix_timestamp - &integer_decimal_tp;
    if !frac_decimal_tp.is_ok() {
        return Ok(None);
    }
    let frac_decimal_tp = frac_decimal_tp.unwrap();
    let nano = Decimal::from(NANOS_PER_SEC);
    let x = &frac_decimal_tp * &nano;
    if x.is_overflow() {
        return Err(Error::overflow("DECIMAL", "").into());
    }
    if x.is_truncated() {
        return Err(Error::truncated().into());
    }
    let x = x.unwrap();
    let fractional_part = x.as_i64(); // here fractionalPart is result multiplying the original fractional part by 10^9.
    if fractional_part.is_overflow() {
        return Err(Error::overflow("DECIMAL", "").into());
    }
    let fractional_part = fractional_part.unwrap();
    if fsp < 0 {
        fsp = MAX_FSP;
    }
    let tmp = DateTime::from_unixtime(
        ctx,
        integral_part,
        fractional_part as u32,
        TimeType::DateTime,
        fsp,
    )?;
    Ok(Some(tmp))
}

fn find_zone_transition(
    t: chrono::DateTime<chrono_tz::Tz>,
) -> Result<chrono::DateTime<chrono_tz::Tz>> {
    let mut t1 = t - chrono::Duration::hours(12);
    let mut t2 = t + chrono::Duration::hours(12);

    let offset1 = t1.offset().fix();
    let offset2 = t2.offset().fix();
    if offset1 == offset2 {
        return Err(
            Error::incorrect_datetime_value("offset1 == offset2 in find_zone_transition").into(),
        );
    }

    let mut i = 0;
    while t1 + chrono::Duration::seconds(1) < t2 {
        if i > 100 {
            // Just avoid infinity loop because of potential bug, maybe we can remove it in
            // the future
            return Err(
                Error::incorrect_datetime_value("Encounter infinity loop caused by bug").into(),
            );
        }
        i += 1;

        let t3_res = (t1 + ((t2 - t1) / 2)).duration_round(chrono::Duration::seconds(1));
        let t3 = match t3_res {
            Ok(val) => val,
            Err(err) => return Err(Error::incorrect_datetime_value(err).into()),
        };
        let offset = t3.offset().fix();
        if offset == offset1 {
            t1 = t3;
        } else {
            t2 = t3;
        }
    }
    Ok(t2)
}

// unix_timestamp_to_mysql_unix_timestamp converts micto timestamp into MySQL's
// Unix timestamp. MySQL's Unix timestamp ranges from '1970-01-01
// 00:00:01.000000' UTC to '3001-01-18 23:59:59.999999' UTC. Values out of range
// should be rewritten to 0. https://dev.mysql.com/doc/refman/8.0/en/date-and-time-functions.html#function_unix-timestamp
fn unix_timestamp_to_mysql_unix_timestamp(
    ctx: &mut EvalContext,
    micro_time: i64,
    frac: i8,
) -> Result<Decimal> {
    if !(1000000..=32536771199999999).contains(&micro_time) {
        return Ok(Decimal::zero());
    }

    let time_in_decimal = Decimal::from(micro_time);
    let value: std::result::Result<Decimal, Error> = time_in_decimal
        .shift(-6)
        .into_result(ctx)?
        .round(frac, RoundMode::Truncate)
        .into();
    Ok(value?)
}

fn get_micro_timestamp(time: &DateTime, tz: &Tz) -> Result<i64> {
    let year = time.year() as i32;
    let month = time.month();
    let day = time.day();
    let hour = time.hour();
    let minute = time.minute();
    let second = time.second();
    let naive_date = match chrono::NaiveDate::from_ymd_opt(year, month, day) {
        Some(v) => v,
        None => return Ok(0),
    };
    let naive_time = match chrono::NaiveTime::from_hms_micro_opt(hour, minute, second, time.micro())
    {
        Some(v) => v,
        None => return Ok(0),
    };
    let naive_datetime = chrono::NaiveDateTime::new(naive_date, naive_time);

    // MySQL chooses earliest time when there are multi mapped time
    let res = match tz.from_local_datetime(&naive_datetime).earliest() {
        Some(val) => val,
        None => {
            let chrono_tz = match tz.get_chrono_tz() {
                Some(val) => val,
                None => return Err(Error::incorrect_parameters("Can't get chrono tz").into()),
            };

            // year, month, day, hour, minute and second is enough
            let time_with_tz = chrono::Utc
                .with_ymd_and_hms(year, month, day, hour, minute, second)
                .unwrap()
                .with_timezone(&chrono_tz);
            match find_zone_transition(time_with_tz) {
                Ok(val) => return Ok(val.naive_utc().and_utc().timestamp_micros()),
                Err(err) => return Err(err),
            }
        }
    };
    Ok(res.naive_utc().and_utc().timestamp_micros())
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn unix_timestamp_int(ctx: &mut EvalContext, time: &DateTime) -> Result<Option<i64>> {
    let timestamp = get_micro_timestamp(time, &ctx.cfg.tz)?;

    let res: std::result::Result<i64, Error> =
        unix_timestamp_to_mysql_unix_timestamp(ctx, timestamp, 1)?
            .as_i64()
            .into();
    Ok(Some(res?))
}

#[rpn_fn(capture = [ctx, extra])]
#[inline]
pub fn unix_timestamp_decimal(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    time: &DateTime,
) -> Result<Option<Decimal>> {
    let timestamp = get_micro_timestamp(time, &ctx.cfg.tz)?;

    let res = unix_timestamp_to_mysql_unix_timestamp(
        ctx,
        timestamp,
        extra.ret_field_type.get_decimal() as i8,
    )?;
    Ok(Some(res))
}

fn build_timestamp_diff_meta(expr: &mut crate::types::function::CallBuild) -> Result<IntervalUnit> {
    let children = expr.args();
    if children.len() != 3 {
        return Err(box_err!(
            "wrong timestamp_diff expr size {}",
            children.len()
        ));
    }
    let Some(unit_bytes) = children[0].constant_bytes() else {
        return Err(box_err!(
            "unknown unit type {}",
            children[0].source_type_name()
        ));
    };
    let unit_str = std::str::from_utf8(unit_bytes).map_err(Error::Encoding)?;
    let unit = IntervalUnit::from_str(unit_str)?;
    if !unit.is_valid_for_timestamp() {
        return Err(box_err!("wrong unit {:?} for timestamp_diff", unit));
    }
    Ok(unit)
}

/// See https://dev.mysql.com/doc/refman/5.7/en/date-and-time-functions.html#function_timestampdiff
#[rpn_fn(capture = [ctx, metadata], metadata_mapper = build_timestamp_diff_meta)]
#[inline]
pub fn timestamp_diff(
    ctx: &mut EvalContext,
    metadata: &IntervalUnit,
    _unit: BytesRef,
    time1: &DateTime,
    time2: &DateTime,
) -> Result<Option<i64>> {
    if time1.invalid_zero() {
        return ctx
            .handle_invalid_time_error(Error::incorrect_datetime_value(time1))
            .map(|_| Ok(None))?;
    }
    if time2.invalid_zero() {
        return ctx
            .handle_invalid_time_error(Error::incorrect_datetime_value(time2))
            .map(|_| Ok(None))?;
    }
    time1
        .timestamp_diff(time2, *metadata)
        .map(Some)
        .or_else(|e| ctx.handle_invalid_time_error(e).map(|_| Ok(None))?)
}

#[rpn_fn(capture = [ctx])]
#[inline]
pub fn str_to_date_date(
    ctx: &mut EvalContext,
    date: BytesRef,
    format: BytesRef,
) -> Result<Option<DateTime>> {
    let (succ, mut t) =
        Time::parse_from_string_with_format(ctx, from_utf8(date)?, from_utf8(format)?);
    if !succ {
        ctx.handle_invalid_time_error(Error::truncated_wrong_val("DATETIME", t))?;
        return Ok(None);
    }
    if ctx.cfg.sql_mode.contains(SqlMode::NO_ZERO_DATE)
        && (t.year() == 0 || t.month() == 0 || t.day() == 0)
    {
        ctx.handle_invalid_time_error(Error::truncated_wrong_val("DATETIME", t))?;
        return Ok(None);
    }
    t.set_time_type(TimeType::Date)?;
    t.set_fsp(0);
    Ok(Some(t))
}

#[rpn_fn(capture = [ctx, extra])]
#[inline]
pub fn str_to_date_datetime(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    date: BytesRef,
    format: BytesRef,
) -> Result<Option<DateTime>> {
    let (succ, mut t) =
        Time::parse_from_string_with_format(ctx, from_utf8(date)?, from_utf8(format)?);
    if !succ {
        ctx.handle_invalid_time_error(Error::truncated_wrong_val("DATETIME", t))?;
        return Ok(None);
    }
    if ctx.cfg.sql_mode.contains(SqlMode::NO_ZERO_DATE)
        && (t.year() == 0 || t.month() == 0 || t.day() == 0)
    {
        ctx.handle_invalid_time_error(Error::truncated_wrong_val("DATETIME", t))?;
        return Ok(None);
    }
    t.set_time_type(TimeType::DateTime)?;
    t.set_fsp(extra.ret_field_type.get_decimal() as u8);
    Ok(Some(t))
}

#[rpn_fn(capture = [ctx, extra])]
#[inline]
pub fn str_to_date_duration(
    ctx: &mut EvalContext,
    extra: &RpnFnCallExtra,
    date: BytesRef,
    format: BytesRef,
) -> Result<Option<Duration>> {
    let (succ, mut t) =
        Time::parse_from_string_with_format(ctx, from_utf8(date)?, from_utf8(format)?);
    if !succ {
        ctx.handle_invalid_time_error(Error::truncated_wrong_val("DATETIME", t))?;
        return Ok(None);
    }
    t.set_fsp(extra.ret_field_type.get_decimal() as u8);
    let duration: Duration = t.convert(ctx)?;
    Ok(Some(duration))
}

#[cfg(test)]
mod native_date_core_worker_tests {
    use super::*;

    #[test]
    fn fixed_date_core_workers_preserve_raw_zero_modes_bits_and_legacy_predicate() {
        assert_eq!(date_core_native_fn_meta().name, "date_core_native");
        assert_eq!(
            date_core_predicate_legacy_fn_meta().name,
            "date_core_predicate_legacy"
        );
        let zero = 0u64.to_le_bytes();
        let reserved_only = 1u64.to_le_bytes();
        let clock_only = (1u64 << 36).to_le_bytes();
        let high = u64::MAX.to_le_bytes();
        let zero_results = [Some(0), None, Some(0), None, Some(0), None, Some(0), None];
        let hidden_results = [Some(0), Some(0), None, None, Some(0), Some(0), None, None];
        for modes in 0..=7i64 {
            assert!(date_core_native_args_valid(Some(&high), Some(modes)));
            // Even invalid stored calendar fields remain projections, and the
            // computed high bit must survive transport as signed integer bits.
            assert_eq!(
                date_core_native(&high, &modes).unwrap(),
                Some(-2_199_023_255_552)
            );
            assert_eq!(
                date_core_native(&zero, &modes).unwrap(),
                zero_results[modes as usize]
            );
            assert_eq!(
                date_core_native(&reserved_only, &modes).unwrap(),
                hidden_results[modes as usize]
            );
            assert_eq!(
                date_core_native(&clock_only, &modes).unwrap(),
                hidden_results[modes as usize]
            );
        }
        let date = (2024u64 << 50) | (2u64 << 46) | (29u64 << 41);
        let with_clock = (date | (23u64 << 36) | 15).to_le_bytes();
        assert_eq!(
            date_core_native(&with_clock, &7).unwrap(),
            Some(date as i64)
        );
        let year_only = (2024u64 << 50).to_le_bytes();
        assert_eq!(
            date_core_native(&year_only, &1).unwrap(),
            Some((2024u64 << 50) as i64)
        );
        assert_eq!(date_core_native(&year_only, &2).unwrap(), None);
        assert_eq!(date_core_predicate_legacy(None).unwrap(), None);
        assert_eq!(date_core_predicate_legacy(Some(&zero)).unwrap(), Some(0));
        assert_eq!(
            date_core_predicate_legacy(Some(&reserved_only)).unwrap(),
            Some(0)
        );
        assert_eq!(
            date_core_predicate_legacy(Some(&clock_only)).unwrap(),
            Some(0)
        );
        assert_eq!(date_core_predicate_legacy(Some(&high)).unwrap(), Some(1));
        assert_eq!(
            date_core_predicate_legacy(Some(&year_only)).unwrap(),
            Some(1)
        );
        assert!(!date_core_native_args_valid(None, Some(0)));
        assert!(!date_core_native_args_valid(Some(&zero), None));
        for invalid in [Vec::new(), vec![0; 7], vec![0; 9]] {
            assert!(!date_core_native_args_valid(Some(&invalid), Some(0)));
            assert!(date_core_native(&invalid, &0).is_err());
            assert!(date_core_predicate_legacy(Some(&invalid)).is_err());
        }
        for modes in [-1, 8, i64::MIN, i64::MAX] {
            assert!(!date_core_native_args_valid(Some(&zero), Some(modes)));
            assert!(date_core_native(&zero, &modes).is_err());
        }
    }
}

#[cfg(test)]
mod native_clock_worker_tests {
    use super::*;

    fn clock_packet(secs: i64, nanos: u32, offset: i32) -> [u8; 16] {
        let mut packet = [0; 16];
        packet[..8].copy_from_slice(&secs.to_le_bytes());
        packet[8..12].copy_from_slice(&nanos.to_le_bytes());
        packet[12..16].copy_from_slice(&offset.to_le_bytes());
        packet
    }

    #[test]
    fn fixed_now_current_date_sysdate_workers_preserve_clock_policies() {
        for (meta, name) in [
            (now_native_fn_meta(), "now_native"),
            (current_date_native_fn_meta(), "current_date_native"),
            (sysdate_native_fn_meta(), "sysdate_native"),
        ] {
            assert_eq!(meta.name, name);
        }
        let packet = clock_packet(86_399, 999_999_600, 3_600);
        assert_eq!(
            now_native(&packet, &0).unwrap(),
            Some(b"1970-01-02 00:59:59".to_vec())
        );
        assert_eq!(
            now_native(&packet, &6).unwrap(),
            Some(b"1970-01-02 00:59:59.999999".to_vec())
        );
        assert_eq!(
            current_date_native(&packet).unwrap(),
            Some(b"1970-01-02".to_vec())
        );
        // SYSDATE's actual pure policy rounds, unlike NOW's truncation. The
        // sysdate_is_now caller selects the NOW profile rather than changing
        // this kernel or injecting an operation/rounding flag into its frame.
        assert_eq!(
            sysdate_native(&packet, &0).unwrap(),
            Some(b"1970-01-02 01:00:00".to_vec())
        );
        assert_eq!(
            sysdate_native(&packet, &6).unwrap(),
            Some(b"1970-01-02 01:00:00.000000".to_vec())
        );
        let negative = clock_packet(-1, 123_456_789, -3_600);
        assert_eq!(
            now_native(&negative, &6).unwrap(),
            Some(b"1969-12-31 22:59:59.123456".to_vec())
        );
        assert_eq!(
            current_date_native(&negative).unwrap(),
            Some(b"1969-12-31".to_vec())
        );
        assert_eq!(
            sysdate_native(&negative, &6).unwrap(),
            Some(b"1969-12-31 22:59:59.123457".to_vec())
        );
        assert_eq!(
            current_date_native(&clock_packet(0, 0, -1)).unwrap(),
            Some(b"1969-12-31".to_vec())
        );
    }

    #[test]
    fn fixed_now_current_date_sysdate_workers_keep_frame_fsp_boundaries() {
        for packet in [Vec::new(), vec![0; 15], vec![0; 17]] {
            assert!(now_native(&packet, &0).is_err());
            assert!(current_date_native(&packet).is_err());
            assert!(sysdate_native(&packet, &0).is_err());
        }
        let packet = clock_packet(0, 0, 0);
        for fsp in [-1, 7, i64::MIN, i64::MAX] {
            assert!(now_native(&packet, &fsp).is_err());
            assert!(sysdate_native(&packet, &fsp).is_err());
        }
        // These are the old raw nanos policies, not a newly validated physical
        // timestamp domain. NOW can render more than six fraction digits when
        // handed that original raw field; CURDATE ignores it completely.
        let raw = clock_packet(0, u32::MAX, 0);
        assert!(native_clock_fsp_args_valid(&raw, 6));
        assert_eq!(
            now_native(&raw, &6).unwrap(),
            Some(b"1970-01-01 00:00:00.4294967".to_vec())
        );
        assert_eq!(
            current_date_native(&raw).unwrap(),
            Some(b"1970-01-01".to_vec())
        );
        assert_eq!(
            sysdate_native(&clock_packet(0, 1_500_000_000, 0), &0).unwrap(),
            Some(b"1970-01-01 00:00:01".to_vec())
        );
        let extreme = clock_packet(i64::MAX, u32::MAX, i32::MAX);
        assert!(native_clock_args_valid(&extreme));
        assert!(native_clock_fsp_args_valid(&extreme, 0));
    }

    #[test]
    fn fixed_native_clock_workers_keep_rounding_offset_and_null_policies() {
        for (meta, name) in [
            (utc_date_native_fn_meta(), "utc_date_native"),
            (utc_timestamp_native_fn_meta(), "utc_timestamp_native"),
            (
                current_time_without_fsp_native_fn_meta(),
                "current_time_without_fsp_native",
            ),
            (
                current_time_with_fsp_native_fn_meta(),
                "current_time_with_fsp_native",
            ),
            (
                utc_time_without_fsp_native_fn_meta(),
                "utc_time_without_fsp_native",
            ),
            (
                utc_time_with_fsp_native_fn_meta(),
                "utc_time_with_fsp_native",
            ),
            (utc_time_null_native_fn_meta(), "utc_time_null_native"),
        ] {
            assert_eq!(meta.name, name);
        }
        // UTC lies just before midnight, whereas the actual offset has already
        // advanced the local date. Explicit time signatures truncate to micros
        // first; UTC_TIMESTAMP rounds the original nanos directly.
        let packet = clock_packet(86_399, 999_999_600, 3_600);
        assert!(native_clock_args_valid(&packet));
        assert_eq!(
            utc_date_native(&packet).unwrap(),
            Some(b"1970-01-01".to_vec())
        );
        assert_eq!(
            utc_timestamp_native(&packet, &6).unwrap(),
            Some(b"1970-01-02 00:00:00.000000".to_vec())
        );
        assert_eq!(
            current_time_without_fsp_native(&packet).unwrap(),
            Some(b"00:59:59".to_vec())
        );
        assert_eq!(
            current_time_with_fsp_native(&packet, &0).unwrap(),
            Some(b"01:00:00".to_vec())
        );
        assert_eq!(
            current_time_with_fsp_native(&packet, &6).unwrap(),
            Some(b"00:59:59.999999".to_vec())
        );
        assert_eq!(
            utc_time_without_fsp_native(&packet).unwrap(),
            Some(b"23:59:59".to_vec())
        );
        assert_eq!(
            utc_time_with_fsp_native(&packet, &0).unwrap(),
            Some(b"00:00:00".to_vec())
        );
        assert_eq!(
            utc_time_with_fsp_native(&packet, &6).unwrap(),
            Some(b"23:59:59.999999".to_vec())
        );
        let negative = clock_packet(-1, 0, -3_600);
        assert_eq!(
            utc_date_native(&negative).unwrap(),
            Some(b"1969-12-31".to_vec())
        );
        assert_eq!(
            utc_timestamp_native(&negative, &0).unwrap(),
            Some(b"1969-12-31 23:59:59".to_vec())
        );
        assert_eq!(
            current_time_without_fsp_native(&negative).unwrap(),
            Some(b"22:59:59".to_vec())
        );
        assert_eq!(
            utc_time_without_fsp_native(&negative).unwrap(),
            Some(b"23:59:59".to_vec())
        );
        assert_eq!(utc_time_null_native(None).unwrap(), None);
        assert!(utc_time_null_native(Some(&0)).is_err());
    }

    #[test]
    fn fixed_native_clock_workers_validate_only_framing_and_fsp() {
        for packet in [Vec::new(), vec![0; 15], vec![0; 17]] {
            assert!(!native_clock_args_valid(&packet));
            assert!(!native_clock_fsp_args_valid(&packet, 0));
            assert!(utc_date_native(&packet).is_err());
            assert!(utc_timestamp_native(&packet, &0).is_err());
            assert!(current_time_without_fsp_native(&packet).is_err());
            assert!(current_time_with_fsp_native(&packet, &0).is_err());
            assert!(utc_time_without_fsp_native(&packet).is_err());
            assert!(utc_time_with_fsp_native(&packet, &0).is_err());
        }
        let packet = clock_packet(0, 0, 0);
        for fsp in [-1, 7, i64::MIN, i64::MAX] {
            assert!(!native_clock_fsp_args_valid(&packet, fsp));
            assert!(utc_timestamp_native(&packet, &fsp).is_err());
            assert!(current_time_with_fsp_native(&packet, &fsp).is_err());
            assert!(utc_time_with_fsp_native(&packet, &fsp).is_err());
        }
        // Admission must not invent clock-domain errors or normalize source
        // fields. Any later arithmetic panic belongs to the real pure worker.
        let extreme = clock_packet(i64::MAX, u32::MAX, i32::MAX);
        assert!(native_clock_args_valid(&extreme));
        assert!(native_clock_fsp_args_valid(&extreme, 0));
        assert!(native_clock_fsp_args_valid(&extreme, 6));
        let decoded = decode_native_clock(&extreme).unwrap();
        assert_eq!(decoded.utc_secs, i64::MAX);
        assert_eq!(decoded.nanos, u32::MAX);
        assert_eq!(decoded.tz_offset, i32::MAX);
        let negative = decode_native_clock(&clock_packet(i64::MIN, u32::MAX, i32::MIN)).unwrap();
        assert_eq!(negative.utc_secs, i64::MIN);
        assert_eq!(negative.tz_offset, i32::MIN);
        let raw_nanos = clock_packet(0, u32::MAX, i32::MAX);
        assert_eq!(
            utc_date_native(&raw_nanos).unwrap(),
            Some(b"1970-01-01".to_vec())
        );
        assert_eq!(
            utc_time_without_fsp_native(&raw_nanos).unwrap(),
            Some(b"00:00:00".to_vec())
        );
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, sync::Arc};

    use tidb_query_datatype::{
        FieldTypeTp,
        builder::FieldTypeBuilder,
        codec::{
            batch::LazyBatchColumnVec,
            data_type::*,
            error::ERR_TRUNCATE_WRONG_VALUE,
            mysql::{MAX_FSP, Time},
        },
        expr::EvalConfig,
    };
    use tipb::{FieldType, ScalarFuncSig};
    use tipb_helper::ExprDefBuilder;

    use super::*;
    use crate::{RpnExpressionBuilder, types::test_util::RpnFnScalarEvaluator};

    #[test]
    fn test_native_date_format_source_literals_and_early_paths() {
        // Identical preexisting native text/public-Time source fixture literals.
        let layout =
            b"%b %M %m %c %D %d %e %j %k %h %i %p %r %T %s %f %U %u %V %v %a %W %w %X %x %Y %y %%";
        let expected = b"Jan January 01 1 7th 07 7 007 23 11 12 PM 11:12:34 PM 23:12:34 34 123450 01 01 01 01 Thu Thursday 4 2010 2010 2010 10 %";
        assert_eq!(
            date_format_text_native(Some(b"2010-01-07 23:12:34.12345"), Some(layout)).unwrap(),
            Some(expected.to_vec())
        );
        let core = ((2010_u64 << 50)
            | (1_u64 << 46)
            | (7_u64 << 41)
            | (23_u64 << 36)
            | (12_u64 << 30)
            | (34_u64 << 24)
            | (123_450_u64 << 4))
            .to_le_bytes();
        assert_eq!(
            date_format_core_native(Some(&core), Some(layout)).unwrap(),
            Some(expected.to_vec())
        );
        assert_eq!(
            date_format_text_native(Some(b"0000-01-01"), Some(b"%X %x")).unwrap(),
            Some(b"0000 4294967295".to_vec())
        );
        // Original public-Time month-zero and trailing-percent fixtures.
        let invalid_month = ((2010_u64 << 50) | (1_u64 << 41)).to_le_bytes();
        assert_eq!(
            date_format_core_native(Some(&invalid_month), Some(b"%M")).unwrap(),
            None
        );
        assert_eq!(
            date_format_core_native(Some(&invalid_month), Some(b"trailing%")).unwrap(),
            Some(b"trailing".to_vec())
        );
        // Policy-derived literal: text DATE_FORMAT falls back to midnight for
        // a bad clock, and unlike public-Time retains a trailing percent.
        assert_eq!(
            date_format_text_native(Some(b"2010-01-07 23:12:99"), Some(b"%T%")).unwrap(),
            Some(b"00:00:00%".to_vec())
        );
        assert_eq!(date_format_text_native(None, Some(b"%Y")).unwrap(), None);
        assert_eq!(date_format_core_native(Some(&core), None).unwrap(), None);
        assert!(date_format_core_native(None, Some(b"%Y")).is_err());
        assert_eq!(date_format_null_native(None).unwrap(), None);
        assert!(date_format_null_native(Some(&0)).is_err());
        assert_eq!(date_format_missing_native().unwrap(), Some(0));
    }

    #[test]
    fn test_native_time_format_probe_and_source_literals() {
        let source = b"1990-05-07 19:30:10";
        assert_eq!(
            duration_text_probe_native(Some(source)).unwrap(),
            Some(source.to_vec())
        );
        assert_eq!(
            time_format_text_native(Some(source), Some(b"%H %i %s")).unwrap(),
            Some(b"19 30 10".to_vec())
        );
        assert_eq!(
            time_format_text_native(Some(b"12:34:56"), Some(b"")).unwrap(),
            None
        );
        // Policy-derived from the original duration parser/format branches:
        // retain the actual untrimmed probe text; hour25 is PM, not modulo-AM.
        let original = b"  -25:01:02.3  ";
        assert_eq!(
            duration_text_probe_native(Some(original)).unwrap(),
            Some(original.to_vec())
        );
        assert_eq!(
            time_format_text_native(Some(original), Some(b"%H|%h|%p|%T|%f|%M|%")).unwrap(),
            Some(b"-25|01|PM|-25:01:02|300000|M|%".to_vec())
        );
        assert_eq!(duration_text_probe_native(Some(b"12:99:00")).unwrap(), None);
        assert_eq!(duration_text_probe_native(None).unwrap(), None);
        assert_eq!(time_format_text_native(Some(source), None).unwrap(), None);
    }

    #[test]
    fn test_native_last_day_source_literals_and_suffix_policy() {
        for (text, expected) in [
            ("2003-02-05", Some("2003-02-28")),
            ("2004-02-05", Some("2004-02-29")),
            ("2004-01-01 01:01:01", Some("2004-01-31")),
            ("2007-10-07 23:59:61", None),
            ("0000-00-00", None),
            // These two are explicit policy-derived literals: the old parser
            // trims Unicode whitespace but does not accept a T separator.
            (" 2004-01-01\u{2003}01:01:01 ", Some("2004-01-31")),
            ("2004-01-01T01:01:01", None),
        ] {
            assert_eq!(
                last_day_text_native(Some(text.as_bytes())).unwrap(),
                expected.map(|text| text.as_bytes().to_vec())
            );
        }
        assert_eq!(last_day_text_native(None).unwrap(), None);
    }

    #[test]
    fn test_native_make_date_and_from_days_source_literals() {
        // Existing native go_time_values and time_fn/tests literal fixtures.
        for (year, day, expected) in [
            (69, 1, Some("2069-01-01")),
            (70, 1, Some("1970-01-01")),
            (2060, 2_900_025, Some("9999-12-31")),
            (2060, 2_900_026, None),
        ] {
            assert_eq!(
                make_date_native(Some(&year), Some(&day)).unwrap(),
                expected.map(|text| text.as_bytes().to_vec())
            );
        }
        assert_eq!(make_date_native(None, Some(&1)).unwrap(), None);
        assert_eq!(make_date_native(Some(&2060), None).unwrap(), None);
        for (day, expected) in [
            (-140, Some("0000-00-00")),
            (140, Some("0000-00-00")),
            (735_000, Some("2012-05-12")),
            (3_652_424, Some("9999-12-31")),
            (3_652_425, None),
        ] {
            assert_eq!(
                from_days_native(Some(&day)).unwrap(),
                expected.map(|text| text.as_bytes().to_vec())
            );
        }
        assert_eq!(from_days_native(None).unwrap(), None);
    }

    #[test]
    fn test_native_make_time_parts_source_literals() {
        // Seconds below are the exact whole-second values of existing native
        // 12:15:30, -25:15:30 and unsigned-hour 838:59:59 fixture results.
        for (hour, minute, second, unsigned, expected_seconds) in [
            (12_i64, 15, 30_f64, false, 44_130_f64),
            (-25, 15, 30_f64, false, -90_930_f64),
            (-1, 0, 0_f64, true, 3_020_399_f64),
        ] {
            let mut hour = hour.to_le_bytes().to_vec();
            hour.push(u8::from(unsigned));
            let second = second.to_bits().to_le_bytes();
            assert_eq!(
                make_time_parts_native(Some(&hour), Some(&minute), Some(&second)).unwrap(),
                Some(expected_seconds.to_bits().to_le_bytes().to_vec())
            );
        }
        let mut hour = [0; 9];
        hour[..8].copy_from_slice(&12_i64.to_le_bytes());
        let second = 30_f64.to_bits().to_le_bytes();
        assert_eq!(
            make_time_parts_native(None, Some(&15), Some(&second)).unwrap(),
            None
        );
        assert_eq!(
            make_time_parts_native(Some(&hour), None, Some(&second)).unwrap(),
            None
        );
        assert_eq!(
            make_time_parts_native(Some(&hour), Some(&15), None).unwrap(),
            None
        );
        // Original MAKETIME(12, 60, 0) fixture is NULL, not clamped.
        let second = 0_f64.to_bits().to_le_bytes();
        assert_eq!(
            make_time_parts_native(Some(&hour), Some(&60), Some(&second)).unwrap(),
            None
        );
    }

    #[test]
    fn test_native_duration_formatter_source_literals_and_fsp7() {
        // Existing SEC_TO_TIME and MAKETIME expected literals; no provider oracle.
        for (seconds, fsp, expected) in [
            (2_378_f64, 0, "00:39:38"),
            (3_864_000_f64, 0, "838:59:59"),
            (-86_401.4, 1, "-24:00:01.4"),
            (123.456_789_1, 6, "00:02:03.456789"),
            (44_130.000_000_5, 6, "12:15:30.000001"),
            // The original formatter pads to the actual FSP, including seven;
            // its half-up carry increments only second, never minute/hour.
            (1_f64, 7, "00:00:01.0000000"),
            (59.999_999_95, 7, "00:00:60.0000000"),
        ] {
            let seconds = seconds.to_bits().to_le_bytes();
            assert_eq!(
                sec_to_time_native(Some(&seconds), Some(&fsp)).unwrap(),
                Some(expected.as_bytes().to_vec())
            );
        }
        assert_eq!(sec_to_time_native(None, None).unwrap(), None);
        assert!(sec_to_time_native(None, Some(&0)).is_err());
    }

    #[test]
    fn test_native_week_text_literal_oracles() {
        // Pinned time_fn/tests.rs and calendar.rs boundary fixtures; expected
        // values are literals, never recomputed through the shared provider.
        for (text, mode, expected) in [
            ("2008-02-20", Some(0), 7),
            ("2008-02-20", Some(1), 8),
            ("2008-12-31", Some(1), 53),
            ("2023-01-01", None, 1),
            ("2000-12-31", Some(0), 53),
            ("2000-12-31", Some(6), 1),
            ("2005-12-3", Some(6), 48),
            ("2016-01-01", Some(0), 0),
            ("2016-01-01", Some(3), 53),
        ] {
            assert_eq!(
                week_text_native(Some(text.as_bytes()), mode.as_ref()).unwrap(),
                Some(expected),
                "{text} {mode:?}"
            );
        }
        for (text, mode, expected) in [
            ("2000-01-01", Some(0), 199_952),
            ("2000-01-01", None, 199_952),
            ("2020-01-01", Some(3), 202_001),
            ("2016-01-01", Some(3), 201_553),
            ("0000-01-01", Some(3), 4_294_967_295),
        ] {
            assert_eq!(
                year_week_text_native(Some(text.as_bytes()), mode.as_ref()).unwrap(),
                Some(expected),
                "{text} {mode:?}"
            );
        }
        for (text, expected) in [
            ("2024-03-15", 11),
            ("2024-01-01", 1),
            ("2020-12-31", 53),
            ("0000-01-01", 52),
            ("0000-01-01 99:99:99.bad", 52),
        ] {
            assert_eq!(
                week_of_year_text_native(Some(text.as_bytes())).unwrap(),
                Some(expected),
                "{text}"
            );
        }
    }

    #[test]
    fn test_native_week_probe_null_and_raw_core() {
        for text in [
            " 2008/02/20 99:99:99.bad ",
            "0000-02-29",
            "4294967295-12-31",
        ] {
            let mut input = text.as_bytes().to_vec();
            let owned = week_date_text_native(Some(&input)).unwrap().unwrap();
            input.fill(b'x');
            assert_eq!(owned, text.as_bytes());
        }
        assert_eq!(week_date_text_native(None).unwrap(), None);
        assert_eq!(week_text_native(None, Some(&3)).unwrap(), None);
        assert_eq!(year_week_text_native(None, Some(&3)).unwrap(), None);
        assert_eq!(week_of_year_text_native(None).unwrap(), None);
        for text in [
            "",
            "0000-00-00",
            "2008-13-01",
            "2008-01-00",
            "2023-02-29",
            "2024-02-30",
            "4294967296-12-31",
        ] {
            let input = Some(text.as_bytes());
            assert_eq!(week_date_text_native(input).unwrap(), None, "{text}");
            assert_eq!(week_text_native(input, None).unwrap(), None, "{text}");
            assert_eq!(year_week_text_native(input, None).unwrap(), None, "{text}");
            assert_eq!(week_of_year_text_native(input).unwrap(), None, "{text}");
        }
        assert!(week_date_text_native(Some(b"\xff")).is_err());
        assert!(week_text_native(Some(b"\xff"), None).is_err());
        assert!(year_week_text_native(Some(b"\xff"), None).is_err());
        assert!(week_of_year_text_native(Some(b"\xff")).is_err());
        assert_eq!(week_null_native(None).unwrap(), None);
        assert!(week_null_native(Some(&0)).is_err());
        assert!(week_null_native(Some(&1)).is_err());

        // Original CoreTime::week returns zero for zero month/day, including
        // clock-only raw values, without calendar construction or validation.
        let clock = (1_u64 << 41) - 1;
        let date = (2008_u64 << 50) | (2_u64 << 46) | (20_u64 << 41);
        for (raw, expected) in [
            (0, 0),
            (clock, 0),
            ((2008_u64 << 50) | (1_u64 << 41) | clock, 0),
            ((2008_u64 << 50) | (1_u64 << 46) | clock, 0),
            (date, 7),
            (date | clock, 7),
        ] {
            assert_eq!(
                week_core_native(Some(&raw.to_le_bytes())).unwrap(),
                Some(expected),
                "{raw:#x}"
            );
        }
        assert_eq!(week_core_native(None).unwrap(), None);
        for malformed in [b"".as_slice(), &[0; 7], &[0; 9]] {
            assert!(week_core_native(Some(malformed)).is_err());
        }
    }

    #[test]
    fn test_native_date_diff_day_counts_and_tso_literals() {
        // Fixed pinned-native fixtures/formulas, not new-provider oracles.
        for (left, right, expected) in [
            ("2004-05-21", "2004:01:02", 140),
            ("2008-12-31 99:99:99.bad", "2008-12-30", 1),
            ("0000-03-01", "0000-02-28", 2),
            ("4294967295-12-31", "4294967295-01-01", 364),
            ("1010-11-30", "2210-11-01", -438_262),
        ] {
            assert_eq!(
                date_diff_text_native(Some(left.as_bytes()), Some(right.as_bytes())).unwrap(),
                Some(expected)
            );
        }
        let valid = Some(b"2009-11-29".as_slice());
        assert_eq!(date_diff_text_native(None, valid).unwrap(), None);
        assert_eq!(date_diff_text_native(valid, None).unwrap(), None);
        assert_eq!(
            date_diff_text_native(Some(b"2009-13-01"), valid).unwrap(),
            None
        );
        assert_eq!(
            date_diff_text_native(valid, Some(b"0000-00-00")).unwrap(),
            None
        );
        assert!(date_diff_text_native(Some(b"\xff"), valid).is_err());
        assert!(date_diff_text_native(valid, Some(b"\xff")).is_err());
        assert_eq!(date_diff_null_native(None).unwrap(), None);
        assert!(date_diff_null_native(Some(&0)).is_err());
        assert!(date_diff_null_native(Some(&1)).is_err());

        let cases: [(&str, Int, Int); 7] = [
            ("2009-11-29", 734_105, 63_426_672_000),
            ("2009-11-29 13:43:32.1234567", 734_105, 63_426_721_412),
            ("2009-11-29T13:43:32.", 734_105, 63_426_721_412),
            ("0000-01-01", 1, 86_400),
            ("0000-02-29", 60, 5_184_000),
            ("0000-03-01", 60, 5_184_000),
            (
                "4294967295-12-31",
                1_568_704_592_609,
                135_536_076_801_417_600,
            ),
        ];
        for (text, days, seconds) in cases {
            assert_eq!(
                to_days_text_native(Some(text.as_bytes())).unwrap(),
                Some(days),
                "{text}"
            );
            assert_eq!(
                to_seconds_text_native(Some(text.as_bytes())).unwrap(),
                Some(seconds),
                "{text}"
            );
        }
        let kernels: [fn(Option<BytesRef>) -> Result<Option<Int>>; 2] =
            [to_days_text_native, to_seconds_text_native];
        for kernel in kernels {
            assert_eq!(kernel(None).unwrap(), None);
            assert!(kernel(Some(b"\xff")).is_err());
            for invalid in [
                "0000-00-00",
                "2009-13-01",
                "2007-10-07 23:59:61",
                "2009-11-29 24:00:00",
                "2009-11-29 13:43:32.123456x",
            ] {
                // Clock and the entire fraction are validated even for TO_DAYS.
                assert_eq!(kernel(Some(invalid.as_bytes())).unwrap(), None, "{invalid}");
            }
        }
        assert_eq!(tso_logical_native(None).unwrap(), None);
        for (input, expected) in [
            (452_605_852_463_012_352, Some(137_728)),
            (262_144, Some(0)),
            (262_143, Some(262_143)),
            (1, Some(1)),
            (i64::MAX, Some(262_143)),
            (0, None),
            (-1, None),
        ] {
            assert_eq!(tso_logical_native(Some(&input)).unwrap(), expected);
        }
    }

    #[test]
    fn test_native_date_diff_core_raw_policy() {
        // Pack only fixture inputs in the original 14/4/5-bit layout; expected
        // differences are literals from the old native i32 day-number formula.
        let core = |year: u64, month: u64, day: u64, clock: u64| {
            (year << 50) | (month << 46) | (day << 41) | clock
        };
        let base = core(2000, 1, 1, 0);
        let clock = (1_u64 << 41) - 1;
        for (left, right, expected) in [
            (0, core(0, 1, 1, 0), -1),
            (core(2000, 0, 1, 0), base, -31),
            (core(2000, 13, 1, 0), base, 366),
            (core(2000, 1, 0, 0), base, -1),
            (core(2000, 1, 1, clock), base, 0),
            (clock, 0, 0),
            (u64::MAX, 0, 5_984_224),
            (0, u64::MAX, -5_984_224),
        ] {
            assert_eq!(
                date_diff_core_native(Some(&left.to_le_bytes()), Some(&right.to_le_bytes()))
                    .unwrap(),
                Some(expected)
            );
        }
        let raw = base.to_le_bytes();
        assert_eq!(date_diff_core_native(None, Some(&raw)).unwrap(), None);
        assert_eq!(date_diff_core_native(Some(&raw), None).unwrap(), None);
        for malformed in [b"".as_slice(), &[0; 7], &[0; 9]] {
            assert!(date_diff_core_native(Some(malformed), Some(&raw)).is_err());
            assert!(date_diff_core_native(Some(&raw), Some(malformed)).is_err());
        }
    }

    #[test]
    fn test_native_weekday_fields_literal_oracles() {
        // Pinned native fixtures and independent old civil-formula literals;
        // year zero and the full u32 year domain are not wire Time validation.
        let cases: [(&str, Int, Int, Int, &str); 7] = [
            ("2017-12-01", 6, 4, 335, "Friday"),
            ("0000-01-01", 7, 5, 1, "Saturday"),
            ("0000-12-01", 6, 4, 336, "Friday"),
            ("2020-02-29", 7, 5, 60, "Saturday"),
            ("2000-12-31", 1, 6, 366, "Sunday"),
            ("1900-03-01", 5, 3, 60, "Thursday"),
            ("4294967295-12-31", 7, 5, 365, "Saturday"),
        ];
        for (text, day_of_week, weekday, day_of_year, day_name) in cases {
            let input = Some(text.as_bytes());
            assert_eq!(
                day_of_week_text_native(input).unwrap(),
                Some(day_of_week),
                "{text}"
            );
            assert_eq!(weekday_text_native(input).unwrap(), Some(weekday), "{text}");
            assert_eq!(
                day_of_year_text_native(input).unwrap(),
                Some(day_of_year),
                "{text}"
            );
            assert_eq!(
                day_name_text_native(input).unwrap(),
                Some(day_name.as_bytes().to_vec()),
                "{text}"
            );
        }
    }

    #[test]
    fn test_native_weekday_fields_null_invalid_and_transport() {
        let invalid = [
            "",
            "2017-00-01",
            "2017-01-00",
            "2008-13-01",
            "2023-02-29",
            "2024-02-30",
            "0000-00-00",
            "0000-00-00 00:00:11.000000",
            "4294967296-12-31",
        ];
        let kernels: [fn(Option<BytesRef>) -> Result<Option<Int>>; 3] = [
            day_of_week_text_native,
            weekday_text_native,
            day_of_year_text_native,
        ];
        for kernel in kernels {
            assert_eq!(kernel(None).unwrap(), None);
            for text in invalid {
                assert_eq!(kernel(Some(text.as_bytes())).unwrap(), None, "{text}");
            }
        }
        assert_eq!(day_name_text_native(None).unwrap(), None);
        for text in invalid {
            assert_eq!(
                day_name_text_native(Some(text.as_bytes())).unwrap(),
                None,
                "{text}"
            );
        }
        let errors = [
            day_of_week_text_native(Some(b"\xff")).unwrap_err(),
            weekday_text_native(Some(b"\xff")).unwrap_err(),
            day_of_year_text_native(Some(b"\xff")).unwrap_err(),
            day_name_text_native(Some(b"\xff")).unwrap_err(),
        ];
        for error in errors {
            assert!(matches!(
                error.0.as_ref(),
                tidb_query_common::error::ErrorInner::Evaluate(EvaluateError::Other(_))
            ));
        }
    }

    #[test]
    fn test_native_period_values_nulls_and_causes() {
        use tidb_query_common::error::ErrorInner;

        let kernels: [fn(Option<&Int>, Option<&Int>) -> Result<Option<Int>>; 2] =
            [period_add_native, period_diff_native];
        for kernel in kernels {
            assert_eq!(kernel(None, None).unwrap(), None);
            assert_eq!(kernel(None, Some(&0)).unwrap(), None);
            assert_eq!(kernel(Some(&200813), None).unwrap(), None);
        }
        // Independent literals from the pinned native wrapping formulas,
        // including inverse zero, unsigned underflow and the signed-add boundary.
        let add_cases: [(Int, Int, Int); 8] = [
            (200802, 2, 200804),
            (200802, -13, 200701),
            (6901, 12, 207001),
            (7001, -1, 196912),
            (800_000_000_001, 0, 800_000_000_001),
            (1, -24000, 0),
            (1, -24001, 6_148_914_691_236_517_176),
            (1, i64::MAX, 3_074_457_345_618_458_544),
        ];
        for (period, months, expected) in add_cases {
            assert_eq!(
                period_add_native(Some(&period), Some(&months)).unwrap(),
                Some(expected)
            );
        }
        let diff_cases: [(Int, Int, Int); 4] = [
            (1, 2, -1),
            (7001, 6912, -1199),
            (i64::MAX, 200001, 1_106_804_644_422_549_102),
            (200001, i64::MAX, -1_106_804_644_422_549_102),
        ];
        for (left, right, expected) in diff_cases {
            assert_eq!(
                period_diff_native(Some(&left), Some(&right)).unwrap(),
                Some(expected)
            );
        }
        for invalid in [0, -1, 200800, 200813] {
            let failures = [
                (
                    period_add_native(Some(&invalid), Some(&1)).unwrap_err(),
                    true,
                    "Incorrect arguments to period_add",
                ),
                (
                    period_diff_native(Some(&invalid), Some(&200801)).unwrap_err(),
                    false,
                    "Incorrect arguments to period_diff",
                ),
                (
                    period_diff_native(Some(&200801), Some(&invalid)).unwrap_err(),
                    false,
                    "Incorrect arguments to period_diff",
                ),
            ];
            for (failure, is_add, message) in failures {
                let ErrorInner::Evaluate(error) = failure.0.as_ref() else {
                    panic!("expected a typed evaluation error");
                };
                assert_eq!(error.code(), 1210);
                assert_eq!(error.to_string(), message);
                if is_add {
                    assert!(matches!(error, EvaluateError::PeriodAddIncorrectArguments));
                } else {
                    assert!(matches!(error, EvaluateError::PeriodDiffIncorrectArguments));
                }
            }
        }
    }

    #[test]
    fn test_native_get_format_bytes_and_null_witness() {
        assert_eq!(get_format_native(None, None).unwrap(), None);
        assert_eq!(get_format_native(None, Some(b"\xff")).unwrap(), None);
        assert_eq!(get_format_native(Some(b"\xff"), None).unwrap(), None);
        // Literal table answers preserve exact type case and ASCII-only locale case.
        let cases: [(&[u8], &[u8], &[u8]); 10] = [
            (b"DATE", b"usa", b"%m.%d.%Y"),
            (b"DATE", b"iSo", b"%Y-%m-%d"),
            (b"DATE", b"INTERNAL", b"%Y%m%d"),
            (b"DATETIME", b"EUR", b"%Y-%m-%d %H.%i.%s"),
            (b"TIMESTAMP", b"JIS", b"%Y-%m-%d %H:%i:%s"),
            (b"TIME", b"usa", b"%h:%i:%s %p"),
            (b"date", b"USA", b""),
            (b"DATE", b"unknown", b""),
            (b"\xff", b"ISO", b""),
            (b"DATE", b"\xff", b""),
        ];
        for (kind, locale, expected) in cases {
            assert_eq!(
                get_format_native(Some(kind), Some(locale)).unwrap(),
                Some(expected.to_vec())
            );
        }
        assert_eq!(get_format_null_native(None).unwrap(), None);
        assert!(get_format_null_native(Some(b"")).is_err());
        assert!(get_format_null_native(Some(b"DATE")).is_err());
    }

    #[test]
    fn test_native_month_name_and_time_to_sec_text() {
        assert_eq!(month_name_text_native(None).unwrap(), None);
        assert!(month_name_text_native(Some(&[0xff])).is_err());
        // Month names require a valid complete calendar date, not just a month.
        let month_cases: [(&str, Option<&str>); 7] = [
            ("", None),
            ("2024-01-15", Some("January")),
            ("2024-02-29", Some("February")),
            ("2023-02-29", None),
            ("2024-13-01", None),
            ("2024-02-30", None),
            ("4294967295-12-31", Some("December")),
        ];
        for (text, expected) in month_cases {
            assert_eq!(
                month_name_text_native(Some(text.as_bytes())).unwrap(),
                expected.map(|name| name.as_bytes().to_vec()),
                "{text:?}",
            );
        }

        assert_eq!(time_to_sec_text_native(None).unwrap(), None);
        assert!(time_to_sec_text_native(Some(&[0xff])).is_err());
        // Literal answers retain the old signed-hour and permissive-fraction
        // policy; they are not generated by the new datatype provider.
        let seconds_cases: [(&str, Option<Int>); 13] = [
            ("", Some(0)),
            ("junk", Some(0)),
            ("01:02:03", Some(3723)),
            ("-01:02:03", Some(-3723)),
            ("--1:00", Some(3600)),
            ("--900:00", Some(3_240_000)),
            ("900:30:15", None),
            ("12:60:00", None),
            ("01:02:03.badxyzmore", Some(3723)),
            ("1990-05-07 19:30:10", Some(70210)),
            ("1990-05-07\t19:30:10", None),
            ("123456junk", Some(45296)),
            ("2024-01-15", Some(1224)),
        ];
        for (text, expected) in seconds_cases {
            assert_eq!(
                time_to_sec_text_native(Some(text.as_bytes())).unwrap(),
                expected,
                "{text:?}"
            );
        }
    }

    #[test]
    fn test_native_hms_text_fields() {
        let kernels: [fn(Option<&[u8]>) -> Result<Option<Int>>; 3] =
            [hour_text_native, minute_text_native, second_text_native];
        for kernel in kernels {
            assert_eq!(kernel(None).unwrap(), None);
            assert!(kernel(Some(&[0xff])).is_err());
        }
        // Literal answers follow the pre-move string parser, not provider output.
        let cases: [(&str, Option<[Int; 3]>); 10] = [
            ("", None),
            ("900:30:15", Some([838, 59, 59])),
            ("900:60:15", None),
            ("2024-01-15", Some([0, 20, 24])),
            ("-12:34:56", Some([12, 34, 56])),
            ("12:34:56.", Some([12, 34, 56])),
            ("12:34:56.5junk", None),
            ("123456junk", Some([12, 34, 56])),
            ("junk 12:34:56", None),
            ("2024-01-15 12:34:56.000", Some([12, 34, 56])),
        ];
        for (text, expected) in cases {
            for (index, kernel) in kernels.iter().enumerate() {
                assert_eq!(
                    kernel(Some(text.as_bytes())).unwrap(),
                    expected.map(|parts| parts[index]),
                    "{text:?} part {index}",
                );
            }
        }
    }

    #[test]
    fn test_native_hms_nanos_fields() {
        let kernels: [fn(Option<&Int>) -> Result<Option<Int>>; 3] =
            [hour_nanos_native, minute_nanos_native, second_nanos_native];
        for kernel in kernels {
            assert_eq!(kernel(None).unwrap(), None);
        }
        // Independent literals from absolute-nanosecond integer arithmetic;
        // unlike SQL text, this domain neither clamps nor rounds to an FSP.
        let cases: [(Int, [Int; 3]); 8] = [
            (0, [0, 0, 0]),
            (999_999_999, [0, 0, 0]),
            (-999_999_999, [0, 0, 0]),
            (45_296_123_456_789, [12, 34, 56]),
            (-45_296_123_456_789, [12, 34, 56]),
            (3_241_815_999_999_999, [900, 30, 15]),
            (-3_241_815_999_999_999, [900, 30, 15]),
            (i64::MIN, [2_562_047, 47, 16]),
        ];
        for (nanos, expected) in cases {
            for (kernel, value) in kernels.iter().zip(expected) {
                assert_eq!(kernel(Some(&nanos)).unwrap(), Some(value));
            }
        }
    }

    #[test]
    fn test_native_calendar_core_fields() {
        let kernels: [fn(Option<&[u8]>) -> Result<Option<Int>>; 4] = [
            year_core_native,
            month_core_native,
            day_of_month_core_native,
            quarter_core_native,
        ];
        for kernel in kernels {
            assert_eq!(kernel(None).unwrap(), None);
            assert!(kernel(Some(&[0; 7])).is_err());
            assert!(kernel(Some(&[0; 9])).is_err());
        }
        // Calendar fields occupy 63..50, 49..46 and 45..41. Expected answers
        // are literal calendar values, not obtained from the new provider.
        const NORMAL: u64 = (2024u64 << 50) | (11u64 << 46) | (30u64 << 41);
        let cases: [(u64, [Int; 4]); 5] = [
            (0, [0, 0, 0, 0]),
            (NORMAL, [2024, 11, 30, 4]),
            (NORMAL | ((1u64 << 41) - 1), [2024, 11, 30, 4]),
            (
                (2024u64 << 50) | (15u64 << 46) | (31u64 << 41),
                [2024, 15, 31, 5],
            ),
            (u64::MAX, [16383, 15, 31, 5]),
        ];
        for (raw, expected) in cases {
            let encoded = raw.to_le_bytes();
            for (kernel, value) in kernels.iter().zip(expected) {
                assert_eq!(kernel(Some(&encoded)).unwrap(), Some(value));
            }
        }
    }

    #[test]
    fn test_add_duration_and_duration() {
        let cases = vec![
            (Some("00:01:01"), Some("00:01:01"), Some("00:02:02")),
            (Some("11:59:59"), Some("00:00:01"), Some("12:00:00")),
            (Some("23:59:59"), Some("00:00:01"), Some("24:00:00")),
            (Some("23:59:59"), Some("00:00:02"), Some("24:00:01")),
            (None, None, None),
        ];
        let mut ctx = EvalContext::default();
        for (duration1, duration2, exp) in cases {
            let expected = exp.map(|exp| Duration::parse(&mut ctx, exp, MAX_FSP).unwrap());
            let duration1 = duration1.map(|arg1| Duration::parse(&mut ctx, arg1, MAX_FSP).unwrap());
            let duration2 = duration2.map(|arg2| Duration::parse(&mut ctx, arg2, MAX_FSP).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(duration1)
                .push_param(duration2)
                .evaluate(ScalarFuncSig::AddDurationAndDuration)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_add_duration_and_string() {
        let cases = vec![
            (Some("00:01:01"), Some("00:01:01"), Some("00:02:02")),
            (Some("11:59:59"), Some("00:00:01"), Some("12:00:00")),
            (Some("23:59:59"), Some("00:00:01"), Some("24:00:00")),
            (Some("23:59:59"), Some("00:00:02"), Some("24:00:01")),
            (None, None, None),
        ];
        let mut ctx = EvalContext::default();
        for (duration, string, exp) in cases {
            let expected = exp.map(|exp| Duration::parse(&mut ctx, exp, MAX_FSP).unwrap());
            let duration = duration.map(|arg1| Duration::parse(&mut ctx, arg1, MAX_FSP).unwrap());
            let string = string.map(|str| str.as_bytes().to_vec());
            let output = RpnFnScalarEvaluator::new()
                .push_param(duration)
                .push_param(string)
                .evaluate(ScalarFuncSig::AddDurationAndString)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_sub_duration_and_duration() {
        let cases = vec![
            (Some("00:02:02"), Some("00:01:01"), Some("00:01:01")),
            (Some("12:00:00"), Some("00:00:01"), Some("11:59:59")),
            (Some("24:00:00"), Some("00:00:01"), Some("23:59:59")),
            (Some("24:00:01"), Some("00:00:02"), Some("23:59:59")),
            (None, None, None),
        ];
        let mut ctx = EvalContext::default();
        for (duration1, duration2, exp) in cases {
            let expected = exp.map(|exp| Duration::parse(&mut ctx, exp, MAX_FSP).unwrap());
            let duration1 = duration1.map(|arg1| Duration::parse(&mut ctx, arg1, MAX_FSP).unwrap());
            let duration2 = duration2.map(|arg2| Duration::parse(&mut ctx, arg2, MAX_FSP).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(duration1)
                .push_param(duration2)
                .evaluate(ScalarFuncSig::SubDurationAndDuration)
                .unwrap();
            assert_eq!(output, expected);
        }
    }
    #[test]
    fn test_sub_duration_and_string() {
        let cases = vec![
            (Some("00:02:02"), Some("00:01:01"), Some("00:01:01")),
            (Some("12:00:00"), Some("00:00:01"), Some("11:59:59")),
            (Some("24:00:00"), Some("00:00:01"), Some("23:59:59")),
            (Some("24:00:01"), Some("00:00:02"), Some("23:59:59")),
            (None, None, None),
        ];
        let mut ctx = EvalContext::default();
        for (duration, string, exp) in cases {
            let expected = exp.map(|exp| Duration::parse(&mut ctx, exp, MAX_FSP).unwrap());
            let duration = duration.map(|arg1| Duration::parse(&mut ctx, arg1, MAX_FSP).unwrap());
            let string = string.map(|str| str.as_bytes().to_vec());
            let output = RpnFnScalarEvaluator::new()
                .push_param(duration)
                .push_param(string)
                .evaluate(ScalarFuncSig::SubDurationAndString)
                .unwrap();
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_date_format() {
        use std::sync::Arc;

        use tidb_query_datatype::expr::{EvalConfig, EvalContext, Flag, SqlMode};

        let cases = vec![
            (
                "2010-01-07 23:12:34.12345",
                "%b %M %m %c %D %d %e %j %k %h %i %p %r %T %s %f %U %u
                %V %v %a %W %w %X %x %Y %y %%",
                "Jan January 01 1 7th 07 7 007 23 11 12 PM 11:12:34 PM 23:12:34 34 123450 01 01
                01 01 Thu Thursday 4 2010 2010 2010 10 %",
            ),
            (
                "2012-12-21 23:12:34.123456",
                "%b %M %m %c %D %d %e %j %k %h %i %p %r %T %s %f %U
                %u %V %v %a %W %w %X %x %Y %y %%",
                "Dec December 12 12 21st 21 21 356 23 11 12 PM 11:12:34 PM 23:12:34 34 123456 51
                51 51 51 Fri Friday 5 2012 2012 2012 12 %",
            ),
            (
                "0000-01-01 00:00:00.123456",
                // Functions week() and yearweek() don't support multi mode,
                // so the result of "%U %u %V %Y" is different from MySQL.
                "%b %M %m %c %D %d %e %j %k %h %i %p %r %T %s %f %v
                %x %Y %y %%",
                "Jan January 01 1 1st 01 1 001 0 12 00 AM 12:00:00 AM 00:00:00 00 123456 52
                4294967295 0000 00 %",
            ),
            (
                "2016-09-3 00:59:59.123456",
                "abc%b %M %m %c %D %d %e %j %k %h %i %p %r %T %s %f %U
                %u %V %v %a %W %w %X %x %Y %y!123 %%xyz %z",
                "abcSep September 09 9 3rd 03 3 247 0 12 59 AM 12:59:59 AM 00:59:59 59 123456 35
                35 35 35 Sat Saturday 6 2016 2016 2016 16!123 %xyz z",
            ),
            (
                "2012-10-01 00:00:00",
                "%b %M %m %c %D %d %e %j %k %H %i %p %r %T %s %f %v
                %x %Y %y %%",
                "Oct October 10 10 1st 01 1 275 0 00 00 AM 12:00:00 AM 00:00:00 00 000000 40
                2012 2012 12 %",
            ),
        ];
        for (date, format, expect) in cases {
            let date =
                Some(DateTime::parse_datetime(&mut EvalContext::default(), date, 6, true).unwrap());
            let format = Some(format.as_bytes().to_vec());
            let expect = Some(expect.as_bytes().to_vec());

            let output = RpnFnScalarEvaluator::new()
                .push_param(date)
                .push_param(format.clone())
                .evaluate(ScalarFuncSig::DateFormatSig)
                .unwrap();
            assert_eq!(output, expect, "{:?} {:?}", date, format);
        }

        // TODO: pass this test after refactoring the issue #3953 is fixed.
        // {
        //     let format: Option<Bytes> = Some(
        //          "abc%b %M %m %c %D %d %e %j".as_bytes().to_vec());
        //     let time: Option<DateTime> =
        //         Some(DateTime::parse_utc_datetime(
        //            "0000-00-00 00:00:00", 6).unwrap());

        //     let mut cfg = EvalConfig::new();
        //     cfg.set_flag(Flag::IN_UPDATE_OR_DELETE_STMT)
        //         .set_sql_mode(SqlMode::NO_ZERO_DATE | SqlMode::STRICT_ALL_TABLES);
        //     let ctx = EvalContext::new(Arc::new(cfg));

        //     let output = RpnFnScalarEvaluator::new()
        //         .context(ctx)
        //         .push_param(time.clone())
        //         .push_param(format)
        //         .evaluate::<Bytes>(ScalarFuncSig::DateFormatSig);
        //     assert!(output.is_err());
        // }

        {
            let mut cfg = EvalConfig::new();
            cfg.set_flag(Flag::IN_UPDATE_OR_DELETE_STMT)
                .set_sql_mode(SqlMode::NO_ZERO_DATE | SqlMode::STRICT_ALL_TABLES);
            let ctx = EvalContext::new(Arc::new(cfg));

            let output = RpnFnScalarEvaluator::new()
                .context(ctx)
                .push_param(None::<DateTime>)
                .push_param(None::<Bytes>)
                .evaluate::<Bytes>(ScalarFuncSig::DateFormatSig)
                .unwrap();
            assert_eq!(output, None);
        }

        // test date format when format is None
        let cases: Vec<(Option<&str>, Option<&str>)> = vec![
            (Some("2010-01-07 23:12:34.12345"), None),
            (None, None),
            // TODO: pass this test after refactoring the issue #3953 is fixed.
            //            (
            //                "0000-00-00 00:00:00",
            //                Some(
            //                    "%b %M %m %c %D %d %e %j %k %H %i %p %r %T %s %f %v
            //                            %x %Y %y %%",
            //                ),
            //            ),
        ];

        for (date, format) in cases {
            let date = date.map(|d| {
                DateTime::parse_datetime(&mut EvalContext::default(), d, 6, true).unwrap()
            });
            let format = format.map(|s| s.as_bytes().to_vec());

            let output = RpnFnScalarEvaluator::new()
                .push_param(date)
                .push_param(format.clone())
                .evaluate::<Bytes>(ScalarFuncSig::DateFormatSig)
                .unwrap();
            assert_eq!(output, None, "{:?} {:?}", date, format);
        }
    }

    #[test]
    fn test_date() {
        let cases = vec![
            ("2011-11-11", Some("2011-11-11")),
            ("2011-11-11 10:10:10", Some("2011-11-11")),
            ("0000-00-00 00:00:00", None),
        ];
        let mut ctx = EvalContext::default();
        for (date, expect) in cases {
            let date = Some(DateTime::parse_datetime(&mut ctx, date, MAX_FSP, true).unwrap());
            let expect =
                expect.map(|expect| Time::parse_datetime(&mut ctx, expect, MAX_FSP, true).unwrap());

            let output = RpnFnScalarEvaluator::new()
                .push_param(date)
                .evaluate(ScalarFuncSig::Date)
                .unwrap();
            assert_eq!(output, expect, "{:?}", date);
        }
    }

    #[test]
    fn test_week_with_mode() {
        let cases = vec![
            ("2008-02-20 00:00:00", Some(1), Some(8i64)),
            ("2008-12-31 00:00:00", Some(1), Some(53i64)),
            ("2000-01-01", Some(0), Some(0i64)),
            ("2008-02-20", Some(0), Some(7i64)),
            ("2017-01-01", Some(0), Some(1i64)),
            ("2017-01-01", Some(1), Some(0i64)),
            ("2017-01-01", Some(2), Some(1i64)),
            ("2017-01-01", Some(3), Some(52i64)),
            ("2017-01-01", Some(4), Some(1i64)),
            ("2017-01-01", Some(5), Some(0i64)),
            ("2017-01-01", Some(6), Some(1i64)),
            ("2017-01-01", Some(7), Some(52i64)),
            ("2017-12-31", Some(0), Some(53i64)),
            ("2017-12-31", Some(1), Some(52i64)),
            ("2017-12-31", Some(2), Some(53i64)),
            ("2017-12-31", Some(3), Some(52i64)),
            ("2017-12-31", Some(4), Some(53i64)),
            ("2017-12-31", Some(5), Some(52i64)),
            ("2017-12-31", Some(6), Some(1i64)),
            ("2017-12-31", Some(7), Some(52i64)),
            ("2017-12-31", Some(14), Some(1i64)),
            ("2017-12-31", None::<Int>, None),
            ("0000-00-00", Some(0), None),
            ("2018-12-00", Some(0), None),
            ("2018-00-03", Some(0), None),
        ];
        let mut ctx = EvalContext::default();
        for (arg1, arg2, exp) in cases {
            let datetime = Some(DateTime::parse_datetime(&mut ctx, arg1, 6, true).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(datetime)
                .push_param(arg2)
                .evaluate(ScalarFuncSig::WeekWithMode)
                .unwrap();
            assert_eq!(output, exp);
        }
        let output = RpnFnScalarEvaluator::new()
            .push_param(None::<DateTime>)
            .push_param(Some(0))
            .evaluate::<Int>(ScalarFuncSig::WeekWithMode)
            .unwrap();
        assert_eq!(output, None);
    }

    #[test]
    fn test_week_without_mode() {
        let cases = vec![
            ("2000-01-01", Some(0i64)),
            ("2008-02-20", Some(7i64)),
            ("2017-01-01", Some(1i64)),
            ("0000-00-00", None),
            ("2018-12-00", None),
            ("2018-00-03", None),
        ];
        let mut ctx = EvalContext::default();
        for (datetime, exp) in cases {
            let datetime = DateTime::parse_datetime(&mut ctx, datetime, MAX_FSP, true).unwrap();
            let output = RpnFnScalarEvaluator::new()
                .push_param(datetime)
                .evaluate(ScalarFuncSig::WeekWithoutMode)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_week_day() {
        let cases = vec![
            ("2018-12-03", Some(0i64)),
            ("2018-12-04", Some(1i64)),
            ("2018-12-05", Some(2i64)),
            ("2018-12-06", Some(3i64)),
            ("2018-12-07", Some(4i64)),
            ("2018-12-08", Some(5i64)),
            ("2018-12-09", Some(6i64)),
            ("0000-00-00", None),
            ("2018-12-00", None),
            ("2018-00-03", None),
        ];
        let mut ctx = EvalContext::default();
        for (arg, exp) in cases {
            let datetime = Some(DateTime::parse_datetime(&mut ctx, arg, 6, true).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(datetime)
                .evaluate(ScalarFuncSig::WeekDay)
                .unwrap();
            assert_eq!(output, exp);
        }
        let output = RpnFnScalarEvaluator::new()
            .push_param(None::<DateTime>)
            .evaluate::<Int>(ScalarFuncSig::WeekDay)
            .unwrap();
        assert_eq!(output, None);
    }

    #[test]
    fn test_week_of_year() {
        let cases = vec![
            ("2018-01-01", Some(1i64)),
            ("2018-02-28", Some(9i64)),
            ("2018-06-01", Some(22i64)),
            ("2018-07-31", Some(31i64)),
            ("2018-11-01", Some(44i64)),
            ("2018-12-30", Some(52i64)),
            ("2018-12-31", Some(1i64)),
            ("2017-01-01", Some(52i64)),
            ("2017-12-31", Some(52i64)),
            ("0000-00-00", None),
            ("2018-12-00", None),
            ("2018-00-03", None),
        ];
        let mut ctx = EvalContext::default();
        for (arg, exp) in cases {
            let datetime = Some(DateTime::parse_datetime(&mut ctx, arg, 6, true).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(datetime)
                .evaluate(ScalarFuncSig::WeekOfYear)
                .unwrap();
            assert_eq!(output, exp);
        }
        let output = RpnFnScalarEvaluator::new()
            .push_param(None::<DateTime>)
            .evaluate::<Int>(ScalarFuncSig::WeekOfYear)
            .unwrap();
        assert_eq!(output, None);
    }

    #[test]
    fn test_year_week_with_mode() {
        let cases = vec![
            ("1987-01-01", 0, Some(198652i64)),
            ("2000-01-01", 0, Some(199952i64)),
            ("0000-01-01", 0, Some(1i64)),
            ("0000-01-01", 1, Some(4294967295i64)),
            ("0000-01-01", 2, Some(1i64)),
            ("0000-01-01", 3, Some(4294967295i64)),
            ("0000-01-01", 4, Some(1i64)),
            ("0000-01-01", 5, Some(4294967295i64)),
            ("0000-01-01", 6, Some(1i64)),
            ("0000-01-01", 7, Some(4294967295i64)),
            ("0000-01-01", 15, Some(4294967295i64)),
            ("0000-00-00", 0, None),
        ];
        let mut ctx = EvalContext::default();
        for (arg1, arg2, exp) in cases {
            let datetime = DateTime::parse_datetime(&mut ctx, arg1, MAX_FSP, true).unwrap();
            let output = RpnFnScalarEvaluator::new()
                .push_param(datetime)
                .push_param(arg2)
                .evaluate(ScalarFuncSig::YearWeekWithMode)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_year_week_without_mode() {
        let cases = vec![
            ("1987-01-01", Some(198652i64)),
            ("2000-01-01", Some(199952i64)),
            ("0000-01-01", Some(1i64)),
            ("0000-00-00", None),
        ];
        let mut ctx = EvalContext::default();
        for (datetime, exp) in cases {
            let datetime = DateTime::parse_datetime(&mut ctx, datetime, MAX_FSP, true).unwrap();
            let output = RpnFnScalarEvaluator::new()
                .push_param(datetime)
                .evaluate(ScalarFuncSig::YearWeekWithoutMode)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_day_of_week() {
        let cases = vec![
            ("2018-11-11 00:00:00.000000", Some(1)),
            ("2018-11-12 00:00:00.000000", Some(2)),
            ("2018-11-13 00:00:00.000000", Some(3)),
            ("2018-11-14 00:00:00.000000", Some(4)),
            ("2018-11-15 00:00:00.000000", Some(5)),
            ("2018-11-16 00:00:00.000000", Some(6)),
            ("2018-11-17 00:00:00.000000", Some(7)),
            ("2018-11-18 00:00:00.000000", Some(1)),
            ("0000-00-00 00:00:00.000000", None),
            ("2018-11-00 00:00:00.000000", None),
            ("2018-00-11 00:00:00.000000", None),
        ];
        let mut ctx = EvalContext::default();
        for (arg, exp) in cases {
            let datetime = Some(DateTime::parse_datetime(&mut ctx, arg, 6, true).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(datetime)
                .evaluate(ScalarFuncSig::DayOfWeek)
                .unwrap();
            assert_eq!(output, exp);
        }
        let output = RpnFnScalarEvaluator::new()
            .push_param(None::<DateTime>)
            .evaluate::<Int>(ScalarFuncSig::DayOfWeek)
            .unwrap();
        assert_eq!(output, None);
    }

    #[test]
    fn test_day_of_year() {
        let cases = vec![
            ("2018-11-11 00:00:00.000000", Some(315)),
            ("2018-11-12 00:00:00.000000", Some(316)),
            ("2018-11-30 00:00:00.000000", Some(334)),
            ("2018-12-31 00:00:00.000000", Some(365)),
            ("2016-12-31 00:00:00.000000", Some(366)),
            ("0000-00-00 00:00:00.000000", None),
            ("2018-11-00 00:00:00.000000", None),
            ("2018-00-11 00:00:00.000000", None),
        ];
        let mut ctx = EvalContext::default();
        for (arg, exp) in cases {
            let datetime = Some(DateTime::parse_datetime(&mut ctx, arg, 6, true).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(datetime)
                .evaluate(ScalarFuncSig::DayOfYear)
                .unwrap();
            assert_eq!(output, exp);
        }
        let output = RpnFnScalarEvaluator::new()
            .push_param(None::<DateTime>)
            .evaluate::<Int>(ScalarFuncSig::DayOfYear)
            .unwrap();
        assert_eq!(output, None);
    }

    #[test]
    fn test_to_days() {
        let cases = vec![
            (Some("950501"), Some(728779)),
            (Some("2007-10-07"), Some(733321)),
            (Some("2008-10-07"), Some(733687)),
            (Some("08-10-07"), Some(733687)),
            (Some("0000-01-01"), Some(1)),
            (Some("2007-10-07 00:00:59"), Some(733321)),
            (Some("0000-00-00 00:00:00"), None),
            (None, None),
        ];

        let mut ctx = EvalContext::default();
        for (arg, exp) in cases {
            let time = arg.map(|arg| Time::parse_datetime(&mut ctx, arg, 6, true).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(time)
                .evaluate(ScalarFuncSig::ToDays)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_to_seconds() {
        let cases = vec![
            ("950501", Some(62966505600)),
            ("2009-11-29", Some(63426672000)),
            ("2009-11-29 13:43:32", Some(63426721412)),
            ("09-11-29 13:43:32", Some(63426721412)),
            ("99-11-29 13:43:32", Some(63111102212)),
            ("0000-00-00 00:00:00", None),
        ];

        let mut ctx = EvalContext::default();
        for (arg, exp) in cases {
            let time = Time::parse_datetime(&mut ctx, arg, 6, true).unwrap();
            let output = RpnFnScalarEvaluator::new()
                .push_param(time)
                .evaluate(ScalarFuncSig::ToSeconds)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_add_sub_string_and_duration() {
        let cases = vec![
            // normal cases
            (
                Some("01:00:00.999999"),
                Some("02:00:00.999998"),
                Some("03:00:01.999997"),
            ),
            (Some("23:59:59"), Some("00:00:01"), Some("24:00:00")),
            (Some("110:00:00"), Some("1 02:00:00"), Some("136:00:00")),
            (Some("-110:00:00"), Some("1 02:00:00"), Some("-84:00:00")),
            (Some("00:00:01"), Some("-00:00:01"), Some("00:00:00")),
            (Some("00:00:03"), Some("-00:00:01"), Some("00:00:02")),
            (
                Some("2018-02-28 23:00:00"),
                Some("01:30:30.123456"),
                Some("2018-03-01 00:30:30.123456"),
            ),
            (
                Some("2016-02-28 23:00:00"),
                Some("01:30:30"),
                Some("2016-02-29 00:30:30"),
            ),
            (
                Some("2018-12-31 23:00:00"),
                Some("01:30:30"),
                Some("2019-01-01 00:30:30"),
            ),
            (
                Some("2018-12-31 23:00:00"),
                Some("1 01:30:30"),
                Some("2019-01-02 00:30:30"),
            ),
            (
                Some("2019-01-01 01:00:00"),
                Some("-01:01:00"),
                Some("2018-12-31 23:59:00"),
            ),
            // null cases
            (None, None, None),
            (None, Some("11:30:45.123456"), None),
            (Some("00:00:00"), None, Some("00:00:00")),
            (Some("01:00:00"), None, Some("01:00:00")),
            (
                Some("2019-01-01 01:00:00"),
                None,
                Some("2019-01-01 01:00:00"),
            ),
        ];

        for (arg_str, arg_dur, sum) in cases {
            let arg_str = arg_str.map(|str| str.as_bytes().to_vec());
            let arg_dur = match arg_dur {
                Some(arg_dur) => {
                    Some(Duration::parse(&mut EvalContext::default(), arg_dur, MAX_FSP).unwrap())
                }
                None => Some(Duration::zero()),
            };
            let sum = sum.map(|str| str.as_bytes().to_vec());
            let add_output = RpnFnScalarEvaluator::new()
                .push_param(arg_str.clone())
                .push_param(arg_dur)
                .evaluate(ScalarFuncSig::AddStringAndDuration)
                .unwrap();
            assert_eq!(add_output, sum);

            let sub_output = RpnFnScalarEvaluator::new()
                .push_param(sum)
                .push_param(arg_dur)
                .evaluate(ScalarFuncSig::SubStringAndDuration)
                .unwrap();
            assert_eq!(sub_output, arg_str);
        }
    }

    #[test]
    fn test_date_diff() {
        let cases = vec![
            (
                "0000-01-01 00:00:00.000000",
                "0000-01-01 00:00:00.000000",
                Some(0),
            ),
            (
                "2018-02-01 00:00:00.000000",
                "2018-02-01 00:00:00.000000",
                Some(0),
            ),
            (
                "2018-02-02 00:00:00.000000",
                "2018-02-01 00:00:00.000000",
                Some(1),
            ),
            (
                "2018-02-01 00:00:00.000000",
                "2018-02-02 00:00:00.000000",
                Some(-1),
            ),
            (
                "2018-02-02 00:00:00.000000",
                "2018-02-01 23:59:59.999999",
                Some(1),
            ),
            (
                "2018-02-01 23:59:59.999999",
                "2018-02-02 00:00:00.000000",
                Some(-1),
            ),
        ];
        let mut ctx = EvalContext::default();
        for (arg1, arg2, exp) in cases {
            let arg1 = Time::parse_datetime(&mut ctx, arg1, 6, true).unwrap();
            let arg2 = Time::parse_datetime(&mut ctx, arg2, 6, true).unwrap();
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg1)
                .push_param(arg2)
                .evaluate(ScalarFuncSig::DateDiff)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_null_time_diff() {
        let output = RpnFnScalarEvaluator::new()
            .evaluate::<Duration>(ScalarFuncSig::NullTimeDiff)
            .unwrap();
        assert_eq!(output, None);
    }

    #[test]
    fn test_add_datetime_and_duration() {
        let mut ctx = EvalContext::default();
        let cases = vec![
            // null cases
            (None, None, None),
            (None, Some("11:30:45.123456"), None),
            (Some("2019-01-01 01:00:00"), None, None),
            // normal cases
            (
                Some("2018-01-01"),
                Some("11:30:45.123456"),
                Some("2018-01-01 11:30:45.123456"),
            ),
            (
                Some("2018-02-28 23:00:00"),
                Some("01:30:30.123456"),
                Some("2018-03-01 00:30:30.123456"),
            ),
            (
                Some("2016-02-28 23:00:00"),
                Some("01:30:30"),
                Some("2016-02-29 00:30:30"),
            ),
            (
                Some("2018-12-31 23:00:00"),
                Some("01:30:30"),
                Some("2019-01-01 00:30:30"),
            ),
            (
                Some("2018-12-31 23:00:00"),
                Some("1 01:30:30"),
                Some("2019-01-02 00:30:30"),
            ),
            (
                Some("2019-01-01 01:00:00"),
                Some("-01:01:00"),
                Some("2018-12-31 23:59:00"),
            ),
        ];
        for (arg1, arg2, exp) in cases {
            let exp = exp.map(|exp| Time::parse_datetime(&mut ctx, exp, MAX_FSP, true).unwrap());
            let arg1 =
                arg1.map(|arg1| Time::parse_datetime(&mut ctx, arg1, MAX_FSP, true).unwrap());
            let arg2 = arg2.map(|arg2| Duration::parse(&mut ctx, arg2, MAX_FSP).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg1)
                .push_param(arg2)
                .evaluate(ScalarFuncSig::AddDatetimeAndDuration)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_add_datetime_and_string() {
        let mut ctx = EvalContext::default();
        let cases = vec![
            // null cases
            (None, None, None),
            (None, Some("11:30:45.123456"), None),
            (Some("2019-01-01 01:00:00"), None, None),
            // normal cases
            (
                Some("2018-01-01"),
                Some("11:30:45.123456"),
                Some("2018-01-01 11:30:45.123456"),
            ),
            (
                Some("2018-02-28 23:00:00"),
                Some("01:30:30.123456"),
                Some("2018-03-01 00:30:30.123456"),
            ),
            (
                Some("2016-02-28 23:00:00"),
                Some("01:30:30"),
                Some("2016-02-29 00:30:30"),
            ),
            (
                Some("2018-12-31 23:00:00"),
                Some("01:30:30"),
                Some("2019-01-01 00:30:30"),
            ),
        ];
        for (arg0, arg1, exp) in cases {
            let exp = exp.map(|exp| Time::parse_datetime(&mut ctx, exp, MAX_FSP, true).unwrap());
            let arg0 =
                arg0.map(|arg0| Time::parse_datetime(&mut ctx, arg0, MAX_FSP, true).unwrap());
            let arg1 = arg1.map(|str| str.as_bytes().to_vec());

            let output = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .push_param(arg1)
                .evaluate(ScalarFuncSig::AddDatetimeAndString)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_sub_datetime_and_duration() {
        let mut ctx = EvalContext::default();
        let cases = vec![
            // null cases
            (None, None, None, true),
            (None, Some("11:30:45.123456"), None, true),
            (Some("2019-01-01 01:00:00"), None, None, true),
            // normal cases
            (
                Some("2018-01-01"),
                Some("11:30:45.123456"),
                Some("2018-01-01 11:30:45.123456"),
                false,
            ),
            (
                Some("2018-02-28 23:00:00"),
                Some("01:30:30.123456"),
                Some("2018-03-01 00:30:30.123456"),
                false,
            ),
            (
                Some("2016-02-28 23:00:00"),
                Some("01:30:30"),
                Some("2016-02-29 00:30:30"),
                false,
            ),
            (
                Some("2018-12-31 23:00:00"),
                Some("01:30:30"),
                Some("2019-01-01 00:30:30"),
                false,
            ),
            (
                Some("2018-12-31 23:00:00"),
                Some("1 01:30:30"),
                Some("2019-01-02 00:30:30"),
                false,
            ),
            (
                Some("2019-01-01 01:00:00"),
                Some("-01:01:00"),
                Some("2018-12-31 23:59:00"),
                false,
            ),
        ];
        for (arg1, arg2, exp, null_case) in cases {
            let exp = exp.map(|exp| Time::parse_datetime(&mut ctx, exp, MAX_FSP, true).unwrap());
            let arg1 =
                arg1.map(|arg1| Time::parse_datetime(&mut ctx, arg1, MAX_FSP, true).unwrap());
            let arg2 = arg2.map(|arg2| Duration::parse(&mut ctx, arg2, MAX_FSP).unwrap());
            if null_case {
                let output = RpnFnScalarEvaluator::new()
                    .push_param(arg1)
                    .push_param(arg2)
                    .evaluate(ScalarFuncSig::SubDatetimeAndDuration)
                    .unwrap();
                assert_eq!(output, exp);
            } else {
                let output = RpnFnScalarEvaluator::new()
                    .push_param(exp)
                    .push_param(arg2)
                    .evaluate(ScalarFuncSig::SubDatetimeAndDuration)
                    .unwrap();
                assert_eq!(output, arg1);
            }
        }
    }

    #[test]
    fn test_sub_datetime_and_string() {
        let mut ctx = EvalContext::default();
        let cases = vec![
            // null cases
            (None, None, None),
            (None, Some("11:30:45.123456"), None),
            (Some("2019-01-01 01:00:00"), None, None),
            // normal cases
            (
                Some("2018-01-01 11:30:45.123456"),
                Some("11:30:45.123456"),
                Some("2018-01-01"),
            ),
            (
                Some("2018-03-01 00:30:30.123456"),
                Some("01:30:30.123456"),
                Some("2018-02-28 23:00:00"),
            ),
            (
                Some("2016-02-29 00:30:30"),
                Some("01:30:30"),
                Some("2016-02-28 23:00:00"),
            ),
            (
                Some("2019-01-01 00:30:30"),
                Some("01:30:30"),
                Some("2018-12-31 23:00:00"),
            ),
        ];
        for (arg0, arg1, exp) in cases {
            let exp = exp.map(|exp| Time::parse_datetime(&mut ctx, exp, MAX_FSP, true).unwrap());
            let arg0 =
                arg0.map(|arg0| Time::parse_datetime(&mut ctx, arg0, MAX_FSP, true).unwrap());
            let arg1 = arg1.map(|str| str.as_bytes().to_vec());

            let output = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .push_param(arg1)
                .evaluate(ScalarFuncSig::SubDatetimeAndString)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_add_date_and_string() {
        let mut ctx = EvalContext::default();
        let cases = vec![
            (Some("2021-03-26"), Some("00:00:00"), Some("2021-03-26")),
            (Some("2021-03-26"), Some("23:59:59"), Some("2021-03-26")),
            (Some("2021-03-26"), Some("24:00:01"), Some("2021-03-27")),
            (Some("2021-03-26"), Some("48:00:01"), Some("2021-03-28")),
            (Some("2021-03-26"), Some("-00:01:00"), Some("2021-03-25")),
            (
                Some("2021-03-26 00:00:30"),
                Some("-00:01:00"),
                Some("2021-03-25"),
            ),
            (
                Some("2018-12-31 23:00:00"),
                Some("1 01:30:30"),
                Some("2019-01-02"),
            ),
            (
                Some("2021-03-26"),
                Some("00:00:00.123456"),
                Some("2021-03-26"),
            ),
            // null cases
            (None, None, None),
            (None, Some("11:30:45.123456"), None),
            (Some("2019-01-01 01:00:00"), None, None),
        ];
        for (arg0, arg1, exp) in cases {
            let exp = exp.map(|exp| Time::parse_datetime(&mut ctx, exp, MAX_FSP, true).unwrap());
            let arg0 =
                arg0.map(|arg0| Time::parse_datetime(&mut ctx, arg0, MAX_FSP, true).unwrap());
            let arg1 = arg1.map(|arg1| arg1.as_bytes().to_vec());

            let output = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .push_param(arg1)
                .evaluate(ScalarFuncSig::AddDateAndString);
            let output = output.unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_add_time_result_null() {
        let mut ctx = EvalContext::default();
        let cases = vec![
            (Some("2021-03-26"), Some("2021-03-26 00:00:00")),
            (Some("2021-03-26 00:00:00"), Some("2021-03-26")),
            (Some("2021-03-26 00:00:00"), Some("2021-03-26 23:59:59")),
        ];
        for (arg0, arg1) in cases {
            let arg0 =
                arg0.map(|arg0| Time::parse_datetime(&mut ctx, arg0, MAX_FSP, true).unwrap());
            let arg1 =
                arg1.map(|arg1| Time::parse_datetime(&mut ctx, arg1, MAX_FSP, true).unwrap());

            let output: Result<Option<DateTime>> = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .push_param(arg1)
                .evaluate(ScalarFuncSig::AddTimeDateTimeNull);
            let output = output.unwrap();
            assert_eq!(output, None);

            let output: Result<Option<Duration>> = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .push_param(arg1)
                .evaluate(ScalarFuncSig::AddTimeDurationNull);
            let output = output.unwrap();
            assert_eq!(output, None);

            let output: Result<Option<Bytes>> = RpnFnScalarEvaluator::new()
                .push_param(arg0)
                .push_param(arg1)
                .evaluate(ScalarFuncSig::AddTimeStringNull);
            let output = output.unwrap();
            assert_eq!(output, None);
        }
    }

    #[test]
    fn test_from_days() {
        let cases = vec![
            (ScalarValue::Int(Some(-140)), Some("0000-00-00")), /* mysql FROM_DAYS returns
                                                                 * 0000-00-00 for any day <=
                                                                 * 365. */
            (ScalarValue::Int(Some(140)), Some("0000-00-00")), /* mysql FROM_DAYS returns 0000-00-00 for any day <= 365. */
            (ScalarValue::Int(Some(735_000)), Some("2012-05-12")), // Leap year.
            (ScalarValue::Int(Some(735_030)), Some("2012-06-11")),
            (ScalarValue::Int(Some(735_130)), Some("2012-09-19")),
            (ScalarValue::Int(Some(734_909)), Some("2012-02-11")),
            (ScalarValue::Int(Some(734_878)), Some("2012-01-11")),
            (ScalarValue::Int(Some(734_927)), Some("2012-02-29")),
            (ScalarValue::Int(Some(734_634)), Some("2011-05-12")), // Non Leap year.
            (ScalarValue::Int(Some(734_664)), Some("2011-06-11")),
            (ScalarValue::Int(Some(734_764)), Some("2011-09-19")),
            (ScalarValue::Int(Some(734_544)), Some("2011-02-11")),
            (ScalarValue::Int(Some(734_513)), Some("2011-01-11")),
            (ScalarValue::Int(Some(3_652_424)), Some("9999-12-31")),
            (ScalarValue::Int(Some(3_652_425)), Some("0000-00-00")), /* mysql FROM_DAYS returns
                                                                      * 0000-00-00 for any day
                                                                      * >= 3652425 */
            (ScalarValue::Int(None), None),
        ];
        let mut ctx = EvalContext::default();
        for (arg, exp) in cases {
            let datetime: Option<Time> =
                exp.map(|exp: &str| Time::parse_date(&mut ctx, exp).unwrap());
            let output: Option<Time> = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .evaluate(ScalarFuncSig::FromDays)
                .unwrap();
            assert_eq!(output, datetime);
        }
    }

    #[test]
    fn test_make_date() {
        let null_cases = vec![
            (None, None),
            (Some(2014i64), Some(0i64)),
            (Some(10000i64), Some(1i64)),
            (Some(9999i64), Some(366i64)),
            (Some(-1i64), Some(1i64)),
            (Some(-4294965282i64), Some(1i64)),
            (Some(0i64), Some(0i64)),
            (Some(0i64), Some(-1i64)),
            (Some(10i64), Some(-1i64)),
            (Some(0i64), Some(9223372036854775807i64)),
            (Some(100i64), Some(9999 * 366i64)),
            (Some(9999i64), Some(9999 * 366i64)),
            (Some(100i64), Some(3615901i64)),
        ];
        for (arg1, arg2) in null_cases {
            let exp: Option<Time> = None;
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg1)
                .push_param(arg2)
                .evaluate(ScalarFuncSig::MakeDate)
                .unwrap();
            assert_eq!(output, exp);
        }
        let cases = vec![
            (0, 1, "2000-01-01"),
            (70, 1, "1970-01-01"),
            (71, 1, "1971-01-01"),
            (99, 1, "1999-01-01"),
            (100, 1, "0100-01-01"),
            (101, 1, "0101-01-01"),
            (2014, 224234, "2627-12-07"),
            (2014, 1, "2014-01-01"),
            (7900, 705000, "9830-03-23"),
            (7901, 705000, "9831-03-23"),
            (7904, 705000, "9834-03-22"),
            (8000, 705000, "9930-03-23"),
            (8001, 705000, "9931-03-24"),
            (100, 3615900, "9999-12-31"),
        ];
        let mut ctx = EvalContext::default();
        for (arg1, arg2, exp) in cases {
            let exp = Some(Time::parse_date(&mut ctx, exp).unwrap());
            let arg2 = Some(arg2);
            let arg1 = Some(arg1);
            let output = RpnFnScalarEvaluator::new()
                .push_param(arg1)
                .push_param(arg2)
                .evaluate(ScalarFuncSig::MakeDate)
                .unwrap();
            assert_eq!(output, exp);
        }
    }

    #[test]
    fn test_month() {
        let cases = vec![
            (Some("0000-00-00 00:00:00"), Some(0i64)),
            (Some("2018-01-01 01:01:01"), Some(1i64)),
            (Some("2018-02-01 01:01:01"), Some(2i64)),
            (Some("2018-03-01 01:01:01"), Some(3i64)),
            (Some("2018-04-01 01:01:01"), Some(4i64)),
            (Some("2018-05-01 01:01:01"), Some(5i64)),
            (Some("2018-06-01 01:01:01"), Some(6i64)),
            (Some("2018-07-01 01:01:01"), Some(7i64)),
            (Some("2018-08-01 01:01:01"), Some(8i64)),
            (Some("2018-09-01 01:01:01"), Some(9i64)),
            (Some("2018-10-01 01:01:01"), Some(10i64)),
            (Some("2018-11-01 01:01:01"), Some(11i64)),
            (Some("2018-12-01 01:01:01"), Some(12i64)),
            (None, None),
        ];
        let mut ctx = EvalContext::default();
        for (time, expect) in cases {
            let time = time.map(|t| DateTime::parse_datetime(&mut ctx, t, 6, true).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(time)
                .evaluate(ScalarFuncSig::Month)
                .unwrap();
            assert_eq!(output, expect);
        }
    }

    #[test]
    fn test_month_name() {
        let cases = vec![
            (None, None, None),
            (
                Some("0000-00-00 00:00:00"),
                Some(ERR_TRUNCATE_WRONG_VALUE),
                None,
            ),
            (
                Some("2018-01-00 01:01:01"),
                Some(ERR_TRUNCATE_WRONG_VALUE),
                None,
            ),
            (
                Some("2018-00-00 01:01:01"),
                Some(ERR_TRUNCATE_WRONG_VALUE),
                None,
            ),
            (
                Some("2018-00-01 01:01:01"),
                Some(ERR_TRUNCATE_WRONG_VALUE),
                None,
            ),
            (Some("2018-01-01 01:01:01"), None, Some("January")),
            (Some("2018-02-01 01:01:01"), None, Some("February")),
            (Some("2018-03-01 01:01:01"), None, Some("March")),
            (Some("2018-04-01 01:01:01"), None, Some("April")),
            (Some("2018-05-01 01:01:01"), None, Some("May")),
            (Some("2018-06-01 01:01:01"), None, Some("June")),
            (Some("2018-07-01 01:01:01"), None, Some("July")),
            (Some("2018-08-01 01:01:01"), None, Some("August")),
            (Some("2018-09-01 01:01:01"), None, Some("September")),
            (Some("2018-10-01 01:01:01"), None, Some("October")),
            (Some("2018-11-01 01:01:01"), None, Some("November")),
            (Some("2018-12-01 01:01:01"), None, Some("December")),
        ];
        for (time, err_code, expect) in cases {
            let mut ctx = EvalContext::default();
            let time = time.map(|time: &str| Time::parse_date(&mut ctx, time).unwrap());
            let (output, ctx) = RpnFnScalarEvaluator::new()
                .push_param(time)
                .context(ctx)
                .evaluate_raw(FieldTypeTp::String, ScalarFuncSig::MonthName);
            let output = output.unwrap();
            assert_eq!(output.as_bytes(), expect.map(|v| v.as_bytes()));
            if let Some(err_code) = err_code {
                assert_eq!(ctx.warnings.warnings[0].get_code(), err_code);
            }
        }
    }

    #[test]
    fn test_hour_min_sec_micro_sec() {
        // test hour, minute, second, micro_second
        let cases: Vec<(&str, i8, i64, i64, i64, i64)> = vec![
            ("0 00:00:00.0", 0, 0, 0, 0, 0),
            ("31 11:30:45", 0, 31 * 24 + 11, 30, 45, 0),
            ("11:30:45.123345", 3, 11, 30, 45, 123000),
            ("11:30:45.123345", 5, 11, 30, 45, 123350),
            ("11:30:45.123345", 6, 11, 30, 45, 123345),
            ("11:30:45.1233456", 6, 11, 30, 45, 123346),
            ("11:30:45.000010", 6, 11, 30, 45, 10),
            ("11:30:45.00010", 5, 11, 30, 45, 100),
            ("-11:30:45.9233456", 0, 11, 30, 46, 0),
            ("-11:30:45.9233456", 1, 11, 30, 45, 900000),
            ("272:59:59.94", 2, 272, 59, 59, 940000),
            ("272:59:59.99", 1, 273, 0, 0, 0),
            ("272:59:59.99", 0, 273, 0, 0, 0),
        ];

        for (arg, fsp, h, m, s, ms) in cases {
            let duration = Some(Duration::parse(&mut EvalContext::default(), arg, fsp).unwrap());
            let test_case_func = |sig, res| {
                let output = RpnFnScalarEvaluator::new()
                    .push_param(duration)
                    .evaluate::<Int>(sig)
                    .unwrap();
                assert_eq!(output, Some(res));
            };
            test_case_func(ScalarFuncSig::Hour, h);
            test_case_func(ScalarFuncSig::Minute, m);
            test_case_func(ScalarFuncSig::Second, s);
            test_case_func(ScalarFuncSig::MicroSecond, ms);
        }

        // test NULL case
        let test_null_case = |sig| {
            let output = RpnFnScalarEvaluator::new()
                .push_param(None::<Duration>)
                .evaluate::<Int>(sig)
                .unwrap();
            assert_eq!(output, None);
        };

        test_null_case(ScalarFuncSig::Hour);
        test_null_case(ScalarFuncSig::Minute);
        test_null_case(ScalarFuncSig::Second);
        test_null_case(ScalarFuncSig::MicroSecond);
    }

    #[test]
    fn test_time_to_sec() {
        let cases: Vec<(&str, i8, i64)> = vec![
            ("31 11:30:45", 0, 2719845),
            ("11:30:45.123345", 3, 41445),
            ("-11:30:45.1233456", 0, -41445),
            ("272:59:59.14", 0, 982799),
        ];
        for (arg, fsp, s) in cases {
            let duration = Some(Duration::parse(&mut EvalContext::default(), arg, fsp).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(duration)
                .evaluate::<Int>(ScalarFuncSig::TimeToSec)
                .unwrap();
            assert_eq!(output, Some(s));
        }
        // test NULL case
        let output = RpnFnScalarEvaluator::new()
            .push_param(None::<Duration>)
            .evaluate::<Int>(ScalarFuncSig::TimeToSec)
            .unwrap();
        assert_eq!(output, None);
    }

    #[test]
    fn test_year() {
        let cases = vec![
            (Some("0000-00-00 00:00:00"), Some(0i64)),
            (Some("1-01-01 01:01:01"), Some(1i64)),
            (Some("2018-01-01 01:01:01"), Some(2018i64)),
            (Some("2019-01-01 01:01:01"), Some(2019i64)),
            (Some("2020-01-01 01:01:01"), Some(2020i64)),
            (Some("2021-01-01 01:01:01"), Some(2021i64)),
            (Some("2022-01-01 01:01:01"), Some(2022i64)),
            (Some("2023-01-01 01:01:01"), Some(2023i64)),
            (Some("2024-01-01 01:01:01"), Some(2024i64)),
            (Some("2025-01-01 01:01:01"), Some(2025i64)),
            (Some("2026-01-01 01:01:01"), Some(2026i64)),
            (Some("2027-01-01 01:01:01"), Some(2027i64)),
            (Some("2028-01-01 01:01:01"), Some(2028i64)),
            (Some("2029-01-01 01:01:01"), Some(2029i64)),
            (None, None),
        ];

        let mut ctx = EvalContext::default();
        for (time, expect) in cases {
            let time = time.map(|t| DateTime::parse_datetime(&mut ctx, t, 6, true).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(time)
                .evaluate(ScalarFuncSig::Year)
                .unwrap();
            assert_eq!(output, expect);
        }
    }

    #[test]
    fn test_day_of_month() {
        let cases = vec![
            (Some("0000-00-00 00:00:00.000000"), Some(0)),
            (Some("2018-02-01 00:00:00.000000"), Some(1)),
            (Some("2018-02-15 00:00:00.000000"), Some(15)),
            (Some("2018-02-28 00:00:00.000000"), Some(28)),
            (Some("2016-02-29 00:00:00.000000"), Some(29)),
            (None, None),
        ];
        let mut ctx = EvalContext::default();
        for (time, expect) in cases {
            let time = time.map(|t| DateTime::parse_datetime(&mut ctx, t, 6, true).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(time)
                .evaluate(ScalarFuncSig::DayOfMonth)
                .unwrap();
            assert_eq!(output, expect);
        }
    }

    #[test]
    fn test_day_name() {
        let cases = vec![
            (None, None, None),
            (Some("0000-00-00"), Some(ERR_TRUNCATE_WRONG_VALUE), None),
            (Some("2019-11-17"), None, Some("Sunday")),
            (Some("2019-11-18"), None, Some("Monday")),
            (Some("2019-11-19"), None, Some("Tuesday")),
            (Some("2019-11-20"), None, Some("Wednesday")),
            (Some("2019-11-21"), None, Some("Thursday")),
            (Some("2019-11-22"), None, Some("Friday")),
            (Some("2019-11-23"), None, Some("Saturday")),
            (Some("2019-11-24"), None, Some("Sunday")),
            (Some("2019-11-00"), Some(ERR_TRUNCATE_WRONG_VALUE), None),
            (Some("2019-00-00"), Some(ERR_TRUNCATE_WRONG_VALUE), None),
            (Some("2019-00-01"), Some(ERR_TRUNCATE_WRONG_VALUE), None),
            (Some("2019-11-24 00:00:00.000000"), None, Some("Sunday")),
        ];

        for (arg, err_code, exp) in cases {
            let mut ctx = EvalContext::default();
            let arg = arg.map(|arg: &str| Time::parse_date(&mut ctx, arg).unwrap());
            let (output, ctx) = RpnFnScalarEvaluator::new()
                .push_param(arg)
                .context(ctx)
                .evaluate_raw(FieldTypeTp::String, ScalarFuncSig::DayName);
            let output = output.unwrap();
            assert_eq!(output.as_bytes(), exp.map(|v| v.as_bytes()));
            if let Some(err_code) = err_code {
                assert_eq!(ctx.warnings.warnings[0].get_code(), err_code);
            }
        }
    }

    #[test]
    fn test_period_add() {
        let cases = vec![
            (2, 222, 201808),
            (0, 222, 0),
            (196802, 14, 196904),
            (6901, 13, 207002),
            (7001, 13, 197102),
            (200212, 9223372036854775807, 200211),
            (9223372036854775807, 0, 27201459511),
            (9223372036854775807, 9223372036854775807, 27201459510),
            (201611, 2, 201701),
            (201611, 3, 201702),
            (201611, -13, 201510),
            (1611, 3, 201702),
            (7011, 3, 197102),
            (12323, 10, 12509),
            (0, 3, 0),
        ];
        for (arg1, arg2, exp) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(Some(arg1))
                .push_param(Some(arg2))
                .evaluate(ScalarFuncSig::PeriodAdd)
                .unwrap();
            assert_eq!(output, Some(exp));
        }
    }

    #[test]
    fn test_period_diff() {
        let cases = vec![
            (213002, 7010, 1912),
            (213002, 215810, -344),
            (2202, 9601, 313),
            (202202, 9601, 313),
            (200806, 6907, -733),
            (201611, 201611, 0),
            (200802, 200703, 11),
            (0, 999999999, -120000086),
            (9999999, 0, 1200086),
            (411, 200413, -2),
            (197000, 207700, -1284),
            (201701, 201611, 2),
            (201702, 201611, 3),
            (201510, 201611, -13),
            (201702, 1611, 3),
            (197102, 7011, 3),
            (12509, 12323, 10),
            (12509, 12323, 10),
        ];

        for (arg1, arg2, exp) in cases {
            let output = RpnFnScalarEvaluator::new()
                .push_param(Some(arg1))
                .push_param(Some(arg2))
                .evaluate(ScalarFuncSig::PeriodDiff)
                .unwrap();
            assert_eq!(output, Some(exp));
        }
    }

    #[test]
    fn test_last_day() {
        let cases = vec![
            ("2011-11-11", "2011-11-30"),
            ("2008-02-10", "2008-02-29"),
            ("2000-02-11", "2000-02-29"),
            ("2100-02-11", "2100-02-28"),
            ("2011-11-11", "2011-11-30"),
            ("2011-11-11 10:10:10", "2011-11-30 00:00:00"),
            ("2011-01-00 10:00:00", "2011-01-31"),
        ];
        let mut ctx = EvalContext::default();
        for (arg, exp) in cases {
            let time = Some(Time::parse_date(&mut ctx, arg).unwrap());
            let exp_val = Some(Time::parse_date(&mut ctx, exp).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(time)
                .evaluate(ScalarFuncSig::LastDay)
                .unwrap();
            assert_eq!(output, exp_val);
        }
        let none_cases = vec!["2011-00-01 10:10:10"];
        for case in none_cases {
            let time = Some(Time::parse_date(&mut ctx, case).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(time)
                .evaluate::<Time>(ScalarFuncSig::LastDay)
                .unwrap();
            assert_eq!(output, None);
        }
        let output = RpnFnScalarEvaluator::new()
            .push_param(None::<Time>)
            .evaluate::<Time>(ScalarFuncSig::LastDay)
            .unwrap();
        assert_eq!(output, None);
    }

    #[test]
    fn test_make_time() {
        let cases = vec![
            (12_i64, 15_i64, 30_f64, "12:15:30"),
            (25, 15, 30.0, "25:15:30"),
            (-25, 15, 30_f64, "-25:15:30"),
            (12, 15, 30.1, "12:15:30.1"),
            (12, 15, 30.2, "12:15:30.2"),
            (12, 15, 30.3000001, "12:15:30.3"),
            (12, 15, 30.0000005, "12:15:30.000001"),
            (0, 0, 0.0, "00:00:00"),
            (0, 1, 59.1, "00:01:59.1"),
            (837, 59, 59.1, "837:59:59.1"),
            (838, 59, 59.1, "838:59:59"),
            (838, 58, 59.1, "838:58:59.1"),
            (838, 0, 59.1, "838:00:59.1"),
            (-838, 59, 59.1, "-838:59:59"),
            (1000, 1, 1.0, "838:59:59"),
            (-1000, 1, 1.23, "-838:59:59"),
            (1000, 59, 1.0, "838:59:59"),
            (1000, 1, 59.1, "838:59:59"),
            (i64::MIN, 0, 1.0, "-838:59:59"),
            (i64::MAX, 0, 0.0, "838:59:59"),
        ];
        let mut ctx = EvalContext::default();
        for (hour, minute, second, ans) in cases {
            for fsp in 0..MAX_FSP {
                let ans_val = Some(Duration::parse(&mut ctx, ans, fsp).unwrap());
                let output = RpnFnScalarEvaluator::new()
                    .push_param(Some(hour))
                    .push_param(Some(minute))
                    .push_param(Some(Real::new(second).unwrap()))
                    .return_field_type(
                        FieldTypeBuilder::new()
                            .tp(FieldTypeTp::Duration)
                            .decimal(fsp as isize)
                            .build(),
                    )
                    .evaluate(ScalarFuncSig::MakeTime)
                    .unwrap();
                assert_eq!(output, ans_val);
            }
        }
        let none_case = vec![
            (12_i64, -15_i64, 30_f64),
            (12, 15, -30.0),
            (12, 15, 60.0),
            (12, 60, 0.0),
            (i64::MAX, i64::MAX, f64::MAX),
        ];
        for (hour, minute, second) in none_case {
            for fsp in 0..MAX_FSP {
                let output = RpnFnScalarEvaluator::new()
                    .push_param(Some(hour))
                    .push_param(Some(minute))
                    .push_param(Some(Real::new(second).unwrap()))
                    .return_field_type(
                        FieldTypeBuilder::new()
                            .tp(FieldTypeTp::Duration)
                            .decimal(fsp as isize)
                            .build(),
                    )
                    .evaluate::<Duration>(ScalarFuncSig::MakeTime)
                    .unwrap();
                assert_eq!(output, None);
            }
        }
        {
            let output = RpnFnScalarEvaluator::new()
                .push_param(None::<i64>)
                .push_param(None::<i64>)
                .push_param(None::<Real>)
                .return_field_type(
                    FieldTypeBuilder::new()
                        .tp(FieldTypeTp::Duration)
                        .decimal(MAX_FSP as isize)
                        .build(),
                )
                .evaluate::<Duration>(ScalarFuncSig::MakeTime)
                .unwrap();
            assert_eq!(output, None);
        }
        {
            let output = RpnFnScalarEvaluator::new()
                .push_param(Some::<i64>(1))
                .push_param(Some::<i64>(1))
                .push_param(Some::<f64>(1.0))
                .return_field_type(
                    FieldTypeBuilder::new()
                        .tp(FieldTypeTp::Duration)
                        .decimal((MAX_FSP + 1) as isize)
                        .build(),
                )
                .evaluate::<Duration>(ScalarFuncSig::MakeTime);
            output.unwrap_err();
        }
    }

    #[test]
    fn test_duration_duration_time_diff() {
        let cases = vec![
            (Some("00:02:02"), Some("00:01:01"), Some("00:01:01")),
            (Some("12:00:00"), Some("00:00:01"), Some("11:59:59")),
            (Some("24:00:00"), Some("00:00:01"), Some("23:59:59")),
            (Some("24:00:01"), Some("00:00:02"), Some("23:59:59")),
            (None, None, None),
            // corner case
            (
                Some("00:59:59.999999"),
                Some("01:00:00.000000"),
                Some("-00:00:00.000001"),
            ),
            (
                Some("00:59:59.999999"),
                Some("-00:00:00.000001"),
                Some("01:00:00.000000"),
            ),
            (
                Some("-00:00:00.000001"),
                Some("00:00:00.000001"),
                Some("-00:00:00.000002"),
            ),
            (
                Some("-00:00:00.000001"),
                Some("-00:00:00.000001"),
                Some("00:00:00.000000"),
            ),
            // overflow or underflow case
            (
                Some("-00:00:01"),
                Some("838:59:59.000000"),
                Some("-838:59:59.000000"),
            ),
            (
                Some("838:59:59.000000"),
                Some("-00:00:01"),
                Some("838:59:59.000000"),
            ),
            (
                Some("838:59:59.000000"),
                Some("-838:59:59.000000"),
                Some("838:59:59.000000"),
            ),
        ];
        let mut ctx = EvalContext::default();
        for (duration1, duration2, exp) in cases {
            let expected = exp.map(|exp| Duration::parse(&mut ctx, exp, MAX_FSP).unwrap());
            let duration1 = duration1.map(|arg1| Duration::parse(&mut ctx, arg1, MAX_FSP).unwrap());
            let duration2 = duration2.map(|arg2| Duration::parse(&mut ctx, arg2, MAX_FSP).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(duration1)
                .push_param(duration2)
                .evaluate::<Duration>(ScalarFuncSig::DurationDurationTimeDiff)
                .unwrap();
            assert_eq!(output, expected, "got {}", output.unwrap());
        }
    }

    #[test]
    fn test_string_duration_time_diff() {
        let cases = vec![
            (Some("00:02:02"), Some("00:01:01"), Some("00:01:01")),
            (Some("12:00:00"), Some("00:00:01"), Some("11:59:59")),
            (Some("24:00:00"), Some("00:00:01"), Some("23:59:59")),
            (Some("24:00:01"), Some("00:00:02"), Some("23:59:59")),
            (None, None, None),
            // corner case
            (
                Some("00:59:59.999999"),
                Some("01:00:00.000000"),
                Some("-00:00:00.000001"),
            ),
            (
                Some("00:59:59.999999"),
                Some("-00:00:00.000001"),
                Some("01:00:00.000000"),
            ),
            (
                Some("-00:00:00.000001"),
                Some("00:00:00.000001"),
                Some("-00:00:00.000002"),
            ),
            (
                Some("-00:00:00.000001"),
                Some("-00:00:00.000001"),
                Some("00:00:00.000000"),
            ),
            // overflow or underflow case
            (
                Some("-00:00:01"),
                Some("838:59:59.000000"),
                Some("-838:59:59.000000"),
            ),
            (
                Some("838:59:59.000000"),
                Some("-00:00:01"),
                Some("838:59:59.000000"),
            ),
            (
                Some("838:59:59.000000"),
                Some("-838:59:59.000000"),
                Some("838:59:59.000000"),
            ),
        ];
        let mut ctx = EvalContext::default();
        for (string, duration, exp) in cases {
            let expected = exp.map(|exp| Duration::parse(&mut ctx, exp, MAX_FSP).unwrap());
            let string = string.map(|str| str.as_bytes().to_vec());
            let duration = duration.map(|arg1| Duration::parse(&mut ctx, arg1, MAX_FSP).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(string)
                .push_param(duration)
                .evaluate::<Duration>(ScalarFuncSig::StringDurationTimeDiff)
                .unwrap();
            assert_eq!(output, expected, "got {}", output.unwrap());
        }
    }

    #[test]
    fn test_string_string_time_diff() {
        let cases = vec![
            (Some("00:02:02"), Some("00:01:01"), Some("00:01:01")),
            (Some("12:00:00"), Some("00:00:01"), Some("11:59:59")),
            (Some("24:00:00"), Some("00:00:01"), Some("23:59:59")),
            (Some("24:00:01"), Some("00:00:02"), Some("23:59:59")),
            (None, None, None),
            // corner case
            (
                Some("00:59:59.999999"),
                Some("01:00:00.000000"),
                Some("-00:00:00.000001"),
            ),
            (
                Some("00:59:59.999999"),
                Some("-00:00:00.000001"),
                Some("01:00:00.000000"),
            ),
            (
                Some("-00:00:00.000001"),
                Some("00:00:00.000001"),
                Some("-00:00:00.000002"),
            ),
            (
                Some("-00:00:00.000001"),
                Some("-00:00:00.000001"),
                Some("00:00:00.000000"),
            ),
            // overflow or underflow case
            (
                Some("-00:00:01"),
                Some("838:59:59.000000"),
                Some("-838:59:59.000000"),
            ),
            (
                Some("838:59:59.000000"),
                Some("-00:00:01"),
                Some("838:59:59.000000"),
            ),
            (
                Some("838:59:59.000000"),
                Some("-838:59:59.000000"),
                Some("838:59:59.000000"),
            ),
        ];
        let mut ctx = EvalContext::default();
        for (string1, string2, exp) in cases {
            let expected = exp.map(|exp| Duration::parse(&mut ctx, exp, MAX_FSP).unwrap());
            let string1 = string1.map(|str| str.as_bytes().to_vec());
            let string2 = string2.map(|str| str.as_bytes().to_vec());
            let output = RpnFnScalarEvaluator::new()
                .push_param(string1)
                .push_param(string2)
                .evaluate::<Duration>(ScalarFuncSig::StringStringTimeDiff)
                .unwrap();
            assert_eq!(output, expected, "got {}", output.unwrap());
        }
    }

    #[test]
    fn test_duration_string_time_diff() {
        let cases = vec![
            (Some("00:02:02"), Some("00:01:01"), Some("00:01:01")),
            (Some("12:00:00"), Some("00:00:01"), Some("11:59:59")),
            (Some("24:00:00"), Some("00:00:01"), Some("23:59:59")),
            (Some("24:00:01"), Some("00:00:02"), Some("23:59:59")),
            (None, None, None),
            // corner case
            (
                Some("00:59:59.999999"),
                Some("01:00:00.000000"),
                Some("-00:00:00.000001"),
            ),
            (
                Some("00:59:59.999999"),
                Some("-00:00:00.000001"),
                Some("01:00:00.000000"),
            ),
            (
                Some("-00:00:00.000001"),
                Some("00:00:00.000001"),
                Some("-00:00:00.000002"),
            ),
            (
                Some("-00:00:00.000001"),
                Some("-00:00:00.000001"),
                Some("00:00:00.000000"),
            ),
            // overflow or underflow case
            (
                Some("-00:00:01"),
                Some("838:59:59.000000"),
                Some("-838:59:59.000000"),
            ),
            (
                Some("838:59:59.000000"),
                Some("-00:00:01"),
                Some("838:59:59.000000"),
            ),
            (
                Some("838:59:59.000000"),
                Some("-838:59:59.000000"),
                Some("838:59:59.000000"),
            ),
        ];
        let mut ctx = EvalContext::default();
        for (duration, string, exp) in cases {
            let expected = exp.map(|exp| Duration::parse(&mut ctx, exp, MAX_FSP).unwrap());
            let duration = duration.map(|arg1| Duration::parse(&mut ctx, arg1, MAX_FSP).unwrap());
            let string = string.map(|str| str.as_bytes().to_vec());
            let output = RpnFnScalarEvaluator::new()
                .push_param(duration)
                .push_param(string)
                .evaluate::<Duration>(ScalarFuncSig::DurationStringTimeDiff)
                .unwrap();
            assert_eq!(output, expected, "got {}", output.unwrap());
        }
    }

    #[test]
    fn test_quarter() {
        let cases = vec![
            (Some("2008-04-01"), Some(2)),
            (Some("2008-01-01"), Some(1)),
            (Some("2008-03-31"), Some(1)),
            (Some("2008-06-30"), Some(2)),
            (Some("2008-07-01"), Some(3)),
            (Some("2008-09-30"), Some(3)),
            (Some("2008-10-01"), Some(4)),
            (Some("2008-12-31"), Some(4)),
            (Some("2008-00-01"), Some(0)),
            (None, None),
        ];
        let mut ctx = EvalContext::default();
        for (datetime, exp) in cases {
            let expected = exp.map(|exp| Int::from(exp));
            let datetime = datetime
                .map(|arg1| DateTime::parse_datetime(&mut ctx, arg1, MAX_FSP, true).unwrap());
            let output = RpnFnScalarEvaluator::new()
                .push_param(datetime)
                .evaluate::<Int>(ScalarFuncSig::Quarter)
                .unwrap();
            assert_eq!(output, expected, "got {}", output.unwrap());
        }
    }

    fn get_add_sub_date_expr_types(sig: ScalarFuncSig) -> (FieldTypeTp, FieldTypeTp, FieldTypeTp) {
        use FieldTypeTp::*;
        use ScalarFuncSig::*;
        match sig {
            AddDateStringString | SubDateStringString => (String, String, String),
            AddDateStringInt | SubDateStringInt => (String, Long, String),
            AddDateStringReal | SubDateStringReal => (String, Float, String),
            AddDateStringDecimal | SubDateStringDecimal => (String, NewDecimal, String),
            AddDateIntString | SubDateIntString => (Long, String, String),
            AddDateRealString | SubDateRealString => (Float, String, String),
            AddDateDecimalString | SubDateDecimalString => (NewDecimal, String, String),
            AddDateIntInt | SubDateIntInt => (Long, Long, String),
            AddDateIntReal | SubDateIntReal => (Long, Float, String),
            AddDateIntDecimal | SubDateIntDecimal => (Long, NewDecimal, String),
            AddDateRealInt | SubDateRealInt => (Float, Long, String),
            AddDateRealReal | SubDateRealReal => (Float, Float, String),
            AddDateRealDecimal | SubDateRealDecimal => (Float, NewDecimal, String),
            AddDateDecimalInt | SubDateDecimalInt => (NewDecimal, Long, String),
            AddDateDecimalReal | SubDateDecimalReal => (NewDecimal, Float, String),
            AddDateDecimalDecimal | SubDateDecimalDecimal => (NewDecimal, NewDecimal, String),
            AddDateDatetimeString | SubDateDatetimeString => (DateTime, String, DateTime),
            AddDateDatetimeInt | SubDateDatetimeInt => (DateTime, Long, DateTime),
            AddDateDatetimeReal | SubDateDatetimeReal => (DateTime, Float, DateTime),
            AddDateDatetimeDecimal | SubDateDatetimeDecimal => (DateTime, NewDecimal, DateTime),
            AddDateDurationString | SubDateDurationString => (Duration, String, Duration),
            AddDateDurationInt | SubDateDurationInt => (Duration, Long, Duration),
            AddDateDurationReal | SubDateDurationReal => (Duration, Float, Duration),
            AddDateDurationDecimal | SubDateDurationDecimal => (Duration, NewDecimal, Duration),
            AddDateDurationStringDatetime | SubDateDurationStringDatetime => {
                (Duration, String, DateTime)
            }
            AddDateDurationIntDatetime | SubDateDurationIntDatetime => (Duration, Long, DateTime),
            AddDateDurationRealDatetime | SubDateDurationRealDatetime => {
                (Duration, Float, DateTime)
            }
            AddDateDurationDecimalDatetime | SubDateDurationDecimalDatetime => {
                (Duration, NewDecimal, DateTime)
            }
            _ => panic!("unknown sig {:?}", sig),
        }
    }

    #[test]
    fn test_add_sub_date() {
        let cases = {
            use ScalarFuncSig::*;
            vec![
                (
                    AddDateStringString,
                    Some("2008-04-01"),
                    Some("5"),
                    "day",
                    Some("2008-04-06"),
                ),
                (
                    SubDateStringString,
                    Some("2008-04-01"),
                    Some("5"),
                    "day",
                    Some("2008-03-27"),
                ),
                (
                    AddDateStringInt,
                    Some("2024-09-01 12:10:01"),
                    Some("21"),
                    "Minute",
                    Some("2024-09-01 12:31:01"),
                ),
                (
                    SubDateStringInt,
                    Some("2024-09-01 12:10:01"),
                    Some("21"),
                    "Minute",
                    Some("2024-09-01 11:49:01"),
                ),
                (
                    AddDateStringReal,
                    Some("2024-09-01 12:10:01"),
                    Some("21.1239"),
                    "Minute_microsecond",
                    Some("2024-09-01 12:10:22.123900"),
                ),
                (
                    SubDateStringReal,
                    Some("2024-09-01 12:10:01"),
                    Some("10.22"),
                    "minute_microsecond",
                    Some("2024-09-01 12:09:50.780000"),
                ),
                (
                    AddDateStringDecimal,
                    Some("2024-09-01 12:10:01"),
                    Some("10.200"),
                    "Day_Minute",
                    Some("2024-09-02 01:30:01"),
                ),
                (
                    SubDateStringDecimal,
                    Some("2024-09-01 12:10:01"),
                    Some("-10.22"),
                    "Day_Minute",
                    Some("2024-09-01 22:32:01"),
                ),
                (
                    AddDateIntString,
                    Some("20240901"),
                    Some("2e3"),
                    "second",
                    Some("2024-09-01 00:33:20"),
                ),
                (
                    SubDateIntString,
                    Some("20240901"),
                    Some("2e3"),
                    "second",
                    Some("2024-08-31 23:26:40"),
                ),
                (
                    AddDateRealString,
                    Some("070118"),
                    Some("-1e4"),
                    "SECOND",
                    Some("2007-01-17 21:13:20"),
                ),
                (
                    SubDateRealString,
                    Some("121231113045.123"),
                    Some("2-20"),
                    "year_month",
                    Some("2009-04-30 11:30:45.123001"),
                ),
                (
                    AddDateDecimalString,
                    Some("1203"),
                    Some("-22"),
                    "quarter",
                    Some("1995-06-03"),
                ),
                (
                    SubDateDecimalString,
                    Some("170105084059.575601"),
                    Some("123.221456"),
                    "second_microsecond",
                    Some("2017-01-05 08:38:56.354145"),
                ),
                (
                    AddDateIntInt,
                    Some("691231235959"),
                    Some("123"),
                    "Minute_Second",
                    Some("2070-01-01 00:02:02"),
                ),
                (
                    SubDateIntInt,
                    Some("691231235959"),
                    Some("321"),
                    "Hour_Second",
                    Some("2069-12-31 23:54:38"),
                ),
                (
                    AddDateIntReal,
                    Some("591231"),
                    Some("-1.678"),
                    "Week",
                    Some("2059-12-17"),
                ),
                (
                    SubDateIntReal,
                    Some("591231"),
                    Some("6.678"),
                    "MONTH",
                    Some("2059-05-31"),
                ),
                (
                    AddDateIntDecimal,
                    Some("19990101000000"),
                    Some("238.12390"),
                    "Day_Microsecond",
                    Some("1999-01-01 00:03:58.123900"),
                ),
                (
                    SubDateIntDecimal,
                    Some("991231235959"),
                    Some("238.12390"),
                    "Day_Microsecond",
                    Some("1999-12-31 23:56:00.876100"),
                ),
                (
                    AddDateRealInt,
                    Some("121231113045.9999999"),
                    Some("1234"),
                    "MICROSECOND",
                    Some("2012-12-31 11:30:46.001234"),
                ),
                (
                    SubDateRealInt,
                    Some("121231113045.9999999"),
                    Some("8912"),
                    "day",
                    Some("1988-08-07 11:30:46"),
                ),
                (
                    AddDateRealReal,
                    Some("170105084059.575601"),
                    Some("-98.123"),
                    "second",
                    Some("2017-01-05 08:39:21.452592"),
                ),
                (
                    SubDateRealReal,
                    Some("170105084059.575601"),
                    Some("-98.123"),
                    "HOUR",
                    Some("2017-01-09 10:40:59.575592"),
                ),
                (
                    AddDateRealDecimal,
                    Some("1210"),
                    Some("9876.1234"),
                    "Minute_Microsecond",
                    Some("2000-12-10 02:44:36.123400"),
                ),
                (
                    SubDateRealDecimal,
                    Some("1210"),
                    Some("9876.1234"),
                    "Minute_Microsecond",
                    Some("2000-12-09 21:15:23.876600"),
                ),
                (
                    AddDateDecimalInt,
                    Some("121231113045.999999"),
                    Some("1234"),
                    "MICROSECOND",
                    Some("2012-12-31 11:30:46.001233"),
                ),
                (
                    SubDateDecimalInt,
                    Some("240924"),
                    Some("1234"),
                    "WEEK",
                    Some("2001-01-30"),
                ),
                (
                    AddDateDecimalReal,
                    Some("121231113045.999999"),
                    Some("1234.892"),
                    "MICROSECOND",
                    Some("2012-12-31 11:30:46.001234"),
                ),
                (
                    SubDateDecimalReal,
                    Some("240924"),
                    Some("1234.99"),
                    "Hour_Microsecond",
                    Some("2024-09-23 23:39:25.010000"),
                ),
                (
                    AddDateDecimalDecimal,
                    Some("121231113045.999999"),
                    Some("1234.892"),
                    "MICROSECOND",
                    Some("2012-12-31 11:30:46.001234"),
                ),
                (
                    SubDateDecimalDecimal,
                    Some("240924"),
                    Some("1234.99"),
                    "minute_microsecond",
                    Some("2024-09-23 23:39:25.010000"),
                ),
                (
                    AddDateDatetimeString,
                    Some("2024-01-01"),
                    Some("8"),
                    "DaY",
                    Some("2024-01-09"),
                ),
                (
                    SubDateDatetimeString,
                    Some("2024-01-01"),
                    Some("8 12:60:128.9123"),
                    "day_mIcroseconD",
                    Some("2023-12-23 10:57:51.087700"),
                ),
                (
                    SubDateDatetimeString,
                    Some("2024-01-01 12:22:12.321"),
                    Some("-7 55:03:09.629"),
                    "day_mIcroseconD",
                    Some("2024-01-10 19:25:21.950000"),
                ),
                (
                    AddDateDatetimeInt,
                    Some("2001-02-03"),
                    Some("782"),
                    "minUte",
                    Some("2001-02-03 13:02:00.000000"),
                ),
                (
                    SubDateDatetimeInt,
                    Some("2001-02-03"),
                    Some("782"),
                    "minUte",
                    Some("2001-02-02 10:58:00.000000"),
                ),
                (
                    AddDateDatetimeReal,
                    Some("2002-02-28 23:59:22.222"),
                    Some("1.678"),
                    "Minute_Second",
                    Some("2002-03-01 00:11:40.222000"),
                ),
                (
                    SubDateDatetimeReal,
                    Some("2002-02-28 23:59:22.222"),
                    Some("1238123.123489"),
                    "Minute_Second",
                    Some("1999-10-21 18:18:13.222000"),
                ),
                (
                    AddDateDatetimeDecimal,
                    Some("2024-12-30"),
                    Some("-98264.678"),
                    "Second",
                    Some("2024-12-28 20:42:15.322000"),
                ),
                (
                    SubDateDatetimeDecimal,
                    Some("2024-12-31"),
                    Some("778.12348"),
                    "Day_Hour",
                    Some("2021-06-17 12:00:00.000000"),
                ),
                (
                    AddDateDurationString,
                    Some("12:26:12.212"),
                    Some("29 12:23:36.1234"),
                    "day_microsecond",
                    Some("720:49:48.335400"),
                ),
                (
                    SubDateDurationString,
                    Some("12:26:12.212"),
                    Some("29 12:23:36.1234"),
                    "day_microsecond",
                    Some("-695:57:23.911400"),
                ),
                (
                    AddDateDurationInt,
                    Some("1 10:11:12.1234565"),
                    Some("123"),
                    "minute",
                    Some("36:14:12.123457"),
                ),
                (
                    SubDateDurationInt,
                    Some("1 10:11:12.1234565"),
                    Some("123"),
                    "minute",
                    Some("32:08:12.123457"),
                ),
                (
                    AddDateDurationReal,
                    Some("1112"),
                    Some("-234.889"),
                    "MINUTE_SECOND",
                    Some("-03:57:37.000000"),
                ),
                (
                    SubDateDurationReal,
                    Some("1112"),
                    Some("-234.889"),
                    "MINUTE_SECOND",
                    Some("04:20:01.000000"),
                ),
                (
                    AddDateDurationDecimal,
                    Some("1 12"),
                    Some("1.2345"),
                    "MINUTE_MICROSECOND",
                    Some("36:00:01.234500"),
                ),
                (
                    SubDateDurationDecimal,
                    Some("-1 12"),
                    Some("1.2345"),
                    "second_MICROSECOND",
                    Some("-36:00:01.234500"),
                ),
                (
                    AddDateDurationStringDatetime,
                    Some("12:26:12.212"),
                    Some("29 12:23:36"),
                    "DAY_SECOND",
                    Some("2020-03-03 00:49:48.212000"),
                ),
                (
                    SubDateDurationStringDatetime,
                    Some("12:26:12.212"),
                    Some("29 12:23:36"),
                    "DAY_SECOND",
                    Some("2020-01-04 00:02:36.212000"),
                ),
                (
                    AddDateDurationIntDatetime,
                    Some("1 10:11:12.1234565"),
                    Some("123"),
                    "QUARTER",
                    Some("2050-11-03 10:11:12.123457"),
                ),
                (
                    SubDateDurationIntDatetime,
                    Some("1 10:11:12.1234565"),
                    Some("123"),
                    "QUARTER",
                    Some("1989-05-03 10:11:12.123457"),
                ),
                (
                    AddDateDurationRealDatetime,
                    Some("1112"),
                    Some("-41.12"),
                    "DAY_HOUR",
                    Some("2019-12-22 12:11:12.000000"),
                ),
                (
                    SubDateDurationRealDatetime,
                    Some("1112"),
                    Some("-41.12"),
                    "DAY_HOUR",
                    Some("2020-03-14 12:11:12.000000"),
                ),
                (
                    AddDateDurationDecimalDatetime,
                    Some("-35:30:46"),
                    Some("12.99"),
                    "Year",
                    Some("2033-01-31 12:29:14.000000"),
                ),
                (
                    SubDateDurationDecimalDatetime,
                    Some("-35:30:46"),
                    Some("12.99"),
                    "year_month",
                    Some("1999-10-31 12:29:14.000000"),
                ),
                (
                    SubDateDurationDecimalDatetime,
                    Some("-35:30:46"),
                    Some("12.99000"),
                    "year_month",
                    None,
                ),
                (AddDateDecimalInt, None, Some("1234"), "MICROSECOND", None),
                (SubDateIntString, Some("20240901"), None, "second", None),
                (AddDateIntInt, Some("0"), Some("0"), "day_hour", None),
                (SubDateIntDecimal, Some("0"), Some("0.0"), "minute", None),
            ]
        };
        let builder_push_param = |ctx: &mut EvalContext,
                                  mut builder: ExprDefBuilder,
                                  param: Option<&str>,
                                  field_type: FieldTypeTp|
         -> ExprDefBuilder {
            if param.is_none() {
                builder = builder.push_child(ExprDefBuilder::constant_null(field_type));
                return builder;
            }
            let param = param.unwrap();
            match field_type {
                FieldTypeTp::String => {
                    builder = builder
                        .push_child(ExprDefBuilder::constant_bytes(param.as_bytes().to_vec()))
                }
                FieldTypeTp::Long => {
                    let p = i64::from_str(param).unwrap();
                    builder = builder.push_child(ExprDefBuilder::constant_int(p));
                }
                FieldTypeTp::Float => {
                    let p = f64::from_str(param).unwrap();
                    builder = builder.push_child(ExprDefBuilder::constant_real(p));
                }
                FieldTypeTp::NewDecimal => {
                    let p = Decimal::from_str(param).unwrap();
                    builder = builder.push_child(ExprDefBuilder::constant_decimal(p));
                }
                FieldTypeTp::DateTime => {
                    let p = Time::parse_without_type(ctx, param, MAX_FSP, true).unwrap();
                    builder = builder.push_child(ExprDefBuilder::constant_time(
                        p.to_packed_u64(ctx).unwrap(),
                        p.get_time_type(),
                    ));
                }
                FieldTypeTp::Duration => {
                    let p = Duration::parse(ctx, param, MAX_FSP).unwrap();
                    builder = builder.push_child(ExprDefBuilder::constant_duration(p));
                }
                _ => panic!("unknown field type {:?}", field_type),
            }
            builder
        };

        for (func_sig, time, interval, unit, expected) in cases {
            let mut cfg = EvalConfig::default();
            cfg.is_test = true;
            let mut ctx = EvalContext::new(Arc::new(cfg));
            let (time_type, interval_type, result_type) = get_add_sub_date_expr_types(func_sig);
            let mut result_field_type: FieldType = result_type.into();
            result_field_type.set_decimal(MAX_FSP as i32);
            let mut builder = ExprDefBuilder::scalar_func(func_sig, result_field_type);

            builder = builder_push_param(&mut ctx, builder, time, time_type);
            builder = builder_push_param(&mut ctx, builder, interval, interval_type);
            builder = builder.push_child(ExprDefBuilder::constant_bytes(unit.as_bytes().to_vec()));
            let node = builder.build();
            let exp = RpnExpressionBuilder::build_from_expr_tree(node, &mut ctx, 1).unwrap();

            let schema = &[];
            let mut columns = LazyBatchColumnVec::empty();
            let val = exp.eval(&mut ctx, schema, &mut columns, &[0], 1).unwrap();

            assert!(val.is_vector());
            let value = val.vector_value().unwrap().as_ref();
            assert_eq!(value.len(), 1);
            match result_type {
                FieldTypeTp::String => {
                    let v = value.to_bytes_vec();
                    assert_eq!(
                        v[0].as_deref().and_then(|s| std::str::from_utf8(s).ok()),
                        expected,
                        "{:?} {:?} {:?} {:?}",
                        func_sig,
                        time,
                        interval,
                        unit
                    );
                }
                FieldTypeTp::DateTime => {
                    let v = value.to_date_time_vec();
                    assert_eq!(
                        v[0].map(|s| s.to_string()).as_deref(),
                        expected,
                        "{:?} {:?} {:?} {:?}",
                        func_sig,
                        time,
                        interval,
                        unit
                    );
                }
                FieldTypeTp::Duration => {
                    let v = value.to_duration_vec();
                    assert_eq!(
                        v[0].map(|s| s.to_string()).as_deref(),
                        expected,
                        "{:?} {:?} {:?} {:?}",
                        func_sig,
                        time,
                        interval,
                        unit
                    );
                }
                _ => panic!("unknown field type {:?}", result_type),
            }
        }
    }

    #[test]
    fn test_from_unixtime_1_arg() {
        let cases = vec![
            (1451606400.0, 0, Some("2016-01-01 00:00:00")),
            (1451606400.123456, 6, Some("2016-01-01 00:00:00.123456")),
            (1451606400.999999, 6, Some("2016-01-01 00:00:00.999999")),
            (1451606400.9999999, 6, Some("2016-01-01 00:00:01.000000")),
            (1451606400.9999995, 6, Some("2016-01-01 00:00:01.000000")),
            (1451606400.9999994, 6, Some("2016-01-01 00:00:00.999999")),
            (1451606400.123, 3, Some("2016-01-01 00:00:00.123")),
            (5000000000.0, 0, Some("2128-06-11 08:53:20")),
            (32536771199.99999, 6, Some("3001-01-18 23:59:59.999990")),
            (0.0, 6, Some("1970-01-01 00:00:00.000000")),
            (-1.0, 6, None),
            (32536771200.0, 6, None),
        ];
        let mut ctx = EvalContext::default();
        for (datetime, fsp, expected) in cases {
            let decimal = Decimal::from_f64(datetime).unwrap();
            let mut result_field_type: FieldType = FieldTypeTp::DateTime.into();
            result_field_type.set_decimal(fsp as i32);

            let (result, _) = RpnFnScalarEvaluator::new()
                .push_param(decimal)
                .evaluate_raw(result_field_type, ScalarFuncSig::FromUnixTime1Arg);
            let output: Option<DateTime> = result.unwrap().into();

            let expected =
                expected.map(|arg1| DateTime::parse_datetime(&mut ctx, arg1, fsp, false).unwrap());
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_from_unixtime_2_arg() {
        let cases = vec![
            (
                1451606400.0,
                "%Y %D %M %h:%i:%s %x",
                0,
                Some("2016 1st January 12:00:00 2015"),
            ),
            (
                1451606400.123456,
                "%Y %D %M %h:%i:%s %x",
                6,
                Some("2016 1st January 12:00:00 2015"),
            ),
            (
                1451606400.999999,
                "%Y %D %M %h:%i:%s %x",
                6,
                Some("2016 1st January 12:00:00 2015"),
            ),
            (
                1451606400.9999999,
                "%Y %D %M %h:%i:%s %x",
                6,
                Some("2016 1st January 12:00:01 2015"),
            ),
        ];
        for (datetime, format, fsp, expected) in cases {
            let decimal = Decimal::from_f64(datetime).unwrap();
            let mut result_field_type: FieldType = FieldTypeTp::String.into();
            result_field_type.set_decimal(fsp);
            let (result, _) = RpnFnScalarEvaluator::new()
                .push_param(decimal)
                .push_param(format)
                .evaluate_raw(result_field_type, ScalarFuncSig::FromUnixTime2Arg);
            let output: Option<Bytes> = result.unwrap().into();

            let expected = expected.map(|str| str.as_bytes().to_vec());
            assert_eq!(output, expected);
        }
    }

    #[test]
    fn test_unixtime_int() {
        let cases = vec![
            ("2016-01-01 00:00:00", 0, "", 1451606400),
            ("2015-11-13 10:20:19", 0, "", 1447410019),
            ("2015-11-13 10:20:19", 3, "", 1447410016),
            ("2015-11-13 10:20:19", -3, "", 1447410022),
            ("1970-01-01 00:00:00", 0, "", 0),
            ("1969-12-31 23:59:59", 0, "", 0),
            ("3001-01-19 00:00:00", 0, "", 0),
            ("4001-01-19 00:00:00", 0, "", 0),
            ("2015-11-13 10:20:19", 0, "US/Eastern", 1447428019),
            ("2009-09-20 07:32:39", 0, "US/Eastern", 1253446359),
            ("2020-03-29 03:45:00", 0, "Europe/Vilnius", 1585443600),
            ("2020-10-25 03:45:00", 0, "Europe/Vilnius", 1603586700),
            ("0-10-25 03:45:00", 0, "", 0),
            ("2000-0-25 03:45:00", 0, "", 0),
            ("2000-1-0 03:45:00", 0, "", 0),
        ];

        for (datetime, offset, time_zone_name, expected) in cases {
            let mut cfg = EvalConfig::new();
            if time_zone_name.is_empty() {
                cfg.set_time_zone_by_offset(offset).unwrap();
            } else {
                cfg.set_time_zone_by_name(time_zone_name).unwrap();
            }

            let mut ctx = EvalContext::new(Arc::<EvalConfig>::new(cfg));
            let result_field_type: FieldType = FieldTypeTp::LongLong.into();
            let arg = DateTime::parse_datetime(&mut ctx, datetime, 0, false).unwrap();
            let (result, _) = RpnFnScalarEvaluator::new()
                .context(ctx)
                .push_param(arg)
                .evaluate_raw(result_field_type, ScalarFuncSig::UnixTimestampInt);
            let output: Option<i64> = result.unwrap().into();

            assert_eq!(output.unwrap(), expected);
        }
    }

    #[test]
    fn test_unixtime_decimal() {
        let cases = vec![
            (
                "2016-01-01 00:00:00.123",
                0,
                "",
                3,
                Decimal::from_str("1451606400.123").unwrap(),
            ),
            (
                "2015-11-13 10:20:19.342",
                0,
                "",
                3,
                Decimal::from_str("1447410019.342").unwrap(),
            ),
            (
                "2015-11-13 10:20:19.522",
                3,
                "",
                3,
                Decimal::from_str("1447410016.522").unwrap(),
            ),
            (
                "2015-11-13 10:20:19.223",
                -3,
                "",
                3,
                Decimal::from_str("1447410022.223").unwrap(),
            ),
            (
                "2015-11-13 10:20:19.2",
                -3,
                "",
                1,
                Decimal::from_str("1447410022.2").unwrap(),
            ),
            (
                "1970-01-01 00:00:00.234",
                0,
                "",
                3,
                Decimal::from_str("0").unwrap(),
            ),
            (
                "1969-12-31 23:59:59.432",
                0,
                "",
                3,
                Decimal::from_str("0").unwrap(),
            ),
            (
                "3001-01-19 00:00:00.432",
                0,
                "",
                3,
                Decimal::from_str("0").unwrap(),
            ),
            (
                "4001-01-19 00:00:00.533",
                0,
                "",
                3,
                Decimal::from_str("0").unwrap(),
            ),
            (
                "2015-11-13 10:20:19",
                0,
                "US/Eastern",
                0,
                Decimal::from_str("1447428019").unwrap(),
            ),
            (
                "2009-09-20 07:32:39",
                0,
                "US/Eastern",
                0,
                Decimal::from_str("1253446359").unwrap(),
            ),
            (
                "2020-03-29 03:45:03.123",
                0,
                "Europe/Vilnius",
                0,
                Decimal::from_str("1585443600").unwrap(),
            ),
        ];

        for (datetime, offset, time_zone_name, fsp, expected) in cases {
            let mut cfg = EvalConfig::new();
            if time_zone_name.is_empty() {
                cfg.set_time_zone_by_offset(offset).unwrap();
            } else {
                cfg.set_time_zone_by_name(time_zone_name).unwrap();
            }
            let mut ctx = EvalContext::new(Arc::<EvalConfig>::new(cfg));
            let mut result_field_type: FieldType = FieldTypeTp::NewDecimal.into();
            result_field_type.set_decimal(fsp);

            let arg = DateTime::parse_datetime(&mut ctx, datetime, 3, false).unwrap();
            let (result, _) = RpnFnScalarEvaluator::new()
                .context(ctx)
                .push_param(arg)
                .evaluate_raw(result_field_type, ScalarFuncSig::UnixTimestampDec);
            let output: Option<Decimal> = result.unwrap().into();

            assert_eq!(output.unwrap(), expected);
        }
    }

    #[test]
    fn test_timestamp_diff() {
        let test_cases = vec![
            (
                Some("2025-02-05 12:00:00"),
                Some("2025-02-05 12:00:00"),
                "MicroseCond",
                Some(0),
            ),
            (
                Some("2025-02-05 12:00:00"),
                Some("2025-02-05 12:00:00.000001"),
                "microsecond",
                Some(1),
            ),
            (
                Some("2025-02-05 12:00:00"),
                Some("2025-02-05 12:00:10"),
                "Second",
                Some(10),
            ),
            (
                Some("2025-02-05 12:00:00"),
                Some("2025-02-05 12:10:00"),
                "Minute",
                Some(10),
            ),
            (
                Some("2025-02-05 12:00:00"),
                Some("2025-02-05 14:00:00"),
                "Hour",
                Some(2),
            ),
            (
                Some("2025-02-05 12:00:00"),
                Some("2025-02-07 12:00:00"),
                "Day",
                Some(2),
            ),
            (
                Some("2025-02-05 12:00:00"),
                Some("2025-03-05 12:00:00"),
                "Month",
                Some(1),
            ),
            (
                Some("2025-02-05 12:00:00"),
                Some("2025-08-05 12:00:00"),
                "Quarter",
                Some(2),
            ),
            (
                Some("2025-02-05 12:00:00"),
                Some("2027-02-05 12:00:00"),
                "Year",
                Some(2),
            ),
            (
                Some("2024-02-29 12:00:00"),
                Some("2025-02-28 12:00:00"),
                "Year",
                Some(0),
            ),
            (
                Some("2024-12-31 23:59:59"),
                Some("2025-01-01 00:00:00"),
                "Second",
                Some(1),
            ),
            (
                Some("2024-03-31 23:59:59"),
                Some("2024-04-01 00:00:00"),
                "Second",
                Some(1),
            ),
            (
                Some("2024-02-28 12:00:00"),
                Some("2024-02-29 12:00:00"),
                "Day",
                Some(1),
            ),
            (
                Some("2024-02-28 12:00:00"),
                Some("2024-03-01 12:00:00"),
                "DAY",
                Some(2),
            ),
            (
                Some("2025-02-05 12:00:00"),
                Some("2028-02-05 12:00:00"),
                "Year",
                Some(3),
            ),
            (
                Some("2025-02-05 12:00:00"),
                Some("2025-12-05 12:00:00"),
                "Month",
                Some(10),
            ),
            (
                Some("2025-02-05 12:00:00"),
                Some("2025-02-19 12:00:00"),
                "Week",
                Some(2),
            ),
            (
                Some("2025-02-05 12:00:00"),
                Some("2025-02-05 16:00:00"),
                "hour",
                Some(4),
            ),
            (
                Some("2025-02-05 12:00:00"),
                Some("2025-02-06 00:00:00"),
                "HOUR",
                Some(12),
            ),
            (None, Some("2025-02-06 00:00:00"), "DAY", None),
            (Some("2025-02-06 00:00:00"), None, "YEAR", None),
            (None, None, "MONTH", None),
        ];
        let mut ctx = EvalContext::default();
        for (t1, t2, unit, expected) in test_cases {
            let mut builder =
                ExprDefBuilder::scalar_func(ScalarFuncSig::TimestampDiff, FieldTypeTp::LongLong);
            builder = builder.push_child(ExprDefBuilder::constant_bytes(unit.as_bytes().to_vec()));
            if let Some(t) = t1 {
                let time = DateTime::parse_timestamp(&mut ctx, t, MAX_FSP, true).unwrap();
                builder = builder.push_child(ExprDefBuilder::constant_time(
                    time.to_packed_u64(&mut ctx).unwrap(),
                    TimeType::Timestamp,
                ));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::DateTime));
            }
            if let Some(t) = t2 {
                let time = DateTime::parse_timestamp(&mut ctx, t, MAX_FSP, true).unwrap();
                builder = builder.push_child(ExprDefBuilder::constant_time(
                    time.to_packed_u64(&mut ctx).unwrap(),
                    TimeType::Timestamp,
                ));
            } else {
                builder = builder.push_child(ExprDefBuilder::constant_null(FieldTypeTp::DateTime));
            }
            let node = builder.build();
            let exp = RpnExpressionBuilder::build_from_expr_tree(node, &mut ctx, 1).unwrap();

            let schema = &[];
            let mut columns = LazyBatchColumnVec::empty();
            let val = exp.eval(&mut ctx, schema, &mut columns, &[0], 1).unwrap();

            assert!(val.is_vector());
            let value = val.vector_value().unwrap().as_ref().to_int_vec();
            assert_eq!(value.len(), 1);
            assert_eq!(
                value[0], expected,
                "expected {:?}, got {:?}",
                expected, value[0]
            );
        }
    }

    #[test]
    fn test_str_to_date() {
        use tidb_query_datatype::codec::mysql::TimeType;
        let mut ctx = EvalContext::default();
        let cases = vec![
            (
                Some("10/28/2011 9:46:29 pm"),
                Some("%m/%d/%Y %l:%i:%s %p"),
                Time::from_slice(
                    &mut ctx,
                    &[2011, 10, 28, 21, 46, 29, 0],
                    TimeType::DateTime,
                    0,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("10/28/2011 9:46:29 Pm"),
                Some("%m/%d/%Y %l:%i:%s %p"),
                Time::from_slice(
                    &mut ctx,
                    &[2011, 10, 28, 21, 46, 29, 0],
                    TimeType::DateTime,
                    0,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("2011/10/28 9:46:29 am"),
                Some("%Y/%m/%d %l:%i:%s %p"),
                Time::from_slice(
                    &mut ctx,
                    &[2011, 10, 28, 9, 46, 29, 0],
                    TimeType::DateTime,
                    0,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("20161122165022"),
                Some("%Y%m%d%H%i%s"),
                Time::from_slice(
                    &mut ctx,
                    &[2016, 11, 22, 16, 50, 22, 0],
                    TimeType::DateTime,
                    0,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("2016 11 22 16 50 22"),
                Some("%Y%m%d%H%i%s"),
                Time::from_slice(
                    &mut ctx,
                    &[2016, 11, 22, 16, 50, 22, 0],
                    TimeType::DateTime,
                    0,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("16-50-22 2016 11 22"),
                Some("%H-%i-%s%Y%m%d"),
                Time::from_slice(
                    &mut ctx,
                    &[2016, 11, 22, 16, 50, 22, 0],
                    TimeType::DateTime,
                    0,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("16-50 2016 11 22"),
                Some("%H-%i-%s%Y%m%d"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("15-01-2001 1:59:58.999"),
                Some("%d-%m-%Y %I:%i:%s.%f"),
                Time::from_slice(
                    &mut ctx,
                    &[2001, 1, 15, 1, 59, 58, 999000],
                    TimeType::DateTime,
                    6,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("15-01-2001 1:59:58.1"),
                Some("%d-%m-%Y %H:%i:%s.%f"),
                Time::from_slice(
                    &mut ctx,
                    &[2001, 1, 15, 1, 59, 58, 100000],
                    TimeType::DateTime,
                    6,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("15-01-2001 1:59:58."),
                Some("%d-%m-%Y %H:%i:%s.%f"),
                Time::from_slice(
                    &mut ctx,
                    &[2001, 1, 15, 1, 59, 58, 0],
                    TimeType::DateTime,
                    6,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("15-01-2001 1:9:8.999"),
                Some("%d-%m-%Y %H:%i:%s.%f"),
                Time::from_slice(
                    &mut ctx,
                    &[2001, 1, 15, 1, 9, 8, 999000],
                    TimeType::DateTime,
                    6,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("15-01-2001 1:9:8.999"),
                Some("%d-%m-%Y %H:%i:%S.%f"),
                Time::from_slice(
                    &mut ctx,
                    &[2001, 1, 15, 1, 9, 8, 999000],
                    TimeType::DateTime,
                    6,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("2003-01-02 10:11:12.0012"),
                Some("%Y-%m-%d %H:%i:%S.%f"),
                Time::from_slice(
                    &mut ctx,
                    &[2003, 1, 2, 10, 11, 12, 1200],
                    TimeType::DateTime,
                    6,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("2003-01-02 10:11:12 PM"),
                Some("%Y-%m-%d %H:%i:%S %p"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("10:20:10AM"),
                Some("%H:%i:%S%p"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            // test %@(skip alpha), %#(skip number), %.(skip punct)
            (
                Some("2020-10-10ABCD"),
                Some("%Y-%m-%d%@"),
                Time::from_slice(&mut ctx, &[2020, 10, 10, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("2020-10-101234"),
                Some("%Y-%m-%d%#"),
                Time::from_slice(&mut ctx, &[2020, 10, 10, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("2020-10-10...."),
                Some("%Y-%m-%d%."),
                Time::from_slice(&mut ctx, &[2020, 10, 10, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("2020-10-10.1"),
                Some("%Y-%m-%d%.%#%@"),
                Time::from_slice(&mut ctx, &[2020, 10, 10, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("abcd2020-10-10.1"),
                Some("%@%Y-%m-%d%.%#%@"),
                Time::from_slice(&mut ctx, &[2020, 10, 10, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("abcd-2020-10-10.1"),
                Some("%@-%Y-%m-%d%.%#%@"),
                Time::from_slice(&mut ctx, &[2020, 10, 10, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("2020-10-10"),
                Some("%Y-%m-%d%@"),
                Time::from_slice(&mut ctx, &[2020, 10, 10, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("2020-10-10abcde123abcdef"),
                Some("%Y-%m-%d%@%#"),
                Time::from_slice(&mut ctx, &[2020, 10, 10, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            // some input for '%r'
            (
                Some("12:3:56pm  13/05/2019"),
                Some("%r %d/%c/%Y"),
                Time::from_slice(
                    &mut ctx,
                    &[2019, 5, 13, 12, 3, 56, 0],
                    TimeType::DateTime,
                    6,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("11:13:56 am"),
                Some("%r"),
                None,
                Some(Duration::new_from_parts(false, 11, 13, 56, 0, 6).unwrap()),
                ScalarFuncSig::StrToDateDuration,
            ),
            // some input for '%T'
            (
                Some("12:13:56 13/05/2019"),
                Some("%T %d/%c/%Y"),
                Time::from_slice(
                    &mut ctx,
                    &[2019, 5, 13, 12, 13, 56, 0],
                    TimeType::DateTime,
                    6,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("19:3:56  13/05/2019"),
                Some("%T %d/%c/%Y"),
                Time::from_slice(
                    &mut ctx,
                    &[2019, 5, 13, 19, 3, 56, 0],
                    TimeType::DateTime,
                    6,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("21:13:24"),
                Some("%T"),
                None,
                Some(Duration::new_from_parts(false, 21, 13, 24, 0, 6).unwrap()),
                ScalarFuncSig::StrToDateDuration,
            ),
            // More test cases
            (
                Some("01,05,2013"),
                Some("%d,%m,%Y"),
                Time::from_slice(&mut ctx, &[2013, 5, 1, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("5 12 2021"),
                Some("%m%d%Y"),
                Time::from_slice(&mut ctx, &[2021, 5, 12, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("May 01, 2013"),
                Some("%M %d,%Y"),
                Time::from_slice(&mut ctx, &[2013, 5, 1, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("a09:30:17"),
                Some("a%h:%i:%s"),
                None,
                Some(Duration::new_from_parts(false, 9, 30, 17, 0, 6).unwrap()),
                ScalarFuncSig::StrToDateDuration,
            ),
            (
                Some("09:30:17a"),
                Some("%h:%i:%s"),
                None,
                Some(Duration::new_from_parts(false, 9, 30, 17, 0, 6).unwrap()),
                ScalarFuncSig::StrToDateDuration,
            ),
            (
                Some("12:43:24"),
                Some("%h:%i:%s"),
                None,
                Some(Duration::new_from_parts(false, 0, 43, 24, 0, 6).unwrap()),
                ScalarFuncSig::StrToDateDuration,
            ),
            (
                Some("abc"),
                Some("abc"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 0, 0, 0, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("09"),
                Some("%m"),
                Time::from_slice(&mut ctx, &[0, 9, 0, 0, 0, 0, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("09"),
                Some("%s"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 0, 0, 9, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("12:43:24 AM"),
                Some("%r"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 0, 43, 24, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("12:43:24 PM"),
                Some("%r"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 12, 43, 24, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("11:43:24 PM"),
                Some("%r"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 23, 43, 24, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("00:12:13"),
                Some("%T"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 0, 12, 13, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("23:59:59"),
                Some("%T"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 23, 59, 59, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("00/00/0000"),
                Some("%m/%d/%Y"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 0, 0, 0, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("04/30/2004"),
                Some("%m/%d/%Y"),
                Time::from_slice(&mut ctx, &[2004, 4, 30, 0, 0, 0, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("15:35:00"),
                Some("%H:%i:%s"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 15, 35, 0, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("Jul 17 33"),
                Some("%b %k %S"),
                Time::from_slice(&mut ctx, &[0, 7, 0, 17, 0, 33, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("2016-January:7 432101"),
                Some("%Y-%M:%l %f"),
                Time::from_slice(
                    &mut ctx,
                    &[2016, 1, 0, 7, 0, 0, 432101],
                    TimeType::DateTime,
                    6,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("10:13 PM"),
                Some("%l:%i %p"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 22, 13, 0, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("12:00:00 AM"),
                Some("%h:%i:%s %p"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 0, 0, 0, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("12:00:00 PM"),
                Some("%h:%i:%s %p"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 12, 0, 0, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("12:00:00 PM"),
                Some("%I:%i:%s %p"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 12, 0, 0, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("1:00:00 PM"),
                Some("%h:%i:%s %p"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 13, 0, 0, 0], TimeType::DateTime, 6),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("18/10/22"),
                Some("%y/%m/%d"),
                Time::from_slice(&mut ctx, &[2018, 10, 22, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("8/10/22"),
                Some("%y/%m/%d"),
                Time::from_slice(&mut ctx, &[2008, 10, 22, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("69/10/22"),
                Some("%y/%m/%d"),
                Time::from_slice(&mut ctx, &[2069, 10, 22, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("70/10/22"),
                Some("%y/%m/%d"),
                Time::from_slice(&mut ctx, &[1970, 10, 22, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("18/10/22"),
                Some("%Y/%m/%d"),
                Time::from_slice(&mut ctx, &[2018, 10, 22, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("2018/10/22"),
                Some("%Y/%m/%d"),
                Time::from_slice(&mut ctx, &[2018, 10, 22, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("8/10/22"),
                Some("%Y/%m/%d"),
                Time::from_slice(&mut ctx, &[2008, 10, 22, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("69/10/22"),
                Some("%Y/%m/%d"),
                Time::from_slice(&mut ctx, &[2069, 10, 22, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("70/10/22"),
                Some("%Y/%m/%d"),
                Time::from_slice(&mut ctx, &[1970, 10, 22, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("18/10/22"),
                Some("%Y/%m/%d"),
                Time::from_slice(&mut ctx, &[2018, 10, 22, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("100/10/22"),
                Some("%Y/%m/%d"),
                Time::from_slice(&mut ctx, &[100, 10, 22, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("09/10/1021"),
                Some("%d/%m/%y"),
                Time::from_slice(&mut ctx, &[2010, 10, 9, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("09/10/1021"),
                Some("%d/%m/%Y"),
                Time::from_slice(&mut ctx, &[1021, 10, 9, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            (
                Some("09/10/10"),
                Some("%d/%m/%Y"),
                Time::from_slice(&mut ctx, &[2010, 10, 9, 0, 0, 0, 0], TimeType::Date, 6),
                None,
                ScalarFuncSig::StrToDateDate,
            ),
            //'%b'/'%M' should be case insensitive
            (
                Some("31/may/2016 12:34:56.1234"),
                Some("%d/%b/%Y %H:%i:%S.%f"),
                Time::from_slice(
                    &mut ctx,
                    &[2016, 5, 31, 12, 34, 56, 123400],
                    TimeType::DateTime,
                    6,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("30/april/2016 12:34:56."),
                Some("%d/%M/%Y %H:%i:%s.%f"),
                Time::from_slice(
                    &mut ctx,
                    &[2016, 4, 30, 12, 34, 56, 0],
                    TimeType::DateTime,
                    6,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("31/mAy/2016 12:34:56.1234"),
                Some("%d/%b/%Y %H:%i:%S.%f"),
                Time::from_slice(
                    &mut ctx,
                    &[2016, 5, 31, 12, 34, 56, 123400],
                    TimeType::DateTime,
                    6,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("30/apRil/2016 12:34:56."),
                Some("%d/%M/%Y %H:%i:%s.%f"),
                Time::from_slice(
                    &mut ctx,
                    &[2016, 4, 30, 12, 34, 56, 0],
                    TimeType::DateTime,
                    6,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            // '%r'
            (
                Some(" 04 :13:56 AM13/05/2019"),
                Some("%r %d/%c/%Y"),
                Time::from_slice(
                    &mut ctx,
                    &[2019, 5, 13, 4, 13, 56, 0],
                    TimeType::DateTime,
                    0,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("12: 13:56 AM 13/05/2019"),
                Some("%r%d/%c/%Y"),
                Time::from_slice(
                    &mut ctx,
                    &[2019, 5, 13, 0, 13, 56, 0],
                    TimeType::DateTime,
                    0,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("12:13 :56 pm 13/05/2019"),
                Some("%r %d/%c/%Y"),
                Time::from_slice(
                    &mut ctx,
                    &[2019, 5, 13, 12, 13, 56, 0],
                    TimeType::DateTime,
                    0,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("12:3: 56pm  13/05/2019"),
                Some("%r %d/%c/%Y"),
                Time::from_slice(
                    &mut ctx,
                    &[2019, 5, 13, 12, 3, 56, 0],
                    TimeType::DateTime,
                    0,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("11:13:56"),
                Some("%r"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 11, 13, 56, 0], TimeType::DateTime, 0),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("11:13"),
                Some("%r"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 11, 13, 0, 0], TimeType::DateTime, 0),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("11:"),
                Some("%r"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 11, 0, 0, 0], TimeType::DateTime, 0),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("11"),
                Some("%r"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 11, 0, 0, 0], TimeType::DateTime, 0),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("12"),
                Some("%r"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 0, 0, 0, 0], TimeType::DateTime, 0),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            // "%T"
            (
                Some(" 4 :13:56 13/05/2019"),
                Some("%T %d/%c/%Y"),
                Time::from_slice(
                    &mut ctx,
                    &[2019, 5, 13, 4, 13, 56, 0],
                    TimeType::DateTime,
                    0,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("23: 13:56  13/05/2019"),
                Some("%T%d/%c/%Y"),
                Time::from_slice(
                    &mut ctx,
                    &[2019, 5, 13, 23, 13, 56, 0],
                    TimeType::DateTime,
                    0,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("12:13 :56 13/05/2019"),
                Some("%T %d/%c/%Y"),
                Time::from_slice(
                    &mut ctx,
                    &[2019, 5, 13, 12, 13, 56, 0],
                    TimeType::DateTime,
                    0,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("19:3: 56  13/05/2019"),
                Some("%T %d/%c/%Y"),
                Time::from_slice(
                    &mut ctx,
                    &[2019, 5, 13, 19, 3, 56, 0],
                    TimeType::DateTime,
                    0,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("21:13"),
                Some("%T"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 21, 13, 0, 0], TimeType::DateTime, 0),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("21:"),
                Some("%T"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 21, 0, 0, 0], TimeType::DateTime, 0),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            // More patterns than input string
            (
                Some(" 2/Jun"),
                Some("%d/%b/%Y"),
                Time::from_slice(&mut ctx, &[0, 6, 2, 0, 0, 0, 0], TimeType::DateTime, 0),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some(" liter"),
                Some("lit era l"),
                Time::from_slice(&mut ctx, &[0, 0, 0, 0, 0, 0, 0], TimeType::DateTime, 0),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            // Feb 29 in leap-year
            (
                Some("29/Feb/2020 12:34:56."),
                Some("%d/%b/%Y %H:%i:%s.%f"),
                Time::from_slice(
                    &mut ctx,
                    &[2020, 2, 29, 12, 34, 56, 0],
                    TimeType::DateTime,
                    0,
                ),
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            // When `AllowInvalidDate` is true, check only that the month is in the range from 1 to
            // 12 and the day is in the range from 1 to 31
            // TODO: Now tikv don't support this flag `AllowInvalidDate` , so we don't test it.
            //(
            //    Some("31/April/2016 12:34:56."),
            //    Some("%d/%M/%Y %H:%i:%s.%f"),
            //    Time::from_slice(
            //        &mut ctx,
            //        &[2016, 4, 31, 12, 34, 56, 0],
            //        TimeType::DateTime,
            //        0,
            //    ),
            //    None,
            //    ScalarFuncSig::StrToDateDatetime,
            //),
            //(
            //    Some("29/Feb/2021 12:34:56."),
            //    Some("%d/%b/%Y %H:%i:%s.%f"),
            //    Time::from_slice(
            //        &mut ctx,
            //        &[2021, 2, 29, 12, 34, 56, 0],
            //        TimeType::DateTime,
            //        0,
            //    ),
            //    None,
            //    ScalarFuncSig::StrToDateDatetime,
            //),
            //(
            //    Some("30/Feb/2016 12:34:56.1234"),
            //    Some("%d/%b/%Y %H:%i:%S.%f"),
            //    Time::from_slice(
            //        &mut ctx,
            //        &[2016, 2, 30, 12, 34, 56, 123400],
            //        TimeType::DateTime,
            //        0,
            //    ),
            //    None,
            //    ScalarFuncSig::StrToDateDatetime,
            //),
            // Test Failed Case
            // invalid days when `AllowInvalidDate` is false
            (
                Some("04/31/2004"),
                Some("%m/%d/%Y"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ), // not exists in the real world
            (
                Some("29/Feb/2021 12:34:56."),
                Some("%d/%b/%Y %H:%i:%s.%f"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ), // Feb 29 in non-leap-year
            // MySQL will try to parse '51' for '%m', fail
            (
                Some("512 2021"),
                Some("%m%d %Y"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            // format mismatch
            (
                Some("a09:30:17"),
                Some("%h:%i:%s"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            // followed by incomplete 'AM'/'PM'
            (
                Some("12:43:24 a"),
                Some("%r"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            // invalid minute
            (
                Some("23:60:12"),
                Some("%T"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("18"),
                Some("%l"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("00:21:22 AM"),
                Some("%h:%i:%s %p"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("100/10/22"),
                Some("%y/%m/%d"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("2010-11-12 11 am"),
                Some("%Y-%m-%d %H %p"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("2010-11-12 13 am"),
                Some("%Y-%m-%d %h %p"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            (
                Some("2010-11-12 0 am"),
                Some("%Y-%m-%d %h %p"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            // MySQL accept `SEPTEMB` as `SEPTEMBER`, but we don't want this "feature" in TiDB
            // unless we have to.
            (
                Some("15 SEPTEMB 2001"),
                Some("%d %M %Y"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            // '%r' tests
            // hh = 13 with am is invalid
            (
                Some("13:13:56 AM13/5/2019"),
                Some("%r"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            // hh = 0 with am is invalid
            (
                Some("00:13:56 AM13/05/2019"),
                Some("%r"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            // hh = 0 with pm is invalid
            (
                Some("00:13:56 pM13/05/2019"),
                Some("%r"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
            // EOF while parsing "AM"/"PM"
            (
                Some("11:13:56a"),
                Some("%r"),
                None,
                None,
                ScalarFuncSig::StrToDateDatetime,
            ),
        ];
        for (date, format, expected_time, expected_duration, func_sig) in cases {
            let date = date.map(|str| str.as_bytes().to_vec());
            let format = format.map(|str| str.as_bytes().to_vec());
            let output = RpnFnScalarEvaluator::new()
                .push_param(date)
                .push_param(format);
            match func_sig {
                ScalarFuncSig::StrToDateDatetime | ScalarFuncSig::StrToDateDate => {
                    let output = output.evaluate::<Time>(func_sig).unwrap();
                    assert_eq!(output, expected_time, "got {}", output.unwrap());
                }
                ScalarFuncSig::StrToDateDuration => {
                    let output = output.evaluate::<Duration>(func_sig).unwrap();
                    assert_eq!(output, expected_duration, "got {}", output.unwrap());
                }
                _ => {}
            }
        }
    }
}
