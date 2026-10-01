// Copyright 2018 TiKV Project Authors. Licensed under Apache-2.0.

use chrono::Weekday;

use super::{Time, weekmode::WeekMode};

pub trait WeekdayExtension {
    fn name(&self) -> &'static str;
    fn name_abbr(&self) -> &'static str;
}

impl WeekdayExtension for Weekday {
    fn name(&self) -> &'static str {
        Time::weekday_name_from_sunday_index(self.num_days_from_sunday())
    }

    fn name_abbr(&self) -> &'static str {
        match *self {
            Weekday::Mon => "Mon",
            Weekday::Tue => "Tue",
            Weekday::Wed => "Wed",
            Weekday::Thu => "Thu",
            Weekday::Fri => "Fri",
            Weekday::Sat => "Sat",
            Weekday::Sun => "Sun",
        }
    }
}

pub trait DateTimeExtension {
    fn days(&self) -> i32;
    fn calc_year_week(
        &self,
        monday_first: bool,
        week_year: bool,
        first_weekday: bool,
    ) -> (i32, i32);
    fn calc_year_week_by_week_mode(&self, week_mode: WeekMode) -> (i32, i32);
    fn week(&self, mode: WeekMode) -> i32;
    fn year_week(&self, mode: WeekMode) -> (i32, i32);
    fn abbr_day_of_month(&self) -> &'static str;
    fn day_number(&self) -> i32;
    fn second_number(&self) -> i64;
}

impl DateTimeExtension for Time {
    /// returns the day of year starting from 1.
    /// implements TiDB YearDay().
    fn days(&self) -> i32 {
        self.ordinal()
    }

    /// returns the week of year and year. should not be called directly.
    /// - when monday_first == true, Monday is considered as the first day in
    ///   the week, otherwise Sunday.
    /// - when week_year == true, week is from 1 to 53, otherwise from 0 to 53.
    /// - when first_weekday == true, the week that contains the first
    ///   'first-day-of-week' is week 1, otherwise weeks are numbered according
    ///   to ISO 8601:1988.
    fn calc_year_week(
        &self,
        monday_first: bool,
        week_year: bool,
        first_weekday: bool,
    ) -> (i32, i32) {
        Time::native_calc_week_i32(
            self.year() as i32,
            self.month() as i32,
            self.day() as i32,
            monday_first,
            week_year,
            first_weekday,
        )
    }

    /// returns the week of year according to week mode. should not be called
    /// directly. implements TiDB calcWeek()
    fn calc_year_week_by_week_mode(&self, week_mode: WeekMode) -> (i32, i32) {
        let mode = week_mode.to_normalized();
        let monday_first = mode.contains(WeekMode::BEHAVIOR_MONDAY_FIRST);
        let week_year = mode.contains(WeekMode::BEHAVIOR_YEAR);
        let first_weekday = mode.contains(WeekMode::BEHAVIOR_FIRST_WEEKDAY);
        self.calc_year_week(monday_first, week_year, first_weekday)
    }

    /// returns the week of year.
    /// implements TiDB Week().
    fn week(&self, mode: WeekMode) -> i32 {
        if self.month() == 0 || self.day() == 0 {
            return 0;
        }
        let (_, week) = self.calc_year_week_by_week_mode(mode);
        week
    }

    /// returns the week of year and year.
    /// implements TiDB YearWeek().
    fn year_week(&self, mode: WeekMode) -> (i32, i32) {
        self.calc_year_week_by_week_mode(mode | WeekMode::BEHAVIOR_YEAR)
    }

    /// returns the abbreviation of the day of month.
    fn abbr_day_of_month(&self) -> &'static str {
        match self.day() {
            1 | 21 | 31 => "st",
            2 | 22 => "nd",
            3 | 23 => "rd",
            _ => "th",
        }
    }

    /// returns the days since 0000-00-00
    fn day_number(&self) -> i32 {
        calc_day_number(self.year() as i32, self.month() as i32, self.day() as i32)
    }

    /// returns the seconds since 0000-00-00 00:00:00
    fn second_number(&self) -> i64 {
        let days = self.day_number();
        days as i64 * 86400
            + self.hour() as i64 * 3600
            + self.minute() as i64 * 60
            + self.second() as i64
    }
}

// calculates days since 0000-00-00.
fn calc_day_number(year: i32, month: i32, day: i32) -> i32 {
    Time::native_calc_daynr_i32(year, month, day)
}
