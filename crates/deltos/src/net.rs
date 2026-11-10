//! The Tokio runtime that drivers, row loading and the updater run on, and the bridge
//! that hands their results back to GPUI's executor.

use std::future::Future;

use futures::FutureExt;
use gpui::Global;

pub struct Net {
    runtime: tokio::runtime::Runtime,
}

impl Global for Net {}

impl Net {
    pub fn new() -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(
                std::thread::available_parallelism()
                    .map_or(2, |n| n.get())
                    .clamp(2, 4),
            )
            .thread_name("deltos-io")
            .enable_all()
            .build()?;
        Ok(Self { runtime })
    }

    pub fn handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    /// Runs `fut` on the runtime without waiting for its result.
    pub fn spawn<F>(&self, fut: F)
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        drop(self.runtime.spawn(fut));
    }

    /// Runs `fut` on the runtime and returns a future any executor can await. Resolves to
    /// `None` if the task panicked or the runtime shut down.
    pub fn run<T, F>(&self, fut: F) -> impl Future<Output = Option<T>> + use<T, F>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.runtime.spawn(async move {
            let _ = tx.send(fut.await);
        });
        rx.map(Result::ok)
    }

    /// Runs blocking work, such as a keychain call, on the runtime's blocking pool.
    pub fn run_blocking<T, F>(&self, f: F) -> impl Future<Output = Option<T>> + use<T, F>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let task = self.runtime.spawn_blocking(f);
        task.map(Result::ok)
    }
}
