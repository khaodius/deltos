//! A query tab: an SQL editor above, its results below. Each result streams into a
//! [`StreamLoader`], which spills it to disk as it is read, so a `SELECT` over a table of
//! any size shows its first rows at once and scrolls through the rest without holding
//! them in memory.
//!
//! Running several statements opens a result tab for each one that returns rows, with a
//! summary of what every statement did. A new run replaces the result tabs, except the
//! ones kept with the pin. A connection runs one statement at a time, so a result still
//! open when the next statement starts keeps the rows read so far (at least the first
//! [`KEEP_ROWS`]) and says that it was closed.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use deltos_data::{CancelHandle, Connection, DriverKind, ExecuteOptions};
use deltos_dock::PanelId;
use deltos_drivers::classify;
use deltos_grid::{
    CacheConfig, GridCache, PageLoader, PageRequest, RowCount, Status, StreamConfig, StreamLoader,
};
use deltos_settings::Data;
use futures::StreamExt;
use gpui::{
    AnyElement, App, AppContext, Bounds, Context, Entity, FocusHandle, Focusable, FontWeight,
    IntoElement, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Render, SharedString,
    Subscription, Task, Window, actions, canvas, div, prelude::*, px, uniform_list,
};

use crate::lang::format::{self as sqlformat, Options as FormatOptions};
use crate::lang::plan::{self, Plan, PlanSource};
use crate::lang::{Language, lex};
use crate::net::Net;
use crate::safety::{self, Confirm, Environment};
use crate::state::{AppState, HistoryEntry, Inspected, StateEvent};
use crate::theme::{Theme, theme};
use crate::ui::completion::query_completer;
use crate::ui::editor::{Editor, EditorEvent};
use crate::ui::environment;
use crate::ui::grid::{GridEvent, GridView};
use crate::ui::icons::{Icon, icon};
use crate::ui::{
    ButtonStyle, MenuItem, Tooltip, button, dot, empty_state, icon_button, menu, plan_view,
    thousands,
};

actions!(query, [RunQuery, RunAll, ExplainQuery, FormatSql]);

/// Rows of an open result read before the next statement on the connection closes it.
const KEEP_ROWS: u64 = 10_000;
/// Most rows read from an `EXPLAIN`.
const PLAN_ROWS: usize = 10_000;
/// Most result tabs open at once. Each holds a result file and a cache, and a script of a
/// thousand `SELECT setval(...)` lines should not open a thousand of them.
const MAX_RESULT_TABS: usize = 50;

/// Where a statement's run stands.
enum Outcome {
    /// Waiting for the server's first answer.
    Running,
    /// The result is open; rows may still be arriving.
    Open {
        loader: Arc<StreamLoader>,
        /// Time to the first answer.
        elapsed: Duration,
    },
    Failed(String),
}

struct Run {
    outcome: Outcome,
    /// Another statement ran after this one and closed its result.
    closed: bool,
}

enum PlanState {
    Running,
    Ready(Plan),
    Failed(String),
}

struct PlanTab {
    statement: String,
    source: PlanSource,
    analyze: bool,
    started: Instant,
    elapsed: Option<Duration>,
    state: PlanState,
}

#[derive(Clone)]
enum Step {
    Waiting,
    Running,
    Rows {
        tab: u64,
    },
    /// It returned rows, but too many result tabs were open to keep them.
    NotKept,
    Affected(Option<u64>),
    Failed(String),
    Skipped,
}

struct SummaryItem {
    statement: String,
    step: Step,
    elapsed: Option<Duration>,
}

enum Body {
    Result {
        run: Run,
        grid: Entity<GridView>,
        _subs: Vec<Subscription>,
    },
    Plan(PlanTab),
    Summary(Vec<SummaryItem>),
}

struct ResultTab {
    id: u64,
    title: SharedString,
    pinned: bool,
    body: Body,
}

/// The statement on the wire.
struct Running {
    started: Instant,
    cancel: Option<Arc<dyn CancelHandle>>,
    /// Its result tab, if it has one yet.
    tab: Option<u64>,
    /// Its line in the summary, in a run of several.
    index: Option<usize>,
}

/// What waits for the person to confirm it.
enum Pending {
    /// A run held before `statements[next]`; the ones before it ran. At 0 nothing has
    /// started yet.
    Run {
        statements: Rc<[String]>,
        next: usize,
        confirm: Confirm,
    },
    /// A plan to measure, which runs the statement.
    Explain(Confirm),
}

impl Pending {
    fn confirm(&self) -> &Confirm {
        match self {
            Pending::Run { confirm, .. } | Pending::Explain(confirm) => confirm,
        }
    }
}

pub struct QueryPanel {
    state: Entity<AppState>,
    connection: Option<String>,
    /// The connection, shared with the editor's completer.
    connection_cell: Rc<RefCell<Option<String>>>,
    editor: Entity<Editor>,
    tabs: Vec<ResultTab>,
    active_tab: Option<u64>,
    next_tab: u64,
    running: Option<Running>,
    /// The run in progress; dropping it stops the statements not started yet.
    batch: Option<Task<()>>,
    /// Set to stop reading ahead in the open results before the next statement.
    closing: Option<Arc<AtomicBool>>,
    /// Plans are measured by running the statement.
    explain_analyze: bool,
    /// The editor's share of the panel's height.
    split: f32,
    dragging_split: bool,
    bounds: Bounds<Pixels>,
    picker_open: bool,
    /// What waits for the person to confirm it.
    confirm: Option<Pending>,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl QueryPanel {
    pub fn new(
        state: Entity<AppState>,
        _id: PanelId,
        connection: Option<String>,
        text: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| Editor::new(text, "Write a statement and press Ctrl+Enter", cx));
        let connection = connection.or_else(|| state.read(cx).active.clone());
        let connection_cell = Rc::new(RefCell::new(connection.clone()));
        let completer = query_completer(state.clone(), connection_cell.clone(), None);
        editor.update(cx, |e, _| e.set_completer(completer));
        let subs = vec![
            cx.subscribe(&editor, |_, _, e, cx| match e {
                EditorEvent::Changed => cx.notify(),
            }),
            cx.subscribe(&state, |this, state, e, cx| {
                if let StateEvent::Sessions | StateEvent::ActiveSession = e {
                    // A tab opened before any connection follows the first one that opens.
                    if this.connection.is_none() {
                        this.connection = state.read(cx).active.clone();
                        // The tab and the title bar show the connection's environment.
                        cx.refresh_windows();
                    }
                    this.sync_connection(cx);
                    cx.notify();
                }
            }),
        ];
        let mut this = Self {
            state,
            connection,
            connection_cell,
            editor,
            tabs: Vec::new(),
            active_tab: None,
            next_tab: 1,
            running: None,
            batch: None,
            closing: None,
            explain_analyze: false,
            split: 0.4,
            dragging_split: false,
            bounds: Bounds::default(),
            picker_open: false,
            confirm: None,
            focus: cx.focus_handle(),
            _subs: subs,
        };
        this.sync_connection(cx);
        this
    }

    pub fn title(&self) -> SharedString {
        "SQL Query".into()
    }

    pub fn connection_id(&self) -> Option<&str> {
        self.connection.as_deref()
    }

    pub fn text(&self, cx: &App) -> String {
        self.editor.read(cx).text().to_owned()
    }

    fn connection_name(&self, cx: &App) -> Option<String> {
        let id = self.connection.as_ref()?;
        let s = self.state.read(cx);
        s.session(id)
            .map(|x| x.name.clone())
            .or_else(|| s.settings.connection(id).map(|c| c.name.clone()))
    }

    fn language(&self, cx: &App) -> Language {
        let s = self.state.read(cx);
        self.connection
            .as_deref()
            .and_then(|id| {
                s.session(id)
                    .map(|x| x.kind)
                    .or_else(|| s.settings.connection(id).map(|c| c.kind))
            })
            .map_or_else(Language::default, Language::for_kind)
    }

    /// Tells the editor and its completer which connection and language are in use.
    fn sync_connection(&mut self, cx: &mut Context<Self>) {
        *self.connection_cell.borrow_mut() = self.connection.clone();
        let language = self.language(cx);
        self.editor.update(cx, |e, cx| e.set_language(language, cx));
    }

