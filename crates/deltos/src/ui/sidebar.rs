//! The object sidebar: the active connection's namespaces and their tables, views and
//! collections, with a filter field on top. The list is virtualized, so a database with
//! thousands of tables scrolls as smoothly as one with ten.

use std::rc::Rc;
use std::sync::Arc;

use deltos_data::{ObjectKind, ObjectRef};
use deltos_settings::PanelKind;
use gpui::{
    Context, Entity, Focusable, FontWeight, IntoElement, MouseButton, Render, SharedString,
    Subscription, Window, div, prelude::*, px, uniform_list,
};

use crate::state::{AppState, StateEvent};
use crate::theme::theme;
use crate::ui::edit;
use crate::ui::icons::{Icon, icon};
use crate::ui::text_input::{InputEvent, TextInput};
use crate::ui::{ButtonStyle, MenuItem, button, compact_count, context_menu, empty_state};

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
    /// An open right-click menu: where, and over which row.
    context: Option<(gpui::Point<gpui::Pixels>, Row)>,
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
            context: None,
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

    /// The right-click items for a row of the list.
    fn menu_items(&self, row: &Row, cx: &gpui::App) -> Vec<MenuItem> {
        let Some(session) = self.state.read(cx).active_session() else {
            return Vec::new();
        };
        let (sid, kind) = (session.id.clone(), session.kind);
        let conn = session.conn.clone();
        let page_rows = self.state.read(cx).settings.data.page_size;
        let safety = crate::ui::transfer::Safety::of(self.state.read(cx), &sid);
        let this = self.state.clone();
        let copy = |text: String| {
            move |_: &mut Window, cx: &mut gpui::App| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()))
            }
        };
        let mut items = Vec::new();
        match row {
            Row::Object { object, .. } => {
                let open = |pin: bool| {
                    let (state, object, sid) = (this.clone(), object.clone(), sid.clone());
                    move |_: &mut Window, cx: &mut gpui::App| {
                        if !object.kind.has_rows() {
                            return;
                        }
                        let kind = PanelKind::Table {
                            connection: sid.clone(),
                            object: object.clone(),
                        };
                        state.update(cx, |s, cx| s.open(kind, pin, cx));
                    }
                };
                let browsable = object.kind.has_rows();
                items.push(MenuItem::new("Open", open(false)).enabled(browsable));
                items.push(MenuItem::new("Open in a new tab", open(true)).enabled(browsable));
                let qualified = if edit::speaks_sql(kind) {
                    edit::table_name(kind, object)
                } else {
                    object.name.clone()
                };
                let select = if kind == deltos_data::DriverKind::Mongodb {
                    format!(
                        "db.getCollection({}).find({{}}).limit(100)",
                        serde_json::Value::String(object.name.clone())
                    )
                } else {
                    format!("SELECT * FROM {qualified} LIMIT 100;")
                };
                if browsable {
                    let (state, sid, text) = (this.clone(), sid.clone(), select.clone());
                    items.push(MenuItem::new("New query on this table", move |_, cx| {
                        let kind = PanelKind::Query {
                            connection: Some(sid.clone()),
                            text: text.clone(),
                        };
                        state.update(cx, |s, cx| s.open(kind, true, cx));
                    }));
                }
                items.push(MenuItem::separator());
                items.push(MenuItem::new("Copy name", copy(object.name.clone())));
                if qualified.contains('.') {
                    items.push(MenuItem::new("Copy full name", copy(qualified)));
                }
                if browsable {
                    items.push(MenuItem::new("Copy SELECT statement", copy(select)));
                }
                items.push(MenuItem::separator());
                let (state, s3, p3) = (this.clone(), sid.clone(), object.namespace.clone());
                items.extend(crate::ui::transfer::table_items(
                    conn.clone(),
                    object.clone(),
                    None,
                    None,
                    page_rows,
                    safety.clone(),
                    Some(Rc::new(move |cx: &mut gpui::App| {
                        state.update(cx, |s, cx| s.load_objects(&s3, &p3, cx))
                    })),
                ));
                items.push(MenuItem::separator());
                items.extend(structure_items(&this, &sid, kind, &conn, &safety, object));
                items.push(MenuItem::separator());
                let (state, sid, path) = (this.clone(), sid.clone(), object.namespace.clone());
                items.push(MenuItem::new("Reload the list", move |_, cx| {
                    state.update(cx, |s, cx| s.load_objects(&sid, &path, cx))
                }));
            }
            Row::Namespace { path, expanded, .. } => {
                let (state, s1, p1) = (this.clone(), sid.clone(), path.clone());
                items.push(MenuItem::new(
                    if *expanded { "Collapse" } else { "Expand" },
                    move |_, cx| state.update(cx, |s, cx| s.toggle_namespace(&s1, &p1, cx)),
                ));
                let (state, s2, p2) = (this.clone(), sid.clone(), path.clone());
                items.push(MenuItem::new("Reload", move |_, cx| {
                    state.update(cx, |s, cx| s.load_objects(&s2, &p2, cx))
                }));
                // A PostgreSQL database holds schemas; tables go in those.
                if edit::speaks_sql(kind)
                    && !(kind == deltos_data::DriverKind::Postgres && path.len() < 2)
                {
                    let (state, s4, p4) = (this.clone(), sid.clone(), path.clone());
                    items.push(read_only_off(
                        MenuItem::new("New table…", move |window, cx| {
                            crate::ui::structure::new_table(
                                state.clone(),
                                s4.clone(),
                                p4.clone(),
                                window,
                                cx,
                            )
                        }),
                        &safety,
                    ));
                }
                if edit::speaks_sql(kind) {
                    let (state, s3, p3) = (this.clone(), sid.clone(), path.clone());
                    items.push(crate::ui::transfer::script_item(
                        conn.clone(),
                        path.clone(),
                        safety.clone(),
                        Some(Rc::new(move |cx: &mut gpui::App| {
                            state.update(cx, |s, cx| s.load_objects(&s3, &p3, cx))
                        })),
                    ));
                }
                items.push(MenuItem::separator());
                items.push(MenuItem::new(
                    "Copy name",
                    copy(path.last().cloned().unwrap_or_default()),
                ));
            }
            Row::Note(_) => {}
        }
        items
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
        let menu = self.context.clone().map(|(position, row)| {
            let items = self.menu_items(&row, cx);
            let weak = cx.entity().downgrade();
            context_menu(
                "objects-menu",
                items,
                position,
                move |_, cx| {
                    weak.update(cx, |p, cx| {
                        p.context = None;
                        cx.notify();
                    })
                    .ok();
                },
                &t,
            )
        });
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
                                .on_mouse_down(MouseButton::Right, {
                                    let (this, row) = (this.clone(), row.clone());
                                    move |e, _, cx| {
                                        this.update(cx, |p, cx| {
                                            p.context = Some((e.position, row.clone()));
                                            cx.notify();
                                        })
                                        .ok();
                                    }
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
                                .on_mouse_down(MouseButton::Right, {
                                    let (this, row) = (this.clone(), row.clone());
                                    move |e, _, cx| {
                                        this.update(cx, |p, cx| {
                                            if let Row::Object { object, .. } = &row {
                                                p.selected = Some(object.clone());
                                            }
                                            p.context = Some((e.position, row.clone()));
                                            cx.notify();
                                        })
                                        .ok();
                                    }
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
            .children(menu)
            .into_any_element()
    }
}

/// `item`, turned off with the reason on a read-only connection.
fn read_only_off(item: MenuItem, safety: &crate::ui::transfer::Safety) -> MenuItem {
    if safety.read_only() {
        item.enabled(false).detail(crate::ui::transfer::READ_ONLY)
    } else {
        item
    }
}

/// "New table", "Empty" and "Drop" for an object of the list.
fn structure_items(
    state: &Entity<AppState>,
    connection: &str,
    kind: deltos_data::DriverKind,
    conn: &Arc<dyn deltos_data::Connection>,
    safety: &crate::ui::transfer::Safety,
    object: &ObjectRef,
) -> Vec<MenuItem> {
    let mut items = Vec::new();
    let changes = conn.capabilities().edit_rows && !crate::demo::is_demo(conn.as_ref());
    if edit::speaks_sql(kind) {
        let (state, sid, path) = (
            state.clone(),
            connection.to_string(),
            object.namespace.clone(),
        );
        items.push(read_only_off(
            MenuItem::new("New table…", move |window, cx| {
                crate::ui::structure::new_table(
                    state.clone(),
                    sid.clone(),
                    path.clone(),
                    window,
                    cx,
                )
            }),
            safety,
        ));
    }
    let reload = |state: &Entity<AppState>| {
        let (state, sid, path) = (
            state.clone(),
            connection.to_string(),
            object.namespace.clone(),
        );
        move || -> Box<dyn FnOnce(&mut gpui::App)> {
            let (state, sid, path) = (state.clone(), sid.clone(), path.clone());
            Box::new(move |cx| state.update(cx, |s, cx| s.load_objects(&sid, &path, cx)))
        }
    };
    let holds_rows = matches!(object.kind, ObjectKind::Table | ObjectKind::Collection);
    if holds_rows {
        let (conn, object, done) = (conn.clone(), object.clone(), reload(state));
        let env = safety.environment.clone();
        let empty = if object.kind == ObjectKind::Collection {
            "Empty collection…"
        } else {
            "Empty table…"
        };
        items.push(read_only_off(
            MenuItem::new(empty, move |window, cx| {
                crate::ui::structure::confirm_truncate(
                    conn.clone(),
                    env.clone(),
                    &object,
                    done(),
                    window,
                    cx,
                )
            })
            .enabled(changes)
            .danger(),
            safety,
        ));
    }
    if holds_rows || matches!(object.kind, ObjectKind::View | ObjectKind::MaterializedView) {
        let (conn, object, done) = (conn.clone(), object.clone(), reload(state));
        let env = safety.environment.clone();
        let label = match object.kind {
            ObjectKind::View | ObjectKind::MaterializedView => "Drop view…",
            ObjectKind::Collection => "Drop collection…",
            _ => "Drop table…",
        };
        items.push(read_only_off(
            MenuItem::new(label, move |window, cx| {
                crate::ui::structure::confirm_drop(
                    conn.clone(),
                    env.clone(),
                    &object,
                    done(),
                    window,
                    cx,
                )
            })
            .enabled(changes)
            .danger(),
            safety,
        ));
    }
    items
}
