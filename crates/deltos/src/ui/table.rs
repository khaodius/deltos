//! A table, view or collection: its rows in the grid with a filter bar and a status bar,
//! or its structure (columns, indexes, foreign keys). Rows come through a
//! [`BrowseLoader`], which seeks by key, so any part of any size of table opens at once.
//!
//! Edits are staged: changed cells, deleted rows and new rows wait in the grid's
//! [`PendingChanges`](crate::ui::pending::PendingChanges) until the person reviews and
//! commits them, which runs them in one call to the driver (one transaction where the
//! database has them). The "Commit edits immediately" setting writes each edit at once
//! instead.

use std::sync::Arc;

use std::rc::Rc;
use std::time::Duration;

use deltos_data::{
    DriverKind, FilterOp, ObjectInfo, ObjectKind, ObjectRef, RowEdit, Sort, SortDirection, Value,
};
use deltos_dock::PanelId;
use deltos_grid::{BrowseConfig, BrowseLoader, BrowseQuery, CacheConfig, GridCache, Status};
use deltos_settings::PanelKind;
use gpui::{
    AnyElement, App, AppContext, ClipboardItem, Context, Entity, FocusHandle, FontWeight,
    IntoElement, Render, SharedString, Subscription, Window, actions, div, prelude::*, px,
    uniform_list,
};

use crate::net::Net;
use crate::state::{AppState, Inspected, RowSource, StateEvent};
use crate::tasks::{self, CacheFollower, TaskOutcome};
use crate::theme::{Theme, theme};
use crate::ui::edit;
use crate::ui::grid::{Batch, CellInput, GridEvent, GridView, MenuTarget, RowRef};
use crate::ui::icons::Icon;
use crate::ui::pending::RowMark;
use crate::ui::structure::{StructureEvent, StructureView};
use crate::ui::{
    ButtonStyle, Tooltip, button, empty_state, icon_button, menu, segmented, thousands,
};

mod filters;
mod links;
mod memory;
mod pager;

pub use links::open_filtered;

actions!(
    table,
    [
        CommitChanges,
        ReviewChanges,
        DiscardChanges,
        AddRow,
        DuplicateRows,
        /// Shows the filter bar and puts the cursor in it.
        FocusFilter,
        /// Opens the row the selected cell's foreign key refers to.
        FollowReference,
    ]
);

/// What the inspector asks of the table its row came from.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// Stage `input` for column `col` of grid row `row`, or take the field's staged change
    /// back when `input` is `None`. `target` is the row the inspector shows, which goes
    /// first when the rows moved since and another row is at `row` now.
    Stage {
        row: u64,
        col: usize,
        input: Option<CellInput>,
        target: RowRef,
    },
    Commit,
    Review,
    Discard,
}

/// Most rows one delete, duplicate or restore takes, so a select-all over a huge table
/// stays a request the person can review.
const ROW_ACTION_LIMIT: usize = 10_000;

/// How a batch of cell changes went, for the status bar.
#[derive(Default)]
struct Tally {
    done: usize,
    nulls: usize,
    empties: usize,
    unloaded: usize,
    refused: usize,
    first_refusal: Option<String>,
}

impl Tally {
    fn refuse(&mut self, why: impl Into<String>) {
        self.refused += 1;
        self.first_refusal.get_or_insert_with(|| why.into());
    }
}

/// A row a batch of cell changes goes to, shared by the row's cells.
#[derive(Clone)]
enum Target {
    New(usize),
    Table(Rc<[Value]>),
}

/// "1 cell", "12 cells".
fn cells_text(n: usize) -> String {
    if n == 1 {
        "1 cell".to_owned()
    } else {
        format!("{} cells", thousands(n as u64))
    }
}

/// The commit review: the statements a commit runs, as they were when it opened.
struct Review {
    statements: Arc<[SharedString]>,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Data,
    Structure,
}

pub struct TablePanel {
    state: Entity<AppState>,
    connection: String,
    object: ObjectRef,
    grid: Entity<GridView>,
    loader: Option<Arc<BrowseLoader>>,
    mode: Mode,
    info: Option<Result<ObjectInfo, String>>,
    sort: Vec<Sort>,
    filters: filters::Filters,
    pager: pager::Pager,
    links: links::Resolved,
    /// The database behind the table, once connected.
    driver: Option<DriverKind>,
    /// The last edit's outcome, shown in the status bar: the text and whether it failed.
    notice: Option<(SharedString, bool)>,
    notice_generation: u64,
    /// Rows waiting for the person to confirm they should be deleted.
    confirm_delete: Option<Vec<u64>>,
    /// Waiting for the person to confirm that staged changes go.
    confirm_discard: bool,
    /// A commit is running; staging waits for it.
    committing: bool,
    /// The tab was made a real tab, so staged changes keep it open.
    pinned: bool,
    /// The inspector waits for its row to be read again, to show it.
    reinspect: bool,
    /// Why the last commit failed, until something changes.
    commit_error: Option<SharedString>,
    review: Option<Review>,
    review_focus: FocusHandle,
    /// The status bar's import and export menu is open.
    transfer_menu: bool,
    structure: Option<Entity<StructureView>>,
    /// Loads and counts, in the task list.
    loading: CacheFollower,
    _subs: Vec<Subscription>,
}

impl TablePanel {
    pub fn new(
        state: Entity<AppState>,
        _id: PanelId,
        connection: String,
        object: ObjectRef,
        cx: &mut Context<Self>,
    ) -> Self {
        let grid = cx.new(GridView::new);
        let subs = vec![
            cx.subscribe(&grid, |this, _, event, cx| match event {
                GridEvent::Sort(c) => this.cycle_sort(*c, cx),
                GridEvent::Selected(row) => this.inspect_row(*row, cx),
                GridEvent::Edit {
                    row,
                    col,
                    input,
                    target,
                } => this.save_cell(*row, *col, input.clone(), target.clone(), cx),
                GridEvent::Edits {
                    cells,
                    batch,
                    outside,
                    limited,
                } => this.save_cells(cells, *batch, *outside, *limited, cx),
                GridEvent::FollowLink { row, col } => this.follow_link(*row, *col, cx),
            }),
            cx.subscribe(&state, |this, _, event, cx| {
                if let StateEvent::TableRequest { owner, request } = event {
                    if *owner == cx.entity_id() {
                        this.handle_request(request.clone(), cx);
                    }
                    return;
                }
                if let StateEvent::Sessions = event {
                    // A connection opened again (after an SSH tunnel came back, or a
                    // reconnect) has a new handle; load from that one.
                    let reopened = this.loader.as_ref().is_some_and(|l| {
                        this.state
                            .read(cx)
                            .session(&this.connection)
                            .is_some_and(|s| !Arc::ptr_eq(&s.conn, l.connection()))
                    });
                    if this.loader.is_none() || reopened {
                        this.loader = None;
                        this.attach(cx);
                    }
                }
            }),
            cx.observe(&grid, |this, _, cx| {
                if this.reinspect {
                    this.refresh_inspector(cx);
                }
                this.sync_links(cx);
                cx.notify()
            }),
            // A closed table's row leaves the inspector with it.
            cx.on_release({
                let id = cx.entity_id();
                move |this, cx| {
                    this.state.update(cx, |s, cx| {
                        if s.inspected
                            .as_ref()
                            .is_some_and(|i| i.source.owner == Some(id))
                        {
                            s.inspect(None, cx);
                        }
                    })
                }
            }),
        ];
        let filters = filters::Filters::new(cx);
        let pager = pager::Pager::new(cx);
        let mut this = Self {
            state,
            connection,
            object,
            grid,
            loader: None,
            mode: Mode::Data,
            info: None,
            sort: Vec::new(),
            filters,
            pager,
            links: links::Resolved::default(),
            driver: None,
            notice: None,
            notice_generation: 0,
            confirm_delete: None,
            confirm_discard: false,
            committing: false,
            pinned: false,
            reinspect: false,
            commit_error: None,
            review: None,
            review_focus: cx.focus_handle(),
            transfer_menu: false,
            structure: None,
            loading: CacheFollower::default(),
            _subs: subs,
        };
        let weak = cx.entity().downgrade();
        this.grid.update(cx, |g, _| {
            g.set_menu_provider(Rc::new(move |target, grid, cx| {
                weak.upgrade()
                    .map(|t| t.read(cx).menu_items(target, grid, &t, cx))
                    .unwrap_or_default()
            }))
        });
        // The sort and filters this table had earlier in the session.
        if let Some(view) = memory::recall(cx, &this.connection, &this.object) {
            this.apply_view(view, cx);
        }
        this.take_requested(cx);
        this.attach(cx);
        this
    }

    pub fn title(&self) -> SharedString {
        self.object.name.clone().into()
    }

    pub fn connection_id(&self) -> &str {
        &self.connection
    }

    pub fn icon(&self) -> Icon {
        match self.object.kind {
            ObjectKind::View | ObjectKind::MaterializedView => Icon::View,
            ObjectKind::Collection => Icon::Collection,
            _ => Icon::Table,
        }
    }

    pub fn on_shown(&mut self, cx: &mut Context<Self>) {
        let requested = self.take_requested(cx);
        if self.loader.is_none() {
            self.attach(cx);
        } else if requested {
            self.requery(cx);
        } else {
            // A tab opened in the background loads once it is first shown.
            self.follow_loading(cx);
        }
    }

