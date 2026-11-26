//! The export dialog for tables, collections and whole databases, laid out as Adminer's
//! export: Output, Format, then what the dump does per database, table and its data, and
//! below them every table with a box for its structure and one for its rows, each column
//! with a box that sets them all, and the server's row estimate. See [`super::dump`].

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use deltos_data::{Column, Connection, DriverKind, ObjectRef};
use deltos_grid::BrowseQuery;
use gpui::{
    AnyElement, App, AppContext, Context, ElementId, Entity, FocusHandle, Focusable, FontWeight,
    IntoElement, Render, SharedString, Subscription, WeakEntity, Window, WindowId, div, prelude::*,
    px, uniform_list,
};

use super::dump::{self, DataMode, Engine, Format, Job, Options, Output, Pick, TablesMode};
use super::export::{default_dir, safe_file_name};
use super::sql::default_rows_per_statement;
use super::{
    JobSpec, Outcome, close_dialog, dialog_frame, file_label, note, open_dialog, option_row,
    start_job,
};
use crate::net::Net;
use crate::theme::{Theme, theme};
use crate::ui::icons::{Icon, icon};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{ButtonStyle, button, field, segmented, thousands};

/// A dump this size or smaller opens in a query tab; a larger one in the system's app.
const OPEN_IN_TAB_BYTES: u64 = 4 * 1024 * 1024;

