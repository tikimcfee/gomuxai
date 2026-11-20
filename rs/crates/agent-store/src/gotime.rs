//! Go's `time.Time` as the store reads and writes it.
//!
//! Every timestamp in the database is text in one of Go's layouts, and a Go
//! binary must read what a Rust one wrote (and the reverse) during rollout, so
//! the formatting and parsing here follow Go's `time` package exactly:
//!
//! - `format_rfc3339`: `time.RFC3339`, whole seconds, in the time's own offset
//!   (`Z` when the offset is zero, as Go writes it whatever the zone).
//! - `format_rfc3339_nano`: `time.RFC3339Nano`, nanoseconds with trailing zeros
//!   (and a bare `.`) dropped.
//! - `format_millis`: `"2006-01-02T15:04:05.000Z07:00"`, milliseconds always
//!   (truncated, not rounded).
//! - `parse_rfc3339`: `time.Parse(time.RFC3339, s)`, which also takes any number
//!   of fractional digits.
//!
//! `Time::default()` is Go's zero time (0001-01-01T00:00:00Z), which the store
//! uses for "not set" (a run's end, a friction entry's acceptance); chrono's own
//! default is the Unix epoch, which is why this is a type and not an alias.

use std::fmt;
use std::ops::{Add, Sub};

use chrono::{
    DateTime, Datelike, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeDelta, TimeZone,
    Timelike, Utc,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A Go `time.Time`: an instant with the offset it is shown in. Equality and
/// ordering compare instants, like Go's `Equal`, `Before` and `After`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time(pub DateTime<FixedOffset>);

fn zero_naive() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(1, 1, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .expect("year 1 is a valid date")
}

fn utc_offset() -> FixedOffset {
    FixedOffset::east_opt(0).expect("zero offset")
}

impl Default for Time {
    fn default() -> Self {
        Time::zero()
    }
}

impl Time {
    /// Go's zero time, 0001-01-01T00:00:00Z.
    pub fn zero() -> Time {
        Time(utc_offset().from_utc_datetime(&zero_naive()))
    }

    /// `time.Now()`: now, in the local zone's offset.
    pub fn now() -> Time {
        Time(Local::now().fixed_offset())
    }

    /// `time.Date(year, month, day, hour, min, sec, nsec, time.UTC)`. Panics on an
    /// invalid date (Go normalizes; callers here pass literal dates).
    pub fn date_utc(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        min: u32,
        sec: u32,
        nsec: u32,
    ) -> Time {
        let naive = NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|d| d.and_hms_nano_opt(hour, min, sec, nsec))
            .expect("a valid date");
        Time(utc_offset().from_utc_datetime(&naive))
    }

    /// `t.IsZero()`.
    pub fn is_zero(&self) -> bool {
        self.0.naive_utc() == zero_naive()
    }

    /// `t.UTC()`.
    pub fn utc(&self) -> Time {
        Time(self.0.with_timezone(&utc_offset()))
    }

    /// `t.Local()`.
    pub fn local(&self) -> Time {
        Time(self.0.with_timezone(&Local).fixed_offset())
    }

    /// `t.Equal(u)`.
    pub fn equal(&self, other: &Time) -> bool {
        self == other
    }

    /// `t.Before(u)`.
    pub fn before(&self, other: &Time) -> bool {
        self < other
    }

    /// `t.After(u)`.
    pub fn after(&self, other: &Time) -> bool {
        self > other
    }

    /// `t.Truncate(time.Second)` for the common case (Go truncates on absolute
    /// time since year 1, which for whole seconds is the same).
    pub fn truncate_seconds(&self) -> Time {
        Time(self.0.with_nanosecond(0).unwrap_or(self.0))
    }

    /// `t.Format(time.RFC3339)`.
    pub fn format_rfc3339(&self) -> String {
        self.format(Frac::None)
    }

    /// `t.Format(time.RFC3339Nano)`.
    pub fn format_rfc3339_nano(&self) -> String {
        self.format(Frac::Nano)
    }

    /// `t.Format("2006-01-02T15:04:05.000Z07:00")`.
    pub fn format_millis(&self) -> String {
        self.format(Frac::Millis)
    }

    fn format(&self, frac: Frac) -> String {
        let t = &self.0;
        let mut s = String::with_capacity(35);
        let year = t.year();
        if year < 0 {
            s.push('-');
        }
        push_padded(&mut s, year.unsigned_abs() as u64, 4);
        s.push('-');
        push_padded(&mut s, t.month() as u64, 2);
        s.push('-');
        push_padded(&mut s, t.day() as u64, 2);
        s.push('T');
        push_padded(&mut s, t.hour() as u64, 2);
        s.push(':');
        push_padded(&mut s, t.minute() as u64, 2);
        s.push(':');
        push_padded(&mut s, t.second() as u64, 2);
        // A leap second shows as nanoseconds >= 1e9 in chrono; Go has none.
        let nanos = (t.nanosecond() % 1_000_000_000) as u64;
        match frac {
            Frac::None => {}
            Frac::Millis => {
                s.push('.');
                push_padded(&mut s, nanos / 1_000_000, 3);
            }
            Frac::Nano => {
                if nanos != 0 {
                    let mut digits = format!("{nanos:09}");
                    while digits.ends_with('0') {
                        digits.pop();
                    }
                    s.push('.');
                    s.push_str(&digits);
                }
            }
        }
        let offset = t.offset().local_minus_utc();
        if offset == 0 {
            s.push('Z');
        } else {
            let mut zone = offset / 60; // minutes
            if zone < 0 {
                s.push('-');
                zone = -zone;
            } else {
                s.push('+');
            }
            push_padded(&mut s, (zone / 60) as u64, 2);
            s.push(':');
            push_padded(&mut s, (zone % 60) as u64, 2);
        }
        s
    }

    /// `time.Parse(time.RFC3339, s)`: Go's RFC 3339 parser, which takes any
    /// number of fractional digits (keeping nine) and `Z` or `±hh:mm`.
    pub fn parse_rfc3339(s: &str) -> Result<Time, ParseError> {
        parse_rfc3339(s.as_bytes()).ok_or_else(|| ParseError(s.to_string()))
    }

    /// `parseTime` in the Go store: a stored timestamp, or the zero time for
    /// anything that doesn't parse (an unset `""` among them).
    pub fn parse_or_zero(s: &str) -> Time {
        Time::parse_rfc3339(s).unwrap_or_default()
    }

    /// Nanoseconds since the Unix epoch (`t.UnixNano()`).
    pub fn unix_nano(&self) -> i64 {
        self.0.timestamp_nanos_opt().unwrap_or(i64::MIN)
    }
}

