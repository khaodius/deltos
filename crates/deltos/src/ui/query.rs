//! A query tab: an SQL editor above, the result below. The result streams into a
//! [`StreamLoader`], which spills it to disk as it is read, so a `SELECT` over a table of
//! any size shows its first rows at once and scrolls through the rest without holding
//! them in memory.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use deltos_data::{CancelHandle, Connection, ExecuteOptions};
use deltos_dock::PanelId;
use deltos_grid::{CacheConfig, GridCache, RowCount, Status, StreamConfig, StreamLoader};
use gpui::{
    AnyElement, App, AppContext, Bounds, Context, Entity, FocusHandle, Focusable, FontWeight,
    IntoElement, MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, Render, SharedString,
    Subscription, Task, Window, actions, canvas, div, prelude::*, px,
};

use crate::net::Net;
use crate::safety::{self, Confirm, Environment};
use crate::state::{AppState, HistoryEntry, Inspected, StateEvent};
use crate::theme::{Theme, theme};
use crate::ui::editor::{Editor, EditorEvent};
use crate::ui::environment;
use crate::ui::grid::{GridEvent, GridView};
use crate::ui::icons::{Icon, icon};
use crate::ui::{ButtonStyle, MenuItem, Tooltip, button, dot, empty_state, menu, thousands};

actions!(query, [RunQuery]);

/// Where the last run stands.
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
    statement: String,
    started: Instant,
    outcome: Outcome,
    cancel: Option<Arc<dyn CancelHandle>>,
    /// Holds the request; dropping it abandons the answer.
    _task: Option<Task<()>>,
}

