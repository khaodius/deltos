//! The data grid: a view over a [`GridCache`] that draws only the rows and columns on
//! screen, so a table of fifty million rows and three hundred columns costs the same to
//! draw as one of ten.
//!
//! Scrolling is kept in rows, as an `f64`, rather than in pixels: GPUI lays out in `f32`
//! pixels, which stop being exact past about sixteen million, while a row position in
//! `f64` is exact far beyond any table. The scrollbars are drawn here for the same
//! reason. Cells are painted directly in one canvas: their text is formatted once per
//! row and column into a small cache and shaped by GPUI's line cache, so a steady frame
//! allocates nothing per cell.

use std::cell::Cell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use deltos_data::{Column, SortDirection, Value, ValueKind};
use deltos_grid::{GridCache, Hooks, NotifyFn, RowCount, Status};
use deltos_settings::DateFormat;
use futures::StreamExt;
use gpui::{
    App, BorderStyle, Bounds, ClipboardItem, Context, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, Hsla, IntoElement, KeyBinding, KeyDownEvent, MouseButton, Pixels, Point, Render,
    SharedString, Subscription, Task, TextRun, Window, actions, canvas, div, fill, outline, point,
    prelude::*, px, size,
};

use crate::net::Net;
use crate::theme::{Theme, theme};
use crate::ui::icons::{Icon, icon};
use crate::ui::pending::{NewRow, PendingChanges, RowChange, RowMark};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::tsv;
use crate::ui::{MenuItem, context_menu};

actions!(
    grid,
    [
        MoveUp,
        MoveDown,
        MoveLeft,
        MoveRight,
        SelectUp,
        SelectDown,
        SelectLeft,
        SelectRight,
        PageUp,
        PageDown,
        GoTop,
        GoBottom,
        RowStart,
        RowEnd,
        CopyCells,
        CopyWithHeaders,
        SelectAllCells,
        EditCell,
        JumpUp,
        JumpDown,
        JumpLeft,
        JumpRight,
        NextCell,
        PreviousCell,
        ClearCells,
        PasteCells,
    ]
);

pub fn bind_keys(cx: &mut App) {
    let ctx = Some("Grid");
    cx.bind_keys([
        KeyBinding::new("up", MoveUp, ctx),
        KeyBinding::new("down", MoveDown, ctx),
        KeyBinding::new("left", MoveLeft, ctx),
        KeyBinding::new("right", MoveRight, ctx),
        KeyBinding::new("shift-up", SelectUp, ctx),
        KeyBinding::new("shift-down", SelectDown, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("pageup", PageUp, ctx),
        KeyBinding::new("pagedown", PageDown, ctx),
        KeyBinding::new("ctrl-home", GoTop, ctx),
        KeyBinding::new("ctrl-end", GoBottom, ctx),
        KeyBinding::new("home", RowStart, ctx),
        KeyBinding::new("end", RowEnd, ctx),
        KeyBinding::new("ctrl-c", CopyCells, ctx),
        KeyBinding::new("ctrl-shift-c", CopyWithHeaders, ctx),
        KeyBinding::new("ctrl-a", SelectAllCells, ctx),
        KeyBinding::new("enter", EditCell, ctx),
        KeyBinding::new("f2", EditCell, ctx),
    ]);
}

/// The context of the text field that edits a cell, for keys that save it and move on
/// (Tab), bound after the field's own keys so they win.
pub const EDITOR_CONTEXT: &str = "Grid > TextInput";

/// A step past any edge: `step` clamps it to the first or last row or column.
const JUMP: i64 = i64::MAX / 4;
/// Most rows a paste or a clear changes at once; the staged changes stay small.
pub const EDIT_ROWS: usize = 10_000;
/// Most rows a copy takes, so a select-all over a huge table cannot fill the clipboard
/// with gigabytes.
const COPY_LIMIT: u64 = 100_000;
/// Formatted cell texts kept between frames.
const TEXT_CACHE_LIMIT: usize = 16_384;
const SCROLLBAR: f32 = 10.0;
const MIN_COLUMN: f32 = 48.0;

/// Builds the hooks a cache reaches the app through. `notify` may fire on any thread; it
/// sets a flag and wakes one task here, which redraws the entity at most once per frame
/// however many pages land at once.
pub fn hooks<T: 'static>(cx: &mut Context<T>) -> (Hooks, Task<()>) {
    let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
    let pending = Arc::new(AtomicBool::new(false));
    let flag = pending.clone();
    let notify: NotifyFn = Arc::new(move || {
        if !flag.swap(true, Ordering::AcqRel) {
            let _ = tx.unbounded_send(());
        }
    });
    let handle = cx.global::<Net>().handle();
    let blocking = handle.clone();
    let hooks = Hooks {
        notify,
        spawn: Arc::new(move |f| {
            handle.spawn(f);
        }),
        spawn_blocking: Arc::new(move |f| {
            blocking.spawn_blocking(f);
        }),
    };
    let task = cx.spawn(async move |this, cx| {
        while rx.next().await.is_some() {
            // Let a burst of pages land together.
            cx.background_executor()
                .timer(Duration::from_millis(8))
                .await;
            pending.store(false, Ordering::Release);
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                break;
            }
        }
    });
    (hooks, task)
}

#[derive(Debug, Clone)]
pub enum GridEvent {
    /// The header of this column was clicked.
    Sort(usize),
    /// The selection moved to this row.
    Selected(u64),
    /// An edited cell was saved: Enter, or a click somewhere else. `target` is the row
    /// the edit was started on, so a reload that moved the rows meanwhile cannot send it
    /// to another row; `None` means the row now at `row`.
    Edit {
        row: u64,
        col: usize,
        input: CellInput,
        target: Option<RowRef>,
    },
    /// Several cells changed at once. `outside` counts the pasted cells that fell past
    /// the last row or column; `limited` says rows past [`EDIT_ROWS`] were left out.
    Edits {
        cells: Vec<(u64, usize, CellInput)>,
        batch: Batch,
        outside: usize,
        limited: bool,
    },
}

/// What changed several cells at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Batch {
    /// Delete, or "Set to NULL": each cell to NULL, or to empty text in a column that
    /// can't be NULL.
    Clear,
    Paste,
}

/// What the person entered in a cell, before it is read as the column's type.
#[derive(Debug, Clone, PartialEq)]
pub enum CellInput {
    Text(String),
    Null,
}

/// A grid row as the owner needs it to stage a change: a staged new row by its place
/// among them, or a table row with the values it was read with.
#[derive(Debug, Clone, PartialEq)]
pub enum RowRef {
    New(usize),
    Table(Vec<Value>),
}

/// One row as the grid shows it.
enum Shown<'a> {
    New(&'a NewRow),
    Row(&'a [Value], Option<&'a RowChange>),
    Loading,
}

impl<'a> Shown<'a> {
    /// The value shown in `col`, and whether it is a staged one. `None` is a new row's
    /// default.
    fn cell(&self, col: usize) -> Option<(Option<&'a Value>, bool)> {
        match *self {
            Shown::New(row) => Some((row.values.get(col)?.as_ref(), true)),
            Shown::Row(values, change) => match change.and_then(|c| c.cell(col)) {
                Some(c) => Some((Some(&c.value), true)),
                None => Some((Some(values.get(col)?), false)),
            },
            Shown::Loading => None,
        }
    }

    fn mark(&self) -> RowMark {
        match self {
            Shown::New(_) => RowMark::Inserted,
            Shown::Row(_, Some(c)) if c.deleted => RowMark::Deleted,
            Shown::Row(_, Some(_)) => RowMark::Changed,
            _ => RowMark::None,
        }
    }

    /// The values as shown, staged changes in, defaults as NULL.
    fn values(&self, n: usize) -> Option<Vec<Value>> {
        match self {
            Shown::Loading => None,
            _ => Some(
                (0..n)
                    .map(|c| {
                        self.cell(c)
                            .and_then(|(v, _)| v.cloned())
                            .unwrap_or(Value::Null)
                    })
                    .collect(),
            ),
        }
    }
}

/// Calls `f` for each grid row in `range`: staged new rows first, then the cache's rows.
fn each_row<'a>(
    cache: Option<&'a GridCache>,
    pending: &'a PendingChanges,
    range: Range<u64>,
    mut f: impl FnMut(u64, Shown<'_>),
) {
    let inserts = pending.inserts();
    let n = inserts.len() as u64;
    for i in range.start..range.end.min(n) {
        f(i, Shown::New(&inserts[i as usize]));
    }
    let rest = range.start.max(n)..range.end.max(n);
    if rest.is_empty() {
        return;
    }
    let Some(cache) = cache else {
        return;
    };
    cache.with_rows(rest.start - n..rest.end - n, |i, row| {
        let shown = match row {
            Some(values) => Shown::Row(values, pending.row(values)),
            None => Shown::Loading,
        };
        f(i + n, shown);
    });
}