    /// Lists the grid's loading (its pages and its count) as a task until it stops.
    /// It has no Cancel: the drivers can only cancel everything a connection runs (on
    /// MySQL, only the query tab's statement), so a Cancel here would stop an unrelated
    /// query on the same connection, and on MySQL not even the load.
    fn follow_loading(&mut self, cx: &mut Context<Self>) {
        let Some(cache) = self.grid.read(cx).cache().cloned() else {
            return;
        };
        let s = self.state.read(cx);
        let Some(session) = s.session(&self.connection) else {
            return;
        };
        let name = Some(SharedString::from(session.name.clone()));
        let label = format!("Loading {}", self.object.name);
        self.loading.follow(cache, label.into(), name, None, cx);
    }

    /// Starts loading once the connection is open.
    fn attach(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = self
            .state
            .read(cx)
            .session(&self.connection)
            .map(|s| s.conn.clone())
        else {
            return;
        };
        let data = self.state.read(cx).settings.data.clone();
        self.driver = Some(conn.kind());
        let editable = conn.capabilities().edit_rows
            && matches!(self.object.kind, ObjectKind::Table | ObjectKind::Collection);
        self.grid.update(cx, |g, _| g.editable = editable);
        let loader = Arc::new(BrowseLoader::new(
            conn.clone(),
            self.object.clone(),
            self.query(cx),
            BrowseConfig {
                exact_count_threshold: if data.exact_counts {
                    u64::MAX
                } else {
                    BrowseConfig::default().exact_count_threshold
                },
            },
        ));
        let config = CacheConfig {
            page_rows: data.page_size,
            max_bytes: data.cache_memory_mb as usize * 1024 * 1024,
            ..CacheConfig::default()
        };
        self.grid.update(cx, |g, cx| {
            let hooks = g.make_hooks(cx);
            let cache = Arc::new(GridCache::new(loader.clone(), config, hooks));
            g.set_cache(Some(cache), false, cx);
        });
        self.loader = Some(loader);
        self.follow_loading(cx);
        // The structure loads alongside, for the structure view and the filter columns.
        let object = self.object.clone();
        let job = cx
            .global::<Net>()
            .run(async move { conn.describe(&object).await.map_err(|e| e.to_string()) });
        cx.spawn(async move |this, cx| {
            let info = job.await.unwrap_or_else(|| Err("cancelled".into()));
            this.update(cx, |this, cx| {
                this.info = Some(info);
                this.sync_structure(cx);
                this.sync_links(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn query(&self, cx: &App) -> BrowseQuery {
        let (filters, match_any, raw_where) = self.filter_parts(cx);
        BrowseQuery {
            sort: self.sort.clone(),
            filters,
            match_any,
            raw_where,
        }
    }

    /// Re-reads with the current sort and filters, keeping the column widths. Staged
    /// changes stay: they are keyed by primary key and find their rows again.
    fn requery(&mut self, cx: &mut Context<Self>) {
        let view = self.saved_view(cx);
        memory::remember(cx, &self.connection, &self.object, view);
        let Some(loader) = self.loader.clone() else {
            return;
        };
        loader.set_query(self.query(cx));
        self.forget_count(cx);
        let columns = self.grid.read(cx).columns().clone();
        let sort: Vec<(usize, SortDirection)> = self
            .sort
            .iter()
            .filter_map(|s| {
                columns
                    .iter()
                    .position(|c| c.name == s.column)
                    .map(|i| (i, s.direction))
            })
            .collect();
        self.grid.update(cx, |g, cx| {
            if let Some(cache) = g.cache() {
                cache.invalidate();
            }
            g.selection = None;
            g.set_sort(sort, cx);
            g.refreshed(cx);
        });
        // Nothing is selected any more, so nothing of this table is inspected either.
        let id = cx.entity_id();
        self.reinspect = false;
        self.state.update(cx, |s, cx| {
            if s.inspected
                .as_ref()
                .is_some_and(|i| i.source.owner == Some(id))
            {
                s.inspect(None, cx);
            }
        });
        self.follow_loading(cx);
        cx.notify();
    }

    fn handle_request(&mut self, request: Request, cx: &mut Context<Self>) {
        match request {
            Request::Stage {
                row,
                col,
                input: Some(input),
                target,
            } => self.save_cell(row, col, input, Some(target), cx),
            Request::Stage {
                row,
                col,
                input: None,
                target,
            } => self.revert_cell(row, col, Some(target), cx),
            Request::Commit => self.commit(None, cx),
            Request::Review if self.pending_count(cx) > 0 => self.open_review(cx),
            Request::Review => {}
            Request::Discard if self.pending_count(cx) > 0 => {
                self.confirm_discard = true;
                cx.notify();
            }
            Request::Discard => {}
        }
    }

    /// Reads the rows again where they are, keeping the selection, after a commit.
    fn reload(&mut self, cx: &mut Context<Self>) {
        self.grid.update(cx, |g, cx| {
            if let Some(cache) = g.cache() {
                cache.invalidate();
            }
            g.refreshed(cx);
        });
        cx.notify();
    }

    /// Shows `row` in the inspector, staged values in, with what it needs to stage
    /// changes to its fields here.
    fn inspect_row(&mut self, row: u64, cx: &mut Context<Self>) {
        let g = self.grid.read(cx);
        let columns = g.columns().clone();
        let Some(values) = g.row_values(row) else {
            return;
        };
        let mark = g.row_mark(row);
        let target = g.row_ref(row);
        let original = match &target {
            Some(RowRef::Table(raw)) => match g.pending.row(raw) {
                Some(change) => (0..columns.len())
                    .map(|c| change.cell(c).map(|cell| cell.original.clone()))
                    .collect(),
                None => Vec::new(),
            },
            _ => Vec::new(),
        };
        let read_only = if !g.editable {
            Some(
                match self.object.kind {
                    ObjectKind::Table | ObjectKind::Collection => {
                        "This connection can't edit rows here"
                    }
                    _ => "Views can't be edited here",
                }
                .into(),
            )
        } else if !g.pending.has_key() && mark != RowMark::Inserted {
            Some("This table has no primary key, so its rows can't be edited here".into())
        } else if mark == RowMark::Deleted {
            Some("This row is staged for deletion. Restore it to edit it.".into())
        } else {
            None
        };
        let source = RowSource {
            owner: Some(cx.entity_id()),
            read_only,
            original,
            mark,
            target,
        };
        let title = self.object.name.clone();
        self.state.update(cx, |s, cx| {
            s.inspect(
                Some(Inspected {
                    title,
                    row,
                    columns,
                    values,
                    source,
                }),
                cx,
            )
        });
    }

    /// Shows the inspected row again when it is one of this table's, after its staged
    /// values changed.
    fn refresh_inspector(&mut self, cx: &mut Context<Self>) {
        let ours = self
            .state
            .read(cx)
            .inspected
            .as_ref()
            .filter(|i| i.source.owner == Some(cx.entity_id()))
            .map(|i| i.row);
        self.reinspect = false;
        let Some(row) = ours else {
            return;
        };
        let g = self.grid.read(cx);
        let row = g.selection.map_or(row, |s| s.head.0);
        if g.row_values(row).is_some() {
            self.inspect_row(row, cx);
            return;
        }
        // The rows are being read again: show the row once it is back, or nothing
        // when it is gone.
        let settled = g
            .cache()
            .is_none_or(|c| matches!(c.status(), Status::Idle | Status::Done | Status::Error(_)));
        if settled
            && g.row_count()
                .value()
                .is_some_and(|n| row >= g.inserted() + n)
        {
            self.state.update(cx, |s, cx| s.inspect(None, cx));
        } else {
            self.reinspect = true;
        }
    }

    /// Shows a message in the status bar for a few seconds, an error for longer.
    fn notify_user(&mut self, text: impl Into<SharedString>, error: bool, cx: &mut Context<Self>) {
        self.notice = Some((text.into(), error));
        self.notice_generation += 1;
        let generation = self.notice_generation;
        {
            let wait = Duration::from_secs(if error { 10 } else { 4 });
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                this.update(cx, |this, cx| {
                    if this.notice_generation == generation {
                        this.notice = None;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    fn connection(&self, cx: &App) -> Option<Arc<dyn deltos_data::Connection>> {
        self.state
            .read(cx)
            .session(&self.connection)
            .map(|s| s.conn.clone())
    }

    /// The connection's name, for the task list.
    fn connection_label(&self, cx: &App) -> Option<SharedString> {
        self.state
            .read(cx)
            .session(&self.connection)
            .map(|s| SharedString::from(s.name.clone()))
    }

    /// Whether edits go to the database at once rather than being staged.
    fn immediate(&self, cx: &App) -> bool {
        self.state.read(cx).settings.data.commit_immediately
    }

    /// The number of staged changes.
    pub fn pending_count(&self, cx: &App) -> usize {
        self.grid.read(cx).pending.count()
    }

    /// Runs `edits` and then `done` with the outcome, back on the UI thread.
    fn apply(
        &mut self,
        edits: Vec<RowEdit>,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, Result<u64, String>, &mut Context<Self>) + 'static,
    ) {
        let Some(conn) = self.connection(cx) else {
            self.notify_user("Not connected", true, cx);
            return;
        };
        let object = self.object.clone();
        let label = format!("Saving changes to {}", object.name);
        let task = tasks::start_task(label, self.connection_label(cx), None, cx);
        let job = cx.global::<Net>().run(async move {
            conn.apply_edits(&object, &edits)
                .await
                .map_err(|e| e.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|| Err("cancelled".into()));
            if let Some(id) = task {
                cx.update(|cx| {
                    if let Ok(n) = &result {
                        tasks::progress(id, *n, cx);
                    }
                    let outcome = match &result {
                        Ok(_) => TaskOutcome::Done,
                        Err(e) => TaskOutcome::Failed(e.clone()),
                    };
                    tasks::finish(id, outcome, cx);
                })
                .ok();
            }
            this.update(cx, |this, cx| done(this, result, cx)).ok();
        })
        .detach();
    }

    /// After the staged changes moved: the tab stays open as a real tab, the inspector
    /// and the review follow, and the status bar redraws.
    fn staged(&mut self, cx: &mut Context<Self>) {
        self.commit_error = None;
        self.confirm_discard = false;
        if !self.pinned && self.pending_count(cx) > 0 {
            // A preview tab with staged changes would be replaced by the next table
            // opened from the sidebar; keep it. Once is enough: showing the tab again
            // on every change would pull its window to the front each time.
            self.pinned = true;
            let kind = PanelKind::Table {
                connection: self.connection.clone(),
                object: self.object.clone(),
            };
            self.state.update(cx, |s, cx| s.open(kind, true, cx));
        }
        if self.review.is_some() {
            self.open_review(cx);
        }
        self.refresh_inspector(cx);
        cx.notify();
    }

    /// Whether staging can go ahead; says why not when it can't.
    fn can_stage(&mut self, cx: &mut Context<Self>) -> bool {
        if self.committing {
            self.notify_user("Wait for the commit to finish", true, cx);
            return false;
        }
        true
    }

    /// The row an edit of grid row `row` is for, and `row` again when that row is still
    /// there. An edit carries the row it began on (`target`); when the rows were read
    /// again meanwhile and another row took its place, the edit still goes to its own
    /// row, found by its key, never to the one now at `row`. `None` when the edited row
    /// is a staged new row that is gone.
    fn edit_target(
        &self,
        row: u64,
        target: Option<RowRef>,
        cx: &App,
    ) -> Option<(RowRef, Option<u64>)> {
        let current = self.grid.read(cx).row_ref(row);
        match target {
            Some(t) if current.as_ref() == Some(&t) => Some((t, Some(row))),
            Some(RowRef::New(_)) => None,
            Some(t) => Some((t, None)),
            None => current.map(|t| (t, Some(row))),
        }
    }

    /// Saves one edited cell: stages it, or with "Commit edits immediately" writes it at
    /// once. A cell of a staged new row is always staged with its row.
    fn save_cell(
        &mut self,
        row: u64,
        col: usize,
        input: CellInput,
        target: Option<RowRef>,
        cx: &mut Context<Self>,
    ) {
        if !self.can_stage(cx) {
            return;
        }
        let Some((target, row)) = self.edit_target(row, target, cx) else {
            self.notify_user("That row is gone; nothing was changed", true, cx);
            return;
        };
        let grid = self.grid.read(cx);
        let columns = grid.columns().clone();
        let Some(column) = columns.get(col) else {
            return;
        };
        let has_key = grid.pending.has_key();
        let value = match edit::parse_input(column, &input) {
            Ok(v) => v,
            Err(e) => {
                self.notify_user(e, true, cx);
                return;
            }
        };
        let values = match target {
            RowRef::New(i) => {
                self.grid.update(cx, |g, cx| {
                    g.pending.set_insert(i, col, Some(value));
                    cx.notify();
                });
                self.staged(cx);
                return;
            }
            RowRef::Table(values) => values,
        };
        if !has_key {
            self.notify_user(
                "This table has no primary key, so its rows can't be edited here",
                true,
                cx,
            );
            return;
        }
        if self.driver.is_some_and(DriverKind::is_document) && column.name == "_id" {
            self.notify_user(
                "The _id of a document cannot be changed; duplicate the document instead",
                true,
                cx,
            );
            return;
        }
        if self.immediate(cx) {
            self.write_cell(row, col, values, value, cx);
            return;
        }
        self.grid.update(cx, |g, cx| {
            g.pending.stage(&values, col, value);
            cx.notify();
        });
        if column.primary_key.is_some() {
            self.notify_user(
                format!(
                    "{} is part of the key; committing gives the row a new key",
                    column.name
                ),
                false,
                cx,
            );
        }
        self.staged(cx);
    }

    /// Saves many cells at once, a paste or a cleared selection: stages them, or with
    /// "Commit edits immediately" writes them in one go. Cells that don't fit their
    /// column are left as they are, and the status bar says how many and why.
    fn save_cells(
        &mut self,
        cells: &[(u64, usize, CellInput)],
        batch: Batch,
        outside: usize,
        limited: bool,
        cx: &mut Context<Self>,
    ) {
        if cells.is_empty() || !self.can_stage(cx) {
            return;
        }
        let immediate = self.immediate(cx);
        let document = self.driver.is_some_and(DriverKind::is_document);
        let grid = self.grid.read(cx);
        let columns = grid.columns().clone();
        let has_key = grid.pending.has_key();
        let mut tally = Tally::default();
        // Parsed first, so the buffer changes in one go below.
        let mut news: Vec<(usize, usize, Value)> = Vec::new();
        let mut olds: Vec<(Rc<[Value]>, usize, Value)> = Vec::new();
        // Cells come row by row: each row is read once, not once per cell.
        let mut current: Option<(u64, Option<Target>, RowMark)> = None;
        for (row, col, input) in cells {
            let Some(column) = columns.get(*col) else {
                continue;
            };
            if current.as_ref().is_none_or(|(r, _, _)| r != row) {
                let target = grid.row_ref(*row).map(|t| match t {
                    RowRef::New(i) => Target::New(i),
                    RowRef::Table(values) => Target::Table(values.into()),
                });
                current = Some((*row, target, grid.row_mark(*row)));
            }
            let Some((_, target, mark)) = &current else {
                continue;
            };
            let Some(target) = target.clone() else {
                tally.unloaded += 1;
                continue;
            };
            if *mark == RowMark::Deleted {
                tally.refuse("Rows staged for deletion can't be changed");
                continue;
            }
            let value = match edit::parse_input(column, input) {
                Ok(v) => v,
                Err(e) => {
                    tally.refuse(e);
                    continue;
                }
            };
            match target {
                Target::New(i) => news.push((i, *col, value)),
                Target::Table(_) if !has_key => {
                    tally.refuse("This table has no primary key, so its rows can't be edited here");
                    continue;
                }
                Target::Table(_) if document && column.name == "_id" => {
                    tally.refuse("The _id of a document cannot be changed");
                    continue;
                }
                Target::Table(values) => olds.push((values, *col, value)),
            }
            tally.done += 1;
            match input {
                CellInput::Null => tally.nulls += 1,
                CellInput::Text(t) if t.is_empty() => tally.empties += 1,
                CellInput::Text(_) => {}
            }
        }
        let mut writes = Vec::new();
        self.grid.update(cx, |g, cx| {
            for (i, col, value) in news {
                g.pending.set_insert(i, col, Some(value));
            }
            if immediate {
                writes = olds;
            } else {
                for (values, col, value) in olds {
                    g.pending.stage(&values, col, value);
                }
            }
            cx.notify();
        });
        let mut message = match batch {
            _ if tally.done == 0 => "Nothing changed".to_owned(),
            Batch::Paste => format!("Pasted {}", cells_text(tally.done)),
            Batch::Clear => match (tally.nulls, tally.empties) {
                (n, 0) => format!("Set {} to NULL", cells_text(n)),
                (0, e) => format!("Set {} to empty text", cells_text(e)),
                (n, e) => format!("Set {} to NULL and {e} to empty text", cells_text(n)),
            },
        };
        if let Some(why) = &tally.first_refusal {
            message.push_str(&format!(
                "; {} left as they were: {why}",
                cells_text(tally.refused)
            ));
        }
        if tally.unloaded > 0 {
            message.push_str(&format!(
                "; {} skipped, their rows aren't loaded",
                cells_text(tally.unloaded)
            ));
        }
        if outside > 0 {
            message.push_str(&format!("; {} fell outside the table", cells_text(outside)));
        }
        if limited {
            message.push_str(&format!(
                "; only the first {} rows",
                thousands(crate::ui::grid::EDIT_ROWS as u64)
            ));
        }
        let failed = tally.done == 0;
        if !writes.is_empty() {
            self.write_cells(writes, message, cx);
        } else {
            self.notify_user(message, failed, cx);
        }
        self.staged(cx);
    }

    /// Writes many cells now, one update per row, in one call to the driver.
    fn write_cells(
        &mut self,
        cells: Vec<(Rc<[Value]>, usize, Value)>,
        message: String,
        cx: &mut Context<Self>,
    ) {
        let columns = self.grid.read(cx).columns().clone();
        let mut edits: Vec<RowEdit> = Vec::new();
        let mut last: Option<&Rc<[Value]>> = None;
        for (values, col, value) in &cells {
            let (Some(column), Some(key)) = (columns.get(*col), edit::key_of(&columns, values))
            else {
                continue;
            };
            if values.get(*col) == Some(value) {
                continue;
            }
            let change = (column.name.clone(), value.clone());
            match edits.last_mut() {
                // Cells come row by row, so a row's cells are next to each other.
                Some(RowEdit::Update { changes, .. })
                    if last.is_some_and(|l| Rc::ptr_eq(l, values)) =>
                {
                    changes.push(change)
                }
                _ => edits.push(RowEdit::Update {
                    key,
                    changes: vec![change],
                }),
            }
            last = Some(values);
        }
        if edits.is_empty() {
            self.notify_user(message, false, cx);
            return;
        }
        self.notify_user("Saving…", false, cx);
        self.apply(edits, cx, move |this, result, cx| {
            match result {
                Ok(_) => this.notify_user(message, false, cx),
                Err(e) => this.notify_user(format!("Not saved: {e}"), true, cx),
            }
            this.reload(cx);
            this.refresh_inspector(cx);
        });
    }

    /// Writes one cell now. The grid shows the new value at once, at `row` when the
    /// row is still there, and the rows are read again if the database refuses it.
    fn write_cell(
        &mut self,
        row: Option<u64>,
        col: usize,
        values: Vec<Value>,
        value: Value,
        cx: &mut Context<Self>,
    ) {
        let columns = self.grid.read(cx).columns().clone();
        let (Some(column), Some(key)) = (columns.get(col), edit::key_of(&columns, &values)) else {
            return;
        };
        if values.get(col) == Some(&value) {
            return;
        }
        let name = column.name.clone();
        if let Some(row) = row {
            self.grid
                .update(cx, |g, cx| g.set_value(row, col, value.clone(), cx));
        }
        self.notify_user("Saving…", false, cx);
        let edits = vec![RowEdit::Update {
            key,
            changes: vec![(name.clone(), value)],
        }];
        // The rows may have moved by the time the answer comes, so the old value is not
        // put back by position: the rows are read again instead.
        self.apply(edits, cx, move |this, result, cx| {
            match result {
                Ok(_) if row.is_some() => {
                    this.notify_user(format!("Saved {name}"), false, cx);
                    this.refresh_inspector(cx);
                    return;
                }
                Ok(_) => this.notify_user(format!("Saved {name}"), false, cx),
                Err(e) => this.notify_user(format!("Not saved: {e}"), true, cx),
            }
            this.reload(cx);
            this.refresh_inspector(cx);
        });
    }

    /// Takes back the staged change of one cell; a new row's cell goes back to its
    /// default.
    fn revert_cell(
        &mut self,
        row: u64,
        col: usize,
        target: Option<RowRef>,
        cx: &mut Context<Self>,
    ) {
        if !self.can_stage(cx) {
            return;
        }
        let Some((target, _)) = self.edit_target(row, target, cx) else {
            return;
        };
        self.grid.update(cx, |g, cx| {
            match target {
                RowRef::New(i) => g.pending.set_insert(i, col, None),
                RowRef::Table(values) => g.pending.revert_cell(&values, col),
            }
            cx.notify();
        });
        self.staged(cx);
    }

    /// The loaded rows among `rows` (at most [`ROW_ACTION_LIMIT`]), as staged changes
    /// refer to them.
    fn row_refs(&self, rows: &[u64], cx: &App) -> Vec<RowRef> {
        let grid = self.grid.read(cx);
        rows.iter()
            .take(ROW_ACTION_LIMIT)
            .filter_map(|r| grid.row_ref(*r))
            .collect()
    }

    /// The loaded rows among `rows`, with their keys.
    fn keys_of(&self, rows: &[u64], cx: &App) -> Option<Vec<Vec<(String, Value)>>> {
        let grid = self.grid.read(cx);
        let columns = grid.columns().clone();
        rows.iter()
            .map(|r| match grid.row_ref(*r) {
                Some(RowRef::Table(v)) => edit::key_of(&columns, &v),
                _ => None,
            })
            .collect()
    }

    /// Deletes `rows`: stages their deletion, or with "Commit edits immediately" deletes
    /// them now (after the person confirmed). Staged new rows among them just go.
    fn delete_rows(&mut self, rows: Vec<u64>, cx: &mut Context<Self>) {
        self.confirm_delete = None;
        if !self.can_stage(cx) {
            return;
        }
        let refs = self.row_refs(&rows, cx);
        let mut new_rows: Vec<usize> = refs
            .iter()
            .filter_map(|r| match r {
                RowRef::New(i) => Some(*i),
                RowRef::Table(_) => None,
            })
            .collect();
        let table_rows: Vec<Vec<Value>> = refs
            .into_iter()
            .filter_map(|r| match r {
                RowRef::Table(v) => Some(v),
                RowRef::New(_) => None,
            })
            .collect();
        if self.immediate(cx) && !table_rows.is_empty() {
            let rows: Vec<u64> = rows
                .into_iter()
                .filter(|r| self.grid.read(cx).cache_row(*r).is_some())
                .collect();
            self.delete_now(rows, cx);
            return;
        }
        if !table_rows.is_empty() && !self.grid.read(cx).pending.has_key() {
            self.notify_user(
                "This table has no primary key, so its rows can't be deleted here",
                true,
                cx,
            );
            return;
        }
        new_rows.sort_unstable();
        self.grid.update(cx, |g, cx| {
            for i in new_rows.into_iter().rev() {
                g.pending.remove_insert(i);
            }
            for values in &table_rows {
                g.pending.delete(values);
            }
            g.selection = None;
            cx.notify();
        });
        self.staged(cx);
    }

    fn delete_now(&mut self, rows: Vec<u64>, cx: &mut Context<Self>) {
        let Some(keys) = self.keys_of(&rows, cx) else {
            self.notify_user(
                "Only loaded rows of a table with a key can be deleted",
                true,
                cx,
            );
            return;
        };
        let edits = keys
            .into_iter()
            .map(|key| RowEdit::Delete { key })
            .collect();
        self.apply(edits, cx, |this, result, cx| match result {
            Ok(n) => {
                this.notify_user(
                    format!(
                        "Deleted {} {}",
                        thousands(n),
                        if n == 1 { "row" } else { "rows" }
                    ),
                    false,
                    cx,
                );
                this.requery(cx);
            }
            Err(e) => this.notify_user(format!("Not deleted: {e}"), true, cx),
        });
    }

    /// Takes back the staged deletion of `rows`, keeping their cell changes.
    fn restore_rows(&mut self, rows: Vec<u64>, cx: &mut Context<Self>) {
        if !self.can_stage(cx) {
            return;
        }
        let refs = self.row_refs(&rows, cx);
        self.grid.update(cx, |g, cx| {
            g.pending.restore_rows(refs.iter().filter_map(|r| match r {
                RowRef::Table(values) => Some(values.as_slice()),
                RowRef::New(_) => None,
            }));
            cx.notify();
        });
        self.staged(cx);
    }

    /// Takes back every staged change of `rows`; staged new rows among them go.
    fn revert_rows(&mut self, rows: Vec<u64>, cx: &mut Context<Self>) {
        if !self.can_stage(cx) {
            return;
        }
        let refs = self.row_refs(&rows, cx);
        let mut new_rows = Vec::new();
        self.grid.update(cx, |g, cx| {
            g.pending.revert_rows(refs.iter().filter_map(|r| match r {
                RowRef::Table(values) => Some(values.as_slice()),
                RowRef::New(i) => {
                    new_rows.push(*i);
                    None
                }
            }));
            new_rows.sort_unstable();
            for i in new_rows.into_iter().rev() {
                g.pending.remove_insert(i);
            }
            cx.notify();
        });
        self.staged(cx);
    }

    /// Copies `rows` without their key, so the database gives the copies new ones. The
    /// copies are staged as new rows, or with "Commit edits immediately" inserted now.
    fn duplicate_rows(&mut self, rows: Vec<u64>, cx: &mut Context<Self>) {
        if !self.can_stage(cx) {
            return;
        }
        let immediate = self.immediate(cx);
        let grid = self.grid.read(cx);
        let columns = grid.columns().clone();
        let mut copies: Vec<Vec<Option<Value>>> = Vec::new();
        for row in rows.iter().take(ROW_ACTION_LIMIT) {
            let Some(values) = grid.row_values(*row) else {
                continue;
            };
            let defaults = grid.cache_row(*row).is_none();
            copies.push(
                columns
                    .iter()
                    .zip(values)
                    .enumerate()
                    .map(|(c, (column, v))| {
                        let keep =
                            column.primary_key.is_none() && !(defaults && grid.is_default(*row, c));
                        keep.then_some(v)
                    })
                    .collect(),
            );
        }
        if copies.is_empty() {
            return;
        }
        if immediate {
            let edits = copies
                .into_iter()
                .map(|values| RowEdit::Insert {
                    values: columns
                        .iter()
                        .zip(values)
                        .filter_map(|(c, v)| Some((c.name.clone(), v?)))
                        .collect(),
                })
                .collect();
            self.apply(edits, cx, |this, result, cx| match result {
                Ok(n) => {
                    this.notify_user(
                        format!(
                            "Added {} {}",
                            thousands(n),
                            if n == 1 { "row" } else { "rows" }
                        ),
                        false,
                        cx,
                    );
                    this.requery(cx);
                }
                Err(e) => this.notify_user(format!("Not added: {e}"), true, cx),
            });
            return;
        }
        let n = copies.len();
        self.grid.update(cx, |g, cx| {
            let first = g.pending.inserts().len() as u64;
            for values in copies {
                g.pending.add_insert(values);
            }
            // The copies are the last of the new rows, above the table's rows.
            let col = g.selection.map_or(0, |s| s.head.1);
            g.selection = Some(crate::ui::grid::Selection {
                anchor: (first, col),
                head: (first + n as u64 - 1, col),
            });
            cx.notify();
        });
        self.notify_user(
            format!(
                "Staged {} new {}",
                thousands(n as u64),
                if n == 1 { "row" } else { "rows" }
            ),
            false,
            cx,
        );
        self.staged(cx);
    }

    /// Stages an empty row, every value left to its default, and starts editing its
    /// first column that is not part of the key.
    fn add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_stage(cx) {
            return;
        }
        if !self.grid.read(cx).editable {
            self.notify_user("Rows can't be added here", true, cx);
            return;
        }
        if self.grid.read(cx).columns().is_empty() {
            return;
        }
        self.mode = Mode::Data;
        self.grid.update(cx, |g, cx| {
            let row = g.pending.add_insert(Vec::new()) as u64;
            let col = g
                .columns()
                .iter()
                .position(|c| c.primary_key.is_none())
                .unwrap_or(0);
            g.select_cell((row, col), cx);
            g.begin_edit((row, col), window, cx);
        });
        self.staged(cx);
    }

    /// Saves an edit still open in the grid, so it is part of what happens next.
    fn flush_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((row, col, input, target)) =
            self.grid.update(cx, |g, cx| g.take_edit(window, cx))
        {
            self.save_cell(row, col, input, target, cx);
        }
    }

    /// Runs every staged change in one call to the driver: one transaction on databases
    /// that have them. On success the staged state clears and the rows are read again;
    /// on failure everything stays staged and the error shows. A database without
    /// transactions gets the changes one by one, and the ones it saved before the
    /// failing one are no longer staged. `done` hears whether it worked.
    fn commit(
        &mut self,
        done: Option<Box<dyn FnOnce(&mut Self, bool, &mut Context<Self>)>>,
        cx: &mut Context<Self>,
    ) {
        if self.committing {
            return;
        }
        let edits = self.grid.read(cx).pending.edits();
        if edits.is_empty() {
            if let Some(done) = done {
                done(self, true, cx);
            }
            return;
        }
        let Some(conn) = self.connection(cx) else {
            self.notify_user("Not connected", true, cx);
            return;
        };
        self.committing = true;
        self.confirm_discard = false;
        self.commit_error = None;
        self.notify_user("Committing…", false, cx);
        let object = self.object.clone();
        let total = edits.len();
        // Listed without Cancel, like saving an edit: a commit stopped halfway would leave
        // part of it saved on a database without transactions.
        let task = tasks::start_task(
            format!("Saving changes to {}", object.name),
            self.connection_label(cx),
            None,
            cx,
        );
        let all_at_once = conn.capabilities().transactions;
        let job = cx.global::<Net>().run(async move {
            if all_at_once {
                return conn
                    .apply_edits(&object, &edits)
                    .await
                    .map(|_| total)
                    .map_err(|e| (0, e.to_string()));
            }
            let mut saved = 0;
            for edit in &edits {
                conn.apply_edits(&object, std::slice::from_ref(edit))
                    .await
                    .map_err(|e| (saved, e.to_string()))?;
                saved += 1;
            }
            Ok(saved)
        });
        cx.spawn(async move |this, cx| {
            let result = job
                .await
                .unwrap_or_else(|| Err((0, "the commit was cancelled".into())));
            if let Some(task) = task {
                cx.update(|cx| {
                    let (saved, outcome) = match &result {
                        Ok(n) => (*n, TaskOutcome::Done),
                        Err((saved, e)) => (*saved, TaskOutcome::Failed(e.clone())),
                    };
                    tasks::progress(task, saved as u64, cx);
                    tasks::finish(task, outcome, cx);
                })
                .ok();
            }
            this.update(cx, |this, cx| {
                this.committing = false;
                let ok = result.is_ok();
                match result {
                    Ok(n) => {
                        this.grid.update(cx, |g, cx| {
                            let inserts = g.pending.inserts().len() as u64;
                            g.pending.clear();
                            // The new rows are in the table now; the rows below them move
                            // up, and the selection with them.
                            if let Some(s) = g.selection.as_mut() {
                                s.anchor.0 = s.anchor.0.saturating_sub(inserts);
                                s.head.0 = s.head.0.saturating_sub(inserts);
                            }
                            cx.notify();
                        });
                        this.review = None;
                        this.notify_user(
                            format!(
                                "Committed {} {}",
                                thousands(n as u64),
                                if n == 1 { "change" } else { "changes" }
                            ),
                            false,
                            cx,
                        );
                        this.reload(cx);
                    }
                    Err((saved, e)) => {
                        let message: SharedString = if saved > 0 {
                            this.grid.update(cx, |g, _| g.pending.drop_applied(saved));
                            this.reload(cx);
                            format!("Saved {saved} of {total} changes, then one failed: {e}").into()
                        } else {
                            format!("Not committed: {e}").into()
                        };
                        this.commit_error = Some(message.clone());
                        if this.review.is_some() {
                            this.open_review(cx);
                        }
                        this.notify_user(message, true, cx);
                    }
                }
                this.refresh_inspector(cx);
                if let Some(done) = done {
                    done(this, ok, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Drops every staged change.
    fn discard(&mut self, cx: &mut Context<Self>) {
        if self.committing {
            return;
        }
        let n = self.pending_count(cx);
        self.grid.update(cx, |g, cx| {
            let inserts = g.pending.inserts().len() as u64;
            g.pending.clear();
            // Rows below the new ones move up with them.
            if let Some(s) = g.selection.as_mut() {
                s.anchor.0 = s.anchor.0.saturating_sub(inserts);
                s.head.0 = s.head.0.saturating_sub(inserts);
            }
            cx.notify();
        });
        self.review = None;
        self.commit_error = None;
        self.confirm_discard = false;
        if n > 0 {
            self.notify_user(
                format!(
                    "Discarded {} {}",
                    thousands(n as u64),
                    if n == 1 { "change" } else { "changes" }
                ),
                false,
                cx,
            );
        }
        self.refresh_inspector(cx);
        cx.notify();
    }

    /// Opens the review of what a commit runs, or brings it up to date.
    fn open_review(&mut self, cx: &mut Context<Self>) {
        let grid = self.grid.read(cx);
        let generation = grid.pending.generation();
        if self
            .review
            .as_ref()
            .is_some_and(|r| r.generation == generation)
        {
            return;
        }
        let kind = self.driver.unwrap_or(DriverKind::Postgres);
        let edits = grid.pending.edits();
        let statements: Arc<[SharedString]> =
            edit::preview_statements(kind, &self.object, grid.columns(), &edits)
                .into_iter()
                .map(SharedString::from)
                .collect();
        self.review = Some(Review {
            statements,
            generation,
        });
        cx.notify();
    }

    /// Asks what happens to staged changes before the tab closes: commit them (and close
    /// when that worked), discard them, or keep the tab. `close` closes it.
    pub fn confirm_close(&mut self, close: Box<dyn FnOnce(&mut App)>, cx: &mut Context<Self>) {
        let n = self.pending_count(cx);
        let Some(window) = cx
            .active_window()
            .or_else(|| cx.windows().into_iter().next())
        else {
            return;
        };
        let message = format!(
            "Commit {} {} to {}?",
            thousands(n as u64),
            if n == 1 { "change" } else { "changes" },
            self.object.name
        );
        // Asked from a task: the window is busy with the click that closes the tab.
        cx.spawn(async move |this, cx| {
            let Ok(prompt) = window.update(cx, |_, window, cx| {
                window.prompt(
                    gpui::PromptLevel::Warning,
                    &message,
                    Some("Otherwise they are lost."),
                    &["Commit", "Discard", "Cancel"],
                    cx,
                )
            }) else {
                return;
            };
            let answer = prompt.await.ok();
            this.update(cx, |this, cx| match answer {
                Some(0) => this.commit(
                    Some(Box::new(move |_, ok, cx| {
                        if ok {
                            close(cx);
                        }
                    })),
                    cx,
                ),
                Some(1) => {
                    this.discard(cx);
                    close(cx);
                }
                _ => {}
            })
            .ok();
        })
        .detach();
    }

    fn sort_by(&mut self, column: usize, direction: Option<SortDirection>, cx: &mut Context<Self>) {
        let Some(name) = self
            .grid
            .read(cx)
            .columns()
            .get(column)
            .map(|c| c.name.clone())
        else {
            return;
        };
        self.sort = direction
            .map(|direction| {
                vec![Sort {
                    column: name,
                    direction,
                }]
            })
            .unwrap_or_default();
        self.requery(cx);
    }

    /// The table's items for the grid's right-click menu.
    fn menu_items(
        &self,
        target: MenuTarget,
        grid: &GridView,
        this: &Entity<Self>,
        cx: &App,
    ) -> Vec<crate::ui::MenuItem> {
        use crate::ui::MenuItem;
        let weak = this.downgrade();
        let run = |f: Box<dyn Fn(&mut Self, &mut Context<Self>)>| {
            let w = weak.clone();
            move |_: &mut Window, cx: &mut App| {
                w.update(cx, |t, cx| f(t, cx)).ok();
            }
        };
        let columns = grid.columns().clone();
        let mut items = Vec::new();
        let col = match target {
            MenuTarget::Cell(_, c) | MenuTarget::Header(c) => c,
        };
        let Some(column) = columns.get(col) else {
            return items;
        };
        let name = column.name.clone();
        let short = |s: &str| {
            let mut out: String = s.chars().take(24).collect();
            if s.chars().count() > 24 {
                out.push('…');
            }
            out
        };
        match target {
            MenuTarget::Cell(row, _) => {
                let value = grid.value(row, col);
                match &value {
                    Some(Value::Null) => {
                        items.push(MenuItem::new(
                            format!("Show rows where {name} is NULL"),
                            run(Box::new(move |t, cx| {
                                t.filter_by(col, FilterOp::IsNull, String::new(), cx)
                            })),
                        ));
                        items.push(MenuItem::new(
                            format!("Show rows where {name} is not NULL"),
                            run(Box::new(move |t, cx| {
                                t.filter_by(col, FilterOp::IsNotNull, String::new(), cx)
                            })),
                        ));
                    }
                    Some(v) => {
                        let text = v.to_string();
                        let shown = short(&text);
                        let (a, b) = (text.clone(), text);
                        items.push(MenuItem::new(
                            format!("Show rows where {name} = {shown}"),
                            run(Box::new(move |t, cx| {
                                t.filter_by(col, FilterOp::Eq, a.clone(), cx)
                            })),
                        ));
                        items.push(MenuItem::new(
                            format!("Hide rows where {name} = {shown}"),
                            run(Box::new(move |t, cx| {
                                t.filter_by(col, FilterOp::NotEq, b.clone(), cx)
                            })),
                        ));
                    }
                    None => {}
                }
                if self.filters.active(cx) {
                    items.push(MenuItem::new(
                        "Clear filters",
                        run(Box::new(|t, cx| t.clear_filters(cx))),
                    ));
                }
                items.extend(self.link_items(row, col, grid, this, cx));
                items.push(MenuItem::separator());
                items.push(MenuItem::new(
                    format!("Sort by {name}, ascending"),
                    run(Box::new(move |t, cx| {
                        t.sort_by(col, Some(SortDirection::Ascending), cx)
                    })),
                ));
                items.push(MenuItem::new(
                    format!("Sort by {name}, descending"),
                    run(Box::new(move |t, cx| {
                        t.sort_by(col, Some(SortDirection::Descending), cx)
                    })),
                ));
                let rows: Vec<u64> = grid
                    .selected_ranges()
                    .map(|(r, _)| r.take(10_000).collect())
                    .unwrap_or_else(|| vec![row]);
                if let Some(kind) = self.driver.filter(|k| edit::speaks_sql(*k)) {
                    items.push(MenuItem::separator());
                    let object = self.object.clone();
                    let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
                    let values: Vec<Vec<Value>> =
                        rows.iter().filter_map(|r| grid.row_values(*r)).collect();
                    let n = values.len();
                    items.push(
                        MenuItem::new(
                            if n == 1 {
                                "Copy row as INSERT".to_owned()
                            } else {
                                format!("Copy {n} rows as INSERT")
                            },
                            move |_, cx| {
                                let names: Vec<&str> = names.iter().map(String::as_str).collect();
                                let sql = edit::insert_statements(kind, &object, &names, &values);
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(sql));
                            },
                        )
                        .enabled(n > 0),
                    );
                    if let Some(key) = grid
                        .row_values(row)
                        .and_then(|v| edit::key_of(&columns, &v))
                    {
                        items.push(MenuItem::new("Copy row's WHERE clause", move |_, cx| {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                edit::where_clause(kind, &key),
                            ))
                        }));
                    }
                }
                if grid.editable {
                    items.push(MenuItem::separator());
                    let n = rows.len();
                    let plural = |n: usize| if n == 1 { "row" } else { "rows" };
                    let count = |n: usize| {
                        if n == 1 {
                            "row".to_owned()
                        } else {
                            format!("{} rows", thousands(n as u64))
                        }
                    };
                    let staging = !self.immediate(cx);
                    let marks: Vec<RowMark> = rows
                        .iter()
                        .take(ROW_ACTION_LIMIT)
                        .map(|r| grid.row_mark(*r))
                        .collect();
                    if staging && grid.value(row, col).is_some() {
                        let cell_staged = match grid.row_ref(row) {
                            Some(RowRef::Table(v)) => {
                                grid.pending.row(&v).is_some_and(|c| c.cell(col).is_some())
                            }
                            Some(RowRef::New(_)) => !grid.is_default(row, col),
                            None => false,
                        };
                        if cell_staged {
                            items.push(MenuItem::new(
                                "Revert cell",
                                run(Box::new(move |t, cx| t.revert_cell(row, col, None, cx))),
                            ));
                        }
                    }
                    if marks.iter().any(|m| *m != RowMark::None) {
                        let revert = rows.clone();
                        items.push(MenuItem::new(
                            format!("Revert changes to {}", count(n)),
                            run(Box::new(move |t, cx| t.revert_rows(revert.clone(), cx))),
                        ));
                    }
                    let dup = rows.clone();
                    items.push(
                        MenuItem::new(
                            format!("Duplicate {}", count(n)),
                            run(Box::new(move |t, cx| t.duplicate_rows(dup.clone(), cx))),
                        )
                        .detail("Ctrl+D"),
                    );
                    items.push(
                        MenuItem::new("Add row", {
                            let w = weak.clone();
                            move |window: &mut Window, cx: &mut App| {
                                w.update(cx, |t, cx| t.add_row(window, cx)).ok();
                            }
                        })
                        .detail("Ctrl+Shift+N"),
                    );
                    let all_deleted =
                        !marks.is_empty() && marks.iter().all(|m| *m == RowMark::Deleted);
                    let del = rows;
                    if all_deleted {
                        items.push(MenuItem::new(
                            format!("Restore {}", count(n)),
                            run(Box::new(move |t, cx| t.restore_rows(del.clone(), cx))),
                        ));
                    } else if staging {
                        items.push(
                            MenuItem::new(
                                format!("Delete {}", count(n)),
                                run(Box::new(move |t, cx| t.delete_rows(del.clone(), cx))),
                            )
                            .danger(),
                        );
                    } else {
                        items.push(
                            MenuItem::new(
                                format!("Delete {} {}…", thousands(n as u64), plural(n)),
                                run(Box::new(move |t, cx| {
                                    t.confirm_delete = Some(del.clone());
                                    cx.notify();
                                })),
                            )
                            .danger(),
                        );
                    }
                }
                items.push(MenuItem::separator());
                items.push(
                    MenuItem::new("Reload", run(Box::new(|t, cx| t.refresh(cx)))).detail("Ctrl+R"),
                );
            }
            MenuTarget::Header(_) => {
                let current = self
                    .sort
                    .first()
                    .filter(|s| s.column == name)
                    .map(|s| s.direction);
                items.push(
                    MenuItem::new(
                        "Sort ascending",
                        run(Box::new(move |t, cx| {
                            t.sort_by(col, Some(SortDirection::Ascending), cx)
                        })),
                    )
                    .checked(current == Some(SortDirection::Ascending)),
                );
                items.push(
                    MenuItem::new(
                        "Sort descending",
                        run(Box::new(move |t, cx| {
                            t.sort_by(col, Some(SortDirection::Descending), cx)
                        })),
                    )
                    .checked(current == Some(SortDirection::Descending)),
                );
                items.push(
                    MenuItem::new(
                        "Don't sort",
                        run(Box::new(move |t, cx| t.sort_by(col, None, cx))),
                    )
                    .enabled(!self.sort.is_empty()),
                );
                items.push(MenuItem::separator());
                items.push(MenuItem::new(
                    format!("Filter by {name}…"),
                    run(Box::new(move |t, cx| {
                        t.add_filter(cx);
                        let name = t.grid.read(cx).columns().get(col).map(|c| c.name.clone());
                        if let (Some(f), Some(name)) = (t.filters.rows.last_mut(), name) {
                            f.column = name;
                        }
                        cx.notify();
                    })),
                ));
                items.push(MenuItem::new(
                    format!("Show rows where {name} is NULL"),
                    run(Box::new(move |t, cx| {
                        t.filter_by(col, FilterOp::IsNull, String::new(), cx)
                    })),
                ));
            }
        }
        items
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.loader.is_none() {
            self.attach(cx);
        } else {
            self.requery(cx);
        }
    }

    /// Ascending, then descending, then unsorted.
    fn cycle_sort(&mut self, column: usize, cx: &mut Context<Self>) {
        let Some(name) = self
            .grid
            .read(cx)
            .columns()
            .get(column)
            .map(|c| c.name.clone())
        else {
            return;
        };
        let next = match self.sort.first() {
            Some(s) if s.column == name && s.direction == SortDirection::Ascending => {
                Some(SortDirection::Descending)
            }
            Some(s) if s.column == name => None,
            _ => Some(SortDirection::Ascending),
        };
        self.sort = next
            .map(|direction| {
                vec![Sort {
                    column: name,
                    direction,
                }]
            })
            .unwrap_or_default();
        self.requery(cx);
    }

    fn status_bar(&mut self, t: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let grid = self.grid.read(cx);
        let (status, memory) = match grid.cache() {
            Some(c) => (c.status(), c.memory_used()),
            None => (Status::Idle, 0),
        };
        let inserted = grid.pending.inserts().len() as u64;
        let selected = grid
            .selection
            .and_then(|s| grid.cache_row(s.head.0))
            .map(|r| r + 1);
        let pending = grid.pending.count();
        let summary = pending_summary(
            grid.pending.changed_cells(),
            grid.pending.deleted_rows(),
            inserted as usize,
        );
        let mode = self.mode;
        let filtering = self.filters.active(cx);
        let committing = self.committing;
        let can_add = grid.editable && !grid.columns().is_empty();
        let small =
            |b: gpui::Stateful<gpui::Div>| b.h(t.s(22.)).px(t.s(9.)).text_size(t.small_size);
        let env = crate::safety::Environment::of(self.state.read(cx), &self.connection);
        let rows = self.pager_bar(t, window, cx);
        div()
            .flex_none()
            .h(t.bar_height)
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(10.))
            .bg(t.base)
            .border_t_1()
            .border_color(t.border)
            .text_size(t.small_size)
            .text_color(t.text_muted)
            .child(segmented(
                "table-mode",
                &[("Data", Mode::Data), ("Structure", Mode::Structure)],
                mode,
                {
                    let weak = cx.entity().downgrade();
                    move |m, _, cx| {
                        weak.update(cx, |this, cx| {
                            this.mode = m;
                            cx.notify();
                        })
                        .ok();
                    }
                },
                t,
            ))
            .child(
                icon_button("toggle-filters", Icon::Filter, t)
                    .when(filtering, |el| el.text_color(t.accent))
                    .tooltip(|_, cx| Tooltip::view("Filter rows", cx))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.filters.rows.is_empty() && !this.filters.raw_mode {
                            this.add_filter(cx);
                        } else {
                            this.filters.open = !this.filters.open;
                            cx.notify();
                        }
                    })),
            )
            .when(can_add, |el| {
                el.child(
                    icon_button("add-row", Icon::Plus, t)
                        .tooltip(|_, cx| Tooltip::view("Add row (Ctrl+Shift+N)", cx))
                        .on_click(cx.listener(|this, _, window, cx| this.add_row(window, cx))),
                )
            })
            .child(self.transfer_button(t, cx))
            .child(div().flex_1())
            .when_some(self.notice.clone(), |el, (text, error)| {
                // A long message is cut short here; the tooltip has all of it.
                let full = text.clone();
                el.child(
                    div()
                        .id("notice")
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(if error { t.danger } else { t.success })
                        .tooltip(move |_, cx| Tooltip::view(full.clone(), cx))
                        .child(text),
                )
            })
            .when(pending > 0, |el| {
                el.child(
                    div()
                        .id("pending-count")
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(t.warning)
                        .child(crate::ui::dot(t.warning, 6.))
                        .child(format!(
                            "{} {}",
                            thousands(pending as u64),
                            if pending == 1 { "change" } else { "changes" }
                        ))
                        .tooltip(move |_, cx| Tooltip::view(summary.clone(), cx)),
                )
                .child(
                    small(button("review", "Review", ButtonStyle::Ghost, t))
                        .tooltip(|_, cx| {
                            Tooltip::view("See the statements a commit runs (Ctrl+Shift+S)", cx)
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.flush_edit(window, cx);
                            this.open_review(cx);
                            window.focus(&this.review_focus);
                        })),
                )
                .child(
                    small(button("discard", "Discard", ButtonStyle::Ghost, t))
                        .when(committing, |el| el.opacity(0.5))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.confirm_discard = true;
                            cx.notify();
                        })),
                )
                .child(
                    small(button(
                        "commit",
                        if committing {
                            "Committing…"
                        } else {
                            "Commit"
                        },
                        ButtonStyle::Primary,
                        t,
                    ))
                    .tooltip(|_, cx| Tooltip::view("Commit staged changes (Ctrl+S)", cx))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.flush_edit(window, cx);
                        this.commit(None, cx);
                    })),
                )
                .child(div().w(px(1.)).h(px(14.)).bg(t.border))
            })
            .children(crate::ui::environment::status(env.as_ref(), t))
            .when_some(selected, |el, n| el.child(format!("Row {}", thousands(n))))
            .when(selected.is_none() && inserted > 0, |el| {
                el.when_some(
                    self.grid.read(cx).selection.filter(|s| s.head.0 < inserted),
                    |el, _| el.child("New row"),
                )
            })
            .child(rows)
            .child(match status {
                Status::Loading => div()
                    .text_color(t.text_faint)
                    .child("Loading…")
                    .into_any_element(),
                Status::Error(_) => div().text_color(t.danger).child("Error").into_any_element(),
                _ => div().into_any_element(),
            })
            .when(memory > 0, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_color(t.text_faint)
                        .child(format!("{:.1} MB", memory as f64 / 1_048_576.0)),
                )
            })
            .child(
                icon_button("refresh", Icon::Refresh, t)
                    .tooltip(|_, cx| Tooltip::view("Reload", cx))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
            .into_any_element()
    }

