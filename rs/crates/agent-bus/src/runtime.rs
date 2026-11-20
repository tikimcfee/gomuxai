//! Internal Tokio runtime and blocking cancellation `Context`.
//!
//! Per rule 4, only `agent-bus` uses Tokio, hidden behind plain blocking
//! functions and synchronous types.

use std::future::Future;
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name("agent-bus")
        .build()
        .expect("building agent-bus tokio runtime")
});

pub(crate) fn handle() -> &'static tokio::runtime::Handle {
    RUNTIME.handle()
}

pub(crate) fn block_on<F: Future>(fut: F) -> F::Output {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| RUNTIME.handle().block_on(fut))
    } else {
        RUNTIME.block_on(fut)
    }
}

/// Go's `context.Canceled` and `context.DeadlineExceeded`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ContextError {
    #[error("context canceled")]
    Canceled,
    #[error("context deadline exceeded")]
    DeadlineExceeded,
}

pub fn is_deadline_exceeded(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ContextError>() == Some(&ContextError::DeadlineExceeded)
}

pub fn is_canceled(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ContextError>() == Some(&ContextError::Canceled)
}

struct ContextInner {
    state: Mutex<Option<ContextError>>,
    cond: Condvar,
    notify: tokio::sync::Notify,
    deadline: Option<Instant>,
}

impl ContextInner {
    fn finish(&self, reason: ContextError) {
        let mut guard = self.state.lock().unwrap();
        if guard.is_none() {
            *guard = Some(reason);
            drop(guard);
            self.cond.notify_all();
            self.notify.notify_waiters();
        }
    }
}

/// A synchronous cancellation and deadline token mirroring Go's `context.Context`.
#[derive(Clone, Default)]
pub struct Context {
    inner: Option<Arc<ContextInner>>,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("err", &self.err())
            .field("deadline", &self.deadline())
            .finish()
    }
}

/// Cancels a [`Context`] when called or dropped if desired via [`CancelGuard`].
/// Calling [`CancelFn::cancel`] is idempotent and cheap; dropping `CancelFn`
/// does not implicitly cancel unless [`CancelFn::cancel`] is called, except in
/// [`Context::with_timeout`] where the background timer exits once canceled.
#[derive(Clone)]
pub struct CancelFn {
    inner: Arc<ContextInner>,
}

impl CancelFn {
    pub fn cancel(&self) {
        self.inner.finish(ContextError::Canceled);
    }
}

impl Context {
    /// Go's `context.Background()`: never canceled, no deadline.
    pub fn background() -> Self {
        Self { inner: None }
    }

    /// Go's `context.WithCancel(parent)`.
    pub fn with_cancel(parent: &Context) -> (Self, CancelFn) {
        let deadline = parent.deadline();
        let inner = Arc::new(ContextInner {
            state: Mutex::new(parent.err()),
            cond: Condvar::new(),
            notify: tokio::sync::Notify::new(),
            deadline,
        });
        if let Some(p) = &parent.inner {
            let p = Arc::clone(p);
            let child = Arc::clone(&inner);
            handle().spawn(async move {
                let err = wait_inner_async(&p).await;
                child.finish(err);
            });
        }
        let cancel = CancelFn {
            inner: Arc::clone(&inner),
        };
        (Self { inner: Some(inner) }, cancel)
    }