/// What the dialog exports.
#[derive(Clone)]
pub enum Target {
    /// A table, view or collection, picked among the others of its namespace. `view`
    /// is the sort and filter its tab shows, and `columns` the tab's columns.
    Table {
        object: ObjectRef,
        view: Option<BrowseQuery>,
        columns: Option<Arc<[Column]>>,
    },
    /// Every table under these namespace paths (all of them when empty), called `label`.
    Database {
        under: Vec<Vec<String>>,
        label: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Everything,
    View,
}

pub struct DumpDialog {
    connection: Arc<dyn Connection>,
    connection_name: Option<String>,
    engine: Engine,
    target: Target,
    /// What the file is named after.
    name: String,
    page_rows: u32,
    options: Options,
    scope: Scope,
    picks: Vec<Pick>,
    loading: bool,
    load_error: Option<SharedString>,
    path: Entity<TextInput>,
    /// The target exists and the next press replaces it.
    confirm_replace: bool,
    error: Option<SharedString>,
    window: Option<WindowId>,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

/// Opens the dialog over `window`.
pub fn open(
    connection: Arc<dyn Connection>,
    connection_name: Option<String>,
    target: Target,
    page_rows: u32,
    window: &mut Window,
    cx: &mut App,
) {
    let id = window.window_handle().window_id();
    let view = cx.new(|cx| {
        let mut d = DumpDialog::new(connection, target, page_rows, cx);
        d.window = Some(id);
        d.connection_name = connection_name;
        d
    });
    let focus = view.read(cx).focus.clone();
    open_dialog(view.into(), window, cx);
    window.focus(&focus);
}

fn narrows(q: &BrowseQuery) -> bool {
    !q.filters.is_empty() || !q.sort.is_empty() || q.raw_where.is_some()
}

impl DumpDialog {
    fn new(
        connection: Arc<dyn Connection>,
        target: Target,
        page_rows: u32,
        cx: &mut Context<Self>,
    ) -> Self {
        let kind = connection.kind();
        let engine = Engine(kind);
        let mut options = Options::last();
        if !engine.script() && options.format == Format::Script {
            options.format = Format::Json;
        }
        let (name, under, scope) = match &target {
            Target::Table { object, view, .. } => {
                // A table's export leaves the database's routines and events out.
                options.routines = false;
                options.events = false;
                let scope = if view.as_ref().is_some_and(narrows) {
                    Scope::View
                } else {
                    Scope::Everything
                };
                (object.name.clone(), vec![object.namespace.clone()], scope)
            }
            Target::Database { under, label } => (label.clone(), under.clone(), Scope::Everything),
        };
        let path = cx.new(|cx| TextInput::new("File".to_owned(), cx));
        let subs = vec![cx.subscribe(&path, |this, _, e, cx| match e {
            InputEvent::Cancel => close_dialog(cx),
            InputEvent::Submit => this.start(cx),
            InputEvent::Changed => {
                this.confirm_replace = false;
                this.error = None;
                cx.notify();
            }
        })];
        let conn = connection.clone();
        let job = cx
            .global::<Net>()
            .run(async move { super::database::entries(conn.as_ref(), &under, true).await });
        cx.spawn(async move |this, cx| {
            let listed = job.await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match listed {
                    Some(Ok(entries)) => this.listed(entries),
                    Some(Err(e)) => {
                        this.load_error = Some(format!("Could not list the tables: {e}").into())
                    }
                    None => this.load_error = Some("Could not list the tables.".into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        let mut this = Self {
            connection,
            connection_name: None,
            engine,
            target,
            name,
            page_rows: page_rows.max(1),
            options,
            scope,
            picks: Vec::new(),
            loading: true,
            load_error: None,
            path,
            confirm_replace: false,
            error: None,
            window: None,
            focus: cx.focus_handle(),
            _subs: subs,
        };
        this.sync_path(cx);
        this
    }

    /// Fills the list. A table's export starts with only that table chosen.
    fn listed(&mut self, entries: Vec<deltos_data::ObjectEntry>) {
        let mut picks: Vec<Pick> = entries
            .into_iter()
            .map(|e| Pick::new(e.object, e.estimated_rows))
            .collect();
        if let Target::Table {
            object, columns, ..
        } = &self.target
        {
            if !picks.iter().any(|p| p.object == *object) {
                picks.push(Pick::new(object.clone(), None));
            }
            for p in &mut picks {
                let chosen = p.object == *object;
                p.structure = chosen;
                p.data = chosen;
                if chosen {
                    p.columns = columns.clone();
                }
            }
        }
        self.picks = picks;
    }

    /// Whether the list spans several namespaces, so rows say which.
    fn several_namespaces(&self) -> bool {
        self.picks.first().is_some_and(|f| {
            self.picks
                .iter()
                .any(|p| p.object.namespace != f.object.namespace)
        })
    }

    fn job_picks(&self) -> Vec<Pick> {
        let mut picks = self.picks.clone();
        if let Target::Table { object, view, .. } = &self.target
            && self.scope == Scope::View
        {
            for p in &mut picks {
                if p.object == *object {
                    p.query = view.clone();
                }
            }
        }
        picks
    }

    fn draft_job(&self, path: PathBuf) -> Job {
        Job {
            kind: self.engine.0,
            options: self.options,
            picks: self.job_picks(),
            page_rows: self.page_rows,
            batch: default_rows_per_statement(self.engine.0),
            path,
        }
    }

    /// The name the output goes by, with the format's extension.
    fn file_name(&self) -> String {
        let job = self.draft_job(PathBuf::new());
        let base = match &self.target {
            // One table out of a database export is named after that table.
            Target::Database { .. }
                if !job.to_folder() && self.options.format != Format::Script =>
            {
                job.picks
                    .iter()
                    .find(|p| p.data)
                    .map_or(self.name.clone(), |p| p.object.name.clone())
            }
            _ => self.name.clone(),
        };
        let base = safe_file_name(&base);
        if job.to_folder() {
            return base;
        }
        let mut name = format!("{base}.{}", self.options.format.extension(self.engine.0));
        if self.options.output == Output::Gzip {
            name.push_str(".gz");
        }
        name
    }

    /// Keeps the file field's name in step with the format, output and selection,
    /// leaving the folder the person typed or chose.
    fn sync_path(&mut self, cx: &mut Context<Self>) {
        let current = PathBuf::from(self.path.read(cx).text().trim());
        let dir = current
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(default_dir);
        let new = dir.join(self.file_name());
        if new != current {
            self.path.update(cx, |i, cx| {
                i.set_text(new.to_string_lossy().into_owned(), cx)
            });
        }
        self.confirm_replace = false;
        self.error = None;
        cx.notify();
    }

    fn set(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut Options)) {
        f(&mut self.options);
        self.sync_path(cx);
    }

    fn choose(&mut self, cx: &mut Context<Self>) {
        let current = PathBuf::from(self.path.read(cx).text().trim());
        let dir = current
            .parent()
            .filter(|p| p.is_dir())
            .map(Path::to_path_buf)
            .unwrap_or_else(default_dir);
        if self.draft_job(current.clone()).to_folder() {
            let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: Some("Export here".into()),
            });
            let name = self.file_name();
            cx.spawn(async move |this, cx| {
                let Ok(Ok(Some(paths))) = paths.await else {
                    return;
                };
                let Some(dir) = paths.into_iter().next() else {
                    return;
                };
                this.update(cx, |this, cx| {
                    let path = dir.join(name);
                    this.path.update(cx, |i, cx| {
                        i.set_text(path.to_string_lossy().into_owned(), cx)
                    });
                    this.confirm_replace = false;
                    cx.notify();
                })
                .ok();
            })
            .detach();
            return;
        }
        let name = current
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        let answer = cx.prompt_for_new_path(&dir, name.as_deref());
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(path))) = answer.await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.path.update(cx, |i, cx| {
                    i.set_text(path.to_string_lossy().into_owned(), cx)
                });
                // The system dialog already asked about replacing.
                this.confirm_replace = path.exists();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn fail(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.error = Some(message.into());
        cx.notify();
    }