/// What a right click landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuTarget {
    Cell(u64, usize),
    Header(usize),
}

/// Adds the owner's items to the grid's right-click menu, after the grid's own.
pub type MenuProvider = Rc<dyn Fn(MenuTarget, &GridView, &App) -> Vec<MenuItem>>;

/// A cell being edited in place.
struct CellEditor {
    row: u64,
    col: usize,
    /// The row being edited, as it was read when the edit began.
    target: Option<RowRef>,
    input: Entity<TextInput>,
    original: String,
    _subs: Vec<Subscription>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (u64, usize),
    pub head: (u64, usize),
}

impl Selection {
    fn rows(&self) -> Range<u64> {
        self.anchor.0.min(self.head.0)..self.anchor.0.max(self.head.0) + 1
    }

    fn cols(&self) -> Range<usize> {
        self.anchor.1.min(self.head.1)..self.anchor.1.max(self.head.1) + 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellKind {
    Plain,
    Number,
    Null,
    Bool,
}

#[derive(Debug, Clone, Copy)]
enum PointerDrag {
    Select,
    Column {
        index: usize,
        start_x: Pixels,
        start_width: f32,
    },
    VBar {
        grab: f32,
    },
    HBar {
        grab: f32,
    },
}

pub struct GridView {
    cache: Option<Arc<GridCache>>,
    columns: Arc<[Column]>,
    /// Column widths in unscaled pixels.
    widths: Vec<f32>,
    /// Whether the widths were fitted to the first loaded rows.
    fitted: bool,
    /// The first visible row, with a fraction for a partly scrolled row.
    top: f64,
    scroll_x: f32,
    pub selection: Option<Selection>,
    sort: Vec<(usize, SortDirection)>,
    focus: FocusHandle,
    body: Rc<Cell<Option<Bounds<Pixels>>>>,
    texts: HashMap<(u64, u32), (SharedString, CellKind)>,
    drag: Option<PointerDrag>,
    /// Shown in the body when there are no columns: a statement that changed rows, an
    /// error, or a hint.
    pub message: Option<SharedString>,
    /// Whether cells can be edited in place, which the owner decides.
    pub editable: bool,
    editor: Option<CellEditor>,
    /// An open right-click menu: where, and over what.
    context: Option<(Point<Pixels>, MenuTarget)>,
    menu_provider: Option<MenuProvider>,
    /// Changes the owner staged and has not committed, drawn over the rows they change.
    pub pending: PendingChanges,
    /// The staged state the formatted texts were made from.
    pending_seen: u64,
    _pump: Option<Task<()>>,
}

impl EventEmitter<GridEvent> for GridView {}

impl GridView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            cache: None,
            columns: Arc::from(Vec::new()),
            widths: Vec::new(),
            fitted: false,
            top: 0.0,
            scroll_x: 0.0,
            selection: None,
            sort: Vec::new(),
            focus: cx.focus_handle(),
            body: Rc::default(),
            texts: HashMap::new(),
            drag: None,
            message: None,
            editable: false,
            editor: None,
            context: None,
            menu_provider: None,
            pending: PendingChanges::default(),
            pending_seen: 0,
            _pump: None,
        }
    }

    pub fn set_menu_provider(&mut self, provider: MenuProvider) {
        self.menu_provider = Some(provider);
    }

    /// The selection, cells as `(rows, columns)`.
    pub fn selected_ranges(&self) -> Option<(Range<u64>, Range<usize>)> {
        self.selection.map(|s| (s.rows(), s.cols()))
    }

    /// Staged new rows, shown above the table's rows.
    pub fn inserted(&self) -> u64 {
        self.pending.inserts().len() as u64
    }

    /// The cache's row behind grid row `row`, or `None` for a staged new row.
    pub fn cache_row(&self, row: u64) -> Option<u64> {
        row.checked_sub(self.inserted())
    }

    /// Runs `f` on one grid row as shown, when it is there.
    fn with_shown<T>(&self, row: u64, f: impl FnOnce(&Shown<'_>) -> T) -> Option<T> {
        let mut out = None;
        let mut f = Some(f);
        each_row(
            self.cache.as_deref(),
            &self.pending,
            row..row + 1,
            |_, shown| {
                if let Some(f) = f.take() {
                    out = Some(f(&shown));
                }
            },
        );
        out
    }

    /// One value as shown, a staged one where there is one. A new row's default is NULL.
    pub fn value(&self, row: u64, col: usize) -> Option<Value> {
        self.with_shown(row, |s| {
            s.cell(col).map(|(v, _)| v.cloned().unwrap_or_default())
        })
        .flatten()
    }

    /// Whether a cell of a staged new row is left to the database's default.
    pub fn is_default(&self, row: u64, col: usize) -> bool {
        self.with_shown(row, |s| {
            matches!((s, s.cell(col)), (Shown::New(_), Some((None, _))))
        })
        .unwrap_or(false)
    }

    /// How staged changes mark a row.
    pub fn row_mark(&self, row: u64) -> RowMark {
        self.with_shown(row, |s| s.mark()).unwrap_or_default()
    }

    /// A loaded row as its staged changes refer to it.
    pub fn row_ref(&self, row: u64) -> Option<RowRef> {
        match self.cache_row(row) {
            None => Some(RowRef::New(row as usize)),
            Some(r) => self
                .cache
                .as_ref()?
                .with_row(r, |v| RowRef::Table(v.to_vec())),
        }
    }

    /// Shows a value the database accepted, without reloading.
    pub fn set_value(&mut self, row: u64, col: usize, value: Value, cx: &mut Context<Self>) {
        if let (Some(cache), Some(r)) = (&self.cache, self.cache_row(row)) {
            cache.set_cell(r, col, value);
        }
        self.texts.remove(&(row, col as u32));
        cx.notify();
    }

    /// Opens the editor on `cell`, its text selected so typing replaces it.
    pub fn begin_edit(&mut self, cell: (u64, usize), window: &mut Window, cx: &mut Context<Self>) {
        self.begin_edit_with(cell, None, window, cx);
    }

    /// Opens the editor on `cell`. With `typed`, the text starts as that instead of the
    /// value, as when typing on a selected cell.
    fn begin_edit_with(
        &mut self,
        cell: (u64, usize),
        typed: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editable || cell.1 >= self.columns.len() {
            return;
        }
        let Some(value) = self.value(cell.0, cell.1) else {
            return;
        };
        if self.row_mark(cell.0) == RowMark::Deleted {
            return;
        }
        let target = self.row_ref(cell.0);
        self.commit_edit(false, window, cx);
        self.context = None;
        let original = if value.is_null() {
            String::new()
        } else {
            value.to_string()
        };
        let placeholder = if self.is_default(cell.0, cell.1) {
            "DEFAULT"
        } else if value.is_null() {
            "NULL"
        } else {
            ""
        };
        let text = typed.map_or_else(|| original.clone(), str::to_owned);
        let select = typed.is_none();
        let input = cx.new(|cx| {
            let mut input = TextInput::new(placeholder, cx);
            input.set_text(text, cx);
            if select {
                input.select_everything(cx);
            }
            input
        });
        let focus = input.read(cx).focus_handle(cx);
        let subs = vec![
            cx.subscribe_in(&input, window, |this, _, e, window, cx| match e {
                // Enter saves and moves down, as in a spreadsheet.
                InputEvent::Submit => this.step(1, 0, false, window, cx),
                InputEvent::Cancel => this.cancel_edit(window, cx),
                InputEvent::Changed => {}
            }),
            // Clicking anywhere else saves, as Enter does.
            cx.on_blur(&focus, window, |this, window, cx| {
                this.commit_edit(false, window, cx)
            }),
        ];
        window.focus(&focus);
        self.selection = Some(Selection {
            anchor: cell,
            head: cell,
        });
        self.editor = Some(CellEditor {
            row: cell.0,
            col: cell.1,
            target,
            input,
            original,
            _subs: subs,
        });
        let t = theme(cx).clone();
        self.reveal(&t);
        cx.notify();
    }

    /// Closes the editor and reports the new text when it changed. `refocus` puts the
    /// keyboard back on the grid, after Enter; a click elsewhere keeps its own focus.
    fn commit_edit(&mut self, refocus: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.take() else {
            return;
        };
        let text = editor.input.read(cx).text().to_owned();
        if text != editor.original {
            cx.emit(GridEvent::Edit {
                row: editor.row,
                col: editor.col,
                input: CellInput::Text(text),
                target: editor.target,
            });
        }
        if refocus {
            window.focus(&self.focus);
        }
        cx.notify();
    }

    /// Closes the editor and hands back what it holds when that changed, for an owner
    /// that saves it before doing something else, such as committing.
    pub fn take_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(u64, usize, CellInput, Option<RowRef>)> {
        let editor = self.editor.take()?;
        window.focus(&self.focus);
        cx.notify();
        let text = editor.input.read(cx).text().to_owned();
        (text != editor.original).then_some((
            editor.row,
            editor.col,
            CellInput::Text(text),
            editor.target,
        ))
    }

    /// Puts the keyboard on the grid.
    pub fn focus(&self, window: &mut Window) {
        window.focus(&self.focus);
    }

    /// Selects one cell and scrolls to it.
    pub fn select_cell(&mut self, cell: (u64, usize), cx: &mut Context<Self>) {
        self.select(cell, false, cx);
        let t = theme(cx).clone();
        self.reveal(&t);
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.take().is_some() {
            window.focus(&self.focus);
            cx.notify();
        }
    }

    /// Sets the selected cells to NULL.
    pub fn set_null(&mut self, cx: &mut Context<Self>) {
        self.clear_cells(true, cx);
    }

    /// Clears the selected cells: NULL where the column takes it, otherwise empty text.
    /// `null_only` asks for NULL everywhere, which a column that can't be NULL refuses.
    fn clear_cells(&mut self, null_only: bool, cx: &mut Context<Self>) {
        let Some((rows, cols)) = self.selected_ranges() else {
            return;
        };
        if !self.editable || self.editor.is_some() {
            return;
        }
        let cols = cols.start..cols.end.min(self.columns.len());
        let all = (rows.end - rows.start) as usize;
        let mut cells = Vec::new();
        for row in rows.take(EDIT_ROWS) {
            for col in cols.clone() {
                let input = if null_only || self.columns[col].nullable != Some(false) {
                    CellInput::Null
                } else {
                    CellInput::Text(String::new())
                };
                cells.push((row, col, input));
            }
        }
        cx.emit(GridEvent::Edits {
            cells,
            batch: Batch::Clear,
            outside: 0,
            limited: all > EDIT_ROWS,
        });
    }

    /// Pastes tab-separated text from the clipboard, a spreadsheet's cells, starting
    /// at the selection's first cell. One value pasted on a selection fills all of it.
    /// An empty field is NULL where the column takes it, as a copy writes NULL.
    fn paste_cells(&mut self, cx: &mut Context<Self>) {
        let Some((rows, cols)) = self.selected_ranges() else {
            return;
        };
        if !self.editable || self.columns.is_empty() {
            return;
        }
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else {
            return;
        };
        let (grid, more) = tsv::decode(&text, EDIT_ROWS);
        if grid.is_empty() {
            return;
        }
        let total = self.total_rows();
        let n_cols = self.columns.len();
        let mut cells = Vec::new();
        let mut outside = 0;
        let mut limited = more;
        let input = |col: usize, text: &str| {
            if text.is_empty() && self.columns[col].nullable != Some(false) {
                CellInput::Null
            } else {
                CellInput::Text(text.to_owned())
            }
        };
        if let [row] = grid.as_slice()
            && let [one] = row.as_slice()
        {
            for r in rows.clone().take(EDIT_ROWS) {
                for c in cols.start..cols.end.min(n_cols) {
                    cells.push((r, c, input(c, one)));
                }
            }
            limited = rows.end - rows.start > EDIT_ROWS as u64;
        } else {
            for (i, fields) in grid.iter().enumerate() {
                let r = rows.start + i as u64;
                if r >= total {
                    outside += fields.len();
                    continue;
                }
                for (j, field) in fields.iter().enumerate() {
                    let c = cols.start + j;
                    if c < n_cols {
                        cells.push((r, c, input(c, field)));
                    } else {
                        outside += 1;
                    }
                }
            }
            // The pasted block becomes the selection, as in a spreadsheet.
            let last_row = (rows.start + grid.len() as u64)
                .min(total)
                .saturating_sub(1);
            let width = grid.iter().map(Vec::len).max().unwrap_or(1);
            let last_col = (cols.start + width).min(n_cols).saturating_sub(1);
            self.selection = Some(Selection {
                anchor: (rows.start, cols.start),
                head: (last_row.max(rows.start), last_col.max(cols.start)),
            });
        }
        cx.emit(GridEvent::Edits {
            cells,
            batch: Batch::Paste,
            outside,
            limited,
        });
        cx.notify();
    }

    /// Hooks for a cache this grid will show; redraws go to this grid.
    pub fn make_hooks(&mut self, cx: &mut Context<Self>) -> Hooks {
        let (hooks, pump) = hooks(cx);
        self._pump = Some(pump);
        hooks
    }

    /// Shows `cache`. `keep_position` keeps the scroll and widths, for a re-sort.
    pub fn set_cache(
        &mut self,
        cache: Option<Arc<GridCache>>,
        keep_position: bool,
        cx: &mut Context<Self>,
    ) {
        self.cache = cache;
        self.texts.clear();
        if !keep_position {
            self.top = 0.0;
            self.scroll_x = 0.0;
            self.selection = None;
            self.columns = Arc::from(Vec::new());
            self.widths.clear();
            self.fitted = false;
        }
        cx.notify();
    }

    pub fn cache(&self) -> Option<&Arc<GridCache>> {
        self.cache.as_ref()
    }

    /// Forgets formatted texts, after the data under them changed.
    pub fn refreshed(&mut self, cx: &mut Context<Self>) {
        self.texts.clear();
        cx.notify();
    }

    pub fn set_sort(&mut self, sort: Vec<(usize, SortDirection)>, cx: &mut Context<Self>) {
        self.sort = sort;
        cx.notify();
    }

    pub fn columns(&self) -> &Arc<[Column]> {
        &self.columns
    }

    /// The values of `row` as shown, staged changes in, when it is loaded.
    pub fn row_values(&self, row: u64) -> Option<Vec<Value>> {
        let n = self.columns.len();
        self.with_shown(row, |s| s.values(n)).flatten()
    }

    /// The table's rows, without staged new rows.
    pub fn row_count(&self) -> RowCount {
        self.cache
            .as_ref()
            .map_or(RowCount::Unknown, |c| c.row_count())
    }

    /// Grid rows: staged new rows, then the table's.
    fn total_rows(&self) -> u64 {
        self.inserted() + self.row_count().value().unwrap_or(0)
    }

    /// Drops formatted texts when the staged changes moved on since they were made.
    fn sync_pending(&mut self) {
        if self.pending.generation() != self.pending_seen {
            self.pending_seen = self.pending.generation();
            self.texts.clear();
        }
    }

    fn row_height(t: &Theme) -> f32 {
        f32::from(t.row_height)
    }

    fn body_size(&self) -> (f32, f32) {
        self.body.get().map_or((800.0, 480.0), |b| {
            (f32::from(b.size.width), f32::from(b.size.height))
        })
    }

    fn visible_rows(&self, t: &Theme) -> u64 {
        (self.body_size().1 / Self::row_height(t)).ceil().max(1.0) as u64
    }

    fn max_top(&self, t: &Theme) -> f64 {
        let fit = self.body_size().1 as f64 / Self::row_height(t) as f64;
        (self.total_rows() as f64 - fit + 0.25).max(0.0)
    }

    fn width(&self, c: usize, t: &Theme) -> f32 {
        self.widths.get(c).copied().unwrap_or(120.0) * t.scale
    }

    fn total_width(&self, t: &Theme) -> f32 {
        (0..self.columns.len()).map(|c| self.width(c, t)).sum()
    }

    fn max_scroll_x(&self, t: &Theme) -> f32 {
        (self.total_width(t) - self.body_size().0).max(0.0)
    }

    fn clamp(&mut self, t: &Theme) {
        self.top = self.top.clamp(0.0, self.max_top(t));
        self.scroll_x = self.scroll_x.clamp(0.0, self.max_scroll_x(t));
    }

    /// Takes up a new column list: widths from the types and names, until rows arrive.
    fn sync_columns(&mut self, t: &Theme) {
        let Some(columns) = self.cache.as_ref().and_then(|c| c.columns()) else {
            return;
        };
        if Arc::ptr_eq(&columns, &self.columns) {
            return;
        }
        let char_w = f32::from(t.mono_size) * 0.62 / t.scale;
        for c in columns.iter().skip(self.widths.len()) {
            let by_type: f32 = match c.kind {
                ValueKind::Bool => 70.0,
                ValueKind::Int => 90.0,
                ValueKind::Float | ValueKind::Decimal => 110.0,
                ValueKind::Date => 110.0,
                ValueKind::Time => 100.0,
                ValueKind::DateTime => 170.0,
                ValueKind::Uuid => 290.0,
                ValueKind::ObjectId => 220.0,
                ValueKind::Json | ValueKind::Array => 240.0,
                _ => 160.0,
            };
            let by_name = c.name.chars().count() as f32 * 7.4 + 36.0;
            self.widths
                .push(by_type.max(by_name).max(char_w * 4.0).min(420.0));
        }
        self.pending.set_columns(&columns);
        self.columns = columns;
        self.texts.clear();
    }

    /// Fits each column to the widest of its first loaded values, once.
    fn fit_columns(&mut self, t: &Theme, first: u64, n: u64) {
        let Some(cache) = self.cache.clone() else {
            return;
        };
        let first = first.saturating_sub(self.inserted());
        if self.fitted || self.columns.is_empty() || !cache.is_loaded(first) {
            return;
        }
        let char_w = f32::from(t.mono_size) * 0.62 / t.scale;
        let mut widest = vec![0usize; self.columns.len()];
        cache.with_rows(first..first + n.min(60), |_, row| {
            if let Some(row) = row {
                for (c, v) in row.iter().enumerate().take(widest.len()) {
                    let len = match v {
                        Value::Null => 4,
                        Value::Text(s) | Value::Json(s) => s.len().min(60),
                        Value::Int(i) => {
                            (i.unsigned_abs().checked_ilog10().unwrap_or(0) + 2) as usize
                        }
                        Value::Float(_) => 10,
                        Value::Bool(_) => 5,
                        Value::Bytes(b) => (b.len() * 2 + 2).min(40),
                    };
                    widest[c] = widest[c].max(len);
                }
            }
        });
        for (c, w) in widest.into_iter().enumerate() {
            let name = self.columns[c].name.chars().count() as f32 * 7.4 + 36.0;
            let fit = (w as f32 * char_w + 22.0)
                .max(name)
                .clamp(MIN_COLUMN, 360.0);
            self.widths[c] = fit;
        }
        self.fitted = true;
    }

    /// The row and column under `p`, a point in window coordinates.
    fn cell_at(&self, p: Point<Pixels>, t: &Theme) -> Option<(u64, usize)> {
        let body = self.body.get()?;
        let local = p - body.origin;
        let rh = Self::row_height(t) as f64;
        let y = f32::from(local.y) as f64 + (self.top - self.top.floor()) * rh;
        let row = self.top.floor() as u64 + (y / rh).floor().max(0.0) as u64;
        let total = self.total_rows();
        if total == 0 {
            return None;
        }
        let mut x = f32::from(local.x) + self.scroll_x;
        let mut col = None;
        for c in 0..self.columns.len() {
            let w = self.width(c, t);
            if x < w {
                col = Some(c);
                break;
            }
            x -= w;
        }
        Some((
            row.min(total - 1),
            col.unwrap_or(self.columns.len().saturating_sub(1)),
        ))
    }

    fn select(&mut self, cell: (u64, usize), extend: bool, cx: &mut Context<Self>) {
        let anchor = match (extend, self.selection) {
            (true, Some(s)) => s.anchor,
            _ => cell,
        };
        let changed_row = self.selection.map(|s| s.head.0) != Some(cell.0);
        self.selection = Some(Selection { anchor, head: cell });
        if changed_row || !extend {
            cx.emit(GridEvent::Selected(cell.0));
        }
        cx.notify();
    }

    /// Scrolls just enough to show the selection's head.
    fn reveal(&mut self, t: &Theme) {
        let Some(s) = self.selection else { return };
        let (row, col) = s.head;
        let fit = (self.body_size().1 / Self::row_height(t)).floor().max(1.0) as f64;
        if (row as f64) < self.top {
            self.top = row as f64;
        } else if row as f64 + 1.0 > self.top + fit {
            self.top = row as f64 + 1.0 - fit;
        }
        let x: f32 = (0..col).map(|c| self.width(c, t)).sum();
        let w = self.width(col, t);
        let view = self.body_size().0;
        if x < self.scroll_x {
            self.scroll_x = x;
        } else if x + w > self.scroll_x + view {
            self.scroll_x = x + w - view;
        }
        self.clamp(t);
    }

    /// Moves the selection's head by rows and columns, or extends the selection there.
    /// An open edit is saved first.
    fn step(
        &mut self,
        dr: i64,
        dc: i64,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_edit(true, window, cx);
        self.context = None;
        let total = self.total_rows();
        if total == 0 || self.columns.is_empty() {
            return;
        }
        let (row, col) = self.selection.map_or((0, 0), |s| s.head);
        let row = (row as i64 + dr).clamp(0, total as i64 - 1) as u64;
        let col = (col as i64 + dc).clamp(0, self.columns.len() as i64 - 1) as usize;
        self.select((row, col), extend, cx);
        let t = theme(cx).clone();
        self.reveal(&t);
    }

    /// Tab and Shift+Tab: saves an open edit and moves to the next or previous cell,
    /// round to the next or previous row at the end of one.
    fn tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let cols = self.columns.len();
        let Some((row, col)) = self.selection.map(|s| s.head) else {
            self.step(0, 0, false, window, cx);
            return;
        };
        let (dr, dc) = match (forward, col) {
            (true, c) if c + 1 >= cols => (1, -JUMP),
            (true, _) => (0, 1),
            (false, 0) => (-1, JUMP),
            (false, _) => (0, -1),
        };
        // At the very first or last cell there is nowhere to go round to.
        let total = self.total_rows();
        if (dr > 0 && row + 1 >= total) || (dr < 0 && row == 0) {
            self.step(0, 0, false, window, cx);
            return;
        }
        self.step(dr, dc, false, window, cx);
    }

    /// Typing a character on a selected cell edits it, starting with that character.
    fn on_key_down(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &e.keystroke.modifiers;
        if self.editor.is_some()
            || !self.editable
            || !self.focus.is_focused(window)
            || m.control
            || m.alt
            || m.platform
            || m.function
        {
            return;
        }
        let Some(typed) = e.keystroke.key_char.as_deref() else {
            return;
        };
        if typed.is_empty() || typed.chars().any(char::is_control) {
            return;
        }
        let Some(head) = self.selection.map(|s| s.head) else {
            return;
        };
        self.begin_edit_with(head, Some(typed), window, cx);
        if self.editor.is_some() {
            // Handled: the window must not type the character into the editor again.
            cx.stop_propagation();
        }
    }

    fn page(&self, cx: &App) -> i64 {
        self.visible_rows(theme(cx)).saturating_sub(1).max(1) as i64
    }

    /// The selected cells in one of the copy formats.
    fn copy_as(&mut self, format: CopyFormat, cx: &mut Context<Self>) {
        let (Some(s), Some(cache)) = (self.selection, self.cache.clone()) else {
            return;
        };
        let rows = s.rows();
        let rows = rows.start..rows.end.min(rows.start + COPY_LIMIT);
        let cols: Vec<usize> = s.cols().filter(|c| *c < self.columns.len()).collect();
        let names: Vec<&str> = cols
            .iter()
            .map(|c| self.columns[*c].name.as_str())
            .collect();
        let mut values: Vec<Vec<Value>> = Vec::new();
        each_row(Some(&*cache), &self.pending, rows, |_, row| {
            values.push(
                cols.iter()
                    .map(|c| {
                        row.cell(*c)
                            .and_then(|(v, _)| v.cloned())
                            .unwrap_or(Value::Null)
                    })
                    .collect(),
            );
        });
        let out = format_rows(format, &names, &values);
        cx.write_to_clipboard(ClipboardItem::new_string(out));
    }

    /// The selected cells as tab-separated text, rows on lines, a value with a tab, a
    /// line break or a quote in quotes as spreadsheets expect. NULL is an empty field.
    fn copy(&mut self, headers: bool, cx: &mut Context<Self>) {
        let (Some(s), Some(cache)) = (self.selection, self.cache.clone()) else {
            return;
        };
        let rows = s.rows();
        let rows = rows.start..rows.end.min(rows.start + COPY_LIMIT);
        let cols = s.cols();
        let mut out = String::new();
        if headers {
            let names = cols
                .clone()
                .filter_map(|c| self.columns.get(c).map(|c| c.name.as_str()));
            for (i, name) in names.enumerate() {
                if i > 0 {
                    out.push('\t');
                }
                tsv::push_field(&mut out, name);
            }
            out.push('\n');
        }
        each_row(Some(&*cache), &self.pending, rows, |_, row| {
            for (i, c) in cols.clone().enumerate() {
                if i > 0 {
                    out.push('\t');
                }
                if let Some((Some(v), _)) = row.cell(c)
                    && !v.is_null()
                {
                    tsv::push_field(&mut out, &v.to_string());
                }
            }
            out.push('\n');
        });
        if out.ends_with('\n') {
            out.pop();
        }
        cx.write_to_clipboard(ClipboardItem::new_string(out));
    }

    fn on_scroll(&mut self, e: &gpui::ScrollWheelEvent, cx: &mut Context<Self>) {
        let t = theme(cx).clone();
        let d = e.delta.pixel_delta(t.row_height);
        let (mut dx, mut dy) = (f32::from(d.x), f32::from(d.y));
        // A plain wheel with shift held scrolls sideways.
        if e.modifiers.shift && dx == 0.0 {
            dx = dy;
            dy = 0.0;
        }
        self.top -= dy as f64 / Self::row_height(&t) as f64;
        self.scroll_x -= dx;
        self.clamp(&t);
        cx.stop_propagation();
        cx.notify();
    }

    fn pointer_moved(&mut self, e: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.drag else { return };
        if e.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            return;
        }
        let t = theme(cx).clone();
        let Some(body) = self.body.get() else { return };
        match drag {
            PointerDrag::Select => {
                if let Some(cell) = self.cell_at(e.position, &t) {
                    self.select(cell, true, cx);
                    // Past the top or bottom, the grid scrolls toward the pointer.
                    if e.position.y < body.top() {
                        self.top -= 1.0;
                    } else if e.position.y > body.bottom() {
                        self.top += 1.0;
                    }
                    self.clamp(&t);
                }
            }
            PointerDrag::Column {
                index,
                start_x,
                start_width,
            } => {
                let dx = f32::from(e.position.x - start_x) / t.scale;
                if let Some(w) = self.widths.get_mut(index) {
                    *w = (start_width + dx).clamp(MIN_COLUMN, 2000.0);
                }
            }
            PointerDrag::VBar { grab } => {
                let track = f32::from(body.size.height) - SCROLLBAR;
                let thumb = self.thumb_len(
                    f32::from(body.size.height),
                    self.body_size().1 as f64 / Self::row_height(&t) as f64,
                    self.total_rows() as f64 + 0.25,
                );
                let y = f32::from(e.position.y - body.top()) - grab;
                let f = (y / (track - thumb).max(1.0)).clamp(0.0, 1.0) as f64;
                self.top = f * self.max_top(&t);
            }
            PointerDrag::HBar { grab } => {
                let track = f32::from(body.size.width) - SCROLLBAR;
                let thumb = self.thumb_len(
                    track,
                    self.body_size().0 as f64,
                    self.total_width(&t) as f64,
                );
                let x = f32::from(e.position.x - body.left()) - grab;
                let f = (x / (track - thumb).max(1.0)).clamp(0.0, 1.0);
                self.scroll_x = f * self.max_scroll_x(&t);
            }
        }
        cx.notify();
    }

    fn thumb_len(&self, track: f32, visible: f64, total: f64) -> f32 {
        if total <= 0.0 {
            return track;
        }
        ((visible / total) as f32 * track).clamp(24.0, track)
    }

    /// Follows drags past the grid's edges. See the GPUI notes in AGENTS.md.
    fn pointer_watch(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().downgrade();
        canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let moving = this.clone();
                window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Bubble {
                        moving
                            .update(cx, |g, cx| {
                                if g.drag.is_some() {
                                    g.pointer_moved(e, cx)
                                }
                            })
                            .ok();
                    }
                });
                let up = this.clone();
                window.on_mouse_event(move |e: &gpui::MouseUpEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Capture && e.button == MouseButton::Left {
                        up.update(cx, |g, _| g.drag = None).ok();
                    }
                });
            },
        )
        .absolute()
        .size_0()
    }

    /// The grid's own right-click items, then the owner's.
    fn menu_items(&self, target: MenuTarget, cx: &mut Context<Self>) -> Vec<MenuItem> {
        let weak = cx.entity().downgrade();
        let act = |f: fn(&mut GridView, &mut Window, &mut Context<GridView>)| {
            let w = weak.clone();
            move |window: &mut Window, cx: &mut App| {
                w.update(cx, |g, cx| f(g, window, cx)).ok();
            }
        };
        let mut items = Vec::new();
        match target {
            MenuTarget::Cell(_, col) => {
                let column = self.columns.get(col);
                let nullable = column.is_some_and(|c| c.nullable != Some(false));
                let several = self
                    .selection
                    .is_some_and(|s| s.rows().end - s.rows().start > 1 || s.cols().len() > 1);
                if self.editable {
                    items.push(
                        MenuItem::new(
                            "Edit cell",
                            act(|g, window, cx| {
                                if let Some(s) = g.selection {
                                    g.begin_edit(s.head, window, cx)
                                }
                            }),
                        )
                        .detail("Enter")
                        .enabled(!several),
                    );
                    items.push(
                        MenuItem::new(
                            if several {
                                "Set cells to NULL"
                            } else {
                                "Set to NULL"
                            },
                            act(|g, _, cx| g.set_null(cx)),
                        )
                        .enabled(nullable),
                    );
                    items.push(
                        MenuItem::new("Paste", act(|g, _, cx| g.paste_cells(cx))).detail("Ctrl+V"),
                    );
                    items.push(MenuItem::separator());
                }
                items.push(
                    MenuItem::new("Copy", act(|g, _, cx| g.copy(false, cx))).detail("Ctrl+C"),
                );
                items.push(
                    MenuItem::new("Copy with headers", act(|g, _, cx| g.copy(true, cx)))
                        .detail("Ctrl+Shift+C"),
                );
                items.push(MenuItem::new(
                    "Copy as CSV",
                    act(|g, _, cx| g.copy_as(CopyFormat::Csv, cx)),
                ));
                items.push(MenuItem::new(
                    "Copy as JSON",
                    act(|g, _, cx| g.copy_as(CopyFormat::Json, cx)),
                ));
                items.push(MenuItem::new(
                    "Copy as Markdown",
                    act(|g, _, cx| g.copy_as(CopyFormat::Markdown, cx)),
                ));
                items.push(MenuItem::separator());
                items.push(
                    MenuItem::new("Select all", act(|g, _, cx| g.select_all(cx))).detail("Ctrl+A"),
                );
            }
            MenuTarget::Header(col) => {
                if let Some(c) = self.columns.get(col) {
                    let name = c.name.clone();
                    items.push(MenuItem::new("Copy column name", move |_, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(name.clone()))
                    }));
                }
                let w = weak.clone();
                items.push(MenuItem::new("Select column", move |_, cx| {
                    w.update(cx, |g, cx| g.select_column(col, cx)).ok();
                }));
                let w = weak.clone();
                items.push(MenuItem::new("Fit column to its contents", move |_, cx| {
                    w.update(cx, |g, cx| g.fit_column(col, cx)).ok();
                }));
                items.push(MenuItem::new(
                    "Fit every column",
                    act(|g, _, cx| {
                        g.fitted = false;
                        cx.notify();
                    }),
                ));
            }
        }
        if let Some(provider) = self.menu_provider.clone() {
            let extra = provider(target, self, cx);
            if !extra.is_empty() {
                items.push(MenuItem::separator());
                items.extend(extra);
            }
        }
        items
    }

    fn select_column(&mut self, col: usize, cx: &mut Context<Self>) {
        let total = self.total_rows();
        if total > 0 {
            self.selection = Some(Selection {
                anchor: (0, col),
                head: (total - 1, col),
            });
            cx.notify();
        }
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        let total = self.total_rows();
        if total > 0 && !self.columns.is_empty() {
            self.selection = Some(Selection {
                anchor: (0, 0),
                head: (total - 1, self.columns.len() - 1),
            });
            cx.notify();
        }
    }

    /// Fits one column to the widest of the values on screen.
    fn fit_column(&mut self, col: usize, cx: &mut Context<Self>) {
        let (Some(cache), Some(column)) = (self.cache.clone(), self.columns.get(col)) else {
            return;
        };
        let t = theme(cx).clone();
        let char_w = f32::from(t.mono_size) * 0.62 / t.scale;
        let first = self.top.floor() as u64;
        let mut widest = column.name.chars().count();
        each_row(
            Some(&*cache),
            &self.pending,
            first..first + self.visible_rows(&t),
            |_, row| {
                if let Some((Some(v), _)) = row.cell(col) {
                    widest = widest.max(v.preview(120).chars().count());
                }
            },
        );
        if let Some(w) = self.widths.get_mut(col) {
            *w = (widest as f32 * char_w + 30.0).clamp(MIN_COLUMN, 900.0);
        }
        cx.notify();
    }

    fn header(
        &self,
        visible: &[(usize, f32, f32)],
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut row = div()
            .relative()
            .flex_none()
            .h(t.row_height + px(4.))
            .w_full()
            .overflow_hidden()
            .bg(t.grid_header)
            .border_b_1()
            .border_color(t.border);
        for &(c, x, w) in visible {
            let Some(column) = self.columns.get(c) else {
                continue;
            };
            let sorted = self.sort.iter().find(|(i, _)| *i == c).map(|(_, d)| *d);
            let numeric = column.kind.is_numeric();
            let start_width = self.widths.get(c).copied().unwrap_or(120.0);
            row = row.child(
                div()
                    .id(ElementId::NamedInteger("col".into(), c as u64))
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px(x))
                    .w(px(w))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .when(numeric, |el| el.flex_row_reverse())
                    .border_r_1()
                    .border_color(t.grid_line)
                    .text_size(t.small_size)
                    .font_weight(t.header_weight())
                    .text_color(t.text_muted)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.hover))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(GridEvent::Sort(c))))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| {
                            cx.stop_propagation();
                            this.context = Some((e.position, MenuTarget::Header(c)));
                            cx.notify();
                        }),
                    )
                    .when(column.primary_key.is_some(), |el| {
                        el.child(icon(Icon::Key).size(px(11.)).text_color(t.warning))
                    })
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(t.header_text(&column.name)),
                    )
                    .when_some(sorted, |el, d| {
                        el.child(
                            icon(if d == SortDirection::Ascending {
                                Icon::SortAsc
                            } else {
                                Icon::SortDesc
                            })
                            .size(px(11.))
                            .text_color(t.accent),
                        )
                    })
                    .child(
                        // The resize grip on the column's right edge.
                        div()
                            .id(ElementId::NamedInteger("resize".into(), c as u64))
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .right(px(-3.))
                            .w(px(7.))
                            .cursor_col_resize()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| {
                                    cx.stop_propagation();
                                    this.drag = Some(PointerDrag::Column {
                                        index: c,
                                        start_x: e.position.x,
                                        start_width,
                                    });
                                }),
                            )
                            .on_click(|_, _, cx| cx.stop_propagation()),
                    ),
            );
        }
        row
    }
}