    /// The status bar's export and import button, and its menu when open.
    fn transfer_button(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut el = icon_button("table-transfer", Icon::Export, t)
            .relative()
            .tooltip(|_, cx| Tooltip::view("Export and import", cx))
            .on_click(cx.listener(|this, _, _, cx| {
                this.transfer_menu = !this.transfer_menu;
                cx.notify();
            }));
        if self.transfer_menu
            && let (Some(conn), Some(loader)) = (self.connection(cx), self.loader.clone())
        {
            let columns = self.grid.read(cx).columns().clone();
            let weak = cx.entity().downgrade();
            let reload = weak.clone();
            let items = crate::ui::transfer::table_items(
                conn,
                self.object.clone(),
                Some(loader.query()),
                (!columns.is_empty()).then_some(columns),
                self.state.read(cx).settings.data.page_size,
                crate::ui::transfer::Safety::of(self.state.read(cx), &self.connection),
                Some(Rc::new(move |cx: &mut App| {
                    reload.update(cx, |t, cx| t.refresh(cx)).ok();
                })),
            );
            el = el.child(menu(
                "table-transfer-menu",
                items,
                220.,
                move |_, cx| {
                    weak.update(cx, |this, cx| {
                        this.transfer_menu = false;
                        cx.notify();
                    })
                    .ok();
                },
                t,
            ));
        }
        el.into_any_element()
    }

