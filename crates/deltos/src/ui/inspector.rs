//! The inspector: the selected row's fields one under another, as the right-hand panel
//! of a database tool shows them. JSON values are pretty-printed.
//!
//! A row of a table can be edited here. A field's edit is saved on Enter (or when the
//! field loses focus, for multi-line fields) and staged in the table the row came from,
//! the same way an edit in its grid is, so the grid and the inspector always show the
//! same staged values. Rows of query results are read-only.

use deltos_data::{Column, Value, ValueKind};
use gpui::{
    Context, Entity, EntityId, Focusable, FontWeight, IntoElement, Render, SharedString,
    Subscription, Window, div, prelude::*, px,
};

use crate::state::{AppState, Inspected, StateEvent};
use crate::theme::{Theme, theme};
use crate::ui::editor::Editor;
use crate::ui::grid::{CellInput, RowRef};
use crate::ui::icons::{Icon, icon};
use crate::ui::pending::RowMark;
use crate::ui::references;
use crate::ui::table::{CommitChanges, DiscardChanges, Request, ReviewChanges};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{Tooltip, empty_state, icon_button, thousands};

/// Values longer than this, or with a line break, get a multi-line field.
const SINGLE_LINE_MAX: usize = 60;

enum FieldInput {
    Line(Entity<TextInput>),
    Multi(Entity<Editor>),
}

/// The editor of one field.
struct Field {
    /// Where an edit goes: the table panel, its grid row, and the row itself, which
    /// wins when the table's rows moved since.
    owner: EntityId,
    row: u64,
    target: RowRef,
    col: usize,
    /// The column holds JSON, which the field shows indented.
    json: bool,
    input: FieldInput,
    /// The text the field was last given, to tell an edit from what was shown.
    shown: String,
    _subs: Vec<Subscription>,
}

impl Field {
    fn text(&self, cx: &gpui::App) -> String {
        match &self.input {
            FieldInput::Line(i) => i.read(cx).text().to_owned(),
            FieldInput::Multi(e) => e.read(cx).text().to_owned(),
        }
    }

    fn set_text(&mut self, text: String, cx: &mut gpui::App) {
        match &self.input {
            FieldInput::Line(i) => i.update(cx, |i, cx| i.set_text(text.clone(), cx)),
            FieldInput::Multi(e) => e.update(cx, |e, cx| e.set_text(text.clone(), cx)),
        }
        self.shown = text;
    }

    fn set_placeholder(&self, placeholder: &'static str, cx: &mut gpui::App) {
        match &self.input {
            FieldInput::Line(i) => i.update(cx, |i, cx| i.set_placeholder(placeholder, cx)),
            FieldInput::Multi(e) => e.update(cx, |e, cx| e.set_placeholder(placeholder, cx)),
        }
    }

    fn is_focused(&self, window: &Window, cx: &gpui::App) -> bool {
        match &self.input {
            FieldInput::Line(i) => i.focus_handle(cx).is_focused(window),
            FieldInput::Multi(e) => e.focus_handle(cx).is_focused(window),
        }
    }
}

/// Which row the field editors were made for.
#[derive(Debug, Clone, PartialEq)]
struct Bound {
    owner: EntityId,
    row: u64,
    target: RowRef,
    columns: usize,
}

pub struct InspectorPanel {
    state: Entity<AppState>,
    filter: Entity<TextInput>,
    bound: Option<Bound>,
    fields: Vec<Field>,
    _subs: Vec<Subscription>,
}