enum Frac {
    None,
    Millis,
    Nano,
}

fn push_padded(s: &mut String, v: u64, width: usize) {
    use std::fmt::Write;
    let _ = write!(s, "{v:0width$}");
}

/// A timestamp Go's RFC 3339 parser would refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "parsing time {:?} as \"2006-01-02T15:04:05Z07:00\": cannot parse",
            self.0
        )
    }
}

impl std::error::Error for ParseError {}

fn days_in(month: u32, year: i32) -> u32 {
    match month {
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Go's `parseRFC3339` (time/format_rfc3339.go), the path `time.Parse` takes
/// for the RFC3339 layouts.
fn parse_rfc3339(s: &[u8]) -> Option<Time> {
    let ok = std::cell::Cell::new(true);
    let parse_uint = |s: &[u8], min: i64, max: i64| -> i64 {
        let mut x: i64 = 0;
        for &c in s {
            if !c.is_ascii_digit() {
                ok.set(false);
                return min;
            }
            x = x * 10 + i64::from(c - b'0');
        }
        if x < min || max < x {
            ok.set(false);
            return min;
        }
        x
    };

    if s.len() < "2006-01-02T15:04:05".len() {
        return None;
    }
    let year = parse_uint(&s[0..4], 0, 9999);
    let month = parse_uint(&s[5..7], 1, 12);
    let day = parse_uint(&s[8..10], 1, i64::from(days_in(month as u32, year as i32)));
    let hour = parse_uint(&s[11..13], 0, 23);
    let min = parse_uint(&s[14..16], 0, 59);
    let sec = parse_uint(&s[17..19], 0, 59);
    if !ok.get()
        || !(s[4] == b'-' && s[7] == b'-' && s[10] == b'T' && s[13] == b':' && s[16] == b':')
    {
        return None;
    }
    let mut s = &s[19..];

    let mut nsec: u32 = 0;
    if s.len() >= 2 && s[0] == b'.' && s[1].is_ascii_digit() {
        let mut n = 2;
        while n < s.len() && s[n].is_ascii_digit() {
            n += 1;
        }
        // parseNanoseconds: at most nine digits count, scaled to nanoseconds.
        let digits = &s[1..n.min(10)];
        let mut ns: u32 = 0;
        for &c in digits {
            ns = ns * 10 + u32::from(c - b'0');
        }
        for _ in digits.len()..9 {
            ns *= 10;
        }
        nsec = ns;
        s = &s[n..];
    }

    let naive = NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32)?.and_hms_nano_opt(
        hour as u32,
        min as u32,
        sec as u32,
        nsec,
    )?;
    let mut offset = 0i32;
    if !(s.len() == 1 && s[0] == b'Z') {
        if s.len() != "-07:00".len() {
            return None;
        }
        let hr = parse_uint(&s[1..3], 0, 23);
        let mm = parse_uint(&s[4..6], 0, 59);
        if !ok.get() || !((s[0] == b'-' || s[0] == b'+') && s[3] == b':') {
            return None;
        }
        offset = ((hr * 60 + mm) * 60) as i32;
        if s[0] == b'-' {
            offset = -offset;
        }
    }
    let tz = FixedOffset::east_opt(offset)?;
    // The text is local time in that offset.
    let t = tz.from_local_datetime(&naive).single()?;
    Some(Time(t))
}

impl From<DateTime<FixedOffset>> for Time {
    fn from(t: DateTime<FixedOffset>) -> Self {
        Time(t)
    }
}

impl From<DateTime<Utc>> for Time {
    fn from(t: DateTime<Utc>) -> Self {
        Time(t.fixed_offset())
    }
}

impl From<DateTime<Local>> for Time {
    fn from(t: DateTime<Local>) -> Self {
        Time(t.fixed_offset())
    }
}

impl Add<TimeDelta> for Time {
    type Output = Time;
    fn add(self, d: TimeDelta) -> Time {
        Time(self.0 + d)
    }
}

impl Sub<TimeDelta> for Time {
    type Output = Time;
    fn sub(self, d: TimeDelta) -> Time {
        Time(self.0 - d)
    }
}

impl Sub<Time> for Time {
    type Output = TimeDelta;
    fn sub(self, other: Time) -> TimeDelta {
        self.0 - other.0
    }
}

impl fmt::Debug for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format_rfc3339_nano())
    }
}

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format_rfc3339_nano())
    }
}

