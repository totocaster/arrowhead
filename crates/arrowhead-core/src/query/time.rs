use std::str::FromStr;

use anyhow::{Context, Result, bail};
use chrono::{
    DateTime, Datelike, Duration, Months, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc,
    Weekday,
};

/// Represents a concrete bound in a date range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateRangeBound {
    /// Instant expressed in UTC.
    pub value: DateTime<Utc>,
    /// Whether the value is included in the range (`>=`/`<=`) or exclusive (`>`/`<`).
    pub inclusive: bool,
}

/// Inclusive/exclusive range of timestamps used for filtering.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DateRange {
    /// Optional lower bound (filesystem microseconds).
    pub start: Option<DateRangeBound>,
    /// Optional upper bound (filesystem microseconds).
    pub end: Option<DateRangeBound>,
}

impl DateRange {
    /// Create a new range from optional lower/upper bounds.
    pub fn new(start: Option<DateRangeBound>, end: Option<DateRangeBound>) -> Self {
        Self { start, end }
    }

    /// Intersect this range with another, returning `None` if the intersection is empty.
    pub fn intersect(&self, other: &DateRange) -> Option<DateRange> {
        let start = match (&self.start, &other.start) {
            (Some(lhs), Some(rhs)) => Some(max_bound(lhs, rhs)),
            (Some(bound), None) | (None, Some(bound)) => Some(*bound),
            (None, None) => None,
        };

        let end = match (&self.end, &other.end) {
            (Some(lhs), Some(rhs)) => Some(min_bound(lhs, rhs)),
            (Some(bound), None) | (None, Some(bound)) => Some(*bound),
            (None, None) => None,
        };

        if let (Some(start), Some(end)) = (start, end) {
            if !bounds_overlap(&start, &end) {
                return None;
            }
        }

        Some(DateRange { start, end })
    }

    /// Return `true` when the range represents an empty interval.
    pub fn is_empty(&self) -> bool {
        if let (Some(start), Some(end)) = (&self.start, &self.end) {
            !bounds_overlap(start, end)
        } else {
            false
        }
    }

    /// Lower bound in microseconds since epoch with inclusivity handled.
    pub fn lower_bound_micros(&self) -> Option<i64> {
        self.start.map(|bound| bound_lower_micros(&bound) as i64)
    }

    /// Upper bound in microseconds since epoch with inclusivity handled.
    pub fn upper_bound_micros(&self) -> Option<i64> {
        self.end.map(|bound| bound_upper_micros(&bound) as i64)
    }
}

fn bounds_overlap(start: &DateRangeBound, end: &DateRangeBound) -> bool {
    let start_micros = bound_lower_micros(start);
    let end_micros = bound_upper_micros(end);
    start_micros <= end_micros
}

fn bound_lower_micros(bound: &DateRangeBound) -> i128 {
    let base = bound.value.timestamp_micros() as i128;
    if bound.inclusive { base } else { base + 1 }
}

fn bound_upper_micros(bound: &DateRangeBound) -> i128 {
    let base = bound.value.timestamp_micros() as i128;
    if bound.inclusive { base } else { base - 1 }
}

fn max_bound(a: &DateRangeBound, b: &DateRangeBound) -> DateRangeBound {
    match a.value.cmp(&b.value) {
        std::cmp::Ordering::Greater => *a,
        std::cmp::Ordering::Less => *b,
        std::cmp::Ordering::Equal => DateRangeBound {
            value: a.value,
            inclusive: a.inclusive && b.inclusive,
        },
    }
}

fn min_bound(a: &DateRangeBound, b: &DateRangeBound) -> DateRangeBound {
    match a.value.cmp(&b.value) {
        std::cmp::Ordering::Greater => *b,
        std::cmp::Ordering::Less => *a,
        std::cmp::Ordering::Equal => DateRangeBound {
            value: a.value,
            inclusive: a.inclusive && b.inclusive,
        },
    }
}

/// Represents a parsed date literal including whether time information was present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedDate {
    pub instant: DateTime<Utc>,
    pub has_time: bool,
}

impl ParsedDate {
    pub fn with_time(instant: DateTime<Utc>) -> Self {
        Self {
            instant,
            has_time: true,
        }
    }

    pub fn date_only(instant: DateTime<Utc>) -> Self {
        Self {
            instant,
            has_time: false,
        }
    }
}

/// Attempt to parse an absolute date/datetime literal.
pub fn parse_absolute_date(value: &str) -> Result<ParsedDate> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Ok(ParsedDate::with_time(dt.with_timezone(&Utc)));
    }

    if let Ok(naive) = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S") {
        return Ok(ParsedDate::with_time(Utc.from_utc_datetime(&naive)));
    }

    if let Ok(naive) = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M") {
        return Ok(ParsedDate::with_time(Utc.from_utc_datetime(&naive)));
    }

    if let Ok(naive) = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S") {
        return Ok(ParsedDate::with_time(Utc.from_utc_datetime(&naive)));
    }

    if let Ok(naive) = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M") {
        return Ok(ParsedDate::with_time(Utc.from_utc_datetime(&naive)));
    }

    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        let instant = date
            .and_hms_opt(0, 0, 0)
            .context("invalid midnight value")?;
        return Ok(ParsedDate::date_only(Utc.from_utc_datetime(&instant)));
    }

    bail!("invalid date literal `{value}`")
}