impl InspectorPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| TextInput::new("Filter fields", cx));
        let subs = vec![
            cx.subscribe(&state, |_, _, e, cx| {
                if matches!(e, StateEvent::Inspect | StateEvent::Settings) {
                    cx.notify();
                }
            }),
            cx.subscribe(&filter, |_, input, e, cx| match e {
                InputEvent::Changed => cx.notify(),
                InputEvent::Cancel => input.update(cx, |i, cx| i.clear(cx)),
                InputEvent::Submit => {}
            }),
            // A foreign key followed from the row, or a step back.
            cx.observe_global::<references::Trail>(|_, cx| cx.notify()),
        ];
        Self {
            state,
            filter,
            bound: None,
            fields: Vec::new(),
            _subs: subs,
        }
    }

    /// Makes the field editors for an editable row, or brings their text up to date when
    /// they were made for this row already. A field being edited keeps what is typed.
    fn sync_fields(&mut self, row: &Inspected, window: &mut Window, cx: &mut Context<Self>) {
        let editable = row.source.read_only.is_none();
        let bound = match (row.source.owner, &row.source.target) {
            (Some(owner), Some(target)) if editable => Some(Bound {
                owner,
                row: row.row,
                target: target.clone(),
                columns: row.columns.as_ptr() as *const () as usize,
            }),
            _ => None,
        };
        if bound.is_none() {
            self.unbind(cx);
            return;
        }
        if bound != self.bound {
            // Edits typed into the old row's fields go to that row before they are gone.
            self.unbind(cx);
            self.bound = bound;
            self.fields = row
                .columns
                .iter()
                .enumerate()
                .map(|(c, column)| {
                    let value = row.values.get(c).cloned().unwrap_or_default();
                    self.make_field(row, c, column, &value, window, cx)
                })
                .collect();
            return;
        }
        for field in &mut self.fields {
            let Some(column) = row.columns.get(field.col) else {
                continue;
            };
            let value = row.values.get(field.col).cloned().unwrap_or_default();
            // NULL and empty text both show as nothing; the placeholder tells them apart.
            field.set_placeholder(placeholder(&value, row.source.mark), cx);
            let text = edit_text(&value, column.kind);
            if text == field.shown {
                continue;
            }
            if field.is_focused(window, cx) {
                // What was typed is now the staged value.
                if field.text(cx) == text {
                    field.shown = text;
                }
                continue;
            }
            field.set_text(text, cx);
        }
    }

    fn make_field(
        &mut self,
        row: &Inspected,
        col: usize,
        column: &Column,
        value: &Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Field {
        let mark = row.source.mark;
        let text = edit_text(value, column.kind);
        let placeholder = placeholder(value, mark);
        let multi = matches!(column.kind, ValueKind::Json | ValueKind::Array)
            || text.contains('\n')
            || text.chars().count() > SINGLE_LINE_MAX;
        let mut subs = Vec::new();
        let input = if multi {
            let editor = cx.new(|cx| {
                let mut e = Editor::new(String::new(), placeholder, cx).plain();
                e.set_text(text.clone(), cx);
                e
            });
            let focus = editor.focus_handle(cx);
            subs.push(cx.on_blur(&focus, window, move |this, _, cx| this.save(col, cx)));
            FieldInput::Multi(editor)
        } else {
            let input = cx.new(|cx| {
                let mut i = TextInput::new(placeholder, cx);
                i.set_text(text.clone(), cx);
                i
            });
            subs.push(cx.subscribe(&input, move |this, _, e, cx| match e {
                InputEvent::Submit => this.save(col, cx),
                InputEvent::Cancel => this.reset(col, cx),
                InputEvent::Changed => {}
            }));
            let focus = input.focus_handle(cx);
            subs.push(cx.on_blur(&focus, window, move |this, _, cx| this.save(col, cx)));
            FieldInput::Line(input)
        };
        Field {
            owner: row.source.owner.unwrap_or(cx.entity_id()),
            row: row.row,
            target: row
                .source
                .target
                .clone()
                .unwrap_or(RowRef::Table(Vec::new())),
            col,
            json: matches!(column.kind, ValueKind::Json | ValueKind::Array),
            input,
            shown: text,
            _subs: subs,
        }
    }

    fn field(&self, col: usize) -> Option<&Field> {
        self.fields.iter().find(|f| f.col == col)
    }

    /// Drops the field editors, staging first what was typed into them and not saved
    /// yet, in the row they were made for.
    fn unbind(&mut self, cx: &mut Context<Self>) {
        self.bound = None;
        for mut field in std::mem::take(&mut self.fields) {
            send(&self.state, &mut field, cx);
        }
    }

    /// Stages what a field holds, when it differs from what it showed.
    fn save(&mut self, col: usize, cx: &mut Context<Self>) {
        let state = self.state.clone();
        if let Some(field) = self.fields.iter_mut().find(|f| f.col == col) {
            send(&state, field, cx);
        }
    }

    /// Puts back what a field showed, dropping what was typed.
    fn reset(&mut self, col: usize, cx: &mut Context<Self>) {
        if let Some(field) = self.fields.iter_mut().find(|f| f.col == col) {
            let shown = field.shown.clone();
            field.set_text(shown, cx);
        }
    }

    fn stage(&mut self, col: usize, input: Option<CellInput>, cx: &mut Context<Self>) {
        if let Some(b) = self.bound.clone() {
            let request = Request::Stage {
                row: b.row,
                col,
                input,
                target: b.target,
            };
            self.state
                .update(cx, |s, cx| s.table_request(b.owner, request, cx));
        }
    }

    /// Passes a shortcut for the table on to it, after saving the field being edited.
    fn forward(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        let Some(owner) = self.bound.as_ref().map(|b| b.owner) else {
            return;
        };
        if let Some(col) = self
            .fields
            .iter()
            .find(|f| f.is_focused(window, cx))
            .map(|f| f.col)
        {
            self.save(col, cx);
        }
        self.state
            .update(cx, |s, cx| s.table_request(owner, request, cx));
    }
}

