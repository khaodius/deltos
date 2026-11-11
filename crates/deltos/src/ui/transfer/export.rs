//! Exporting a table, its filtered view or a query result to a file: CSV, JSON, SQL
//! `INSERT` statements or Markdown. Rows stream from the database through
//! [`deltos_grid::export`] one page at a time into a temporary file next to the target,
//! which is renamed over the target when every row is written, so a cancelled or failed
//! export never leaves half a file under the name the person chose.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use deltos_data::{Column, Connection, DriverKind, ObjectKind, ObjectRef};
use deltos_grid::{
    BrowseConfig, BrowseLoader, BrowseQuery, CsvFormat, CsvWriter, ExportOptions, JsonStyle,
    JsonWriter, MarkdownWriter, PageLoader, QuoteStyle, RowWriter, StreamLoader,
};
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, Render,
    SharedString, Subscription, Window, WindowId, div, prelude::*, px,
};

use super::sql::{InsertWriter, default_rows_per_statement};
use super::{
    DELIMITERS, JobSpec, Outcome, Progress, close_dialog, count, dialog_frame, file_label, note,
    open_dialog, option_row, start_job,
};
use crate::net::Net;
use crate::theme::{Theme, theme};
use crate::ui::edit;
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{ButtonStyle, button, field, segmented, switch};

/// What to export.
#[derive(Clone)]
pub enum Source {
    /// A table, view or collection. `view` is the sort and filter the table tab shows,
    /// offered as a choice when it narrows anything; `columns` the tab's column list,
    /// which for a document collection holds the fields seen so far.
    Table {
        object: ObjectRef,
        view: Option<BrowseQuery>,
        columns: Option<Arc<[Column]>>,
    },
    /// The open result of a query tab.
    Result { loader: Arc<StreamLoader> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Csv,
    Json,
    Sql,
    Markdown,
}

impl Format {
    fn extension(self, json: JsonStyle, delimiter: u8) -> &'static str {
        match self {
            Format::Csv if delimiter == b'\t' => "tsv",
            Format::Csv => "csv",
            Format::Json if json == JsonStyle::Lines => "jsonl",
            Format::Json => "json",
            Format::Sql => "sql",
            Format::Markdown => "md",
        }
    }
}

const EXTENSIONS: [&str; 6] = ["csv", "tsv", "json", "jsonl", "sql", "md"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Everything,
    View,
}