    /// The structure view, made on first show. It edits the table's columns, indexes and
    /// keys; see [`crate::ui::structure`].
    fn structure(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.structure.is_none() {
            let (state, connection, object) = (
                self.state.clone(),
                self.connection.clone(),
                self.object.clone(),
            );
            let view = cx.new(|cx| StructureView::new(state, connection, object, window, cx));
            self._subs
                .push(cx.subscribe(&view, |this, _, event, cx| match event {
                    StructureEvent::Applied => this.structure_changed(cx),
                }));
            self.structure = Some(view);
            self.sync_structure(cx);
        }
        // Row changes are staged against the columns as they are: they go to the database
        // first, or are discarded, before the columns change under them.
        let pending = self.pending_count(cx);
        let blocked = if self.committing {
            Some("Wait for the row changes to be committed".to_owned())
        } else if pending > 0 {
            Some(format!(
                "Commit or discard the {} staged row {} first",
                thousands(pending as u64),
                if pending == 1 { "change" } else { "changes" }
            ))
        } else {
            None
        };
        if let Some(view) = &self.structure {
            view.update(cx, |v, _| v.set_blocked(blocked));
        }
        self.structure
            .clone()
            .map_or_else(|| div().into_any_element(), |v| v.into_any_element())
    }

    fn sync_structure(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = &self.structure {
            let info = self.info.clone();
            view.update(cx, |v, cx| v.set_info(info, cx));
        }
    }

