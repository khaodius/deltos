//! The bounded page cache the table view reads from on every frame.
//!
//! See the crate documentation for the design. In short: rows live in pages of
//! `page_rows`; [`GridCache::set_viewport`] says which rows are on screen and schedules the
//! pages they need plus a little prefetch; completed pages are inserted and the least
//! recently used pages outside the window are evicted until the byte budget holds; reads
//! take one lock and hand out slices without copying.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::mem;
use std::ops::Range;
use std::sync::Arc;

use deltos_data::{Column, Result, Row, Value};
use parking_lot::Mutex;

use crate::columns::{ColumnSet, widen};
use crate::loader::{
    Hooks, LoadedPage, LoaderEvent, PageLoader, PageRequest, Reporter, RowCount, Status,
};

/// Fixed per-row cost counted toward the budget beyond the values themselves: the
/// `Box<[Value]>` fat pointer in the page's vector and the allocation header behind it.
const ROW_OVERHEAD: usize = 32;
/// Fixed per-page cost: the hash map entry, the LRU entry and the row vector.
const PAGE_OVERHEAD: usize = 128;
const VALUE_SIZE: usize = mem::size_of::<Value>();

/// Tuning for one cache. The defaults suit a table view: a page is one network round
/// trip of 500 rows, the budget is 256 MiB, and two pages ahead are fetched in the scroll
/// direction so a steady scroll never shows placeholders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheConfig {
    /// Rows per page. Every load asks for exactly this many rows at a page boundary.
    pub page_rows: u32,
    /// The memory budget for resident pages, counted with [`Value::heap_size`] plus fixed
    /// overheads. Pages on screen are never evicted, so a viewport larger than the budget
    /// is the one case that exceeds it.
    pub max_bytes: usize,
    /// Pages to load beyond the viewport in the direction of scrolling.
    pub prefetch_pages: u32,
    /// Loads in flight at once. The rest wait in a queue that every viewport change
    /// rebuilds, so a page that scrolled out of view before its turn is never fetched.
    pub max_concurrent_loads: u32,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            page_rows: 500,
            max_bytes: 256 * 1024 * 1024,
            prefetch_pages: 2,
            max_concurrent_loads: 2,
        }
    }
}

/// Counters for the status bar and for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    pub resident_pages: usize,
    pub bytes: usize,
    pub in_flight: usize,
    pub queued: usize,
    /// Pages loaded since the cache was created or last invalidated.
    pub loads: u64,
    pub evictions: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Unknown,
    Forward,
    Backward,
}

struct Page {
    rows: Vec<Row>,
    bytes: usize,
    /// Position in the LRU order; larger is more recent.
    tick: u64,
}

impl Page {
    fn new(rows: Vec<Row>, tick: u64) -> Self {
        let bytes = PAGE_OVERHEAD + rows.iter().map(|r| row_bytes(r)).sum::<usize>();
        Self { rows, bytes, tick }
    }
}

fn row_bytes(row: &[Value]) -> usize {
    ROW_OVERHEAD + row.len() * VALUE_SIZE + row.iter().map(Value::heap_size).sum::<usize>()
}

struct Inner {
    config: CacheConfig,
    loader: Arc<dyn PageLoader>,
    /// Bumped by `invalidate`; a result carrying an older generation is dropped.
    generation: u64,
    /// Bumped by `set_loader`; an event from a loader attached under an older number is
    /// from a source that was replaced and is dropped.
    attachment: u64,
    viewport_set: bool,
    tick: u64,
    columns: ColumnSet,
    pages: HashMap<u64, Page>,
    /// Pages by last use, oldest first.
    lru: BTreeMap<u64, u64>,
    bytes: usize,
    viewport: Range<u64>,
    /// Pages the viewport and its prefetch cover; these are never evicted.
    window: Range<u64>,
    direction: Direction,
    queue: VecDeque<u64>,
    in_flight: HashSet<u64>,
    /// Pages whose load failed. They are not asked for again until `invalidate`, so a
    /// failing page does not turn every frame into a new request.
    failed: HashSet<u64>,
    row_count: RowCount,
    counting: bool,
    reading: bool,
    finished: bool,
    /// The last failed page load. The page stays a placeholder until the cache is
    /// invalidated, so the message stays as long.
    load_error: Option<String>,
    /// What the loader itself reported (a broken stream, the spill cap). It stays until
    /// the cache is invalidated, since the condition does.
    source_error: Option<String>,
    stats: CacheStats,
}

