// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

//! Native CONVERT_TZ's text domain and the legacy session instant policy.

use std::sync::LazyLock;

use chrono::{
    DateTime, FixedOffset, Local, LocalResult, NaiveDate, NaiveDateTime, TimeZone as _, Utc,
};
use regex::Regex;
use tidb_query_datatype::codec::mysql::{Time, time::NativeSessionTimeZone};

/// Validate every present operand even when another operand is SQL NULL.
/// Date and zone syntax belong to execution, not transport admission.
pub fn convert_tz_native_args_valid(
    datetime: Option<&[u8]>,
    from: Option<&[u8]>,
    to: Option<&[u8]>,
) -> bool {
    [datetime, from, to]
        .into_iter()
        .all(|value| value.map_or(true, |bytes| std::str::from_utf8(bytes).is_ok()))
}

// Preserve the source's Unicode \d, including the subsequent integer parse's
// panic for a non-ASCII digit accepted by this exact regex.
static TZ_OFFSET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^[-+](0?[0-9]|1[0-3]):[0-5]?\d$)|(^\+14:00?$)").unwrap());

fn parse_conv_tz(s: &str) -> Option<NativeSessionTimeZone> {
    if s.is_empty() {
        return None;
    }
    if TZ_OFFSET_RE.is_match(s) {
        let sign = if s.starts_with('-') { -1 } else { 1 };
        let body = &s[1..];
        let (h, m) = body.split_once(':').expect("regex guarantees a colon");
        let h: i32 = h.parse().expect("regex guarantees digits");
        let m: i32 = m.parse().expect("regex guarantees digits");
        return Some(NativeSessionTimeZone::Fixed {
            name: String::new(),
            offset_secs: sign * (h * 3600 + m * 60),
        });
    }
    if s.eq_ignore_ascii_case("SYSTEM") {
        return Some(NativeSessionTimeZone::Local);
    }
    // The variant fixes the inferred parser to native chrono-tz 0.10.4, not
    // the independent wire timezone type/database.
    s.parse().ok().map(NativeSessionTimeZone::Named)
}

fn local_to_instant(naive: NaiveDateTime, tz: &NativeSessionTimeZone) -> Option<DateTime<Utc>> {
    match tz {
        NativeSessionTimeZone::Fixed { offset_secs, .. } => {
            let offset = FixedOffset::east_opt(*offset_secs)?;
            Some(
                offset
                    .from_local_datetime(&naive)
                    .single()?
                    .with_timezone(&Utc),
            )
        }
        NativeSessionTimeZone::Named(tz) => native_legacy_local_to_instant(tz, &naive),
        NativeSessionTimeZone::Local => match Local.from_local_datetime(&naive) {
            LocalResult::Single(value) => Some(value.with_timezone(&Utc)),
            // SYSTEM has its own source policy: later overlap, but no gap adjustment.
            LocalResult::Ambiguous(_, later) => Some(later.with_timezone(&Utc)),
            LocalResult::None => None,
        },
    }
}

fn parse_datetime(s: &str) -> Option<(NaiveDateTime, String)> {
    let input = s.trim();
    let (year, month, day) = Time::parse_native_date_ymd(input)?;
    let date = NaiveDate::from_ymd_opt(i32::try_from(year).ok()?, month, day)?;
    let time_text = input
        .split_once(char::is_whitespace)
        .map(|(_, time)| time.trim());
    let (h, mi, sec, frac) = match time_text {
        None | Some("") => (0, 0, 0, String::new()),
        Some(t) => Time::parse_native_clock_with_fraction(t)?,
    };
    let micros: u32 = if frac.is_empty() {
        0
    } else {
        format!("{frac:0<6}").parse().ok()?
    };
    Some((date.and_hms_micro_opt(h, mi, sec, micros)?, frac))
}

pub(crate) fn evaluate_native_convert_tz(dt: &str, from_s: &str, to_s: &str) -> Option<String> {
    let (naive, frac) = parse_datetime(dt)?;
    // Both zone parsers run before testing either Option, even when from is absent.
    let (Some(from_tz), Some(to_tz)) = (parse_conv_tz(from_s), parse_conv_tz(to_s)) else {
        return None;
    };
    let instant = local_to_instant(naive, &from_tz)?;
    let local = match &to_tz {
        NativeSessionTimeZone::Fixed { offset_secs, .. } => {
            let offset = FixedOffset::east_opt(*offset_secs)?;
            instant.with_timezone(&offset).naive_local()
        }
        NativeSessionTimeZone::Named(tz) => instant.with_timezone(tz).naive_local(),
        NativeSessionTimeZone::Local => instant.with_timezone(&Local).naive_local(),
    };
    use chrono::{Datelike, Timelike};
    let mut out = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        local.year(),
        local.month(),
        local.day(),
        local.hour(),
        local.minute(),
        local.second()
    );
    if !frac.is_empty() {
        out.push('.');
        out.push_str(&frac);
    }
    Some(out)
}