    /// The table's shape changed: columns the sort and filters name may be gone, so they
    /// go, and everything loads again.
    fn structure_changed(&mut self, cx: &mut Context<Self>) {
        self.sort.clear();
        self.filters.rows.clear();
        self.filters.raw.update(cx, |i, cx| i.clear(cx));
        self.filters.raw_mode = false;
        self.filters.open = false;
        self.filters.menu = None;
        // Reopened later in the session, the table comes back without them too.
        let view = self.saved_view(cx);
        memory::remember(cx, &self.connection, &self.object, view);
        self.forget_count(cx);
        self.loader = None;
        self.info = None;
        // The row in the inspector had the old columns.
        let id = cx.entity_id();
        self.reinspect = false;
        self.state.update(cx, |s, cx| {
            if s.inspected
                .as_ref()
                .is_some_and(|i| i.source.owner == Some(id))
            {
                s.inspect(None, cx);
            }
        });
        self.sync_structure(cx);
        self.attach(cx);
    }
}

impl TablePanel {
    /// The review sheet over the grid: every statement a commit runs, in order.
    fn review_sheet(&mut self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let review = self.review.as_ref()?;
        let statements = review.statements.clone();
        let n = statements.len();
        let error = self.commit_error.clone();
        let committing = self.committing;
        let tl = t.clone();
        let list = uniform_list("review-statements", n, move |range, _, _| {
            let t = &tl;
            range
                .map(|i| {
                    div()
                        .w_full()
                        .h(t.row_height)
                        .px(px(12.))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .border_b_1()
                        .border_color(t.grid_line)
                        .child(
                            div()
                                .flex_none()
                                .w(px(28.))
                                .text_color(t.text_faint)
                                .text_size(t.small_size)
                                .child((i + 1).to_string()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .font_family(t.mono_font.clone())
                                .text_size(t.mono_size)
                                .text_color(t.text)
                                .child(statements[i].clone()),
                        )
                })
                .collect()
        })
        .flex_1();
        let all = review.statements.clone();
        let title = format!(
            "{} {} to {}",
            thousands(n as u64),
            if n == 1 { "statement" } else { "statements" },
            self.object.name
        );
        let note = if self.driver.is_some_and(DriverKind::is_document) {
            "Run in order. MongoDB has no transaction here: what succeeds before a failure stays saved."
        } else {
            "Run in order in one transaction: all of them, or none if one fails. Values are sent as parameters."
        };
        Some(
            div()
                .id("review-sheet")
                .key_context("ReviewSheet")
                .track_focus(&self.review_focus)
                .occlude()
                .absolute()
                .top(px(0.))
                .left(px(0.))
                .right(px(0.))
                .max_h(px(420.))
                .h(px((n as f32 * f32::from(t.row_height) + 110.).min(420.)))
                .flex()
                .flex_col()
                .bg(t.floating)
                .border_b_1()
                .border_color(t.border_strong)
                .shadow_lg()
                .on_key_down(cx.listener(|this, e: &gpui::KeyDownEvent, window, cx| {
                    if e.keystroke.key == "escape" {
                        this.review = None;
                        this.grid.read(cx).focus(window);
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
                .child(
                    div()
                        .flex_none()
                        .px(px(12.))
                        .pt(px(10.))
                        .pb(px(6.))
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(t.ui_size)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(t.text)
                                .child(title),
                        )
                        .child(
                            div()
                                .text_size(t.small_size)
                                .text_color(t.text_faint)
                                .child(note),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .border_t_1()
                        .border_color(t.border)
                        .child(list),
                )
                .children(error.map(|e| {
                    div()
                        .flex_none()
                        .mx(px(12.))
                        .mt(px(8.))
                        .p(px(8.))
                        .rounded(t.radius_small)
                        .border_1()
                        .border_color(t.danger)
                        .text_size(t.small_size)
                        .text_color(t.danger)
                        .whitespace_normal()
                        .child(e)
                }))
                .child(
                    div()
                        .flex_none()
                        .px(px(12.))
                        .py(px(8.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            button("review-copy", "Copy SQL", ButtonStyle::Ghost, t).on_click(
                                move |_, _, cx| {
                                    let text = all
                                        .iter()
                                        .map(|s| s.as_ref())
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                                },
                            ),
                        )
                        .child(div().flex_1())
                        .child(
                            button("review-close", "Close", ButtonStyle::Secondary, t).on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.review = None;
                                    this.grid.read(cx).focus(window);
                                    cx.notify();
                                }),
                            ),
                        )
                        .child(
                            button("review-discard", "Discard all", ButtonStyle::Danger, t)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.confirm_discard = true;
                                    cx.notify();
                                })),
                        )
                        .child(
                            button(
                                "review-commit",
                                if committing {
                                    "Committing…"
                                } else {
                                    "Commit"
                                },
                                ButtonStyle::Primary,
                                t,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.commit(None, cx))),
                        ),
                )
                .into_any_element(),
        )
    }
}