/// Stages what a field holds in the row the field was made for, when it differs from
/// what the field showed. What was sent counts as shown, so it is not sent twice.
fn send(state: &Entity<AppState>, field: &mut Field, cx: &mut gpui::App) {
    let text = field.text(cx);
    if text == field.shown {
        return;
    }
    field.shown = text.clone();
    let text = if field.json {
        compact_json(&text)
    } else {
        text
    };
    let request = Request::Stage {
        row: field.row,
        col: field.col,
        input: Some(CellInput::Text(text)),
        target: field.target.clone(),
    };
    let owner = field.owner;
    state.update(cx, |s, cx| s.table_request(owner, request, cx));
}

/// The full text of a value for the inspector: JSON indented, everything else as sent.
pub fn full_text(value: &Value, kind: ValueKind) -> String {
    match value {
        Value::Json(s) => pretty_json(s),
        Value::Text(s) if matches!(kind, ValueKind::Json | ValueKind::Array) => pretty_json(s),
        v => v.to_string(),
    }
}

fn pretty_json(s: &str) -> String {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| s.to_owned())
}

/// What an empty field stands for: a new row's default, NULL, or empty text.
fn placeholder(value: &Value, mark: RowMark) -> &'static str {
    match (value.is_null(), mark) {
        (true, RowMark::Inserted) => "DEFAULT",
        (true, _) => "NULL",
        (false, _) => "",
    }
}

/// The text a field starts with: empty for NULL, JSON indented with its keys in their
/// order, everything else as sent.
fn edit_text(value: &Value, kind: ValueKind) -> String {
    match value {
        Value::Null => String::new(),
        Value::Json(s) => indent_json(s),
        Value::Text(s) if matches!(kind, ValueKind::Json | ValueKind::Array) => indent_json(s),
        v => v.to_string(),
    }
}

/// Walks JSON text, calling `f` with each character outside strings and each whole
/// string. Order and content stay as written; only the spacing between tokens can change.
fn json_tokens(s: &str, mut f: impl FnMut(Result<char, &str>)) {
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c != '"' {
            f(Ok(c));
            continue;
        }
        let mut end = s.len();
        let mut escaped = false;
        for (j, d) in chars.by_ref() {
            if escaped {
                escaped = false;
            } else if d == '\\' {
                escaped = true;
            } else if d == '"' {
                end = j + 1;
                break;
            }
        }
        f(Err(&s[i..end]));
    }
}

/// JSON with two spaces of indent per level, keys in the order they were written, or the
/// text as it is when it is not JSON.
fn indent_json(s: &str) -> String {
    if serde_json::from_str::<serde::de::IgnoredAny>(s).is_err() {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() * 2);
    let mut depth = 0usize;
    let mut open = false;
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.extend(std::iter::repeat_n("  ", depth));
    };
    json_tokens(s, |t| {
        let c = match t {
            Err(string) => {
                if open {
                    newline(&mut out, depth);
                    open = false;
                }
                out.push_str(string);
                return;
            }
            Ok(c) if c.is_whitespace() => return,
            Ok(c) => c,
        };
        match c {
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                if !open {
                    newline(&mut out, depth);
                }
                open = false;
                out.push(c);
            }
            _ => {
                if open {
                    newline(&mut out, depth);
                    open = false;
                }
                out.push(c);
                match c {
                    '{' | '[' => {
                        depth += 1;
                        open = true;
                    }
                    ',' => newline(&mut out, depth),
                    ':' => out.push(' '),
                    _ => {}
                }
            }
        }
    });
    out
}

