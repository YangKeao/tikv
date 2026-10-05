// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native CoreTime arithmetic without wire Time construction or SQL-mode
//! validation. Calendar addition and duration addition retain distinct bounds.

use super::Time;

const SECONDS_IN_24_HOURS: i64 = 86_400;
const DAYS_BY_MONTH: [u8; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// Adds calendar fields with the native month-end rule. None represents the
/// original single DateAddError outcome; raw clock fields are not validated.
pub fn native_core_add_date(raw: u64, years: i64, months: i64, days: i64) -> Option<u64> {
    const MAX_ADD: i64 = 10_000 * 365;
    if !(-MAX_ADD..=MAX_ADD).contains(&years)
        || !(-MAX_ADD..=MAX_ADD).contains(&months)
        || !(-MAX_ADD..=MAX_ADD).contains(&days)
    {
        return None;
    }
    let [
        original_year,
        original_month,
        original_day,
        hour,
        minute,
        second,
        microsecond,
    ] = Time::native_core_fields(raw);
    let total_months = i64::from(original_year)
        .checked_mul(12)
        .and_then(|value| value.checked_add(i64::from(original_month) - 1))
        .and_then(|value| value.checked_add(years.checked_mul(12)?))
        .and_then(|value| value.checked_add(months))?;
    let mut year = total_months.div_euclid(12);
    let mut month = total_months.rem_euclid(12) + 1;
    let mut day = i64::from(original_day);

    if days == 0 && (years != 0 || months != 0) {
        day += native_core_fix_days(raw, years, months, days);
    } else {
        day = day.checked_add(days)?;
        normalize_day(&mut year, &mut month, &mut day);
    }
    if !(0..=9999).contains(&year) {
        return None;
    }
    Some(Time::native_core_from_fields(
        year as u16,
        month as u8,
        day as u8,
        hour as u8,
        minute as u8,
        second as u8,
        microsecond as u32,
    ))
}

/// Adds signed nanoseconds after truncating toward zero to microseconds.
/// Preserve ordinary i64 arithmetic, Euclidean clock splitting and the native
/// u32 day-number cast. This does not impose calendar-addition year limits.
pub fn native_core_add_duration(raw: u64, nanoseconds: i64) -> u64 {
    let [year, month, day, hour, minute, second, microsecond] = Time::native_core_fields(raw);
    let own_micros =
        i64::from(Time::native_calc_daynr_i32(year, month, day)) * SECONDS_IN_24_HOURS * 1_000_000
            + i64::from(hour) * 3_600_000_000
            + i64::from(minute) * 60_000_000
            + i64::from(second) * 1_000_000
            + i64::from(microsecond);
    let result = own_micros + nanoseconds / 1_000;
    let daynr = result.div_euclid(SECONDS_IN_24_HOURS * 1_000_000);
    let time = result.rem_euclid(SECONDS_IN_24_HOURS * 1_000_000);
    let (year, month, day) = native_get_date_from_daynr(daynr as u32);
    let seconds = time / 1_000_000;
    Time::native_core_from_fields(
        year as u16,
        month as u8,
        day as u8,
        (seconds / 3_600) as u8,
        (seconds % 3_600 / 60) as u8,
        (seconds % 60) as u8,
        (time % 1_000_000) as u32,
    )
}

/// Native unsigned day-number inversion, including its 3_652_500 upper bound.
/// The wire Time helper uses a different bound and must not replace this one.
pub const fn native_get_date_from_daynr(daynr: u32) -> (u32, u32, u32) {
    if daynr <= 365 || daynr >= 3_652_500 {
        return (0, 0, 0);
    }
    let mut year = daynr * 100 / 36_525;
    let temp = (((year - 1) / 100 + 1) * 3) / 4;
    let mut day_of_year = daynr - year * 365 - (year - 1) / 4 + temp;
    let mut days_in_year = Time::native_calc_days_in_year_i32(year as i32) as u32;
    while day_of_year > days_in_year {
        day_of_year -= days_in_year;
        year += 1;
        days_in_year = Time::native_calc_days_in_year_i32(year as i32) as u32;
    }
    let mut leap_day = 0;
    if days_in_year == 366 && day_of_year > 59 {
        day_of_year -= 1;
        if day_of_year == 59 {
            leap_day = 1;
        }
    }
    let mut month = 1;
    let mut index = 0;
    while index < DAYS_BY_MONTH.len() {
        let days = DAYS_BY_MONTH[index] as u32;
        if day_of_year <= days {
            break;
        }
        day_of_year -= days;
        month += 1;
        index += 1;
    }
    (year, month, day_of_year + leap_day)
}

fn last_day(year: i32, month: u8) -> u8 {
    Time::native_days_in_month(i64::from(year), u32::from(month)) as u8
}

fn normalize_day(year: &mut i64, month: &mut i64, day: &mut i64) {
    while *day <= 0 {
        *month -= 1;
        if *month == 0 {
            *month = 12;
            *year -= 1;
        }
        *day += i64::from(last_day(*year as i32, *month as u8));
    }
    loop {
        let days_in_month = i64::from(last_day(*year as i32, *month as u8));
        if *day <= days_in_month {
            break;
        }
        *day -= days_in_month;
        *month += 1;
        if *month == 13 {
            *month = 1;
            *year += 1;
        }
    }
}

/// Native month-end adjustment, shared with the original helper's direct tests.
/// Keep the plain arithmetic and the i32/u8 month-length projection unchanged.
pub fn native_core_fix_days(raw: u64, years: i64, months: i64, days: i64) -> i64 {
    if (years == 0 && months == 0) || days != 0 {
        return 0;
    }
    let [original_year, original_month, original_day, _, _, _, _] = Time::native_core_fields(raw);
    let total_months =
        i64::from(original_year) * 12 + i64::from(original_month) - 1 + years * 12 + months;
    let year = total_months.div_euclid(12);
    let month = total_months.rem_euclid(12) + 1;
    let last = i64::from(last_day(year as i32, month as u8));
    (last - i64::from(original_day)).min(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_core_arithmetic_keeps_month_clamping_raw_clock_and_daynr_domain() {
        let pack = Time::native_core_from_fields;
        let january = pack(2018, 1, 31, 1, 2, 3, 4);
        assert_eq!(
            native_core_add_date(january, 0, 1, 0),
            Some(pack(2018, 2, 28, 1, 2, 3, 4))
        );
        assert_eq!(
            native_core_add_date(january, 0, 1, 12),
            Some(pack(2018, 3, 15, 1, 2, 3, 4))
        );
        assert_eq!(
            native_core_add_date(pack(2020, 2, 29, 0, 0, 0, 0), 1, 0, 0),
            Some(pack(2021, 2, 28, 0, 0, 0, 0))
        );
        assert_eq!(
            native_core_add_date(january, 0, 0, -31),
            Some(pack(2017, 12, 31, 1, 2, 3, 4))
        );
        assert_eq!(native_core_add_date(january, i64::MAX, 0, 0), None);
        assert_eq!(native_core_add_date(january, 0, -3_650_001, 0), None);
        assert_eq!(native_core_add_date(0, 0, 0, 0), None);
        let raw_clock = pack(2018, 1, 31, 31, 63, 63, 1_048_575);
        assert_eq!(
            native_core_add_date(raw_clock | 15, 0, 1, 0),
            Some(pack(2018, 2, 28, 31, 63, 63, 1_048_575))
        );
        assert_eq!(
            native_core_fix_days(pack(2000, 1, 31, 0, 0, 0, 0), 2000, 1, 0),
            -2
        );
        assert_eq!(native_core_fix_days(january, 0, 1, 12), 0);
        assert_eq!(
            native_core_add_duration(pack(2020, 1, 1, 23, 59, 59, 999_999) | 15, 1_000),
            pack(2020, 1, 2, 0, 0, 0, 0)
        );
        assert_eq!(native_core_add_duration(january | 15, -999), january);
        assert_eq!(
            native_core_add_duration(0, -1_000),
            pack(0, 0, 0, 23, 59, 59, 999_999)
        );
        assert_eq!(
            native_core_add_duration(pack(2020, 1, 1, 0, 0, 0, 1_048_575), 0),
            pack(2020, 1, 1, 0, 0, 1, 48_575)
        );
        const LAST_NATIVE_DATE: (u32, u32, u32) = native_get_date_from_daynr(3_652_499);
        assert_eq!(native_get_date_from_daynr(365), (0, 0, 0));
        assert_eq!(native_get_date_from_daynr(366), (1, 1, 1));
        assert_eq!(native_get_date_from_daynr(3_652_425), (10000, 1, 1));
        assert_eq!(LAST_NATIVE_DATE, (10000, 3, 15));
        assert_eq!(native_get_date_from_daynr(3_652_500), (0, 0, 0));
        assert_eq!(native_get_date_from_daynr(u32::MAX), (0, 0, 0));
        let beyond_calendar =
            native_core_add_duration(pack(9999, 12, 31, 0, 0, 0, 0), 86_400_000_000_000);
        assert_eq!(beyond_calendar, pack(10000, 1, 1, 0, 0, 0, 0));
        assert_eq!(native_core_add_date(beyond_calendar, 0, 0, 0), None);
    }
}