    fn set_connection(&mut self, id: String, cx: &mut Context<Self>) {
        self.connection = Some(id);
        // A question asked about the other connection does not hold for this one.
        self.drop_confirm();
        self.sync_connection(cx);
        // The tab and the title bar show the connection's environment.
        cx.refresh_windows();
    }

    // ---- running -------------------------------------------------------------------------

    /// Runs the selection, or the statement the cursor is in.
    pub fn run_current(&mut self, cx: &mut Context<Self>) {
        let statement = self.editor.read(cx).statement_at_cursor();
        if !statement.is_empty() {
            self.run_statements(vec![statement], cx);
        }
    }

    /// Runs every statement of the editor (or of the selection) in order.
    pub fn run_all(&mut self, cx: &mut Context<Self>) {
        let statements = self.editor.read(cx).all_statements();
        if !statements.is_empty() {
            self.run_statements(statements, cx);
        }
    }

    /// What to ask before `statement` runs on this tab's connection, if anything.
    fn confirmation(&self, statement: &str, cx: &App) -> Option<Confirm> {
        let s = self.state.read(cx);
        let environment = self
            .connection
            .as_deref()
            .and_then(|id| Environment::of(s, id));
        let reason = safety::confirmation(statement, environment.as_ref(), &s.settings.data)?;
        Some(Confirm {
            statement: statement.to_owned(),
            reason,
            environment,
        })
    }

    /// Drops the question in the confirm bar. A run paused at it ends there, and the
    /// statements it had not run yet say so.
    fn drop_confirm(&mut self) {
        if let Some(Pending::Run { next, .. }) = self.confirm.take()
            && next > 0
        {
            self.fail_waiting("Cancelled.".into());
        }
    }

    /// Runs `statements`, asking first when the first one is to be confirmed, before
    /// anything changes. Each statement after it is checked when the run reaches it.
    fn run_statements(&mut self, statements: Vec<String>, cx: &mut Context<Self>) {
        self.drop_confirm();
        if let Some(confirm) = statements.first().and_then(|s| self.confirmation(s, cx)) {
            self.confirm = Some(Pending::Run {
                statements: statements.into(),
                next: 0,
                confirm,
            });
            cx.notify();
            return;
        }
        self.execute(statements, cx);
    }

    /// The connection to run on, or why there is none.
    fn open_connection(&self, cx: &App) -> Result<(String, Arc<dyn Connection>), String> {
        let id = self
            .connection
            .clone()
            .ok_or("Pick a connection to run this on.")?;
        let conn = self
            .state
            .read(cx)
            .session(&id)
            .map(|s| s.conn.clone())
            .ok_or("The connection is not open.")?;
        Ok((id, conn))
    }

    /// Starts a run: drops the result tabs that are not kept and runs `statements` one
    /// after the other, stopping at the first error. The first statement has been
    /// checked; see [`Self::run_from`] for the others.
    fn execute(&mut self, statements: Vec<String>, cx: &mut Context<Self>) {
        self.confirm = None;
        self.cancel(None, cx);
        self.drop_unpinned();
        let many = statements.len() > 1;
        if many {
            let items = statements
                .iter()
                .map(|s| SummaryItem {
                    statement: s.clone(),
                    step: Step::Waiting,
                    elapsed: None,
                })
                .collect();
            let id = self.add_tab("Summary".into(), Body::Summary(items));
            self.active_tab = Some(id);
        }
        if let Err(e) = self.open_connection(cx) {
            let first = statements.first().cloned().unwrap_or_default();
            self.fail(&first, e, cx);
            return;
        }
        self.run_from(statements.into(), 0, cx);
    }

    /// Runs `statements` from `from` on. `statements[from]` is cleared to run; each one
    /// after it goes through the connection's checks first, and one to confirm holds the
    /// run there until the person answers in the confirm bar.
    fn run_from(&mut self, statements: Rc<[String]>, from: usize, cx: &mut Context<Self>) {
        self.confirm = None;
        let many = statements.len() > 1;
        let task = cx.spawn(async move |this, cx| {
            for (index, statement) in statements.iter().enumerate().skip(from) {
                let statement = statement.clone();
                if index > from {
                    let held = this
                        .update(cx, |this, cx| this.hold(&statements, index, cx))
                        .unwrap_or(true);
                    if held {
                        return;
                    }
                }
                if close_open_results(&this, cx).await.is_none() {
                    return;
                }
                let Ok(Some((job, started))) = this.update(cx, |this, cx| {
                    this.start_statement(index, many, &statement, cx)
                }) else {
                    return;
                };
                let result = job
                    .await
                    .unwrap_or_else(|| Err("The request was cancelled.".into()));
                let go_on = this
                    .update(cx, |this, cx| {
                        this.statement_done(started, statement, result, cx)
                    })
                    .unwrap_or(false);
                if !go_on {
                    return;
                }
            }
        });
        self.batch = Some(task);
        cx.notify();
    }

    /// Holds the run before `statements[index]` when it is to be confirmed. Returns
    /// whether it did.
    fn hold(&mut self, statements: &Rc<[String]>, index: usize, cx: &mut Context<Self>) -> bool {
        let Some(confirm) = statements.get(index).and_then(|s| self.confirmation(s, cx)) else {
            return false;
        };
        self.confirm = Some(Pending::Run {
            statements: statements.clone(),
            next: index,
            confirm,
        });
        cx.notify();
        true
    }