/// The ways the right-click menu copies cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyFormat {
    Csv,
    Json,
    Markdown,
}

/// Rows of values as text in `format`, `names` naming the columns.
pub fn format_rows(format: CopyFormat, names: &[&str], rows: &[Vec<Value>]) -> String {
    match format {
        CopyFormat::Csv => {
            let field = |s: &str| {
                if s.contains([',', '"', '\n', '\r']) {
                    format!("\"{}\"", s.replace('"', "\"\""))
                } else {
                    s.to_owned()
                }
            };
            let mut out = names.iter().map(|n| field(n)).collect::<Vec<_>>().join(",");
            for row in rows {
                out.push('\n');
                let cells: Vec<String> = row
                    .iter()
                    .map(|v| {
                        if v.is_null() {
                            String::new()
                        } else {
                            field(&v.to_string())
                        }
                    })
                    .collect();
                out.push_str(&cells.join(","));
            }
            out
        }
        CopyFormat::Json => {
            let list: Vec<serde_json::Value> = rows
                .iter()
                .map(|row| {
                    let mut object = serde_json::Map::new();
                    for (name, v) in names.iter().zip(row) {
                        object.insert((*name).to_owned(), json_value(v));
                    }
                    serde_json::Value::Object(object)
                })
                .collect();
            let value = if list.len() == 1 {
                list.into_iter().next().unwrap_or_default()
            } else {
                serde_json::Value::Array(list)
            };
            serde_json::to_string_pretty(&value).unwrap_or_default()
        }
        CopyFormat::Markdown => {
            let cell = |s: &str| s.replace('|', "\\|").replace(['\n', '\r'], " ");
            let mut out = format!(
                "| {} |\n|{}|",
                names
                    .iter()
                    .map(|n| cell(n))
                    .collect::<Vec<_>>()
                    .join(" | "),
                names.iter().map(|_| " --- ").collect::<Vec<_>>().join("|")
            );
            for row in rows {
                let cells: Vec<String> = row.iter().map(|v| cell(&v.to_string())).collect();
                out.push_str(&format!("\n| {} |", cells.join(" | ")));
            }
            out
        }
    }
}

