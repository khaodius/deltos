//! The Documents view of a collection: one line per document on the left, the selected
//! document whole on the right, to read and edit as the shell writes it.
//!
//! The list reads the same bounded row cache as the grid, so it opens any part of a
//! collection of any size at once and follows the grid's filters and sort. It keeps its
//! own scroll position as a fractional row index, like the grid, because a pixel offset
//! does not hold a list of hundreds of millions of lines.
//!
//! The document on the right is read again from the server by its `_id`
//! ([`Connection::document`]), so it shows every field with its exact type (an Int64 as
//! `NumberLong`, a date as `ISODate`), even ones the grid leaves out. Saving replaces the
//! document in one write ([`Connection::replace_document`]), so a field removed in the
//! text is removed from the document; New inserts one ([`Connection::insert_document`]).
//! Saves go to the database at once, after the same confirmation a query would need on
//! that connection.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use deltos_data::{Column, Connection, ObjectRef, RowEdit, Value, ValueKind};
use gpui::{
    App, Bounds, ClipboardItem, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement,
    KeyDownEvent, MouseButton, Pixels, Render, ScrollWheelEvent, SharedString, Subscription,
    Window, canvas, div, prelude::*, px,
};

use crate::lang::Language;
use crate::net::Net;
use crate::safety::Environment;
use crate::state::AppState;
use crate::theme::{Theme, theme};
use crate::ui::editor::{Editor, EditorEvent};
use crate::ui::grid::GridView;
use crate::ui::icons::Icon;
use crate::ui::{ButtonStyle, Tooltip, button, empty_state, icon_button, thousands};

/// A preview line holds at most this many characters.
const PREVIEW_CHARS: usize = 400;
/// Previews kept between frames; past this the cache starts over.
const PREVIEW_CACHE: usize = 512;

/// The document on the right.
enum Detail {
    Nothing,
    Loading,
    /// Read from the server: its key and its text as read.
    Loaded {
        key: Vec<(String, Value)>,
        text: String,
    },
    /// A new document, not inserted yet.
    New,
    Missing,
    Failed(SharedString),
}

/// A write held back until the person confirms it.
#[derive(Clone)]
enum Pending {
    Save(String),
    Delete,
}

pub struct DocumentsView {
    state: Entity<AppState>,
    connection: String,
    object: ObjectRef,
    grid: Entity<GridView>,
    /// The first line on screen, as a fractional row of the cache.
    top: f64,
    /// The selected document, as a row of the cache.
    selected: Option<u64>,
    detail: Detail,
    editor: Entity<Editor>,
    /// The editor holds changes not saved yet.
    dirty: bool,
    busy: bool,
    notice: Option<(SharedString, bool)>,
    confirm: Option<(Pending, SharedString)>,
    focus: FocusHandle,
    list_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    previews: HashMap<u64, SharedString>,
    /// Bumped by every read of the detail, so a slow read never shows over a newer one.
    generation: u64,
    _subs: Vec<Subscription>,
}

