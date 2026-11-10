//! The contract between [`GridCache`](crate::GridCache) and whatever feeds it rows, plus
//! the hooks through which this crate reaches the app's runtime without depending on it.

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use deltos_data::{Column, Error, Result, Row};
use futures::FutureExt;
use futures::channel::oneshot;
use futures::future::{BoxFuture, Either};

/// Wakes the UI: new rows, a new row count or a new status are ready to draw. Called from
/// whatever thread finished the work, at most once per page or event, never per row.
pub type NotifyFn = Arc<dyn Fn() + Send + Sync>;

/// Runs a future to completion on the app's async runtime. The future never blocks the
/// thread it is polled on: disk work goes through [`SpawnBlockingFn`].
pub type SpawnFn = Arc<dyn Fn(BoxFuture<'static, ()>) + Send + Sync>;

/// Runs a closure that may block (file reads and writes) on a thread meant for blocking
/// work, such as Tokio's blocking pool. The spill store does all of its disk access this
/// way, so the async runtime's threads are never held up by the disk.
pub type SpawnBlockingFn = Arc<dyn Fn(Box<dyn FnOnce() + Send>) + Send + Sync>;

/// How the cache reaches the app: a redraw request and two ways to run work. The app
/// builds these once from its runtime (for Tokio: `handle.spawn(fut)` and
/// `handle.spawn_blocking(f)`) and hands a clone to every cache it creates.
#[derive(Clone)]
pub struct Hooks {
    pub notify: NotifyFn,
    pub spawn: SpawnFn,
    pub spawn_blocking: SpawnBlockingFn,
}

impl fmt::Debug for Hooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Hooks")
    }
}

/// Runs `f` through the blocking hook and resolves with its result. Without hooks the
/// closure runs inline, which is what the export path and the tests use. The future
/// resolves with [`Error::Cancelled`] if the runtime drops the job without running it.
pub fn run_blocking<T: Send + 'static>(
    hooks: Option<&Hooks>,
    f: impl FnOnce() -> T + Send + 'static,
) -> impl Future<Output = Result<T>> + Send {
    match hooks {
        None => Either::Left(futures::future::ready(Ok(f()))),
        Some(hooks) => {
            let (tx, rx) = oneshot::channel();
            (hooks.spawn_blocking)(Box::new(move || {
                let _ = tx.send(f());
            }));
            Either::Right(rx.map(|r| r.map_err(|_| Error::Cancelled)))
        }
    }
}

/// How many rows the source has, as well as it is known right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowCount {
    #[default]
    Unknown,
    /// This many rows have been seen and more may follow (a stream still being read).
    AtLeast(u64),
    /// The database's estimate, which can be off in either direction.
    Estimated(u64),
    Exact(u64),
}

impl RowCount {
    /// The number to size a scrollbar from, whatever its confidence.
    pub fn value(self) -> Option<u64> {
        match self {
            RowCount::Unknown => None,
            RowCount::AtLeast(n) | RowCount::Estimated(n) | RowCount::Exact(n) => Some(n),
        }
    }

    pub fn is_exact(self) -> bool {
        matches!(self, RowCount::Exact(_))
    }

    /// Folds a newer piece of knowledge in. An exact count replaces anything and is only
    /// replaced by another exact count; a lower bound grows but never shrinks; an estimate
    /// is kept until rows are seen past it.
    pub fn refine(&mut self, new: RowCount) {
        *self = match (*self, new) {
            (_, RowCount::Unknown) => *self,
            (_, RowCount::Exact(n)) => RowCount::Exact(n),
            (RowCount::Exact(n), _) => RowCount::Exact(n),
            (RowCount::Unknown, new) => new,
            (RowCount::AtLeast(a), RowCount::AtLeast(b)) => RowCount::AtLeast(a.max(b)),
            (RowCount::AtLeast(a), RowCount::Estimated(e)) => {
                if e >= a {
                    RowCount::Estimated(e)
                } else {
                    RowCount::AtLeast(a)
                }
            }
            (RowCount::Estimated(e), RowCount::AtLeast(a)) => {
                if a > e {
                    RowCount::AtLeast(a)
                } else {
                    RowCount::Estimated(e)
                }
            }
            (RowCount::Estimated(_), RowCount::Estimated(e)) => RowCount::Estimated(e),
        };
    }
}

impl fmt::Display for RowCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RowCount::Unknown => f.write_str("unknown"),
            RowCount::AtLeast(n) => write!(f, "{n}+"),
            RowCount::Estimated(n) => write!(f, "~{n}"),
            RowCount::Exact(n) => write!(f, "{n}"),
        }
    }
}

/// What the cache is doing, for the status bar.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Status {
    /// Nothing in flight.
    #[default]
    Idle,
    /// Pages are being loaded or the source is being read in the background.
    Loading,
    /// The last load or the source failed. The message is shown as it is.
    Error(String),
    /// Every row of the source has been read (a stream reached its end) and nothing is
    /// in flight.
    Done,
}

