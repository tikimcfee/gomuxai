//! Go's `context.Context`, for the service's waits. The Go service takes a
//! context on every method, but only its sleeps (`sleepCtx`) and a few
//! checks of `ctx.Err()` ever look at it. So here only the methods that can
//! wait take one (spawn, send, deliver, resume, ...), and it is the bus's
//! [`Context`] (agent_bus::Context), so the bus's RPC server passes its own
//! straight through. A wait it ends returns a [`ContextError`] (Go's
//! `context.Canceled` and `context.DeadlineExceeded`, with their words),
//! which callers find with `downcast_ref::<ContextError>()` as the Go uses
//! `errors.Is`.

use std::time::Duration;

pub use agent_bus::{CancelFn, Context, ContextError};

/// The context the service's waiting methods take.
pub(crate) type Ctx = Context;

/// sleepCtx: waits for d, or until the context is done.
pub(crate) trait CtxExt {
    fn sleep(&self, d: Duration) -> Result<(), ContextError>;
}

impl CtxExt for Context {
    fn sleep(&self, d: Duration) -> Result<(), ContextError> {
        match self.sleep_or_done(d) {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}