    /// Sends one statement. Returns the answer to wait for and when it started.
    fn start_statement(
        &mut self,
        index: usize,
        many: bool,
        statement: &str,
        cx: &mut Context<Self>,
    ) -> Option<(
        impl std::future::Future<Output = Option<Result<StreamLoader, String>>> + use<>,
        Instant,
    )> {
        let (_, conn) = match self.open_connection(cx) {
            Ok(c) => c,
            Err(e) => {
                self.fail(statement, e, cx);
                return None;
            }
        };
        let s = self.state.read(cx);
        let data = s.settings.data.clone();
        let spill_dir = s.paths.as_ref().map(|p| p.spill_dir());
        let stream_config = StreamConfig {
            page_rows: data.page_size,
            max_spill_bytes: (data.spill_limit_mb > 0)
                .then(|| u64::from(data.spill_limit_mb) * 1024 * 1024),
            temp_dir: spill_dir,
        };
        let options = ExecuteOptions {
            batch_size: data.page_size.max(100),
            namespace: None,
        };
        let sql = statement.to_owned();
        let c2 = conn.clone();
        let job = cx.global::<Net>().run(async move {
            let stream = c2
                .execute(&sql, &options)
                .await
                .map_err(|e| e.to_string())?;
            // The spill file is the only disk work; it is created here, off the UI thread.
            if let Some(dir) = &stream_config.temp_dir {
                let _ = std::fs::create_dir_all(dir);
            }
            StreamLoader::new(stream, stream_config)
                .map_err(|e| format!("Could not create the result file: {e}"))
        });
        let started = Instant::now();
        let tab = if many {
            self.set_step(index, Step::Running, None);
            None
        } else {
            let grid = cx.new(GridView::new);
            let subs = self.watch_grid(&grid, cx);
            let id = self.add_tab(
                tab_title(statement, self.next_tab),
                Body::Result {
                    run: Run {
                        outcome: Outcome::Running,
                        closed: false,
                    },
                    grid,
                    _subs: subs,
                },
            );
            self.active_tab = Some(id);
            Some(id)
        };
        self.running = Some(Running {
            started,
            cancel: conn.cancel_handle(),
            tab,
            index: many.then_some(index),
        });
        let timeout = data.query_timeout_secs;
        if timeout > 0 {
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_secs(u64::from(timeout)))
                    .await;
                this.update(cx, |this, cx| {
                    if this.running.as_ref().is_some_and(|r| r.started == started) {
                        this.cancel(
                            Some(format!(
                                "Cancelled after {timeout} s (the statement timeout)."
                            )),
                            cx,
                        );
                    }
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
        Some((job, started))
    }

    /// Takes a statement's first answer. Returns whether the run goes on.
    fn statement_done(
        &mut self,
        started: Instant,
        statement: String,
        result: Result<StreamLoader, String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(running) = self.running.take_if(|r| r.started == started) else {
            return false;
        };
        let elapsed = started.elapsed();
        let config = self.cache_config(cx);
        let (rows, error) = match result {
            Ok(loader) => {
                let loader = Arc::new(loader);
                let rows = loader
                    .affected()
                    .or_else(|| loader.is_complete().then(|| loader.rows_read()));
                let has_rows = !loader.columns().is_empty();
                let tab = match running.tab {
                    Some(id) => Some(id),
                    None if has_rows
                        && self
                            .tabs
                            .iter()
                            .filter(|t| matches!(t.body, Body::Result { .. }))
                            .count()
                            < MAX_RESULT_TABS =>
                    {
                        let grid = cx.new(GridView::new);
                        let subs = self.watch_grid(&grid, cx);
                        let id = self.add_tab(
                            tab_title(&statement, self.next_tab),
                            Body::Result {
                                run: Run {
                                    outcome: Outcome::Running,
                                    closed: false,
                                },
                                grid,
                                _subs: subs,
                            },
                        );
                        // The first result of a run is the one to look at.
                        if self
                            .tab(self.active_tab)
                            .is_some_and(|t| matches!(t.body, Body::Summary(_)))
                            && !self.tabs.iter().any(|t| {
                                t.id != id && !t.pinned && matches!(t.body, Body::Result { .. })
                            })
                        {
                            self.active_tab = Some(id);
                        }
                        Some(id)
                    }
                    None => None,
                };
                if let Some(Body::Result { run, grid, .. }) = tab
                    .and_then(|id| self.tab_mut(Some(id)))
                    .map(|t| &mut t.body)
                {
                    if has_rows {
                        let l = loader.clone();
                        grid.update(cx, |g, cx| {
                            let hooks = g.make_hooks(cx);
                            let cache = Arc::new(GridCache::new(l, config, hooks));
                            g.selection = None;
                            g.set_cache(Some(cache), false, cx);
                        });
                    }
                    run.outcome = Outcome::Open {
                        loader: loader.clone(),
                        elapsed,
                    };
                }
                if let Some(index) = running.index {
                    let step = match tab {
                        Some(tab) if has_rows => Step::Rows { tab },
                        None if has_rows => Step::NotKept,
                        _ => Step::Affected(loader.affected()),
                    };
                    self.set_step(index, step, Some(elapsed));
                }
                (rows, None)
            }
            Err(e) => {
                if let Some(Body::Result { run, .. }) =
                    self.tab_mut(running.tab).map(|t| &mut t.body)
                {
                    run.outcome = Outcome::Failed(e.clone());
                }
                if let Some(index) = running.index {
                    self.set_step(index, Step::Failed(e.clone()), Some(elapsed));
                    self.skip_after(index);
                    // The summary says what went wrong.
                    self.active_tab = self
                        .tabs
                        .iter()
                        .find(|t| matches!(t.body, Body::Summary(_)))
                        .map(|t| t.id)
                        .or(self.active_tab);
                }
                (None, Some(e))
            }
        };
        let failed = error.is_some();
        let entry = HistoryEntry {
            at: SystemTime::now(),
            connection: self.connection.clone().unwrap_or_default(),
            statement,
            duration: elapsed,
            rows,
            error,
        };
        self.state.update(cx, |s, cx| s.record(entry, cx));
        cx.notify();
        !failed
    }

    fn cache_config(&self, cx: &App) -> CacheConfig {
        let data = &self.state.read(cx).settings.data;
        CacheConfig {
            page_rows: data.page_size,
            max_bytes: data.cache_memory_mb as usize * 1024 * 1024,
            ..CacheConfig::default()
        }
    }

    fn watch_grid(&mut self, grid: &Entity<GridView>, cx: &mut Context<Self>) -> Vec<Subscription> {
        vec![
            cx.subscribe(grid, |this, grid, e, cx| {
                if let GridEvent::Selected(row) = e {
                    let g = grid.read(cx);
                    let columns = g.columns().clone();
                    if let Some(values) = g.row_values(*row) {
                        let title = "Query result".to_owned();
                        let row = *row;
                        this.state.update(cx, |s, cx| {
                            s.inspect(
                                Some(Inspected {
                                    title,
                                    row,
                                    columns,
                                    values,
                                    source: Default::default(),
                                }),
                                cx,
                            )
                        });
                    }
                }
            }),
            cx.observe(grid, |_, _, cx| cx.notify()),
        ]
    }

    /// Shows an error that came before anything could run.
    fn fail(&mut self, statement: &str, message: String, cx: &mut Context<Self>) {
        let summary = self
            .tabs
            .iter_mut()
            .find(|t| matches!(t.body, Body::Summary(_)) && !t.pinned);
        if let Some(ResultTab {
            body: Body::Summary(items),
            id,
            ..
        }) = summary
        {
            if let Some(first) = items.iter_mut().find(|i| matches!(i.step, Step::Waiting)) {
                first.step = Step::Failed(message);
            }
            for item in items.iter_mut() {
                if matches!(item.step, Step::Waiting) {
                    item.step = Step::Skipped;
                }
            }
            self.active_tab = Some(*id);
        } else {
            let grid = cx.new(GridView::new);
            let id = self.add_tab(
                tab_title(statement, self.next_tab),
                Body::Result {
                    run: Run {
                        outcome: Outcome::Failed(message),
                        closed: false,
                    },
                    grid,
                    _subs: Vec::new(),
                },
            );
            self.active_tab = Some(id);
        }
        cx.notify();
    }

    /// Stops the statement on the wire and the ones after it. `reason` replaces the
    /// plain "Cancelled." message.
    fn cancel(&mut self, reason: Option<String>, cx: &mut Context<Self>) {
        self.batch = None;
        if let Some(stop) = self.closing.take() {
            stop.store(true, Ordering::Relaxed);
            // Stopped before the next statement went out: it and those after it are not run.
            if self.running.is_none() {
                self.fail_waiting(reason.clone().unwrap_or_else(|| "Cancelled.".into()));
                cx.notify();
            }
        }
        let Some(running) = self.running.take() else {
            return;
        };
        if let Some(handle) = running.cancel {
            cx.global::<Net>().spawn(async move {
                if let Err(e) = handle.cancel().await {
                    tracing::warn!("cancel failed: {e}");
                }
            });
        }
        let message = reason.unwrap_or_else(|| "Cancelled.".into());
        if let Some(index) = running.index {
            self.set_step(
                index,
                Step::Failed(message.clone()),
                Some(running.started.elapsed()),
            );
            self.skip_after(index);
        }
        match self.tab_mut(running.tab).map(|t| &mut t.body) {
            Some(Body::Result { run, .. }) if matches!(run.outcome, Outcome::Running) => {
                run.outcome = Outcome::Failed(message);
            }
            Some(Body::Plan(p)) if matches!(p.state, PlanState::Running) => {
                p.state = PlanState::Failed(message);
            }
            _ => {}
        }
        cx.notify();
    }

    /// Cancels a running statement and stops reading every result.
    fn stop(&mut self, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            if let Body::Result {
                run:
                    Run {
                        outcome: Outcome::Open { loader, .. },
                        ..
                    },
                ..
            } = &tab.body
            {
                loader.stop_fetching();
            }
        }
        self.cancel(None, cx);
        cx.notify();
    }

    fn is_running(&self) -> bool {
        self.running.is_some() || self.closing.is_some()
    }

    /// Marks the first statement of the run's summary that has not started as failed with
    /// `message`, and the rest as not run.
    fn fail_waiting(&mut self, message: String) {
        let mut message = Some(message);
        for tab in &mut self.tabs {
            if let Body::Summary(items) = &mut tab.body
                && !tab.pinned
            {
                for item in items.iter_mut() {
                    if matches!(item.step, Step::Waiting) {
                        item.step = match message.take() {
                            Some(m) => Step::Failed(m),
                            None => Step::Skipped,
                        };
                    }
                }
            }
        }
    }

    // ---- explain -------------------------------------------------------------------------

    /// Shows the plan of the statement at the cursor.
    pub fn explain(&mut self, cx: &mut Context<Self>) {
        let statement = self.editor.read(cx).statement_at_cursor();
        if statement.is_empty() {
            return;
        }
        self.explain_statement(statement, cx);
    }

    fn explain_statement(&mut self, statement: String, cx: &mut Context<Self>) {
        let source = PlanSource::for_dialect(self.language(cx).dialect());
        if self.explain_analyze && source.can_analyze() {
            let s = self.state.read(cx);
            let environment = self
                .connection
                .as_deref()
                .and_then(|id| Environment::of(s, id));
            if let Some(reason) =
                analyze_confirmation(&statement, source, environment.as_ref(), &s.settings.data)
            {
                self.drop_confirm();
                self.confirm = Some(Pending::Explain(Confirm {
                    statement,
                    reason,
                    environment,
                }));
                cx.notify();
                return;
            }
        }
        self.run_explain(statement, cx);
    }

    fn run_explain(&mut self, statement: String, cx: &mut Context<Self>) {
        self.confirm = None;
        self.cancel(None, cx);
        self.drop_unpinned();
        let source = PlanSource::for_dialect(self.language(cx).dialect());
        let analyze = self.explain_analyze && source.can_analyze();
        let started = Instant::now();
        let id = self.add_tab(
            "Plan".into(),
            Body::Plan(PlanTab {
                statement: statement.clone(),
                source,
                analyze,
                started,
                elapsed: None,
                state: PlanState::Running,
            }),
        );
        self.active_tab = Some(id);
        let conn = match self.open_connection(cx) {
            Ok((_, conn)) => conn,
            Err(e) => {
                if let Some(Body::Plan(p)) = self.tab_mut(Some(id)).map(|t| &mut t.body) {
                    p.state = PlanState::Failed(e);
                }
                cx.notify();
                return;
            }
        };
        // A read-only connection refuses the block a measured PostgreSQL plan runs in, as
        // it cannot tell what the block does. It checks a plain `EXPLAIN ANALYZE` like any
        // statement, and the server refuses writes on it too.
        let in_place = analyze
            && source == PlanSource::Postgres
            && self
                .connection
                .as_deref()
                .and_then(|id| Environment::of(self.state.read(cx), id))
                .is_some_and(|e| e.read_only);
        // `EXPLAIN a; b` would run `b` for real: one statement only.
        let sql = if crate::lang::split::statement_ranges(&statement, self.language(cx)).len() > 1 {
            Err("Explain shows one statement at a time. Select just one.".to_owned())
        } else if in_place {
            Ok(plan::measured_in_place(&statement))
        } else {
            plan::explain_statement(source, &statement, analyze)
        };
        let sql = match sql {
            Ok(sql) => sql,
            Err(e) => {
                if let Some(Body::Plan(p)) = self.tab_mut(Some(id)).map(|t| &mut t.body) {
                    p.state = PlanState::Failed(e);
                }
                cx.notify();
                return;
            }
        };
        let options = ExecuteOptions::default();
        self.running = Some(Running {
            started,
            cancel: conn.cancel_handle(),
            tab: Some(id),
            index: None,
        });
        let task = cx.spawn(async move |this, cx| {
            if close_open_results(&this, cx).await.is_none() {
                return;
            }
            let Ok(job) = this.update(cx, |_, cx| {
                cx.global::<Net>().run(async move {
                    let mut stream = conn
                        .execute(&sql, &options)
                        .await
                        .map_err(|e| e.to_string())?;
                    let columns: Vec<String> =
                        stream.columns.iter().map(|c| c.name.clone()).collect();
                    let messages = std::mem::take(&mut stream.messages);
                    let mut rows = Vec::new();
                    let mut batches = stream.batches;
                    'read: while let Some(batch) = batches.next().await {
                        for row in batch.map_err(|e| e.to_string())?.rows {
                            rows.push(row.into_vec());
                            if rows.len() >= PLAN_ROWS {
                                break 'read;
                            }
                        }
                    }
                    if analyze && source == PlanSource::Postgres && !in_place {
                        rows = plan::measured_plan(&messages)
                            .ok_or("The database returned no plan.")?;
                    }
                    plan::parse(source, &columns, &rows)
                })
            }) else {
                return;
            };
            let result = job
                .await
                .unwrap_or_else(|| Err("The request was cancelled.".into()));
            this.update(cx, |this, cx| {
                if this.running.take_if(|r| r.started == started).is_none() {
                    return;
                }
                if let Some(Body::Plan(p)) = this.tab_mut(Some(id)).map(|t| &mut t.body) {
                    p.elapsed = Some(p.started.elapsed());
                    p.state = match result {
                        Ok(plan) => PlanState::Ready(plan),
                        Err(e) => PlanState::Failed(e),
                    };
                }
                cx.notify();
            })
            .ok();
        });
        self.batch = Some(task);
        cx.notify();
    }

