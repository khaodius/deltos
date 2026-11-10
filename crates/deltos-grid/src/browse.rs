//! Pages of a table, view or collection through [`Connection::browse`], seeking by key
//! from one page to the next.
//!
//! Every page the loader reads leaves behind the order-column values of its last row.
//! When the page after it is requested, those values go out as `seek_after`, so a driver
//! that can seeks (`WHERE (a, id) > (?, ?)`) instead of skipping millions of rows. The
//! keys are a handful of values per page and are kept after the cache evicts the page
//! itself, so scrolling back over evicted ground seeks as well. A jump to a page whose
//! predecessor was never read falls back to a plain offset, which is what the driver
//! would do anyway.
//!
//! The row count comes as the database's estimate first, because an exact `COUNT(*)` on
//! a large table can take minutes. When the estimate is under
//! [`BrowseConfig::exact_count_threshold`] the exact count is taken in the background
//! and reported when it lands.

use std::collections::HashMap;
use std::sync::Arc;

use deltos_data::{BrowseRequest, Connection, Error, Filter, ObjectRef, Result, Sort, Value};
use futures::future::BoxFuture;
use parking_lot::Mutex;

use crate::loader::{LoadedPage, LoaderEvent, PageLoader, PageRequest, Reporter, RowCount};

/// Past this many remembered page keys the table is being read end to end, and the
/// oldest keys are worth less than the memory; the map starts over.
const MAX_KEYS: usize = 1 << 18;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowseConfig {
    /// An exact count is taken in the background when the estimate is at most this.
    pub exact_count_threshold: u64,
}

impl Default for BrowseConfig {
    fn default() -> Self {
        Self {
            exact_count_threshold: 5_000_000,
        }
    }
}

/// The sort and filter the table view currently shows.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BrowseQuery {
    pub sort: Vec<Sort>,
    pub filters: Vec<Filter>,
    pub raw_where: Option<String>,
}

impl BrowseQuery {
    fn request(
        &self,
        offset: u64,
        limit: u32,
        seek_after: Option<Vec<(String, Value)>>,
    ) -> BrowseRequest {
        BrowseRequest {
            offset,
            limit,
            sort: self.sort.clone(),
            filters: self.filters.clone(),
            raw_where: self.raw_where.clone(),
            seek_after,
        }
    }
}

/// How pages were asked for, for the status bar and the tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BrowseStats {
    pub seek_requests: u64,
    pub offset_requests: u64,
}

struct State {
    query: BrowseQuery,
    /// Order-column values of the last row of each page read, by page number.
    keys: HashMap<u64, Vec<(String, Value)>>,
    reporter: Option<Reporter>,
    stats: BrowseStats,
}

struct Inner {
    connection: Arc<dyn Connection>,
    object: ObjectRef,
    config: BrowseConfig,
    state: Mutex<State>,
}

/// A [`PageLoader`] over one browsable object of a connection.
pub struct BrowseLoader {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for BrowseLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrowseLoader")
            .field("object", &self.inner.object)
            .finish_non_exhaustive()
    }
}

impl BrowseLoader {
    pub fn new(
        connection: Arc<dyn Connection>,
        object: ObjectRef,
        query: BrowseQuery,
        config: BrowseConfig,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                connection,
                object,
                config,
                state: Mutex::new(State {
                    query,
                    keys: HashMap::new(),
                    reporter: None,
                    stats: BrowseStats::default(),
                }),
            }),
        }
    }

    pub fn object(&self) -> &ObjectRef {
        &self.inner.object
    }

    pub fn query(&self) -> BrowseQuery {
        self.inner.state.lock().query.clone()
    }

    /// Changes the sort or filter. Page keys belong to the old order and are dropped.
    /// The cache holding this loader must be invalidated afterwards.
    pub fn set_query(&self, query: BrowseQuery) {
        let mut state = self.inner.state.lock();
        state.query = query;
        state.keys.clear();
    }

    pub fn stats(&self) -> BrowseStats {
        self.inner.state.lock().stats
    }

    /// How many page keys are remembered.
    pub fn remembered_keys(&self) -> usize {
        self.inner.state.lock().keys.len()
    }
}

impl PageLoader for BrowseLoader {
    fn random_access(&self) -> bool {
        true
    }

