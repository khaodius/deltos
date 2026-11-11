//! The editor's completion popup and what fills it for a query tab: keywords, functions,
//! the connection's schemas and tables from its object tree, and the columns of the
//! tables a statement names. Columns load in the background the first time a table is
//! needed and stay cached for as long as the connection is open.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use deltos_data::{Connection, ObjectKind, ObjectRef};
use gpui::{
    AnyElement, App, Entity, FontWeight, Global, IntoElement, Pixels, Point, SharedString,
    WeakEntity, div, prelude::*, px,
};

use crate::lang::complete::{self, Item, ItemKind, Want};
use crate::lang::{Dialect, Language};
use crate::net::Net;
use crate::state::{AppState, NamespaceNode};
use crate::theme::Theme;
use crate::ui::editor::Editor;
use crate::ui::icons::{Icon, icon};

/// Most items the popup holds.
const LIMIT: usize = 60;
/// Rows shown at once.
const VISIBLE: usize = 9;

/// What the editor asks its completer.
pub struct Request<'a> {
    pub text: &'a str,
    pub cursor: usize,
    /// The statement the cursor is in, when there is one.
    pub statement: Option<Range<usize>>,
    /// Asked for by key, rather than by typing.
    pub explicit: bool,
    pub editor: WeakEntity<Editor>,
}

pub struct Completions {
    /// What an accepted item replaces, in the editor's text.
    pub replace: Range<usize>,
    pub items: Vec<Item>,
    pub explicit: bool,
}