struct Shared {
    inner: Mutex<Inner>,
    hooks: Hooks,
}

/// A shared, bounded cache of rows in front of a [`PageLoader`]. Cloning shares the same
/// cache; dropping the last clone lets in-flight loads finish into nothing.
#[derive(Clone)]
pub struct GridCache {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for GridCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stats = self.stats();
        f.debug_struct("GridCache")
            .field("rows", &self.row_count())
            .field("pages", &stats.resident_pages)
            .field("bytes", &stats.bytes)
            .finish()
    }
}

impl GridCache {
    /// Takes the loader, attaches to it and asks for the row count right away.
    pub fn new(loader: Arc<dyn PageLoader>, config: CacheConfig, hooks: Hooks) -> Self {
        let config = CacheConfig {
            page_rows: config.page_rows.max(1),
            max_concurrent_loads: config.max_concurrent_loads.max(1),
            ..config
        };
        let inner = Inner {
            config,
            loader: loader.clone(),
            generation: 0,
            attachment: 0,
            viewport_set: false,
            tick: 0,
            columns: ColumnSet::default(),
            pages: HashMap::new(),
            lru: BTreeMap::new(),
            bytes: 0,
            viewport: 0..0,
            window: 0..0,
            direction: Direction::Unknown,
            queue: VecDeque::new(),
            in_flight: HashSet::new(),
            failed: HashSet::new(),
            row_count: RowCount::Unknown,
            counting: false,
            reading: false,
            finished: false,
            load_error: None,
            source_error: None,
            stats: CacheStats::default(),
        };
        let cache = Self {
            shared: Arc::new(Shared {
                inner: Mutex::new(inner),
                hooks,
            }),
        };
        cache.attach(&loader);
        cache.ensure_count();
        cache
    }

    pub fn config(&self) -> CacheConfig {
        self.shared.inner.lock().config.clone()
    }

    pub fn loader(&self) -> Arc<dyn PageLoader> {
        self.shared.inner.lock().loader.clone()
    }

    /// Swaps the source and drops everything held, columns included. For a sort or
    /// filter change on the same loader, change the loader and call [`GridCache::invalidate`]
    /// instead so the header keeps its columns.
    pub fn set_loader(&self, loader: Arc<dyn PageLoader>) {
        {
            let mut inner = self.shared.inner.lock();
            inner.loader = loader.clone();
            inner.attachment += 1;
            inner.reset();
            inner.columns.clear();
            inner.row_count = RowCount::Unknown;
        }
        self.attach(&loader);
        self.ensure_count();
        self.reschedule();
    }

    /// Records the rows on screen and schedules what they need. Cheap enough to call on
    /// every scroll event; a call with the same range as before does nothing.
    pub fn set_viewport(&self, rows: Range<u64>) {
        let requests = {
            let mut inner = self.shared.inner.lock();
            if rows == inner.viewport {
                return;
            }
            inner.direction = if !inner.viewport_set {
                Direction::Unknown
            } else {
                match rows.start.cmp(&inner.viewport.start) {
                    std::cmp::Ordering::Greater => Direction::Forward,
                    std::cmp::Ordering::Less => Direction::Backward,
                    std::cmp::Ordering::Equal => inner.direction,
                }
            };
            inner.viewport_set = true;
            inner.viewport = rows;
            inner.plan()
        };
        self.start(requests);
    }

    pub fn viewport(&self) -> Range<u64> {
        self.shared.inner.lock().viewport.clone()
    }

    /// Runs `f` on one row's values in merged column order, or returns `None` while the
    /// row is not loaded. The slice is the stored row; nothing is copied.
    pub fn with_row<R>(&self, index: u64, f: impl FnOnce(&[Value]) -> R) -> Option<R> {
        let mut inner = self.shared.inner.lock();
        let inner = &mut *inner;
        let page_rows = u64::from(inner.config.page_rows);
        let width = inner.columns.len();
        let page = inner.pages.get_mut(&(index / page_rows))?;
        let offset = (index % page_rows) as usize;
        let row = page.rows.get_mut(offset)?;
        if row.len() != width {
            let before = row_bytes(row);
            *row = widen(mem::take(row), width);
            let delta = row_bytes(row) as isize - before as isize;
            page.bytes = page.bytes.saturating_add_signed(delta);
            inner.bytes = inner.bytes.saturating_add_signed(delta);
        }
        Some(f(row))
    }

