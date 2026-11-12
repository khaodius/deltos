//! Import and export: the dialogs that set them up, and the jobs that run them in the
//! background with their progress in a corner of the window.
//!
//! One [`Host`] entity, kept as a global, holds the open dialog (at most one, shown over
//! the window that asked for it) and the jobs. Each window draws it once, through
//! [`overlay`]; it draws only what belongs to that window. A job runs on the runtime's
//! blocking pool, since it reads or writes a file, and reports through a shared
//! [`Progress`] that the host polls a few times a second while anything runs, so a job
//! never redraws the window per row.

pub mod export;
pub mod import;
pub mod read;
pub mod sql;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use gpui::{
    AnyElement, AnyView, App, AppContext, Context, Entity, FontWeight, Global, IntoElement, Render,
    SharedString, Window, WindowId, div, prelude::*, px,
};

use crate::net::Net;
use crate::safety::Environment;
use crate::state::AppState;
use crate::theme::{Theme, theme};
use crate::ui::icons::{Icon, icon};
use crate::ui::{ButtonStyle, Tooltip, button, icon_button, thousands};

/// What a running job shares with the window that shows it.
#[derive(Debug, Default)]
pub struct Progress {
    /// Rows (or statements) done.
    pub rows: AtomicU64,
    /// Bytes of the file read so far, and its size, for a percentage.
    pub bytes: AtomicU64,
    pub total_bytes: AtomicU64,
    /// The rows expected, when known or estimated; 0 when not.
    pub total_rows: AtomicU64,
    cancel: AtomicBool,
}

impl Progress {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// How a job ended, in words for the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub message: String,
    pub failed: bool,
    /// Further lines, such as the rows an import skipped.
    pub details: Vec<String>,
    /// The file the job wrote, to show in the file manager.
    pub file: Option<PathBuf>,
}

impl Outcome {
    pub fn done(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            failed: false,
            details: Vec::new(),
            file: None,
        }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            failed: true,
            details: Vec::new(),
            file: None,
        }
    }
}

struct Job {
    id: u64,
    title: SharedString,
    /// What the running count counts: "rows", "statements".
    unit: &'static str,
    progress: Arc<Progress>,
    outcome: Option<Outcome>,
    window: Option<WindowId>,
    /// Called on the UI thread when the job ends, to reload what it changed.
    on_done: Option<Box<dyn FnOnce(&Outcome, &mut App)>>,
}

/// The open dialog and the jobs. See the module documentation.
pub struct Host {
    dialog: Option<(WindowId, AnyView)>,
    jobs: Vec<Job>,
    next_id: u64,
    ticking: bool,
}

struct HostGlobal(Entity<Host>);

impl Global for HostGlobal {}

/// The host, made on first use.
pub fn host(cx: &mut App) -> Entity<Host> {
    if let Some(h) = cx.try_global::<HostGlobal>() {
        return h.0.clone();
    }
    let h = cx.new(|_| Host {
        dialog: None,
        jobs: Vec::new(),
        next_id: 0,
        ticking: false,
    });
    cx.set_global(HostGlobal(h.clone()));
    h
}

/// The dialog and job cards for one window, drawn over everything else in it.
pub fn overlay(cx: &mut App) -> AnyElement {
    AnyView::from(host(cx)).into_any_element()
}

/// Shows `view` as the dialog over `window`, replacing any open one.
pub fn open_dialog(view: AnyView, window: &mut Window, cx: &mut App) {
    let id = window.window_handle().window_id();
    host(cx).update(cx, |h, cx| {
        h.dialog = Some((id, view));
        cx.notify();
    });
}

pub fn close_dialog(cx: &mut App) {
    host(cx).update(cx, |h, cx| {
        h.dialog = None;
        cx.notify();
    });
}