    fn toggle_analyze(&mut self, cx: &mut Context<Self>) {
        self.explain_analyze = !self.explain_analyze;
        let statement = match self.tab(self.active_tab).map(|t| &t.body) {
            Some(Body::Plan(p)) => Some(p.statement.clone()),
            _ => None,
        };
        if let Some(statement) = statement {
            self.explain_statement(statement, cx);
        }
        cx.notify();
    }

    // ---- format --------------------------------------------------------------------------

    /// Lays out the selection, or the whole text, as formatted SQL.
    pub fn format(&mut self, cx: &mut Context<Self>) {
        let Some(dialect) = self.language(cx).dialect() else {
            return;
        };
        let options = FormatOptions {
            uppercase: self.state.read(cx).settings.editor.format_uppercase,
        };
        self.editor.update(cx, |e, cx| {
            let selection = e.selection();
            let range = if selection.is_empty() {
                0..e.text().len()
            } else {
                selection
            };
            let Some(original) = e.text().get(range.clone()) else {
                return;
            };
            let formatted = sqlformat::format_sql(original, dialect, options);
            if formatted != original {
                e.replace_range(range, &formatted, cx);
            }
        });
    }

    // ---- result tabs ---------------------------------------------------------------------

    fn add_tab(&mut self, title: SharedString, body: Body) -> u64 {
        let id = self.next_tab;
        self.next_tab += 1;
        self.tabs.push(ResultTab {
            id,
            title,
            pinned: false,
            body,
        });
        id
    }

    fn tab(&self, id: Option<u64>) -> Option<&ResultTab> {
        let id = id?;
        self.tabs.iter().find(|t| t.id == id)
    }

