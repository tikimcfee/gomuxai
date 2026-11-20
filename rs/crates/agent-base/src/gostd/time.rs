//! Go's `time.Duration` text: `String` ("1h30m0s", "1.5s", "250ms") and
//! `ParseDuration`, which flags, messages and help text use.
//!
//! A Go duration is signed nanoseconds in an i64; the `_nanos` functions keep
//! that, and the others take or give a `std::time::Duration`.

use std::time::Duration;

/// Go's `time.Duration.String` for `d`, saturating at Go's largest duration.
pub fn format_duration(d: Duration) -> String {
    format_duration_nanos(i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// Go's `time.Duration.String` for a signed count of nanoseconds, e.g.
/// "72h3m0.5s", "1.5µs", "0s".
pub fn format_duration_nanos(d: i64) -> String {
    let neg = d < 0;
    let mut u = d.unsigned_abs();
    // Built backwards, as Go does.
    let mut buf: Vec<char> = Vec::with_capacity(32);
    if u < 1_000_000_000 {
        buf.push('s');
        let prec;
        if u == 0 {
            return "0s".to_string();
        } else if u < 1_000 {
            prec = 0;
            buf.push('n');
        } else if u < 1_000_000 {
            prec = 3;
            buf.push('µ');
        } else {
            prec = 6;
            buf.push('m');
        }
        u = fmt_frac(&mut buf, u, prec);
        fmt_int(&mut buf, u);
    } else {
        buf.push('s');
        u = fmt_frac(&mut buf, u, 9);
        fmt_int(&mut buf, u % 60);
        u /= 60;
        if u > 0 {
            buf.push('m');
            fmt_int(&mut buf, u % 60);
            u /= 60;
            if u > 0 {
                buf.push('h');
                fmt_int(&mut buf, u);
            }
        }
    }
    if neg {
        buf.push('-');
    }
    buf.iter().rev().collect()
}

// fmt_frac pushes (backwards) the fraction of v/10**prec, e.g. ".12345",
// omitting trailing zeros, and the point too when the fraction is 0. It
// returns v/10**prec.
fn fmt_frac(buf: &mut Vec<char>, mut v: u64, prec: u32) -> u64 {
    let mut print = false;
    for _ in 0..prec {
        let digit = v % 10;
        print = print || digit != 0;
        if print {
            buf.push(char::from(b'0' + digit as u8));
        }
        v /= 10;
    }
    if print {
        buf.push('.');
    }
    v
}

fn fmt_int(buf: &mut Vec<char>, mut v: u64) {
    if v == 0 {
        buf.push('0');
        return;
    }
    while v > 0 {
        buf.push(char::from(b'0' + (v % 10) as u8));
        v /= 10;
    }
}

/// Go's `time.ParseDuration`, as a `std::time::Duration`. A negative duration
/// ("-5s"), which Go returns as is, comes back as zero: every Go caller treats
/// a duration `<= 0` the same way. `parse_duration_nanos` keeps the sign.
pub fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    let n = parse_duration_nanos(s)?;
    Ok(Duration::from_nanos(n.max(0) as u64))
}

/// Go's `time.ParseDuration`: a possibly signed sequence of decimal numbers,
/// each with an optional fraction and a unit suffix ("300ms", "-1.5h",
/// "2h45m"); units are ns, us (or µs), ms, s, m, h. Errors keep Go's words:
/// `time: invalid duration "x"`, `time: missing unit in duration "5"`,
/// `time: unknown unit "d" in duration "5d"`.
pub fn parse_duration_nanos(s: &str) -> anyhow::Result<i64> {
    // [-+]?([0-9]*(\.[0-9]*)?[a-z]+)+
    let orig = s;
    let invalid = || anyhow::anyhow!("time: invalid duration {}", time_quote(orig));
    let mut s = s.as_bytes();
    let mut d: u64 = 0;
    let mut neg = false;
    if let Some(&c) = s.first()
        && (c == b'-' || c == b'+')
    {
        neg = c == b'-';
        s = &s[1..];
    }
    if s == b"0" {
        return Ok(0);
    }
    if s.is_empty() {
        return Err(invalid());
    }
    while !s.is_empty() {
        // The next character must be [0-9.].
        if !(s[0] == b'.' || s[0].is_ascii_digit()) {
            return Err(invalid());
        }
        let pl = s.len();
        let (v, rest) = leading_int(s).ok_or_else(invalid)?;
        let mut v = v;
        s = rest;
        let pre = pl != s.len(); // whether we consumed anything before a period
        let mut post = false;
        let mut f: u64 = 0;
        let mut scale: f64 = 1.0;
        if s.first() == Some(&b'.') {
            s = &s[1..];
            let pl = s.len();
            let (ff, sc, rest) = leading_fraction(s);
            f = ff;
            scale = sc;
            s = rest;
            post = pl != s.len();
        }
        if !pre && !post {
            // no digits (e.g. ".s" or "-.s")
            return Err(invalid());
        }
        // Consume the unit.
        let i = s
            .iter()
            .position(|&c| c == b'.' || c.is_ascii_digit())
            .unwrap_or(s.len());
        if i == 0 {
            return Err(anyhow::anyhow!(
                "time: missing unit in duration {}",
                time_quote(orig)
            ));
        }
        let u = &s[..i];
        s = &s[i..];
        let unit: u64 = match u {
            b"ns" => 1,
            b"us" => 1_000,
            b"\xc2\xb5s" => 1_000, // U+00B5 micro sign
            b"\xce\xbcs" => 1_000, // U+03BC Greek mu
            b"ms" => 1_000_000,
            b"s" => 1_000_000_000,
            b"m" => 60 * 1_000_000_000,
            b"h" => 3600 * 1_000_000_000,
            _ => {
                return Err(anyhow::anyhow!(
                    "time: unknown unit {} in duration {}",
                    time_quote(&String::from_utf8_lossy(u)),
                    time_quote(orig)
                ));
            }
        };
        if v > (1u64 << 63) / unit {
            // overflow
            return Err(invalid());
        }
        v *= unit;
        if f > 0 {
            // f64 is needed to be nanosecond accurate for fractions of hours.
            // v >= 0 && (f*unit/scale) <= 3.6e+12 (ns/h, h is the largest unit)
            v += (f as f64 * (unit as f64 / scale)) as u64;
            if v > 1u64 << 63 {
                return Err(invalid());
            }
        }
        d += v;
        if d > 1u64 << 63 {
            return Err(invalid());
        }
    }
    if neg {
        return Ok((d as i64).wrapping_neg());
    }
    if d > (1u64 << 63) - 1 {
        return Err(invalid());
    }
    Ok(d as i64)
}

// leading_int consumes the leading [0-9]* from s; None on overflow.
fn leading_int(s: &[u8]) -> Option<(u64, &[u8])> {
    let mut i = 0;
    let mut x: u64 = 0;
    while i < s.len() && s[i].is_ascii_digit() {
        if x > (1u64 << 63) / 10 {
            return None;
        }
        x = x * 10 + u64::from(s[i] - b'0');
        if x > 1u64 << 63 {
            return None;
        }
        i += 1;
    }
    Some((x, &s[i..]))
}

// leading_fraction consumes the leading [0-9]* from s. It is used only for
// fractions, so it does not return an error on overflow: it stops
// accumulating precision.
fn leading_fraction(s: &[u8]) -> (u64, f64, &[u8]) {
    let mut i = 0;
    let mut x: u64 = 0;
    let mut scale = 1.0;
    let mut overflow = false;
    while i < s.len() && s[i].is_ascii_digit() {
        if overflow {
            i += 1;
            continue;
        }
        if x > ((1u64 << 63) - 1) / 10 {
            // It's possible for overflow to give a positive number, so take care.
            overflow = true;
            i += 1;
            continue;
        }
        let y = x * 10 + u64::from(s[i] - b'0');
        if y > 1u64 << 63 {
            overflow = true;
            i += 1;
            continue;
        }
        x = y;
        scale *= 10.0;
        i += 1;
    }
    (x, scale, &s[i..])
}

// time_quote is the time package's own quoting, simpler than strconv.Quote:
// non-ASCII and control bytes become \xNN.
fn time_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        if (c as u32) >= 0x80 || (c as u32) < 0x20 {
            let mut b = [0u8; 4];
            for byte in c.encode_utf8(&mut b).bytes() {
                out.push_str(&format!("\\x{byte:02x}"));
            }
        } else {
            if c == '"' || c == '\\' {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_matches_go() {
        for (d, want) in [
            (0i64, "0s"),
            (1, "1ns"),
            (1_100, "1.1µs"),
            (2_200_000, "2.2ms"),
            (3_300_000_000, "3.3s"),
            (4 * 60 * 1_000_000_000 + 5_000_000_000, "4m5s"),
            (4 * 60 * 1_000_000_000 + 5_001_000_000, "4m5.001s"),
            (
                5 * 3600 * 1_000_000_000 + 6 * 60 * 1_000_000_000 + 7_001_000_000,
                "5h6m7.001s",
            ),
            (8 * 60 * 1_000_000_000 + 1, "8m0.000000001s"),
            (30 * 60 * 1_000_000_000, "30m0s"),
            (15_000_000_000, "15s"),
            (-1, "-1ns"),
            (i64::MAX, "2562047h47m16.854775807s"),
            (i64::MIN, "-2562047h47m16.854775808s"),
        ] {
            assert_eq!(format_duration_nanos(d), want, "format({d})");
        }
        assert_eq!(format_duration(Duration::from_millis(30)), "30ms");
    }

    #[test]
    fn parse_matches_go() {
        for (s, want) in [
            ("0", 0i64),
            ("5s", 5_000_000_000),
            ("30s", 30_000_000_000),
            ("1478s", 1_478_000_000_000),
            ("-5s", -5_000_000_000),
            ("+5s", 5_000_000_000),
            ("-0", 0),
            ("+0", 0),
            ("5.0s", 5_000_000_000),
            ("5.6s", 5_600_000_000),
            ("5.s", 5_000_000_000),
            (".5s", 500_000_000),
            ("1.004s", 1_004_000_000),
            ("100.00100s", 100_001_000_000),
            ("10ns", 10),
            ("11us", 11_000),
            ("12µs", 12_000),
            ("12μs", 12_000),
            ("13ms", 13_000_000),
            ("14s", 14_000_000_000),
            ("15m", 900_000_000_000),
            ("16h", 57_600_000_000_000),
            ("3h30m", 12_600_000_000_000),
            ("10.5s4m", 250_500_000_000),
            ("-2m3.4s", -123_400_000_000),
            ("1h2m3s4ms5us6ns", 3_723_004_005_006),
            ("39h9m14.425s", 140_954_425_000_000),
            ("52763797000ns", 52_763_797_000),
            ("0.3333333333333333333h", 1_200_000_000_000),
            ("9007199254740993ns", 9_007_199_254_740_993),
            ("9223372036854775807ns", i64::MAX),
            ("-9223372036854775808ns", i64::MIN),
            ("0.100000000000000000000h", 360_000_000_000),
            ("0.830103483285477580700h", 2_988_372_539_827),
        ] {
            assert_eq!(parse_duration_nanos(s).unwrap(), want, "parse({s:?})");
        }
        for (s, msg) in [
            ("", r#"time: invalid duration """#),
            ("3", r#"time: missing unit in duration "3""#),
            ("-", r#"time: invalid duration "-""#),
            ("s", r#"time: invalid duration "s""#),
            (".", r#"time: invalid duration ".""#),
            ("-.", r#"time: invalid duration "-.""#),
            (".s", r#"time: invalid duration ".s""#),
            ("+.s", r#"time: invalid duration "+.s""#),
            ("1d", r#"time: unknown unit "d" in duration "1d""#),
            (
                "\u{85}\u{85}",
                r#"time: invalid duration "\xc2\x85\xc2\x85""#,
            ),
            (
                "9223372036854775810ns",
                r#"time: invalid duration "9223372036854775810ns""#,
            ),
            (
                "9223372036854775808ns",
                r#"time: invalid duration "9223372036854775808ns""#,
            ),
        ] {
            assert_eq!(
                parse_duration_nanos(s).unwrap_err().to_string(),
                msg,
                "parse({s:?})"
            );
        }
        assert_eq!(parse_duration("-5s").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
    }
}