/// What [`start_job`] needs besides the work itself.
pub struct JobSpec {
    pub title: String,
    pub unit: &'static str,
    pub window: Option<WindowId>,
    pub on_done: Option<Box<dyn FnOnce(&Outcome, &mut App)>>,
}

/// Runs `work` on the blocking pool and shows it as a card until the person closes it.
pub fn start_job(
    spec: JobSpec,
    work: impl FnOnce(Arc<Progress>) -> Outcome + Send + 'static,
    cx: &mut App,
) {
    let progress = Arc::new(Progress::default());
    let host = host(cx);
    let id = host.update(cx, |h, cx| {
        let id = h.next_id;
        h.next_id += 1;
        h.jobs.push(Job {
            id,
            title: spec.title.into(),
            unit: spec.unit,
            progress: progress.clone(),
            outcome: None,
            window: spec.window,
            on_done: spec.on_done,
        });
        h.tick(cx);
        cx.notify();
        id
    });
    let task = cx.global::<Net>().run_blocking(move || work(progress));
    let weak = host.downgrade();
    cx.spawn(async move |cx| {
        let outcome = task
            .await
            .unwrap_or_else(|| Outcome::failed("The job stopped unexpectedly."));
        weak.update(cx, |h, cx| h.finish(id, outcome, cx)).ok();
    })
    .detach();
}