    fn tab_mut(&mut self, id: Option<u64>) -> Option<&mut ResultTab> {
        let id = id?;
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    /// The tab to show: the chosen one, or the last.
    fn shown_tab(&self) -> Option<&ResultTab> {
        self.tab(self.active_tab).or_else(|| self.tabs.last())
    }

    fn close_tab(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.running.as_ref().is_some_and(|r| r.tab == Some(id)) {
            self.cancel(None, cx);
        }
        let Some(ix) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        self.tabs.remove(ix);
        if self.active_tab == Some(id) {
            self.active_tab = self.tabs.get(ix).or_else(|| self.tabs.last()).map(|t| t.id);
        }
        cx.notify();
    }

    fn set_step(&mut self, index: usize, step: Step, elapsed: Option<Duration>) {
        for tab in &mut self.tabs {
            if let Body::Summary(items) = &mut tab.body
                && !tab.pinned
                && let Some(item) = items.get_mut(index)
            {
                item.step = step;
                item.elapsed = elapsed.or(item.elapsed);
                return;
            }
        }
    }

    fn skip_after(&mut self, index: usize) {
        for tab in &mut self.tabs {
            if let Body::Summary(items) = &mut tab.body
                && !tab.pinned
            {
                for item in items.iter_mut().skip(index + 1) {
                    if matches!(item.step, Step::Waiting) {
                        item.step = Step::Skipped;
                    }
                }
            }
        }
    }

    /// Results whose stream is still open, to read on before the next statement.
    fn open_loaders(&self) -> Vec<(u64, Arc<StreamLoader>)> {
        self.tabs
            .iter()
            .filter_map(|t| match &t.body {
                Body::Result {
                    run:
                        Run {
                            outcome: Outcome::Open { loader, .. },
                            closed: false,
                            ..
                        },
                    ..
                } if !loader.is_complete()
                    && loader.stopped_reason().is_none()
                    && !loader.columns().is_empty() =>
                {
                    Some((t.id, loader.clone()))
                }
                _ => None,
            })
            .collect()
    }

    /// Drops the result tabs that are not pinned. The pinned ones keep reading until the
    /// next statement needs the connection.
    fn drop_unpinned(&mut self) {
        self.tabs.retain(|t| {
            if !t.pinned
                && let Body::Result {
                    run:
                        Run {
                            outcome: Outcome::Open { loader, .. },
                            ..
                        },
                    ..
                } = &t.body
            {
                loader.stop_fetching();
            }
            t.pinned
        });
    }

    fn mark_closed(&mut self, id: u64) {
        if let Some(Body::Result { run, .. }) = self.tab_mut(Some(id)).map(|t| &mut t.body)
            && let Outcome::Open { loader, .. } = &run.outcome
        {
            loader.stop_fetching();
            run.closed = !loader.is_complete();
        }
    }

    // ---- drawing -------------------------------------------------------------------------

    fn connection_picker(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let s = self.state.read(cx);
        let name = self.connection_name(cx);
        let open = self
            .connection
            .as_ref()
            .is_some_and(|id| s.session(id).is_some());
        let tag = self
            .connection
            .as_ref()
            .and_then(|id| s.session(id).and_then(|x| x.tag.color()))
            .map_or(t.accent, crate::theme::color);
        let mut picker = div()
            .id("query-connection")
            .relative()
            .h(t.s(24.))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(6.))
            .rounded(t.radius_small)
            .border_1()
            .border_color(t.border)
            .bg(t.input)
            .cursor_pointer()
            .hover(|s| s.bg(t.hover))
            .text_size(t.small_size)
            .child(dot(if open { tag } else { t.text_faint }, 7.))
            .child(
                div()
                    .text_color(if name.is_some() { t.text } else { t.text_faint })
                    .child(name.unwrap_or_else(|| "No connection".into())),
            )
            .child(
                icon(Icon::ChevronDown)
                    .size(px(11.))
                    .text_color(t.text_faint),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.picker_open = !this.picker_open;
                cx.notify();
            }));
        if self.picker_open {
            let weak = cx.entity().downgrade();
            let mut items: Vec<MenuItem> = Vec::new();
            let mut seen = Vec::new();
            for session in &s.sessions {
                seen.push(session.id.clone());
                let (w, id) = (weak.clone(), session.id.clone());
                items.push(
                    MenuItem::new(session.name.clone(), move |_, cx| {
                        w.update(cx, |this, cx| this.set_connection(id.clone(), cx))
                            .ok();
                    })
                    .detail("open")
                    .checked(self.connection.as_deref() == Some(session.id.as_str())),
                );
            }
            for saved in s
                .settings
                .connections
                .iter()
                .filter(|c| !seen.contains(&c.id))
            {
                let (w, id) = (weak.clone(), saved.id.clone());
                items.push(
                    MenuItem::new(saved.name.clone(), move |_, cx| {
                        w.update(cx, |this, cx| {
                            this.set_connection(id.clone(), cx);
                            let id = id.clone();
                            this.state.update(cx, |s, cx| s.connect(&id, cx));
                        })
                        .ok();
                    })
                    .checked(self.connection.as_deref() == Some(saved.id.as_str())),
                );
            }
            if items.is_empty() {
                items.push(MenuItem::new("No connections", |_, _| {}).enabled(false));
            }
            let close = weak.clone();
            picker = picker.child(menu(
                "query-connection-menu",
                items,
                200.,
                move |_, cx| {
                    close
                        .update(cx, |this, cx| {
                            this.picker_open = false;
                            cx.notify();
                        })
                        .ok();
                },
                t,
            ));
        }
        picker.into_any_element()
    }

    fn toolbar(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let running = self.is_running();
        let sql = self.language(cx).dialect().is_some();
        let keys = &self.state.read(cx).settings.keybinds;
        let run_tip = format!(
            "Run the statement at the cursor ({}), or every statement ({})",
            key_label(keys.get("run-query")),
            key_label(keys.get("run-all"))
        );
        let explain_tip = format!(
            "Show how the database runs the statement at the cursor ({})",
            key_label(keys.get("explain-query"))
        );
        let format_tip = format!(
            "Lay out the selection or the whole text ({})",
            key_label(keys.get("format-sql"))
        );
        div()
            .flex_none()
            .h(t.bar_height)
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(6.))
            .bg(t.base)
            .border_b_1()
            .border_color(t.border)
            .child(self.connection_picker(t, cx))
            .child(div().flex_1())
            .when(running, |el| {
                el.child(
                    button("query-stop", "Stop", ButtonStyle::Secondary, t)
                        .h(t.s(24.))
                        .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                )
            })
            .when(sql, |el| {
                el.child(
                    button("query-format", "Format", ButtonStyle::Ghost, t)
                        .h(t.s(24.))
                        .px(px(8.))
                        .text_size(t.small_size)
                        .tooltip(move |_, cx| Tooltip::view(format_tip.clone(), cx))
                        .on_click(cx.listener(|this, _, _, cx| this.format(cx))),
                )
            })
            .child(
                button("query-explain", "Explain", ButtonStyle::Ghost, t)
                    .h(t.s(24.))
                    .px(px(8.))
                    .text_size(t.small_size)
                    .tooltip(move |_, cx| Tooltip::view(explain_tip.clone(), cx))
                    .on_click(cx.listener(|this, _, _, cx| this.explain(cx))),
            )
            .child(
                div()
                    .id("query-run")
                    .h(t.s(24.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .rounded(t.radius_small)
                    .bg(t.accent)
                    .text_color(t.accent_text)
                    .text_size(t.small_size)
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.accent_hover))
                    .tooltip(move |_, cx| Tooltip::view(run_tip.clone(), cx))
                    .child(icon(Icon::Play).size(px(11.)))
                    .child("Run")
                    .on_click(cx.listener(|this, e: &gpui::ClickEvent, _, cx| {
                        if e.modifiers().shift {
                            this.run_all(cx);
                        } else {
                            this.run_current(cx);
                        }
                    })),
            )
            .into_any_element()
    }

    /// Asks before a statement runs, naming the connection in its environment's color.
    fn confirm_bar(&mut self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pending = self.confirm.as_ref()?;
        let confirm = pending.confirm();
        let env = confirm.environment.as_ref();
        let tint = env.and_then(|e| environment::tag_color(e.tag));
        let message = match pending {
            Pending::Run {
                statements, next, ..
            } if statements.len() > 1 => format!(
                "Statement {} of {}: {} Run it anyway?",
                next + 1,
                statements.len(),
                confirm.reason
            ),
            _ => format!("{} Run it anyway?", confirm.reason),
        };
        Some(
            div()
                .flex_none()
                .px(px(10.))
                .py(px(6.))
                .flex()
                .items_center()
                .gap(px(8.))
                .bg(tint.map_or(t.surface_alt, |c| c.opacity(0.08)))
                .border_b_1()
                .border_color(tint.map_or(t.border, |c| c.opacity(0.5)))
                .text_size(t.small_size)
                .child(
                    icon(Icon::Lock)
                        .size(px(12.))
                        .text_color(tint.unwrap_or(t.warning)),
                )
                .children(env.map(|e| environment::named(e, t)))
                .child(div().flex_1().min_w_0().text_color(t.text).child(message))
                .child(
                    button("confirm-cancel", "Cancel", ButtonStyle::Ghost, t).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.drop_confirm();
                            cx.notify();
                        }),
                    ),
                )
                .child(
                    button("confirm-run", "Run", ButtonStyle::Danger, t).on_click(cx.listener(
                        |this, _, _, cx| match this.confirm.take() {
                            Some(Pending::Run {
                                statements,
                                next: 0,
                                ..
                            }) => this.execute(statements.to_vec(), cx),
                            Some(Pending::Run {
                                statements, next, ..
                            }) => this.run_from(statements, next, cx),
                            Some(Pending::Explain(c)) => this.run_explain(c.statement, cx),
                            None => {}
                        },
                    )),
                )
                .into_any_element(),
        )
    }

    /// The strip of result tabs, when there is more than one or one is kept.
    fn tab_strip(&mut self, t: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.tabs.len() < 2 && !self.tabs.iter().any(|t| t.pinned) {
            return None;
        }
        let shown = self.shown_tab().map(|t| t.id);
        let mut strip = div()
            .id("result-tabs")
            .flex_none()
            .h(t.s(28.))
            .px(px(4.))
            .flex()
            .items_end()
            .gap(px(2.))
            .bg(t.base)
            .border_b_1()
            .border_color(t.border)
            .overflow_x_scroll();
        for tab in &self.tabs {
            let id = tab.id;
            let active = shown == Some(id);
            let which = match tab.body {
                Body::Result { .. } => Icon::Grid,
                Body::Plan(_) => Icon::Structure,
                Body::Summary(_) => Icon::History,
            };
            let failed = match &tab.body {
                Body::Result { run, .. } => matches!(run.outcome, Outcome::Failed(_)),
                Body::Plan(p) => matches!(p.state, PlanState::Failed(_)),
                Body::Summary(items) => items.iter().any(|i| matches!(i.step, Step::Failed(_))),
            };
            strip = strip.child(
                div()
                    .id(("result-tab", id))
                    .group("result-tab")
                    .h(t.s(24.))
                    .pl(px(8.))
                    .pr(px(4.))
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .rounded_t(t.radius_small)
                    .text_size(t.small_size)
                    .cursor_pointer()
                    .when(active, |el| {
                        el.bg(t.surface)
                            .text_color(t.text)
                            .border_1()
                            .border_b_0()
                            .border_color(t.border)
                    })
                    .when(!active, |el| {
                        el.text_color(t.text_muted)
                            .hover(|s| s.bg(t.hover).text_color(t.text))
                    })
                    .child(icon(which).size(px(11.)).text_color(if failed {
                        t.danger
                    } else {
                        t.text_faint
                    }))
                    .child(div().whitespace_nowrap().child(tab.title.clone()))
                    .when(tab.pinned, |el| {
                        el.child(icon(Icon::Lock).size(px(10.)).text_color(t.accent))
                    })
                    .child(
                        div()
                            .id(("result-tab-close", id))
                            .size(px(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(t.radius_small)
                            .text_color(t.text_faint)
                            .invisible()
                            .group_hover("result-tab", |s| s.visible())
                            .when(active, |el| el.visible())
                            .hover(|s| s.bg(t.hover).text_color(t.text))
                            .child(icon(Icon::Close).size(px(10.)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_tab(id, cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.active_tab = Some(id);
                        cx.notify();
                    })),
            );
        }
        Some(strip.into_any_element())
    }

    fn body(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.shown_tab() else {
            return empty_state(
                "No result yet",
                Some("Run a statement to see its rows here.".into()),
                t,
            )
            .bg(t.surface)
            .into_any_element();
        };
        match &tab.body {
            Body::Result { run, grid, .. } => result(run, grid, t),
            Body::Plan(p) => {
                let weak = cx.entity().downgrade();
                let plan = match &p.state {
                    PlanState::Ready(plan) => Some(plan),
                    _ => None,
                };
                let header = plan_view::header(
                    plan,
                    p.source,
                    p.analyze,
                    move |_, cx| {
                        weak.update(cx, |this, cx| this.toggle_analyze(cx)).ok();
                    },
                    t,
                );
                let content = match &p.state {
                    PlanState::Running => {
                        empty_state("Asking for the plan…", None, t).into_any_element()
                    }
                    PlanState::Failed(e) => error_box(e, t),
                    PlanState::Ready(plan) => plan_view::tree(plan, t),
                };
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .bg(t.surface)
                    .child(header)
                    .child(div().flex_1().min_h_0().child(content))
                    .into_any_element()
            }
            Body::Summary(items) => summary(tab.id, items.len(), t, cx),
        }
    }

    fn status_bar(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut left: Vec<AnyElement> = Vec::new();
        let mut fetch_all = None;
        let env = self
            .connection
            .as_deref()
            .and_then(|id| Environment::of(self.state.read(cx), id));
        let mut pin: Option<(u64, bool)> = None;
        let note = |text: String, color| div().text_color(color).child(text).into_any_element();
        match self.shown_tab() {
            Some(ResultTab {
                id,
                pinned,
                body: Body::Result { run, grid, .. },
                ..
            }) => {
                pin = Some((*id, *pinned));
                match &run.outcome {
                    Outcome::Running => left.push(note("Running…".into(), t.text_faint)),
                    Outcome::Failed(_) => left.push(note("Error".into(), t.danger)),
                    Outcome::Open { loader, elapsed } => {
                        let complete = loader.is_complete();
                        let read = loader.rows_read();
                        let count = grid.read(cx).cache().map(|c| (c.row_count(), c.status()));
                        let rows: String = match (loader.columns().is_empty(), complete) {
                            (true, _) => String::new(),
                            (false, true) => format!(
                                "{} {}",
                                thousands(read),
                                if read == 1 { "row" } else { "rows" }
                            ),
                            (false, false) => format!("{}+ rows", thousands(read)),
                        };
                        if !rows.is_empty() {
                            left.push(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(t.text)
                                    .child(rows)
                                    .into_any_element(),
                            );
                        }
                        left.push(note(
                            format!("{:.0} ms", elapsed.as_secs_f64() * 1000.0),
                            t.text_faint,
                        ));
                        if run.closed && !complete {
                            left.push(note(
                                "Closed when the next statement ran; the rows read so far stay"
                                    .into(),
                                t.warning,
                            ));
                        } else if let Some(reason) = loader.stopped_reason() {
                            left.push(note(reason, t.warning));
                        }
                        if let Some((_, Status::Error(e))) = count {
                            if !run.closed {
                                left.push(note(e, t.danger));
                            }
                        } else if let Some((
                            RowCount::AtLeast(_) | RowCount::Unknown,
                            Status::Loading,
                        )) = count
                        {
                            left.push(note("Reading…".into(), t.text_faint));
                        }
                        let spill = loader.spill_bytes();
                        if spill > 0 {
                            left.push(note(
                                format!("{:.1} MB on disk", spill as f64 / 1_048_576.0),
                                t.text_faint,
                            ));
                        }
                        if !complete
                            && !run.closed
                            && loader.stopped_reason().is_none()
                            && !loader.columns().is_empty()
                        {
                            fetch_all = Some(loader.clone());
                        }
                    }
                }
            }
            Some(ResultTab {
                id,
                pinned,
                body: Body::Plan(p),
                ..
            }) => {
                pin = Some((*id, *pinned));
                match &p.state {
                    PlanState::Running => left.push(note("Running…".into(), t.text_faint)),
                    PlanState::Failed(_) => left.push(note("Error".into(), t.danger)),
                    PlanState::Ready(plan) => {
                        let steps = plan.lines().len();
                        left.push(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(t.text)
                                .child(format!(
                                    "{} {}",
                                    thousands(steps as u64),
                                    if steps == 1 { "step" } else { "steps" }
                                ))
                                .into_any_element(),
                        );
                        if let Some(e) = p.elapsed {
                            left.push(note(
                                format!("{:.0} ms", e.as_secs_f64() * 1000.0),
                                t.text_faint,
                            ));
                        }
                    }
                }
            }
            Some(ResultTab {
                body: Body::Summary(items),
                ..
            }) => {
                let done = items
                    .iter()
                    .filter(|i| matches!(i.step, Step::Rows { .. } | Step::Affected(_)))
                    .count();
                left.push(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(t.text)
                        .child(format!("{done} of {} statements ran", items.len()))
                        .into_any_element(),
                );
                let total: Duration = items.iter().filter_map(|i| i.elapsed).sum();
                left.push(note(
                    format!("{:.0} ms", total.as_secs_f64() * 1000.0),
                    t.text_faint,
                ));
            }
            None => {}
        }
        div()
            .flex_none()
            .h(t.bar_height)
            .px(px(10.))
            .flex()
            .items_center()
            .gap(px(12.))
            .bg(t.base)
            .border_t_1()
            .border_color(t.border)
            .text_size(t.small_size)
            .text_color(t.text_muted)
            // A long message is cut at the edge, so the buttons and the environment
            // badge stay in view.
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .children(left),
            )
            .when_some(fetch_all, |el, loader| {
                let l2 = loader.clone();
                el.child(
                    button("fetch-all", "Fetch all", ButtonStyle::Ghost, t)
                        .h(t.s(22.))
                        .tooltip(|_, cx| {
                            Tooltip::view("Read the whole result now, for an exact count", cx)
                        })
                        .on_click(cx.listener(move |_, _, _, cx| {
                            loader.fetch_all();
                            cx.notify();
                        })),
                )
                .child(
                    button("stop-reading", "Stop", ButtonStyle::Ghost, t)
                        .h(t.s(22.))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            l2.stop_fetching();
                            cx.notify();
                        })),
                )
            })
            .when_some(pin, |el, (id, pinned)| {
                el.child(
                    icon_button("result-pin", Icon::Lock, t)
                        .when(pinned, |el| el.text_color(t.accent))
                        .tooltip(move |_, cx| {
                            Tooltip::view(
                                if pinned {
                                    "Kept: the next run opens a new tab. Click to let it go."
                                } else {
                                    "Keep this result: the next run opens a new tab"
                                },
                                cx,
                            )
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(tab) = this.tab_mut(Some(id)) {
                                tab.pinned = !tab.pinned;
                            }
                            cx.notify();
                        })),
                )
            })
            .children(environment::status(env.as_ref(), t))
            .into_any_element()
    }
}

