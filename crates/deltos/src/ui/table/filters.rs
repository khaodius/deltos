//! The filter bar of a table view. Each row is a column, an operator and a value, and
//! the rows are joined with AND, or with OR when the person picks "Any". A second mode
//! takes a condition in the database's own language instead: a `WHERE` condition for SQL
//! databases, a filter document for MongoDB. Under the rows, a muted line shows the
//! `WHERE` clause (or the filter document) that runs, and "Copy as SQL" copies the whole
//! statement. Values always go to the database as bound parameters; the preview writes
//! them as escaped literals only for reading and copying.

use deltos_data::{
    BrowseRequest, Column, DriverKind, Filter, FilterOp, ObjectRef, Value, ValueKind,
};
use deltos_drivers::sql::{self, Dialect};
use gpui::{
    AnyElement, App, AppContext, ClipboardItem, Context, Entity, Focusable, SharedString,
    Subscription, Window, div, prelude::*, px,
};

use super::TablePanel;
use super::memory::{SavedCondition, SavedView};
use crate::theme::Theme;
use crate::ui::edit;
use crate::ui::icons::{Icon, icon};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{ButtonStyle, MenuItem, Tooltip, button, field, icon_button, menu, segmented};

/// The operators in the order the menu lists them, in groups.
const OP_GROUPS: [&[FilterOp]; 4] = [
    &[
        FilterOp::Eq,
        FilterOp::NotEq,
        FilterOp::Lt,
        FilterOp::LtEq,
        FilterOp::Gt,
        FilterOp::GtEq,
    ],
    &[
        FilterOp::Contains,
        FilterOp::NotContains,
        FilterOp::StartsWith,
        FilterOp::EndsWith,
        FilterOp::Like,
    ],
    &[FilterOp::In, FilterOp::NotIn, FilterOp::Between],
    &[
        FilterOp::IsNull,
        FilterOp::IsNotNull,
        FilterOp::IsEmpty,
        FilterOp::IsNotEmpty,
    ],
];

pub fn op_label(op: FilterOp) -> &'static str {
    match op {
        FilterOp::Eq => "=",
        FilterOp::NotEq => "≠",
        FilterOp::Lt => "<",
        FilterOp::LtEq => "≤",
        FilterOp::Gt => ">",
        FilterOp::GtEq => "≥",
        FilterOp::Contains => "contains",
        FilterOp::NotContains => "doesn't contain",
        FilterOp::StartsWith => "starts with",
        FilterOp::EndsWith => "ends with",
        FilterOp::Like => "LIKE",
        FilterOp::In => "in list",
        FilterOp::NotIn => "not in list",
        FilterOp::Between => "between",
        FilterOp::IsNull => "is NULL",
        FilterOp::IsNotNull => "is not NULL",
        FilterOp::IsEmpty => "is empty",
        FilterOp::IsNotEmpty => "is not empty",
    }
}

fn placeholder(op: FilterOp) -> &'static str {
    match op {
        FilterOp::In | FilterOp::NotIn => "a, b, \"c, d\"",
        FilterOp::Between => "From",
        FilterOp::Like => "Pattern: % any, _ one",
        _ => "Value",
    }
}

/// The SQL flavour of a driver, for the preview.
pub fn dialect(kind: DriverKind) -> Option<Dialect> {
    match kind {
        DriverKind::Postgres => Some(Dialect::Postgres),
        DriverKind::Mysql => Some(Dialect::Mysql),
        DriverKind::Sqlite => Some(Dialect::Sqlite),
        _ => None,
    }
}

/// Text typed for a comparison, as a value of the column's kind where it reads as one.
/// Anything else goes as text, which the database converts or refuses.
pub fn typed_value(kind: ValueKind, text: &str) -> Value {
    let trimmed = text.trim();
    match kind {
        _ if kind.is_numeric() => trimmed
            .parse::<i64>()
            .map(Value::Int)
            .or_else(|_| trimmed.parse::<f64>().map(Value::Float))
            .unwrap_or_else(|_| Value::text(text)),
        ValueKind::Bool => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "1" | "t" => Value::Bool(true),
            "false" | "0" | "f" => Value::Bool(false),
            _ => Value::text(text),
        },
        _ => Value::text(text),
    }
}

