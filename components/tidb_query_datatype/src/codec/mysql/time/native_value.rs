// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native temporal construction and validation over a lossless raw value.
//! Calendar bits remain separate from kind and FSP, including synthetic values
//! which cannot be represented by merging all three into a shared Time word.

use std::fmt;

use chrono::{TimeZone, Utc};

use super::{
    NativeDateTimeValidationError, NativeFspError, NativeTimeConversionError, Time, TimeType,
    native_core_to_datetime,
};

/// Temporal construction or conversion failure in the native value domain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeTimeError {
    /// Fractional-seconds precision was outside TiDB's accepted domain.
    InvalidFsp(NativeFspError),
    /// One calendar field exceeded its representable or valid range.
    OutOfRange(&'static str),
    /// Calendar-to-timezone conversion failed.
    Conversion(NativeTimeConversionError),
    /// A zero month or day is forbidden by the conversion flags.
    ZeroInDate,
    /// An all-zero numeric date is forbidden by `FlagIgnoreZeroDateErr`.
    ZeroDate,
    /// Month/day fields do not form an accepted MySQL date.
    InvalidDate,
    /// Hour/minute/second fields exceed MySQL's clock range.
    InvalidClock,
    /// TIMESTAMP falls outside TiDB's UTC storage range.
    TimestampOutOfRange,
    /// A temporal operation received an unsupported interval unit.
    InvalidUnit(String),
}

impl fmt::Display for NativeTimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFsp(error) => error.fmt(formatter),
            Self::OutOfRange(field) => write!(formatter, "time {field} is out of range"),
            Self::Conversion(error) => error.fmt(formatter),
            Self::ZeroInDate => formatter.write_str("zero month or day in date"),
            Self::ZeroDate => formatter.write_str("zero date"),
            Self::InvalidDate => formatter.write_str("invalid MySQL date"),
            Self::InvalidClock => formatter.write_str("invalid MySQL clock"),
            Self::TimestampOutOfRange => formatter.write_str("timestamp is out of range"),
            Self::InvalidUnit(unit) => write!(formatter, "invalid unit {unit}"),
        }
    }
}

impl std::error::Error for NativeTimeError {}

impl From<NativeTimeConversionError> for NativeTimeError {
    fn from(error: NativeTimeConversionError) -> Self {
        Self::Conversion(error)
    }
}

/// Exact native calendar bits plus independent temporal metadata.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NativeTemporalValue {
    pub raw: u64,
    pub kind: TimeType,
    pub fsp: u8,
}

impl NativeTemporalValue {
    /// Constructs raw transport storage without FSP or calendar validation.
    pub const fn from_raw_parts(raw: u64, kind: TimeType, fsp: u8) -> Self {
        Self { raw, kind, fsp }
    }

    /// Normalize only FSP. DATE ignores even invalid FSP and retains all clock
    /// fields and reserved raw bits; calendar validation is a separate
    /// operation.
    pub fn new(raw: u64, kind: TimeType, fsp: i64) -> Result<Self, NativeTimeError> {
        let fsp = if kind == TimeType::Date {
            0
        } else {
            Time::native_normalize_fsp(fsp)
                .ok_or(NativeTimeError::InvalidFsp(NativeFspError::InvalidFsp(fsp)))?
                as u8
        };
        Ok(Self { raw, kind, fsp })
    }

    /// Check field storage widths in source order before applying FSP policy.
    #[allow(clippy::too_many_arguments)]
    pub fn from_date_checked(
        year: i32,
        month: i32,
        day: i32,
        hour: i32,
        minute: i32,
        second: i32,
        microsecond: i32,
        kind: TimeType,
        fsp: i64,
    ) -> Result<Self, NativeTimeError> {
        for (name, value, limit) in [
            ("year", year, 1 << 14),
            ("month", month, 1 << 4),
            ("day", day, 1 << 5),
            ("hour", hour, 1 << 5),
            ("minute", minute, 1 << 6),
            ("second", second, 1 << 6),
            ("microsecond", microsecond, 1 << 20),
        ] {
            if !(0..limit).contains(&value) {
                return Err(NativeTimeError::OutOfRange(name));
            }
        }
        Self::new(
            Time::native_core_from_fields(
                year as u16,
                month as u8,
                day as u8,
                hour as u8,
                minute as u8,
                second as u8,
                microsecond as u32,
            ),
            kind,
            fsp,
        )
    }

