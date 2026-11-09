//! The object sidebar: the active connection's namespaces and their tables, views and
//! collections, with a filter field on top. The list is virtualized, so a database with
//! thousands of tables scrolls as smoothly as one with ten.

use std::rc::Rc;

use deltos_data::{ObjectKind, ObjectRef};
use deltos_settings::PanelKind;
use gpui::{
    Context, Entity, Focusable, FontWeight, IntoElement, MouseButton, Render, SharedString,
    Subscription, Window, div, prelude::*, px, uniform_list,
};

use crate::state::{AppState, StateEvent};
use crate::theme::theme;
use crate::ui::icons::{Icon, icon};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{ButtonStyle, button, compact_count, empty_state};

#[derive(Clone)]
enum Row {
    Namespace {
        path: Vec<String>,
        label: SharedString,
        expanded: bool,
        loading: bool,
        count: Option<usize>,
    },
    Object {
        object: ObjectRef,
        rows: Option<u64>,
    },
    Note(SharedString),
}

pub struct ObjectsPanel {
    state: Entity<AppState>,
    search: Entity<TextInput>,
    selected: Option<ObjectRef>,
    _subs: Vec<Subscription>,
}

impl ObjectsPanel {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextInput::new("Search", cx));
        let subs = vec![
            cx.subscribe(&state, |_, _, e, cx| {
                if matches!(
                    e,
                    StateEvent::Sessions | StateEvent::ActiveSession | StateEvent::Settings
                ) {
                    cx.notify();
                }
            }),
            cx.subscribe(&search, |this, input, e, cx| match e {
                InputEvent::Changed => cx.notify(),
                InputEvent::Cancel => input.update(cx, |i, cx| i.clear(cx)),
                InputEvent::Submit => {
                    // Enter opens the first match.
                    if let Some(object) = this.rows(cx).into_iter().find_map(|r| match r {
                        Row::Object { object, .. } => Some(object),
                        _ => None,
                    }) {
                        this.open(object, true, cx);
                    }
                }
            }),
        ];
        Self {
            state,
            search,
            selected: None,
            _subs: subs,
        }
    }

    pub fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.search.focus_handle(cx));
    }

    fn open(&mut self, object: ObjectRef, pin: bool, cx: &mut Context<Self>) {
        let Some(connection) = self.state.read(cx).active.clone() else {
            return;
        };
        self.selected = Some(object.clone());
        if !object.kind.has_rows() {
            cx.notify();
            return;
        }
        self.state.update(cx, |s, cx| {
            s.open(PanelKind::Table { connection, object }, pin, cx)
        });
        cx.notify();
    }

    fn rows(&self, cx: &gpui::App) -> Vec<Row> {
        let s = self.state.read(cx);
        let Some(session) = s.active_session() else {
            return Vec::new();
        };
        let needle = self.search.read(cx).text().trim().to_lowercase();
        let mut rows = Vec::new();
        let single = session.namespaces.len() == 1;
        for ns in &session.namespaces {
            let objects: Vec<_> = ns
                .objects
                .iter()
                .flatten()
                .filter(|o| needle.is_empty() || o.object.name.to_lowercase().contains(&needle))
                .collect();
            if !needle.is_empty() && objects.is_empty() {
                continue;
            }
            let expanded = ns.expanded || !needle.is_empty();
            if !single {
                rows.push(Row::Namespace {
                    path: ns.namespace.path.clone(),
                    label: ns.namespace.path.last().cloned().unwrap_or_default().into(),
                    expanded,
                    loading: ns.loading,
                    count: ns.objects.as_ref().map(|o| o.len()),
                });
            }
            if expanded || single {
                if let Some(e) = &ns.error {
                    rows.push(Row::Note(e.clone().into()));
                } else if ns.objects.as_ref().is_some_and(|o| o.is_empty()) {
                    rows.push(Row::Note("No objects".into()));
                }
                for o in objects {
                    rows.push(Row::Object {
                        object: o.object.clone(),
                        rows: o.estimated_rows,
                    });
                }
            }
        }
        rows
    }
}

fn object_icon(kind: ObjectKind) -> Icon {
    match kind {
        ObjectKind::Table => Icon::Table,
        ObjectKind::View | ObjectKind::MaterializedView => Icon::View,
        ObjectKind::Collection => Icon::Collection,
        ObjectKind::Function | ObjectKind::Procedure => Icon::Function,
        ObjectKind::Sequence => Icon::Sequence,
        ObjectKind::Index => Icon::Key,
        ObjectKind::Trigger => Icon::Play,
    }
}

