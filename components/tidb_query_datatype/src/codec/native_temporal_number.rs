// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Exact native temporal numeric construction. Rendering precision follows raw
//! metadata, including the original substring/negative-FSP panic boundaries.

use super::{
    mysql::{
        Duration, NativeDecimalParseValue, Time, native_decimal_from_literal,
        time::{NativeTemporalValue, TimeType},
    },
    native_duration_convert::NativeDurationParts,
};

/// Native Time.ToNumber, preserving raw-zero and DATE's precision bypass.
/// This is not temporal rounding or SQL datetime validation.
pub fn native_time_to_number(value: NativeTemporalValue) -> NativeDecimalParseValue {
    if value.raw == 0 {
        return NativeDecimalParseValue::from_int(0);
    }
    let [year, month, day, hour, minute, second, microsecond] = Time::native_core_fields(value.raw);
    let mut text = if value.kind == TimeType::Date {
        format!("{year:04}{month:02}{day:02}")
    } else {
        format!("{year:04}{month:02}{day:02}{hour:02}{minute:02}{second:02}")
    };
    if value.kind != TimeType::Date && value.fsp > 0 {
        let fraction = format!("{microsecond:06}");
        text.push('.');
        text.push_str(&fraction[..usize::from(value.fsp)]);
    }
    native_decimal_from_literal(&text)
}

/// Native Duration.ToNumber. Fraction slicing is not normalization, and sign
/// reversal uses the shared decimal arithmetic owner, including zero policy.
pub fn native_duration_to_number(value: NativeDurationParts) -> NativeDecimalParseValue {
    let hour = Duration::hours_from_nanos(value.nanoseconds);
    let minute = Duration::minutes_from_nanos(value.nanoseconds);
    let second = Duration::secs_from_nanos(value.nanoseconds);
    let literal = if value.fsp == 0 {
        format!("{hour:02}{minute:02}{second:02}")
    } else {
        let fraction = format!("{:06}", Duration::micro_secs_from_nanos(value.nanoseconds));
        format!(
            "{hour:02}{minute:02}{second:02}.{}",
            &fraction[..usize::try_from(value.fsp).expect("nonnegative duration FSP")]
        )
    };
    let number = native_decimal_from_literal(&literal);
    if value.nanoseconds < 0 {
        number.negate()
    } else {
        number
    }
}

#[cfg(test)]
#[test]
fn temporal_numbers_keep_owned_coefficients_raw_fsp_negative_zero_and_panics() {
    fn check(
        value: NativeDecimalParseValue,
        negative: bool,
        digits: &[u8],
        scale: u32,
        storage: u32,
    ) {
        let view = value.as_ref();
        assert_eq!(view.negative, negative);
        assert_eq!(view.digits, digits);
        assert_eq!(view.scale, scale);
        assert_eq!(view.storage_scale, storage);
        assert_eq!(view.declared_shape, None);
    }
    let raw = Time::native_core_from_fields(2020, 1, 2, 3, 4, 5, 123456) | 15;
    let time = NativeTemporalValue {
        raw,
        kind: TimeType::DateTime,
        fsp: 3,
    };
    check(
        native_time_to_number(time),
        false,
        b"20200102030405123",
        3,
        3,
    );
    check(
        native_time_to_number(NativeTemporalValue {
            kind: TimeType::Date,
            fsp: 255,
            ..time
        }),
        false,
        b"20200102",
        0,
        0,
    );
    check(
        native_time_to_number(NativeTemporalValue {
            raw: 0,
            fsp: 255,
            ..time
        }),
        false,
        b"0",
        0,
        0,
    );
    check(
        native_time_to_number(NativeTemporalValue {
            raw: 1,
            fsp: 6,
            ..time
        }),
        false,
        b"000000",
        6,
        6,
    );
    let wide_fraction = NativeTemporalValue {
        raw: Time::native_core_from_fields(0, 0, 0, 0, 0, 0, 1_048_575),
        fsp: 7,
        ..time
    };
    check(
        native_time_to_number(wide_fraction),
        false,
        b"1048575",
        7,
        7,
    );
    check(
        native_duration_to_number(NativeDurationParts {
            nanoseconds: -1,
            fsp: 6,
        }),
        false,
        b"000000",
        6,
        6,
    );
    check(
        native_duration_to_number(NativeDurationParts {
            nanoseconds: -45_296_123_456_000,
            fsp: 3,
        }),
        true,
        b"123456123",
        3,
        3,
    );
    check(
        native_duration_to_number(NativeDurationParts {
            nanoseconds: 3_600_000_000_000,
            fsp: 0,
        }),
        false,
        b"10000",
        0,
        0,
    );
    assert!(
        std::panic::catch_unwind(|| native_time_to_number(NativeTemporalValue {
            fsp: 8,
            ..wide_fraction
        }))
        .is_err()
    );
    assert!(
        std::panic::catch_unwind(|| native_time_to_number(NativeTemporalValue {
            fsp: 255,
            ..time
        }))
        .is_err()
    );
    for fsp in [-1, 7] {
        assert!(
            std::panic::catch_unwind(|| native_duration_to_number(NativeDurationParts {
                nanoseconds: 0,
                fsp
            }))
            .is_err()
        );
    }
}
