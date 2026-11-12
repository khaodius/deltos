//! Importing a CSV or JSON file into a table, and running a SQL script.
//!
//! A file import reads the file one record at a time ([`super::read`]), turns each mapped
//! field into a value of its column's type with [`edit::parse_input`], the same rules as
//! typing into a cell, and inserts the rows [`BATCH`] at a time through
//! `Connection::apply_edits`. Nothing but the batch in flight is held in memory. A row that
//! does not fit stops the import at its line, or, with "Skip bad rows", is left out and
//! listed when the import ends; a batch the database refuses is then retried row by row
//! so only the rows at fault are left out.
//!
//! A script is split into statements a chunk at a time by the drivers' splitter and run
//! in order; the first failing statement stops it.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use deltos_data::{
    Column, Connection, DriverKind, ExecuteOptions, ObjectRef, RowEdit, Value, ValueKind,
};
use deltos_drivers::sql::Dialect;
use futures::StreamExt;
use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement,
    Render, SharedString, Subscription, Window, WindowId, div, prelude::*, px,
};

use super::read::{Counting, CsvReader, JsonReader, ReadError, ScriptReader};
use super::sql::truncate_statement;
use super::{
    DELIMITERS, JobSpec, Outcome, Progress, Safety, Write, close_dialog, count, dialog_frame,
    file_label, note, open_dialog, option_row, start_job, warning,
};
use crate::net::Net;
use crate::theme::{Theme, theme};
use crate::ui::edit;
use crate::ui::grid::CellInput;
use crate::ui::icons::{Icon, icon};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{ButtonStyle, MenuItem, button, field, menu, segmented, switch};

/// Rows per `apply_edits` call.
pub const BATCH: usize = 500;
/// Records the preview reads and shows.
const PREVIEW_ROWS: usize = 6;
/// Records the preview reads to find a JSON file's keys.
const PREVIEW_SCAN: usize = 50;
/// Bad rows an import lists by line; the rest are counted.
const LISTED_ERRORS: usize = 50;

/// What a file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    Csv,
    Json,
    Sql,
}

impl FileFormat {
    /// The format a file's extension suggests.
    pub fn guess(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "csv" | "tsv" | "txt" => FileFormat::Csv,
            "json" | "jsonl" | "ndjson" => FileFormat::Json,
            "sql" => FileFormat::Sql,
            _ => return None,
        })
    }
}

/// How to read a CSV file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvRead {
    pub delimiter: u8,
    pub header: bool,
    /// A field with exactly this text and no quotes is NULL; a quoted one is always
    /// text. When empty, an empty field is NULL, except in a text column that cannot be
    /// NULL, where it is an empty string.
    pub null_text: String,
}

/// Everything a file import needs besides the connection.
#[derive(Debug, Clone)]
pub struct Plan {
    pub object: ObjectRef,
    pub columns: Vec<Column>,
    pub format: FileFormat,
    pub csv: CsvRead,
    /// The file's column names (a CSV header, or JSON keys), and for each the table
    /// column it goes into, or `None` to skip it.
    pub file_columns: Vec<String>,
    pub targets: Vec<Option<usize>>,
    pub skip_bad: bool,
    pub truncate: bool,
}

/// The table column each file column goes into: the one of the same name, ignoring case.
/// A table column already taken by an earlier file column is not given out again.
pub fn auto_map(file_columns: &[String], columns: &[Column]) -> Vec<Option<usize>> {
    let mut taken = vec![false; columns.len()];
    file_columns
        .iter()
        .map(|name| {
            let name = name.trim();
            let free = |i: &usize| !taken[*i];
            let found = (0..columns.len())
                .filter(free)
                .find(|i| columns[*i].name == name)
                .or_else(|| {
                    (0..columns.len())
                        .filter(free)
                        .find(|i| columns[*i].name.eq_ignore_ascii_case(name))
                });
            if let Some(i) = found {
                taken[i] = true;
            }
            found
        })
        .collect()
}

/// The first table column that two file columns are mapped to, if any.
pub fn mapped_twice<'a>(targets: &[Option<usize>], columns: &'a [Column]) -> Option<&'a str> {
    targets.iter().enumerate().find_map(|(i, t)| {
        let t = (*t)?;
        targets[..i]
            .contains(&Some(t))
            .then(|| columns.get(t).map(|c| c.name.as_str()))
            .flatten()
    })
}

/// A CSV field as cell input for `column`. A `quoted` field is text whatever it says,
/// as the export writes text that reads like the NULL text in quotes.
pub fn csv_input(text: &str, quoted: bool, null_text: &str, column: &Column) -> CellInput {
    if quoted {
        return CellInput::Text(text.to_owned());
    }
    if null_text.is_empty() {
        let text_column = matches!(column.kind, ValueKind::Text | ValueKind::Other);
        if text.is_empty() && !(text_column && column.nullable == Some(false)) {
            return CellInput::Null;
        }
    } else if text == null_text {
        return CellInput::Null;
    }
    CellInput::Text(text.to_owned())
}

/// A JSON value as cell input: strings as they are, numbers and booleans as written,
/// nested objects and arrays as compact JSON.
pub fn json_input(value: &serde_json::Value) -> CellInput {
    match value {
        serde_json::Value::Null => CellInput::Null,
        serde_json::Value::String(s) => CellInput::Text(s.clone()),
        other => CellInput::Text(other.to_string()),
    }
}

/// The value `input` stands for in `column`. Bytes written as `0x` and hex, as the
/// export writes them, become bytes; everything else follows the cell editor's rules.
pub fn convert(column: &Column, input: &CellInput) -> Result<Value, String> {
    if column.kind == ValueKind::Bytes
        && let CellInput::Text(text) = input
        && let Some(hex) = text.trim().strip_prefix("0x")
    {
        return parse_hex(hex)
            .map(|b| Value::Bytes(b.into()))
            .ok_or_else(|| format!("{} needs hex bytes after 0x", column.name));
    }
    edit::parse_input(column, input)
}

fn parse_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

/// One record of the file as an insert, or why it does not fit.
fn record_edit(plan: &Plan, record: &Record) -> Result<RowEdit, String> {
    let mut values = Vec::new();
    for (i, target) in plan.targets.iter().enumerate() {
        let Some(column) = target.and_then(|c| plan.columns.get(c)) else {
            continue;
        };
        let input = match record {
            Record::Csv(record) => match record.fields.get(i) {
                Some(text) => {
                    let quoted = record.quoted.get(i).copied().unwrap_or(false);
                    csv_input(text, quoted, &plan.csv.null_text, column)
                }
                None => continue,
            },
            Record::Json(object) => match plan.file_columns.get(i).and_then(|k| object.get(k)) {
                Some(v) => json_input(v),
                None => continue,
            },
        };
        values.push((column.name.clone(), convert(column, &input)?));
    }
    Ok(RowEdit::Insert { values })
}