    /// Visits every row of `range` under one lock, loaded or not, in order. The view draws
    /// its whole viewport with one call and a placeholder for each `None`.
    pub fn with_rows(&self, range: Range<u64>, mut f: impl FnMut(u64, Option<&[Value]>)) {
        if range.is_empty() {
            return;
        }
        let mut inner = self.shared.inner.lock();
        let inner = &mut *inner;
        let page_rows = u64::from(inner.config.page_rows);
        let width = inner.columns.len();
        let first_page = range.start / page_rows;
        let last_page = (range.end - 1) / page_rows;
        for page_index in first_page..=last_page {
            let page_start = page_index * page_rows;
            let sub = range.start.max(page_start)..range.end.min(page_start + page_rows);
            match inner.pages.get_mut(&page_index) {
                Some(page) => {
                    let mut delta = 0isize;
                    for index in sub {
                        let offset = (index - page_start) as usize;
                        match page.rows.get_mut(offset) {
                            Some(row) => {
                                if row.len() != width {
                                    let before = row_bytes(row);
                                    *row = widen(mem::take(row), width);
                                    delta += row_bytes(row) as isize - before as isize;
                                }
                                f(index, Some(row));
                            }
                            None => f(index, None),
                        }
                    }
                    page.bytes = page.bytes.saturating_add_signed(delta);
                    inner.bytes = inner.bytes.saturating_add_signed(delta);
                }
                None => {
                    for index in sub {
                        f(index, None);
                    }
                }
            }
        }
    }

    pub fn is_loaded(&self, index: u64) -> bool {
        let inner = self.shared.inner.lock();
        let page_rows = u64::from(inner.config.page_rows);
        inner
            .pages
            .get(&(index / page_rows))
            .is_some_and(|p| ((index % page_rows) as usize) < p.rows.len())
    }

    /// The merged column list, `None` until the first page or column event arrives.
    pub fn columns(&self) -> Option<Arc<[Column]>> {
        self.shared.inner.lock().columns.shared()
    }

    pub fn row_count(&self) -> RowCount {
        self.shared.inner.lock().row_count
    }

    pub fn status(&self) -> Status {
        self.shared.inner.lock().status()
    }

    /// Bytes held by resident pages, as counted against the budget.
    pub fn memory_used(&self) -> usize {
        self.shared.inner.lock().bytes
    }

    pub fn stats(&self) -> CacheStats {
        let inner = self.shared.inner.lock();
        CacheStats {
            resident_pages: inner.pages.len(),
            bytes: inner.bytes,
            in_flight: inner.in_flight.len(),
            queued: inner.queue.len(),
            ..inner.stats
        }
    }

    /// Drops every page and every pending result and reloads the viewport. Call it after
    /// a sort, filter or edit. The column list stays, so the header does not flicker; an
    /// exact row count becomes an estimate until the loader counts again.
    pub fn invalidate(&self) {
        let loader = {
            let mut inner = self.shared.inner.lock();
            inner.reset();
            if let RowCount::Exact(n) = inner.row_count {
                inner.row_count = RowCount::Estimated(n);
            }
            inner.loader.clone()
        };
        loader.invalidate();
        self.ensure_count();
        self.reschedule();
        (self.shared.hooks.notify)();
    }

    fn attach(&self, loader: &Arc<dyn PageLoader>) {
        let weak = Arc::downgrade(&self.shared);
        let attachment = self.shared.inner.lock().attachment;
        let send = Arc::new(move |event: LoaderEvent| {
            if let Some(shared) = weak.upgrade() {
                Shared::on_event(&shared, attachment, event);
            }
        });
        loader.attach(Reporter::new(self.shared.hooks.clone(), send));
    }

    fn reschedule(&self) {
        let requests = self.shared.inner.lock().plan();
        self.start(requests);
    }

    fn ensure_count(&self) {
        let (loader, generation) = {
            let mut inner = self.shared.inner.lock();
            if inner.counting {
                return;
            }
            inner.counting = true;
            (inner.loader.clone(), inner.generation)
        };
        let weak = Arc::downgrade(&self.shared);
        (self.shared.hooks.spawn)(Box::pin(async move {
            let result = loader.count().await;
            if let Some(shared) = weak.upgrade() {
                Shared::on_count(&shared, generation, result);
            }
        }));
    }