pub struct QueryPanel {
    state: Entity<AppState>,
    connection: Option<String>,
    editor: Entity<Editor>,
    grid: Entity<GridView>,
    run: Option<Run>,
    /// The editor's share of the panel's height.
    split: f32,
    dragging_split: bool,
    bounds: Bounds<Pixels>,
    picker_open: bool,
    /// A statement waiting for the person to confirm it.
    confirm: Option<Confirm>,
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
        let grid = cx.new(GridView::new);
        let subs = vec![
            cx.subscribe(&editor, |_, _, e, cx| match e {
                EditorEvent::Changed => cx.notify(),
            }),
            cx.subscribe(&grid, |this, grid, e, cx| {
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
            cx.observe(&grid, |_, _, cx| cx.notify()),
            cx.subscribe(&state, |this, state, e, cx| {
                if let StateEvent::Sessions | StateEvent::ActiveSession = e {
                    // A tab opened before any connection follows the first one that opens.
                    if this.connection.is_none() {
                        this.connection = state.read(cx).active.clone();
                        // The tab and the title bar show the connection's environment.
                        cx.refresh_windows();
                    }
                    cx.notify();
                }
            }),
        ];
        let connection = connection.or_else(|| state.read(cx).active.clone());
        Self {
            state,
            connection,
            editor,
            grid,
            run: None,
            split: 0.4,
            dragging_split: false,
            bounds: Bounds::default(),
            picker_open: false,
            confirm: None,
            focus: cx.focus_handle(),
            _subs: subs,
        }
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

    /// Runs the selection, or the statement the cursor is in.
    pub fn run_current(&mut self, cx: &mut Context<Self>) {
        let statement = self.editor.read(cx).statement_at_cursor();
        if statement.is_empty() {
            return;
        }
        let s = self.state.read(cx);
        let environment = self
            .connection
            .as_deref()
            .and_then(|id| Environment::of(s, id));
        if let Some(reason) =
            safety::confirmation(&statement, environment.as_ref(), &s.settings.data)
        {
            self.confirm = Some(Confirm {
                statement,
                reason,
                environment,
            });
            cx.notify();
            return;
        }
        self.execute(statement, cx);
    }

    fn execute(&mut self, statement: String, cx: &mut Context<Self>) {
        self.confirm = None;
        self.stop(cx);
        let Some(id) = self.connection.clone() else {
            self.fail(statement, "Pick a connection to run this on.".into(), cx);
            return;
        };
        let Some(conn) = self.state.read(cx).session(&id).map(|s| s.conn.clone()) else {
            self.fail(statement, "The connection is not open.".into(), cx);
            return;
        };
        let s = self.state.read(cx);
        let data = s.settings.data.clone();
        let spill_dir = s.paths.as_ref().map(|p| p.spill_dir());
        let stream_config = StreamConfig {
            page_rows: data.page_size,
            max_spill_bytes: (data.spill_limit_mb > 0)
                .then(|| u64::from(data.spill_limit_mb) * 1024 * 1024),
            temp_dir: spill_dir.clone(),
        };
        let cache_config = CacheConfig {
            page_rows: data.page_size,
            max_bytes: data.cache_memory_mb as usize * 1024 * 1024,
            ..CacheConfig::default()
        };
        let options = ExecuteOptions {
            batch_size: data.page_size.max(100),
            namespace: None,
        };
        let sql = statement.clone();
        let c2: Arc<dyn Connection> = conn.clone();
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
        let timeout = data.query_timeout_secs;
        let task = cx.spawn(async move |this, cx| {
            let result = job
                .await
                .unwrap_or_else(|| Err("The request was cancelled.".into()));
            this.update(cx, |this, cx| this.finished(result, cache_config, cx))
                .ok();
        });
        self.run = Some(Run {
            statement,
            started,
            outcome: Outcome::Running,
            cancel: conn.cancel_handle(),
            _task: Some(task),
        });
        self.grid.update(cx, |g, cx| g.set_cache(None, false, cx));
        if timeout > 0 {
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_secs(u64::from(timeout)))
                    .await;
                this.update(cx, |this, cx| {
                    let waiting = this.run.as_ref().is_some_and(|r| {
                        matches!(r.outcome, Outcome::Running) && r.started == started
                    });
                    if waiting {
                        this.cancel(cx);
                        if let Some(run) = &mut this.run {
                            run.outcome = Outcome::Failed(format!(
                                "Cancelled after {timeout} s (the statement timeout)."
                            ));
                        }
                    }
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    fn finished(
        &mut self,
        result: Result<StreamLoader, String>,
        config: CacheConfig,
        cx: &mut Context<Self>,
    ) {
        let Some(run) = &mut self.run else { return };
        if !matches!(run.outcome, Outcome::Running) {
            return;
        }
        let elapsed = run.started.elapsed();
        let (rows, error) = match result {
            Ok(loader) => {
                let loader = Arc::new(loader);
                let rows = loader
                    .affected()
                    .or_else(|| loader.is_complete().then(|| loader.rows_read()));
                if !loader.columns().is_empty() {
                    let l = loader.clone();
                    self.grid.update(cx, |g, cx| {
                        let hooks = g.make_hooks(cx);
                        let cache = Arc::new(GridCache::new(l, config, hooks));
                        g.selection = None;
                        g.set_cache(Some(cache), false, cx);
                    });
                }
                run.outcome = Outcome::Open { loader, elapsed };
                (rows, None)
            }
            Err(e) => {
                run.outcome = Outcome::Failed(e.clone());
                (None, Some(e))
            }
        };
        run.cancel = None;
        let entry = HistoryEntry {
            at: SystemTime::now(),
            connection: self.connection.clone().unwrap_or_default(),
            statement: run.statement.clone(),
            duration: elapsed,
            rows,
            error,
        };
        self.state.update(cx, |s, cx| s.record(entry, cx));
        cx.notify();
    }

    fn fail(&mut self, statement: String, message: String, cx: &mut Context<Self>) {
        self.run = Some(Run {
            statement,
            started: Instant::now(),
            outcome: Outcome::Failed(message),
            cancel: None,
            _task: None,
        });
        cx.notify();
    }

    /// Asks the server to stop a statement still running.
    fn cancel(&mut self, cx: &mut Context<Self>) {
        let Some(run) = &mut self.run else { return };
        if let Some(handle) = run.cancel.take() {
            cx.global::<Net>().spawn(async move {
                if let Err(e) = handle.cancel().await {
                    tracing::warn!("cancel failed: {e}");
                }
            });
        }
        run._task = None;
        if matches!(run.outcome, Outcome::Running) {
            run.outcome = Outcome::Failed("Cancelled.".into());
        }
    }

    /// Cancels a running statement and stops reading the current result.
    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(Run {
            outcome: Outcome::Open { loader, .. },
            ..
        }) = &self.run
        {
            loader.stop_fetching();
        }
        self.cancel(cx);
        cx.notify();
    }

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
                        w.update(cx, |this, cx| {
                            this.connection = Some(id.clone());
                            this.confirm = None;
                            cx.refresh_windows();
                        })
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
                            this.connection = Some(id.clone());
                            this.confirm = None;
                            let id = id.clone();
                            this.state.update(cx, |s, cx| s.connect(&id, cx));
                            cx.refresh_windows();
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
        let running = self
            .run
            .as_ref()
            .is_some_and(|r| matches!(r.outcome, Outcome::Running));
        div()
            .flex_none()
            .h(t.bar_height)
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(8.))
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
                    .tooltip(|_, cx| {
                        Tooltip::view("Run the statement at the cursor (Ctrl+Enter)", cx)
                    })
                    .child(icon(Icon::Play).size(px(11.)))
                    .child("Run")
                    .on_click(cx.listener(|this, _, _, cx| this.run_current(cx))),
            )
            .into_any_element()
    }

    /// Asks before a statement runs, naming the connection in its environment's color.
    fn confirm_bar(&mut self, confirm: &Confirm, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let statement = confirm.statement.clone();
        let env = confirm.environment.as_ref();
        let tint = env.and_then(|e| environment::tag_color(e.tag));
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
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(t.text)
                    .child(format!("{} Run it anyway?", confirm.reason)),
            )
            .child(
                button("confirm-cancel", "Cancel", ButtonStyle::Ghost, t).on_click(cx.listener(
                    |this, _, _, cx| {
                        this.confirm = None;
                        cx.notify();
                    },
                )),
            )
            .child(
                button("confirm-run", "Run", ButtonStyle::Danger, t).on_click(
                    cx.listener(move |this, _, _, cx| this.execute(statement.clone(), cx)),
                ),
            )
            .into_any_element()
    }