/// One page the cache wants: rows `start..start + rows`, which is page number `page` at
/// the cache's page size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRequest {
    pub page: u64,
    pub start: u64,
    pub rows: u32,
}

impl PageRequest {
    pub fn new(page: u64, page_rows: u32) -> Self {
        Self {
            page,
            start: page * u64::from(page_rows),
            rows: page_rows,
        }
    }
}

/// The answer to a [`PageRequest`]. `columns` is this page's own column list, which for
/// document stores may differ from page to page; the cache merges them.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedPage {
    pub columns: Arc<[Column]>,
    pub rows: Vec<Row>,
    /// No rows exist past this page, so the source's row count is known exactly.
    pub end: bool,
}

/// Something a loader learned outside of a page load.
#[derive(Debug, Clone, PartialEq)]
pub enum LoaderEvent {
    /// The column list, known before any row (a statement's result description).
    Columns(Arc<[Column]>),
    /// A better row count: an exact count that finished, or a stream's growing total.
    RowCount(RowCount),
    /// Background reading started or stopped.
    Reading(bool),
    /// Every row the source will ever produce has been read.
    Finished,
    /// The source failed or stopped early; the message is shown as it is.
    Error(String),
}

/// The loader's line back to the cache that holds it: a channel for events and the
/// runtime hooks for background work of its own, such as an exact count or a stream pull.
#[derive(Clone)]
pub struct Reporter {
    hooks: Hooks,
    send: Arc<dyn Fn(LoaderEvent) + Send + Sync>,
}

impl Reporter {
    pub fn new(hooks: Hooks, send: Arc<dyn Fn(LoaderEvent) + Send + Sync>) -> Self {
        Self { hooks, send }
    }

    pub fn hooks(&self) -> &Hooks {
        &self.hooks
    }

    pub fn report(&self, event: LoaderEvent) {
        (self.send)(event);
    }

    pub fn spawn(&self, future: BoxFuture<'static, ()>) {
        (self.hooks.spawn)(future);
    }
}

impl fmt::Debug for Reporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Reporter")
    }
}

/// A source of pages. Implementations are shared behind `Arc` and called from the UI
/// thread, so `load` and `count` only build a future and return; the work happens when
/// the future runs on the app's runtime.
pub trait PageLoader: Send + Sync {
    /// Whether any page can be read without reading the ones before it. A forward-only
    /// stream answers false, and a jump far ahead then costs reading everything up to it.
    fn random_access(&self) -> bool;

    fn load(&self, request: PageRequest) -> BoxFuture<'static, Result<LoadedPage>>;

    /// The row count as quickly as it can be had, an estimate where the database keeps
    /// one. A loader that can improve on its answer later does so through the
    /// [`Reporter`] it was given in [`PageLoader::attach`].
    fn count(&self) -> BoxFuture<'static, Result<RowCount>>;

    /// Called once by the cache that takes the loader. The default keeps nothing.
    fn attach(&self, reporter: Reporter) {
        let _ = reporter;
    }

    /// The cache dropped everything it held, after a sort, filter or edit; anything the
    /// loader remembers about row positions is stale.
    fn invalidate(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refine_prefers_better_knowledge() {
        let mut c = RowCount::Unknown;
        c.refine(RowCount::Estimated(1000));
        assert_eq!(c, RowCount::Estimated(1000));
        c.refine(RowCount::AtLeast(500));
        assert_eq!(
            c,
            RowCount::Estimated(1000),
            "a bound under the estimate is noise"
        );
        c.refine(RowCount::AtLeast(1400));
        assert_eq!(
            c,
            RowCount::AtLeast(1400),
            "rows seen past the estimate win"
        );
        c.refine(RowCount::Estimated(1200));
        assert_eq!(c, RowCount::AtLeast(1400));
        c.refine(RowCount::Exact(1450));
        assert_eq!(c, RowCount::Exact(1450));
        c.refine(RowCount::Estimated(9));
        c.refine(RowCount::AtLeast(99_999));
        assert_eq!(c, RowCount::Exact(1450), "nothing beats exact but exact");
        c.refine(RowCount::Exact(1449));
        assert_eq!(c, RowCount::Exact(1449));
        c.refine(RowCount::Unknown);
        assert_eq!(c, RowCount::Exact(1449));
    }

    #[test]
    fn run_blocking_inline_and_hooked() {
        assert_eq!(
            futures::executor::block_on(run_blocking(None, || 7)).ok(),
            Some(7)
        );
        let hooks = Hooks {
            notify: Arc::new(|| {}),
            spawn: Arc::new(|_| {}),
            spawn_blocking: Arc::new(|f| f()),
        };
        assert_eq!(
            futures::executor::block_on(run_blocking(Some(&hooks), || "x")).ok(),
            Some("x")
        );
        let dropping = Hooks {
            spawn_blocking: Arc::new(drop),
            ..hooks
        };
        assert!(matches!(
            futures::executor::block_on(run_blocking(Some(&dropping), || 1)),
            Err(Error::Cancelled)
        ));
    }
}