/// A comma separated list as values of the column's kind. An item in single or double
/// quotes is taken as written, commas and all, with a doubled quote for a quote; an
/// unquoted `NULL` is NULL; empty unquoted items are skipped.
pub fn list_values(kind: ValueKind, text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let Some(&first) = chars.peek() else {
            break;
        };
        if first == '"' || first == '\'' {
            chars.next();
            let mut item = String::new();
            while let Some(c) = chars.next() {
                if c == first {
                    if chars.next_if_eq(&first).is_some() {
                        item.push(first);
                        continue;
                    }
                    break;
                }
                item.push(c);
            }
            out.push(match kind {
                ValueKind::Text => Value::text(item),
                _ => typed_value(kind, &item),
            });
            // Anything between the closing quote and the next comma is dropped.
            for c in chars.by_ref() {
                if c == ',' {
                    break;
                }
            }
            continue;
        }
        let mut item = String::new();
        for c in chars.by_ref() {
            if c == ',' {
                break;
            }
            item.push(c);
        }
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        out.push(if item.eq_ignore_ascii_case("null") {
            Value::Null
        } else {
            typed_value(kind, item)
        });
    }
    out
}

/// One condition: the conditions as the drivers take them, from what was typed.
pub fn compile(column: &str, kind: ValueKind, op: FilterOp, value: &str, high: &str) -> Filter {
    match op {
        FilterOp::In | FilterOp::NotIn => Filter::list(column, op, list_values(kind, value)),
        FilterOp::Between => {
            let ends = [value, high]
                .iter()
                .filter(|t| !t.trim().is_empty())
                .map(|t| typed_value(kind, t))
                .collect();
            Filter::list(column, op, ends)
        }
        FilterOp::Contains
        | FilterOp::NotContains
        | FilterOp::StartsWith
        | FilterOp::EndsWith
        | FilterOp::Like => Filter::new(column, op, Value::text(value)),
        _ if !op.takes_value() => Filter::new(column, op, Value::Null),
        _ => Filter::new(column, op, typed_value(kind, value)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMenu {
    Column(usize),
    Op(usize),
}

pub struct FilterRow {
    pub column: String,
    pub op: FilterOp,
    pub value: Entity<TextInput>,
    /// The upper end of a `BETWEEN`.
    pub high: Entity<TextInput>,
    _subs: [Subscription; 2],
}

/// The filter bar's state.
pub struct Filters {
    pub rows: Vec<FilterRow>,
    pub match_any: bool,
    pub raw_mode: bool,
    pub raw: Entity<TextInput>,
    pub open: bool,
    pub menu: Option<OpenMenu>,
    _subs: Subscription,
}

impl Filters {
    pub fn new(cx: &mut Context<TablePanel>) -> Self {
        let raw = cx.new(|cx| TextInput::new("", cx));
        let sub = watch(&raw, cx);
        Self {
            rows: Vec::new(),
            match_any: false,
            raw_mode: false,
            raw,
            open: false,
            menu: None,
            _subs: sub,
        }
    }

    /// Whether anything filters the rows now.
    pub fn active(&self, cx: &App) -> bool {
        if self.raw_mode {
            !self.raw.read(cx).text().trim().is_empty()
        } else {
            !self.rows.is_empty()
        }
    }
}

/// Enter applies, and each change redraws the preview.
fn watch(input: &Entity<TextInput>, cx: &mut Context<TablePanel>) -> Subscription {
    cx.subscribe(input, |this, _, e, cx| match e {
        InputEvent::Submit => this.requery(cx),
        InputEvent::Changed => cx.notify(),
        InputEvent::Cancel => {}
    })
}

impl TablePanel {
    fn column_kind(&self, name: &str, cx: &App) -> ValueKind {
        let grid = self.grid.read(cx);
        grid.columns()
            .iter()
            .find(|c| c.name == name)
            .or_else(|| {
                self.info
                    .as_ref()
                    .and_then(|i| i.as_ref().ok())
                    .and_then(|i| i.columns.iter().find(|c| c.name == name))
            })
            .map_or(ValueKind::Text, |c| c.kind)
    }

    /// The filters, the OR flag and the raw condition the bar stands for now.
    pub(super) fn filter_parts(&self, cx: &App) -> (Vec<Filter>, bool, Option<String>) {
        let f = &self.filters;
        if f.raw_mode {
            let raw = f.raw.read(cx).text().trim().to_owned();
            return (Vec::new(), false, (!raw.is_empty()).then_some(raw));
        }
        let filters = f
            .rows
            .iter()
            .filter(|r| !r.column.is_empty())
            // A condition still waiting for its value filters nothing yet.
            .filter(|r| {
                !r.op.takes_value()
                    || !r.value.read(cx).text().is_empty()
                    || (r.op == FilterOp::Between && !r.high.read(cx).text().is_empty())
            })
            .map(|r| {
                compile(
                    &r.column,
                    self.column_kind(&r.column, cx),
                    r.op,
                    r.value.read(cx).text(),
                    r.high.read(cx).text(),
                )
            })
            .collect();
        (filters, f.match_any, None)
    }

    /// The sort and filter bar as they stand, for the session's memory.
    pub(super) fn saved_view(&self, cx: &App) -> SavedView {
        let f = &self.filters;
        SavedView {
            sort: self.sort.clone(),
            conditions: f
                .rows
                .iter()
                .map(|r| SavedCondition {
                    column: r.column.clone(),
                    op: r.op,
                    value: r.value.read(cx).text().to_owned(),
                    high: r.high.read(cx).text().to_owned(),
                })
                .collect(),
            match_any: f.match_any,
            raw_mode: f.raw_mode,
            raw: f.raw.read(cx).text().to_owned(),
            open: f.open,
        }
    }

    /// Puts a remembered or requested view in place. The caller reloads.
    pub(super) fn apply_view(&mut self, view: SavedView, cx: &mut Context<Self>) {
        self.sort = view.sort;
        self.filters.rows.clear();
        self.filters.menu = None;
        for c in view.conditions {
            self.push_condition(c, cx);
        }
        self.filters.match_any = view.match_any;
        self.filters.raw_mode = view.raw_mode;
        let raw = view.raw;
        self.filters.raw.update(cx, |i, cx| i.set_text(raw, cx));
        self.filters.open = view.open;
        cx.notify();
    }

    fn push_condition(&mut self, c: SavedCondition, cx: &mut Context<Self>) {
        let (value, high) = (c.value, c.high);
        let value = cx.new(|cx| {
            let mut input = TextInput::new(placeholder(c.op), cx);
            input.set_text(value, cx);
            input
        });
        let high = cx.new(|cx| {
            let mut input = TextInput::new("To", cx);
            input.set_text(high, cx);
            input
        });
        let subs = [watch(&value, cx), watch(&high, cx)];
        self.filters.rows.push(FilterRow {
            column: c.column,
            op: c.op,
            value,
            high,
            _subs: subs,
        });
    }

    /// Adds a condition on the selected column, or the first one, and shows the bar.
    pub(super) fn add_filter(&mut self, cx: &mut Context<Self>) {
        let grid = self.grid.read(cx);
        let columns = grid.columns();
        let column = grid
            .selection
            .and_then(|s| columns.get(s.head.1))
            .or_else(|| columns.first())
            .map(|c| c.name.clone())
            .unwrap_or_default();
        self.push_condition(SavedCondition::new(column, FilterOp::Eq, ""), cx);
        self.filters.raw_mode = false;
        self.filters.open = true;
        cx.notify();
    }

    /// Adds a condition and runs it: the cell and header menus' quick filters.
    pub(super) fn filter_by(
        &mut self,
        column: usize,
        op: FilterOp,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self
            .grid
            .read(cx)
            .columns()
            .get(column)
            .map(|c| c.name.clone())
        else {
            return;
        };
        // A quick filter adds to the conditions, so a raw condition gives way to them.
        self.filters.raw_mode = false;
        self.push_condition(SavedCondition::new(name, op, text), cx);
        self.filters.open = true;
        self.requery(cx);
    }

    pub(super) fn clear_filters(&mut self, cx: &mut Context<Self>) {
        self.filters.rows.clear();
        self.filters.raw.update(cx, |i, cx| i.clear(cx));
        self.filters.open = false;
        self.filters.menu = None;
        self.requery(cx);
    }

    /// Ctrl+F: shows the bar and puts the cursor in its last value, or the raw field.
    pub fn focus_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = super::Mode::Data;
        if !self.filters.raw_mode && self.filters.rows.is_empty() {
            self.add_filter(cx);
        }
        self.filters.open = true;
        let input = if self.filters.raw_mode {
            Some(self.filters.raw.clone())
        } else {
            self.filters.rows.last().map(|r| r.value.clone())
        };
        if let Some(input) = input {
            window.focus(&input.focus_handle(cx));
        }
        cx.notify();
    }

    fn set_raw_mode(&mut self, raw: bool, cx: &mut Context<Self>) {
        if raw == self.filters.raw_mode {
            return;
        }
        // The conditions carry over as the starting text, so the raw mode begins with
        // what was already filtering.
        if raw && self.filters.raw.read(cx).text().trim().is_empty() {
            let start = match self.preview(cx) {
                Some(Ok(text)) if self.driver.is_some_and(|k| dialect(k).is_some()) => text
                    .strip_prefix("WHERE ")
                    .map(str::to_owned)
                    .unwrap_or_default(),
                Some(Ok(text)) if text != "{}" => text,
                _ => String::new(),
            };
            self.filters.raw.update(cx, |i, cx| i.set_text(start, cx));
        }
        self.filters.raw_mode = raw;
        self.filters.menu = None;
        cx.notify();
    }

    /// The request the bar stands for, as a count would send it.
    fn preview_request(&self, cx: &App) -> BrowseRequest {
        let (filters, match_any, raw_where) = self.filter_parts(cx);
        BrowseRequest {
            sort: self.sort.clone(),
            filters,
            match_any,
            raw_where,
            ..Default::default()
        }
    }

    /// What runs, as the person would write it: the `WHERE` clause for SQL databases,
    /// the filter document for MongoDB. `None` where there is nothing to show.
    fn preview(&self, cx: &App) -> Option<Result<String, String>> {
        let kind = self.driver?;
        let request = self.preview_request(cx);
        if let Some(dialect) = dialect(kind) {
            return Some(sql::where_preview(dialect, &request).map_err(|e| e.to_string()));
        }
        if kind == DriverKind::Mongodb {
            let columns = self.grid.read(cx).columns().clone();
            let kind_of = |name: &str| {
                columns
                    .iter()
                    .find(|c: &&Column| c.name == name)
                    .map(|c| c.kind)
            };
            return Some(
                deltos_drivers::mongodb::filter_preview(&request, kind_of)
                    .map_err(|e| e.to_string()),
            );
        }
        None
    }

    /// The whole statement for what the table shows, ready to paste into a query.
    fn copy_as_sql(&self, cx: &mut App) {
        let Some(kind) = self.driver else { return };
        let text = match dialect(kind) {
            Some(dialect) => {
                let from = edit::table_name(kind, &self.object);
                sql::select_preview(dialect, &from, &self.preview_request(cx))
            }
            None => match self.preview(cx) {
                Some(Ok(filter)) => Ok(mongo_find(&self.object, &filter)),
                Some(Err(e)) => Err(deltos_data::Error::Query(e)),
                None => return,
            },
        };
        if let Ok(text) = text {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    pub(super) fn filter_bar(
        &mut self,
        t: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let columns = self.grid.read(cx).columns().clone();
        let sql_like = self.driver.is_some_and(|k| dialect(k).is_some());
        let mut bar = div()
            .flex_none()
            .px(px(8.))
            .py(px(6.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .bg(t.sidebar)
            .border_b_1()
            .border_color(t.border);
        if self.filters.raw_mode {
            let label = if sql_like { "WHERE" } else { "Filter" };
            let hint = if sql_like {
                "status = 'open' AND total > 100"
            } else {
                "{ status: \"open\", total: { $gt: 100 } }"
            };
            self.filters
                .raw
                .update(cx, |i, cx| i.set_placeholder(hint, cx));
            bar = bar.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .w(px(52.))
                            .flex_none()
                            .font_family(t.mono_font.clone())
                            .text_size(t.mono_size)
                            .text_color(t.syntax_keyword)
                            .child(label),
                    )
                    .child(div().flex_1().font_family(t.mono_font.clone()).child(field(
                        &self.filters.raw,
                        t,
                        window,
                        cx,
                    ))),
            );
        }
        for (i, f) in self.filters.rows.iter().enumerate() {
            if self.filters.raw_mode {
                break;
            }
            let column_name: SharedString = if f.column.is_empty() {
                "Column".into()
            } else {
                f.column.clone().into()
            };
            let open = self.filters.menu;
            let needs_value = f.op.takes_value();
            let picker = |id: (&'static str, usize), label: SharedString, width: f32| {
                div()
                    .id(id)
                    .relative()
                    .flex_none()
                    .w(px(width))
                    .h(t.s(26.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .rounded(t.radius_small)
                    .bg(t.input)
                    .border_1()
                    .border_color(t.border_strong)
                    .cursor_pointer()
                    .text_size(t.ui_size)
                    .child(div().overflow_hidden().whitespace_nowrap().child(label))
                    .child(
                        icon(Icon::ChevronDown)
                            .size(px(12.))
                            .text_color(t.text_faint),
                    )
            };
            let mut col_picker = picker(("filter-col", i), column_name, 180.).on_click(
                cx.listener(move |this, _, _, cx| {
                    this.filters.menu = Some(OpenMenu::Column(i));
                    cx.notify();
                }),
            );
            let weak = cx.entity().downgrade();
            let close = {
                let weak = weak.clone();
                move |_: &mut Window, cx: &mut App| {
                    weak.update(cx, |this, cx| {
                        this.filters.menu = None;
                        cx.notify();
                    })
                    .ok();
                }
            };
            if open == Some(OpenMenu::Column(i)) {
                let items = columns
                    .iter()
                    .map(|col| {
                        let (weak, name) = (weak.clone(), col.name.clone());
                        MenuItem::new(col.name.clone(), move |_, cx| {
                            weak.update(cx, |this, cx| {
                                if let Some(f) = this.filters.rows.get_mut(i) {
                                    f.column = name.clone();
                                }
                                cx.notify();
                            })
                            .ok();
                        })
                        .detail(col.type_name.clone())
                        .checked(col.name == f.column)
                    })
                    .collect();
                col_picker = col_picker.child(menu("column-menu", items, 220., close.clone(), t));
            }
            let mut op_picker = picker(("filter-op", i), op_label(f.op).into(), 130.).on_click(
                cx.listener(move |this, _, _, cx| {
                    this.filters.menu = Some(OpenMenu::Op(i));
                    cx.notify();
                }),
            );
            if open == Some(OpenMenu::Op(i)) {
                let mut items = Vec::new();
                for group in OP_GROUPS {
                    items.push(MenuItem::separator());
                    for &op in group {
                        let weak = weak.clone();
                        let label = if op == FilterOp::Like && !sql_like {
                            "matches pattern"
                        } else {
                            op_label(op)
                        };
                        items.push(
                            MenuItem::new(label, move |_, cx| {
                                weak.update(cx, |this, cx| {
                                    if let Some(f) = this.filters.rows.get_mut(i) {
                                        f.op = op;
                                        f.value.update(cx, |v, cx| {
                                            v.set_placeholder(placeholder(op), cx)
                                        });
                                    }
                                    cx.notify();
                                })
                                .ok();
                            })
                            .checked(op == f.op),
                        );
                    }
                }
                op_picker = op_picker.child(menu("op-menu", items, 160., close, t));
            }
            let between = f.op == FilterOp::Between;
            bar = bar.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .w(px(30.))
                            .flex_none()
                            .text_size(t.small_size)
                            .text_color(t.text_faint)
                            .child(match (i, self.filters.match_any) {
                                (0, _) => "",
                                (_, true) => "or",
                                (_, false) => "and",
                            }),
                    )
                    .child(col_picker)
                    .child(op_picker)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .when(needs_value, |el| {
                                el.child(div().flex_1().child(field(&f.value, t, window, cx)))
                            })
                            .when(between, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(t.small_size)
                                        .text_color(t.text_faint)
                                        .child("and"),
                                )
                                .child(div().flex_1().child(field(&f.high, t, window, cx)))
                            })
                            .when(!needs_value, |el| el.h(t.s(26.))),
                    )
                    .child(
                        icon_button(("filter-remove", i), Icon::Close, t)
                            .tooltip(|_, cx| Tooltip::view("Remove this condition", cx))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if i < this.filters.rows.len() {
                                    this.filters.rows.remove(i);
                                }
                                this.filters.menu = None;
                                if this.filters.rows.is_empty() {
                                    this.filters.open = false;
                                }
                                this.requery(cx);
                            })),
                    ),
            );
        }

        // What runs, in the database's own words.
        let preview = self.preview(cx);
        if let Some(preview) = preview.filter(|p| p.as_ref().map_or(true, |s| !s.is_empty())) {
            let (text, failed) = match preview {
                Ok(text) => (text, false),
                Err(e) => (e, true),
            };
            if !self.filters.raw_mode || failed {
                bar = bar.child(
                    div()
                        .pl(px(36.))
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .font_family(t.mono_font.clone())
                        .text_size(t.small_size)
                        .text_color(if failed { t.danger } else { t.text_faint })
                        .child(text),
                );
            }
        }

        let weak = cx.entity().downgrade();
        let mode_labels: &[(&'static str, bool)] = if sql_like {
            &[("Conditions", false), ("SQL", true)]
        } else {
            &[("Conditions", false), ("JSON", true)]
        };
        let show_any = !self.filters.raw_mode && self.filters.rows.len() > 1;
        bar.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(
                    button("filter-add", "Add condition", ButtonStyle::Ghost, t).on_click(
                        cx.listener(|this, _, window, cx| {
                            if this.filters.raw_mode {
                                this.set_raw_mode(false, cx);
                            }
                            this.add_filter(cx);
                            if let Some(row) = this.filters.rows.last() {
                                window.focus(&row.value.focus_handle(cx));
                            }
                        }),
                    ),
                )
                .when(show_any, |el| {
                    let weak = weak.clone();
                    el.child(segmented(
                        "filter-join",
                        &[("All", false), ("Any", true)],
                        self.filters.match_any,
                        move |any, _, cx| {
                            weak.update(cx, |this, cx| {
                                this.filters.match_any = any;
                                this.requery(cx);
                            })
                            .ok();
                        },
                        t,
                    ))
                })
                .child(div().flex_1())
                .child(segmented(
                    "filter-mode",
                    mode_labels,
                    self.filters.raw_mode,
                    {
                        let weak = weak.clone();
                        move |raw, _, cx| {
                            weak.update(cx, |this, cx| this.set_raw_mode(raw, cx)).ok();
                        }
                    },
                    t,
                ))
                .when(self.driver.is_some(), |el| {
                    el.child(
                        button(
                            "filter-copy",
                            if sql_like {
                                "Copy as SQL"
                            } else {
                                "Copy query"
                            },
                            ButtonStyle::Ghost,
                            t,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.copy_as_sql(cx))),
                    )
                })
                .child(
                    button("filter-clear", "Clear", ButtonStyle::Secondary, t)
                        .on_click(cx.listener(|this, _, _, cx| this.clear_filters(cx))),
                )
                .child(
                    button("filter-apply", "Apply", ButtonStyle::Primary, t)
                        .on_click(cx.listener(|this, _, _, cx| this.requery(cx))),
                ),
        )
        .into_any_element()
    }
}

