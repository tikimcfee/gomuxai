//! Go `internal/buildinfo`: which build of the agent binary is running.
//!
//! Go reads the module and VCS information its toolchain embeds; here
//! build.rs records the same (the git revision, whether the tree had
//! uncommitted changes, the commit time) and the compiler's version, so the
//! line reads like Go's with the toolchain naming rustc, e.g.
//! "0.2.0 1a2b3c4-dirty 2026-11-20T10:00:00Z rustc 1.98.1 linux/amd64".

use std::fmt;
use std::sync::OnceLock;

/// What the build recorded about this binary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Build {
    /// The crate version (Go: the module version, "devel" for an untagged build).
    pub version: String,
    /// Short VCS revision, "" if none was recorded.
    pub revision: String,
    /// Built from a working tree with uncommitted changes.
    pub modified: bool,
    /// Commit time (RFC 3339), "" if unknown.
    pub time: String,
    /// The compiler, e.g. "rustc 1.98.1" (Go: GoVersion, "go1.25.1").
    pub toolchain: String,
    /// GOOS/GOARCH, in Go's names: "linux/amd64", "darwin/arm64".
    pub platform: String,
}

/// What build.rs recorded, raw: Go's `debug.BuildInfo` for this port.
#[derive(Debug, Clone, Default)]
pub struct BuildInfo {
    /// The package version Cargo built ("0.2.0"); Go's Main.Version.
    pub main_version: String,
    /// The toolchain, "rustc 1.98.1".
    pub toolchain: String,
    /// vcs.revision: the full commit hash, or "".
    pub vcs_revision: String,
    /// vcs.time, or "".
    pub vcs_time: String,
    /// vcs.modified: "true", "false", or "".
    pub vcs_modified: String,
}

/// This binary's recorded build information.
pub fn read_build_info() -> BuildInfo {
    BuildInfo {
        main_version: env!("CARGO_PKG_VERSION").to_string(),
        toolchain: env!("AGENT_BUILD_TOOLCHAIN").to_string(),
        vcs_revision: env!("AGENT_BUILD_VCS_REVISION").to_string(),
        vcs_time: env!("AGENT_BUILD_VCS_TIME").to_string(),
        vcs_modified: env!("AGENT_BUILD_VCS_MODIFIED").to_string(),
    }
}

/// This binary's build.
pub fn current() -> &'static Build {
    static CURRENT: OnceLock<Build> = OnceLock::new();
    CURRENT.get_or_init(|| from_build_info(Some(&read_build_info())))
}

/// A one-word id for this binary's build, for recording alongside what it
/// did, e.g. "c7253f9-dirty", "0.2.0".
pub fn id() -> String {
    current().id()
}

impl Build {
    /// A one-word id for b: the short revision (with "-dirty" if modified),
    /// else the version.
    pub fn id(&self) -> String {
        if self.revision.is_empty() {
            return self.version.clone();
        }
        if self.modified {
            return format!("{}-dirty", self.revision);
        }
        self.revision.clone()
    }
}

/// b on one line, e.g.
/// "0.2.0 c7253f9-dirty 2026-10-24T21:18:44Z rustc 1.98.1 linux/amd64".
impl fmt::Display for Build {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = vec![self.version.clone()];
        if !self.revision.is_empty() {
            parts.push(self.id());
        } else {
            parts.push("(no VCS info)".to_string());
        }
        if !self.time.is_empty() {
            parts.push(self.time.clone());
        }
        parts.push(self.toolchain.clone());
        parts.push(self.platform.clone());
        f.write_str(&parts.join(" "))
    }
}

/// The platform in Go's names (GOOS/GOARCH): "linux/amd64", "darwin/arm64".
pub fn platform() -> String {
    format!(
        "{}/{}",
        go_os(std::env::consts::OS),
        go_arch(std::env::consts::ARCH)
    )
}

fn go_os(os: &str) -> &str {
    match os {
        "macos" => "darwin",
        other => other,
    }
}

fn go_arch(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "powerpc64" if cfg!(target_endian = "little") => "ppc64le",
        "powerpc64" => "ppc64",
        "loongarch64" => "loong64",
        "mips64" if cfg!(target_endian = "little") => "mips64le",
        "mips" if cfg!(target_endian = "little") => "mipsle",
        other => other,
    }
}

/// The toolchain to name when none was recorded: rustc, version unknown.
const UNKNOWN_TOOLCHAIN: &str = "rustc unknown";