pub type Completer = Rc<dyn Fn(&Request<'_>, &mut App) -> Option<Completions>>;

/// An open popup.
pub struct CompletionMenu {
    pub items: Vec<Item>,
    pub selected: usize,
    pub replace: Range<usize>,
    pub explicit: bool,
}

impl CompletionMenu {
    pub fn new(c: Completions) -> Option<Self> {
        (!c.items.is_empty()).then_some(Self {
            items: c.items,
            selected: 0,
            replace: c.replace,
            explicit: c.explicit,
        })
    }

    pub fn selected_label(&self) -> Option<String> {
        self.items.get(self.selected).map(|i| i.label.clone())
    }

    /// Keeps the highlighted item highlighted while the list narrows.
    pub fn select_label(&mut self, label: &str) {
        if let Some(i) = self.items.iter().position(|i| i.label == label) {
            self.selected = i;
        }
    }

    pub fn move_by(&mut self, step: isize) {
        let n = self.items.len() as isize;
        if n > 0 {
            self.selected = (self.selected as isize + step).rem_euclid(n) as usize;
        }
    }

    /// The range to replace and the text to put there.
    pub fn accept(self, index: Option<usize>) -> Option<(Range<usize>, String)> {
        let item = self.items.into_iter().nth(index.unwrap_or(self.selected))?;
        Some((self.replace, item.insert))
    }

    /// The popup at `position` (window coordinates, below the word being typed).
    pub fn render(
        &self,
        position: Point<Pixels>,
        editor: WeakEntity<Editor>,
        t: &Theme,
    ) -> AnyElement {
        let first = self
            .selected
            .saturating_sub(VISIBLE / 2)
            .min(self.items.len().saturating_sub(VISIBLE));
        let mut list = div()
            .id("completions")
            .occlude()
            .min_w(px(240.))
            .max_w(px(460.))
            .py(px(3.))
            .rounded(t.radius)
            .bg(t.floating)
            .border_1()
            .border_color(t.border_strong)
            .shadow_lg()
            .font_family(t.ui_font.clone())
            .text_size(t.small_size)
            .flex()
            .flex_col();
        for (ix, item) in self.items.iter().enumerate().skip(first).take(VISIBLE) {
            let selected = ix == self.selected;
            let editor = editor.clone();
            list = list.child(
                div()
                    .id(ix)
                    .mx(px(3.))
                    .h(t.s(22.))
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .rounded(t.radius_small)
                    .cursor_pointer()
                    .when(selected, |el| el.bg(t.accent).text_color(t.accent_text))
                    .when(!selected, |el| {
                        el.text_color(t.text).hover(|s| s.bg(t.hover))
                    })
                    .child(
                        div()
                            .w(px(14.))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .when(!selected, |el| el.text_color(t.text_faint))
                            .children(kind_icon(item.kind).map(|i| icon(i).size(px(12.)))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_family(t.mono_font.clone())
                            .when(item.kind == ItemKind::Keyword, |el| {
                                el.font_weight(FontWeight::MEDIUM)
                            })
                            .child(SharedString::from(item.label.clone())),
                    )
                    .children(item.detail.clone().map(|d| {
                        div()
                            .flex_none()
                            .max_w(px(180.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .opacity(if selected { 0.8 } else { 1.0 })
                            .when(!selected, |el| el.text_color(t.text_faint))
                            .child(d)
                    }))
                    .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                        cx.stop_propagation();
                        editor.update(cx, |e, cx| e.pick_completion(ix, cx)).ok();
                    }),
            );
        }
        if self.items.len() > VISIBLE {
            list = list.child(
                div()
                    .px(px(10.))
                    .pt(px(2.))
                    .text_color(t.text_faint)
                    .child(format!("{} of {}", self.selected + 1, self.items.len())),
            );
        }
        gpui::deferred(
            gpui::anchored()
                .position(position)
                .snap_to_window_with_margin(px(4.))
                .child(list),
        )
        .with_priority(3)
        .into_any_element()
    }
}

fn kind_icon(kind: ItemKind) -> Option<Icon> {
    Some(match kind {
        ItemKind::Column => Icon::Columns,
        ItemKind::Table => Icon::Table,
        ItemKind::View => Icon::View,
        ItemKind::Collection => Icon::Collection,
        ItemKind::Schema => Icon::Schema,
        ItemKind::Function | ItemKind::Method => Icon::Function,
        ItemKind::Keyword => return None,
    })
}

// ---- the column cache ----------------------------------------------------------------

enum Columns {
    /// Editors to tell when the list arrives.
    Loading(Vec<WeakEntity<Editor>>),
    Ready(Rc<[(String, String)]>),
    Failed,
}

/// Columns by object, per open connection.
#[derive(Default)]
struct Catalog {
    connections: HashMap<String, ConnectionColumns>,
}

struct ConnectionColumns {
    /// Which connection object the columns came from, so a reconnect starts afresh.
    key: usize,
    columns: HashMap<ObjectRef, Columns>,
}

impl Global for Catalog {}

fn connection_key(conn: &Arc<dyn Connection>) -> usize {
    Arc::as_ptr(conn) as *const () as usize
}

/// The columns of `object` (name and type), or `None` while they load. A first request
/// starts loading and tells `editor` when they are there.
fn columns_of(
    connection: &str,
    conn: &Arc<dyn Connection>,
    object: &ObjectRef,
    editor: &WeakEntity<Editor>,
    cx: &mut App,
) -> Option<Rc<[(String, String)]>> {
    let key = connection_key(conn);
    let catalog = cx.default_global::<Catalog>();
    let entry = catalog
        .connections
        .entry(connection.to_owned())
        .or_insert_with(|| ConnectionColumns {
            key,
            columns: HashMap::new(),
        });
    if entry.key != key {
        entry.key = key;
        entry.columns.clear();
    }
    match entry.columns.get_mut(object) {
        Some(Columns::Ready(list)) => return Some(list.clone()),
        Some(Columns::Failed) => return Some(Rc::from(Vec::new())),
        Some(Columns::Loading(waiting)) => {
            if !waiting.iter().any(|w| w == editor) {
                waiting.push(editor.clone());
            }
            return None;
        }
        None => {}
    }
    entry
        .columns
        .insert(object.clone(), Columns::Loading(vec![editor.clone()]));
    let (c2, o2) = (conn.clone(), object.clone());
    let job = cx
        .global::<Net>()
        .run(async move { c2.describe(&o2).await.map_err(|e| e.to_string()) });
    let (connection, object) = (connection.to_owned(), object.clone());
    cx.spawn(async move |cx| {
        let result = job.await;
        cx.update(|cx| {
            let ready = match result {
                Some(Ok(info)) => Columns::Ready(
                    info.columns
                        .into_iter()
                        .map(|c| (c.name, c.type_name))
                        .collect::<Vec<_>>()
                        .into(),
                ),
                _ => Columns::Failed,
            };
            let catalog = cx.default_global::<Catalog>();
            let Some(entry) = catalog.connections.get_mut(&connection) else {
                return;
            };
            if entry.key != key {
                return;
            }
            let waiting = match entry.columns.insert(object, ready) {
                Some(Columns::Loading(w)) => w,
                _ => Vec::new(),
            };
            for editor in waiting {
                editor.update(cx, |e, cx| e.refresh_completions(cx)).ok();
            }
        })
        .ok();
    })
    .detach();
    None
}

// ---- the completer for query tabs ----------------------------------------------------

/// Extra items from elsewhere in the app, such as saved queries, offered with the
/// statement's own.
pub type ExtraItems = Rc<dyn Fn(Option<&str>, &App) -> Vec<Item>>;

/// A completer that reads the connection named by `connection` (which the query tab
/// keeps current) from the app state.
pub fn query_completer(
    state: Entity<AppState>,
    connection: Rc<RefCell<Option<String>>>,
    extra: Option<ExtraItems>,
) -> Completer {
    Rc::new(move |request, cx| {
        let connection = connection.borrow().clone();
        let s = state.read(cx);
        if !request.explicit && !s.settings.editor.autocomplete {
            return None;
        }
        let session = connection.as_deref().and_then(|id| s.session(id));
        let language = session.map_or_else(Language::default, |s| Language::for_kind(s.kind));
        let conn = session.map(|s| s.conn.clone());

        // The statement around the cursor, and the cursor in it. Outside every statement
        // (in a comment between two) only the cursor's line is read, never the whole text.
        let (base, piece) = match &request.statement {
            Some(r) if r.start <= request.cursor => {
                let end = r.end.max(request.cursor).min(request.text.len());
                (r.start, request.text.get(r.start..end)?)
            }
            Some(r) => (request.cursor, request.text.get(request.cursor..r.end)?),
            None => {
                let text = request.text;
                let at = request.cursor.min(text.len());
                let start = text.get(..at)?.rfind('\n').map_or(0, |p| p + 1);
                let end = text[at..].find('\n').map_or(text.len(), |p| at + p);
                (start, &text[start..end])
            }
        };
        let at = request.cursor - base;
        let analysis = match language {
            Language::Sql(d) => complete::analyze_sql(piece, at, d),
            Language::Mongo => complete::analyze_mongo(piece, at),
        };
        let prefix = &piece[analysis.prefix.clone()];
        let replace = base + analysis.prefix.start..base + analysis.prefix.end;
        if !request.explicit
            && prefix.is_empty()
            && !matches!(analysis.want, Want::Member { .. })
            && !matches!(
                analysis.want,
                Want::Collections | Want::CollectionMethods | Want::CursorMethods
            )
        {
            return None;
        }
        let dialect = language.dialect().unwrap_or(Dialect::Postgres);

        // Names from the object tree, read in place: a schema can hold many thousands of
        // tables, and only those that match what was typed become items.
        let namespaces = session.map_or(&[][..], |s| s.namespaces.as_slice());
        let objects = || {
            namespaces.iter().flat_map(|n| {
                n.objects
                    .iter()
                    .flatten()
                    .filter(|o| o.object.kind.has_rows())
                    .map(move |o| (&n.namespace.path, &o.object))
            })
        };
        let table_items = |objects: &mut dyn Iterator<Item = (&Vec<String>, &ObjectRef)>,
                           priority: u8| {
            // With nothing typed every name ranks the same, so the first ones do.
            let most = if prefix.is_empty() { LIMIT } else { usize::MAX };
            objects
                .filter(|(_, o)| complete::matches(prefix, &o.name))
                .take(most)
                .map(|(path, o)| {
                    let kind = match o.kind {
                        ObjectKind::View | ObjectKind::MaterializedView => ItemKind::View,
                        ObjectKind::Collection => ItemKind::Collection,
                        _ => ItemKind::Table,
                    };
                    let insert = match language {
                        Language::Mongo => o.name.clone(),
                        Language::Sql(_) => complete::quote_if_needed(dialect, &o.name),
                    };
                    let mut item = Item::new(o.name.clone(), kind, priority).insert(insert);
                    if let Some(ns) = path.last() {
                        item = item.detail(ns.clone());
                    }
                    item
                })
                .collect::<Vec<_>>()
        };
        // Finds a statement's table in the tree.
        let lookup = |schema: Option<&str>, name: &str| -> Option<ObjectRef> {
            let in_schema = |path: &Vec<String>| {
                schema.is_none_or(|s| path.last().is_some_and(|p| p.eq_ignore_ascii_case(s)))
            };
            objects()
                .find(|(path, o)| in_schema(path) && o.name == name)
                .or_else(|| objects().find(|(_, o)| o.name.eq_ignore_ascii_case(name)))
                .map(|(_, o)| o.clone())
        };
        let mut items: Vec<Item> = Vec::new();
        // Tables whose columns to offer: (table, priority, name the table in the detail).
        let mut wanted: Vec<(ObjectRef, u8, bool)> = Vec::new();
        match &analysis.want {
            Want::Nothing => return None,
            Want::Tables => {
                items.extend(table_items(&mut objects(), 0));
                items.extend(schemas(namespaces, 2));
            }
            Want::Member { qualifier } => {
                let table = complete::resolve(&analysis.tables, qualifier)
                    .and_then(|t| lookup(t.schema.as_deref(), &t.name))
                    .or_else(|| {
                        let schema = qualifier
                            .len()
                            .checked_sub(2)
                            .map(|i| qualifier[i].as_str());
                        qualifier.last().and_then(|name| lookup(schema, name))
                    });
                if let Some(object) = table {
                    wanted.push((object, 0, false));
                }
                // `schema.`: the tables in it.
                if let Some(last) = qualifier.last() {
                    items.extend(table_items(
                        &mut objects().filter(|(path, _)| {
                            path.last().is_some_and(|p| p.eq_ignore_ascii_case(last))
                        }),
                        1,
                    ));
                }
            }
            Want::General => {
                let many = analysis.tables.len() > 1;
                for t in &analysis.tables {
                    if let Some(object) = lookup(t.schema.as_deref(), &t.name) {
                        wanted.push((object, 0, many));
                    }
                }
                if !prefix.is_empty() {
                    items.extend(table_items(&mut objects(), 2));
                }
                items.extend(complete::keywords(dialect).into_iter().map(|k| {
                    Item::new(k, ItemKind::Keyword, 3).insert(complete::keyword_case(prefix, k))
                }));
                items.extend(
                    complete::functions(dialect)
                        .into_iter()
                        .map(|f| Item::new(f, ItemKind::Function, 4).insert(format!("{f}("))),
                );
            }
            Want::Collections => {
                items.extend(table_items(&mut objects(), 0));
                items.extend(
                    complete::MONGO_DB_METHODS
                        .iter()
                        .map(|m| Item::new(*m, ItemKind::Method, 2).insert(format!("{m}("))),
                );
            }
            Want::CollectionMethods => items.extend(
                complete::MONGO_COLLECTION_METHODS
                    .iter()
                    .map(|m| Item::new(*m, ItemKind::Method, 0).insert(format!("{m}("))),
            ),
            Want::CursorMethods => items.extend(
                complete::MONGO_CURSOR_METHODS
                    .iter()
                    .map(|m| Item::new(*m, ItemKind::Method, 0).insert(format!("{m}("))),
            ),
        }
        if let Some(extra) = &extra
            && matches!(analysis.want, Want::General)
            && !prefix.is_empty()
        {
            items.extend(extra(connection.as_deref(), cx));
        }
        let prefix = prefix.to_owned();
        // Columns last: asking for them may start loading them, which needs the app.
        if let (Some(id), Some(conn)) = (connection.as_deref(), conn.as_ref()) {
            for (object, priority, with_table) in wanted {
                let Some(list) = columns_of(id, conn, &object, &request.editor, cx) else {
                    continue;
                };
                items.extend(list.iter().map(|(name, ty)| {
                    let detail = if with_table {
                        format!("{ty} · {}", object.name)
                    } else {
                        ty.clone()
                    };
                    Item::new(name.clone(), ItemKind::Column, priority)
                        .detail(detail)
                        .insert(complete::quote_if_needed(dialect, name))
                }));
            }
        }
        let items = complete::rank(&prefix, items, LIMIT);
        // Typing a whole word that is the only match needs no popup.
        if !request.explicit && items.len() == 1 && items[0].label.eq_ignore_ascii_case(&prefix) {
            return None;
        }
        Some(Completions {
            replace,
            items,
            explicit: request.explicit,
        })
    })
}

fn schemas(namespaces: &[NamespaceNode], priority: u8) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    for path in namespaces.iter().map(|n| &n.namespace.path) {
        if path.len() > 1
            && let Some(name) = path.last()
        {
            out.push(Item::new(name.clone(), ItemKind::Schema, priority));
        }
    }
    out
}