impl Render for ObjectsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let s = self.state.read(cx);
        let show_counts = s.settings.appearance.sidebar_row_counts;
        let session = s
            .active_session()
            .map(|x| (x.id.clone(), x.loading, x.error.clone()));
        let focused = self.search.focus_handle(cx).is_focused(window);
        let search = div().flex_none().p(px(8.)).child(
            div()
                .h(t.s(26.))
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(6.))
                .rounded(t.radius_small)
                .bg(t.input)
                .border_1()
                .border_color(if focused { t.accent } else { t.border })
                .text_size(t.ui_size)
                .child(icon(Icon::Search).size(px(13.)).text_color(t.text_faint))
                .child(self.search.clone()),
        );
        let Some((session_id, loading, error)) = session else {
            let state = self.state.clone();
            return div()
                .size_full()
                .flex()
                .flex_col()
                .bg(t.sidebar)
                .child(
                    empty_state(
                        "No connection",
                        Some("Open one to browse its tables.".into()),
                        &t,
                    )
                    .child(
                        button(
                            "open-connections",
                            "Connections",
                            ButtonStyle::Secondary,
                            &t,
                        )
                        .mt(px(8.))
                        .on_click(move |_, _, cx| {
                            state.update(cx, |s, cx| s.open(PanelKind::Connections, true, cx))
                        }),
                    ),
                )
                .into_any_element();
        };
        let rows = Rc::new(self.rows(cx));
        let selected = self.selected.clone();
        let this = cx.entity().downgrade();
        let state = self.state.clone();
        let t2 = t.clone();
        let list_rows = rows.clone();
        let list = uniform_list("objects", rows.len(), move |range, _, _| {
            let t = &t2;
            range
                .map(|i| {
                    let row = &list_rows[i];
                    let base = div()
                        .id(i)
                        .h(t.list_row)
                        .mx(px(6.))
                        .px(px(6.))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .rounded(t.radius_small)
                        .text_size(t.ui_size)
                        .whitespace_nowrap();
                    match row {
                        Row::Namespace {
                            path,
                            label,
                            expanded,
                            loading,
                            count,
                        } => {
                            let (state, sid, path) =
                                (state.clone(), session_id.clone(), path.clone());
                            base.cursor_pointer()
                                .text_color(t.text_muted)
                                .hover(|s| s.bg(t.hover))
                                .on_click(move |_, _, cx| {
                                    state.update(cx, |s, cx| s.toggle_namespace(&sid, &path, cx))
                                })
                                .child(
                                    icon(if *expanded {
                                        Icon::ChevronDown
                                    } else {
                                        Icon::ChevronRight
                                    })
                                    .size(px(12.))
                                    .text_color(t.text_faint),
                                )
                                .child(icon(Icon::Schema).size(px(13.)).text_color(t.text_faint))
                                .child(
                                    div()
                                        .flex_1()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(label.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(t.small_size)
                                        .text_color(t.text_faint)
                                        .child(if *loading {
                                            "…".to_owned()
                                        } else {
                                            count.map(|c| c.to_string()).unwrap_or_default()
                                        }),
                                )
                                .into_any_element()
                        }
                        Row::Object { object, rows } => {
                            let is_selected = selected.as_ref() == Some(object);
                            let open = this.clone();
                            let o1 = object.clone();
                            base.pl(px(24.))
                                .cursor_pointer()
                                .when(is_selected, |el| el.bg(t.selected).text_color(t.text))
                                .when(!is_selected, |el| {
                                    el.text_color(t.text).hover(|s| s.bg(t.hover))
                                })
                                .on_mouse_down(MouseButton::Left, move |e, _, cx| {
                                    let pin_it = e.click_count >= 2;
                                    open.update(cx, |p, cx| p.open(o1.clone(), pin_it, cx)).ok();
                                })
                                .child(
                                    icon(object_icon(object.kind)).size(px(13.)).text_color(
                                        if is_selected { t.accent } else { t.text_faint },
                                    ),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .child(object.name.clone()),
                                )
                                .when(show_counts, |el| {
                                    el.children(rows.map(|n| {
                                        div()
                                            .flex_none()
                                            .text_size(t.small_size)
                                            .text_color(t.text_faint)
                                            .child(compact_count(n))
                                    }))
                                })
                                .into_any_element()
                        }
                        Row::Note(text) => base
                            .pl(px(24.))
                            .text_size(t.small_size)
                            .text_color(t.text_faint)
                            .child(text.clone())
                            .into_any_element(),
                    }
                })
                .collect()
        })
        .flex_1()
        .pb(px(8.));
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.sidebar)
            .child(search)
            .when(loading, |el| {
                el.child(
                    div()
                        .px(px(14.))
                        .text_size(t.small_size)
                        .text_color(t.text_faint)
                        .child("Loading…"),
                )
            })
            .when_some(error, |el, e| {
                el.child(
                    div()
                        .px(px(14.))
                        .text_size(t.small_size)
                        .text_color(t.danger)
                        .child(e),
                )
            })
            .child(list)
            .into_any_element()
    }
}