pub fn from_build_info(bi: Option<&BuildInfo>) -> Build {
    let mut b = Build {
        version: "devel".to_string(),
        toolchain: UNKNOWN_TOOLCHAIN.to_string(),
        platform: platform(),
        ..Default::default()
    };
    let Some(bi) = bi else {
        return b;
    };
    if !bi.toolchain.is_empty() {
        b.toolchain = bi.toolchain.clone();
    }
    b.revision = bi.vcs_revision.clone();
    b.time = bi.vcs_time.clone();
    b.modified = bi.vcs_modified == "true";
    if b.revision.len() > 7 {
        b.revision.truncate(7);
    }
    // Go: builds in a checkout get a pseudo-version made of the commit time
    // and revision, which the other fields already say; only a tag adds
    // news. Cargo's version is always a release version, so it is kept.
    let v = bi
        .main_version
        .strip_suffix("+dirty")
        .unwrap_or(&bi.main_version);
    if !v.is_empty() && v != "(devel)" && !is_pseudo_version(v) {
        b.version = v.to_string();
    }
    b
}

/// Whether v is a Go pseudo-version, which ends in
/// "<yyyymmddhhmmss>-<12 hex digits>", e.g. "v0.0.0-20261024211844-c7253f98712b".
pub fn is_pseudo_version(v: &str) -> bool {
    let v = v.as_bytes();
    let Some(i) = v.iter().rposition(|&c| c == b'-') else {
        return false;
    };
    if i < 15 || v.len() - i - 1 != 12 {
        return false;
    }
    let stamp = &v[i - 14..i];
    stamp.iter().all(u8::is_ascii_digit)
        && v[i + 1..]
            .iter()
            .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_build_info() {
        let plat = platform();
        let vcs = |modified: &str, main: &str| BuildInfo {
            main_version: main.to_string(),
            toolchain: "rustc 1.98.1".to_string(),
            vcs_revision: "c7253f98712b95668bc56c584c9696e3907be2fc".to_string(),
            vcs_time: "2026-10-24T21:18:44Z".to_string(),
            vcs_modified: modified.to_string(),
        };
        let no_vcs = |main: &str| BuildInfo {
            main_version: main.to_string(),
            toolchain: "rustc 1.98.1".to_string(),
            ..Default::default()
        };
        let tests: Vec<(&str, Option<BuildInfo>, &str, String)> = vec![
            (
                "no build info",
                None,
                "devel",
                format!("devel (no VCS info) rustc unknown {plat}"),
            ),
            (
                "no version, no VCS",
                Some(no_vcs("")),
                "devel",
                format!("devel (no VCS info) rustc 1.98.1 {plat}"),
            ),
            (
                "checkout, pseudo-version",
                Some(vcs("false", "v0.0.0-20261024211844-c7253f98712b")),
                "c7253f9",
                format!("devel c7253f9 2026-10-24T21:18:44Z rustc 1.98.1 {plat}"),
            ),
            (
                "checkout, modified",
                Some(vcs("true", "0.2.0")),
                "c7253f9-dirty",
                format!("0.2.0 c7253f9-dirty 2026-10-24T21:18:44Z rustc 1.98.1 {plat}"),
            ),
            (
                "crate version",
                Some(vcs("false", "0.2.0")),
                "c7253f9",
                format!("0.2.0 c7253f9 2026-10-24T21:18:44Z rustc 1.98.1 {plat}"),
            ),
            (
                "no VCS (a source tarball)",
                Some(no_vcs("0.2.0")),
                "0.2.0",
                format!("0.2.0 (no VCS info) rustc 1.98.1 {plat}"),
            ),
        ];
        for (name, bi, id, line) in tests {
            let b = from_build_info(bi.as_ref());
            assert_eq!(b.id(), id, "{name}: id()");
            assert_eq!(b.to_string(), line, "{name}: to_string()");
        }
    }

    #[test]
    fn test_is_pseudo_version() {
        for (v, want) in [
            ("v0.0.0-20261024211844-c7253f98712b", true),
            ("v1.2.4-0.20261024211844-c7253f98712b", true),
            ("v1.3.0-pre.0.20261024211844-c7253f98712b", true),
            ("v1.2.0", false),
            ("v1.2.0-rc.1", false),
            ("v1.2.0-beta1", false),
        ] {
            assert_eq!(is_pseudo_version(v), want, "is_pseudo_version({v:?})");
        }
    }

    #[test]
    fn current_names_rustc_and_the_platform() {
        let line = current().to_string();
        assert!(
            line.starts_with(concat!(env!("CARGO_PKG_VERSION"), " ")),
            "{line}"
        );
        assert!(line.contains(" rustc "), "{line}");
        assert!(line.ends_with(&platform()), "{line}");
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(platform(), "linux/amd64");
        }
    }
}