    fn result(&mut self, t: &Theme) -> AnyElement {
        let Some(run) = &self.run else {
            return empty_state(
                "No result yet",
                Some("Run a statement to see its rows here.".into()),
                t,
            )
            .bg(t.surface)
            .into_any_element();
        };
        match &run.outcome {
            Outcome::Running => empty_state("Running…", None, t)
                .bg(t.surface)
                .into_any_element(),
            Outcome::Failed(e) => div()
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
                        .child(e.clone()),
                )
                .into_any_element(),
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
            Outcome::Open { .. } => self.grid.clone().into_any_element(),
        }
    }

    fn status_bar(&mut self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut left: Vec<AnyElement> = Vec::new();
        let mut fetch_all = None;
        let env = self
            .connection
            .as_deref()
            .and_then(|id| Environment::of(self.state.read(cx), id));
        if let Some(run) = &self.run {
            match &run.outcome {
                Outcome::Running => {
                    left.push(
                        div()
                            .text_color(t.text_faint)
                            .child("Running…")
                            .into_any_element(),
                    );
                }
                Outcome::Failed(_) => {
                    left.push(div().text_color(t.danger).child("Error").into_any_element());
                }
                Outcome::Open { loader, elapsed } => {
                    let complete = loader.is_complete();
                    let read = loader.rows_read();
                    let count = self
                        .grid
                        .read(cx)
                        .cache()
                        .map(|c| (c.row_count(), c.status()));
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
                    left.push(
                        div()
                            .text_color(t.text_faint)
                            .child(format!("{:.0} ms", elapsed.as_secs_f64() * 1000.0))
                            .into_any_element(),
                    );
                    if let Some(reason) = loader.stopped_reason() {
                        left.push(div().text_color(t.warning).child(reason).into_any_element());
                    }
                    if let Some((_, Status::Error(e))) = count {
                        left.push(div().text_color(t.danger).child(e).into_any_element());
                    } else if let Some((
                        RowCount::AtLeast(_) | RowCount::Unknown,
                        Status::Loading,
                    )) = count
                    {
                        left.push(
                            div()
                                .text_color(t.text_faint)
                                .child("Reading…")
                                .into_any_element(),
                        );
                    }
                    let spill = loader.spill_bytes();
                    if spill > 0 {
                        left.push(
                            div()
                                .text_color(t.text_faint)
                                .child(format!("{:.1} MB on disk", spill as f64 / 1_048_576.0))
                                .into_any_element(),
                        );
                    }
                    if !complete
                        && loader.stopped_reason().is_none()
                        && !loader.columns().is_empty()
                    {
                        fetch_all = Some(loader.clone());
                    }
                }
            }
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
            .children(environment::status(env.as_ref(), t))
            .into_any_element()
    }
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
        let confirm = self.confirm.clone().map(|c| self.confirm_bar(&c, &t, cx));
        let result = self.result(&t);
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
                    .child(div().flex_1().min_h_0().child(result))
                    .child(status),
            )
    }
}