/// JSON without the spacing between tokens, keys in the order they were written, or the
/// text as it is when it is not JSON.
fn compact_json(s: &str) -> String {
    if serde_json::from_str::<serde::de::IgnoredAny>(s).is_err() {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    json_tokens(s, |t| match t {
        Err(string) => out.push_str(string),
        Ok(c) if c.is_whitespace() => {}
        Ok(c) => out.push(c),
    });
    out
}

fn field_header(
    column: &Column,
    staged: bool,
    can_null: bool,
    col: usize,
    t: &Theme,
    cx: &mut Context<InspectorPanel>,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .text_size(t.small_size)
        .child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .text_color(if staged { t.warning } else { t.text_muted })
                .child(column.name.clone()),
        )
        .when(column.primary_key.is_some(), |el| {
            el.child(icon(Icon::Key).size(px(10.)).text_color(t.warning))
        })
        .child(div().flex_1())
        .when(can_null, |el| {
            el.child(
                div()
                    .id(("set-null", col))
                    .px(px(4.))
                    .rounded(t.radius_small)
                    .text_color(t.text_faint)
                    .cursor_pointer()
                    .hover(|s| s.bg(t.hover).text_color(t.text))
                    .child("NULL")
                    .tooltip(|_, cx| Tooltip::view("Set to NULL", cx))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.stage(col, Some(CellInput::Null), cx)
                    })),
            )
        })
        .when(staged, |el| {
            el.child(
                icon_button(("revert-field", col), Icon::Undo, t)
                    .size(t.s(18.))
                    .tooltip(|_, cx| Tooltip::view("Revert to the value in the database", cx))
                    .on_click(cx.listener(move |this, _, _, cx| this.stage(col, None, cx))),
            )
        })
        .child(
            div()
                .text_color(t.text_faint)
                .child(column.type_name.clone()),
        )
}