/// Parse a relative-date shorthand (e.g. `past7d`, `next2w`).
pub fn parse_relative_range(value: &str, now: DateTime<Utc>) -> Result<Option<DateRange>> {
    let lowered = value.trim().to_ascii_lowercase();

    if let Some(named) = parse_named_range(&lowered, now)? {
        return Ok(Some(named));
    }

    if !(lowered.starts_with("past") || lowered.starts_with("next")) {
        return Ok(None);
    }

    let (direction, rest) = if let Some(rest) = lowered.strip_prefix("past") {
        ("past", rest)
    } else if let Some(rest) = lowered.strip_prefix("next") {
        ("next", rest)
    } else {
        return Ok(None);
    };

    if rest.is_empty() {
        bail!("relative date `{value}` missing length component");
    }

    let mut digits = String::new();
    let mut chars = rest.chars();
    while let Some(ch) = chars.next() {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            let unit = ch;
            let length = u32::from_str(&digits)
                .with_context(|| format!("relative date `{value}` missing numeric duration"))?;
            if length == 0 {
                bail!("relative date `{value}` must be at least 1");
            }
            return relative_range(direction, length, unit, now);
        }
    }

    bail!("relative date `{value}` missing unit suffix (d/w/m)")
}

fn relative_range(
    direction: &str,
    length: u32,
    unit: char,
    now: DateTime<Utc>,
) -> Result<Option<DateRange>> {
    let (start, end) = match (direction, unit) {
        ("past", 'd') => (now - Duration::days(length as i64), now),
        ("past", 'w') => (now - Duration::weeks(length as i64), now),
        ("past", 'm') => {
            let months = Months::new(length);
            let start = now
                .checked_sub_months(months)
                .context("relative date month subtraction overflow")?;
            (start, now)
        }
        ("next", 'd') => (now, now + Duration::days(length as i64)),
        ("next", 'w') => (now, now + Duration::weeks(length as i64)),
        ("next", 'm') => {
            let months = Months::new(length);
            let end = now
                .checked_add_months(months)
                .context("relative date month addition overflow")?;
            (now, end)
        }
        _ => bail!("relative date unit `{unit}` not supported (use d, w, or m)"),
    };

    Ok(Some(DateRange::new(
        Some(DateRangeBound {
            value: start,
            inclusive: true,
        }),
        Some(DateRangeBound {
            value: end,
            inclusive: true,
        }),
    )))
}

/// Create a range covering the entire day if `parsed` does not include time data.
pub fn range_from_parsed_date(parsed: ParsedDate) -> DateRange {
    if parsed.has_time {
        DateRange::new(
            Some(DateRangeBound {
                value: parsed.instant,
                inclusive: true,
            }),
            Some(DateRangeBound {
                value: parsed.instant,
                inclusive: true,
            }),
        )
    } else {
        let start = parsed.instant;
        let end = parsed
            .instant
            .checked_add_signed(Duration::days(1))
            .unwrap_or(parsed.instant)
            .checked_sub_signed(Duration::microseconds(1))
            .unwrap_or(parsed.instant);

        DateRange::new(
            Some(DateRangeBound {
                value: start,
                inclusive: true,
            }),
            Some(DateRangeBound {
                value: end,
                inclusive: true,
            }),
        )
    }
}

/// Build a range with only a lower bound.
pub fn range_from_lower(bound: DateRangeBound) -> DateRange {
    DateRange::new(Some(bound), None)
}

/// Build a range with only an upper bound.
pub fn range_from_upper(bound: DateRangeBound) -> DateRange {
    DateRange::new(None, Some(bound))
}

fn parse_named_range(name: &str, now: DateTime<Utc>) -> Result<Option<DateRange>> {
    let range = match name {
        "today" => Some(day_range(start_of_day(now))),
        "yesterday" => {
            let start = start_of_day(now - Duration::days(1));
            Some(day_range(start))
        }
        "thisweek" => Some(week_range(start_of_week(now))),
        "lastweek" => {
            let start = start_of_week(now) - Duration::weeks(1);
            Some(week_range(start))
        }
        "thismonth" => Some(month_range(start_of_month(now))),
        "lastmonth" => Some(month_range(start_of_prev_month(now))),
        _ => None,
    };
    Ok(range)
}

fn day_range(start: DateTime<Utc>) -> DateRange {
    let end = end_of_day(start);
    DateRange::new(
        Some(DateRangeBound {
            value: start,
            inclusive: true,
        }),
        Some(DateRangeBound {
            value: end,
            inclusive: true,
        }),
    )
}

