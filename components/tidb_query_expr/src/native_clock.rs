// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native statement-clock formatting policies. These consume the original
//! clock tuple, not a timezone-adjusted or already-rounded host answer.

use tidb_query_datatype::codec::mysql::Time;

/// Native SQL clock policies. Alias spellings with distinct precision errors
/// retain distinct identities; the statement/live clock is always actual data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeClockOperation {
    /// Local truncating NOW and its timestamp aliases.
    Now,
    /// UTC timestamp, half-up at every arity.
    UtcTimestamp,
    /// Local date without arguments.
    CurrentDate,
    /// UTC date without arguments.
    UtcDate,
    /// CURTIME's precision label and local time policy.
    Curtime,
    /// CURRENT_TIME's precision label and local time policy.
    CurrentTime,
    /// UTC time, with frontend-owned NULL suppression.
    UtcTime,
    /// Live SYSDATE; its NOW alias is resolved before calling the renderer.
    Sysdate,
}
impl NativeClockOperation {
    /// Original SQL argument count policy.
    pub fn accepts_arity(self, arity: usize) -> bool {
        match self {
            Self::CurrentDate | Self::UtcDate => arity == 0,
            _ => arity <= 1,
        }
    }
    /// Original precision/error identity (NOW aliases share NOW).
    pub fn name(self) -> &'static str {
        match self {
            Self::Now => "now",
            Self::UtcTimestamp => "utc_timestamp",
            Self::CurrentDate => "curdate",
            Self::UtcDate => "utc_date",
            Self::Curtime => "curtime",
            Self::CurrentTime => "current_time",
            Self::UtcTime => "utc_time",
            Self::Sysdate => "sysdate",
        }
    }
}
/// Failure of the actual finite renderer, never SQL NULL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeClockValueError {
    /// Original raw tuple/FSP cannot be replaced with contradictory metadata.
    InvalidPrecision,
    /// Finite formatter work was refused before materialization.
    Resource,
}
/// Render original clock fields using the existing unique policy kernels.
/// Raw nanoseconds, wide dates and ordinary arithmetic are not normalized.
pub fn native_clock_values(
    operation: NativeClockOperation,
    clock: NativeClockInput,
    fsp: Option<u32>,
    maximum: usize,
) -> Result<String, NativeClockValueError> {
    use NativeClockOperation as Op;
    if maximum == usize::MAX || maximum < 1024 {
        return Err(NativeClockValueError::Resource);
    }
    let valid = match operation {
        Op::CurrentDate | Op::UtcDate => fsp.is_none(),
        Op::Now | Op::UtcTimestamp | Op::Sysdate => fsp.is_some_and(|v| v <= 6),
        _ => fsp.is_none_or(|v| v <= 6),
    };
    if !valid {
        return Err(NativeClockValueError::InvalidPrecision);
    }
    Ok(match operation {
        Op::Now => native_now(clock, fsp.unwrap()),
        Op::UtcTimestamp => native_utc_timestamp(clock, fsp.unwrap()),
        Op::CurrentDate => native_current_date(clock),
        Op::UtcDate => native_utc_date(clock),
        Op::Curtime | Op::CurrentTime => match fsp {
            Some(fsp) => native_current_time_with_fsp(clock, fsp),
            None => native_current_time_without_fsp(clock),
        },
        Op::UtcTime => match fsp {
            Some(fsp) => native_utc_time_with_fsp(clock, fsp),
            None => native_utc_time_without_fsp(clock),
        },
        Op::Sysdate => native_sysdate(clock, fsp.unwrap()),
    })
}

/// The actual statement clock fields, without validation or normalization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeClockInput {
    pub utc_secs: i64,
    pub nanos: u32,
    pub tz_offset: i32,
}

