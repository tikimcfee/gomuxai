//! Memory limits for a job's scope, in systemd's size syntax.

use anyhow::{Result, bail};

/// The defaults, as percentages of physical RAM.
pub const DEFAULT_HIGH_PERCENT: u64 = 75;
pub const DEFAULT_MAX_PERCENT: u64 = 85;

/// Limits are the memory properties a job's scope gets, each as systemd
/// takes it ("24G", "75%", "infinity", bytes); "" leaves it unset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    pub memory_high: String,
    pub memory_max: String,
    pub memory_swap_max: String,
}

impl Limits {
    /// resolve checks the sizes given and fills in the defaults: MemoryMax
    /// 85% of RAM, MemoryHigh 75% (or, under a MemoryMax given alone, the
    /// same 75:85 of it), MemorySwapMax 0. Without the swap limit a job over
    /// MemoryMax is never killed on a machine with swap (zram here): it
    /// just pushes everything else out. ram is physical memory in bytes.
    pub fn resolve(
        high: Option<&str>,
        max: Option<&str>,
        swap_max: Option<&str>,
        ram: u64,
    ) -> Result<Limits> {
        let given = |flag: &str, v: Option<&str>| -> Result<Option<(String, u64)>> {
            match v {
                None => Ok(None),
                Some(v) => {
                    let n = parse_size(v, ram).map_err(|e| anyhow::anyhow!("{flag}: {e}"))?;
                    Ok(Some((v.to_string(), n)))
                }
            }
        };
        let max = given("--memory-max", max)?;
        let high = given("--memory-high", high)?;
        let swap = given("--memory-swap-max", swap_max)?;

        let default_max = ram / 100 * DEFAULT_MAX_PERCENT;
        let default_high = ram / 100 * DEFAULT_HIGH_PERCENT;
        let (max_text, max_bytes) = max.unwrap_or_else(|| (format_size(default_max), default_max));
        let (high_text, high_bytes) = match high {
            Some(h) => h,
            None if max_bytes == u64::MAX => (format_size(default_high), default_high),
            None => {
                let b = default_high.min(max_bytes / DEFAULT_MAX_PERCENT * DEFAULT_HIGH_PERCENT);
                (format_size(b), b)
            }
        };
        if high_bytes > max_bytes {
            bail!("--memory-high {high_text} is above --memory-max {max_text}");
        }
        Ok(Limits {
            memory_high: high_text,
            memory_max: max_text,
            memory_swap_max: swap.map(|(t, _)| t).unwrap_or_else(|| "0".to_string()),
        })
    }

    /// properties are the scope's `-p` values.
    pub fn properties(&self) -> Vec<String> {
        let mut p = Vec::new();
        for (name, v) in [
            ("MemoryHigh", &self.memory_high),
            ("MemoryMax", &self.memory_max),
            ("MemorySwapMax", &self.memory_swap_max),
        ] {
            if !v.is_empty() {
                p.push(format!("{name}={v}"));
            }
        }
        p
    }
}

/// parse_size reads a size as systemd's MemoryMax= takes it: bytes, or a
/// number (decimals allowed) with a K, M, G, T, P or E suffix (powers of
/// 1024), a percentage of physical memory (ram), or "infinity" (u64::MAX).
pub fn parse_size(s: &str, ram: u64) -> Result<u64> {
    let bad = || {
        anyhow::anyhow!(
            "invalid size {s:?}: use bytes, a K/M/G/T suffix (24G), a percentage of RAM (80%), or infinity"
        )
    };
    if s == "infinity" {
        return Ok(u64::MAX);
    }
    if let Some(pct) = s.strip_suffix('%') {
        let p = parse_decimal(pct).ok_or_else(bad)?;
        if p > 100.0 {
            return Err(bad());
        }
        return Ok((ram as f64 * p / 100.0) as u64);
    }
    let (num, mult) = match s.chars().last() {
        Some(c @ ('K' | 'M' | 'G' | 'T' | 'P' | 'E')) => {
            let shift = 10 * (1 + "KMGTPE".find(c).unwrap_or(0) as u32);
            (&s[..s.len() - 1], 1u64 << shift)
        }
        Some('B') => (&s[..s.len() - 1], 1),
        _ => (s, 1),
    };
    if mult == 1 {
        return num.parse::<u64>().map_err(|_| bad());
    }
    let v = parse_decimal(num).ok_or_else(bad)? * mult as f64;
    if v >= u64::MAX as f64 {
        return Err(bad());
    }
    Ok(v as u64)
}