impl DocumentsView {
    pub fn new(
        state: Entity<AppState>,
        connection: String,
        object: ObjectRef,
        grid: Entity<GridView>,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut e = Editor::new(String::new(), "{ field: \"value\" }", cx);
            e.set_language(Language::Mongo, cx);
            e
        });
        let subs = vec![
            cx.observe(&grid, |this, _, cx| {
                this.previews.clear();
                cx.notify();
            }),
            cx.subscribe(&editor, |this, _, _: &EditorEvent, cx| {
                let dirty = match &this.detail {
                    Detail::Loaded { text, .. } => this.editor.read(cx).text() != text,
                    Detail::New => !this.editor.read(cx).text().trim().is_empty(),
                    _ => false,
                };
                if dirty != this.dirty {
                    this.dirty = dirty;
                    cx.notify();
                }
            }),
        ];
        let top = grid.read(cx).rows_in_view(cx).start as f64;
        let top = (top - grid.read(cx).inserted() as f64).max(0.0);
        Self {
            state,
            connection,
            object,
            grid,
            top,
            selected: None,
            detail: Detail::Nothing,
            editor,
            dirty: false,
            busy: false,
            notice: None,
            confirm: None,
            focus: cx.focus_handle(),
            list_bounds: Rc::default(),
            previews: HashMap::new(),
            generation: 0,
            _subs: subs,
        }
    }

    fn conn(&self, cx: &App) -> Option<Arc<dyn Connection>> {
        self.state
            .read(cx)
            .session(&self.connection)
            .map(|s| s.conn.clone())
    }

    fn editable(&self, cx: &App) -> bool {
        self.object.kind == deltos_data::ObjectKind::Collection
            && self
                .conn(cx)
                .is_some_and(|c| c.capabilities().edit_rows && !crate::demo::is_demo(c.as_ref()))
    }

    /// Rows in the cache, as far as they are known.
    fn total(&self, cx: &App) -> u64 {
        self.grid
            .read(cx)
            .cache()
            .and_then(|c| c.row_count().value())
            .unwrap_or(0)
    }

    fn row_height(t: &Theme) -> f32 {
        f32::from(t.row_height)
    }

    fn fit(&self, t: &Theme) -> f64 {
        let h = self
            .list_bounds
            .get()
            .map_or(480.0, |b| f32::from(b.size.height));
        h as f64 / Self::row_height(t) as f64
    }

    fn clamp(&mut self, t: &Theme, cx: &App) {
        let max = (self.total(cx) as f64 - self.fit(t)).max(0.0);
        self.top = self.top.clamp(0.0, max);
    }

    fn on_scroll(&mut self, e: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let t = theme(cx).clone();
        let d = e.delta.pixel_delta(t.row_height);
        self.top -= f32::from(d.y) as f64 / Self::row_height(&t) as f64;
        self.clamp(&t, cx);
        cx.stop_propagation();
        cx.notify();
    }

    fn on_key(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let t = theme(cx).clone();
        let total = self.total(cx);
        if total == 0 {
            return;
        }
        let page = self.fit(&t).floor().max(1.0) as u64;
        let current = self.selected.unwrap_or(self.top as u64);
        let next = match e.keystroke.key.as_str() {
            "down" => current.saturating_add(1),
            "up" => current.saturating_sub(1),
            "pagedown" => current.saturating_add(page),
            "pageup" => current.saturating_sub(page),
            "home" => 0,
            "end" => total - 1,
            _ => return,
        }
        .min(total - 1);
        cx.stop_propagation();
        // Keep the selection on screen.
        if (next as f64) < self.top {
            self.top = next as f64;
        } else if (next + 1) as f64 > self.top + self.fit(&t) {
            self.top = (next + 1) as f64 - self.fit(&t);
        }
        self.clamp(&t, cx);
        self.select(next, window, cx);
    }

    /// The `_id` of cache row `row`, as the key of a [`RowEdit`].
    fn key_of(&self, row: u64, cx: &App) -> Option<Vec<(String, Value)>> {
        let grid = self.grid.read(cx);
        let cache = grid.cache()?;
        let columns = cache.columns()?;
        let at = columns.iter().position(|c| c.name == "_id")?;
        let id = cache.with_row(row, |values| values.get(at).cloned())??;
        (!id.is_null()).then(|| vec![("_id".to_owned(), id)])
    }

    fn select(&mut self, row: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected == Some(row) && !matches!(self.detail, Detail::New) {
            return;
        }
        if self.dirty {
            self.notify(
                "Save or revert the changes to this document first",
                true,
                cx,
            );
            return;
        }
        self.selected = Some(row);
        self.confirm = None;
        // The grid follows, so the inspector shows the same document.
        self.grid.update(cx, |g, cx| {
            let shown = row + g.inserted();
            g.select_cell((shown, 0), cx);
        });
        window.focus(&self.focus);
        self.load(cx);
    }

    /// Reads the selected document from the server.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let Some(row) = self.selected else {
            self.detail = Detail::Nothing;
            cx.notify();
            return;
        };
        let (Some(conn), Some(key)) = (self.conn(cx), self.key_of(row, cx)) else {
            self.detail =
                Detail::Failed("This document has no _id loaded yet, or has none at all.".into());
            cx.notify();
            return;
        };
        self.detail = Detail::Loading;
        cx.notify();
        let object = self.object.clone();
        let read_key = key.clone();
        let job = cx
            .global::<Net>()
            .run(async move { conn.document(&object, &read_key).await });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.detail = match result {
                    Some(Ok(Some(text))) => {
                        this.editor.update(cx, |e, cx| e.set_text(text.clone(), cx));
                        Detail::Loaded { key, text }
                    }
                    Some(Ok(None)) => Detail::Missing,
                    Some(Err(e)) => Detail::Failed(e.to_string().into()),
                    None => Detail::Failed("The read was cancelled.".into()),
                };
                this.dirty = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Shows `text` under the document for a while: longer for an error.
    fn notify(&mut self, text: impl Into<SharedString>, error: bool, cx: &mut Context<Self>) {
        let text = text.into();
        self.notice = Some((text.clone(), error));
        let wait = std::time::Duration::from_secs(if error { 12 } else { 4 });
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            this.update(cx, |this, cx| {
                if this.notice.as_ref().is_some_and(|(t, _)| *t == text) {
                    this.notice = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn new_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.dirty {
            self.notify(
                "Save or revert the changes to this document first",
                true,
                cx,
            );
            return;
        }
        self.generation += 1;
        self.detail = Detail::New;
        self.confirm = None;
        self.editor
            .update(cx, |e, cx| e.set_text("{\n  \n}".to_owned(), cx));
        self.dirty = false;
        let focus = self.editor.read(cx).focus_handle(cx);
        window.focus(&focus);
        cx.notify();
    }

    fn revert(&mut self, cx: &mut Context<Self>) {
        self.confirm = None;
        match &self.detail {
            Detail::Loaded { text, .. } => {
                let text = text.clone();
                self.editor.update(cx, |e, cx| e.set_text(text, cx));
            }
            Detail::New => {
                self.detail = Detail::Nothing;
                self.editor
                    .update(cx, |e, cx| e.set_text(String::new(), cx));
            }
            _ => {}
        }
        self.dirty = false;
        cx.notify();
    }

    /// Why a write needs the person's yes on this connection, as a query would.
    fn confirmation(&self, statement: &str, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        let env = Environment::of(state, &self.connection);
        crate::safety::confirmation(statement, env.as_ref(), &state.settings.data)
    }

    fn collection_call(&self, call: &str) -> String {
        format!(
            "db.getCollection({}).{call}",
            serde_json::Value::String(self.object.name.clone())
        )
    }

    fn save(&mut self, confirmed: bool, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let text = self.editor.read(cx).text().to_owned();
        let statement = match &self.detail {
            Detail::New => self.collection_call(&format!("insertOne({text})")),
            Detail::Loaded { .. } => self.collection_call(&format!("replaceOne({{}}, {text})")),
            _ => return,
        };
        if !confirmed && let Some(reason) = self.confirmation(&statement, cx) {
            self.confirm = Some((Pending::Save(text), reason.into()));
            cx.notify();
            return;
        }
        self.write(Pending::Save(text), cx);
    }

    fn delete(&mut self, confirmed: bool, cx: &mut Context<Self>) {
        if self.busy || !matches!(self.detail, Detail::Loaded { .. }) {
            return;
        }
        if !confirmed {
            let reason = self
                .confirmation(&self.collection_call("deleteOne({_id: 1})"), cx)
                .unwrap_or_else(|| "This can't be undone.".to_owned());
            self.confirm = Some((Pending::Delete, reason.into()));
            cx.notify();
            return;
        }
        self.write(Pending::Delete, cx);
    }

    fn write(&mut self, what: Pending, cx: &mut Context<Self>) {
        let Some(conn) = self.conn(cx) else {
            self.notify("Not connected", true, cx);
            return;
        };
        let key = match &self.detail {
            Detail::Loaded { key, .. } => Some(key.clone()),
            _ => None,
        };
        self.confirm = None;
        self.busy = true;
        cx.notify();
        let object = self.object.clone();
        let deleting = matches!(what, Pending::Delete);
        let job = cx.global::<Net>().run(async move {
            match (what, key) {
                (Pending::Save(text), Some(key)) => conn
                    .replace_document(&object, &key, &text)
                    .await
                    .map(|_| None),
                (Pending::Save(text), None) => conn.insert_document(&object, &text).await.map(Some),
                (Pending::Delete, Some(key)) => conn
                    .apply_edits(&object, &[RowEdit::Delete { key }])
                    .await
                    .map(|_| None),
                (Pending::Delete, None) => Ok(None),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Some(Ok(inserted)) => {
                        this.dirty = false;
                        match inserted {
                            Some(key) => {
                                let text = this.editor.read(cx).text().to_owned();
                                this.detail = Detail::Loaded { key, text };
                                this.selected = None;
                                this.notify("Inserted the document", false, cx);
                            }
                            None if deleting => {
                                this.detail = Detail::Nothing;
                                this.selected = None;
                                this.editor
                                    .update(cx, |e, cx| e.set_text(String::new(), cx));
                                this.notify("Deleted the document", false, cx);
                            }
                            None => {
                                this.notify("Saved", false, cx);
                                this.load(cx);
                            }
                        }
                        let id = this.connection.clone();
                        this.state
                            .update(cx, |s, cx| s.data_changed(&id, false, cx));
                    }
                    Some(Err(e)) => this.notify(e.to_string(), true, cx),
                    None => this.notify("The write was cancelled", true, cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn copy(&self, cx: &mut App) {
        let text = self.editor.read(cx).text().to_owned();
        if !text.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// One line for cache row `row`: `{ _id: …, name: "…", … }`, or `None` while it loads.
    fn preview(&mut self, row: u64, columns: &[Column], cx: &App) -> Option<SharedString> {
        if let Some(p) = self.previews.get(&row) {
            return Some(p.clone());
        }
        let grid = self.grid.read(cx);
        let cache = grid.cache()?;
        let line = cache.with_row(row, |values| preview_line(columns, values))?;
        let line: SharedString = line.into();
        if self.previews.len() >= PREVIEW_CACHE {
            self.previews.clear();
        }
        self.previews.insert(row, line.clone());
        Some(line)
    }

    fn list(&mut self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let total = self.total(cx);
        let columns = self
            .grid
            .read(cx)
            .cache()
            .and_then(|c| c.columns())
            .unwrap_or_else(|| Arc::from(Vec::new()));
        self.clamp(t, cx);
        let rh = Self::row_height(t);
        let first = self.top.floor() as u64;
        let offset = ((self.top - first as f64) * rh as f64) as f32;
        let wanted = self.fit(t).ceil() as u64 + 1;
        let end = (first + wanted).min(total);
        if let Some(cache) = self.grid.read(cx).cache() {
            let exact = cache.row_count().is_exact();
            cache.set_viewport(first..if exact { end } else { first + wanted });
        }
        let digits = thousands(total.max(1)).len().max(3);
        let number_w = t.s(8.0 + 7.0 * digits as f32);
        let mut rows = Vec::with_capacity((end - first) as usize);
        for row in first..end {
            let preview = self.preview(row, &columns, cx);
            let selected = self.selected == Some(row);
            rows.push(
                div()
                    .id(("document", row as usize))
                    .h(px(rh))
                    .w_full()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(8.))
                    .cursor_pointer()
                    .border_b_1()
                    .border_color(t.grid_line)
                    .when(selected, |el| el.bg(t.selected_row))
                    .when(!selected, |el| el.hover(|s| s.bg(t.hover)))
                    .child(
                        div()
                            .flex_none()
                            .w(number_w)
                            .text_right()
                            .text_size(t.small_size)
                            .text_color(t.text_faint)
                            .child(thousands(row + 1)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_family(t.grid_font.clone())
                            .text_size(t.mono_size)
                            .text_color(if preview.is_some() {
                                t.text
                            } else {
                                t.text_faint
                            })
                            .child(preview.unwrap_or_else(|| "…".into())),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| this.select(row, window, cx))),
            );
        }
        let bounds = self.list_bounds.clone();
        div()
            .id("documents-list")
            .relative()
            .size_full()
            .overflow_hidden()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, _| window.focus(&this.focus)),
            )
            .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, _, cx| this.on_scroll(e, cx)))
            .child(
                canvas(
                    move |b, window, _| {
                        let old = bounds.replace(Some(b));
                        if old.map(|o| o.size) != Some(b.size) {
                            window.request_animation_frame();
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                div()
                    .absolute()
                    .top(px(-offset))
                    .left_0()
                    .right_0()
                    .flex()
                    .flex_col()
                    .children(rows),
            )
            .when_some(self.thumb(t, total), |el, (y, h)| {
                // Where the screen is in the whole collection.
                el.child(
                    div()
                        .absolute()
                        .right(px(2.))
                        .top(px(y))
                        .w(px(4.))
                        .h(px(h))
                        .rounded_full()
                        .bg(t.border_strong),
                )
            })
    }

    /// The scroll mark's top and height, when not everything fits.
    fn thumb(&self, t: &Theme, total: u64) -> Option<(f32, f32)> {
        let fit = self.fit(t);
        let share = fit / total.max(1) as f64;
        if share >= 1.0 {
            return None;
        }
        let h = self
            .list_bounds
            .get()
            .map_or(480.0, |b| f32::from(b.size.height));
        let thumb = (share.max(0.02) * h as f64) as f32;
        let max = (total as f64 - fit).max(1.0);
        Some(((self.top / max) as f32 * (h - thumb), thumb))
    }

    fn detail(&mut self, t: &Theme, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let editable = self.editable(cx);
        let title: SharedString = match &self.detail {
            Detail::Loaded { key, .. } => key
                .first()
                .map(|(_, v)| format!("_id {}", short_value(v)).into())
                .unwrap_or_else(|| "Document".into()),
            Detail::New => "New document".into(),
            _ => "Document".into(),
        };
        let body = match &self.detail {
            Detail::Nothing => empty_state(
                "No document selected",
                Some("Pick a document on the left, or add one with New.".into()),
                t,
            )
            .into_any_element(),
            Detail::Loading => empty_state("Reading the document…", None, t).into_any_element(),
            Detail::Missing => empty_state(
                "This document is gone",
                Some("It was changed or deleted since the list was read.".into()),
                t,
            )
            .into_any_element(),
            Detail::Failed(e) => {
                empty_state("The document could not be read", Some(e.clone()), t).into_any_element()
            }
            Detail::Loaded { text, .. } if !editable => read_only_text(text, t).into_any_element(),
            Detail::Loaded { .. } | Detail::New => div()
                .size_full()
                .child(self.editor.clone())
                .into_any_element(),
        };
        let has_doc = matches!(self.detail, Detail::Loaded { .. } | Detail::New);
        let loaded = matches!(self.detail, Detail::Loaded { .. });
        let dirty = self.dirty;
        let busy = self.busy;
        let confirm = self.confirm.clone().map(|(what, reason)| {
            let (label, style) = match what {
                Pending::Save(_) => ("Save anyway", ButtonStyle::Primary),
                Pending::Delete => ("Delete", ButtonStyle::Danger),
            };
            div()
                .flex_none()
                .px(px(10.))
                .py(px(6.))
                .flex()
                .items_center()
                .gap(px(8.))
                .border_b_1()
                .border_color(t.danger)
                .text_size(t.ui_size)
                .child(div().flex_1().text_color(t.text).child(reason))
                .child(
                    button("doc-confirm-cancel", "Cancel", ButtonStyle::Secondary, t).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.confirm = None;
                            cx.notify();
                        }),
                    ),
                )
                .child(button("doc-confirm", label, style, t).on_click(cx.listener(
                    move |this, _, _, cx| match &what {
                        Pending::Save(_) => this.save(true, cx),
                        Pending::Delete => this.delete(true, cx),
                    },
                )))
        });
        let notice = self.notice.clone().map(|(text, error)| {
            div()
                .flex_none()
                .px(px(10.))
                .py(px(4.))
                .text_size(t.small_size)
                .text_color(if error { t.danger } else { t.text_muted })
                .child(text)
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.surface)
            .child(
                div()
                    .flex_none()
                    .h(t.bar_height)
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(t.border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(t.ui_size)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.text)
                            .child(title),
                    )
                    .when(has_doc, |el| {
                        el.child(
                            icon_button("doc-copy", Icon::Copy, t)
                                .tooltip(|_, cx| Tooltip::view("Copy the document", cx))
                                .on_click(cx.listener(|this, _, _, cx| this.copy(cx))),
                        )
                    })
                    .when(editable && loaded && !dirty, |el| {
                        el.child(
                            icon_button("doc-delete", Icon::Trash, t)
                                .tooltip(|_, cx| Tooltip::view("Delete the document", cx))
                                .on_click(cx.listener(|this, _, _, cx| this.delete(false, cx))),
                        )
                    })
                    .when(editable && has_doc && (dirty || !loaded), |el| {
                        el.child(
                            button("doc-revert", "Revert", ButtonStyle::Ghost, t)
                                .on_click(cx.listener(|this, _, _, cx| this.revert(cx))),
                        )
                        .child(
                            button(
                                "doc-save",
                                if busy {
                                    "Saving…"
                                } else if loaded {
                                    "Save"
                                } else {
                                    "Insert"
                                },
                                ButtonStyle::Primary,
                                t,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.save(false, cx))),
                        )
                    }),
            )
            .children(confirm)
            .child(div().flex_1().min_h_0().child(body))
            .children(notice)
    }
}

impl Render for DocumentsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = theme(cx).clone();
        let total = self.total(cx);
        let editable = self.editable(cx);
        let count = match self.grid.read(cx).cache().map(|c| c.row_count()) {
            Some(n) if n.is_exact() => format!("{} documents", thousands(total)),
            Some(_) if total > 0 => format!("About {} documents", thousands(total)),
            _ => "Documents".to_owned(),
        };
        let list = if total == 0 {
            empty_state("No documents", None, &t).into_any_element()
        } else {
            self.list(&t, cx).into_any_element()
        };
        let detail = self.detail(&t, cx);
        div()
            .size_full()
            .flex()
            .flex_row()
            .bg(t.surface)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .border_r_1()
                    .border_color(t.border)
                    .child(
                        div()
                            .flex_none()
                            .h(t.bar_height)
                            .px(px(8.))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .border_b_1()
                            .border_color(t.border)
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(t.small_size)
                                    .text_color(t.text_muted)
                                    .child(count),
                            )
                            .when(editable, |el| {
                                el.child(
                                    icon_button("doc-new", Icon::Plus, &t)
                                        .tooltip(|_, cx| Tooltip::view("New document", cx))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.new_document(window, cx)
                                        })),
                                )
                            })
                            .child(
                                icon_button("doc-refresh", Icon::Refresh, &t)
                                    .tooltip(|_, cx| Tooltip::view("Read the document again", cx))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if !this.dirty {
                                            this.load(cx);
                                        }
                                    })),
                            ),
                    )
                    .child(div().flex_1().min_h_0().child(list)),
            )
            .child(div().flex_1().min_w_0().h_full().child(detail))
    }
}

