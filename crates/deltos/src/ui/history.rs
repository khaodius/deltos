//! Statements that ran this session, newest first, with how long they took and what
//! they returned. Clicking one opens it in a new query tab.

use std::rc::Rc;
use std::time::UNIX_EPOCH;

use deltos_settings::PanelKind;
use gpui::{
    Context, Entity, IntoElement, Render, Subscription, Window, div, prelude::*, px, uniform_list,
};

use crate::state::{AppState, StateEvent};
use crate::theme::theme;
use crate::ui::{empty_state, thousands};

pub struct HistoryPanel {
    state: Entity<AppState>,
    _subs: Vec<Subscription>,
}

impl HistoryPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let subs = vec![cx.subscribe(&state, |_, _, e, cx| {
            if matches!(e, StateEvent::History | StateEvent::Settings) {
                cx.notify();
            }
        })];
        Self { state, _subs: subs }
    }
}

/// `HH:MM:SS` of a time, in UTC: the history is for this session, so the clock time is
/// enough to tell entries apart.
fn clock(at: std::time::SystemTime) -> String {
    let secs = at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

impl Render for HistoryPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let entries: Rc<Vec<_>> =
            Rc::new(self.state.read(cx).history.iter().rev().cloned().collect());
        if entries.is_empty() {
            return div()
                .size_full()
                .bg(t.sidebar)
                .child(empty_state(
                    "No statements yet",
                    Some("Queries you run appear here.".into()),
                    &t,
                ))
                .into_any_element();
        }
        let state = self.state.clone();
        let t2 = t.clone();
        div()
            .size_full()
            .bg(t.sidebar)
            .flex()
            .flex_col()
            .child(
                uniform_list("history", entries.len(), move |range, _, _| {
                    let t = &t2;
                    range
                        .map(|i| {
                            let e = &entries[i];
                            let (state, text, conn) =
                                (state.clone(), e.statement.clone(), e.connection.clone());
                            let detail = match (&e.error, e.rows) {
                                (Some(err), _) => err.clone(),
                                (None, Some(n)) => format!("{} rows", thousands(n)),
                                (None, None) => "done".into(),
                            };
                            div()
                                .id(i)
                                .h(t.list_row)
                                .px(px(10.))
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .border_b_1()
                                .border_color(t.grid_line)
                                .cursor_pointer()
                                .hover(|s| s.bg(t.hover))
                                .on_click(move |_, _, cx| {
                                    let kind = PanelKind::Query {
                                        connection: Some(conn.clone()),
                                        text: text.clone(),
                                    };
                                    state.update(cx, |s, cx| s.open(kind, true, cx));
                                })
                                .text_size(t.small_size)
                                .child(
                                    div()
                                        .flex_none()
                                        .w(px(58.))
                                        .text_color(t.text_faint)
                                        .child(clock(e.at)),
                                )
                                .child(
                                    div().flex_none().w(px(64.)).text_color(t.text_faint).child(
                                        format!("{:.0} ms", e.duration.as_secs_f64() * 1000.0),
                                    ),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .font_family(t.mono_font.clone())
                                        .text_size(t.mono_size)
                                        .text_color(t.text)
                                        .child(e.statement.replace('\n', " ")),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .max_w(px(260.))
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .text_color(if e.error.is_some() {
                                            t.danger
                                        } else {
                                            t.text_muted
                                        })
                                        .child(detail),
                                )
                        })
                        .collect()
                })
                .flex_1(),
            )
            .into_any_element()
    }
}