impl Render for InspectorPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let Some(row) = self.state.read(cx).inspected.clone() else {
            self.unbind(cx);
            return div()
                .size_full()
                .bg(t.sidebar)
                .child(empty_state(
                    "No row selected",
                    Some("Select a row to see its fields here.".into()),
                    &t,
                ))
                .into_any_element();
        };
        if let Some(trail) = references::view(&self.state, &row, &self.filter, window, cx) {
            return trail;
        }
        self.sync_fields(&row, window, cx);
        let editable = !self.fields.is_empty();
        let note: Option<SharedString> = match (&row.source.owner, &row.source.read_only) {
            (None, _) => {
                Some("Query results are read-only. Open the table to edit its rows.".into())
            }
            (Some(_), Some(reason)) => Some(reason.clone()),
            _ => None,
        };
        let needle = self.filter.read(cx).text().trim().to_lowercase();
        let focused = self.filter.focus_handle(cx).is_focused(window);
        let line_height = (t.editor_size * 1.55).round();
        let mark = row.source.mark;
        let mut fields = div().flex().flex_col().px(px(10.)).pb(px(12.)).gap(px(10.));
        for (c, column) in row.columns.iter().enumerate() {
            if !needle.is_empty() && !column.name.to_lowercase().contains(&needle) {
                continue;
            }
            let value = row.values.get(c).cloned().unwrap_or_default();
            let null = value.is_null();
            let original = row.source.original.get(c).cloned().flatten();
            let staged = original.is_some() || (mark == RowMark::Inserted && !null);
            let tint = match mark {
                RowMark::Inserted => Some(t.staged_insert),
                RowMark::Deleted => Some(t.staged_delete),
                _ if staged => Some(t.staged_change),
                _ => None,
            };
            let frame = div()
                .rounded(t.radius_small)
                .bg(tint.unwrap_or(t.input))
                .border_1()
                .border_color(if staged { t.warning } else { t.border })
                .font_family(t.mono_font.clone())
                .text_size(t.mono_size)
                .text_color(if null { t.text_faint } else { t.text });
            let body = match self.field(c).filter(|_| editable) {
                Some(Field {
                    input: FieldInput::Line(input),
                    ..
                }) => {
                    let focused = input.focus_handle(cx).is_focused(window);
                    frame
                        .h(t.s(26.))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .when(focused, |el| el.border_color(t.accent))
                        .child(input.clone())
                        .into_any_element()
                }
                Some(Field {
                    input: FieldInput::Multi(editor),
                    ..
                }) => {
                    let focused = editor.focus_handle(cx).is_focused(window);
                    let lines = editor.read(cx).text().lines().count().clamp(3, 14) as f32;
                    frame
                        .h(line_height * lines + px(8.))
                        .py(px(4.))
                        .overflow_hidden()
                        .when(focused, |el| el.border_color(t.accent))
                        .on_key_down(cx.listener(move |this, e: &gpui::KeyDownEvent, _, cx| {
                            if e.keystroke.key == "escape" {
                                this.reset(c, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .child(editor.clone())
                        .into_any_element()
                }
                None => {
                    let text: SharedString = if null {
                        t.null_text.into()
                    } else {
                        full_text(&value, column.kind).into()
                    };
                    frame
                        .px(px(8.))
                        .py(px(5.))
                        .whitespace_normal()
                        .when(mark == RowMark::Deleted, |el| el.line_through())
                        .child(text)
                        .into_any_element()
                }
            };
            let can_null =
                editable && !null && column.nullable != Some(false) && column.primary_key.is_none();
            let key_warning = editable && staged && column.primary_key.is_some();
            fields = fields.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(field_header(
                        column,
                        staged && editable,
                        can_null,
                        c,
                        &t,
                        cx,
                    ))
                    .child(body)
                    .when_some(original.filter(|_| editable), |el, was| {
                        let was: SharedString = if was.is_null() {
                            format!("was {}", t.null_text).into()
                        } else {
                            format!("was {}", was.preview(80)).into()
                        };
                        el.child(
                            div()
                                .text_size(t.small_size)
                                .text_color(t.text_faint)
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(was),
                        )
                    })
                    .when(key_warning, |el| {
                        el.child(
                            div()
                                .text_size(t.small_size)
                                .text_color(t.warning)
                                .whitespace_normal()
                                .child(
                                    "Part of the primary key: committing gives the row a new key.",
                                ),
                        )
                    }),
            );
        }
        let position: SharedString = match mark {
            RowMark::Inserted => "new row".into(),
            _ => format!("row {}", thousands(row.row + 1)).into(),
        };
        let status: Option<(SharedString, gpui::Hsla)> = match mark {
            RowMark::Changed => Some(("changed".into(), t.warning)),
            RowMark::Deleted => Some(("to be deleted".into(), t.danger)),
            RowMark::Inserted => Some(("not committed".into(), t.success)),
            RowMark::None => None,
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context("Inspector")
            .bg(t.sidebar)
            .on_action(cx.listener(|this, _: &CommitChanges, window, cx| {
                this.forward(Request::Commit, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ReviewChanges, window, cx| {
                this.forward(Request::Review, window, cx)
            }))
            .on_action(cx.listener(|this, _: &DiscardChanges, window, cx| {
                this.forward(Request::Discard, window, cx)
            }))
            .child(
                div()
                    .flex_none()
                    .px(px(10.))
                    .pt(px(8.))
                    .pb(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(t.small_size)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.text)
                            .child(row.title.clone()),
                    )
                    .child(div().text_color(t.text_faint).child(position))
                    .when_some(status, |el, (text, color)| {
                        el.child(div().text_color(color).child(text))
                    }),
            )
            .children(note.map(|n| {
                div()
                    .flex_none()
                    .px(px(10.))
                    .pb(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(t.small_size)
                    .text_color(t.text_faint)
                    .child(icon(Icon::Lock).size(px(11.)))
                    .child(div().flex_1().whitespace_normal().child(n))
            }))
            .child(
                div().flex_none().px(px(8.)).pb(px(8.)).child(
                    div()
                        .h(t.s(24.))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .rounded(t.radius_small)
                        .bg(t.input)
                        .border_1()
                        .border_color(if focused { t.accent } else { t.border })
                        .text_size(t.small_size)
                        .child(icon(Icon::Search).size(px(12.)).text_color(t.text_faint))
                        .child(self.filter.clone()),
                ),
            )
            .child(
                div()
                    .id("fields")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(fields),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_pretty() {
        let v = Value::Json(r#"{"a":1,"b":[1,2]}"#.into());
        assert!(full_text(&v, ValueKind::Json).contains("\n  \"a\": 1"));
        assert_eq!(
            full_text(&Value::Json("not json".into()), ValueKind::Json),
            "not json"
        );
        assert_eq!(full_text(&Value::Int(3), ValueKind::Int), "3");
    }

    #[test]
    fn edited_json_keeps_its_key_order() {
        let text = r#"{"z":1,"a":{"s":"x, \"y\": {z}","e":[]},"m":[1, 2]}"#;
        let indented = indent_json(text);
        assert_eq!(
            indented,
            "{\n  \"z\": 1,\n  \"a\": {\n    \"s\": \"x, \\\"y\\\": {z}\",\n    \"e\": []\n  },\n  \"m\": [\n    1,\n    2\n  ]\n}"
        );
        assert_eq!(
            compact_json(&indented),
            r#"{"z":1,"a":{"s":"x, \"y\": {z}","e":[]},"m":[1,2]}"#
        );
        assert_eq!(indent_json("{oops"), "{oops");
        assert_eq!(compact_json("not json"), "not json");
        assert_eq!(
            edit_text(&Value::text("[1,2]"), ValueKind::Array),
            "[\n  1,\n  2\n]"
        );
        assert_eq!(edit_text(&Value::Null, ValueKind::Text), "");
    }
}