/// A document shown for reading only, one line per line of text.
fn read_only_text(text: &str, t: &Theme) -> impl IntoElement + use<> {
    const MAX_LINES: usize = 5_000;
    let lines: Vec<SharedString> = text
        .lines()
        .take(MAX_LINES)
        .map(|l| SharedString::from(l.to_owned()))
        .collect();
    let cut = text.lines().nth(MAX_LINES).is_some();
    div()
        .id("document-text")
        .size_full()
        .overflow_y_scroll()
        .p(px(10.))
        .font_family(t.editor_font.clone())
        .text_size(t.editor_size)
        .text_color(t.text)
        .children(
            lines
                .into_iter()
                .map(|l| div().whitespace_nowrap().child(l)),
        )
        .when(cut, |el| {
            el.child(
                div()
                    .pt(px(6.))
                    .text_color(t.text_faint)
                    .child("Longer documents are cut here. Copy it to read the rest."),
            )
        })
}

/// A short form of a key value, for the header.
fn short_value(v: &Value) -> String {
    match v {
        Value::Text(s) => s.to_string(),
        Value::Json(s) => s.to_string(),
        other => format!("{other:?}"),
    }
}

/// `{ _id: …, name: "…", … }`, the fields a row has, cut at [`PREVIEW_CHARS`].
pub fn preview_line(columns: &[Column], values: &[Value]) -> String {
    let mut out = String::from("{ ");
    let mut first = true;
    for (column, value) in columns.iter().zip(values) {
        if value.is_null() {
            continue;
        }
        if !first {
            out.push_str(", ");
        }
        first = false;
        out.push_str(&column.name);
        out.push_str(": ");
        push_value(&mut out, value, column.kind);
        if out.chars().count() > PREVIEW_CHARS {
            let cut: String = out.chars().take(PREVIEW_CHARS).collect();
            return cut + "…";
        }
    }
    out.push_str(if first { "}" } else { " }" });
    out
}