fn parse_decimal(s: &str) -> Option<f64> {
    let ok = !s.is_empty()
        && s.chars().all(|c| c.is_ascii_digit() || c == '.')
        && s.chars().filter(|&c| c == '.').count() <= 1
        && !s.starts_with('.')
        && !s.ends_with('.');
    if ok { s.parse().ok() } else { None }
}

/// format_size writes bytes the way a person would pass them: whole G when
/// it is, else whole M (rounded down), else bytes.
pub fn format_size(bytes: u64) -> String {
    const M: u64 = 1 << 20;
    const G: u64 = 1 << 30;
    if bytes >= G && bytes.is_multiple_of(G) {
        format!("{}G", bytes / G)
    } else if bytes >= M {
        format!("{}M", bytes / M)
    } else {
        bytes.to_string()
    }
}

/// format_exact writes bytes in systemd's size syntax without losing any:
/// the largest of G, M or K it is a whole number of, else bytes; infinity
/// for u64::MAX.
pub fn format_exact(bytes: u64) -> String {
    if bytes == u64::MAX {
        return "infinity".to_string();
    }
    for (unit, shift) in [("G", 30), ("M", 20), ("K", 10)] {
        let n = 1u64 << shift;
        if bytes >= n && bytes.is_multiple_of(n) {
            return format!("{}{unit}", bytes / n);
        }
    }
    bytes.to_string()
}

/// size_matches says whether systemd applied the size asked for: the same,
/// or the same rounded down to a whole page (as the kernel takes a limit).
pub fn size_matches(asked: u64, applied: u64) -> bool {
    if asked == applied {
        return true;
    }
    if asked == u64::MAX || applied == u64::MAX {
        return false;
    }
    // SAFETY: sysconf only reads system configuration.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    applied <= asked && asked - applied < page
}