/// As Go's `Time.MarshalJSON`: RFC 3339 with nanoseconds.
impl Serialize for Time {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.format_rfc3339_nano())
    }
}

/// As Go's `Time.UnmarshalJSON`: RFC 3339; `null` is the zero time.
impl<'de> Deserialize<'de> for Time {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s: Option<String> = Option::deserialize(d)?;
        match s {
            None => Ok(Time::zero()),
            Some(s) => Time::parse_rfc3339(&s).map_err(serde::de::Error::custom),
        }
    }
}

/// For `skip_serializing_if`: Go's `omitzero` on a time.
pub fn is_zero(t: &Time) -> bool {
    t.is_zero()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_go() {
        let t = Time::date_utc(2026, 10, 24, 14, 0, 0, 0);
        assert_eq!(t.format_rfc3339(), "2026-10-24T14:00:00Z");
        assert_eq!(t.format_rfc3339_nano(), "2026-10-24T14:00:00Z");
        assert_eq!(t.format_millis(), "2026-10-24T14:00:00.000Z");
        let t = Time::date_utc(2026, 10, 24, 14, 0, 5, 120_000_900);
        assert_eq!(t.format_rfc3339(), "2026-10-24T14:00:05Z");
        assert_eq!(t.format_rfc3339_nano(), "2026-10-24T14:00:05.1200009Z");
        assert_eq!(t.format_millis(), "2026-10-24T14:00:05.120Z");
        let t = Time::date_utc(2026, 10, 24, 14, 0, 5, 999_999_999);
        assert_eq!(t.format_millis(), "2026-10-24T14:00:05.999Z");
        let west = Time(t.0.with_timezone(&FixedOffset::west_opt(6 * 3600).unwrap()));
        assert_eq!(west.format_rfc3339(), "2026-10-24T08:00:05-06:00");
        let east = Time(t.0.with_timezone(&FixedOffset::east_opt(5 * 3600 + 1800).unwrap()));
        assert_eq!(
            east.format_rfc3339_nano(),
            "2026-10-24T19:30:05.999999999+05:30"
        );
        assert_eq!(Time::zero().format_rfc3339(), "0001-01-01T00:00:00Z");
        assert_eq!(west.utc().format_millis(), "2026-10-24T14:00:05.999Z");
    }

    #[test]
    fn parses_like_go() {
        let t = Time::parse_rfc3339("2026-10-24T08:00:05.5-06:00").unwrap();
        assert_eq!(t, Time::date_utc(2026, 10, 24, 14, 0, 5, 500_000_000));
        assert_eq!(t.format_rfc3339_nano(), "2026-10-24T08:00:05.5-06:00");
        let t = Time::parse_rfc3339("2026-10-24T14:00:05.1234567891234Z").unwrap();
        assert_eq!(t.0.nanosecond(), 123_456_789);
        for bad in [
            "",
            "x",
            "2026-10-24",
            "2026-10-24 14:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-10-24T14:00:00",
            "2026-10-24T14:00:00+0600",
            "2026-10-24T24:00:00Z",
        ] {
            assert!(Time::parse_rfc3339(bad).is_err(), "{bad:?} parsed");
            assert!(Time::parse_or_zero(bad).is_zero());
        }
        assert!(Time::parse_or_zero("0001-01-01T00:00:00Z").is_zero());
        assert!(Time::default().is_zero());
        assert!(!Time::now().is_zero());
    }

    #[test]
    fn json_round_trip() {
        let t = Time::date_utc(2026, 10, 24, 14, 0, 0, 1_000);
        let j = serde_json::to_string(&t).unwrap();
        assert_eq!(j, "\"2026-10-24T14:00:00.000001Z\"");
        let back: Time = serde_json::from_str(&j).unwrap();
        assert_eq!(back, t);
    }
}