fn push_value(out: &mut String, value: &Value, kind: ValueKind) {
    match (value, kind) {
        (Value::Text(s), ValueKind::ObjectId) => {
            out.push_str("ObjectId(\"");
            out.push_str(s);
            out.push_str("\")");
        }
        (Value::Text(s), ValueKind::DateTime | ValueKind::Date) => {
            out.push_str("ISODate(\"");
            out.push_str(s);
            out.push_str("\")");
        }
        (Value::Text(s), ValueKind::Uuid) => {
            out.push_str("UUID(\"");
            out.push_str(s);
            out.push_str("\")");
        }
        (Value::Text(s), ValueKind::Decimal) => out.push_str(s),
        (Value::Text(s), _) => {
            let quoted = serde_json::Value::String(s.to_string()).to_string();
            out.push_str(&quoted);
        }
        (Value::Json(s), _) => out.push_str(s),
        (Value::Bool(b), _) => out.push_str(if *b { "true" } else { "false" }),
        (Value::Int(n), _) => out.push_str(&n.to_string()),
        (Value::Float(f), _) => out.push_str(&f.to_string()),
        (Value::Bytes(b), _) => out.push_str(&format!("BinData(0, …{} bytes)", b.len())),
        (other, _) => out.push_str(&format!("{other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_a_row_as_shell_text() {
        let columns = [
            Column::new("_id", "objectId", ValueKind::ObjectId),
            Column::new("name", "string", ValueKind::Text),
            Column::new("gone", "string", ValueKind::Text),
            Column::new("tags", "array", ValueKind::Array),
            Column::new("n", "int", ValueKind::Int),
        ];
        let values = [
            Value::text("65a1b2c3d4e5f60718293a4b"),
            Value::text("a \"b\""),
            Value::Null,
            Value::Json("[\"x\"]".into()),
            Value::Int(3),
        ];
        assert_eq!(
            preview_line(&columns, &values),
            r#"{ _id: ObjectId("65a1b2c3d4e5f60718293a4b"), name: "a \"b\"", tags: ["x"], n: 3 }"#
        );
        assert_eq!(preview_line(&columns[..1], &[Value::Null]), "{ }");
        let long = [Value::text("x".repeat(1_000))];
        let line = preview_line(&columns[1..2], &long);
        assert!(line.ends_with('…'));
        assert!(line.chars().count() <= PREVIEW_CHARS + 1);
    }
}