    /// Why the export cannot start as set, or `None`.
    fn refusal(&self) -> Option<&'static str> {
        if self.loading {
            return Some("The tables are still being listed.");
        }
        let script = self.options.format == Format::Script;
        let any = self.picks.iter().any(|p| {
            (script && p.structure && self.options.tables != TablesMode::None)
                || (p.data && (!script || self.options.data != DataMode::None))
        });
        if any {
            return None;
        }
        Some(if script {
            "Choose a table, and Tables or Data to write for it."
        } else {
            "Choose a table with Data to export."
        })
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        if let Some(reason) = self.refusal() {
            self.fail(reason, cx);
            return;
        }
        let open = self.options.output == Output::Open;
        let path = if open {
            // A file of its own in the temporary folder, opened when written.
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            std::env::temp_dir()
                .join(format!("deltos-export-{}-{n}", std::process::id()))
                .join(self.file_name())
        } else {
            let text = self.path.read(cx).text().trim().to_owned();
            if text.is_empty() {
                self.fail("Choose a file to export to.", cx);
                return;
            }
            PathBuf::from(text)
        };
        let job = self.draft_job(path.clone());
        if open {
            if let Some(parent) = path.parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                self.fail(format!("Could not make a temporary folder: {e}"), cx);
                return;
            }
        } else {
            let parent_missing = path
                .parent()
                .is_some_and(|p| !p.as_os_str().is_empty() && !p.is_dir());
            if parent_missing {
                self.fail("The folder it goes in does not exist.", cx);
                return;
            }
            if job.to_folder() {
                if path.is_file() {
                    self.fail("That is a file; the tables go into a folder.", cx);
                    return;
                }
                let filled = std::fs::read_dir(&path).is_ok_and(|mut d| d.next().is_some());
                if filled && !self.confirm_replace {
                    self.confirm_replace = true;
                    self.fail(
                        format!(
                            "{} has files in it. Export again to write into it; files named \
                             after the tables are replaced.",
                            file_label(&path)
                        ),
                        cx,
                    );
                    return;
                }
            } else {
                if path.is_dir() {
                    self.fail("That is a folder; add a file name.", cx);
                    return;
                }
                if path.exists() && !self.confirm_replace {
                    self.confirm_replace = true;
                    self.fail(
                        format!("{} exists. Export again to replace it.", file_label(&path)),
                        cx,
                    );
                    return;
                }
            }
        }
        self.options.remember();
        let script = self.options.format == Format::Script;
        let folder = job.to_folder();
        let conn = self.connection.clone();
        let on_done: Option<Box<dyn FnOnce(&Outcome, &mut App)>> = open.then(|| {
            let conn = conn.clone();
            Box::new(move |outcome: &Outcome, cx: &mut App| {
                let Some(path) = outcome.file.clone().filter(|_| !outcome.failed) else {
                    return;
                };
                let small = std::fs::metadata(&path).is_ok_and(|m| m.len() <= OPEN_IN_TAB_BYTES);
                if script
                    && !folder
                    && small
                    && let Ok(text) = std::fs::read_to_string(&path)
                    && super::open_in_query_tab(&conn, text, cx)
                {
                    return;
                }
                cx.open_with_system(&path);
            }) as Box<dyn FnOnce(&Outcome, &mut App)>
        });
        let handle = cx.global::<Net>().handle();
        let spec = JobSpec {
            title: format!("Exporting {} to {}", self.name, file_label(&path)),
            connection: self.connection_name.clone(),
            unit: "rows",
            window: self.window,
            on_done,
        };
        start_job(
            spec,
            move |progress| dump::run(&handle, &conn, &job, &progress),
            cx,
        );
        close_dialog(cx);
    }