/// Reads on in every open result and marks it closed, before another statement runs on
/// the connection. `None` when the panel is gone.
async fn close_open_results(
    this: &gpui::WeakEntity<QueryPanel>,
    cx: &mut gpui::AsyncApp,
) -> Option<()> {
    let (open, page_rows) = this
        .update(cx, |this, cx| {
            (
                this.open_loaders(),
                this.state.read(cx).settings.data.page_size.max(1),
            )
        })
        .ok()?;
    if open.is_empty() {
        return Some(());
    }
    // Stop shows while this reads, and stops it between pages: a page is read whole, so
    // what was read stays in order.
    let stop = Arc::new(AtomicBool::new(false));
    this.update(cx, |this, cx| {
        this.closing = Some(stop.clone());
        cx.notify();
    })
    .ok()?;
    for (id, loader) in open {
        let (l, flag) = (loader.clone(), stop.clone());
        let job = this
            .update(cx, |_, cx| {
                cx.global::<Net>().run(async move {
                    let rows = u64::from(page_rows);
                    for page in l.rows_read() / rows..KEEP_ROWS.div_ceil(rows) {
                        if flag.load(Ordering::Relaxed)
                            || l.is_complete()
                            || l.stopped_reason().is_some()
                            || l.load(PageRequest::new(page, page_rows)).await.is_err()
                        {
                            break;
                        }
                    }
                })
            })
            .ok()?;
        job.await;
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        this.update(cx, |this, cx| {
            this.mark_closed(id);
            cx.notify();
        })
        .ok()?;
    }
    this.update(cx, |this, cx| {
        if this.closing.as_ref().is_some_and(|c| Arc::ptr_eq(c, &stop)) {
            this.closing = None;
        }
        cx.notify();
    })
    .ok()?;
    Some(())
}

