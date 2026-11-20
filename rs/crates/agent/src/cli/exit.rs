//! Exit codes (Go: internal/cli/exit.go): 0 done, 1 failed, and the codes a
//! command chooses with an [`ExitError`]: wait's 2 on a timeout, friction add's 3
//! when it finds a similar entry instead of filing.

use std::fmt;

/// An error that exits with a code other than 1, e.g. wait's timeout. Commands
/// return it rather than exiting, so the run is still recorded and the bus
/// publisher flushed. It prints as the error it carries.
#[derive(Debug)]
pub struct ExitError {
    pub code: i32,
    pub err: anyhow::Error,
}

impl ExitError {
    pub fn new(code: i32, err: anyhow::Error) -> ExitError {
        ExitError { code, err }
    }
}

impl fmt::Display for ExitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The whole chain of the error it carries, as Go's err.Error() is.
        write!(f, "{:#}", self.err)
    }
}

impl std::error::Error for ExitError {}

/// The process exit code for execute's error: an ExitError's code anywhere in
/// its chain, else 1.
pub fn exit_code(err: &anyhow::Error) -> i32 {
    err.chain()
        .find_map(|e| e.downcast_ref::<ExitError>())
        .map_or(1, |e| e.code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(&anyhow::anyhow!("failed")), 1);
        let timeout = anyhow::Error::new(ExitError::new(2, anyhow::anyhow!("timed out after 1s")));
        assert_eq!(exit_code(&timeout), 2);
        assert_eq!(timeout.to_string(), "timed out after 1s");
        let wrapped = timeout.context("waiting");
        assert_eq!(
            exit_code(&wrapped),
            2,
            "found through context, as errors.As does"
        );
        let inner = anyhow::anyhow!("no such entry").context("filing");
        assert_eq!(
            format!("{:#}", anyhow::Error::new(ExitError::new(3, inner))),
            "filing: no such entry"
        );
    }
}