enum Record {
    Csv(super::read::Record),
    Json(serde_json::Map<String, serde_json::Value>),
}

/// Reads records from a CSV or JSON file, with the line each starts on.
enum Source {
    Csv(CsvReader<FileReader>),
    Json(JsonReader<FileReader>),
}

type FileReader = BufReader<Counting<File>>;

/// Opens `path` for reading, with a count of the bytes read so far and the file's size.
fn open_file(path: &Path) -> Result<(FileReader, Arc<AtomicU64>, u64), String> {
    let file = File::open(path).map_err(|e| format!("Could not open {}: {e}", path.display()))?;
    let size = file.metadata().map_or(0, |m| m.len());
    let counter = Arc::new(AtomicU64::new(0));
    let reader = BufReader::with_capacity(256 * 1024, Counting::new(file, counter.clone()));
    Ok((reader, counter, size))
}

impl Source {
    fn new(reader: FileReader, format: FileFormat, delimiter: u8) -> Self {
        match format {
            FileFormat::Json => Source::Json(JsonReader::new(reader)),
            _ => Source::Csv(CsvReader::new(reader, delimiter)),
        }
    }

    fn next(&mut self) -> Result<Option<(u64, Record)>, ReadError> {
        Ok(match self {
            Source::Csv(r) => r.next_record()?.map(|r| (r.line, Record::Csv(r))),
            Source::Json(r) => r.next_object()?.map(|(line, o)| (line, Record::Json(o))),
        })
    }
}

/// What the preview shows: the file's columns and its first rows as text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Preview {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Reads the start of a CSV or JSON file for the preview.
pub fn preview(path: &Path, format: FileFormat, csv: &CsvRead) -> Result<Preview, String> {
    let (reader, _, _) = open_file(path)?;
    let mut source = Source::new(reader, format, csv.delimiter);
    let mut out = Preview::default();
    match &mut source {
        Source::Csv(reader) => {
            let mut first = true;
            while out.rows.len() < PREVIEW_ROWS {
                let Some(record) = reader.next_record().map_err(|e| e.to_string())? else {
                    break;
                };
                if first && csv.header {
                    out.columns = record.fields;
                    first = false;
                    continue;
                }
                first = false;
                if out.columns.len() < record.fields.len() && !csv.header {
                    out.columns =
                        (out.columns.len()..record.fields.len()).fold(out.columns, |mut c, i| {
                            c.push(format!("Column {}", i + 1));
                            c
                        });
                }
                out.rows.push(record.fields);
            }
        }
        Source::Json(reader) => {
            let mut objects = Vec::new();
            for _ in 0..PREVIEW_SCAN {
                let Some((_, object)) = reader.next_object().map_err(|e| e.to_string())? else {
                    break;
                };
                for key in object.keys() {
                    if !out.columns.contains(key) {
                        out.columns.push(key.clone());
                    }
                }
                if objects.len() < PREVIEW_ROWS {
                    objects.push(object);
                }
            }
            out.rows = objects
                .iter()
                .map(|o| {
                    out.columns
                        .iter()
                        .map(|k| match o.get(k) {
                            None => String::new(),
                            Some(serde_json::Value::String(s)) => s.clone(),
                            Some(v) => v.to_string(),
                        })
                        .collect()
                })
                .collect();
        }
    }
    Ok(out)
}

/// What became of the rows: how many went in and which were left out.
#[derive(Debug, Default)]
struct Tally {
    inserted: u64,
    skipped: u64,
    listed: Vec<String>,
}

impl Tally {
    fn skip(&mut self, line: u64, message: &str) {
        self.skipped += 1;
        if self.listed.len() < LISTED_ERRORS {
            self.listed.push(format!("Line {line}: {message}"));
        }
    }
}

/// Runs a file import on the calling thread, which may block.
pub fn run_import(
    handle: &tokio::runtime::Handle,
    conn: &Arc<dyn Connection>,
    plan: &Plan,
    path: &Path,
    progress: &Progress,
) -> Outcome {
    let mut tally = Tally::default();
    let result = import_rows(handle, conn, plan, path, progress, &mut tally);
    progress.rows.store(tally.inserted, Ordering::Relaxed);
    let into = &plan.object.name;
    let mut outcome = match result {
        Ok(()) => Outcome::done(format!(
            "Imported {} into {into}{}",
            count(tally.inserted, "row", "rows"),
            if tally.skipped > 0 {
                format!(", skipped {}", count(tally.skipped, "bad row", "bad rows"))
            } else {
                String::new()
            }
        )),
        Err(Stop::Cancelled) => Outcome::failed(format!(
            "Cancelled; {} had been imported into {into}",
            count(tally.inserted, "row", "rows")
        )),
        Err(Stop::Failed(e)) => Outcome::failed(format!(
            "{e} {} before it {} imported into {into}.",
            count(tally.inserted, "row", "rows"),
            if tally.inserted == 1 { "was" } else { "were" }
        )),
    };
    if tally.skipped > tally.listed.len() as u64 {
        let more = tally.skipped - tally.listed.len() as u64;
        tally
            .listed
            .push(format!("…and {} more", count(more, "row", "rows")));
    }
    outcome.details = tally.listed;
    outcome
}

enum Stop {
    Cancelled,
    Failed(String),
}

fn import_rows(
    handle: &tokio::runtime::Handle,
    conn: &Arc<dyn Connection>,
    plan: &Plan,
    path: &Path,
    progress: &Progress,
    tally: &mut Tally,
) -> Result<(), Stop> {
    let (reader, counter, size) = open_file(path).map_err(Stop::Failed)?;
    progress.total_bytes.store(size, Ordering::Relaxed);
    let mut source = Source::new(reader, plan.format, plan.csv.delimiter);
    if plan.truncate {
        let statement = truncate_statement(conn.kind(), &plan.object);
        let options = ExecuteOptions {
            namespace: Some(plan.object.namespace.clone()),
            ..ExecuteOptions::default()
        };
        handle
            .block_on(async {
                let mut stream = conn.execute(&statement, &options).await?;
                while let Some(batch) = stream.batches.next().await {
                    batch?;
                }
                Ok::<_, deltos_data::Error>(())
            })
            .map_err(|e| Stop::Failed(format!("Could not empty {}: {e}.", plan.object.name)))?;
    }
    let mut batch: Vec<(u64, RowEdit)> = Vec::with_capacity(BATCH);
    let mut first = true;
    loop {
        if progress.cancelled() {
            return Err(Stop::Cancelled);
        }
        let next = source.next();
        progress
            .bytes
            .store(counter.load(Ordering::Relaxed), Ordering::Relaxed);
        let next = match next {
            Ok(n) => n,
            Err(e) => {
                flush(handle, conn, plan, &mut batch, tally, progress)?;
                return Err(Stop::Failed(format!("Stopped at {e}.")));
            }
        };
        let Some((line, record)) = next else { break };
        if std::mem::take(&mut first) && plan.format == FileFormat::Csv && plan.csv.header {
            continue;
        }
        match record_edit(plan, &record) {
            Ok(edit) => batch.push((line, edit)),
            Err(e) if plan.skip_bad => tally.skip(line, &e),
            Err(e) => {
                flush(handle, conn, plan, &mut batch, tally, progress)?;
                return Err(Stop::Failed(format!("Stopped at line {line}: {e}.")));
            }
        }
        if batch.len() >= BATCH {
            flush(handle, conn, plan, &mut batch, tally, progress)?;
        }
    }
    flush(handle, conn, plan, &mut batch, tally, progress)
}