    // ---- drawing -------------------------------------------------------------------------

    fn option_rows(&self, t: &Theme, cx: &mut Context<Self>, window: &Window) -> Vec<AnyElement> {
        let weak = cx.entity().downgrade();
        let o = self.options;
        let engine = self.engine;
        let mut rows = Vec::new();
        let w = weak.clone();
        rows.push(
            option_row(
                "Output",
                segmented(
                    "dump-output",
                    &[
                        ("Open", Output::Open),
                        ("Save", Output::Save),
                        ("Gzip", Output::Gzip),
                    ],
                    o.output,
                    move |v, _, cx| update(&w, cx, |o| o.output = v),
                    t,
                ),
                t,
            )
            .into_any_element(),
        );
        let mut formats: Vec<(&'static str, Format)> = Vec::new();
        if engine.script() {
            formats.push((engine.script_label(), Format::Script));
        }
        formats.extend([
            ("CSV ,", Format::Csv),
            ("CSV ;", Format::CsvSemicolon),
            ("TSV", Format::Tsv),
            ("JSON", Format::Json),
        ]);
        let w = weak.clone();
        rows.push(
            option_row(
                "Format",
                segmented(
                    "dump-format",
                    &formats,
                    o.format,
                    move |v, _, cx| update(&w, cx, |o| o.format = v),
                    t,
                ),
                t,
            )
            .into_any_element(),
        );
        if o.format == Format::Script {
            if let Some(label) = engine.database_label() {
                let w = weak.clone();
                let mut extras = Vec::new();
                if engine.routines() {
                    let w = weak.clone();
                    extras.push(
                        checkbox("dump-routines", o.routines, Some("Routines"), t)
                            .on_click(move |_, _, cx| update(&w, cx, |o| o.routines = !o.routines))
                            .into_any_element(),
                    );
                }
                if engine.events() {
                    let w = weak.clone();
                    extras.push(
                        checkbox("dump-events", o.events, Some("Events"), t)
                            .on_click(move |_, _, cx| update(&w, cx, |o| o.events = !o.events))
                            .into_any_element(),
                    );
                }
                rows.push(
                    option_row(
                        label,
                        with_extras(
                            segmented(
                                "dump-database",
                                engine.database_modes(),
                                o.database,
                                move |v, _, cx| update(&w, cx, |o| o.database = v),
                                t,
                            ),
                            extras,
                        ),
                        t,
                    )
                    .into_any_element(),
                );
            }
            let mut extras = Vec::new();
            if engine.auto_increment() {
                let w = weak.clone();
                extras.push(
                    checkbox(
                        "dump-auto-increment",
                        o.auto_increment,
                        Some("Auto Increment"),
                        t,
                    )
                    .on_click(move |_, _, cx| {
                        update(&w, cx, |o| o.auto_increment = !o.auto_increment)
                    })
                    .into_any_element(),
                );
            }
            if engine.triggers() {
                let w = weak.clone();
                extras.push(
                    checkbox("dump-triggers", o.triggers, Some("Triggers"), t)
                        .on_click(move |_, _, cx| update(&w, cx, |o| o.triggers = !o.triggers))
                        .into_any_element(),
                );
            }
            let w = weak.clone();
            rows.push(
                option_row(
                    if engine.0 == DriverKind::Mongodb {
                        "Collections"
                    } else {
                        "Tables"
                    },
                    with_extras(
                        segmented(
                            "dump-tables",
                            &[
                                ("None", TablesMode::None),
                                ("DROP+CREATE", TablesMode::DropCreate),
                                ("CREATE", TablesMode::Create),
                            ],
                            o.tables,
                            move |v, _, cx| update(&w, cx, |o| o.tables = v),
                            t,
                        ),
                        extras,
                    ),
                    t,
                )
                .into_any_element(),
            );
            let w = weak.clone();
            rows.push(
                option_row(
                    "Data",
                    segmented(
                        "dump-data",
                        &[
                            ("None", DataMode::None),
                            ("TRUNCATE+INSERT", DataMode::TruncateInsert),
                            ("INSERT", DataMode::Insert),
                            ("INSERT+UPDATE", DataMode::InsertUpdate),
                        ],
                        o.data,
                        move |v, _, cx| update(&w, cx, |o| o.data = v),
                        t,
                    ),
                    t,
                )
                .into_any_element(),
            );
        }
        if let Target::Table { view: Some(q), .. } = &self.target
            && narrows(q)
        {
            let w = weak.clone();
            let label: &'static str = match q.filters.len() {
                0 => "As sorted",
                1 => "1 filter",
                _ => "Filtered",
            };
            rows.push(
                option_row(
                    "Rows",
                    segmented(
                        "dump-scope",
                        &[("Whole table", Scope::Everything), (label, Scope::View)],
                        self.scope,
                        move |s, _, cx| {
                            w.update(cx, |this, cx| {
                                this.scope = s;
                                cx.notify();
                            })
                            .ok();
                        },
                        t,
                    ),
                    t,
                )
                .into_any_element(),
            );
        }
        if o.output != Output::Open {
            let folder = self.draft_job(PathBuf::new()).to_folder();
            rows.push(
                option_row(
                    if folder { "Folder" } else { "File" },
                    div()
                        .flex_1()
                        .flex()
                        .gap(px(6.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(field(&self.path, t, window, cx)),
                        )
                        .child(
                            button("dump-choose", "Choose…", ButtonStyle::Secondary, t)
                                .on_click(cx.listener(|this, _, _, cx| this.choose(cx))),
                        ),
                    t,
                )
                .into_any_element(),
            );
        }
        rows
    }

    /// The table list: a header with a box for every table's structure and one for every
    /// table's data, then a row per table.
    fn table_list(&self, t: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let script = self.options.format == Format::Script;
        let structure_col = script && self.options.tables != TablesMode::None;
        let data_col = !script || self.options.data != DataMode::None;
        let frame = div()
            .w_full()
            .flex()
            .flex_col()
            .rounded(t.radius)
            .border_1()
            .border_color(t.border)
            .bg(t.base);
        if self.loading || self.load_error.is_some() || self.picks.is_empty() {
            let text: SharedString = if let Some(e) = &self.load_error {
                e.clone()
            } else if self.loading {
                "Listing the tables…".into()
            } else {
                "There are no tables here.".into()
            };
            return frame
                .p(px(10.))
                .child(note(text, self.load_error.is_some(), t))
                .into_any_element();
        }
        let all_structure = self.picks.iter().all(|p| p.structure);
        let all_data = self.picks.iter().all(|p| p.data);
        let header = div()
            .h(t.list_row)
            .w_full()
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(8.))
            .border_b_1()
            .border_color(t.border)
            .text_size(t.small_size)
            .font_weight(FontWeight::MEDIUM)
            .text_color(t.text_muted)
            .when(structure_col, |el| {
                el.child(
                    checkbox("dump-all-structure", all_structure, None, t).on_click(cx.listener(
                        move |this, _, _, cx| {
                            for p in &mut this.picks {
                                p.structure = !all_structure;
                            }
                            this.sync_path(cx);
                        },
                    )),
                )
            })
            .child(
                div()
                    .flex_1()
                    .child(if self.engine.0 == DriverKind::Mongodb {
                        "Collections"
                    } else {
                        "Tables"
                    }),
            )
            .child("Rows")
            .when(data_col, |el| {
                el.child(div().w(px(36.)).flex().justify_end().child("Data"))
                    .child(
                        checkbox("dump-all-data", all_data, None, t).on_click(cx.listener(
                            move |this, _, _, cx| {
                                for p in &mut this.picks {
                                    p.data = !all_data;
                                }
                                this.sync_path(cx);
                            },
                        )),
                    )
            });
        let several = self.several_namespaces();
        let rows: Rc<Vec<(SharedString, SharedString, bool, bool, bool)>> = Rc::new(
            self.picks
                .iter()
                .map(|p| {
                    let name = if several {
                        let mut parts = p.object.namespace.clone();
                        parts.push(p.object.name.clone());
                        parts.join(".")
                    } else {
                        p.object.name.clone()
                    };
                    let estimate = match p.estimated_rows {
                        Some(0) => "0".to_owned(),
                        Some(n) => format!("~ {}", thousands(n)),
                        None => String::new(),
                    };
                    (
                        name.into(),
                        estimate.into(),
                        p.structure,
                        p.data,
                        p.is_view(),
                    )
                })
                .collect(),
        );
        let weak = cx.entity().downgrade();
        let t2 = t.clone();
        let count = rows.len();
        let list =
            uniform_list("dump-tables", count, move |range, _, _| {
                let t = &t2;
                range
                    .map(|i| {
                        let (name, estimate, structure, data, view) = rows[i].clone();
                        let (w1, w2) = (weak.clone(), weak.clone());
                        div()
                            .id(("dump-table", i))
                            .h(t.list_row)
                            .w_full()
                            .px(px(8.))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .border_b_1()
                            .border_color(t.grid_line)
                            .text_size(t.ui_size)
                            .when(structure_col, |el| {
                                el.child(
                                    checkbox(("dump-structure", i), structure, None, t).on_click(
                                        move |_, _, cx| {
                                            toggle(&w1, cx, i, |p| p.structure = !p.structure)
                                        },
                                    ),
                                )
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .when(view, |el| el.italic().text_color(t.text_muted))
                                    .child(name),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(t.small_size)
                                    .text_color(t.text_faint)
                                    .child(estimate),
                            )
                            .when(data_col, |el| {
                                el.child(checkbox(("dump-data", i), data, None, t).on_click(
                                    move |_, _, cx| toggle(&w2, cx, i, |p| p.data = !p.data),
                                ))
                            })
                            .into_any_element()
                    })
                    .collect()
            });
        let height = (count.min(12) as f32) * f32::from(t.list_row);
        frame
            .child(header)
            .child(div().h(px(height)).w_full().child(list.size_full()))
            .into_any_element()
    }
}

fn update(weak: &WeakEntity<DumpDialog>, cx: &mut App, f: impl FnOnce(&mut Options)) {
    weak.update(cx, |this, cx| this.set(cx, f)).ok();
}

fn toggle(weak: &WeakEntity<DumpDialog>, cx: &mut App, i: usize, f: impl FnOnce(&mut Pick)) {
    weak.update(cx, |this, cx| {
        if let Some(p) = this.picks.get_mut(i) {
            f(p);
        }
        this.sync_path(cx);
    })
    .ok();
}

/// A segmented control with checkboxes after it, as Adminer puts "Routines" and
/// "Events" beside the database's select.
fn with_extras(control: impl IntoElement, extras: Vec<AnyElement>) -> gpui::Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(12.))
        .child(control)
        .children(extras)
}

