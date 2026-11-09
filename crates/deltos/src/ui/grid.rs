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
    App, BorderStyle, Bounds, ClipboardItem, Context, ElementId, EventEmitter, FocusHandle,
    Focusable, FontWeight, Hsla, IntoElement, KeyBinding, MouseButton, Pixels, Point, Render,
    SharedString, Task, TextRun, Window, actions, canvas, div, fill, outline, point, prelude::*,
    px, size,
};

use crate::net::Net;
use crate::theme::{Theme, theme};
use crate::ui::icons::{Icon, icon};

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
    ]);
}

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
            _pump: None,
        }
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

    /// The values of `row`, when it is loaded.
    pub fn row_values(&self, row: u64) -> Option<Vec<Value>> {
        self.cache.as_ref()?.with_row(row, |r| r.to_vec())
    }

    pub fn row_count(&self) -> RowCount {
        self.cache
            .as_ref()
            .map_or(RowCount::Unknown, |c| c.row_count())
    }

    fn total_rows(&self) -> u64 {
        self.row_count().value().unwrap_or(0)
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
        self.columns = columns;
        self.texts.clear();
    }

    /// Fits each column to the widest of its first loaded values, once.
    fn fit_columns(&mut self, t: &Theme, first: u64, n: u64) {
        let Some(cache) = self.cache.clone() else {
            return;
        };
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

    /// The text and style of one cell, formatted once and kept.
    fn cell_text(
        &mut self,
        row: u64,
        col: usize,
        value: &Value,
        t: &Theme,
    ) -> (SharedString, CellKind) {
        if let Some(v) = self.texts.get(&(row, col as u32)) {
            return v.clone();
        }
        if self.texts.len() >= TEXT_CACHE_LIMIT {
            self.texts.clear();
        }
        let kind = self.columns.get(col).map_or(ValueKind::Text, |c| c.kind);
        let out = format_cell(value, kind, t.null_text, t.date_format);
        self.texts.insert((row, col as u32), out.clone());
        out
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

    fn step(&mut self, dr: i64, dc: i64, extend: bool, cx: &mut Context<Self>) {
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

    fn page(&self, cx: &App) -> i64 {
        self.visible_rows(theme(cx)).saturating_sub(1).max(1) as i64
    }

    /// The selected cells as tab-separated text, rows on lines.
    fn copy(&mut self, headers: bool, cx: &mut Context<Self>) {
        let (Some(s), Some(cache)) = (self.selection, self.cache.clone()) else {
            return;
        };
        let rows = s.rows();
        let rows = rows.start..rows.end.min(rows.start + COPY_LIMIT);
        let cols = s.cols();
        let mut out = String::new();
        if headers {
            let names: Vec<&str> = cols
                .clone()
                .filter_map(|c| self.columns.get(c).map(|c| c.name.as_str()))
                .collect();
            out.push_str(&names.join("\t"));
            out.push('\n');
        }
        cache.with_rows(rows, |_, row| {
            for (i, c) in cols.clone().enumerate() {
                if i > 0 {
                    out.push('\t');
                }
                if let Some(v) = row.and_then(|r| r.get(c))
                    && !v.is_null()
                {
                    let text = v.to_string();
                    out.extend(text.chars().map(|ch| {
                        if ch == '\t' || ch == '\n' || ch == '\r' {
                            ' '
                        } else {
                            ch
                        }
                    }));
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
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.text_muted)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.hover))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(GridEvent::Sort(c))))
                    .when(column.primary_key.is_some(), |el| {
                        el.child(icon(Icon::Key).size(px(11.)).text_color(t.warning))
                    })
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(SharedString::from(column.name.clone())),
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
    cells: Option<Vec<(SharedString, CellKind)>>,
}

impl Render for GridView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        self.sync_columns(&t);
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
            cache.set_viewport(first..first + if exact { count } else { wanted });
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
        if let Some(cache) = self.cache.clone() {
            // Only the visible columns are looked at, and a cell already formatted is
            // reused, so nothing is copied out of the cache.
            cache.with_rows(first..first + count, |i, row| {
                let cells = row.map(|r| {
                    visible
                        .iter()
                        .map(|(c, _, _)| match r.get(*c) {
                            Some(v) => self.cell_text(i, *c, v, &t),
                            None => (SharedString::default(), CellKind::Plain),
                        })
                        .collect()
                });
                rows.push(PaintRow {
                    index: i,
                    y: (i - first) as f32 * rh - offset,
                    cells,
                });
            });
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
                let font = gpui::font(t.mono_font.clone());
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
                        if selection.is_some_and(|s| s.rows().contains(&r.index)) {
                            window.paint_quad(fill(row_bounds, t.selected_row));
                        }
                        for (n, &(c, x, w)) in visible_paint.iter().enumerate() {
                            let cell = Bounds {
                                origin: point(origin.x + px(x), top),
                                size: size(px(w), px(rh)),
                            };
                            if selection.is_some_and(|s| {
                                s.rows().contains(&r.index) && s.cols().contains(&c)
                            }) {
                                window.paint_quad(fill(cell, t.selection));
                            }
                            match &r.cells {
                                Some(cells) => {
                                    let Some((text, kind)) = cells.get(n) else {
                                        continue;
                                    };
                                    if text.is_empty() {
                                        continue;
                                    }
                                    let color: Hsla = match kind {
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
                                        strikethrough: None,
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
        } else if matches!(self.row_count(), RowCount::Exact(0)) {
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
        div()
            .id("grid")
            .key_context("Grid")
            .track_focus(&self.focus)
            .size_full()
            .flex()
            .flex_col()
            .bg(t.surface)
            .on_action(cx.listener(|this, _: &MoveUp, _, cx| this.step(-1, 0, false, cx)))
            .on_action(cx.listener(|this, _: &MoveDown, _, cx| this.step(1, 0, false, cx)))
            .on_action(cx.listener(|this, _: &MoveLeft, _, cx| this.step(0, -1, false, cx)))
            .on_action(cx.listener(|this, _: &MoveRight, _, cx| this.step(0, 1, false, cx)))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| this.step(-1, 0, true, cx)))
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| this.step(1, 0, true, cx)))
            .on_action(cx.listener(|this, _: &SelectLeft, _, cx| this.step(0, -1, true, cx)))
            .on_action(cx.listener(|this, _: &SelectRight, _, cx| this.step(0, 1, true, cx)))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| {
                let n = this.page(cx);
                this.step(-n, 0, false, cx)
            }))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| {
                let n = this.page(cx);
                this.step(n, 0, false, cx)
            }))
            .on_action(cx.listener(|this, _: &GoTop, _, cx| this.step(i64::MIN / 2, 0, false, cx)))
            .on_action(
                cx.listener(|this, _: &GoBottom, _, cx| this.step(i64::MAX / 2, 0, false, cx)),
            )
            .on_action(cx.listener(|this, _: &RowStart, _, cx| this.step(0, -(1 << 20), false, cx)))
            .on_action(cx.listener(|this, _: &RowEnd, _, cx| this.step(0, 1 << 20, false, cx)))
            .on_action(cx.listener(|this, _: &CopyCells, _, cx| this.copy(false, cx)))
            .on_action(cx.listener(|this, _: &CopyWithHeaders, _, cx| this.copy(true, cx)))
            .on_action(cx.listener(|this, _: &SelectAllCells, _, cx| {
                let total = this.total_rows();
                if total > 0 && !this.columns.is_empty() {
                    this.selection = Some(Selection {
                        anchor: (0, 0),
                        head: (total - 1, this.columns.len() - 1),
                    });
                    cx.notify();
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
                            window.focus(&this.focus);
                            let t = theme(cx).clone();
                            if let Some(cell) = this.cell_at(e.position, &t) {
                                this.drag = Some(PointerDrag::Select);
                                this.select(cell, e.modifiers.shift, cx);
                            }
                        }),
                    )
                    .child(body)
                    .children(vbar)
                    .children(hbar)
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