    fn load(&self, request: PageRequest) -> BoxFuture<'static, Result<LoadedPage>> {
        let inner = self.inner.clone();
        let browse = {
            let mut state = inner.state.lock();
            let seek_after = match request.page.checked_sub(1) {
                Some(previous) => state.keys.get(&previous).cloned(),
                None => None,
            };
            if seek_after.is_some() {
                state.stats.seek_requests += 1;
            } else {
                state.stats.offset_requests += 1;
            }
            state.query.request(request.start, request.rows, seek_after)
        };
        Box::pin(async move {
            let page = inner.connection.browse(&inner.object, &browse).await?;
            if let Some(key) = page.last_key
                && !page.rows.is_empty()
            {
                let mut state = inner.state.lock();
                if state.keys.len() >= MAX_KEYS {
                    state.keys.clear();
                }
                state.keys.insert(request.page, key);
            }
            Ok(LoadedPage {
                columns: page.columns,
                rows: page.rows,
                end: !page.more,
            })
        })
    }

    fn count(&self) -> BoxFuture<'static, Result<RowCount>> {
        let inner = self.inner.clone();
        let (request, reporter) = {
            let state = inner.state.lock();
            (state.query.request(0, 0, None), state.reporter.clone())
        };
        Box::pin(async move {
            let estimate = inner.connection.count(&inner.object, &request, false).await;
            let estimate = match estimate {
                Ok(n) => n,
                Err(Error::Unsupported(_)) => {
                    return inner
                        .connection
                        .count(&inner.object, &request, true)
                        .await
                        .map(RowCount::Exact);
                }
                Err(e) => return Err(e),
            };
            if estimate <= inner.config.exact_count_threshold
                && let Some(reporter) = reporter
            {
                let events = reporter.clone();
                let inner = inner.clone();
                reporter.spawn(Box::pin(async move {
                    // A failed exact count leaves the estimate standing; the person
                    // already has a usable number and an error here would only alarm.
                    if let Ok(exact) = inner.connection.count(&inner.object, &request, true).await {
                        events.report(LoaderEvent::RowCount(RowCount::Exact(exact)));
                    }
                }));
            }
            Ok(RowCount::Estimated(estimate))
        })
    }

    fn attach(&self, reporter: Reporter) {
        self.inner.state.lock().reporter = Some(reporter);
    }

    fn invalidate(&self) {
        self.inner.state.lock().keys.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CacheConfig, GridCache};
    use crate::testing::{FakeConnection, TestExecutor};

    fn loader(connection: &Arc<FakeConnection>, threshold: u64) -> Arc<BrowseLoader> {
        Arc::new(BrowseLoader::new(
            connection.clone(),
            FakeConnection::object(),
            BrowseQuery::default(),
            BrowseConfig {
                exact_count_threshold: threshold,
            },
        ))
    }

    fn config() -> CacheConfig {
        CacheConfig {
            page_rows: 500,
            max_bytes: 64 << 20,
            prefetch_pages: 1,
            max_concurrent_loads: 1,
        }
    }

    #[test]
    fn sequential_paging_seeks_and_jumps_use_offsets() {
        let exec = TestExecutor::new();
        let connection = Arc::new(FakeConnection::new(50_000_000));
        let loader = loader(&connection, 0);
        let cache = GridCache::new(loader.clone(), config(), exec.hooks());
        exec.run();

        // Scroll down page by page from the top: the first request is an offset, every
        // page after it seeks from the key of the page before.
        for page in 0..20u64 {
            cache.set_viewport(page * 500..page * 500 + 50);
            exec.run();
        }
        let stats = connection.stats();
        assert_eq!(stats.offset_requests, 1);
        assert_eq!(
            stats.seek_requests, 20,
            "pages 1 to 20 (the last one prefetched)"
        );
        assert_eq!(loader.stats().seek_requests, 20);

        // A jump has no neighbour: offset for the landing page, then seek for the prefetch.
        cache.set_viewport(40_000_000..40_000_050);
        exec.run();
        let stats = connection.stats();
        assert_eq!(stats.offset_requests, 2);
        assert_eq!(stats.seek_requests, 21);
        assert_eq!(
            cache.with_row(40_000_000, |r| r[0].clone()),
            Some(Value::Int(40_000_000))
        );
    }

    #[test]
    fn keys_outlive_evicted_pages() {
        let exec = TestExecutor::new();
        let connection = Arc::new(FakeConnection::new(1_000_000));
        let loader = loader(&connection, 0);
        // A budget of about two pages.
        let cache = GridCache::new(
            loader.clone(),
            CacheConfig {
                max_bytes: 150_000,
                prefetch_pages: 0,
                ..config()
            },
            exec.hooks(),
        );
        for page in 0..10u64 {
            cache.set_viewport(page * 500..page * 500 + 1);
            exec.run();
        }
        assert!(cache.stats().evictions > 0);
        assert!(!cache.is_loaded(500));
        assert_eq!(loader.remembered_keys(), 10);
        // Page 1 is gone from the cache; reading it again still seeks from page 0's key.
        cache.set_viewport(500..501);
        exec.run();
        assert_eq!(connection.stats().offset_requests, 1);
        assert_eq!(connection.stats().seek_requests, 10);

        // A new sort order makes every key meaningless.
        loader.set_query(BrowseQuery {
            sort: vec![Sort {
                column: "name".into(),
                direction: deltos_data::SortDirection::Descending,
            }],
            ..BrowseQuery::default()
        });
        assert_eq!(loader.remembered_keys(), 0);
        cache.invalidate();
        exec.run();
        assert_eq!(connection.stats().offset_requests, 2);
    }

    #[test]
    fn estimate_first_then_exact_under_threshold() {
        let exec = TestExecutor::new();
        let connection = Arc::new(FakeConnection::new(1234).with_estimate(1200));
        let cache = GridCache::new(loader(&connection, 5_000_000), config(), exec.hooks());
        exec.run();
        assert_eq!(cache.row_count(), RowCount::Exact(1234));
        let stats = connection.stats();
        assert_eq!((stats.estimate_counts, stats.exact_counts), (1, 1));

        let connection = Arc::new(FakeConnection::new(9_000_000).with_estimate(8_500_000));
        let cache = GridCache::new(loader(&connection, 5_000_000), config(), exec.hooks());
        exec.run();
        assert_eq!(cache.row_count(), RowCount::Estimated(8_500_000));
        let stats = connection.stats();
        assert_eq!((stats.estimate_counts, stats.exact_counts), (1, 0));
    }
}