/// `db.getCollection("name").find({...})` for a filter document.
fn mongo_find(object: &ObjectRef, filter: &str) -> String {
    let name = serde_json::to_string(&object.name).unwrap_or_else(|_| "\"\"".into());
    format!("db.getCollection({name}).find({filter})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_split_on_commas_outside_quotes() {
        assert_eq!(
            list_values(ValueKind::Int, " 1, 2 ,, x, null "),
            vec![Value::Int(1), Value::Int(2), Value::text("x"), Value::Null]
        );
        assert_eq!(
            list_values(ValueKind::Text, r#"a, "b, c", 'it''s', "NULL" "#),
            vec![
                Value::text("a"),
                Value::text("b, c"),
                Value::text("it's"),
                Value::text("NULL"),
            ]
        );
        assert_eq!(list_values(ValueKind::Text, "  "), Vec::<Value>::new());
        // An unclosed quote takes the rest.
        assert_eq!(
            list_values(ValueKind::Text, "\"open, end"),
            vec![Value::text("open, end")]
        );
    }

    #[test]
    fn compiles_what_was_typed() {
        assert_eq!(
            compile("n", ValueKind::Int, FilterOp::Gt, " 5 ", ""),
            Filter::new("n", FilterOp::Gt, Value::Int(5))
        );
        assert_eq!(
            compile("n", ValueKind::Int, FilterOp::Contains, "5", ""),
            Filter::new("n", FilterOp::Contains, Value::text("5"))
        );
        assert_eq!(
            compile("n", ValueKind::Float, FilterOp::Between, "1", "2.5"),
            Filter::list(
                "n",
                FilterOp::Between,
                vec![Value::Int(1), Value::Float(2.5)]
            )
        );
        assert_eq!(
            compile("ok", ValueKind::Bool, FilterOp::Eq, "t", ""),
            Filter::new("ok", FilterOp::Eq, Value::Bool(true))
        );
        assert_eq!(
            compile("x", ValueKind::Text, FilterOp::IsEmpty, "ignored", ""),
            Filter::new("x", FilterOp::IsEmpty, Value::Null)
        );
        // Only the end that was typed: the driver then says BETWEEN needs two.
        assert_eq!(
            compile("n", ValueKind::Int, FilterOp::Between, "1", " ").values,
            vec![Value::Int(1)]
        );
    }

    #[test]
    fn every_operator_has_a_place_in_the_menu() {
        let listed: Vec<FilterOp> = OP_GROUPS.iter().flat_map(|g| g.iter().copied()).collect();
        assert_eq!(listed.len(), FilterOp::ALL.len());
        for op in FilterOp::ALL {
            assert!(listed.contains(&op), "{op:?}");
            assert!(!op_label(op).is_empty());
        }
    }

    #[test]
    fn copies_a_mongo_find() {
        let object = ObjectRef {
            namespace: vec!["db".into()],
            name: "my \"docs\"".into(),
            kind: deltos_data::ObjectKind::Collection,
        };
        assert_eq!(
            mongo_find(&object, "{\"a\":1}"),
            "db.getCollection(\"my \\\"docs\\\"\").find({\"a\":1})"
        );
    }
}
