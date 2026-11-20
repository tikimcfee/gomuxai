use super::ErrUnsupported;

/// Whether spawn_disclaimed works in this build.
pub const SUPPORTED: bool = false;

/// Always fails outside darwin builds, with `ErrUnsupported`.
pub fn spawn_disclaimed<A: AsRef<str>, E: AsRef<str>>(
    _path: &str,
    _argv: &[A],
    _env: &[E],
) -> anyhow::Result<i32> {
    Err(ErrUnsupported.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_here() {
        let err = spawn_disclaimed("/bin/true", &["true"], &[] as &[&str]).unwrap_err();
        assert!(err.downcast_ref::<ErrUnsupported>().is_some());
    }
}
