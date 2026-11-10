//! Where the drivers' network work runs.
//!
//! The client libraries need a Tokio runtime for their sockets and timers. The app may
//! poll our futures from another executor (GPUI's, for one), so each connection keeps the
//! handle of the runtime it was opened on and spawns every operation there. When `connect`
//! is called outside any Tokio runtime, a small runtime owned by this crate is used.

use std::future::Future;
use std::sync::OnceLock;

use deltos_data::{Error, Result};
use futures::future::BoxFuture;
use tokio::runtime::{Handle, Runtime};

static FALLBACK: OnceLock<std::result::Result<Runtime, String>> = OnceLock::new();

/// The current runtime, or this crate's own one.
pub(crate) fn handle() -> Result<Handle> {
    if let Ok(handle) = Handle::try_current() {
        return Ok(handle);
    }
    let runtime = FALLBACK.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("deltos-db")
            .enable_all()
            .build()
            .map_err(|e| format!("could not start the database runtime: {e}"))
    });
    match runtime {
        Ok(runtime) => Ok(runtime.handle().clone()),
        Err(message) => Err(Error::Other(message.clone())),
    }
}

/// Runs `future` on `handle` and returns a future that waits for it from any executor.
pub(crate) fn spawn<T, F>(handle: &Handle, future: F) -> BoxFuture<'static, Result<T>>
where
    T: Send + 'static,
    F: Future<Output = Result<T>> + Send + 'static,
{
    let task = handle.spawn(future);
    Box::pin(async move {
        match task.await {
            Ok(result) => result,
            Err(e) if e.is_cancelled() => Err(Error::Cancelled),
            Err(_) => Err(Error::Other(
                "the database task stopped unexpectedly".to_string(),
            )),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn works_without_a_runtime() {
        let handle = handle().expect("fallback runtime");
        let value = futures::executor::block_on(spawn(&handle, async { Ok(41 + 1) }));
        assert_eq!(value.ok(), Some(42));
    }
}