/// A checkbox, with its label when it has one.
fn checkbox(
    id: impl Into<ElementId>,
    checked: bool,
    label: Option<&'static str>,
    t: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .flex_none()
        .flex()
        .items_center()
        .gap(px(6.))
        .cursor_pointer()
        .child(
            div()
                .flex_none()
                .size(px(14.))
                .rounded(px(3.))
                .flex()
                .items_center()
                .justify_center()
                .border_1()
                .when(checked, |el| {
                    el.bg(t.accent)
                        .border_color(t.accent)
                        .child(icon(Icon::Check).size(px(10.)).text_color(t.accent_text))
                })
                .when(!checked, |el| el.bg(t.input).border_color(t.border_strong)),
        )
        .children(label.map(|l| {
            div()
                .text_size(t.small_size)
                .text_color(t.text_muted)
                .child(l)
        }))
}

impl Focusable for DumpDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DumpDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let mut body = div().flex().flex_col().gap(px(10.));
        body = body.children(self.option_rows(&t, cx, window));
        if let Some(e) = &self.error {
            body = body.child(note(e.clone(), true, &t));
        }
        body = body.child(self.table_list(&t, cx));
        let buttons = vec![
            button("dump-cancel", "Cancel", ButtonStyle::Ghost, &t)
                .on_click(|_, _, cx| close_dialog(cx))
                .into_any_element(),
            button(
                "dump-start",
                if self.confirm_replace {
                    "Replace"
                } else {
                    "Export"
                },
                if self.confirm_replace {
                    ButtonStyle::Danger
                } else {
                    ButtonStyle::Primary
                },
                &t,
            )
            .on_click(cx.listener(|this, _, _, cx| this.start(cx)))
            .into_any_element(),
        ];
        let subtitle: SharedString = match &self.target {
            Target::Table { object, .. } => {
                let mut path = object.namespace.clone();
                path.push(object.name.clone());
                path.join(".").into()
            }
            Target::Database { under, .. } => {
                let names: Vec<String> = under.iter().map(|u| u.join(".")).collect();
                if names.is_empty() || names.iter().all(String::is_empty) {
                    "Every database".into()
                } else {
                    names.join(", ").into()
                }
            }
        };
        let title = format!("Export {}", self.name);
        div()
            .track_focus(&self.focus)
            .child(dialog_frame(title, Some(subtitle), body, buttons, &t).w(px(720.)))
    }
}
