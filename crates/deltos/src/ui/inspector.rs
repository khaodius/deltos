//! The inspector: the selected row's fields one under another, as the right-hand panel
//! of a database tool shows them. JSON values are pretty-printed.

use deltos_data::{Value, ValueKind};
use gpui::{
    Context, Entity, Focusable, FontWeight, IntoElement, Render, SharedString, Subscription,
    Window, div, prelude::*, px,
};

use crate::state::{AppState, StateEvent};
use crate::theme::theme;
use crate::ui::icons::{Icon, icon};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{empty_state, thousands};

pub struct InspectorPanel {
    state: Entity<AppState>,
    filter: Entity<TextInput>,
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
        ];
        Self {
            state,
            filter,
            _subs: subs,
        }
    }
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

impl Render for InspectorPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let s = self.state.read(cx);
        let Some(row) = s.inspected.clone() else {
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
        let needle = self.filter.read(cx).text().trim().to_lowercase();
        let focused = self.filter.focus_handle(cx).is_focused(window);
        let mut fields = div().flex().flex_col().px(px(10.)).pb(px(12.)).gap(px(10.));
        for (c, column) in row.columns.iter().enumerate() {
            if !needle.is_empty() && !column.name.to_lowercase().contains(&needle) {
                continue;
            }
            let value = row.values.get(c).cloned().unwrap_or_default();
            let null = value.is_null();
            let text: SharedString = if null {
                t.null_text.into()
            } else {
                full_text(&value, column.kind).into()
            };
            fields = fields.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(t.small_size)
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(t.text_muted)
                                    .child(column.name.clone()),
                            )
                            .when(column.primary_key.is_some(), |el| {
                                el.child(icon(Icon::Key).size(px(10.)).text_color(t.warning))
                            })
                            .child(div().flex_1())
                            .child(
                                div()
                                    .text_color(t.text_faint)
                                    .child(column.type_name.clone()),
                            ),
                    )
                    .child(
                        div()
                            .px(px(8.))
                            .py(px(5.))
                            .rounded(t.radius_small)
                            .bg(t.input)
                            .border_1()
                            .border_color(t.border)
                            .font_family(t.mono_font.clone())
                            .text_size(t.mono_size)
                            .text_color(if null { t.text_faint } else { t.text })
                            .whitespace_normal()
                            .child(text),
                    ),
            );
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.sidebar)
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
                    .child(
                        div()
                            .text_color(t.text_faint)
                            .child(format!("row {}", thousands(row.row + 1))),
                    ),
            )
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
}