fn result(run: &Run, grid: &Entity<GridView>, t: &Theme) -> AnyElement {
    match &run.outcome {
        Outcome::Running => empty_state("Running…", None, t)
            .bg(t.surface)
            .into_any_element(),
        Outcome::Failed(e) => error_box(e, t),
        Outcome::Open { loader, .. } if loader.columns().is_empty() => {
            let detail: SharedString = match loader.affected() {
                Some(n) => format!(
                    "{} {} affected",
                    thousands(n),
                    if n == 1 { "row" } else { "rows" }
                )
                .into(),
                None => "The statement ran.".into(),
            };
            let messages = loader.messages().join("\n");
            empty_state(detail, (!messages.is_empty()).then(|| messages.into()), t)
                .bg(t.surface)
                .into_any_element()
        }
        Outcome::Open { .. } => grid.clone().into_any_element(),
    }
}

fn error_box(message: &str, t: &Theme) -> AnyElement {
    div()
        .size_full()
        .bg(t.surface)
        .p(px(12.))
        .child(
            div()
                .px(px(10.))
                .py(px(8.))
                .rounded(t.radius_small)
                .border_1()
                .border_color(t.danger.opacity(0.4))
                .bg(t.danger.opacity(0.06))
                .font_family(t.mono_font.clone())
                .text_size(t.mono_size)
                .text_color(t.danger)
                .whitespace_normal()
                .child(message.to_owned()),
        )
        .into_any_element()
}

/// One line per statement of a run: what it did and how long it took. A line with rows
/// opens its tab. Only the lines in view are built, as a script can hold thousands.
fn summary(tab_id: u64, len: usize, t: &Theme, cx: &mut Context<QueryPanel>) -> AnyElement {
    let panel = cx.entity().downgrade();
    let t = t.clone();
    let bg = t.surface;
    uniform_list("run-summary", len, move |range, _, cx| {
        let Some(this) = panel.upgrade() else {
            return Vec::new();
        };
        let this = this.read(cx);
        let Some(Body::Summary(items)) = this.tab(Some(tab_id)).map(|tab| &tab.body) else {
            return Vec::new();
        };
        range
            .filter_map(|ix| Some(summary_line(ix, items.get(ix)?, &panel, &t)))
            .collect()
    })
    .size_full()
    .bg(bg)
    .into_any_element()
}