fn week_range(start: DateTime<Utc>) -> DateRange {
    let end = start + Duration::weeks(1) - Duration::microseconds(1);
    DateRange::new(
        Some(DateRangeBound {
            value: start,
            inclusive: true,
        }),
        Some(DateRangeBound {
            value: end,
            inclusive: true,
        }),
    )
}

fn month_range(start: DateTime<Utc>) -> DateRange {
    let next_month = add_months_clamped(start, 1);
    let end = next_month - Duration::microseconds(1);
    DateRange::new(
        Some(DateRangeBound {
            value: start,
            inclusive: true,
        }),
        Some(DateRangeBound {
            value: end,
            inclusive: true,
        }),
    )
}

fn start_of_day(ts: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(ts.year(), ts.month(), ts.day(), 0, 0, 0)
        .single()
        .expect("valid start of day")
}

fn end_of_day(ts: DateTime<Utc>) -> DateTime<Utc> {
    start_of_day(ts) + Duration::days(1) - Duration::microseconds(1)
}

fn start_of_week(ts: DateTime<Utc>) -> DateTime<Utc> {
    let weekday = ts.weekday();
    let days_from_monday = match weekday {
        Weekday::Mon => 0,
        Weekday::Tue => 1,
        Weekday::Wed => 2,
        Weekday::Thu => 3,
        Weekday::Fri => 4,
        Weekday::Sat => 5,
        Weekday::Sun => 6,
    };
    start_of_day(ts - Duration::days(days_from_monday))
}

fn start_of_month(ts: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(ts.year(), ts.month(), 1, 0, 0, 0)
        .single()
        .expect("valid month start")
}

fn start_of_prev_month(ts: DateTime<Utc>) -> DateTime<Utc> {
    add_months_clamped(start_of_month(ts), -1)
}

fn add_months_clamped(ts: DateTime<Utc>, months: i32) -> DateTime<Utc> {
    let naive = ts.naive_utc();
    let mut month = naive.month() as i32 + months;
    let mut year = naive.year();
    while month <= 0 {
        month += 12;
        year -= 1;
    }
    while month > 12 {
        month -= 12;
        year += 1;
    }
    let day = naive.day().min(days_in_month(year, month as u32));
    Utc.with_ymd_and_hms(
        year,
        month as u32,
        day,
        naive.hour(),
        naive.minute(),
        naive.second(),
    )
    .single()
    .expect("valid adjusted month")
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_absolute_date_variants() {
        assert!(parse_absolute_date("2024-01-10").is_ok());
        assert!(parse_absolute_date("2024-01-10T12:30").is_ok());
        assert!(parse_absolute_date("2024-01-10T12:30:45Z").is_ok());
        assert!(parse_absolute_date("2024-01-10 12:30:45").is_ok());
    }

    #[test]
    fn relative_range_past_days() {
        let now = Utc.with_ymd_and_hms(2024, 5, 10, 12, 0, 0).unwrap();
        let range = parse_relative_range("past7d", now).unwrap().unwrap();
        assert_eq!(
            range.start.unwrap().value,
            Utc.with_ymd_and_hms(2024, 5, 3, 12, 0, 0).unwrap()
        );
        assert_eq!(range.end.unwrap().value, now);
    }

    #[test]
    fn relative_range_next_weeks() {
        let now = Utc.with_ymd_and_hms(2024, 5, 10, 12, 0, 0).unwrap();
        let range = parse_relative_range("next2w", now).unwrap().unwrap();
        assert_eq!(range.start.unwrap().value, now);
        assert_eq!(
            range.end.unwrap().value,
            Utc.with_ymd_and_hms(2024, 5, 24, 12, 0, 0).unwrap()
        );
    }

    #[test]
    fn named_relative_ranges_supported() {
        let now = Utc.with_ymd_and_hms(2024, 5, 15, 9, 30, 0).unwrap();
        let today = parse_relative_range("today", now).unwrap().unwrap();
        assert_eq!(
            today.start.unwrap().value,
            Utc.with_ymd_and_hms(2024, 5, 15, 0, 0, 0).unwrap()
        );
        let yesterday = parse_relative_range("yesterday", now).unwrap().unwrap();
        assert_eq!(
            yesterday.start.unwrap().value,
            Utc.with_ymd_and_hms(2024, 5, 14, 0, 0, 0).unwrap()
        );
        let this_week = parse_relative_range("thisweek", now).unwrap().unwrap();
        assert_eq!(
            this_week.start.unwrap().value,
            Utc.with_ymd_and_hms(2024, 5, 13, 0, 0, 0).unwrap()
        );
        let last_month = parse_relative_range("lastmonth", now).unwrap().unwrap();
        assert_eq!(
            last_month.start.unwrap().value,
            Utc.with_ymd_and_hms(2024, 4, 1, 0, 0, 0).unwrap()
        );
    }
}