/// What the staged changes are, for the status bar's tooltip.
fn pending_summary(cells: usize, deleted: usize, inserted: usize) -> SharedString {
    let mut parts = Vec::new();
    let mut part = |n: usize, one: &str, many: &str| {
        if n > 0 {
            parts.push(format!(
                "{} {}",
                thousands(n as u64),
                if n == 1 { one } else { many }
            ));
        }
    };
    part(cells, "changed cell", "changed cells");
    part(deleted, "row to delete", "rows to delete");
    part(inserted, "new row", "new rows");
    if parts.is_empty() {
        "Nothing staged".into()
    } else {
        format!("{}. Not committed yet.", parts.join(", ")).into()
    }
}

impl Render for TablePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        if self.loader.is_none() {
            let connecting = self
                .state
                .read(cx)
                .connecting
                .contains_key(&self.connection);
            let id = self.connection.clone();
            let name = self
                .state
                .read(cx)
                .settings
                .connection(&id)
                .map_or_else(|| id.clone(), |c| c.name.clone());
            return div()
                .size_full()
                .bg(t.surface)
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(10.))
                .child(
                    empty_state(
                        format!("{} is not connected", name),
                        Some(
                            format!("{}.{}", self.object.namespace.join("."), self.object.name)
                                .into(),
                        ),
                        &t,
                    )
                    .size_auto(),
                )
                .child(
                    button(
                        "connect",
                        if connecting {
                            "Connecting…"
                        } else {
                            "Connect"
                        },
                        ButtonStyle::Primary,
                        &t,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let id = id.clone();
                        this.state.update(cx, |s, cx| s.connect(&id, cx));
                    })),
                )
                .into_any_element();
        }
        let body = match self.mode {
            Mode::Data => self.grid.clone().into_any_element(),
            Mode::Structure => self.structure(window, cx),
        };
        let filters =
            (self.filters.open && self.mode == Mode::Data).then(|| self.filter_bar(&t, window, cx));
        let confirm = self.confirm_delete.clone().map(|rows| {
            let n = rows.len();
            div()
                .flex_none()
                .px(px(10.))
                .py(px(6.))
                .flex()
                .items_center()
                .gap(px(8.))
                .bg(t.base)
                .border_b_1()
                .border_color(t.danger)
                .text_size(t.ui_size)
                .child(div().flex_1().text_color(t.text).child(format!(
                    "Delete {} {} from {}? This can't be undone.",
                    thousands(n as u64),
                    if n == 1 { "row" } else { "rows" },
                    self.object.name
                )))
                .child(
                    button("delete-cancel", "Cancel", ButtonStyle::Secondary, &t).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.confirm_delete = None;
                            cx.notify();
                        }),
                    ),
                )
                .child(
                    button("delete-confirm", "Delete", ButtonStyle::Danger, &t).on_click(
                        cx.listener(move |this, _, _, cx| this.delete_rows(rows.clone(), cx)),
                    ),
                )
        });
        let discard = self.confirm_discard.then(|| {
            let n = self.pending_count(cx);
            div()
                .flex_none()
                .px(px(10.))
                .py(px(6.))
                .flex()
                .items_center()
                .gap(px(8.))
                .bg(t.base)
                .border_b_1()
                .border_color(t.danger)
                .text_size(t.ui_size)
                .child(div().flex_1().text_color(t.text).child(format!(
                    "Discard {} staged {} to {}? They have not been committed.",
                    thousands(n as u64),
                    if n == 1 { "change" } else { "changes" },
                    self.object.name
                )))
                .child(
                    button("discard-cancel", "Keep", ButtonStyle::Secondary, &t).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.confirm_discard = false;
                            cx.notify();
                        }),
                    ),
                )
                .child(
                    button("discard-confirm", "Discard", ButtonStyle::Danger, &t)
                        .on_click(cx.listener(|this, _, _, cx| this.discard(cx))),
                )
        });
        let review = self.review_sheet(&t, cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context("TablePanel")
            .bg(t.surface)
            .on_action(
                cx.listener(|this, _: &FocusFilter, window, cx| this.focus_filter(window, cx)),
            )
            .on_action(cx.listener(|this, _: &FollowReference, _, cx| this.follow_selected(cx)))
            .on_action(cx.listener(|this, _: &CommitChanges, window, cx| {
                this.flush_edit(window, cx);
                this.commit(None, cx);
            }))
            .on_action(cx.listener(|this, _: &ReviewChanges, window, cx| {
                this.flush_edit(window, cx);
                if this.review.is_some() {
                    this.review = None;
                    this.grid.read(cx).focus(window);
                    cx.notify();
                } else if this.pending_count(cx) > 0 {
                    this.open_review(cx);
                    window.focus(&this.review_focus);
                }
            }))
            .on_action(cx.listener(|this, _: &DiscardChanges, window, cx| {
                this.flush_edit(window, cx);
                if this.pending_count(cx) > 0 {
                    this.confirm_discard = true;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &AddRow, window, cx| this.add_row(window, cx)))
            .on_action(cx.listener(|this, _: &DuplicateRows, window, cx| {
                this.flush_edit(window, cx);
                let grid = this.grid.read(cx);
                if !grid.editable {
                    return;
                }
                let Some((rows, _)) = grid.selected_ranges() else {
                    return;
                };
                let rows: Vec<u64> = rows.take(ROW_ACTION_LIMIT).collect();
                this.duplicate_rows(rows, cx);
            }))
            .children(confirm)
            .children(discard)
            .children(filters)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(body)
                    .children(review),
            )
            .child(self.status_bar(&t, window, cx))
            .into_any_element()
    }
}