fn summary_line(
    ix: usize,
    item: &SummaryItem,
    panel: &gpui::WeakEntity<QueryPanel>,
    t: &Theme,
) -> AnyElement {
    let (text, color): (String, _) = match &item.step {
        Step::Waiting => ("Waiting".into(), t.text_faint),
        Step::Running => ("Running…".into(), t.text_muted),
        Step::Rows { .. } => ("Rows: open the tab".into(), t.accent),
        Step::Affected(Some(n)) => (
            format!(
                "{} {} affected",
                thousands(*n),
                if *n == 1 { "row" } else { "rows" }
            ),
            t.text_muted,
        ),
        Step::Affected(None) => ("Done".into(), t.text_muted),
        Step::NotKept => (
            format!("Rows not kept: {MAX_RESULT_TABS} results are open"),
            t.text_muted,
        ),
        Step::Failed(e) => (e.clone(), t.danger),
        Step::Skipped => ("Not run".into(), t.text_faint),
    };
    let open = match item.step {
        Step::Rows { tab } => Some(tab),
        _ => None,
    };
    // An error can be longer than the line: all of it shows on hover.
    let full = matches!(item.step, Step::Failed(_)).then(|| text.clone());
    let panel = panel.clone();
    div()
        .id(("summary-line", ix))
        .w_full()
        .h(t.s(26.))
        .px(px(10.))
        .flex()
        .items_center()
        .gap(px(10.))
        .border_b_1()
        .border_color(t.grid_line)
        .text_size(t.small_size)
        .when_some(open, |el, tab| {
            el.cursor_pointer()
                .hover(|s| s.bg(t.hover))
                .on_click(move |_, _, cx| {
                    panel
                        .update(cx, |this, cx| {
                            this.active_tab = Some(tab);
                            cx.notify();
                        })
                        .ok();
                })
        })
        .when_some(full, |el, full| {
            el.tooltip(move |_, cx| Tooltip::view(full.clone(), cx))
        })
        .child(
            div()
                .flex_none()
                .w(px(24.))
                .text_color(t.text_faint)
                .child(format!("{}", ix + 1)),
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
                .child(one_line(&item.statement)),
        )
        .child(
            div()
                .flex_none()
                .max_w(px(360.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(color)
                .child(text),
        )
        .child(
            div()
                .flex_none()
                .w(px(64.))
                .text_right()
                .text_color(t.text_faint)
                .child(
                    item.elapsed
                        .map(|e| format!("{:.0} ms", e.as_secs_f64() * 1000.0))
                        .unwrap_or_default(),
                ),
        )
        .into_any_element()
}

/// A statement on one line, cut short for a list.
fn one_line(statement: &str) -> String {
    // Comment lines above the statement say less about it than its first words.
    let code = statement
        .lines()
        .skip_while(|l| l.trim().is_empty() || l.trim_start().starts_with("--"))
        .collect::<Vec<_>>();
    let text = if code.is_empty() {
        statement.split_whitespace().collect::<Vec<_>>()
    } else {
        code.iter().flat_map(|l| l.split_whitespace()).collect()
    };
    let mut s: String = text.join(" ");
    if s.len() > 300 {
        let mut cut = 300;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
    s
}

/// A result tab's name: the table it reads from, or `Result n`.
fn tab_title(statement: &str, n: u64) -> SharedString {
    let tokens = lex::tokens(statement, crate::lang::Dialect::Postgres);
    let code: Vec<&lex::Token> = tokens.iter().filter(|t| t.is_code()).collect();
    for (i, t) in code.iter().enumerate() {
        if t.kind == lex::Kind::Word && t.text(statement).eq_ignore_ascii_case("from") {
            let mut j = i + 1;
            let mut name = None;
            while let Some(part) = code.get(j) {
                if !matches!(part.kind, lex::Kind::Word | lex::Kind::Quoted) {
                    break;
                }
                name = Some(lex::unquote(part.text(statement)));
                if code.get(j + 1).is_some_and(|d| d.text(statement) == ".") {
                    j += 2;
                } else {
                    break;
                }
            }
            if let Some(name) = name {
                return name.into();
            }
        }
    }
    // MongoDB: `db.name.find(...)`.
    if let Some(rest) = statement.trim_start().strip_prefix("db.") {
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        if !name.is_empty() && rest[name.len()..].starts_with('.') {
            return name.into();
        }
    }
    format!("Result {n}").into()
}

/// A key binding as people write it: `ctrl-shift-enter` as `Ctrl+Shift+Enter`. The first
/// of several bindings.
pub fn key_label(keys: Option<&str>) -> String {
    let Some(first) = keys.and_then(|k| k.split_whitespace().next()) else {
        return "unbound".into();
    };
    first
        .split('-')
        .filter(|p| !p.is_empty())
        .map(|part| {
            let mut c = part.chars();
            match c.next() {
                Some(f) => f.to_uppercase().chain(c).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Why measuring the plan of `statement` should be confirmed. Measuring runs it, so
/// anything the classifier does not know to be a read is asked about, led by what the
/// connection's own checks say about it. A read-only connection asks nothing: it refuses
/// a statement that writes.
fn analyze_confirmation(
    statement: &str,
    source: PlanSource,
    env: Option<&Environment>,
    data: &Data,
) -> Option<String> {
    if env.is_some_and(|e| e.read_only) {
        return None;
    }
    let kind = env.map_or(
        match source {
            PlanSource::Postgres => DriverKind::Postgres,
            PlanSource::Mysql => DriverKind::Mysql,
            PlanSource::Sqlite => DriverKind::Sqlite,
            PlanSource::Mongo => DriverKind::Mongodb,
        },
        |e| e.kind,
    );
    let a = classify::assess(statement, kind);
    if a.is_read_only() {
        return None;
    }
    let what = safety::confirmation(statement, env, data)
        .unwrap_or_else(|| format!("{} changes data.", a.what));
    Some(match source {
        PlanSource::Postgres => format!(
            "{what} Measuring the plan runs it. What it changes is rolled back, but it takes \
             the time and the locks it needs."
        ),
        _ => format!("{what} Measuring the plan runs it."),
    })
}

impl Focusable for QueryPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for QueryPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let toolbar = self.toolbar(&t, cx);
        let confirm = self.confirm_bar(&t, cx);
        let strip = self.tab_strip(&t, cx);
        let body = self.body(&t, cx);
        let status = self.status_bar(&t, cx);
        let split = self.split;
        let weak = cx.entity().downgrade();
        div()
            .key_context("QueryPanel")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(t.surface)
            .on_action(cx.listener(|this, _: &RunQuery, _, cx| this.run_current(cx)))
            .on_action(cx.listener(|this, _: &RunAll, _, cx| this.run_all(cx)))
            .on_action(cx.listener(|this, _: &ExplainQuery, _, cx| this.explain(cx)))
            .on_action(cx.listener(|this, _: &FormatSql, _, cx| this.format(cx)))
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                if this.dragging_split && this.bounds.size.height > px(0.) {
                    let y = (e.position.y - this.bounds.origin.y) / this.bounds.size.height;
                    this.split = y.clamp(0.1, 0.9);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    if this.dragging_split {
                        this.dragging_split = false;
                        cx.notify();
                    }
                }),
            )
            .child(
                canvas(
                    move |bounds, _, cx| {
                        weak.update(cx, |this, _| this.bounds = bounds).ok();
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(toolbar)
            .children(confirm)
            .child(
                div()
                    .flex_grow()
                    .flex_basis(gpui::relative(split))
                    .min_h(px(60.))
                    .bg(t.surface)
                    .child(self.editor.clone()),
            )
            .child(
                div()
                    .id("query-split")
                    .flex_none()
                    .h(px(5.))
                    .flex()
                    .items_center()
                    .cursor_row_resize()
                    .child(div().w_full().h(px(1.)).bg(if self.dragging_split {
                        t.accent
                    } else {
                        t.border
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.dragging_split = true;
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            )
            .child(
                div()
                    .flex_grow()
                    .flex_basis(gpui::relative(1.0 - split))
                    .min_h(px(60.))
                    .flex()
                    .flex_col()
                    .children(strip)
                    .child(div().flex_1().min_h_0().child(body))
                    .child(status),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_settings::ConnectionTag;

    #[test]
    fn measuring_a_plan_asks_before_a_write() {
        let data = Data::default();
        let pg = |sql: &str| analyze_confirmation(sql, PlanSource::Postgres, None, &data);
        assert!(pg("insert into t values (1)").is_some());
        assert!(pg("with x as (delete from t returning *) select * from x").is_some());
        assert!(pg("with x as (select 1) select * from x").is_none());
        assert!(pg("select * from deleted_rows").is_none());
        assert!(pg("-- tidy up\ndelete from t").is_some());
        assert!(pg("/* a */ (update t set a = 1)").is_some());
        assert!(pg("select * into copy_of_t from t").is_some());
        assert!(pg("select 'delete' from t").is_none());
        let mongo = |s: &str| analyze_confirmation(s, PlanSource::Mongo, None, &data);
        assert!(mongo("db.orders.find({})").is_none());
        assert!(mongo("db.orders.aggregate([{$out: 'copy'}])").is_some());
        // What the connection's checks say about the statement leads.
        assert!(
            pg("delete from t")
                .is_some_and(|m| m.starts_with("DELETE without WHERE touches every row."))
        );
        let env = |tag, read_only| Environment {
            name: "Main".into(),
            kind: DriverKind::Postgres,
            tag,
            read_only,
        };
        // A plain run of this write asks nothing here; measuring it still does.
        let dev = env(ConnectionTag::Development, false);
        let ask = |sql: &str, e: &Environment| {
            analyze_confirmation(sql, PlanSource::Postgres, Some(e), &data)
        };
        assert!(safety::confirmation("insert into t values (1)", Some(&dev), &data).is_none());
        assert!(ask("insert into t values (1)", &dev).is_some());
        assert!(ask("select 1", &env(ConnectionTag::Production, false)).is_none());
        // A read-only connection refuses the write itself.
        assert!(ask("delete from t", &env(ConnectionTag::Production, true)).is_none());
    }

    /// What a read-only connection sends to measure a plan: its own checks pass a read
    /// and refuse a write.
    #[test]
    fn read_only_connections_measure_in_place() {
        let read = plan::measured_in_place("select * from t;");
        assert_eq!(
            read,
            "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) select * from t"
        );
        assert!(classify::assess(&read, DriverKind::Postgres).is_read_only());
        let write = plan::measured_in_place("delete from t where id = 1");
        assert!(!classify::assess(&write, DriverKind::Postgres).is_read_only());
    }

    #[test]
    fn result_tabs_are_named_after_their_table() {
        assert_eq!(tab_title("select * from public.orders o", 1), "orders");
        assert_eq!(tab_title("SELECT 1", 2), "Result 2");
        assert_eq!(tab_title("select * from \"My Table\"", 3), "My Table");
        assert_eq!(tab_title("db.orders.find({})", 4), "orders");
        assert_eq!(tab_title("select (select 1 from t) as x", 5), "t");
    }

    #[test]
    fn summary_lines_skip_comments_above_the_statement() {
        assert_eq!(one_line("-- why\n\nselect 1\n  from t"), "select 1 from t");
        assert_eq!(one_line("-- only a note"), "-- only a note");
        assert_eq!(one_line("select 1 -- trailing"), "select 1 -- trailing");
    }

    #[test]
    fn key_labels_read_naturally() {
        assert_eq!(key_label(Some("ctrl-shift-enter")), "Ctrl+Shift+Enter");
        assert_eq!(key_label(Some("ctrl-e alt-e")), "Ctrl+E");
        assert_eq!(key_label(None), "unbound");
        assert_eq!(key_label(Some("")), "unbound");
    }
}