/// Inserts the rows waiting in `batch`. When the database refuses the batch and bad rows
/// may be skipped, the rows go in one at a time so only the bad ones stay out.
///
/// A database without transactions (MongoDB) keeps the rows of a batch that went in
/// before the one it refused, so a batch there could neither be retried row by row
/// without doubling them nor be reported as not imported. Its rows go in one at a time
/// from the start; its driver writes them one by one anyway.
fn flush(
    handle: &tokio::runtime::Handle,
    conn: &Arc<dyn Connection>,
    plan: &Plan,
    batch: &mut Vec<(u64, RowEdit)>,
    tally: &mut Tally,
    progress: &Progress,
) -> Result<(), Stop> {
    if batch.is_empty() {
        return Ok(());
    }
    if !conn.capabilities().transactions {
        for (line, edit) in std::mem::take(batch) {
            if progress.cancelled() {
                return Err(Stop::Cancelled);
            }
            match handle.block_on(conn.apply_edits(&plan.object, std::slice::from_ref(&edit))) {
                Ok(_) => tally.inserted += 1,
                Err(e) if plan.skip_bad => tally.skip(line, &e.to_string()),
                Err(e) => {
                    return Err(Stop::Failed(format!(
                        "The database refused the row at line {line}: {e}."
                    )));
                }
            }
            progress.rows.store(tally.inserted, Ordering::Relaxed);
        }
        return Ok(());
    }
    let edits: Vec<RowEdit> = batch.iter().map(|(_, e)| e.clone()).collect();
    let result = handle.block_on(conn.apply_edits(&plan.object, &edits));
    let lines = (batch[0].0, batch[batch.len() - 1].0);
    match result {
        Ok(_) => tally.inserted += edits.len() as u64,
        Err(e) if !plan.skip_bad => {
            batch.clear();
            let at = if lines.0 == lines.1 {
                format!("line {}", lines.0)
            } else {
                format!("lines {} to {}", lines.0, lines.1)
            };
            return Err(Stop::Failed(format!(
                "The database refused the rows at {at}: {e}. The rows of that batch were not imported;"
            )));
        }
        Err(_) => {
            for (line, edit) in batch.iter() {
                if progress.cancelled() {
                    batch.clear();
                    return Err(Stop::Cancelled);
                }
                match handle.block_on(conn.apply_edits(&plan.object, std::slice::from_ref(edit))) {
                    Ok(_) => tally.inserted += 1,
                    Err(e) => tally.skip(*line, &e.to_string()),
                }
            }
        }
    }
    batch.clear();
    progress.rows.store(tally.inserted, Ordering::Relaxed);
    Ok(())
}

/// The splitter's dialect for a database.
pub fn dialect(kind: DriverKind) -> Dialect {
    match kind {
        DriverKind::Mysql => Dialect::Mysql,
        DriverKind::Sqlite => Dialect::Sqlite,
        _ => Dialect::Postgres,
    }
}

/// Runs every statement of a script in order on the calling thread, which may block.
/// Results are read to their end and dropped a batch at a time.
pub fn run_script(
    handle: &tokio::runtime::Handle,
    conn: &Arc<dyn Connection>,
    namespace: Option<Vec<String>>,
    path: &Path,
    progress: &Progress,
) -> Outcome {
    let (reader, counter, size) = match open_file(path) {
        Ok(r) => r,
        Err(e) => return Outcome::failed(e),
    };
    progress.total_bytes.store(size, Ordering::Relaxed);
    let mut script = ScriptReader::new(reader, dialect(conn.kind()));
    let options = ExecuteOptions {
        batch_size: 1000,
        namespace,
    };
    let mut done = 0u64;
    let ran = |n: u64| count(n, "statement", "statements");
    loop {
        if progress.cancelled() {
            return Outcome::failed(format!("Cancelled after {}", ran(done)));
        }
        let statement = match script.next_statement() {
            Ok(Some(s)) => s,
            Ok(None) => break,
            Err(e) => {
                return Outcome::failed(format!("Stopped at {e}. {} ran before it.", ran(done)));
            }
        };
        progress
            .bytes
            .store(counter.load(Ordering::Relaxed), Ordering::Relaxed);
        let result = handle.block_on(async {
            let mut stream = conn.execute(&statement.text, &options).await?;
            while let Some(batch) = stream.batches.next().await {
                batch?;
            }
            Ok::<_, deltos_data::Error>(())
        });
        if let Err(e) = result {
            return Outcome::failed(format!(
                "Statement {} (line {}) failed: {e}. {} ran before it.",
                done + 1,
                statement.line,
                ran(done)
            ));
        }
        done += 1;
        progress.rows.store(done, Ordering::Relaxed);
    }
    Outcome::done(format!("Ran {} from {}", ran(done), file_label(path)))
}

/// The file and options a preview was read with.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewKey {
    path: PathBuf,
    format: FileFormat,
    delimiter: u8,
    header: bool,
}

/// Where an import goes.
#[derive(Clone)]
pub enum Target {
    /// Rows into a table or collection; a SQL script runs in its namespace.
    Table(ObjectRef),
    /// Only a script, run in this namespace.
    Namespace(Vec<String>),
}

