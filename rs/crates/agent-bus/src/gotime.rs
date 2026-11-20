//! Go's `time.Time` on the bus wire: RFC 3339 with nanoseconds, zero value
//! `0001-01-01T00:00:00Z`.

use std::fmt;
use std::ops::{Add, Sub};
use std::time::Duration;

use chrono::{
    DateTime, Datelike, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeDelta, TimeZone,
    Timelike, Utc,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A Go `time.Time`: an instant with the offset it is formatted in.
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
        Self::zero()
    }
}

impl Time {
    /// Go's zero time, `0001-01-01T00:00:00Z`.
    pub fn zero() -> Self {
        Self(utc_offset().from_utc_datetime(&zero_naive()))
    }

    /// `time.Now()`: now, in the local zone's offset.
    pub fn now() -> Self {
        Self(Local::now().fixed_offset())
    }

    /// `time.Date(..., time.UTC)`.
    pub fn date_utc(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        min: u32,
        sec: u32,
        nsec: u32,
    ) -> Self {
        let naive = NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|d| d.and_hms_nano_opt(hour, min, sec, nsec))
            .expect("valid UTC date");
        Self(utc_offset().from_utc_datetime(&naive))
    }

    /// `t.IsZero()`.
    pub fn is_zero(&self) -> bool {
        self.0.naive_utc() == zero_naive()
    }

    /// `t.After(u)`.
    pub fn after(&self, other: &Self) -> bool {
        self > other
    }

    /// `t.Before(u)`.
    pub fn before(&self, other: &Self) -> bool {
        self < other
    }

    /// `t.Format(time.RFC3339Nano)`.
    pub fn format_rfc3339_nano(&self) -> String {
        use std::fmt::Write;
        let t = &self.0;
        let mut s = String::with_capacity(35);
        let year = t.year();
        if year < 0 {
            s.push('-');
        }
        let _ = write!(
            s,
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            year.unsigned_abs(),
            t.month(),
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        );
        let nanos = t.nanosecond() % 1_000_000_000;
        if nanos != 0 {
            let mut digits = format!("{nanos:09}");
            while digits.ends_with('0') {
                digits.pop();
            }
            s.push('.');
            s.push_str(&digits);
        }
        let offset = t.offset().local_minus_utc();
        if offset == 0 {
            s.push('Z');
        } else {
            let mut zone = offset / 60;
            if zone < 0 {
                s.push('-');
                zone = -zone;
            } else {
                s.push('+');
            }
            let _ = write!(s, "{:02}:{:02}", zone / 60, zone % 60);
        }
        s
    }

    /// `time.Parse(time.RFC3339, s)` (accepting fractional seconds).
    pub fn parse_rfc3339(s: &str) -> Result<Self, String> {
        parse_rfc3339_bytes(s.as_bytes()).ok_or_else(|| {
            format!("parsing time {s:?} as \"2006-01-02T15:04:05Z07:00\": cannot parse")
        })
    }
}

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

fn parse_rfc3339_bytes(s: &[u8]) -> Option<Time> {
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
    let t = tz.from_local_datetime(&naive).single()?;
    Some(Time(t))
}

impl From<DateTime<FixedOffset>> for Time {
    fn from(t: DateTime<FixedOffset>) -> Self {
        Self(t)
    }
}

impl From<Time> for DateTime<FixedOffset> {
    fn from(t: Time) -> Self {
        t.0
    }
}

impl From<DateTime<Utc>> for Time {
    fn from(t: DateTime<Utc>) -> Self {
        Self(t.fixed_offset())
    }
}

impl From<DateTime<Local>> for Time {
    fn from(t: DateTime<Local>) -> Self {
        Self(t.fixed_offset())
    }
}

impl Add<TimeDelta> for Time {
    type Output = Self;
    fn add(self, d: TimeDelta) -> Self {
        Self(self.0 + d)
    }
}

impl Sub<TimeDelta> for Time {
    type Output = Self;
    fn sub(self, d: TimeDelta) -> Self {
        Self(self.0 - d)
    }
}

impl Add<Duration> for Time {
    type Output = Self;
    fn add(self, d: Duration) -> Self {
        Self(self.0 + TimeDelta::from_std(d).unwrap_or(TimeDelta::MAX))
    }
}

impl Sub<Duration> for Time {
    type Output = Self;
    fn sub(self, d: Duration) -> Self {
        Self(self.0 - TimeDelta::from_std(d).unwrap_or(TimeDelta::MAX))
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

impl Serialize for Time {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.format_rfc3339_nano())
    }
}

impl<'de> Deserialize<'de> for Time {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s: Option<String> = Option::deserialize(d)?;
        match s {
            None => Ok(Self::zero()),
            Some(s) => Self::parse_rfc3339(&s).map_err(serde::de::Error::custom),
        }
    }
}