fn json_value(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::from(*i),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or_else(
            || serde_json::Value::String(f.to_string()),
            serde_json::Value::Number,
        ),
        Value::Json(s) => serde_json::from_str(s).unwrap_or_else(|_| s.to_string().into()),
        Value::Text(s) => serde_json::Value::String(s.to_string()),
        Value::Bytes(_) => serde_json::Value::String(v.to_string()),
    }
}

/// A cell's text and how it is drawn. Dates follow the date format setting.
fn format_cell(
    value: &Value,
    kind: ValueKind,
    null_text: &'static str,
    dates: DateFormat,
) -> (SharedString, CellKind) {
    match value {
        Value::Null => (null_text.into(), CellKind::Null),
        Value::Bool(b) => ((if *b { "true" } else { "false" }).into(), CellKind::Bool),
        Value::Int(i) => (i.to_string().into(), CellKind::Number),
        Value::Float(f) => (f.to_string().into(), CellKind::Number),
        Value::Text(s) if kind == ValueKind::DateTime => {
            (format_datetime(s, dates).into(), CellKind::Plain)
        }
        Value::Text(_) if kind.is_numeric() => (value.preview(120).into(), CellKind::Number),
        _ => (value.preview(200).into(), CellKind::Plain),
    }
}

fn format_datetime(s: &str, format: DateFormat) -> String {
    match format {
        DateFormat::Raw => s.to_owned(),
        DateFormat::Iso => {
            let mut out: String = s.replacen('T', " ", 1);
            if out.len() >= 19 && out.is_char_boundary(19) {
                out.truncate(19);
            }
            out
        }
        DateFormat::Rfc3339 => {
            let mut out = s.replacen(' ', "T", 1);
            let has_zone = out.len() > 19 && out[19..].contains(['Z', '+', '-']);
            if !has_zone {
                out.push('Z');
            }
            out
        }
    }
}