    /// Spawns the loads `plan` decided on. Runs with the lock released: a loader may take
    /// its own lock while building a future and may report events while holding it.
    fn start(&self, requests: Vec<(PageRequest, u64, Arc<dyn PageLoader>)>) {
        for (request, generation, loader) in requests {
            let weak = Arc::downgrade(&self.shared);
            let future = loader.load(request);
            (self.shared.hooks.spawn)(Box::pin(async move {
                let result = future.await;
                if let Some(shared) = weak.upgrade() {
                    Shared::on_page(&shared, request, generation, result);
                }
            }));
        }
    }
}

impl Shared {
    fn on_page(
        shared: &Arc<Shared>,
        request: PageRequest,
        generation: u64,
        result: Result<LoadedPage>,
    ) {
        let requests = {
            let mut inner = shared.inner.lock();
            if inner.generation != generation {
                return;
            }
            inner.in_flight.remove(&request.page);
            match result {
                Ok(page) => inner.insert(request, page),
                Err(e) => {
                    inner.failed.insert(request.page);
                    inner.load_error = Some(e.to_string());
                }
            }
            inner.pump()
        };
        let cache = GridCache {
            shared: shared.clone(),
        };
        cache.start(requests);
        (shared.hooks.notify)();
    }

    fn on_count(shared: &Arc<Shared>, generation: u64, result: Result<RowCount>) {
        {
            let mut inner = shared.inner.lock();
            inner.counting = false;
            if inner.generation != generation {
                return;
            }
            match result {
                Ok(count) => inner.row_count.refine(count),
                Err(e) => inner.load_error = Some(e.to_string()),
            }
        }
        (shared.hooks.notify)();
    }

    fn on_event(shared: &Arc<Shared>, attachment: u64, event: LoaderEvent) {
        {
            let mut inner = shared.inner.lock();
            // Events from a loader that `set_loader` replaced are stale. A plain
            // `invalidate` keeps the loader, and its events stay welcome.
            if attachment != inner.attachment {
                return;
            }
            match event {
                LoaderEvent::Columns(columns) => {
                    if inner.columns.is_empty() {
                        inner.columns.merge(&columns);
                    }
                }
                LoaderEvent::RowCount(count) => inner.row_count.refine(count),
                LoaderEvent::Reading(reading) => inner.reading = reading,
                LoaderEvent::Finished => inner.finished = true,
                LoaderEvent::Error(message) => inner.source_error = Some(message),
            }
        }
        (shared.hooks.notify)();
    }
}

impl Inner {
    fn status(&self) -> Status {
        if let Some(error) = self.source_error.as_ref().or(self.load_error.as_ref()) {
            return Status::Error(error.clone());
        }
        if self.reading || self.counting || !self.in_flight.is_empty() || !self.queue.is_empty() {
            return Status::Loading;
        }
        if self.finished {
            Status::Done
        } else {
            Status::Idle
        }
    }