pub struct ExportDialog {
    connection: Arc<dyn Connection>,
    kind: DriverKind,
    source: Source,
    /// The name the exported rows go by: the object's, or "query_result".
    name: String,
    page_rows: u32,
    format: Format,
    scope: Scope,
    delimiter: u8,
    header: bool,
    quote: QuoteStyle,
    json: JsonStyle,
    create_table: bool,
    /// The object's `CREATE` statement, once described.
    definition: Option<String>,
    null_text: Entity<TextInput>,
    rows_per_statement: Entity<TextInput>,
    table_name: Entity<TextInput>,
    path: Entity<TextInput>,
    /// The target exists and the next press replaces it.
    confirm_replace: bool,
    error: Option<SharedString>,
    /// The window the dialog is over, where the job's card shows.
    window: Option<WindowId>,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

/// Opens the export dialog over `window`.
pub fn open(
    connection: Arc<dyn Connection>,
    source: Source,
    page_rows: u32,
    window: &mut Window,
    cx: &mut App,
) {
    let id = window.window_handle().window_id();
    let view = cx.new(|cx| {
        let mut d = ExportDialog::new(connection, source, page_rows, cx);
        d.window = Some(id);
        d
    });
    // Typing goes to the file field first.
    let focus = view.read(cx).path.read(cx).focus_handle(cx);
    open_dialog(view.into(), window, cx);
    window.focus(&focus);
}

/// Where files go when the person has not picked a folder: the home folder.
pub fn default_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

impl ExportDialog {
    fn new(
        connection: Arc<dyn Connection>,
        source: Source,
        page_rows: u32,
        cx: &mut Context<Self>,
    ) -> Self {
        let kind = connection.kind();
        let name = match &source {
            Source::Table { object, .. } => object.name.clone(),
            Source::Result { .. } => "query_result".to_owned(),
        };
        let input = |cx: &mut Context<Self>, placeholder: &str, text: String| {
            cx.new(|cx| {
                let mut i = TextInput::new(placeholder.to_owned(), cx);
                i.set_text(text, cx);
                i
            })
        };
        let null_text = input(cx, "Empty", String::new());
        let rows_per_statement = input(cx, "Rows", default_rows_per_statement(kind).to_string());
        let table_name = input(cx, "Table", name.clone());
        let file = default_dir().join(format!("{}.csv", safe_file_name(&name)));
        let path = input(cx, "File", file.to_string_lossy().into_owned());
        let mut subs = Vec::new();
        for i in [&null_text, &rows_per_statement, &table_name, &path] {
            subs.push(cx.subscribe(i, |this, _, e, cx| match e {
                InputEvent::Cancel => close_dialog(cx),
                InputEvent::Submit => this.start(cx),
                InputEvent::Changed => {
                    this.confirm_replace = false;
                    this.error = None;
                    cx.notify();
                }
            }));
        }
        let scope = match &source {
            Source::Table { view: Some(q), .. } if narrows(q) => Scope::View,
            _ => Scope::Everything,
        };
        // The CREATE statement, for the SQL format's "Create the table first".
        if let Source::Table { object, .. } = &source {
            let (conn, object) = (connection.clone(), object.clone());
            let job = cx
                .global::<Net>()
                .run(async move { conn.describe(&object).await.ok() });
            cx.spawn(async move |this, cx| {
                let info = job.await.flatten();
                this.update(cx, |this, cx| {
                    this.definition = info.and_then(|i| i.definition);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        Self {
            connection,
            kind,
            source,
            name,
            page_rows: page_rows.max(1),
            format: Format::Csv,
            scope,
            delimiter: b',',
            header: true,
            quote: QuoteStyle::Necessary,
            json: JsonStyle::Array,
            create_table: false,
            definition: None,
            null_text,
            rows_per_statement,
            table_name,
            path,
            confirm_replace: false,
            error: None,
            window: None,
            focus: cx.focus_handle(),
            _subs: subs,
        }
    }

    fn title(&self) -> String {
        match &self.source {
            Source::Table { object, .. } => format!("Export {}", object.name),
            Source::Result { .. } => "Export the query result".to_owned(),
        }
    }

    /// Puts the format's extension on the file name, when it has one of ours.
    fn sync_extension(&mut self, cx: &mut Context<Self>) {
        let text = self.path.read(cx).text().to_owned();
        let path = PathBuf::from(&text);
        let known = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
        if known {
            let new = path.with_extension(self.format.extension(self.json, self.delimiter));
            self.path.update(cx, |i, cx| {
                i.set_text(new.to_string_lossy().into_owned(), cx)
            });
        }
        self.confirm_replace = false;
        cx.notify();
    }

    fn choose(&mut self, cx: &mut Context<Self>) {
        let current = PathBuf::from(self.path.read(cx).text());
        let dir = current
            .parent()
            .filter(|p| p.is_dir())
            .map(Path::to_path_buf)
            .unwrap_or_else(default_dir);
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

    fn writer(&self, cx: &App) -> Result<Box<dyn RowWriter>, String> {
        Ok(match self.format {
            Format::Csv => Box::new(CsvWriter::new(CsvFormat {
                delimiter: self.delimiter,
                header: self.header,
                quote: self.quote,
                null_text: self.null_text.read(cx).text().to_owned(),
                line_end: "\r\n",
            })),
            Format::Json => Box::new(JsonWriter::new(self.json, false)),
            Format::Markdown => Box::new(MarkdownWriter::new()),
            Format::Sql => {
                let rows = self.rows_per_statement.read(cx).text().trim();
                let rows: usize = rows
                    .parse()
                    .ok()
                    .filter(|n| (1..=100_000).contains(n))
                    .ok_or("Rows per INSERT needs a whole number from 1 to 100,000.")?;
                let object = match &self.source {
                    Source::Table { object, .. } => object.clone(),
                    Source::Result { .. } => {
                        let name = self.table_name.read(cx).text().trim().to_owned();
                        if name.is_empty() {
                            return Err("Name the table the INSERT statements go into.".into());
                        }
                        ObjectRef {
                            namespace: Vec::new(),
                            name,
                            kind: ObjectKind::Table,
                        }
                    }
                };
                let preamble = self.create_table.then(|| self.definition.clone()).flatten();
                Box::new(InsertWriter::new(self.kind, &object, rows).with_preamble(preamble))
            }
        })
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let text = self.path.read(cx).text().trim().to_owned();
        if text.is_empty() {
            self.error = Some("Choose a file to export to.".into());
            cx.notify();
            return;
        }
        let path = PathBuf::from(text);
        if path.is_dir() {
            self.error = Some("That is a folder; add a file name.".into());
            cx.notify();
            return;
        }
        if path
            .parent()
            .is_some_and(|p| !p.as_os_str().is_empty() && !p.is_dir())
        {
            self.error = Some("The folder does not exist.".into());
            cx.notify();
            return;
        }
        if path.exists() && !self.confirm_replace {
            self.confirm_replace = true;
            self.error =
                Some(format!("{} exists. Export again to replace it.", file_label(&path)).into());
            cx.notify();
            return;
        }
        let writer = match self.writer(cx) {
            Ok(w) => w,
            Err(e) => {
                self.error = Some(e.into());
                cx.notify();
                return;
            }
        };
        // A count to measure progress against: the table's estimate, which is instant,
        // or a result's size once it has been read whole. A filtered count can take as
        // long as the export, so a filtered view goes without.
        let expect = match (&self.source, self.scope) {
            (Source::Table { .. }, Scope::Everything) => true,
            (Source::Table { view, .. }, Scope::View) => !view.as_ref().is_some_and(narrows),
            (Source::Result { .. }, _) => false,
        };
        let (loader, stream, options): (Arc<dyn PageLoader>, Option<Arc<StreamLoader>>, _) =
            match &self.source {
                Source::Table {
                    object,
                    view,
                    columns,
                } => {
                    let query = match self.scope {
                        Scope::View => view.clone().unwrap_or_default(),
                        Scope::Everything => BrowseQuery::default(),
                    };
                    let loader = BrowseLoader::new(
                        self.connection.clone(),
                        object.clone(),
                        query,
                        BrowseConfig::default(),
                    );
                    (
                        Arc::new(loader),
                        None,
                        ExportOptions {
                            page_rows: self.page_rows,
                            columns: columns.clone(),
                        },
                    )
                }
                Source::Result { loader } => (
                    loader.clone(),
                    Some(loader.clone()),
                    ExportOptions {
                        // Pages of a result are read at the size they were stored at.
                        page_rows: loader.page_rows(),
                        columns: None,
                    },
                ),
            };
        let handle = cx.global::<Net>().handle();
        let spec = JobSpec {
            title: format!("Exporting {} to {}", self.name, file_label(&path)),
            unit: "rows",
            window: self.window,
            on_done: None,
        };
        start_job(
            spec,
            move |progress| {
                let total = match &stream {
                    Some(s) if s.is_complete() => Some(s.rows_read()),
                    Some(_) => None,
                    None if expect => handle.block_on(loader.count()).ok().and_then(|c| c.value()),
                    None => None,
                };
                progress
                    .total_rows
                    .store(total.unwrap_or(0), Ordering::Relaxed);
                let mut outcome = run(&handle, &*loader, writer, &options, &path, &progress);
                if let Some(reason) = stream.and_then(|s| s.stopped_reason())
                    && !outcome.failed
                {
                    outcome.message = format!(
                        "Only {} were exported: {reason}",
                        count(progress.rows.load(Ordering::Relaxed), "row", "rows")
                    );
                    outcome.failed = true;
                }
                outcome
            },
            cx,
        );
        close_dialog(cx);
    }

    fn csv_options(&self, t: &Theme, cx: &mut Context<Self>, window: &Window) -> Vec<AnyElement> {
        let weak = cx.entity().downgrade();
        let w2 = weak.clone();
        let w3 = weak.clone();
        let header = self.header;
        vec![
            option_row(
                "Delimiter",
                segmented(
                    "export-delimiter",
                    &DELIMITERS,
                    self.delimiter,
                    move |d, _, cx| {
                        weak.update(cx, |this, cx| {
                            this.delimiter = d;
                            this.sync_extension(cx);
                        })
                        .ok();
                    },
                    t,
                ),
                t,
            )
            .into_any_element(),
            option_row(
                "Quote fields",
                segmented(
                    "export-quote",
                    &[
                        ("When needed", QuoteStyle::Necessary),
                        ("Text", QuoteStyle::NonNumeric),
                        ("All", QuoteStyle::Always),
                    ],
                    self.quote,
                    move |q, _, cx| {
                        w2.update(cx, |this, cx| {
                            this.quote = q;
                            cx.notify();
                        })
                        .ok();
                    },
                    t,
                ),
                t,
            )
            .into_any_element(),
            option_row(
                "Header row",
                switch("export-header", header, t).on_click(move |_, _, cx| {
                    w3.update(cx, |this, cx| {
                        this.header = !this.header;
                        cx.notify();
                    })
                    .ok();
                }),
                t,
            )
            .into_any_element(),
            option_row(
                "NULL as",
                div()
                    .w(px(160.))
                    .child(field(&self.null_text, t, window, cx)),
                t,
            )
            .into_any_element(),
        ]
    }

    fn format_options(
        &self,
        t: &Theme,
        cx: &mut Context<Self>,
        window: &Window,
    ) -> Vec<AnyElement> {
        match self.format {
            Format::Csv => self.csv_options(t, cx, window),
            Format::Json => {
                let weak = cx.entity().downgrade();
                vec![
                    option_row(
                        "Layout",
                        segmented(
                            "export-json",
                            &[
                                ("Array", JsonStyle::Array),
                                ("JSON Lines", JsonStyle::Lines),
                            ],
                            self.json,
                            move |s, _, cx| {
                                weak.update(cx, |this, cx| {
                                    this.json = s;
                                    this.sync_extension(cx);
                                })
                                .ok();
                            },
                            t,
                        ),
                        t,
                    )
                    .into_any_element(),
                ]
            }
            Format::Sql => {
                let weak = cx.entity().downgrade();
                let mut rows = vec![
                    option_row(
                        "Rows per INSERT",
                        div()
                            .w(px(100.))
                            .child(field(&self.rows_per_statement, t, window, cx)),
                        t,
                    )
                    .into_any_element(),
                ];
                if matches!(self.source, Source::Result { .. }) {
                    rows.push(
                        option_row(
                            "Into table",
                            div()
                                .w(px(240.))
                                .child(field(&self.table_name, t, window, cx)),
                            t,
                        )
                        .into_any_element(),
                    );
                } else {
                    let available = self.definition.is_some();
                    rows.push(
                        option_row(
                            "CREATE first",
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .child(
                                    switch("export-create", self.create_table && available, t)
                                        .when(available, |el| {
                                            el.on_click(move |_, _, cx| {
                                                weak.update(cx, |this, cx| {
                                                    this.create_table = !this.create_table;
                                                    cx.notify();
                                                })
                                                .ok();
                                            })
                                        })
                                        .when(!available, |el| el.opacity(0.5).cursor_default()),
                                )
                                .child(note(
                                    if available {
                                        "Write the table's CREATE statement before the rows"
                                    } else {
                                        "The database gave no CREATE statement for this object"
                                    },
                                    false,
                                    t,
                                )),
                            t,
                        )
                        .into_any_element(),
                    );
                }
                rows
            }
            Format::Markdown => Vec::new(),
        }
    }
}

/// Whether a table view's query narrows or orders anything, so offering it is worth it.
fn narrows(q: &BrowseQuery) -> bool {
    !q.filters.is_empty() || !q.sort.is_empty() || q.raw_where.is_some()
}

/// A name with the characters file systems refuse replaced.
pub fn safe_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "export".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The temporary file an export writes before it is renamed to `path`. Each export gets
/// its own, so two exports to the same file never write into one another.
fn temp_path(path: &Path) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".into());
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(".{name}.{}-{n}.part", std::process::id()))
}

/// Writes every row of `loader` through `writer` into `path`, on the calling thread,
/// which may block. Rows land in a temporary file that replaces `path` at the end.
pub fn run(
    handle: &tokio::runtime::Handle,
    loader: &dyn PageLoader,
    mut writer: Box<dyn RowWriter>,
    options: &ExportOptions,
    path: &Path,
    progress: &Progress,
) -> Outcome {
    let temp = temp_path(path);
    let result = (|| -> Result<u64, deltos_data::Error> {
        let file = File::create(&temp).map_err(|e| {
            deltos_data::Error::Other(format!("could not create {}: {e}", temp.display()))
        })?;
        let mut out = BufWriter::with_capacity(256 * 1024, file);
        let written = handle.block_on(deltos_grid::export(
            loader,
            &mut *writer,
            options,
            &mut out,
            |n| {
                progress.rows.store(n, Ordering::Relaxed);
                !progress.cancelled()
            },
        ))?;
        let file = out
            .into_inner()
            .map_err(|e| deltos_data::Error::Other(format!("could not write the file: {e}")))?;
        file.sync_all()
            .map_err(|e| deltos_data::Error::Other(format!("could not write the file: {e}")))?;
        drop(file);
        std::fs::rename(&temp, path).map_err(|e| {
            deltos_data::Error::Other(format!("could not save {}: {e}", path.display()))
        })?;
        Ok(written)
    })();
    match result {
        Ok(n) => Outcome {
            file: Some(path.to_path_buf()),
            ..Outcome::done(format!(
                "Exported {} to {}",
                count(n, "row", "rows"),
                file_label(path)
            ))
        },
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            match e {
                deltos_data::Error::Cancelled => Outcome::failed(format!(
                    "Cancelled after {}; nothing was saved",
                    count(progress.rows.load(Ordering::Relaxed), "row", "rows")
                )),
                e => Outcome::failed(format!("Export failed: {e}")),
            }
        }
    }
}

impl Focusable for ExportDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ExportDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let weak = cx.entity().downgrade();
        let mut formats = vec![("CSV", Format::Csv), ("JSON", Format::Json)];
        if edit::speaks_sql(self.kind) {
            formats.push(("SQL", Format::Sql));
        }
        formats.push(("Markdown", Format::Markdown));
        let mut body = div().flex().flex_col().gap(px(10.));
        if let Source::Table { view: Some(q), .. } = &self.source
            && narrows(q)
        {
            let w = weak.clone();
            let n = q.filters.len();
            let label: &'static str = if n == 0 {
                "As sorted"
            } else if n == 1 {
                "1 filter"
            } else {
                "Filtered"
            };
            body = body.child(option_row(
                "Rows",
                segmented(
                    "export-scope",
                    &[("Whole table", Scope::Everything), (label, Scope::View)],
                    self.scope,
                    move |s, _, cx| {
                        w.update(cx, |this, cx| {
                            this.scope = s;
                            cx.notify();
                        })
                        .ok();
                    },
                    &t,
                ),
                &t,
            ));
        }
        let w = weak.clone();
        body = body.child(option_row(
            "Format",
            segmented(
                "export-format",
                &formats,
                self.format,
                move |f, _, cx| {
                    w.update(cx, |this, cx| {
                        this.format = f;
                        this.sync_extension(cx);
                    })
                    .ok();
                },
                &t,
            ),
            &t,
        ));
        body = body.children(self.format_options(&t, cx, window));
        body = body.child(option_row(
            "File",
            div()
                .flex_1()
                .flex()
                .gap(px(6.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(field(&self.path, &t, window, cx)),
                )
                .child(
                    button("export-choose", "Choose…", ButtonStyle::Secondary, &t)
                        .on_click(cx.listener(|this, _, _, cx| this.choose(cx))),
                ),
            &t,
        ));
        if let Some(e) = &self.error {
            body = body.child(note(e.clone(), true, &t));
        }
        let buttons = vec![
            button("export-cancel", "Cancel", ButtonStyle::Ghost, &t)
                .on_click(|_, _, cx| close_dialog(cx))
                .into_any_element(),
            button(
                "export-start",
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
        let subtitle: SharedString = match &self.source {
            Source::Table { object, .. } => {
                let mut path = object.namespace.clone();
                path.push(object.name.clone());
                path.join(".").into()
            }
            Source::Result { loader } => format!(
                "{} columns, {} read so far",
                loader.columns().len(),
                count(loader.rows_read(), "row", "rows")
            )
            .into(),
        };
        div().track_focus(&self.focus).child(dialog_frame(
            self.title(),
            Some(subtitle),
            body,
            buttons,
            &t,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_safe() {
        assert_eq!(safe_file_name("orders"), "orders");
        assert_eq!(safe_file_name("a/b:c?"), "a_b_c_");
        assert_eq!(safe_file_name(" .. "), "export");
        let temp = temp_path(Path::new("/tmp/x/orders.csv"));
        assert_eq!(temp.parent(), Some(Path::new("/tmp/x")));
        assert!(file_label(&temp).starts_with(".orders.csv."));
        assert_ne!(temp, temp_path(Path::new("/tmp/x/orders.csv")));
    }

    /// Exports a real SQLite table through the same path the dialog takes.
    #[test]
    fn exports_a_table_to_a_file_and_replaces_it_whole() {
        let dir = std::env::temp_dir().join(format!("deltos-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("shop.db");
        let _ = std::fs::remove_file(&db);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let conn = rt
            .block_on(async {
                let driver = deltos_drivers::driver(DriverKind::Sqlite).unwrap();
                let config = deltos_data::ConnectionConfig {
                    database: db.to_string_lossy().into_owned(),
                    options: vec![("create".into(), "true".into())],
                    ..Default::default()
                };
                driver.connect(&config).await
            })
            .unwrap();
        let options = deltos_data::ExecuteOptions::default();
        for sql in [
            "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, price REAL)",
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2500) \
             INSERT INTO items SELECT i, 'item ' || i, i * 0.5 FROM n",
        ] {
            rt.block_on(conn.execute(sql, &options)).unwrap();
        }
        let object = ObjectRef {
            namespace: vec!["main".into()],
            name: "items".into(),
            kind: ObjectKind::Table,
        };
        let target = dir.join("items.csv");
        std::fs::write(&target, "old").unwrap();
        let loader = BrowseLoader::new(
            conn.clone(),
            object.clone(),
            BrowseQuery::default(),
            BrowseConfig::default(),
        );
        let progress = Progress::default();
        let outcome = run(
            rt.handle(),
            &loader,
            Box::new(CsvWriter::new(CsvFormat::default())),
            &ExportOptions {
                page_rows: 1000,
                columns: None,
            },
            &target,
            &progress,
        );
        assert!(!outcome.failed, "{}", outcome.message);
        assert_eq!(outcome.message, "Exported 2,500 rows to items.csv");
        let text = std::fs::read_to_string(&target).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2501);
        assert_eq!(lines[0], "id,name,price");
        assert_eq!(lines[1], "1,item 1,0.5");
        assert_eq!(lines[2500], "2500,item 2500,1250");
        let no_part = || {
            std::fs::read_dir(&dir)
                .unwrap()
                .all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".part"))
        };
        assert!(no_part(), "no temporary file is left");

        // A cancelled export leaves the old file as it was.
        let progress = Progress::default();
        progress.cancel();
        let info = rt.block_on(conn.describe(&object)).unwrap_or_default();
        let outcome = run(
            rt.handle(),
            &loader,
            Box::new(
                InsertWriter::new(DriverKind::Sqlite, &object, 100).with_preamble(info.definition),
            ),
            &ExportOptions::default(),
            &target,
            &progress,
        );
        assert!(outcome.failed);
        assert!(
            outcome.message.starts_with("Cancelled"),
            "{}",
            outcome.message
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), text);
        assert!(no_part(), "a cancelled export removes its temporary file");

        // SQL with the CREATE statement first runs back into an empty database.
        let sql_path = dir.join("items.sql");
        let outcome = run(
            rt.handle(),
            &loader,
            Box::new(
                InsertWriter::new(DriverKind::Sqlite, &object, 100)
                    .with_preamble(rt.block_on(conn.describe(&object)).unwrap().definition),
            ),
            &ExportOptions::default(),
            &sql_path,
            &Progress::default(),
        );
        assert!(!outcome.failed, "{}", outcome.message);
        let sql = std::fs::read_to_string(&sql_path).unwrap();
        assert!(sql.starts_with("CREATE TABLE items"), "{sql}");
        assert_eq!(sql.matches("INSERT INTO").count(), 25);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