    pub fn set_kind(&mut self, kind: TimeType) {
        self.kind = kind;
        if kind == TimeType::Date {
            self.fsp = 0;
        }
    }

    pub fn set_fsp(&mut self, fsp: i64) -> Result<(), NativeTimeError> {
        if self.kind == TimeType::Date {
            return Ok(());
        }
        self.fsp = Time::native_normalize_fsp(fsp)
            .ok_or(NativeTimeError::InvalidFsp(NativeFspError::InvalidFsp(fsp)))?
            as u8;
        Ok(())
    }

    /// Preserve Timestamp's exact-zero and conversion-first policy. Neither
    /// validation branch changes raw fields, normalizes FSP, or adjusts a gap.
    pub fn validate<TZ: TimeZone>(
        self,
        allow_zero_in_date: bool,
        allow_invalid_date: bool,
        timezone: &TZ,
    ) -> Result<(), NativeTimeError> {
        if self.kind == TimeType::Timestamp {
            if self.raw == 0 {
                return Ok(());
            }
            let utc = native_core_to_datetime(self.raw, timezone, false)?.with_timezone(&Utc);
            let seconds = utc.timestamp();
            if !(1..=2_147_483_647).contains(&seconds) {
                return Err(NativeTimeError::TimestampOutOfRange);
            }
            return Ok(());
        }
        let core = Time(self.raw);
        Time::validate_native_datetime_fields(
            core.year() as i32,
            core.month() as u8,
            core.day() as u8,
            core.hour() as u8,
            core.minute() as u8,
            core.second() as u8,
            core.micro(),
            allow_zero_in_date,
            allow_invalid_date,
        )
        .map_err(|error| match error {
            NativeDateTimeValidationError::InvalidDate => NativeTimeError::InvalidDate,
            NativeDateTimeValidationError::InvalidClock => NativeTimeError::InvalidClock,
            NativeDateTimeValidationError::ZeroInDate => NativeTimeError::ZeroInDate,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_temporal_value_keeps_raw_metadata_and_validation_order() {
        assert_eq!(
            [
                TimeType::Date as u8,
                TimeType::DateTime as u8,
                TimeType::Timestamp as u8
            ],
            [0, 1, 2]
        );
        let raw = Time::native_core_from_fields(2020, 2, 29, 23, 59, 58, 123456) | 0b1011;
        assert_eq!(
            NativeTemporalValue::from_raw_parts(u64::MAX, TimeType::Date, u8::MAX),
            NativeTemporalValue {
                raw: u64::MAX,
                kind: TimeType::Date,
                fsp: u8::MAX,
            }
        );
        assert_eq!(
            NativeTemporalValue::new(raw, TimeType::DateTime, -2),
            Err(NativeTimeError::InvalidFsp(NativeFspError::InvalidFsp(-2)))
        );
        assert_eq!(
            NativeTemporalValue::new(raw, TimeType::DateTime, -1)
                .unwrap()
                .fsp,
            0
        );
        assert_eq!(
            NativeTemporalValue::new(raw, TimeType::DateTime, i64::MAX)
                .unwrap()
                .fsp,
            6
        );
        let date = NativeTemporalValue::new(raw, TimeType::Date, -2).unwrap();
        assert_eq!(date.raw, raw);
        assert_eq!(date.fsp, 0);
        assert_eq!(date.validate(false, false, &Utc), Ok(()));
        let mut synthetic = NativeTemporalValue {
            raw,
            kind: TimeType::Date,
            fsp: 255,
        };
        synthetic.set_fsp(-2).unwrap();
        assert_eq!(synthetic.fsp, 255);
        synthetic.set_kind(TimeType::DateTime);
        assert_eq!(synthetic.fsp, 255);
        assert_eq!(
            synthetic.set_fsp(-2),
            Err(NativeTimeError::InvalidFsp(NativeFspError::InvalidFsp(-2)))
        );
        assert_eq!(synthetic.fsp, 255);
        synthetic.set_fsp(i64::MAX).unwrap();
        assert_eq!(synthetic.fsp, 6);
        synthetic.set_fsp(-1).unwrap();
        assert_eq!(synthetic.fsp, 0);
        synthetic.set_fsp(4).unwrap();
        synthetic.set_kind(TimeType::Date);
        assert_eq!(synthetic.raw, raw);
        assert_eq!(synthetic.fsp, 0);

        assert_eq!(
            NativeTemporalValue::from_date_checked(
                -1,
                16,
                32,
                32,
                64,
                64,
                1 << 20,
                TimeType::DateTime,
                -2
            ),
            Err(NativeTimeError::OutOfRange("year"))
        );
        assert_eq!(
            NativeTemporalValue::from_date_checked(
                2020,
                16,
                32,
                32,
                64,
                64,
                1 << 20,
                TimeType::Date,
                -2
            ),
            Err(NativeTimeError::OutOfRange("month"))
        );
        assert_eq!(
            NativeTemporalValue::from_date_checked(
                2020,
                1,
                1,
                0,
                0,
                0,
                1 << 20,
                TimeType::DateTime,
                -2
            ),
            Err(NativeTimeError::OutOfRange("microsecond"))
        );
        let unchecked_calendar = NativeTemporalValue::from_date_checked(
            16383,
            15,
            31,
            31,
            63,
            63,
            (1 << 20) - 1,
            TimeType::Date,
            -2,
        )
        .unwrap();
        assert_eq!(
            unchecked_calendar.raw,
            Time::native_core_from_fields(16383, 15, 31, 31, 63, 63, (1 << 20) - 1)
        );
        assert_eq!(
            unchecked_calendar.validate(true, true, &Utc),
            Err(NativeTimeError::InvalidDate)
        );

        let timestamp = |raw| NativeTemporalValue {
            raw,
            kind: TimeType::Timestamp,
            fsp: 255,
        };
        assert_eq!(timestamp(0).validate(false, false, &Utc), Ok(()));
        assert_eq!(
            timestamp(1).validate(true, true, &Utc),
            Err(NativeTimeError::Conversion(
                NativeTimeConversionError::InvalidCalendar
            ))
        );
        assert_eq!(
            timestamp(Time::native_core_from_fields(1960, 0, 1, 0, 0, 0, 0))
                .validate(true, true, &Utc),
            Err(NativeTimeError::Conversion(
                NativeTimeConversionError::InvalidCalendar
            ))
        );
        assert_eq!(
            timestamp(Time::native_core_from_fields(1960, 1, 1, 0, 0, 0, 0))
                .validate(true, true, &Utc),
            Err(NativeTimeError::TimestampOutOfRange)
        );
        assert_eq!(
            timestamp(Time::native_core_from_fields(1970, 1, 1, 0, 0, 1, 0))
                .validate(false, false, &Utc),
            Ok(())
        );
        assert_eq!(
            timestamp(Time::native_core_from_fields(2038, 1, 19, 3, 14, 7, 999999))
                .validate(false, false, &Utc),
            Ok(())
        );
        assert_eq!(
            timestamp(Time::native_core_from_fields(2038, 1, 19, 3, 14, 8, 0))
                .validate(false, false, &Utc),
            Err(NativeTimeError::TimestampOutOfRange)
        );
    }
}