/// Go `time.Date` for a NAMED zone: the instant a wall clock names.
///
/// chrono answers this as a three-way `LocalResult`, and mapping those three
/// onto Go's answer case by case gets the AMBIGUOUS case wrong. On the autumn
/// transition 02:30 occurs twice, and Go takes the SECOND occurrence --
/// captured from a real session, `UNIX_TIMESTAMP('2025-10-26 02:30:00')` in
/// `Europe/Paris` is 1761442200, an hour after chrono's `earliest`.
///
/// Go never chooses between occurrences at all, which is why a case analysis
/// mis-models it: `time.Date` reads the offset in force at the wall clock
/// READ AS UTC and subtracts it, re-reading the offset once if the result
/// crossed a transition. The second occurrence is simply what that arithmetic
/// produces. Doing the same here leaves one rule and no cases -- try that
/// offset, then the offset at the instant it names, and take the first that
/// renders back to the wall clock we started from.
///
/// A wall clock that renders back from NEITHER exists in no offset at all: it
/// is inside a spring-forward gap, which is [`dst_gap_bound`]'s subject.
pub fn native_legacy_local_to_instant<TZ: chrono::TimeZone>(
    tz: &TZ,
    naive: &NaiveDateTime,
) -> Option<chrono::DateTime<Utc>> {
    let first = *naive - chrono::Duration::seconds(i64::from(offset_at(tz, naive)));
    let second = *naive - chrono::Duration::seconds(i64::from(offset_at(tz, &first)));
    for candidate in [first, second] {
        if candidate.and_utc().with_timezone(tz).naive_local() == *naive {
            return Some(candidate.and_utc());
        }
    }
    dst_gap_bound(tz, naive)
}

/// The zone's offset east of UTC at a UTC instant (Go's `Location.lookup`).
fn offset_at<TZ: chrono::TimeZone>(tz: &TZ, instant: &NaiveDateTime) -> i32 {
    use chrono::Offset as _;
    tz.offset_from_utc_datetime(instant).fix().local_minus_utc()
}

/// Go `types.CoreTime.AdjustedGoTime` (`pkg/types/core_time.go`), reached
/// from `adjustTimestampErrForDST` whenever a TIMESTAMP's wall clock falls in
/// a daylight-saving gap.
///
/// Go does not reject such a value. `time.Date` normalizes it into the new
/// offset, `ZoneBounds` then names the transition either side of that
/// instant, and the CLOSER bound becomes the answer -- unless both are more
/// than four hours away, which is Go's own guard against a zone whose
/// transition this heuristic would not really be describing.
///
/// `2025-03-30 02:30:00` in `Europe/Paris` is the recorded case: the clock
/// jumps 02:00 -> 03:00, so the value lands on the transition itself and
/// `UNIX_TIMESTAMP` answers 1743296400 rather than 0.
///
/// The transition is found by bisecting the UTC offset over the day either
/// side of the value, because chrono-tz publishes offsets rather than the
/// transition table itself. Within a gap the PRECEDING bound is always the
/// nearer one -- the following transition is a season away -- so the bisection
/// only has to find that one.
fn dst_gap_bound<TZ: chrono::TimeZone>(
    tz: &TZ,
    naive: &NaiveDateTime,
) -> Option<chrono::DateTime<Utc>> {
    use chrono::Offset as _;
    let offset_at =
        |instant: &NaiveDateTime| tz.offset_from_utc_datetime(instant).fix().local_minus_utc();
    let mut before = *naive - chrono::Duration::hours(24);
    let mut after = *naive + chrono::Duration::hours(24);
    let (offset_before, offset_after) = (offset_at(&before), offset_at(&after));
    if offset_before == offset_after {
        return None;
    }
    while after - before > chrono::Duration::seconds(1) {
        let middle = before + (after - before) / 2;
        if offset_at(&middle) == offset_before {
            before = middle;
        } else {
            after = middle;
        }
    }
    // Go's own normalization of the nonexistent wall clock: the instant it
    // names using the offset still in force before the transition.
    let normalized = *naive - chrono::Duration::seconds(i64::from(offset_before));
    if (after - normalized).abs() > chrono::Duration::hours(4) {
        return None;
    }
    Some(after.and_utc())
}

