//! The data layer behind the Deltos table view: a bounded cache of rows in front of a
//! table or a query result of any size, so a person can open a table of hundreds of
//! millions of rows, jump anywhere and scroll, while memory stays inside a fixed budget
//! and the UI thread never waits on the database or the disk.
//!
//! This crate knows nothing about the UI toolkit or the async runtime. The app hands it
//! [`Hooks`]: a `notify` callback that becomes a redraw, a `spawn` for futures and a
//! `spawn_blocking` for disk work. Everything here is `Send + Sync` and object safe.
//!
//! # Pieces
//!
//! - [`GridCache`] holds pages of rows behind one mutex that the view takes once per
//!   frame. [`GridCache::set_viewport`] says what is on screen; [`GridCache::with_rows`]
//!   visits it, handing out `&[Value]` slices without copying and `None` for a row still
//!   on its way, which the view draws as a placeholder.
//! - [`PageLoader`] is what feeds the cache: `load(page)`, `count()`, and a [`Reporter`]
//!   through which a loader sends what it learns later (a finished exact count, a
//!   stream's growing total, an error).
//! - [`BrowseLoader`] reads an object through `Connection::browse`, seeking by the key of
//!   the previous page where the driver can, so page 100,000 costs the same as page 1.
//! - [`StreamLoader`] reads a statement's forward-only result into a [`SpillStore`] on
//!   disk as the viewport approaches the end of what has been read, or to the end on
//!   request, and decodes pages from the file.
//! - [`export_csv`] streams any loader to CSV one page at a time.
//!
//! # The memory bound
//!
//! Rows are kept in pages of [`CacheConfig::page_rows`] rows. Each page's cost is
//! counted when it arrives, as the sum of its values' [`deltos_data::Value::heap_size`] plus 24 bytes
//! per value and small fixed per-row and per-page overheads, and the total is kept at or
//! under [`CacheConfig::max_bytes`] by evicting whole pages, least recently used first,
//! every time a page is inserted. Pages in the window (the viewport and its prefetch)
//! are never evicted, so the true bound is `max(max_bytes, window)`; with the defaults
//! the window is a few pages against a 256 MiB budget. Nothing else grows with the
//! source: the loaders remember a few bytes per page visited (seek keys, file offsets),
//! and the spill store keeps one `u64` per page of a result in memory and the rows on
//! disk, where [`StreamConfig::max_spill_bytes`] can cap them.
//!
//! Loads are bounded too: at most [`CacheConfig::max_concurrent_loads`] requests are in
//! flight, the queue behind them is rebuilt from scratch on every viewport change, and a
//! result from before an [`GridCache::invalidate`] is recognised by its generation number
//! and dropped. A stream is pulled only as far as the window needs, one batch at a time.
//!
//! # Threads
//!
//! The view thread only ever takes the cache lock for a dictionary lookup or a short
//! walk over a viewport; no I/O happens under it, and the cache never calls into a
//! loader while holding it. Completed loads take the lock briefly to insert, then call
//! `notify` once per page. Disk reads and writes run through `spawn_blocking`, so an
//! async runtime's threads are not held up by the disk either.
//!
//! # Columns
//!
//! Document stores return a column list per page that grows as new fields are seen. The
//! cache merges them by name into one list that only grows at the end, lays each page's
//! rows out in that order when they arrive, and widens older rows with nulls the first
//! time they are read after the list grew.

#![forbid(unsafe_code)]

mod browse;
mod cache;
mod columns;
mod export;
mod loader;
mod spill;
mod stream;
#[cfg(test)]
mod testing;

pub use browse::{BrowseConfig, BrowseLoader, BrowseQuery, BrowseStats};
pub use cache::{CacheConfig, CacheStats, GridCache};
pub use export::{CsvOptions, export_csv};
pub use loader::{
    Hooks, LoadedPage, LoaderEvent, NotifyFn, PageLoader, PageRequest, Reporter, RowCount,
    SpawnBlockingFn, SpawnFn, Status, run_blocking,
};
pub use spill::{PageSpan, SpillStore, decode_rows, encode_row, read_span};
pub use stream::{StreamConfig, StreamLoader};