pub struct ImportDialog {
    connection: Arc<dyn Connection>,
    kind: DriverKind,
    target: Target,
    columns: Option<Result<Vec<Column>, String>>,
    format: FileFormat,
    delimiter: u8,
    header: bool,
    skip_bad: bool,
    truncate: bool,
    /// What the last press asked the person to confirm; the next press with the same
    /// question goes ahead.
    asked: Option<String>,
    /// The connection's environment: read-only refuses, staging and production confirm.
    safety: Safety,
    null_text: Entity<TextInput>,
    path: Entity<TextInput>,
    preview: Option<Result<Preview, String>>,
    /// The file and options `preview` was read with. Typing a new path or changing an
    /// option reads it again a moment later; until then it describes another file.
    preview_for: Option<PreviewKey>,
    preview_generation: u64,
    targets: Vec<Option<usize>>,
    /// The file column whose target menu is open.
    menu: Option<usize>,
    error: Option<SharedString>,
    window: Option<WindowId>,
    on_done: Option<Box<dyn FnOnce(&Outcome, &mut App)>>,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

/// Opens the import dialog over `window`. `on_done` runs when an import or script ends,
/// to reload what it changed.
pub fn open(
    connection: Arc<dyn Connection>,
    target: Target,
    safety: Safety,
    on_done: Option<Box<dyn FnOnce(&Outcome, &mut App)>>,
    window: &mut Window,
    cx: &mut App,
) {
    let id = window.window_handle().window_id();
    let view = cx.new(|cx| {
        let mut d = ImportDialog::new(connection, target, cx);
        d.safety = safety;
        d.window = Some(id);
        d.on_done = on_done;
        d
    });
    // Typing goes to the file field first.
    let focus = view.read(cx).path.read(cx).focus_handle(cx);
    open_dialog(view.into(), window, cx);
    window.focus(&focus);
}

impl ImportDialog {
    fn new(connection: Arc<dyn Connection>, target: Target, cx: &mut Context<Self>) -> Self {
        let kind = connection.kind();
        let null_text = cx.new(|cx| TextInput::new("Empty field", cx));
        let path = cx.new(|cx| TextInput::new("File to import", cx));
        let mut subs = Vec::new();
        subs.push(cx.subscribe(&path, |this, _, e, cx| match e {
            InputEvent::Cancel => close_dialog(cx),
            InputEvent::Submit => this.reload_preview(cx),
            InputEvent::Changed => {
                this.error = None;
                this.asked = None;
                this.schedule_preview(cx);
            }
        }));
        subs.push(cx.subscribe(&null_text, |_, _, e, cx| {
            if let InputEvent::Cancel = e {
                close_dialog(cx)
            }
        }));
        let format = match &target {
            Target::Namespace(_) => FileFormat::Sql,
            Target::Table(_) => FileFormat::Csv,
        };
        if let Target::Table(object) = &target {
            let (conn, object) = (connection.clone(), object.clone());
            let job = cx.global::<Net>().run(async move {
                conn.describe(&object)
                    .await
                    .map(|i| i.columns)
                    .map_err(|e| e.to_string())
            });
            cx.spawn(async move |this, cx| {
                let columns = job.await.unwrap_or_else(|| Err("cancelled".into()));
                this.update(cx, |this, cx| {
                    this.columns = Some(columns);
                    this.remap();
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        Self {
            connection,
            kind,
            target,
            columns: None,
            format,
            delimiter: b',',
            header: true,
            skip_bad: false,
            truncate: false,
            asked: None,
            safety: Safety::default(),
            null_text,
            path,
            preview: None,
            preview_for: None,
            preview_generation: 0,
            targets: Vec::new(),
            menu: None,
            error: None,
            window: None,
            on_done: None,
            focus: cx.focus_handle(),
            _subs: subs,
        }
    }

    fn object(&self) -> Option<&ObjectRef> {
        match &self.target {
            Target::Table(o) => Some(o),
            Target::Namespace(_) => None,
        }
    }

    fn csv_read(&self, cx: &App) -> CsvRead {
        CsvRead {
            delimiter: self.delimiter,
            header: self.header,
            null_text: self.null_text.read(cx).text().to_owned(),
        }
    }

    /// What the preview depends on, as the dialog stands.
    fn preview_key(&self, cx: &App) -> Option<PreviewKey> {
        Some(PreviewKey {
            path: self.file(cx)?,
            format: self.format,
            delimiter: self.delimiter,
            header: self.header,
        })
    }

    fn file(&self, cx: &App) -> Option<PathBuf> {
        let text = self.path.read(cx).text().trim().to_owned();
        (!text.is_empty()).then(|| PathBuf::from(text))
    }

    /// Reads the preview again a moment after typing stops.
    fn schedule_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_generation += 1;
        let generation = self.preview_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            this.update(cx, |this, cx| {
                if this.preview_generation == generation {
                    this.reload_preview(cx);
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn reload_preview(&mut self, cx: &mut Context<Self>) {
        self.preview_generation += 1;
        let generation = self.preview_generation;
        let Some(path) = self.file(cx) else {
            self.preview = None;
            cx.notify();
            return;
        };
        if self.format == FileFormat::Sql {
            self.preview = None;
            cx.notify();
            return;
        }
        let (format, csv) = (self.format, self.csv_read(cx));
        let key = self.preview_key(cx);
        let job = cx
            .global::<Net>()
            .run_blocking(move || preview(&path, format, &csv));
        cx.spawn(async move |this, cx| {
            let result = job.await.unwrap_or_else(|| Err("cancelled".into()));
            this.update(cx, |this, cx| {
                if this.preview_generation != generation {
                    return;
                }
                this.preview = Some(result);
                this.preview_for = key;
                this.remap();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Matches the file's columns to the table's again.
    fn remap(&mut self) {
        let (Some(Ok(preview)), Some(Ok(columns))) = (&self.preview, &self.columns) else {
            return;
        };
        let names = &preview.columns;
        self.targets = if self.format == FileFormat::Csv && !self.header {
            // Without names, columns go in order.
            (0..names.len())
                .map(|i| (i < columns.len()).then_some(i))
                .collect()
        } else {
            auto_map(names, columns)
        };
    }

    fn set_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(format) = FileFormat::guess(&path)
            && (self.object().is_some() || format == FileFormat::Sql)
            && (format != FileFormat::Sql || edit::speaks_sql(self.kind))
        {
            self.format = format;
        }
        if path.extension().and_then(|e| e.to_str()) == Some("tsv") {
            self.delimiter = b'\t';
        }
        self.path.update(cx, |i, cx| {
            i.set_text(path.to_string_lossy().into_owned(), cx)
        });
        self.reload_preview(cx);
    }

    fn choose(&mut self, cx: &mut Context<Self>) {
        let answer = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = answer.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |this, cx| this.set_path(path, cx)).ok();
        })
        .detach();
    }

    fn fail(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.error = Some(message.into());
        cx.notify();
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.file(cx) else {
            return self.fail("Choose a file to import.", cx);
        };
        if !path.is_file() {
            return self.fail("The file does not exist.", cx);
        }
        if let Some(refusal) = self.safety.refusal() {
            self.asked = None;
            return self.fail(refusal, cx);
        }
        let file = file_label(&path);
        if self.format == FileFormat::Sql
            && let Some(question) = self.safety.confirmation(Write::Script { file: &file })
            && self.asked.as_ref() != Some(&question)
        {
            self.asked = Some(question.clone());
            return self.fail(question, cx);
        }
        let conn = self.connection.clone();
        let handle = cx.global::<Net>().handle();
        let on_done = self.on_done.take();
        if self.format == FileFormat::Sql {
            let namespace = match &self.target {
                Target::Table(o) => o.namespace.clone(),
                Target::Namespace(n) => n.clone(),
            };
            let spec = JobSpec {
                title: format!("Running {}", file_label(&path)),
                unit: "statements",
                window: self.window,
                on_done,
            };
            start_job(
                spec,
                move |progress| run_script(&handle, &conn, Some(namespace), &path, &progress),
                cx,
            );
            close_dialog(cx);
            return;
        }
        if matches!(self.preview, Some(Ok(_))) && self.preview_for != self.preview_key(cx) {
            self.on_done = on_done;
            self.reload_preview(cx);
            return self.fail("Wait for the preview of this file.", cx);
        }
        let (Some(object), Some(Ok(columns)), Some(Ok(preview))) =
            (self.object().cloned(), &self.columns, &self.preview)
        else {
            self.on_done = on_done;
            return self.fail("Wait for the preview, or fix what it says.", cx);
        };
        if !self.targets.iter().any(Option::is_some) {
            self.on_done = on_done;
            return self.fail("Map at least one column of the file to the table.", cx);
        }
        if let Some(name) = mapped_twice(&self.targets, columns) {
            let message = format!("Two columns of the file go into {name}. Skip one of them.");
            self.on_done = on_done;
            return self.fail(message, cx);
        }
        let write = Write::Import {
            file: &file,
            table: &object.name,
            truncate: self.truncate,
        };
        if let Some(question) = self.safety.confirmation(write)
            && self.asked.as_ref() != Some(&question)
        {
            self.on_done = on_done;
            self.asked = Some(question.clone());
            return self.fail(question, cx);
        }
        let plan = Plan {
            object: object.clone(),
            columns: columns.clone(),
            format: self.format,
            csv: self.csv_read(cx),
            file_columns: preview.columns.clone(),
            targets: self.targets.clone(),
            skip_bad: self.skip_bad,
            truncate: self.truncate,
        };
        let spec = JobSpec {
            title: format!("Importing {} into {}", file_label(&path), object.name),
            unit: "rows",
            window: self.window,
            on_done,
        };
        start_job(
            spec,
            move |progress| run_import(&handle, &conn, &plan, &path, &progress),
            cx,
        );
        close_dialog(cx);
    }

    fn preview_table(&self, preview: &Preview, t: &Theme) -> AnyElement {
        let cell = |text: &str, header: bool| {
            let mut shown: String = text.chars().take(40).collect();
            if text.chars().count() > 40 {
                shown.push('…');
            }
            div()
                .w(px(130.))
                .flex_none()
                .px(px(6.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .when(header, |el| {
                    el.font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.text_muted)
                })
                .child(shown.replace(['\n', '\r'], " "))
        };
        let row = |cells: Vec<gpui::Div>, header: bool, zebra: bool| {
            div()
                .flex()
                .h(t.row_height)
                .items_center()
                .when(header, |el| el.bg(t.grid_header))
                .when(zebra, |el| el.bg(t.surface_alt))
                .border_b_1()
                .border_color(t.grid_line)
                .children(cells)
        };
        let mut list = div().flex().flex_col().child(row(
            preview.columns.iter().map(|c| cell(c, true)).collect(),
            true,
            false,
        ));
        for (i, r) in preview.rows.iter().enumerate() {
            list = list.child(row(
                (0..preview.columns.len())
                    .map(|c| cell(r.get(c).map_or("", String::as_str), false))
                    .collect(),
                false,
                t.zebra && i % 2 == 1,
            ));
        }
        div()
            .id("import-preview")
            .max_w_full()
            .overflow_x_scroll()
            .rounded(t.radius_small)
            .border_1()
            .border_color(t.border)
            .font_family(t.mono_font.clone())
            .text_size(t.small_size)
            .child(list)
            .into_any_element()
    }

    fn mapping(
        &self,
        preview: &Preview,
        columns: &[Column],
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let weak = cx.entity().downgrade();
        let mut list = div().flex().flex_col().gap(px(4.));
        for (i, name) in preview.columns.iter().enumerate() {
            let target = self.targets.get(i).copied().flatten();
            let label: SharedString = target
                .and_then(|c| columns.get(c))
                .map_or_else(|| "Skip".into(), |c| c.name.clone().into());
            let mut picker = div()
                .id(("import-target", i))
                .relative()
                .w(px(220.))
                .h(t.s(24.))
                .px(px(8.))
                .flex()
                .items_center()
                .justify_between()
                .rounded(t.radius_small)
                .bg(t.input)
                .border_1()
                .border_color(t.border_strong)
                .cursor_pointer()
                .text_size(t.small_size)
                .text_color(if target.is_some() {
                    t.text
                } else {
                    t.text_faint
                })
                .child(div().overflow_hidden().whitespace_nowrap().child(label))
                .child(
                    icon(Icon::ChevronDown)
                        .size(px(11.))
                        .text_color(t.text_faint),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.menu = Some(i);
                    cx.notify();
                }));
            if self.menu == Some(i) {
                let mut items = Vec::new();
                let w = weak.clone();
                items.push(
                    MenuItem::new("Skip", move |_, cx| {
                        w.update(cx, |this, cx| {
                            if let Some(t) = this.targets.get_mut(i) {
                                *t = None;
                            }
                            cx.notify();
                        })
                        .ok();
                    })
                    .checked(target.is_none()),
                );
                items.push(MenuItem::separator());
                for (c, column) in columns.iter().enumerate() {
                    let w = weak.clone();
                    items.push(
                        MenuItem::new(column.name.clone(), move |_, cx| {
                            w.update(cx, |this, cx| {
                                if let Some(t) = this.targets.get_mut(i) {
                                    *t = Some(c);
                                }
                                cx.notify();
                            })
                            .ok();
                        })
                        .detail(column.type_name.clone())
                        .checked(target == Some(c)),
                    );
                }
                let close = weak.clone();
                picker = picker.child(menu(
                    "import-target-menu",
                    items,
                    220.,
                    move |_, cx| {
                        close
                            .update(cx, |this, cx| {
                                this.menu = None;
                                cx.notify();
                            })
                            .ok();
                    },
                    t,
                ));
            }
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .w(px(200.))
                            .flex_none()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_family(t.mono_font.clone())
                            .text_size(t.small_size)
                            .child(name.clone()),
                    )
                    .child(
                        icon(Icon::ChevronRight)
                            .size(px(11.))
                            .text_color(t.text_faint),
                    )
                    .child(picker),
            );
        }
        list.into_any_element()
    }
}

impl Focusable for ImportDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ImportDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let weak = cx.entity().downgrade();
        let mut body = div().flex().flex_col().gap(px(10.));
        // Where it goes, in the environment's color, on staging and production.
        if let Some(env) = self.safety.sensitive() {
            body = body.child(option_row(
                "Connection",
                crate::ui::environment::named(env, &t),
                &t,
            ));
        }
        if let Some(refusal) = self.safety.refusal() {
            body = body.child(warning(refusal, &t));
        }
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
                    button("import-choose", "Choose…", ButtonStyle::Secondary, &t)
                        .on_click(cx.listener(|this, _, _, cx| this.choose(cx))),
                ),
            &t,
        ));
        if self.object().is_some() {
            let mut formats = vec![("CSV", FileFormat::Csv), ("JSON", FileFormat::Json)];
            if edit::speaks_sql(self.kind) {
                formats.push(("SQL script", FileFormat::Sql));
            }
            let w = weak.clone();
            body = body.child(option_row(
                "Format",
                segmented(
                    "import-format",
                    &formats,
                    self.format,
                    move |f, _, cx| {
                        w.update(cx, |this, cx| {
                            this.format = f;
                            this.asked = None;
                            this.error = None;
                            this.reload_preview(cx);
                        })
                        .ok();
                    },
                    &t,
                ),
                &t,
            ));
        }
        if self.format == FileFormat::Csv {
            let (w1, w2) = (weak.clone(), weak.clone());
            body = body
                .child(option_row(
                    "Delimiter",
                    segmented(
                        "import-delimiter",
                        &DELIMITERS,
                        self.delimiter,
                        move |d, _, cx| {
                            w1.update(cx, |this, cx| {
                                this.delimiter = d;
                                this.reload_preview(cx);
                            })
                            .ok();
                        },
                        &t,
                    ),
                    &t,
                ))
                .child(option_row(
                    "Header row",
                    switch("import-header", self.header, &t).on_click(move |_, _, cx| {
                        w2.update(cx, |this, cx| {
                            this.header = !this.header;
                            this.reload_preview(cx);
                        })
                        .ok();
                    }),
                    &t,
                ))
                .child(option_row(
                    "NULL is",
                    div()
                        .w(px(160.))
                        .child(field(&self.null_text, &t, window, cx)),
                    &t,
                ));
        }
        if self.format == FileFormat::Sql {
            body = body.child(note(
                "The script's statements run one after another, and the first that fails stops it. Statements run as they are, without asking.",
                false,
                &t,
            ));
        } else {
            match (&self.preview, &self.columns) {
                (_, Some(Err(e))) => {
                    body = body.child(note(format!("Could not read the table: {e}"), true, &t));
                }
                (Some(Err(e)), _) => body = body.child(note(e.clone(), true, &t)),
                (Some(Ok(p)), Some(Ok(columns))) if p.columns.is_empty() => {
                    let _ = columns;
                    body = body.child(note("The file has no rows.", true, &t));
                }
                (Some(Ok(p)), Some(Ok(columns))) => {
                    let p = p.clone();
                    let columns = columns.clone();
                    body = body
                        .child(crate::ui::caption("Preview", &t))
                        .child(self.preview_table(&p, &t))
                        .child(crate::ui::caption("Columns", &t))
                        .child(self.mapping(&p, &columns, &t, cx));
                }
                (Some(Ok(_)), None) => body = body.child(note("Reading the table…", false, &t)),
                (None, _) => {
                    body = body.child(note(
                        "Choose a CSV or JSON file (an array of objects, or one object per line).",
                        false,
                        &t,
                    ))
                }
            }
            let (w1, w2) = (weak.clone(), weak.clone());
            body = body
                .child(option_row(
                    "Skip bad rows",
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(switch("import-skip", self.skip_bad, &t).on_click(
                            move |_, _, cx| {
                                w1.update(cx, |this, cx| {
                                    this.skip_bad = !this.skip_bad;
                                    cx.notify();
                                })
                                .ok();
                            },
                        ))
                        .child(note(
                            if self.skip_bad {
                                "Rows that don't fit are left out and listed at the end"
                            } else {
                                "The first row that doesn't fit stops the import"
                            },
                            false,
                            &t,
                        )),
                    &t,
                ))
                .child(option_row(
                    "Empty first",
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(switch("import-truncate", self.truncate, &t).on_click(
                            move |_, _, cx| {
                                w2.update(cx, |this, cx| {
                                    this.truncate = !this.truncate;
                                    this.asked = None;
                                    this.error = None;
                                    cx.notify();
                                })
                                .ok();
                            },
                        ))
                        .child(if self.truncate {
                            warning("Deletes every row of the table before importing", &t)
                        } else {
                            note("Keep the rows the table has", false, &t)
                        }),
                    &t,
                ));
        }
        if let Some(e) = &self.error {
            body = body.child(if self.asked.is_some() {
                warning(e.clone(), &t)
            } else {
                note(e.clone(), true, &t)
            });
        }
        let danger = self.asked.is_some();
        let label = match self.format {
            FileFormat::Sql => "Run",
            _ if self.truncate => "Empty and import",
            _ => "Import",
        };
        let buttons = vec![
            button("import-cancel", "Cancel", ButtonStyle::Ghost, &t)
                .on_click(|_, _, cx| close_dialog(cx))
                .into_any_element(),
            button(
                "import-start",
                label,
                if danger {
                    ButtonStyle::Danger
                } else {
                    ButtonStyle::Primary
                },
                &t,
            )
            .on_click(cx.listener(|this, _, _, cx| this.start(cx)))
            .into_any_element(),
        ];
        let (title, subtitle): (String, SharedString) = match &self.target {
            Target::Table(o) => (format!("Import into {}", o.name), {
                let mut path = o.namespace.clone();
                path.push(o.name.clone());
                path.join(".").into()
            }),
            Target::Namespace(n) => ("Run a SQL file".to_owned(), n.join(".").into()),
        };
        div()
            .track_focus(&self.focus)
            .child(dialog_frame(title, Some(subtitle), body, buttons, &t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_data::{ObjectKind, ValueKind};

    fn column(name: &str, kind: ValueKind, nullable: bool) -> Column {
        let mut c = Column::new(name, "t", kind);
        c.nullable = Some(nullable);
        c
    }

    #[test]
    fn maps_columns_by_name_ignoring_case() {
        let columns = vec![
            column("id", ValueKind::Int, false),
            column("Name", ValueKind::Text, true),
            column("name", ValueKind::Text, true),
        ];
        let file: Vec<String> = ["ID", "name", " NAME ", "other"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            auto_map(&file, &columns),
            [Some(0), Some(2), Some(1), None],
            "an exact match wins over one that ignores case"
        );
        // A column already taken is not given to a second file column.
        let file: Vec<String> = ["id", "ID"].iter().map(|s| s.to_string()).collect();
        assert_eq!(auto_map(&file, &columns), [Some(0), None]);
        assert_eq!(
            mapped_twice(&[Some(1), None, Some(1)], &columns),
            Some("Name")
        );
        assert_eq!(mapped_twice(&[Some(0), None, Some(1)], &columns), None);
    }

    #[test]
    fn reads_fields_as_column_values() {
        let int = column("n", ValueKind::Int, true);
        let text = column("t", ValueKind::Text, false);
        let bytes = column("b", ValueKind::Bytes, true);
        assert_eq!(csv_input("", false, "", &int), CellInput::Null);
        assert_eq!(
            csv_input("", false, "", &text),
            CellInput::Text(String::new())
        );
        assert_eq!(csv_input("\\N", false, "\\N", &int), CellInput::Null);
        assert_eq!(
            csv_input("", false, "\\N", &text),
            CellInput::Text(String::new())
        );
        // Quotes make a field text, as the export writes text that reads like NULL.
        let nullable_text = column("u", ValueKind::Text, true);
        assert_eq!(csv_input("", false, "", &nullable_text), CellInput::Null);
        assert_eq!(
            csv_input("", true, "", &nullable_text),
            CellInput::Text(String::new())
        );
        assert_eq!(
            csv_input("\\N", true, "\\N", &nullable_text),
            CellInput::Text("\\N".into())
        );
        assert_eq!(
            convert(&int, &CellInput::Text("7".into())),
            Ok(Value::Int(7))
        );
        assert!(convert(&int, &CellInput::Text("x".into())).is_err());
        assert_eq!(
            convert(&bytes, &CellInput::Text("0x00ff".into())),
            Ok(Value::Bytes(vec![0, 255].into()))
        );
        assert!(convert(&bytes, &CellInput::Text("0x0".into())).is_err());
        assert_eq!(
            json_input(&serde_json::json!({"a": [1]})),
            CellInput::Text("{\"a\":[1]}".into())
        );
        assert_eq!(
            json_input(&serde_json::json!(2.5)),
            CellInput::Text("2.5".into())
        );
        assert_eq!(json_input(&serde_json::Value::Null), CellInput::Null);
        assert_eq!(
            FileFormat::guess(Path::new("a.NDJSON")),
            Some(FileFormat::Json)
        );
        assert_eq!(FileFormat::guess(Path::new("a")), None);
    }

    struct Db {
        rt: tokio::runtime::Runtime,
        conn: Arc<dyn Connection>,
        dir: PathBuf,
        object: ObjectRef,
    }

    impl Db {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("deltos-import-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let db = dir.join("shop.db");
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
            let this = Self {
                rt,
                conn,
                dir,
                object: ObjectRef {
                    namespace: vec!["main".into()],
                    name: "people".into(),
                    kind: ObjectKind::Table,
                },
            };
            this.run("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INTEGER, note TEXT)");
            this
        }

        fn run(&self, sql: &str) -> Vec<Vec<Value>> {
            self.rt.block_on(async {
                let mut stream = self
                    .conn
                    .execute(sql, &ExecuteOptions::default())
                    .await
                    .unwrap();
                let mut rows = Vec::new();
                while let Some(batch) = stream.batches.next().await {
                    rows.extend(batch.unwrap().rows.into_iter().map(|r| r.into_vec()));
                }
                rows
            })
        }

        fn plan(&self, format: FileFormat, file_columns: &[&str]) -> Plan {
            let columns = self
                .rt
                .block_on(self.conn.describe(&self.object))
                .unwrap()
                .columns;
            let file_columns: Vec<String> = file_columns.iter().map(|s| s.to_string()).collect();
            Plan {
                object: self.object.clone(),
                targets: auto_map(&file_columns, &columns),
                columns,
                format,
                csv: CsvRead {
                    delimiter: b',',
                    header: true,
                    null_text: String::new(),
                },
                file_columns,
                skip_bad: false,
                truncate: false,
            }
        }

        fn import(&self, plan: &Plan, file: &str, text: &str) -> Outcome {
            let path = self.dir.join(file);
            std::fs::write(&path, text).unwrap();
            run_import(
                self.rt.handle(),
                &self.conn,
                plan,
                &path,
                &Progress::default(),
            )
        }
    }

    impl Drop for Db {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn imports_csv_in_batches_and_stops_at_a_bad_line() {
        let db = Db::new("csv");
        let mut csv = String::from("ID,Name,age,extra\n");
        for i in 1..=1200 {
            csv.push_str(&format!("{i},\"person, {i}\",{},x\n", i % 90));
        }
        let plan = db.plan(FileFormat::Csv, &["ID", "Name", "age", "extra"]);
        let outcome = db.import(&plan, "people.csv", &csv);
        assert!(!outcome.failed, "{}", outcome.message);
        assert_eq!(outcome.message, "Imported 1,200 rows into people");
        assert_eq!(
            db.run("SELECT count(*), max(name) FROM people")[0],
            [Value::Int(1200), Value::text("person, 999")]
        );

        // Line 4 holds a word where a number goes: the rows before it go in, it stops.
        let bad = "id,name,age\n2001,a,1\n2002,b,2\n2003,c,old\n2004,d,4\n";
        let outcome = db.import(&plan_for(&db, &["id", "name", "age"]), "bad.csv", bad);
        assert!(outcome.failed);
        assert!(
            outcome
                .message
                .starts_with("Stopped at line 4: age needs a whole number"),
            "{}",
            outcome.message
        );
        assert!(
            outcome
                .message
                .ends_with("2 rows before it were imported into people."),
            "{}",
            outcome.message
        );
        assert_eq!(
            db.run("SELECT count(*) FROM people WHERE id > 2000")[0],
            [Value::Int(2)]
        );

        // Skipping: the bad line and a duplicate key are left out and listed.
        let mut plan = plan_for(&db, &["id", "name", "age"]);
        plan.skip_bad = true;
        let outcome = db.import(
            &plan,
            "bad2.csv",
            "id,name,age\n3001,a,1\n3002,b,x\n2001,dup,3\n3003,c,\n",
        );
        assert!(!outcome.failed, "{}", outcome.message);
        assert_eq!(
            outcome.message,
            "Imported 2 rows into people, skipped 2 bad rows"
        );
        assert_eq!(outcome.details.len(), 2);
        assert!(
            outcome.details[0].starts_with("Line 3: age"),
            "{:?}",
            outcome.details
        );
        assert!(
            outcome.details[1].starts_with("Line 4: "),
            "{:?}",
            outcome.details
        );
        assert_eq!(
            db.run("SELECT age FROM people WHERE id = 3003")[0],
            [Value::Null]
        );

        // A quoted empty field is an empty string; a bare one is NULL.
        let outcome = db.import(
            &plan_for(&db, &["id", "name", "note"]),
            "empty.csv",
            "id,name,note\n4001,a,\"\"\n4002,b,\n",
        );
        assert!(!outcome.failed, "{}", outcome.message);
        assert_eq!(
            db.run("SELECT note IS NULL FROM people WHERE id > 4000 ORDER BY id"),
            [[Value::Int(0)], [Value::Int(1)]]
        );

        // Emptying first.
        let mut plan = plan_for(&db, &["id", "name"]);
        plan.truncate = true;
        let outcome = db.import(&plan, "small.csv", "id,name\n1,only\n");
        assert!(!outcome.failed, "{}", outcome.message);
        assert_eq!(db.run("SELECT count(*) FROM people")[0], [Value::Int(1)]);
    }

    fn plan_for(db: &Db, names: &[&str]) -> Plan {
        db.plan(FileFormat::Csv, names)
    }

    #[test]
    fn imports_json_arrays_and_lines() {
        let db = Db::new("json");
        let plan = db.plan(FileFormat::Json, &["id", "name", "note"]);
        let outcome = db.import(
            &plan,
            "people.json",
            "[{\"id\": 1, \"name\": \"Ann\", \"note\": {\"tags\": [\"a\"]}},\n {\"id\": 2, \"name\": \"Bo\"}]",
        );
        assert!(!outcome.failed, "{}", outcome.message);
        assert_eq!(
            db.run("SELECT id, name, note FROM people ORDER BY id"),
            [
                vec![
                    Value::Int(1),
                    Value::text("Ann"),
                    Value::text("{\"tags\":[\"a\"]}")
                ],
                vec![Value::Int(2), Value::text("Bo"), Value::Null],
            ]
        );
        let outcome = db.import(
            &plan,
            "more.jsonl",
            "{\"id\": 3, \"name\": \"Cy\"}\n{\"id\": 4, \"name\": null}\n",
        );
        assert!(outcome.failed);
        assert!(
            outcome
                .message
                .starts_with("Stopped at line 2: name can't be NULL"),
            "{}",
            outcome.message
        );
    }

    #[test]
    fn read_only_connections_take_no_imports_or_scripts() {
        let db = Db::new("read-only");
        db.run("INSERT INTO people (id, name) VALUES (1, 'kept')");
        let guarded = crate::safety::guard(db.conn.clone(), true);
        let path = db.dir.join("rows.csv");
        std::fs::write(&path, "id,name\n2,new\n").unwrap();
        let mut plan = plan_for(&db, &["id", "name"]);
        for truncate in [false, true] {
            plan.truncate = truncate;
            let outcome = run_import(db.rt.handle(), &guarded, &plan, &path, &Progress::default());
            assert!(outcome.failed, "{}", outcome.message);
            assert!(outcome.message.contains("Read-only"), "{}", outcome.message);
        }
        let script = db.dir.join("change.sql");
        std::fs::write(
            &script,
            "DELETE FROM people;\nINSERT INTO people (name) VALUES ('x');\n",
        )
        .unwrap();
        let outcome = run_script(
            db.rt.handle(),
            &guarded,
            None,
            &script,
            &Progress::default(),
        );
        assert!(outcome.failed, "{}", outcome.message);
        assert_eq!(
            db.run("SELECT id, name FROM people"),
            [[Value::Int(1), Value::text("kept")]]
        );
    }

    fn environment(tag: deltos_settings::ConnectionTag, read_only: bool) -> Safety {
        Safety {
            environment: Some(crate::safety::Environment {
                name: "Main".into(),
                kind: DriverKind::Postgres,
                tag,
                read_only,
            }),
            confirm_writes: true,
        }
    }

    #[test]
    fn sensitive_connections_confirm_with_their_name() {
        use deltos_settings::ConnectionTag;
        let script = Write::Script { file: "seed.sql" };
        let import = |truncate| Write::Import {
            file: "people.csv",
            table: "people",
            truncate,
        };
        let prod = environment(ConnectionTag::Production, false);
        for write in [script, import(true), import(false)] {
            let asked = prod.confirmation(write).unwrap_or_default();
            assert!(asked.contains("Production · Main"), "{asked}");
        }
        // "Confirm every write on production" off: an import that only adds rows goes
        // ahead, the rest still asks.
        let relaxed = Safety {
            confirm_writes: false,
            ..prod.clone()
        };
        assert_eq!(relaxed.confirmation(import(false)), None);
        assert!(relaxed.confirmation(script).is_some());
        assert!(relaxed.confirmation(import(true)).is_some());
        // Elsewhere only emptying the table asks.
        let dev = environment(ConnectionTag::Development, false);
        assert_eq!(dev.confirmation(script), None);
        assert_eq!(dev.confirmation(import(false)), None);
        let asked = dev.confirmation(import(true)).unwrap_or_default();
        assert!(
            asked.starts_with("Every row of people will be deleted"),
            "{asked}"
        );
        assert_eq!(Safety::default().confirmation(script), None);
        // Read-only refuses instead.
        assert!(
            environment(ConnectionTag::Staging, true)
                .refusal()
                .is_some()
        );
        assert!(prod.refusal().is_none());
    }

    #[test]
    fn runs_scripts_and_stops_at_the_failing_statement() {
        let db = Db::new("sql");
        let path = db.dir.join("seed.sql");
        std::fs::write(
            &path,
            "INSERT INTO people (name) VALUES ('a;b');\n-- two\nINSERT INTO people (name) VALUES ('c');\nINSERT INTO nowhere VALUES (1);\nINSERT INTO people (name) VALUES ('never');\n",
        )
        .unwrap();
        let outcome = run_script(db.rt.handle(), &db.conn, None, &path, &Progress::default());
        assert!(outcome.failed);
        assert!(
            outcome.message.starts_with("Statement 3 (line 4) failed"),
            "{}",
            outcome.message
        );
        assert!(
            outcome.message.ends_with("2 statements ran before it."),
            "{}",
            outcome.message
        );
        assert_eq!(db.run("SELECT count(*) FROM people")[0], [Value::Int(2)]);
    }
}