impl Focusable for GridView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

struct PaintRow {
    index: u64,
    y: f32,
    mark: RowMark,
    /// Per visible column: the text, its style, and whether it is a staged value.
    cells: Option<Vec<(SharedString, CellKind, bool)>>,
}

const DEFAULT_TEXT: SharedString = SharedString::new_static("DEFAULT");

/// The text and style of one cell, formatted once and kept.
fn cell_text(
    texts: &mut HashMap<(u64, u32), (SharedString, CellKind)>,
    columns: &[Column],
    row: u64,
    col: usize,
    value: &Value,
    t: &Theme,
) -> (SharedString, CellKind) {
    if let Some(v) = texts.get(&(row, col as u32)) {
        return v.clone();
    }
    if texts.len() >= TEXT_CACHE_LIMIT {
        texts.clear();
    }
    let kind = columns.get(col).map_or(ValueKind::Text, |c| c.kind);
    let out = format_cell(value, kind, t.null_text, t.date_format);
    texts.insert((row, col as u32), out.clone());
    out
}

impl Render for GridView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        self.sync_columns(&t);
        self.sync_pending();
        self.clamp(&t);
        let rh = Self::row_height(&t);
        let total = self.total_rows();
        let first = self.top.floor() as u64;
        let offset = ((self.top - first as f64) * rh as f64) as f32;
        let wanted = self.visible_rows(&t) + 1;
        let count = wanted.min(total.saturating_sub(first));
        if let Some(cache) = &self.cache {
            // Past a count that is not exact (a result still streaming in) the rows may
            // exist, so ask for a full screen and let the loader find out.
            let exact = matches!(cache.row_count(), RowCount::Exact(_));
            let n = self.pending.inserts().len() as u64;
            let end = first + if exact { count } else { wanted };
            cache.set_viewport(first.saturating_sub(n)..end.saturating_sub(n));
        }
        self.fit_columns(&t, first, count);

        // Visible columns: (index, x from the body's left, width).
        let (view_w, _) = self.body_size();
        let mut visible = Vec::new();
        let mut x = -self.scroll_x;
        for c in 0..self.columns.len() {
            let w = self.width(c, &t);
            if x + w > 0.0 && x < view_w {
                visible.push((c, x, w));
            }
            x += w;
            if x >= view_w {
                break;
            }
        }
        let content_right = x.min(view_w);

        let mut rows: Vec<PaintRow> = Vec::with_capacity(count as usize);
        {
            // Only the visible columns are looked at, and a cell already formatted is
            // reused, so nothing is copied out of the cache. Staged changes are found by
            // the row's key without copying it.
            let (texts, columns) = (&mut self.texts, &self.columns);
            each_row(
                self.cache.as_deref(),
                &self.pending,
                first..first + count,
                |i, shown| {
                    let cells = (!matches!(shown, Shown::Loading)).then(|| {
                        visible
                            .iter()
                            .map(|(c, _, _)| match shown.cell(*c) {
                                Some((Some(v), staged)) => {
                                    let (text, kind) = cell_text(texts, columns, i, *c, v, &t);
                                    (text, kind, staged)
                                }
                                Some((None, _)) => (DEFAULT_TEXT, CellKind::Null, false),
                                None => (SharedString::default(), CellKind::Plain, false),
                            })
                            .collect()
                    });
                    rows.push(PaintRow {
                        index: i,
                        y: (i - first) as f32 * rh - offset,
                        mark: shown.mark(),
                        cells,
                    });
                },
            );
        }

        let status = self.cache.as_ref().map(|c| c.status());
        let selection = self.selection;
        let body_bounds = self.body.clone();
        let paint_t = t.clone();
        let visible_paint = visible.clone();
        let body = canvas(
            move |bounds, _, _| {
                body_bounds.set(Some(bounds));
            },
            move |bounds, _, window, cx| {
                let t = paint_t;
                let origin = bounds.origin;
                let font = gpui::font(t.grid_font.clone());
                let font_size = t.mono_size;
                let line_height = px(rh);
                window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
                    let right = origin.x + px(content_right);
                    for r in &rows {
                        let top = origin.y + px(r.y);
                        let row_bounds = Bounds {
                            origin: point(origin.x, top),
                            size: size(px(content_right), px(rh)),
                        };
                        if t.zebra && r.index % 2 == 1 {
                            window.paint_quad(fill(row_bounds, t.surface_alt));
                        }
                        let deleted = r.mark == RowMark::Deleted;
                        match r.mark {
                            RowMark::Deleted => {
                                window.paint_quad(fill(row_bounds, t.staged_delete))
                            }
                            RowMark::Inserted => {
                                window.paint_quad(fill(row_bounds, t.staged_insert))
                            }
                            _ => {}
                        }
                        if selection.is_some_and(|s| s.rows().contains(&r.index)) {
                            window.paint_quad(fill(row_bounds, t.selected_row));
                        }
                        for (n, &(c, x, w)) in visible_paint.iter().enumerate() {
                            let cell = Bounds {
                                origin: point(origin.x + px(x), top),
                                size: size(px(w), px(rh)),
                            };
                            let staged = r.mark == RowMark::Changed
                                && r.cells
                                    .as_ref()
                                    .and_then(|cells| cells.get(n))
                                    .is_some_and(|(_, _, staged)| *staged);
                            if staged {
                                window.paint_quad(fill(cell, t.staged_change));
                            }
                            if selection.is_some_and(|s| {
                                s.rows().contains(&r.index) && s.cols().contains(&c)
                            }) {
                                window.paint_quad(fill(cell, t.selection));
                            }
                            match &r.cells {
                                Some(cells) => {
                                    let Some((text, kind, _)) = cells.get(n) else {
                                        continue;
                                    };
                                    if text.is_empty() {
                                        continue;
                                    }
                                    let color: Hsla = match kind {
                                        _ if deleted => t.text_faint,
                                        CellKind::Null => t.text_faint,
                                        CellKind::Bool => t.syntax_keyword,
                                        _ => t.text,
                                    };
                                    let run = TextRun {
                                        len: text.len(),
                                        font: font.clone(),
                                        color,
                                        background_color: None,
                                        underline: None,
                                        strikethrough: deleted.then_some(
                                            gpui::StrikethroughStyle {
                                                thickness: px(1.),
                                                color: Some(t.text_faint),
                                            },
                                        ),
                                    };
                                    let line = window.text_system().shape_line(
                                        text.clone(),
                                        font_size,
                                        &[run],
                                        None,
                                    );
                                    let pad = px(8.);
                                    let tx = if *kind == CellKind::Number {
                                        cell.right() - pad - line.width
                                    } else {
                                        cell.left() + pad
                                    };
                                    let clip = Bounds {
                                        origin: point(cell.left() + px(4.), cell.top()),
                                        size: size(cell.size.width - px(8.), cell.size.height),
                                    };
                                    window.with_content_mask(
                                        Some(gpui::ContentMask { bounds: clip }),
                                        |window| {
                                            let _ = line.paint(
                                                point(tx.max(cell.left() + pad), top),
                                                line_height,
                                                window,
                                                cx,
                                            );
                                        },
                                    );
                                }
                                None => {
                                    // A row on its way: a quiet bar where its text will be.
                                    let bar_w = (w * 0.55).clamp(12.0, 120.0);
                                    window.paint_quad(
                                        fill(
                                            Bounds {
                                                origin: point(
                                                    cell.left() + px(8.),
                                                    top + px(rh * 0.5 - 3.),
                                                ),
                                                size: size(px(bar_w), px(6.)),
                                            },
                                            t.hover,
                                        )
                                        .corner_radii(px(3.)),
                                    );
                                }
                            }
                        }
                        // A staged row carries a bar at its left edge, in its color.
                        let bar = match r.mark {
                            RowMark::Changed => Some(t.warning),
                            RowMark::Deleted => Some(t.danger),
                            RowMark::Inserted => Some(t.success),
                            RowMark::None => None,
                        };
                        if let Some(color) = bar {
                            window.paint_quad(fill(
                                Bounds {
                                    origin: point(origin.x, top),
                                    size: size(px(2.), px(rh)),
                                },
                                color,
                            ));
                        }
                        if t.grid_lines {
                            window.paint_quad(fill(
                                Bounds {
                                    origin: point(origin.x, top + px(rh) - px(1.)),
                                    size: size(right - origin.x, px(1.)),
                                },
                                t.grid_line,
                            ));
                        }
                    }
                    if t.grid_lines {
                        let bottom = rows.last().map_or(origin.y, |r| origin.y + px(r.y + rh));
                        for &(_, x, w) in &visible_paint {
                            window.paint_quad(fill(
                                Bounds {
                                    origin: point(origin.x + px(x + w) - px(1.), origin.y),
                                    size: size(px(1.), bottom - origin.y),
                                },
                                t.grid_line,
                            ));
                        }
                    }
                    // The active cell.
                    if let Some(s) = selection
                        && let Some(r) = rows.iter().find(|r| r.index == s.head.0)
                        && let Some(&(_, x, w)) =
                            visible_paint.iter().find(|(c, _, _)| *c == s.head.1)
                    {
                        window.paint_quad(outline(
                            Bounds {
                                origin: point(origin.x + px(x), origin.y + px(r.y)),
                                size: size(px(w) - px(1.), px(rh) - px(1.)),
                            },
                            t.accent,
                            BorderStyle::Solid,
                        ));
                    }
                });
            },
        )
        .size_full();

        // Scrollbars, drawn here because the content height can exceed what f32 pixels hold.
        let (bw, bh) = self.body_size();
        let fit_rows = bh as f64 / rh as f64;
        let rows_total = total as f64 + 0.25;
        let vbar = (rows_total > fit_rows).then(|| {
            let track = bh - SCROLLBAR;
            let thumb = self.thumb_len(track, fit_rows, rows_total);
            let max = self.max_top(&t).max(1e-9);
            let y = ((self.top / max) as f32).clamp(0.0, 1.0) * (track - thumb);
            div()
                .id("vbar")
                .absolute()
                .top_0()
                .right_0()
                .bottom(px(SCROLLBAR))
                .w(px(SCROLLBAR))
                .child(
                    div()
                        .absolute()
                        .top(px(y))
                        .right(px(2.))
                        .w(px(6.))
                        .h(px(thumb))
                        .rounded_full()
                        .bg(t.text_faint)
                        .opacity(0.55)
                        .hover(|s| s.opacity(0.85)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        let Some(body) = this.body.get() else { return };
                        let local = f32::from(e.position.y - body.top());
                        let grab = if local >= y && local <= y + thumb {
                            local - y
                        } else {
                            thumb / 2.0
                        };
                        this.drag = Some(PointerDrag::VBar { grab });
                        this.pointer_moved(
                            &gpui::MouseMoveEvent {
                                position: e.position,
                                pressed_button: Some(MouseButton::Left),
                                modifiers: e.modifiers,
                            },
                            cx,
                        );
                    }),
                )
        });
        let total_w = self.total_width(&t);
        let hbar = (total_w > bw + 0.5).then(|| {
            let track = bw - SCROLLBAR;
            let thumb = self.thumb_len(track, bw as f64, total_w as f64);
            let max = self.max_scroll_x(&t).max(1e-6);
            let x = (self.scroll_x / max).clamp(0.0, 1.0) * (track - thumb);
            div()
                .id("hbar")
                .absolute()
                .left_0()
                .bottom_0()
                .right(px(SCROLLBAR))
                .h(px(SCROLLBAR))
                .child(
                    div()
                        .absolute()
                        .left(px(x))
                        .bottom(px(2.))
                        .h(px(6.))
                        .w(px(thumb))
                        .rounded_full()
                        .bg(t.text_faint)
                        .opacity(0.55)
                        .hover(|s| s.opacity(0.85)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        let Some(body) = this.body.get() else { return };
                        let local = f32::from(e.position.x - body.left());
                        let grab = if local >= x && local <= x + thumb {
                            local - x
                        } else {
                            thumb / 2.0
                        };
                        this.drag = Some(PointerDrag::HBar { grab });
                        this.pointer_moved(
                            &gpui::MouseMoveEvent {
                                position: e.position,
                                pressed_button: Some(MouseButton::Left),
                                modifiers: e.modifiers,
                            },
                            cx,
                        );
                    }),
                )
        });

        let overlay = if let Some(msg) = self.message.clone().filter(|_| self.columns.is_empty()) {
            Some(msg)
        } else if self.columns.is_empty() {
            match &status {
                Some(Status::Error(e)) => Some(e.clone().into()),
                Some(_) => Some("Loading…".into()),
                None => None,
            }
        } else if matches!(self.row_count(), RowCount::Exact(0)) && self.inserted() == 0 {
            Some("No rows".into())
        } else {
            None
        };
        let error = match &status {
            Some(Status::Error(e)) if !self.columns.is_empty() => {
                Some(SharedString::from(e.clone()))
            }
            _ => None,
        };
        let _ = window;
        let editor = self.editor.as_ref().map(|ed| {
            let y = ((ed.row as f64 - self.top) * rh as f64) as f32;
            let x: f32 = (0..ed.col).map(|c| self.width(c, &t)).sum::<f32>() - self.scroll_x;
            let w = self.width(ed.col, &t).max(120.0);
            div()
                .id("cell-editor")
                .occlude()
                .absolute()
                .left(px(x))
                .top(px(y - 2.0))
                .w(px(w))
                .h(px(rh + 4.0))
                .px(px(7.))
                .flex()
                .items_center()
                .bg(t.input)
                .border_2()
                .border_color(t.accent)
                .rounded(t.radius_small)
                .shadow_md()
                .font_family(t.grid_font.clone())
                .text_size(t.mono_size)
                .text_color(t.text)
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation()
                })
                .child(ed.input.clone())
        });
        let menu = self.context.map(|(position, target)| {
            let items = self.menu_items(target, cx);
            let weak = cx.entity().downgrade();
            context_menu(
                "grid-menu",
                items,
                position,
                move |_, cx| {
                    weak.update(cx, |g, cx| {
                        g.context = None;
                        cx.notify();
                    })
                    .ok();
                },
                &t,
            )
        });
        div()
            .id("grid")
            .key_context("Grid")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(t.surface)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(
                cx.listener(|this, _: &MoveUp, window, cx| this.step(-1, 0, false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveDown, window, cx| this.step(1, 0, false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveLeft, window, cx| this.step(0, -1, false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &MoveRight, window, cx| this.step(0, 1, false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectUp, window, cx| this.step(-1, 0, true, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectDown, window, cx| this.step(1, 0, true, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectLeft, window, cx| this.step(0, -1, true, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectRight, window, cx| this.step(0, 1, true, window, cx)),
            )
            .on_action(cx.listener(|this, _: &PageUp, window, cx| {
                let n = this.page(cx);
                this.step(-n, 0, false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PageDown, window, cx| {
                let n = this.page(cx);
                this.step(n, 0, false, window, cx)
            }))
            // Ctrl+Home and Ctrl+End go to the first and the last cell, as in spreadsheets.
            .on_action(
                cx.listener(|this, _: &GoTop, window, cx| {
                    this.step(-JUMP, -JUMP, false, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &GoBottom, window, cx| {
                this.step(JUMP, JUMP, false, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &RowStart, window, cx| {
                    this.step(0, -JUMP, false, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &RowEnd, window, cx| this.step(0, JUMP, false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &JumpUp, window, cx| this.step(-JUMP, 0, false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &JumpDown, window, cx| this.step(JUMP, 0, false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &JumpLeft, window, cx| {
                    this.step(0, -JUMP, false, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &JumpRight, window, cx| {
                    this.step(0, JUMP, false, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &NextCell, window, cx| this.tab(true, window, cx)))
            .on_action(cx.listener(|this, _: &ClearCells, _, cx| this.clear_cells(false, cx)))
            .on_action(cx.listener(|this, _: &PasteCells, _, cx| this.paste_cells(cx)))
            .on_action(
                cx.listener(|this, _: &PreviousCell, window, cx| this.tab(false, window, cx)),
            )
            .on_action(cx.listener(|this, _: &CopyCells, _, cx| this.copy(false, cx)))
            .on_action(cx.listener(|this, _: &CopyWithHeaders, _, cx| this.copy(true, cx)))
            .on_action(cx.listener(|this, _: &SelectAllCells, _, cx| this.select_all(cx)))
            .on_action(cx.listener(|this, _: &EditCell, window, cx| {
                if let Some(s) = this.selection {
                    this.begin_edit(s.head, window, cx);
                }
            }))
            .child(self.header(&visible, &t, cx))
            .child(
                div()
                    .id("grid-body")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .on_scroll_wheel(
                        cx.listener(|this, e: &gpui::ScrollWheelEvent, _, cx| {
                            this.on_scroll(e, cx)
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, e: &gpui::MouseDownEvent, window, cx| {
                            this.context = None;
                            let t = theme(cx).clone();
                            let Some(cell) = this.cell_at(e.position, &t) else {
                                window.focus(&this.focus);
                                return;
                            };
                            if e.click_count >= 2 && this.editable && !e.modifiers.shift {
                                this.drag = None;
                                this.begin_edit(cell, window, cx);
                                // Keeps the grid, which tracks focus, from taking the
                                // keyboard back from the editor on this same press.
                                window.prevent_default();
                                return;
                            }
                            // A press elsewhere saves an open edit first, through its blur.
                            window.focus(&this.focus);
                            this.drag = Some(PointerDrag::Select);
                            this.select(cell, e.modifiers.shift, cx);
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, e: &gpui::MouseDownEvent, window, cx| {
                            let t = theme(cx).clone();
                            let Some(cell) = this.cell_at(e.position, &t) else {
                                return;
                            };
                            window.focus(&this.focus);
                            // A right click inside the selection keeps it, so a menu can act
                            // on many cells; outside it selects the cell under the pointer.
                            let inside = this.selection.is_some_and(|s| {
                                s.rows().contains(&cell.0) && s.cols().contains(&cell.1)
                            });
                            if !inside {
                                this.select(cell, false, cx);
                            }
                            this.context = Some((e.position, MenuTarget::Cell(cell.0, cell.1)));
                            cx.notify();
                        }),
                    )
                    .child(body)
                    .children(vbar)
                    .children(hbar)
                    .children(editor)
                    .children(overlay.map(|m| {
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .p(px(24.))
                            .text_size(t.ui_size)
                            .text_color(t.text_muted)
                            .child(m)
                    }))
                    .children(error.map(|e| {
                        div()
                            .absolute()
                            .left(px(12.))
                            .right(px(12.))
                            .bottom(px(16.))
                            .p(px(8.))
                            .rounded(t.radius_small)
                            .bg(t.floating)
                            .border_1()
                            .border_color(t.danger)
                            .text_size(t.small_size)
                            .text_color(t.danger)
                            .child(e)
                    })),
            )
            .child(self.pointer_watch(cx))
            .children(menu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_ranges_are_ordered() {
        let s = Selection {
            anchor: (10, 4),
            head: (2, 1),
        };
        assert_eq!(s.rows(), 2..11);
        assert_eq!(s.cols(), 1..5);
    }

    #[test]
    fn dates_follow_the_setting() {
        assert_eq!(
            format_datetime("2024-03-09T14:05:00.123Z", DateFormat::Iso),
            "2024-03-09 14:05:00"
        );
        assert_eq!(
            format_datetime("2024-03-09 14:05:00", DateFormat::Rfc3339),
            "2024-03-09T14:05:00Z"
        );
        assert_eq!(
            format_datetime("2024-03-09 14:05:00+02", DateFormat::Rfc3339),
            "2024-03-09T14:05:00+02"
        );
        assert_eq!(format_datetime("x", DateFormat::Raw), "x");
        let (text, kind) = format_cell(&Value::Null, ValueKind::Int, "NULL", DateFormat::Raw);
        assert_eq!((text.as_ref(), kind), ("NULL", CellKind::Null));
        let (_, kind) = format_cell(
            &Value::text("12.50"),
            ValueKind::Decimal,
            "NULL",
            DateFormat::Raw,
        );
        assert_eq!(kind, CellKind::Number);
    }
}