#[cfg(test)]
mod tests {
    use chrono::{Datelike, Timelike};

    use super::*;

    #[test]
    fn convert_tz_core_keeps_text_zone_order_and_original_offset_domain() {
        for (dt, from, to, expected) in [
            (
                "2004-01-01 12:00:00.11111111111",
                "-00:00",
                "+12:34",
                Some("2004-01-02 00:34:00.111111"),
            ),
            ("20040101", "+0:0", "+14:0", Some("2004-01-01 14:00:00")),
            (
                "20000-01-01 00:00:00.010",
                "+00:00",
                "+00:00",
                Some("20000-01-01 00:00:00.010"),
            ),
            ("2021-01-01T00:00:00", "+00:00", "+00:00", None),
            ("2004-01-01 12:00:00", "-14:00", "+00:00", None),
            ("2004-01-01 12:00:00", "+00:00", "+14:01", None),
            ("2004-01-01 12:00:00", " +00:00", "+00:00", None),
            ("2004-01-01 12:00:00", "", "+00:00", None),
            ("2004-01-01 12:00:00.123456x", "+00:00", "+00:00", None),
            ("not-a-date", "", "+1:٢", None),
        ] {
            assert_eq!(
                evaluate_native_convert_tz(dt, from, to).as_deref(),
                expected,
                "{dt} / {from} / {to}"
            );
        }
        assert!(
            std::panic::catch_unwind(|| evaluate_native_convert_tz(
                "2004-01-01 12:00:00",
                "",
                "+1:٢"
            ))
            .is_err()
        );
        let naive = NaiveDate::from_ymd_opt(2004, 1, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let system_expected = match Local.from_local_datetime(&naive) {
            LocalResult::Single(value) => Some(value.with_timezone(&Utc)),
            LocalResult::Ambiguous(_, later) => Some(later.with_timezone(&Utc)),
            LocalResult::None => None,
        };
        assert_eq!(
            local_to_instant(naive, &NativeSessionTimeZone::Local),
            system_expected
        );
        assert!(convert_tz_native_args_valid(
            None,
            Some(b"bad zone"),
            Some(b"")
        ));
        assert!(!convert_tz_native_args_valid(None, Some(&[255]), None));
        assert!(!convert_tz_native_args_valid(None, None, Some(&[255])));
    }

    #[test]
    fn legacy_instant_helper_keeps_naive_width_overlap_and_fractional_gap_search() {
        let paris = NativeSessionTimeZone::Named("Europe/Paris".parse().unwrap());
        let eastern = NativeSessionTimeZone::Named("US/Eastern".parse().unwrap());
        let repeat = NaiveDate::from_ymd_opt(2025, 10, 26)
            .unwrap()
            .and_hms_opt(2, 30, 0)
            .unwrap();
        assert_eq!(
            native_legacy_local_to_instant(&paris, &repeat)
                .unwrap()
                .timestamp(),
            1_761_442_200
        );
        let repeat = NaiveDate::from_ymd_opt(2021, 11, 7)
            .unwrap()
            .and_hms_opt(1, 30, 0)
            .unwrap();
        let early = native_legacy_local_to_instant(&eastern, &repeat).unwrap();
        assert_eq!((early.hour(), early.minute()), (5, 30));
        let gap = NaiveDate::from_ymd_opt(2025, 3, 30)
            .unwrap()
            .and_hms_nano_opt(2, 30, 0, 123_456_789)
            .unwrap();
        let bound = native_legacy_local_to_instant(&paris, &gap).unwrap();
        assert_eq!(
            (bound.timestamp(), bound.nanosecond()),
            (1_743_296_400, 123_456_789)
        );
        let apia = NativeSessionTimeZone::Named("Pacific/Apia".parse().unwrap());
        let skipped = NaiveDate::from_ymd_opt(2011, 12, 30)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        assert!(native_legacy_local_to_instant(&apia, &skipped).is_none());
        let wide = NaiveDate::from_ymd_opt(20_000, 1, 1)
            .unwrap()
            .and_hms_nano_opt(0, 0, 0, 123_456_789)
            .unwrap();
        let wide_instant = native_legacy_local_to_instant(&Utc, &wide).unwrap();
        assert_eq!(
            (wide_instant.year(), wide_instant.nanosecond()),
            (20_000, 123_456_789)
        );
        let fixed = FixedOffset::east_opt(3600).unwrap();
        assert!(
            std::panic::catch_unwind(|| native_legacy_local_to_instant(
                &fixed,
                &NaiveDateTime::MIN
            ))
            .is_err()
        );
    }
}