    /// Go's `context.WithTimeout(parent, timeout)`.
    pub fn with_timeout(parent: &Context, timeout: Duration) -> (Self, CancelFn) {
        let now = Instant::now();
        let own_deadline = now.checked_add(timeout).unwrap_or(now);
        let deadline = match parent.deadline() {
            Some(pd) if pd < own_deadline => pd,
            _ => own_deadline,
        };
        let initial = if let Some(err) = parent.err() {
            Some(err)
        } else if timeout.is_zero() {
            Some(ContextError::DeadlineExceeded)
        } else {
            None
        };
        let inner = Arc::new(ContextInner {
            state: Mutex::new(initial),
            cond: Condvar::new(),
            notify: tokio::sync::Notify::new(),
            deadline: Some(deadline),
        });
        if initial.is_none() {
            let child = Arc::clone(&inner);
            let parent_inner = parent.inner.clone();
            handle().spawn(async move {
                let sleep_dur = deadline.saturating_duration_since(Instant::now());
                tokio::select! {
                    () = tokio::time::sleep(sleep_dur) => {
                        child.finish(ContextError::DeadlineExceeded);
                    }
                    err = async {
                        match &parent_inner {
                            Some(p) => wait_inner_async(p).await,
                            None => std::future::pending().await,
                        }
                    } => {
                        child.finish(err);
                    }
                    _ = wait_inner_async_ref(&child) => {}
                }
            });
        }
        let cancel = CancelFn {
            inner: Arc::clone(&inner),
        };
        (Self { inner: Some(inner) }, cancel)
    }

    /// Reports whether the context has been canceled or timed out.
    pub fn is_done(&self) -> bool {
        self.err().is_some()
    }

    /// Go's `ctx.Err()`.
    pub fn err(&self) -> Option<ContextError> {
        let inner = self.inner.as_ref()?;
        let mut guard = inner.state.lock().unwrap();
        if guard.is_none()
            && let Some(dl) = inner.deadline
            && Instant::now() >= dl
        {
            *guard = Some(ContextError::DeadlineExceeded);
            drop(guard);
            inner.cond.notify_all();
            inner.notify.notify_waiters();
            return Some(ContextError::DeadlineExceeded);
        }
        *guard
    }

    /// Go's `ctx.Deadline()`.
    pub fn deadline(&self) -> Option<Instant> {
        self.inner.as_ref().and_then(|i| i.deadline)
    }

    /// Blocks the current thread until the context is canceled or reaches its deadline.
    pub fn wait_done(&self) -> ContextError {
        let Some(inner) = &self.inner else {
            loop {
                std::thread::park();
            }
        };
        let mut guard = inner.state.lock().unwrap();
        loop {
            if let Some(err) = *guard {
                return err;
            }
            if let Some(dl) = inner.deadline {
                let now = Instant::now();
                if now >= dl {
                    *guard = Some(ContextError::DeadlineExceeded);
                    drop(guard);
                    inner.cond.notify_all();
                    inner.notify.notify_waiters();
                    return ContextError::DeadlineExceeded;
                }
                let (g, _) = inner.cond.wait_timeout(guard, dl - now).unwrap();
                guard = g;
            } else {
                guard = inner.cond.wait(guard).unwrap();
            }
        }
    }

    /// Blocks the current thread for up to `d`, returning `Some(err)` if the
    /// context finished first or `None` if `d` elapsed.
    pub fn sleep_or_done(&self, d: Duration) -> Option<ContextError> {
        if let Some(err) = self.err() {
            return Some(err);
        }
        let Some(inner) = &self.inner else {
            std::thread::sleep(d);
            return None;
        };
        let end = Instant::now() + d;
        let target = match inner.deadline {
            Some(dl) if dl < end => dl,
            _ => end,
        };
        let mut guard = inner.state.lock().unwrap();
        loop {
            if let Some(err) = *guard {
                return Some(err);
            }
            let now = Instant::now();
            if now >= target {
                drop(guard);
                return self.err();
            }
            let (g, _) = inner.cond.wait_timeout(guard, target - now).unwrap();
            guard = g;
        }
    }

    pub(crate) async fn done(&self) -> ContextError {
        match &self.inner {
            Some(inner) => wait_inner_async(inner).await,
            None => std::future::pending().await,
        }
    }
}

async fn wait_inner_async(inner: &Arc<ContextInner>) -> ContextError {
    wait_inner_async_ref(inner).await
}

async fn wait_inner_async_ref(inner: &ContextInner) -> ContextError {
    loop {
        let notified = inner.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(err) = *inner.state.lock().unwrap() {
            return err;
        }
        notified.await;
    }
}