/// physical_memory is this machine's RAM in bytes (0 if it can't be read).
pub fn physical_memory() -> u64 {
    // SAFETY: sysconf only reads system configuration.
    let (pages, size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    if pages <= 0 || size <= 0 {
        return 0;
    }
    pages as u64 * size as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u64 = 1 << 30;

    #[test]
    fn parse_size_systemd_syntax() {
        let ram = 64 * G;
        assert_eq!(parse_size("24G", ram).unwrap(), 24 * G);
        assert_eq!(parse_size("1.5G", ram).unwrap(), 3 * G / 2);
        assert_eq!(parse_size("64M", ram).unwrap(), 64 << 20);
        assert_eq!(parse_size("512K", ram).unwrap(), 512 << 10);
        assert_eq!(parse_size("1T", ram).unwrap(), 1 << 40);
        assert_eq!(parse_size("1000", ram).unwrap(), 1000);
        assert_eq!(parse_size("1000B", ram).unwrap(), 1000);
        assert_eq!(parse_size("50%", ram).unwrap(), 32 * G);
        assert_eq!(parse_size("infinity", ram).unwrap(), u64::MAX);
        for bad in [
            "", "G", "24g", "24 G", "-1G", "1.G", ".5G", "1.2.3G", "150%", "x%", "lots", "1.5",
        ] {
            assert!(parse_size(bad, ram).is_err(), "{bad:?} parsed");
        }
        let e = parse_size("24g", ram).unwrap_err().to_string();
        assert!(e.contains(r#"invalid size "24g""#), "{e}");
    }

    #[test]
    fn format_size_reads_back() {
        assert_eq!(format_size(48 * G), "48G");
        assert_eq!(format_size(48 * G + (1 << 20) * 5 + 7), "49157M");
        assert_eq!(format_size(100), "100");
        for b in [48 * G, 3 * G / 2, 77 << 20] {
            assert_eq!(parse_size(&format_size(b), 0).unwrap(), b);
        }
    }

    #[test]
    fn resolve_defaults() {
        let ram = 64 * G;
        let l = Limits::resolve(None, None, None, ram).unwrap();
        assert_eq!(l.memory_max, format_size(ram / 100 * 85));
        assert_eq!(l.memory_high, format_size(ram / 100 * 75));
        assert_eq!(l.memory_swap_max, "0");
        assert_eq!(
            l.properties(),
            vec![
                format!("MemoryHigh={}", l.memory_high),
                format!("MemoryMax={}", l.memory_max),
                "MemorySwapMax=0".to_string()
            ]
        );
    }

    #[test]
    fn resolve_overrides() {
        let ram = 64 * G;
        let l = Limits::resolve(Some("20G"), Some("24G"), Some("0"), ram).unwrap();
        assert_eq!(
            l,
            Limits {
                memory_high: "20G".into(),
                memory_max: "24G".into(),
                memory_swap_max: "0".into()
            }
        );
        assert_eq!(
            l.properties(),
            vec!["MemoryHigh=20G", "MemoryMax=24G", "MemorySwapMax=0"]
        );

        // A max given alone brings the default high under it, in proportion.
        let l = Limits::resolve(None, Some("17G"), None, ram).unwrap();
        assert_eq!(l.memory_max, "17G");
        assert_eq!(l.memory_high, format_size(17 * G / 85 * 75));
        // ... but never above the default.
        let l = Limits::resolve(None, Some("infinity"), None, ram).unwrap();
        assert_eq!(l.memory_high, format_size(ram / 100 * 75));
        let l = Limits::resolve(None, Some("100%"), None, ram).unwrap();
        assert_eq!(l.memory_high, format_size(ram / 100 * 75));

        // A high given alone stays under the default max, or is refused.
        assert!(Limits::resolve(Some("10G"), None, None, ram).is_ok());
        let e = Limits::resolve(Some("60G"), None, None, ram)
            .unwrap_err()
            .to_string();
        assert!(e.contains("--memory-high 60G is above --memory-max"), "{e}");
        let e = Limits::resolve(Some("30G"), Some("24G"), None, ram)
            .unwrap_err()
            .to_string();
        assert_eq!(e, "--memory-high 30G is above --memory-max 24G");
        let e = Limits::resolve(None, Some("24gb"), None, ram)
            .unwrap_err()
            .to_string();
        assert!(e.starts_with("--memory-max: invalid size"), "{e}");
    }

    #[test]
    fn exact_sizes_and_matching() {
        assert_eq!(format_exact(24 * G), "24G");
        assert_eq!(format_exact(3 * G / 2), "1536M");
        assert_eq!(format_exact(4096), "4K");
        assert_eq!(format_exact(55171268608), "53878192K");
        assert_eq!(format_exact(1000), "1000");
        assert_eq!(format_exact(0), "0");
        assert_eq!(format_exact(u64::MAX), "infinity");
        for b in [24 * G, 4096, 1000, 55171268608] {
            assert_eq!(parse_size(&format_exact(b), 0).unwrap(), b);
        }
        // 85% of this machine's RAM, as systemd applied it: a page under.
        assert!(size_matches(55171271884, 55171268608));
        assert!(size_matches(u64::MAX, u64::MAX));
        assert!(size_matches(0, 0));
        assert!(!size_matches(2 * G, G));
        assert!(!size_matches(G, 2 * G));
        assert!(!size_matches(u64::MAX, 64 * G));
    }

    #[test]
    fn physical_memory_is_read() {
        assert!(physical_memory() > 64 << 20);
    }
}