impl Host {
    fn finish(&mut self, id: u64, outcome: Outcome, cx: &mut Context<Self>) {
        if let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) {
            if let Some(done) = job.on_done.take() {
                let o = outcome.clone();
                cx.defer(move |cx| done(&o, cx));
            }
            job.outcome = Some(outcome);
        }
        cx.notify();
    }

    /// Redraws a few times a second while a job runs.
    fn tick(&mut self, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let running = this
                    .update(cx, |h, cx| {
                        cx.notify();
                        let running = h.jobs.iter().any(|j| j.outcome.is_none());
                        if !running {
                            h.ticking = false;
                        }
                        running
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        })
        .detach();
    }

    fn job_card(&self, job: &Job, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let id = job.id;
        let rows = job.progress.rows.load(Ordering::Relaxed);
        let total = job.progress.total_bytes.load(Ordering::Relaxed);
        let read = job.progress.bytes.load(Ordering::Relaxed);
        let expected = job.progress.total_rows.load(Ordering::Relaxed);
        let share = if total > 0 {
            Some(read as f64 / total as f64)
        } else {
            (expected > 0).then(|| rows as f64 / expected as f64)
        }
        .map(|s| s.clamp(0.0, 1.0));
        let (line, color): (SharedString, _) = match &job.outcome {
            None => (
                format!(
                    "{}{} {}{}",
                    thousands(rows),
                    if total == 0 && expected > 0 {
                        format!(" of ≈ {}", thousands(expected))
                    } else {
                        String::new()
                    },
                    job.unit,
                    share.map_or(String::new(), |s| format!(" · {:.0}%", s * 100.0))
                )
                .into(),
                t.text_muted,
            ),
            Some(o) => (
                o.message.clone().into(),
                if o.failed { t.danger } else { t.success },
            ),
        };
        let details = job
            .outcome
            .as_ref()
            .map(|o| o.details.clone())
            .unwrap_or_default();
        let running = job.outcome.is_none();
        let path = job.outcome.as_ref().and_then(|o| o.file.clone());
        let progress = job.progress.clone();
        div()
            .id(("transfer-job", id as usize))
            .w(px(340.))
            .p(px(10.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .rounded(t.radius)
            .bg(t.floating)
            .border_1()
            .border_color(t.border_strong)
            .shadow_lg()
            .text_size(t.small_size)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.text)
                            .child(job.title.clone()),
                    )
                    .when(!running, |el| {
                        el.child(
                            icon_button(("transfer-job-close", id as usize), Icon::Close, t)
                                .tooltip(|_, cx| Tooltip::view("Close", cx))
                                .on_click(cx.listener(move |h, _, _, cx| {
                                    h.jobs.retain(|j| j.id != id);
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .when_some(share.filter(|_| running), |el, share| {
                el.child(
                    div().h(px(3.)).w_full().rounded_full().bg(t.border).child(
                        div()
                            .h_full()
                            .w(gpui::relative(share as f32))
                            .rounded_full()
                            .bg(t.accent),
                    ),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(color)
                            .whitespace_normal()
                            .child(line),
                    )
                    .when(running, |el| {
                        el.child(
                            button(
                                ("transfer-job-cancel", id as usize),
                                if progress.cancelled() {
                                    "Stopping…"
                                } else {
                                    "Cancel"
                                },
                                ButtonStyle::Secondary,
                                t,
                            )
                            .h(t.s(22.))
                            .on_click(move |_, _, _| progress.cancel()),
                        )
                    })
                    .when_some(path, |el, path| {
                        el.child(
                            button(
                                ("transfer-job-show", id as usize),
                                "Show",
                                ButtonStyle::Ghost,
                                t,
                            )
                            .h(t.s(22.))
                            .tooltip(|_, cx| Tooltip::view("Show the file in its folder", cx))
                            .on_click(move |_, _, cx| cx.reveal_path(&path)),
                        )
                    }),
            )
            .children((!details.is_empty()).then(|| {
                div()
                    .max_h(px(120.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .font_family(t.mono_font.clone())
                    .text_color(t.text_muted)
                    .children(details.into_iter().map(|d| {
                        div()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(d)
                    }))
            }))
            .into_any_element()
    }
}

impl Render for Host {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let here = window.window_handle().window_id();
        let dialog = self
            .dialog
            .as_ref()
            .filter(|(w, _)| *w == here)
            .map(|(_, v)| v.clone());
        let jobs: Vec<AnyElement> = self
            .jobs
            .iter()
            .filter(|j| j.window.is_none_or(|w| w == here))
            .map(|j| self.job_card(j, &t, cx))
            .collect();
        if dialog.is_none() && jobs.is_empty() {
            return div().into_any_element();
        }
        div()
            .absolute()
            .inset_0()
            .children(dialog.map(|view| {
                div()
                    .id("transfer-backdrop")
                    .occlude()
                    .absolute()
                    .inset_0()
                    .bg(t.base.opacity(0.55))
                    .flex()
                    .items_center()
                    .justify_center()
                    .p(px(24.))
                    .child(view)
            }))
            .when(!jobs.is_empty(), |el| {
                el.child(
                    div()
                        .absolute()
                        .bottom(px(36.))
                        .right(px(16.))
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .children(jobs),
                )
            })
            .into_any_element()
    }
}

/// The frame every transfer dialog uses: a title, the body, and buttons at the bottom
/// right.
pub fn dialog_frame(
    title: impl Into<SharedString>,
    subtitle: Option<SharedString>,
    body: impl IntoElement,
    buttons: Vec<AnyElement>,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id("transfer-dialog")
        .w(px(600.))
        .max_w_full()
        .max_h_full()
        .flex()
        .flex_col()
        .rounded(t.radius)
        .bg(t.surface)
        .border_1()
        .border_color(t.border_strong)
        .shadow_lg()
        .font_family(t.ui_font.clone())
        .text_size(t.ui_size)
        .text_color(t.text)
        .child(
            div()
                .flex_none()
                .px(px(16.))
                .pt(px(14.))
                .pb(px(10.))
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(t.s(14.))
                        .child(title.into()),
                )
                .children(subtitle.map(|s| {
                    div()
                        .text_size(t.small_size)
                        .text_color(t.text_faint)
                        .child(s)
                })),
        )
        .child(
            div()
                .id("transfer-dialog-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px(px(16.))
                .pb(px(12.))
                .flex()
                .flex_col()
                .gap(px(10.))
                .child(body),
        )
        .child(
            div()
                .flex_none()
                .px(px(16.))
                .py(px(10.))
                .flex()
                .items_center()
                .justify_end()
                .gap(px(8.))
                .border_t_1()
                .border_color(t.border)
                .children(buttons),
        )
}

/// A labelled line of a dialog: the label on the left, the control filling the rest.
pub fn option_row(label: &'static str, control: impl IntoElement, t: &Theme) -> gpui::Div {
    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(12.))
        .child(
            div()
                .w(px(120.))
                .flex_none()
                .text_size(t.small_size)
                .text_color(t.text_muted)
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .child(control),
        )
}

/// A line of small text under the options: a hint, or an error in the danger color.
pub fn note(text: impl Into<SharedString>, error: bool, t: &Theme) -> gpui::Div {
    div()
        .text_size(t.small_size)
        .text_color(if error { t.danger } else { t.text_faint })
        .whitespace_normal()
        .child(text.into())
}

/// The delimiters the CSV options offer.
pub const DELIMITERS: [(&str, u8); 4] = [
    ("Comma", b','),
    ("Semicolon", b';'),
    ("Tab", b'\t'),
    ("Pipe", b'|'),
];

/// What a connection's environment asks of an import or a SQL file: a read-only
/// connection takes neither, and on staging and production they wait for a second press
/// that names the environment, as the query tab's confirm bar does.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Safety {
    pub environment: Option<Environment>,
    /// "Confirm every write on production" is on: an import there is confirmed too.
    pub confirm_writes: bool,
}

/// What an import dialog is about to do, for [`Safety::confirmation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write<'a> {
    /// Run every statement of a SQL file.
    Script { file: &'a str },
    /// Insert the rows of a file into a table, emptying it first when `truncate`.
    Import {
        file: &'a str,
        table: &'a str,
        truncate: bool,
    },
}

/// The reason import and SQL-file entries are off on a read-only connection.
pub const READ_ONLY: &str = "Read-only connection";

impl Safety {
    /// The connection `id` as it stands now.
    pub fn of(state: &AppState, id: &str) -> Self {
        Self {
            environment: Environment::of(state, id),
            confirm_writes: state.settings.data.confirm_production_writes,
        }
    }

    pub fn read_only(&self) -> bool {
        self.environment.as_ref().is_some_and(|e| e.read_only)
    }

    /// The environment, when it is staging or production.
    pub fn sensitive(&self) -> Option<&Environment> {
        self.environment.as_ref().filter(|e| e.is_sensitive())
    }

    /// Why nothing can be imported or run here, or `None`.
    pub fn refusal(&self) -> Option<String> {
        let env = self.environment.as_ref().filter(|e| e.read_only)?;
        Some(format!(
            "{READ_ONLY}: nothing is imported into or run on {}. Turn off Read-only in the connection's settings to change its data.",
            env.name
        ))
    }

    /// What to say before `write` goes ahead, or `None` to start it at once. The person
    /// confirms by pressing the button again.
    pub fn confirmation(&self, write: Write<'_>) -> Option<String> {
        let env = self
            .sensitive()
            .map(|e| format!("{} · {}", e.tag.label(), e.name));
        match (write, env) {
            (Write::Script { file }, Some(env)) => Some(format!(
                "Every statement of {file} runs on {env}, without asking for each one. Press Run again to go ahead."
            )),
            (Write::Script { .. }, None) => None,
            (
                Write::Import {
                    table,
                    truncate: true,
                    ..
                },
                Some(env),
            ) => Some(format!(
                "Every row of {table} on {env} will be deleted before the import. Press Empty and import again to go ahead."
            )),
            (
                Write::Import {
                    table,
                    truncate: true,
                    ..
                },
                None,
            ) => Some(format!(
                "Every row of {table} will be deleted before the import. Press Import again to go ahead."
            )),
            (Write::Import { file, table, .. }, Some(env)) if self.confirm_writes => Some(format!(
                "The rows of {file} go into {table} on {env}. Press Import again to go ahead."
            )),
            (Write::Import { .. }, _) => None,
        }
    }
}

/// A path's file name for messages, or the whole path when it has none.
pub fn file_label(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// `1 row`, `2 rows`.
pub fn count(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", thousands(n), if n == 1 { one } else { many })
}

/// A small helper so dialogs can show an icon next to a warning line.
pub fn warning(text: impl Into<SharedString>, t: &Theme) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .text_size(t.small_size)
        .text_color(t.danger)
        .child(icon(Icon::Lock).size(px(12.)))
        .child(div().flex_1().whitespace_normal().child(text.into()))
}

/// The import and export items of a table's menus: the table tab's, with its current
/// `view` and `columns`, and the sidebar's, without. `on_import` runs after an import
/// or script ends, to reload what shows the table.
pub fn table_items(
    connection: Arc<dyn deltos_data::Connection>,
    object: deltos_data::ObjectRef,
    view: Option<deltos_grid::BrowseQuery>,
    columns: Option<Arc<[deltos_data::Column]>>,
    page_rows: u32,
    safety: Safety,
    on_import: Option<std::rc::Rc<dyn Fn(&mut App)>>,
) -> Vec<crate::ui::MenuItem> {
    use crate::ui::MenuItem;
    let kind = connection.kind();
    let browsable = object.kind.has_rows();
    let read_only = safety.read_only();
    let editable = connection.capabilities().edit_rows
        && matches!(
            object.kind,
            deltos_data::ObjectKind::Table | deltos_data::ObjectKind::Collection
        );
    let after = on_import.clone();
    let done = move || {
        after.clone().map(|f| {
            Box::new(move |_: &Outcome, cx: &mut App| f(cx)) as Box<dyn FnOnce(&Outcome, &mut App)>
        })
    };
    let mut items = Vec::new();
    let (c1, o1) = (connection.clone(), object.clone());
    items.push(
        MenuItem::new("Export…", move |window, cx| {
            let source = export::Source::Table {
                object: o1.clone(),
                view: view.clone(),
                columns: columns.clone(),
            };
            export::open(c1.clone(), source, page_rows, window, cx);
        })
        .enabled(browsable),
    );
    let (c2, o2, d2, s2) = (
        connection.clone(),
        object.clone(),
        done.clone(),
        safety.clone(),
    );
    let mut import = MenuItem::new("Import from CSV or JSON…", move |window, cx| {
        import::open(
            c2.clone(),
            import::Target::Table(o2.clone()),
            s2.clone(),
            d2(),
            window,
            cx,
        );
    })
    .enabled(editable && !read_only);
    if read_only {
        import = import.detail(READ_ONLY);
    }
    items.push(import);
    if crate::ui::edit::speaks_sql(kind) {
        items.push(script_item(
            connection,
            object.namespace.clone(),
            safety,
            on_import,
        ));
    }
    items
}

/// "Run a SQL file…" in `namespace`, off on a read-only connection. `on_done` runs after
/// the script, to reload what it changed.
pub fn script_item(
    connection: Arc<dyn deltos_data::Connection>,
    namespace: Vec<String>,
    safety: Safety,
    on_done: Option<std::rc::Rc<dyn Fn(&mut App)>>,
) -> crate::ui::MenuItem {
    let read_only = safety.read_only();
    let item = crate::ui::MenuItem::new("Run a SQL file…", move |window, cx| {
        let done = on_done.clone().map(|f| {
            Box::new(move |_: &Outcome, cx: &mut App| f(cx)) as Box<dyn FnOnce(&Outcome, &mut App)>
        });
        import::open(
            connection.clone(),
            import::Target::Namespace(namespace.clone()),
            safety.clone(),
            done,
            window,
            cx,
        );
    })
    .enabled(!read_only);
    if read_only {
        item.detail(READ_ONLY)
    } else {
        item
    }
}