/// Render an epoch second using the native wide Gregorian calendar policy.
pub fn native_format_clock_date(secs: i64) -> String {
    let (y, m, d) = Time::native_civil_from_days(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

fn format_clock_hms(secs: i64) -> String {
    let secs_of_day = secs.rem_euclid(86_400);
    let (hour, minute, second) = (
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60,
    );
    format!("{hour:02}:{minute:02}:{second:02}")
}

fn clock_fraction_suffix(nanos: u32, fsp: u32) -> String {
    if fsp == 0 {
        return String::new();
    }
    let fraction = nanos / 10u32.pow(9 - fsp);
    format!(".{fraction:0width$}", width = fsp as usize)
}

fn round_clock_nanos(nanos: u32, fsp: u32) -> (i64, u32) {
    let scale = 10u32.pow(9 - fsp);
    let half_up = nanos + scale / 2;
    if half_up >= 1_000_000_000 {
        (1, 0)
    } else {
        (0, (half_up / scale) * scale)
    }
}

/// The original native datetime formatter, also shared by callers that have
/// not migrated their evaluation boundary. FSP is checked by those callers;
/// arithmetic and raw nanosecond behavior remain deliberately unchanged.
pub fn native_format_clock_datetime(secs: i64, nanos: u32, fsp: u32, round: bool) -> String {
    let (carry, nanos) = if round {
        round_clock_nanos(nanos, fsp)
    } else {
        (0, nanos)
    };
    let secs = secs + carry;
    format!(
        "{} {}{}",
        native_format_clock_date(secs),
        format_clock_hms(secs),
        clock_fraction_suffix(nanos, fsp)
    )
}

fn format_clock_time_only(secs: i64, nanos: u32, fsp: u32, round: bool) -> String {
    let (carry, nanos) = if round {
        round_clock_nanos(nanos, fsp)
    } else {
        (0, nanos)
    };
    let secs = secs + carry;
    format!(
        "{}{}",
        format_clock_hms(secs),
        clock_fraction_suffix(nanos, fsp)
    )
}

/// NOW/CURRENT_TIMESTAMP truncates the original statement-clock fraction in
/// the local zone. Frontends retain FSP parsing and statement-clock selection.
pub fn native_now(clock: NativeClockInput, fsp: u32) -> String {
    native_format_clock_datetime(
        clock.utc_secs + i64::from(clock.tz_offset),
        clock.nanos,
        fsp,
        false,
    )
}

/// CURDATE/CURRENT_DATE ignores the fraction but applies the statement offset.
pub fn native_current_date(clock: NativeClockInput) -> String {
    native_format_clock_date(clock.utc_secs + i64::from(clock.tz_offset))
}

/// Live SYSDATE rounds its captured fraction in the frozen session zone. The
/// sysdate-is-now rewrite instead selects NOW and its truncating policy.
pub fn native_sysdate(clock: NativeClockInput, fsp: u32) -> String {
    native_format_clock_datetime(
        clock.utc_secs + i64::from(clock.tz_offset),
        clock.nanos,
        fsp,
        true,
    )
}

/// UTC_DATE ignores both the fractional field and the session offset.
pub fn native_utc_date(clock: NativeClockInput) -> String {
    native_format_clock_date(clock.utc_secs)
}

/// UTC_TIMESTAMP rounds the original nanoseconds directly, at every arity.
pub fn native_utc_timestamp(clock: NativeClockInput, fsp: u32) -> String {
    native_format_clock_datetime(clock.utc_secs, clock.nanos, fsp, true)
}

/// Zero-argument CURTIME/CURRENT_TIME truncates in the statement's local zone.
pub fn native_current_time_without_fsp(clock: NativeClockInput) -> String {
    format_clock_time_only(
        clock.utc_secs + i64::from(clock.tz_offset),
        clock.nanos,
        0,
        false,
    )
}

/// Explicit CURTIME/CURRENT_TIME first truncates to microseconds, then rounds.
pub fn native_current_time_with_fsp(clock: NativeClockInput, fsp: u32) -> String {
    let nanos = clock.nanos / 1_000 * 1_000;
    format_clock_time_only(
        clock.utc_secs + i64::from(clock.tz_offset),
        nanos,
        fsp,
        true,
    )
}

/// Zero-argument UTC_TIME truncates without applying the session offset.
pub fn native_utc_time_without_fsp(clock: NativeClockInput) -> String {
    format_clock_time_only(clock.utc_secs, clock.nanos, 0, false)
}

/// Explicit UTC_TIME preserves the microsecond-truncation-before-rounding step.
pub fn native_utc_time_with_fsp(clock: NativeClockInput, fsp: u32) -> String {
    let nanos = clock.nanos / 1_000 * 1_000;
    format_clock_time_only(clock.utc_secs, nanos, fsp, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_local_date_now_and_live_sysdate_keep_distinct_fractions() {
        let clock = NativeClockInput {
            utc_secs: 1_700_000_000,
            nanos: 654_320_955,
            tz_offset: 19_800,
        };
        assert_eq!(native_current_date(clock), "2023-11-15");
        assert_eq!(native_now(clock, 0), "2023-11-15 03:43:20");
        assert_eq!(native_sysdate(clock, 0), "2023-11-15 03:43:21");
        assert_eq!(native_now(clock, 6), "2023-11-15 03:43:20.654320");
        assert_eq!(native_sysdate(clock, 6), "2023-11-15 03:43:20.654321");

        let end_of_day = NativeClockInput {
            utc_secs: 86_399,
            nanos: 999_999_500,
            tz_offset: 0,
        };
        assert_eq!(native_current_date(end_of_day), "1970-01-01");
        assert_eq!(native_now(end_of_day, 6), "1970-01-01 23:59:59.999999");
        assert_eq!(native_sysdate(end_of_day, 6), "1970-01-02 00:00:00.000000");
    }

    #[test]
    fn native_local_clock_keeps_raw_nanoseconds_and_negative_offset() {
        let raw = NativeClockInput {
            utc_secs: 0,
            nanos: 1_234_567_890,
            tz_offset: -1,
        };
        assert_eq!(native_current_date(raw), "1969-12-31");
        assert_eq!(native_now(raw, 6), "1969-12-31 23:59:59.1234567");
        assert_eq!(native_sysdate(raw, 6), "1970-01-01 00:00:00.000000");
        let wide = NativeClockInput {
            nanos: u32::MAX,
            ..raw
        };
        assert_eq!(native_current_date(wide), "1969-12-31");
        assert_eq!(native_now(wide, 0), "1969-12-31 23:59:59");
        assert_eq!(native_now(wide, 6), "1969-12-31 23:59:59.4294967");
    }

    #[test]
    fn native_clock_fixed_family_literal_policies() {
        let clock = NativeClockInput {
            utc_secs: 1_700_000_000,
            nanos: 654_320_955,
            tz_offset: 19_800,
        };
        assert_eq!(native_utc_date(clock), "2023-11-14");
        assert_eq!(native_utc_timestamp(clock, 0), "2023-11-14 22:13:21");
        assert_eq!(native_utc_timestamp(clock, 6), "2023-11-14 22:13:20.654321");
        assert_eq!(native_current_time_without_fsp(clock), "03:43:20");
        assert_eq!(native_current_time_with_fsp(clock, 0), "03:43:21");
        assert_eq!(native_current_time_with_fsp(clock, 6), "03:43:20.654320");
        assert_eq!(native_utc_time_without_fsp(clock), "22:13:20");
        assert_eq!(native_utc_time_with_fsp(clock, 0), "22:13:21");
        assert_eq!(native_utc_time_with_fsp(clock, 6), "22:13:20.654320");
        // Thin native NOW/CURDATE/SYSDATE aliases use these same pure bodies,
        // without claiming that those callers have migrated their roots.
        assert_eq!(native_format_clock_date(1_700_019_800), "2023-11-15");
        assert_eq!(
            native_format_clock_datetime(1_700_019_800, 654_320_955, 6, false),
            "2023-11-15 03:43:20.654320"
        );
        assert_eq!(
            native_format_clock_datetime(1_700_019_800, 654_320_955, 6, true),
            "2023-11-15 03:43:20.654321"
        );
    }

    #[test]
    fn native_clock_epoch_carry_and_raw_fields() {
        let clock = NativeClockInput {
            utc_secs: -1,
            nanos: 999_999_500,
            tz_offset: 1,
        };
        assert_eq!(native_utc_date(clock), "1969-12-31");
        assert_eq!(native_utc_timestamp(clock, 6), "1970-01-01 00:00:00.000000");
        assert_eq!(native_utc_time_without_fsp(clock), "23:59:59");
        assert_eq!(native_utc_time_with_fsp(clock, 0), "00:00:00");
        assert_eq!(native_utc_time_with_fsp(clock, 6), "23:59:59.999999");
        assert_eq!(native_current_time_without_fsp(clock), "00:00:00");
        assert_eq!(native_current_time_with_fsp(clock, 6), "00:00:00.999999");
        let raw = NativeClockInput {
            utc_secs: 0,
            nanos: u32::MAX,
            tz_offset: i32::MIN,
        };
        assert_eq!(native_utc_date(raw), "1970-01-01");
        assert_eq!(native_utc_time_without_fsp(raw), "00:00:00");
        assert_eq!(
            native_format_clock_datetime(0, 1_234_567_890, 6, false),
            "1970-01-01 00:00:00.1234567"
        );
        assert_eq!(
            native_utc_timestamp(
                NativeClockInput {
                    nanos: 1_500_000_000,
                    ..raw
                },
                0
            ),
            "1970-01-01 00:00:01"
        );
        assert_eq!(native_format_clock_date(-62_167_219_200), "0000-01-01");
        assert_eq!(native_format_clock_date(253_402_300_800), "10000-01-01");
    }
}

#[cfg(test)]
mod actual_value_tests {
    use super::*;
    #[test]
    fn actual_clock_renderer_keeps_raw_fields_and_finite_precision_contracts() {
        let c = NativeClockInput {
            utc_secs: -1,
            nanos: 999999500,
            tz_offset: 1,
        };
        assert_eq!(
            native_clock_values(NativeClockOperation::Now, c, Some(6), 4096).unwrap(),
            "1970-01-01 00:00:00.999999"
        );
        assert_eq!(
            native_clock_values(NativeClockOperation::UtcTimestamp, c, Some(6), 4096).unwrap(),
            "1970-01-01 00:00:00.000000"
        );
        assert_eq!(
            native_clock_values(NativeClockOperation::Curtime, c, None, 4096).unwrap(),
            "00:00:00"
        );
        assert_eq!(
            native_clock_values(NativeClockOperation::CurrentTime, c, Some(0), 4096).unwrap(),
            "00:00:01"
        );
        assert_eq!(
            native_clock_values(NativeClockOperation::UtcTime, c, Some(6), 4096).unwrap(),
            "23:59:59.999999"
        );
        for max in [0, usize::MAX] {
            assert_eq!(
                native_clock_values(NativeClockOperation::Now, c, Some(0), max),
                Err(NativeClockValueError::Resource)
            );
        }
        for (op, fsp) in [
            (NativeClockOperation::Now, None),
            (NativeClockOperation::Sysdate, Some(7)),
            (NativeClockOperation::CurrentDate, Some(0)),
        ] {
            assert_eq!(
                native_clock_values(op, c, fsp, 4096),
                Err(NativeClockValueError::InvalidPrecision)
            );
        }
    }
    #[test]
    fn actual_clock_catalog_uses_capabilities_at_only_original_arities() {
        use crate::local::{BatchCallSignature, SqlBatchHostOperation};
        for op in [
            NativeClockOperation::Now,
            NativeClockOperation::UtcTimestamp,
            NativeClockOperation::CurrentDate,
            NativeClockOperation::UtcDate,
            NativeClockOperation::Curtime,
            NativeClockOperation::CurrentTime,
            NativeClockOperation::UtcTime,
            NativeClockOperation::Sysdate,
        ] {
            for arity in 0..3 {
                let host = SqlBatchHostOperation::ClockNative {
                    operation: op,
                    arity: arity as u8,
                };
                assert_eq!(host.accepts_arity(arity), op.accepts_arity(arity));
                if op.accepts_arity(arity) {
                    assert!(
                        BatchCallSignature::for_host(host, arity)
                            .unwrap()
                            .uses_native_value_inputs()
                    );
                }
            }
        }
    }
}