    /// Forgets every page and pending result; the next generation starts empty.
    fn reset(&mut self) {
        self.generation += 1;
        self.pages.clear();
        self.lru.clear();
        self.bytes = 0;
        self.queue.clear();
        self.in_flight.clear();
        self.failed.clear();
        self.load_error = None;
        self.source_error = None;
        self.finished = false;
        self.stats = CacheStats::default();
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn touch(&mut self, page_index: u64) {
        let tick = self.next_tick();
        if let Some(page) = self.pages.get_mut(&page_index) {
            self.lru.remove(&page.tick);
            page.tick = tick;
            self.lru.insert(tick, page_index);
        }
    }

    /// Works out the window for the current viewport, marks its pages most recently
    /// used, rebuilds the queue and returns the loads to start now.
    fn plan(&mut self) -> Vec<(PageRequest, u64, Arc<dyn PageLoader>)> {
        let page_rows = u64::from(self.config.page_rows);
        let mut rows = self.viewport.clone();
        if let RowCount::Exact(n) = self.row_count {
            rows.end = rows.end.min(n);
        }
        self.queue.clear();
        if rows.start >= rows.end {
            self.window = 0..0;
            return Vec::new();
        }
        let first = rows.start / page_rows;
        let last = (rows.end - 1) / page_rows;
        let prefetch = u64::from(self.config.prefetch_pages);
        let (ahead, behind) = match self.direction {
            Direction::Forward => (prefetch, 0),
            Direction::Backward => (0, prefetch),
            Direction::Unknown => (prefetch, prefetch),
        };
        let window_start = first.saturating_sub(behind);
        let window_end = last.saturating_add(ahead).saturating_add(1);
        self.window = window_start..window_end;

        for page in first..=last {
            self.touch(page);
        }
        // Past an exact count there is nothing to fetch.
        let last_page = match self.row_count {
            RowCount::Exact(n) => (n.saturating_sub(1)) / page_rows,
            _ => u64::MAX,
        };
        let mut wanted: Vec<u64> = (first..=last).collect();
        for distance in 1..=prefetch {
            if distance <= ahead {
                wanted.push(last + distance);
            }
            if distance <= behind && first >= distance {
                wanted.push(first - distance);
            }
        }
        for page in wanted {
            if page <= last_page
                && !self.pages.contains_key(&page)
                && !self.in_flight.contains(&page)
                && !self.failed.contains(&page)
            {
                self.queue.push_back(page);
            }
        }
        self.pump()
    }

    /// Moves queued pages into flight up to the concurrency limit.
    fn pump(&mut self) -> Vec<(PageRequest, u64, Arc<dyn PageLoader>)> {
        let mut out = Vec::new();
        let limit = self.config.max_concurrent_loads as usize;
        while self.in_flight.len() < limit {
            let Some(page) = self.queue.pop_front() else {
                break;
            };
            if self.in_flight.insert(page) {
                out.push((
                    PageRequest::new(page, self.config.page_rows),
                    self.generation,
                    self.loader.clone(),
                ));
            }
        }
        out
    }

    fn insert(&mut self, request: PageRequest, loaded: LoadedPage) {
        self.stats.loads += 1;
        let map = self.columns.merge(&loaded.columns);
        let width = self.columns.len();
        let rows: Vec<Row> = loaded
            .rows
            .into_iter()
            .map(|row| map.align(row, width))
            .collect();

        let end_row = request.start + rows.len() as u64;
        if loaded.end && rows.is_empty() {
            // The source ended somewhere before this page. All that is learned is an
            // upper bound, so an estimate that pointed past it comes down to it.
            if self.row_count.value().is_none_or(|n| n > request.start) {
                self.row_count = RowCount::Estimated(request.start);
            }
        } else if loaded.end {
            self.row_count.refine(RowCount::Exact(end_row));
        } else if !rows.is_empty() {
            self.row_count.refine(RowCount::AtLeast(end_row));
        }

        let tick = self.next_tick();
        let page = Page::new(rows, tick);
        self.bytes += page.bytes;
        if let Some(old) = self.pages.insert(request.page, page) {
            self.lru.remove(&old.tick);
            self.bytes -= old.bytes;
        }
        self.lru.insert(tick, request.page);
        self.evict();
    }

    /// Drops least recently used pages outside the window until the budget holds.
    fn evict(&mut self) {
        while self.bytes > self.config.max_bytes {
            let victim = self
                .lru
                .iter()
                .map(|(tick, page)| (*tick, *page))
                .find(|(_, page)| !self.window.contains(page));
            let Some((tick, page_index)) = victim else {
                break;
            };
            self.lru.remove(&tick);
            if let Some(page) = self.pages.remove(&page_index) {
                self.bytes -= page.bytes;
                self.stats.evictions += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeLoader, TestExecutor, fake_row};
    use deltos_data::ValueKind;

    fn config(max_bytes: usize) -> CacheConfig {
        CacheConfig {
            page_rows: 500,
            max_bytes,
            prefetch_pages: 2,
            max_concurrent_loads: 2,
        }
    }

    #[test]
    fn jump_to_row_40m_loads_only_nearby_pages() {
        let exec = TestExecutor::new();
        let loader = Arc::new(FakeLoader::new(50_000_000));
        let cache = GridCache::new(loader.clone(), config(64 << 20), exec.hooks());
        exec.run();
        assert_eq!(cache.row_count(), RowCount::Exact(50_000_000));

        cache.set_viewport(40_000_000..40_000_050);
        exec.run();
        let pages = loader.requested_pages();
        assert_eq!(
            pages.len(),
            5,
            "the viewport page and two prefetch pages each side: {pages:?}"
        );
        assert!(
            pages.iter().all(|p| (79_998..=80_002).contains(p)),
            "{pages:?}"
        );
        assert_eq!(cache.stats().resident_pages, 5);
        assert_eq!(
            cache.with_row(40_000_007, |row| row.to_vec()),
            Some(fake_row(40_000_007, 4).to_vec())
        );
        assert!(cache.with_row(0, |_| ()).is_none());
        assert!(cache.with_row(49_999_999, |_| ()).is_none());
        assert_eq!(cache.status(), Status::Idle);
    }

    #[test]
    fn memory_stays_under_budget_while_scrolling_two_million_rows() {
        let exec = TestExecutor::new();
        let loader = Arc::new(FakeLoader::new(50_000_000));
        let budget = 4 << 20;
        let cache = GridCache::new(loader.clone(), config(budget), exec.hooks());
        let mut top = 0u64;
        while top < 2_000_000 {
            cache.set_viewport(top..top + 60);
            exec.run();
            assert!(
                cache.memory_used() <= budget,
                "{} bytes resident at row {top}",
                cache.memory_used()
            );
            cache.with_rows(top..top + 60, |index, row| {
                let row = row.expect("the viewport is loaded once the executor is idle");
                assert_eq!(row[0], Value::Int(index as i64));
            });
            top += 250;
        }
        let stats = cache.stats();
        assert!(stats.evictions > 0);
        assert!(stats.resident_pages * 500 < 2_000_000);
        assert_eq!(stats.loads, loader.requested_pages().len() as u64);
        // Every page was asked for once: nothing left the window before its turn.
        assert_eq!(stats.loads, 4000 + 2);
    }

    #[test]
    fn eviction_never_drops_viewport_pages() {
        let exec = TestExecutor::new();
        let loader = Arc::new(FakeLoader::new(1_000_000));
        let no_prefetch = |max_bytes| CacheConfig {
            prefetch_pages: 0,
            ..config(max_bytes)
        };
        let page_bytes = {
            let cache = GridCache::new(loader.clone(), no_prefetch(usize::MAX), exec.hooks());
            cache.set_viewport(0..1);
            exec.run();
            cache.memory_used()
        };

        // Room for one and a half pages, with a viewport that spans three.
        let cache = GridCache::new(
            loader.clone(),
            no_prefetch(page_bytes + page_bytes / 2),
            exec.hooks(),
        );
        cache.set_viewport(1200..2300);
        exec.run();
        for page in 2..=4 {
            assert!(
                cache.is_loaded(page * 500),
                "page {page} must stay resident"
            );
        }
        assert!(
            cache.memory_used() > cache.config().max_bytes,
            "the viewport wins over the budget"
        );
        // Scrolling on releases the old viewport pages.
        cache.set_viewport(10_000..10_010);
        exec.run();
        assert!(cache.is_loaded(10_000));
        assert!(!cache.is_loaded(1200));
        assert!(cache.memory_used() <= cache.config().max_bytes);

        // The oldest page outside the viewport goes first.
        let cache = GridCache::new(
            loader.clone(),
            no_prefetch(page_bytes * 2 + page_bytes / 2),
            exec.hooks(),
        );
        for page in 0..3 {
            cache.set_viewport(page * 500..page * 500 + 1);
            exec.run();
        }
        assert!(!cache.is_loaded(0));
        assert!(cache.is_loaded(500));
        assert!(cache.is_loaded(1000));
        cache.set_viewport(1000..1501);
        exec.run();
        assert!(!cache.is_loaded(500));
        assert!(cache.is_loaded(1000));
        assert!(cache.is_loaded(1500));
        assert_eq!(cache.stats().evictions, 2);
    }

    #[test]
    fn prefetch_follows_scroll_direction() {
        let exec = TestExecutor::new();
        let loader = Arc::new(FakeLoader::new(1_000_000));
        let cache = GridCache::new(loader.clone(), config(64 << 20), exec.hooks());
        cache.set_viewport(5000..5050);
        exec.run();
        let mut pages = loader.requested_pages();
        pages.sort_unstable();
        assert_eq!(pages, [8, 9, 10, 11, 12], "no direction yet: both sides");
        loader.clear_requests();
        cache.set_viewport(4000..4050);
        exec.run();
        let mut pages = loader.requested_pages();
        pages.sort_unstable();
        assert_eq!(pages, [6, 7], "scrolling up prefetches above only");
    }

    #[test]
    fn column_growth_merges_by_name() {
        let exec = TestExecutor::new();
        let loader = Arc::new(FakeLoader::new(5000));
        loader.set_columns_for_page(|page| {
            let names: &[&str] = match page {
                0 => &["id", "name"],
                1 => &["name", "score", "id"],
                _ => &["extra", "id"],
            };
            names
                .iter()
                .map(|n| Column::new(*n, "x", ValueKind::Text))
                .collect()
        });
        let cache = GridCache::new(
            loader,
            CacheConfig {
                prefetch_pages: 0,
                ..config(64 << 20)
            },
            exec.hooks(),
        );
        cache.set_viewport(0..10);
        exec.run();
        let names = |cache: &GridCache| -> Vec<String> {
            cache
                .columns()
                .unwrap()
                .iter()
                .map(|c| c.name.clone())
                .collect()
        };
        assert_eq!(names(&cache), ["id", "name"]);
        assert_eq!(cache.with_row(3, |r| r.len()), Some(2));

        cache.set_viewport(500..510);
        exec.run();
        assert_eq!(names(&cache), ["id", "name", "score"]);
        // Page 1 came as [name, score, id] and is stored permuted.
        let row = cache.with_row(502, |r| r.to_vec()).unwrap();
        let source = fake_row(502, 3);
        assert_eq!(
            row,
            vec![source[2].clone(), source[0].clone(), source[1].clone()]
        );
        // Page 0 was stored two wide and is widened on read.
        let before = cache.memory_used();
        let row = cache.with_row(3, |r| r.to_vec()).unwrap();
        assert_eq!(row.len(), 3);
        assert_eq!(row[2], Value::Null);
        assert!(cache.memory_used() > before, "widening is counted");
        let again = cache.memory_used();
        cache.with_row(3, |_| ());
        assert_eq!(cache.memory_used(), again, "widening happens once");

        cache.set_viewport(1000..1010);
        exec.run();
        assert_eq!(names(&cache), ["id", "name", "score", "extra"]);
        cache.with_rows(498..1002, |index, row| {
            let row = row.unwrap();
            assert_eq!(row.len(), 4, "row {index}");
        });
        // Page 2 came as [extra, id]: "id" lands in slot 0, "extra" in the new slot 3.
        let row = cache.with_row(1001, |r| r.to_vec()).unwrap();
        let source = fake_row(1001, 2);
        assert_eq!(
            row,
            vec![
                source[1].clone(),
                Value::Null,
                Value::Null,
                source[0].clone()
            ]
        );
    }

    #[test]
    fn invalidate_drops_stale_in_flight_results() {
        let exec = TestExecutor::new();
        let loader = Arc::new(FakeLoader::new(10_000));
        loader.hold();
        let cache = GridCache::new(loader.clone(), config(64 << 20), exec.hooks());
        cache.set_viewport(0..10);
        exec.run();
        assert_eq!(loader.held(), 2, "two loads wait on the gate");
        assert_eq!(cache.status(), Status::Loading);

        cache.invalidate();
        exec.run();
        assert_eq!(
            loader.held(),
            4,
            "the old loads still wait; new ones were issued"
        );

        loader.release_oldest(2);
        exec.run();
        assert!(
            cache.with_row(0, |_| ()).is_none(),
            "stale results are dropped"
        );
        assert_eq!(cache.stats().loads, 0);

        loader.release_oldest(2);
        exec.run();
        assert_eq!(cache.with_row(0, |r| r[0].clone()), Some(Value::Int(0)));
        assert_eq!(cache.stats().loads, 2);
    }

    #[test]
    fn concurrency_limit_and_queue_rebuild() {
        let exec = TestExecutor::new();
        let loader = Arc::new(FakeLoader::new(1_000_000));
        loader.hold();
        let cache = GridCache::new(loader.clone(), config(64 << 20), exec.hooks());
        cache.set_viewport(0..1500);
        exec.run();
        assert_eq!(loader.held(), 2);
        assert_eq!(
            cache.stats().queued,
            3,
            "one viewport page and two prefetch pages wait"
        );
        // A jump replaces the queue; the two in-flight loads cannot be recalled.
        cache.set_viewport(500_000..500_010);
        exec.run();
        assert_eq!(loader.held(), 2);
        let stats = cache.stats();
        assert_eq!(stats.in_flight, 2);
        assert_eq!(stats.queued, 3);
        loader.release_oldest(2);
        exec.run();
        assert_eq!(loader.held(), 2, "two more started once slots freed");
        assert!(loader.requested_pages().contains(&1000));
        loader.release_all();
        exec.run();
        loader.release_all();
        exec.run();
        assert!(cache.is_loaded(500_000));
        assert_eq!(cache.stats().in_flight, 0);
        assert_eq!(cache.status(), Status::Idle);
    }

    #[test]
    fn row_count_is_refined_from_page_ends() {
        let exec = TestExecutor::new();
        let loader = Arc::new(FakeLoader::new(1234));
        loader.set_count(RowCount::Estimated(1000));
        let cache = GridCache::new(loader.clone(), config(64 << 20), exec.hooks());
        exec.run();
        assert_eq!(cache.row_count(), RowCount::Estimated(1000));
        cache.set_viewport(990..1000);
        exec.run();
        // Page 1 (500..1000) is not the end, page 2 (1000..1234) is, and the empty page 3
        // that was already queued must not move the count.
        assert_eq!(cache.row_count(), RowCount::Exact(1234));
        let loads = cache.stats().loads;
        cache.set_viewport(5000..5010);
        exec.run();
        assert_eq!(
            cache.stats().loads,
            loads,
            "nothing is asked for past an exact end"
        );
        assert!(cache.with_row(1233, |_| ()).is_some());

        // An estimate that points past the real end comes down when an empty page proves it.
        let loader = Arc::new(FakeLoader::new(700));
        loader.set_count(RowCount::Estimated(5000));
        let cache = GridCache::new(loader, config(64 << 20), exec.hooks());
        exec.run();
        cache.set_viewport(4000..4010);
        exec.run();
        assert_eq!(cache.row_count(), RowCount::Estimated(3000));
    }

    #[test]
    fn notify_fires_per_page_not_per_row() {
        let exec = TestExecutor::new();
        let loader = Arc::new(FakeLoader::new(1_000_000));
        let cache = GridCache::new(loader, config(64 << 20), exec.hooks());
        exec.run();
        let before = exec.notifications();
        cache.set_viewport(0..1000);
        exec.run();
        let fired = exec.notifications() - before;
        assert!(
            (4..=5).contains(&fired),
            "one per page (plus nothing per row): {fired}"
        );
    }

    #[test]
    fn failed_pages_are_not_retried_until_invalidate() {
        let exec = TestExecutor::new();
        let loader = Arc::new(FakeLoader::new(10_000));
        loader.fail_page(1);
        let cache = GridCache::new(loader.clone(), config(64 << 20), exec.hooks());
        cache.set_viewport(500..510);
        exec.run();
        assert!(matches!(cache.status(), Status::Error(_)));
        cache.set_viewport(500..520);
        cache.set_viewport(500..530);
        exec.run();
        assert_eq!(
            loader.requested_pages().iter().filter(|p| **p == 1).count(),
            1
        );
        loader.clear_failures();
        cache.invalidate();
        exec.run();
        assert!(cache.is_loaded(500));
        assert_eq!(cache.status(), Status::Idle);
    }

    #[test]
    fn set_loader_replaces_everything() {
        let exec = TestExecutor::new();
        let first = Arc::new(FakeLoader::new(100));
        let cache = GridCache::new(first, config(64 << 20), exec.hooks());
        cache.set_viewport(0..10);
        exec.run();
        assert_eq!(cache.row_count(), RowCount::Exact(100));
        let second = Arc::new(FakeLoader::new(10));
        second.set_columns_for_page(|_| vec![Column::new("only", "x", ValueKind::Int)]);
        cache.set_loader(second);
        exec.run();
        assert_eq!(cache.row_count(), RowCount::Exact(10));
        assert_eq!(cache.columns().unwrap().len(), 1);
        assert_eq!(cache.with_row(3, |r| r.len()), Some(1));
    }
}
